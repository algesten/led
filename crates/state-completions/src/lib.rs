//! LSP completion popup state.
//!
//! The driver produces `LspEvent::Completion` batches; the runtime
//! parks them here as a `CompletionSession`. The query layer reads
//! the session to render the popover, and dispatch mutates
//! `selected` / `filtered` / `session` on navigation / commit /
//! dismiss.
//!
//! One session at a time — mirrors legacy's manager state and
//! avoids the UX oddity of two popups fighting over the same
//! cursor. Opening a new session (different tab, new trigger)
//! replaces any prior one.

use std::sync::Arc;

use led_core::CanonPath;
use led_driver_lsp_core::CompletionItem;
use led_state_tabs::TabId;

/// Per-session completion state. `session: None` means no popup
/// is open; every field below is implicitly reset at that point.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompletionsState {
    pub session: Option<CompletionSession>,
    /// Monotonic sequence generator for request / resolve round
    /// trips. Handed to `LspCmd::RequestCompletion.seq` on
    /// dispatch; the runtime gates incoming events by comparing
    /// to the latest issued id so typing races can't display
    /// stale items.
    pub seq_gen: u64,
    /// Execute-pattern outbox — dispatch pushes one request per
    /// auto-trigger or explicit invoke, the driver-dispatch
    /// phase drains them and ships as `LspCmd::RequestCompletion`.
    /// `Vec` rather than `Option` so two tabs each triggering
    /// on the same tick both get serviced, but the runtime
    /// coalesces repeated requests for the same tab into the
    /// latest (server seq-gating handles the rest).
    pub pending_requests: Vec<PendingCompletionRequest>,
    /// Execute-pattern outbox for `completionItem/resolve`
    /// dispatched on commit. Drained into `LspCmd::ResolveCompletion`
    /// by the driver-dispatch phase.
    pub pending_resolves: Vec<PendingResolveRequest>,
}

/// Queued completion request waiting to be flushed to the LSP
/// driver. `seq` is already allocated from `seq_gen` so the
/// driver-dispatch phase just forwards.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingCompletionRequest {
    pub path: CanonPath,
    pub seq: u64,
    pub line: u32,
    pub col: u32,
    pub trigger: Option<char>,
}

/// Queued resolve request — committed item whose
/// `additional_text_edits` we want to fetch.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingResolveRequest {
    pub path: CanonPath,
    pub seq: u64,
    pub item: CompletionItem,
}

/// One active popup. Created on the first matching
/// `LspEvent::Completion` and cleared on commit / dismiss / tab
/// switch.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionSession {
    /// The tab the popup was triggered on. Used as a guard —
    /// switching tabs dismisses.
    pub tab: TabId,
    /// The buffer. Mostly a sanity check against events arriving
    /// after the user swapped tabs; all indexing into the rope
    /// goes through the tab's current buffer, not a snapshot.
    pub path: CanonPath,
    /// `seq` of the `LspCmd::RequestCompletion` that produced
    /// these items. Responses older than this are dropped.
    pub seq: u64,
    /// Cursor line at request time. The popup dismisses if the
    /// cursor moves to a different line (legacy behaviour —
    /// completions rarely span lines and matching LSP servers
    /// against a multi-line prefix is brittle).
    pub prefix_line: u32,
    /// Char col where the user's in-progress identifier starts.
    /// Client-side refilter extracts the prefix as
    /// `buffer.line(prefix_line)[prefix_start_col..cursor_col]`;
    /// edits that move the cursor LEFT of `prefix_start_col`
    /// dismiss the popup.
    pub prefix_start_col: u32,
    /// All items the server returned, unfiltered. Ref-counted so
    /// refilter + render can share the same Vec cheaply across
    /// frames.
    pub items: Arc<Vec<CompletionItem>>,
    /// Indices into `items`, in display order. Rebuilt on every
    /// keystroke (fuzzy-match + rank by nucleo); empty means
    /// "no items match the current prefix" → popup dismisses.
    pub filtered: Arc<Vec<usize>>,
    /// 0-indexed selection into `filtered`. Clamped on every
    /// refilter; arrow keys bump by ±1.
    pub selected: usize,
    /// Top index into `filtered` for the visible window.
    /// Popover shows at most ~10 rows; scroll moves this up/down
    /// to keep `selected` visible.
    pub scroll: usize,
}

impl CompletionsState {
    /// Allocate the next request / resolve sequence id.
    pub fn next_seq(&mut self) -> u64 {
        self.seq_gen = self.seq_gen.wrapping_add(1);
        self.seq_gen
    }

    /// Drop any active popup. Cheap no-op when already clear.
    pub fn dismiss(&mut self) {
        self.session = None;
    }

    /// Queue a completion request. Returns the allocated `seq`
    /// so the caller can stash it in the session when the
    /// corresponding response arrives.
    pub fn queue_request(
        &mut self,
        path: CanonPath,
        line: u32,
        col: u32,
        trigger: Option<char>,
    ) -> u64 {
        let seq = self.next_seq();
        self.pending_requests.push(PendingCompletionRequest {
            path,
            seq,
            line,
            col,
            trigger,
        });
        seq
    }

    /// Queue a resolve request for the item the user just
    /// committed. Returns the allocated `seq`.
    pub fn queue_resolve(&mut self, path: CanonPath, item: CompletionItem) -> u64 {
        let seq = self.next_seq();
        self.pending_resolves.push(PendingResolveRequest {
            path,
            seq,
            item,
        });
        seq
    }
}

/// `true` when an item's effective insertion text exactly equals
/// the user's typed prefix — i.e. committing would make no
/// change. Dispatch uses this together with `filtered.len() == 1`
/// to suppress the popup in the "only remaining suggestion is
/// already fully typed" case: the completion is correct but
/// pointless, and showing it distracts from surrounding code.
///
/// Effective insertion text is `text_edit.new_text` when
/// present, else `insert_text`, else `label` — same precedence
/// the commit path uses.
pub fn is_identity_match(item: &CompletionItem, prefix: &str) -> bool {
    let effective: &str = item
        .text_edit
        .as_ref()
        .map(|te| te.new_text.as_ref())
        .or_else(|| item.insert_text.as_deref())
        .unwrap_or_else(|| item.label.as_ref());
    effective == prefix
}

/// Rank + filter completion items against a typed prefix using
/// nucleo's smart-case fuzzy matcher. Empty prefix returns all
/// items in their original order (matches legacy's "no filter
/// on empty input" behaviour).
///
/// Output is a list of indices into `items`, ordered by descending
/// fuzzy score; ties are broken by ascending `sort_text` (falling
/// back to `label`). Items that don't match the prefix at all
/// are dropped — an empty return value signals "nothing matches,
/// dismiss the popup" to the dispatch side.
///
/// Port of `fuzzy_filter_completions` at
/// `/Users/martin/dev/led/crates/lsp/src/manager.rs:2057`.
pub fn refilter(items: &[CompletionItem], prefix: &str) -> Vec<usize> {
    use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
    use nucleo_matcher::{Config, Matcher, Utf32Str};

    if prefix.is_empty() {
        return (0..items.len()).collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        prefix,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, u32)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let haystack_str = item.label.as_ref();
            let haystack = Utf32Str::new(haystack_str, &mut buf);
            let score = pattern.score(haystack, &mut matcher)?;
            Some((i, score))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| {
            let a_key = items[a.0]
                .sort_text
                .as_deref()
                .unwrap_or(items[a.0].label.as_ref());
            let b_key = items[b.0]
                .sort_text
                .as_deref()
                .unwrap_or(items[b.0].label.as_ref());
            a_key.cmp(b_key)
        })
    });
    scored.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_driver_lsp_core::CompletionTextEdit;
    use std::sync::Arc;

    fn item(label: &str, sort: Option<&str>) -> CompletionItem {
        CompletionItem {
            label: Arc::<str>::from(label),
            detail: None,
            sort_text: sort.map(Arc::<str>::from),
            insert_text: None,
            text_edit: None,
            kind: None,
            resolve_needed: false,
            resolve_data: None,
        }
    }

    #[test]
    fn next_seq_is_monotonic() {
        let mut s = CompletionsState::default();
        assert_eq!(s.next_seq(), 1);
        assert_eq!(s.next_seq(), 2);
        assert_eq!(s.next_seq(), 3);
    }

    #[test]
    fn dismiss_is_idempotent() {
        let mut s = CompletionsState::default();
        s.dismiss();
        assert!(s.session.is_none());
    }

    #[test]
    fn refilter_empty_prefix_returns_identity_order() {
        let items = vec![item("foo", None), item("bar", None), item("baz", None)];
        let out = refilter(&items, "");
        assert_eq!(out, vec![0, 1, 2]);
    }

    #[test]
    fn refilter_fuzzy_matches_are_ranked_by_score() {
        // "pr" should rank println!/print! higher than
        // "pub" (no match) and similar things.
        let items = vec![
            item("println!", None),
            item("print!", None),
            item("pub", None),
            item("process", None),
        ];
        let out = refilter(&items, "pr");
        // "pub" can't fuzzy-match "pr" (no `r` in the string), so
        // it's filtered out. The rest are all matches.
        assert!(!out.contains(&2), "pub should be filtered: got {out:?}");
        // The three matchers appear in some order; all three
        // must be present.
        let mut labels: Vec<&str> = out
            .iter()
            .map(|&i| items[i].label.as_ref())
            .collect();
        labels.sort();
        assert_eq!(labels, vec!["print!", "println!", "process"]);
    }

    #[test]
    fn refilter_breaks_ties_by_sort_text_then_label() {
        // Two items with identical fuzzy match scores on "abc":
        // the one with the earlier sortText should come first.
        let items = vec![
            item("abc_two", Some("2")),
            item("abc_one", Some("1")),
        ];
        let out = refilter(&items, "abc");
        // sort_text "1" < "2", so abc_one (index 1) comes first.
        assert_eq!(out, vec![1, 0]);
    }

    #[test]
    fn refilter_case_insensitive() {
        let items = vec![item("PrintLn", None), item("pub", None)];
        let out = refilter(&items, "pl");
        assert_eq!(out, vec![0]);
    }

    #[test]
    fn refilter_returns_empty_when_nothing_matches() {
        let items = vec![item("foo", None), item("bar", None)];
        let out = refilter(&items, "xyz");
        assert!(out.is_empty());
    }

    #[test]
    fn is_identity_match_uses_insert_text_then_label() {
        // With insert_text present, the label is ignored.
        let with_insert = CompletionItem {
            label: Arc::<str>::from("show label"),
            detail: None,
            sort_text: None,
            insert_text: Some(Arc::<str>::from("PathBuf")),
            text_edit: None,
            kind: None,
            resolve_needed: false,
            resolve_data: None,
        };
        assert!(is_identity_match(&with_insert, "PathBuf"));
        assert!(!is_identity_match(&with_insert, "show label"));

        // Without insert_text or text_edit, falls back to label.
        let bare = item("PathBuf", None);
        assert!(is_identity_match(&bare, "PathBuf"));
        assert!(!is_identity_match(&bare, "Path"));
    }

    #[test]
    fn is_identity_match_uses_text_edit_first() {
        // text_edit.new_text wins over insert_text and label.
        let with_edit = CompletionItem {
            label: Arc::<str>::from("label"),
            detail: None,
            sort_text: None,
            insert_text: Some(Arc::<str>::from("insert")),
            text_edit: Some(CompletionTextEdit {
                line: 0,
                col_start: 0,
                col_end: 5,
                new_text: Arc::<str>::from("edit_text"),
            }),
            kind: None,
            resolve_needed: false,
            resolve_data: None,
        };
        assert!(is_identity_match(&with_edit, "edit_text"));
        assert!(!is_identity_match(&with_edit, "insert"));
        assert!(!is_identity_match(&with_edit, "label"));
    }
}
