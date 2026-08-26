//! 个股详情：K 线 + 指标面板。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::core::bar::{Bar, Timeframe};
use crate::core::indicator::{Kdj, Macd, kdj, macd};
use crate::core::quote::Quote;
use crate::core::symbol::Symbol;
use crate::ui::paint;
use crate::ui::surface::Surface;
use crate::ui::timeaxis;
use crate::ui::viewport::Viewport;

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
    pub viewport: Viewport,
    /// 当前绘图后端的名字，显示在标题里
    pub surface_label: &'static str,
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
    let (lo, hi) = v.viewport.range(bars.len());
    let window = &bars[lo..hi];

    let pos = if v.viewport.at_latest() {
        "最新".to_string()
    } else {
        format!("←{} 根前", bars.len() - hi)
    };
    let chart_block = Block::default().borders(Borders::ALL).title(format!(
        " {} · {} 根 / 共 {} · {} · {} ",
        v.timeframe.label(),
        window.len(),
        bars.len(),
        pos,
        v.surface_label
    ));
    let inner = chart_block.inner(chart_area);
    frame.render_widget(chart_block, chart_area);

    let axis_w = AXIS_W.min(inner.width);
    // 底部留一行给时间刻度
    let time_h = u16::from(inner.height > 4);
    let body_h = inner.height.saturating_sub(time_h);
    let plot = Rect::new(inner.x, inner.y, inner.width.saturating_sub(axis_w), body_h);
    let axis = Rect::new(inner.x + plot.width, inner.y, axis_w, body_h);

    let mut vscale = None;
    surface.draw(plot, frame.buffer_mut(), |c| {
        vscale = paint::candles(c, window);
    });
    if let Some(vs) = vscale {
        render_price_axis(frame, axis, vs);
    }
    if time_h > 0 {
        render_time_axis(
            frame,
            Rect::new(inner.x, inner.y + body_h, plot.width, 1),
            window,
            v.timeframe,
            v.symbol.market.timezone(),
        );
    }

    let ind_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", v.indicator.label()));
    let ind_inner = ind_block.inner(ind_area);
    frame.render_widget(ind_block, ind_area);
    // 指标面板与 K 线共用同一条右侧留白，横坐标才对得齐
    let ind_plot = Rect::new(
        ind_inner.x,
        ind_inner.y,
        ind_inner.width.saturating_sub(axis_w),
        ind_inner.height,
    );
    // 指标必须在**全量**数据上算完再按视口切 —— 只拿窗口内的数据算，
    // 左边缘会因为缺少预热而失真（MACD 要 26+9 根、EMA576 要上千根）。
    match v.indicator {
        IndicatorKind::Macd => {
            let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
            let full = macd(&closes, 12, 26, 9);
            let m = Macd {
                dif: full.dif[lo..hi].to_vec(),
                dea: full.dea[lo..hi].to_vec(),
                hist: full.hist[lo..hi].to_vec(),
            };
            surface.draw(ind_plot, frame.buffer_mut(), |c| paint::macd(c, &m));
        }
        IndicatorKind::Kdj => {
            let full = kdj(bars, 9, 3.0, 3.0);
            let k = Kdj {
                k: full.k[lo..hi].to_vec(),
                d: full.d[lo..hi].to_vec(),
                j: full.j[lo..hi].to_vec(),
            };
            surface.draw(ind_plot, frame.buffer_mut(), |c| paint::kdj(c, &k));
        }
    }
}

/// 横轴时间刻度。标签左边缘对齐所标的那根 K 线，最后一个右对齐
/// 免得被面板边框截断。
fn render_time_axis(
    frame: &mut Frame,
    area: Rect,
    bars: &[Bar],
    tf: Timeframe,
    tz: chrono_tz::Tz,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::default().fg(Color::Gray);
    let ticks = timeaxis::ticks(bars, tf, tz, area.width);
    let last = ticks.len().saturating_sub(1);
    for (i, t) in ticks.iter().enumerate() {
        let label_w = t.label.chars().count() as u16;
        let col = timeaxis::column_of(t.index, bars.len(), area.width);
        // 最后一个刻度右对齐，否则会被边框切掉半个日期
        let x = if i == last {
            area.width.saturating_sub(label_w)
        } else {
            col.min(area.width.saturating_sub(label_w))
        };
        frame.render_widget(
            Paragraph::new(Span::styled(t.label.clone(), style)),
            Rect::new(area.x + x, area.y, label_w.min(area.width - x), 1),
        );
    }
}

/// 价格刻度。画在专属栏位里，不与蜡烛争地盘。
fn render_price_axis(frame: &mut Frame, area: Rect, scale: paint::VScale) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // 行数够就画 5 档，不够就退到首尾两档
    // 之前用 DarkGray，在深色背景上几乎看不见 —— 刻度是要读数的，不是装饰
    let ticks = if area.height >= 9 { 5 } else { 2 };
    let style = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::BOLD);
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

    fn draw_vp(w: u16, h: u16, n: usize, vp: Viewport) -> ratatui::buffer::Buffer {
        draw_full(w, h, n, crate::ui::surface::Backend::Braille, vp)
    }

    fn draw_with(
        w: u16,
        h: u16,
        n: usize,
        backend: crate::ui::surface::Backend,
    ) -> ratatui::buffer::Buffer {
        draw_full(w, h, n, backend, Viewport::default())
    }

    fn draw_full(
        w: u16,
        h: u16,
        n: usize,
        backend: crate::ui::surface::Backend,
        vp: Viewport,
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
                    viewport: vp,
                    surface_label: backend.label(),
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
    fn 标题写明可视根数与总根数() {
        let t = text(&draw(120, 40, 1500), 120, 40);
        assert!(t.contains("共 1500"), "应说明总共加载了多少");
        assert!(t.contains("250 根"), "默认视口是 250 根");
    }

    #[test]
    fn 数据少于视口时全部显示() {
        let t = text(&draw(200, 40, 50), 200, 40);
        assert!(t.contains("50 根 / 共 50"), "画得下就该显示全部");
    }

    #[test]
    fn 贴着最新时标注最新() {
        let t = text(&draw(120, 40, 1500), 120, 40);
        assert!(t.contains("最新"), "默认应贴在最新一根");
    }

    #[test]
    fn 滚动后标注离最新多远() {
        let mut vp = Viewport::default();
        vp.pan_left(1500);
        let t = text(&draw_vp(120, 40, 1500, vp), 120, 40);
        assert!(!t.contains("· 最新 ·"), "滚走之后不该还说「最新」");
        assert!(t.contains("根前"), "应标注离最新多远：{}", &t[..160.min(t.len())]);
    }

    #[test]
    fn 缩放改变可视根数() {
        let mut vp = Viewport::default();
        vp.zoom_in(1500);
        let (lo, hi) = vp.range(1500);
        let t = text(&draw_vp(120, 40, 1500, vp), 120, 40);
        assert!(t.contains(&format!("{} 根", hi - lo)), "标题应反映缩放后的根数");
    }

    #[test]
    fn 横轴显示日期() {
        let t = text(&draw(160, 40, 300), 160, 40);
        // fixture 的 K 线从 2026-01-01 起按天递增
        assert!(t.contains("2026-"), "横轴没有日期刻度：{}", &t[t.len().saturating_sub(400)..]);
    }

    #[test]
    fn 横轴刻度落在图表下沿不占用蜡烛区() {
        let (w, h) = (160u16, 40u16);
        let buf = draw(w, h, 300);
        let lines: Vec<String> = (0..h).map(|y| {
            let mut out = String::new();
            let mut x = 0u16;
            while x < w {
                let sym = buf[(x, y)].symbol();
                out.push_str(sym);
                x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            }
            out
        }).collect();
        let dated: Vec<usize> = lines.iter().enumerate()
            .filter(|(_, l)| l.contains("2026-"))
            .map(|(i, _)| i).collect();
        assert!(!dated.is_empty(), "找不到日期行");
        // K 线区占 Fill(3)，指标区 Fill(1)。日期应贴在 K 线区下沿。
        let chart_bottom = (3 + (h - 3) * 3 / 4) as usize;
        assert!(
            dated.iter().all(|y| *y < chart_bottom),
            "日期行 {dated:?} 应在 K 线区内（< {chart_bottom}），不该跑到指标区"
        );
    }

    #[test]
    fn 面板太矮时不挤时间轴() {
        // 高度不够就把整行让给蜡烛，别为了刻度把图压没
        let _ = draw(160, 10, 100);
        let _ = draw(160, 24, 100);
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
