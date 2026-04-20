//! Cross-source query layer.
//!
//! Every memo that combines two or more drivers' sources lives here.
//! The drivers themselves are strictly isolated — they know only their
//! own data. The runtime owns the glue:
//!
//! - **`#[drv::input]` projections** for every subset of a driver
//!   source the runtime needs. Each carries a `new(&source)`
//!   constructor the call site uses to project.
//! - **Memos** that combine those inputs into actionable results:
//!   `LoadAction`s for `FileReadDriver::execute` to consume, and
//!   `Frame`s for `paint` to render.

#[allow(unused_imports)]
use led_core::CanonPath;
use led_driver_buffers_core::{BufferStore, LoadAction, LoadState};
use led_driver_terminal_core::{BodyModel, Dims, Frame, TabBarModel, Terminal};
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_tabs::{Cursor, Scroll, Tab, TabId, Tabs};
use ropey::Rope;
use std::sync::Arc;

// ── Inputs on Tabs ─────────────────────────────────────────────────────

/// Open-tabs slice (for "what files do we need loaded?" queries).
#[drv::input]
#[derive(Copy, Clone)]
pub struct TabsOpenInput<'a> {
    pub open: &'a imbl::Vector<Tab>,
}

impl<'a> TabsOpenInput<'a> {
    pub fn new(tabs: &'a Tabs) -> Self {
        Self { open: &tabs.open }
    }
}

/// Active-tab slice (for rendering: both `active` id and the `open`
/// list so the memo can resolve the id to a Tab).
#[drv::input]
#[derive(Copy, Clone)]
pub struct TabsActiveInput<'a> {
    pub open: &'a imbl::Vector<Tab>,
    pub active: &'a Option<TabId>,
}

impl<'a> TabsActiveInput<'a> {
    pub fn new(tabs: &'a Tabs) -> Self {
        Self {
            open: &tabs.open,
            active: &tabs.active,
        }
    }
}

// ── Inputs on BufferEdits ──────────────────────────────────────────────

/// Full `buffers` map for memos that read rope contents (body_model).
/// On cache hit the projection is a pointer copy; when an edit lands,
/// the `HashMap` pointer changes and the cache invalidates.
#[drv::input]
#[derive(Copy, Clone)]
pub struct EditedBuffersInput<'a> {
    pub buffers: &'a imbl::HashMap<CanonPath, EditedBuffer>,
}

impl<'a> EditedBuffersInput<'a> {
    pub fn new(edits: &'a BufferEdits) -> Self {
        Self {
            buffers: &edits.buffers,
        }
    }
}

// ── Inputs on BufferStore ──────────────────────────────────────────────

/// Whole-map projection over `BufferStore`'s single field.
#[drv::input]
#[derive(Copy, Clone)]
pub struct StoreLoadedInput<'a> {
    pub loaded: &'a imbl::HashMap<CanonPath, LoadState>,
}

impl<'a> StoreLoadedInput<'a> {
    pub fn new(store: &'a BufferStore) -> Self {
        Self {
            loaded: &store.loaded,
        }
    }
}

// ── Input on Terminal ──────────────────────────────────────────────────

/// Viewport dims only. A push to `Terminal.pending` is deliberately
/// outside this input so incoming events don't invalidate `render_frame`.
#[drv::input]
#[derive(Copy, Clone)]
pub struct TerminalDimsInput<'a> {
    pub dims: &'a Option<Dims>,
}

impl<'a> TerminalDimsInput<'a> {
    pub fn new(term: &'a Terminal) -> Self {
        Self { dims: &term.dims }
    }
}

// ── Memos ──────────────────────────────────────────────────────────────

/// "What files need a load started?"
///
/// Diff between the paths open in tabs and the `BufferStore` map.
/// Absent → `Load`; `Pending | Ready | Error` → skip. Once a load is
/// in flight, the `Pending` entry prevents re-triggering.
///
/// Filters before cloning so we only allocate `CanonPath`s for paths
/// that actually need loading.
#[drv::memo(single)]
pub fn file_load_action<'a, 'b>(
    store: StoreLoadedInput<'a>,
    tabs: TabsOpenInput<'b>,
) -> imbl::Vector<LoadAction> {
    tabs.open
        .iter()
        .filter(|t| !store.loaded.contains_key(&t.path))
        .map(|t| LoadAction::Load(t.path.clone()))
        .collect()
}

/// Tab-bar slice of the render frame.
///
/// Labels are wrapped in `Arc` so cache-hit clones of [`TabBarModel`]
/// (inside `Frame`, deep inside `render_frame`'s cache slot) are a
/// pointer copy. A `*` prefix marks tabs whose buffer has been
/// modified since load.
#[drv::memo(single)]
pub fn tab_bar_model<'a, 'b>(
    tabs: TabsActiveInput<'a>,
    edits: EditedBuffersInput<'b>,
) -> TabBarModel {
    let labels: Vec<String> = tabs
        .open
        .iter()
        .map(|t| {
            let base = t
                .path
                .file_name()
                .map(|os| os.to_string_lossy().into_owned())
                .unwrap_or_else(|| t.path.display().to_string());
            let dirty = edits.buffers.get(&t.path).map(|b| b.dirty).unwrap_or(false);
            if dirty {
                let mut s = String::with_capacity(base.len() + 1);
                s.push('*');
                s.push_str(&base);
                s
            } else {
                base
            }
        })
        .collect();
    let active = tabs
        .active
        .and_then(|id| tabs.open.iter().position(|t| t.id == id));
    TabBarModel {
        labels: Arc::new(labels),
        active,
    }
}

/// Body slice of the render frame.
///
/// Reads the active tab's cursor + scroll to produce the visible line
/// slice and a body-relative cursor position. Scroll is source state
/// on [`Tab`]; dispatch maintains the "keep cursor visible" invariant
/// so the cursor is normally inside the returned window.
///
/// Prefers [`BufferEdits`] (the user-edited view) over [`BufferStore`]
/// (the disk snapshot). In steady state — loaded + seeded — the
/// edits branch always wins; the store fallback covers the brief
/// window between a load completion and the runtime's next
/// BufferEdits seed, plus Pending / Error paths that never made it
/// to `Ready`.
#[drv::memo(single)]
pub fn body_model<'e, 'a, 'b>(
    edits: EditedBuffersInput<'e>,
    store: StoreLoadedInput<'a>,
    tabs: TabsActiveInput<'b>,
    dims: Dims,
) -> BodyModel {
    let Some(id) = *tabs.active else {
        return BodyModel::Empty;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
        return BodyModel::Empty;
    };
    if let Some(eb) = edits.buffers.get(&tab.path) {
        return render_content(&eb.rope, tab.cursor, tab.scroll, dims);
    }
    // Only the Pending / Error paths need a rendered path string, and
    // even then we allocate only once per recompute; `Arc<str>` keeps
    // cache-hit clones O(1).
    match store.loaded.get(&tab.path) {
        None | Some(LoadState::Pending) => BodyModel::Pending {
            path_display: path_display(tab),
        },
        Some(LoadState::Error(msg)) => BodyModel::Error {
            path_display: path_display(tab),
            message: Arc::<str>::from(msg.as_str()),
        },
        Some(LoadState::Ready(rope)) => render_content(rope, tab.cursor, tab.scroll, dims),
    }
}

fn path_display(tab: &Tab) -> Arc<str> {
    Arc::<str>::from(tab.path.display().to_string())
}

fn render_content(rope: &Rope, cursor: Cursor, scroll: Scroll, dims: Dims) -> BodyModel {
    let body_rows = dims.rows.saturating_sub(1) as usize;
    let line_count = rope.len_lines();
    let cols = dims.cols as usize;

    let mut lines: Vec<String> = Vec::with_capacity(body_rows);
    for ln in scroll.top..scroll.top.saturating_add(body_rows) {
        if ln >= line_count {
            break;
        }
        let mut s = rope.line(ln).to_string();
        strip_trailing_newline(&mut s);
        truncate_to_cols_in_place(&mut s, cols);
        lines.push(s);
    }

    BodyModel::Content {
        lines: Arc::new(lines),
        cursor: visible_cursor(cursor, scroll, dims),
    }
}

fn visible_cursor(c: Cursor, s: Scroll, dims: Dims) -> Option<(u16, u16)> {
    let body_rows = dims.rows.saturating_sub(1) as usize;
    if body_rows == 0 {
        return None;
    }
    if c.line < s.top || c.line >= s.top.saturating_add(body_rows) {
        return None;
    }
    let row = (c.line - s.top) as u16;
    let col = c.col.min(dims.cols.saturating_sub(1) as usize) as u16;
    Some((row, col))
}

fn strip_trailing_newline(s: &mut String) {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
}

/// Truncate `s` to at most `cols` Unicode characters, in place.
/// No allocation when the string already fits.
fn truncate_to_cols_in_place(s: &mut String, cols: usize) {
    if let Some((byte_idx, _)) = s.char_indices().nth(cols) {
        s.truncate(byte_idx);
    }
}

/// Top-level render model. Composes `tab_bar_model` + `body_model` —
/// each independently cached in its own per-memo thread-local cache.
///
/// The call site pattern for a memo composing sibling memos: pass the
/// same input values through — drv 0.3's input types are `Copy` over
/// references, so forwarding is free.
#[drv::memo(single)]
pub fn render_frame<'t, 'e, 'b, 'a>(
    term: TerminalDimsInput<'t>,
    edits: EditedBuffersInput<'e>,
    store: StoreLoadedInput<'b>,
    tabs: TabsActiveInput<'a>,
) -> Option<Frame> {
    let dims = (*term.dims)?;
    let tab_bar = tab_bar_model(tabs, edits);
    let body = body_model(edits, store, tabs, dims);
    // Translate the body-relative cursor (row, col) into absolute
    // screen coords (col, row). +1 on the row accounts for the tab
    // bar; the painter wants column-major order (crossterm).
    let cursor = match &body {
        BodyModel::Content {
            cursor: Some((row, col)),
            ..
        } => Some((*col, row.saturating_add(1))),
        _ => None,
    };
    Some(Frame {
        tab_bar,
        body,
        cursor,
        dims,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use ropey::Rope;
    use std::sync::Arc;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn fixture(
        paths: &[(&str, u64)],
        active: Option<u64>,
        loaded: &[(&str, LoadState)],
        dims: Option<Dims>,
    ) -> (Tabs, BufferEdits, BufferStore, Terminal) {
        let mut t = Tabs::default();
        for (p, id) in paths {
            t.open.push_back(Tab {
                id: TabId(*id),
                path: canon(p),
                ..Default::default()
            });
        }
        t.active = active.map(TabId);

        let mut s = BufferStore::default();
        for (p, st) in loaded {
            s.loaded.insert(canon(p), st.clone());
        }

        // M3 default: tests exercise the fallback (no edits seeded).
        // Individual cases that want to exercise the edits path seed
        // entries directly before rendering.
        let e = BufferEdits::default();

        let term = Terminal {
            dims,
            ..Default::default()
        };

        (t, e, s, term)
    }

    #[test]
    fn load_action_emits_load_for_absent_paths() {
        let store = BufferStore::default();
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            ..Default::default()
        });
        tabs.open.push_back(Tab {
            id: TabId(2),
            path: canon("b.rs"),
            ..Default::default()
        });

        let acts = file_load_action(
            StoreLoadedInput::new(&store),
            TabsOpenInput::new(&tabs),
        );
        assert_eq!(acts.len(), 2);
    }

    #[test]
    fn load_action_skips_already_tracked() {
        let mut store = BufferStore::default();
        store.loaded.insert(canon("pending.rs"), LoadState::Pending);
        store.loaded.insert(
            canon("ready.rs"),
            LoadState::Ready(Arc::new(Rope::from_str("x"))),
        );

        let mut tabs = Tabs::default();
        for (i, p) in ["pending.rs", "ready.rs", "new.rs"].iter().enumerate() {
            tabs.open.push_back(Tab {
                id: TabId(i as u64 + 1),
                path: canon(p),
                ..Default::default()
            });
        }

        let acts = file_load_action(
            StoreLoadedInput::new(&store),
            TabsOpenInput::new(&tabs),
        );
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0], LoadAction::Load(canon("new.rs")));
    }

    fn render(t: &Tabs, e: &BufferEdits, s: &BufferStore, term: &Terminal) -> Option<Frame> {
        render_frame(
            TerminalDimsInput::new(term),
            EditedBuffersInput::new(e),
            StoreLoadedInput::new(s),
            TabsActiveInput::new(t),
        )
    }

    #[test]
    fn render_frame_none_until_dims_known() {
        let (t, e, s, term) = fixture(&[("a.rs", 1)], Some(1), &[], None);
        assert!(render(&t, &e, &s, &term).is_none());
    }

    #[test]
    fn render_frame_shows_pending_before_content_arrives() {
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[],
            Some(Dims { cols: 80, rows: 24 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        assert_eq!(*frame.tab_bar.labels, vec!["a.rs".to_string()]);
        assert_eq!(frame.tab_bar.active, Some(0));
        assert!(matches!(frame.body, BodyModel::Pending { .. }));
    }

    #[test]
    fn render_frame_shows_content_truncated_to_viewport() {
        let body = (0..30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[("a.rs", LoadState::Ready(Arc::new(Rope::from_str(&body))))],
            Some(Dims { cols: 10, rows: 5 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, cursor } => {
                assert_eq!(lines.len(), 4); // dims.rows - 1 tab bar row
                assert_eq!(lines[0], "line 0");
                assert_eq!(lines[3], "line 3");
                // Default cursor at (0, 0) — visible at top-left of body.
                assert_eq!(*cursor, Some((0, 0)));
            }
            other => panic!("expected Content, got {other:?}"),
        }
        // Frame-level cursor: body-relative (row=0, col=0) + tab-bar row → (col=0, row=1).
        assert_eq!(frame.cursor, Some((0, 1)));
    }

    #[test]
    fn body_model_scrolls_and_reports_cursor_inside_window() {
        let body = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let (mut t, e, s, term) = fixture(
            &[("big.rs", 1)],
            Some(1),
            &[("big.rs", LoadState::Ready(Arc::new(Rope::from_str(&body))))],
            Some(Dims { cols: 40, rows: 11 }), // 10 body rows
        );
        // Place cursor at line 25 with scroll.top = 20 → cursor visible at row 5.
        t.open[0].cursor = Cursor { line: 25, col: 2, preferred_col: 2 };
        t.open[0].scroll = Scroll { top: 20 };

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, cursor } => {
                assert_eq!(lines.len(), 10);
                assert_eq!(lines[0], "line 20");
                assert_eq!(lines[5], "line 25");
                assert_eq!(*cursor, Some((5, 2)));
            }
            other => panic!("expected Content, got {other:?}"),
        }
        // Absolute frame cursor = (col=2, row=5+1).
        assert_eq!(frame.cursor, Some((2, 6)));
    }

    #[test]
    fn body_model_hides_cursor_when_scrolled_away() {
        let body = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let (mut t, e, s, term) = fixture(
            &[("big.rs", 1)],
            Some(1),
            &[("big.rs", LoadState::Ready(Arc::new(Rope::from_str(&body))))],
            Some(Dims { cols: 40, rows: 6 }), // 5 body rows
        );
        // Cursor far outside the scroll window.
        t.open[0].cursor = Cursor { line: 40, col: 0, preferred_col: 0 };
        t.open[0].scroll = Scroll { top: 0 };

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { cursor, .. } => assert_eq!(*cursor, None),
            other => panic!("expected Content, got {other:?}"),
        }
        assert_eq!(frame.cursor, None);
    }

    #[test]
    fn render_frame_shows_error_when_load_failed() {
        let (t, e, s, term) = fixture(
            &[("bad.rs", 1)],
            Some(1),
            &[("bad.rs", LoadState::Error(Arc::new("No such file".into())))],
            Some(Dims { cols: 80, rows: 24 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match frame.body {
            BodyModel::Error { message, .. } => assert_eq!(&*message, "No such file"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn render_frame_body_empty_when_no_tabs() {
        let (t, e, s, term) = fixture(&[], None, &[], Some(Dims { cols: 80, rows: 24 }));
        let frame = render(&t, &e, &s, &term).expect("dims set");
        assert!(frame.tab_bar.labels.is_empty());
        assert_eq!(frame.tab_bar.active, None);
        assert!(matches!(frame.body, BodyModel::Empty));
    }

    // ── M3: edits-first body + dirty-prefixed tab bar ───────────────────

    #[test]
    fn body_model_prefers_edits_over_store() {
        let (t, mut e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[(
                "a.rs",
                LoadState::Ready(Arc::new(Rope::from_str("disk-version"))),
            )],
            Some(Dims { cols: 40, rows: 5 }),
        );
        // Seed edits with a different rope — this is what the user sees.
        e.buffers.insert(
            canon("a.rs"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("edited-version")),
                version: 1,
                dirty: true,
            },
        );

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                assert_eq!(lines[0], "edited-version");
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn body_model_falls_back_to_store_when_edits_absent() {
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[(
                "a.rs",
                LoadState::Ready(Arc::new(Rope::from_str("from-disk"))),
            )],
            Some(Dims { cols: 40, rows: 5 }),
        );
        // No seed → fallback path.
        assert!(e.buffers.is_empty());
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                assert_eq!(lines[0], "from-disk");
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn tab_bar_prefixes_dirty_labels_with_asterisk() {
        let (t, mut e, s, term) = fixture(
            &[("a.rs", 1), ("b.rs", 2)],
            Some(1),
            &[
                (
                    "a.rs",
                    LoadState::Ready(Arc::new(Rope::from_str("x"))),
                ),
                (
                    "b.rs",
                    LoadState::Ready(Arc::new(Rope::from_str("y"))),
                ),
            ],
            Some(Dims { cols: 40, rows: 5 }),
        );
        // a.rs clean, b.rs dirty.
        e.buffers.insert(
            canon("a.rs"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("x")),
                version: 0,
                dirty: false,
            },
        );
        e.buffers.insert(
            canon("b.rs"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("yy")),
                version: 1,
                dirty: true,
            },
        );

        let frame = render(&t, &e, &s, &term).expect("dims set");
        assert_eq!(
            *frame.tab_bar.labels,
            vec!["a.rs".to_string(), "*b.rs".to_string()]
        );
    }
}
