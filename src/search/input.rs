use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Single-line text input with shell-style editing keys.
#[derive(Clone, Debug, Default)]
pub struct Input {
    pub text: String,
    /// Cursor position in chars.
    pub cursor: usize,
}

impl Input {
    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// Returns true if the text changed. Keys it doesn't handle are ignored.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('w') if ctrl => {
                let end = self.byte_at(self.cursor);
                let head = &self.text[..end];
                let trimmed = head.trim_end_matches(' ');
                let start = trimmed.rfind([' ', '/']).map(|i| i + 1).unwrap_or(0);
                let removed = self.text[start..end].chars().count();
                self.text.replace_range(start..end, "");
                self.cursor -= removed;
                removed > 0
            }
            KeyCode::Char('u') if ctrl => {
                let end = self.byte_at(self.cursor);
                self.text.replace_range(..end, "");
                let changed = self.cursor > 0;
                self.cursor = 0;
                changed
            }
            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                false
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor = self.len();
                false
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                let at = self.byte_at(self.cursor);
                self.text.insert(at, c);
                self.cursor += 1;
                true
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte_at(self.cursor);
                self.text.remove(at);
                true
            }
            KeyCode::Delete if self.cursor < self.len() => {
                let at = self.byte_at(self.cursor);
                self.text.remove(at);
                true
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                false
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.len());
                false
            }
            KeyCode::Home => {
                self.cursor = 0;
                false
            }
            KeyCode::End => {
                self.cursor = self.len();
                false
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn editing() {
        let mut i = Input::default();
        for c in "src/main rs".chars() {
            i.handle(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(i.handle(key(KeyCode::Char('w'), KeyModifiers::CONTROL)));
        assert_eq!(i.text, "src/main ");
        i.handle(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(i.text, "src/");
        i.handle(key(KeyCode::Left, KeyModifiers::NONE));
        i.handle(key(KeyCode::Char('é'), KeyModifiers::NONE));
        assert_eq!(i.text, "srcé/");
        i.handle(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(i.text, "src/");
        i.handle(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!((i.text.as_str(), i.cursor), ("/", 0));
    }
}
