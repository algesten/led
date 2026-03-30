use led_core::{CharOffset, Doc};
use tree_sitter::{InputEdit, Parser, Point, Tree};

/// Parse document contents using tree-sitter's incremental parsing callback.
pub(crate) fn parse_doc(
    parser: &mut Parser,
    doc: &dyn Doc,
    old_tree: Option<&Tree>,
) -> Option<Tree> {
    #[allow(deprecated)]
    parser.parse_with(
        &mut |byte_offset, _position: Point| -> &[u8] {
            if byte_offset >= doc.len_bytes() {
                return &[];
            }
            let (chunk, chunk_byte_start) = doc.chunk_at_byte(byte_offset);
            let start_within = byte_offset - chunk_byte_start;
            &chunk.as_bytes()[start_within..]
        },
        old_tree,
    )
}

/// Compute byte and Point from a char index.
#[allow(dead_code)]
fn byte_and_point(doc: &dyn Doc, char_idx: usize) -> (usize, Point) {
    let byte = doc.char_to_byte(char_idx);
    let line = doc.char_to_line(CharOffset(char_idx));
    let line_byte = doc.line_to_byte(line);
    (
        byte,
        Point {
            row: line.0,
            column: byte - line_byte,
        },
    )
}

/// Compute InputEdit for an insert operation (call BEFORE mutating the doc).
#[allow(dead_code)]
pub(crate) fn edit_for_insert(doc: &dyn Doc, char_idx: usize, text: &str) -> InputEdit {
    let (start_byte, start_pos) = byte_and_point(doc, char_idx);
    let new_end_byte = start_byte + text.len();
    let mut end_row = start_pos.row;
    let mut end_col = start_pos.column;
    for b in text.bytes() {
        if b == b'\n' {
            end_row += 1;
            end_col = 0;
        } else {
            end_col += 1;
        }
    }
    InputEdit {
        start_byte,
        old_end_byte: start_byte,
        new_end_byte,
        start_position: start_pos,
        old_end_position: start_pos,
        new_end_position: Point {
            row: end_row,
            column: end_col,
        },
    }
}

/// Compute InputEdit for a remove operation (call BEFORE mutating the doc).
#[allow(dead_code)]
pub(crate) fn edit_for_remove(doc: &dyn Doc, char_start: usize, char_end: usize) -> InputEdit {
    let (start_byte, start_pos) = byte_and_point(doc, char_start);
    let (end_byte, end_pos) = byte_and_point(doc, char_end);
    InputEdit {
        start_byte,
        old_end_byte: end_byte,
        new_end_byte: start_byte,
        start_position: start_pos,
        old_end_position: end_pos,
        new_end_position: start_pos,
    }
}

/// Extract text from a tree-sitter node.
pub(crate) fn node_text(doc: &dyn Doc, node: &tree_sitter::Node) -> String {
    let start = CharOffset(doc.byte_to_char(node.start_byte()));
    let end = CharOffset(doc.byte_to_char(node.end_byte().min(doc.len_bytes())));
    doc.slice(start, end)
}

/// Rope-based text provider for tree-sitter query predicate evaluation.
pub(crate) struct DocProvider<'a> {
    pub doc: &'a dyn Doc,
}

impl<'a> tree_sitter::TextProvider<&'a [u8]> for DocProvider<'a> {
    type I = DocChunks<'a>;
    fn text(&mut self, node: tree_sitter::Node) -> Self::I {
        let start = node.start_byte();
        // Clamp end to doc length — stale tree nodes may exceed current doc size
        let end = node.end_byte().min(self.doc.len_bytes());
        DocChunks {
            doc: self.doc,
            byte_offset: start,
            end,
        }
    }
}

pub(crate) struct DocChunks<'a> {
    doc: &'a dyn Doc,
    byte_offset: usize,
    end: usize,
}

impl<'a> DocChunks<'a> {
    #[cfg(test)]
    pub(crate) fn new(doc: &'a dyn Doc, byte_offset: usize, end: usize) -> Self {
        Self {
            doc,
            byte_offset,
            end,
        }
    }
}

impl<'a> Iterator for DocChunks<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        if self.byte_offset >= self.end {
            return None;
        }
        let (chunk, chunk_byte_start) = self.doc.chunk_at_byte(self.byte_offset);
        let start_within = self.byte_offset - chunk_byte_start;
        let available = chunk.len() - start_within;
        let needed = self.end - self.byte_offset;
        let take = available.min(needed);
        // Guard: if the chunk has no bytes available at this offset (e.g. at a
        // chunk boundary where ropey returns the preceding chunk), terminate
        // rather than spinning forever with byte_offset stuck in place.
        if take == 0 {
            return None;
        }
        let slice = &chunk.as_bytes()[start_within..start_within + take];
        self.byte_offset += take;
        Some(slice)
    }
}
