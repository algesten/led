mod bracket;
mod config;
mod highlight;
mod indent;
mod injection;
mod language;
mod modeline;
mod outline;
mod parse;
mod state;

pub mod import;

pub use bracket::{BracketMatch, assign_rainbow_depth};
pub use config::{IndentDelta, IndentSuggestion};
pub use import::ImportItem;
pub use led_state::HighlightSpan;
pub use outline::OutlineItem;
pub use state::SyntaxState;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{CanonPath, Col, Doc, DocVersion, EditOp, Row};
use led_state::BracketPair;
use tokio::sync::mpsc;

// ── Driver protocol ──

#[derive(Clone)]
pub enum SyntaxOut {
    BufferChanged {
        path: CanonPath,
        doc: Arc<dyn Doc>,
        version: DocVersion,
        edit_ops: Vec<EditOp>,
        scroll_row: Row,
        buffer_height: usize,
        cursor_row: Row,
        cursor_col: Col,
        indent_row: Option<Row>,
    },
    BufferClosed {
        path: CanonPath,
    },
}

#[derive(Clone)]
pub struct SyntaxIn {
    pub path: CanonPath,
    pub doc_version: DocVersion,
    pub highlights: Rc<Vec<(Row, HighlightSpan)>>,
    pub bracket_pairs: Vec<BracketPair>,
    pub matching_bracket: Option<(Row, Col)>,
    pub indent: Option<String>,
    pub indent_row: Option<Row>,
    /// Characters that trigger re-indentation when typed, as declared by the language.
    pub reindent_chars: Arc<[char]>,
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
            last_ver: DocVersion,
            last_doc: Arc<dyn Doc>,
            last_scroll: Row,
            last_end_line: Row,
            cached_highlights: Rc<Vec<(Row, HighlightSpan)>>,
            cached_brackets: Vec<led_state::BracketPair>,
            reindent_chars: Arc<[char]>,
        }
        let mut states: HashMap<CanonPath, BufSyntax> = HashMap::new();

        // Scratch space for coalescing queued messages per buffer.
        struct Coalesced {
            doc: Arc<dyn Doc>,
            version: DocVersion,
            edit_ops: Vec<EditOp>,
            scroll_row: Row,
            buffer_height: usize,
            indent_row: Option<Row>,
        }

        while let Some(cmd) = cmd_rx.recv().await {
            // Coalesce: drain any queued messages, keeping the latest
            // state per path while merging edit_ops and indent_row.
            let mut pending: HashMap<CanonPath, Coalesced> = HashMap::new();
            let mut closes: Vec<CanonPath> = Vec::new();
            {
                let mut first = Some(cmd);
                loop {
                    let msg = if let Some(m) = first.take() {
                        m
                    } else if let Ok(m) = cmd_rx.try_recv() {
                        m
                    } else {
                        break;
                    };
                    match msg {
                        SyntaxOut::BufferChanged {
                            path,
                            doc,
                            version,
                            edit_ops,
                            scroll_row,
                            buffer_height,
                            indent_row,
                            ..
                        } => {
                            if let Some(existing) = pending.get_mut(&path) {
                                existing.edit_ops.extend(edit_ops);
                                existing.doc = doc;
                                existing.version = version;
                                existing.scroll_row = scroll_row;
                                existing.buffer_height = buffer_height;
                                existing.indent_row = indent_row.or(existing.indent_row);
                            } else {
                                pending.insert(
                                    path,
                                    Coalesced {
                                        doc,
                                        version,
                                        edit_ops,
                                        scroll_row,
                                        buffer_height,
                                        indent_row,
                                    },
                                );
                            }
                        }
                        SyntaxOut::BufferClosed { path } => {
                            pending.remove(&path);
                            closes.push(path);
                        }
                    }
                }
            }

            for path in closes {
                states.remove(&path);
            }

            for (
                path,
                Coalesced {
                    doc,
                    version,
                    edit_ops,
                    scroll_row,
                    buffer_height,
                    indent_row,
                },
            ) in pending
            {
                // Auto-initialize if not yet opened
                if !states.contains_key(&path) {
                    if let Some(ss) = SyntaxState::from_path_and_doc(path.as_path(), &*doc) {
                        let reindent_chars = ss.reindent_chars().clone();
                        states.insert(
                            path.clone(),
                            BufSyntax {
                                state: ss,
                                last_ver: version,
                                last_doc: doc.clone(),
                                last_scroll: Row(usize::MAX),
                                last_end_line: Row(0),
                                cached_highlights: Rc::new(Vec::new()),
                                cached_brackets: Vec::new(),
                                reindent_chars,
                            },
                        );
                    } else {
                        if indent_row.is_some() {
                            let _ = tx
                                .send(SyntaxIn {
                                    path,
                                    doc_version: version,
                                    highlights: Rc::new(vec![]),
                                    bracket_pairs: vec![],
                                    matching_bracket: None,
                                    indent: None,
                                    indent_row,
                                    reindent_chars: Arc::from([]),
                                })
                                .await;
                        }
                        continue;
                    }
                }
                let bs = states.get_mut(&path).unwrap();

                // Update parse tree if doc changed
                let doc_changed = version != bs.last_ver;
                if doc_changed {
                    let new_op_count = (*version - *bs.last_ver) as usize;
                    if !edit_ops.is_empty() && edit_ops.len() >= new_op_count {
                        let ops = &edit_ops[edit_ops.len() - new_op_count..];
                        if new_op_count == 1 {
                            bs.state.apply_edit_op(&ops[0], &*doc);
                        } else {
                            // Replay ops: mark each edit using the pre-edit
                            // doc for correct byte positions, then reparse
                            // once with the final doc.
                            let mut shadow: Arc<dyn Doc> = bs.last_doc.clone();
                            for op in ops {
                                bs.state.mark_edit(op, &*shadow);
                                // Advance shadow to match post-edit state.
                                let off = led_core::CharOffset(
                                    op.offset.0.min(shadow.byte_to_char(shadow.len_bytes())),
                                );
                                if !op.old_text.is_empty() {
                                    let end =
                                        led_core::CharOffset(off.0 + op.old_text.chars().count());
                                    shadow = shadow.remove(off, end);
                                }
                                if !op.new_text.is_empty() {
                                    shadow = shadow.insert(off, &op.new_text);
                                }
                            }
                            bs.state.finish_edits(&*doc);
                        }
                    } else {
                        bs.state.reparse(&*doc);
                    }
                    bs.last_ver = version;
                    bs.last_doc = doc.clone();
                }

                // Recompute highlights/brackets when doc or viewport changed
                let end_line = Row((*scroll_row + buffer_height + 5).min(doc.line_count()));
                let viewport_changed = scroll_row != bs.last_scroll || end_line != bs.last_end_line;

                if doc_changed || viewport_changed || bs.cached_highlights.is_empty() {
                    bs.cached_highlights =
                        Rc::new(bs.state.highlights_for_lines(&*doc, *scroll_row, *end_line));
                    bs.cached_brackets = to_state_brackets(&bs.state, &*doc, scroll_row, end_line);
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
                        path,
                        doc_version: version,
                        highlights: bs.cached_highlights.clone(),
                        bracket_pairs: bs.cached_brackets.clone(),
                        matching_bracket: None,
                        indent,
                        indent_row,
                        reindent_chars: bs.reindent_chars.clone(),
                    })
                    .await;
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

fn to_state_brackets(
    ss: &SyntaxState,
    doc: &dyn Doc,
    scroll_row: Row,
    end_line: Row,
) -> Vec<BracketPair> {
    let start_byte = doc.line_to_byte(scroll_row);
    let end_byte = if *end_line < doc.line_count() {
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
            let open_col = Col(open_char - doc.line_to_char(open_line).0);

            let close_byte = bm.close_range.start;
            let close_line = doc.byte_to_line(close_byte);
            let close_char = doc.byte_to_char(close_byte);
            let close_col = Col(close_char - doc.line_to_char(close_line).0);

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
