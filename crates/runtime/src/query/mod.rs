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

pub mod actions;
pub mod browser;
pub mod desired;
pub mod inputs;
pub mod render;

pub use actions::{
    buffer_state_sum, clipboard_action, external_reread_targets, file_load_action,
    file_save_action, find_file_action, notify_hash_index, static_deadline,
    sync_check_cmds,
};
pub use browser::{
    browser_auto_expanded, browser_entries, browser_selected_idx, file_categories_map,
    file_list_action, BrowserDerivedInputs,
};
pub use desired::{
    desired_inlay_hint_requests, desired_lsp_buffer_changed, desired_syntax_parses,
    desired_watches, lsp_watched_file_notifications,
};
pub use inputs::{
    AlertExpiryInput, AlertsInput, BrowserUiInput, ClipboardStateInput, ClockInput,
    CompletionsSessionInput, DiagnosticsStatesInput, EditedBuffersInput,
    FileWatchEventsInput, FileWatchRegistryInput, FindFileInput, FsRootInput,
    FsTreeInput, GitStateInput, HashIndexInput, KbdMacroRecordingInput,
    LspExtrasOverlayInput, LspInlayHintsEnabledInput, LspInlayHintsRequestedInput,
    LspNotifiedInput, LspStatusesInput, LspWatchedGlobsInput, NotifyDirInput,
    OverlaysInput, PendingSavesInput, StoreLoadedInput, SyntaxStatesInput,
    TabsActiveInput, TabsOpenInput, TerminalDimsInput, UndoFlushDebounceInput,
    UndoPersistenceInput,
};
pub use render::{
    body_model, code_action_popup_model, completion_popup_model, popover_model,
    rebased_line_spans, rename_popup_model, render_frame, side_panel_model,
    status_bar_model, tab_bar_model, BodyInputs, RenderInputs, SidePanelInputs,
    StatusBarInputs,
};

// Re-export internal constants and helpers needed by the in-tree
// tests below.
#[cfg(test)]
pub(crate) use render::GUTTER_WIDTH;
#[cfg(test)]
use render::body::{merged_gutter_category, tokens_to_line_spans};
#[cfg(test)]
use render::side_panel::{file_search_side_panel, trim_preview_at_budget};
#[cfg(test)]
use render::status_bar::{format_lsp_status, lsp_progress_message};

#[cfg(test)]
use led_core::{BufferVersion, CanonPath, SavedVersion};
#[cfg(test)]
use led_driver_buffers_core::{BufferStore, LoadAction, LoadState, SaveAction};
#[cfg(test)]
use led_driver_fs_list_core::ListCmd;
#[cfg(test)]
use led_driver_terminal_core::{
    BodyModel, Dims, Frame, PopoverModel, PopoverSeverity, Rect, SidePanelRow,
    StatusBarModel, Terminal,
};
#[cfg(test)]
use led_state_alerts::AlertState;
#[cfg(test)]
use led_state_browser::{BrowserUi, Focus};
#[cfg(test)]
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
#[cfg(test)]
use led_state_diagnostics::{
    BufferDiagnostics, Diagnostic, DiagnosticSeverity, DiagnosticsStates, LspServerStatus,
    LspStatuses,
};
#[cfg(test)]
use led_state_syntax::{SyntaxStates, TokenKind, TokenSpan};
#[cfg(test)]
use led_state_tabs::{Cursor, Scroll, Tab, TabId, Tabs};

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
    fn list_action_skips_failed_dirs() {
        // The spin-bug regression test: a path in `expanded_dirs`
        // but not in `dir_contents` would normally re-emit
        // `ListCmd::List` every tick; with the path also recorded
        // in `failed_dirs` (the runtime's marker for "we tried,
        // it didn't work"), the memo must skip it. Without this
        // gate the main loop sat at 100 % CPU as the fs-list
        // worker fail-loop signalled the wake notifier on every
        // attempt.
        let mut fs = led_state_browser::FsTree {
            root: Some(canon("/proj")),
            ..Default::default()
        };
        // Root listing already cached so the root itself doesn't
        // contribute a List action — we want the test to isolate
        // the `expanded_dirs` path.
        fs.dir_contents.insert(canon("/proj"), imbl::Vector::new());
        // One healthy expansion (no listing yet → should be
        // emitted) and one failed expansion (should be skipped).
        let mut ui = led_state_browser::BrowserUi::default();
        ui.expanded_dirs.insert(canon("/proj/healthy"));
        ui.expanded_dirs.insert(canon("/proj/missing"));
        fs.failed_dirs.insert(canon("/proj/missing"));

        let tabs = Tabs::default();
        let edits = BufferEdits::default();
        let acts = file_list_action(BrowserDerivedInputs {
            fs: FsTreeInput::new(&fs),
            ui: BrowserUiInput::new(&ui),
            tabs: TabsActiveInput::new(&tabs),
            edits: EditedBuffersInput::new(&edits),
        });
        assert_eq!(acts.len(), 1, "only the healthy path should emit");
        assert_eq!(acts[0], ListCmd::List(canon("/proj/healthy")));
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
        let ff = None;
        let is = None;
        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        let kbd_macro_default = led_state_kbd_macro::KbdMacroState::default();
        render_frame(RenderInputs {
            term: TerminalDimsInput::new(term),
            edits: EditedBuffersInput::new(e),
            store: StoreLoadedInput::new(s),
            tabs: TabsActiveInput::new(t),
            alerts: AlertsInput::new(&alerts),
            browser: BrowserUiInput::new(&browser),
            fs: FsTreeInput::new(&led_state_browser::FsTree::default()),
            overlays: OverlaysInput::new(&ff, &is, &None),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            completions: CompletionsSessionInput::new(
                &led_state_completions::CompletionsState::default(),
            ),
            lsp_extras: LspExtrasOverlayInput::new(&led_state_lsp::LspExtrasState::default()),
            git: GitStateInput::new(&led_state_git::GitState::default()),
            render_tick: 0,
            kbd_macro: KbdMacroRecordingInput::new(&kbd_macro_default),
        })
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
        // Pre-load body renders as a blank Content frame: the
        // single rope line (len_lines=1) paints as an empty body
        // row, every row past it paints as a tilde. No inline
        // "loading..." placeholder — we keep the editing canvas
        // clean while the async read resolves.
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                // First body row is the empty content line.
                assert_eq!(lines[0].text.trim_end(), "");
                // Every row past line 0 is a tilde (past-EOF).
                assert!(
                    lines[1..].iter().all(|l| l.text.starts_with("~ ")),
                    "rows past line 0 should all be tildes",
                );
            }
            other => panic!("expected Content (blank), got {other:?}"),
        }
    }

    #[test]
    fn render_frame_parks_cursor_in_status_bar_when_find_file_active() {
        use led_state_find_file::{FindFileState, FindFileMode};
        // Any body content works — when the overlay is open the
        // cursor moves to the status-bar prompt regardless of what
        // the buffer contains.
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[("a.rs", LoadState::Ready(Arc::new(Rope::from_str("hi"))))],
            Some(Dims { cols: 80, rows: 24 }),
        );
        let alerts = AlertState::default();
        let browser = BrowserUi { visible: false, ..Default::default() };
        // Open mode: prefix " Find file: " is 12 cols; `input.cursor`
        // at byte 4 in "abcd" is 4 chars → absolute col 16.
        let mut ff_state = FindFileState::open("abcd".to_string());
        ff_state.input.cursor = 4;
        assert_eq!(ff_state.mode, FindFileMode::Open);
        let ff = Some(ff_state);
        let is = None;

        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        let kbd_macro_default = led_state_kbd_macro::KbdMacroState::default();
        let frame = render_frame(RenderInputs {
            term: TerminalDimsInput::new(&term),
            edits: EditedBuffersInput::new(&e),
            store: StoreLoadedInput::new(&s),
            tabs: TabsActiveInput::new(&t),
            alerts: AlertsInput::new(&alerts),
            browser: BrowserUiInput::new(&browser),
            fs: FsTreeInput::new(&led_state_browser::FsTree::default()),
            overlays: OverlaysInput::new(&ff, &is, &None),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            completions: CompletionsSessionInput::new(
                &led_state_completions::CompletionsState::default(),
            ),
            lsp_extras: LspExtrasOverlayInput::new(&led_state_lsp::LspExtrasState::default()),
            git: GitStateInput::new(&led_state_git::GitState::default()),
            render_tick: 0,
            kbd_macro: KbdMacroRecordingInput::new(&kbd_macro_default),
        })
        .expect("dims set");

        // dims.rows = 24 → status bar at row 23.
        assert_eq!(frame.cursor, Some((16, 23)));
    }

    #[test]
    fn render_frame_hides_cursor_when_side_panel_focused() {
        // With focus on the side panel the editor cursor must be
        // `None` — otherwise crossterm would `Show` it at the body
        // origin while the user is navigating the tree.
        let body = "hello".to_string();
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[("a.rs", LoadState::Ready(Arc::new(Rope::from_str(&body))))],
            Some(Dims { cols: 80, rows: 24 }),
        );
        let alerts = AlertState::default();
        let browser = BrowserUi {
            visible: false,
            focus: Focus::Side,
            ..Default::default()
        };
        let ff = None;
        let is = None;
        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        let kbd_macro_default = led_state_kbd_macro::KbdMacroState::default();
        let frame = render_frame(RenderInputs {
            term: TerminalDimsInput::new(&term),
            edits: EditedBuffersInput::new(&e),
            store: StoreLoadedInput::new(&s),
            tabs: TabsActiveInput::new(&t),
            alerts: AlertsInput::new(&alerts),
            browser: BrowserUiInput::new(&browser),
            fs: FsTreeInput::new(&led_state_browser::FsTree::default()),
            overlays: OverlaysInput::new(&ff, &is, &None),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            completions: CompletionsSessionInput::new(
                &led_state_completions::CompletionsState::default(),
            ),
            lsp_extras: LspExtrasOverlayInput::new(&led_state_lsp::LspExtrasState::default()),
            git: GitStateInput::new(&led_state_git::GitState::default()),
            render_tick: 0,
            kbd_macro: KbdMacroRecordingInput::new(&kbd_macro_default),
        })
        .expect("dims set");
        assert_eq!(frame.cursor, None);
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
            BodyModel::Content { lines, cursor, .. } => {
                // body_rows = dims.rows - 2 (tab bar + status bar).
                assert_eq!(lines.len(), 3);
                // Each content row is 2-col gutter + truncated content.
                assert_eq!(lines[0].text, "  line 0");
                assert_eq!(lines[2].text, "  line 2");
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
        t.open[0].scroll = Scroll { top: 20, top_sub_line: led_core::SubLine(0) };

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, cursor, .. } => {
                assert_eq!(lines.len(), 9);
                assert_eq!(lines[0].text, "  line 20");
                assert_eq!(lines[5].text, "  line 25");
                // Cursor col 2 → screen col 4 (gutter shift).
                assert_eq!(*cursor, Some((5, 4)));
            }
            other => panic!("expected Content, got {other:?}"),
        }
        // Absolute frame cursor = (col, row) — body starts at row 0.
        assert_eq!(frame.cursor, Some((4, 5)));
    }

    #[test]
    fn body_model_wraps_long_logical_line_across_multiple_body_rows() {
        // cols=12 → editor_area.cols=12; minus 2 gutter + 0
        // trailing reserved col = content_cols 10, wrap_width 9
        // (one trailing col per non-last sub: `\`).
        // A 50-char line splits into 6 sub-lines of widths
        // 9/9/9/9/9/5.
        let rope = Arc::new(Rope::from_str(
            "abcdefghij0123456789ABCDEFGHIJ!@#$%^&*()qwertyuiop",
        ));
        let (mut t, e, s, term) = fixture(
            &[("wide.rs", 1)],
            Some(1),
            &[("wide.rs", LoadState::Ready(rope))],
            Some(Dims { cols: 12, rows: 11 }), // body_rows = 9
        );
        // Cursor at col 25 → sub 25/9 = 2, within 25 % 9 = 7.
        t.open[0].cursor = Cursor { line: 0, col: 25, preferred_col: 7 };
        t.open[0].scroll = Scroll { top: 0, top_sub_line: led_core::SubLine(0) };

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, cursor, .. } => {
                assert_eq!(lines.len(), 9);
                assert_eq!(lines[0].text, "  abcdefghi\\");
                assert_eq!(lines[1].text, "  j01234567\\");
                assert_eq!(lines[2].text, "  89ABCDEFG\\");
                assert_eq!(lines[3].text, "  HIJ!@#$%^\\");
                assert_eq!(lines[4].text, "  &*()qwert\\");
                assert_eq!(lines[5].text, "  yuiop");
                assert_eq!(lines[6].text, "~ ");
                // Cursor on sub 2 within 7 → body row 2, screen col 2+7=9.
                assert_eq!(*cursor, Some((2, 9)));
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn body_model_renders_wrap_glyph_on_every_non_last_sub_of_long_line() {
        // Regression guard for the README.md rendering bug where
        // the last visible wrap row on a long logical line came
        // out without its trailing `\` (the user saw `algesten/s`
        // where `algesten/s\` was expected, with `tr0m).` on the
        // next row). The line is the full M1 README warning
        // paragraph (410 chars). At content_cols=102 it wraps
        // into 5 sub-lines: 4 non-last + 1 last. Every non-last
        // sub must carry `\` regardless of whether it's the final
        // row the body rendered.
        let line = "> **Vibe coded.** This project is an experiment in getting an \
AI assistant to follow Functional Reactive Programming (FRP) principles and \
produce reasonable code within that discipline. I've focused on the overall \
architecture rather than reviewing the code output in detail. For projects \
I've mostly written by hand, see [ureq](https://github.com/algesten/ureq) and \
[str0m](https://github.com/algesten/str0m).";
        assert_eq!(line.chars().count(), 410);
        let rope = Arc::new(Rope::from_str(line));
        let (t, e, s, term) = fixture(
            &[("README.md", 1)],
            Some(1),
            &[("README.md", LoadState::Ready(rope))],
            // cols=104 → editor_area.cols=104; content_cols=102;
            // wrap_width=101 → 5 sub-lines (101/101/101/101/6).
            // rows=8 gives body_rows=6, enough to show all 5 +
            // one tilde row.
            Some(Dims { cols: 104, rows: 8 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                // Subs 0..3 are non-last → must end with `\`.
                for i in 0..4 {
                    assert!(
                        lines[i].text.ends_with('\\'),
                        "sub {i} missing wrap glyph: {:?}",
                        lines[i].text
                    );
                }
                // Sub 4 is last → no `\`.
                assert!(!lines[4].text.ends_with('\\'));
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn body_model_wrap_glyph_survives_full_paint_pipeline() {
        // End-to-end regression for the reported `\ missing on
        // wrapped rows` bug. A 100-char logical line at a realistic
        // editor width should produce `\` at the right edge of each
        // non-last sub-line, visible in the painted byte stream.
        let text: String = (0..100).map(|i| (b'A' + (i % 26) as u8) as char).collect();
        let rope = Arc::new(Rope::from_str(&text));
        let (t, e, s, term) = fixture(
            &[("long.md", 1)],
            Some(1),
            &[("long.md", LoadState::Ready(rope))],
            // rows=8 → body_rows=6; cols=30 → editor_area.cols=30;
            // content_cols=28, wrap_width=27 → sub-lines of 27/27/27/19.
            Some(Dims { cols: 30, rows: 8 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                // Sub 0/1/2 non-last → end in `\`; sub 3 last → no `\`.
                assert!(
                    lines[0].text.ends_with('\\'),
                    "sub 0 missing wrap glyph: {:?}",
                    lines[0].text
                );
                assert!(lines[1].text.ends_with('\\'));
                assert!(lines[2].text.ends_with('\\'));
                assert!(!lines[3].text.ends_with('\\'));
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn body_model_honours_scroll_top_sub_line_on_wrapped_line() {
        // Start scrolled past the first two sub-lines of the same
        // logical line — body must show sub-line 2 onward.
        // body_rows = 4, content_cols = 10, wrap_width = 9.
        let rope = Arc::new(Rope::from_str(
            "abcdefghij0123456789ABCDEFGHIJ!@#$%^&*()qwertyuiop",
        ));
        let (mut t, e, s, term) = fixture(
            &[("wide.rs", 1)],
            Some(1),
            &[("wide.rs", LoadState::Ready(rope))],
            Some(Dims { cols: 12, rows: 6 }),
        );
        t.open[0].cursor = Cursor { line: 0, col: 25, preferred_col: 7 };
        t.open[0].scroll = Scroll { top: 0, top_sub_line: led_core::SubLine(2) };

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, cursor, .. } => {
                assert_eq!(lines[0].text, "  89ABCDEFG\\");
                assert_eq!(lines[1].text, "  HIJ!@#$%^\\");
                assert_eq!(lines[2].text, "  &*()qwert\\");
                assert_eq!(lines[3].text, "  yuiop");
                // Cursor on sub 2 within 7 → body row 0, screen col 2+7=9.
                assert_eq!(*cursor, Some((0, 9)));
            }
            other => panic!("expected Content, got {other:?}"),
        }
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
        t.open[0].scroll = Scroll { top: 0, top_sub_line: led_core::SubLine(0) };

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { cursor, .. } => assert_eq!(*cursor, None),
            other => panic!("expected Content, got {other:?}"),
        }
        assert_eq!(frame.cursor, None);
    }

    #[test]
    fn render_frame_shows_blank_body_when_load_failed() {
        // Legitimate load errors (permission denied, etc.) render as
        // a blank body instead of painting the `io::Error` message
        // inside the editing canvas. Future milestones surface the
        // failure as a status-bar alert; the body stays clean.
        let (t, e, s, term) = fixture(
            &[("bad.rs", 1)],
            Some(1),
            &[("bad.rs", LoadState::Error(Arc::new("No such file".into())))],
            Some(Dims { cols: 80, rows: 24 }),
        );
        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                // Empty-rope body: first row blank, rest tildes.
                assert_eq!(lines[0].text.trim_end(), "");
                assert!(
                    lines[1..].iter().all(|l| l.text.starts_with("~ ")),
                    "error body should paint tildes, not inline the error message",
                );
                // The `io::Error` message must NOT appear anywhere
                // in the body text.
                for l in lines.iter() {
                    assert!(
                        !l.text.contains("No such file"),
                        "body rendered the error message inline: {:?}",
                        l.text,
                    );
                }
            }
            other => panic!("expected Content (blank), got {other:?}"),
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
                version: BufferVersion(1),
                saved_version: SavedVersion(0),
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );

        let frame = render(&t, &e, &s, &term).expect("dims set");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                assert_eq!(lines[0].text, "  edited-version");
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
                assert_eq!(lines[0].text, "  from-disk");
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
                version: BufferVersion(3),
                saved_version: SavedVersion(0),
                disk_content_hash: led_core::PersistedContentHash::default(),
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
                assert_eq!(*version, BufferVersion(3));
            }
            SaveAction::SaveAs { .. } => panic!("unexpected SaveAs"),
        }
    }

    #[test]
    fn file_save_action_emits_save_as_from_pending_map() {
        let mut e = BufferEdits::default();
        let from = canon("a.rs");
        let to = canon("b.rs");
        let rope = Arc::new(Rope::from_str("payload"));
        e.buffers.insert(
            from.clone(),
            EditedBuffer {
                rope: rope.clone(),
                version: BufferVersion(2),
                saved_version: SavedVersion(2), // pristine — SaveAs still fires
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );
        e.pending_save_as.insert(from.clone(), to.clone());

        let actions = file_save_action(
            PendingSavesInput::new(&e),
            EditedBuffersInput::new(&e),
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SaveAction::SaveAs {
                from: f,
                to: t,
                rope: r,
                version,
            } => {
                assert_eq!(f, &from);
                assert_eq!(t, &to);
                assert!(Arc::ptr_eq(r, &rope));
                assert_eq!(*version, BufferVersion(2));
            }
            SaveAction::Save { .. } => panic!("unexpected Save"),
        }
    }

    #[test]
    fn file_save_action_emits_save_for_clean_buffer_too() {
        // "Save should always save": dispatch only inserts a path
        // into `pending_saves` when the user explicitly asks (Save
        // / SaveNoFormat). The query honours that intent and emits
        // a `Save` action even when the buffer is byte-identical
        // to disk — a no-op on disk, but the user's request still
        // round-trips through the file-write driver. SaveAll is
        // the gated path; it filters dirty buffers in
        // `request_save_all` before populating `pending_saves`.
        let mut e = BufferEdits::default();
        let path = canon("clean.rs");
        let rope = Arc::new(Rope::from_str("x"));
        e.buffers.insert(
            path.clone(),
            EditedBuffer {
                rope: rope.clone(),
                version: BufferVersion(0),
                saved_version: SavedVersion(0), // dirty() == false
                disk_content_hash: led_core::EphemeralContentHash::of_rope(&rope).persist(),
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
            SaveAction::Save { path: p, .. } => assert_eq!(p, &path),
            other => panic!("expected SaveAction::Save, got {:?}", other),
        }
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
        let a_rope = Arc::new(Rope::from_str("x"));
        e.buffers.insert(
            canon("a.rs"),
            EditedBuffer {
                rope: a_rope.clone(),
                version: BufferVersion(0),
                saved_version: SavedVersion(0),
                disk_content_hash: led_core::EphemeralContentHash::of_rope(&a_rope).persist(),
                history: Default::default(),
            },
        );
        e.buffers.insert(
            canon("b.rs"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("yy")),
                version: BufferVersion(1),
                saved_version: SavedVersion(0),
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );

        let frame = render(&t, &e, &s, &term).expect("dims set");
        assert_eq!(
            *frame.tab_bar.labels,
            vec![" a.rs".to_string(), "\u{25cf}b.rs".to_string()]
        );
    }

    // ── M19: gutter category ─────────────────────────────────────────────

    #[test]
    fn merged_gutter_picks_git_unstaged() {
        // Bar is git/PR only — LSP severity is rendered separately
        // as the diagnostic dot in gutter col 1, never as the bar.
        use led_core::IssueCategory;
        use led_core::git::LineStatus;
        let statuses = vec![LineStatus {
            category: IssueCategory::Unstaged,
            rows: 0..1,
        }];
        let cat = merged_gutter_category(Some(&statuses), None, 0);
        assert_eq!(cat, Some(IssueCategory::Unstaged));
    }

    #[test]
    fn merged_gutter_falls_back_to_none_without_git_status() {
        // No git line status on the row → no bar, regardless of
        // any LSP severity that may live there.
        assert_eq!(merged_gutter_category(None, None, 0), None);
        let statuses: Vec<led_core::git::LineStatus> = Vec::new();
        assert_eq!(merged_gutter_category(Some(&statuses), None, 0), None);
    }

    #[test]
    fn body_model_paints_git_gutter_on_unstaged_line() {
        // Two-line rope. Line 1 carries a git Unstaged range; the
        // rendered row should carry `gutter_category = Some(Unstaged)`.
        use led_core::git::LineStatus;
        use led_core::IssueCategory;
        let (t, e, s, term) = fixture(
            &[("a.rs", 1)],
            Some(1),
            &[(
                "a.rs",
                LoadState::Ready(Arc::new(Rope::from_str("clean\ndirty"))),
            )],
            Some(Dims { cols: 20, rows: 5 }),
        );
        let path = &t.open[0].path;
        let mut git = led_state_git::GitState::default();
        git.line_statuses.insert(
            path.clone(),
            led_state_git::GitLineStatuses {
                anchor_hash: led_core::PersistedContentHash::default(),
                statuses: Arc::new(vec![LineStatus {
                    category: IssueCategory::Unstaged,
                    rows: 1..2,
                }]),
            },
        );
        let alerts = AlertState::default();
        let browser = BrowserUi {
            visible: false,
            ..Default::default()
        };
        let ff = None;
        let is = None;
        let fsrch = None;
        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        let kbd_macro_default = led_state_kbd_macro::KbdMacroState::default();
        let frame = render_frame(RenderInputs {
            term: TerminalDimsInput::new(&term),
            edits: EditedBuffersInput::new(&e),
            store: StoreLoadedInput::new(&s),
            tabs: TabsActiveInput::new(&t),
            alerts: AlertsInput::new(&alerts),
            browser: BrowserUiInput::new(&browser),
            fs: FsTreeInput::new(&led_state_browser::FsTree::default()),
            overlays: OverlaysInput::new(&ff, &is, &fsrch),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            completions: CompletionsSessionInput::new(
                &led_state_completions::CompletionsState::default(),
            ),
            lsp_extras: LspExtrasOverlayInput::new(
                &led_state_lsp::LspExtrasState::default(),
            ),
            git: GitStateInput::new(&git),
            render_tick: 0,
            kbd_macro: KbdMacroRecordingInput::new(&kbd_macro_default),
        })
        .expect("dims");
        match &frame.body {
            BodyModel::Content { lines, .. } => {
                assert!(lines[0].gutter_category.is_none());
                assert_eq!(
                    lines[1].gutter_category,
                    Some(IssueCategory::Unstaged),
                );
            }
            other => panic!("expected Content, got {other:?}"),
        }
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
                assert_eq!(lines[0].text, "  one");
                assert_eq!(lines[1].text, "  two");
                assert_eq!(lines[2].text, "~ ");
                assert_eq!(lines[3].text, "~ ");
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    // ── M9: status bar model ────────────────────────────────────────────

    fn status(a: &AlertState, t: &Tabs, e: &BufferEdits) -> StatusBarModel {
        let ff = None;
        let is = None;
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        let git = led_state_git::GitState::default();
        let kbd_macro_default = led_state_kbd_macro::KbdMacroState::default();
        status_bar_model(StatusBarInputs {
            alerts: AlertsInput::new(a),
            tabs: TabsActiveInput::new(t),
            edits: EditedBuffersInput::new(e),
            overlays: OverlaysInput::new(&ff, &is, &None),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            lsp_extras: LspExtrasOverlayInput::new(
                &led_state_lsp::LspExtrasState::default(),
            ),
            git: GitStateInput::new(&git),
            render_tick: 0,
            kbd_macro: KbdMacroRecordingInput::new(&kbd_macro_default),
        })
    }

    /// Status-bar model with caller-controlled `KbdMacroState`.
    /// Used by M22 tests to check the macro-recording indicator.
    fn status_with_macro(
        a: &AlertState,
        t: &Tabs,
        e: &BufferEdits,
        km: &led_state_kbd_macro::KbdMacroState,
    ) -> StatusBarModel {
        let ff = None;
        let is = None;
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        let git = led_state_git::GitState::default();
        status_bar_model(StatusBarInputs {
            alerts: AlertsInput::new(a),
            tabs: TabsActiveInput::new(t),
            edits: EditedBuffersInput::new(e),
            overlays: OverlaysInput::new(&ff, &is, &None),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            lsp_extras: LspExtrasOverlayInput::new(
                &led_state_lsp::LspExtrasState::default(),
            ),
            git: GitStateInput::new(&git),
            render_tick: 0,
            kbd_macro: KbdMacroRecordingInput::new(km),
        })
    }

    fn status_with_git(
        a: &AlertState,
        t: &Tabs,
        e: &BufferEdits,
        g: &led_state_git::GitState,
    ) -> StatusBarModel {
        let ff = None;
        let is = None;
        let diags = DiagnosticsStates::default();
        let lsp = LspStatuses::default();
        let kbd_macro_default = led_state_kbd_macro::KbdMacroState::default();
        status_bar_model(StatusBarInputs {
            alerts: AlertsInput::new(a),
            tabs: TabsActiveInput::new(t),
            edits: EditedBuffersInput::new(e),
            overlays: OverlaysInput::new(&ff, &is, &None),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            lsp: LspStatusesInput::new(&lsp),
            lsp_extras: LspExtrasOverlayInput::new(
                &led_state_lsp::LspExtrasState::default(),
            ),
            git: GitStateInput::new(g),
            render_tick: 0,
            kbd_macro: KbdMacroRecordingInput::new(&kbd_macro_default),
        })
    }

    #[test]
    fn status_bar_default_empty_when_no_tab() {
        // Legacy shape: ` {branch}{modified}{pr}{lsp}` → always
        // has the one leading space, even when every dynamic
        // piece is empty. The right-side position string falls
        // back to `L1:C1 ` when no tab is active so the post-kill
        // status bar still anchors a position (matches legacy
        // `display.rs` reading the zero-init cursor row/col).
        let s = status(&AlertState::default(), &Tabs::default(), &BufferEdits::default());
        assert_eq!(&*s.left, " ");
        assert_eq!(&*s.right, "L1:C1 ");
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
        // Leading space from legacy's ` {modified}{lsp}` format
        // prefix, even with nothing in the dynamic slots.
        assert_eq!(&*s.left, " ");
        assert_eq!(&*s.right, "L1:C1 ");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_default_with_branch_prepends_name() {
        // M19: a live workspace branch shows as ` main …`.
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            cursor: Cursor { line: 0, col: 0, preferred_col: 0 },
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let git = led_state_git::GitState {
            branch: Some("main".into()),
            ..Default::default()
        };
        let s = status_with_git(
            &AlertState::default(),
            &tabs,
            &BufferEdits::default(),
            &git,
        );
        // Legacy shape: ` {branch}{modified}{pr}{lsp}` — leading
        // space, then " main", nothing further.
        assert_eq!(&*s.left, "  main");
    }

    #[test]
    fn status_bar_default_with_branch_and_dirty() {
        // Dirty buffer + branch.
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            cursor: Cursor { line: 0, col: 0, preferred_col: 0 },
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a.rs"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("x")),
                version: BufferVersion(2),
                saved_version: SavedVersion(1),
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );
        let git = led_state_git::GitState {
            branch: Some("feature/xyz".into()),
            ..Default::default()
        };
        let s = status_with_git(&AlertState::default(), &tabs, &edits, &git);
        // ` ` + ` feature/xyz` + ` ●`.
        assert_eq!(&*s.left, "  feature/xyz \u{25cf}");
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
                version: BufferVersion(3),
                saved_version: SavedVersion(1), // dirty
                disk_content_hash: led_core::PersistedContentHash::default(),
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

    // ── M22 — macro-recording indicator ──────────────────────────────

    #[test]
    fn status_bar_recording_indicator_replaces_default_left() {
        // Per `ui-chrome.md`: while `kbd_macro.recording` is true,
        // the default-left content is replaced by the fixed
        // " Defining kbd macro..." string. Position string still
        // shows on the right.
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            cursor: Cursor { line: 4, col: 11, preferred_col: 11 },
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        // Pre-populate `current` to confirm the projection ignores
        // it (only `recording` is exposed).
        let km = led_state_kbd_macro::KbdMacroState {
            recording: true,
            current: vec![led_core::Command::CursorDown],
            ..Default::default()
        };
        let s = status_with_macro(
            &AlertState::default(),
            &tabs,
            &BufferEdits::default(),
            &km,
        );
        assert_eq!(&*s.left, " Defining kbd macro...");
        assert_eq!(&*s.right, "L5:C12 ");
        assert!(!s.is_warn);
    }

    #[test]
    fn status_bar_recording_indicator_yields_to_info_alert() {
        // Info alert (priority 2) wins over the recording
        // indicator (priority 4 default). Until the alert TTL
        // expires the user sees the alert text; after that the
        // persistent indicator takes over again.
        let a = AlertState {
            info: Some("Saved a.rs".into()),
            ..Default::default()
        };
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let km = led_state_kbd_macro::KbdMacroState {
            recording: true,
            ..Default::default()
        };
        let s = status_with_macro(&a, &tabs, &BufferEdits::default(), &km);
        assert_eq!(&*s.left, " Saved a.rs");
    }

    #[test]
    fn status_bar_recording_indicator_off_when_flag_false() {
        // When `recording` is false the default-left
        // composition takes over. With no branch / dirty / lsp
        // segments, that's just the leading space.
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("a.rs"),
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let km = led_state_kbd_macro::KbdMacroState::default();
        let s = status_with_macro(
            &AlertState::default(),
            &tabs,
            &BufferEdits::default(),
            &km,
        );
        assert_eq!(&*s.left, " ");
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

    // ── file-search side-panel scroll-follow ───────────────────────

    fn fs_group(
        relative: &str,
        hits: usize,
    ) -> led_state_file_search::FileSearchGroup {
        let path = canon(relative);
        let hits = (1..=hits)
            .map(|i| led_state_file_search::FileSearchHit {
                path: path.clone(),
                line: i,
                col: 1,
                preview: format!("hit {i}"),
                match_start: 0,
                match_end: 0,
            })
            .collect();
        led_state_file_search::FileSearchGroup {
            path,
            relative: relative.into(),
            hits,
        }
    }

    fn fs_state_with_results(
        groups: Vec<led_state_file_search::FileSearchGroup>,
        selection: led_state_file_search::FileSearchSelection,
    ) -> led_state_file_search::FileSearchState {
        let flat: Vec<_> = groups.iter().flat_map(|g| g.hits.iter().cloned()).collect();
        let mut query = led_core::TextInput::default();
        query.set("needle");
        led_state_file_search::FileSearchState {
            query,
            results: groups,
            flat_hits: flat,
            selection,
            ..Default::default()
        }
    }

    #[test]
    fn body_model_carries_match_highlight_when_preview_hit_is_on_active_tab() {
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let path = canon("a.rs");
        let rope = Arc::new(Rope::from_str("line zero\nline one\n    foo here\nlast\n"));

        // Active tab is a.rs, scrolled to line 2 ("    foo here").
        let tab = Tab {
            id: TabId(1),
            path: path.clone(),
            cursor: Cursor::default(),
            scroll: Scroll { top: 2, top_sub_line: led_core::SubLine(0) },
            ..Default::default()
        };
        let mut t = Tabs::default();
        t.open.push_back(tab);
        t.active = Some(TabId(1));

        let mut e = BufferEdits::default();
        e.buffers.insert(
            path.clone(),
            led_state_buffer_edits::EditedBuffer::fresh(rope.clone()),
        );
        let s = BufferStore::default();

        // File-search overlay with selection on the hit: line 3
        // (1-indexed), "foo" at col 5 (0-indexed char 4), match
        // len 3.
        let hit = FileSearchHit {
            path: path.clone(),
            line: 3,
            col: 5,
            preview: "    foo here".into(),
            match_start: 4,
            match_end: 7,
        };
        let fs = Some(led_state_file_search::FileSearchState {
            results: vec![FileSearchGroup {
                path: path.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            selection: led_state_file_search::FileSearchSelection::Result(0),
            ..Default::default()
        });
        let is = None;

        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let model = body_model(BodyInputs {
            edits: EditedBuffersInput::new(&e),
            store: StoreLoadedInput::new(&s),
            tabs: TabsActiveInput::new(&t),
            overlays: OverlaysInput::new(&None, &is, &fs),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            git: GitStateInput::new(&led_state_git::GitState::default()),
            area: Rect { x: 0, y: 0, cols: 40, rows: 5 },
        });
        match model {
            BodyModel::Content {
                match_highlight: Some(mh),
                ..
            } => {
                // Scroll.top = 2, hit line = 2 → body row 0.
                assert_eq!(mh.row, 0);
                // col_start = 4 + GUTTER_WIDTH(2) = 6.
                assert_eq!(mh.col_start, 6);
                // col_end = 7 + GUTTER_WIDTH = 9.
                assert_eq!(mh.col_end, 9);
            }
            other => panic!("expected Content with highlight, got {other:?}"),
        }
    }

    #[test]
    fn body_model_has_no_highlight_when_active_tab_differs_from_hit() {
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let a = canon("a.rs");
        let b = canon("b.rs");
        let rope = Arc::new(Rope::from_str("text\n"));

        let mut t = Tabs::default();
        t.open.push_back(Tab {
            id: TabId(1),
            path: b.clone(), // active tab = b.rs
            ..Default::default()
        });
        t.active = Some(TabId(1));

        let mut e = BufferEdits::default();
        e.buffers.insert(
            b.clone(),
            led_state_buffer_edits::EditedBuffer::fresh(rope),
        );
        let s = BufferStore::default();

        // Hit lives on a.rs — should NOT paint a highlight on
        // b.rs's body.
        let hit = FileSearchHit {
            path: a.clone(),
            line: 1,
            col: 1,
            preview: "text".into(),
            match_start: 0,
            match_end: 4,
        };
        let fs = Some(led_state_file_search::FileSearchState {
            results: vec![FileSearchGroup {
                path: a.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            selection: led_state_file_search::FileSearchSelection::Result(0),
            ..Default::default()
        });
        let is = None;

        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let model = body_model(BodyInputs {
            edits: EditedBuffersInput::new(&e),
            store: StoreLoadedInput::new(&s),
            tabs: TabsActiveInput::new(&t),
            overlays: OverlaysInput::new(&None, &is, &fs),
            syntax: SyntaxStatesInput::new(&syntax),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            git: GitStateInput::new(&led_state_git::GitState::default()),
            area: Rect { x: 0, y: 0, cols: 40, rows: 5 },
        });
        match model {
            BodyModel::Content { match_highlight, .. } => {
                assert_eq!(match_highlight, None);
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn file_search_sidebar_renders_from_explicit_scroll_offset() {
        // `scroll_offset` is maintained by dispatch; the renderer
        // just trusts it. Scroll=4 means tree rendering starts at
        // stream row 4 (hits 4 + 5 of a 6-hit group).
        let state = fs_state_with_results(
            vec![fs_group("a.rs", 6)],
            led_state_file_search::FileSearchSelection::Result(4),
        );
        let mut state = state;
        state.scroll_offset = 4;
        let model = file_search_side_panel(&state, 4);
        let names: Vec<&str> = model.rows.iter().map(|r| &*r.name).collect();
        assert_eq!(
            names,
            vec![" Aa   .*   =>", "needle", "   4: hit 4", "   5: hit 5"],
        );
        assert!(model.rows[3].selected);
    }

    #[test]
    fn trim_preview_centers_match_when_line_overflows_budget() {
        use led_state_file_search::FileSearchHit;
        let path = canon("a.rs");
        // 28-char line, "needle" (6 chars) starts at col 18
        // (char idx 17). With a 12-char budget the centering
        // window picks up `needle` plus three chars of context
        // on each side: matches legacy
        // `display.rs::file_search_hit_spans` (`context_before
        // = (avail - match_len) / 2`).
        let hit = FileSearchHit {
            path: path.clone(),
            line: 42,
            col: 18,
            preview: "aaaabbbbccccdddd_needle_xxxx".into(),
            match_start: 17,
            match_end: 23,
        };
        assert_eq!(trim_preview_at_budget(&hit, 12), "dd_needle_xx");
    }

    #[test]
    fn trim_preview_is_a_noop_when_line_fits_in_the_budget() {
        use led_state_file_search::FileSearchHit;
        let path = canon("a.rs");
        // "  needle at start" is 17 chars; with a 24-char
        // budget it fits whole, so the preview is returned
        // untouched (no center, no ellipsis).
        let hit = FileSearchHit {
            path: path.clone(),
            line: 1,
            col: 3,
            preview: "  needle at start".into(),
            match_start: 2,
            match_end: 8,
        };
        assert_eq!(trim_preview_at_budget(&hit, 24), "  needle at start");
    }

    #[test]
    fn hit_row_carries_match_range_covering_the_query() {
        // Short line, match at col 5 for a 3-char query. Row name
        // = "   42: aaaabbb". Prefix = 3 + 2 + 2 = 7 chars. Match
        // starts at char 5-1=4 in the preview (no trim), so
        // match_range = (7+4, 7+4+3) = (11, 14).
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let path = canon("a.rs");
        let hit = FileSearchHit {
            path: path.clone(),
            line: 42,
            col: 5,
            preview: "aaaabbbcccc".into(),
            match_start: 4,
            match_end: 7, // 3-char match ("bbb")
        };
        let state = led_state_file_search::FileSearchState {
            results: vec![FileSearchGroup {
                path: path.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            selection: led_state_file_search::FileSearchSelection::SearchInput,
            ..Default::default()
        };
        let model = file_search_side_panel(&state, 20);
        // Row 0 = header, row 1 = query, row 2 = group header, row 3 = hit.
        let hit_row = &model.rows[3];
        assert_eq!(&*hit_row.name, "   42: aaaabbbcccc");
        assert_eq!(hit_row.match_range, Some((11, 14)));
    }

    #[test]
    fn hit_row_match_range_tracks_through_the_centered_window() {
        // Long line: "aaaabbbbccccdddd_needle_xxxx" — "needle"
        // (6 chars) starts at char 17, col=18 (1-indexed). Side
        // panel content cols = 24, prefix `   1: ` = 6 chars,
        // preview budget = 18. Centering picks the rightmost
        // 18-char window that contains the match: chars[10..28]
        // = "ccdddd_needle_xxxx". Match offset in the window =
        // 17 - 10 = 7, so the row's match_range =
        // (6 + 7, 6 + 7 + 6) = (13, 19), and the chars at
        // that range spell `needle`.
        use led_state_file_search::{FileSearchGroup, FileSearchHit};

        let path = canon("a.rs");
        let hit = FileSearchHit {
            path: path.clone(),
            line: 1,
            col: 18,
            preview: "aaaabbbbccccdddd_needle_xxxx".into(),
            match_start: 17,
            match_end: 23,
        };
        let state = led_state_file_search::FileSearchState {
            results: vec![FileSearchGroup {
                path: path.clone(),
                relative: "a.rs".into(),
                hits: vec![hit.clone()],
            }],
            flat_hits: vec![hit],
            selection: led_state_file_search::FileSearchSelection::SearchInput,
            ..Default::default()
        };
        let model = file_search_side_panel(&state, 20);
        let hit_row = &model.rows[3];
        assert_eq!(hit_row.match_range, Some((13, 19)));
        // The chars at the computed range spell out "needle".
        let chars: Vec<char> = hit_row.name.chars().collect();
        let (s, e) = hit_row.match_range.unwrap();
        let slice: String = chars[s as usize..e as usize].iter().collect();
        assert_eq!(slice, "needle");
    }

    #[test]
    fn trim_preview_handles_multibyte_chars_via_col_count() {
        use led_state_file_search::FileSearchHit;
        let path = canon("a.rs");
        // "🎈🎈🎈🎈🎈 needle" — five balloons (1 char each, 4 bytes
        // each in UTF-8), a space, then "needle" starting at char
        // index 6 (col=7 1-indexed). 12-char preview, 8-char
        // budget → centering window keeps `needle` visible while
        // dropping balloons from the left.
        let hit = FileSearchHit {
            path,
            line: 1,
            col: 7,
            preview: "🎈🎈🎈🎈🎈 needle".into(),
            match_start: "🎈🎈🎈🎈🎈 ".len(),
            match_end: "🎈🎈🎈🎈🎈 needle".len(),
        };
        let trimmed = trim_preview_at_budget(&hit, 8);
        assert!(trimmed.contains("needle"), "got {trimmed:?}");
        assert_eq!(trimmed.chars().count(), 8);
    }

    // ── Syntax span projection ───────────────────────────────────────

    #[test]
    fn tokens_to_line_spans_slices_on_a_single_line() {
        // Rope: "fn main\n"  → line 0 starts at char 0, length 7.
        // Pretend "fn" is a keyword (chars 0..2), "main" is a
        // function (chars 3..7).
        let tokens = vec![
            TokenSpan {
                char_start: 0,
                char_end: 2,
                kind: TokenKind::Keyword,
            },
            TokenSpan {
                char_start: 3,
                char_end: 7,
                kind: TokenKind::Function,
            },
        ];
        let spans = tokens_to_line_spans(&tokens, /* line_char_start */ 0, /* line_char_len */ 7, /* content_cols */ 40);
        // GUTTER_WIDTH is 2 — columns shift right by 2.
        assert_eq!(
            spans,
            vec![
                led_driver_terminal_core::LineSpan {
                    col_start: 2,
                    col_end: 4,
                    kind: TokenKind::Keyword,
                },
                led_driver_terminal_core::LineSpan {
                    col_start: 5,
                    col_end: 9,
                    kind: TokenKind::Function,
                },
            ]
        );
    }

    #[test]
    fn tokens_to_line_spans_clips_spans_crossing_line_boundaries() {
        // Single token `char_start=3, char_end=9`; line starts at 5
        // and has length 10. The [3, 9) overlap with [5, 15) is
        // [5, 9) → rel_start=0, rel_end=4.
        let tokens = vec![TokenSpan {
            char_start: 3,
            char_end: 9,
            kind: TokenKind::String,
        }];
        let spans = tokens_to_line_spans(&tokens, 5, 10, 40);
        assert_eq!(
            spans,
            vec![led_driver_terminal_core::LineSpan {
                col_start: 2,
                col_end: 6,
                kind: TokenKind::String,
            }]
        );
    }

    #[test]
    fn tokens_to_line_spans_drops_default_kind() {
        let tokens = vec![
            TokenSpan {
                char_start: 0,
                char_end: 5,
                kind: TokenKind::Default,
            },
            TokenSpan {
                char_start: 5,
                char_end: 10,
                kind: TokenKind::Keyword,
            },
        ];
        let spans = tokens_to_line_spans(&tokens, 0, 10, 40);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].kind, TokenKind::Keyword);
    }

    #[test]
    fn tokens_to_line_spans_clamps_to_content_cols() {
        // Span extends past the truncated row — clamp col_end.
        let tokens = vec![TokenSpan {
            char_start: 0,
            char_end: 20,
            kind: TokenKind::Comment,
        }];
        // line_char_len = 20 but content_cols = 5 → clip to col 5 + gutter.
        let spans = tokens_to_line_spans(&tokens, 0, 20, 5);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].col_end, (5 + GUTTER_WIDTH) as u16);
    }

    // ── LSP status formatter (matches legacy's
    // `format_lsp_status` at /crates/ui/src/display.rs:803) ──

    #[test]
    fn format_lsp_status_empty_server_returns_empty() {
        assert_eq!(format_lsp_status("", true, Some("indexing"), 0), "");
    }

    #[test]
    fn format_lsp_status_busy_no_detail_shows_spinner_and_name() {
        // Tick=0 → frame 0 = '⠋'. Two leading spaces + spinner + space + name.
        assert_eq!(format_lsp_status("rust-analyzer", true, None, 0), "  ⠋ rust-analyzer");
    }

    #[test]
    fn format_lsp_status_busy_with_detail_has_two_spinners() {
        // Tick=0 → main spinner frame 0 = '⠋'. Detail spinner
        // offset by 5 (≈ 400ms out of phase) → frame 5 = '⠴'.
        // Separator: two spaces between name and detail.
        let s = format_lsp_status("rust-analyzer", true, Some("indexing crates"), 0);
        assert_eq!(s, "  ⠋ rust-analyzer  ⠴ indexing crates");
    }

    #[test]
    fn format_lsp_status_idle_with_detail_omits_spinners() {
        let s = format_lsp_status("rust-analyzer", false, Some("indexing crates"), 0);
        assert_eq!(s, "  rust-analyzer  indexing crates");
    }

    #[test]
    fn format_lsp_status_idle_no_detail_just_name() {
        let s = format_lsp_status("rust-analyzer", false, None, 0);
        assert_eq!(s, "  rust-analyzer");
    }

    #[test]
    fn format_lsp_status_empty_detail_treated_as_none() {
        // Legacy's `.filter(|d| !d.is_empty())` drops empty detail.
        let s = format_lsp_status("rust-analyzer", true, Some(""), 0);
        assert_eq!(s, "  ⠋ rust-analyzer");
    }

    // ── file_categories_map / browser row status ──────────

    #[test]
    fn file_categories_map_emits_lsp_error_and_warning_only() {
        let mut diags = DiagnosticsStates::default();
        let items = vec![
            Diagnostic {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 3,
                severity: DiagnosticSeverity::Error,
                message: String::new(),
                source: None,
                code: None,
            },
            Diagnostic {
                start_line: 1,
                start_col: 0,
                end_line: 1,
                end_col: 3,
                severity: DiagnosticSeverity::Warning,
                message: String::new(),
                source: None,
                code: None,
            },
            Diagnostic {
                start_line: 2,
                start_col: 0,
                end_line: 2,
                end_col: 3,
                severity: DiagnosticSeverity::Info,
                message: String::new(),
                source: None,
                code: None,
            },
            Diagnostic {
                start_line: 3,
                start_col: 0,
                end_line: 3,
                end_col: 3,
                severity: DiagnosticSeverity::Hint,
                message: String::new(),
                source: None,
                code: None,
            },
        ];
        diags.by_path.insert(
            canon("/p/a.rs"),
            BufferDiagnostics::new(
                led_core::PersistedContentHash(1),
                items,
            ),
        );
        let git = led_state_git::GitState::default();
        let map = file_categories_map(
            DiagnosticsStatesInput::new(&diags),
            GitStateInput::new(&git),
        );
        let cats = map.get(&canon("/p/a.rs")).expect("entry");
        assert!(cats.contains(&led_core::IssueCategory::LspError));
        assert!(cats.contains(&led_core::IssueCategory::LspWarning));
        // Info / Hint MUST NOT make it into the map.
        assert_eq!(cats.len(), 2, "only Error + Warning colour the browser");
    }

    #[test]
    fn file_categories_map_empty_when_no_diagnostics() {
        let diags = DiagnosticsStates::default();
        let git = led_state_git::GitState::default();
        let map = file_categories_map(
            DiagnosticsStatesInput::new(&diags),
            GitStateInput::new(&git),
        );
        assert!(map.is_empty());
    }

    #[test]
    fn file_categories_map_merges_git_and_lsp() {
        // Same file carries both an LSP error and a git Unstaged
        // category. `resolve_display` should pick LspError by
        // precedence (LspError < Unstaged in numeric value).
        let p = canon("/p/a.rs");
        let mut diags = DiagnosticsStates::default();
        diags.by_path.insert(
            p.clone(),
            BufferDiagnostics::new(
                led_core::PersistedContentHash(1),
                vec![Diagnostic {
                    start_line: 0,
                    start_col: 0,
                    end_line: 0,
                    end_col: 3,
                    severity: DiagnosticSeverity::Error,
                    message: String::new(),
                    source: None,
                    code: None,
                }],
            ),
        );
        let mut git = led_state_git::GitState::default();
        let mut cats_for_file = imbl::HashSet::default();
        cats_for_file.insert(led_core::IssueCategory::Unstaged);
        git.file_statuses.insert(p.clone(), cats_for_file);

        let map = file_categories_map(
            DiagnosticsStatesInput::new(&diags),
            GitStateInput::new(&git),
        );
        let cats = map.get(&p).expect("merged");
        assert!(cats.contains(&led_core::IssueCategory::LspError));
        assert!(cats.contains(&led_core::IssueCategory::Unstaged));
        // `resolve_display` selects the precedence-winning category
        // (LspError) even though both are present.
        let shown = led_core::resolve_display(cats).expect("some");
        assert_eq!(shown.category, led_core::IssueCategory::LspError);
    }

    #[test]
    fn file_categories_map_includes_git_only_file() {
        // Untracked file with no diagnostics — carries through.
        let p = canon("/p/new.rs");
        let diags = DiagnosticsStates::default();
        let mut git = led_state_git::GitState::default();
        let mut cats_for_file = imbl::HashSet::default();
        cats_for_file.insert(led_core::IssueCategory::Untracked);
        git.file_statuses.insert(p.clone(), cats_for_file);

        let map = file_categories_map(
            DiagnosticsStatesInput::new(&diags),
            GitStateInput::new(&git),
        );
        let cats = map.get(&p).expect("git-only file present");
        assert!(cats.contains(&led_core::IssueCategory::Untracked));
    }

    #[test]
    fn side_panel_row_status_marks_error_file_and_parent_dir() {
        use imbl::Vector;
        use led_state_browser::{BrowserUi, DirEntry, DirEntryKind, FsTree};
        // Tree: /p/sub/err.rs (with LspError), /p/sub/ok.rs (clean).
        let mut fs = FsTree {
            root: Some(canon("/p")),
            ..Default::default()
        };
        let mut root_kids = Vector::new();
        root_kids.push_back(DirEntry {
            name: "sub".into(),
            path: canon("/p/sub"),
            kind: DirEntryKind::Directory,
        });
        fs.dir_contents.insert(canon("/p"), root_kids);
        let mut sub_kids = Vector::new();
        sub_kids.push_back(DirEntry {
            name: "err.rs".into(),
            path: canon("/p/sub/err.rs"),
            kind: DirEntryKind::File,
        });
        sub_kids.push_back(DirEntry {
            name: "ok.rs".into(),
            path: canon("/p/sub/ok.rs"),
            kind: DirEntryKind::File,
        });
        fs.dir_contents.insert(canon("/p/sub"), sub_kids);

        let mut browser = BrowserUi::default();
        browser.expanded_dirs.insert(canon("/p/sub"));

        let mut diags = DiagnosticsStates::default();
        diags.by_path.insert(
            canon("/p/sub/err.rs"),
            BufferDiagnostics::new(
                led_core::PersistedContentHash(1),
                vec![Diagnostic {
                    start_line: 0,
                    start_col: 0,
                    end_line: 0,
                    end_col: 3,
                    severity: DiagnosticSeverity::Error,
                    message: String::new(),
                    source: None,
                    code: None,
                }],
            ),
        );

        let tabs = Tabs::default();
        let ff = None;
        let is = None;
        let fsrch = None;
        let git = led_state_git::GitState::default();
        let edits = led_state_buffer_edits::BufferEdits::default();
        let panel = side_panel_model(SidePanelInputs {
            fs: FsTreeInput::new(&fs),
            browser: BrowserUiInput::new(&browser),
            overlays: OverlaysInput::new(&ff, &is, &fsrch),
            tabs: TabsActiveInput::new(&tabs),
            diagnostics: DiagnosticsStatesInput::new(&diags),
            git: GitStateInput::new(&git),
            edits: EditedBuffersInput::new(&edits),
            rows: 10,
        });
        let rows: &Vec<SidePanelRow> = &panel.rows;
        let by_name = |name: &str| rows.iter().find(|r| &*r.name == name).cloned();
        // `/p/sub` (directory) — aggregates LspError from descendant.
        let sub = by_name("sub").expect("sub row");
        let sub_status = sub.status.expect("sub row inherits descendant error");
        assert_eq!(sub_status.category, led_core::IssueCategory::LspError);
        assert_eq!(sub_status.letter, '\u{2022}'); // directories always bullet
        // `err.rs` (file) — direct LspError, bullet letter (Error
        // has no `browser_letter`).
        let err = by_name("err.rs").expect("err row");
        let err_status = err.status.expect("err file has status");
        assert_eq!(err_status.category, led_core::IssueCategory::LspError);
        assert_eq!(err_status.letter, '\u{2022}');
        // `ok.rs` (file) — no diagnostic → no status.
        let ok = by_name("ok.rs").expect("ok row");
        assert!(ok.status.is_none(), "clean file has no status");
    }

    #[test]
    fn lsp_progress_message_persists_server_name_while_idle() {
        // Regression: an idle server with no progress detail
        // must still surface its name. Legacy shows
        // "  rust-analyzer" on the status bar for the entire
        // lifetime of the server — busy/idle just toggles the
        // spinner and detail *around* the name. A previous
        // incarnation of this function returned `None` here,
        // making "rust-analyzer" disappear the instant
        // indexing finished.
        let mut lsp = LspStatuses::default();
        lsp.by_server.insert(
            "rust-analyzer".into(),
            LspServerStatus {
                busy: false,
                detail: None,
                ready: true,
            },
        );
        let msg = lsp_progress_message(LspStatusesInput::new(&lsp), 0)
            .expect("server visible while idle");
        assert!(msg.contains("rust-analyzer"), "got: {msg:?}");
    }

    #[test]
    fn format_lsp_status_spinner_advances_with_tick() {
        // Each tick bucket (80ms) advances the main spinner by
        // one frame in the 10-frame cycle.
        let t0 = format_lsp_status("ra", true, None, 0);
        let t1 = format_lsp_status("ra", true, None, 1);
        let t2 = format_lsp_status("ra", true, None, 2);
        assert_ne!(t0, t1);
        assert_ne!(t1, t2);
        // After 10 buckets the cycle wraps.
        assert_eq!(t0, format_lsp_status("ra", true, None, 10));
    }

    // ── popover_model ────────────────────────────────────────

    fn popover_fixture(
        cursor_line: usize,
        diag_start_line: usize,
        diag_end_line: usize,
        severity: DiagnosticSeverity,
        message: &str,
        buf_version: BufferVersion,
        // `false` stamps the diagnostic with the buffer's actual
        // content hash (popover-visible); `true` stamps with a
        // deliberately-wrong hash so the no-smear gate hides it.
        diag_hash_mismatches: bool,
    ) -> (
        Tabs,
        BufferEdits,
        BrowserUi,
        DiagnosticsStates,
        Option<led_state_find_file::FindFileState>,
        Option<led_state_isearch::IsearchState>,
        Option<led_state_file_search::FileSearchState>,
    ) {
        let path = canon("a.rs");
        let mut t = Tabs::default();
        t.open.push_back(Tab {
            id: TabId(1),
            path: path.clone(),
            cursor: Cursor {
                line: cursor_line,
                col: 0,
                preferred_col: 0,
            },
            ..Default::default()
        });
        t.active = Some(TabId(1));

        let rope = Arc::new(Rope::from_str("line\n"));
        let buf_hash = led_core::EphemeralContentHash::of_rope(&rope).persist();
        let mut e = BufferEdits::default();
        let mut eb = led_state_buffer_edits::EditedBuffer::fresh(rope);
        eb.version = buf_version;
        e.buffers.insert(path.clone(), eb);

        let browser = BrowserUi {
            visible: false,
            ..Default::default()
        };

        let diag_hash = if diag_hash_mismatches {
            // Force a deliberate mismatch by xor'ing the low bit.
            led_core::PersistedContentHash(buf_hash.0 ^ 1)
        } else {
            buf_hash
        };

        let mut diags = DiagnosticsStates::default();
        diags.by_path.insert(
            path,
            BufferDiagnostics::new(
                diag_hash,
                vec![Diagnostic {
                    start_line: diag_start_line,
                    start_col: 0,
                    end_line: diag_end_line,
                    end_col: 5,
                    severity,
                    message: message.to_string(),
                    source: None,
                    code: None,
                }],
            ),
        );

        (t, e, browser, diags, None, None, None)
    }

    fn call_popover(
        t: &Tabs,
        e: &BufferEdits,
        browser: &BrowserUi,
        diags: &DiagnosticsStates,
        ff: &Option<led_state_find_file::FindFileState>,
        is: &Option<led_state_isearch::IsearchState>,
        fs: &Option<led_state_file_search::FileSearchState>,
    ) -> Option<PopoverModel> {
        popover_model(
            EditedBuffersInput::new(e),
            TabsActiveInput::new(t),
            OverlaysInput::new(ff, is, fs),
            BrowserUiInput::new(browser),
            DiagnosticsStatesInput::new(diags),
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 24,
            },
        )
    }

    #[test]
    fn popover_shows_for_error_on_cursor_row() {
        let (t, e, br, d, ff, is, fs) = popover_fixture(
            3,
            3,
            3,
            DiagnosticSeverity::Error,
            "expected `;`",
            BufferVersion(0),
            false,
        );
        let pop = call_popover(&t, &e, &br, &d, &ff, &is, &fs).expect("popover");
        assert_eq!(pop.lines.len(), 1);
        assert_eq!(&*pop.lines[0].text, "expected `;`");
        assert_eq!(pop.lines[0].severity, Some(PopoverSeverity::Error));
    }

    #[test]
    fn popover_hidden_when_cursor_above_diagnostic_range() {
        let (t, e, br, d, ff, is, fs) = popover_fixture(
            1,
            3,
            3,
            DiagnosticSeverity::Error,
            "x",
            BufferVersion(0),
            false,
        );
        assert!(call_popover(&t, &e, &br, &d, &ff, &is, &fs).is_none());
    }

    #[test]
    fn popover_hidden_when_hash_stale_no_smear() {
        // Diagnostic stamped with a content hash that doesn't
        // match the buffer's current hash — hide rather than
        // show stale.
        let (t, e, br, d, ff, is, fs) = popover_fixture(
            3,
            3,
            3,
            DiagnosticSeverity::Error,
            "x",
            BufferVersion(2),
            true,
        );
        assert!(call_popover(&t, &e, &br, &d, &ff, &is, &fs).is_none());
    }

    #[test]
    fn popover_hidden_for_info_and_hint_severity() {
        for sev in [DiagnosticSeverity::Info, DiagnosticSeverity::Hint] {
            let (t, e, br, d, ff, is, fs) =
                popover_fixture(3, 3, 3, sev, "x", BufferVersion(0), false);
            assert!(
                call_popover(&t, &e, &br, &d, &ff, &is, &fs).is_none(),
                "severity {sev:?} must be silent"
            );
        }
    }

    #[test]
    fn popover_hidden_when_find_file_overlay_active() {
        let (t, e, br, d, _, is, fs) = popover_fixture(
            3,
            3,
            3,
            DiagnosticSeverity::Error,
            "x",
            BufferVersion(0),
            false,
        );
        let ff = Some(led_state_find_file::FindFileState::open(String::new()));
        assert!(call_popover(&t, &e, &br, &d, &ff, &is, &fs).is_none());
    }

    #[test]
    fn popover_hidden_when_browser_focused() {
        let (t, e, mut br, d, ff, is, fs) = popover_fixture(
            3,
            3,
            3,
            DiagnosticSeverity::Error,
            "x",
            BufferVersion(0),
            false,
        );
        br.focus = Focus::Side;
        assert!(call_popover(&t, &e, &br, &d, &ff, &is, &fs).is_none());
    }

    #[test]
    fn popover_wraps_long_message_into_multiple_lines() {
        let msg = "this is a long diagnostic message that should wrap across several lines when rendered in the popover box";
        let (t, e, br, d, ff, is, fs) = popover_fixture(
            3,
            3,
            3,
            DiagnosticSeverity::Error,
            msg,
            BufferVersion(0),
            false,
        );
        let pop = call_popover(&t, &e, &br, &d, &ff, &is, &fs).expect("popover");
        assert!(pop.lines.len() >= 2, "wrap produces multiple lines");
    }

    #[test]
    fn popover_shows_when_cursor_on_middle_of_multiline_diagnostic() {
        // Diagnostic spans rows 3..=5; cursor on row 4 must still
        // produce a popover.
        let (t, e, br, d, ff, is, fs) = popover_fixture(
            4,
            3,
            5,
            DiagnosticSeverity::Warning,
            "spans three lines",
            BufferVersion(0),
            false,
        );
        let pop = call_popover(&t, &e, &br, &d, &ff, &is, &fs).expect("popover");
        assert_eq!(pop.lines[0].severity, Some(PopoverSeverity::Warning));
    }
}
