use chrono::{DateTime, Datelike, NaiveTime, Timelike, Utc, Weekday};
use chrono_tz::Tz;

use crate::core::symbol::Market;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    /// 开盘前
    PreOpen,
    /// 交易中
    Open,
    /// 午间休市
    Lunch,
    /// 已收盘 / 非交易日
    Closed,
}

impl Session {
    /// 这个时段里报价是否应该持续更新。用来决定 age 该不该报成延迟。
    pub fn expects_updates(self) -> bool {
        matches!(self, Session::Open)
    }
}

/// 一个交易时段的起止（市场本地时间）
struct Window(NaiveTime, NaiveTime);

fn t(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).expect("时间常量必须合法")
}

impl Market {
    /// 市场所在时区，用于判断交易时段
    pub fn timezone(self) -> Tz {
        match self {
            Market::Cn => chrono_tz::Asia::Shanghai,
            Market::Hk => chrono_tz::Asia::Hong_Kong,
            Market::Us => chrono_tz::America::New_York,
            Market::Jp => chrono_tz::Asia::Tokyo,
        }
    }

    /// 腾讯给这个市场的报价时间戳所用的时区。
    /// 与 timezone() 不同 —— 腾讯给日股打的是北京时间戳，不是东京时间。
    /// 实测：按东京时间算延迟是 79 分钟，按北京时间是 18 分钟，后者才合理。
    pub fn quote_timezone(self) -> Tz {
        match self {
            Market::Us => chrono_tz::America::New_York,
            // CN / HK / JP 一律北京时间（HK 与北京同为 UTC+8）
            _ => chrono_tz::Asia::Shanghai,
        }
    }

    /// 上下午两段交易时间（市场本地时间）。美股没有午休，第二段为 None。
    fn windows(self) -> (Window, Option<Window>) {
        match self {
            Market::Cn => (Window(t(9, 30), t(11, 30)), Some(Window(t(13, 0), t(15, 0)))),
            Market::Hk => (Window(t(9, 30), t(12, 0)), Some(Window(t(13, 0), t(16, 0)))),
            Market::Jp => (Window(t(9, 0), t(11, 30)), Some(Window(t(12, 30), t(15, 30)))),
            Market::Us => (Window(t(9, 30), t(16, 0)), None),
        }
    }

    /// 已知限制：不处理法定节假日。四个市场的假期表各不相同且每年变动，
    /// 引入假期日历是单独一件事。非交易日会被判为 Open 但没有新报价，
    /// 表现为 age 持续增大 —— 阶段 1 接受这个不精确。
    pub fn session_at(self, now: DateTime<Utc>) -> Session {
        let local = now.with_timezone(&self.timezone());
        if matches!(local.weekday(), Weekday::Sat | Weekday::Sun) {
            return Session::Closed;
        }
        let now_t = NaiveTime::from_hms_opt(local.hour(), local.minute(), local.second())
            .expect("从 DateTime 取出的时分秒必然合法");
        let (morning, afternoon) = self.windows();

        if now_t < morning.0 {
            return Session::PreOpen;
        }
        if now_t < morning.1 {
            return Session::Open;
        }
        match afternoon {
            None => Session::Closed,
            Some(a) => {
                if now_t < a.0 {
                    Session::Lunch
                } else if now_t < a.1 {
                    Session::Open
                } else {
                    Session::Closed
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn a股报价时间戳用北京时区() {
        assert_eq!(Market::Cn.quote_timezone(), chrono_tz::Asia::Shanghai);
    }

    #[test]
    fn 日股报价时间戳用北京时区而非东京() {
        // 实测：腾讯 jp7203 的时间戳按东京时间算延迟是 79 分钟，按北京时间是 18 分钟。
        // 后者才合理，所以腾讯给日股打的是北京时间戳。
        assert_eq!(Market::Jp.quote_timezone(), chrono_tz::Asia::Shanghai);
        // 但市场本身在东京时区，交易时段判断要用它
        assert_eq!(Market::Jp.timezone(), chrono_tz::Asia::Tokyo);
    }

    #[test]
    fn 美股报价时间戳用纽约时区() {
        assert_eq!(Market::Us.quote_timezone(), chrono_tz::America::New_York);
    }

    #[test]
    fn a股上午盘() {
        // 2026-08-26 是周三。北京 10:30 → 交易中
        assert_eq!(Market::Cn.session_at(utc("2026-08-26T02:30:00Z")), Session::Open);
    }

    #[test]
    fn a股午休() {
        // 北京 12:00 → 午休（11:30-13:00）
        assert_eq!(Market::Cn.session_at(utc("2026-08-26T04:00:00Z")), Session::Lunch);
    }

    #[test]
    fn a股收盘后() {
        // 北京 16:00 → 已收盘
        assert_eq!(Market::Cn.session_at(utc("2026-08-26T08:00:00Z")), Session::Closed);
    }

    #[test]
    fn 周末一律休市() {
        // 2026-08-29 是周六
        assert_eq!(Market::Cn.session_at(utc("2026-08-29T02:30:00Z")), Session::Closed);
        assert_eq!(Market::Us.session_at(utc("2026-08-29T18:00:00Z")), Session::Closed);
    }

    #[test]
    fn 东证午休时段与a股不同() {
        // 东京 12:00 → 东证午休是 11:30-12:30
        assert_eq!(Market::Jp.session_at(utc("2026-08-26T03:00:00Z")), Session::Lunch);
        // 东京 13:00 → 下午盘
        assert_eq!(Market::Jp.session_at(utc("2026-08-26T04:00:00Z")), Session::Open);
    }

    #[test]
    fn 只有交易中才期待更新() {
        assert!(Session::Open.expects_updates());
        assert!(!Session::Lunch.expects_updates());
        assert!(!Session::Closed.expects_updates());
        assert!(!Session::PreOpen.expects_updates());
    }
}
