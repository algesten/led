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
                .map(|(line, body, author)| PrComment {
                    line: *line,
                    body: body.clone(),
                    author: author.clone(),
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

    // OpenPrUrl action → extract URL, set pending open
    let open_pr_url_s = raw_actions
        .filter(|a| matches!(a, Action::OpenPrUrl))
        .sample_combine(state)
        .filter_map(|(_, s)| s.git.pr.as_ref().map(|pr| pr.url.clone()))
        .map(Mut::SetPendingOpenUrl)
        .stream();

    let merged: Stream<Mut> = Stream::new();
    pr_loaded_s.forward(&merged);
    branch_clear_s.forward(&merged);
    open_pr_url_s.forward(&merged);
    merged
}
