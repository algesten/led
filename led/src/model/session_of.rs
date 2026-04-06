use std::collections::{HashMap, HashSet, VecDeque};

use led_core::rx::Stream;
use led_core::{CanonPath, UserPath};
use led_state::JumpPosition;
use led_workspace::{SessionBuffer, WorkspaceIn as WI};

use super::Mut;

pub fn session_of(workspace_in: &Stream<WI>) -> Stream<Mut> {
    workspace_in
        .filter(|ev| matches!(ev, WI::SessionRestored { .. }))
        .map(|ev| {
            let WI::SessionRestored { session } = ev else {
                unreachable!()
            };
            match session {
                Some(session) => {
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

                    let pending_lists: Vec<CanonPath> =
                        browser_expanded_dirs.iter().cloned().collect();

                    Mut::SessionRestored {
                        active_tab_order: Some(session.active_tab_order),
                        show_side_panel: session.show_side_panel,
                        positions,
                        pending_opens: paths,
                        browser_selected,
                        browser_scroll_offset,
                        browser_expanded_dirs,
                        jump_entries,
                        jump_index,
                        pending_lists,
                    }
                }
                None => Mut::SessionRestored {
                    active_tab_order: None,
                    show_side_panel: true,
                    positions: HashMap::new(),
                    pending_opens: Vec::new(),
                    browser_selected: 0,
                    browser_scroll_offset: 0,
                    browser_expanded_dirs: HashSet::new(),
                    jump_entries: VecDeque::new(),
                    jump_index: 0,
                    pending_lists: Vec::new(),
                },
            }
        })
        .stream()
}
