//! K 线视口：决定「显示哪一段、显示多少根」。
//!
//! `span` 是可视根数（越小越放大），`offset` 是右边缘距离最新一根有多远
//! （0 = 贴着最新）。两者一起定位出 `[lo, hi)` 这个窗口。

/// 可视根数的上下限。太少看不出形态，太多每根不足 1 像素。
pub const MIN_SPAN: usize = 20;
/// 默认窗口。和主流看盘软件的日线默认视野接近。
pub const DEFAULT_SPAN: usize = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub span: usize,
    pub offset: usize,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            span: DEFAULT_SPAN,
            offset: 0,
        }
    }
}

impl Viewport {
    /// 算出实际的窗口 `[lo, hi)`，已按总根数夹紧
    pub fn range(&self, total: usize) -> (usize, usize) {
        if total == 0 {
            return (0, 0);
        }
        let span = self.span.clamp(MIN_SPAN.min(total), total);
        let offset = self.offset.min(total - span);
        let hi = total - offset;
        (hi - span, hi)
    }

    /// 放大：可视根数减少。以窗口右边缘为锚点，缩放时右边不动。
    pub fn zoom_in(&mut self, total: usize) {
        let next = (self.span as f64 / 1.3).round() as usize;
        self.span = next.max(MIN_SPAN.min(total.max(1)));
        self.clamp(total);
    }

    /// 缩小：可视根数增加，直到看到全部
    pub fn zoom_out(&mut self, total: usize) {
        let next = (self.span as f64 * 1.3).round() as usize + 1;
        self.span = next.min(total.max(MIN_SPAN));
        self.clamp(total);
    }

    /// 向左（更早）滚动。步长按当前视野的比例走 —— 放大时走得细，缩小时走得快。
    pub fn pan_left(&mut self, total: usize) {
        self.offset = self.offset.saturating_add(self.step());
        self.clamp(total);
    }

    /// 向右（更新）滚动
    pub fn pan_right(&mut self, total: usize) {
        self.offset = self.offset.saturating_sub(self.step());
        self.clamp(total);
    }

    /// 跳到最新一根
    pub fn jump_latest(&mut self) {
        self.offset = 0;
    }

    /// 跳到最早一根
    pub fn jump_oldest(&mut self, total: usize) {
        self.offset = total.saturating_sub(self.span);
        self.clamp(total);
    }

    fn step(&self) -> usize {
        (self.span / 6).max(1)
    }

    fn clamp(&mut self, total: usize) {
        if total == 0 {
            self.offset = 0;
            return;
        }
        self.span = self.span.clamp(MIN_SPAN.min(total), total);
        self.offset = self.offset.min(total - self.span);
    }

    /// 是否贴在最新一根上。UI 据此提示用户「已在最新」。
    pub fn at_latest(&self) -> bool {
        self.offset == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认贴着最新() {
        let v = Viewport::default();
        assert_eq!(v.range(1000), (750, 1000));
        assert!(v.at_latest());
    }

    #[test]
    fn 总数少于窗口时显示全部() {
        let v = Viewport::default();
        assert_eq!(v.range(80), (0, 80));
    }

    #[test]
    fn 空数据不panic() {
        assert_eq!(Viewport::default().range(0), (0, 0));
    }

    #[test]
    fn 放大后可视根数变少() {
        let mut v = Viewport::default();
        let before = v.range(1000).1 - v.range(1000).0;
        v.zoom_in(1000);
        let after = v.range(1000).1 - v.range(1000).0;
        assert!(after < before, "{before} -> {after}");
    }

    #[test]
    fn 缩小后可视根数变多() {
        let mut v = Viewport::default();
        v.zoom_out(1000);
        assert!(v.range(1000).1 - v.range(1000).0 > DEFAULT_SPAN);
    }

    #[test]
    fn 放大有下限不会缩到零() {
        let mut v = Viewport::default();
        for _ in 0..100 {
            v.zoom_in(1000);
        }
        let (lo, hi) = v.range(1000);
        assert!(hi - lo >= MIN_SPAN, "缩到了 {} 根", hi - lo);
    }

    #[test]
    fn 缩小上限是全部数据() {
        let mut v = Viewport::default();
        for _ in 0..100 {
            v.zoom_out(1000);
        }
        assert_eq!(v.range(1000), (0, 1000), "应该正好看到全部");
    }

    #[test]
    fn 缩放锚定右边缘() {
        let mut v = Viewport::default();
        let right = v.range(1000).1;
        v.zoom_in(1000);
        assert_eq!(v.range(1000).1, right, "缩放时右边缘不该移动");
    }

    #[test]
    fn 向左滚动窗口后移() {
        let mut v = Viewport::default();
        let (lo0, _) = v.range(1000);
        v.pan_left(1000);
        let (lo1, _) = v.range(1000);
        assert!(lo1 < lo0, "向左应看到更早的数据");
        assert!(!v.at_latest());
    }

    #[test]
    fn 向右滚回最新后不再越界() {
        let mut v = Viewport::default();
        v.pan_left(1000);
        v.pan_left(1000);
        for _ in 0..20 {
            v.pan_right(1000);
        }
        assert_eq!(v.range(1000).1, 1000, "应停在最新");
        assert!(v.at_latest());
    }

    #[test]
    fn 向左滚到头不越界() {
        let mut v = Viewport::default();
        for _ in 0..500 {
            v.pan_left(1000);
        }
        assert_eq!(v.range(1000).0, 0, "应停在最早");
    }

    #[test]
    fn 滚动步长随缩放变化() {
        let mut fine = Viewport { span: 30, offset: 0 };
        let mut coarse = Viewport { span: 600, offset: 0 };
        fine.pan_left(2000);
        coarse.pan_left(2000);
        assert!(coarse.offset > fine.offset, "视野大时应滚得更快");
    }

    #[test]
    fn 跳转到首尾() {
        let mut v = Viewport::default();
        v.jump_oldest(1000);
        assert_eq!(v.range(1000).0, 0);
        v.jump_latest();
        assert_eq!(v.range(1000).1, 1000);
    }

    #[test]
    fn 窗口永远落在合法区间内() {
        // 各种操作乱序组合后，range 都必须是合法且非空的
        let mut v = Viewport::default();
        for total in [1usize, 5, 19, 20, 100, 1500] {
            for op in 0..40 {
                match op % 6 {
                    0 => v.zoom_in(total),
                    1 => v.zoom_out(total),
                    2 => v.pan_left(total),
                    3 => v.pan_right(total),
                    4 => v.jump_oldest(total),
                    _ => v.jump_latest(),
                }
                let (lo, hi) = v.range(total);
                assert!(lo <= hi, "总数 {total}：lo={lo} > hi={hi}");
                assert!(hi <= total, "总数 {total}：hi={hi} 越界");
                assert!(hi > lo || total == 0, "总数 {total}：窗口不该为空");
            }
        }
    }
}
