//! 板块与热度。
//!
//! 热度是纯函数：输入是一个板块的当日快照与它之前若干天的快照，输出热度值与
//! 三个可展示的分量。分量要单独给出来 —— 用户要能自己判断公式选得对不对，
//! 只给一个总分他没法判断。

use crate::core::symbol::Symbol;

/// 板块的一日快照。change_pct 是百分数（-0.96 表示跌 0.96%），turnover 是元。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    pub change_pct: f64,
    pub turnover: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectorKind {
    Industry,
    Concept,
}

impl SectorKind {
    /// 落库用的值
    pub fn as_str(self) -> &'static str {
        match self {
            SectorKind::Industry => "industry",
            SectorKind::Concept => "concept",
        }
    }

    /// 界面上显示的值
    pub fn label(self) -> &'static str {
        match self {
            SectorKind::Industry => "行业",
            SectorKind::Concept => "概念",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sector {
    pub code: String,
    pub name: String,
    pub kind: SectorKind,
    pub snapshot: Snapshot,
}

/// 板块内的一只成分股。名次不进结构体 —— 它是「在这一次抓取的这个板块里排第几」，
/// 属于落库时的上下文，不属于标的本身。
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub symbol: Symbol,
    pub name: String,
    /// 当日涨幅，百分数
    pub change_pct: f64,
}

/// 热度与它的三个分量。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Heat {
    pub heat: f64,
    /// 今日成交额 / 前 N 日均值。没有历史快照（或均值为 0）时为 None ——
    /// 此时不能编一个 1.0 出来充数，界面要显示「—」。
    pub turnover_ratio: Option<f64>,
    /// N 日累计涨幅，含今日
    pub cum_change: f64,
    pub today_change: f64,
}

/// `w_turnover × (今日成交额 / 前 N 日均值) + w_change × N 日累计涨幅（含今日）`
///
/// `prev` 是今日之前的快照，最多 N 条，顺序无关。历史不足就按实际条数算；
/// 一条都没有（首次运行）时放大倍数无从谈起，热度退化为当日涨幅。
pub fn heat(today: &Snapshot, prev: &[Snapshot], w_turnover: f64, w_change: f64) -> Heat {
    let cum_change = today.change_pct + prev.iter().map(|s| s.change_pct).sum::<f64>();
    let mean = prev.iter().map(|s| s.turnover).sum::<f64>() / prev.len().max(1) as f64;
    let turnover_ratio = (!prev.is_empty() && mean > 0.0).then(|| today.turnover / mean);
    Heat {
        heat: turnover_ratio.map_or(0.0, |r| w_turnover * r) + w_change * cum_change,
        turnover_ratio,
        cum_change,
        today_change: today.change_pct,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(change_pct: f64, turnover: f64) -> Snapshot {
        Snapshot {
            change_pct,
            turnover,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn 满五日历史按手算值() {
        // 前 5 日成交额均值 = (100+200+300+400+500)/5 = 300，今日 600 → 放大 2.0
        // 累计涨幅 = 2.0 + (1.0-0.5+2.0+0.5+1.0) = 6.0
        // 热度 = 1.0×2.0 + 1.0×6.0 = 8.0
        let prev = [
            snap(1.0, 100.0),
            snap(-0.5, 200.0),
            snap(2.0, 300.0),
            snap(0.5, 400.0),
            snap(1.0, 500.0),
        ];
        let h = heat(&snap(2.0, 600.0), &prev, 1.0, 1.0);
        assert_eq!(h.turnover_ratio, Some(2.0));
        assert!(close(h.cum_change, 6.0), "{h:?}");
        assert!(close(h.heat, 8.0), "{h:?}");
        assert!(close(h.today_change, 2.0));
    }

    #[test]
    fn 权重按比例缩放两项() {
        let prev = [
            snap(1.0, 100.0),
            snap(-0.5, 200.0),
            snap(2.0, 300.0),
            snap(0.5, 400.0),
            snap(1.0, 500.0),
        ];
        // 2.0×2.0 + 0.5×6.0 = 7.0
        let h = heat(&snap(2.0, 600.0), &prev, 2.0, 0.5);
        assert!(close(h.heat, 7.0), "{h:?}");
    }

    #[test]
    fn 历史不足n日按实际天数算() {
        // 只有 2 天：均值 (100+300)/2 = 200，今日 500 → 2.5
        // 累计涨幅 = 1.5 + 1.0 + 2.0 = 4.5；热度 = 2.5 + 4.5 = 7.0
        let prev = [snap(1.0, 100.0), snap(2.0, 300.0)];
        let h = heat(&snap(1.5, 500.0), &prev, 1.0, 1.0);
        assert_eq!(h.turnover_ratio, Some(2.5));
        assert!(close(h.cum_change, 4.5), "{h:?}");
        assert!(close(h.heat, 7.0), "{h:?}");
    }

    #[test]
    fn 零日历史退化为当日涨幅() {
        let h = heat(&snap(3.0, 900.0), &[], 1.0, 1.0);
        assert_eq!(h.turnover_ratio, None, "没有基准就不该编一个放大倍数出来");
        assert!(close(h.cum_change, 3.0));
        assert!(close(h.heat, 3.0), "{h:?}");
    }

    #[test]
    fn 零日历史时涨幅权重仍生效() {
        let h = heat(&snap(3.0, 900.0), &[], 5.0, 2.0);
        assert!(close(h.heat, 6.0), "成交额项应整体缺席，而不是按 1.0 计入：{h:?}");
    }

    #[test]
    fn 历史成交额全为零时不产生无穷大() {
        let h = heat(&snap(2.0, 100.0), &[snap(1.0, 0.0)], 1.0, 1.0);
        assert_eq!(h.turnover_ratio, None);
        assert!(h.heat.is_finite());
        assert!(close(h.heat, 3.0));
    }
}
