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
use led_driver_buffers_core::{BufferStore, LoadAction, LoadState, SaveAction};
use led_driver_clipboard_core::ClipboardAction;
use led_driver_fs_list_core::ListCmd;
use led_driver_terminal_core::{
    BodyModel, Dims, Frame, Layout, Rect, SidePanelModel, SidePanelRow, StatusBarModel,
    TabBarModel, Terminal,
};
use led_state_alerts::AlertState;
use led_state_browser::{BrowserUi, Focus, TreeEntry, TreeEntryKind};
use led_state_clipboard::ClipboardState;
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

/// Pending-save requests. Narrow projection so a cursor move or a
/// rope edit on `BufferEdits.buffers` doesn't invalidate save-related
/// memo caches.
#[drv::input]
#[derive(Copy, Clone)]
pub struct PendingSavesInput<'a> {
    pub paths: &'a imbl::HashSet<CanonPath>,
}

impl<'a> PendingSavesInput<'a> {
    pub fn new(edits: &'a BufferEdits) -> Self {
        Self {
            paths: &edits.pending_saves,
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

// ── Input on ClipboardState ────────────────────────────────────────────

#[drv::input]
#[derive(Copy, Clone)]
pub struct ClipboardStateInput<'a> {
    pub pending_yank: &'a Option<TabId>,
    pub read_in_flight: &'a bool,
    pub pending_write: &'a Option<Arc<str>>,
}

impl<'a> ClipboardStateInput<'a> {
    pub fn new(c: &'a ClipboardState) -> Self {
        Self {
            pending_yank: &c.pending_yank,
            read_in_flight: &c.read_in_flight,
            pending_write: &c.pending_write,
        }
    }
}

// ── Input on AlertState ────────────────────────────────────────────────

/// Narrow projection — excludes `info_expires_at` since it changes
/// every 10ms and would thrash the status-bar memo cache. The expiry
/// is the runtime's concern, not the painter's.
#[drv::input]
#[derive(Copy, Clone)]
pub struct AlertsInput<'a> {
    pub info: &'a Option<String>,
    pub warns: &'a Vec<(String, String)>,
    pub confirm_kill: &'a Option<TabId>,
}

impl<'a> AlertsInput<'a> {
    pub fn new(a: &'a AlertState) -> Self {
        Self {
            info: &a.info,
            warns: &a.warns,
            confirm_kill: &a.confirm_kill,
        }
    }
}

// ── Input on BrowserUi ──────────────────────────────────────────────

/// External-fact projection for [`FsTree`]. Written by the FS driver;
/// consumed by `file_list_action` and (indirectly) `side_panel_model`.
#[drv::input]
#[derive(Copy, Clone)]
pub struct FsTreeInput<'a> {
    pub root: &'a Option<CanonPath>,
    pub dir_contents: &'a imbl::HashMap<CanonPath, imbl::Vector<led_state_browser::DirEntry>>,
}

impl<'a> FsTreeInput<'a> {
    pub fn new(fs: &'a led_state_browser::FsTree) -> Self {
        Self {
            root: &fs.root,
            dir_contents: &fs.dir_contents,
        }
    }
}

/// User-decision projection for [`BrowserUi`]. Mutated by dispatch.
#[drv::input]
#[derive(Copy, Clone)]
pub struct BrowserUiInput<'a> {
    pub expanded_dirs: &'a imbl::HashSet<CanonPath>,
    pub entries: &'a Arc<Vec<TreeEntry>>,
    pub selected: &'a usize,
    pub scroll_offset: &'a usize,
    pub visible: &'a bool,
    pub focus: &'a Focus,
}

impl<'a> BrowserUiInput<'a> {
    pub fn new(b: &'a BrowserUi) -> Self {
        Self {
            expanded_dirs: &b.expanded_dirs,
            entries: &b.entries,
            selected: &b.selected,
            scroll_offset: &b.scroll_offset,
            visible: &b.visible,
            focus: &b.focus,
        }
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

/// "What saves should we dispatch now?"
///
/// Diffs the user's save requests (`pending_saves`) against the
/// edited buffers, emitting one `SaveAction` per path that is both
/// requested and dirty. Runtime sync-clears `pending_saves` for the
/// emitted paths before calling `FileWriteDriver::execute` — without
/// that clear the next tick's query would emit the same saves again.
///
/// Idle: `pending_saves` is empty → returns `Vec::new()` (no alloc).
#[drv::memo(single)]
pub fn file_save_action<'p, 'b>(
    pending: PendingSavesInput<'p>,
    buffers: EditedBuffersInput<'b>,
) -> Vec<SaveAction> {
    let mut out: Vec<SaveAction> = Vec::new();
    for path in pending.paths.iter() {
        let Some(eb) = buffers.buffers.get(path) else {
            continue;
        };
        if !eb.dirty() {
            continue;
        }
        out.push(SaveAction::Save {
            path: path.clone(),
            rope: eb.rope.clone(),
            version: eb.version,
        });
    }
    out
}

/// Tab-bar slice of the render frame.
///
/// Labels are wrapped in `Arc` so cache-hit clones of [`TabBarModel`]
/// (inside `Frame`, deep inside `render_frame`'s cache slot) are a
/// pointer copy.
///
/// Format per label: `<prefix><name>` where `<prefix>` is `●`
/// (filled circle) when the buffer is dirty, else a space. The painter
/// wraps each label in `" <label> "`, so the two cases render as
/// `"  foo.rs "` (clean) and `" ●foo.rs "` (dirty) — the `●`
/// replaces the second leading space, matching the legacy goldens.
#[drv::memo(single)]
pub fn tab_bar_model<'a, 'b>(
    tabs: TabsActiveInput<'a>,
    edits: EditedBuffersInput<'b>,
) -> TabBarModel {
    let active = tabs
        .active
        .and_then(|id| tabs.open.iter().position(|t| t.id == id));
    let labels: Vec<String> = tabs
        .open
        .iter()
        .map(|t| {
            let base = t
                .path
                .file_name()
                .map(|os| os.to_string_lossy().into_owned())
                .unwrap_or_else(|| t.path.display().to_string());
            let dirty = edits
                .buffers
                .get(&t.path)
                .map(|b| b.dirty())
                .unwrap_or(false);
            let mut s = String::with_capacity(base.len() + "\u{25cf}".len());
            if dirty {
                s.push('\u{25cf}'); // ●
            } else {
                s.push(' ');
            }
            s.push_str(&base);
            s
        })
        .collect();
    TabBarModel {
        labels: Arc::new(labels),
        active,
    }
}

// Gutter width reserved on the left of every body row. M9 renders
// two blank cols; future milestones fill col 0 with git marks and
// col 1 with diagnostic severity.
const GUTTER_WIDTH: usize = 2;

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
    area: Rect,
) -> BodyModel {
    let Some(id) = *tabs.active else {
        return BodyModel::Empty;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
        return BodyModel::Empty;
    };
    if let Some(eb) = edits.buffers.get(&tab.path) {
        return render_content(&eb.rope, tab.cursor, tab.scroll, area);
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
        Some(LoadState::Ready(rope)) => render_content(rope, tab.cursor, tab.scroll, area),
    }
}

fn path_display(tab: &Tab) -> Arc<str> {
    Arc::<str>::from(tab.path.display().to_string())
}

fn render_content(rope: &Rope, cursor: Cursor, scroll: Scroll, area: Rect) -> BodyModel {
    let body_rows = area.rows as usize;
    let line_count = rope.len_lines();
    let cols = area.cols as usize;
    let content_cols = cols.saturating_sub(GUTTER_WIDTH);

    let mut lines: Vec<String> = Vec::with_capacity(body_rows);
    for i in 0..body_rows {
        let ln = scroll.top.saturating_add(i);
        if ln < line_count {
            // Content row: 2-space gutter, then truncated buffer line.
            let mut s = String::with_capacity(cols);
            s.push_str("  ");
            let mut content = rope.line(ln).to_string();
            strip_trailing_newline(&mut content);
            truncate_to_cols_in_place(&mut content, content_cols);
            s.push_str(&content);
            lines.push(s);
        } else {
            // Past-EOF sentinel: tilde in gutter col 0. Painter's
            // clear-to-EOL blanks the rest of the row.
            lines.push("~ ".to_string());
        }
    }

    BodyModel::Content {
        lines: Arc::new(lines),
        cursor: visible_cursor(cursor, scroll, area),
    }
}

fn visible_cursor(c: Cursor, s: Scroll, area: Rect) -> Option<(u16, u16)> {
    let body_rows = area.rows as usize;
    if body_rows == 0 {
        return None;
    }
    if c.line < s.top || c.line >= s.top.saturating_add(body_rows) {
        return None;
    }
    let row = (c.line - s.top) as u16;
    let max_col = (area.cols as usize).saturating_sub(1);
    let col = (c.col + GUTTER_WIDTH).min(max_col) as u16;
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

/// Status-bar slice of the render frame.
///
/// Priority chain (highest wins):
/// 1. **Confirm-kill prompt** — blocks other content; no position
///    indicator. Matches legacy dismiss-on-first-keystroke UX.
/// 2. **Info alert** — transient; shown alongside the position.
/// 3. **Warn alert** — persistent; shown alongside the position with
///    the `is_warn` flag set (painter renders white-on-red-bold).
/// 4. **Default** — left is `"  ●"` when the active buffer is dirty
///    else empty; right is `"L<row>:C<col> "` (1-indexed human
///    coords, trailing space).
///
/// All strings are `Arc<str>` so cache-hit clones of
/// [`StatusBarModel`] are a pointer copy.
#[drv::memo(single)]
pub fn status_bar_model<'a, 'b, 'c>(
    alerts: AlertsInput<'a>,
    tabs: TabsActiveInput<'b>,
    edits: EditedBuffersInput<'c>,
) -> StatusBarModel {
    // Priority 1 — confirm-kill prompt.
    if let Some(kill_id) = *alerts.confirm_kill {
        let name = tabs
            .open
            .iter()
            .find(|t| t.id == kill_id)
            .and_then(|t| t.path.file_name().map(|os| os.to_string_lossy().into_owned()))
            .unwrap_or_default();
        return StatusBarModel {
            left: Arc::from(format!(" Kill buffer '{name}'? (y/N) ")),
            right: Arc::from(""),
            is_warn: false,
        };
    }

    let right = position_string(tabs, edits);

    // Priority 2 — info alert.
    if let Some(msg) = alerts.info.as_deref() {
        let mut left = String::with_capacity(msg.len() + 1);
        left.push(' ');
        left.push_str(msg);
        return StatusBarModel {
            left: Arc::from(left),
            right,
            is_warn: false,
        };
    }

    // Priority 3 — warn alert (first-arrived).
    if let Some((_, msg)) = alerts.warns.first() {
        let mut left = String::with_capacity(msg.len() + 1);
        left.push(' ');
        left.push_str(msg);
        return StatusBarModel {
            left: Arc::from(left),
            right,
            is_warn: true,
        };
    }

    // Priority 4 — default. Dirty dot in cols 2–3 (padded for
    // visual alignment with the gutter above).
    let dirty = active_is_dirty(tabs, edits);
    let left: Arc<str> = if dirty {
        Arc::from("  \u{25cf}")
    } else {
        Arc::from("")
    };
    StatusBarModel {
        left,
        right,
        is_warn: false,
    }
}

fn active_tab<'t>(tabs: TabsActiveInput<'t>) -> Option<&'t Tab> {
    let id = (*tabs.active)?;
    tabs.open.iter().find(|t| t.id == id)
}

fn active_is_dirty(tabs: TabsActiveInput<'_>, edits: EditedBuffersInput<'_>) -> bool {
    let Some(tab) = active_tab(tabs) else {
        return false;
    };
    edits
        .buffers
        .get(&tab.path)
        .map(|eb| eb.dirty())
        .unwrap_or(false)
}

fn position_string(tabs: TabsActiveInput<'_>, _edits: EditedBuffersInput<'_>) -> Arc<str> {
    let Some(tab) = active_tab(tabs) else {
        return Arc::from("");
    };
    // 1-indexed for human display — matches legacy goldens.
    let row = tab.cursor.line + 1;
    let col = tab.cursor.col + 1;
    Arc::from(format!("L{row}:C{col} "))
}

/// Side-panel slice of the render frame. Walks the visible window
/// of `browser.entries` and produces one `SidePanelRow` per row.
/// Empty when the browser has no entries.
#[drv::memo(single)]
pub fn side_panel_model<'b>(browser: BrowserUiInput<'b>, rows: u16) -> SidePanelModel {
    let rows = rows as usize;
    let start = *browser.scroll_offset;
    let end = start.saturating_add(rows).min(browser.entries.len());
    let selected = *browser.selected;
    let focused = *browser.focus == Focus::Side;
    let mut out: Vec<SidePanelRow> = Vec::with_capacity(end.saturating_sub(start));
    for (i, entry) in browser.entries[start..end].iter().enumerate() {
        let chevron = match entry.kind {
            TreeEntryKind::File => None,
            TreeEntryKind::Directory { expanded } => Some(expanded),
        };
        out.push(SidePanelRow {
            depth: entry.depth as u16,
            chevron,
            name: Arc::<str>::from(entry.name.as_str()),
            selected: start + i == selected,
        });
    }
    SidePanelModel {
        rows: Arc::new(out),
        focused,
    }
}

/// "What clipboard action should we fire this tick?"
///
/// Returns `None` on an idle tick (no yank pending, no write
/// queued). Returns `Some(Read)` when a yank is pending and no
/// read is in flight. Returns `Some(Write(_))` with a clone of the
/// pending text when a kill queued one. When both signals are
/// live, yank wins — matches legacy ordering.
///
/// Zero allocation on idle (returns a simple `Option`); one Arc
/// clone on the Write path, which is the same as the driver's own
/// execute.
#[drv::memo(single)]
pub fn clipboard_action<'c>(clip: ClipboardStateInput<'c>) -> Option<ClipboardAction> {
    if clip.pending_yank.is_some() && !*clip.read_in_flight {
        Some(ClipboardAction::Read)
    } else {
        clip.pending_write
            .as_ref()
            .map(|text| ClipboardAction::Write(text.clone()))
    }
}

/// "What directory listings do we still need?"
///
/// Emits one `ListCmd::List` per path that's expected to have a
/// listing (workspace root + every expanded dir) but isn't in
/// `dir_contents` yet. Used to drive `FsListDriver::execute`.
#[drv::memo(single)]
pub fn file_list_action<'f, 'u>(
    fs: FsTreeInput<'f>,
    ui: BrowserUiInput<'u>,
) -> Vec<ListCmd> {
    let mut out: Vec<ListCmd> = Vec::new();
    if let Some(root) = fs.root.as_ref()
        && !fs.dir_contents.contains_key(root)
    {
        out.push(ListCmd::List(root.clone()));
    }
    for dir in ui.expanded_dirs.iter() {
        if !fs.dir_contents.contains_key(dir) {
            out.push(ListCmd::List(dir.clone()));
        }
    }
    out
}

/// Top-level render model. Composes the per-region memos — each
/// independently cached in its own per-memo thread-local cache.
///
/// The call site pattern for a memo composing sibling memos: pass the
/// same input values through — drv 0.3's input types are `Copy` over
/// references, so forwarding is free.
#[drv::memo(single)]
pub fn render_frame<'t, 'e, 'b, 'a, 'al, 'br>(
    term: TerminalDimsInput<'t>,
    edits: EditedBuffersInput<'e>,
    store: StoreLoadedInput<'b>,
    tabs: TabsActiveInput<'a>,
    alerts: AlertsInput<'al>,
    browser: BrowserUiInput<'br>,
) -> Option<Frame> {
    let dims = (*term.dims)?;
    let layout = Layout::compute(dims, *browser.visible);
    let tab_bar = tab_bar_model(tabs, edits);
    let body = body_model(edits, store, tabs, layout.editor_area);
    let status_bar = status_bar_model(alerts, tabs, edits);
    let side_panel = layout
        .side_area
        .map(|area| side_panel_model(browser, area.rows));
    // Body cursor is body-area-relative. Shift to absolute screen
    // coords by adding the editor area's origin.
    let cursor = match &body {
        BodyModel::Content {
            cursor: Some((row, col)),
            ..
        } => Some((
            layout.editor_area.x.saturating_add(*col),
            layout.editor_area.y.saturating_add(*row),
        )),
        _ => None,
    };
    Some(Frame {
        tab_bar,
        body,
        status_bar,
        side_panel,
        layout,
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
        let alerts = AlertState::default();
        // Tests render without the side panel so body layout matches
        // the pre-M11 assertions — M11 tests for the side panel are
        // separate.
        let browser = BrowserUi {
            visible: false,
            ..Default::default()
        };
        render_frame(
            TerminalDimsInput::new(term),
            EditedBuffersInput::new(e),
            StoreLoadedInput::new(s),
            TabsActiveInput::new(t),
            AlertsInput::new(&alerts),
            BrowserUiInput::new(&browser),
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
        assert_eq!(*frame.tab_bar.labels, vec![" a.rs".to_string()]);
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
                // body_rows = dims.rows - 2 (tab bar + status bar).
                assert_eq!(lines.len(), 3);
                // Each content row is 2-col gutter + truncated content.
                assert_eq!(lines[0], "  line 0");
                assert_eq!(lines[2], "  line 2");
                // Default cursor at (0, 0) → gutter-shifted to col 2.
                assert_eq!(*cursor, Some((0, 2)));
            }
            other => panic!("expected Content, got {other:?}"),
        }
        // Body starts at row 0 now — no +1.
        assert_eq!(frame.cursor, Some((2, 0)));
    }

    #[test]
    fn body_model_scrolls_and_reports_cursor_inside_window() {
        let body = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let (mut t, e, s, term) = fixture(
            &[("big.rs", 1)],
            Some(1),
            &[("big.rs", LoadState::Ready(Arc::new(Rope::from_str(&body))))],
            Some(Dims { cols: 40, rows: 11 }), // body_rows = 11 - 2 = 9
        );
        // Place cursor at line 25 with scroll.top = 20 → cursor visible at row 5.
        t.open[0].cursor = Cursor { line: 25, col: 2, preferred_col: 2 };
        t.open[0].scroll = Scroll { top: 20 };

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, cursor } => {
                assert_eq!(lines.len(), 9);
                assert_eq!(lines[0], "  line 20");
                assert_eq!(lines[5], "  line 25");
                // Cursor col 2 → screen col 4 (gutter shift).
                assert_eq!(*cursor, Some((5, 4)));
            }
            other => panic!("expected Content, got {other:?}"),
        }
        // Absolute frame cursor = (col, row) — body starts at row 0.
        assert_eq!(frame.cursor, Some((4, 5)));
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
                saved_version: 0,
                history: Default::default(),
            },
        );

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                assert_eq!(lines[0], "  edited-version");
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
                assert_eq!(lines[0], "  from-disk");
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    // ── M4: save action memo ────────────────────────────────────────────

    #[test]
    fn file_save_action_empty_when_nothing_pending() {
        let e = BufferEdits::default();
        let actions = file_save_action(
            PendingSavesInput::new(&e),
            EditedBuffersInput::new(&e),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn file_save_action_emits_save_for_pending_dirty_buffer() {
        let mut e = BufferEdits::default();
        let path = canon("a.rs");
        let rope = Arc::new(Rope::from_str("payload"));
        e.buffers.insert(
            path.clone(),
            EditedBuffer {
                rope: rope.clone(),
                version: 3,
                saved_version: 0,
                history: Default::default(),
            },
        );
        e.pending_saves.insert(path.clone());

        let actions = file_save_action(
            PendingSavesInput::new(&e),
            EditedBuffersInput::new(&e),
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SaveAction::Save {
                path: p,
                rope: r,
                version,
            } => {
                assert_eq!(p, &path);
                assert!(Arc::ptr_eq(r, &rope));
                assert_eq!(*version, 3);
            }
        }
    }

    #[test]
    fn file_save_action_skips_clean_buffers() {
        let mut e = BufferEdits::default();
        let path = canon("clean.rs");
        e.buffers.insert(
            path.clone(),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("x")),
                version: 0,
                saved_version: 0, // dirty() == false
                history: Default::default(),
            },
        );
        e.pending_saves.insert(path);

        let actions = file_save_action(
            PendingSavesInput::new(&e),
            EditedBuffersInput::new(&e),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn file_save_action_skips_pending_paths_with_no_buffer() {
        // Could happen if pending entry leaked past a tab close. Memo
        // must not panic or emit phantom saves.
        let mut e = BufferEdits::default();
        e.pending_saves.insert(canon("ghost.rs"));

        let actions = file_save_action(
            PendingSavesInput::new(&e),
            EditedBuffersInput::new(&e),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn tab_bar_prefixes_dirty_labels_with_dot() {
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
                saved_version: 0,
                history: Default::default(),
            },
        );
        e.buffers.insert(
            canon("b.rs"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("yy")),
                version: 1,
                saved_version: 0,
                history: Default::default(),
            },
        );

        let frame = render(&t, &e, &s, &term).expect("dims set");
        assert_eq!(
            *frame.tab_bar.labels,
            vec![" a.rs".to_string(), "\u{25cf}b.rs".to_string()]
        );
    }

    // ── M9: past-EOF tildes ─────────────────────────────────────────────

    #[test]
    fn body_model_fills_past_eof_rows_with_tilde() {
        // Two-line rope in a six-row viewport: body_rows = 4, so rows
        // 2 and 3 are past-EOF.
        let (t, e, s, term) = fixture(
            &[("short.rs", 1)],
            Some(1),
            &[(
                "short.rs",
                LoadState::Ready(Arc::new(Rope::from_str("one\ntwo"))),
            )],
            Some(Dims { cols: 20, rows: 6 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                assert_eq!(lines.len(), 4);
                assert_eq!(lines[0], "  one");
                assert_eq!(lines[1], "  two");
                assert_eq!(lines[2], "~ ");
                assert_eq!(lines[3], "~ ");
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    // ── M9: status bar model ────────────────────────────────────────────

    fn status(a: &AlertState, t: &Tabs, e: &BufferEdits) -> StatusBarModel {
        status_bar_model(
            AlertsInput::new(a),
            TabsActiveInput::new(t),
            EditedBuffersInput::new(e),
        )
    }

    #[test]
    fn status_bar_default_empty_when_no_tab() {
        let s = status(&AlertState::default(), &Tabs::default(), &BufferEdits::default());
        assert_eq!(&*s.left, "");
        assert_eq!(&*s.right, "");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_default_clean_shows_position_only() {
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            cursor: Cursor { line: 0, col: 0, preferred_col: 0 },
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let s = status(&AlertState::default(), &tabs, &BufferEdits::default());
        assert_eq!(&*s.left, "");
        assert_eq!(&*s.right, "L1:C1 ");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_default_dirty_shows_dot_and_position() {
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            cursor: Cursor { line: 4, col: 10, preferred_col: 10 },
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a.rs"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("x")),
                version: 3,
                saved_version: 1, // dirty
                history: Default::default(),
            },
        );
        let s = status(&AlertState::default(), &tabs, &edits);
        assert_eq!(&*s.left, "  \u{25cf}");
        assert_eq!(&*s.right, "L5:C11 ");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_shows_info_alert() {
        let a = AlertState {
            info: Some("Saved foo.rs".into()),
            ..Default::default()
        };
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let s = status(&a, &tabs, &BufferEdits::default());
        assert_eq!(&*s.left, " Saved foo.rs");
        assert_eq!(&*s.right, "L1:C1 ");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_shows_warn_with_warn_flag() {
        let a = AlertState {
            warns: vec![("a.rs".into(), "save a.rs: permission denied".into())],
            ..Default::default()
        };
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let s = status(&a, &tabs, &BufferEdits::default());
        assert_eq!(&*s.left, " save a.rs: permission denied");
        assert!(s.is_warn);
    }

    #[test]
    fn status_bar_info_wins_over_warn() {
        let a = AlertState {
            info: Some("Saved".into()),
            warns: vec![("k".into(), "oh no".into())],
            ..Default::default()
        };
        let s = status(&a, &Tabs::default(), &BufferEdits::default());
        assert_eq!(&*s.left, " Saved");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_confirm_kill_wins_over_info() {
        let a = AlertState {
            confirm_kill: Some(TabId(1)),
            info: Some("Saved".into()),
            ..Default::default()
        };
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("draft.txt"),
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let s = status(&a, &tabs, &BufferEdits::default());
        assert_eq!(&*s.left, " Kill buffer 'draft.txt'? (y/N) ");
        assert_eq!(&*s.right, "");
    }

    #[test]
    fn render_frame_composes_status_bar() {
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[("a.rs", LoadState::Ready(Arc::new(Rope::from_str("x"))))],
            Some(Dims { cols: 40, rows: 5 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        assert_eq!(&*frame.status_bar.right, "L1:C1 ");
    }
}
