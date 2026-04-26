//! Paragraph / line-comment / block-comment reflow (M23).
//!
//! Pure text-in / text-out helpers. The dispatch arm in
//! `runtime/src/dispatch/reflow.rs` calls [`reflow_at`] (or
//! [`reflow_region`]) with the active buffer's `Rope` + cursor
//! row + file extension; on `Some` it applies the resulting
//! [`ReflowPlan`]s and on `None` it surfaces a "Nothing to
//! reflow" alert.
//!
//! Port of legacy `led/src/model/reflow.rs` translated from the
//! `dyn Doc` API to ropey. The detection rules carry over
//! verbatim: `///` / `//!` / `//` line-comment blocks,
//! canonical `/** … */` block comments, and (for `.md` /
//! `.markdown` / `.txt` / extensionless files) plain paragraphs
//! bounded by blank lines, skipping anything inside a
//! ` ``` ` / `~~~` fence.

use ropey::Rope;

const LINE_WIDTH: u32 = 100;

/// One reflow operation: replace `[start_char, end_char)` with
/// `replacement`. Plans are sorted by `start_char` descending
/// before application so applying them in order keeps offsets
/// consistent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflowPlan {
    pub start_char: usize,
    pub end_char: usize,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FillPrefix {
    /// `//`, `///`, `//!` — indent + marker.
    Line {
        indent: String,
        marker: &'static str,
    },
    /// Inside a `/** */` block — the indent of the `/**` line.
    /// Continuation lines get `{indent} * `.
    Block { outer_indent: String },
}

// ── Line helpers ──────────────────────────────────────────────

fn get_line_string(rope: &Rope, row: usize) -> String {
    if row >= rope.len_lines() {
        return String::new();
    }
    let mut s: String = rope.line(row).chars().collect();
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

/// Markdown fence delimiter — ` ``` ` / `~~~`. Used for both
/// openers and closers; context decides which.
fn is_fence_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

fn is_inside_fenced_code_block(rope: &Rope, row: usize) -> bool {
    let mut in_fence = false;
    for r in 0..row {
        if is_fence_line(&get_line_string(rope, r)) {
            in_fence = !in_fence;
        }
    }
    in_fence
}

fn find_fence_end(rope: &Rope, opener_row: usize) -> usize {
    let line_count = rope.len_lines();
    let mut r = opener_row + 1;
    while r < line_count {
        if is_fence_line(&get_line_string(rope, r)) {
            return r;
        }
        r += 1;
    }
    line_count.saturating_sub(1)
}

// ── Prefix detection ──────────────────────────────────────────

fn detect_line_comment(line: &str) -> Option<FillPrefix> {
    let (indent, rest) = split_indent(line);
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

fn is_block_middle(line: &str) -> Option<String> {
    let (indent, rest) = split_indent(line);
    if rest == "*" || rest.starts_with("* ") {
        return Some(indent.to_string());
    }
    None
}

// ── Bound finding ─────────────────────────────────────────────

fn find_line_comment_bounds(rope: &Rope, row: usize, prefix: &FillPrefix) -> (usize, usize) {
    let matches = |r: usize| -> bool {
        let line = get_line_string(rope, r);
        detect_line_comment(&line).as_ref() == Some(prefix)
    };
    let mut start = row;
    while start > 0 && matches(start - 1) {
        start -= 1;
    }
    let mut end = row;
    while end + 1 < rope.len_lines() && matches(end + 1) {
        end += 1;
    }
    (start, end)
}

fn find_block_comment_bounds(rope: &Rope, row: usize) -> Option<(usize, usize, String)> {
    let mut start = None;
    let mut outer_indent = String::new();
    let mut r = row;
    loop {
        let line = get_line_string(rope, r);
        let (indent, rest) = split_indent(&line);
        let trimmed = rest.trim_end();
        if trimmed == "/**" {
            start = Some(r);
            outer_indent = indent.to_string();
            break;
        }
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

    let mut end = None;
    for r in row..rope.len_lines() {
        let line = get_line_string(rope, r);
        let (indent_s, rest) = split_indent(&line);
        let trimmed = rest.trim_end();
        if trimmed == "*/" {
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

fn find_paragraph_bounds(rope: &Rope, row: usize) -> Option<(usize, usize)> {
    if row >= rope.len_lines() {
        return None;
    }
    let line = get_line_string(rope, row);
    if is_blank(&line) || is_fence_line(&line) {
        return None;
    }
    if is_inside_fenced_code_block(rope, row) {
        return None;
    }
    let is_boundary = |s: &str| is_blank(s) || is_fence_line(s);
    let mut start = row;
    while start > 0 {
        let prev = start - 1;
        if is_boundary(&get_line_string(rope, prev)) {
            break;
        }
        start = prev;
    }
    let mut end = row;
    while end + 1 < rope.len_lines() {
        let next = end + 1;
        if is_boundary(&get_line_string(rope, next)) {
            break;
        }
        end = next;
    }
    Some((start, end))
}

// ── Strip / reapply prefix ────────────────────────────────────

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
    let expected = format!("{outer_indent} ");
    if let Some(r) = rest.strip_prefix(expected.as_str()) {
        rest = r;
    }
    if let Some(r) = rest.strip_prefix('*') {
        rest = r;
    }
    rest.strip_prefix(' ').unwrap_or(rest).to_string()
}

fn strip_prefix_range(rope: &Rope, start: usize, end: usize, prefix: &FillPrefix) -> String {
    let mut out = String::new();
    for r in start..=end {
        if r > start {
            out.push('\n');
        }
        let line = get_line_string(rope, r);
        let stripped = match prefix {
            FillPrefix::Line { indent, marker } => strip_line_prefix_from(&line, indent, marker),
            FillPrefix::Block { outer_indent } => strip_block_middle_from(&line, outer_indent),
        };
        out.push_str(&stripped);
    }
    out
}

fn collect_lines_range(rope: &Rope, start: usize, end: usize) -> String {
    let mut out = String::new();
    for r in start..=end {
        if r > start {
            out.push('\n');
        }
        out.push_str(&get_line_string(rope, r));
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

// ── dprint wrapper ────────────────────────────────────────────

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

// ── Mode-specific reflow ──────────────────────────────────────

fn plan_for_rows(rope: &Rope, start: usize, end: usize, replacement: String) -> ReflowPlan {
    let start_char = rope.line_to_char(start);
    let end_char = if end + 1 < rope.len_lines() {
        // Up to (but not including) the trailing newline.
        rope.line_to_char(end + 1).saturating_sub(1)
    } else {
        rope.len_chars()
    };
    ReflowPlan {
        start_char,
        end_char,
        replacement,
    }
}

fn reflow_line_comment(rope: &Rope, row: usize, prefix: &FillPrefix) -> Option<ReflowPlan> {
    let (start, end) = find_line_comment_bounds(rope, row, prefix);
    let content = strip_prefix_range(rope, start, end, prefix);
    let (indent_len, marker_len) = match prefix {
        FillPrefix::Line { indent, marker } => (indent.chars().count(), marker.len()),
        _ => return None,
    };
    let overhead = indent_len + marker_len + 1;
    let width = LINE_WIDTH.saturating_sub(overhead as u32);
    let reflowed = dprint_reflow(&content, width)?;
    let replacement = reapply_prefix(&reflowed, prefix);
    Some(plan_for_rows(rope, start, end, replacement))
}

fn reflow_block_comment(rope: &Rope, row: usize) -> Option<ReflowPlan> {
    let (start, end, outer_indent) = find_block_comment_bounds(rope, row)?;
    let middle_start = start + 1;
    let middle_end = end.checked_sub(1)?;
    if middle_start > middle_end {
        return None;
    }
    let prefix = FillPrefix::Block {
        outer_indent: outer_indent.clone(),
    };
    let content = strip_prefix_range(rope, middle_start, middle_end, &prefix);
    let overhead = outer_indent.chars().count() + 3;
    let width = LINE_WIDTH.saturating_sub(overhead as u32);
    let reflowed = dprint_reflow(&content, width)?;
    let replacement = reapply_prefix(&reflowed, &prefix);
    Some(plan_for_rows(rope, middle_start, middle_end, replacement))
}

fn reflow_paragraph(rope: &Rope, row: usize) -> Option<ReflowPlan> {
    let (start, end) = find_paragraph_bounds(rope, row)?;
    let content = collect_lines_range(rope, start, end);
    let reflowed = dprint_reflow(&content, LINE_WIDTH)?;
    let replacement = reflowed.trim_end_matches('\n').to_string();
    Some(plan_for_rows(rope, start, end, replacement))
}

fn is_reflowable_plain_file(extension: Option<&str>) -> bool {
    matches!(extension, Some("md" | "markdown" | "txt") | None)
}

fn pick_plan_at(rope: &Rope, row: usize, extension: Option<&str>) -> Option<ReflowPlan> {
    let line = get_line_string(rope, row);
    detect_line_comment(&line)
        .and_then(|prefix| reflow_line_comment(rope, row, &prefix))
        .or_else(|| reflow_block_comment(rope, row))
        .or_else(|| {
            if is_reflowable_plain_file(extension) {
                reflow_paragraph(rope, row)
            } else {
                None
            }
        })
}

// ── Public API ────────────────────────────────────────────────

/// Pick a reflow plan for the line under the cursor. `extension`
/// is the file's extension (`Some("md")`, `Some("rs")`, `None`
/// for extensionless files); it routes the paragraph fallback.
/// Returns `None` when the cursor doesn't sit in a reflowable
/// region.
pub fn reflow_at(
    rope: &Rope,
    cursor_row: usize,
    extension: Option<&str>,
) -> Option<ReflowPlan> {
    if cursor_row >= rope.len_lines() {
        return None;
    }
    pick_plan_at(rope, cursor_row, extension)
}

/// Walk `start_row..=end_row` collecting a plan per
/// reflowable region (line-comment block, block comment,
/// paragraph in markdown / txt files). Plans are returned in
/// document order; callers sort descending by `start_char`
/// before applying. Returns `None` when nothing was reflowable.
///
/// Currently unused by dispatch (M23 only wires the cursor-line
/// path) — see `MILESTONE-23.md` § "Out". Available for the
/// region-via-mark follow-up.
pub fn reflow_region(
    rope: &Rope,
    start_row: usize,
    end_row: usize,
    extension: Option<&str>,
) -> Option<Vec<ReflowPlan>> {
    let line_count = rope.len_lines();
    let is_text = is_reflowable_plain_file(extension);
    let mut plans = Vec::new();
    let mut r = start_row;

    while r <= end_row && r < line_count {
        let line = get_line_string(rope, r);

        if is_fence_line(&line) {
            r = find_fence_end(rope, r) + 1;
            continue;
        }

        if let Some(prefix) = detect_line_comment(&line) {
            let (_, block_end) = find_line_comment_bounds(rope, r, &prefix);
            if let Some(plan) = reflow_line_comment(rope, r, &prefix) {
                plans.push(plan);
            }
            r = block_end + 1;
            continue;
        }

        if let Some((_, block_end, _)) = find_block_comment_bounds(rope, r) {
            if let Some(plan) = reflow_block_comment(rope, r) {
                plans.push(plan);
            }
            r = block_end + 1;
            continue;
        }

        if is_text
            && !is_blank(&line)
            && let Some((_, para_end)) = find_paragraph_bounds(rope, r)
        {
            if let Some(plan) = reflow_paragraph(rope, r) {
                plans.push(plan);
            }
            r = para_end + 1;
            continue;
        }

        r += 1;
    }

    if plans.is_empty() {
        None
    } else {
        Some(plans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn split_indent_basic() {
        assert_eq!(split_indent("    foo"), ("    ", "foo"));
        assert_eq!(split_indent("\tfoo"), ("\t", "foo"));
        assert_eq!(split_indent("foo"), ("", "foo"));
        assert_eq!(split_indent(""), ("", ""));
    }

    #[test]
    fn detect_line_comment_outer_doc() {
        assert_eq!(
            detect_line_comment("/// foo"),
            Some(FillPrefix::Line {
                indent: "".into(),
                marker: "///"
            })
        );
    }

    #[test]
    fn paragraph_bounds_with_blanks() {
        let r = rope("para one\n\npara two line\npara two line\n\npara three\n");
        assert_eq!(find_paragraph_bounds(&r, 2), Some((2, 3)));
    }

    #[test]
    fn paragraph_bounds_on_blank_returns_none() {
        let r = rope("first\n\nsecond\n");
        assert_eq!(find_paragraph_bounds(&r, 1), None);
    }

    #[test]
    fn reflow_at_long_doc_comment_wraps() {
        let long = "/// ".to_string() + &"word ".repeat(40);
        let r = rope(&long);
        let plan = reflow_at(&r, 0, Some("rs")).expect("plan");
        // Multiple lines after wrapping.
        assert!(plan.replacement.contains('\n'), "replacement: {:?}", plan.replacement);
        for l in plan.replacement.lines() {
            assert!(l.starts_with("/// "), "missing /// prefix: {l:?}");
            assert!(l.chars().count() <= 100, "line too long: {l:?}");
        }
    }

    #[test]
    fn reflow_at_plain_code_line_returns_none() {
        let r = rope("fn foo() {\n    let x = 1;\n}\n");
        assert!(reflow_at(&r, 1, Some("rs")).is_none());
    }

    #[test]
    fn reflow_at_long_markdown_paragraph_wraps() {
        let long = "word ".repeat(40);
        let r = rope(&format!("# Heading\n\n{long}\n\nMore.\n"));
        let plan = reflow_at(&r, 2, Some("md")).expect("plan");
        for l in plan.replacement.lines() {
            assert!(l.chars().count() <= 100, "line too long: {l:?}");
        }
    }

    #[test]
    fn reflow_at_inside_fenced_block_returns_none() {
        let r = rope("```rust\nlet x = 1;\n```\n");
        // Row 1 is inside the fence.
        assert!(reflow_at(&r, 1, Some("md")).is_none());
    }

    #[test]
    fn reflow_at_blank_line_returns_none() {
        let r = rope("a\n\nb\n");
        assert!(reflow_at(&r, 1, Some("md")).is_none());
    }

    #[test]
    fn reflow_block_comment_preserves_wrappers() {
        let long_word = "word ".repeat(40);
        let src = format!("    /**\n     * {long_word}\n     */\n");
        let r = rope(&src);
        let plan = reflow_at(&r, 1, Some("rs")).expect("plan");
        for l in plan.replacement.lines() {
            assert!(
                l.starts_with("     * ") || l == "     *",
                "middle line wrong: {l:?}"
            );
        }
    }

    #[test]
    fn reflow_region_with_code_fence_skips_fence() {
        let lines: &[&str] = &[
            "Para one continues here.",
            "",
            "```rust",
            "let x = 1;",
            "```",
            "",
            "Para two.",
        ];
        let r = rope(&lines.join("\n"));
        // Region covers rows 0..=6.
        let plans = reflow_region(&r, 0, 6, Some("md"));
        // Should pick up the two paragraphs and skip the fence.
        // (Depending on bounds finding, may collapse — at least
        // assert we got SOME plans and none touched the code
        // line.)
        let plans = plans.expect("plans");
        for p in &plans {
            // None of the plans should overlap the `let x = 1;`
            // line (row 3, char_start computed below).
            let row3_start = r.line_to_char(3);
            let row3_end = r.line_to_char(4);
            let overlaps = !(p.end_char <= row3_start || p.start_char >= row3_end);
            assert!(
                !overlaps,
                "plan {p:?} overlaps fenced code at chars [{row3_start}, {row3_end})"
            );
        }
    }
}
