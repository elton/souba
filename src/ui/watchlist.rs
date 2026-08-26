use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::core::quote::Freshness;
use crate::ui::layout::Breakpoint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKey {
    Code,
    Name,
    Last,
    ChangePct,
    Change,
    Freshness,
}

#[derive(Debug, Clone, Copy)]
pub struct Column {
    pub key: ColumnKey,
    pub title: &'static str,
    pub cells: u16,
}

/// 列优先级：代码 > 名称 > 现价 > 涨幅 > 涨跌额 > 新鲜度。
/// 窄屏从右往左砍 —— 代码和现价在任何断点下都必须保留。
const ALL: &[Column] = &[
    Column { key: ColumnKey::Code,      title: "代码",   cells: 10 },
    Column { key: ColumnKey::Name,      title: "名称",   cells: 14 },
    Column { key: ColumnKey::Last,      title: "现价",   cells: 11 },
    Column { key: ColumnKey::ChangePct, title: "涨幅",   cells: 9 },
    Column { key: ColumnKey::Change,    title: "涨跌",   cells: 10 },
    Column { key: ColumnKey::Freshness, title: "行情",   cells: 10 },
];

pub fn columns_for(bp: Breakpoint) -> &'static [Column] {
    match bp {
        Breakpoint::Wide => ALL,
        Breakpoint::Medium => &ALL[..4],
        Breakpoint::Narrow | Breakpoint::TooSmall => &ALL[..3],
    }
}

/// 按**显示格**截断并右侧补空格到恰好 max_cells 宽。
///
/// 不能用 chars().take() ——「贵州茅台」是 4 个码点却占 8 个显示格，
/// 按码点算会让表格每一行都错位。这是正确性问题不是美观问题。
pub fn truncate_display(s: &str, max_cells: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > max_cells {
            break;
        }
        out.push(ch);
        used += w;
    }
    // 宽字符放不进最后一格时会剩空隙，补空格保证列宽严格相等
    for _ in used..max_cells {
        out.push(' ');
    }
    debug_assert_eq!(out.width(), max_cells);
    out
}

/// 新鲜度的显示文案。
/// 「休市」与「延迟」必须区分 —— 午休和收盘后数据本来就不更新，报成延迟是误导。
pub fn freshness_label(f: Freshness) -> String {
    match f {
        Freshness::Live => "实时".to_string(),
        Freshness::Halted => "休市".to_string(),
        Freshness::Delayed(d) => {
            let secs = d.num_seconds().max(0);
            if secs < 60 {
                format!("延迟{secs}秒")
            } else {
                format!("延迟{}分", secs / 60)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn 中文按显示格计算宽度() {
        // 「贵州茅台」是 4 个码点但占 8 个显示格 —— 按码点算会让每一行都错位
        assert_eq!("贵州茅台".chars().count(), 4);
        assert_eq!("贵州茅台".width(), 8);
    }

    #[test]
    fn 截断按显示格不按码点() {
        assert_eq!(truncate_display("贵州茅台", 8), "贵州茅台");
        // 6 格放得下 3 个汉字
        assert_eq!(truncate_display("贵州茅台", 6).width(), 6);
        // 5 格放不下第 3 个汉字，只放 2 个（4 格），剩 1 格补空格
        assert_eq!(truncate_display("贵州茅台", 5).width(), 5);
    }

    #[test]
    fn 截断结果永远等于目标宽度() {
        for s in [
            "贵州茅台",
            "Toyota Motor Corp.",
            "腾讯控股",
            "AAPL",
            "",
            "日経225",
        ] {
            for max in [4usize, 6, 8, 10, 20] {
                assert_eq!(
                    truncate_display(s, max).width(),
                    max,
                    "{s:?} 截到 {max} 格时宽度不对"
                );
            }
        }
    }

    #[test]
    fn 不切断半个汉字() {
        let out = truncate_display("贵州茅台", 5);
        assert!(!out.contains('\u{fffd}'));
        assert!(out.starts_with("贵州"));
    }

    #[test]
    fn 宽屏显示全部列() {
        assert_eq!(columns_for(Breakpoint::Wide).len(), 6);
    }

    #[test]
    fn 窄屏从右往左砍列() {
        let wide = columns_for(Breakpoint::Wide);
        let narrow = columns_for(Breakpoint::Narrow);
        assert!(narrow.len() < wide.len());
        // 代码和现价在任何断点下都必须保留
        assert!(narrow.iter().any(|c| c.key == ColumnKey::Code));
        assert!(narrow.iter().any(|c| c.key == ColumnKey::Last));
    }

    #[test]
    fn 实时显示为实时() {
        assert_eq!(freshness_label(Freshness::Live), "实时");
    }

    #[test]
    fn 休市显示为休市而不是延迟() {
        // 午休和收盘后数据本来就不更新，报成延迟是误导
        assert_eq!(freshness_label(Freshness::Halted), "休市");
    }

    #[test]
    fn 延迟显示具体分钟数() {
        assert_eq!(freshness_label(Freshness::Delayed(Duration::seconds(45))), "延迟45秒");
        assert_eq!(freshness_label(Freshness::Delayed(Duration::seconds(900))), "延迟15分");
        assert_eq!(freshness_label(Freshness::Delayed(Duration::seconds(1140))), "延迟19分");
    }
}
