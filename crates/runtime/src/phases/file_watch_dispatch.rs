//! M26 file-watch dispatch + clipboard action + per-tick FlushUndo
//! walk + LSP outbox drains. Order matters for trace reproducibility:
//! clipboard before FlushUndo, FlushUndo before the LSP cmd batch.

use std::time::Duration;

use led_driver_clipboard_core::ClipboardAction;
use led_driver_lsp_core::LspCmd;
use led_driver_session_core::SessionCmd;
use led_state_buffer_edits::EditGroup;

use crate::query::EditedBuffersInput;

use crate::apply::edit::distance_from_save_for;
use crate::apply::fs::diff_watch_actions;
use crate::apply::session::{disk_content_hash_for, new_chain_id};
use crate::phases::TickEnv;
use crate::query::{self, ClipboardStateInput};
use crate::query::clipboard_action;
use crate::{Atoms, LspNotified, UndoFlushDebounce, UndoPersistTracker};
use led_core::UndoDbSeq;

pub(crate) fn run(atoms: &mut Atoms, env: &TickEnv<'_>) {
    let Atoms {
        tabs,
        edits,
        clip,
        clock,
        fs,
        syntax: _,
        completions_pending,
        lsp_extras,
        lsp_pending,
        lsp_notified,
        lsp_requested_state_sum,
        session,
        undo_persistence,
        undo_flush_debounce,
        file_watch,
        watch_id_seq,
        ..
    } = atoms;

    // ── M26 watch-actions diff ──────────────────────────────
    if !env.no_workspace
        && session.init_done
        && let Some(root) = fs.root.as_ref()
        && let Some(notify_dir) = env.resolved_notify_dir.as_ref()
    {
        let desired = query::desired_watches(
            query::FsRootInput::new(fs),
            query::NotifyDirInput::new(env.resolved_notify_dir),
            EditedBuffersInput::new(edits),
        );
        let watch_cmds = diff_watch_actions(
            &desired,
            file_watch,
            watch_id_seq,
            root,
            notify_dir,
        );
        if !watch_cmds.is_empty() {
            env.drivers.file_watch.execute(watch_cmds.iter(), file_watch);
        }
    }

    // ── Clipboard action (must run before FlushUndo) ────────
    let clip_action = clipboard_action(ClipboardStateInput::new(clip));
    match clip_action {
        Some(ClipboardAction::Read) => {
            clip.read_in_flight = true;
            env.drivers.clipboard.execute([&ClipboardAction::Read]);
        }
        Some(ClipboardAction::Write(_)) => {
            let text = clip.pending_write.take().expect("memo agreed write");
            env.drivers.clipboard.execute([&ClipboardAction::Write(text)]);
        }
        None => {}
    }

    // ── Per-tick FlushUndo walk ─────────────────────────────
    let now = clock.now;
    let debounce = Duration::from_millis(200);
    if session.init_done {
        for tab in tabs.open.iter() {
            let path = &tab.path;
            let Some(eb) = edits.buffers.get(path) else {
                continue;
            };
            let current_len = eb.history.past_groups().len();
            let persisted = undo_persistence
                .get(path)
                .map(|t| t.persisted_len)
                .unwrap_or(0);
            if current_len <= persisted {
                continue;
            }
            let tracker = undo_persistence
                .entry(path.clone())
                .or_insert_with(|| UndoPersistTracker {
                    chain_id: new_chain_id(),
                    persisted_len: 0,
                    last_seq: UndoDbSeq(0),
                });
            let needs_window_init = match undo_flush_debounce.get(path) {
                Some(entry) => entry.last_version != eb.version,
                None => true,
            };
            if needs_window_init {
                undo_flush_debounce.insert(
                    path.clone(),
                    UndoFlushDebounce {
                        last_version: eb.version,
                        first_seen: now,
                    },
                );
            }
            let entry = undo_flush_debounce.get(path).expect("just inserted");
            if now < entry.first_seen + debounce {
                continue;
            }
            let new_groups: Vec<EditGroup> = eb
                .history
                .past_groups()[tracker.persisted_len..current_len]
                .to_vec();
            if new_groups.iter().all(|g| g.ops.is_empty()) {
                tracker.persisted_len = current_len;
                undo_flush_debounce.remove(path);
                continue;
            }
            let content_hash = disk_content_hash_for(eb);
            let distance = distance_from_save_for(eb);
            let chain_id = tracker.chain_id.clone();
            env.drivers.session.execute(std::iter::once(
                &SessionCmd::FlushUndo {
                    path: path.clone(),
                    chain_id,
                    content_hash,
                    undo_cursor: current_len,
                    distance_from_save: distance,
                    entries: new_groups,
                },
            ));
            tracker.persisted_len = current_len;
            undo_flush_debounce.remove(path);
        }
    }

    // ── LSP outbox batch ────────────────────────────────────
    let mut lsp_cmds: Vec<LspCmd> = Vec::new();
    let buffer_changed = query::desired_lsp_buffer_changed(
        EditedBuffersInput::new(edits),
        query::LspNotifiedInput::new(lsp_notified),
    );
    for cmd in buffer_changed.iter() {
        if let LspCmd::BufferChanged { path, .. } = cmd
            && let Some(eb) = edits.buffers.get(path)
        {
            lsp_notified.insert(
                path.clone(),
                LspNotified {
                    version: eb.version,
                    saved_version: eb.saved_version,
                },
            );
        }
        lsp_cmds.push(cmd.clone());
    }
    let current_sum = query::buffer_state_sum(EditedBuffersInput::new(edits));
    let should_request_diag =
        !lsp_notified.is_empty() && Some(current_sum) != *lsp_requested_state_sum;
    if should_request_diag {
        lsp_cmds.push(LspCmd::RequestDiagnostics);
        *lsp_requested_state_sum = Some(current_sum);
    }
    for req in completions_pending.pending_requests.drain(..) {
        lsp_cmds.push(LspCmd::RequestCompletion {
            path: req.path,
            seq: req.seq,
            line: req.line,
            col: req.col,
            trigger: req.trigger,
        });
    }
    for resolve in completions_pending.pending_resolves.drain(..) {
        lsp_cmds.push(LspCmd::ResolveCompletion {
            path: resolve.path,
            seq: resolve.seq,
            item: resolve.item,
        });
    }
    for req in lsp_pending.pending_goto.drain(..) {
        lsp_cmds.push(LspCmd::RequestGotoDefinition {
            path: req.path,
            seq: req.seq,
            line: req.line,
            col: req.col,
        });
    }
    for req in lsp_pending.pending_rename.drain(..) {
        lsp_cmds.push(LspCmd::RequestRename {
            path: req.path,
            seq: req.seq,
            line: req.line,
            col: req.col,
            new_name: req.new_name,
        });
    }
    for req in lsp_pending.pending_code_action.drain(..) {
        lsp_cmds.push(LspCmd::RequestCodeAction {
            path: req.path,
            seq: req.seq,
            start_line: req.start_line,
            start_col: req.start_col,
            end_line: req.end_line,
            end_col: req.end_col,
        });
    }
    for req in lsp_pending.pending_code_action_select.drain(..) {
        lsp_cmds.push(LspCmd::SelectCodeAction {
            path: req.path,
            seq: req.seq,
            action: req.action,
        });
    }
    for req in lsp_pending.pending_format.drain(..) {
        lsp_cmds.push(LspCmd::RequestFormat {
            path: req.path,
            seq: req.seq,
        });
    }
    let inlay_requests = query::desired_inlay_hint_requests(
        EditedBuffersInput::new(edits),
        query::LspInlayHintsEnabledInput::new(lsp_extras),
        query::LspInlayHintsRequestedInput::new(lsp_pending),
    );
    if lsp_extras.inlay_hints_enabled {
        for (path, version, start_line, end_line) in inlay_requests.iter() {
            lsp_pending.queue_inlay_hints(
                path.clone(),
                *version,
                *start_line,
                *end_line,
            );
        }
        for req in lsp_pending.pending_inlay_hint.drain(..) {
            lsp_cmds.push(LspCmd::RequestInlayHints {
                path: req.path,
                seq: req.seq,
                version: req.version,
                start_line: req.start_line,
                end_line: req.end_line,
            });
        }
    } else {
        lsp_pending.pending_inlay_hint.clear();
    }
    if !lsp_cmds.is_empty() {
        env.drivers.lsp.execute(lsp_cmds.iter());
    }
}
