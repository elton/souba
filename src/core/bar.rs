use chrono::{DateTime, Utc};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
