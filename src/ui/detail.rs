//! 个股详情：K 线 + 指标面板。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::core::bar::{Bar, Timeframe};
use crate::core::indicator::{kdj, macd};
use crate::core::quote::Quote;
use crate::core::symbol::Symbol;
use crate::ui::paint;
use crate::ui::surface::Surface;

/// 详情面板要显示的指标
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndicatorKind {
    Macd,
    Kdj,
}

impl IndicatorKind {
    pub fn label(self) -> &'static str {
        match self {
            IndicatorKind::Macd => "MACD(12,26,9)",
            IndicatorKind::Kdj => "KDJ(9,3,3)",
        }
    }
    pub fn next(self) -> Self {
        match self {
            IndicatorKind::Macd => IndicatorKind::Kdj,
            IndicatorKind::Kdj => IndicatorKind::Macd,
        }
    }
}

/// 详情视图的数据装载状态。**「加载中」「无数据源」「出错」必须区分** ——
/// 三者对用户意味着完全不同的事，混成一个空白面板是误导。
#[derive(Debug, Clone)]
pub enum BarState {
    Loading,
    Ready(Vec<Bar>),
    Unsupported(String),
    Failed(String),
}

pub struct DetailView<'a> {
    pub symbol: &'a Symbol,
    pub quote: Option<&'a Quote>,
    pub timeframe: Timeframe,
    pub indicator: IndicatorKind,
    pub bars: &'a BarState,
}

pub fn render(frame: &mut Frame, area: Rect, v: &DetailView, surface: &mut Surface) {
    let [head, chart_area, ind_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(3),
        Constraint::Fill(1),
    ])
    .areas(area);

    render_head(frame, head, v);

    let bars = match v.bars {
        BarState::Ready(b) if !b.is_empty() => b,
        other => {
            render_placeholder(frame, chart_area, ind_area, other, v);
            return;
        }
    };

    // 右侧留出独立的价格轴栏位 —— 否则刻度文字会盖掉蜡烛
    const AXIS_W: u16 = 10;
    let plot_w = chart_area.width.saturating_sub(AXIS_W + 2);
    let (step, _) = paint::layout(
        surface.backend.canvas_size(Rect::new(0, 0, plot_w, 1)).0,
        bars.len(),
    );
    let cw = surface.backend.canvas_size(Rect::new(0, 0, plot_w, 1)).0;
    let shown = ((cw / step.max(1)) as usize).min(bars.len());

    let chart_block = Block::default().borders(Borders::ALL).title(format!(
        " {} · 显示 {} / 共 {} 根 · {} ",
        v.timeframe.label(),
        shown,
        bars.len(),
        surface.backend.label()
    ));
    let inner = chart_block.inner(chart_area);
    frame.render_widget(chart_block, chart_area);

    let axis_w = AXIS_W.min(inner.width);
    let plot = Rect::new(inner.x, inner.y, inner.width.saturating_sub(axis_w), inner.height);
    let axis = Rect::new(inner.x + plot.width, inner.y, axis_w, inner.height);

    let mut vscale = None;
    surface.draw(plot, frame.buffer_mut(), |c| {
        vscale = paint::candles(c, bars);
    });
    if let Some(vs) = vscale {
        render_price_axis(frame, axis, vs);
    }

    let ind_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", v.indicator.label()));
    let ind_inner = ind_block.inner(ind_area);
    frame.render_widget(ind_block, ind_area);
    // 指标面板与 K 线共用同一条右侧留白，纵向刻度对齐
    let ind_plot = Rect::new(
        ind_inner.x,
        ind_inner.y,
        ind_inner.width.saturating_sub(axis_w),
        ind_inner.height,
    );
    let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
    let n = bars.len();
    match v.indicator {
        IndicatorKind::Macd => {
            let m = macd(&closes, 12, 26, 9);
            surface.draw(ind_plot, frame.buffer_mut(), |c| paint::macd(c, &m, n));
        }
        IndicatorKind::Kdj => {
            let k = kdj(bars, 9, 3.0, 3.0);
            surface.draw(ind_plot, frame.buffer_mut(), |c| paint::kdj(c, &k, n));
        }
    }
}

/// 价格刻度。画在专属栏位里，不与蜡烛争地盘。
fn render_price_axis(frame: &mut Frame, area: Rect, scale: paint::VScale) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // 行数够就画 5 档，不够就退到首尾两档
    let ticks = if area.height >= 9 { 5 } else { 2 };
    let style = Style::default().fg(Color::DarkGray);
    for i in 0..ticks {
        let frac = i as f64 / (ticks - 1).max(1) as f64;
        let price = scale.max - (scale.max - scale.min) * frac;
        let y = area.y + ((area.height - 1) as f64 * frac).round() as u16;
        frame.render_widget(
            Paragraph::new(Span::styled(format!("{price:.2}"), style)),
            Rect::new(area.x + 1, y, area.width.saturating_sub(1), 1),
        );
    }
}

fn render_head(frame: &mut Frame, area: Rect, v: &DetailView) {
    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut spans = vec![Span::styled(
        format!(" {} ", v.symbol),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if let Some(q) = v.quote {
        let color = if q.is_up() { Color::Red } else { Color::Green };
        spans.push(Span::styled(format!("{}  ", q.name), Style::default()));
        spans.push(Span::styled(
            format!("{}  ", q.last),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("{}  {:.2}%  ", q.change, q.change_pct),
            Style::default().fg(color),
        ));
        spans.push(Span::styled(
            format!("高 {}  低 {}", q.high, q.low),
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::styled(
            "报价加载中…",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn render_placeholder(
    frame: &mut Frame,
    chart: Rect,
    ind: Rect,
    state: &BarState,
    v: &DetailView,
) {
    let (msg, color) = match state {
        BarState::Loading => (format!("正在加载 {} 数据…", v.timeframe.label()), Color::DarkGray),
        BarState::Ready(_) => ("该周期没有数据".to_string(), Color::DarkGray),
        BarState::Unsupported(why) => (why.clone(), Color::Yellow),
        BarState::Failed(why) => (format!("加载失败：{why}"), Color::Red),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", v.timeframe.label()));
    let inner = block.inner(chart);
    frame.render_widget(block, chart);
    frame.render_widget(
        Paragraph::new(Span::styled(msg, Style::default().fg(color))),
        Rect::new(inner.x, inner.y + inner.height / 2, inner.width, 1),
    );
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", v.indicator.label())),
        ind,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 指标在两者之间循环() {
        assert_eq!(IndicatorKind::Macd.next(), IndicatorKind::Kdj);
        assert_eq!(IndicatorKind::Kdj.next(), IndicatorKind::Macd);
    }

    #[test]
    fn 三种缺数据状态互不相同() {
        // 加载中 / 无数据源 / 出错 对用户意味着完全不同的事，
        // 不能都渲染成同一个空白面板
        let states = [
            BarState::Loading,
            BarState::Unsupported("日股无源".into()),
            BarState::Failed("超时".into()),
        ];
        let labels: std::collections::HashSet<String> = states
            .iter()
            .map(|s| format!("{:?}", std::mem::discriminant(s)))
            .collect();
        assert_eq!(labels.len(), 3);
    }
}

#[cfg(test)]
mod axis_tests {
    use super::*;
    use crate::core::bar::Bar;
    use chrono::{TimeZone, Utc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn bars(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| {
                let c = (i as f64 * 0.2).sin() * 50.0 + 1300.0;
                Bar {
                    ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
                        + chrono::Duration::days(i as i64),
                    open: c - 2.0,
                    high: c + 6.0,
                    low: c - 6.0,
                    close: c,
                    volume: 1.0,
                }
            })
            .collect()
    }

    fn draw(w: u16, h: u16, n: usize) -> ratatui::buffer::Buffer {
        draw_with(w, h, n, crate::ui::surface::Backend::Braille)
    }

    fn draw_with(
        w: u16,
        h: u16,
        n: usize,
        backend: crate::ui::surface::Backend,
    ) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut surface = Surface::new(backend);
        let sym = crate::core::symbol::Symbol::parse("CN:600519").unwrap();
        let state = BarState::Ready(bars(n));
        term.draw(|f| {
            render(
                f,
                Rect::new(0, 0, w, h),
                &DetailView {
                    symbol: &sym,
                    quote: None,
                    timeframe: Timeframe::Day,
                    indicator: IndicatorKind::Macd,
                    bars: &state,
                },
                &mut surface,
            )
        })
        .unwrap();
        term.backend().buffer().clone()
    }

    /// ratatui 对宽字符是首格存字符、次格留空，朴素拼接会得到「显 示」这种带空隙的
    /// 结果。必须按字符显示宽度跳过填充格。
    fn text(buf: &ratatui::buffer::Buffer, w: u16, h: u16) -> String {
        use unicode_width::UnicodeWidthStr;
        (0..h)
            .map(|y| {
                let mut out = String::new();
                let mut x = 0u16;
                while x < w {
                    let sym = buf[(x, y)].symbol();
                    out.push_str(sym);
                    x += sym.width().max(1) as u16;
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn 价格刻度不覆盖蜡烛() {
        let (w, h) = (120u16, 40u16);
        let buf = draw(w, h, 300);
        // 布局：头部 3 行 + K线区 Fill(3) + 指标区 Fill(1)。
        // 只查 K线区 —— 指标面板同样用块字符画柱子，把它算进来是误判。
        let chart_rows = 3..(3 + (h - 3) * 3 / 4);
        for y in chart_rows {
            for x in (w - 10)..w {
                let sym = buf[(x, y)].symbol();
                assert!(
                    !matches!(sym, "█" | "▀" | "▄"),
                    "价格轴栏位 ({x},{y}) 出现了蜡烛块 {sym:?}，说明两者仍在抢地盘"
                );
            }
        }
    }

    #[test]
    fn 标题区分显示根数与加载根数() {
        let t = text(&draw(120, 40, 1500), 120, 40);
        assert!(t.contains("共 1500 根"), "应说明总共加载了多少");
        assert!(t.contains("显示 "), "应说明实际显示了多少 —— 只说 1500 会误导");
        assert!(!t.contains("显示 1500"), "120 列画不下 1500 根，显示数不该等于总数");
    }

    #[test]
    fn 数据少于宽度时显示数等于总数() {
        let t = text(&draw(200, 40, 50), 200, 40);
        assert!(t.contains("显示 50 / 共 50 根"), "画得下就该显示全部");
    }

    #[test]
    fn 蜡烛有间隔后显示根数要按间隔折算() {
        // 每根占 2-3 列，所以 120 列画不下 120 根
        let t = text(&draw(120, 40, 1500), 120, 40);
        assert!(!t.contains("显示 1500"), "显示数不该等于总数");
        let shown: usize = t
            .split("显示 ")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .expect("标题里应有显示根数");
        assert!(shown > 0 && shown < 120, "120 列按 2-3 列一根，显示数应远小于列数，实际 {shown}");
    }

    #[test]
    fn 窄面板退到两档刻度也不panic() {
        let _ = draw(60, 12, 100);
        let _ = draw(42, 24, 100);
    }

    #[test]
    fn 位图后端下详情屏不写字符层但产生转义序列() {
        use crate::ui::kitty::CellPixels;
        use crate::ui::surface::Backend;
        // 位图模式下 K线区应是空的（图叠在字符层之上），
        // 但边框、标题、刻度这些字符仍然要画
        let buf = draw_with(140, 40, 300, Backend::Kitty(CellPixels::FALLBACK));
        let t = text(&buf, 140, 40);
        assert!(t.contains("位图"), "标题应标明当前后端：{}", &t[..120.min(t.len())]);
        assert!(t.contains("日线"), "边框标题仍要画");
        let blocks = buf.content().iter().filter(|c| matches!(c.symbol(), "█" | "▀" | "▄")).count();
        assert_eq!(blocks, 0, "位图模式不该往字符层画块字符");
    }

    #[test]
    fn 盲文后端下标题标明后端() {
        let t = text(&draw(140, 40, 300), 140, 40);
        assert!(t.contains("盲文"), "标题应标明当前后端");
    }
}
