use led_core::{CanonPath, ChainId, PersistedContentHash, UndoDbSeq};
use led_driver_session_core::SyncResultKind;
use led_state_buffer_edits::EditGroup;
use rusqlite::{Connection, params};

/// M26 — cross-instance sync probe.
///
/// Returns one of three [`SyncResultKind`]s, all with the path
/// field left as a placeholder; the caller stamps the actual
/// path via [`attach_sync_path`]. Branches:
///
/// - `buffer_undo_state` row missing → `ExternalSave` (peer
///   saved + cleared its undo).
/// - State row's `chain_id` differs from ours → treat as
///   external (chain-id mismatch means a peer rewrote the
///   timeline). Routed as `SyncEntries { … }` only when at
///   least one row sits past `last_seen_seq`; otherwise
///   `NoChange`.
/// - State row's `chain_id` matches and there are new rows →
///   `SyncEntries`.
/// - State row matches and no new rows → `NoChange` (the
///   common self-echo case).
pub(crate) fn check_sync(
    conn: &Connection,
    root_path: &str,
    file_path: &str,
    last_seen_seq: i64,
    current_chain_id: &str,
) -> rusqlite::Result<SyncResultKind> {
    let placeholder = CanonPath::default();
    // Read the state row (if any).
    let state: Option<(String, i64)> = conn
        .prepare_cached(
            "SELECT chain_id, content_hash FROM buffer_undo_state
             WHERE root_path = ?1 AND file_path = ?2",
        )?
        .query_row(params![root_path, file_path], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(("".to_string(), 0)),
            other => Err(other),
        })
        .map(|t| if t.0.is_empty() { None } else { Some(t) })?;

    // Read entries past last_seen_seq.
    let mut stmt = conn.prepare_cached(
        "SELECT seq, entry_data FROM undo_entries
         WHERE root_path = ?1 AND file_path = ?2 AND seq > ?3
         ORDER BY seq ASC",
    )?;
    let mut rows = stmt.query(params![root_path, file_path, last_seen_seq])?;
    let mut entries: Vec<EditGroup> = Vec::new();
    let mut max_seq: i64 = last_seen_seq;
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        let entry: EditGroup = rmp_serde::from_slice(&bytes).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                bytes.len(),
                rusqlite::types::Type::Blob,
                Box::new(std::io::Error::other(e)),
            )
        })?;
        entries.push(entry);
        if seq > max_seq {
            max_seq = seq;
        }
    }
    drop(rows);
    drop(stmt);

    let kind = match (state, entries.is_empty()) {
        (None, true) => {
            // No state row + no entries — peer saved + cleared.
            SyncResultKind::ExternalSave { path: placeholder }
        }
        (None, false) => {
            // No state row but entries present (race window).
            // Treat as external save; the entries are stale.
            SyncResultKind::ExternalSave { path: placeholder }
        }
        (Some((chain_id, hash)), false) => {
            if chain_id == current_chain_id {
                // Self-echo from our own flush — but new rows
                // means a peer also flushed onto the same chain.
                // Apply.
                SyncResultKind::SyncEntries {
                    path: placeholder,
                    chain_id: ChainId::new(chain_id),
                    content_hash: PersistedContentHash(hash as u64),
                    entries,
                    new_last_seen_seq: UndoDbSeq(max_seq),
                }
            } else {
                // Chain mismatch: peer rewrote the timeline.
                // The runtime synthesises a reread.
                SyncResultKind::ExternalSave { path: placeholder }
            }
        }
        (Some((chain_id, _)), true) => {
            if chain_id == current_chain_id {
                SyncResultKind::NoChange { path: placeholder }
            } else {
                // Chain shifted but no new rows — peer cleared
                // and rewrote nothing. External save.
                SyncResultKind::ExternalSave { path: placeholder }
            }
        }
    };
    Ok(kind)
}
