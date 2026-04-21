//! `TextInput` — a single-line editable string with cursor + transient
//! prompt hint.
//!
//! Shared by every overlay / minibuffer the app exposes: find-file,
//! save-as, in-buffer isearch (M13), file-search (M14), LSP
//! completion prefix (M17), any future command-palette or
//! replace-prompt. The primitives here are the UTF-8-safe editing
//! operations; *what* the surrounding state does with the input
//! (filter completions, drive a regex, ...) stays in its own
//! overlay-specific struct.
//!
//! The `hint` slot carries Emacs-style inline feedback —
//! `[No match]`, `[Sole completion]`, `[Complete, but not unique]`,
//! or validation messages like `[Invalid]` — with a TTL. The
//! runtime's ingest phase calls [`TextInput::expire_hint`] on every
//! tick (same pattern as `AlertState::expire_info`), and folds
//! `hint_expires_at` into `nearest_deadline` so the loop wakes when
//! the hint should clear.

use std::time::{Duration, Instant};

/// Editable single-line text with cursor + optional transient hint.
///
/// `cursor` is a **byte offset** into `text`, always on a UTF-8 char
/// boundary. All mutating methods maintain that invariant.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextInput {
    pub text: String,
    pub cursor: usize,
    /// Transient feedback rendered next to the cursor (`[No match]`,
    /// `[Complete]`, …). Cleared when `hint_expires_at` is reached
    /// via [`expire_hint`].
    pub hint: Option<String>,
    pub hint_expires_at: Option<Instant>,
}

impl TextInput {
    /// Build an input seeded with `initial`, cursor parked at the
    /// end — the common "start editing from the end of a
    /// pre-filled value" case.
    pub fn new(initial: impl Into<String>) -> Self {
        let text = initial.into();
        let cursor = text.len();
        Self {
            text,
            cursor,
            hint: None,
            hint_expires_at: None,
        }
    }

    // ── Bulk replacement ────────────────────────────────────────

    /// Replace the whole `text`, park the cursor at the end, and
    /// clear any active hint. Used by Tab-completion, arrow-nav
    /// input rewrites, and programmatic "set this path" flows.
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.hint = None;
        self.hint_expires_at = None;
    }

    /// Empty the input; cursor → 0; hint cleared.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.hint = None;
        self.hint_expires_at = None;
    }

    // ── Cursor-walking edits ────────────────────────────────────

    /// Insert `c` at the cursor, advance past it. Returns `true` if
    /// the text changed (always, given the contract that `c` is a
    /// valid char).
    pub fn insert_char(&mut self, c: char) -> bool {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        true
    }

    /// Delete the char immediately before the cursor. Returns
    /// `true` when something was deleted; `false` at the start of
    /// the input.
    pub fn delete_back(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        true
    }

    /// Delete the char at the cursor (doesn't move the cursor).
    /// Returns `true` when something was deleted; `false` at end
    /// of input.
    pub fn delete_forward(&mut self) -> bool {
        if self.cursor >= self.text.len() {
            return false;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
        true
    }

    /// Ctrl-k: drop everything from the cursor to end of line.
    /// Returns `true` if anything was truncated.
    pub fn kill_to_end(&mut self) -> bool {
        if self.cursor >= self.text.len() {
            return false;
        }
        self.text.truncate(self.cursor);
        true
    }

    /// Step cursor one char to the left. Returns `true` if moved.
    pub fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor = prev_char_boundary(&self.text, self.cursor);
        true
    }

    /// Step cursor one char to the right. Returns `true` if moved.
    pub fn move_right(&mut self) -> bool {
        if self.cursor >= self.text.len() {
            return false;
        }
        self.cursor = next_char_boundary(&self.text, self.cursor);
        true
    }

    pub fn to_line_start(&mut self) {
        self.cursor = 0;
    }
    pub fn to_line_end(&mut self) {
        self.cursor = self.text.len();
    }

    // ── Hint (transient inline feedback) ────────────────────────

    /// Arm a transient hint that auto-clears at `now + ttl`.
    /// Replaces any existing hint.
    pub fn set_hint(&mut self, text: impl Into<String>, now: Instant, ttl: Duration) {
        self.hint = Some(text.into());
        self.hint_expires_at = Some(now + ttl);
    }

    /// Drop the hint once `now` passes the stored expiry. Runtime
    /// calls this every tick.
    pub fn expire_hint(&mut self, now: Instant) {
        if let Some(deadline) = self.hint_expires_at
            && now >= deadline
        {
            self.hint = None;
            self.hint_expires_at = None;
        }
    }

    /// Dismiss any live hint immediately — for "user interacted,
    /// so the flashed feedback has served its purpose" semantics.
    /// Used by the overlay dispatch layer at the top of its
    /// command handler so the next keystroke after a "[No match]"
    /// flash clears the hint without waiting out the TTL.
    pub fn dismiss_hint(&mut self) {
        self.hint = None;
        self.hint_expires_at = None;
    }
}

fn prev_char_boundary(s: &str, byte_pos: usize) -> usize {
    s[..byte_pos]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_char_boundary(s: &str, byte_pos: usize) -> usize {
    match s[byte_pos..].chars().next() {
        Some(c) => byte_pos + c.len_utf8(),
        None => byte_pos,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_parks_cursor_at_end() {
        let i = TextInput::new("hello");
        assert_eq!(i.text, "hello");
        assert_eq!(i.cursor, 5);
    }

    #[test]
    fn insert_and_delete_respect_utf8() {
        let mut i = TextInput::new("ä"); // 2 bytes
        assert_eq!(i.cursor, 2);
        // Insert at end.
        i.insert_char('ö'); // 2 more bytes
        assert_eq!(i.text, "äö");
        assert_eq!(i.cursor, 4);
        // Delete back.
        i.delete_back();
        assert_eq!(i.text, "ä");
        assert_eq!(i.cursor, 2);
    }

    #[test]
    fn move_left_and_right_walk_char_boundaries() {
        let mut i = TextInput::new("/ä/"); // bytes: / (0), ä (1..3), / (3..4)
        assert_eq!(i.cursor, 4);
        i.move_left();
        assert_eq!(i.cursor, 3);
        i.move_left();
        assert_eq!(i.cursor, 1); // past 'ä' (2 bytes)
        i.move_right();
        assert_eq!(i.cursor, 3);
    }

    #[test]
    fn delete_forward_at_eol_is_noop() {
        let mut i = TextInput::new("x");
        assert!(!i.delete_forward());
        assert_eq!(i.text, "x");
    }

    #[test]
    fn delete_back_at_bol_is_noop() {
        let mut i = TextInput::new("x");
        i.to_line_start();
        assert!(!i.delete_back());
        assert_eq!(i.text, "x");
    }

    #[test]
    fn kill_to_end_truncates_at_cursor() {
        let mut i = TextInput::new("abcdef");
        i.cursor = 2;
        assert!(i.kill_to_end());
        assert_eq!(i.text, "ab");
    }

    #[test]
    fn set_replaces_text_and_parks_cursor() {
        let mut i = TextInput::new("old");
        i.cursor = 1;
        i.set("newer");
        assert_eq!(i.text, "newer");
        assert_eq!(i.cursor, 5);
    }

    #[test]
    fn set_hint_and_expire_hint_cycle() {
        let mut i = TextInput::new("");
        let t0 = Instant::now();
        i.set_hint("[No match]", t0, Duration::from_millis(500));
        assert_eq!(i.hint.as_deref(), Some("[No match]"));

        i.expire_hint(t0 + Duration::from_millis(100));
        assert!(i.hint.is_some());

        i.expire_hint(t0 + Duration::from_millis(600));
        assert!(i.hint.is_none());
        assert!(i.hint_expires_at.is_none());
    }
}
