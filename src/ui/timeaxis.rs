//! K 线的横轴时间刻度。
//!
//! 日期用**终端文字**渲染，不画进位图 —— 交给终端字体更清晰，
//! 也省掉在画布上做字形栅格化。

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::core::bar::{Bar, Timeframe};

/// 一个刻度：贴在第几根 K 线上，显示什么文字
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick {
    /// 在可视窗口里的下标
    pub index: usize,
    pub label: String,
}

/// 时间戳格式随周期变化 —— 月线标到月，分钟线标到分。
fn format_at(ts: DateTime<Utc>, tf: Timeframe, tz: Tz) -> String {
    let t = ts.with_timezone(&tz);
    match tf {
        Timeframe::Month => t.format("%Y-%m").to_string(),
        Timeframe::Week | Timeframe::Day => t.format("%Y-%m-%d").to_string(),
        _ => t.format("%m-%d %H:%M").to_string(),
    }
}

/// 计算横轴刻度。
///
/// `width` 是可用的字符列数。刻度之间至少留 `label_width + 3` 列，
/// 否则文字会挤在一起互相盖住。
pub fn ticks(bars: &[Bar], tf: Timeframe, tz: Tz, width: u16) -> Vec<Tick> {
    if bars.is_empty() || width == 0 {
        return Vec::new();
    }
    let sample = format_at(bars[0].ts, tf, tz);
    let label_w = sample.chars().count() as u16;
    let slot = label_w + 3;
    if width < slot {
        return Vec::new();
    }
    // 最多放几个刻度；两端各留半个标签的余量，避免贴边被截断
    let max_ticks = ((width / slot) as usize).clamp(1, 8);
    if max_ticks == 1 {
        // 只放得下一个就标最新那根 —— 看盘时最关心右端
        return vec![Tick {
            index: bars.len() - 1,
            label: format_at(bars[bars.len() - 1].ts, tf, tz),
        }];
    }
    (0..max_ticks)
        .map(|i| {
            let idx = i * (bars.len() - 1) / (max_ticks - 1);
            Tick {
                index: idx,
                label: format_at(bars[idx].ts, tf, tz),
            }
        })
        .collect()
}

/// 刻度下标 → 字符列偏移。与蜡烛的横向布局用同一个比例，才对得齐。
pub fn column_of(index: usize, bar_count: usize, width: u16) -> u16 {
    if bar_count <= 1 || width == 0 {
        return 0;
    }
    let ratio = index as f64 / (bar_count - 1) as f64;
    ((ratio * (width - 1) as f64).round() as u16).min(width - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const SH: Tz = chrono_tz::Asia::Shanghai;

    fn bars(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| Bar {
                ts: Utc.with_ymd_and_hms(2026, 1, 1, 1, 30, 0).unwrap()
                    + chrono::Duration::days(i as i64),
                open: 10.0,
                high: 11.0,
                low: 9.0,
                close: 10.5,
                volume: 1.0,
            })
            .collect()
    }

    #[test]
    fn 日线标到日() {
        let t = ticks(&bars(100), Timeframe::Day, SH, 200);
        assert!(t[0].label.starts_with("2026-01-01"), "实际 {}", t[0].label);
    }

    #[test]
    fn 月线只标到月() {
        let t = ticks(&bars(100), Timeframe::Month, SH, 200);
        assert_eq!(t[0].label, "2026-01");
    }

    #[test]
    fn 分钟线标到分() {
        let t = ticks(&bars(100), Timeframe::Min15, SH, 200);
        // UTC 01:30 → 北京 09:30
        assert!(t[0].label.ends_with("09:30"), "实际 {}", t[0].label);
    }

    #[test]
    fn 按市场时区换算而不是utc() {
        let ny = ticks(&bars(10), Timeframe::Min60, chrono_tz::America::New_York, 200);
        let sh = ticks(&bars(10), Timeframe::Min60, SH, 200);
        assert_ne!(ny[0].label, sh[0].label, "不同时区应给出不同的本地时间");
    }

    #[test]
    fn 首尾都有刻度() {
        let bs = bars(100);
        let t = ticks(&bs, Timeframe::Day, SH, 200);
        assert_eq!(t.first().unwrap().index, 0);
        assert_eq!(t.last().unwrap().index, 99, "最后一个刻度应落在最新一根上");
    }

    #[test]
    fn 刻度之间不重叠() {
        let bs = bars(500);
        for width in [30u16, 60, 120, 240, 400] {
            let t = ticks(&bs, Timeframe::Day, SH, width);
            if t.len() < 2 {
                continue;
            }
            let label_w = t[0].label.chars().count() as u16;
            for w in t.windows(2) {
                let c0 = column_of(w[0].index, bs.len(), width);
                let c1 = column_of(w[1].index, bs.len(), width);
                assert!(
                    c1 >= c0 + label_w,
                    "宽度 {width}：刻度 {c0} 和 {c1} 会重叠（标签宽 {label_w}）"
                );
            }
        }
    }

    #[test]
    fn 窄到放不下就一个都不放() {
        assert!(ticks(&bars(50), Timeframe::Day, SH, 8).is_empty());
    }

    #[test]
    fn 只放得下一个时标最新那根() {
        let bs = bars(50);
        let t = ticks(&bs, Timeframe::Day, SH, 13);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].index, 49, "只能标一个时应标最新，看盘最关心右端");
    }

    #[test]
    fn 空数据不panic() {
        assert!(ticks(&[], Timeframe::Day, SH, 200).is_empty());
    }

    #[test]
    fn 单根数据不panic() {
        let t = ticks(&bars(1), Timeframe::Day, SH, 200);
        assert!(t.iter().all(|x| x.index == 0));
        assert_eq!(column_of(0, 1, 100), 0);
    }

    #[test]
    fn 列映射覆盖整个宽度() {
        assert_eq!(column_of(0, 100, 200), 0, "第一根贴左边");
        assert_eq!(column_of(99, 100, 200), 199, "最后一根贴右边");
    }

    #[test]
    fn 列映射不越界() {
        for n in [1usize, 2, 7, 250, 5000] {
            for w in [1u16, 5, 80, 300] {
                for i in [0, n / 2, n.saturating_sub(1)] {
                    assert!(column_of(i, n, w) < w.max(1), "n={n} w={w} i={i} 越界");
                }
            }
        }
    }
}
