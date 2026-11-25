//! Terminal colors and the small text helpers every view shares.

use hypereffect::{Value, serde_json};

#[derive(Clone, Copy)]
pub struct Style {
    pub color: bool,
    /// Longest text shown, in characters.
    pub max_text: usize,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }

    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }

    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }

    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }

    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }

    pub fn magenta(&self, s: &str) -> String {
        self.paint("35", s)
    }

    pub fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
}

pub fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// `s` as a JSON string, cut to `max` characters.
pub fn quote(s: &str, max: usize) -> String {
    compact(&Value::String(clip(s, max)))
}

/// `s` cut to `max` characters, with an ellipsis if it was longer.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}
