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
/// 网格线。要能看见但不能抢眼 —— 它是背景参考，不是内容。
pub const GRID: Rgb = Rgb(70, 76, 88);

/// 网格规格。横线按价格刻度的档位走，竖线贴在时间刻度上，
/// 这样网格和坐标轴数字是对齐的 —— 不对齐的网格只会添乱。
#[derive(Debug, Clone, Copy)]
pub struct Grid<'a> {
    /// 竖线画在第几根 K 线上（可视窗口内的下标）
    pub v_at: &'a [usize],
    /// 横线分成几档（含首尾）。0 表示不画横线。
    pub h_lines: usize,
}

/// 虚线密度：每 4 个像素画 2 个。实线会盖过蜡烛的细影线。
const DASH_ON: u32 = 2;
const DASH_PERIOD: u32 = 4;

// 网格是最先画的，底下没有内容，所以用满 alpha —— 半透明在空画布上
// 反而会混出一个不确定的颜色，既不好测也没好处。
fn dashed_h(c: &mut Canvas, y: i64, color: Rgb) {
    for x in 0..c.w {
        if x % DASH_PERIOD < DASH_ON {
            c.set(x as i64, y, color);
        }
    }
}

fn dashed_v(c: &mut Canvas, x: i64, color: Rgb) {
    for y in 0..c.h {
        if y % DASH_PERIOD < DASH_ON {
            c.set(x, y as i64, color);
        }
    }
}

/// 画网格。**必须在蜡烛之前调用** —— 网格是背景，压在数据上就本末倒置了。
pub fn grid(c: &mut Canvas, vs: VScale, g: &Grid, bar_count: usize) {
    if c.w == 0 || c.h == 0 {
        return;
    }
    if g.h_lines >= 2 {
        for i in 0..g.h_lines {
            let frac = i as f64 / (g.h_lines - 1) as f64;
            let price = vs.max - (vs.max - vs.min) * frac;
            dashed_h(c, vs.y(price) as i64, GRID);
        }
    }
    if bar_count > 0 {
        let (step, body) = layout(c.w, bar_count);
        for idx in g.v_at {
            let x = (*idx as f64 * step + body as f64 / 2.0).round() as i64;
            dashed_v(c, x, GRID);
        }
    }
}

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

/// 传进来的这一段 K 线**精确铺满**画布宽度。
///
/// 返回 (第 i 根的左边缘 x, 实体宽度)。用浮点步距而不是整数 ——
/// 整数步距会把余量堆在一侧形成空白条，那是个很显眼的 bug。
pub fn layout(canvas_w: u32, bar_count: usize) -> (f64, u32) {
    if bar_count == 0 || canvas_w == 0 {
        return (3.0, 2);
    }
    let step = canvas_w as f64 / bar_count as f64;
    // 实体占步距的 2/3，其余留白；至少 1 像素，否则蜡烛消失
    let body = ((step * 2.0 / 3.0).floor() as u32).max(1);
    (step, body)
}

/// 画蜡烛图，返回用到的价格映射（调用方据此画刻度）
/// 画蜡烛图。`bars` 就是要显示的那一段，由调用方按视口切好 ——
/// 这里不再自己截取，避免「视口说显示 A 段、绘图却画了 B 段」。
pub fn candles(c: &mut Canvas, bars: &[Bar], g: Option<&Grid>) -> Option<VScale> {
    if bars.is_empty() || c.w == 0 || c.h == 0 {
        return None;
    }
    let (step, body_w) = layout(c.w, bars.len());

    let min = bars.iter().fold(f64::MAX, |a, b| a.min(b.low));
    let max = bars.iter().fold(f64::MIN, |a, b| a.max(b.high));
    let vs = VScale::new(min, max, c.h);

    // 先画网格，蜡烛盖在上面
    if let Some(g) = g {
        grid(c, vs, g, bars.len());
    }

    for (i, b) in bars.iter().enumerate() {
        let x = (i as f64 * step).round() as i64;
        let color = if b.close >= b.open { UP } else { DOWN };
        // 影线画在实体中线上
        c.v_line(
            x + (body_w / 2) as i64,
            vs.y(b.high) as i64,
            vs.y(b.low) as i64,
            color,
        );
        // 实体；开收相等（一字板）时至少画 1 像素高，否则整根消失
        let (yt, yb) = (vs.y(b.open.max(b.close)), vs.y(b.open.min(b.close)));
        let h = ((yb - yt).round() as i64).max(1);
        c.fill_rect(x, yt as i64, body_w as i64, h, color);
    }
    Some(vs)
}

/// 把一条序列画成折线，与蜡烛用同一套横坐标。
fn line(c: &mut Canvas, values: &[f64], step: f64, vs: VScale, color: Rgb, width: u32) {
    let pts: Vec<(f32, f32)> = values
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_finite())
        .map(|(i, v)| ((i as f64 * step + step / 2.0) as f32, vs.y(*v)))
        .collect();
    c.polyline(&pts, color, width);
}

/// MACD：柱状体 + DIF/DEA
/// MACD 面板。`m` 已经是按视口切好的那一段（但指标本身要在**全量**数据上
/// 算完再切，否则窗口左边缘的值会因缺少预热而失真）。
pub fn macd(c: &mut Canvas, m: &Macd, v_at: &[usize]) {
    if m.hist.is_empty() || c.w == 0 || c.h == 0 {
        return;
    }
    let (step, body_w) = layout(c.w, m.hist.len());
    let (hist, dif, dea) = (&m.hist, &m.dif, &m.dea);

    let all: Vec<f64> = hist
        .iter()
        .chain(dif)
        .chain(dea)
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    if all.is_empty() {
        return;
    }
    let lo = all.iter().fold(f64::MAX, |a, b| a.min(*b));
    let hi = all.iter().fold(f64::MIN, |a, b| a.max(*b));
    let vs = VScale::new(lo, hi, c.h);

    // 竖线与主图对齐，方便把 MACD 的拐点对到日期上
    grid(c, vs, &Grid { v_at, h_lines: 0 }, m.hist.len());

    // 零轴比网格显眼一点 —— MACD 的正负分界是要读的
    let zero_y = vs.y(0.0);
    for x in 0..c.w {
        if x % 3 == 0 {
            c.blend(x as i64, zero_y as i64, AXIS, 0.8);
        }
    }

    for (i, v) in hist.iter().enumerate() {
        if !v.is_finite() {
            continue;
        }
        let x = (i as f64 * step).round() as i64;
        let color = if *v >= 0.0 { UP } else { DOWN };
        let y = vs.y(*v);
        let (top, h) = if y < zero_y {
            (y, (zero_y - y).max(1.0))
        } else {
            (zero_y, (y - zero_y).max(1.0))
        };
        c.fill_rect(x, top as i64, body_w as i64, h.round() as i64, color);
    }
    line(c, dea, step, vs, LINE_B, 2);
    line(c, dif, step, vs, LINE_A, 2);
}

/// KDJ：纵轴固定 0–100，另画 20/50/80 三条参考线
pub fn kdj(c: &mut Canvas, k: &Kdj, v_at: &[usize]) {
    if k.k.is_empty() || c.w == 0 || c.h == 0 {
        return;
    }
    let (step, _) = layout(c.w, k.k.len());
    let vs = VScale::new(0.0, 100.0, c.h);
    grid(c, vs, &Grid { v_at, h_lines: 0 }, k.k.len());
    // 20/50/80 是 KDJ 的超买超卖参考位，比普通网格显眼
    for lvl in [20.0, 50.0, 80.0] {
        let y = vs.y(lvl) as i64;
        for x in 0..c.w {
            if x % 4 == 0 {
                c.blend(x as i64, y, AXIS, 0.7);
            }
        }
    }
    line(c, &k.j, step, vs, LINE_C, 2);
    line(c, &k.d, step, vs, LINE_B, 2);
    line(c, &k.k, step, vs, LINE_A, 2);
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
        assert!((body as f64) < step, "实体宽度必须小于步距，否则挨在一起");
        assert!(step - body as f64 >= 1.0, "至少留 1 像素间隔");
    }

    #[test]
    fn 精确铺满画布宽度不留空白条() {
        // 之前用整数步距，余量堆在一侧形成一条明显的空白 —— 那是个显眼的 bug
        for (w, n) in [(4160u32, 1500usize), (1000, 7), (333, 100), (2000, 2000)] {
            let (step, _) = layout(w, n);
            let last_right = (n - 1) as f64 * step;
            let unused = w as f64 - last_right;
            assert!(
                unused <= step + 1.0,
                "{w}px 画 {n} 根，右端剩了 {unused:.1}px 没用，说明没铺满"
            );
            assert!(step > 0.0);
        }
    }

    #[test]
    fn 退化输入不panic() {
        assert_eq!(layout(0, 0), (3.0, 2));
        assert_eq!(layout(100, 0), (3.0, 2));
        let (step, body) = layout(10, 1000);
        assert!(step > 0.0 && body >= 1, "根数远多于像素时实体也要保底 1 像素");
    }

    #[test]
    fn 蜡烛画出内容() {
        let mut c = Canvas::new(400, 200);
        assert!(candles(&mut c, &bars(120), None).is_some());
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
        candles(&mut c, &[flat], None).unwrap();
        assert!(painted(&c) > 0, "一字板整根消失了");
    }

    #[test]
    fn 空数据不画也不panic() {
        let mut c = Canvas::new(100, 50);
        assert!(candles(&mut c, &[], None).is_none());
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
        candles(&mut c, &[up, down], None).unwrap();
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
        macd(&mut c, &m, &[]);
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
        kdj(&mut c, &k, &[]);
        assert!(painted(&c) > 500);
    }

    #[test]
    fn 网格画在蜡烛下层不覆盖数据() {
        // 网格是背景参考，压在数据上就本末倒置
        let bs = bars(60);
        let mut with_grid = Canvas::new(400, 200);
        candles(&mut with_grid, &bs, Some(&Grid { v_at: &[0, 30, 59], h_lines: 5 })).unwrap();
        let mut no_grid = Canvas::new(400, 200);
        candles(&mut no_grid, &bs, None).unwrap();

        // 无网格版本里画到的每一个像素，有网格版本必须是同样的颜色
        for y in 0..200u32 {
            for x in 0..400u32 {
                if let Some(c0) = no_grid.color_at(x, y) {
                    assert_eq!(
                        with_grid.color_at(x, y),
                        Some(c0),
                        "({x},{y}) 的蜡烛被网格盖掉了"
                    );
                }
            }
        }
    }

    #[test]
    fn 网格确实画了东西() {
        let bs = bars(60);
        let mut c = Canvas::new(400, 200);
        candles(&mut c, &bs, Some(&Grid { v_at: &[0, 30, 59], h_lines: 5 })).unwrap();
        let grid_px = (0..200u32)
            .flat_map(|y| (0..400u32).map(move |x| (x, y)))
            .filter(|(x, y)| c.color_at(*x, *y) == Some(GRID))
            .count();
        assert!(grid_px > 200, "网格像素太少：{grid_px}");
    }

    #[test]
    fn 网格是虚线不是实线() {
        // 实线会盖过蜡烛的细影线，视觉上很吵
        let mut c = Canvas::new(200, 100);
        grid(&mut c, VScale::new(0.0, 100.0, 100), &Grid { v_at: &[], h_lines: 3 }, 10);
        let row_y = (0..100u32).find(|y| (0..200u32).any(|x| c.is_set(x, *y))).unwrap();
        let on = (0..200u32).filter(|x| c.is_set(*x, row_y)).count();
        assert!(on > 0 && on < 200, "整行 200 像素画了 {on} 个 —— 应是虚线");
    }

    #[test]
    fn 不给网格时一个网格像素都没有() {
        let mut c = Canvas::new(200, 100);
        candles(&mut c, &bars(30), None).unwrap();
        let grid_px = (0..100u32)
            .flat_map(|y| (0..200u32).map(move |x| (x, y)))
            .filter(|(x, y)| c.color_at(*x, *y) == Some(GRID))
            .count();
        assert_eq!(grid_px, 0);
    }

    #[test]
    fn 各种画布尺寸都不panic() {
        let bs = bars(300);
        let closes: Vec<f64> = bs.iter().map(|b| b.close).collect();
        let m = crate::core::indicator::macd(&closes, 12, 26, 9);
        let kd = crate::core::indicator::kdj(&bs, 9, 3.0, 3.0);
        for (w, h) in [(1u32, 1u32), (10, 4), (80, 40), (2800, 800), (40, 2000)] {
            let mut c = Canvas::new(w, h);
            let _ = candles(&mut c, &bs, Some(&Grid { v_at: &[0, 50, 299], h_lines: 5 }));
            let mut c2 = Canvas::new(w, h);
            macd(&mut c2, &m, &[0, 100, 299]);
            let mut c3 = Canvas::new(w, h);
            kdj(&mut c3, &kd, &[0, 100, 299]);
        }
    }
}
