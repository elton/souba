//! 图表绘制逻辑。**只跟像素画布打交道，不知道后端是谁。**
//!
//! 坐标系是像素，`Backend` 决定画布多大 —— Kitty 下是真实像素，
//! 盲文下是 2×4/格。同一份代码两边都跑。

use crate::core::bar::Bar;
use crate::core::indicator::{Kdj, Macd};
use crate::ui::canvas::{Canvas, Rgb};

/// A股 惯例红涨绿跌
pub const UP: Rgb = Rgb(220, 60, 60);
pub const DOWN: Rgb = Rgb(30, 190, 130);
pub const LINE_A: Rgb = Rgb(235, 235, 235);
pub const LINE_B: Rgb = Rgb(235, 200, 60);
pub const LINE_C: Rgb = Rgb(200, 90, 220);
pub const AXIS: Rgb = Rgb(90, 90, 90);

/// 纵向价格映射
#[derive(Debug, Clone, Copy)]
pub struct VScale {
    pub min: f64,
    pub max: f64,
    pub h: u32,
}

impl VScale {
    pub fn new(min: f64, max: f64, h: u32) -> Self {
        let pad = ((max - min) * 0.06).max(f64::EPSILON);
        Self {
            min: min - pad,
            max: max + pad,
            h,
        }
    }

    pub fn y(&self, v: f64) -> f32 {
        if self.h == 0 {
            return 0.0;
        }
        let span = self.max - self.min;
        if span <= 0.0 {
            return (self.h / 2) as f32;
        }
        let t = ((self.max - v) / span).clamp(0.0, 1.0);
        (t * (self.h - 1) as f64) as f32
    }
}

/// 每根 K 线占多少像素。至少留 1 像素间隔，否则糊成一片。
pub fn layout(canvas_w: u32, bar_count: usize) -> (u32, u32) {
    if bar_count == 0 {
        return (3, 2);
    }
    let step = (canvas_w / bar_count.max(1) as u32).clamp(2, 14);
    // 实体宽度占 step 的 2/3，其余留白；至少 1 像素
    let body = ((step * 2) / 3).max(1);
    (step, body)
}

/// 画蜡烛图，返回用到的价格映射（调用方据此画刻度）
pub fn candles(c: &mut Canvas, bars: &[Bar]) -> Option<VScale> {
    if bars.is_empty() || c.w == 0 || c.h == 0 {
        return None;
    }
    let (step, body_w) = layout(c.w, bars.len());
    let n = ((c.w / step) as usize).min(bars.len()).max(1);
    let shown = &bars[bars.len() - n..];

    let min = shown.iter().fold(f64::MAX, |a, b| a.min(b.low));
    let max = shown.iter().fold(f64::MIN, |a, b| a.max(b.high));
    let vs = VScale::new(min, max, c.h);

    let used = n as u32 * step;
    let left = c.w.saturating_sub(used);
    for (i, b) in shown.iter().enumerate() {
        let x = left + i as u32 * step;
        let color = if b.close >= b.open { UP } else { DOWN };
        // 影线画在实体中线上
        let mid = x + body_w / 2;
        c.v_line(mid as i64, vs.y(b.high) as i64, vs.y(b.low) as i64, color);
        // 实体；开收相等（一字板）时至少画 1 像素高，否则整根消失
        let (yt, yb) = (vs.y(b.open.max(b.close)), vs.y(b.open.min(b.close)));
        let h = ((yb - yt).round() as i64).max(1);
        c.fill_rect(x as i64, yt as i64, body_w as i64, h, color);
    }
    Some(vs)
}

/// 把一条序列画成折线。右对齐到画布右端，与蜡烛同步。
fn line(c: &mut Canvas, values: &[f64], step: u32, vs: VScale, color: Rgb, width: u32) {
    if values.is_empty() {
        return;
    }
    let n = ((c.w / step.max(1)) as usize).min(values.len()).max(1);
    let shown = &values[values.len() - n..];
    let left = c.w.saturating_sub(n as u32 * step);
    let pts: Vec<(f32, f32)> = shown
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_finite())
        .map(|(i, v)| ((left + i as u32 * step + step / 2) as f32, vs.y(*v)))
        .collect();
    c.polyline(&pts, color, width);
}

/// MACD：柱状体 + DIF/DEA
pub fn macd(c: &mut Canvas, m: &Macd, bar_count: usize) {
    if m.hist.is_empty() || c.w == 0 || c.h == 0 {
        return;
    }
    let (step, body_w) = layout(c.w, bar_count.max(m.hist.len()));
    let n = ((c.w / step) as usize).min(m.hist.len()).max(1);
    let take = |v: &[f64]| v[v.len().saturating_sub(n)..].to_vec();
    let (hist, dif, dea) = (take(&m.hist), take(&m.dif), take(&m.dea));

    let all: Vec<f64> = hist
        .iter()
        .chain(&dif)
        .chain(&dea)
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    if all.is_empty() {
        return;
    }
    let lo = all.iter().fold(f64::MAX, |a, b| a.min(*b));
    let hi = all.iter().fold(f64::MIN, |a, b| a.max(*b));
    let vs = VScale::new(lo, hi, c.h);

    // 零轴
    let zero_y = vs.y(0.0);
    for x in 0..c.w {
        if x % 3 == 0 {
            c.blend(x as i64, zero_y as i64, AXIS, 0.6);
        }
    }

    let left = c.w.saturating_sub(n as u32 * step);
    for (i, v) in hist.iter().enumerate() {
        if !v.is_finite() {
            continue;
        }
        let x = left + i as u32 * step;
        let color = if *v >= 0.0 { UP } else { DOWN };
        let y = vs.y(*v);
        let (top, h) = if y < zero_y {
            (y, (zero_y - y).max(1.0))
        } else {
            (zero_y, (y - zero_y).max(1.0))
        };
        c.fill_rect(x as i64, top as i64, body_w as i64, h.round() as i64, color);
    }
    line(c, &dea, step, vs, LINE_B, 1);
    line(c, &dif, step, vs, LINE_A, 1);
}

/// KDJ：纵轴固定 0–100，另画 20/50/80 三条参考线
pub fn kdj(c: &mut Canvas, k: &Kdj, bar_count: usize) {
    if k.k.is_empty() || c.w == 0 || c.h == 0 {
        return;
    }
    let (step, _) = layout(c.w, bar_count.max(k.k.len()));
    let vs = VScale::new(0.0, 100.0, c.h);
    for lvl in [20.0, 50.0, 80.0] {
        let y = vs.y(lvl) as i64;
        for x in 0..c.w {
            if x % 4 == 0 {
                c.blend(x as i64, y, AXIS, 0.5);
            }
        }
    }
    line(c, &k.j, step, vs, LINE_C, 1);
    line(c, &k.d, step, vs, LINE_B, 1);
    line(c, &k.k, step, vs, LINE_A, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn bars(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| {
                let cl = (i as f64 * 0.27).sin() * 30.0 + 100.0;
                Bar {
                    ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
                        + chrono::Duration::days(i as i64),
                    open: cl - 2.0,
                    high: cl + 5.0,
                    low: cl - 5.0,
                    close: cl,
                    volume: 1.0,
                }
            })
            .collect()
    }

    fn painted(c: &Canvas) -> usize {
        (0..c.h)
            .flat_map(|y| (0..c.w).map(move |x| (x, y)))
            .filter(|(x, y)| c.is_set(*x, *y))
            .count()
    }

    #[test]
    fn 价格映射上高下低() {
        let vs = VScale::new(0.0, 100.0, 200);
        assert!(vs.y(vs.max) < vs.y(vs.min), "高价应在上方（y 更小）");
        assert!((vs.y(vs.max) - 0.0).abs() < 1.0);
    }

    #[test]
    fn 蜡烛之间有间隔() {
        let (step, body) = layout(1000, 100);
        assert!(body < step, "实体宽度必须小于步距，否则挨在一起");
        assert!(step - body >= 1, "至少留 1 像素间隔");
    }

    #[test]
    fn 间隔随空间自适应但有上下限() {
        assert_eq!(layout(100, 1000).0, 2, "数据密集时压到最小步距");
        assert_eq!(layout(10000, 10).0, 14, "空间极富余时步距封顶");
        assert_eq!(layout(0, 0), (3, 2), "退化输入不 panic");
    }

    #[test]
    fn 蜡烛画出内容() {
        let mut c = Canvas::new(400, 200);
        assert!(candles(&mut c, &bars(120)).is_some());
        assert!(painted(&c) > 1000, "画出的像素太少：{}", painted(&c));
    }

    #[test]
    fn 一字板也画得出实体() {
        // 开=收=高=低，实体高度为 0，必须兜到 1 像素否则整根消失
        let flat = Bar {
            ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            open: 10.0, high: 10.0, low: 10.0, close: 10.0, volume: 1.0,
        };
        let mut c = Canvas::new(60, 40);
        candles(&mut c, &[flat]).unwrap();
        assert!(painted(&c) > 0, "一字板整根消失了");
    }

    #[test]
    fn 空数据不画也不panic() {
        let mut c = Canvas::new(100, 50);
        assert!(candles(&mut c, &[]).is_none());
        assert_eq!(painted(&c), 0);
    }

    #[test]
    fn 涨绿跌红分色() {
        let up = Bar {
            ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            open: 10.0, high: 12.0, low: 9.0, close: 11.0, volume: 1.0,
        };
        let down = Bar { close: 9.5, ..up };
        let mut c = Canvas::new(60, 40);
        candles(&mut c, &[up, down]).unwrap();
        let has = |col: Rgb| {
            (0..c.h)
                .flat_map(|y| (0..c.w).map(move |x| (x, y)))
                .any(|(x, y)| c.color_at(x, y) == Some(col))
        };
        assert!(has(UP) && has(DOWN), "涨跌应有不同颜色");
    }

    #[test]
    fn macd画出柱子和两条线() {
        let closes: Vec<f64> = bars(200).iter().map(|b| b.close).collect();
        let m = crate::core::indicator::macd(&closes, 12, 26, 9);
        let mut c = Canvas::new(600, 120);
        macd(&mut c, &m, 200);
        let colors: std::collections::HashSet<(u8, u8, u8)> = (0..c.h)
            .flat_map(|y| (0..c.w).map(move |x| (x, y)))
            .filter_map(|(x, y)| c.color_at(x, y))
            .map(|r| (r.0, r.1, r.2))
            .collect();
        assert!(colors.len() >= 3, "应有柱子两色 + 至少一条线，实际 {} 种", colors.len());
    }

    #[test]
    fn kdj画出参考线和三条曲线() {
        let k = crate::core::indicator::kdj(&bars(200), 9, 3.0, 3.0);
        let mut c = Canvas::new(600, 120);
        kdj(&mut c, &k, 200);
        assert!(painted(&c) > 500);
    }

    #[test]
    fn 各种画布尺寸都不panic() {
        let bs = bars(300);
        let closes: Vec<f64> = bs.iter().map(|b| b.close).collect();
        let m = crate::core::indicator::macd(&closes, 12, 26, 9);
        let kd = crate::core::indicator::kdj(&bs, 9, 3.0, 3.0);
        for (w, h) in [(1u32, 1u32), (10, 4), (80, 40), (2800, 800), (40, 2000)] {
            let mut c = Canvas::new(w, h);
            let _ = candles(&mut c, &bs);
            let mut c2 = Canvas::new(w, h);
            macd(&mut c2, &m, 300);
            let mut c3 = Canvas::new(w, h);
            kdj(&mut c3, &kd, 300);
        }
    }
}
