use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;

use crate::core::symbol::Symbol;

#[derive(Debug, Clone)]
pub struct Quote {
    pub symbol: Symbol,
    pub name: String,
    pub last: Decimal,
    pub prev_close: Decimal,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub volume: Decimal,
    pub change: Decimal,
    pub change_pct: Decimal,
    /// 数据源给的时间戳，已按该市场的 quote_timezone 归一到 UTC
    pub stamped_at: DateTime<Utc>,
    /// 哪个源给的 —— UI 必须能显示
    pub source: &'static str,
}

/// 报价的新鲜度。UI 必须显示它 —— 把延迟数据显示成实时是本项目的红线。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// 交易时段内且足够新
    Live,
    /// 交易时段内但明显落后
    Delayed(Duration),
    /// 非交易时段，数据本来就不该更新
    Halted,
}

/// 交易时段内超过这个岁数就算延迟。
/// A股 Level-1 本身是 3 秒快照，留出网络与时钟偏差的余量。
const LIVE_THRESHOLD_SECS: i64 = 30;

impl Quote {
    pub fn freshness(&self, now: DateTime<Utc>) -> Freshness {
        if !self.symbol.market.session_at(now).expects_updates() {
            return Freshness::Halted;
        }
        let age = now - self.stamped_at;
        if age.num_seconds() <= LIVE_THRESHOLD_SECS {
            Freshness::Live
        } else {
            Freshness::Delayed(age)
        }
    }

    /// 供 UI 判断涨跌着色
    pub fn is_up(&self) -> bool {
        self.change >= Decimal::ZERO
    }
}
