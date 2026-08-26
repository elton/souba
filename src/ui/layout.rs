use ratatui::layout::{Constraint, Layout, Rect};

/// 能正常显示所需的最小尺寸。低于此显示提示而不是画崩。
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breakpoint {
    /// ≥120 列：自选股 + K线/指标 左右分栏
    Wide,
    /// 80–119 列：上下分栏
    Medium,
    /// 40–79 列：只显示自选股精简列
    Narrow,
    /// 太小，显示提示
    TooSmall,
}

impl Breakpoint {
    pub fn of(area: Rect) -> Self {
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            return Breakpoint::TooSmall;
        }
        if area.width >= 120 {
            Breakpoint::Wide
        } else if area.width >= 80 {
            Breakpoint::Medium
        } else {
            Breakpoint::Narrow
        }
    }
}

pub struct Panes {
    pub header: Rect,
    pub body: Rect,
    pub footer: Rect,
}

/// 顶栏 1 行、底栏 1 行、中间全部给 body —— 用 Fill 而不是固定高度，
/// 这样终端尺寸变化时自动铺满。
pub fn split(area: Rect) -> Panes {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);
    Panes {
        header,
        body,
        footer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    #[test]
    fn 宽屏断点() {
        assert_eq!(Breakpoint::of(rect(160, 50)), Breakpoint::Wide);
        assert_eq!(Breakpoint::of(rect(120, 30)), Breakpoint::Wide);
    }

    #[test]
    fn 中屏断点() {
        assert_eq!(Breakpoint::of(rect(119, 30)), Breakpoint::Medium);
        assert_eq!(Breakpoint::of(rect(80, 24)), Breakpoint::Medium);
    }

    #[test]
    fn 窄屏断点() {
        assert_eq!(Breakpoint::of(rect(79, 24)), Breakpoint::Narrow);
        assert_eq!(Breakpoint::of(rect(60, 24)), Breakpoint::Narrow);
    }

    #[test]
    fn 过小时明确报出而不是画崩() {
        assert_eq!(Breakpoint::of(rect(60, 23)), Breakpoint::TooSmall);
        assert_eq!(Breakpoint::of(rect(39, 40)), Breakpoint::TooSmall);
    }

    #[test]
    fn 三段布局铺满且不重叠() {
        let area = rect(120, 40);
        let p = split(area);
        assert_eq!(p.header.height, 1);
        assert_eq!(p.footer.height, 1);
        // 铺满：三段高度之和等于总高
        assert_eq!(p.header.height + p.body.height + p.footer.height, area.height);
        // 不重叠
        assert_eq!(p.body.y, p.header.y + p.header.height);
        assert_eq!(p.footer.y, p.body.y + p.body.height);
    }

    #[test]
    fn 布局随终端尺寸变化而铺满() {
        for (w, h) in [(80u16, 24u16), (120, 40), (200, 60), (100, 30)] {
            let area = rect(w, h);
            let p = split(area);
            assert_eq!(p.header.width, w, "{w}x{h} header 没铺满");
            assert_eq!(p.body.width, w, "{w}x{h} body 没铺满");
            assert_eq!(p.header.height + p.body.height + p.footer.height, h);
        }
    }
}
