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

    let show_side_panel_s = session_s
        .map(|sd| Mut::SetShowSidePanel(sd.show_side_panel))
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

    // With pending opens: create tabs/buffers, set phase to Resuming
    let resume_tabs_s = session_s
        .filter(|sd| !sd.pending_opens.is_empty())
        .flat_map(|sd| {
            sd.pending_opens
                .iter()
                .map(|p| Mut::EnsureTab(p.clone(), false))
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

    // Without pending opens: ensure startup arg buffers + resolve focus
    let no_resume_arg_tabs_s = session_s
        .filter(|sd| sd.pending_opens.is_empty())
        .sample_combine(state)
        .flat_map(|(_, s)| {
            s.startup
                .arg_paths
                .iter()
                .map(|p| Mut::EnsureTab(p.clone(), true))
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
    resume_tabs_s.forward(&muts);
    resume_entries_s.forward(&muts);
    resume_phase_s.forward(&muts);
    no_resume_phase_s.forward(&muts);
    no_resume_arg_tabs_s.forward(&muts);
    no_resume_focus_s.forward(&muts);
    no_resume_reveal_s.forward(&muts);
    muts
}
