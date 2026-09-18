//! 候选求值、板块内排序、落 `scan_results`。
//!
//! 这一整段**不联网**：输入是库里已经落好的候选（`sector_members`）与本地复权日线，
//! 输出是 `scan_results` 的内容与一段可打印的文本。所以它能在内存库上整段测试，
//! 而 `run_cli` 那层只剩「拉数据 → 调这里 → 打印」。
//!
//! 扫描是盘后离线求值，喂给策略的 `quote` 一律是 `None` —— 盘口那一路数据这里没有，
//! 硬塞一个当天的快照进去只会让「昨天收盘算出来的信号」和「今天盘中算出来的」对不上。

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::core::bar::{Bar, Timeframe};
use crate::core::strategy::vegas::{READY_FACET, Vegas, VegasParams};
use crate::core::strategy::{FacetState, MarketData, Signal, Stance, Strategy};
use crate::core::symbol::{Market, Symbol};
use crate::settings::{ScanParams, Settings};
use crate::source::backfill;
use crate::store::{ScanRow, Store};

/// 「还在回补历史」落库时的 `stance`。它**不是** `Stance` 的取值 ——
/// 「数据不足」是「补完了但根数仍然不够，结论不可信」，「回补中」是「还没补，等一会儿就有」，
/// 对用户意味着完全不同的事，机会面板据此把两者分开显示。
///
/// 配套约定：回补中的行 `freshness` 为 NULL、`facets_json` 为空数组。
pub const BACKFILLING: &str = "backfilling";

/// 一只候选的求值结果，板块内排序的输入。
#[derive(Debug, Clone)]
pub struct Scored {
    pub symbol: Symbol,
    pub name: String,
    pub signal: Signal,
}

/// 进入板块 top 的一只。
#[derive(Debug, Clone)]
pub struct Pick {
    pub symbol: Symbol,
    pub name: String,
    /// 还在回补的候选没有可信求值结果，此时为 `None`
    pub signal: Option<Signal>,
    /// 「预备」补位进来的（`stance` 是 `Watch` 而不是 `Long`）
    pub filler: bool,
}

#[derive(Debug, Clone)]
pub struct SectorPicks {
    pub name: String,
    pub picks: Vec<Pick>,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub sectors: Vec<SectorPicks>,
    /// 求值失败的（代码, 原因）。单只失败不中断整次扫描，但要在摘要里说出来。
    pub failed: Vec<(String, String)>,
}

/// 板块内排序：从 `Long` 里按「趋势确立的新鲜度」取前 `top`，不够就用
/// 「预备」为真的 `Watch` 补位并标注。纯函数 —— 落库与打印吃的是同一份输出。
///
/// 两级排序（窗口内优先，再按根数升序）是 spec 的措辞。新鲜度就是根数，所以
/// 这两级实际上等价于直接按根数升序；仍然照两级写，免得日后窗口的含义变了看不出来。
/// 排序是稳定的：新鲜度相同的保持传入次序，也就是板块内涨幅名次。
pub fn pick_top(scored: Vec<Scored>, top: usize, fresh_window: usize) -> Vec<Pick> {
    let mut long = Vec::new();
    let mut ready = Vec::new();
    for s in scored {
        match s.signal.stance {
            Stance::Long => long.push(s),
            Stance::Watch if is_ready(&s.signal) => ready.push(s),
            _ => {}
        }
    }
    long.sort_by_key(|s| {
        // Long 必然带新鲜度（主判定里「确认」为真才可能是 Long），
        // 真出现 None 也不该 panic —— 排到最后去就是了
        let n = s.signal.fresh_bars.unwrap_or(usize::MAX);
        (n > fresh_window, n)
    });

    let mut out: Vec<Pick> = long.into_iter().map(|s| pick(s, false)).collect();
    out.truncate(top);
    for s in ready {
        if out.len() >= top {
            break;
        }
        out.push(pick(s, true));
    }
    out
}

fn pick(s: Scored, filler: bool) -> Pick {
    Pick {
        symbol: s.symbol,
        name: s.name,
        signal: Some(s.signal),
        filler,
    }
}

/// 「预备」维度看多。补位只看这一条 —— 主判定已经是 `Watch` 了。
fn is_ready(s: &Signal) -> bool {
    s.facets
        .iter()
        .any(|f| f.label == READY_FACET && f.state == FacetState::Bullish)
}

/// 求值 → 排序 → 落库。`sectors` 是 (板块码, 板块名)，按热度序。
///
/// 全部板块算完才写库，写库自己是「先清当日再写」的一次事务。所以求值阶段任何一步
/// 出错都会在写库之前 bail，库里上一次的结果原样保留 —— 板块列表拉不到时整次扫描
/// 直接 bail，连这个函数都不会被调用，昨天的结果自然还在。
pub fn run(
    store: &Store,
    market: Market,
    date: &str,
    sectors: &[(String, String)],
    cfg: &Settings,
) -> anyhow::Result<Report> {
    let mut report = Report::default();
    let mut rows = Vec::new();
    for (code, name) in sectors {
        let mut scored = Vec::new();
        let mut backfilling = Vec::new();
        for (_, symbol, member_name) in store.sector_members(market, code, date)? {
            match evaluate_one(store, &symbol, cfg.vegas) {
                Ok(Some(signal)) => scored.push(Scored {
                    symbol: Symbol::parse(&symbol)?,
                    name: member_name,
                    signal,
                }),
                Ok(None) => backfilling.push((symbol, member_name)),
                // 一只读库读炸了、bar 序列有问题，不该把整个板块连坐
                Err(e) => report.failed.push((symbol, e.to_string())),
            }
        }

        let mut picks = pick_top(scored, cfg.scan.top, cfg.vegas.fresh_window);
        // 回补中的排在最后补满剩下的位子：首日全都在回补，面板得先有东西看，
        // 补完之后下一次求值自然把它们换成真的结论（story 34）
        for (symbol, member_name) in backfilling {
            if picks.len() >= cfg.scan.top {
                break;
            }
            picks.push(Pick {
                symbol: Symbol::parse(&symbol)?,
                name: member_name,
                signal: None,
                filler: false,
            });
        }

        for (i, p) in picks.iter().enumerate() {
            rows.push(row(code, i + 1, p));
        }
        report.sectors.push(SectorPicks {
            name: name.clone(),
            picks,
        });
    }
    store.record_scan_results(market, date, &rows)?;
    Ok(report)
}

/// 一只候选的求值。还在回补（或者本地一根日线都没有）时返回 `None`。
fn evaluate_one(store: &Store, symbol: &str, params: VegasParams) -> anyhow::Result<Option<Signal>> {
    let sym = Symbol::parse(symbol)?;
    if backfill::needs(store, &sym, Timeframe::Day)? {
        return Ok(None);
    }
    let bars = store.adjusted_bars(&sym, Timeframe::Day)?;
    if bars.is_empty() {
        return Ok(None);
    }
    Ok(Some(day_signal(&sym, bars, params)))
}

/// 在日线上跑一次 Vegas。只装日线 —— 周线由 `Vegas` 自己从日线聚合，
/// 两边各装一份只会引入「日线与周线不同步」这种查都难查的偏差。
fn day_signal(symbol: &Symbol, bars: Vec<Bar>, params: VegasParams) -> Signal {
    let mut loaded = HashMap::new();
    loaded.insert(Timeframe::Day, bars);
    Vegas { params }.evaluate(&MarketData::new(symbol, None, &loaded))
}

/// 落库行。补位的「预备」不另加列：`scan_results` 里 `stance = watch` 的行
/// 按构造就是补位来的（top 只从 `Long` 里选），面板据此标注。
fn row(sector_code: &str, rank: usize, p: &Pick) -> ScanRow {
    let (stance, freshness, facets_json) = match &p.signal {
        None => (BACKFILLING.to_string(), None, "[]".to_string()),
        Some(s) => (
            s.stance.as_str().to_string(),
            // schema 的约定：freshness 仅 Long 有意义
            (s.stance == Stance::Long)
                .then_some(s.fresh_bars)
                .flatten()
                .map(|n| n as i64),
            facets_json(s),
        ),
    };
    ScanRow {
        sector_code: sector_code.to_string(),
        symbol: p.symbol.clone(),
        rank,
        stance,
        freshness,
        facets_json,
    }
}

/// `Signal.facets` 的 JSON。`state` 直接存显示用的箭头 —— 面板要的就是
/// 「维度名 + 箭头」，存成箭头省掉一次枚举往返。`detail` 是喂给大模型的那份理由。
fn facets_json(s: &Signal) -> String {
    let v: Vec<Value> = s
        .facets
        .iter()
        .map(|f| {
            json!({
                "label": f.label,
                "state": f.state.glyph(),
                "detail": f.detail,
            })
        })
        .collect();
    serde_json::to_string(&v).expect("facets 的 JSON 化不会失败")
}

/// 主判定的中文文案。回补中不是 `Stance` 的取值，所以在这里合流。
fn stance_label(p: &Pick) -> String {
    match &p.signal {
        None => "回补中".to_string(),
        Some(s) if p.filler => format!("{}（{READY_FACET}）", s.stance.label()),
        Some(s) => s.stance.label().to_string(),
    }
}

fn freshness_label(p: &Pick) -> String {
    match p.signal.as_ref().and_then(|s| s.fresh_bars) {
        Some(n) => format!("{n} 根前确立"),
        None => "—".to_string(),
    }
}

/// 五维度状态缩写，沿用详情屏信号行的写法（维度名 + 箭头）。
fn facets_label(p: &Pick) -> String {
    match &p.signal {
        None => String::new(),
        Some(s) => s
            .facets
            .iter()
            .map(|f| format!("{}{}", f.label, f.state.glyph()))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

pub fn render(report: &Report, p: &ScanParams) -> String {
    let mut out = format!("\n板块 top {}\n", p.top);
    for s in &report.sectors {
        out.push_str(&format!("\n{}\n", s.name));
        if s.picks.is_empty() {
            out.push_str("  没有符合条件的候选\n");
            continue;
        }
        for (i, pick) in s.picks.iter().enumerate() {
            out.push_str(&format!(
                "  {} {}  {}  {}  {}  {}\n",
                super::pad(&format!("{}", i + 1), 3),
                super::pad(&pick.symbol.to_string(), 10),
                super::pad(&pick.name, 12),
                super::pad(&stance_label(pick), 14),
                super::pad(&freshness_label(pick), 12),
                facets_label(pick),
            ));
        }
    }
    if !report.failed.is_empty() {
        out.push_str(&format!("\n求值失败 {} 只\n", report.failed.len()));
        for (symbol, e) in &report.failed {
            out.push_str(&format!("  {symbol}　{e}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sector::Member;
    use crate::core::strategy::{Adequacy, Facet};
    use chrono::{Duration, TimeZone, Utc};

    fn sym(s: &str) -> Symbol {
        Symbol::parse(s).unwrap()
    }

    fn facets(ready: FacetState) -> Vec<Facet> {
        vec![Facet {
            label: READY_FACET.into(),
            state: ready,
            detail: "测试用".into(),
        }]
    }

    fn scored(code: &str, stance: Stance, fresh: Option<usize>, ready: FacetState) -> Scored {
        Scored {
            symbol: sym(code),
            name: code.into(),
            signal: Signal {
                stance,
                facets: facets(ready),
                adequacy: Adequacy {
                    have: 2000,
                    need: 1330,
                },
                fresh_bars: fresh,
            },
        }
    }

    fn codes(picks: &[Pick]) -> Vec<String> {
        picks.iter().map(|p| p.symbol.to_string()).collect()
    }

    // ── 排序（手算对照）

    #[test]
    fn 全是long时按新鲜度升序取前top() {
        let got = pick_top(
            vec![
                scored("CN:600000", Stance::Long, Some(40), FacetState::Neutral),
                scored("CN:600001", Stance::Long, Some(1), FacetState::Neutral),
                scored("CN:600002", Stance::Long, Some(12), FacetState::Neutral),
                scored("CN:600003", Stance::Long, Some(3), FacetState::Neutral),
            ],
            3,
            3,
        );
        // 窗口内的 1、3 排最前，其余按根数升序 12 < 40，取前 3
        assert_eq!(codes(&got), ["CN:600001", "CN:600003", "CN:600002"]);
        assert!(got.iter().all(|p| !p.filler), "全是 Long 时不该有补位");
    }

    #[test]
    fn 新鲜度相同时保持板块内名次() {
        let got = pick_top(
            vec![
                scored("CN:600000", Stance::Long, Some(5), FacetState::Neutral),
                scored("CN:600001", Stance::Long, Some(5), FacetState::Neutral),
            ],
            5,
            3,
        );
        assert_eq!(codes(&got), ["CN:600000", "CN:600001"]);
    }

    #[test]
    fn long不够时用预备为真的watch补位并标注() {
        let got = pick_top(
            vec![
                scored("CN:600000", Stance::Long, Some(9), FacetState::Neutral),
                // 预备为中性的 Watch 不该进来
                scored("CN:600001", Stance::Watch, None, FacetState::Neutral),
                scored("CN:600002", Stance::Watch, None, FacetState::Bullish),
                scored("CN:600003", Stance::Watch, None, FacetState::Bullish),
                // 离场与数据不足一律不进
                scored("CN:600004", Stance::Exit, None, FacetState::Bullish),
                scored("CN:600005", Stance::Insufficient, None, FacetState::Bullish),
            ],
            5,
            3,
        );
        assert_eq!(codes(&got), ["CN:600000", "CN:600002", "CN:600003"]);
        assert_eq!(
            got.iter().map(|p| p.filler).collect::<Vec<_>>(),
            [false, true, true],
            "补位的要能被认出来"
        );
    }

    #[test]
    fn 补位不会把结果撑过top() {
        let got = pick_top(
            vec![
                scored("CN:600000", Stance::Long, Some(1), FacetState::Neutral),
                scored("CN:600001", Stance::Watch, None, FacetState::Bullish),
                scored("CN:600002", Stance::Watch, None, FacetState::Bullish),
            ],
            2,
            3,
        );
        assert_eq!(codes(&got), ["CN:600000", "CN:600001"]);
    }

    #[test]
    fn 全是数据不足时一只都不选() {
        let got = pick_top(
            vec![
                scored("CN:600000", Stance::Insufficient, None, FacetState::Bullish),
                scored("CN:600001", Stance::Insufficient, None, FacetState::Bullish),
            ],
            5,
            3,
        );
        assert!(got.is_empty(), "数据不足不是「预备」，不能拿来充数");
    }

    // ── 端到端：内存库 + 合成日线

    /// 长期常数后突变，与 vegas 的测试同款：常数段让所有 EMA 收敛到同一个值，
    /// 突变段的快慢差异完全可控。
    fn step(n_before: usize, before: f64, n_after: usize, after: f64) -> Vec<Bar> {
        let t0 = Utc.with_ymd_and_hms(2018, 1, 1, 0, 0, 0).unwrap();
        let mut v = vec![before; n_before];
        v.extend(std::iter::repeat_n(after, n_after));
        v.iter()
            .enumerate()
            .map(|(i, c)| Bar {
                ts: t0 + Duration::days(i as i64),
                open: *c,
                high: *c,
                low: *c,
                close: *c,
                volume: 1.0,
            })
            .collect()
    }

    fn mem(code: &str, name: &str) -> Member {
        Member {
            symbol: sym(code),
            name: name.into(),
            change_pct: 1.0,
        }
    }

    /// 装一只补完历史的候选。`bars` 决定它算出来是 Long / Watch / Insufficient。
    fn seed(st: &Store, code: &str, bars: Vec<Bar>) {
        let s = sym(code);
        st.upsert_bars(&s, Timeframe::Day, &bars).unwrap();
        st.set_backfill_status(&s, Timeframe::Day, backfill::DONE)
            .unwrap();
    }

    const DATE: &str = "2026-09-18";

    fn sectors() -> Vec<(String, String)> {
        vec![("gn_a".into(), "甲概念".into())]
    }

    fn cfg(top: usize) -> Settings {
        Settings {
            scan: ScanParams {
                top,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn 端到端求值排序落库() {
        let st = Store::open_in_memory().unwrap();
        st.record_members(
            Market::Cn,
            "gn_a",
            DATE,
            &[
                mem("CN:600000", "做多甲"),
                mem("CN:600001", "横盘乙"),
                mem("CN:600002", "做多丙"),
                mem("CN:600003", "回补丁"),
            ],
        )
        .unwrap();
        // 甲：1400 根横盘后跳高 200 根 → Long，确立于 200 根之前
        seed(&st, "CN:600000", step(1400, 100.0, 200, 200.0));
        // 乙：全程横盘 → Watch 且「预备」为真（价格贴在隧道里）
        seed(&st, "CN:600001", step(1600, 100.0, 0, 100.0));
        // 丙：跳高之后只走了 5 根 → 同样 Long，但比甲新鲜得多
        seed(&st, "CN:600002", step(1400, 100.0, 5, 200.0));
        // 丁：没打 done，还在回补

        let report = run(&st, Market::Cn, DATE, &sectors(), &cfg(5)).unwrap();

        let picks = &report.sectors[0].picks;
        assert_eq!(codes(picks), ["CN:600002", "CN:600000", "CN:600001", "CN:600003"]);
        assert_eq!(picks[0].signal.as_ref().unwrap().stance, Stance::Long);
        assert!(picks[2].filler, "横盘那只是「预备」补位进来的");
        assert!(picks[3].signal.is_none(), "回补中的没有求值结果");
        assert!(report.failed.is_empty(), "{:?}", report.failed);

        let rows = st.scan_results(Market::Cn, DATE).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter().map(|r| r.rank).collect::<Vec<_>>(),
            [1, 2, 3, 4],
            "名次要按最终排序重新编"
        );
        assert_eq!(rows[0].symbol, sym("CN:600002"));
        assert_eq!(rows[0].stance, "long");
        // 跳高那根之后 EMA12 要隔一根才站上快隧道上沿，所以 5 根走完确立于 4 根前
        assert_eq!(rows[0].freshness, Some(4));
        assert_eq!(rows[1].freshness, Some(199));
        assert_eq!(rows[2].stance, "watch");
        assert_eq!(rows[2].freshness, None, "freshness 只有 Long 有意义");
        assert_eq!(rows[3].stance, BACKFILLING, "回补中要能跟数据不足分开");
        assert_eq!(rows[3].freshness, None);
        assert_eq!(rows[3].facets_json, "[]");

        let facets: Vec<Value> = serde_json::from_str(&rows[0].facets_json).unwrap();
        assert_eq!(facets.len(), 5, "五个维度都要落进去");
        assert_eq!(facets[0]["label"], "趋势");
        assert_eq!(facets[0]["state"], "↑");
        assert!(!facets[0]["detail"].as_str().unwrap().is_empty());
    }

    #[test]
    fn 回补中与数据不足是两种不同的落库值() {
        let st = Store::open_in_memory().unwrap();
        st.record_members(
            Market::Cn,
            "gn_a",
            DATE,
            &[mem("CN:600000", "补完但太短"), mem("CN:600001", "还在补")],
        )
        .unwrap();
        // 补完了，但只有 100 根 —— 慢隧道不可信，是「数据不足」
        seed(&st, "CN:600000", step(100, 100.0, 0, 100.0));
        // 打了 done 但库里一根都没有，也算还在补
        st.set_backfill_status(&sym("CN:600001"), Timeframe::Day, backfill::DONE)
            .unwrap();

        run(&st, Market::Cn, DATE, &sectors(), &cfg(5)).unwrap();
        let rows = st.scan_results(Market::Cn, DATE).unwrap();
        // 数据不足的进不了 top，只有回补中的那只补位
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol, sym("CN:600001"));
        assert_eq!(rows[0].stance, BACKFILLING);
        assert_ne!(rows[0].stance, Stance::Insufficient.as_str());
    }

    #[test]
    fn 单只失败只记账不中断整个板块() {
        let st = Store::open_in_memory().unwrap();
        st.record_members(Market::Cn, "gn_a", DATE, &[mem("CN:600000", "正常")])
            .unwrap();
        seed(&st, "CN:600000", step(1400, 100.0, 5, 200.0));
        // record_members 不收无法解析的代码，直接往库里塞一条坏的
        st.exec(
            "INSERT INTO sector_members (market, sector_code, symbol, name, as_of, rank)
             VALUES ('CN', 'gn_a', '这不是代码', '坏数据', '2026-09-18', 2)",
        )
        .unwrap();

        let report = run(&st, Market::Cn, DATE, &sectors(), &cfg(5)).unwrap();
        assert_eq!(codes(&report.sectors[0].picks), ["CN:600000"]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "这不是代码");
        assert_eq!(st.scan_results(Market::Cn, DATE).unwrap().len(), 1);
    }

    #[test]
    fn 求值出错时不清掉库里已有的当日结果() {
        let st = Store::open_in_memory().unwrap();
        st.record_scan_results(
            Market::Cn,
            DATE,
            &[ScanRow {
                sector_code: "gn_a".into(),
                symbol: sym("CN:600519"),
                rank: 1,
                stance: "long".into(),
                freshness: Some(2),
                facets_json: "[]".into(),
            }],
        )
        .unwrap();
        // rank 写成文本 —— 读候选那一步就会炸，写库那一步根本走不到
        st.exec(
            "INSERT INTO sector_members (market, sector_code, symbol, name, as_of, rank)
             VALUES ('CN', 'gn_a', 'CN:600000', '甲', '2026-09-18', 'x')",
        )
        .unwrap();

        assert!(run(&st, Market::Cn, DATE, &sectors(), &cfg(5)).is_err());
        let rows = st.scan_results(Market::Cn, DATE).unwrap();
        assert_eq!(rows.len(), 1, "清当日只能发生在求值成功之后");
        assert_eq!(rows[0].symbol, sym("CN:600519"));
    }

    // ── 打印

    fn report_of(st: &Store) -> Report {
        run(st, Market::Cn, DATE, &sectors(), &cfg(5)).unwrap()
    }

    fn rendered() -> String {
        let st = Store::open_in_memory().unwrap();
        st.record_members(
            Market::Cn,
            "gn_a",
            DATE,
            &[
                mem("CN:600000", "做多甲"),
                mem("CN:600001", "横盘乙"),
                mem("CN:600002", "回补丙"),
            ],
        )
        .unwrap();
        seed(&st, "CN:600000", step(1400, 100.0, 5, 200.0));
        seed(&st, "CN:600001", step(1600, 100.0, 0, 100.0));
        render(&report_of(&st), &ScanParams::default())
    }

    #[test]
    fn 打印带板块名次代码名称主判定新鲜度与五维度() {
        let text = rendered();
        assert!(text.contains("甲概念"), "{text}");
        assert!(text.contains("CN:600000") && text.contains("做多甲"), "{text}");
        assert!(text.contains("做多"), "{text}");
        assert!(text.contains("4 根前确立"), "{text}");
        for label in ["趋势", "共振", "位置", "确认", "预备"] {
            assert!(text.contains(label), "缺「{label}」维度：{text}");
        }
        assert!(text.contains("↑"), "缺状态缩写：{text}");
    }

    #[test]
    fn 打印把补位与回补中标出来() {
        let text = rendered();
        assert!(text.contains("观望（预备）"), "补位的要标「预备」：{text}");
        assert!(text.contains("回补中"), "回补中的要说出来：{text}");
        assert!(text.contains("—"), "没有新鲜度的显示「—」：{text}");
    }

    #[test]
    fn 打印的中文名按显示格对齐() {
        let text = rendered();
        let cols: Vec<usize> = text
            .lines()
            .filter(|l| l.contains("CN:"))
            .map(|l| {
                let at = l.find("CN:").unwrap();
                unicode_width::UnicodeWidthStr::width(&l[..at])
            })
            .collect();
        assert_eq!(cols.len(), 3);
        assert!(cols.iter().all(|c| *c == cols[0]), "代码列没对齐：\n{text}");
    }

    #[test]
    fn 一个候选都没有的板块说出来而不是留白() {
        let st = Store::open_in_memory().unwrap();
        let text = render(&report_of(&st), &ScanParams::default());
        assert!(text.contains("甲概念"), "{text}");
        assert!(text.contains("没有符合条件的候选"), "{text}");
    }

    #[test]
    fn 求值失败的在摘要里列出() {
        let report = Report {
            sectors: Vec::new(),
            failed: vec![("CN:600000".into(), "读库失败".into())],
        };
        let text = render(&report, &ScanParams::default());
        assert!(text.contains("求值失败 1 只"), "{text}");
        assert!(text.contains("CN:600000") && text.contains("读库失败"), "{text}");
    }
}
