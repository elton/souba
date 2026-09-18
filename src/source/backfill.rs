//! A 股日线的全量回补：新浪原始日线 + qfq 因子 → 本地库。
//!
//! 为什么要落库：新浪日线一次给全量（茅台 5990 根），而 EMA576 要 1330 根才收敛，
//! 腾讯那条路只有 640 根。补一次之后就本地读，既快又不用反复打人家接口。

use crate::core::adjust::AdjFactor;
use crate::core::bar::Timeframe;
use crate::core::symbol::{Market, Symbol};
use crate::source::history::{HistoryClient, HistoryError};
use crate::store::Store;

/// 回补开始时写入。中途退出就停在这个状态，下次开机接着补。
pub const RUNNING: &str = "running";
/// 全量日线与因子都已落库
pub const DONE: &str = "done";

/// 新浪日线一次吐全量，给个够大的上限即可（茅台 5990 根）
const FULL_HISTORY: usize = 6000;

/// 目前只有 A 股日线走本地库。港股 / 美股仍然每次直连源 ——
/// 它们没有全量历史源，也没有因子接口，落库落不出东西来。
pub fn is_local(symbol: &Symbol, tf: Timeframe) -> bool {
    symbol.market == Market::Cn && tf == Timeframe::Day
}

/// 是否还需要回补。只有明确标成 `done` 才跳过 ——
/// 没有记录、或者停在 `running`（上次补到一半被杀），都要再跑一遍。
pub fn needs(store: &Store, symbol: &Symbol, tf: Timeframe) -> anyhow::Result<bool> {
    Ok(store.backfill_status(symbol, tf)?.as_deref() != Some(DONE))
}

/// 拉全量原始日线与因子并落库，返回落库的根数。
///
/// 两个请求都走 `HistoryClient` 的 `Throttle`（7 秒间隔 + 指数退避），
/// 所以这个函数天然是慢的 —— 调用方必须在后台跑，不能卡 UI。
///
/// 中途失败不回滚：已经写进去的 bar 是对的，`upsert_bars` 幂等，
/// 状态停在 `running`，下次进来重跑一遍就补齐了。
pub async fn run(
    store: &Store,
    client: &HistoryClient,
    symbol: &Symbol,
) -> Result<usize, BackfillError> {
    let tf = Timeframe::Day;
    store.set_backfill_status(symbol, tf, RUNNING)?;
    let bars = client.bars(symbol, tf, FULL_HISTORY).await?;
    store.upsert_bars(symbol, tf, &bars)?;
    let factors: Vec<AdjFactor> = client.adj_factors(symbol).await?;
    store.upsert_adj_factors(symbol, &factors)?;
    store.set_backfill_status(symbol, tf, DONE)?;
    Ok(bars.len())
}

#[derive(Debug, thiserror::Error)]
pub enum BackfillError {
    #[error("{0}")]
    Source(#[from] HistoryError),
    #[error("写库失败：{0}")]
    Store(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bar::Bar;

    fn sym(s: &str) -> Symbol {
        Symbol::parse(s).unwrap()
    }

    #[test]
    fn 只有a股日线走本地库() {
        assert!(is_local(&sym("CN:600519"), Timeframe::Day));
        assert!(!is_local(&sym("CN:600519"), Timeframe::Min60));
        assert!(!is_local(&sym("HK:00700"), Timeframe::Day));
        assert!(!is_local(&sym("US:AAPL"), Timeframe::Day));
    }

    #[test]
    fn 没记录和停在进行中都要补_已完成的不补() {
        let st = Store::open_in_memory().unwrap();
        let s = sym("CN:600519");
        assert!(needs(&st, &s, Timeframe::Day).unwrap(), "没记录时要补");
        st.set_backfill_status(&s, Timeframe::Day, RUNNING).unwrap();
        assert!(needs(&st, &s, Timeframe::Day).unwrap(), "停在 running 时要接着补");
        st.set_backfill_status(&s, Timeframe::Day, DONE).unwrap();
        assert!(!needs(&st, &s, Timeframe::Day).unwrap());
    }

    /// 回补到一半被杀 → 重开一个 Store 指向同一个文件 → 已补完的不重来、
    /// 补了一半的接着补，而且已落库的 bar 还在。
    #[test]
    fn 重启后从上次的进度继续而不是重头() {
        let path = std::env::temp_dir().join(format!(
            "souba-backfill-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let (half, done) = (sym("CN:600519"), sym("CN:000001"));
        let bars: Vec<Bar> = (0..100)
            .map(|i| Bar {
                ts: chrono::DateTime::from_timestamp(1_600_000_000 + i * 86_400, 0).unwrap(),
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 1.0,
            })
            .collect();
        {
            let st = Store::open(&path).unwrap();
            st.upsert_bars(&half, Timeframe::Day, &bars).unwrap();
            st.set_backfill_status(&half, Timeframe::Day, RUNNING).unwrap();
            st.set_backfill_status(&done, Timeframe::Day, DONE).unwrap();
        }
        let st = Store::open(&path).unwrap();
        assert!(needs(&st, &half, Timeframe::Day).unwrap(), "补了一半的要接着补");
        assert!(!needs(&st, &done, Timeframe::Day).unwrap(), "补完的不该重头来");
        assert_eq!(
            st.raw_bars(&half, Timeframe::Day).unwrap().len(),
            100,
            "重启不该丢掉上次已经补进去的 bar"
        );
        drop(st);
        let _ = std::fs::remove_file(&path);
    }

    /// 真回补一只 A 股。会打新浪两次、跑十几秒，所以默认不跑：
    /// `cargo test -- --ignored 真实回补茅台全量日线`
    #[tokio::test]
    #[ignore = "联网冒烟测试，手动跑"]
    async fn 真实回补茅台全量日线() {
        let st = Store::open_in_memory().unwrap();
        let client = HistoryClient::new().unwrap();
        let s = sym("CN:600519");
        let n = run(&st, &client, &s).await.unwrap();
        assert!(n > 5000, "只回补到 {n} 根，新浪应当给全量");
        assert_eq!(st.backfill_status(&s, Timeframe::Day).unwrap().as_deref(), Some(DONE));
        assert!(!needs(&st, &s, Timeframe::Day).unwrap());
        let adj = st.adjusted_bars(&s, Timeframe::Day).unwrap();
        assert!(adj.len() > 5000, "读回来只有 {} 根", adj.len());
        assert!(!st.adj_factors(&s).unwrap().is_empty(), "因子也要落库");
        // 最早那几根复权后必然远低于原始价（茅台累计因子接近 9）
        let raw = st.raw_bars(&s, Timeframe::Day).unwrap();
        assert!(adj[0].close < raw[0].close / 2.0);
    }
}
