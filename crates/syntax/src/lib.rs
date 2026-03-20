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
        indent_row: Option<usize>,
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
    pub indent_row: Option<usize>,
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
        struct BufSyntax {
            state: SyntaxState,
            last_ver: u64,
            last_scroll: usize,
            last_end_line: usize,
            cached_highlights: Vec<(usize, led_state::HighlightSpan)>,
            cached_brackets: Vec<led_state::BracketPair>,
        }
        let mut states: HashMap<BufferId, BufSyntax> = HashMap::new();

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                SyntaxOut::BufferOpened { buf_id, path, doc } => {
                    if let Some(ss) = SyntaxState::from_path_and_doc(&path, &*doc) {
                        let highlights = ss.highlights_for_lines(&*doc, 0, 50);
                        let state_highlights = to_state_highlights(&highlights);
                        let bracket_pairs = to_state_brackets(&ss, &*doc, 0, 50);
                        let matching = ss.matching_bracket(&*doc, 0, 0);

                        let ver = doc.version();
                        states.insert(
                            buf_id,
                            BufSyntax {
                                state: ss,
                                last_ver: ver,
                                last_scroll: 0,
                                last_end_line: 50,
                                cached_highlights: state_highlights.clone(),
                                cached_brackets: bracket_pairs.clone(),
                            },
                        );

                        let _ = tx
                            .send(SyntaxIn {
                                buf_id,
                                doc_version: ver,
                                highlights: state_highlights,
                                bracket_pairs,
                                matching_bracket: matching,
                                indent: None,
                                indent_row: None,
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
                    cursor_row: _,
                    cursor_col: _,
                    indent_row,
                } => {
                    // Auto-initialize if not yet opened
                    if !states.contains_key(&buf_id) {
                        if let Some(ss) = SyntaxState::from_path_and_doc(&path, &*doc) {
                            states.insert(
                                buf_id,
                                BufSyntax {
                                    state: ss,
                                    last_ver: version,
                                    last_scroll: usize::MAX,
                                    last_end_line: 0,
                                    cached_highlights: Vec::new(),
                                    cached_brackets: Vec::new(),
                                },
                            );
                        } else {
                            if indent_row.is_some() {
                                let _ = tx
                                    .send(SyntaxIn {
                                        buf_id,
                                        doc_version: version,
                                        highlights: vec![],
                                        bracket_pairs: vec![],
                                        matching_bracket: None,
                                        indent: None,
                                        indent_row,
                                    })
                                    .await;
                            }
                            continue;
                        }
                    }
                    let bs = states.get_mut(&buf_id).unwrap();

                    // Update parse tree if doc changed
                    if version != bs.last_ver {
                        let new_op_count = (version - bs.last_ver) as usize;
                        if !edit_ops.is_empty() && edit_ops.len() >= new_op_count {
                            for op in &edit_ops[edit_ops.len() - new_op_count..] {
                                bs.state.apply_edit_op(op, &*doc);
                            }
                        } else {
                            bs.state.reparse(&*doc);
                        }
                        bs.last_ver = version;
                    }

                    // Recompute highlights/brackets only when doc or viewport changed
                    let end_line = (scroll_row + buffer_height + 5).min(doc.line_count());
                    let viewport_changed =
                        scroll_row != bs.last_scroll || end_line != bs.last_end_line;

                    if viewport_changed || bs.cached_highlights.is_empty() {
                        bs.cached_highlights = to_state_highlights(
                            &bs.state.highlights_for_lines(&*doc, scroll_row, end_line),
                        );
                        bs.cached_brackets =
                            to_state_brackets(&bs.state, &*doc, scroll_row, end_line);
                        bs.last_scroll = scroll_row;
                        bs.last_end_line = end_line;
                    }

                    // matching_bracket is computed from cached bracket_pairs
                    // in the model layer — no tree-sitter query needed.

                    let indent = if let Some(row) = indent_row {
                        bs.state.compute_auto_indent(&*doc, row)
                    } else {
                        None
                    };

                    let _ = tx
                        .send(SyntaxIn {
                            buf_id,
                            doc_version: version,
                            highlights: bs.cached_highlights.clone(),
                            bracket_pairs: bs.cached_brackets.clone(),
                            matching_bracket: None,
                            indent,
                            indent_row,
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
