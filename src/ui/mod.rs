pub mod braille;
pub mod chart;
pub mod detail;
pub mod layout;
pub mod watchlist;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::core::bar::Timeframe;
use crate::ui::detail::IndicatorKind;

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
    /// 用户改过周期/标的后置位，主循环据此重新拉历史
    pub bars_dirty: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            should_quit: false,
            selected: 0,
            screen: Screen::Watchlist,
            timeframe: Timeframe::Day,
            indicator: IndicatorKind::Macd,
            bars_dirty: false,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, row_count: usize) {
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
            Screen::Detail => self.on_key_detail(key, row_count),
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
                self.bars_dirty = true;
            }
            _ => {}
        }
    }

    fn on_key_detail(&mut self, key: KeyEvent, row_count: usize) {
        match key.code {
            // 详情里 q 和 Esc 都是返回列表，不是退出程序 ——
            // 在子屏按 q 直接杀掉程序是很讨厌的行为
            KeyCode::Char('q') | KeyCode::Esc => self.screen = Screen::Watchlist,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.timeframe = self.timeframe.next();
                self.bars_dirty = true;
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.timeframe = self.timeframe.prev();
                self.bars_dirty = true;
            }
            KeyCode::Char('i') => self.indicator = self.indicator.next(),
            // 详情里上下切标的，不用退回列表
            KeyCode::Down | KeyCode::Char('j') if self.selected + 1 < row_count => {
                self.selected += 1;
                self.bars_dirty = true;
            }
            KeyCode::Up | KeyCode::Char('k') if self.selected > 0 => {
                self.selected -= 1;
                self.bars_dirty = true;
            }
            _ => {}
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
    fn 详情里tab切周期并触发重新拉取() {
        let mut app = App::new();
        app.on_key(press(KeyCode::Enter), 3);
        app.bars_dirty = false;
        let before = app.timeframe;
        app.on_key(press(KeyCode::Tab), 3);
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
