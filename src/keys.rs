//! Key chords for user-bindable actions (plugins today, bookmarks next).
//!
//! A chord is parsed from a small, forgiving string syntax — a key, optionally
//! prefixed with `ctrl-`/`alt-`/`shift-` modifiers joined by `-`:
//!
//! ```text
//! g          ctrl-g       alt-x        shift-b
//! f5         ctrl-f5      shift-f5     ctrl-alt-delete
//! enter      space        pageup
//! ```
//!
//! Matching is deliberately lenient about `shift` on letters (an uppercase
//! char already encodes it, exactly like the built-in `G`/`S`/`E` bindings)
//! and about the case of a `ctrl-`ed letter (terminals deliver `ctrl-g` as a
//! lowercase `g`). Modifiers `ctrl`/`alt` must match exactly, so a plain `g`
//! binding never fires on `ctrl-g` and vice-versa.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A parsed key binding: a base key plus the modifiers that must accompany it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    /// Only enforced for non-`Char` keys (function keys, Enter, …). For a
    /// letter, shift is folded into the char's case at parse time.
    pub shift: bool,
}

impl KeyChord {
    /// Parse a chord string. Returns a human-readable error naming the problem.
    pub fn parse(s: &str) -> Result<KeyChord, String> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err("empty key binding".into());
        }
        // A lone single character is the key itself (covers "-", "?", "g").
        if trimmed.chars().count() == 1 {
            let c = trimmed.chars().next().unwrap();
            return Ok(KeyChord {
                code: KeyCode::Char(c),
                ctrl: false,
                alt: false,
                shift: false,
            });
        }

        let parts: Vec<&str> = trimmed.split('-').collect();
        let (mods, key) = parts.split_at(parts.len() - 1);
        let key = key[0];

        let (mut ctrl, mut alt, mut shift) = (false, false, false);
        for m in mods {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "c" => ctrl = true,
                "alt" | "option" | "opt" | "meta" | "a" | "m" => alt = true,
                "shift" | "s" => shift = true,
                "" => return Err(format!("{s:?}: empty modifier (stray '-'?)")),
                other => return Err(format!("{s:?}: unknown modifier {other:?}")),
            }
        }

        let mut code = parse_key(key).ok_or_else(|| format!("{s:?}: unknown key {key:?}"))?;

        // Fold shift into a letter's case so matching stays case-based (like the
        // built-in bindings); keep it as a flag for function/named keys.
        if let (true, KeyCode::Char(c)) = (shift, code)
            && c.is_ascii_alphabetic()
        {
            code = KeyCode::Char(c.to_ascii_uppercase());
            shift = false;
        }

        Ok(KeyChord {
            code,
            ctrl,
            alt,
            shift,
        })
    }

    /// Whether `event` triggers this chord.
    pub fn matches(&self, event: &KeyEvent) -> bool {
        let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
        let alt = event.modifiers.contains(KeyModifiers::ALT);
        if ctrl != self.ctrl || alt != self.alt {
            return false;
        }
        match (self.code, event.code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => {
                // ctrl-letter arrives lowercase regardless of shift; otherwise
                // the char's case is significant (uppercase = shift).
                if self.ctrl {
                    a.eq_ignore_ascii_case(&b)
                } else {
                    a == b
                }
            }
            (a, b) => a == b && self.shift == event.modifiers.contains(KeyModifiers::SHIFT),
        }
    }

    /// Render the chord back to its canonical string, for help/error text.
    pub fn label(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("ctrl-");
        }
        if self.alt {
            out.push_str("alt-");
        }
        if self.shift {
            out.push_str("shift-");
        }
        out.push_str(&key_label(self.code));
        out
    }
}

/// Parse the key token (already modifier-stripped): a single char, a function
/// key `f1`..`f24`, or a named special key.
fn parse_key(token: &str) -> Option<KeyCode> {
    if token.chars().count() == 1 {
        return Some(KeyCode::Char(token.chars().next().unwrap()));
    }
    let lower = token.to_ascii_lowercase();
    if let Some(n) = lower.strip_prefix('f')
        && let Ok(n) = n.parse::<u8>()
        && (1..=24).contains(&n)
    {
        return Some(KeyCode::F(n));
    }
    Some(match lower.as_str() {
        "enter" | "return" | "ret" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        _ => return None,
    })
}

fn key_label(code: KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("f{n}"),
        KeyCode::Enter => "enter".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn parses_plain_char() {
        let c = KeyChord::parse("g").unwrap();
        assert_eq!(c.code, KeyCode::Char('g'));
        assert!(!c.ctrl && !c.alt && !c.shift);
        assert_eq!(c.label(), "g");
    }

    #[test]
    fn parses_ctrl_and_alt() {
        let c = KeyChord::parse("ctrl-g").unwrap();
        assert_eq!(c.code, KeyCode::Char('g'));
        assert!(c.ctrl && !c.alt);
        assert_eq!(c.label(), "ctrl-g");

        let c = KeyChord::parse("alt-x").unwrap();
        assert!(c.alt && !c.ctrl);
        assert_eq!(c.label(), "alt-x");

        // Aliases.
        assert_eq!(
            KeyChord::parse("control-g").unwrap(),
            KeyChord::parse("ctrl-g").unwrap()
        );
        assert_eq!(
            KeyChord::parse("option-x").unwrap(),
            KeyChord::parse("alt-x").unwrap()
        );
    }

    #[test]
    fn shift_letter_folds_into_uppercase() {
        let c = KeyChord::parse("shift-b").unwrap();
        assert_eq!(c.code, KeyCode::Char('B'));
        assert!(!c.shift, "shift folded into the char");
        // Equivalent to writing the uppercase letter directly.
        assert_eq!(c, KeyChord::parse("B").unwrap());
        assert_eq!(c.label(), "B");
    }

    #[test]
    fn parses_function_and_named_keys() {
        assert_eq!(KeyChord::parse("f5").unwrap().code, KeyCode::F(5));
        let c = KeyChord::parse("shift-f5").unwrap();
        assert_eq!(c.code, KeyCode::F(5));
        assert!(c.shift);
        assert_eq!(c.label(), "shift-f5");
        assert_eq!(KeyChord::parse("enter").unwrap().code, KeyCode::Enter);
        assert_eq!(KeyChord::parse("pageup").unwrap().code, KeyCode::PageUp);
        assert_eq!(KeyChord::parse("space").unwrap().code, KeyCode::Char(' '));
    }

    #[test]
    fn rejects_garbage() {
        assert!(KeyChord::parse("").is_err());
        assert!(
            KeyChord::parse("hyper-g")
                .unwrap_err()
                .contains("unknown modifier")
        );
        assert!(
            KeyChord::parse("ctrl-nope")
                .unwrap_err()
                .contains("unknown key")
        );
        assert!(
            KeyChord::parse("ctrl-")
                .unwrap_err()
                .contains("unknown key")
        );
    }

    #[test]
    fn matches_respects_modifiers() {
        let plain = KeyChord::parse("g").unwrap();
        assert!(plain.matches(&ev(KeyCode::Char('g'), KeyModifiers::NONE)));
        // A plain binding must NOT fire on ctrl-g.
        assert!(!plain.matches(&ev(KeyCode::Char('g'), KeyModifiers::CONTROL)));

        let ctrl_g = KeyChord::parse("ctrl-g").unwrap();
        assert!(ctrl_g.matches(&ev(KeyCode::Char('g'), KeyModifiers::CONTROL)));
        assert!(!ctrl_g.matches(&ev(KeyCode::Char('g'), KeyModifiers::NONE)));
        // ctrl-letter comparison is case-insensitive (terminals send lowercase).
        assert!(
            KeyChord::parse("ctrl-G")
                .unwrap()
                .matches(&ev(KeyCode::Char('g'), KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn matches_uppercase_char_ignoring_shift_modifier() {
        let upper = KeyChord::parse("B").unwrap();
        // Some terminals set SHIFT alongside the uppercase char, some don't.
        assert!(upper.matches(&ev(KeyCode::Char('B'), KeyModifiers::SHIFT)));
        assert!(upper.matches(&ev(KeyCode::Char('B'), KeyModifiers::NONE)));
        // But not the lowercase.
        assert!(!upper.matches(&ev(KeyCode::Char('b'), KeyModifiers::NONE)));
    }

    #[test]
    fn matches_function_key_with_shift() {
        let c = KeyChord::parse("shift-f5").unwrap();
        assert!(c.matches(&ev(KeyCode::F(5), KeyModifiers::SHIFT)));
        assert!(!c.matches(&ev(KeyCode::F(5), KeyModifiers::NONE)));
        assert!(!c.matches(&ev(KeyCode::F(6), KeyModifiers::SHIFT)));
    }
}
