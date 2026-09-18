//! 前复权：库里存**原始价**，读时按因子表折算。
//!
//! 这样除权不需要重写历史 —— 每次分红只多一条因子，`bars` 永远只增不改。

use chrono::NaiveDate;
use chrono_tz::Tz;

use crate::core::bar::Bar;

/// 新浪 `qfq.js` 的一条累计前复权因子。最新一条恒为 1.0，
/// 最老一条是 `1900-01-01` 的兜底项（覆盖上市以来到第一次除权之前）。
#[derive(Debug, Clone, PartialEq)]
pub struct AdjFactor {
    /// 除权日
    pub effective_date: NaiveDate,
    /// 该除权日起往前回溯时要除掉的累计因子
    pub factor: f64,
}

/// 按因子表把原始价折算成前复权价。**`factors` 必须按除权日升序**
/// （`parse_sina_qfq` 与 `Store::adj_factors` 都保证了这一点）。
///
/// 语义是拿茅台 fixture 对腾讯 qfq 实测定下来的：交易日 t 用**不晚于 t 的
/// 最近一个除权日**的因子，价格除以它。另一种读法（用 t 之后的下一个除权日
/// 的因子）实测偏差大三倍，已排除：
///
/// | 日期 | 新浪原始 | 本实现 | 腾讯 qfq | 偏差 | 另一种读法的偏差 |
/// |---|---|---|---|---|---|
/// | 2024-06-14 | 1555.00 | 1413.08 | 1420.661 | -0.53% | +1.53% |
/// | 2024-06-21 | 1471.00 | 1364.44 | 1367.537 | -0.23% | +1.33% |
/// | 2025-06-20 | 1428.66 | 1345.89 | 1349.079 | -0.24% | |
/// | 2025-06-27 | 1403.09 | 1347.70 | 1351.109 | -0.25% | |
///
/// 残留的 0.2-0.5% 是两家算法不同，不是 bug：腾讯做的是**减法**复权
/// （原始价减去其后所有分红，实测 2024-06-19 的 10 派 308.76 正好等于
/// 两侧差额之差 30.876），新浪给的是**除法**因子。距今越远差得越多。
pub fn apply_factors(bars: &mut [Bar], factors: &[AdjFactor], tz: Tz) {
    if factors.is_empty() {
        return;
    }
    for b in bars.iter_mut() {
        // ts 是 UTC，日期要按市场本地时区取，否则 A 股的 00:00 会退回前一天
        let day = b.ts.with_timezone(&tz).date_naive();
        let i = factors.partition_point(|f| f.effective_date <= day);
        // 比最早的因子还早：没有因子可用，保持原样而不是瞎猜
        let Some(f) = i.checked_sub(1).map(|i| factors[i].factor) else {
            continue;
        };
        if f > 0.0 {
            b.open /= f;
            b.high /= f;
            b.low /= f;
            b.close /= f;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    const SH: Tz = chrono_tz::Asia::Shanghai;

    fn factor(d: &str, f: f64) -> AdjFactor {
        AdjFactor {
            effective_date: d.parse().unwrap(),
            factor: f,
        }
    }

    fn bar(d: &str, close: f64) -> Bar {
        let day: NaiveDate = d.parse().unwrap();
        Bar {
            ts: SH
                .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
                .unwrap()
                .with_timezone(&Utc),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
        }
    }

    #[test]
    fn 用不晚于该日的最近一个因子() {
        let fs = [factor("2024-01-01", 2.0), factor("2025-01-01", 1.0)];
        let mut bs = [bar("2023-12-31", 100.0), bar("2024-06-01", 100.0), bar("2025-06-01", 100.0)];
        apply_factors(&mut bs, &fs, SH);
        assert_eq!(bs[0].close, 100.0, "早于所有因子的 bar 不动");
        assert_eq!(bs[1].close, 50.0, "落在 2.0 那一段");
        assert_eq!(bs[2].close, 100.0, "最新一段因子为 1，不变");
    }

    #[test]
    fn 除权日当天用新因子() {
        let fs = [factor("2024-01-01", 2.0), factor("2024-06-19", 1.0)];
        let mut bs = [bar("2024-06-18", 100.0), bar("2024-06-19", 100.0)];
        apply_factors(&mut bs, &fs, SH);
        assert_eq!(bs[0].close, 50.0);
        assert_eq!(bs[1].close, 100.0, "除权日当天已经是除权后的价，不再折算");
    }

    #[test]
    fn 四个价格一起折算成交量不动() {
        // 成交量各家复权口径不一，而 Vegas 只看价格 —— 不碰它比碰错好
        let fs = [factor("2020-01-01", 2.0)];
        let mut bs = [Bar {
            open: 10.0,
            high: 20.0,
            low: 5.0,
            close: 15.0,
            volume: 999.0,
            ..bar("2021-01-01", 0.0)
        }];
        apply_factors(&mut bs, &fs, SH);
        assert_eq!((bs[0].open, bs[0].high, bs[0].low, bs[0].close), (5.0, 10.0, 2.5, 7.5));
        assert_eq!(bs[0].volume, 999.0);
    }

    #[test]
    fn 没有因子时原样返回() {
        let mut bs = [bar("2024-06-01", 100.0)];
        apply_factors(&mut bs, &[], SH);
        assert_eq!(bs[0].close, 100.0);
    }
}
