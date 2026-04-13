use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use led_core::rx::Stream;
use led_core::{CanonPath, UserPath};
use led_state::{AppState, JumpPosition, Phase};
use led_workspace::{SessionBuffer, WorkspaceIn as WI};

use super::Mut;

/// Parsed session data — shared by all child streams.
#[derive(Clone)]
struct SessionData {
    active_tab_order: Option<usize>,
    show_side_panel: bool,
    positions: HashMap<CanonPath, SessionBuffer>,
    pending_opens: Vec<CanonPath>,
    browser_selected: usize,
    browser_scroll_offset: usize,
    browser_expanded_dirs: HashSet<CanonPath>,
    jump_entries: VecDeque<JumpPosition>,
    jump_index: usize,
}

fn parse_session(ev: WI) -> SessionData {
    let WI::SessionRestored { session } = ev else {
        unreachable!()
    };
    let Some(session) = session else {
        return SessionData {
            active_tab_order: None,
            show_side_panel: true,
            positions: HashMap::new(),
            pending_opens: Vec::new(),
            browser_selected: 0,
            browser_scroll_offset: 0,
            browser_expanded_dirs: HashSet::new(),
            jump_entries: VecDeque::new(),
            jump_index: 0,
        };
    };

    let paths: Vec<CanonPath> = session
        .buffers
        .iter()
        .map(|b| b.file_path.canonicalize())
        .collect();

    let mut positions: HashMap<CanonPath, SessionBuffer> = HashMap::new();
    for buf in session.buffers {
        positions.insert(buf.file_path.canonicalize(), buf);
    }

    let browser_selected = session
        .kv
        .get("browser.selected")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let browser_scroll_offset = session
        .kv
        .get("browser.scroll_offset")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let browser_expanded_dirs = session
        .kv
        .get("browser.expanded_dirs")
        .map(|v| {
            v.lines()
                .filter(|l| !l.is_empty())
                .map(|l| UserPath::new(l).canonicalize())
                .collect::<HashSet<CanonPath>>()
        })
        .unwrap_or_default();

    let (jump_entries, jump_index) = session
        .kv
        .get("jump_list.entries")
        .and_then(|json| serde_json::from_str::<VecDeque<JumpPosition>>(json).ok())
        .map(|entries| {
            let index = session
                .kv
                .get("jump_list.index")
                .and_then(|v| v.parse().ok())
                .unwrap_or(entries.len());
            (entries, index)
        })
        .unwrap_or_default();

    SessionData {
        active_tab_order: Some(session.active_tab_order),
        show_side_panel: session.show_side_panel,
        positions,
        pending_opens: paths,
        browser_selected,
        browser_scroll_offset,
        browser_expanded_dirs,
        jump_entries,
        jump_index,
    }
}

pub fn session_of(workspace_in: &Stream<WI>, state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    // Common parent: parsed session data
    let session_s: Stream<SessionData> = workspace_in
        .filter(|ev| matches!(ev, WI::SessionRestored { .. }))
        .map(parse_session)
        .stream();

    // Scalar field assignments
    let active_tab_order_s = session_s
        .map(|sd| Mut::SetActiveTabOrder(sd.active_tab_order))
        .stream();

    // Standalone mode initializes `show_side_panel` in `AppState::new`
    // (hidden by default, user-toggleable). Don't let the session-restore
    // default (`true`, from a `None` session) clobber it.
    let show_side_panel_s = session_s
        .sample_combine(state)
        .filter(|(_, s)| !s.startup.no_workspace)
        .map(|(sd, _)| Mut::SetShowSidePanel(sd.show_side_panel))
        .stream();

    let positions_s = session_s
        .map(|sd| Mut::SetSessionPositions(sd.positions))
        .stream();

    let browser_s = session_s
        .map(|sd| Mut::SetBrowserState {
            selected: sd.browser_selected,
            scroll_offset: sd.browser_scroll_offset,
            expanded_dirs: sd.browser_expanded_dirs,
        })
        .stream();

    let jump_s = session_s
        .map(|sd| Mut::SetJumpState {
            entries: sd.jump_entries,
            index: sd.jump_index,
        })
        .stream();

    // Pending dir listings (expanded dirs from session)
    let pending_lists_s = session_s
        .map(|sd| {
            let dirs: Vec<CanonPath> = sd.browser_expanded_dirs.iter().cloned().collect();
            Mut::SetPendingLists(dirs)
        })
        .filter(|m| matches!(m, Mut::SetPendingLists(v) if !v.is_empty()))
        .stream();

    // Standalone mode: no Mut::Workspace ever fires to seed the browser,
    // so kick off an initial listing of `start_dir` here. Without this
    // the sidebar renders blank (browser.root is set by AppState::new
    // but dir_contents is empty until the fs driver responds).
    let standalone_browser_list_s = session_s
        .sample_combine(state)
        .filter(|(_, s)| s.startup.no_workspace)
        .map(|(_, s)| Mut::SetPendingLists(vec![(*s.startup.start_dir).clone()]))
        .stream();

    // With pending opens: create tabs/buffers, set phase to Resuming.
    // Session entries are stored as canonical paths; the symlink chain
    // normally can't be recovered. BUT — if the user re-invoked led
    // with the same symlink arg (e.g. `led ~/.profile`), we can find a
    // matching UserPath in `arg_user_paths` and use it to rebuild the
    // chain. Otherwise fall back to `new_from_canon` (degenerate chain).
    // All decisions stay in the combinator (Principle 1).
    let resume_tabs_s = session_s
        .filter(|sd| !sd.pending_opens.is_empty())
        .sample_combine(state)
        .flat_map(|(sd, s)| {
            sd.pending_opens
                .iter()
                .map(|p| {
                    let buf = s
                        .startup
                        .arg_user_paths
                        .iter()
                        .find(|u| u.canonicalize() == *p)
                        .map(|u| led_state::BufferState::new(u.clone()))
                        .unwrap_or_else(|| led_state::BufferState::new_from_canon(p.clone()));
                    Mut::EnsureTab(std::rc::Rc::new(buf))
                })
                .collect::<Vec<_>>()
        });

    let resume_entries_s = session_s
        .filter(|sd| !sd.pending_opens.is_empty())
        .map(|sd| Mut::SetResumeEntries(sd.pending_opens))
        .stream();

    let resume_phase_s = session_s
        .filter(|sd| !sd.pending_opens.is_empty())
        .map(|_| Mut::SetPhase(Phase::Resuming))
        .stream();

    // Without pending opens: go straight to Running
    let no_resume_phase_s = session_s
        .filter(|sd| sd.pending_opens.is_empty())
        .map(|_| Mut::SetPhase(Phase::Running))
        .stream();

    // Without pending opens: ensure startup arg buffers + resolve focus.
    // Use arg_user_paths so the buffer constructor walks the symlink
    // chain — needed for correct syntax/LSP detection on dotfile
    // symlinks like `~/.profile`.
    let no_resume_arg_tabs_s = session_s
        .filter(|sd| sd.pending_opens.is_empty())
        .sample_combine(state)
        .flat_map(|(_, s)| {
            s.startup
                .arg_user_paths
                .iter()
                .filter(|u| !s.buffers.contains_key(&u.canonicalize()))
                .map(|u| {
                    let buf = led_state::BufferState::new(u.clone()).with_create_if_missing(true);
                    Mut::EnsureTab(std::rc::Rc::new(buf))
                })
                .collect::<Vec<_>>()
        });

    let no_resume_focus_s = session_s
        .filter(|sd| sd.pending_opens.is_empty())
        .sample_combine(state)
        .map(|(_, s)| Mut::SetFocus(super::resolve_focus_slot(&s)))
        .stream();

    let no_resume_reveal_s = session_s
        .filter(|sd| sd.pending_opens.is_empty())
        .sample_combine(state)
        .filter_map(|(_, s)| s.startup.arg_dir.clone())
        .map(Mut::BrowserReveal)
        .stream();

    // Wire all children
    let muts: Stream<Mut> = Stream::new();
    active_tab_order_s.forward(&muts);
    show_side_panel_s.forward(&muts);
    positions_s.forward(&muts);
    browser_s.forward(&muts);
    jump_s.forward(&muts);
    pending_lists_s.forward(&muts);
    standalone_browser_list_s.forward(&muts);
    resume_tabs_s.forward(&muts);
    resume_entries_s.forward(&muts);
    resume_phase_s.forward(&muts);
    no_resume_phase_s.forward(&muts);
    no_resume_arg_tabs_s.forward(&muts);
    no_resume_focus_s.forward(&muts);
    no_resume_reveal_s.forward(&muts);
    muts
}
