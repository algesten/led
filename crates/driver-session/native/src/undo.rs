use led_core::{ChainId, PersistedContentHash, UndoDbSeq};
use led_state_buffer_edits::EditGroup;
use led_state_session::UndoRestoreData;
use rusqlite::{Connection, params};

// ── Undo flush / clear / load (legacy-exact structure) ───────

/// Bundle of SQL params + entries slice for [`flush_undo`].
/// Carved out so the helper takes a small `&FlushUndoArgs<'_>`
/// instead of an 8-positional-arg list.
pub(crate) struct FlushUndoArgs<'a> {
    pub(crate) root_path: &'a str,
    pub(crate) file_path: &'a str,
    pub(crate) chain_id: &'a str,
    pub(crate) content_hash: PersistedContentHash,
    pub(crate) undo_cursor: usize,
    pub(crate) distance_from_save: i32,
    pub(crate) entries: &'a [EditGroup],
}

pub(crate) fn flush_undo(conn: &Connection, args: &FlushUndoArgs<'_>) -> rusqlite::Result<i64> {
    let tx = conn.unchecked_transaction()?;

    tx.prepare_cached(
        "INSERT OR REPLACE INTO buffer_undo_state
         (root_path, file_path, chain_id, content_hash, undo_cursor, distance_from_save)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        args.root_path,
        args.file_path,
        args.chain_id,
        args.content_hash.0 as i64,
        args.undo_cursor as i64,
        args.distance_from_save,
    ])?;

    let mut stmt = tx.prepare_cached(
        "INSERT INTO undo_entries (root_path, file_path, entry_data) VALUES (?1, ?2, ?3)",
    )?;
    for entry in args.entries {
        let bytes = rmp_serde::to_vec(entry).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
        })?;
        stmt.execute(params![args.root_path, args.file_path, bytes])?;
    }
    drop(stmt);

    let last_seq: i64 = tx
        .prepare_cached(
            "SELECT COALESCE(MAX(seq), 0) FROM undo_entries
             WHERE root_path = ?1 AND file_path = ?2",
        )?
        .query_row(params![args.root_path, args.file_path], |row| row.get(0))?;

    tx.commit()?;
    Ok(last_seq)
}

pub(crate) fn clear_undo(
    conn: &Connection,
    root_path: &str,
    file_path: &str,
) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "DELETE FROM undo_entries WHERE root_path = ?1 AND file_path = ?2",
    )?
    .execute(params![root_path, file_path])?;
    conn.prepare_cached(
        "DELETE FROM buffer_undo_state WHERE root_path = ?1 AND file_path = ?2",
    )?
    .execute(params![root_path, file_path])?;
    Ok(())
}

pub(crate) fn load_undo_all(
    conn: &Connection,
    root_path: &str,
    file_path: &str,
) -> rusqlite::Result<Option<UndoRestoreData>> {
    let state = conn
        .prepare_cached(
            "SELECT chain_id, content_hash, undo_cursor, distance_from_save
             FROM buffer_undo_state WHERE root_path = ?1 AND file_path = ?2",
        )?
        .query_row(params![root_path, file_path], |row| {
            Ok(UndoRestoreData {
                chain_id: ChainId::new(row.get::<_, String>(0)?),
                content_hash: PersistedContentHash(row.get::<_, i64>(1)? as u64),
                undo_cursor: row.get::<_, Option<i64>>(2)?.map(|v| v as usize),
                distance_from_save: row.get(3)?,
                entries: Vec::new(),
                last_seq: UndoDbSeq(0),
            })
        });
    let mut restore = match state {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e),
    };

    let mut stmt = conn.prepare_cached(
        "SELECT seq, entry_data FROM undo_entries
         WHERE root_path = ?1 AND file_path = ?2
         ORDER BY seq",
    )?;
    let rows: Vec<(i64, Vec<u8>)> = stmt
        .query_map(params![root_path, file_path], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    restore.last_seq = UndoDbSeq(rows.last().map(|(seq, _)| *seq).unwrap_or(0));
    restore.entries = rows
        .into_iter()
        .filter_map(|(_, data)| rmp_serde::from_slice::<EditGroup>(&data).ok())
        .collect();
    Ok(Some(restore))
}
