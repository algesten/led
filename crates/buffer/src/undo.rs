use std::time::Instant;

use led_core::{Context, TextDoc};

use crate::{Buffer, EditKind, EditOp, GROUP_TIMEOUT_MS, PendingGroup, UndoEntry};

impl Buffer {
    pub fn undo(&mut self, doc: &mut TextDoc) {
        self.flush_pending();

        if self.undo_cursor.is_none() {
            self.undo_cursor = Some(self.undo_history.len());
        }

        let pos = self.undo_cursor.unwrap();
        if pos == 0 {
            return;
        }

        let entry = self.undo_history[pos - 1].clone();
        let inverse = self.invert_entry(&entry);

        let se = self.syntax_edit_for_op(&*doc, &inverse.op);
        self.apply_op(doc, &inverse.op);
        self.apply_syntax_edit(&*doc, se);
        self.cursor_row = inverse.cursor_after.0;
        self.cursor_col = inverse.cursor_after.1;
        self.distance_from_save -= entry.direction;
        self.dirty = self.distance_from_save != 0;

        self.undo_history.push(inverse);
        self.undo_cursor = Some(pos - 1);
    }

    fn invert_entry(&self, entry: &UndoEntry) -> UndoEntry {
        let inv_op = match &entry.op {
            EditOp::Insert { char_idx, text } => EditOp::Remove {
                char_idx: *char_idx,
                text: text.clone(),
            },
            EditOp::Remove { char_idx, text } => EditOp::Insert {
                char_idx: *char_idx,
                text: text.clone(),
            },
        };
        UndoEntry {
            op: inv_op,
            cursor_before: entry.cursor_after,
            cursor_after: entry.cursor_before,
            direction: -entry.direction,
        }
    }

    pub(crate) fn apply_op(&mut self, doc: &mut TextDoc, op: &EditOp) {
        match op {
            EditOp::Insert { char_idx, text } => {
                doc.insert(*char_idx, text);
            }
            EditOp::Remove { char_idx, text } => {
                let end = *char_idx + text.chars().count();
                doc.remove(*char_idx, end);
            }
        }
    }

    // --- Undo grouping ---

    pub(crate) fn record_edit(
        &mut self,
        kind: EditKind,
        op: EditOp,
        cursor_before: (usize, usize),
        cursor_after: (usize, usize),
    ) {
        let now = Instant::now();

        if let Some(ref mut pg) = self.pending_group {
            let elapsed = now.duration_since(pg.last_time).as_millis();
            if pg.kind == kind && elapsed < GROUP_TIMEOUT_MS {
                match (&mut pg.op, &op) {
                    (EditOp::Insert { text: acc, .. }, EditOp::Insert { text: new, .. }) => {
                        acc.push_str(new);
                    }
                    (
                        EditOp::Remove {
                            char_idx: acc_idx,
                            text: acc,
                        },
                        EditOp::Remove {
                            char_idx: new_idx,
                            text: new,
                        },
                    ) => {
                        if kind == EditKind::DeleteBackward {
                            acc.insert_str(0, new);
                            *acc_idx = *new_idx;
                        } else {
                            acc.push_str(new);
                        }
                    }
                    _ => {
                        self.flush_pending_inner();
                        self.pending_group = Some(PendingGroup {
                            kind,
                            op,
                            cursor_before,
                            cursor_after,
                            last_time: now,
                        });
                        return;
                    }
                }
                pg.cursor_after = cursor_after;
                pg.last_time = now;
                return;
            }
        }

        self.flush_pending();
        self.pending_group = Some(PendingGroup {
            kind,
            op,
            cursor_before,
            cursor_after,
            last_time: now,
        });
    }

    pub(crate) fn flush_pending(&mut self) {
        self.flush_pending_inner();
    }

    fn flush_pending_inner(&mut self) {
        if let Some(pg) = self.pending_group.take() {
            self.distance_from_save += 1;
            self.undo_history.push(UndoEntry {
                op: pg.op,
                cursor_before: pg.cursor_before,
                cursor_after: pg.cursor_after,
                direction: 1,
            });
            self.undo_cursor = None;
        }
    }

    pub(crate) fn push_undo(&mut self, entry: UndoEntry) {
        self.distance_from_save += 1;
        self.undo_history.push(entry);
        self.undo_cursor = None;
    }

    pub(crate) fn break_undo_chain(&mut self) {
        self.flush_pending();
        self.undo_cursor = None;
    }

    // --- Undo persistence ---

    pub(crate) fn has_unpersisted_undo(&self) -> bool {
        self.pending_group.is_some() || self.undo_history.len() > self.persisted_undo_len
    }

    pub(crate) fn flush_undo_to_db(&mut self, ctx: &Context) {
        let Some(conn) = ctx.db else { return };
        let file_str = match self.path {
            Some(ref p) => p.to_string_lossy().into_owned(),
            None => return,
        };
        let root_str = ctx.root.to_string_lossy();

        self.flush_pending();
        let start = self.persisted_undo_len;
        if start >= self.undo_history.len() {
            return;
        }

        if self.chain_id.is_none() {
            self.chain_id = Some(Self::generate_chain_id());
        }
        let chain_id = self.chain_id.as_ref().unwrap();

        let entries: Vec<Vec<u8>> = self.undo_history[start..]
            .iter()
            .map(|entry| rmp_serde::to_vec(entry).expect("serialize undo entry"))
            .collect();
        self.persisted_undo_len = self.undo_history.len();

        let result: rusqlite::Result<()> = (|| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO buffer_undo_state (root_path, file_path, chain_id, content_hash, undo_cursor, distance_from_save)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(root_path, file_path) DO UPDATE SET
                    chain_id = excluded.chain_id,
                    content_hash = excluded.content_hash,
                    undo_cursor = excluded.undo_cursor,
                    distance_from_save = excluded.distance_from_save",
                rusqlite::params![
                    &*root_str, &*file_str, chain_id,
                    self.base_content_hash as i64,
                    self.undo_cursor.map(|v| v as i64),
                    self.distance_from_save
                ],
            )?;
            for data in &entries {
                tx.execute(
                    "INSERT INTO undo_entries (root_path, file_path, entry_data)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![&*root_str, &*file_str, data],
                )?;
            }
            tx.commit()?;
            Ok(())
        })();

        if let Err(e) = result {
            log::warn!("failed to flush undo entries: {e}");
        }

        if let Ok(max_seq) = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM undo_entries WHERE root_path = ?1 AND file_path = ?2",
            rusqlite::params![&*root_str, &*file_str],
            |row| row.get::<_, i64>(0),
        ) {
            self.last_seen_seq = max_seq;
        }

        self.touch_notify();
    }

    pub(crate) fn mark_externally_saved(&mut self, doc: &TextDoc) {
        self.dirty = false;
        self.distance_from_save = 0;
        self.base_content_hash = Self::hash_rope(doc.rope());
        self.undo_history.clear();
        self.undo_cursor = None;
        self.save_history_len = 0;
        self.persisted_undo_len = 0;
        self.chain_id = None;
        self.last_seen_seq = 0;
    }

    pub(crate) fn apply_remote_entries(
        &mut self,
        doc: &mut TextDoc,
        entries: Vec<UndoEntry>,
        new_last_seen_seq: i64,
    ) {
        self.flush_pending();
        for entry in &entries {
            let se = self.syntax_edit_for_op(&*doc, &entry.op);
            self.apply_op(doc, &entry.op);
            self.apply_syntax_edit(&*doc, se);
            self.distance_from_save += entry.direction;
        }
        self.undo_history.extend(entries);
        self.persisted_undo_len = self.undo_history.len();
        self.last_seen_seq = new_last_seen_seq;
        if let Some(last) = self.undo_history.last() {
            self.cursor_row = last.cursor_after.0;
            self.cursor_col = last.cursor_after.1;
        }
        self.dirty = self.distance_from_save != 0;
    }

    pub fn restore_undo(
        &mut self,
        doc: &mut TextDoc,
        entries: Vec<UndoEntry>,
        undo_cursor: Option<usize>,
        distance_from_save: i32,
    ) {
        for entry in &entries {
            let se = self.syntax_edit_for_op(&*doc, &entry.op);
            self.apply_op(doc, &entry.op);
            self.apply_syntax_edit(&*doc, se);
        }
        self.undo_history = entries;
        self.undo_cursor = undo_cursor;
        self.distance_from_save = distance_from_save;
        self.dirty = distance_from_save != 0;
        self.persisted_undo_len = self.undo_history.len();
        self.save_history_len = 0;
        if let Some(last) = self.undo_history.last() {
            self.cursor_row = last.cursor_after.0;
            self.cursor_col = last.cursor_after.1;
        }
    }

    pub(crate) fn load_entries_after(
        conn: &rusqlite::Connection,
        root: &str,
        file: &str,
        after_seq: i64,
    ) -> Vec<(i64, UndoEntry)> {
        conn.prepare(
            "SELECT seq, entry_data FROM undo_entries
             WHERE root_path = ?1 AND file_path = ?2 AND seq > ?3
             ORDER BY seq",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map(rusqlite::params![root, file, after_seq], |row| {
                let seq: i64 = row.get(0)?;
                let data: Vec<u8> = row.get(1)?;
                let entry: UndoEntry = rmp_serde::from_slice(&data).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        Box::new(e),
                    )
                })?;
                Ok((seq, entry))
            })?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default()
    }
}
