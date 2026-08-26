//! 技术指标。
//!
//! 这里是手写实现，没有引入 `yata` / `ta-rs`。原因不是「不想加依赖」，而是
//! **KDJ 用的中国式 SMA 平滑，西方指标库都没有**：
//!
//! ```text
//! SMA(X, N, M) = (M * X_今日 + (N - M) * SMA_昨日) / N
//! ```
//!
//! 它和算术平均、和 Wilder 平滑都不是一回事。`yata::indicators::StochasticOscillator`
//! 给的是西方 Stochastic，K/D 数值与国内行情软件对不上。既然 KDJ 无论如何都要自己写，
//! EMA 和 MACD 再多二十行，就没必要为此背一个 2024 年之后再没发过版的依赖。
//!
//! 约定（与国内行情软件一致，与 TradingView 有两处差异，已在各函数注明）：
//! - `EMA` 用首个值做种子，不用前 N 根的 SMA
//! - `MACD` 柱状体是 `(DIF - DEA) * 2`

use crate::core::bar::Bar;

/// 指数移动平均。
///
/// 种子用序列首值（TradingView 用前 N 根的 SMA）。差异只影响开头若干根 ——
/// 权重按 `(1-α)^k` 衰减，`EMA576` 在 1330 根后种子残留低于 1%，
/// 而 A股/美股 的历史深度远超这个数。
pub fn ema(values: &[f64], period: usize) -> Vec<f64> {
    if values.is_empty() || period == 0 {
        return Vec::new();
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    let mut out = Vec::with_capacity(values.len());
    let mut prev = values[0];
    out.push(prev);
    for v in &values[1..] {
        prev = alpha * v + (1.0 - alpha) * prev;
        out.push(prev);
    }
    out
}

/// 中国式平滑：`SMA(X, N, M) = (M * X + (N - M) * prev) / N`
///
/// KDJ 专用。`seed` 是首值的初始状态（K/D 惯例取 50）。
fn cn_sma(values: &[f64], n: f64, m: f64, seed: f64) -> Vec<f64> {
    let mut out = Vec::with_capacity(values.len());
    let mut prev = seed;
    for v in values {
        prev = (m * v + (n - m) * prev) / n;
        out.push(prev);
    }
    out
}

#[derive(Debug, Clone)]
pub struct Macd {
    pub dif: Vec<f64>,
    pub dea: Vec<f64>,
    /// 柱状体。国内软件是 `(DIF - DEA) * 2`，TradingView 不乘 2。
    pub hist: Vec<f64>,
}

pub fn macd(closes: &[f64], fast: usize, slow: usize, signal: usize) -> Macd {
    let ef = ema(closes, fast);
    let es = ema(closes, slow);
    let dif: Vec<f64> = ef.iter().zip(&es).map(|(a, b)| a - b).collect();
    let dea = ema(&dif, signal);
    let hist = dif.iter().zip(&dea).map(|(d, s)| (d - s) * 2.0).collect();
    Macd { dif, dea, hist }
}

#[derive(Debug, Clone)]
pub struct Kdj {
    pub k: Vec<f64>,
    pub d: Vec<f64>,
    pub j: Vec<f64>,
}

/// KDJ。`n` 是 RSV 的回看窗口（惯例 9），`m1`/`m2` 是 K/D 的平滑周期（惯例 3/3）。
///
/// 前 `n` 根不足一个完整窗口时用已有的全部数据算 RSV —— 与国内软件一致，
/// 不是留空。
pub fn kdj(bars: &[Bar], n: usize, m1: f64, m2: f64) -> Kdj {
    let n = n.max(1);
    let mut rsv = Vec::with_capacity(bars.len());
    for i in 0..bars.len() {
        let from = i.saturating_sub(n - 1);
        let win = &bars[from..=i];
        let hh = win.iter().fold(f64::MIN, |a, b| a.max(b.high));
        let ll = win.iter().fold(f64::MAX, |a, b| a.min(b.low));
        // 窗口内全平（涨跌停一字板）时分母为 0，此时定义 RSV = 50 —— 既非超买亦非超卖
        rsv.push(if (hh - ll).abs() < f64::EPSILON {
            50.0
        } else {
            (bars[i].close - ll) / (hh - ll) * 100.0
        });
    }
    let k = cn_sma(&rsv, m1, 1.0, 50.0);
    let d = cn_sma(&k, m2, 1.0, 50.0);
    let j = k.iter().zip(&d).map(|(k, d)| 3.0 * k - 2.0 * d).collect();
    Kdj { k, d, j }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn bar(h: f64, l: f64, c: f64) -> Bar {
        Bar {
            ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            open: c,
            high: h,
            low: l,
            close: c,
            volume: 1.0,
        }
    }

    fn close(v: f64) -> f64 {
        (v * 1e6).round() / 1e6
    }

    #[test]
    fn ema_手算逐项吻合() {
        // period=3 → alpha=0.5，种子取首值
        // 1, 0.5*2+0.5*1=1.5, 0.5*3+0.5*1.5=2.25, 0.5*4+0.5*2.25=3.125, 0.5*5+0.5*3.125=4.0625
        let got: Vec<f64> = ema(&[1.0, 2.0, 3.0, 4.0, 5.0], 3).iter().map(|v| close(*v)).collect();
        assert_eq!(got, vec![1.0, 1.5, 2.25, 3.125, 4.0625]);
    }

    #[test]
    fn ema_长度与输入一致() {
        assert_eq!(ema(&[1.0; 100], 12).len(), 100);
    }

    #[test]
    fn ema_常数序列恒等于该常数() {
        for v in ema(&[7.0; 50], 20) {
            assert!((v - 7.0).abs() < 1e-9);
        }
    }

    #[test]
    fn ema_空输入与零周期不panic() {
        assert!(ema(&[], 12).is_empty());
        assert!(ema(&[1.0, 2.0], 0).is_empty());
    }

    #[test]
    fn ema576_种子残留符合推导() {
        // 设计文档断言：1330 根后种子残留低于 1%。这里用「首值 100、其余 0」的序列
        // 直接量出残留 —— 末值就是种子的剩余权重乘以 100。
        let mut v = vec![0.0; 1330];
        v[0] = 100.0;
        let residue = *ema(&v, 576).last().unwrap();
        assert!(residue < 1.0, "1330 根后残留 {residue}%，应低于 1%");
        // 对照：腾讯的 640 根上限，同样算 EMA576，残留应仍然显著
        let mut v2 = vec![0.0; 640];
        v2[0] = 100.0;
        let r640 = *ema(&v2, 576).last().unwrap();
        assert!(r640 > 5.0, "640 根时残留应仍然显著，实测 {r640}%");
    }

    #[test]
    fn macd_三条线长度一致() {
        let closes: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        let m = macd(&closes, 12, 26, 9);
        assert_eq!(m.dif.len(), 100);
        assert_eq!(m.dea.len(), 100);
        assert_eq!(m.hist.len(), 100);
    }

    #[test]
    fn macd_柱状体是差值的两倍() {
        let closes: Vec<f64> = (1..=60).map(|i| (i as f64 * 0.7).sin() * 10.0 + 50.0).collect();
        let m = macd(&closes, 12, 26, 9);
        for i in 0..closes.len() {
            assert!(
                ((m.dif[i] - m.dea[i]) * 2.0 - m.hist[i]).abs() < 1e-9,
                "第 {i} 根柱状体不等于 (DIF-DEA)*2"
            );
        }
    }

    #[test]
    fn macd_单调上涨时dif为正() {
        let closes: Vec<f64> = (1..=200).map(|i| i as f64).collect();
        let m = macd(&closes, 12, 26, 9);
        // 快线跑在慢线前面，DIF 应当为正
        assert!(*m.dif.last().unwrap() > 0.0);
    }

    #[test]
    fn kdj_中国式平滑手算吻合() {
        // 构造 RSV 恒为 100 的序列（收盘价即区间最高）
        let bars: Vec<Bar> = (0..5).map(|_| bar(10.0, 0.0, 10.0)).collect();
        let r = kdj(&bars, 9, 3.0, 3.0);
        // K = (1*100 + 2*50)/3 = 66.666..., 再 (1*100 + 2*66.666)/3 = 77.777...
        assert!((r.k[0] - 200.0 / 3.0).abs() < 1e-9, "K[0]={}", r.k[0]);
        assert!((r.k[1] - (100.0 + 2.0 * (200.0 / 3.0)) / 3.0).abs() < 1e-9);
    }

    #[test]
    fn kdj_j等于三k减二d() {
        let bars: Vec<Bar> = (0..40)
            .map(|i| {
                let c = (i as f64 * 0.4).sin() * 5.0 + 20.0;
                bar(c + 1.0, c - 1.0, c)
            })
            .collect();
        let r = kdj(&bars, 9, 3.0, 3.0);
        for i in 0..bars.len() {
            assert!((3.0 * r.k[i] - 2.0 * r.d[i] - r.j[i]).abs() < 1e-9, "第 {i} 根 J 不等于 3K-2D");
        }
    }

    #[test]
    fn kdj_k与d恒在0到100之间() {
        let bars: Vec<Bar> = (0..200)
            .map(|i| {
                let c = (i as f64 * 0.31).cos() * 30.0 + 50.0;
                bar(c + 2.0, c - 2.0, c)
            })
            .collect();
        let r = kdj(&bars, 9, 3.0, 3.0);
        for (i, (k, d)) in r.k.iter().zip(&r.d).enumerate() {
            assert!((0.0..=100.0).contains(k), "第 {i} 根 K={k} 越界");
            assert!((0.0..=100.0).contains(d), "第 {i} 根 D={d} 越界");
        }
        // J 可以越界，这是 KDJ 的固有特性，不该被夹取
    }

    #[test]
    fn kdj_一字板不产生除零() {
        // 涨停一字板：开高低收全等，窗口内 hh == ll
        let bars: Vec<Bar> = (0..12).map(|_| bar(10.0, 10.0, 10.0)).collect();
        let r = kdj(&bars, 9, 3.0, 3.0);
        assert!(r.k.iter().all(|v| v.is_finite()), "一字板不应产生 NaN/Inf");
        assert!((r.k.last().unwrap() - 50.0).abs() < 1.0, "全平时应收敛到中性 50");
    }

    #[test]
    fn kdj_空输入不panic() {
        let r = kdj(&[], 9, 3.0, 3.0);
        assert!(r.k.is_empty() && r.d.is_empty() && r.j.is_empty());
    }
}
