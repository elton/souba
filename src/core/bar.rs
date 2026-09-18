use chrono::{DateTime, Datelike, Utc};
use chrono_tz::Tz;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Timeframe {
    Day,
    Week,
    Month,
    Min60,
    Min30,
    Min15,
    Min5,
}

impl Timeframe {
    pub const CYCLE: &'static [Timeframe] = &[
        Timeframe::Day,
        Timeframe::Week,
        Timeframe::Month,
        Timeframe::Min60,
        Timeframe::Min30,
        Timeframe::Min15,
        Timeframe::Min5,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Timeframe::Day => "日线",
            Timeframe::Week => "周线",
            Timeframe::Month => "月线",
            Timeframe::Min60 => "60分",
            Timeframe::Min30 => "30分",
            Timeframe::Min15 => "15分",
            Timeframe::Min5 => "5分",
        }
    }

    /// 存库用的稳定标识，与 D1 共用。阶段 4 落库时才有调用方，
    /// 现在留着是因为「本地 SQLite 与 D1 共用同一套 schema」是设计的硬要求，
    /// 周期标识必须在两边保持一致。
    #[allow(dead_code)]
    pub fn key(self) -> &'static str {
        match self {
            Timeframe::Day => "1d",
            Timeframe::Week => "1w",
            Timeframe::Month => "1mo",
            Timeframe::Min60 => "60m",
            Timeframe::Min30 => "30m",
            Timeframe::Min15 => "15m",
            Timeframe::Min5 => "5m",
        }
    }

    pub fn next(self) -> Timeframe {
        let i = Self::CYCLE.iter().position(|t| *t == self).unwrap_or(0);
        Self::CYCLE[(i + 1) % Self::CYCLE.len()]
    }

    pub fn prev(self) -> Timeframe {
        let i = Self::CYCLE.iter().position(|t| *t == self).unwrap_or(0);
        Self::CYCLE[(i + Self::CYCLE.len() - 1) % Self::CYCLE.len()]
    }
}

/// 一根 K 线。价格用 f64 而不是 Decimal —— 指标计算是浮点数学，
/// 每根都转换一次反而更慢也更啰嗦。报价展示仍然走 Decimal 保精度。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    pub ts: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// 把日线聚合成周线：自然周 Mon–Fri，首根开盘、区间最高最低、末根收盘、成交量求和。
///
/// 两个细节都不能省：
/// - **分组要在市场本地时区里做**。`ts` 是按市场时区归一到 UTC 存的，A股周一
///   00:00 上海 = 周日 16:00 UTC，直接按 UTC 的日期分组会把周一并进上一周。
/// - **用 ISO 周号，不要 `(年, 第几周)` 手算**。跨年那一周（2025-12-29 周一 ~
///   2026-01-02 周五）属于 2026-W01，朴素分组会把它劈成两根残周线。
///
/// 输入必须已按时间升序 —— 全仓的 bar 序列都满足这个约定。
pub fn to_weekly(bars: &[Bar], tz: Tz) -> Vec<Bar> {
    let mut out: Vec<Bar> = Vec::new();
    let mut current: Option<(i32, u32)> = None;
    for b in bars {
        let iso = b.ts.with_timezone(&tz).iso_week();
        let key = (iso.year(), iso.week());
        if current == Some(key) {
            let w = out.last_mut().expect("周键已存在就必然已经推入过一根");
            w.high = w.high.max(b.high);
            w.low = w.low.min(b.low);
            w.close = b.close;
            w.volume += b.volume;
        } else {
            current = Some(key);
            out.push(*b);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const SH: Tz = chrono_tz::Asia::Shanghai;

    #[test]
    fn 周期循环首尾相接() {
        let mut t = Timeframe::Day;
        for _ in 0..Timeframe::CYCLE.len() {
            t = t.next();
        }
        assert_eq!(t, Timeframe::Day, "绕一圈应回到起点");
    }

    #[test]
    fn 前后互为逆操作() {
        for t in Timeframe::CYCLE {
            assert_eq!(t.next().prev(), *t);
            assert_eq!(t.prev().next(), *t);
        }
    }

    #[test]
    fn 存库标识唯一() {
        let mut keys: Vec<&str> = Timeframe::CYCLE.iter().map(|t| t.key()).collect();
        keys.sort_unstable();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n, "存库标识不能重复，否则不同周期会覆盖");
    }

    #[test]
    fn 周线聚合手算对照() {
        // 2026-01-05(一) ~ 01-09(五) 一周，01-12(一) ~ 01-13(二) 下一周
        let d = |day: u32, o: f64, h: f64, l: f64, c: f64, v: f64| Bar {
            ts: SH.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap().with_timezone(&Utc),
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        };
        let daily = vec![
            d(5, 10.0, 12.0, 9.0, 11.0, 100.0),
            d(6, 11.0, 15.0, 10.5, 14.0, 200.0),
            d(7, 14.0, 14.5, 8.0, 9.0, 300.0),
            d(8, 9.0, 11.0, 8.5, 10.0, 400.0),
            d(9, 10.0, 13.0, 9.5, 12.5, 500.0),
            d(12, 20.0, 21.0, 19.0, 20.5, 600.0),
            d(13, 20.5, 22.0, 18.0, 19.0, 700.0),
        ];
        let w = to_weekly(&daily, SH);
        assert_eq!(w.len(), 2, "两个自然周应聚成两根");
        assert_eq!(w[0].open, 10.0, "首开取周一开盘");
        assert_eq!(w[0].high, 15.0, "最高取全周最高");
        assert_eq!(w[0].low, 8.0, "最低取全周最低");
        assert_eq!(w[0].close, 12.5, "末收取周五收盘");
        assert_eq!(w[0].volume, 1500.0, "成交量求和");
        assert_eq!(w[0].ts, daily[0].ts, "周线时间戳取该周第一根");
        assert_eq!(w[1].open, 20.0);
        assert_eq!(w[1].high, 22.0);
        assert_eq!(w[1].low, 18.0);
        assert_eq!(w[1].close, 19.0);
        assert_eq!(w[1].volume, 1300.0);
    }

    #[test]
    fn 跨年那一周不被劈成两根() {
        // 2025-12-29(一) ~ 2026-01-02(五) 同属 ISO 2026-W01
        let mut daily: Vec<Bar> = (29..=31)
            .map(|day| Bar {
                ts: SH.with_ymd_and_hms(2025, 12, day, 0, 0, 0).unwrap().with_timezone(&Utc),
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 1.0,
            })
            .collect();
        daily.extend((1..=2).map(|day| Bar {
            ts: SH.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap().with_timezone(&Utc),
            open: 2.0,
            high: 2.0,
            low: 2.0,
            close: 2.0,
            volume: 1.0,
        }));
        let w = to_weekly(&daily, SH);
        assert_eq!(w.len(), 1, "跨年周必须是一根，实际 {} 根", w.len());
        assert_eq!(w[0].volume, 5.0);
        assert_eq!(w[0].close, 2.0);
    }

    #[test]
    fn 周线分组按市场本地时区而不是utc() {
        // A股周一 00:00 上海 = 周日 16:00 UTC。按 UTC 分组会把它算进上一周。
        let mon = SH.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap().with_timezone(&Utc);
        assert_eq!(mon.weekday(), chrono::Weekday::Sun, "前提：UTC 下它确实是周日");
        let prev_fri = SH.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap().with_timezone(&Utc);
        let b = |ts: DateTime<Utc>, c: f64| Bar { ts, open: c, high: c, low: c, close: c, volume: 1.0 };
        let w = to_weekly(&[b(prev_fri, 1.0), b(mon, 2.0)], SH);
        assert_eq!(w.len(), 2, "上周五与本周一不能并成一根");
    }

    #[test]
    fn 周线聚合空输入不panic() {
        assert!(to_weekly(&[], SH).is_empty());
    }
}
