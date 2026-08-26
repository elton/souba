//! 盲文点阵画布。
//!
//! 每个字符单元是 2×4 个点，所以一块 W×H 的区域有 2W×4H 的作图精度 ——
//! 纵向是半块字符（`▀▄█`）的 4 倍。
//!
//! 它只负责「把点阵落成字符」；连线、抗锯齿这些在 `canvas` 里做完，
//! 这边只是 `surface` 降采样的落地端。
//!
//! Unicode 盲文点位到比特的映射（U+2800 + mask）：
//!
//! ```text
//!   列0  列1
//!   0x01 0x08   ← 行0
//!   0x02 0x10   ← 行1
//!   0x04 0x20   ← 行2
//!   0x40 0x80   ← 行3
//! ```

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

const DOTS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

pub struct Braille {
    w: u16,
    h: u16,
    /// 每个字符单元的点掩码
    mask: Vec<u8>,
    /// 每个字符单元的颜色。后写入的颜色覆盖先前的。
    color: Vec<Option<Color>>,
}

impl Braille {
    pub fn new(w: u16, h: u16) -> Self {
        Self {
            w,
            h,
            mask: vec![0; w as usize * h as usize],
            color: vec![None; w as usize * h as usize],
        }
    }

    pub fn sub_width(&self) -> u16 {
        self.w.saturating_mul(2)
    }
    pub fn sub_height(&self) -> u16 {
        self.h.saturating_mul(4)
    }

    /// 点亮一个子像素。越界静默忽略。
    pub fn set(&mut self, sx: u16, sy: u16, color: Color) {
        if sx >= self.sub_width() || sy >= self.sub_height() {
            return;
        }
        let (cx, cy) = (sx / 2, sy / 4);
        let idx = cy as usize * self.w as usize + cx as usize;
        self.mask[idx] |= DOTS[(sx % 2) as usize][(sy % 4) as usize];
        self.color[idx] = Some(color);
    }

    /// 把画布刷到终端缓冲区。只写非空单元，不覆盖已有内容之外的地方。
    pub fn blit(&self, area: Rect, buf: &mut Buffer) {
        for cy in 0..self.h.min(area.height) {
            for cx in 0..self.w.min(area.width) {
                let idx = cy as usize * self.w as usize + cx as usize;
                let m = self.mask[idx];
                if m == 0 {
                    continue;
                }
                let ch = char::from_u32(0x2800 + m as u32).unwrap_or('⠿');
                let mut style = Style::default();
                if let Some(c) = self.color[idx] {
                    style = style.fg(c);
                }
                buf[(area.x + cx, area.y + cy)]
                    .set_char(ch)
                    .set_style(style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 子像素精度是二乘四() {
        let b = Braille::new(10, 5);
        assert_eq!(b.sub_width(), 20);
        assert_eq!(b.sub_height(), 20);
    }

    #[test]
    fn 单点映射到正确的比特() {
        let mut b = Braille::new(1, 1);
        b.set(0, 0, Color::White);
        assert_eq!(b.mask[0], 0x01, "左上角应是 dot1");
        let mut b = Braille::new(1, 1);
        b.set(1, 3, Color::White);
        assert_eq!(b.mask[0], 0x80, "右下角应是 dot8");
    }

    #[test]
    fn 同格多点合并成一个字符() {
        let mut b = Braille::new(1, 1);
        for sy in 0..4 {
            b.set(0, sy, Color::White);
            b.set(1, sy, Color::White);
        }
        assert_eq!(b.mask[0], 0xFF, "八个点全亮应是满格");
        let r = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(r);
        b.blit(r, &mut buf);
        assert_eq!(buf[(0u16, 0u16)].symbol(), "⣿");
    }

    #[test]
    fn 越界写入被静默忽略不panic() {
        let mut b = Braille::new(2, 2);
        b.set(999, 999, Color::White);
        b.set(0, 999, Color::White);
        assert!(b.mask.iter().all(|m| *m == 0));
    }

    #[test]
    fn 空画布不写任何格子() {
        let b = Braille::new(4, 3);
        let r = Rect::new(0, 0, 4, 3);
        let mut buf = Buffer::empty(r);
        b.blit(r, &mut buf);
        assert!(buf.content().iter().all(|c| c.symbol() == " "));
    }
}
