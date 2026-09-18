//! 个股详情：K 线 + 指标面板。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::core::bar::{Bar, Timeframe};
use crate::core::indicator::{Kdj, Macd, kdj, macd};
use crate::core::quote::Quote;
use crate::core::strategy::vegas::{Vegas, VegasParams};
use crate::core::strategy::{FacetState, MarketData, Signal, Stance, Strategy};
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

/// AI 解读文本区的状态。和 K 线一样，「加载中」「失败」必须分得开。
#[derive(Debug, Clone)]
pub enum AiState {
    Loading,
    Ready(String),
    Failed(String),
}

/// 详情屏上的 AI 解读文本区。`Some` 即表示它开着。
#[derive(Debug)]
pub struct AiPane {
    pub state: AiState,
    pub scroll: usize,
    /// 渲染时回填的滚动上限。折行要到渲染时才知道面板有多宽，
    /// 而按键处理拿不到宽度 —— 用一个 Cell 把上限带回去，比给 on_key 加参数省事。
    pub max_scroll: std::cell::Cell<usize>,
}

impl AiPane {
    pub fn loading() -> Self {
        Self {
            state: AiState::Loading,
            scroll: 0,
            max_scroll: std::cell::Cell::new(0),
        }
    }
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
    /// 鼠标格子坐标，用来画十字光标
    pub mouse: Option<(u16, u16)>,
    /// 信号行用的 Vegas 参数（来自 settings 表）
    pub vegas: VegasParams,
    /// AI 解读文本区。`Some` 时它盖住 K 线与指标面板。
    pub ai: Option<&'a AiPane>,
}

/// 鼠标停在哪个面板上
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoverPane {
    Chart,
    Indicator,
}

/// 鼠标落点换算出的读数
struct Hover {
    pane: HoverPane,
    bar: Bar,
    /// 在可视窗口里的下标
    index: usize,
    /// 鼠标那一行对应的数值（主图是价格，指标面板是指标值）
    value: f64,
    /// 鼠标所在的终端行，横线画这里
    row: u16,
    /// 鼠标所在的终端列，竖线画这里
    col: u16,
}

/// 鼠标是否落在某个矩形里
fn inside(mouse: (u16, u16), r: Rect) -> bool {
    let (x, y) = mouse;
    r.width > 0 && r.height > 0 && x >= r.x && y >= r.y && x < r.x + r.width && y < r.y + r.height
}

/// 把鼠标的格子坐标换算成「第几根 K 线 / 什么数值」。
///
/// 两个面板横向布局相同，所以下标算法共用；纵向的映射各用各的刻度。
/// 落在两个面板之外返回 None —— 不能让十字光标停在上一次的位置上骗人。
fn hover_at(
    mouse: Option<(u16, u16)>,
    chart: Rect,
    indicator: Rect,
    window: &[Bar],
    chart_vs: paint::VScale,
    ind_vs: Option<paint::VScale>,
) -> Option<Hover> {
    let m = mouse?;
    if window.is_empty() {
        return None;
    }
    let (pane, area, vs) = if inside(m, chart) {
        (HoverPane::Chart, chart, Some(chart_vs))
    } else if inside(m, indicator) {
        (HoverPane::Indicator, indicator, ind_vs)
    } else {
        return None;
    };
    let vs = vs?;
    let fx = (m.0 - area.x) as f64 / (area.width.max(2) - 1) as f64;
    let index = ((fx * (window.len() - 1) as f64).round() as usize).min(window.len() - 1);
    let fy = (m.1 - area.y) as f64 / (area.height.max(2) - 1) as f64;
    Some(Hover {
        pane,
        bar: window[index],
        index,
        value: vs.max - (vs.max - vs.min) * fy,
        row: m.1,
        col: m.0,
    })
}

pub fn render(frame: &mut Frame, area: Rect, v: &DetailView, surface: &mut Surface) {
    let [head, chart_area, ind_area, sig_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(3),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    render_head(frame, head, v);
    // 信号行先画 —— 没有 K 线时下面会提前返回，而「数据不足」恰恰是那时最该说的话
    render_signal(frame, sig_area, v);

    // 文本区开着就盖住图区。图和解读并排会把两边都挤成没法看。
    if let Some(pane) = v.ai {
        render_ai(frame, chart_area.union(ind_area), pane);
        return;
    }

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

    // 指标必须在**全量**数据上算完再按视口切 —— 只拿窗口内的数据算，
    // 左边缘会因为缺少预热而失真（MACD 要 26+9 根、EMA576 要上千根）。
    // 指标必须在**全量**数据上算完再按视口切 —— 只拿窗口内的数据算，
    // 左边缘会因为缺少预热而失真（MACD 要 26+9 根、EMA576 要上千根）。
    let macd_win = (v.indicator == IndicatorKind::Macd).then(|| {
        let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
        let f = macd(&closes, 12, 26, 9);
        Macd {
            dif: f.dif[lo..hi].to_vec(),
            dea: f.dea[lo..hi].to_vec(),
            hist: f.hist[lo..hi].to_vec(),
        }
    });
    let kdj_win = (v.indicator == IndicatorKind::Kdj).then(|| {
        let f = kdj(bars, 9, 3.0, 3.0);
        Kdj {
            k: f.k[lo..hi].to_vec(),
            d: f.d[lo..hi].to_vec(),
            j: f.j[lo..hi].to_vec(),
        }
    });
    let (macd_win, kdj_win) = (macd_win.as_ref(), kdj_win.as_ref());

    let ind_inner_probe = Block::default().borders(Borders::ALL).inner(ind_area);
    let ind_plot = Rect::new(
        ind_inner_probe.x,
        ind_inner_probe.y,
        ind_inner_probe.width.saturating_sub(axis_w),
        ind_inner_probe.height,
    );

    // 两个面板各自的纵向刻度，用于命中测试。与绘制共用同一套映射函数。
    let canvas_h = |r: Rect| surface.backend.canvas_size(r).1;
    let chart_vs = paint::vscale_of(window, canvas_h(plot));
    let ind_vs = match (macd_win, kdj_win) {
        (Some(m), _) => paint::macd_scale(m, canvas_h(ind_plot)),
        (_, Some(_)) => Some(paint::kdj_scale(canvas_h(ind_plot))),
        _ => None,
    };
    let hover = chart_vs.and_then(|cvs| hover_at(v.mouse, plot, ind_plot, window, cvs, ind_vs));

    // 竖线贴在时间刻度上、横线用价格刻度的档位 —— 网格与坐标轴数字对齐，
    // 才谈得上「参考」，不然只是花纹。
    let tz = v.symbol.market.timezone();
    let tick_idx: Vec<usize> = timeaxis::ticks(window, v.timeframe, tz, plot.width)
        .iter()
        .map(|t| t.index)
        .collect();
    let price_levels = if plot.height >= 9 { 5 } else { 2 };
    let g = paint::Grid {
        v_at: &tick_idx,
        h_lines: price_levels,
    };

    let mut vscale = None;
    surface.draw(plot, frame.buffer_mut(), |c| {
        vscale = paint::candles(c, window, Some(&g));
    });
    let vscale = vscale.or(chart_vs);

    if let Some(vs) = vscale {
        render_price_axis(frame, axis, vs);
    }
    if time_h > 0 {
        render_time_axis(
            frame,
            Rect::new(inner.x, inner.y + body_h, plot.width, 1),
            window,
            v.timeframe,
            tz,
        );
    }

    // 指标面板标题带上悬停那根的数值
    let ind_title = match (&hover, macd_win, kdj_win) {
        (Some(h), Some(m), _) if h.index < m.dif.len() => format!(
            " {} · DIF {:.2}  DEA {:.2}  MACD {:.2} ",
            v.indicator.label(),
            m.dif[h.index],
            m.dea[h.index],
            m.hist[h.index]
        ),
        (Some(h), _, Some(k)) if h.index < k.k.len() => format!(
            " {} · K {:.1}  D {:.1}  J {:.1} ",
            v.indicator.label(),
            k.k[h.index],
            k.d[h.index],
            k.j[h.index]
        ),
        _ => format!(" {} ", v.indicator.label()),
    };
    let ind_block = Block::default().borders(Borders::ALL).title(ind_title);
    frame.render_widget(ind_block, ind_area);
    match (macd_win, kdj_win) {
        (Some(m), _) => {
            surface.draw(ind_plot, frame.buffer_mut(), |c| paint::macd(c, m, &tick_idx))
        }
        (_, Some(k)) => surface.draw(ind_plot, frame.buffer_mut(), |c| paint::kdj(c, k, &tick_idx)),
        _ => {}
    }

    // 十字光标：竖线贯穿两个面板（才能把指标的拐点对到 K 线上），
    // 横线只画在鼠标所在的那个面板。
    if let Some(h) = &hover {
        render_crosshair(frame, plot, ind_plot, h);
        let (label_area, txt) = match h.pane {
            HoverPane::Chart => (axis, format!("{:.2}", h.value)),
            HoverPane::Indicator => (
                Rect::new(ind_plot.x + ind_plot.width, ind_plot.y, axis_w, ind_plot.height),
                format!("{:.2}", h.value),
            ),
        };
        if label_area.width > 1 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    txt.clone(),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                )),
                Rect::new(
                    label_area.x + 1,
                    h.row.clamp(label_area.y, label_area.y + label_area.height - 1),
                    (txt.chars().count() as u16).min(label_area.width - 1),
                    1,
                ),
            );
        }
        // 悬停那根的 OHLC 显示在图表顶部 —— 光有十字线不知道读的是什么
        let when = h.bar.ts.with_timezone(&tz);
        let fmt = if matches!(v.timeframe, Timeframe::Day | Timeframe::Week | Timeframe::Month) {
            when.format("%Y-%m-%d").to_string()
        } else {
            when.format("%m-%d %H:%M").to_string()
        };
        let up = h.bar.close >= h.bar.open;
        let color = if up { Color::Red } else { Color::Green };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {fmt} "), Style::default().fg(Color::Gray)),
                Span::styled(
                    format!(
                        "开 {:.2}  高 {:.2}  低 {:.2}  收 {:.2}",
                        h.bar.open, h.bar.high, h.bar.low, h.bar.close
                    ),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ])),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }
}

/// 十字光标。用终端字符而不是画进位图 —— 位图每帧几十 MB，
/// 鼠标一动就重发会直接卡死；字符层重画几十个格子是零成本。
///
/// 竖线**贯穿主图与指标面板**：把 MACD 的拐点对到某一天的 K 线上，
/// 是看指标最常做的事，两条断开的竖线就对不上了。
/// 横线只画在鼠标所在的面板 —— 它读的是那个面板的纵轴。
fn render_crosshair(frame: &mut Frame, chart: Rect, indicator: Rect, h: &Hover) {
    let style = Style::default().fg(Color::Gray);
    let buf = frame.buffer_mut();

    for pane in [chart, indicator] {
        if pane.width == 0 || pane.height == 0 || h.col < pane.x || h.col >= pane.x + pane.width {
            continue;
        }
        for y in pane.y..(pane.y + pane.height) {
            if y == h.row {
                continue;
            }
            buf[(h.col, y)].set_char('│').set_style(style);
        }
    }

    let own = match h.pane {
        HoverPane::Chart => chart,
        HoverPane::Indicator => indicator,
    };
    for x in own.x..(own.x + own.width) {
        if x == h.col {
            continue;
        }
        buf[(x, h.row)].set_char('─').set_style(style);
    }
    buf[(h.col, h.row)].set_char('┼').set_style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
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
    debug_assert!(ticks >= 2, "刻度档数要与网格横线档数保持一致");
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

/// 按**显示格**折行。CJK 一个字占两格，按字符数折会把右边框撑破。
/// 换行符照原样断行，模型分段的地方就该分段。
///
/// 唯一的例外：面板只剩 1 格宽时全角字放不下，仍然单独成行（由渲染层裁掉），
/// 宁可少显示半个字也不静默吞掉内容。
fn wrap(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut w = 0usize;
        for ch in para.trim_end_matches('\r').chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if w + cw > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                w = 0;
            }
            line.push(ch);
            w += cw;
        }
        out.push(line);
    }
    out
}

/// AI 解读文本区。**失败一律显示「AI 不可用：原因」** —— 跟行情源挂了一个待遇，
/// 不静默、不装作没问过。
fn render_ai(frame: &mut Frame, area: Rect, pane: &AiPane) {
    if area.width == 0 || area.height == 0 {
        pane.max_scroll.set(0);
        return;
    }
    let (text, style) = match &pane.state {
        AiState::Loading => (
            "正在解读…".to_string(),
            Style::default().fg(Color::DarkGray),
        ),
        AiState::Ready(t) => (t.clone(), Style::default()),
        AiState::Failed(why) => (
            format!("AI 不可用：{why}"),
            Style::default().fg(Color::Red),
        ),
    };

    // 先探出内框宽度再折行 —— 标题要写「第几行/共几行」，得先知道折了几行
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let lines = wrap(&text, inner.width as usize);
    let page = inner.height as usize;
    let max = lines.len().saturating_sub(page);
    pane.max_scroll.set(max);
    let from = pane.scroll.min(max);

    let title = if max > 0 {
        format!(" AI 解读 · {}-{} / 共 {} 行 ", from + 1, (from + page).min(lines.len()), lines.len())
    } else {
        " AI 解读 ".to_string()
    };
    frame.render_widget(Block::default().borders(Borders::ALL).title(title), area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let shown: Vec<Line> = lines[from..]
        .iter()
        .take(page)
        .map(|l| Line::from(Span::styled(l.clone(), style)))
        .collect();
    frame.render_widget(Paragraph::new(shown), inner);
}

/// 在日线上跑一次 Vegas。信号行与 AI 解读读的是同一份结果 ——
/// 让 AI 解释一份跟屏幕上不一样的信号是最糟的 bug。
///
/// 周期不是日线、或者日线还没到，返回 `None`：不能拿日线的结论顶到周线那一栏上去，
/// 那和把延迟报价显示成实时是同一类谎。
pub fn day_signal(
    symbol: &Symbol,
    quote: Option<&Quote>,
    tf: Timeframe,
    bars: &BarState,
    params: VegasParams,
) -> Option<Signal> {
    let day_bars = match (tf, bars) {
        (Timeframe::Day, BarState::Ready(b)) if !b.is_empty() => b,
        _ => return None,
    };
    let mut loaded = std::collections::HashMap::new();
    loaded.insert(Timeframe::Day, day_bars.clone());
    Some(Vegas { params }.evaluate(&MarketData::new(symbol, quote, &loaded)))
}

/// 策略信号行：主判定 + 五个维度的状态缩写。
///
/// 现在详情屏的日线来自腾讯，最多 640 根，`EMA576` 的种子残留还有 5% 以上 ——
/// 所以真实数据一律会落在「数据不足」上。这正是要显示的东西：**不能拿一条不可信的
/// 慢隧道画个像模像样的结论出来**。
fn render_signal(frame: &mut Frame, area: Rect, v: &DetailView) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let gray = Style::default().fg(Color::DarkGray);
    let strategy = Vegas { params: v.vegas };

    let Some(sig) = day_signal(v.symbol, v.quote, v.timeframe, v.bars, v.vegas) else {
        let msg = if v.timeframe == Timeframe::Day {
            format!(" {} · 等待日线数据 ", strategy.name())
        } else {
            format!(" {} · 仅在日线上求值 ", strategy.name())
        };
        frame.render_widget(Paragraph::new(Span::styled(msg, gray)), area);
        return;
    };

    let stance_color = match sig.stance {
        Stance::Long => Color::Red,
        Stance::Exit => Color::Green,
        Stance::Watch => Color::Gray,
        Stance::Insufficient => Color::Yellow,
    };
    let mut spans = vec![
        Span::styled(format!(" {} ", strategy.name()), gray),
        Span::styled(
            format!("{}  ", sig.stance.label()),
            Style::default()
                .fg(stance_color)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    for f in &sig.facets {
        let color = match f.state {
            FacetState::Bullish => Color::Red,
            FacetState::Neutral => Color::DarkGray,
            FacetState::Bearish => Color::Green,
        };
        spans.push(Span::styled(
            format!("{}{}  ", f.label, f.state.glyph()),
            Style::default().fg(color),
        ));
    }
    if sig.adequacy.ok() {
        if let Some(n) = sig.fresh_bars {
            spans.push(Span::styled(format!("· {n} 根前确立"), gray));
        }
    } else {
        spans.push(Span::styled(
            format!(
                "· 慢隧道不可信（{}/{} 根）",
                sig.adequacy.have, sig.adequacy.need
            ),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
mod wrap_tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn 每行都不超过给定的显示格数() {
        let text = "机器生成的策略分析，不构成投资建议。慢隧道向上倾斜，EMA12 已经站上快隧道上沿。";
        for w in [1usize, 2, 3, 7, 20, 41, 200] {
            for line in wrap(text, w) {
                // 1 格宽时一个全角字都放不下，只能让它单独成行交给渲染层裁
                assert!(
                    line.width() <= w.max(2),
                    "宽度 {w} 下折出了 {} 格的行：{line:?}",
                    line.width()
                );
            }
        }
    }

    #[test]
    fn cjk按两格算而不是一个字符() {
        // 按字符数折的话「中文测试」四个字会挤进 4 格，实际要 8 格
        assert_eq!(wrap("中文测试", 4), vec!["中文", "测试"]);
        assert_eq!(wrap("中文测试", 8), vec!["中文测试"]);
    }

    #[test]
    fn 宽字符不会被劈成半格() {
        // 奇数宽度下最后一格放不下一个全角字，只能留空
        let lines = wrap("中文测试", 5);
        for l in &lines {
            assert!(l.width() <= 5, "{l:?}");
        }
        assert_eq!(lines.concat(), "中文测试", "折行不能吞字也不能加字");
    }

    #[test]
    fn 换行符照原样断行() {
        assert_eq!(wrap("甲\n乙", 10), vec!["甲", "乙"]);
        // 空段落保留成空行 —— 模型用空行分段，吞掉就糊成一坨
        assert_eq!(wrap("甲\n\n乙", 10), vec!["甲", "", "乙"]);
    }

    #[test]
    fn 宽度为零时不死循环也不panic() {
        assert!(wrap("中文", 0).is_empty());
    }

    #[test]
    fn 折行不丢字() {
        let text = "机器生成的策略分析，不构成投资建议";
        assert_eq!(wrap(text, 6).concat(), text);
    }
}

#[cfg(test)]
mod ai_pane_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use unicode_width::UnicodeWidthStr;

    fn draw(w: u16, h: u16, pane: &AiPane) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_ai(f, Rect::new(0, 0, w, h), pane)).unwrap();
        term.backend().buffer().clone()
    }

    fn text(buf: &ratatui::buffer::Buffer, w: u16, h: u16) -> String {
        (0..h)
            .map(|y| {
                let mut out = String::new();
                let mut x = 0u16;
                while x < w {
                    let s = buf[(x, y)].symbol();
                    out.push_str(s);
                    x += UnicodeWidthStr::width(s).max(1) as u16;
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn pane(state: AiState) -> AiPane {
        AiPane { state, scroll: 0, max_scroll: std::cell::Cell::new(0) }
    }

    #[test]
    fn 加载中与失败与回答三种文案互不相同() {
        let l = text(&draw(60, 10, &pane(AiState::Loading)), 60, 10);
        let f = text(&draw(60, 10, &pane(AiState::Failed("未配置 LLM_API_KEY".into()))), 60, 10);
        let r = text(&draw(60, 10, &pane(AiState::Ready("慢隧道向上。".into()))), 60, 10);
        assert!(l.contains("正在解读"), "{l}");
        assert!(f.contains("AI 不可用：未配置 LLM_API_KEY"), "失败要说清原因：{f}");
        assert!(r.contains("慢隧道向上。"), "{r}");
        assert_ne!(l, f);
        assert_ne!(f, r);
    }

    #[test]
    fn 各尺寸都不panic且每行宽度等于面板宽() {
        let long = "机器生成的策略分析，不构成投资建议\n\n".to_string()
            + &"慢隧道向上倾斜，EMA12 站上快隧道上沿，位置维度近 5 根有 4 根在隧道上方。".repeat(6);
        for (w, h) in [(1u16, 1u16), (2, 2), (3, 3), (20, 5), (60, 12), (140, 40), (240, 70)] {
            let p = pane(AiState::Ready(long.clone()));
            let buf = draw(w, h, &p);
            for y in 0..h {
                let mut width = 0usize;
                let mut x = 0u16;
                while x < w {
                    let sw = UnicodeWidthStr::width(buf[(x, y)].symbol()).max(1);
                    width += sw;
                    x += sw as u16;
                }
                assert_eq!(width, w as usize, "{w}x{h} 第 {y} 行宽度 {width} != {w}");
            }
        }
    }

    #[test]
    fn 内容超出一屏时回填滚动上限否则为零() {
        let 短 = pane(AiState::Ready("一行".into()));
        draw(60, 12, &短);
        assert_eq!(短.max_scroll.get(), 0, "装得下就不该允许滚动");

        let 长 = pane(AiState::Ready((0..50).map(|i| format!("第 {i} 行")).collect::<Vec<_>>().join("\n")));
        draw(60, 12, &长);
        assert!(长.max_scroll.get() > 0, "装不下要报出滚动上限");
    }

    #[test]
    fn 滚动之后显示的是后面的行() {
        let body = (0..50).map(|i| format!("第{i}行")).collect::<Vec<_>>().join("\n");
        let mut p = pane(AiState::Ready(body.clone()));
        let 顶部 = text(&draw(60, 12, &p), 60, 12);
        p.scroll = 20;
        let 中段 = text(&draw(60, 12, &p), 60, 12);
        assert!(顶部.contains("第0行") && !顶部.contains("第20行"));
        assert!(中段.contains("第20行") && !中段.contains("第0行"));

        // 滚过头只到底，不会渲染出空白页
        p.scroll = usize::MAX;
        let 底部 = text(&draw(60, 12, &p), 60, 12);
        assert!(底部.contains("第49行"), "{底部}");
    }

    #[test]
    fn 可滚动时标题写明位置() {
        let body = (0..50).map(|i| format!("第{i}行")).collect::<Vec<_>>().join("\n");
        let t = text(&draw(60, 12, &pane(AiState::Ready(body))), 60, 12);
        assert!(t.contains("共 50 行"), "标题要让人知道还有多少没看：{t}");
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

    fn draw_tf(w: u16, h: u16, n: usize, tf: Timeframe) -> ratatui::buffer::Buffer {
        draw_full(w, h, n, crate::ui::surface::Backend::Braille, Viewport::default(), tf)
    }

    fn draw_vp(w: u16, h: u16, n: usize, vp: Viewport) -> ratatui::buffer::Buffer {
        draw_full(w, h, n, crate::ui::surface::Backend::Braille, vp, Timeframe::Day)
    }

    fn draw_with(
        w: u16,
        h: u16,
        n: usize,
        backend: crate::ui::surface::Backend,
    ) -> ratatui::buffer::Buffer {
        draw_full(w, h, n, backend, Viewport::default(), Timeframe::Day)
    }

    fn draw_full(
        w: u16,
        h: u16,
        n: usize,
        backend: crate::ui::surface::Backend,
        vp: Viewport,
        tf: Timeframe,
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
                    timeframe: tf,
                    indicator: IndicatorKind::Macd,
                    bars: &state,
                    viewport: vp,
                    surface_label: backend.label(),
                    mouse: None,
                    vegas: VegasParams::default(),
                    ai: None,
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

    /// 取最后一行的可见文本 —— 信号行就在那里
    fn last_line(buf: &ratatui::buffer::Buffer, w: u16, h: u16) -> String {
        text(buf, w, h).lines().last().unwrap_or_default().to_string()
    }

    #[test]
    fn 信号行显示主判定与五个维度() {
        // 1400 根够 1330，慢隧道可信，才谈得上给判定
        let t = last_line(&draw(160, 50, 1400), 160, 50);
        assert!(t.contains("Vegas"), "信号行不在最后一行：{t:?}");
        for label in ["趋势", "共振", "位置", "确认", "预备"] {
            assert!(t.contains(label), "信号行缺「{label}」维度：{t:?}");
        }
        assert!(!t.contains("慢隧道不可信"), "1400 根不该报数据不足：{t:?}");
    }

    #[test]
    fn 根数不足时信号行说数据不足而不是给结论() {
        // 腾讯只给 640 根，真实数据现在一律走这条路
        let t = last_line(&draw(160, 50, 640), 160, 50);
        assert!(t.contains("数据不足"), "{t:?}");
        assert!(t.contains("慢隧道不可信"), "要说清为什么不足：{t:?}");
        assert!(t.contains("640/1330"), "要把差多少根摆出来：{t:?}");
        assert!(!t.contains("做多"), "数据不足时不能顺手喊多：{t:?}");
    }

    #[test]
    fn 非日线周期不冒充日线结论() {
        let t = last_line(&draw_tf(160, 50, 1400, Timeframe::Week), 160, 50);
        assert!(t.contains("仅在日线上求值"), "{t:?}");
        assert!(!t.contains("趋势"), "周线图上不该摆日线的维度：{t:?}");
    }

    #[test]
    fn 没有k线时信号行仍然画得出来() {
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut surface = Surface::new(crate::ui::surface::Backend::Braille);
        let sym = crate::core::symbol::Symbol::parse("CN:600519").unwrap();
        let state = BarState::Loading;
        term.draw(|f| {
            render(
                f,
                Rect::new(0, 0, 120, 30),
                &DetailView {
                    symbol: &sym,
                    quote: None,
                    timeframe: Timeframe::Day,
                    indicator: IndicatorKind::Macd,
                    bars: &state,
                    viewport: Viewport::default(),
                    surface_label: "盲文",
                    mouse: None,
                    vegas: VegasParams::default(),
                    ai: None,
                },
                &mut surface,
            )
        })
        .unwrap();
        let t = last_line(&term.backend().buffer().clone(), 120, 30);
        assert!(t.contains("等待日线数据"), "加载中也要有信号行占位：{t:?}");
    }

    #[test]
    fn 信号行各尺寸都不panic且行宽等于终端宽() {
        for (w, h) in [(40u16, 6u16), (60, 12), (80, 24), (160, 50)] {
            let buf = draw(w, h, 1400);
            for y in 0..h {
                let mut width = 0usize;
                let mut x = 0u16;
                while x < w {
                    let sym = buf[(x, y)].symbol();
                    let sw = unicode_width::UnicodeWidthStr::width(sym).max(1);
                    width += sw;
                    x += sw as u16;
                }
                assert_eq!(width, w as usize, "{w}x{h} 第 {y} 行宽度 {width} != {w}");
            }
        }
    }
}

#[cfg(test)]
mod hover_tests {
    use super::*;
    use crate::core::bar::Bar;
    use chrono::{TimeZone, Utc};

    fn bars(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| Bar {
                ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
                    + chrono::Duration::days(i as i64),
                open: 100.0 + i as f64,
                high: 110.0 + i as f64,
                low: 90.0 + i as f64,
                close: 105.0 + i as f64,
                volume: 1.0,
            })
            .collect()
    }

    const CHART: Rect = Rect { x: 10, y: 5, width: 100, height: 40 };
    const IND: Rect = Rect { x: 10, y: 47, width: 100, height: 10 };

    fn cvs(b: &[Bar]) -> paint::VScale {
        paint::vscale_of(b, 400).unwrap()
    }
    fn ivs() -> Option<paint::VScale> {
        Some(paint::kdj_scale(100))
    }

    fn hit(pos: Option<(u16, u16)>, b: &[Bar]) -> Option<Hover> {
        hover_at(pos, CHART, IND, b, cvs(b), ivs())
    }

    #[test]
    fn 鼠标在两个面板之外返回none() {
        let b = bars(50);
        for pos in [(0u16, 0u16), (9, 20), (200, 20), (50, 4), (50, 46), (50, 100)] {
            assert!(hit(Some(pos), &b).is_none(), "{pos:?} 在面板外，不该产生读数");
        }
    }

    #[test]
    fn 没有鼠标时返回none() {
        assert!(hit(None, &bars(50)).is_none());
    }

    #[test]
    fn 落在主图判定为主图() {
        let h = hit(Some((CHART.x + 20, CHART.y + 10)), &bars(50)).unwrap();
        assert_eq!(h.pane, HoverPane::Chart);
    }

    #[test]
    fn 落在指标面板判定为指标面板() {
        let h = hit(Some((IND.x + 20, IND.y + 3)), &bars(50)).unwrap();
        assert_eq!(h.pane, HoverPane::Indicator);
    }

    #[test]
    fn 两个面板同一列命中同一根k线() {
        // 竖线贯穿两个面板的前提：横向布局一致
        let b = bars(200);
        let x = CHART.x + 37;
        let a = hit(Some((x, CHART.y + 5)), &b).unwrap();
        let c = hit(Some((x, IND.y + 5)), &b).unwrap();
        assert_eq!(a.index, c.index, "同一列在两个面板应命中同一根");
    }

    #[test]
    fn 左边缘命中第一根右边缘命中最后一根() {
        let b = bars(50);
        assert_eq!(hit(Some((CHART.x, CHART.y + 10)), &b).unwrap().index, 0);
        assert_eq!(
            hit(Some((CHART.x + CHART.width - 1, CHART.y + 10)), &b).unwrap().index,
            b.len() - 1
        );
    }

    #[test]
    fn 命中的是鼠标下方那根的真实数据() {
        let b = bars(50);
        let h = hit(Some((CHART.x + CHART.width / 2, CHART.y + 10)), &b).unwrap();
        assert_eq!(h.bar.close, b[h.index].close, "读数必须来自命中的那一根");
    }

    #[test]
    fn 主图顶部读到高价底部读到低价() {
        let b = bars(50);
        let top = hit(Some((CHART.x + 5, CHART.y)), &b).unwrap();
        let bot = hit(Some((CHART.x + 5, CHART.y + CHART.height - 1)), &b).unwrap();
        assert!(top.value > bot.value, "上方价格应更高");
        let sc = cvs(&b);
        assert!((top.value - sc.max).abs() < 1.0, "顶部应贴近区间上沿");
        assert!((bot.value - sc.min).abs() < 1.0, "底部应贴近区间下沿");
    }

    #[test]
    fn 指标面板用自己的纵轴而不是价格轴() {
        // KDJ 固定 0-100，绝不该读出四位数的股价
        let b = bars(50);
        let h = hit(Some((IND.x + 5, IND.y + 5)), &b).unwrap();
        assert!(
            (-10.0..=110.0).contains(&h.value),
            "指标面板读出了 {} —— 用错了纵轴",
            h.value
        );
    }

    #[test]
    fn 指标刻度缺失时不命中指标面板() {
        // 数据不足算不出 MACD 刻度时，宁可不显示也不能读出乱数
        let b = bars(50);
        assert!(
            hover_at(Some((IND.x + 5, IND.y + 5)), CHART, IND, &b, cvs(&b), None).is_none()
        );
    }

    #[test]
    fn 空数据不panic() {
        assert!(
            hover_at(Some((20, 10)), CHART, IND, &[], paint::VScale::new(0.0, 1.0, 10), ivs())
                .is_none()
        );
    }

    #[test]
    fn 极小面板不panic() {
        let b = bars(3);
        for r in [Rect::new(0, 0, 1, 1), Rect::new(0, 0, 2, 1), Rect::new(0, 0, 1, 2)] {
            let _ = hover_at(Some((0, 0)), r, r, &b, cvs(&b), ivs());
        }
    }

    fn render_hover(mouse: Option<(u16, u16)>, ind: IndicatorKind) -> String {
        let b = bars(300);
        let (w, h) = (160u16, 44u16);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let sym = crate::core::symbol::Symbol::parse("CN:600519").unwrap();
        let state = BarState::Ready(b);
        let mut surface = Surface::new(crate::ui::surface::Backend::Braille);
        term.draw(|f| {
            render(
                f,
                Rect::new(0, 0, w, h),
                &DetailView {
                    symbol: &sym,
                    quote: None,
                    timeframe: Timeframe::Day,
                    indicator: ind,
                    bars: &state,
                    viewport: Viewport::default(),
                    surface_label: "盲文",
                    mouse,
                    vegas: VegasParams::default(),
                    ai: None,
                },
                &mut surface,
            )
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                let mut out = String::new();
                let mut x = 0u16;
                while x < w {
                    let sym = buf[(x, y)].symbol();
                    out.push_str(sym);
                    x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn 悬停主图时显示ohlc() {
        let t = render_hover(Some((60, 12)), IndicatorKind::Macd);
        assert!(t.contains("开 ") && t.contains("收 "), "悬停应显示 OHLC 读数");
    }

    #[test]
    fn 悬停时指标标题显示当根数值() {
        let t = render_hover(Some((60, 12)), IndicatorKind::Macd);
        assert!(t.contains("DIF") && t.contains("DEA"), "MACD 标题应显示 DIF/DEA：{t}");
        let k = render_hover(Some((60, 12)), IndicatorKind::Kdj);
        assert!(k.contains("K ") && k.contains("J "), "KDJ 标题应显示 K/D/J");
    }

    #[test]
    fn 不悬停时指标标题只有名字() {
        let t = render_hover(None, IndicatorKind::Macd);
        assert!(t.contains("MACD(12,26,9)"));
        assert!(!t.contains("DIF"), "没悬停不该显示数值");
    }

    #[test]
    fn 十字光标的竖线贯穿两个面板() {
        let t = render_hover(Some((60, 12)), IndicatorKind::Macd);
        let lines: Vec<&str> = t.lines().collect();
        // 主图区和指标区各自都要有竖线字符
        let chart_rows = 4..30usize;
        let ind_rows = 34..43usize;
        let has = |rows: std::ops::Range<usize>| {
            rows.filter_map(|i| lines.get(i)).any(|l| l.contains('│'))
        };
        assert!(has(chart_rows), "主图区没有竖线");
        assert!(has(ind_rows), "指标区没有竖线 —— 竖线应贯穿两个面板");
    }
}
