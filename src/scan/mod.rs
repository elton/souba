//! 扫描编排：拉板块列表 → 落库 → 按热度排序 → 输出。
//!
//! 这一层保持薄：能测的东西都在 `core::sector`（热度）与 `store`（幂等落库）里，
//! 这里只负责把它们串起来和排版。

use unicode_width::UnicodeWidthStr;

use crate::core::sector::{Heat, Sector, SectorKind, heat};
use crate::core::symbol::Market;
use crate::source::sina_sector::SectorSource;
use crate::store::Store;

/// 扫描参数。默认值来自 spec 的「默认参数」一节。
/// 下一块会改成从 `settings` 表装载，现在先是纯粹的默认值。
#[derive(Debug, Clone)]
pub struct ScanParams {
    /// 榜单取前几个板块
    pub sectors: usize,
    /// 热度回看天数
    pub heat_days: usize,
    pub w_turnover: f64,
    pub w_change: f64,
}

impl Default for ScanParams {
    fn default() -> Self {
        Self {
            sectors: 8,
            heat_days: 5,
            w_turnover: 1.0,
            w_change: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Ranked {
    pub sector: Sector,
    pub heat: Heat,
}

/// `souba scan`：行业与概念一起拉、一起落库、一起排榜。
///
/// 板块列表拉不到就整体失败 —— 榜单缺了一半不如不给。
pub async fn run_cli(store: &Store) -> anyhow::Result<()> {
    let p = ScanParams::default();
    let date = today();
    let src = SectorSource::new()?;
    let mut all = fetch(&src, SectorKind::Industry).await?;
    all.extend(fetch(&src, SectorKind::Concept).await?);

    store.record_sectors(Market::Cn, &date, &all)?;
    let total = all.len();
    let ranked = rank(store, &date, all, &p)?;
    print!("{}", render(&date, total, &ranked, &p));
    Ok(())
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
