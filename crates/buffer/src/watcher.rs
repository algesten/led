use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use led_core::{Context, Effect, Event, Waker};
use notify::{RecursiveMode, Watcher};
use ropey::Rope;

use crate::{Buffer, UndoEntry, notify_dir};

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

    pub(crate) fn handle_tick(&mut self, ctx: &mut Context) -> Vec<Effect> {
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
        self.check_cross_instance_sync(ctx);

        // Check disk state for external modifications
        let Some(ref path) = self.path else {
            return vec![];
        };

        if !path.exists() {
            if !self.disk_deleted {
                self.disk_deleted = true;
                return vec![Effect::SetMessage(format!(
                    "Warning: {} deleted externally.",
                    self.filename()
                ))];
            }
            return vec![];
        }

        // File exists — read and hash it
        let disk_hash = match File::open(path) {
            Ok(f) => match Rope::from_reader(BufReader::new(f)) {
                Ok(rope) => Self::hash_rope(&rope),
                Err(_) => return vec![],
            },
            Err(_) => return vec![],
        };

        // If disk hash matches our base, no external change (covers own save)
        if disk_hash == self.base_content_hash {
            return vec![];
        }

        // Clear deleted flag since file exists now
        self.disk_deleted = false;

        if self.has_local_changes() {
            // Buffer is dirty — flag conflict, don't reload
            if !self.disk_modified {
                self.disk_modified = true;
                return vec![Effect::SetMessage(format!(
                    "Warning: {} changed on disk (you have unsaved changes).",
                    self.filename()
                ))];
            }
            vec![]
        } else {
            // Buffer is clean — auto-reload
            self.reload_from_disk();
            self.disk_modified = false;
            self.disk_deleted = false;
            let max_line = self.line_count().saturating_sub(1);
            if self.cursor_row > max_line {
                self.cursor_row = max_line;
            }
            self.clamp_cursor_col();
            let mut effects = vec![Effect::SetMessage(format!(
                "Reloaded {} (changed on disk).",
                self.filename()
            ))];
            if let Some(ref path) = self.path {
                effects.push(Effect::Emit(Event::FileSaved(path.clone())));
            }
            effects
        }
    }

    fn check_cross_instance_sync(&mut self, ctx: &mut Context) {
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
                self.reload_from_disk();
                self.mark_externally_saved();
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
                self.apply_remote_entries(entries, max_seq);
            }
        } else {
            if self.base_content_hash != remote_content_hash {
                self.reload_from_disk();
            }
            let new_chain = remote_chain_id.clone();
            let all_entries = Self::load_entries_after(conn, &root_str, &file_str, 0);
            let max_seq = all_entries.last().map(|(s, _)| *s).unwrap_or(0);
            let entries: Vec<UndoEntry> = all_entries.into_iter().map(|(_, e)| e).collect();
            if !entries.is_empty() {
                self.apply_remote_entries(entries, max_seq);
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

    pub(crate) fn reload_from_disk(&mut self) {
        let Some(ref path) = self.path else { return };
        let Ok(file) = File::open(path) else { return };
        let Ok(rope) = Rope::from_reader(BufReader::new(file)) else {
            return;
        };
        self.rope = rope;
        self.syntax = crate::syntax::SyntaxState::from_path_and_rope(path, &self.rope);
        self.base_content_hash = Self::hash_rope(&self.rope);
        self.undo_history.clear();
        self.undo_cursor = None;
        self.pending_group = None;
        self.distance_from_save = 0;
        self.save_history_len = 0;
        self.persisted_undo_len = 0;
        self.dirty = false;
    }

    pub fn save(&mut self, ctx: &Context) -> io::Result<()> {
        self.flush_pending();
        let Some(path) = self.path.clone() else {
            return Err(io::Error::new(io::ErrorKind::Other, "No file path set"));
        };
        let len = self.rope.len_chars();
        if len == 0 || self.rope.char(len - 1) != '\n' {
            let se = self.syntax_edit_insert(len, "\n");
            self.rope.insert_char(len, '\n');
            self.apply_syntax_edit(se);
        }
        let file = File::create(&path)?;
        self.rope.write_to(BufWriter::new(file))?;
        self.dirty = false;
        self.distance_from_save = 0;
        self.save_history_len = self.undo_history.len();
        self.persisted_undo_len = self.save_history_len;
        self.base_content_hash = self.content_hash();
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
