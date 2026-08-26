mod core;
mod source;
mod store;
mod ui;

use std::time::Duration;

use crossterm::event::{self, Event};
use ratatui::layout::Constraint;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::core::quote::Quote;
use crate::core::symbol::Symbol;
use crate::source::QuoteSource;
use crate::source::tencent::TencentSource;
use crate::store::Store;
use crate::ui::App;
use crate::ui::layout::{Breakpoint, MIN_HEIGHT, MIN_WIDTH, split};
use crate::ui::watchlist::{ColumnKey, columns_for, freshness_label, truncate_display};

/// 盘中刷新间隔。节流器保证不会打得更快。
const REFRESH: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let store = Store::open(&Store::default_path()?)?;
    seed_if_empty(&store)?;

    let source = TencentSource::new()?;
    let (tx, mut rx) = tokio::sync::watch::channel(Vec::<Quote>::new());

    // 后台刷新：与渲染解耦，网络慢不会卡住界面
    let watch: Vec<Symbol> = store.watchlist()?.into_iter().map(|w| w.symbol).collect();
    tokio::spawn(async move {
        loop {
            match source.quotes(&watch).await {
                Ok(qs) => {
                    let _ = tx.send(qs);
                }
                // 界面已经在跑，这里不能 panic —— 打日志后按节奏重试
                Err(e) => eprintln!("[souba] 刷新失败：{e}"),
            }
            tokio::time::sleep(REFRESH).await;
        }
    });

    let mut terminal = ratatui::init();
    let mut app = App::new();
    let result = run(&mut terminal, &mut app, &mut rx).await;
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &mut tokio::sync::watch::Receiver<Vec<Quote>>,
) -> anyhow::Result<()> {
    while !app.should_quit {
        let quotes = rx.borrow().clone();
        terminal.draw(|f| draw(f, app, &quotes))?;
        // 有按键就处理，没有就 100ms 后重绘 —— 让新鲜度显示随时间走
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(k) = event::read()?
        {
            app.on_key(k, quotes.len());
        }
    }
    Ok(())
}

fn draw(frame: &mut ratatui::Frame, app: &App, quotes: &[Quote]) {
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
    let now = chrono::Utc::now();

    frame.render_widget(
        Paragraph::new(Span::styled(
            " souba  相場",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        panes.header,
    );

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
                    // 日股整数），源自己的标度就是它的最小报价单位，抹掉尾随零
                    // 会让同一列出现 446.4 和 1303.38 这种参差。
                    ColumnKey::Last => q.last.to_string(),
                    ColumnKey::Change => q.change.to_string(),
                    // 涨跌幅例外：日股会给到 8 位小数（-0.03244646），必须收敛
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
    frame.render_stateful_widget(table, panes.body, &mut state);

    frame.render_widget(
        Paragraph::new(" ↑↓/jk 移动   q 退出").style(Style::default().fg(Color::DarkGray)),
        panes.footer,
    );
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
mod render_tests {
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

    fn sample() -> Vec<Quote> {
        vec![
            quote("CN:600519", "贵州茅台", "1303.38"),
            quote("HK:00700", "腾讯控股", "446.4"),
            quote("JP:7203", "Toyota Motor Corp.", "3081"),
        ]
    }

    fn render_at(w: u16, h: u16) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let app = App::new();
        term.draw(|f| draw(f, &app, &sample())).unwrap();
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
