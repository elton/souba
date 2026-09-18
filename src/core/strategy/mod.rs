//! 策略引擎：`Strategy` trait 与它的求值结果 `Signal`。
//!
//! 这层抽象是刻意的。为了不让它长成「围着 Vegas 捏出来的伪接口」，trait 的形状
//! 在设计阶段用 MACD 金叉、KDJ 超买超卖、布林带突破反向检验过（设计文档 §7.1）。
//!
//! `Signal` **不合成单一分数**：各维度失效的原因不同，合成之后就说不清「为什么」，
//! 而「为什么」正是要喂给大模型的东西。

pub mod vegas;

use std::collections::HashMap;

use anyhow::Result;

use crate::core::bar::{Bar, Timeframe};
use crate::core::quote::Quote;
use crate::core::symbol::Symbol;

/// 主判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stance {
    Long,
    Watch,
    Exit,
    /// 数据不够，不是「不看好」。两者必须能被用户区分开。
    Insufficient,
}

impl Stance {
    /// 落库用的稳定标识。界面文案走 `label()` —— 改文案不该动到库里的值。
    pub fn as_str(self) -> &'static str {
        match self {
            Stance::Long => "long",
            Stance::Watch => "watch",
            Stance::Exit => "exit",
            Stance::Insufficient => "insufficient",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Stance::Long => "做多",
            Stance::Watch => "观望",
            Stance::Exit => "离场",
            Stance::Insufficient => "数据不足",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacetState {
    Bullish,
    Neutral,
    Bearish,
}

impl FacetState {
    /// 一屏塞五个维度，只能用一个字符表达状态
    pub fn glyph(self) -> &'static str {
        match self {
            FacetState::Bullish => "↑",
            FacetState::Neutral => "→",
            FacetState::Bearish => "↓",
        }
    }
}

/// 策略自定义的一个正交维度，自己独立可解释。
#[derive(Debug, Clone)]
pub struct Facet {
    pub label: String,
    pub state: FacetState,
    /// 人话理由。要能直接念给用户听，也要能直接喂给大模型。
    /// 信号行只放得下状态缩写，读它的是机会面板与 AI 解读那两张票。
    #[allow(dead_code)]
    pub detail: String,
}

/// 数据是否够。不够时 `stance` 强制为 `Insufficient`，
/// **而不是照样算一条不可信的线**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adequacy {
    pub have: usize,
    pub need: usize,
}

impl Adequacy {
    pub fn ok(self) -> bool {
        self.have >= self.need
    }
}

#[derive(Debug, Clone)]
pub struct Signal {
    pub stance: Stance,
    pub facets: Vec<Facet>,
    pub adequacy: Adequacy,
    /// 「确认」维度从假变真以来的根数；没确认时为 `None`。
    /// 板块 top5 按它排序 —— 刚站上的排最前。
    pub fresh_bars: Option<usize>,
}

/// 参数规格：UI 据此生成配置界面，配置文件与 `souba set` 据此校验。
///
/// 字段现在只有 `configure()` 内部和测试读 —— 把规格摆出来的那张票（`souba set`
/// 与配置界面）才是它的正经调用方。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub default: f64,
    pub min: f64,
    pub max: f64,
}

/// 参数取值。Vegas 的参数全是数字（周期、根数、窗口），不需要更花的类型。
#[allow(dead_code)]
pub type ParamMap = HashMap<String, f64>;

/// 策略求值的输入。按周期提供已排序（时间升序）的 K 线与最新报价。
/// 由调用方保证 `required_timeframes()` 声明的周期都已装载。
pub struct MarketData<'a> {
    pub symbol: &'a Symbol,
    /// 盘中有，回测时为 `None`。Vegas 只看已收盘的 bar，
    /// 用到它的是后面那些看盘口的策略。
    #[allow(dead_code)]
    pub quote: Option<&'a Quote>,
    bars: &'a HashMap<Timeframe, Vec<Bar>>,
}

impl<'a> MarketData<'a> {
    pub fn new(
        symbol: &'a Symbol,
        quote: Option<&'a Quote>,
        bars: &'a HashMap<Timeframe, Vec<Bar>>,
    ) -> Self {
        Self {
            symbol,
            quote,
            bars,
        }
    }

    pub fn bars(&self, tf: Timeframe) -> &[Bar] {
        self.bars.get(&tf).map_or(&[], |v| v.as_slice())
    }

    /// 扫描编排靠它判断某只标的的历史够不够，再决定要不要回补
    #[allow(dead_code)]
    pub fn len(&self, tf: Timeframe) -> usize {
        self.bars(tf).len()
    }
}

/// 详情屏只用到 `name()` 与 `evaluate()`；其余几个方法的调用方在后面的票里
/// （扫描编排读 `required_timeframes`/`min_bars`，`souba set` 读 `param_specs`/`configure`）。
/// 形状是设计文档 §7.1 定的，不按调用方到齐与否裁剪。
#[allow(dead_code)]
pub trait Strategy: Send + Sync {
    /// 存库与配置键的稳定标识
    fn id(&self) -> &str;
    fn name(&self) -> &str;

    /// 需要哪些周期的数据
    fn required_timeframes(&self) -> &[Timeframe];

    /// 结果可信所需的最少 K 线根数
    fn min_bars(&self, tf: Timeframe) -> usize;

    fn param_specs(&self) -> &[ParamSpec];
    fn configure(&mut self, params: &ParamMap) -> Result<()>;

    /// 求值。必须廉价 —— 全市场扫描会调用几千次。
    fn evaluate(&self, data: &MarketData) -> Signal;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn bar(c: f64) -> Bar {
        Bar {
            ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            open: c,
            high: c,
            low: c,
            close: c,
            volume: 1.0,
        }
    }

    #[test]
    fn 缺失周期返回空切片而不是panic() {
        let sym = Symbol::parse("CN:600519").unwrap();
        let mut m = HashMap::new();
        m.insert(Timeframe::Day, vec![bar(1.0), bar(2.0)]);
        let data = MarketData::new(&sym, None, &m);
        assert_eq!(data.len(Timeframe::Day), 2);
        assert_eq!(data.len(Timeframe::Min60), 0);
        assert!(data.bars(Timeframe::Week).is_empty());
    }

    #[test]
    fn 充足性判定看根数不看别的() {
        assert!(!Adequacy { have: 1329, need: 1330 }.ok());
        assert!(Adequacy { have: 1330, need: 1330 }.ok());
    }

    #[test]
    fn 数据不足与不看好是两种不同的主判定() {
        assert_ne!(Stance::Insufficient.label(), Stance::Watch.label());
        assert_ne!(Stance::Insufficient.label(), Stance::Exit.label());
    }
}
