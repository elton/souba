pub mod layout;
pub mod watchlist;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub struct App {
    pub should_quit: bool,
    pub selected: usize,
}

impl App {
    pub fn new() -> Self {
        Self {
            should_quit: false,
            selected: 0,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, row_count: usize) {
        // 只响应按下 —— 否则 Windows 终端上按一次会触发两次
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if row_count > 0 {
                    self.selected = (self.selected + 1).min(row_count - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
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
