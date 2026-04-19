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
use led_state_tabs::{Tab, TabId, Tabs};

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
#[drv::memo(single)]
pub fn file_load_action<'a, 'b>(
    store: StoreLoadedInput<'a>,
    tabs: TabsOpenInput<'b>,
) -> imbl::Vector<LoadAction> {
    tabs.open
        .iter()
        .map(|t| t.path.clone())
        .filter(|p| !store.loaded.contains_key(p))
        .map(LoadAction::Load)
        .collect()
}

/// Tab-bar slice of the render frame.
#[drv::memo(single)]
pub fn tab_bar_model<'a>(tabs: TabsActiveInput<'a>) -> TabBarModel {
    let labels: Vec<String> = tabs
        .open
        .iter()
        .map(|t| {
            t.path
                .file_name()
                .map(|os| os.to_string_lossy().into_owned())
                .unwrap_or_else(|| t.path.display().to_string())
        })
        .collect();
    let active = tabs
        .active
        .and_then(|id| tabs.open.iter().position(|t| t.id == id));
    TabBarModel { labels, active }
}

/// Body slice of the render frame.
#[drv::memo(single)]
pub fn body_model<'a, 'b>(
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
    let path_display = tab.path.display().to_string();

    match store.loaded.get(&tab.path) {
        None | Some(LoadState::Pending) => BodyModel::Pending { path_display },
        Some(LoadState::Error(msg)) => BodyModel::Error {
            path_display,
            message: (**msg).clone(),
        },
        Some(LoadState::Ready(rope)) => {
            let body_rows = dims.rows.saturating_sub(1) as usize;
            let lines: Vec<String> = rope
                .lines()
                .take(body_rows)
                .map(|l| {
                    let s = l.to_string();
                    let s = s.strip_suffix('\n').unwrap_or(&s);
                    let s = s.strip_suffix('\r').unwrap_or(s);
                    truncate_to_cols(s, dims.cols as usize)
                })
                .collect();
            BodyModel::Content { lines }
        }
    }
}

fn truncate_to_cols(s: &str, cols: usize) -> String {
    if s.chars().count() <= cols {
        s.to_string()
    } else {
        s.chars().take(cols).collect()
    }
}

/// Top-level render model. Composes `tab_bar_model` + `body_model` —
/// each independently cached in its own per-memo thread-local cache.
///
/// The call site pattern for a memo composing sibling memos: pass the
/// same input values through — drv 0.3's input types are `Copy` over
/// references, so forwarding is free.
#[drv::memo(single)]
pub fn render_frame<'t, 'b, 'a>(
    term: TerminalDimsInput<'t>,
    store: StoreLoadedInput<'b>,
    tabs: TabsActiveInput<'a>,
) -> Option<Frame> {
    let dims = (*term.dims)?;
    let tab_bar = tab_bar_model(tabs);
    let body = body_model(store, tabs, dims);
    Some(Frame {
        tab_bar,
        body,
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
    ) -> (Tabs, BufferStore, Terminal) {
        let mut t = Tabs::default();
        for (p, id) in paths {
            t.open.push_back(Tab {
                id: TabId(*id),
                path: canon(p),
            });
        }
        t.active = active.map(TabId);

        let mut s = BufferStore::default();
        for (p, st) in loaded {
            s.loaded.insert(canon(p), st.clone());
        }

        let mut term = Terminal::default();
        term.dims = dims;

        (t, s, term)
    }

    #[test]
    fn load_action_emits_load_for_absent_paths() {
        let store = BufferStore::default();
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
        });
        tabs.open.push_back(Tab {
            id: TabId(2),
            path: canon("b.rs"),
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
            });
        }

        let acts = file_load_action(
            StoreLoadedInput::new(&store),
            TabsOpenInput::new(&tabs),
        );
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0], LoadAction::Load(canon("new.rs")));
    }

    fn render(t: &Tabs, s: &BufferStore, term: &Terminal) -> Option<Frame> {
        render_frame(
            TerminalDimsInput::new(term),
            StoreLoadedInput::new(s),
            TabsActiveInput::new(t),
        )
    }

    #[test]
    fn render_frame_none_until_dims_known() {
        let (t, s, term) = fixture(&[("a.rs", 1)], Some(1), &[], None);
        assert!(render(&t, &s, &term).is_none());
    }

    #[test]
    fn render_frame_shows_pending_before_content_arrives() {
        let (t, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[],
            Some(Dims { cols: 80, rows: 24 }),
        );
        let frame = render(&t, &s, &term).expect("dims set");
        assert_eq!(frame.tab_bar.labels, vec!["a.rs".to_string()]);
        assert_eq!(frame.tab_bar.active, Some(0));
        assert!(matches!(frame.body, BodyModel::Pending { .. }));
    }

    #[test]
    fn render_frame_shows_content_truncated_to_viewport() {
        let body = (0..30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let (t, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[("a.rs", LoadState::Ready(Arc::new(Rope::from_str(&body))))],
            Some(Dims { cols: 10, rows: 5 }),
        );
        let frame = render(&t, &s, &term).expect("dims set");
        match frame.body {
            BodyModel::Content { lines } => {
                assert_eq!(lines.len(), 4); // dims.rows - 1 tab bar row
                assert_eq!(lines[0], "line 0");
                assert_eq!(lines[3], "line 3");
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn render_frame_shows_error_when_load_failed() {
        let (t, s, term) = fixture(
            &[("bad.rs", 1)],
            Some(1),
            &[("bad.rs", LoadState::Error(Arc::new("No such file".into())))],
            Some(Dims { cols: 80, rows: 24 }),
        );
        let frame = render(&t, &s, &term).expect("dims set");
        match frame.body {
            BodyModel::Error { message, .. } => assert_eq!(message, "No such file"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn render_frame_body_empty_when_no_tabs() {
        let (t, s, term) = fixture(&[], None, &[], Some(Dims { cols: 80, rows: 24 }));
        let frame = render(&t, &s, &term).expect("dims set");
        assert_eq!(frame.tab_bar.labels, Vec::<String>::new());
        assert_eq!(frame.tab_bar.active, None);
        assert!(matches!(frame.body, BodyModel::Empty));
    }
}
