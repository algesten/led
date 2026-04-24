//! Diagnostic-offer helpers: decide whether an inbound diagnostic
//! delivery is acceptable against the current buffer state, and
//! (in the replay case) transform its positions forward through
//! any edits that have landed since the stamped hash was current.
//!
//! Port of legacy led's `BufferState::offer_diagnostics` +
//! `replay_diagnostics` (`crates/state/src/lib.rs:1203-1316`).
//! Kept in the runtime because it stitches together the buffer
//! rope (`EditedBuffer.rope`), the edit history
//! (`EditedBuffer.history`), the inbound LSP payload, and the
//! diagnostic atom — three crates at once, which is the shape
//! of every runtime-side query memo.

use led_core::{EphemeralContentHash, PersistedContentHash};
use led_state_buffer_edits::{EditGroup, EditOp, EditedBuffer};
use led_state_diagnostics::Diagnostic;
use ropey::Rope;

/// Outcome of offering a diagnostic delivery against a buffer.
///
/// `Accept(diags)` — the caller should write a `BufferDiagnostics
/// { hash, diagnostics }` record into the atom, stamped with the
/// buffer's CURRENT ephemeral hash (promoted to persisted). When
/// the path is fast (stamp matches the rope directly) `diags` is
/// the input unchanged; when the path is replay (stamp matches a
/// save-point marker and there's unsaved work on top), `diags` is
/// the input with same-row diagnostics dropped and structural
/// diagnostics shifted. Both flavours reflect the current rope.
///
/// `Reject` — the stamp is neither the current content nor any
/// reachable save-point marker. The caller drops the delivery
/// silently; a later pull will re-fetch against a hash that
/// matches again.
pub enum OfferOutcome {
    Accept(Vec<Diagnostic>),
    Reject,
}

/// Try to accept `diags` against `eb`, given that they were
/// computed for content with hash `stamped`. Returns `Accept` with
/// the diagnostics to store (possibly transformed) or `Reject`.
///
/// The transformation path is pure: it doesn't mutate history or
/// the rope; it just projects diagnostic positions through the
/// edits that have landed since the matching save-point marker.
pub fn offer_diagnostics(
    eb: &EditedBuffer,
    stamped: PersistedContentHash,
    diags: Vec<Diagnostic>,
) -> OfferOutcome {
    let current = EphemeralContentHash::of_rope(&eb.rope);
    // Fast path: the rope still holds exactly the bytes the
    // server analysed. Common after save, before any further
    // typing, and also the case where the user undid every edit
    // since the save-point.
    if current.matches(stamped) {
        return OfferOutcome::Accept(diags);
    }

    // Replay path: the rope has moved, but the buffer holds a
    // save-point marker for this hash in its undo history. Walk
    // forward through every edit after the marker, transforming
    // diagnostic positions as we go. Same-row edits clear
    // diagnostics on that row (the content under them has
    // changed — showing them on the new characters would be
    // misleading); structural edits shift row numbers.
    let Some(save_idx) = eb.history.find_save_point(stamped) else {
        return OfferOutcome::Reject;
    };

    // Reconstruct save-time rope by inverting every op from
    // `save_idx + 1` forward, in reverse. The current rope IS
    // the post-application rope; walk backward applying each
    // op's inverse to get the pre-save-point state.
    let mut doc: Rope = (*eb.rope).clone();
    let groups_forward: Vec<&EditGroup> = eb
        .history
        .groups_from(save_idx + 1)
        .filter(|g| !g.ops.is_empty())
        .collect();
    for group in groups_forward.iter().rev() {
        for op in group.ops.iter().rev() {
            invert_op(&mut doc, op);
        }
    }

    // Walk forward, applying each op and transforming the
    // current working diagnostic list.
    let mut working: Vec<Diagnostic> = diags;
    for group in groups_forward {
        for op in &group.ops {
            let (edit_row, delta) = describe_op(&doc, op);
            if delta == 0 {
                // Same-row content edit — drop any diag whose
                // range touches this row.
                working.retain(|d| !(d.start_line <= edit_row && d.end_line >= edit_row));
            } else {
                // Structural edit — shift rows past the edit
                // row by `delta`, drop diags that fell inside a
                // deletion.
                working.retain_mut(|d| {
                    if delta < 0 {
                        let deleted_end = edit_row + (-delta) as usize;
                        if d.start_line >= edit_row && d.end_line <= deleted_end {
                            return false;
                        }
                    }
                    if d.start_line > edit_row {
                        d.start_line = (d.start_line as isize + delta).max(0) as usize;
                    }
                    if d.end_line > edit_row {
                        d.end_line = (d.end_line as isize + delta).max(0) as usize;
                    }
                    true
                });
            }
            apply_op(&mut doc, op);
        }
    }

    OfferOutcome::Accept(working)
}

/// Apply `op` to `doc` in place. Mirrors dispatch/edit.rs's
/// rope edits so the replay's forward walk tracks what actually
/// happened in the buffer.
fn apply_op(doc: &mut Rope, op: &EditOp) {
    match op {
        EditOp::Insert { at, text } => {
            doc.insert(*at, text);
        }
        EditOp::Delete { at, text } => {
            let len = text.chars().count();
            doc.remove(*at..*at + len);
        }
    }
}

/// Inverse of `apply_op` — used to walk backward from the
/// current rope to the save-time rope.
fn invert_op(doc: &mut Rope, op: &EditOp) {
    match op {
        EditOp::Insert { at, text } => {
            let len = text.chars().count();
            doc.remove(*at..*at + len);
        }
        EditOp::Delete { at, text } => {
            doc.insert(*at, text);
        }
    }
}

/// Return `(edit_row, row_delta)` for the op about to be applied
/// to `doc`. `row_delta = new_newlines - old_newlines`, where the
/// counts are the newline counts of the affected text (inserted
/// or deleted). Matches legacy's same-row vs structural branch.
fn describe_op(doc: &Rope, op: &EditOp) -> (usize, isize) {
    let (at, old_newlines, new_newlines) = match op {
        EditOp::Insert { at, text } => (
            *at,
            0usize,
            text.chars().filter(|c| *c == '\n').count(),
        ),
        EditOp::Delete { at, text } => (
            *at,
            text.chars().filter(|c| *c == '\n').count(),
            0usize,
        ),
    };
    let edit_row = doc.char_to_line(at);
    let delta = new_newlines as isize - old_newlines as isize;
    (edit_row, delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_state_buffer_edits::EditedBuffer;
    use led_state_diagnostics::DiagnosticSeverity;
    use led_state_tabs::Cursor;
    use std::sync::Arc;

    fn diag(line: usize, message: &str) -> Diagnostic {
        Diagnostic {
            start_line: line,
            start_col: 0,
            end_line: line,
            end_col: 5,
            severity: DiagnosticSeverity::Error,
            message: message.to_string(),
            source: None,
            code: None,
        }
    }

    fn cur(line: usize, col: usize) -> Cursor {
        Cursor {
            line,
            col,
            preferred_col: col,
        }
    }

    #[test]
    fn fast_path_accepts_when_hash_matches() {
        let rope = Arc::new(Rope::from_str("one\ntwo\nthree\n"));
        let eb = EditedBuffer::fresh(rope.clone());
        let h = EphemeralContentHash::of_rope(&rope).persist();
        let out = offer_diagnostics(&eb, h, vec![diag(1, "err")]);
        match out {
            OfferOutcome::Accept(diags) => {
                assert_eq!(diags.len(), 1);
                assert_eq!(diags[0].start_line, 1);
            }
            OfferOutcome::Reject => panic!("expected accept"),
        }
    }

    #[test]
    fn reject_when_no_matching_save_point() {
        let rope = Arc::new(Rope::from_str("one\ntwo\n"));
        let eb = EditedBuffer::fresh(rope);
        let bogus = led_core::PersistedContentHash(0xDEADBEEF);
        let out = offer_diagnostics(&eb, bogus, vec![diag(0, "err")]);
        assert!(matches!(out, OfferOutcome::Reject));
    }

    #[test]
    fn replay_clears_diagnostic_on_same_row_edit() {
        // Scenario: save at state A, then same-row edit moves
        // buffer to state B. A diagnostic stamped with A's hash
        // arrives — replay must drop it because the edited row
        // no longer reflects the content the server analysed.
        let at_save = Arc::new(Rope::from_str("fn main() {}\n"));
        let save_hash = EphemeralContentHash::of_rope(&at_save).persist();
        let mut eb = EditedBuffer::fresh(at_save);
        eb.history.insert_save_point(save_hash);

        // Apply a same-row edit: insert 'x' at char 2 ("fxn main()...").
        let mut rope2: Rope = (*eb.rope).clone();
        rope2.insert_char(2, 'x');
        eb.rope = Arc::new(rope2);
        eb.version = 1;
        eb.history.record_insert_char(2, 'x', cur(0, 2), cur(0, 3));
        eb.history.finalise();

        let diags = vec![diag(0, "expected semicolon")];
        let out = offer_diagnostics(&eb, save_hash, diags);
        match out {
            OfferOutcome::Accept(d) => assert!(
                d.is_empty(),
                "same-row edit must drop diagnostics on that row; got {d:?}"
            ),
            OfferOutcome::Reject => panic!("expected accept+drop"),
        }
    }

    #[test]
    fn replay_shifts_diagnostic_past_structural_insert() {
        // Save at state A: diag on row 2. Then insert a newline
        // on row 0 — row 2 becomes row 3 in the new rope.
        let at_save = Arc::new(Rope::from_str("a\nb\nc\n"));
        let save_hash = EphemeralContentHash::of_rope(&at_save).persist();
        let mut eb = EditedBuffer::fresh(at_save);
        eb.history.insert_save_point(save_hash);

        // Insert a newline at char 0 ("\na\nb\nc\n").
        let mut rope2: Rope = (*eb.rope).clone();
        rope2.insert_char(0, '\n');
        eb.rope = Arc::new(rope2);
        eb.version = 1;
        eb.history
            .record_insert(0, Arc::<str>::from("\n"), cur(0, 0), cur(1, 0));
        eb.history.finalise();

        let diags = vec![diag(2, "err on c")];
        let out = offer_diagnostics(&eb, save_hash, diags);
        match out {
            OfferOutcome::Accept(d) => {
                assert_eq!(d.len(), 1);
                assert_eq!(d[0].start_line, 3, "diag shifted down one row");
                assert_eq!(d[0].end_line, 3);
            }
            OfferOutcome::Reject => panic!("expected accept"),
        }
    }

    #[test]
    fn replay_accepts_verbatim_when_edits_reverse_to_save_content() {
        // Type-then-delete: buffer content ends up identical to
        // the save hash. Fast path matches; replay never runs.
        let rope = Arc::new(Rope::from_str("hello\n"));
        let save_hash = EphemeralContentHash::of_rope(&rope).persist();
        let mut eb = EditedBuffer::fresh(rope);
        eb.history.insert_save_point(save_hash);

        // Type 'x' then delete it, ending back at hello.
        let mut r: Rope = (*eb.rope).clone();
        r.insert_char(5, 'x');
        eb.rope = Arc::new(r);
        eb.version = 1;
        eb.history.record_insert_char(5, 'x', cur(0, 5), cur(0, 6));
        eb.history.finalise();

        let mut r: Rope = (*eb.rope).clone();
        r.remove(5..6);
        eb.rope = Arc::new(r);
        eb.version = 2;
        eb.history
            .record_delete(5, Arc::<str>::from("x"), cur(0, 6), cur(0, 5));
        eb.history.finalise();

        // Current hash should equal save_hash.
        let current = EphemeralContentHash::of_rope(&eb.rope).persist();
        assert_eq!(current, save_hash);

        let out = offer_diagnostics(&eb, save_hash, vec![diag(0, "err")]);
        match out {
            OfferOutcome::Accept(d) => assert_eq!(d.len(), 1),
            OfferOutcome::Reject => panic!("expected accept"),
        }
    }
}
