//! `Ctrl-x i` sort imports (M23).
//!
//! Glue between `led_state_syntax::import::sort_imports` and the
//! dispatch surface. Pulls the active buffer's `Tree` from
//! `Sources.syntax`, runs the sort plan, applies the replacement,
//! and surfaces the matching alert ("Imports sorted" /
//! "Imports already sorted").

use std::sync::Arc;

use led_state_alerts::AlertState;
use led_state_buffer_edits::BufferEdits;
use led_state_syntax::SyntaxStates;
use led_state_tabs::Tabs;

use super::shared::{bump, with_active};

const ALERT_TTL_SECS: u64 = 2;

/// Sort the import block in the active buffer. See `MILESTONE-23.md`
/// § "InsertTab dispatch arm" + § D7 for the cursor handling rule.
pub(super) fn sort_imports(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    syntax: &SyntaxStates,
    alerts: &mut AlertState,
) {
    let mut alert: Option<&'static str> = None;
    with_active(tabs, edits, |tab, eb| {
        if tab.preview {
            alert = Some("Imports already sorted");
            return;
        }

        // Grab the language + tree (if available). Missing tree
        // / missing imports.scm / no imports → "already sorted"
        // alert. D3 in the milestone doc covers this.
        let plan = syntax
            .by_path
            .get(&tab.path)
            .and_then(|s| s.tree.as_ref().map(|t| (s.language, t)))
            .and_then(|(lang, tree)| {
                led_state_syntax::import::sort_imports(lang, tree, &eb.rope)
            });

        let Some(plan) = plan else {
            alert = Some("Imports already sorted");
            return;
        };

        let before = tab.cursor;

        // Apply the replacement: remove old slice, insert new.
        let mut rope = (*eb.rope).clone();
        let removed: String = rope.slice(plan.start_char..plan.end_char).chars().collect();
        rope.remove(plan.start_char..plan.end_char);
        rope.insert(plan.start_char, &plan.replacement);
        bump(eb, rope);

        // Cursor handling (D7): if the cursor was inside the
        // import block, snap it to the start of `start_char`'s
        // row. Otherwise leave it alone. `tab.cursor.col` is a
        // grapheme col; convert to a char idx through the line
        // before adding it to the line's char start.
        let cursor_line_slice = if tab.cursor.line < eb.rope.len_lines() {
            eb.rope.line(tab.cursor.line)
        } else {
            eb.rope.slice(eb.rope.len_chars()..eb.rope.len_chars())
        };
        let cursor_char = eb.rope.line_to_char(tab.cursor.line)
            + led_core::grapheme_col_to_char(cursor_line_slice, tab.cursor.col);
        let new_end_char = plan.start_char + plan.replacement.chars().count();
        if cursor_char >= plan.start_char && cursor_char < new_end_char {
            let row = eb.rope.char_to_line(plan.start_char);
            tab.cursor.line = row;
            tab.cursor.col = 0;
            tab.cursor.preferred_col = 0;
        }
        let after = tab.cursor;

        eb.history.finalise();
        eb.history.record_replace(
            plan.start_char,
            Arc::<str>::from(removed.as_str()),
            Arc::<str>::from(plan.replacement.as_str()),
            before,
            after,
            None,
        );

        alert = Some("Imports sorted");
    });

    if let Some(text) = alert {
        alerts.set_info(
            text.to_string(),
            std::time::Instant::now(),
            std::time::Duration::from_secs(ALERT_TTL_SECS),
        );
    }
}

