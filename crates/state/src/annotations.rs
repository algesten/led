//! Single source of truth for deriving [`IssueCategory`]-tagged annotations
//! from [`AppState`].
//!
//! The gutter, cursor popover, and file browser all consume from this
//! module. They share the same underlying data sources and filtering rules
//! (e.g. PR drift gate), so they can't drift out of sync.
//!
//! Surface:
//! - [`file_categories`] — browser file-row coloring.
//! - [`buffer_line_annotations`] — gutter per-line coloring.
//! - [`comments_at_line`] — cursor popover content.

use std::collections::{HashMap, HashSet};

use led_core::IssueCategory;
use led_core::git::LineStatus;
use led_lsp::DiagnosticSeverity;

use crate::{AppState, BufferState, PrComment, PrInfo};

/// Categories present on a file (for the browser). Combines git file
/// statuses, PR membership (comments + diff), and LSP diagnostic severity.
pub fn file_categories(state: &AppState, path: &led_core::CanonPath) -> HashSet<IssueCategory> {
    let mut cats = state
        .git
        .file_statuses
        .get(path)
        .cloned()
        .unwrap_or_default();
    if let Some(pr) = &state.git.pr {
        if pr.comments.contains_key(path) {
            cats.insert(IssueCategory::PrComment);
        }
        if pr.diff_files.contains_key(path) {
            cats.insert(IssueCategory::PrDiff);
        }
    }
    // LSP diagnostics from any open buffer for this file.
    if let Some(buf) = state.buffers.values().find(|b| b.path() == Some(path)) {
        for d in buf.status().diagnostics() {
            match d.severity {
                DiagnosticSeverity::Error => {
                    cats.insert(IssueCategory::LspError);
                }
                DiagnosticSeverity::Warning => {
                    cats.insert(IssueCategory::LspWarning);
                }
                _ => {}
            }
        }
    }
    cats
}

/// Precomputed categories for every file in [`AppState`] that has any
/// annotation. Used by the browser for both per-file and directory
/// aggregation (via [`led_core::directory_categories`]).
pub fn file_categories_map(
    state: &AppState,
) -> HashMap<led_core::CanonPath, HashSet<IssueCategory>> {
    let mut map: HashMap<led_core::CanonPath, HashSet<IssueCategory>> = HashMap::new();

    // Git file statuses
    for (path, cats) in &state.git.file_statuses {
        if !cats.is_empty() {
            map.entry(path.clone()).or_default().extend(cats.iter());
        }
    }

    // PR membership
    if let Some(pr) = &state.git.pr {
        for path in pr.comments.keys() {
            map.entry(path.clone())
                .or_default()
                .insert(IssueCategory::PrComment);
        }
        for path in pr.diff_files.keys() {
            map.entry(path.clone())
                .or_default()
                .insert(IssueCategory::PrDiff);
        }
    }

    // LSP diagnostics from open buffers
    for buf in state.buffers.values() {
        let Some(path) = buf.path() else { continue };
        for d in buf.status().diagnostics() {
            let cat = match d.severity {
                DiagnosticSeverity::Error => IssueCategory::LspError,
                DiagnosticSeverity::Warning => IssueCategory::LspWarning,
                _ => continue,
            };
            map.entry(path.clone()).or_default().insert(cat);
        }
    }

    map
}

/// All line-level annotations for a buffer (for the gutter).
///
/// Merges three sources:
/// - Git line statuses (Unstaged / StagedModified / StagedNew / Untracked)
/// - PR diff line ranges — suppressed when the file has diverged from the
///   PR's committed version (drift gate), since the line numbers are
///   meaningless after drift.
/// - PR comment lines — always included; the text is useful even with drift.
///
/// Multiple entries may overlap at the same row. Use
/// [`led_core::git::best_category_at`] to query with correct precedence.
pub fn buffer_line_annotations(state: &AppState, buf: &BufferState) -> Vec<LineStatus> {
    let mut out: Vec<LineStatus> = buf.status().git_line_statuses().to_vec();

    let Some(path) = buf.path() else {
        return out;
    };
    let Some(pr) = &state.git.pr else {
        return out;
    };

    // PR diff ranges — only when the file matches the PR head commit.
    if !pr_file_diverged(pr, path, buf) {
        if let Some(ranges) = pr.diff_files.get(path) {
            out.extend(ranges.iter().cloned());
        }
    }

    // PR comments — always included, one LineStatus per commented line.
    if let Some(comments) = pr.comments.get(path) {
        for c in comments {
            out.push(LineStatus {
                category: IssueCategory::PrComment,
                rows: *c.line..*c.line + 1,
            });
        }
    }

    out
}

/// PR comments on a specific row in a file (for the cursor popover).
/// Returns an empty slice when the file has no comments or no PR is loaded.
pub fn comments_at_line<'a>(
    state: &'a AppState,
    path: &led_core::CanonPath,
    row: led_core::Row,
) -> Vec<&'a PrComment> {
    let Some(pr) = &state.git.pr else {
        return Vec::new();
    };
    let Some(comments) = pr.comments.get(path) else {
        return Vec::new();
    };
    comments.iter().filter(|c| c.line == row).collect()
}

/// Whether the buffer has diverged from the PR's committed version of this
/// file. The drift gate for PR diff marks.
fn pr_file_diverged(pr: &PrInfo, path: &led_core::CanonPath, buf: &BufferState) -> bool {
    let Some(pr_hash) = pr.file_hashes.get(path) else {
        return false;
    };
    buf.content_hash() != *pr_hash || buf.is_dirty()
}
