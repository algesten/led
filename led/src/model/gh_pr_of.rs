use std::collections::HashMap;
use std::rc::Rc;

use led_core::rx::Stream;
use led_core::{Action, CanonPath};
use led_gh_pr::GhPrIn;
use led_state::{AppState, Phase, PrComment, PrInfo, PrStatus};

use super::Mut;

fn to_pr_info(ev: &GhPrIn) -> Option<PrInfo> {
    let GhPrIn::PrLoaded {
        number,
        state,
        url,
        api_endpoint,
        etag,
        diff_lines,
        comments,
        file_hashes,
    } = ev
    else {
        return None;
    };

    let status = match state.as_str() {
        "MERGED" => PrStatus::Merged,
        "CLOSED" => PrStatus::Closed,
        _ => PrStatus::Open,
    };

    let comments: HashMap<CanonPath, Vec<PrComment>> = comments
        .iter()
        .map(|(path, entries)| {
            let pcs = entries
                .iter()
                .map(|c| PrComment {
                    line: c.line,
                    body: c.body.clone(),
                    author: c.author.clone(),
                    url: c.url.clone(),
                })
                .collect();
            (path.clone(), pcs)
        })
        .collect();

    Some(PrInfo {
        number: *number,
        status,
        url: url.clone(),
        api_endpoint: api_endpoint.clone(),
        etag: etag.clone(),
        diff_files: diff_lines.clone(),
        comments,
        file_hashes: file_hashes.clone(),
    })
}

pub fn gh_pr_of(
    gh_pr_in: &Stream<GhPrIn>,
    raw_actions: &Stream<Action>,
    state: &Stream<Rc<AppState>>,
) -> Stream<Mut> {
    // Driver result → SetPrInfo (conversion in combinator chain, not reducer)
    // PrUnchanged (304) is filtered out — no state update needed.
    let pr_loaded_s = gh_pr_in
        .filter(|ev| !matches!(ev, GhPrIn::PrUnchanged))
        .map(|ev| Mut::SetPrInfo(to_pr_info(&ev)))
        .stream();

    // Branch change → clear PR state immediately
    let branch_clear_s = state
        .dedupe_by(|s| s.git.branch.clone())
        .filter(|s| s.phase == Phase::Running)
        .map(|_| Mut::SetPrInfo(None))
        .stream();

    // OpenPrUrl action → open comment URL if cursor is on a comment, else PR URL
    let open_pr_url_s = raw_actions
        .filter(|a| matches!(a, Action::OpenPrUrl))
        .sample_combine(state)
        .filter_map(|(_, s)| open_pr_target_url(&s))
        .map(Mut::SetPendingOpenUrl)
        .stream();

    let merged: Stream<Mut> = Stream::new();
    pr_loaded_s.forward(&merged);
    branch_clear_s.forward(&merged);
    open_pr_url_s.forward(&merged);
    merged
}

/// URL to open for the `OpenPrUrl` action: comment URL if the cursor is on a
/// PR comment line, otherwise the PR URL itself. Returns `None` when there is
/// no PR loaded.
fn open_pr_target_url(state: &AppState) -> Option<String> {
    let pr = state.git.pr.as_ref()?;
    Some(comment_url_at_cursor(state).unwrap_or_else(|| pr.url.clone()))
}

/// Find the (non-empty) comment URL on the active buffer's current line.
fn comment_url_at_cursor(state: &AppState) -> Option<String> {
    let buf = state.buffers.get(state.active_tab.as_ref()?)?;
    let path = buf.path()?;
    let pr = state.git.pr.as_ref()?;
    let comments = pr.comments.get(path)?;
    let crow = buf.cursor_row();
    comments
        .iter()
        .find(|c| c.line == crow)
        .map(|c| c.url.clone())
        .filter(|u| !u.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::{Row, Startup, UserPath};
    use led_state::{PrInfo, PrStatus};
    use std::sync::Arc;

    fn empty_state() -> AppState {
        AppState::new(Startup {
            headless: true,
            enable_watchers: false,
            arg_paths: vec![],
            arg_user_paths: vec![],
            arg_dir: None,
            start_dir: Arc::new(UserPath::new("/tmp").canonicalize()),
            user_start_dir: UserPath::new("/tmp"),
            config_dir: UserPath::new("/tmp/config"),
            test_lsp_server: None,
            test_gh_binary: None,
            golden_trace: None,
            no_workspace: false,
        })
    }

    fn pr_with_comment(path: CanonPath, line: usize, comment_url: &str) -> PrInfo {
        let mut comments = HashMap::new();
        comments.insert(
            path,
            vec![PrComment {
                line: Row(line),
                body: "x".into(),
                author: "u".into(),
                url: comment_url.into(),
            }],
        );
        PrInfo {
            number: led_core::PrNumber(1),
            status: PrStatus::Open,
            url: "https://github.com/o/r/pull/1".into(),
            api_endpoint: "repos/o/r/pulls/1".into(),
            etag: None,
            diff_files: HashMap::new(),
            comments,
            file_hashes: HashMap::new(),
        }
    }

    #[test]
    fn open_url_returns_none_without_pr() {
        let state = empty_state();
        assert_eq!(open_pr_target_url(&state), None);
    }

    #[test]
    fn open_url_falls_back_to_pr_url_when_no_comment_on_cursor_line() {
        let state = empty_state();
        // Note: there's no active tab, so comment_url_at_cursor returns None,
        // and open_pr_target_url falls back to pr.url.
        let mut state = state;
        let path = UserPath::new("/tmp/x").canonicalize();
        state.git_mut().pr = Some(pr_with_comment(path, 5, "https://example/c"));
        assert_eq!(
            open_pr_target_url(&state),
            Some("https://github.com/o/r/pull/1".into())
        );
    }

    #[test]
    fn comment_url_returns_none_for_empty_url() {
        let state = empty_state();
        // Even if a matching comment exists, an empty URL is filtered out.
        let mut state = state;
        let path = UserPath::new("/tmp/x").canonicalize();
        state.git_mut().pr = Some(pr_with_comment(path, 5, ""));
        // No active tab → still None.
        assert_eq!(comment_url_at_cursor(&state), None);
    }
}
