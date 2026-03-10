use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use led_core::{Context, Effect, Event, TextDoc, Waker};
use notify::{RecursiveMode, Watcher};
use ropey::Rope;

use crate::{Buffer, EditOp, UndoEntry, notify_dir};

enum DiskState {
    Unchanged,
    DeletedNew,
    DeletedAlready,
    ConflictNew,
    ConflictAlready,
    Reloadable,
}

fn classify_disk_state(
    disk_hash: Option<u64>,
    base_hash: u64,
    has_local_changes: bool,
    disk_modified: bool,
    disk_deleted: bool,
) -> DiskState {
    let Some(disk_hash) = disk_hash else {
        // File doesn't exist on disk
        return if disk_deleted {
            DiskState::DeletedAlready
        } else {
            DiskState::DeletedNew
        };
    };

    // Disk hash matches our base — no external change (covers own save)
    if disk_hash == base_hash {
        return DiskState::Unchanged;
    }

    // External change detected
    if has_local_changes {
        if disk_modified {
            DiskState::ConflictAlready
        } else {
            DiskState::ConflictNew
        }
    } else {
        DiskState::Reloadable
    }
}

impl Buffer {
    pub(crate) fn create_watcher(
        source_path: &std::path::Path,
        changed: &Arc<AtomicBool>,
        waker: Option<&Waker>,
    ) -> Option<notify::RecommendedWatcher> {
        let changed = changed.clone();
        let waker = waker.cloned();
        let source_file = source_path.to_path_buf();
        let source_parent = source_path.parent()?.to_path_buf();
        let notify_hash = Self::notify_hash_for_path(source_path);
        let notify_dir = notify_dir();
        let notify_dir_for_closure = notify_dir.clone();

        let mut watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                let Ok(ev) = res else { return };
                match ev.kind {
                    notify::EventKind::Create(_)
                    | notify::EventKind::Modify(_)
                    | notify::EventKind::Remove(_) => {}
                    _ => return,
                }
                let dominated = ev.paths.iter().any(|p| {
                    if p == &source_file {
                        return true;
                    }
                    if let Some(ref nd) = notify_dir_for_closure {
                        if *p == nd.join(&notify_hash) {
                            return true;
                        }
                    }
                    false
                });
                if dominated {
                    changed.store(true, Ordering::SeqCst);
                    if let Some(ref w) = waker {
                        w();
                    }
                }
            })
            .ok()?;

        watcher
            .watch(&source_parent, RecursiveMode::NonRecursive)
            .ok()?;
        if let Some(ref nd) = notify_dir {
            let _ = std::fs::create_dir_all(nd);
            let _ = watcher.watch(nd, RecursiveMode::NonRecursive);
        }
        Some(watcher)
    }

    fn has_local_changes(&self) -> bool {
        self.dirty
    }

    pub(crate) fn handle_tick(&mut self, doc: &mut TextDoc, ctx: &mut Context) -> Vec<Effect> {
        // Adopt background syntax parsing result
        if self.syntax_ready.swap(false, Ordering::SeqCst) {
            if let Ok(mut guard) = self.pending_syntax.lock() {
                self.syntax = guard.take();
            }
        }

        if !self.changed.swap(false, Ordering::SeqCst) {
            return vec![];
        }

        // Check cross-instance sync first
        self.check_cross_instance_sync(doc, ctx);

        // Check disk state for external modifications
        let Some(ref path) = self.path else {
            return vec![];
        };

        let disk_hash = if path.exists() {
            match File::open(path) {
                Ok(f) => match Rope::from_reader(BufReader::new(f)) {
                    Ok(rope) => Some(Self::hash_rope(&rope)),
                    Err(_) => return vec![],
                },
                Err(_) => return vec![],
            }
        } else {
            None
        };

        let state = classify_disk_state(
            disk_hash,
            self.base_content_hash,
            self.has_local_changes(),
            self.disk_modified,
            self.disk_deleted,
        );

        match state {
            DiskState::Unchanged => vec![],
            DiskState::DeletedNew => {
                self.disk_deleted = true;
                log::warn!("File deleted externally: {}", self.filename());
                vec![Effect::SetMessage(format!(
                    "Warning: {} deleted externally",
                    self.filename()
                ))]
            }
            DiskState::DeletedAlready => vec![],
            DiskState::ConflictNew => {
                self.disk_deleted = false;
                self.disk_modified = true;
                log::warn!(
                    "File changed on disk with unsaved changes: {}",
                    self.filename()
                );
                vec![Effect::SetMessage(format!(
                    "Warning: {} changed on disk (you have unsaved changes)",
                    self.filename()
                ))]
            }
            DiskState::ConflictAlready => {
                self.disk_deleted = false;
                vec![]
            }
            DiskState::Reloadable => {
                log::info!("Reloaded {} (changed on disk)", self.filename());
                self.reload_from_disk(doc);
                self.disk_modified = false;
                self.disk_deleted = false;
                let max_line = doc.line_count().saturating_sub(1);
                if self.cursor_row > max_line {
                    self.cursor_row = max_line;
                }
                self.clamp_cursor_col(&*doc);
                let mut effects = vec![Effect::SetMessage(format!(
                    "Reloaded {} (changed on disk)",
                    self.filename()
                ))];
                if let Some(ref path) = self.path {
                    effects.push(Effect::Emit(Event::FileSaved(path.clone())));
                }
                effects
            }
        }
    }

    fn check_cross_instance_sync(&mut self, doc: &mut TextDoc, ctx: &mut Context) {
        if self.self_notified {
            self.self_notified = false;
            return;
        }
        let Some(conn) = ctx.db else { return };
        let file_str = match self.path {
            Some(ref p) => p.to_string_lossy().into_owned(),
            None => return,
        };
        let root_str = ctx.root.to_string_lossy();

        struct Row {
            chain_id: String,
            content_hash: i64,
            seq: Option<i64>,
            entry_data: Option<Vec<u8>>,
        }
        let rows: Vec<Row> = conn
            .prepare(
                "SELECT s.chain_id, s.content_hash, e.seq, e.entry_data
                 FROM buffer_undo_state s
                 LEFT JOIN undo_entries e
                   ON e.root_path = s.root_path AND e.file_path = s.file_path AND e.seq > ?3
                 WHERE s.root_path = ?1 AND s.file_path = ?2
                 ORDER BY e.seq",
            )
            .and_then(|mut stmt| {
                let mapped = stmt.query_map(
                    rusqlite::params![&*root_str, &*file_str, self.last_seen_seq],
                    |row| {
                        Ok(Row {
                            chain_id: row.get(0)?,
                            content_hash: row.get(1)?,
                            seq: row.get(2)?,
                            entry_data: row.get(3)?,
                        })
                    },
                )?;
                Ok(mapped.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();

        if rows.is_empty() {
            if self.dirty {
                self.reload_from_disk(doc);
                self.mark_externally_saved(doc);
            }
            return;
        }

        let remote_chain_id = &rows[0].chain_id;
        let remote_content_hash = rows[0].content_hash as u64;
        let same_chain = self.chain_id.as_deref() == Some(remote_chain_id);

        if same_chain {
            let mut entries = Vec::new();
            let mut max_seq = self.last_seen_seq;
            for row in &rows {
                if let (Some(seq), Some(data)) = (row.seq, &row.entry_data) {
                    if let Ok(entry) = rmp_serde::from_slice::<UndoEntry>(data) {
                        entries.push(entry);
                        max_seq = max_seq.max(seq);
                    }
                }
            }
            if !entries.is_empty() {
                self.apply_remote_entries(doc, entries, max_seq);
            }
        } else {
            if self.base_content_hash != remote_content_hash {
                self.reload_from_disk(doc);
            }
            let new_chain = remote_chain_id.clone();
            let all_entries = Self::load_entries_after(conn, &root_str, &file_str, 0);
            let max_seq = all_entries.last().map(|(s, _)| *s).unwrap_or(0);
            let entries: Vec<UndoEntry> = all_entries.into_iter().map(|(_, e)| e).collect();
            if !entries.is_empty() {
                self.apply_remote_entries(doc, entries, max_seq);
            } else {
                self.last_seen_seq = max_seq;
            }
            self.chain_id = Some(new_chain);
        }
    }

    pub(crate) fn notify_hash_for_path(path: &std::path::Path) -> String {
        use std::hash::Hash;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        format!("{:016x}", std::hash::Hasher::finish(&hasher))
    }

    pub(crate) fn touch_notify(&mut self) {
        let Some(ref path) = self.path else { return };
        let Some(dir) = notify_dir() else { return };
        let hash = Self::notify_hash_for_path(path);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(&hash), b"");
        self.self_notified = true;
    }

    pub(crate) fn reload_from_disk(&mut self, doc: &mut TextDoc) {
        let Some(ref path) = self.path else { return };
        let Ok(file) = File::open(path) else { return };
        let Ok(rope) = Rope::from_reader(BufReader::new(file)) else {
            return;
        };
        doc.replace_rope(rope);
        self.syntax = crate::syntax::SyntaxState::from_path_and_rope(path, doc.rope());
        self.base_content_hash = Self::hash_rope(doc.rope());
        self.undo_history.clear();
        self.undo_cursor = None;
        self.pending_group = None;
        self.distance_from_save = 0;
        self.save_history_len = 0;
        self.persisted_undo_len = 0;
        self.dirty = false;
    }

    pub fn save(&mut self, doc: &mut TextDoc, ctx: &Context) -> io::Result<()> {
        self.flush_pending();
        let Some(path) = self.path.clone() else {
            return Err(io::Error::new(io::ErrorKind::Other, "No file path set"));
        };
        // Strip trailing whitespace from each line
        for row in (0..doc.line_count()).rev() {
            let line_len = doc.line_len(row);
            let trimmed = doc.line(row).trim_end().chars().count();
            if trimmed < line_len {
                let start = doc.char_idx(row, trimmed);
                let end = doc.char_idx(row, line_len);
                let se = self.syntax_edit_remove(&*doc, start, end);
                doc.remove(start, end);
                self.apply_syntax_edit(&*doc, se);
            }
        }
        // Clamp cursor in case trailing whitespace was removed from cursor line
        self.clamp_cursor_col(doc);
        // Ensure final newline
        let len = doc.len_chars();
        if len == 0 || doc.char(len - 1) != '\n' {
            let se = self.syntax_edit_insert(&*doc, len, "\n");
            doc.insert_char(len, '\n');
            self.apply_syntax_edit(&*doc, se);
        }
        // Record deferred compound undo for format-on-save.  The entry
        // captures the full transformation (format edits + trailing-whitespace
        // strip + final newline) as a single undo step.
        if let Some((before_text, cursor_before)) = self.pre_format_snapshot.take() {
            let after_text: String = doc.to_string();
            let cursor_after = (self.cursor_row, self.cursor_col);
            // Anchor (d=1) at lower index, continuation (d=0) at higher index.
            // See apply_text_edits() for detailed explanation of the layout.
            self.undo_history.push(UndoEntry {
                op: EditOp::Remove {
                    char_idx: 0,
                    text: before_text,
                },
                cursor_before,
                cursor_after: cursor_before,
                direction: 1,
            });
            self.undo_history.push(UndoEntry {
                op: EditOp::Insert {
                    char_idx: 0,
                    text: after_text,
                },
                cursor_before,
                cursor_after,
                direction: 0,
            });
            self.undo_cursor = None;
        }
        let file = File::create(&path)?;
        doc.write_to(BufWriter::new(file))?;
        // Drain pending changes so the whitespace/newline mutations above
        // aren't sent as didChange later — didSave with text handles it.
        doc.drain_changes();
        self.dirty = false;
        self.distance_from_save = 0;
        self.save_history_len = self.undo_history.len();
        self.persisted_undo_len = self.save_history_len;
        self.base_content_hash = Self::hash_rope(doc.rope());
        self.chain_id = None;
        self.last_seen_seq = 0;
        self.disk_modified = false;
        self.disk_deleted = false;

        if let Some(conn) = ctx.db {
            let root_str = ctx.root.to_string_lossy();
            let file_str = path.to_string_lossy();
            let _ = conn.execute(
                "DELETE FROM buffer_undo_state WHERE root_path = ?1 AND file_path = ?2",
                rusqlite::params![&*root_str, &*file_str],
            );
        }
        self.touch_notify();

        Ok(())
    }
}
