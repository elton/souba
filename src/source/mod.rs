pub mod tencent;
pub mod throttle;

use crate::core::quote::Quote;
use crate::core::symbol::Symbol;

/// 一个行情源。
///
/// 实现者负责自己的节流；返回的每条 Quote 必须带 source 与可算出 age 的时间戳 ——
/// UI 要靠它区分实时与延迟，这是本项目的红线。
#[allow(async_fn_in_trait)]
pub trait QuoteSource {
    fn id(&self) -> &'static str;
    async fn quotes(&self, symbols: &[Symbol]) -> anyhow::Result<Vec<Quote>>;
}
