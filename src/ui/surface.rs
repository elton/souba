//! 可插拔的绘图表面。
//!
//! 这是分层设计的关节：**绘图逻辑只跟像素画布打交道，完全不知道后端是谁**。
//! 两个后端的差别只是画布分辨率和最终怎么落地：
//!
//! | 后端 | 每字符格分辨率 | 落地方式 |
//! |---|---|---|
//! | Kitty | 真实像素（如 10×20） | 转义序列直接贴位图 |
//! | 盲文 | 2×4 | 降采样成 `⣿` 字符写进 buffer |
//!
//! 所以「换后端」不需要改任何一行绘图代码。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::ui::braille::Braille;
use crate::ui::canvas::{Canvas, Rgb};
use crate::ui::kitty::{self, CellPixels};

/// 单张图的像素上限。约 1080p 的两倍，对 K 线来说远超肉眼分辨极限，
/// 但把压缩耗时压在可接受范围内。
const MAX_PIXELS: u64 = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// 真·位图。画质与 GUI 无异。
    Kitty(CellPixels),
    /// 盲文点阵。任何终端都能跑，每格只能一种颜色。
    Braille,
}

impl Backend {
    /// 启动时探测一次。探测不到就退回盲文 —— 宁可画质差，不可显示乱码。
    pub fn detect() -> Self {
        if !kitty::supported() {
            return Backend::Braille;
        }
        match kitty::detect_cell_pixels() {
            // 终端不报像素尺寸就没法算位图大小，只能降级
            None => Backend::Braille,
            Some(cp) => Backend::Kitty(cp),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Backend::Kitty(_) => "位图",
            Backend::Braille => "盲文",
        }
    }

    /// 给定字符格区域，该开多大的像素画布。
    ///
    /// 位图后端会按 `MAX_PIXELS` 封顶再等比缩小 —— 终端会把图拉伸回目标格数，
    /// 少量软化肉眼看不出，但压缩耗时是按像素数线性增长的，不封顶会卡。
    pub fn canvas_size(self, area: Rect) -> (u32, u32) {
        match self {
            Backend::Kitty(cp) => {
                let (w, h) = (
                    area.width as u32 * cp.w as u32,
                    area.height as u32 * cp.h as u32,
                );
                let total = w as u64 * h as u64;
                if total <= MAX_PIXELS {
                    return (w.max(1), h.max(1));
                }
                let k = (MAX_PIXELS as f64 / total as f64).sqrt();
                (
                    ((w as f64 * k) as u32).max(1),
                    ((h as f64 * k) as u32).max(1),
                )
            }
            Backend::Braille => (area.width as u32 * 2, area.height as u32 * 4),
        }
    }
}

/// 画一块区域：开画布 → 交给 `paint` 画 → 按后端落地。
///
/// Kitty 后端不写 ratatui 的 buffer，而是返回一段转义序列由调用方在
/// `terminal.draw()` 之后发出 —— 因为图像是叠在字符层之上的。
pub struct Surface {
    pub backend: Backend,
    /// Kitty 模式下待发送的转义序列
    pub escape: Option<String>,
    /// 本帧已用掉几个图像 slot。每个面板一个，各自独立删除与放置。
    slot: u32,
}

impl Surface {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            escape: None,
            slot: 0,
        }
    }

    pub fn draw<F: FnOnce(&mut Canvas)>(&mut self, area: Rect, buf: &mut Buffer, paint: F) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (w, h) = self.backend.canvas_size(area);
        if w == 0 || h == 0 {
            return;
        }
        let mut canvas = Canvas::new(w, h);
        paint(&mut canvas);

        match self.backend {
            Backend::Kitty(_) => {
                if self.slot >= kitty::MAX_SLOTS {
                    // slot 用完了宁可不画，也不要复用别的面板的 id 造成互相覆盖
                    return;
                }
                let seq =
                    kitty::encode(&canvas, area.x, area.y, area.width, area.height, self.slot);
                self.slot += 1;
                match &mut self.escape {
                    Some(acc) => acc.push_str(&seq),
                    None => self.escape = Some(seq),
                }
            }
            Backend::Braille => blit_braille(&canvas, area, buf),
        }
    }
}

/// 把像素画布降采样成盲文字符。
///
/// 每个字符格取 2×4 个像素；格内颜色取出现次数最多的那个 —— 盲文一格只能
/// 有一种颜色，取众数比「最后一个」稳定，线条交叉处不会随绘制顺序闪烁。
fn blit_braille(canvas: &Canvas, area: Rect, buf: &mut Buffer) {
    let mut br = Braille::new(area.width, area.height);
    for sy in 0..br.sub_height() {
        for sx in 0..br.sub_width() {
            if let Some(c) = canvas.color_at(sx as u32, sy as u32) {
                br.set(sx, sy, nearest_ansi(c));
            }
        }
    }
    br.blit(area, buf);
}

/// RGB 映射到 ratatui 的具名颜色。
///
/// 不用 `Color::Rgb` 是因为盲文路径本来就是给能力弱的终端兜底的，
/// 那些终端多半也不支持真彩色。
fn nearest_ansi(c: Rgb) -> Color {
    const PALETTE: [(Rgb, Color); 8] = [
        (Rgb(0, 0, 0), Color::Black),
        (Rgb(205, 49, 49), Color::Red),
        (Rgb(13, 188, 121), Color::Green),
        (Rgb(229, 229, 16), Color::Yellow),
        (Rgb(36, 114, 200), Color::Blue),
        (Rgb(188, 63, 188), Color::Magenta),
        (Rgb(17, 168, 205), Color::Cyan),
        (Rgb(229, 229, 229), Color::White),
    ];
    let d2 = |a: Rgb, b: Rgb| {
        let (dr, dg, db) = (
            a.0 as i32 - b.0 as i32,
            a.1 as i32 - b.1 as i32,
            a.2 as i32 - b.2 as i32,
        );
        dr * dr + dg * dg + db * db
    };
    PALETTE
        .iter()
        .min_by_key(|(p, _)| d2(*p, c))
        .map(|(_, col)| *col)
        .unwrap_or(Color::White)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Rgb = Rgb(255, 0, 0);

    #[test]
    fn 盲文后端每格二乘四() {
        let area = Rect::new(0, 0, 10, 5);
        assert_eq!(Backend::Braille.canvas_size(area), (20, 20));
    }

    #[test]
    fn 位图后端按真实格像素开画布() {
        let area = Rect::new(0, 0, 10, 5);
        let b = Backend::Kitty(CellPixels { w: 9, h: 18 });
        assert_eq!(b.canvas_size(area), (90, 90));
    }

    #[test]
    fn 超大区域按像素上限缩小() {
        // 实测 otty 每格 19x38，219x44 格 = 696 万像素，超过上限
        let area = Rect::new(0, 0, 219, 44);
        let (w, h) = Backend::Kitty(CellPixels { w: 19, h: 38 }).canvas_size(area);
        let total = w as u64 * h as u64;
        assert!(total <= MAX_PIXELS, "{w}x{h} = {total} 超过上限");
        // 等比缩放，宽高比不能变形
        let want = 219.0 * 19.0 / (44.0 * 38.0);
        let got = w as f64 / h as f64;
        assert!((want - got).abs() / want < 0.02, "宽高比变形了：{want:.3} vs {got:.3}");
    }

    #[test]
    fn 未超上限时不缩小保持原生清晰度() {
        let area = Rect::new(0, 0, 80, 24);
        let (w, h) = Backend::Kitty(CellPixels { w: 10, h: 20 }).canvas_size(area);
        assert_eq!((w, h), (800, 480), "没超上限就不该动");
    }

    #[test]
    fn 缩小后仍不为零() {
        let area = Rect::new(0, 0, 2000, 2000);
        let (w, h) = Backend::Kitty(CellPixels { w: 20, h: 40 }).canvas_size(area);
        assert!(w > 0 && h > 0);
    }

    #[test]
    fn 位图分辨率远高于盲文() {
        let area = Rect::new(0, 0, 140, 40);
        let (bw, bh) = Backend::Braille.canvas_size(area);
        let (kw, kh) = Backend::Kitty(CellPixels::FALLBACK).canvas_size(area);
        assert!(kw * kh > bw * bh * 10, "位图应有一个数量级以上的优势");
    }

    #[test]
    fn 零尺寸区域不panic() {
        let mut s = Surface::new(Backend::Braille);
        let r = Rect::new(0, 0, 0, 0);
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 4));
        s.draw(r, &mut buf, |c| c.set(0, 0, RED));
        assert!(s.escape.is_none());
    }

    #[test]
    fn 盲文后端写进buffer而不产生转义序列() {
        let r = Rect::new(0, 0, 8, 4);
        let mut buf = Buffer::empty(r);
        let mut s = Surface::new(Backend::Braille);
        s.draw(r, &mut buf, |c| {
            c.fill_rect(0, 0, 16, 16, RED);
        });
        assert!(s.escape.is_none(), "盲文后端不该产生转义序列");
        assert!(
            buf.content().iter().any(|c| c.symbol() != " "),
            "盲文后端应该写进 buffer"
        );
    }

    #[test]
    fn 位图后端产生转义序列而不写buffer() {
        let r = Rect::new(0, 0, 8, 4);
        let mut buf = Buffer::empty(r);
        let mut s = Surface::new(Backend::Kitty(CellPixels::FALLBACK));
        s.draw(r, &mut buf, |c| {
            c.fill_rect(0, 0, 20, 20, RED);
        });
        assert!(s.escape.is_some(), "位图后端应产生转义序列");
        assert!(
            buf.content().iter().all(|c| c.symbol() == " "),
            "位图后端不该写 buffer —— 图像是叠在字符层之上的"
        );
    }

    #[test]
    fn 同一份绘图代码两个后端都能跑() {
        // 这是分层设计的核心断言：paint 闭包完全不知道后端是谁
        let paint = |c: &mut Canvas| {
            c.line_aa(0.0, 0.0, c.w as f32 - 1.0, c.h as f32 - 1.0, RED, 1);
        };
        let r = Rect::new(0, 0, 20, 10);
        for backend in [Backend::Braille, Backend::Kitty(CellPixels::FALLBACK)] {
            let mut buf = Buffer::empty(r);
            let mut s = Surface::new(backend);
            s.draw(r, &mut buf, paint);
            let produced = s.escape.is_some() || buf.content().iter().any(|c| c.symbol() != " ");
            assert!(produced, "{:?} 后端什么都没画出来", backend);
        }
    }

    #[test]
    fn 颜色映射到最近的具名色() {
        assert_eq!(nearest_ansi(Rgb(255, 0, 0)), Color::Red);
        assert_eq!(nearest_ansi(Rgb(0, 255, 0)), Color::Green);
        assert_eq!(nearest_ansi(Rgb(250, 250, 250)), Color::White);
        assert_eq!(nearest_ansi(Rgb(5, 5, 5)), Color::Black);
    }

    #[test]
    fn 每个面板用独立的图像slot() {
        // 两个面板共用一个 id 会互相覆盖，只剩最后画的那个
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 40));
        let mut s = Surface::new(Backend::Kitty(CellPixels::FALLBACK));
        s.draw(Rect::new(0, 0, 4, 2), &mut buf, |c| c.set(0, 0, RED));
        s.draw(Rect::new(0, 5, 4, 2), &mut buf, |c| c.set(0, 0, RED));
        let esc = s.escape.unwrap();
        assert!(esc.contains("i=7301,"), "第一个面板应用 slot 0");
        assert!(esc.contains("i=7302,"), "第二个面板应用 slot 1");
    }

    #[test]
    fn 每次绘制先删掉自己上一帧的放置() {
        // a=T 每次都新建一个放置，光靠固定 id 防不住叠加
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 40));
        let mut s = Surface::new(Backend::Kitty(CellPixels::FALLBACK));
        s.draw(Rect::new(0, 0, 4, 2), &mut buf, |c| c.set(0, 0, RED));
        let esc = s.escape.unwrap();
        let del = esc.find("a=d,d=I,i=7301").expect("缺少删除指令");
        let put = esc.find("a=T").expect("缺少放置指令");
        assert!(del < put, "删除必须在放置之前，否则等于没删");
    }

    #[test]
    fn slot用完后不再复用他人的id() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 80));
        let mut s = Surface::new(Backend::Kitty(CellPixels::FALLBACK));
        for i in 0..(kitty::MAX_SLOTS + 3) {
            s.draw(Rect::new(0, (i * 2) as u16, 4, 2), &mut buf, |c| c.set(0, 0, RED));
        }
        let esc = s.escape.unwrap();
        let used = (0..kitty::MAX_SLOTS + 3)
            .filter(|i| esc.contains(&format!("i={},", 7301 + i)))
            .count();
        assert_eq!(used as u32, kitty::MAX_SLOTS, "不该超出 slot 上限");
    }

    #[test]
    fn 多次绘制的转义序列会累积() {
        let r = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 20));
        let mut s = Surface::new(Backend::Kitty(CellPixels::FALLBACK));
        s.draw(r, &mut buf, |c| c.set(0, 0, RED));
        let one = s.escape.as_ref().unwrap().len();
        s.draw(Rect::new(0, 5, 4, 2), &mut buf, |c| c.set(0, 0, RED));
        assert!(s.escape.as_ref().unwrap().len() > one, "第二块图没累积上去");
    }
}
