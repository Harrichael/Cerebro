//! Crossterm key presses in the notation nvim reads them in.
//!
//! Nvim takes typing as text: an ordinary character is itself, and everything
//! else is a name in angle brackets -- `<CR>`, `<Esc>`, `<C-w>`, `<S-Tab>`.
//! Which means the one character that cannot be sent as itself is `<`, and a
//! translator that forgets it turns a keystroke into the start of a key name.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What to send to nvim for this press, or `None` for a press that is not
/// typing at all -- a bare modifier, a key nvim has no name for.
pub fn notation(key: KeyEvent) -> Option<String> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let named = |name: &str| Some(wrap(name, ctrl, alt, shift));
    match key.code {
        KeyCode::Char(c) => {
            // Shift is already in the character crossterm decoded, so naming
            // it again would ask nvim for `<S-A>`, which is not a key.
            if !ctrl && !alt {
                return Some(if c == '<' { "<lt>".into() } else { c.to_string() });
            }
            Some(wrap(&c.to_string(), ctrl, alt, false))
        }
        KeyCode::Enter => named("CR"),
        KeyCode::Esc => named("Esc"),
        KeyCode::Backspace => named("BS"),
        KeyCode::Tab => named("Tab"),
        // Crossterm has already resolved shift-tab into its own key, so
        // saying shift again would double it.
        KeyCode::BackTab => Some(wrap("Tab", ctrl, alt, true)),
        KeyCode::Delete => named("Del"),
        KeyCode::Insert => named("Insert"),
        KeyCode::Left => named("Left"),
        KeyCode::Right => named("Right"),
        KeyCode::Up => named("Up"),
        KeyCode::Down => named("Down"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PageUp"),
        KeyCode::PageDown => named("PageDown"),
        KeyCode::F(n) => named(&format!("F{n}")),
        _ => None,
    }
}

fn wrap(name: &str, ctrl: bool, alt: bool, shift: bool) -> String {
    let mut out = String::from("<");
    for (on, prefix) in [(ctrl, "C-"), (alt, "M-"), (shift, "S-")] {
        if on {
            out.push_str(prefix);
        }
    }
    out.push_str(name);
    out.push('>');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
        notation(KeyEvent::new(code, modifiers))
    }

    /// Plain typing goes through as itself, with the one exception that would
    /// otherwise be read as the start of a key name.
    #[test]
    fn ordinary_characters_are_themselves_and_a_less_than_is_not() {
        assert_eq!(key(KeyCode::Char('a'), KeyModifiers::NONE).as_deref(), Some("a"));
        assert_eq!(key(KeyCode::Char('A'), KeyModifiers::SHIFT).as_deref(), Some("A"));
        assert_eq!(key(KeyCode::Char('<'), KeyModifiers::NONE).as_deref(), Some("<lt>"));
    }

    #[test]
    fn the_keys_with_names_get_them() {
        assert_eq!(key(KeyCode::Enter, KeyModifiers::NONE).as_deref(), Some("<CR>"));
        assert_eq!(key(KeyCode::Esc, KeyModifiers::NONE).as_deref(), Some("<Esc>"));
        assert_eq!(key(KeyCode::Char('w'), KeyModifiers::CONTROL).as_deref(), Some("<C-w>"));
        assert_eq!(key(KeyCode::BackTab, KeyModifiers::SHIFT).as_deref(), Some("<S-Tab>"));
        assert_eq!(key(KeyCode::Left, KeyModifiers::CONTROL).as_deref(), Some("<C-Left>"));
        assert_eq!(key(KeyCode::F(5), KeyModifiers::NONE).as_deref(), Some("<F5>"));
    }
}
