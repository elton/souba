//! 机会面板：读当天落库的扫描结果，按板块热度列出，展开看板块内 top。
//!
//! 这一屏**不求值**。`scan_results` 里存的已经是算完的 `stance` / 新鲜度 / 五维度，
//! 渲染路径只做「读库 + 拼字符串」。要重算就得重跑扫描（`r`），那是后台任务的事。
//!
//! 现价与涨幅不从这里拉 —— 面板里的标的并进主循环那次腾讯批量报价（一次请求
//! 最多 100 只、可混市场），所以这里只接收算好的 `Quote` 切片。

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use serde_json::Value;

use crate::core::quote::Quote;
use crate::core::sector::{Heat, SectorKind, heat};
use crate::core::strategy::Stance;
use crate::core::strategy::vegas::READY_FACET;
use crate::core::symbol::{Market, Symbol};
use crate::scan::Progress;
use crate::scan::evaluate::BACKFILLING;
use crate::settings::ScanParams;
use crate::store::Store;
use crate::ui::layout::Breakpoint;
use crate::ui::watchlist::truncate_display;

/// 面板上的一只标的。`stance` 保留库里的稳定标识，文案在渲染时才生成。
#[derive(Debug, Clone)]
pub struct PanelPick {
    pub symbol: Symbol,
    pub name: String,
    pub stance: String,
    /// 几根前确立，仅 `long` 有值
    pub fresh_bars: Option<i64>,
    /// 五维度缩写，已拼成「预备↑ 位置↑ …」
    pub facets: String,
}

#[derive(Debug, Clone)]
pub struct PanelSector {
    pub name: String,
    pub kind: SectorKind,
    pub heat: Heat,
    pub picks: Vec<PanelPick>,
}

/// 一次扫描的结果快照。空的 `sectors` = 今天还没扫出东西。
#[derive(Debug, Clone, Default)]
pub struct Panel {
    pub date: String,
    pub sectors: Vec<PanelSector>,
}

impl Panel {
    pub fn sector(&self, i: usize) -> Option<&PanelSector> {
        self.sectors.get(i)
    }

    pub fn pick(&self, sector: usize, pick: usize) -> Option<&PanelPick> {
        self.sector(sector)?.picks.get(pick)
    }

    /// 面板里出现的全部标的，去重。主循环把它并进自选股那次批量报价。
    pub fn symbols(&self) -> Vec<Symbol> {
        let mut out: Vec<Symbol> = Vec::new();
        for s in &self.sectors {
            for p in &s.picks {
                if !out.contains(&p.symbol) {
                    out.push(p.symbol.clone());
                }
            }
        }
        out
    }
}

/// 从库里装一次面板数据。板块按热度降序 —— `scan_results` 只记了板块内名次，
/// 板块之间的次序要用当日快照重新算一遍热度（几个板块几行历史，很便宜）。
pub fn load(
    store: &Store,
    market: Market,
    date: &str,
    p: &ScanParams,
) -> anyhow::Result<Panel> {
    let rows = store.scan_results(market, date)?;
    let mut sectors = Vec::new();
    for (code, name, kind, snapshot) in store.scan_sectors(market, date)? {
        let prev = store.sector_history(market, &code, date, p.heat_days)?;
        let names: HashMap<String, String> = store
            .sector_members(market, &code, date)?
            .into_iter()
            .map(|(_, symbol, name)| (symbol, name))
            .collect();
        let picks = rows
            .iter()
            .filter(|r| r.sector_code == code)
            .map(|r| PanelPick {
                name: names
                    .get(&r.symbol.to_string())
                    .cloned()
                    // 候选表里没有这只（库被改过 / 跨日残留）时显示代码，不空着
                    .unwrap_or_else(|| r.symbol.to_string()),
                symbol: r.symbol.clone(),
                stance: r.stance.clone(),
                fresh_bars: r.freshness,
                facets: facets_label(&r.facets_json),
            })
            .collect();
        sectors.push(PanelSector {
            name,
            kind,
            heat: heat(&snapshot, &prev, p.w_turnover, p.w_change),
            picks,
        });
    }
    sectors.sort_by(|a, b| b.heat.heat.total_cmp(&a.heat.heat));
    Ok(Panel {
        date: date.to_string(),
        sectors,
    })
}

/// `facets_json` → 「预备↑ 位置↑ …」。存的 `state` 已经是箭头了（落库时就那么存的）。
/// 解析不了就给空串 —— 一行维度缩写读不出来，不该让整个面板打不开。
fn facets_label(json: &str) -> String {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|v| {
            Some(format!(
                "{}{}",
                v.get("label")?.as_str()?,
                v.get("state")?.as_str()?
            ))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ── 扫描状态

/// 面板顶部那一行的状态。后台任务推 `Progress`，这里归约成显示用的状态。
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ScanStatus {
    /// 本次会话没在扫 —— 库里有没有今天的结果是另一回事
    #[default]
    Idle,
    Sectors,
    Members { done: usize, total: usize },
    Backfill { done: usize, total: usize },
    Evaluating,
    Done,
    Failed(String),
}

impl ScanStatus {
    /// 正在跑就不再起第二个 —— 启动自动扫和手动 `r` 靠这个判定不打架
    pub fn running(&self) -> bool {
        matches!(
            self,
            ScanStatus::Sectors
                | ScanStatus::Members { .. }
                | ScanStatus::Backfill { .. }
                | ScanStatus::Evaluating
        )
    }
}

/// 进度 → 状态。`Log` 是 CLI 的详细文本，面板不看。
pub fn apply(status: &mut ScanStatus, p: &Progress) {
    *status = match p {
        Progress::Log(_) => return,
        Progress::Sectors => ScanStatus::Sectors,
        Progress::Members { done, total } => ScanStatus::Members {
            done: *done,
            total: *total,
        },
        Progress::Backfill { done, total } => ScanStatus::Backfill {
            done: *done,
            total: *total,
        },
        Progress::Evaluating => ScanStatus::Evaluating,
        Progress::Done => ScanStatus::Done,
        Progress::Failed(e) => ScanStatus::Failed(e.clone()),
    };
}

/// 顶部状态行。`has_data` = 库里有今天的结果 —— 没在扫的时候，有没有东西看
/// 是两种完全不同的状态，不能都说「完成」。
pub fn status_line(date: &str, status: &ScanStatus, has_data: bool) -> String {
    let s = match status {
        ScanStatus::Idle if has_data => "今日结果".to_string(),
        ScanStatus::Idle => "今日尚无扫描结果，按 r 手动扫描".to_string(),
        ScanStatus::Sectors => "扫描中：拉板块列表".to_string(),
        ScanStatus::Members { done, total } => format!("扫描中 {done}/{total} 板块"),
        ScanStatus::Backfill { done, total } => format!("回补中 {done}/{total}"),
        ScanStatus::Evaluating => "扫描中：求值".to_string(),
        ScanStatus::Done => "扫描完成".to_string(),
        ScanStatus::Failed(e) => format!("扫描失败：{e}"),
    };
    format!(" {date}　{s}")
}

// ── 渲染

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectorCol {
    Rank,
    Name,
    Kind,
    ChangePct,
    Ratio,
    Heat,
}

impl SectorCol {
    fn title(self) -> &'static str {
        match self {
            SectorCol::Rank => "#",
            SectorCol::Name => "板块",
            SectorCol::Kind => "类型",
            SectorCol::ChangePct => "涨幅",
            SectorCol::Ratio => "量比",
            SectorCol::Heat => "热度",
        }
    }

    fn cells(self) -> u16 {
        match self {
            SectorCol::Rank => 3,
            SectorCol::Name => 16,
            SectorCol::Kind => 4,
            SectorCol::ChangePct => 9,
            SectorCol::Ratio => 8,
            SectorCol::Heat => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickCol {
    Code,
    Name,
    Last,
    ChangePct,
    Stance,
    Fresh,
    Facets,
}

impl PickCol {
    fn title(self) -> &'static str {
        match self {
            PickCol::Code => "代码",
            PickCol::Name => "名称",
            PickCol::Last => "现价",
            PickCol::ChangePct => "涨幅",
            PickCol::Stance => "判定",
            PickCol::Fresh => "新鲜度",
            PickCol::Facets => "维度",
        }
    }

    fn cells(self) -> u16 {
        match self {
            PickCol::Code => 10,
            PickCol::Name => 12,
            PickCol::Last => 10,
            PickCol::ChangePct => 8,
            PickCol::Stance => 14,
            PickCol::Fresh => 10,
            PickCol::Facets => 30,
        }
    }
}

const SECTOR_ALL: &[SectorCol] = &[
    SectorCol::Rank,
    SectorCol::Name,
    SectorCol::Kind,
    SectorCol::ChangePct,
    SectorCol::Ratio,
    SectorCol::Heat,
];

fn sector_columns(bp: Breakpoint) -> &'static [SectorCol] {
    match bp {
        Breakpoint::Wide => SECTOR_ALL,
        Breakpoint::Medium => &SECTOR_ALL[..5],
        Breakpoint::Narrow | Breakpoint::TooSmall => &SECTOR_ALL[..4],
    }
}

/// 窄屏保代码与判定 —— 少了判定这一屏就没有意义了，所以它不能被从右往左砍掉。
fn pick_columns(bp: Breakpoint) -> &'static [PickCol] {
    const WIDE: &[PickCol] = &[
        PickCol::Code,
        PickCol::Name,
        PickCol::Last,
        PickCol::ChangePct,
        PickCol::Stance,
        PickCol::Fresh,
        PickCol::Facets,
    ];
    const MEDIUM: &[PickCol] = &[
        PickCol::Code,
        PickCol::Name,
        PickCol::Last,
        PickCol::ChangePct,
        PickCol::Stance,
        PickCol::Fresh,
    ];
    const NARROW: &[PickCol] = &[PickCol::Code, PickCol::Name, PickCol::Stance];
    match bp {
        Breakpoint::Wide => WIDE,
        Breakpoint::Medium => MEDIUM,
        Breakpoint::Narrow | Breakpoint::TooSmall => NARROW,
    }
}

pub struct PanelView<'a> {
    pub panel: &'a Panel,
    pub status: &'a ScanStatus,
    pub quotes: &'a [Quote],
    pub sector_cursor: usize,
    pub pick_cursor: usize,
    /// 焦点在标的列表（`Tab` 切换）
    pub focus_picks: bool,
    pub bp: Breakpoint,
}

pub fn render(frame: &mut Frame, area: Rect, v: &PanelView) {
    let [status, top, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Fill(1),
    ])
    .areas(area);

    let has_data = !v.panel.sectors.is_empty();
    frame.render_widget(
        Paragraph::new(status_line(&v.panel.date, v.status, has_data)),
        status,
    );
    if !has_data {
        frame.render_widget(
            Paragraph::new("　还没有可看的扫描结果").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("板块热度"),
            ),
            top,
        );
        return;
    }
    sector_table(frame, top, v);
    pick_table(frame, bottom, v);
}

fn sector_table(frame: &mut Frame, area: Rect, v: &PanelView) {
    let cols = sector_columns(v.bp);
    let rows: Vec<Row> = v
        .panel
        .sectors
        .iter()
        .enumerate()
        .map(|(i, s)| {
            Row::new(cols.iter().map(|c| {
                let text = match c {
                    SectorCol::Rank => format!("{}", i + 1),
                    SectorCol::Name => s.name.clone(),
                    SectorCol::Kind => s.kind.label().to_string(),
                    SectorCol::ChangePct => format!("{:+.2}%", s.heat.today_change),
                    // 没有基准时显示「—」而不是编一个 1.00 出来
                    SectorCol::Ratio => match s.heat.turnover_ratio {
                        Some(r) => format!("{r:.2}x"),
                        None => "—".to_string(),
                    },
                    SectorCol::Heat => format!("{:.2}", s.heat.heat),
                };
                let style = match c {
                    SectorCol::ChangePct => Style::default().fg(up_down(s.heat.today_change)),
                    _ => Style::default(),
                };
                Cell::from(truncate_display(&text, c.cells() as usize)).style(style)
            }))
        })
        .collect();
    table(
        frame,
        area,
        "板块热度",
        cols.iter().map(|c| (c.title(), c.cells())),
        rows,
        v.sector_cursor,
        !v.focus_picks,
    );
}

fn pick_table(frame: &mut Frame, area: Rect, v: &PanelView) {
    let cols = pick_columns(v.bp);
    let sector = v.panel.sector(v.sector_cursor);
    let title = match sector {
        Some(s) => format!("{}　top {}", s.name, s.picks.len()),
        None => "标的".to_string(),
    };
    let rows: Vec<Row> = sector
        .map(|s| {
            s.picks
                .iter()
                .map(|p| {
                    let q = v.quotes.iter().find(|q| q.symbol == p.symbol);
                    Row::new(cols.iter().map(|c| {
                        let text = match c {
                            PickCol::Code => p.symbol.to_string(),
                            PickCol::Name => p.name.clone(),
                            // 没有报价就显示「—」，不拿别的数字顶替
                            PickCol::Last => q.map_or("—".into(), |q| q.last.to_string()),
                            PickCol::ChangePct => {
                                q.map_or("—".into(), |q| format!("{:.2}%", q.change_pct))
                            }
                            PickCol::Stance => stance_label(p),
                            PickCol::Fresh => fresh_label(p),
                            PickCol::Facets => p.facets.clone(),
                        };
                        let style = match c {
                            PickCol::Last | PickCol::ChangePct => Style::default().fg(q
                                .map_or(Color::DarkGray, |q| {
                                    if q.is_up() { Color::Red } else { Color::Green }
                                })),
                            PickCol::Stance => Style::default().fg(stance_color(p)),
                            PickCol::Fresh | PickCol::Facets => {
                                Style::default().fg(Color::DarkGray)
                            }
                            _ => Style::default(),
                        };
                        Cell::from(truncate_display(&text, c.cells() as usize)).style(style)
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    table(
        frame,
        area,
        &title,
        cols.iter().map(|c| (c.title(), c.cells())),
        rows,
        v.pick_cursor,
        v.focus_picks,
    );
}

/// 两张表长得一样，只是列与数据不同。焦点那张用反显，另一张用加粗 ——
/// 完全不标的话，翻标的时会看不出这些标的属于哪个板块。
fn table<'a>(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    cols: impl Iterator<Item = (&'a str, u16)> + Clone,
    rows: Vec<Row<'a>>,
    cursor: usize,
    focused: bool,
) {
    let header = Row::new(
        cols.clone()
            .map(|(t, w)| Cell::from(truncate_display(t, w as usize))),
    );
    let widths: Vec<Constraint> = cols.map(|(_, w)| Constraint::Length(w)).collect();
    let highlight = if focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let t = Table::new(rows, widths)
        .header(header.style(Style::default().add_modifier(Modifier::REVERSED)))
        .row_highlight_style(highlight)
        .block(Block::default().borders(Borders::ALL).title(title.to_string()));
    let mut state = TableState::default().with_selected(Some(cursor));
    frame.render_stateful_widget(t, area, &mut state);
}

fn up_down(v: f64) -> Color {
    if v >= 0.0 { Color::Red } else { Color::Green }
}

/// 库里的 `stance` → 面板文案。
///
/// 三种「不是做多」必须能被区分：**回补中**是历史还没补完、等一会儿就有；
/// **数据不足**是补完了根数仍然不够、结论不可信；**观望（预备）**是 top 没凑满时
/// 用预备维度看多的补位（`scan_results` 里 `watch` 的行按构造就是它）。
fn stance_label(p: &PanelPick) -> String {
    if p.stance == BACKFILLING {
        return "回补中".to_string();
    }
    match Stance::parse(&p.stance) {
        Some(Stance::Watch) => format!("观望（{READY_FACET}）"),
        Some(s) => s.label().to_string(),
        None => p.stance.clone(),
    }
}

fn stance_color(p: &PanelPick) -> Color {
    match Stance::parse(&p.stance) {
        Some(Stance::Long) => Color::Red,
        Some(Stance::Exit) => Color::Green,
        Some(Stance::Watch) => Color::Yellow,
        _ => Color::DarkGray,
    }
}

fn fresh_label(p: &PanelPick) -> String {
    match p.fresh_bars {
        Some(n) => format!("{n} 根前"),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sector::{Member, Sector, Snapshot};
    use crate::store::ScanRow;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use unicode_width::UnicodeWidthStr;

    fn sym(s: &str) -> Symbol {
        Symbol::parse(s).unwrap()
    }

    fn pick(code: &str, name: &str, stance: &str, fresh: Option<i64>) -> PanelPick {
        PanelPick {
            symbol: sym(code),
            name: name.into(),
            stance: stance.into(),
            fresh_bars: fresh,
            facets: "预备↑ 位置↑ 确认↑ 趋势→ 过滤↓".into(),
        }
    }

    fn sample() -> Panel {
        Panel {
            date: "2026-09-18".into(),
            sectors: vec![
                PanelSector {
                    name: "华为汽车".into(),
                    kind: SectorKind::Concept,
                    heat: Heat {
                        heat: 12.5,
                        turnover_ratio: Some(3.0),
                        cum_change: 9.5,
                        today_change: 3.2,
                    },
                    picks: vec![
                        pick("CN:002965", "祥鑫科技", "long", Some(2)),
                        pick("CN:002786", "银宝山新", "watch", None),
                        pick("CN:300750", "宁德时代", "insufficient", None),
                        pick("CN:600519", "贵州茅台", BACKFILLING, None),
                    ],
                },
                PanelSector {
                    name: "有色金属采选".into(),
                    kind: SectorKind::Industry,
                    heat: Heat {
                        heat: 2.0,
                        turnover_ratio: None,
                        cum_change: 2.0,
                        today_change: -0.5,
                    },
                    picks: vec![pick("CN:000001", "平安银行", "exit", None)],
                },
            ],
        }
    }

    fn view<'a>(panel: &'a Panel, status: &'a ScanStatus, bp: Breakpoint) -> PanelView<'a> {
        PanelView {
            panel,
            status,
            quotes: &[],
            sector_cursor: 0,
            pick_cursor: 0,
            focus_picks: false,
            bp,
        }
    }

    fn draw(w: u16, h: u16, v: &PanelView) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            let area = f.area();
            render(f, area, v)
        })
        .unwrap();
        term.backend().buffer().clone()
    }

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

    fn text(buf: &Buffer, w: u16, h: u16) -> String {
        (0..h)
            .map(|y| line_text(buf, y, w))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_text(w: u16, h: u16, v: &PanelView) -> String {
        text(&draw(w, h, v), w, h)
    }

    #[test]
    fn 各种尺寸都不panic并且每行宽度等于终端宽() {
        let panel = sample();
        for (w, h) in [(40u16, 24u16), (60, 24), (80, 24), (120, 40), (200, 60)] {
            let bp = Breakpoint::of(Rect::new(0, 0, w, h));
            let buf = draw(w, h, &view(&panel, &ScanStatus::Done, bp));
            for y in 0..h {
                let line = line_text(&buf, y, w);
                assert_eq!(
                    line.width(),
                    w as usize,
                    "{w}x{h} 第 {y} 行宽度 {} != {w}，CJK 宽度算错了：{line:?}",
                    line.width()
                );
            }
        }
    }

    #[test]
    fn 板块名与股票名按显示格截断不切半个汉字() {
        // 「有色金属采选」12 格放得进 16 格的板块列；股票名同理
        let panel = sample();
        let t = render_text(120, 40, &view(&panel, &ScanStatus::Done, Breakpoint::Wide));
        assert!(t.contains("华为汽车"), "{t}");
        assert!(t.contains("有色金属采选"), "{t}");
        assert!(t.contains("祥鑫科技"), "{t}");
        assert!(!t.contains('\u{fffd}'), "出现了替换字符，说明按码点切了");
    }

    #[test]
    fn 板块行带类型涨幅量比热度() {
        let panel = sample();
        let t = render_text(120, 40, &view(&panel, &ScanStatus::Done, Breakpoint::Wide));
        assert!(t.contains("概念") && t.contains("行业"), "缺类型：{t}");
        assert!(t.contains("+3.20%"), "缺当日涨幅：{t}");
        assert!(t.contains("3.00x"), "缺量比：{t}");
        assert!(t.contains("12.50"), "缺热度：{t}");
        assert!(t.contains("—"), "没有基准的板块该显示「—」：{t}");
    }

    #[test]
    fn 标的行带代码判定新鲜度与五维度缩写() {
        let panel = sample();
        let t = render_text(120, 40, &view(&panel, &ScanStatus::Done, Breakpoint::Wide));
        assert!(t.contains("CN:002965"), "{t}");
        assert!(t.contains("做多"), "{t}");
        assert!(t.contains("2 根前"), "缺新鲜度：{t}");
        assert!(t.contains("预备↑"), "缺维度缩写：{t}");
    }

    #[test]
    fn 回补中与数据不足与预备各自标注() {
        // 这三种「不是做多」的原因完全不同，混成一句话就等于没说
        let panel = sample();
        let t = render_text(120, 40, &view(&panel, &ScanStatus::Done, Breakpoint::Wide));
        assert!(t.contains("回补中"), "缺「回补中」：{t}");
        assert!(t.contains("数据不足"), "缺「数据不足」：{t}");
        assert!(t.contains("预备）"), "watch 行要标「预备」：{t}");
    }

    #[test]
    fn 没有报价时现价与涨幅显示破折号() {
        let panel = sample();
        let t = render_text(120, 40, &view(&panel, &ScanStatus::Done, Breakpoint::Wide));
        assert!(t.contains("—"), "{t}");
    }

    #[test]
    fn 有报价时显示现价与涨幅() {
        use chrono::{TimeZone, Utc};
        use rust_decimal::Decimal;
        use std::str::FromStr;
        let d = |s: &str| Decimal::from_str(s).unwrap();
        let q = Quote {
            symbol: sym("CN:002965"),
            name: "祥鑫科技".into(),
            last: d("38.15"),
            prev_close: d("35"),
            open: d("35"),
            high: d("39"),
            low: d("34"),
            volume: d("100"),
            change: d("3.15"),
            change_pct: d("9.0"),
            stamped_at: Utc.with_ymd_and_hms(2026, 9, 18, 2, 30, 0).unwrap(),
            source: "tencent",
        };
        let panel = sample();
        let quotes = vec![q];
        let mut v = view(&panel, &ScanStatus::Done, Breakpoint::Wide);
        v.quotes = &quotes;
        let t = render_text(120, 40, &v);
        assert!(t.contains("38.15"), "缺现价：{t}");
        assert!(t.contains("9.00%"), "缺涨幅：{t}");
    }

    #[test]
    fn 窄屏砍列后仍有代码与判定() {
        let panel = sample();
        let t = render_text(60, 24, &view(&panel, &ScanStatus::Done, Breakpoint::Narrow));
        assert!(t.contains("代码"), "{t}");
        assert!(t.contains("判定"), "窄屏砍掉判定这一屏就没意义了：{t}");
        assert!(!t.contains("维度"), "窄屏不该还留着维度列：{t}");
    }

    #[test]
    fn 四种扫描状态各有各的文案() {
        let panel = sample();
        let cases = [
            (ScanStatus::Members { done: 3, total: 8 }, "扫描中 3/8"),
            (ScanStatus::Backfill { done: 12, total: 160 }, "回补中 12/160"),
            (ScanStatus::Failed("新浪返回空".into()), "扫描失败：新浪返回空"),
            (ScanStatus::Done, "扫描完成"),
        ];
        for (status, want) in cases {
            let t = render_text(120, 40, &view(&panel, &status, Breakpoint::Wide));
            assert!(t.contains(want), "状态 {status:?} 应显示 {want:?}：{t}");
            assert!(t.contains("2026-09-18"), "顶部要有扫描日期：{t}");
        }
    }

    #[test]
    fn 没结果时说清是没扫还是扫完了() {
        // 「没在扫 + 库里空」和「没在扫 + 有结果」是两件事
        assert!(status_line("2026-09-18", &ScanStatus::Idle, false).contains("尚无扫描结果"));
        assert!(!status_line("2026-09-18", &ScanStatus::Idle, true).contains("尚无"));
    }

    #[test]
    fn 空面板不panic并给出提示() {
        let panel = Panel {
            date: "2026-09-18".into(),
            sectors: vec![],
        };
        let t = render_text(120, 40, &view(&panel, &ScanStatus::Idle, Breakpoint::Wide));
        assert!(t.contains("尚无扫描结果"), "{t}");
    }

    // ── 进度归约

    #[test]
    fn 进度归约成状态且log不改变状态() {
        let mut s = ScanStatus::Idle;
        apply(&mut s, &Progress::Sectors);
        assert_eq!(s, ScanStatus::Sectors);
        apply(&mut s, &Progress::Log("一大段表格".into()));
        assert_eq!(s, ScanStatus::Sectors, "CLI 的详细文本不该改面板状态");
        apply(&mut s, &Progress::Backfill { done: 1, total: 9 });
        assert_eq!(s, ScanStatus::Backfill { done: 1, total: 9 });
        apply(&mut s, &Progress::Done);
        assert_eq!(s, ScanStatus::Done);
    }

    #[test]
    fn 只有跑着的阶段算正在扫描() {
        for s in [
            ScanStatus::Sectors,
            ScanStatus::Members { done: 1, total: 8 },
            ScanStatus::Backfill { done: 1, total: 2 },
            ScanStatus::Evaluating,
        ] {
            assert!(s.running(), "{s:?} 该算正在扫描");
        }
        for s in [
            ScanStatus::Idle,
            ScanStatus::Done,
            ScanStatus::Failed("x".into()),
        ] {
            assert!(!s.running(), "{s:?} 不该算正在扫描");
        }
    }

    #[test]
    fn 失败的原因原样带到状态里() {
        let mut s = ScanStatus::Sectors;
        apply(&mut s, &Progress::Failed("概念板块列表拉取失败：超时".into()));
        assert_eq!(
            status_line("2026-09-18", &s, false),
            " 2026-09-18　扫描失败：概念板块列表拉取失败：超时"
        );
    }

    // ── 装载

    fn seeded() -> Store {
        let st = Store::open_in_memory().unwrap();
        let date = "2026-09-18";
        st.record_sectors(
            Market::Cn,
            date,
            &[
                Sector {
                    code: "gn_a".into(),
                    name: "华为汽车".into(),
                    kind: SectorKind::Concept,
                    snapshot: Snapshot {
                        change_pct: 3.0,
                        turnover: 300.0,
                    },
                },
                Sector {
                    code: "hy_b".into(),
                    name: "玻璃行业".into(),
                    kind: SectorKind::Industry,
                    snapshot: Snapshot {
                        change_pct: 9.0,
                        turnover: 100.0,
                    },
                },
            ],
        )
        .unwrap();
        for (code, symbol, name) in [
            ("gn_a", "CN:002965", "祥鑫科技"),
            ("hy_b", "CN:600519", "贵州茅台"),
        ] {
            st.record_members(
                Market::Cn,
                code,
                date,
                &[Member {
                    symbol: sym(symbol),
                    name: name.into(),
                    change_pct: 1.0,
                }],
            )
            .unwrap();
        }
        st.record_scan_results(
            Market::Cn,
            date,
            &[
                ScanRow {
                    sector_code: "gn_a".into(),
                    symbol: sym("CN:002965"),
                    rank: 1,
                    stance: "long".into(),
                    freshness: Some(2),
                    facets_json: r#"[{"label":"预备","state":"↑","detail":"x"}]"#.into(),
                },
                ScanRow {
                    sector_code: "hy_b".into(),
                    symbol: sym("CN:600519"),
                    rank: 1,
                    stance: BACKFILLING.into(),
                    freshness: None,
                    facets_json: "[]".into(),
                },
            ],
        )
        .unwrap();
        st
    }

    #[test]
    fn 从库里装出板块名类型与标的名称() {
        let st = seeded();
        let p = load(&st, Market::Cn, "2026-09-18", &ScanParams::default()).unwrap();
        assert_eq!(p.sectors.len(), 2);
        let names: Vec<&str> = p.sectors.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"华为汽车") && names.contains(&"玻璃行业"), "{names:?}");
        let a = p.sectors.iter().find(|s| s.name == "华为汽车").unwrap();
        assert_eq!(a.kind, SectorKind::Concept);
        assert_eq!(a.picks[0].name, "祥鑫科技", "名称要从候选表 join 出来");
        assert_eq!(a.picks[0].facets, "预备↑");
        assert_eq!(a.picks[0].fresh_bars, Some(2));
    }

    #[test]
    fn 板块按热度降序而不是按板块码() {
        // 玻璃行业涨 9%、华为汽车涨 3%，都没有历史 → 热度就是当日涨幅
        let st = seeded();
        let p = load(&st, Market::Cn, "2026-09-18", &ScanParams::default()).unwrap();
        assert_eq!(p.sectors[0].name, "玻璃行业", "热度高的该排前面");
    }

    #[test]
    fn 面板标的去重后交给批量报价() {
        let st = seeded();
        let p = load(&st, Market::Cn, "2026-09-18", &ScanParams::default()).unwrap();
        let mut got: Vec<String> = p.symbols().iter().map(|s| s.to_string()).collect();
        got.sort();
        assert_eq!(got, ["CN:002965", "CN:600519"]);
    }

    #[test]
    fn 今天没扫过时装出空面板而不是报错() {
        let st = seeded();
        let p = load(&st, Market::Cn, "2026-09-19", &ScanParams::default()).unwrap();
        assert!(p.sectors.is_empty());
        assert_eq!(p.date, "2026-09-19");
    }

    #[test]
    fn 维度json坏了只丢那一行缩写() {
        assert_eq!(facets_label("不是 json"), "");
        assert_eq!(facets_label("[]"), "");
        assert_eq!(
            facets_label(r#"[{"label":"预备","state":"↑"},{"label":"位置","state":"↓"}]"#),
            "预备↑ 位置↓"
        );
    }
}
