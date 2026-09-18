mod ai;
mod core;
mod settings;
mod scan;
mod loader;
mod source;
mod store;
mod ui;

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event};
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::core::bar::Timeframe;
use crate::core::quote::Quote;
use crate::core::symbol::Symbol;
use crate::loader::BarKey;
use crate::source::QuoteSource;
use crate::source::tencent::TencentSource;
use crate::store::Store;
use crate::core::strategy::Signal;
use crate::ui::detail::{AiState, BarState, DetailView};
use crate::ui::surface::{Backend, Surface};
use crate::ui::layout::{Breakpoint, MIN_HEIGHT, MIN_WIDTH, split};
use crate::ui::watchlist::{ColumnKey, columns_for, freshness_label, truncate_display};
use crate::ui::{App, Screen};

/// 盘中刷新间隔。节流器保证不会打得更快。
const REFRESH: Duration = Duration::from_secs(3);
/// 一次拉多少根历史。EMA576 要 1330 根才收敛，日线给足；
/// 分钟线源本身也给不了这么多，多要无害。
const HISTORY_BARS: usize = 1500;

/// 一次 AI 解读请求。喂给模型的是算好的 `Signal`，不是原始 K 线。
struct AiJob {
    symbol: Symbol,
    signal: Signal,
    /// 绕过当天的缓存重问
    force: bool,
}

/// 把命令行给的代码解析成 Symbol。
///
/// 完整形式 `CN:600519` 永远可用；裸 6 位数字按 A股 处理（这是唯一无歧义的简写 ——
/// 4 位既可能是港股也可能是日股，5 位港股与美股代码也会撞，所以其余一律要求写市场前缀）。
fn parse_cli_symbol(raw: &str) -> anyhow::Result<Symbol> {
    let t = raw.trim();
    if t.contains(':') {
        return Symbol::parse(t).map_err(Into::into);
    }
    if t.len() == 6 && t.chars().all(|c| c.is_ascii_digit()) {
        return Symbol::parse(&format!("CN:{t}")).map_err(Into::into);
    }
    anyhow::bail!(
        "无法判断 {t:?} 属于哪个市场，请带上前缀，例如 CN:600519 / HK:00700 / US:AAPL / JP:7203\n\
         （只有 6 位纯数字可以省略前缀，按 A股 处理）"
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let store = Arc::new(Store::open(&Store::default_path()?)?);
    seed_if_empty(&store)?;

    let arg = std::env::args().nth(1);
    if matches!(arg.as_deref(), Some("-h" | "--help")) {
        println!(
            "souba — 终端行情与策略终端\n\n\
             用法：\n  \
             souba              打开自选股列表\n  \
             souba <代码>       直接打开个股详情，例如 souba 600519 或 souba HK:00700\n  \
             souba set <键> <值>  修改策略与扫描参数\n  \
             souba get <键>       查看某个参数的当前值与默认值\n  \
             souba settings       列出全部参数\n  \
             souba scan           拉板块与候选、回补缺的历史并打印热度榜"
        );
        return Ok(());
    }
    if let Some(cmd @ ("set" | "get" | "settings")) = arg.as_deref() {
        let rest: Vec<String> = std::env::args().skip(2).collect();
        return settings::run(cmd, &store, &rest);
    }
    if arg.as_deref() == Some("scan") {
        return scan::run_cli(&store).await;
    }
    let direct = match arg {
        Some(a) => Some(parse_cli_symbol(&a)?),
        None => None,
    };

    let mut watch: Vec<Symbol> = store.watchlist()?.into_iter().map(|w| w.symbol).collect();
    // 命令行指定的标的不在自选股里也要能看 —— 但不写进库，避免顺手查一次就污染自选股
    if let Some(d) = &direct
        && !watch.contains(d)
    {
        watch.insert(0, d.clone());
    }
    let watch = watch;

    // 报价：后台常驻刷新
    let (qtx, mut qrx) = tokio::sync::watch::channel(Vec::<Quote>::new());
    let quote_syms = watch.clone();
    tokio::spawn(async move {
        let source = match TencentSource::new() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[souba] 报价源初始化失败：{e}");
                return;
            }
        };
        loop {
            match source.quotes(&quote_syms).await {
                Ok(qs) => {
                    let _ = qtx.send(qs);
                }
                Err(e) => eprintln!("[souba] 刷新失败：{e}"),
            }
            tokio::time::sleep(REFRESH).await;
        }
    });

    // 历史：按需加载，请求走 channel，结果走 watch
    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<BarKey>(8);
    let (bar_tx, mut bar_rx) =
        tokio::sync::watch::channel::<(Option<BarKey>, BarState)>((None, BarState::Loading));
    loader::spawn(store.clone(), req_rx, bar_tx, HISTORY_BARS);

    let backend = Backend::detect();
    let mut terminal = ratatui::init();
    // 开启鼠标上报才能画十字光标。代价是鼠标框选文字要按住 Shift ——
    // 这是所有全屏 TUI 的通例。具体开哪几个模式、为什么不用 crossterm
    // 的 EnableMouseCapture，见 kitty::MOUSE_ON 的注释。
    let mouse_ok = crate::ui::kitty::emit(crate::ui::kitty::MOUSE_ON).is_ok();
    let mut app = App::new();
    let cfg = settings::Settings::load(&store)?;
    app.vegas = cfg.vegas;

    // AI：按需解读，沿用 K 线那套「请求走 mpsc、结果走 mpsc」
    let (ai_tx, mut ai_rx) = tokio::sync::mpsc::channel::<AiJob>(4);
    let (ai_res_tx, mut ai_res_rx) = tokio::sync::mpsc::channel::<(Symbol, AiState)>(8);
    let ai_store = Arc::clone(&store);
    let ai_model = cfg.ai_model.clone();
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .user_agent("souba/0.1")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[souba] AI 客户端初始化失败：{e}");
                return;
            }
        };
        while let Some(job) = ai_rx.recv().await {
            // 每次都重读端点配置 —— 中途补上 .env 里的 key 不该需要重启
            let ep = ai::Endpoint::from_env(&ai_model).ok();
            let state = match ai::interpret(
                &ai_store,
                &job.symbol,
                &job.signal,
                ep,
                job.force,
                chrono::Utc::now().date_naive(),
                |ep, body| ai::post(&client, ep, body),
            )
            .await
            {
                Ok(text) => AiState::Ready(text),
                Err(e) => AiState::Failed(e.to_string()),
            };
            let _ = ai_res_tx.send((job.symbol, state)).await;
        }
    });
    if let Some(d) = &direct {
        app.selected = watch.iter().position(|s| s == d).unwrap_or(0);
        app.screen = Screen::Detail;
        app.bars_dirty = true;
    }
    let result = run(
        &mut terminal,
        &mut app,
        &watch,
        &mut qrx,
        &req_tx,
        &mut bar_rx,
        &ai_tx,
        &mut ai_res_rx,
        backend,
    )
    .await;
    if mouse_ok {
        let _ = crate::ui::kitty::emit(crate::ui::kitty::MOUSE_OFF);
    }
    // 退出前删掉贴过的位图，否则会残留在滚动缓冲里
    if matches!(backend, Backend::Kitty(_)) {
        let _ = crate::ui::kitty::emit(&crate::ui::kitty::clear());
    }
    ratatui::restore();
    result
}

// 参数就是几组后台通道，捆成一个结构体只是换个地方写同样的字段
#[allow(clippy::too_many_arguments)]
async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    watch: &[Symbol],
    qrx: &mut tokio::sync::watch::Receiver<Vec<Quote>>,
    req_tx: &tokio::sync::mpsc::Sender<BarKey>,
    bar_rx: &mut tokio::sync::watch::Receiver<(Option<BarKey>, BarState)>,
    ai_tx: &tokio::sync::mpsc::Sender<AiJob>,
    ai_res_rx: &mut tokio::sync::mpsc::Receiver<(Symbol, AiState)>,
    backend: Backend,
) -> anyhow::Result<()> {
    // 十字光标随鼠标动，所以鼠标位置也要纳入重绘判定 —— 否则位图不会刷新
    // AI 文本区开着时这一帧不画 K 线，所以它也得进 stamp —— 否则残留的位图
    // 会盖在解读文字上（位图在字符层之上，不清就一直在）
    type Stamp = (Screen, usize, Timeframe, crate::ui::detail::IndicatorKind,
                  crate::ui::viewport::Viewport, (u16, u16), usize, bool);
    let mut last_stamp: Stamp = (Screen::Watchlist, usize::MAX, Timeframe::Day,
                                 crate::ui::detail::IndicatorKind::Macd,
                                 crate::ui::viewport::Viewport { span: 0, offset: usize::MAX },
                                 (0, 0), usize::MAX, false);
    while !app.should_quit {
        if app.bars_dirty {
            app.bars_dirty = false;
            if let Some(sym) = watch.get(app.selected) {
                // 满了就丢弃这次请求 —— 用户还在连按切换，最后一次会补上
                let _ = req_tx.try_send((sym.clone(), app.timeframe));
            }
        }

        let quotes = qrx.borrow().clone();
        let (bar_key, bar_state) = bar_rx.borrow().clone();

        // 结果先收，再派发本轮的请求
        while let Ok((sym, state)) = ai_res_rx.try_recv() {
            // 用户已经翻到别的标的了，就别拿旧标的的解读盖上去
            if watch.get(app.selected) == Some(&sym)
                && let Some(pane) = &mut app.ai
            {
                pane.state = state;
                pane.scroll = 0;
            }
        }
        if let Some(force) = app.ai_request.take()
            && let Some(sym) = watch.get(app.selected)
        {
            // 手上这份数据必须确实是这只标的、这个周期的，否则解读的是别人的信号
            let fresh = bar_key.as_ref() == Some(&(sym.clone(), app.timeframe));
            let quote = quotes.iter().find(|q| &q.symbol == sym);
            let signal = fresh
                .then(|| {
                    ui::detail::day_signal(sym, quote, app.timeframe, &bar_state, app.vegas)
                })
                .flatten();
            match signal {
                Some(signal) => {
                    let job = AiJob { symbol: sym.clone(), signal, force };
                    if ai_tx.try_send(job).is_err()
                        && let Some(pane) = &mut app.ai
                    {
                        pane.state = AiState::Failed("上一次解读还没回来，稍后再按".into());
                    }
                }
                None => {
                    if let Some(pane) = &mut app.ai {
                        pane.state = AiState::Failed(
                            "还没有可解读的信号 —— 策略只在日线上求值，先切回日线等 K 线加载完".into(),
                        );
                    }
                }
            }
        }

        // 先算 stamp 再决定要不要画位图 —— 之前是画完才发现没变然后扔掉，
        // 白白付出一张 400 万像素画布的分配与绘制开销。鼠标移动时这是主要瓶颈。
        let stamp = (
            app.screen,
            app.selected,
            app.timeframe,
            app.indicator,
            app.viewport,
            terminal.size().map(|s| (s.width, s.height)).unwrap_or_default(),
            bar_len(&bar_state),
            app.ai.is_some(),
        );
        let bitmap_unchanged = stamp == last_stamp;
        let mut surface = Surface::with_skip(backend, bitmap_unchanged);
        terminal.draw(|f| draw(f, app, watch, &quotes, &bar_key, &bar_state, &mut surface))?;

        // 位图叠在字符层之上，必须在 ratatui 画完之后才发。
        if let Some(seq) = surface.escape.take() {
            let _ = crate::ui::kitty::emit(&seq);
            last_stamp = stamp;
        } else if !bitmap_unchanged {
            // 这一帧没产生图（切回列表屏、或数据还没到），把残留的图清掉
            if matches!(backend, Backend::Kitty(_)) && last_stamp.0 == Screen::Detail {
                let _ = crate::ui::kitty::emit(&crate::ui::kitty::clear());
            }
            last_stamp = stamp;
        }

        // 任意移动模式的事件量很大（实测晃 15 秒能产生上万个），
        // 一轮循环只处理一个会越积越多、光标跟不上手。
        // 所以先阻塞等第一个，再把已排队的合并掉，然后只画一帧。
        //
        // **必须有上限**：鼠标持续移动时事件是源源不断的，
        // 无上限地「排空到没有为止」会让内层循环永远退不出去，一帧都画不了 ——
        // 表现就是十字光标彻底不出现。踩过这个坑。
        const MAX_DRAIN: usize = 64;
        if event::poll(Duration::from_millis(100))? {
            let bars_now = bar_len(&bar_state);
            let mut drained = 0usize;
            loop {
                match event::read()? {
                    Event::Key(k) => app.on_key_with(k, watch.len(), bars_now),
                    Event::Mouse(m) => {
                        use crossterm::event::MouseEventKind;
                        app.on_mouse(m);
                        // 滚轮缩放是看盘软件的通用手势
                        match m.kind {
                            MouseEventKind::ScrollUp => app.on_scroll(true, bars_now),
                            MouseEventKind::ScrollDown => app.on_scroll(false, bars_now),
                            _ => {}
                        }
                    }
                    _ => {}
                }
                drained += 1;
                // 零超时 poll = 「还有没有已经到了的事件」
                if app.should_quit || drained >= MAX_DRAIN || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn bar_len(s: &BarState) -> usize {
    match s {
        BarState::Ready(b) => b.len(),
        _ => 0,
    }
}

fn draw(
    frame: &mut Frame,
    app: &App,
    watch: &[Symbol],
    quotes: &[Quote],
    bar_key: &Option<BarKey>,
    bar_state: &BarState,
    surface: &mut Surface,
) {
    let ai = app.ai.as_ref();
    let area = frame.area();
    let bp = Breakpoint::of(area);
    if bp == Breakpoint::TooSmall {
        frame.render_widget(
            Paragraph::new(format!("终端太小，至少需要 {MIN_WIDTH}x{MIN_HEIGHT}")),
            area,
        );
        return;
    }

    let panes = split(area);
    frame.render_widget(
        Paragraph::new(Span::styled(
            " souba  相場",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        panes.header,
    );

    match app.screen {
        Screen::Watchlist => {
            draw_watchlist(frame, panes.body, app, quotes, bp);
            frame.render_widget(
                Paragraph::new(" ↑↓/jk 移动   Enter 详情   q 退出")
                    .style(Style::default().fg(Color::DarkGray)),
                panes.footer,
            );
        }
        Screen::Detail => {
            let sym = watch.get(app.selected);
            let quote = sym.and_then(|s| quotes.iter().find(|q| &q.symbol == s));
            // 请求的键与回来的键不一致时说明还在路上，不要拿旧标的的数据冒充新标的
            let want = sym.map(|s| (s.clone(), app.timeframe));
            let state = if bar_key == &want {
                bar_state.clone()
            } else {
                BarState::Loading
            };
            if let Some(sym) = sym {
                ui::detail::render(
                    frame,
                    panes.body,
                    &DetailView {
                        symbol: sym,
                        quote,
                        timeframe: app.timeframe,
                        indicator: app.indicator,
                        bars: &state,
                        viewport: app.viewport,
                        surface_label: surface.backend.label(),
                        mouse: app.mouse,
                        vegas: app.vegas,
                        ai,
                    },
                    surface,
                );
            }
            let hint = if ai.is_some() {
                " ↑↓/jk PgUp/PgDn 滚动   Home/End 首尾   R 重新解读   Esc 关闭解读"
            } else {
                " ←→ 滚动   =- 缩放/滚轮   鼠标悬停读数   Tab 切周期   ↑↓ 切标的   i 切指标   ? AI 解读   Esc 返回"
            };
            frame.render_widget(
                Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
                panes.footer,
            );
        }
    }
}

fn draw_watchlist(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    app: &App,
    quotes: &[Quote],
    bp: Breakpoint,
) {
    let now = chrono::Utc::now();
    let cols = columns_for(bp);
    let header = Row::new(
        cols.iter()
            .map(|c| Cell::from(truncate_display(c.title, c.cells as usize))),
    );

    let rows: Vec<Row> = quotes
        .iter()
        .map(|q| {
            // A股 惯例红涨绿跌
            let color = if q.is_up() { Color::Red } else { Color::Green };
            Row::new(cols.iter().map(|c| {
                let text = match c.key {
                    ColumnKey::Code => q.symbol.to_string(),
                    ColumnKey::Name => q.name.clone(),
                    // 不做 normalize —— 各市场报价精度不同（A股 2 位、港股 3 位、
                    // 日股整数），源自己的标度就是它的最小报价单位。
                    ColumnKey::Last => q.last.to_string(),
                    ColumnKey::Change => q.change.to_string(),
                    // 涨跌幅例外：日股会给到 8 位小数，必须收敛
                    ColumnKey::ChangePct => format!("{:.2}%", q.change_pct),
                    ColumnKey::Freshness => freshness_label(q.freshness(now)),
                };
                let style = match c.key {
                    // 新鲜度不参与涨跌着色 —— 它是元信息不是行情
                    ColumnKey::Freshness => Style::default().fg(Color::DarkGray),
                    _ => Style::default().fg(color),
                };
                Cell::from(truncate_display(&text, c.cells as usize)).style(style)
            }))
        })
        .collect();

    let widths: Vec<Constraint> = cols.iter().map(|c| Constraint::Length(c.cells)).collect();
    let table = Table::new(rows, widths)
        .header(header.style(Style::default().add_modifier(Modifier::REVERSED)))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .block(Block::default().borders(Borders::ALL).title("自选股"));

    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

/// 首次运行时放几只进去，免得开局是空屏。
fn seed_if_empty(store: &Store) -> anyhow::Result<()> {
    if !store.watchlist()?.is_empty() {
        return Ok(());
    }
    for (raw, name) in [
        ("CN:600519", "贵州茅台"),
        ("CN:000001", "平安银行"),
        ("CN:300750", "宁德时代"),
        ("HK:00700", "腾讯控股"),
        ("US:AAPL", "苹果"),
        ("JP:7203", "丰田汽车"),
    ] {
        store.add(&Symbol::parse(raw)?, name)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod render_tests {
    use super::*;
    use crate::core::symbol::Symbol;
    use chrono::{TimeZone, Utc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use unicode_width::UnicodeWidthStr;

    fn quote(sym: &str, name: &str, last: &str) -> Quote {
        Quote {
            symbol: Symbol::parse(sym).unwrap(),
            name: name.to_string(),
            last: Decimal::from_str(last).unwrap(),
            prev_close: Decimal::from_str("100").unwrap(),
            open: Decimal::from_str("100").unwrap(),
            high: Decimal::from_str("110").unwrap(),
            low: Decimal::from_str("90").unwrap(),
            volume: Decimal::from_str("1000").unwrap(),
            change: Decimal::from_str("1.5").unwrap(),
            change_pct: Decimal::from_str("1.5").unwrap(),
            stamped_at: Utc.with_ymd_and_hms(2026, 8, 26, 2, 30, 0).unwrap(),
            source: "tencent",
        }
    }

    pub(crate) fn sample() -> Vec<Quote> {
        vec![
            quote("CN:600519", "贵州茅台", "1303.38"),
            quote("HK:00700", "腾讯控股", "446.4"),
            quote("JP:7203", "Toyota Motor Corp.", "3081"),
        ]
    }

    fn watch() -> Vec<Symbol> {
        sample().iter().map(|q| q.symbol.clone()).collect()
    }

    pub(crate) fn render_at(w: u16, h: u16) -> Buffer {
        render_with(w, h, &App::new(), &BarState::Loading)
    }

    pub(crate) fn render_with(w: u16, h: u16, app: &App, bars: &BarState) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let watch = watch();
        let key = watch.first().map(|s| (s.clone(), app.timeframe));
        let mut surface = crate::ui::surface::Surface::new(crate::ui::surface::Backend::Braille);
        term.draw(|f| draw(f, app, &watch, &sample(), &key, bars, &mut surface))
            .unwrap();
        term.backend().buffer().clone()
    }

    /// 还原一行的可见文本。
    ///
    /// 不能直接把每个格子的 symbol 拼起来 —— ratatui 对宽字符是「首格存字符、
    /// 次格留空」，直拼会得到「终 端 太 小」这种带空隙的结果。要按字符的显示
    /// 宽度跳过填充格。
    fn line_text(buf: &Buffer, y: u16, w: u16) -> String {
        let mut out = String::new();
        let mut x = 0u16;
        while x < w {
            let s = buf[(x, y)].symbol();
            out.push_str(s);
            x += s.width().max(1) as u16;
        }
        out
    }

    fn all_text(buf: &Buffer, w: u16, h: u16) -> String {
        (0..h).map(|y| line_text(buf, y, w)).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn 各种尺寸都能渲染不panic() {
        for (w, h) in [(40u16, 24u16), (80, 24), (120, 40), (200, 60), (30, 10)] {
            let _ = render_at(w, h);
        }
    }

    #[test]
    fn 过小时给出提示而不是画崩() {
        let buf = render_at(30, 10);
        assert!(all_text(&buf, 30, 10).contains("终端太小"));
    }

    #[test]
    fn 高度不足也算过小() {
        // 宽度够但高度不足同样要给提示，不能只看宽度
        let buf = render_at(120, 20);
        assert!(all_text(&buf, 120, 20).contains("终端太小"));
    }

    #[test]
    fn 中文股票名完整出现且不乱码() {
        let buf = render_at(120, 24);
        let text = all_text(&buf, 120, 24);
        assert!(text.contains("贵州茅台"), "中文名没渲染出来：{text}");
        assert!(text.contains("腾讯控股"));
    }

    #[test]
    fn 表格每行显示宽度都等于终端宽度() {
        // CJK 宽度算错的典型表现就是行与行之间列位漂移。
        let (w, h) = (120u16, 30u16);
        let buf = render_at(w, h);
        for y in 0..h {
            let line = line_text(&buf, y, w);
            assert_eq!(
                line.width(),
                w as usize,
                "第 {y} 行显示宽度 {} != 终端宽度 {w}，CJK 宽度算错了：{line:?}",
                line.width()
            );
        }
    }

    #[test]
    fn 窄屏列数减少但仍有代码和现价() {
        let buf = render_at(60, 24);
        let text = all_text(&buf, 60, 24);
        assert!(text.contains("代码"), "{text}");
        assert!(text.contains("现价"));
        // 窄屏砍掉了「行情」列
        assert!(!text.contains("行情"), "窄屏不该显示行情列：{text}");
    }

    #[test]
    fn 宽屏显示新鲜度列() {
        let buf = render_at(140, 24);
        assert!(all_text(&buf, 140, 24).contains("行情"), "宽屏应显示新鲜度列");
    }

    #[test]
    fn 休市时段显示休市而不是延迟() {
        // 样本时间戳是 2026-08-26 02:30 UTC = 北京 10:30，A股 交易中。
        // 但 draw 用的是 Utc::now()，跑测试时多半不在交易时段 —— 所以这里
        // 直接验证 Quote::freshness 而不是渲染结果。
        let q = &sample()[0];
        let 周六 = Utc.with_ymd_and_hms(2026, 8, 29, 2, 30, 0).unwrap();
        assert_eq!(q.freshness(周六), crate::core::quote::Freshness::Halted);
    }
}

#[cfg(test)]
mod detail_render_tests {
    use super::render_tests::*;
    use super::*;
    use crate::core::bar::Bar;
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};

    fn fake_bars(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| {
                let c = (i as f64 * 0.25).sin() * 40.0 + 1300.0;
                Bar {
                    ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
                        + ChronoDuration::days(i as i64),
                    open: c - 3.0,
                    high: c + 8.0,
                    low: c - 8.0,
                    close: c,
                    volume: 1000.0,
                }
            })
            .collect()
    }

    fn detail_app() -> App {
        let mut app = App::new();
        app.screen = Screen::Detail;
        app
    }

    fn text_of(buf: &ratatui::buffer::Buffer, w: u16, h: u16) -> String {
        (0..h)
            .map(|y| {
                let mut out = String::new();
                let mut x = 0u16;
                while x < w {
                    let s = buf[(x, y)].symbol();
                    out.push_str(s);
                    x += unicode_width::UnicodeWidthStr::width(s).max(1) as u16;
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn 详情屏画出蜡烛() {
        // 盲文后端产出的是 U+2800 一族，不是旧的半块字符
        let buf = render_with(140, 40, &detail_app(), &BarState::Ready(fake_bars(200)));
        let painted = buf
            .content()
            .iter()
            .filter(|c| {
                c.symbol()
                    .chars()
                    .next()
                    .is_some_and(|ch| ('\u{2800}'..='\u{28FF}').contains(&ch))
            })
            .count();
        assert!(painted > 100, "蜡烛太少，只画了 {painted} 格");
    }

    #[test]
    fn 详情屏显示周期与根数() {
        let buf = render_with(140, 40, &detail_app(), &BarState::Ready(fake_bars(200)));
        let t = text_of(&buf, 140, 40);
        assert!(t.contains("日线"), "没显示周期：{t}");
        assert!(t.contains("200 根"), "没显示根数");
        assert!(t.contains("MACD"), "没显示指标名");
    }

    #[test]
    fn 无数据源与加载中与失败显示不同文案() {
        let loading = text_of(&render_with(140, 40, &detail_app(), &BarState::Loading), 140, 40);
        let unsup = text_of(
            &render_with(140, 40, &detail_app(), &BarState::Unsupported("日股市场的历史 K 线暂无可用免费数据源".into())),
            140, 40,
        );
        let failed = text_of(&render_with(140, 40, &detail_app(), &BarState::Failed("超时".into())), 140, 40);
        assert!(loading.contains("正在加载"), "加载中文案缺失");
        assert!(unsup.contains("暂无可用免费数据源"), "缺源文案缺失 —— 这条必须让用户看见");
        assert!(failed.contains("加载失败"), "失败文案缺失");
        assert_ne!(loading, unsup);
        assert_ne!(unsup, failed);
    }

    #[test]
    fn 详情屏底栏提示与列表屏不同() {
        let list = text_of(&render_at(140, 40), 140, 40);
        let det = text_of(&render_with(140, 40, &detail_app(), &BarState::Ready(fake_bars(60))), 140, 40);
        assert!(list.contains("Enter 详情"));
        assert!(det.contains("切周期"));
    }

    #[test]
    fn 切到kdj后指标名跟着变() {
        let mut app = detail_app();
        app.indicator = crate::ui::detail::IndicatorKind::Kdj;
        let t = text_of(&render_with(140, 40, &app, &BarState::Ready(fake_bars(120))), 140, 40);
        assert!(t.contains("KDJ"), "指标名没跟着切换");
    }

    #[test]
    fn 解读文本区盖住k线并换掉底栏提示() {
        let mut app = detail_app();
        let 关着 = text_of(&render_with(140, 40, &app, &BarState::Ready(fake_bars(200))), 140, 40);
        assert!(关着.contains("? AI 解读"), "底栏要告诉用户按哪个键：{关着}");

        app.ai = Some(crate::ui::detail::AiPane {
            state: crate::ui::detail::AiState::Ready(
                "机器生成的策略分析，不构成投资建议\n\n慢隧道向上倾斜。".into(),
            ),
            scroll: 0,
            max_scroll: std::cell::Cell::new(0),
        });
        let 开着 = text_of(&render_with(140, 40, &app, &BarState::Ready(fake_bars(200))), 140, 40);
        assert!(开着.contains("AI 解读"), "{开着}");
        assert!(开着.contains("不构成投资建议"), "免责声明必须在屏幕上：{开着}");
        assert!(开着.contains("Esc 关闭解读"), "底栏要换成文本区的键位：{开着}");
        assert!(!开着.contains("MACD"), "文本区开着时不该还画指标面板");
    }

    #[test]
    fn 解读打开时详情屏每行宽度仍等于终端宽度() {
        let mut app = detail_app();
        app.ai = Some(crate::ui::detail::AiPane {
            state: crate::ui::detail::AiState::Ready(
                "机器生成的策略分析，不构成投资建议。慢隧道向上倾斜，EMA12 站上快隧道上沿。".repeat(8),
            ),
            scroll: 0,
            max_scroll: std::cell::Cell::new(0),
        });
        for (w, h) in [(40u16, 24u16), (80, 24), (140, 40), (240, 70)] {
            let buf = render_with(w, h, &app, &BarState::Ready(fake_bars(200)));
            for y in 0..h {
                let mut width = 0usize;
                let mut x = 0u16;
                while x < w {
                    let sw = unicode_width::UnicodeWidthStr::width(buf[(x, y)].symbol()).max(1);
                    width += sw;
                    x += sw as u16;
                }
                assert_eq!(width, w as usize, "{w}x{h} 第 {y} 行宽度 {width} != {w}");
            }
        }
    }

    #[test]
    fn 详情屏各尺寸都不panic() {
        for (w, h) in [(40u16, 24u16), (80, 24), (120, 40), (240, 70)] {
            let _ = render_with(w, h, &detail_app(), &BarState::Ready(fake_bars(300)));
        }
    }

    #[test]
    fn 详情屏每行宽度仍等于终端宽度() {
        let (w, h) = (140u16, 40u16);
        let buf = render_with(w, h, &detail_app(), &BarState::Ready(fake_bars(200)));
        for y in 0..h {
            let mut width = 0usize;
            let mut x = 0u16;
            while x < w {
                let s = buf[(x, y)].symbol();
                let sw = unicode_width::UnicodeWidthStr::width(s).max(1);
                width += sw;
                x += sw as u16;
            }
            assert_eq!(width, w as usize, "详情屏第 {y} 行宽度 {width} != {w}");
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn 完整形式直接可用() {
        assert_eq!(parse_cli_symbol("CN:600519").unwrap().to_string(), "CN:600519");
        assert_eq!(parse_cli_symbol("HK:00700").unwrap().to_string(), "HK:00700");
        assert!(parse_cli_symbol("us:aapl").is_err(), "市场前缀大小写敏感");
        assert_eq!(parse_cli_symbol("US:aapl").unwrap().to_string(), "US:AAPL");
    }

    #[test]
    fn 裸六位数字按a股处理() {
        assert_eq!(parse_cli_symbol("600519").unwrap().to_string(), "CN:600519");
        assert_eq!(parse_cli_symbol("000001").unwrap().to_string(), "CN:000001");
    }

    #[test]
    fn 有歧义的简写被拒绝而不是猜() {
        // 4 位既可能是港股也可能是日股，猜错会静默显示另一只股票
        for ambiguous in ["7203", "0700", "AAPL", "00700"] {
            assert!(
                parse_cli_symbol(ambiguous).is_err(),
                "{ambiguous:?} 有歧义，应要求写明市场而不是替用户猜"
            );
        }
    }

    #[test]
    fn 错误信息说清怎么改() {
        let e = parse_cli_symbol("7203").unwrap_err().to_string();
        assert!(e.contains("CN:600519") && e.contains("JP:7203"), "错误信息要给出正确写法：{e}");
    }
}
