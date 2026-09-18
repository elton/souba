//! 详情屏的 K 线装载。
//!
//! 两条路：A 股日线走本地库（没有就先回补），其余市场仍旧每次直连源。
//! 单任务串行消费请求 —— 节流器本来就把并发请求排成队，再开并发只会互相等。

use std::sync::Arc;

use crate::core::bar::Timeframe;
use crate::core::symbol::Symbol;
use crate::source::backfill;
use crate::source::history::{HistoryClient, HistoryError};
use crate::store::Store;
use crate::ui::detail::BarState;

/// 历史加载请求：标的 + 周期
pub type BarKey = (Symbol, Timeframe);

type Tx = tokio::sync::watch::Sender<(Option<BarKey>, BarState)>;

pub fn spawn(
    store: Arc<Store>,
    mut req_rx: tokio::sync::mpsc::Receiver<BarKey>,
    bar_tx: Tx,
    limit: usize,
) {
    tokio::spawn(async move {
        let client = match HistoryClient::new() {
            Ok(c) => c,
            Err(e) => {
                let _ = bar_tx.send((None, BarState::Failed(e.to_string())));
                return;
            }
        };
        while let Some(key) = req_rx.recv().await {
            let _ = bar_tx.send((Some(key.clone()), BarState::Loading));
            if backfill::is_local(&key.0, key.1) {
                local(&store, &client, &key, &bar_tx).await;
            } else {
                let _ = bar_tx.send((Some(key.clone()), remote(&client, &key, limit).await));
            }
        }
    });
}

async fn remote(client: &HistoryClient, key: &BarKey, limit: usize) -> BarState {
    match client.bars(&key.0, key.1, limit).await {
        Ok(bars) => BarState::Ready(bars),
        // 「没有数据源」和「拉取失败」要分开 —— 前者重试多少次都没用
        Err(
            e @ (HistoryError::MarketUnsupported(_) | HistoryError::TimeframeUnsupported { .. }),
        ) => BarState::Unsupported(e.to_string()),
        Err(e) => BarState::Failed(e.to_string()),
    }
}

/// 本地库路径：有多少先画多少，缺的在后台补，补完再推一次。
///
/// 先画再补这一步是有意的 —— 回补要打两次新浪、隔 7 秒，
/// 让用户对着「回补中」干等十几秒没有道理。
async fn local(store: &Store, client: &HistoryClient, key: &BarKey, tx: &Tx) {
    let (symbol, tf) = key;
    let (bars, will_backfill) = match (
        store.adjusted_bars(symbol, *tf),
        backfill::needs(store, symbol, *tf),
    ) {
        (Ok(bars), Ok(needs)) => (bars, needs),
        (Err(e), _) | (_, Err(e)) => {
            let _ = tx.send((Some(key.clone()), BarState::Failed(e.to_string())));
            return;
        }
    };
    let had_local = !bars.is_empty();
    let _ = tx.send((Some(key.clone()), initial(bars, will_backfill)));

    // ponytail: 补完就再也不拉新 —— 每日增量追加由 Cron / 另一条路负责，本票不做
    if !will_backfill {
        return;
    }
    let state = match backfill::run(store, client, symbol).await {
        Ok(_) => match store.adjusted_bars(symbol, *tf) {
            Ok(bars) => BarState::Ready(bars),
            Err(e) => BarState::Failed(e.to_string()),
        },
        // 回补失败时，已经画出来的本地数据比一屏错误信息有用，留着
        Err(_) if had_local => return,
        Err(e) => BarState::Failed(e.to_string()),
    };
    let _ = tx.send((Some(key.clone()), state));
}

/// 回补开始之前先给 UI 什么。**「回补中」只有真的要去补的时候才能说** ——
/// 补过了却一根都没有，那是「这只股票没有日线」，不是还在路上。
fn initial(bars: Vec<crate::core::bar::Bar>, will_backfill: bool) -> BarState {
    if bars.is_empty() && will_backfill {
        BarState::Backfilling
    } else {
        BarState::Ready(bars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bar::Bar;
    use crate::source::backfill::DONE;

    fn bar(ts: i64, close: f64) -> Bar {
        Bar {
            ts: chrono::DateTime::from_timestamp(ts, 0).unwrap(),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
        }
    }

    #[test]
    fn 回补中只在真的要去补的时候说() {
        assert!(matches!(initial(vec![], true), BarState::Backfilling));
        // 补过了还是一根都没有 → 是「没有数据」，不是「还在路上」
        assert!(matches!(initial(vec![], false), BarState::Ready(b) if b.is_empty()));
        assert!(matches!(initial(vec![bar(1, 1.0)], true), BarState::Ready(b) if b.len() == 1));
    }

    /// 本地已经补齐时一个请求都不该发 —— 真发了这个测试会卡住 7 秒以上
    #[tokio::test]
    async fn 本地有数据且已补齐时直接读本地不打网络() {
        let store = Store::open_in_memory().unwrap();
        let symbol = Symbol::parse("CN:600519").unwrap();
        let bars: Vec<Bar> = (0..3).map(|i| bar(1_700_000_000 + i * 86_400, 10.0)).collect();
        store.upsert_bars(&symbol, Timeframe::Day, &bars).unwrap();
        store
            .set_backfill_status(&symbol, Timeframe::Day, DONE)
            .unwrap();

        let client = HistoryClient::new().unwrap();
        let (tx, rx) = tokio::sync::watch::channel((None, BarState::Loading));
        let key = (symbol, Timeframe::Day);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            local(&store, &client, &key, &tx),
        )
        .await
        .expect("本地有数据就不该等网络");

        let (got_key, state) = rx.borrow().clone();
        assert_eq!(got_key, Some(key));
        match state {
            BarState::Ready(b) => assert_eq!(b.len(), 3, "应当把本地这 3 根原样交出去"),
            other => panic!("期望 Ready，实际 {other:?}"),
        }
    }
}
