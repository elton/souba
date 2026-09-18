pub mod braille;
pub mod canvas;
pub mod detail;
pub mod kitty;
pub mod surface;
pub mod timeaxis;
pub mod viewport;
pub mod layout;
pub mod paint;
pub mod watchlist;
pub mod opportunities;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::core::strategy::vegas::VegasParams;
use crate::core::bar::Timeframe;
use crate::sync;
use crate::ui::detail::{AiPane, AiState, IndicatorKind};
use crate::ui::opportunities::ScanStatus;
use crate::ui::viewport::Viewport;

/// 顶栏右侧那一截同步状态。
///
/// 「没在同步」必须说得出原因 —— 静默地不同步，跟把延迟数据当实时显示是同一类谎。
pub fn sync_label(s: &sync::Status) -> String {
    match s {
        sync::Status::Idle => String::new(),
        sync::Status::Running => "同步中… ".to_string(),
        sync::Status::Ok(at) => format!("已同步 {at} "),
        sync::Status::Failed(why) => format!("未同步：{why} "),
    }
}

/// 界面当前停在哪一屏
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Watchlist,
    Detail,
    /// 机会面板：当天扫描结果
    Opportunities,
}

/// 各列表当前的行数与已加载的 K 线根数。按键处理靠它夹紧边界 ——
/// 三个屏的行数来源不同（自选股 / 板块 / 板块内标的），一个数字说不清。
#[derive(Debug, Clone, Copy, Default)]
pub struct Rows {
    pub watch: usize,
    pub sectors: usize,
    pub picks: usize,
    pub bars: usize,
}

pub struct App {
    pub should_quit: bool,
    pub selected: usize,
    pub screen: Screen,
    pub timeframe: Timeframe,
    pub indicator: IndicatorKind,
    /// K 线视口：显示哪一段、显示多少根
    pub viewport: Viewport,
    /// 鼠标在终端里的格子坐标。None = 不在图上（或终端不报鼠标）
    pub mouse: Option<(u16, u16)>,
    /// 用户改过周期/标的后置位，主循环据此重新拉历史
    pub bars_dirty: bool,
    /// 启动时从 settings 表装载；`souba set` 改了要重启才生效
    pub vegas: VegasParams,
    /// AI 解读文本区。`Some` 表示它开着，此时详情屏的按键全归它。
    pub ai: Option<AiPane>,
    /// 用户按了 `?` 或 `R`，主循环据此发一次解读请求。`true` = 绕过缓存重问。
    pub ai_request: Option<bool>,

    // ── 机会面板
    /// 光标停在哪个板块
    pub sector_cursor: usize,
    /// 光标停在该板块的第几只
    pub pick_cursor: usize,
    /// 焦点在标的列表而不是板块列表（`Tab` 切换）
    pub focus_picks: bool,
    /// 后台扫描的状态，主循环从进度通道归约进来
    pub scan_status: ScanStatus,
    /// 用户按了 `r`，主循环据此起一次扫描
    pub scan_request: bool,
    /// 用户按了 `a`，主循环据此把光标标的加进自选
    pub add_request: bool,
    /// 一行临时提示（「已在自选」之类），下一次按键就清掉
    pub notice: Option<String>,
    /// 详情屏是从哪一屏进来的 —— `Esc` 要回到那里，而不是一律回自选股
    pub detail_from: Screen,
    /// 与 Worker 的同步状态，主循环从同步任务的通道归约进来
    pub sync: sync::Status,
}

impl App {
    pub fn new() -> Self {
        Self {
            should_quit: false,
            selected: 0,
            screen: Screen::Watchlist,
            timeframe: Timeframe::Day,
            indicator: IndicatorKind::Macd,
            viewport: Viewport::default(),
            mouse: None,
            bars_dirty: false,
            vegas: VegasParams::default(),
            ai: None,
            ai_request: None,
            sector_cursor: 0,
            pick_cursor: 0,
            focus_picks: false,
            scan_status: ScanStatus::Idle,
            scan_request: false,
            add_request: false,
            notice: None,
            detail_from: Screen::Watchlist,
            sync: sync::Status::Idle,
        }
    }

    /// 不关心 K 线根数与面板时的便利包装（列表屏用不到视口）
    #[cfg(test)]
    pub fn on_key(&mut self, key: KeyEvent, row_count: usize) {
        self.on_key_with(
            key,
            Rows {
                watch: row_count,
                ..Default::default()
            },
        )
    }

    pub fn on_key_with(&mut self, key: KeyEvent, rows: Rows) {
        // 只响应按下 —— 否则 Windows 终端上按一次会触发两次
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.screen {
            Screen::Watchlist => self.on_key_watchlist(key, rows.watch),
            Screen::Detail => self.on_key_detail(key, rows),
            Screen::Opportunities => self.on_key_panel(key, rows),
        }
    }

    fn on_key_watchlist(&mut self, key: KeyEvent, row_count: usize) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Down | KeyCode::Char('j') if row_count > 0 => {
                self.selected = (self.selected + 1).min(row_count - 1);
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Enter if row_count > 0 => {
                self.detail_from = Screen::Watchlist;
                self.enter_detail();
            }
            // s 进机会面板。扫描结果单独一屏，不往自选股里掺
            KeyCode::Char('s') => self.screen = Screen::Opportunities,
            _ => {}
        }
    }

    /// 进详情屏。每次都回到最新，不带着上次的滚动位置。
    fn enter_detail(&mut self) {
        self.screen = Screen::Detail;
        self.viewport = Viewport::default();
        self.bars_dirty = true;
    }

    /// 机会面板。`Tab` 在板块列表与标的列表间切焦点 —— 面板里没有周期概念，
    /// 这个键空着。
    fn on_key_panel(&mut self, key: KeyEvent, rows: Rows) {
        // 文本区开着就归它 —— 面板这一层的 Esc 是「回自选股」，
        // 关一个浮层不该顺手退两层
        if self.ai.is_some() {
            self.on_key_ai(key);
            return;
        }
        // 任何一次按键都把上一条提示收掉，免得「已在自选」一直挂在底栏
        self.notice = None;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.screen = Screen::Watchlist,
            KeyCode::Tab | KeyCode::BackTab => self.focus_picks = !self.focus_picks,
            KeyCode::Down | KeyCode::Char('j') => {
                if self.focus_picks {
                    if rows.picks > 0 {
                        self.pick_cursor = (self.pick_cursor + 1).min(rows.picks - 1);
                    }
                } else if rows.sectors > 0 {
                    let next = (self.sector_cursor + 1).min(rows.sectors - 1);
                    self.move_sector(next);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus_picks {
                    self.pick_cursor = self.pick_cursor.saturating_sub(1);
                } else {
                    let next = self.sector_cursor.saturating_sub(1);
                    self.move_sector(next);
                }
            }
            KeyCode::Enter if rows.picks > 0 => {
                self.detail_from = Screen::Opportunities;
                self.enter_detail();
            }
            KeyCode::Char('a') if rows.picks > 0 => self.add_request = true,
            KeyCode::Char('r') => self.scan_request = true,
            // ? 把当天全部板块 top 交给 AI 排序。跟详情屏一个键位。
            KeyCode::Char('?') => {
                self.ai = Some(AiPane::loading());
                self.ai_request = Some(false);
            }
            _ => {}
        }
    }

    /// 换板块就把标的光标归零 —— 下一个板块的第 4 只跟这个板块的第 4 只没关系
    fn move_sector(&mut self, next: usize) {
        if next != self.sector_cursor {
            self.sector_cursor = next;
            self.pick_cursor = 0;
        }
    }

    /// 文本区开着时它吃掉所有按键。让 Tab 在这时候还能切周期只会让人搞不清
    /// 屏幕上那段解读到底在说哪一份数据。详情屏与机会面板共用这一套键位。
    fn on_key_ai(&mut self, key: KeyEvent) {
        const PAGE: usize = 10;
        let Some(pane) = &mut self.ai else { return };
        let max = pane.max_scroll.get();
        match key.code {
            // Esc 只关文本区，回到它盖住的那一屏 —— 关一个浮层不该顺手退两层
            KeyCode::Esc | KeyCode::Char('q') => self.ai = None,
            KeyCode::Up | KeyCode::Char('k') => pane.scroll = pane.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => pane.scroll = (pane.scroll + 1).min(max),
            KeyCode::PageUp => pane.scroll = pane.scroll.saturating_sub(PAGE),
            KeyCode::PageDown => pane.scroll = (pane.scroll + PAGE).min(max),
            KeyCode::Home => pane.scroll = 0,
            KeyCode::End => pane.scroll = max,
            // 大写 R 重问，避免跟别处的刷新语义打架
            KeyCode::Char('R') => {
                pane.scroll = 0;
                pane.state = AiState::Loading;
                self.ai_request = Some(true);
            }
            _ => {}
        }
    }

    fn on_key_detail(&mut self, key: KeyEvent, rows: Rows) {
        if self.ai.is_some() {
            self.on_key_ai(key);
            return;
        }
        let bar_count = rows.bars;
        // 从面板进来的详情，上下切的是面板那一列标的，不是自选股
        let from_panel = self.detail_from == Screen::Opportunities;
        let row_count = if from_panel { rows.picks } else { rows.watch };
        match key.code {
            // 详情里 q 和 Esc 都是返回，不是退出程序 ——
            // 在子屏按 q 直接杀掉程序是很讨厌的行为。回哪一屏看是从哪进来的。
            KeyCode::Char('q') | KeyCode::Esc => {
                self.screen = self.detail_from;
                self.mouse = None;
            }

            // ← → 滚动。周期切换让给 Tab —— 看图时滚动的频率远高于切周期。
            KeyCode::Left | KeyCode::Char('h') => self.viewport.pan_left(bar_count),
            KeyCode::Right | KeyCode::Char('l') => self.viewport.pan_right(bar_count),
            KeyCode::Home => self.viewport.jump_oldest(bar_count),
            KeyCode::End => self.viewport.jump_latest(),

            // = / + 放大，- / _ 缩小。= 和 + 同键，不用按 Shift。
            KeyCode::Char('=') | KeyCode::Char('+') => self.viewport.zoom_in(bar_count),
            KeyCode::Char('-') | KeyCode::Char('_') => self.viewport.zoom_out(bar_count),

            KeyCode::Tab => {
                self.timeframe = self.timeframe.next();
                self.viewport = Viewport::default();
                self.bars_dirty = true;
            }
            KeyCode::BackTab => {
                self.timeframe = self.timeframe.prev();
                self.viewport = Viewport::default();
                self.bars_dirty = true;
            }
            KeyCode::Char('i') => self.indicator = self.indicator.next(),

            // ? 让 AI 解读这一只的 Signal，R 强制重问。i 已经是切指标了。
            KeyCode::Char('?') => {
                self.ai = Some(AiPane::loading());
                self.ai_request = Some(false);
            }
            KeyCode::Char('R') => {
                self.ai = Some(AiPane::loading());
                self.ai_request = Some(true);
            }

            // 详情里上下切标的，不用退回列表
            KeyCode::Down | KeyCode::Char('j') if self.cursor(from_panel) + 1 < row_count => {
                self.set_cursor(from_panel, self.cursor(from_panel) + 1);
                self.viewport = Viewport::default();
                self.bars_dirty = true;
            }
            KeyCode::Up | KeyCode::Char('k') if self.cursor(from_panel) > 0 => {
                self.set_cursor(from_panel, self.cursor(from_panel) - 1);
                self.viewport = Viewport::default();
                self.bars_dirty = true;
            }
            _ => {}
        }
    }
}

impl App {
    fn cursor(&self, from_panel: bool) -> usize {
        if from_panel { self.pick_cursor } else { self.selected }
    }

    fn set_cursor(&mut self, from_panel: bool, v: usize) {
        if from_panel {
            self.pick_cursor = v;
        } else {
            self.selected = v;
        }
    }

    /// 鼠标移动。只在详情屏有意义 —— 列表屏没有需要十字光标读数的东西。
    pub fn on_mouse(&mut self, ev: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;
        if self.screen != Screen::Detail {
            self.mouse = None;
            return;
        }
        match ev.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) | MouseEventKind::Down(_) => {
                self.mouse = Some((ev.column, ev.row));
            }
            // 滚轮当作缩放 —— 这是看盘软件的通用手势
            MouseEventKind::ScrollUp => self.mouse = Some((ev.column, ev.row)),
            MouseEventKind::ScrollDown => self.mouse = Some((ev.column, ev.row)),
            _ => {}
        }
    }

    /// 滚轮缩放，需要知道总根数才能夹紧
    pub fn on_scroll(&mut self, up: bool, bar_count: usize) {
        if self.screen != Screen::Detail {
            return;
        }
        if up {
            self.viewport.zoom_in(bar_count);
        } else {
            self.viewport.zoom_out(bar_count);
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod app_tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        // KeyEvent::new 默认 kind = Press、state = NONE
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn q键退出() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Char('q')), 3);
        assert!(app.should_quit);
    }

    #[test]
    fn 上下移动不越界() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Up), 3);
        assert_eq!(app.selected, 0, "在顶端按上不应越界");
        for _ in 0..10 {
            app.on_key(press(KeyCode::Down), 3);
        }
        assert_eq!(app.selected, 2, "在底端按下不应越界");
    }

    #[test]
    fn 空列表时移动不panic() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Down), 0);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn 只响应按下不响应抬起() {
        let mut app = App::new();
        let mut ev = press(KeyCode::Char('q'));
        ev.kind = KeyEventKind::Release;
        app.on_key(ev, 3);
        assert!(
            !app.should_quit,
            "抬起事件不应触发退出，否则 Windows 上按一次会触发两次"
        );
    }
}

#[cfg(test)]
mod nav_tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn rows(watch: usize, bars: usize) -> Rows {
        Rows {
            watch,
            bars,
            ..Default::default()
        }
    }

    #[test]
    fn 回车进入详情() {
        let mut app = App::new();
        assert_eq!(app.screen, Screen::Watchlist);
        app.on_key(press(KeyCode::Enter), 3);
        assert_eq!(app.screen, Screen::Detail);
        assert!(app.bars_dirty, "进详情要触发拉历史");
    }

    #[test]
    fn 空列表按回车不进详情() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 0);
        assert_eq!(app.screen, Screen::Watchlist, "没有标的时进详情是无意义的");
    }

    #[test]
    fn 详情里按q是返回不是退出() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.on_key(press(KeyCode::Char('q')), 3);
        assert_eq!(app.screen, Screen::Watchlist);
        assert!(!app.should_quit, "在子屏按 q 直接杀掉程序是很讨厌的行为");
    }

    #[test]
    fn 列表里按q才退出() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Char('q')), 3);
        assert!(app.should_quit);
    }

    #[test]
    fn 等号键放大减号键缩小() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        let span0 = app.viewport.span;
        app.on_key_with(press(KeyCode::Char('=')), rows(3, 1000));
        assert!(app.viewport.span < span0, "= 应放大（可视根数变少）");
        app.on_key_with(press(KeyCode::Char('-')), rows(3, 1000));
        app.on_key_with(press(KeyCode::Char('-')), rows(3, 1000));
        assert!(app.viewport.span > span0, "- 应缩小（可视根数变多）");
    }

    #[test]
    fn 加号与等号同效免得按shift() {
        let mut a = App::new();
        let mut b = App::new();
        a.on_key(press(KeyCode::Enter), 3);
        b.on_key(press(KeyCode::Enter), 3);
        a.on_key_with(press(KeyCode::Char('=')), rows(3, 1000));
        b.on_key_with(press(KeyCode::Char('+')), rows(3, 1000));
        assert_eq!(a.viewport.span, b.viewport.span);
    }

    #[test]
    fn 左右键滚动而不是切周期() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        let tf = app.timeframe;
        app.on_key_with(press(KeyCode::Left), rows(3, 1000));
        assert!(!app.viewport.at_latest(), "← 应向更早滚动");
        assert_eq!(app.timeframe, tf, "← 不该再切周期 —— 那是 Tab 的活");
        app.on_key_with(press(KeyCode::Right), rows(3, 1000));
        assert!(app.viewport.at_latest(), "→ 应滚回最新");
    }

    #[test]
    fn 滚动不触发重新拉取() {
        // 数据已在内存里，滚动只是换个窗口看，不该再打一次网络
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.bars_dirty = false;
        app.on_key_with(press(KeyCode::Left), rows(3, 1000));
        app.on_key_with(press(KeyCode::Char('=')), rows(3, 1000));
        assert!(!app.bars_dirty, "滚动和缩放都不该触发拉取");
    }

    #[test]
    fn home和end跳到首尾() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.on_key_with(press(KeyCode::Home), rows(3, 1000));
        assert_eq!(app.viewport.range(1000).0, 0, "Home 应跳到最早");
        app.on_key_with(press(KeyCode::End), rows(3, 1000));
        assert_eq!(app.viewport.range(1000).1, 1000, "End 应跳到最新");
    }

    #[test]
    fn 切周期与切标的都重置视口() {
        // 换了标的还带着上一只的滚动位置会很困惑
        for key in [KeyCode::Tab, KeyCode::Down] {
            let mut app = App::new();
            app.on_key(press(KeyCode::Enter), 3);
            app.on_key_with(press(KeyCode::Left), rows(3, 1000));
            assert!(!app.viewport.at_latest());
            app.on_key_with(press(key), rows(3, 1000));
            assert!(app.viewport.at_latest(), "{key:?} 之后视口应重置到最新");
        }
    }

    #[test]
    fn 详情里tab切周期并触发重新拉取() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.bars_dirty = false;
        let before = app.timeframe;
        app.on_key_with(press(KeyCode::Tab), rows(3, 1000));
        assert_ne!(app.timeframe, before);
        assert!(app.bars_dirty);
    }

    #[test]
    fn 详情里i键切指标但不触发重新拉取() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.bars_dirty = false;
        app.on_key(press(KeyCode::Char('i')), 3);
        assert_eq!(app.indicator, IndicatorKind::Kdj);
        assert!(!app.bars_dirty, "切指标只是重算，不该重新拉数据");
    }

    #[test]
    fn 详情里上下切标的并重新拉取() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.bars_dirty = false;
        app.on_key(press(KeyCode::Down), 3);
        assert_eq!(app.selected, 1);
        assert!(app.bars_dirty);
    }

    #[test]
    fn 详情里到边界不越界也不重复拉取() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.on_key(press(KeyCode::Up), 3);
        app.bars_dirty = false;
        app.on_key(press(KeyCode::Up), 3);
        assert_eq!(app.selected, 0);
        assert!(!app.bars_dirty, "到顶了不该再触发拉取");
    }

    #[test]
    fn 问号键打开ai文本区并发起一次解读() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.bars_dirty = false;
        app.on_key(press(KeyCode::Char('?')), 3);
        assert!(app.ai.is_some(), "? 应打开 AI 文本区");
        assert_eq!(app.ai_request, Some(false), "首次按不该绕过缓存");
        assert!(!app.bars_dirty, "解读不该触发重新拉 K 线");
    }

    #[test]
    fn 大写r强制重问且不与刷新冲突() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.on_key(press(KeyCode::Char('?')), 3);
        app.ai_request = None;
        if let Some(p) = &mut app.ai {
            p.state = AiState::Ready("旧回答".into());
            p.scroll = 7;
        }
        app.on_key(press(KeyCode::Char('R')), 3);
        assert_eq!(app.ai_request, Some(true), "R 要绕过缓存");
        let p = app.ai.as_ref().unwrap();
        assert_eq!(p.scroll, 0, "重问要从头看");
        assert!(matches!(p.state, AiState::Loading));
    }

    #[test]
    fn 文本区里按esc回详情屏而不是退回列表() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.on_key(press(KeyCode::Char('?')), 3);
        app.on_key(press(KeyCode::Esc), 3);
        assert!(app.ai.is_none(), "Esc 应关掉文本区");
        assert_eq!(app.screen, Screen::Detail, "关浮层不该顺手退两层");
        // 再按一次才回列表
        app.on_key(press(KeyCode::Esc), 3);
        assert_eq!(app.screen, Screen::Watchlist);
    }

    #[test]
    fn 文本区开着时其余按键不穿透() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.on_key(press(KeyCode::Char('?')), 3);
        let (tf, ind, sel) = (app.timeframe, app.indicator, app.selected);
        for k in [KeyCode::Tab, KeyCode::Char('i'), KeyCode::Char('=')] {
            app.on_key_with(press(k), rows(3, 1000));
        }
        assert_eq!(app.timeframe, tf, "解读开着时切周期会让人搞不清解读的是哪份数据");
        assert_eq!(app.indicator, ind);
        assert_eq!(app.selected, sel);
        assert!(app.ai.is_some());
    }

    #[test]
    fn 文本区滚动被上限夹紧() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.on_key(press(KeyCode::Char('?')), 3);
        // 渲染会回填这个上限，这里直接模拟
        app.ai.as_ref().unwrap().max_scroll.set(3);

        for _ in 0..10 {
            app.on_key(press(KeyCode::Down), 3);
        }
        assert_eq!(app.ai.as_ref().unwrap().scroll, 3, "滚到底就不该再往下跑");
        for _ in 0..10 {
            app.on_key(press(KeyCode::Up), 3);
        }
        assert_eq!(app.ai.as_ref().unwrap().scroll, 0, "滚到顶不该越界");

        app.on_key(press(KeyCode::End), 3);
        assert_eq!(app.ai.as_ref().unwrap().scroll, 3);
        app.on_key(press(KeyCode::PageUp), 3);
        assert_eq!(app.ai.as_ref().unwrap().scroll, 0);
        app.on_key(press(KeyCode::PageDown), 3);
        assert_eq!(app.ai.as_ref().unwrap().scroll, 3);
    }

    #[test]
    fn 列表屏按问号不打开文本区() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Char('?')), 3);
        assert!(app.ai.is_none(), "列表屏还没有选中标的的 Signal 可解读");
        assert_eq!(app.ai_request, None);
    }

    #[test]
    fn ctrl_c在任何屏都退出() {
        for enter_detail in [false, true] {
            let mut app = App::new();
            if enter_detail {
                app.on_key(press(KeyCode::Enter), 3);
            }
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 3);
            assert!(app.should_quit);
        }
    }
}

#[cfg(test)]
mod panel_tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// 2 个板块、每个板块 4 只
    fn rows() -> Rows {
        Rows {
            watch: 3,
            sectors: 2,
            picks: 4,
            bars: 0,
        }
    }

    fn panel() -> App {
        let mut app = App::new();
        app.on_key(press(KeyCode::Char('s')), 3);
        app
    }

    #[test]
    fn s键进面板而不是污染自选股列表() {
        let app = panel();
        assert_eq!(app.screen, Screen::Opportunities);
    }

    #[test]
    fn 面板里esc和q都回自选股列表而不是退出() {
        for key in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut app = panel();
            app.on_key_with(press(key), rows());
            assert_eq!(app.screen, Screen::Watchlist);
            assert!(!app.should_quit, "面板里按 q 不该杀掉程序");
        }
    }

    #[test]
    fn tab在板块与标的之间切焦点() {
        let mut app = panel();
        assert!(!app.focus_picks, "进面板先选板块");
        app.on_key_with(press(KeyCode::Tab), rows());
        assert!(app.focus_picks);
        app.on_key_with(press(KeyCode::Tab), rows());
        assert!(!app.focus_picks);
    }

    #[test]
    fn 上下在焦点那一列移动且不越界() {
        let mut app = panel();
        for _ in 0..5 {
            app.on_key_with(press(KeyCode::Down), rows());
        }
        assert_eq!(app.sector_cursor, 1, "只有 2 个板块，到底就停住");
        assert_eq!(app.pick_cursor, 0, "移的是板块不是标的");

        app.on_key_with(press(KeyCode::Tab), rows());
        for _ in 0..9 {
            app.on_key_with(press(KeyCode::Down), rows());
        }
        assert_eq!(app.pick_cursor, 3, "只有 4 只，到底就停住");
        assert_eq!(app.sector_cursor, 1);
        for _ in 0..9 {
            app.on_key_with(press(KeyCode::Up), rows());
        }
        assert_eq!(app.pick_cursor, 0);
    }

    #[test]
    fn 换板块把标的光标归零() {
        // 下一个板块的第 4 只跟这个板块的第 4 只毫无关系
        let mut app = panel();
        app.on_key_with(press(KeyCode::Tab), rows());
        app.on_key_with(press(KeyCode::Down), rows());
        assert_eq!(app.pick_cursor, 1);
        app.on_key_with(press(KeyCode::Tab), rows());
        app.on_key_with(press(KeyCode::Down), rows());
        assert_eq!(app.sector_cursor, 1);
        assert_eq!(app.pick_cursor, 0);
    }

    #[test]
    fn 空面板上下移动不panic() {
        let mut app = panel();
        let empty = Rows::default();
        for key in [KeyCode::Down, KeyCode::Up] {
            app.on_key_with(press(key), empty);
            app.on_key_with(press(KeyCode::Tab), empty);
            app.on_key_with(press(key), empty);
        }
        assert_eq!(app.sector_cursor, 0);
        assert_eq!(app.pick_cursor, 0);
    }

    #[test]
    fn 回车进详情并记住是从面板来的() {
        let mut app = panel();
        app.on_key_with(press(KeyCode::Enter), rows());
        assert_eq!(app.screen, Screen::Detail);
        assert_eq!(app.detail_from, Screen::Opportunities);
        assert!(app.bars_dirty, "进详情要触发拉历史");
    }

    #[test]
    fn 从面板进的详情按esc回面板而不是自选股() {
        let mut app = panel();
        app.on_key_with(press(KeyCode::Enter), rows());
        app.on_key_with(press(KeyCode::Esc), rows());
        assert_eq!(app.screen, Screen::Opportunities);
    }

    #[test]
    fn 从面板进的详情上下切的是面板那一列标的() {
        let mut app = panel();
        app.on_key_with(press(KeyCode::Enter), rows());
        app.bars_dirty = false;
        app.on_key_with(press(KeyCode::Down), rows());
        assert_eq!(app.pick_cursor, 1);
        assert_eq!(app.selected, 0, "不该动自选股的光标");
        assert!(app.bars_dirty);
        // 到底（4 只）就停住，也不再触发拉取
        for _ in 0..5 {
            app.on_key_with(press(KeyCode::Down), rows());
        }
        assert_eq!(app.pick_cursor, 3);
    }

    #[test]
    fn 从自选股进的详情不受面板光标影响() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.on_key_with(press(KeyCode::Down), rows());
        assert_eq!(app.selected, 1);
        assert_eq!(app.pick_cursor, 0);
        app.on_key_with(press(KeyCode::Esc), rows());
        assert_eq!(app.screen, Screen::Watchlist);
    }

    #[test]
    fn 空面板回车不进详情() {
        let mut app = panel();
        app.on_key_with(press(KeyCode::Enter), Rows::default());
        assert_eq!(app.screen, Screen::Opportunities);
    }

    #[test]
    fn a加自选r重扫各置一次位() {
        let mut app = panel();
        app.on_key_with(press(KeyCode::Char('a')), rows());
        assert!(app.add_request);
        app.on_key_with(press(KeyCode::Char('r')), rows());
        assert!(app.scan_request);
    }

    #[test]
    fn 没有标的时按a不发请求() {
        let mut app = panel();
        app.on_key_with(press(KeyCode::Char('a')), Rows::default());
        assert!(!app.add_request);
    }

    #[test]
    fn 下一次按键收掉上一条提示() {
        let mut app = panel();
        app.notice = Some("已在自选".into());
        app.on_key_with(press(KeyCode::Down), rows());
        assert!(app.notice.is_none());
    }

    #[test]
    fn 面板里问号打开文本区并发起一次整组解读() {
        let mut app = panel();
        app.on_key_with(press(KeyCode::Char('?')), rows());
        assert!(app.ai.is_some(), "? 应打开 AI 文本区");
        assert_eq!(app.ai_request, Some(false), "首次按不该绕过缓存");
        assert_eq!(app.screen, Screen::Opportunities);
    }

    #[test]
    fn 面板的文本区按esc回面板而不是回列表() {
        let mut app = panel();
        app.on_key_with(press(KeyCode::Char('?')), rows());
        app.on_key_with(press(KeyCode::Esc), rows());
        assert!(app.ai.is_none(), "Esc 应关掉文本区");
        assert_eq!(app.screen, Screen::Opportunities, "关浮层不该顺手退两层");
        // 再按一次才回自选股
        app.on_key_with(press(KeyCode::Esc), rows());
        assert_eq!(app.screen, Screen::Watchlist);
    }

    #[test]
    fn 面板的文本区开着时其余按键不穿透() {
        let mut app = panel();
        app.on_key_with(press(KeyCode::Char('?')), rows());
        app.ai.as_ref().unwrap().max_scroll.set(3);
        for k in [
            KeyCode::Tab,
            KeyCode::Enter,
            KeyCode::Char('a'),
            KeyCode::Char('r'),
        ] {
            app.on_key_with(press(k), rows());
        }
        assert!(!app.focus_picks, "Tab 不该在解读开着时切焦点");
        assert_eq!(app.screen, Screen::Opportunities, "Enter 不该穿透去详情屏");
        assert!(!app.add_request, "a 不该穿透去加自选");
        assert!(!app.scan_request, "r 不该穿透去重扫");
        assert!(app.ai.is_some());
    }

    #[test]
    fn 面板的文本区滚动与重问跟详情屏一致() {
        let mut app = panel();
        app.on_key_with(press(KeyCode::Char('?')), rows());
        app.ai_request = None;
        app.ai.as_ref().unwrap().max_scroll.set(4);
        for _ in 0..9 {
            app.on_key_with(press(KeyCode::Down), rows());
        }
        assert_eq!(app.ai.as_ref().unwrap().scroll, 4, "滚到底就不该再往下跑");
        assert_eq!(app.pick_cursor, 0, "↓ 不该同时挪面板的光标");
        app.on_key_with(press(KeyCode::Home), rows());
        assert_eq!(app.ai.as_ref().unwrap().scroll, 0);

        app.on_key_with(press(KeyCode::Char('R')), rows());
        assert_eq!(app.ai_request, Some(true), "R 要绕过缓存");
        assert!(matches!(app.ai.as_ref().unwrap().state, AiState::Loading));
    }

    #[test]
    fn 顶栏说得出没在同步的原因() {
        assert_eq!(sync_label(&sync::Status::Idle), "");
        assert_eq!(sync_label(&sync::Status::Ok("14:32".into())).trim(), "已同步 14:32");
        assert_eq!(
            sync_label(&sync::Status::Failed("密钥不匹配（401）".into())).trim(),
            "未同步：密钥不匹配（401）"
        );
    }

    #[test]
    fn 面板里ctrl_c仍然退出() {
        let mut app = panel();
        app.on_key_with(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            rows(),
        );
        assert!(app.should_quit);
    }
}
