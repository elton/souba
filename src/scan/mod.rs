//! 扫描编排：拉板块列表 → 落库 → 按热度排序 → 每个板块取候选 → 回补缺的历史 → 输出。
//!
//! 这一层保持薄：能测的东西都在 `core::sector`（热度）与 `store`（幂等落库）里，
//! 这里只负责把它们串起来和排版。
//!
//! 回补是长任务（首日约 160 只 × 7 秒 ≈ 19 分钟），这里同步等完；
//! 中途 Ctrl-C 不会丢进度，`backfill_state` 记着补到哪了，下次接着补。

use std::collections::HashSet;
use std::io::Write;

use unicode_width::UnicodeWidthStr;

use crate::core::bar::Timeframe;
use crate::core::sector::{Heat, Member, Sector, SectorKind, heat};
use crate::core::symbol::{Market, Symbol};
use crate::source::history::HistoryClient;
use crate::source::sina_sector::{Members, SectorSource};
use crate::source::backfill;
use crate::store::Store;

/// 扫描参数。默认值来自 spec 的「默认参数」一节。
pub use crate::settings::ScanParams;

#[derive(Debug, Clone)]
pub struct Ranked {
    pub sector: Sector,
    pub heat: Heat,
}

/// `souba scan`：行业与概念一起拉、一起落库、一起排榜。
///
/// 板块列表拉不到就整体失败 —— 榜单缺了一半不如不给。
pub async fn run_cli(store: &Store) -> anyhow::Result<()> {
    let p = crate::settings::Settings::load(store)?.scan;
    let date = today();
    let src = SectorSource::new()?;
    let mut all = fetch(&src, SectorKind::Industry).await?;
    all.extend(fetch(&src, SectorKind::Concept).await?);

    store.record_sectors(Market::Cn, &date, &all)?;
    let total = all.len();
    let ranked = rank(store, &date, all, &p)?;
    print!("{}", render(&date, total, &ranked, &p));

    // 每个热门板块取涨幅前 K 只做候选。一个板块拉不到不该让整次扫描白跑 ——
    // 板块列表已经落库了，其余板块的候选照常取。
    let mut by_sector = Vec::with_capacity(ranked.len());
    for r in &ranked {
        match src.members(&r.sector.code, p.k).await {
            Ok(got) => {
                store.record_members(Market::Cn, &r.sector.code, &date, &got.list)?;
                print!("{}", render_members(&r.sector.name, &got));
                by_sector.push(got.list);
            }
            Err(e) => println!("\n{}　候选拉取失败：{e}", r.sector.name),
        }
    }

    let q = queue(store, &by_sector)?;
    backfill_all(store, q).await
}

/// 逐只回补并打印进度。单只失败只记下来继续 —— 一只退市股拉不到历史，
/// 不该把后面一百多只一起废掉。
async fn backfill_all(store: &Store, q: Queue) -> anyhow::Result<()> {
    let client = HistoryClient::new()?;
    let total = q.pending.len();
    println!("\n候选 {} 只（去重后），其中 {} 只已有历史，需回补 {total} 只", q.ready + total, q.ready);
    let mut done = 0usize;
    let mut failed = Vec::new();
    for (i, m) in q.pending.iter().enumerate() {
        print!("回补 {}/{total}　{} {}　", i + 1, m.symbol, m.name);
        let _ = std::io::stdout().flush();
        match backfill::run(store, &client, &m.symbol).await {
            Ok(n) => {
                done += 1;
                println!("{n} 根");
            }
            Err(e) => {
                println!("失败：{e}");
                failed.push((m.clone(), e.to_string()));
            }
        }
    }
    println!(
        "\n回补汇总：已有历史 {} 只，本次补齐 {done} 只，失败 {} 只",
        q.ready,
        failed.len()
    );
    for (m, e) in &failed {
        println!("  {} {}　{e}", m.symbol, m.name);
    }
    Ok(())
}

/// 回补队列。
struct Queue {
    /// 还缺历史的候选，顺序 = 板块热度序 × 板块内名次，已按标的去重
    pending: Vec<Member>,
    /// 已有全量历史、不用补的候选只数（去重后）
    ready: usize,
}

/// 按「板块热度序 × 板块内名次」把候选摊平去重，滤掉已经补完的。
///
/// 去重是必需的：概念板块大量重叠，同一只出现在三四个板块里很常见，
/// 不去重就会对同一只重复打七秒一次的接口。
fn queue(store: &Store, by_sector: &[Vec<Member>]) -> anyhow::Result<Queue> {
    let mut seen: HashSet<Symbol> = HashSet::new();
    let mut pending = Vec::new();
    let mut ready = 0;
    for m in by_sector.iter().flatten() {
        if !seen.insert(m.symbol.clone()) {
            continue;
        }
        if backfill::needs(store, &m.symbol, Timeframe::Day)? {
            pending.push(m.clone());
        } else {
            ready += 1;
        }
    }
    Ok(Queue { pending, ready })
}

async fn fetch(src: &SectorSource, kind: SectorKind) -> anyhow::Result<Vec<Sector>> {
    src.list(kind)
        .await
        .map_err(|e| anyhow::anyhow!("{}板块列表拉取失败：{e}", kind.label()))
}

fn today() -> String {
    chrono::Utc::now()
        .with_timezone(&Market::Cn.timezone())
        .date_naive()
        .to_string()
}

/// 按热度降序取前 `p.sectors` 个。`date` 当天的快照不计入历史（`sector_history`
/// 只取严格早于它的行），所以先落库再排是安全的。
fn rank(
    store: &Store,
    date: &str,
    sectors: Vec<Sector>,
    p: &ScanParams,
) -> anyhow::Result<Vec<Ranked>> {
    let mut out = Vec::with_capacity(sectors.len());
    for s in sectors {
        let prev = store.sector_history(Market::Cn, &s.code, date, p.heat_days)?;
        let h = heat(&s.snapshot, &prev, p.w_turnover, p.w_change);
        out.push(Ranked { sector: s, heat: h });
    }
    // total_cmp 而不是 partial_cmp().unwrap() —— NaN 也要有确定的位置而不是 panic
    out.sort_by(|a, b| b.heat.heat.total_cmp(&a.heat.heat));
    out.truncate(p.sectors);
    Ok(out)
}

/// 板块名是中文，按显示格补齐而不是按字符数 —— 否则每行列位都会漂。
fn pad(s: &str, cells: usize) -> String {
    let w = s.width();
    format!("{s}{}", " ".repeat(cells.saturating_sub(w)))
}

fn render(date: &str, total: usize, ranked: &[Ranked], p: &ScanParams) -> String {
    let mut out = format!(
        "{date} A股板块热度榜　共 {total} 个板块，取前 {}（回看 {} 日，权重 成交额 {} / 涨幅 {}）\n\n",
        p.sectors, p.heat_days, p.w_turnover, p.w_change
    );
    out.push_str(&format!(
        "{}  {} {}  {}  {}  {}\n",
        pad("#", 2),
        pad("板块", 16),
        pad("类型", 4),
        pad("当日涨幅", 9),
        pad("量比", 7),
        pad(&format!("{}日涨幅", p.heat_days), 9),
    ));
    for (i, r) in ranked.iter().enumerate() {
        let ratio = match r.heat.turnover_ratio {
            // 首次运行没有基准，显示「—」而不是编一个 1.00 出来
            None => "—".to_string(),
            Some(v) => format!("{v:.2}x"),
        };
        out.push_str(&format!(
            "{}  {} {}  {}  {}  {}  热度 {:.2}\n",
            pad(&format!("{}", i + 1), 2),
            pad(&r.sector.name, 16),
            pad(r.sector.kind.label(), 4),
            pad(&format!("{:+.2}%", r.heat.today_change), 9),
            pad(&ratio, 7),
            pad(&format!("{:+.2}%", r.heat.cum_change), 9),
            r.heat.heat,
        ));
    }
    out
}

fn render_members(sector: &str, got: &Members) -> String {
    let skipped = match got.skipped {
        0 => String::new(),
        n => format!("，跳过 {n} 只代码无法识别的"),
    };
    let mut out = format!("\n{sector}　候选 {} 只{skipped}\n", got.list.len());
    for (i, m) in got.list.iter().enumerate() {
        out.push_str(&format!(
            "  {} {}  {}  {:+.2}%\n",
            pad(&format!("{}", i + 1), 3),
            pad(&m.symbol.to_string(), 10),
            pad(&m.name, 12),
            m.change_pct,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sector::Snapshot;

    fn sec(code: &str, name: &str, kind: SectorKind, change_pct: f64, turnover: f64) -> Sector {
        Sector {
            code: code.into(),
            name: name.into(),
            kind,
            snapshot: Snapshot {
                change_pct,
                turnover,
            },
        }
    }

    fn store_with(days: &[(&str, f64, f64)]) -> Store {
        let st = Store::open_in_memory().unwrap();
        for (d, c, t) in days {
            st.record_sectors(
                Market::Cn,
                d,
                &[sec("gn_a", "甲概念", SectorKind::Concept, *c, *t)],
            )
            .unwrap();
        }
        st
    }

    fn mem(code: &str, name: &str, change_pct: f64) -> Member {
        Member {
            symbol: Symbol::parse(code).unwrap(),
            name: name.into(),
            change_pct,
        }
    }

    #[test]
    fn 已有完整历史的不入队() {
        let st = Store::open_in_memory().unwrap();
        let done = Symbol::parse("CN:600519").unwrap();
        st.set_backfill_status(&done, Timeframe::Day, backfill::DONE).unwrap();
        // 补了一半的那只必须照样入队，否则重启后永远停在半截
        let half = Symbol::parse("CN:000001").unwrap();
        st.set_backfill_status(&half, Timeframe::Day, backfill::RUNNING).unwrap();

        let q = queue(
            &st,
            &[vec![
                mem("CN:600519", "贵州茅台", 1.0),
                mem("CN:000001", "平安银行", 2.0),
                mem("CN:300750", "宁德时代", 3.0),
            ]],
        )
        .unwrap();
        let got: Vec<String> = q.pending.iter().map(|m| m.symbol.to_string()).collect();
        assert_eq!(got, ["CN:000001", "CN:300750"]);
        assert_eq!(q.ready, 1);
    }

    #[test]
    fn 队列顺序是板块热度序乘板块内名次且跨板块去重() {
        let st = Store::open_in_memory().unwrap();
        let q = queue(
            &st,
            &[
                vec![mem("CN:600519", "甲", 9.0), mem("CN:000001", "乙", 8.0)],
                // 第二个板块跟第一个重叠一只，它已经排过了，不该再来一次
                vec![mem("CN:000001", "乙", 8.0), mem("CN:300750", "丙", 7.0)],
            ],
        )
        .unwrap();
        let got: Vec<String> = q.pending.iter().map(|m| m.symbol.to_string()).collect();
        assert_eq!(got, ["CN:600519", "CN:000001", "CN:300750"]);
        assert_eq!(q.ready, 0);
    }

    #[test]
    fn 候选行带代码名称与涨幅() {
        let got = Members {
            list: vec![mem("CN:002965", "祥鑫科技", 10.007), mem("CN:002786", "银宝山新", -1.5)],
            skipped: 2,
        };
        let text = render_members("华为汽车", &got);
        assert!(text.contains("华为汽车"), "{text}");
        assert!(text.contains("候选 2 只"), "{text}");
        assert!(text.contains("跳过 2 只"), "跳过的要说出来，不能静默：{text}");
        assert!(text.contains("CN:002965") && text.contains("祥鑫科技"), "{text}");
        assert!(text.contains("+10.01%") && text.contains("-1.50%"), "{text}");
    }

    #[test]
    fn 候选行中文名按显示格对齐() {
        let got = Members {
            list: vec![mem("CN:002965", "祥鑫科技", 1.0), mem("CN:002786", "银宝山新A", 2.0)],
            skipped: 0,
        };
        let text = render_members("华为汽车", &got);
        // 首行是空行、次行是板块标题，候选行从第 3 行起
        let cols: Vec<usize> = text
            .lines()
            .skip(2)
            .map(|l| l[..l.find('%').unwrap()].width())
            .collect();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0], cols[1], "涨幅列没对齐：\n{text}");
    }

    #[test]
    fn 默认参数是spec里的值() {
        let p = ScanParams::default();
        assert_eq!(p.sectors, 8);
        assert_eq!(p.heat_days, 5);
        assert_eq!(p.w_turnover, 1.0);
        assert_eq!(p.w_change, 1.0);
    }

    #[test]
    fn 按热度降序并截断到参数条数() {
        let st = Store::open_in_memory().unwrap();
        let p = ScanParams {
            sectors: 2,
            ..Default::default()
        };
        // 没有历史 → 热度就是当日涨幅
        let list = vec![
            sec("a", "低", SectorKind::Industry, 1.0, 100.0),
            sec("b", "高", SectorKind::Concept, 9.0, 100.0),
            sec("c", "中", SectorKind::Industry, 5.0, 100.0),
        ];
        let r = rank(&st, "2026-09-18", list, &p).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].sector.name, "高");
        assert_eq!(r[1].sector.name, "中");
    }

    #[test]
    fn 排序用的是库里的历史而不是只看当日() {
        // 甲概念前两天成交额 100，今天 1000 → 量比 10；乙概念没有历史
        let st = store_with(&[("2026-09-16", 0.0, 100.0), ("2026-09-17", 0.0, 100.0)]);
        let list = vec![
            sec("gn_a", "甲概念", SectorKind::Concept, 1.0, 1000.0),
            sec("gn_b", "乙概念", SectorKind::Concept, 5.0, 1000.0),
        ];
        let r = rank(&st, "2026-09-18", list, &ScanParams::default()).unwrap();
        assert_eq!(r[0].sector.name, "甲概念", "放量的应压过单日涨得多的");
        assert_eq!(r[0].heat.turnover_ratio, Some(10.0));
        assert!((r[0].heat.heat - 11.0).abs() < 1e-9, "{:?}", r[0].heat);
        assert_eq!(r[1].heat.turnover_ratio, None);
    }

    #[test]
    fn 当天已落库的快照不被算进自己的历史() {
        // 先把今天写进去，再排今天的榜 —— 量比不能因此变成 1.00
        let st = store_with(&[("2026-09-18", 1.0, 500.0)]);
        let list = vec![sec("gn_a", "甲概念", SectorKind::Concept, 1.0, 500.0)];
        let r = rank(&st, "2026-09-18", list, &ScanParams::default()).unwrap();
        assert_eq!(r[0].heat.turnover_ratio, None);
    }

    #[test]
    fn 输出带类型与三个热度分量() {
        let st = store_with(&[("2026-09-17", 2.0, 100.0)]);
        let list = vec![
            sec("gn_a", "甲概念", SectorKind::Concept, 3.0, 300.0),
            sec("new_b", "玻璃行业", SectorKind::Industry, 1.0, 100.0),
        ];
        let p = ScanParams::default();
        let r = rank(&st, "2026-09-18", list, &p).unwrap();
        let text = render("2026-09-18", 224, &r, &p);
        assert!(text.contains("共 224 个板块"), "{text}");
        assert!(text.contains("概念") && text.contains("行业"), "缺类型标注：{text}");
        assert!(text.contains("+3.00%"), "缺当日涨幅：{text}");
        assert!(text.contains("3.00x"), "缺成交额放大倍数：{text}");
        assert!(text.contains("+5.00%"), "缺 N 日累计涨幅：{text}");
        assert!(text.contains("—"), "没有基准的板块应显示「—」：{text}");
    }

    #[test]
    fn 中文板块名按显示格对齐() {
        // 板块名长度差一倍时，后面的列仍应对齐 —— 按字符数补齐会漂
        let st = Store::open_in_memory().unwrap();
        let list = vec![
            sec("a", "钙钛矿", SectorKind::Concept, 1.0, 100.0),
            sec("b", "有色金属采选", SectorKind::Industry, 0.5, 100.0),
        ];
        let p = ScanParams::default();
        let r = rank(&st, "2026-09-18", list, &p).unwrap();
        let text = render("2026-09-18", 2, &r, &p);
        let cols: Vec<usize> = text
            .lines()
            .skip(3)
            .map(|l| l.find("%").map(|_| l[..l.find('%').unwrap()].width()).unwrap())
            .collect();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0], cols[1], "涨幅列没对齐：\n{text}");
    }
}
