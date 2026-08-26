//! RGBA 像素画布。
//!
//! 绘图逻辑统一画到这里，再由后端决定怎么呈现：
//! 支持 Kitty 图形协议的终端直接贴位图，否则降采样成盲文点阵。
//! **两条路共用同一份绘图代码**，不是维护两套。
//!
//! 没有引入 `tiny-skia` 之类的绘图库 —— 这里实际只需要「填充矩形」和
//! 「抗锯齿直线」两个图元，为这两样背一个几百 KB 的依赖不划算。
//! 抗锯齿用 Xiaolin Wu 算法。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub struct Canvas {
    pub w: u32,
    pub h: u32,
    /// RGBA，行优先
    px: Vec<u8>,
}

impl Canvas {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            px: vec![0; (w as usize * h as usize) * 4],
        }
    }

    pub fn rgba(&self) -> &[u8] {
        &self.px
    }

    #[inline]
    fn idx(&self, x: u32, y: u32) -> usize {
        (y as usize * self.w as usize + x as usize) * 4
    }

    /// 按 alpha 混合一个像素。`a` 为 0 时什么都不做。
    pub fn blend(&mut self, x: i64, y: i64, c: Rgb, a: f32) {
        if x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 || a <= 0.0 {
            return;
        }
        let a = a.min(1.0);
        let i = self.idx(x as u32, y as u32);
        for (k, s) in [c.0, c.1, c.2].into_iter().enumerate() {
            let dst = self.px[i + k] as f32;
            self.px[i + k] = (dst * (1.0 - a) + s as f32 * a).round() as u8;
        }
        // alpha 取较大者，保证画过的地方不透明
        let old = self.px[i + 3] as f32 / 255.0;
        self.px[i + 3] = ((old.max(a)) * 255.0).round() as u8;
    }

    pub fn set(&mut self, x: i64, y: i64, c: Rgb) {
        self.blend(x, y, c, 1.0);
    }

    /// 是否被画过（alpha > 0）。降采样成盲文时用它判断这个像素要不要点亮。
    pub fn is_set(&self, x: u32, y: u32) -> bool {
        if x >= self.w || y >= self.h {
            return false;
        }
        self.px[self.idx(x, y) + 3] > 0
    }

    pub fn color_at(&self, x: u32, y: u32) -> Option<Rgb> {
        if x >= self.w || y >= self.h || !self.is_set(x, y) {
            return None;
        }
        let i = self.idx(x, y);
        Some(Rgb(self.px[i], self.px[i + 1], self.px[i + 2]))
    }

    pub fn fill_rect(&mut self, x: i64, y: i64, w: i64, h: i64, c: Rgb) {
        for yy in y..(y + h) {
            for xx in x..(x + w) {
                self.set(xx, yy, c);
            }
        }
    }

    /// 竖直线段，用于蜡烛影线。整数坐标，不需要抗锯齿。
    pub fn v_line(&mut self, x: i64, y0: i64, y1: i64, c: Rgb) {
        let (a, b) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
        for y in a..=b {
            self.set(x, y, c);
        }
    }

    /// Xiaolin Wu 抗锯齿直线。`width` 是线宽（像素），≥2 时向两侧加粗。
    pub fn line_aa(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: Rgb, width: u32) {
        let steep = (y1 - y0).abs() > (x1 - x0).abs();
        let (mut x0, mut y0, mut x1, mut y1) = if steep {
            (y0, x0, y1, x1)
        } else {
            (x0, y0, x1, y1)
        };
        if x0 > x1 {
            std::mem::swap(&mut x0, &mut x1);
            std::mem::swap(&mut y0, &mut y1);
        }
        let dx = x1 - x0;
        let dy = y1 - y0;
        let grad = if dx.abs() < f32::EPSILON { 1.0 } else { dy / dx };

        let spread = width.max(1) as i64 - 1;
        let plot = |cv: &mut Canvas, x: i64, y: i64, a: f32| {
            for o in -(spread / 2)..=(spread - spread / 2) {
                if steep {
                    cv.blend(y + o, x, c, a);
                } else {
                    cv.blend(x, y + o, c, a);
                }
            }
        };

        let mut inter = y0 + grad * (x0.round() - x0);
        for x in (x0.round() as i64)..=(x1.round() as i64) {
            let fy = inter.floor();
            let frac = inter - fy;
            plot(self, x, fy as i64, 1.0 - frac);
            plot(self, x, fy as i64 + 1, frac);
            inter += grad;
        }
    }

    /// 折线：逐段连起来，端点共享
    pub fn polyline(&mut self, pts: &[(f32, f32)], c: Rgb, width: u32) {
        for w in pts.windows(2) {
            self.line_aa(w[0].0, w[0].1, w[1].0, w[1].1, c, width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Rgb = Rgb(255, 0, 0);

    #[test]
    fn 新画布全透明() {
        let c = Canvas::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                assert!(!c.is_set(x, y));
            }
        }
    }

    #[test]
    fn 画点后可读回颜色() {
        let mut c = Canvas::new(4, 4);
        c.set(1, 2, RED);
        assert!(c.is_set(1, 2));
        assert_eq!(c.color_at(1, 2), Some(RED));
        assert_eq!(c.color_at(0, 0), None);
    }

    #[test]
    fn 越界写入被忽略不panic() {
        let mut c = Canvas::new(4, 4);
        c.set(-1, 0, RED);
        c.set(0, -1, RED);
        c.set(999, 999, RED);
        assert!(c.rgba().iter().all(|b| *b == 0));
    }

    #[test]
    fn 填充矩形覆盖预期范围() {
        let mut c = Canvas::new(10, 10);
        c.fill_rect(2, 3, 4, 2, RED);
        assert!(c.is_set(2, 3) && c.is_set(5, 4));
        assert!(!c.is_set(1, 3), "左边界外不该被画");
        assert!(!c.is_set(6, 3), "右边界外不该被画");
        assert!(!c.is_set(2, 5), "下边界外不该被画");
    }

    #[test]
    fn 竖线两端点都画到() {
        let mut c = Canvas::new(6, 10);
        c.v_line(3, 2, 7, RED);
        for y in 2..=7 {
            assert!(c.is_set(3, y), "第 {y} 行没画");
        }
        assert!(!c.is_set(3, 1) && !c.is_set(3, 8));
    }

    #[test]
    fn 竖线端点顺序无关() {
        let mut a = Canvas::new(6, 10);
        let mut b = Canvas::new(6, 10);
        a.v_line(3, 2, 7, RED);
        b.v_line(3, 7, 2, RED);
        assert_eq!(a.rgba(), b.rgba());
    }

    #[test]
    fn 抗锯齿斜线每一列都有像素() {
        let mut c = Canvas::new(40, 40);
        c.line_aa(0.0, 0.0, 39.0, 39.0, RED, 1);
        for x in 0..40u32 {
            let any = (0..40u32).any(|y| c.is_set(x, y));
            assert!(any, "第 {x} 列断了");
        }
    }

    #[test]
    fn 抗锯齿产生中间灰度而不是纯色() {
        // 这是「抗锯齿」的定义：斜线边缘应该有部分透明的像素
        let mut c = Canvas::new(40, 20);
        c.line_aa(0.0, 0.0, 39.0, 19.0, RED, 1);
        let mut partial = 0;
        for y in 0..20u32 {
            for x in 0..40u32 {
                if let Some(Rgb(r, _, _)) = c.color_at(x, y)
                    && r > 0
                    && r < 255
                {
                    partial += 1;
                }
            }
        }
        assert!(partial > 5, "没有中间灰度，说明没有抗锯齿（实际 {partial} 个）");
    }

    #[test]
    fn 线宽大于一时变粗() {
        let count = |w: u32| {
            let mut c = Canvas::new(40, 40);
            c.line_aa(0.0, 20.0, 39.0, 20.0, RED, w);
            (0..40u32)
                .flat_map(|y| (0..40u32).map(move |x| (x, y)))
                .filter(|(x, y)| c.is_set(*x, *y))
                .count()
        };
        assert!(count(3) > count(1), "线宽 3 应该比线宽 1 画到更多像素");
    }

    #[test]
    fn 水平线不panic() {
        let mut c = Canvas::new(20, 20);
        c.line_aa(0.0, 10.0, 19.0, 10.0, RED, 1);
        assert!(c.is_set(10, 10) || c.is_set(10, 11));
    }

    #[test]
    fn 单点线段不panic() {
        let mut c = Canvas::new(20, 20);
        c.line_aa(5.0, 5.0, 5.0, 5.0, RED, 1);
    }

    #[test]
    fn 折线连续() {
        let mut c = Canvas::new(60, 30);
        let pts: Vec<(f32, f32)> = (0..30).map(|i| (i as f32 * 2.0, 15.0 + (i as f32 * 0.5).sin() * 10.0)).collect();
        c.polyline(&pts, RED, 1);
        for x in 0..58u32 {
            assert!((0..30u32).any(|y| c.is_set(x, y)), "折线第 {x} 列断了");
        }
    }
}
