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

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::core::strategy::vegas::VegasParams;
use crate::core::bar::Timeframe;
use crate::ui::detail::{AiPane, AiState, IndicatorKind};
use crate::ui::viewport::Viewport;

/// 界面当前停在哪一屏
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Watchlist,
    Detail,
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
        }
    }

    /// 不关心 K 线根数时的便利包装（列表屏用不到视口）
    #[cfg(test)]
    pub fn on_key(&mut self, key: KeyEvent, row_count: usize) {
        self.on_key_with(key, row_count, 0)
    }

    /// `bar_count` 是当前已加载的 K 线根数，视口操作要靠它夹紧边界
    pub fn on_key_with(&mut self, key: KeyEvent, row_count: usize, bar_count: usize) {
        // 只响应按下 —— 否则 Windows 终端上按一次会触发两次
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.screen {
            Screen::Watchlist => self.on_key_watchlist(key, row_count),
            Screen::Detail => self.on_key_detail(key, row_count, bar_count),
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
                self.screen = Screen::Detail;
                // 每次进详情都回到最新，不带着上次的滚动位置
                self.viewport = Viewport::default();
                self.bars_dirty = true;
            }
            _ => {}
        }
    }

    /// 文本区开着时它吃掉所有按键。让 Tab 在这时候还能切周期只会让人搞不清
    /// 屏幕上那段解读到底在说哪一份数据。
    fn on_key_ai(&mut self, key: KeyEvent) {
        const PAGE: usize = 10;
        let Some(pane) = &mut self.ai else { return };
        let max = pane.max_scroll.get();
        match key.code {
            // Esc 回到详情屏而不是列表 —— 关一个浮层不该顺手退两层
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

    fn on_key_detail(&mut self, key: KeyEvent, row_count: usize, bar_count: usize) {
        if self.ai.is_some() {
            self.on_key_ai(key);
            return;
        }
        match key.code {
            // 详情里 q 和 Esc 都是返回列表，不是退出程序 ——
            // 在子屏按 q 直接杀掉程序是很讨厌的行为
            KeyCode::Char('q') | KeyCode::Esc => {
                self.screen = Screen::Watchlist;
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
            KeyCode::Down | KeyCode::Char('j') if self.selected + 1 < row_count => {
                self.selected += 1;
                self.viewport = Viewport::default();
                self.bars_dirty = true;
            }
            KeyCode::Up | KeyCode::Char('k') if self.selected > 0 => {
                self.selected -= 1;
                self.viewport = Viewport::default();
                self.bars_dirty = true;
            }
            _ => {}
        }
    }
}

impl App {
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
        app.on_key_with(press(KeyCode::Char('=')), 3, 1000);
        assert!(app.viewport.span < span0, "= 应放大（可视根数变少）");
        app.on_key_with(press(KeyCode::Char('-')), 3, 1000);
        app.on_key_with(press(KeyCode::Char('-')), 3, 1000);
        assert!(app.viewport.span > span0, "- 应缩小（可视根数变多）");
    }

    #[test]
    fn 加号与等号同效免得按shift() {
        let mut a = App::new();
        let mut b = App::new();
        a.on_key(press(KeyCode::Enter), 3);
        b.on_key(press(KeyCode::Enter), 3);
        a.on_key_with(press(KeyCode::Char('=')), 3, 1000);
        b.on_key_with(press(KeyCode::Char('+')), 3, 1000);
        assert_eq!(a.viewport.span, b.viewport.span);
    }

    #[test]
    fn 左右键滚动而不是切周期() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        let tf = app.timeframe;
        app.on_key_with(press(KeyCode::Left), 3, 1000);
        assert!(!app.viewport.at_latest(), "← 应向更早滚动");
        assert_eq!(app.timeframe, tf, "← 不该再切周期 —— 那是 Tab 的活");
        app.on_key_with(press(KeyCode::Right), 3, 1000);
        assert!(app.viewport.at_latest(), "→ 应滚回最新");
    }

    #[test]
    fn 滚动不触发重新拉取() {
        // 数据已在内存里，滚动只是换个窗口看，不该再打一次网络
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.bars_dirty = false;
        app.on_key_with(press(KeyCode::Left), 3, 1000);
        app.on_key_with(press(KeyCode::Char('=')), 3, 1000);
        assert!(!app.bars_dirty, "滚动和缩放都不该触发拉取");
    }

    #[test]
    fn home和end跳到首尾() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.on_key_with(press(KeyCode::Home), 3, 1000);
        assert_eq!(app.viewport.range(1000).0, 0, "Home 应跳到最早");
        app.on_key_with(press(KeyCode::End), 3, 1000);
        assert_eq!(app.viewport.range(1000).1, 1000, "End 应跳到最新");
    }

    #[test]
    fn 切周期与切标的都重置视口() {
        // 换了标的还带着上一只的滚动位置会很困惑
        for key in [KeyCode::Tab, KeyCode::Down] {
            let mut app = App::new();
            app.on_key(press(KeyCode::Enter), 3);
            app.on_key_with(press(KeyCode::Left), 3, 1000);
            assert!(!app.viewport.at_latest());
            app.on_key_with(press(key), 3, 1000);
            assert!(app.viewport.at_latest(), "{key:?} 之后视口应重置到最新");
        }
    }

    #[test]
    fn 详情里tab切周期并触发重新拉取() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.bars_dirty = false;
        let before = app.timeframe;
        app.on_key_with(press(KeyCode::Tab), 3, 1000);
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
            app.on_key_with(press(k), 3, 1000);
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
