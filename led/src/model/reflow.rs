use std::path::Path;

use led_core::{CharOffset, Col, Doc, Row};
use led_state::BufferState;

const LINE_WIDTH: u32 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillPrefix {
    /// `//`, `///`, `//!` — indent + marker.
    Line {
        indent: String,
        marker: &'static str,
    },
    /// Inside a `/** */` block — the outer indent of the `/**` line.
    /// Continuation lines use `{indent} * ` (note the extra space).
    Block { outer_indent: String },
}

#[derive(Debug, Clone)]
struct ReflowPlan {
    start_char: CharOffset,
    end_char: CharOffset,
    replacement: String,
}

// ──────────────────────────────────────────────────────────────────────────
// Line helpers
// ──────────────────────────────────────────────────────────────────────────

fn get_line_string(doc: &dyn Doc, row: usize) -> String {
    let mut s = String::new();
    doc.line(Row(row), &mut s);
    while s.ends_with('\n') || s.ends_with('\r') {
        s.pop();
    }
    s
}

fn split_indent(line: &str) -> (&str, &str) {
    let end = line
        .char_indices()
        .find(|(_, c)| *c != ' ' && *c != '\t')
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    (&line[..end], &line[end..])
}

fn is_blank(line: &str) -> bool {
    line.chars().all(|c| c.is_whitespace())
}

/// A line whose first non-whitespace characters are a markdown fenced-code
/// delimiter (` ``` ` or `~~~`). Opens and closes look the same — the caller
/// decides whether it's an opener or closer based on context.
fn is_fence_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// True if scanning from row 0 to `row` leaves an odd number of fence markers
/// behind — i.e. `row` sits inside a fenced code block.
fn is_inside_fenced_code_block(doc: &dyn Doc, row: usize) -> bool {
    let mut in_fence = false;
    for r in 0..row {
        let line = get_line_string(doc, r);
        if is_fence_line(&line) {
            in_fence = !in_fence;
        }
    }
    in_fence
}

/// Given the row of an opening fence, return the row of the matching closing
/// fence (or the last row of the doc if unterminated).
fn find_fence_end(doc: &dyn Doc, opener_row: usize) -> usize {
    let line_count = doc.line_count();
    let mut r = opener_row + 1;
    while r < line_count {
        let line = get_line_string(doc, r);
        if is_fence_line(&line) {
            return r;
        }
        r += 1;
    }
    line_count.saturating_sub(1)
}

// ──────────────────────────────────────────────────────────────────────────
// Prefix detection
// ──────────────────────────────────────────────────────────────────────────

/// Detect a line-style comment prefix (`//`, `///`, `//!`) on `line`.
pub fn detect_line_comment(line: &str) -> Option<FillPrefix> {
    let (indent, rest) = split_indent(line);
    // Order matters: "///" / "//!" must be checked before "//".
    let marker = if rest.starts_with("///") {
        "///"
    } else if rest.starts_with("//!") {
        "//!"
    } else if rest.starts_with("//") {
        "//"
    } else {
        return None;
    };
    Some(FillPrefix::Line {
        indent: indent.to_string(),
        marker,
    })
}

/// True when the (trimmed) line is a block-middle continuation like ` * foo`
/// or a bare `*`. The `indent` returned is the whitespace before the `*`.
fn is_block_middle(line: &str) -> Option<String> {
    let (indent, rest) = split_indent(line);
    if rest == "*" || rest.starts_with("* ") || rest == "*/" || rest.starts_with("*/") {
        // "*/" looks like a middle by prefix but is the closer; caller distinguishes.
        // Here we only match real middles.
        if rest == "*" || rest.starts_with("* ") {
            return Some(indent.to_string());
        }
    }
    None
}

// ──────────────────────────────────────────────────────────────────────────
// Bound finding
// ──────────────────────────────────────────────────────────────────────────

/// Walk up/down from `row` while lines share the same `Line { indent, marker }`
/// prefix. Returns the inclusive (start, end) row range.
fn find_line_comment_bounds(doc: &dyn Doc, row: usize, prefix: &FillPrefix) -> (usize, usize) {
    let matches = |r: usize| -> bool {
        let line = get_line_string(doc, r);
        detect_line_comment(&line).as_ref() == Some(prefix)
    };
    let mut start = row;
    while start > 0 && matches(start - 1) {
        start -= 1;
    }
    let mut end = row;
    while end + 1 < doc.line_count() && matches(end + 1) {
        end += 1;
    }
    (start, end)
}

/// Find the bounds of a `/** ... */` block surrounding `row`.
/// Returns `(start_row, end_row, outer_indent)` or `None`.
///
/// Accepts only the canonical form where:
/// - The `/**` line's trimmed content is exactly `/**`.
/// - The `*/` line's trimmed content is exactly `*/`.
/// - Every line between is a block-middle (` * content` or `*`).
fn find_block_comment_bounds(doc: &dyn Doc, row: usize) -> Option<(usize, usize, String)> {
    // Walk up from `row` looking for `/**`. Any non-middle, non-`/**` line → bail.
    let mut start = None;
    let mut outer_indent = String::new();
    let mut r = row;
    loop {
        let line = get_line_string(doc, r);
        let (indent, rest) = split_indent(&line);
        let trimmed = rest.trim_end();
        if trimmed == "/**" {
            start = Some(r);
            outer_indent = indent.to_string();
            break;
        }
        // On the starting row, accept either /** or a middle line (cursor may be on /** or in middle).
        // On rows above, we require middles all the way up to /**.
        let is_mid = is_block_middle(&line).is_some();
        let is_closer = trimmed == "*/";
        if r == row {
            if !is_mid && !is_closer {
                return None;
            }
        } else if !is_mid {
            return None;
        }
        if r == 0 {
            break;
        }
        r -= 1;
    }
    let start = start?;

    // Walk down from `row` looking for `*/`. Any non-middle, non-`*/` line → bail.
    let mut end = None;
    for r in row..doc.line_count() {
        let line = get_line_string(doc, r);
        let (indent_s, rest) = split_indent(&line);
        let trimmed = rest.trim_end();
        if trimmed == "*/" {
            // Require closer indent to equal outer_indent + " " (aligned with `*`s).
            let expected = format!("{outer_indent} ");
            if indent_s != expected {
                return None;
            }
            end = Some(r);
            break;
        }
        if r == row && trimmed == "/**" {
            continue;
        }
        is_block_middle(&line)?;
    }
    let end = end?;

    if end <= start {
        return None;
    }
    Some((start, end, outer_indent))
}

/// Walk up/down from `row` while lines are non-blank and not fence delimiters.
/// Returns `(start, end)` or `None` if `row` is blank, is itself a fence line,
/// or is inside a fenced code block.
fn find_paragraph_bounds(doc: &dyn Doc, row: usize) -> Option<(usize, usize)> {
    if row >= doc.line_count() {
        return None;
    }
    let line = get_line_string(doc, row);
    if is_blank(&line) || is_fence_line(&line) {
        return None;
    }
    if is_inside_fenced_code_block(doc, row) {
        return None;
    }
    let is_boundary = |s: &str| is_blank(s) || is_fence_line(s);
    let mut start = row;
    while start > 0 {
        let prev = start - 1;
        if is_boundary(&get_line_string(doc, prev)) {
            break;
        }
        start = prev;
    }
    let mut end = row;
    while end + 1 < doc.line_count() {
        let next = end + 1;
        if is_boundary(&get_line_string(doc, next)) {
            break;
        }
        end = next;
    }
    Some((start, end))
}

// ──────────────────────────────────────────────────────────────────────────
// Strip / reapply prefix
// ──────────────────────────────────────────────────────────────────────────

fn strip_line_prefix_from(line: &str, indent: &str, marker: &str) -> String {
    let mut rest = line;
    if let Some(r) = rest.strip_prefix(indent) {
        rest = r;
    }
    if let Some(r) = rest.strip_prefix(marker) {
        rest = r;
    }
    rest.strip_prefix(' ').unwrap_or(rest).to_string()
}

fn strip_block_middle_from(line: &str, outer_indent: &str) -> String {
    let mut rest = line;
    // Middle lines have indent = outer_indent + " ", then "*" and optional " content".
    let expected = format!("{outer_indent} ");
    if let Some(r) = rest.strip_prefix(expected.as_str()) {
        rest = r;
    }
    if let Some(r) = rest.strip_prefix('*') {
        rest = r;
    }
    rest.strip_prefix(' ').unwrap_or(rest).to_string()
}

/// Strip the prefix from each line in `start..=end`, join with `\n`.
fn strip_prefix_range(doc: &dyn Doc, start: usize, end: usize, prefix: &FillPrefix) -> String {
    let mut out = String::new();
    for r in start..=end {
        if r > start {
            out.push('\n');
        }
        let line = get_line_string(doc, r);
        let stripped = match prefix {
            FillPrefix::Line { indent, marker } => strip_line_prefix_from(&line, indent, marker),
            FillPrefix::Block { outer_indent } => strip_block_middle_from(&line, outer_indent),
        };
        out.push_str(&stripped);
    }
    out
}

fn collect_lines_range(doc: &dyn Doc, start: usize, end: usize) -> String {
    let mut out = String::new();
    for r in start..=end {
        if r > start {
            out.push('\n');
        }
        out.push_str(&get_line_string(doc, r));
    }
    out
}

fn reapply_prefix(content: &str, prefix: &FillPrefix) -> String {
    let mut out = String::new();
    let trimmed = content.trim_end_matches('\n');
    for (i, line) in trimmed.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match prefix {
            FillPrefix::Line { indent, marker } => {
                out.push_str(indent);
                out.push_str(marker);
                if !line.is_empty() {
                    out.push(' ');
                    out.push_str(line);
                }
            }
            FillPrefix::Block { outer_indent } => {
                out.push_str(outer_indent);
                out.push(' ');
                out.push('*');
                if !line.is_empty() {
                    out.push(' ');
                    out.push_str(line);
                }
            }
        }
    }
    out
}

// ──────────────────────────────────────────────────────────────────────────
// dprint wrapper
// ──────────────────────────────────────────────────────────────────────────

fn dprint_reflow(input: &str, line_width: u32) -> Option<String> {
    use dprint_plugin_markdown::configuration::{ConfigurationBuilder, TextWrap};
    use dprint_plugin_markdown::format_text;

    let config = ConfigurationBuilder::new()
        .line_width(line_width.max(1))
        .text_wrap(TextWrap::Always)
        .build();

    match format_text(input, &config, |_, _, _| Ok(None)) {
        Ok(Some(s)) => Some(s),
        Ok(None) => Some(input.to_string()),
        Err(_) => None,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Mode-specific reflow
// ──────────────────────────────────────────────────────────────────────────

fn reflow_line_comment(doc: &dyn Doc, row: usize, prefix: &FillPrefix) -> Option<ReflowPlan> {
    let (start, end) = find_line_comment_bounds(doc, row, prefix);
    let content = strip_prefix_range(doc, start, end, prefix);
    let (indent_len, marker_len) = match prefix {
        FillPrefix::Line { indent, marker } => (indent.chars().count(), marker.len()),
        _ => return None,
    };
    let overhead = indent_len + marker_len + 1; // trailing space
    let width = LINE_WIDTH.saturating_sub(overhead as u32);
    let reflowed = dprint_reflow(&content, width)?;
    let replacement = reapply_prefix(&reflowed, prefix);
    Some(plan_for_rows(doc, start, end, replacement))
}

fn reflow_block_comment(doc: &dyn Doc, row: usize) -> Option<ReflowPlan> {
    let (start, end, outer_indent) = find_block_comment_bounds(doc, row)?;
    let middle_start = start + 1;
    let middle_end = end.checked_sub(1)?;
    if middle_start > middle_end {
        return None;
    }
    let prefix = FillPrefix::Block {
        outer_indent: outer_indent.clone(),
    };
    let content = strip_prefix_range(doc, middle_start, middle_end, &prefix);
    let overhead = outer_indent.chars().count() + 3; // " * "
    let width = LINE_WIDTH.saturating_sub(overhead as u32);
    let reflowed = dprint_reflow(&content, width)?;
    let replacement = reapply_prefix(&reflowed, &prefix);
    Some(plan_for_rows(doc, middle_start, middle_end, replacement))
}

fn reflow_paragraph(doc: &dyn Doc, row: usize) -> Option<ReflowPlan> {
    let (start, end) = find_paragraph_bounds(doc, row)?;
    let content = collect_lines_range(doc, start, end);
    let reflowed = dprint_reflow(&content, LINE_WIDTH)?;
    let replacement = reflowed.trim_end_matches('\n').to_string();
    Some(plan_for_rows(doc, start, end, replacement))
}

fn plan_for_rows(doc: &dyn Doc, start: usize, end: usize, replacement: String) -> ReflowPlan {
    let start_char = doc.line_to_char(Row(start));
    let end_char = CharOffset(doc.line_to_char(Row(end)).0 + doc.line_len(Row(end)));
    ReflowPlan {
        start_char,
        end_char,
        replacement,
    }
}

fn is_reflowable_plain_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md" | "markdown" | "txt") | None
    )
}

// ──────────────────────────────────────────────────────────────────────────
// Top-level: called from reflow_of.rs
// ──────────────────────────────────────────────────────────────────────────

/// Run reflow on the buffer at its current cursor position. Returns the
/// modified buffer, or `None` if no reflowable region was found.
pub fn reflow_buffer(buf: &BufferState, path: &Path) -> Option<BufferState> {
    if let Some((mark_row, _)) = buf.mark() {
        let cursor_row = *buf.cursor_row();
        let a = *mark_row;
        let b = cursor_row;
        let start_row = a.min(b);
        let end_row = a.max(b);
        return reflow_region(buf, path, start_row, end_row);
    }

    let row = *buf.cursor_row();
    let col = *buf.cursor_col();
    let doc = &**buf.doc();

    if row >= doc.line_count() {
        return None;
    }

    let plan = pick_plan_at(doc, row, path)?;
    Some(apply_plans(buf, vec![plan], row, col))
}

/// Scan `start_row..=end_row`, collecting a reflow plan for every comment block
/// or (in text/markdown files) paragraph that touches the region. Returns the
/// modified buffer, or `None` if nothing reflowable was found.
fn reflow_region(
    buf: &BufferState,
    path: &Path,
    start_row: usize,
    end_row: usize,
) -> Option<BufferState> {
    let doc = &**buf.doc();
    let is_text = is_reflowable_plain_file(path);
    let mut plans = Vec::new();
    let mut r = start_row;
    let line_count = doc.line_count();

    while r <= end_row && r < line_count {
        let line = get_line_string(doc, r);

        // Skip fenced code blocks entirely: content inside is preserved
        // byte-for-byte and dprint isn't invoked on it.
        if is_fence_line(&line) {
            r = find_fence_end(doc, r) + 1;
            continue;
        }

        if let Some(prefix) = detect_line_comment(&line) {
            let (_, block_end) = find_line_comment_bounds(doc, r, &prefix);
            if let Some(plan) = reflow_line_comment(doc, r, &prefix) {
                plans.push(plan);
            }
            r = block_end + 1;
            continue;
        }

        if let Some((_, block_end, _)) = find_block_comment_bounds(doc, r) {
            if let Some(plan) = reflow_block_comment(doc, r) {
                plans.push(plan);
            }
            r = block_end + 1;
            continue;
        }

        if is_text
            && !is_blank(&line)
            && let Some((_, para_end)) = find_paragraph_bounds(doc, r)
        {
            if let Some(plan) = reflow_paragraph(doc, r) {
                plans.push(plan);
            }
            r = para_end + 1;
            continue;
        }

        r += 1;
    }

    if plans.is_empty() {
        return None;
    }

    let row = *buf.cursor_row();
    let col = *buf.cursor_col();
    Some(apply_plans(buf, plans, row, col))
}

fn pick_plan_at(doc: &dyn Doc, row: usize, path: &Path) -> Option<ReflowPlan> {
    let line = get_line_string(doc, row);
    detect_line_comment(&line)
        .and_then(|prefix| reflow_line_comment(doc, row, &prefix))
        .or_else(|| reflow_block_comment(doc, row))
        .or_else(|| {
            if is_reflowable_plain_file(path) {
                reflow_paragraph(doc, row)
            } else {
                None
            }
        })
}

/// Apply a list of reflow plans to `buf`. Plans are sorted by descending
/// `start_char` so that later edits don't invalidate earlier offsets.
fn apply_plans(
    buf: &BufferState,
    mut plans: Vec<ReflowPlan>,
    cursor_row: usize,
    cursor_col: usize,
) -> BufferState {
    plans.sort_by_key(|p| std::cmp::Reverse(p.start_char.0));

    let mut new_buf = buf.clone();
    new_buf.clear_mark();
    new_buf.close_group_on_move();
    for plan in plans {
        new_buf.remove_text(plan.start_char, plan.end_char);
        new_buf.insert_text(plan.start_char, &plan.replacement);
    }
    new_buf.close_group_on_move();

    let new_doc = &**new_buf.doc();
    let max_row = new_doc.line_count().saturating_sub(1);
    let new_row = cursor_row.min(max_row);
    let new_col = cursor_col.min(new_doc.line_len(Row(new_row)));
    new_buf.set_cursor(Row(new_row), Col(new_col), Col(new_col));
    new_buf.touch();
    new_buf
}

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_doc(text: &str) -> Arc<dyn Doc> {
        Arc::new(led_core::TextDoc::from_reader(std::io::Cursor::new(text.as_bytes())).unwrap())
    }

    // ── split_indent / is_blank ──

    #[test]
    fn split_indent_basic() {
        assert_eq!(split_indent("    foo"), ("    ", "foo"));
        assert_eq!(split_indent("\tfoo"), ("\t", "foo"));
        assert_eq!(split_indent("foo"), ("", "foo"));
        assert_eq!(split_indent("   "), ("   ", ""));
        assert_eq!(split_indent(""), ("", ""));
    }

    #[test]
    fn is_blank_basic() {
        assert!(is_blank(""));
        assert!(is_blank("   "));
        assert!(is_blank("\t "));
        assert!(!is_blank("a"));
        assert!(!is_blank("  a"));
    }

    // ── detect_line_comment ──

    #[test]
    fn detect_doc_comment_outer() {
        assert_eq!(
            detect_line_comment("/// foo"),
            Some(FillPrefix::Line {
                indent: "".into(),
                marker: "///"
            })
        );
    }

    #[test]
    fn detect_doc_comment_inner() {
        assert_eq!(
            detect_line_comment("    //! bar"),
            Some(FillPrefix::Line {
                indent: "    ".into(),
                marker: "//!"
            })
        );
    }

    #[test]
    fn detect_plain_line_comment() {
        assert_eq!(
            detect_line_comment("// todo"),
            Some(FillPrefix::Line {
                indent: "".into(),
                marker: "//"
            })
        );
    }

    #[test]
    fn detect_empty_comment() {
        assert_eq!(
            detect_line_comment("//"),
            Some(FillPrefix::Line {
                indent: "".into(),
                marker: "//"
            })
        );
    }

    #[test]
    fn detect_non_comment() {
        assert_eq!(detect_line_comment(""), None);
        assert_eq!(detect_line_comment("   code"), None);
        assert_eq!(detect_line_comment("fn foo()"), None);
    }

    // ── is_block_middle ──

    #[test]
    fn detect_block_middle_basic() {
        assert_eq!(is_block_middle(" * foo"), Some(" ".into()));
        assert_eq!(is_block_middle("    * bar"), Some("    ".into()));
        assert_eq!(is_block_middle(" *"), Some(" ".into()));
    }

    #[test]
    fn detect_block_middle_rejects_non_middle() {
        assert_eq!(is_block_middle(" */"), None); // closer
        assert_eq!(is_block_middle("*code"), None);
        assert_eq!(is_block_middle("foo"), None);
    }

    // ── find_paragraph_bounds ──

    #[test]
    fn paragraph_bounds_middle() {
        let doc = make_doc("first line\nsecond line\nthird line\n");
        assert_eq!(find_paragraph_bounds(&*doc, 1), Some((0, 2)));
    }

    #[test]
    fn paragraph_bounds_with_blanks() {
        let doc = make_doc("para one\n\npara two line\npara two line\n\npara three\n");
        // Row 2 is "para two line", bounded by blanks at 1 and 4.
        assert_eq!(find_paragraph_bounds(&*doc, 2), Some((2, 3)));
    }

    #[test]
    fn paragraph_bounds_on_blank_returns_none() {
        let doc = make_doc("first\n\nsecond\n");
        assert_eq!(find_paragraph_bounds(&*doc, 1), None);
    }

    #[test]
    fn paragraph_bounds_single_line() {
        let doc = make_doc("only line\n");
        assert_eq!(find_paragraph_bounds(&*doc, 0), Some((0, 0)));
    }

    // ── find_line_comment_bounds ──

    #[test]
    fn line_comment_bounds_three_lines() {
        let doc = make_doc("/// a\n/// b\n/// c\nfn foo() {}\n");
        let prefix = FillPrefix::Line {
            indent: "".into(),
            marker: "///",
        };
        assert_eq!(find_line_comment_bounds(&*doc, 1, &prefix), (0, 2));
    }

    #[test]
    fn line_comment_bounds_terminated_by_code() {
        let doc = make_doc("let x = 1;\n/// doc\n/// more\nfn foo() {}\n");
        let prefix = FillPrefix::Line {
            indent: "".into(),
            marker: "///",
        };
        assert_eq!(find_line_comment_bounds(&*doc, 1, &prefix), (1, 2));
    }

    #[test]
    fn line_comment_bounds_terminated_by_different_prefix() {
        let doc = make_doc("/// a\n// regular\n/// c\n");
        let prefix = FillPrefix::Line {
            indent: "".into(),
            marker: "///",
        };
        // Row 0 is /// alone (row 1 is //, row 2 is /// but separated).
        assert_eq!(find_line_comment_bounds(&*doc, 0, &prefix), (0, 0));
    }

    // ── find_block_comment_bounds ──

    #[test]
    fn block_comment_bounds_canonical() {
        let doc = make_doc("/**\n * first\n * second\n */\n");
        assert_eq!(find_block_comment_bounds(&*doc, 1), Some((0, 3, "".into())));
        assert_eq!(find_block_comment_bounds(&*doc, 2), Some((0, 3, "".into())));
    }

    #[test]
    fn block_comment_bounds_indented() {
        let doc = make_doc("    /**\n     * first\n     * second\n     */\n");
        assert_eq!(
            find_block_comment_bounds(&*doc, 1),
            Some((0, 3, "    ".into()))
        );
    }

    #[test]
    fn block_comment_bounds_single_line_rejected() {
        let doc = make_doc("/** foo */\n");
        assert_eq!(find_block_comment_bounds(&*doc, 0), None);
    }

    // ── strip / reapply round-trip ──

    #[test]
    fn strip_reapply_line_comment() {
        let doc = make_doc("/// one\n/// two\n/// three\n");
        let prefix = FillPrefix::Line {
            indent: "".into(),
            marker: "///",
        };
        let content = strip_prefix_range(&*doc, 0, 2, &prefix);
        assert_eq!(content, "one\ntwo\nthree");
        let re = reapply_prefix(&content, &prefix);
        assert_eq!(re, "/// one\n/// two\n/// three");
    }

    #[test]
    fn strip_reapply_block_middle() {
        let doc = make_doc("/**\n * one\n * two\n */\n");
        let prefix = FillPrefix::Block {
            outer_indent: "".into(),
        };
        let content = strip_prefix_range(&*doc, 1, 2, &prefix);
        assert_eq!(content, "one\ntwo");
        let re = reapply_prefix(&content, &prefix);
        assert_eq!(re, " * one\n * two");
    }

    #[test]
    fn strip_preserves_empty_comment_lines() {
        let doc = make_doc("// one\n//\n// two\n");
        let prefix = FillPrefix::Line {
            indent: "".into(),
            marker: "//",
        };
        let content = strip_prefix_range(&*doc, 0, 2, &prefix);
        assert_eq!(content, "one\n\ntwo");
    }

    // ── dprint smoke test ──

    #[test]
    fn dprint_wraps_long_line() {
        let input = "This is a fairly long paragraph that should definitely exceed the target width and force dprint to wrap it across multiple lines.";
        let out = dprint_reflow(input, 40).unwrap();
        for line in out.lines() {
            assert!(
                line.chars().count() <= 40 || !line.contains(' '),
                "line too long: {line:?}"
            );
        }
        // Must contain the original content.
        let joined: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
        let expected: String = input.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(joined, expected);
    }

    // ── reflow_buffer end-to-end ──

    fn make_buf(text: &str, name: &str) -> (BufferState, std::path::PathBuf) {
        let p = std::path::PathBuf::from(format!("/tmp/reflow_test_{name}"));
        let canon = led_core::UserPath::new(p.clone()).canonicalize();
        let mut buf = BufferState::new(canon);
        let doc: Arc<dyn Doc> = make_doc(text);
        buf.materialize(doc, false);
        (buf, p)
    }

    #[test]
    fn reflow_doc_comment_wraps_long_line() {
        let long = "/// ".to_string() + &"word ".repeat(40);
        let (buf, path) = make_buf(&long, "x.rs");
        buf.set_cursor(Row(0), Col(5), Col(5));
        let new_buf = reflow_buffer(&buf, &path).expect("should reflow");
        let new_doc = &**new_buf.doc();
        assert!(new_doc.line_count() > 1, "expected multiple lines");
        for r in 0..new_doc.line_count() {
            let line = get_line_string(new_doc, r);
            if line.is_empty() {
                continue;
            }
            assert!(line.starts_with("/// "), "missing /// prefix: {line:?}");
            assert!(line.chars().count() <= 100, "line too long: {line:?}");
        }
    }

    #[test]
    fn reflow_plain_code_line_is_noop() {
        let src = "fn foo() {\n    let x = 1;\n}\n";
        let (buf, path) = make_buf(src, "x.rs");
        buf.set_cursor(Row(1), Col(0), Col(0));
        assert!(reflow_buffer(&buf, &path).is_none());
    }

    #[test]
    fn reflow_markdown_paragraph_wraps() {
        let long = "word ".repeat(40);
        let text = format!("# Heading\n\n{long}\n\nMore.\n");
        let (buf, path) = make_buf(&text, "x.md");
        buf.set_cursor(Row(2), Col(5), Col(5));
        let new_buf = reflow_buffer(&buf, &path).expect("should reflow");
        let new_doc = &**new_buf.doc();
        // The heading should be untouched.
        assert_eq!(get_line_string(new_doc, 0), "# Heading");
        // All lines ≤ 100 chars.
        for r in 0..new_doc.line_count() {
            assert!(
                get_line_string(new_doc, r).chars().count() <= 100,
                "line {r} too long"
            );
        }
    }

    #[test]
    fn reflow_region_with_code_fences_preserves_code() {
        // Reproduces the CLAUDE.md Principle 9 case: prose + fenced code blocks.
        // Lines assembled explicitly so leading whitespace inside code blocks
        // survives (Rust `\` line-continuation would strip it).
        let lines: &[&str] = &[
            "The `Mut::Action(Action)` pattern that delegates to a giant `handle_action()` function defeats FRP. Each `Action` variant should become its own stream chain producing fine-grained `Mut`s.",
            "",
            "**Bad:**",
            "```rust",
            "Mut::Action(a) => {",
            "    action::handle_action(&mut s, a);  // 583 lines of imperative code",
            "}",
            "```",
            "",
            "**Good:**",
            "```rust",
            "// In editing_of.rs or per-domain _of files:",
            "let x = 1;",
            "```",
            "",
        ];
        let src = lines.join("\n");

        let (buf, path) = make_buf(&src, "principle.md");
        let last_row = buf.doc().line_count().saturating_sub(1);
        buf.set_cursor(Row(0), Col(0), Col(0));
        let mut buf2 = buf.clone();
        buf2.set_mark_at(Row(0), Col(0));
        buf2.set_cursor(Row(last_row), Col(0), Col(0));
        let new_buf = reflow_buffer(&buf2, &path).expect("should reflow");
        let new_doc = &**new_buf.doc();
        let text: String = (0..new_doc.line_count())
            .map(|r| get_line_string(new_doc, r))
            .collect::<Vec<_>>()
            .join("\n");
        // **Bad:** must be immediately followed by the fence (no blank line
        // inserted by dprint).
        assert!(
            text.contains("**Bad:**\n```rust"),
            "blank line inserted between **Bad:** and fence:\n{text}"
        );
        assert!(
            text.contains("**Good:**\n```rust"),
            "blank line inserted between **Good:** and fence:\n{text}"
        );
        // Code inside fences must be preserved byte-for-byte.
        assert!(
            text.contains("Mut::Action(a) => {"),
            "code altered:\n{text}"
        );
        assert!(
            text.contains("    action::handle_action(&mut s, a);  // 583 lines of imperative code"),
            "code indent stripped:\n{text}"
        );
        assert!(
            text.contains("// In editing_of.rs or per-domain _of files:"),
            "comment content altered:\n{text}"
        );
        // The prose paragraph at the top should be wrapped to ≤100 chars
        // (its original form is a single very long line).
        let first_para_end = text
            .find("\n\n")
            .expect("expected blank separator after prose");
        let first_para = &text[..first_para_end];
        for l in first_para.lines() {
            assert!(l.chars().count() <= 100, "prose line too long: {l:?}");
        }
    }

    #[test]
    fn reflow_region_reflows_multiple_blocks() {
        let word = "word ".repeat(30);
        let src = format!("/// {word}\nfn foo() {{}}\n\n/// {word}\nfn bar() {{}}\n");
        let (buf, path) = make_buf(&src, "region_multi.rs");
        // Region spans rows 0..=3 (both doc comments + code between).
        buf.set_cursor(Row(0), Col(0), Col(0));
        let mut buf2 = buf.clone();
        buf2.set_mark_at(Row(0), Col(0));
        buf2.set_cursor(Row(3), Col(0), Col(0));
        let new_buf = reflow_buffer(&buf2, &path).expect("should reflow");
        let new_doc = &**new_buf.doc();
        let text: String = (0..new_doc.line_count())
            .map(|r| get_line_string(new_doc, r))
            .collect::<Vec<_>>()
            .join("\n");
        // Both comments should be wrapped (multiple /// lines now).
        let slash_lines = text.lines().filter(|l| l.starts_with("/// ")).count();
        assert!(
            slash_lines >= 4,
            "expected ≥4 /// lines after wrapping two blocks, got {slash_lines}\n{text}"
        );
        // Code lines untouched.
        assert!(text.contains("fn foo() {}"));
        assert!(text.contains("fn bar() {}"));
    }

    #[test]
    fn reflow_region_skips_pure_code() {
        let src = "fn foo() {\n    let x = 1;\n    let y = 2;\n}\n";
        let (buf, path) = make_buf(src, "region_code.rs");
        let mut buf2 = buf.clone();
        buf2.set_mark_at(Row(0), Col(0));
        buf2.set_cursor(Row(3), Col(0), Col(0));
        assert!(reflow_buffer(&buf2, &path).is_none());
    }

    #[test]
    fn reflow_region_extends_past_boundary() {
        // Region touches row 0 only, but the /// block extends to row 2.
        // The full block should still be reflowed.
        let word = "word ".repeat(30);
        let src = format!("/// {word}\n/// more\n/// tail\nfn f() {{}}\n");
        let (buf, path) = make_buf(&src, "region_extend.rs");
        let mut buf2 = buf.clone();
        buf2.set_mark_at(Row(0), Col(0));
        buf2.set_cursor(Row(0), Col(5), Col(5));
        let new_buf = reflow_buffer(&buf2, &path).expect("should reflow");
        let new_doc = &**new_buf.doc();
        let text: String = (0..new_doc.line_count())
            .map(|r| get_line_string(new_doc, r))
            .collect::<Vec<_>>()
            .join("\n");
        // All three /// lines merge into a reflowed block.
        assert!(text.contains("fn f() {}"));
        // The words "more" and "tail" should be folded into the reflowed text.
        assert!(text.contains("more"));
        assert!(text.contains("tail"));
    }

    #[test]
    fn reflow_block_comment_preserves_wrappers() {
        let long_word = "word ".repeat(40);
        let src = format!("    /**\n     * {long_word}\n     */\n");
        let (buf, path) = make_buf(&src, "x.rs");
        buf.set_cursor(Row(1), Col(10), Col(10));
        let new_buf = reflow_buffer(&buf, &path).expect("should reflow");
        let new_doc = &**new_buf.doc();
        assert_eq!(get_line_string(new_doc, 0), "    /**");
        // Locate the closer, ignoring any trailing empty phantom line.
        let closer_row = (0..new_doc.line_count())
            .rev()
            .find(|&r| !get_line_string(new_doc, r).is_empty())
            .expect("non-empty doc");
        assert_eq!(get_line_string(new_doc, closer_row), "     */");
        let mut saw_middle = false;
        for r in 1..closer_row {
            let line = get_line_string(new_doc, r);
            assert!(
                line.starts_with("     * ") || line == "     *",
                "middle line wrong: {line:?}"
            );
            saw_middle = true;
        }
        assert!(saw_middle, "expected at least one middle line");
    }
}
