use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct SearchHit {
    pub(crate) row: usize,
    pub(crate) col: usize,
    pub(crate) line_text: String,
    pub(crate) match_start: usize,
    pub(crate) match_end: usize,
}

#[derive(Clone)]
pub(crate) struct FileGroup {
    pub(crate) path: PathBuf,
    pub(crate) relative: String,
    pub(crate) hits: Vec<SearchHit>,
}

#[derive(Clone)]
pub(crate) struct FlatHit {
    pub(crate) group_idx: usize,
    pub(crate) hit_idx: usize,
}

pub(crate) struct SearchRequest {
    pub(crate) query: String,
    pub(crate) root: PathBuf,
    pub(crate) case_sensitive: bool,
    pub(crate) use_regex: bool,
}
