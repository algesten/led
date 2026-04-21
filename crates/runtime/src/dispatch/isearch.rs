//! In-buffer incremental search dispatch (M13).
//!
//! Activation, query editing, advance-to-next-match, accept/abort
//! semantics per `docs/spec/search.md` § "In-buffer isearch".
//!
//! Current scope (M13 stage 1): `InBufferSearch` toggles the
//! overlay on (or re-triggers when already active but the query is
//! empty — legacy last-query recall). Abort closes it. Typing,
//! match-finding, advance-to-next, accept-on-passthrough, and
//! visual highlighting land in subsequent stages.

use led_core::CanonPath;
use led_state_buffer_edits::BufferEdits;
use led_state_isearch::IsearchState;
use led_state_tabs::{Cursor, Scroll, Tabs};

/// `Ctrl-s` handler. Starts a new search if inactive (seeding from
/// the active buffer's current cursor); advances if already active
/// (future stages); recalls `last_query` if active with an empty
/// query (future stages).
pub(super) fn in_buffer_search(
    isearch: &mut Option<IsearchState>,
    tabs: &Tabs,
    edits: &BufferEdits,
) {
    if isearch.is_some() {
        // Stage 2+: advance / wrap / recall last_query. For now,
        // re-triggering `Ctrl-s` while already open is a no-op.
        return;
    }
    let Some(active_id) = tabs.active else {
        return;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == active_id) else {
        return;
    };
    // Only activate when the active buffer is materialized — no
    // search target otherwise.
    if !edits.buffers.contains_key(&tab.path) {
        return;
    }
    let last_query = prior_last_query(&tab.path, isearch);
    *isearch = Some(IsearchState::start(
        tab.cursor,
        tab.scroll,
        last_query,
    ));
}

/// Placeholder for future cross-session last_query recall. For
/// now there's no persistence layer — `last_query` is always
/// `None` on a fresh activation. Signature-compatible so the
/// persistence hook lands without re-threading.
fn prior_last_query(
    _path: &CanonPath,
    _isearch: &Option<IsearchState>,
) -> Option<String> {
    None
}

/// Abort: close the overlay. Cursor/scroll restoration to
/// `origin_cursor` / `origin_scroll` lands with Stage 4 (needs the
/// `&mut Tabs` borrow to rewrite the active tab's cursor).
pub(super) fn deactivate(
    isearch: &mut Option<IsearchState>,
    _tabs: &mut Tabs,
) {
    *isearch = None;
}

// Prevent `Cursor` / `Scroll` unused-import warnings until
// subsequent stages wire them into the full dispatcher.
#[allow(dead_code)]
const _USES: fn() = || {
    let _ = std::mem::size_of::<Cursor>();
    let _ = std::mem::size_of::<Scroll>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use led_state_buffer_edits::EditedBuffer;
    use led_state_tabs::{Tab, TabId};
    use ropey::Rope;
    use std::sync::Arc;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn tabs_with_active(path: &str) -> Tabs {
        let mut t = Tabs::default();
        t.open.push_back(Tab {
            id: TabId(1),
            path: canon(path),
            cursor: Cursor { line: 2, col: 4, preferred_col: 4 },
            ..Default::default()
        });
        t.active = Some(TabId(1));
        t
    }

    fn edits_with_buffer(path: &str, body: &str) -> BufferEdits {
        let mut e = BufferEdits::default();
        e.buffers.insert(
            canon(path),
            EditedBuffer::fresh(Arc::new(Rope::from_str(body))),
        );
        e
    }

    #[test]
    fn in_buffer_search_activates_and_captures_origin() {
        let tabs = tabs_with_active("/tmp/buf.txt");
        let edits = edits_with_buffer("/tmp/buf.txt", "hello\nworld\n");
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        let s = isearch.expect("activated");
        assert_eq!(s.origin_cursor.line, 2);
        assert_eq!(s.origin_cursor.col, 4);
        assert_eq!(s.query.text, "");
    }

    #[test]
    fn in_buffer_search_is_noop_without_active_tab() {
        let tabs = Tabs::default();
        let edits = BufferEdits::default();
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        assert!(isearch.is_none());
    }

    #[test]
    fn in_buffer_search_is_noop_when_buffer_not_materialized() {
        let tabs = tabs_with_active("/tmp/pending.txt");
        let edits = BufferEdits::default();
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        assert!(isearch.is_none());
    }

    #[test]
    fn deactivate_clears_state() {
        let tabs = tabs_with_active("/tmp/x.txt");
        let edits = edits_with_buffer("/tmp/x.txt", "x");
        let mut isearch = None;
        in_buffer_search(&mut isearch, &tabs, &edits);
        assert!(isearch.is_some());
        let mut tabs = Tabs::default();
        deactivate(&mut isearch, &mut tabs);
        assert!(isearch.is_none());
    }
}
