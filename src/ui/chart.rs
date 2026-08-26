//! 终端 K 线与指标绘制。
//!
//! 纵向用半块字符（`▀` 上半、`▄` 下半、`█` 整格）把分辨率翻倍：每个字符单元
//! 覆盖两个「子行」，所以 20 行高的区域实际有 40 级价格精度。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use crate::core::bar::Bar;
use crate::core::indicator::{Kdj, Macd};

/// A股 惯例红涨绿跌
pub const UP: Color = Color::Red;
pub const DOWN: Color = Color::Green;

/// 价格区间。分开成类型是因为 K 线和指标面板要共用同一套映射逻辑。
#[derive(Debug, Clone, Copy)]
pub struct Scale {
    pub min: f64,
    pub max: f64,
    /// 子行总数 = 字符行数 * 2
    pub sub_rows: u16,
}

impl Scale {
    pub fn new(min: f64, max: f64, rows: u16) -> Self {
        // 上下各留一点余量，免得最高最低价贴边看不出形状
        let pad = ((max - min) * 0.05).max(f64::EPSILON);
        Self {
            min: min - pad,
            max: max + pad,
            sub_rows: rows.saturating_mul(2),
        }
    }

    /// 价格 → 子行序号（0 在顶部）。超出区间的夹到边界。
    pub fn sub_row(&self, price: f64) -> u16 {
        if self.sub_rows == 0 {
            return 0;
        }
        let span = self.max - self.min;
        if span <= 0.0 {
            return self.sub_rows / 2;
        }
        let t = ((self.max - price) / span).clamp(0.0, 1.0);
        ((t * (self.sub_rows - 1) as f64).round() as u16).min(self.sub_rows - 1)
    }
}

/// 把两个子行的占用状态合成一个字符
fn half_block(upper: bool, lower: bool) -> Option<char> {
    match (upper, lower) {
        (true, true) => Some('█'),
        (true, false) => Some('▀'),
        (false, true) => Some('▄'),
        (false, false) => None,
    }
}

/// 每根 K 线占多少列。1 列画蜡烛，其余留白 —— 挨在一起会糊成一片。
pub fn step_for(width: u16, bar_count: usize) -> u16 {
    if bar_count == 0 {
        return 2;
    }
    // 空间富余就把间隔拉开，最多 1 蜡烛 + 2 空
    let per = width as usize / bar_count.max(1);
    per.clamp(2, 3) as u16
}

/// 自下而上的八分块，索引即八分之几
const EIGHTHS_UP: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// 绘制蜡烛图。每根占 `step` 列（首列画，其余留白），从右往左填 ——
/// 空间不够时丢掉最老的，看盘关心的是右端。
pub fn render_candles(area: Rect, buf: &mut Buffer, bars: &[Bar]) -> Option<Scale> {
    if area.width == 0 || area.height == 0 || bars.is_empty() {
        return None;
    }
    let step = step_for(area.width, bars.len());
    let n = ((area.width / step) as usize).min(bars.len()).max(1);
    let shown = &bars[bars.len() - n..];

    let min = shown.iter().fold(f64::MAX, |a, b| a.min(b.low));
    let max = shown.iter().fold(f64::MIN, |a, b| a.max(b.high));
    let scale = Scale::new(min, max, area.height);
    let used = n as u16 * step;

    for (i, b) in shown.iter().enumerate() {
        let x = area.x + area.width.saturating_sub(used) + i as u16 * step;
        if x >= area.x + area.width {
            break;
        }
        let up = b.close >= b.open;
        let color = if up { UP } else { DOWN };

        let wick_top = scale.sub_row(b.high);
        let wick_bot = scale.sub_row(b.low);
        let body_top = scale.sub_row(b.open.max(b.close));
        let body_bot = scale.sub_row(b.open.min(b.close));

        for row in 0..area.height {
            let (su, sl) = (row * 2, row * 2 + 1);
            let is_body = |s: u16| s >= body_top && s <= body_bot;
            let is_wick = |s: u16| s >= wick_top && s <= wick_bot;
            let (bu, bl) = (is_body(su), is_body(sl));
            let ch = if bu || bl {
                half_block(bu || is_wick(su), bl || is_wick(sl)).unwrap_or('│')
            } else if is_wick(su) || is_wick(sl) {
                '│'
            } else {
                continue;
            };
            buf[(x, area.y + row)]
                .set_char(ch)
                .set_style(Style::default().fg(color));
        }
    }
    Some(scale)
}

/// 用八分块从零轴画一根柱子，纵向精度是半块的 4 倍。
///
/// 向上的柱子用自下而上的块直接画；向下的柱子没有对应的「自上而下」块字符，
/// 改用互补块 + 前景背景反转来实现同样的精度。
fn draw_bar(buf: &mut Buffer, area: Rect, x: u16, zero_sub8: u16, val_sub8: u16, color: Color) {
    let (from, to) = if val_sub8 < zero_sub8 {
        (val_sub8, zero_sub8)
    } else {
        (zero_sub8, val_sub8)
    };
    for row in 0..area.height {
        let cell_top = row * 8;
        let cell_bot = cell_top + 8;
        if to <= cell_top || from >= cell_bot {
            continue;
        }
        let hi = from.max(cell_top) - cell_top; // 该格内被填充的上边界
        let lo = to.min(cell_bot) - cell_top; // 下边界
        let cell = &mut buf[(x, area.y + row)];
        if hi == 0 && lo == 8 {
            cell.set_char('█').set_style(Style::default().fg(color));
        } else if lo == 8 {
            // 贴着格子底部 → 自下而上的块
            cell.set_char(EIGHTHS_UP[(8 - hi) as usize])
                .set_style(Style::default().fg(color));
        } else {
            // 贴着格子顶部（或悬在中间）→ 画互补块并反转前后景
            cell.set_char(EIGHTHS_UP[(8 - lo) as usize])
                .set_style(Style::default().fg(Color::Reset).bg(color));
        }
    }
}

/// MACD 面板：八分块柱状体 + 盲文 DIF/DEA 折线
pub fn render_macd(area: Rect, buf: &mut Buffer, m: &Macd) {
    if area.width == 0 || area.height == 0 || m.hist.is_empty() {
        return;
    }
    let n = (area.width as usize).min(m.hist.len());
    let take = |v: &[f64]| v[v.len() - n..].to_vec();
    let (hist, dif, dea) = (take(&m.hist), take(&m.dif), take(&m.dea));

    let finite = |v: &f64| v.is_finite();
    let lo = hist.iter().chain(&dif).chain(&dea).filter(|v| finite(v)).fold(f64::MAX, |a, b| a.min(*b));
    let hi = hist.iter().chain(&dif).chain(&dea).filter(|v| finite(v)).fold(f64::MIN, |a, b| a.max(*b));
    if !lo.is_finite() || !hi.is_finite() {
        return;
    }
    let scale = Scale::new(lo, hi, area.height);
    let sub8 = |v: f64| -> u16 {
        let span = scale.max - scale.min;
        if span <= 0.0 {
            return area.height * 4;
        }
        let t = ((scale.max - v) / span).clamp(0.0, 1.0);
        (t * (area.height * 8 - 1) as f64).round() as u16
    };
    let zero = sub8(0.0);
    let x0 = area.x + area.width - n as u16;

    for (i, v) in hist.iter().enumerate() {
        if !v.is_finite() {
            continue;
        }
        let color = if *v >= 0.0 { UP } else { DOWN };
        draw_bar(buf, area, x0 + i as u16, zero, sub8(*v), color);
    }

    // 折线走盲文，叠在柱子上层
    let mut canvas = crate::ui::braille::Braille::new(area.width, area.height);
    plot_line(&mut canvas, area, &dea, scale, Color::Yellow);
    plot_line(&mut canvas, area, &dif, scale, Color::White);
    canvas.blit(area, buf);
}

/// 把一条序列画进盲文画布。横向也按子像素铺开，2 倍水平精度。
fn plot_line(
    canvas: &mut crate::ui::braille::Braille,
    area: Rect,
    values: &[f64],
    scale: Scale,
    color: Color,
) {
    if values.is_empty() || area.width == 0 {
        return;
    }
    let sw = canvas.sub_width();
    let sh = canvas.sub_height();
    if sw == 0 || sh == 0 {
        return;
    }
    let span = scale.max - scale.min;
    let sub_y = |v: f64| -> u16 {
        if span <= 0.0 {
            return sh / 2;
        }
        let t = ((scale.max - v) / span).clamp(0.0, 1.0);
        ((t * (sh - 1) as f64).round() as u16).min(sh - 1)
    };
    let n = values.len();
    // 序列右对齐铺到画布右端
    let start_sub = sw.saturating_sub(n as u16 * 2);
    let mut prev: Option<(u16, u16)> = None;
    for (i, v) in values.iter().enumerate() {
        if !v.is_finite() {
            prev = None;
            continue;
        }
        let x = start_sub + i as u16 * 2;
        if x >= sw {
            break;
        }
        let y = sub_y(*v);
        match prev {
            Some((px, py)) => canvas.line(px, py, x, y, color),
            None => canvas.set(x, y, color),
        }
        prev = Some((x, y));
    }
}

/// KDJ 面板：三条盲文折线，纵轴固定 0–100（J 越界时夹到边缘）
pub fn render_kdj(area: Rect, buf: &mut Buffer, k: &Kdj) {
    if area.width == 0 || area.height == 0 || k.k.is_empty() {
        return;
    }
    let n = (area.width as usize * 2).min(k.k.len());
    let take = |v: &[f64]| v[v.len() - n..].to_vec();
    let scale = Scale::new(0.0, 100.0, area.height);
    let mut canvas = crate::ui::braille::Braille::new(area.width, area.height);
    plot_line(&mut canvas, area, &take(&k.j), scale, Color::Magenta);
    plot_line(&mut canvas, area, &take(&k.d), scale, Color::Yellow);
    plot_line(&mut canvas, area, &take(&k.k), scale, Color::White);
    canvas.blit(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use ratatui::style::Color;

    fn bars(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| {
                let c = (i as f64 * 0.3).sin() * 10.0 + 100.0;
                Bar {
                    ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
                        + chrono::Duration::days(i as i64),
                    open: c - 1.0,
                    high: c + 2.0,
                    low: c - 2.0,
                    close: c,
                    volume: 100.0,
                }
            })
            .collect()
    }

    fn blank(w: u16, h: u16) -> (Rect, Buffer) {
        let r = Rect::new(0, 0, w, h);
        (r, Buffer::empty(r))
    }

    #[test]
    fn 价格映射顶部为最高底部为最低() {
        let s = Scale::new(0.0, 100.0, 10);
        assert_eq!(s.sub_row(s.max), 0, "最高价应在第 0 子行");
        assert_eq!(s.sub_row(s.min), s.sub_rows - 1, "最低价应在最后一子行");
    }

    #[test]
    fn 价格映射越界被夹住不panic() {
        let s = Scale::new(10.0, 20.0, 8);
        assert_eq!(s.sub_row(-1e9), s.sub_rows - 1);
        assert_eq!(s.sub_row(1e9), 0);
    }

    #[test]
    fn 区间为零时不除零() {
        let s = Scale::new(50.0, 50.0, 10);
        assert!(s.sub_row(50.0) < s.sub_rows, "全平时应落在区间内而不是 NaN");
    }

    #[test]
    fn 蜡烛图画出内容且不越界() {
        let (r, mut b) = blank(40, 12);
        let scale = render_candles(r, &mut b, &bars(40));
        assert!(scale.is_some());
        let painted = b.content().iter().filter(|c| c.symbol() != " ").count();
        assert!(painted > 40, "应画出可观数量的格子，实际 {painted}");
    }

    #[test]
    fn 空数据不panic也不画() {
        let (r, mut b) = blank(30, 10);
        assert!(render_candles(r, &mut b, &[]).is_none());
        assert!(b.content().iter().all(|c| c.symbol() == " "));
    }

    #[test]
    fn 零尺寸区域不panic() {
        let (r, mut b) = blank(0, 0);
        assert!(render_candles(r, &mut b, &bars(10)).is_none());
    }

    #[test]
    fn 数据多于宽度时保留最新的() {
        // 看盘关心的是右端，装不下就丢最老的
        let (r, mut b) = blank(40, 12);
        render_candles(r, &mut b, &bars(200)).unwrap();
        let col_used = |x: u16| (0..12).any(|y| b[(x, y)].symbol() != " ");
        // 最后一根落在 width - step 处（末列是间隔），所以查最右两列
        assert!(col_used(38) || col_used(39), "最新一根没画出来");
    }

    #[test]
    fn 蜡烛之间有间隔() {
        // 挨在一起会糊成一片，必须留白
        let (r, mut b) = blank(60, 12);
        render_candles(r, &mut b, &bars(200)).unwrap();
        let col_used = |x: u16| (0..12).any(|y| b[(x, y)].symbol() != " ");
        let used: Vec<u16> = (0..60).filter(|x| col_used(*x)).collect();
        assert!(used.len() >= 15, "画出的蜡烛太少：{}", used.len());
        // 相邻两根之间至少隔一列
        for w in used.windows(2) {
            assert!(w[1] - w[0] >= 2, "第 {} 列和第 {} 列挨在一起了", w[0], w[1]);
        }
    }

    #[test]
    fn 间隔随空间自适应() {
        assert_eq!(step_for(100, 200), 2, "数据密集时用最小间隔");
        assert_eq!(step_for(100, 10), 3, "空间富余时把间隔拉开");
        assert_eq!(step_for(0, 0), 2, "退化输入不 panic");
    }

    #[test]
    fn 数据少于宽度时靠右对齐() {
        let (r, mut b) = blank(60, 12);
        render_candles(r, &mut b, &bars(10)).unwrap();
        let col_used = |x: u16| (0..12).any(|y| b[(x, y)].symbol() != " ");
        assert!(!col_used(0), "左侧应留空");
        assert!((55..60).any(col_used), "少量数据应靠右贴齐最新");
    }

    #[test]
    fn 涨用红跌用绿() {
        let up = Bar { ts: Utc.with_ymd_and_hms(2026,1,1,0,0,0).unwrap(), open: 10.0, high: 12.0, low: 9.0, close: 11.0, volume: 1.0 };
        let down = Bar { close: 9.5, ..up };
        let (r, mut b) = blank(6, 8);
        render_candles(r, &mut b, &[up, down]).unwrap();
        let colors: Vec<Option<Color>> = (0..6)
            .filter_map(|x| (0..8).find_map(|y| {
                let c = &b[(x, y)];
                if c.symbol() != " " { Some(c.style().fg) } else { None }
            }))
            .collect();
        assert_eq!(colors.len(), 2, "应画出两根蜡烛");
        assert_eq!(colors[0], Some(UP), "收高于开应为红");
        assert_eq!(colors[1], Some(DOWN), "收低于开应为绿");
    }

    #[test]
    fn macd面板不panic且画出柱子() {
        let closes: Vec<f64> = bars(120).iter().map(|b| b.close).collect();
        let m = crate::core::indicator::macd(&closes, 12, 26, 9);
        let (r, mut b) = blank(60, 8);
        render_macd(r, &mut b, &m);
        assert!(b.content().iter().any(|c| c.symbol() != " "));
    }

    #[test]
    fn macd柱子用八分块而不只是半块() {
        // 半块只有 ▀▄█ 三种，精度不够；八分块能表达 ▁▂▃▅▆▇ 等中间态
        let closes: Vec<f64> = (0..200).map(|i| (i as f64 * 0.13).sin() * 30.0 + 100.0).collect();
        let m = crate::core::indicator::macd(&closes, 12, 26, 9);
        let (r, mut b) = blank(120, 10);
        render_macd(r, &mut b, &m);
        let eighths: std::collections::HashSet<String> = b
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .filter(|s| "▁▂▃▅▆▇".contains(s.as_str()))
            .collect();
        assert!(!eighths.is_empty(), "没有出现任何八分块中间态，精度没提上去");
    }

    #[test]
    fn 指标折线用盲文() {
        let closes: Vec<f64> = (0..300).map(|i| (i as f64 * 0.07).sin() * 25.0 + 100.0).collect();
        let m = crate::core::indicator::macd(&closes, 12, 26, 9);
        let (r, mut b) = blank(120, 12);
        render_macd(r, &mut b, &m);
        let braille = b.content().iter().filter(|c| {
            c.symbol().chars().next().is_some_and(|ch| ('\u{2800}'..='\u{28FF}').contains(&ch))
        }).count();
        assert!(braille > 20, "盲文点太少（{braille}），折线没画出来");
    }

    #[test]
    fn kdj面板不panic且画出三条线() {
        let k = crate::core::indicator::kdj(&bars(200), 9, 3.0, 3.0);
        let (r, mut b) = blank(90, 10);
        render_kdj(r, &mut b, &k);
        let colors: std::collections::HashSet<_> = b
            .content()
            .iter()
            .filter(|c| c.symbol() != " ")
            .map(|c| format!("{:?}", c.style().fg))
            .collect();
        assert!(colors.len() >= 2, "K/D/J 应有不同颜色，实际 {colors:?}");
    }
}
