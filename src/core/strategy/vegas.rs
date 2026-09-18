//! Vegas 隧道。
//!
//! ```text
//! EMA12              过滤器
//! EMA144 / EMA169    快隧道 —— 价格在其上方是做多的唯一前提
//! EMA576 / EMA676    慢隧道 —— 决定持仓与离场（= 快隧道 × 4）
//! ```
//!
//! **隧道是区间不是线。** 单根 K 线短暂刺破不算有效突破 —— 那条带子就是这套战法
//! 抗插针的全部原因。所以「位置」维度看的是最近 `slope_window` 根的多数，
//! 不是只看最后一根。

use anyhow::{Result, bail};

use crate::core::bar::{Timeframe, to_weekly};
use crate::core::indicator::ema;
use crate::core::strategy::{
    Adequacy, Facet, FacetState, MarketData, ParamMap, ParamSpec, Signal, Stance, Strategy,
};

/// 「预备」维度的标签。板块内排序补位时要按标签找这一维，
/// 两边各写一份字面量的话，改了名字就会静默失配。
pub const READY_FACET: &str = "预备";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VegasParams {
    /// 过滤器 EMA 周期
    pub filter: usize,
    /// 快隧道两条 EMA
    pub fast_lo: usize,
    pub fast_hi: usize,
    /// 慢隧道两条 EMA
    pub slow_lo: usize,
    pub slow_hi: usize,
    /// 慢隧道可信所需的最少日线根数
    pub min_bars: usize,
    /// 斜率与「位置」多数判定的回看窗口
    pub slope_window: usize,
    /// 新鲜度在这个窗口内算「刚确立」
    pub fresh_window: usize,
}

impl Default for VegasParams {
    fn default() -> Self {
        Self {
            filter: 12,
            fast_lo: 144,
            fast_hi: 169,
            slow_lo: 576,
            slow_hi: 676,
            min_bars: 1330,
            slope_window: 5,
            fresh_window: 3,
        }
    }
}

const SPECS: &[ParamSpec] = &[
    ParamSpec { key: "filter", label: "过滤器 EMA", default: 12.0, min: 1.0, max: 200.0 },
    ParamSpec { key: "fast_lo", label: "快隧道下周期", default: 144.0, min: 2.0, max: 2000.0 },
    ParamSpec { key: "fast_hi", label: "快隧道上周期", default: 169.0, min: 2.0, max: 2000.0 },
    ParamSpec { key: "slow_lo", label: "慢隧道下周期", default: 576.0, min: 2.0, max: 5000.0 },
    ParamSpec { key: "slow_hi", label: "慢隧道上周期", default: 676.0, min: 2.0, max: 5000.0 },
    ParamSpec { key: "min_bars", label: "最少根数", default: 1330.0, min: 1.0, max: 20000.0 },
    ParamSpec { key: "slope_window", label: "斜率窗口", default: 5.0, min: 1.0, max: 200.0 },
    ParamSpec { key: "fresh_window", label: "新鲜度窗口", default: 3.0, min: 1.0, max: 200.0 },
];

#[derive(Debug, Clone, Default)]
pub struct Vegas {
    pub params: VegasParams,
}

impl Vegas {
    #[allow(dead_code)]
    const TIMEFRAMES: &'static [Timeframe] = &[Timeframe::Day];
}

impl Strategy for Vegas {
    fn id(&self) -> &str {
        "vegas"
    }

    fn name(&self) -> &str {
        "Vegas 隧道"
    }

    /// 只要日线。周线是**由日线聚合出来的**，不用调用方另外装载一份 ——
    /// 两边各拉一次周线只会引入「日线与周线不同步」这种查都难查的偏差。
    fn required_timeframes(&self) -> &[Timeframe] {
        Self::TIMEFRAMES
    }

    fn min_bars(&self, tf: Timeframe) -> usize {
        match tf {
            Timeframe::Day => self.params.min_bars,
            _ => 0,
        }
    }

    fn param_specs(&self) -> &[ParamSpec] {
        SPECS
    }

    fn configure(&mut self, params: &ParamMap) -> Result<()> {
        let mut next = self.params;
        for (k, v) in params {
            let spec = SPECS
                .iter()
                .find(|s| s.key == k)
                .ok_or_else(|| anyhow::anyhow!("未知参数 {k:?}"))?;
            if !v.is_finite() || *v < spec.min || *v > spec.max {
                bail!("{}（{k}）应在 {} ~ {} 之间，收到 {v}", spec.label, spec.min, spec.max);
            }
            let n = v.round() as usize;
            match spec.key {
                "filter" => next.filter = n,
                "fast_lo" => next.fast_lo = n,
                "fast_hi" => next.fast_hi = n,
                "slow_lo" => next.slow_lo = n,
                "slow_hi" => next.slow_hi = n,
                "min_bars" => next.min_bars = n,
                "slope_window" => next.slope_window = n,
                "fresh_window" => next.fresh_window = n,
                other => bail!("参数 {other:?} 没有落点"),
            }
        }
        // 隧道的次序是这套战法的定义，配反了得到的不是「另一组参数」而是另一个东西
        if !(next.filter < next.fast_lo
            && next.fast_lo < next.fast_hi
            && next.fast_hi < next.slow_lo
            && next.slow_lo < next.slow_hi)
        {
            bail!(
                "周期必须满足 过滤器 < 快隧道下 < 快隧道上 < 慢隧道下 < 慢隧道上，\
                 收到 {} < {} < {} < {} < {}",
                next.filter,
                next.fast_lo,
                next.fast_hi,
                next.slow_lo,
                next.slow_hi
            );
        }
        self.params = next;
        Ok(())
    }

    fn evaluate(&self, data: &MarketData) -> Signal {
        let bars = data.bars(Timeframe::Day);
        let p = &self.params;
        let adequacy = Adequacy {
            have: bars.len(),
            need: p.min_bars,
        };
        if bars.is_empty() {
            return Signal {
                stance: Stance::Insufficient,
                facets: Vec::new(),
                adequacy,
                fresh_bars: None,
            };
        }

        let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
        let e_filter = ema(&closes, p.filter);
        let f_lo = ema(&closes, p.fast_lo);
        let f_hi = ema(&closes, p.fast_hi);
        let s_lo = ema(&closes, p.slow_lo);
        let s_hi = ema(&closes, p.slow_hi);
        let i = bars.len() - 1;
        let eps = eps_of(closes[i]);

        // 隧道是区间：上沿取两条 EMA 的较高者，下沿取较低者
        let (fast_top, fast_bottom) = (f_lo[i].max(f_hi[i]), f_lo[i].min(f_hi[i]));
        let (slow_top, slow_bottom) = (s_lo[i].max(s_hi[i]), s_lo[i].min(s_hi[i]));

        let window = p.slope_window.max(1);

        // ── 趋势：慢隧道两条 EMA 在窗口内同向才算向上/向下，否则走平
        let back = i.saturating_sub(window);
        let (d_lo, d_hi) = (s_lo[i] - s_lo[back], s_hi[i] - s_hi[back]);
        let trend = if d_lo > eps && d_hi > eps {
            FacetState::Bullish
        } else if d_lo < -eps && d_hi < -eps {
            FacetState::Bearish
        } else {
            FacetState::Neutral
        };
        let trend_word = match trend {
            FacetState::Bullish => "向上倾斜",
            FacetState::Neutral => "走平",
            FacetState::Bearish => "向下倾斜",
        };

        // ── 位置：最近 window 根相对快隧道区间的多数。
        // 只看最后一根的话，一根插针就能骗出「站上隧道」。
        let (mut above, mut below, mut inside) = (0usize, 0usize, 0usize);
        for j in (i + 1).saturating_sub(window)..=i {
            let (top, bottom) = (f_lo[j].max(f_hi[j]), f_lo[j].min(f_hi[j]));
            let e = eps_of(closes[j]);
            if closes[j] > top + e {
                above += 1;
            } else if closes[j] < bottom - e {
                below += 1;
            } else {
                inside += 1;
            }
        }
        let position = if above > below && above > inside {
            FacetState::Bullish
        } else if below > above && below > inside {
            FacetState::Bearish
        } else {
            FacetState::Neutral
        };

        // ── 确认：EMA12 是否也站上快隧道上沿（区分真假突破）
        let confirmed = e_filter[i] > fast_top + eps;
        let confirm = if confirmed {
            FacetState::Bullish
        } else if e_filter[i] < fast_bottom - eps {
            FacetState::Bearish
        } else {
            FacetState::Neutral
        };
        // 新鲜度：一路回溯到「确认」从假变真的那一根
        let fresh_bars = confirmed.then(|| {
            let mut j = i;
            while j > 0 && e_filter[j - 1] > f_lo[j - 1].max(f_hi[j - 1]) + eps_of(closes[j - 1]) {
                j -= 1;
            }
            i - j
        });

        // ── 共振：日线快隧道在慢隧道上方 ∧ 周线收盘站上周线快隧道
        let day_above = fast_bottom > slow_top + eps;
        let day_below = fast_top < slow_bottom - eps;
        let weekly = to_weekly(bars, data.symbol.market.timezone());
        let w_closes: Vec<f64> = weekly.iter().map(|b| b.close).collect();
        let wi = w_closes.len() - 1;
        let w_lo = ema(&w_closes, p.fast_lo);
        let w_hi = ema(&w_closes, p.fast_hi);
        let week_top = w_lo[wi].max(w_hi[wi]);
        let week_above = w_closes[wi] > week_top + eps_of(w_closes[wi]);
        let resonance = if day_above && week_above {
            FacetState::Bullish
        } else if day_below {
            FacetState::Bearish
        } else {
            FacetState::Neutral
        };

        // ── 预备：价格在隧道内或刚触及下沿 ∧ 慢隧道走平或转上。只是附注。
        let in_band = closes[i] >= fast_bottom - eps && closes[i] <= fast_top + eps;
        let touching = closes[i] < fast_bottom && bars[i].high >= fast_bottom - eps;
        let ready = if trend == FacetState::Bearish {
            FacetState::Bearish
        } else if in_band || touching {
            FacetState::Bullish
        } else {
            FacetState::Neutral
        };

        // ── 主判定。数据不足压过一切 —— 维度照算，但不能拿不可信的慢隧道喊多。
        let stance = if !adequacy.ok() {
            Stance::Insufficient
        } else if trend == FacetState::Bearish {
            Stance::Exit
        } else if position == FacetState::Bullish && confirmed {
            Stance::Long
        } else {
            Stance::Watch
        };

        let facets = vec![
            Facet {
                label: "趋势".into(),
                state: trend,
                detail: format!(
                    "慢隧道{trend_word}（{}:{:.2} {}:{:.2}，近 {window} 根 {:+.2}/{:+.2}）",
                    p.slow_lo, s_lo[i], p.slow_hi, s_hi[i], d_lo, d_hi
                ),
            },
            Facet {
                label: "共振".into(),
                state: resonance,
                detail: format!(
                    "日线快隧道在慢隧道{}；周线收盘 {:.2} {} 周线快隧道 {:.2}",
                    if day_above {
                        "上方"
                    } else if day_below {
                        "下方"
                    } else {
                        "区间内交叠"
                    },
                    w_closes[wi],
                    if week_above { "站上" } else { "未站上" },
                    week_top
                ),
            },
            Facet {
                label: "位置".into(),
                state: position,
                detail: format!(
                    "近 {window} 根：{above} 根在隧道上方 / {inside} 根隧道内 / {below} 根下方（隧道 {fast_bottom:.2}–{fast_top:.2}）"
                ),
            },
            Facet {
                label: "确认".into(),
                state: confirm,
                detail: match fresh_bars {
                    Some(n) => format!(
                        "EMA{} {:.2} 站上快隧道上沿 {fast_top:.2}，{n} 根前确立{}",
                        p.filter,
                        e_filter[i],
                        if n <= p.fresh_window { "（刚确立）" } else { "" }
                    ),
                    None => format!(
                        "EMA{} {:.2} {}快隧道（{fast_bottom:.2}–{fast_top:.2}），突破未获确认",
                        p.filter,
                        e_filter[i],
                        if confirm == FacetState::Bearish { "跌破" } else { "仍在" }
                    ),
                },
            },
            Facet {
                label: READY_FACET.into(),
                state: ready,
                detail: match ready {
                    FacetState::Bullish if in_band => {
                        format!("价格 {:.2} 在隧道内，慢隧道{trend_word}", closes[i])
                    }
                    FacetState::Bullish => {
                        format!("价格 {:.2} 刚触及隧道下沿 {fast_bottom:.2}，慢隧道{trend_word}", closes[i])
                    }
                    FacetState::Bearish => "慢隧道向下，谈不上预备".to_string(),
                    FacetState::Neutral => {
                        format!("价格 {:.2} 离隧道（{fast_bottom:.2}–{fast_top:.2}）还远", closes[i])
                    }
                },
            },
        ];

        Signal {
            stance,
            facets,
            adequacy,
            fresh_bars,
        }
    }
}

/// 浮点比较的相对容差。
///
/// 常数序列上 `α·c + (1-α)·c` 并不精确等于 `c`，上千根之后会累积出 1e-11 量级的
/// 单向漂移。把它当成「慢隧道在向上倾斜」就是凭空造出的信号 —— 一只 100 块的股票，
/// 斜率 1e-11 就是走平。
fn eps_of(price: f64) -> f64 {
    price.abs().max(1.0) * 1e-9
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bar::Bar;
    use crate::core::symbol::Symbol;
    use chrono::{Duration, TimeZone, Utc};
    use std::collections::HashMap;

    /// 按收盘价造一段日线。开高低都贴着收盘 —— 这些测试只关心 EMA 关系。
    fn series(closes: &[f64]) -> Vec<Bar> {
        let t0 = Utc.with_ymd_and_hms(2018, 1, 1, 0, 0, 0).unwrap();
        closes
            .iter()
            .enumerate()
            .map(|(i, c)| Bar {
                ts: t0 + Duration::days(i as i64),
                open: *c,
                high: *c,
                low: *c,
                close: *c,
                volume: 1.0,
            })
            .collect()
    }

    /// 长期常数后突变。常数段让所有 EMA 精确收敛到同一个值，
    /// 突变段的快慢差异就完全可控了。
    fn step(n_before: usize, before: f64, n_after: usize, after: f64) -> Vec<Bar> {
        let mut v = vec![before; n_before];
        v.extend(std::iter::repeat_n(after, n_after));
        series(&v)
    }

    fn eval(bars: Vec<Bar>) -> Signal {
        let sym = Symbol::parse("CN:600519").unwrap();
        let mut m = HashMap::new();
        m.insert(Timeframe::Day, bars);
        Vegas::default().evaluate(&MarketData::new(&sym, None, &m))
    }

    fn facet<'a>(s: &'a Signal, label: &str) -> &'a Facet {
        s.facets
            .iter()
            .find(|f| f.label == label)
            .unwrap_or_else(|| panic!("没有「{label}」维度，只有 {:?}", s.facets.iter().map(|f| &f.label).collect::<Vec<_>>()))
    }

    #[test]
    fn 五个维度齐全且每个都带人话理由() {
        let s = eval(step(1400, 100.0, 200, 200.0));
        for label in ["趋势", "共振", "位置", "确认", "预备"] {
            let f = facet(&s, label);
            assert!(!f.detail.is_empty(), "「{label}」没写理由 —— 理由才是要喂给大模型的东西");
        }
        assert_eq!(s.facets.len(), 5);
    }

    #[test]
    fn 长期横盘后跳高是做多且各维度看多() {
        let s = eval(step(1400, 100.0, 200, 200.0));
        assert_eq!(s.stance, Stance::Long, "{:?}", s.facets);
        assert_eq!(facet(&s, "趋势").state, FacetState::Bullish);
        assert_eq!(facet(&s, "共振").state, FacetState::Bullish);
        assert_eq!(facet(&s, "位置").state, FacetState::Bullish);
        assert_eq!(facet(&s, "确认").state, FacetState::Bullish);
    }

    #[test]
    fn 长期横盘后跳低是离场且各维度看空() {
        let s = eval(step(1400, 200.0, 200, 100.0));
        assert_eq!(s.stance, Stance::Exit, "{:?}", s.facets);
        assert_eq!(facet(&s, "趋势").state, FacetState::Bearish);
        assert_eq!(facet(&s, "共振").state, FacetState::Bearish);
        assert_eq!(facet(&s, "位置").state, FacetState::Bearish);
        assert_eq!(facet(&s, "确认").state, FacetState::Bearish);
        assert_eq!(facet(&s, "预备").state, FacetState::Bearish, "慢隧道向下时谈不上预备");
    }

    #[test]
    fn 全程横盘是观望且趋势共振位置确认都走平() {
        let s = eval(step(1600, 100.0, 0, 100.0));
        assert_eq!(s.stance, Stance::Watch);
        assert_eq!(facet(&s, "趋势").state, FacetState::Neutral, "常数序列的 EMA 漂移不该被当成斜率");
        assert_eq!(facet(&s, "共振").state, FacetState::Neutral);
        assert_eq!(facet(&s, "位置").state, FacetState::Neutral);
        assert_eq!(facet(&s, "确认").state, FacetState::Neutral);
    }

    #[test]
    fn 预备为真也不会把观望抬成做多() {
        // 价格贴在隧道里、慢隧道走平 —— 正是「预备」为真的形状
        let s = eval(step(1600, 100.0, 0, 100.0));
        assert_eq!(facet(&s, "预备").state, FacetState::Bullish);
        assert_eq!(s.stance, Stance::Watch, "预备只是附注，不能变成买入信号");
    }

    #[test]
    fn 单根向上刺破不改变位置维度() {
        let mut bars = step(1600, 100.0, 0, 100.0);
        let last = bars.len() - 1;
        bars[last].close = 500.0;
        bars[last].high = 500.0;
        let s = eval(bars);
        assert_eq!(
            facet(&s, "位置").state,
            FacetState::Neutral,
            "单根刺破不是有效突破 —— 隧道是区间，这是它抗插针的全部原因"
        );
        assert_ne!(s.stance, Stance::Long);
    }

    #[test]
    fn 单根向下刺破不改变位置维度() {
        let mut bars = step(1600, 100.0, 0, 100.0);
        let last = bars.len() - 1;
        bars[last].close = 1.0;
        bars[last].low = 1.0;
        let s = eval(bars);
        assert_eq!(facet(&s, "位置").state, FacetState::Neutral);
    }

    #[test]
    fn 连续多根站上隧道才算位置在上方() {
        // 与上一条对照：刺破改成持续，位置维度就该翻多
        let mut bars = step(1600, 100.0, 0, 100.0);
        for b in bars.iter_mut().rev().take(5) {
            b.close = 500.0;
            b.high = 500.0;
        }
        let s = eval(bars);
        assert_eq!(facet(&s, "位置").state, FacetState::Bullish);
    }

    #[test]
    fn 根数不足时是数据不足且写明慢隧道不可信() {
        let s = eval(step(1329, 100.0, 0, 100.0));
        assert_eq!(s.stance, Stance::Insufficient);
        assert_eq!(s.adequacy.have, 1329);
        assert_eq!(s.adequacy.need, 1330);
        assert!(!s.adequacy.ok());
    }

    #[test]
    fn 刚够1330根就不再是数据不足() {
        let s = eval(step(1330, 100.0, 0, 100.0));
        assert_ne!(s.stance, Stance::Insufficient, "1330 根正好达标");
        assert!(s.adequacy.ok());
    }

    #[test]
    fn 数据不足时主判定压过一切() {
        // 形状上是完美的做多，但根数不够 —— 必须说数据不足，不能照样喊多
        let s = eval(step(1000, 100.0, 200, 200.0));
        assert_eq!(s.stance, Stance::Insufficient);
        assert_eq!(facet(&s, "位置").state, FacetState::Bullish, "维度本身照算，只是主判定被压住");
    }

    #[test]
    fn 新鲜度是确认从假变真以来的根数() {
        let a = eval(step(1400, 100.0, 200, 200.0));
        let b = eval(step(1400, 100.0, 203, 200.0));
        let (fa, fb) = (a.fresh_bars.expect("已确认应有新鲜度"), b.fresh_bars.expect("已确认应有新鲜度"));
        assert_eq!(fb, fa + 3, "多走三根，确立距今就该多三根");
    }

    #[test]
    fn 未确认时没有新鲜度() {
        let s = eval(step(1600, 100.0, 0, 100.0));
        assert_eq!(s.fresh_bars, None, "没确认就谈不上「几根前确立」");
    }

    #[test]
    fn 空序列不panic() {
        let s = eval(Vec::new());
        assert_eq!(s.stance, Stance::Insufficient);
    }

    #[test]
    fn 只有一根也不panic() {
        let s = eval(series(&[100.0]));
        assert_eq!(s.stance, Stance::Insufficient);
    }

    #[test]
    fn 默认参数就是口诀里的那组() {
        let p = VegasParams::default();
        assert_eq!(
            (p.filter, p.fast_lo, p.fast_hi, p.slow_lo, p.slow_hi),
            (12, 144, 169, 576, 676)
        );
        assert_eq!((p.min_bars, p.slope_window, p.fresh_window), (1330, 5, 3));
        // 慢隧道 = 快隧道 × 4
        assert_eq!(p.slow_lo, p.fast_lo * 4);
        assert_eq!(p.slow_hi, p.fast_hi * 4);
    }

    #[test]
    fn 只声明需要日线() {
        let v = Vegas::default();
        assert_eq!(v.required_timeframes(), &[Timeframe::Day]);
        assert_eq!(v.min_bars(Timeframe::Day), 1330);
        assert_eq!(v.min_bars(Timeframe::Min60), 0);
    }

    #[test]
    fn 合法参数被接受() {
        let mut v = Vegas::default();
        let mut p = ParamMap::new();
        p.insert("min_bars".into(), 800.0);
        p.insert("slope_window".into(), 10.0);
        v.configure(&p).unwrap();
        assert_eq!(v.params.min_bars, 800);
        assert_eq!(v.params.slope_window, 10);
        assert_eq!(v.params.fast_lo, 144, "没给的参数保持原值");
    }

    #[test]
    fn 快隧道比慢隧道还慢会被拒() {
        let mut v = Vegas::default();
        let mut p = ParamMap::new();
        p.insert("fast_lo".into(), 900.0);
        let err = v.configure(&p).unwrap_err().to_string();
        assert!(err.contains("快隧道"), "错误该说清哪里配反了：{err}");
        assert_eq!(v.params.fast_lo, 144, "校验失败不能留下半套参数");
    }

    #[test]
    fn 周期为零会被拒() {
        let mut v = Vegas::default();
        let mut p = ParamMap::new();
        p.insert("filter".into(), 0.0);
        assert!(v.configure(&p).is_err());
    }

    #[test]
    fn 未知参数会被拒() {
        let mut v = Vegas::default();
        let mut p = ParamMap::new();
        p.insert("nonsense".into(), 1.0);
        assert!(v.configure(&p).is_err());
    }
}
