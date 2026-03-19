mod bracket;
mod config;
mod highlight;
mod indent;
mod injection;
mod language;
mod outline;
mod parse;
mod state;

pub mod import;

pub use bracket::{BracketMatch, assign_rainbow_depth};
pub use config::{IndentDelta, IndentSuggestion};
pub use highlight::HighlightSpan;
pub use import::ImportItem;
pub use outline::OutlineItem;
pub use state::SyntaxState;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{BufferId, Doc, EditOp};
use led_state::{BracketPair, HighlightSpan as StateHighlightSpan};
use tokio::sync::mpsc;

// ── Driver protocol ──

#[derive(Clone)]
pub enum SyntaxOut {
    BufferOpened {
        buf_id: BufferId,
        path: PathBuf,
        doc: Arc<dyn Doc>,
    },
    BufferChanged {
        buf_id: BufferId,
        path: PathBuf,
        doc: Arc<dyn Doc>,
        version: u64,
        edit_ops: Vec<EditOp>,
        scroll_row: usize,
        buffer_height: usize,
        cursor_row: usize,
        cursor_col: usize,
        needs_indent: bool,
    },
    BufferClosed {
        buf_id: BufferId,
    },
}

#[derive(Clone)]
pub struct SyntaxIn {
    pub buf_id: BufferId,
    pub doc_version: u64,
    pub highlights: Vec<(usize, StateHighlightSpan)>,
    pub bracket_pairs: Vec<BracketPair>,
    pub matching_bracket: Option<(usize, usize)>,
    pub indent: Option<String>,
}

// ── Driver ──

pub fn driver(out: Stream<SyntaxOut>) -> Stream<SyntaxIn> {
    let stream: Stream<SyntaxIn> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<SyntaxOut>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<SyntaxIn>(64);

    // Bridge 1: rx::Stream → channel
    out.on(move |opt: Option<&SyntaxOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    // Async task: handle syntax work
    let tx = result_tx;
    tokio::task::spawn_local(async move {
        let mut states: HashMap<BufferId, (SyntaxState, u64)> = HashMap::new();

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                SyntaxOut::BufferOpened { buf_id, path, doc } => {
                    if let Some(ss) = SyntaxState::from_path_and_doc(&path, &*doc) {
                        // Collect initial highlights for the first screen
                        let highlights = ss.highlights_for_lines(&*doc, 0, 50);
                        let state_highlights = to_state_highlights(&highlights);
                        let bracket_pairs = to_state_brackets(&ss, &*doc, 0, 50);
                        let matching = ss.matching_bracket(&*doc, 0, 0);

                        let ver = doc.version();
                        states.insert(buf_id, (ss, ver));

                        let _ = tx
                            .send(SyntaxIn {
                                buf_id,
                                doc_version: ver,
                                highlights: state_highlights,
                                bracket_pairs,
                                matching_bracket: matching,
                                indent: None,
                            })
                            .await;
                    }
                }
                SyntaxOut::BufferChanged {
                    buf_id,
                    path,
                    doc,
                    version,
                    edit_ops,
                    scroll_row,
                    buffer_height,
                    cursor_row,
                    cursor_col,
                    needs_indent,
                } => {
                    // Auto-initialize if not yet opened
                    if !states.contains_key(&buf_id) {
                        if let Some(ss) = SyntaxState::from_path_and_doc(&path, &*doc) {
                            states.insert(buf_id, (ss, version));
                        } else {
                            continue;
                        }
                    }
                    let (ss, last_ver) = states.get_mut(&buf_id).unwrap();

                    // Update the parse tree to match the new doc.
                    //
                    // Each doc version bump corresponds to exactly one EditOp
                    // pushed to the pending group.  `edit_ops` is the full
                    // pending list (cumulative).  We only need the NEW ops
                    // since `last_ver` — the last `version - last_ver` items.
                    // If the list is shorter (undo-group flush happened in
                    // between), fall back to a full re-parse.
                    if version != *last_ver {
                        let new_op_count = (version - *last_ver) as usize;
                        if !edit_ops.is_empty() && edit_ops.len() >= new_op_count {
                            for op in &edit_ops[edit_ops.len() - new_op_count..] {
                                ss.apply_edit_op(op, &*doc);
                            }
                        } else {
                            ss.reparse(&*doc);
                        }
                        *last_ver = version;
                    }

                    // Collect highlights for visible lines
                    let end_line = (scroll_row + buffer_height + 5).min(doc.line_count());
                    let highlights = ss.highlights_for_lines(&*doc, scroll_row, end_line);
                    let state_highlights = to_state_highlights(&highlights);

                    let bracket_pairs = to_state_brackets(ss, &*doc, scroll_row, end_line);

                    let matching = ss.matching_bracket(&*doc, cursor_row, cursor_col);

                    // Compute indent if needed
                    let indent = if needs_indent {
                        ss.compute_auto_indent(&*doc, cursor_row)
                    } else {
                        None
                    };

                    let _ = tx
                        .send(SyntaxIn {
                            buf_id,
                            doc_version: version,
                            highlights: state_highlights,
                            bracket_pairs,
                            matching_bracket: matching,
                            indent,
                        })
                        .await;
                }
                SyntaxOut::BufferClosed { buf_id } => {
                    states.remove(&buf_id);
                }
            }
        }
    });

    // Bridge 2: channel → rx::Stream
    let s = stream.clone();
    tokio::task::spawn_local(async move {
        while let Some(v) = result_rx.recv().await {
            s.push(v);
        }
    });

    stream
}

fn to_state_highlights(highlights: &[(usize, HighlightSpan)]) -> Vec<(usize, StateHighlightSpan)> {
    highlights
        .iter()
        .map(|(line, span)| {
            (
                *line,
                StateHighlightSpan {
                    char_start: span.char_start,
                    char_end: span.char_end,
                    capture_name: span.capture_name.clone(),
                },
            )
        })
        .collect()
}

fn to_state_brackets(
    ss: &SyntaxState,
    doc: &dyn Doc,
    scroll_row: usize,
    end_line: usize,
) -> Vec<BracketPair> {
    let start_byte = doc.line_to_byte(scroll_row);
    let end_byte = if end_line < doc.line_count() {
        doc.line_to_byte(end_line)
    } else {
        doc.len_bytes()
    };

    let matches = ss.bracket_ranges(doc, start_byte..end_byte);
    let len = doc.len_bytes();
    matches
        .iter()
        .filter(|bm| bm.open_range.start < len && bm.close_range.start < len)
        .map(|bm| {
            let open_byte = bm.open_range.start;
            let open_line = doc.byte_to_line(open_byte);
            let open_char = doc.byte_to_char(open_byte);
            let open_col = open_char - doc.line_to_char(open_line);

            let close_byte = bm.close_range.start;
            let close_line = doc.byte_to_line(close_byte);
            let close_char = doc.byte_to_char(close_byte);
            let close_col = close_char - doc.line_to_char(close_line);

            BracketPair {
                open_line,
                open_col,
                close_line,
                close_col,
                color_index: bm.color_index,
            }
        })
        .collect()
}
