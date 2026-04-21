//! Desktop worker for the project-wide file-search driver.
//!
//! One worker thread blocks on `Receiver<FileSearchCmd>`, walks the
//! workspace (honouring `.gitignore`), runs `grep-searcher` with a
//! `grep-regex` matcher, and posts a `FileSearchOut` back. Errors —
//! unreadable files, invalid regex — degrade to empty groups; the
//! overlay doesn't distinguish "no matches" from "error walking".

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use ignore::WalkBuilder;

use led_core::{Notifier, UserPath};
use led_driver_file_search_core::{
    FileSearchCmd, FileSearchDriver, FileSearchGroup, FileSearchHit, FileSearchOut,
    FileSearchReplaceCmd, FileSearchReplaceOut, Trace,
};

/// Legacy's hard cap on total hits per search — prevents the worker
/// from burning CPU on a pathological query ("a") against a huge
/// tree. UI shows whatever came in before the cap kicked in.
const MAX_HITS: usize = 1000;

/// Lifecycle marker. Drops when the driver does; the worker self-
/// exits when its command `Sender` hangs up.
pub struct FileSearchNative {
    _marker: (),
}

pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier) -> (FileSearchDriver, FileSearchNative) {
    let (search_tx_cmd, search_rx_cmd) = mpsc::channel::<FileSearchCmd>();
    let (search_tx_done, search_rx_done) = mpsc::channel::<FileSearchOut>();
    let (replace_tx_cmd, replace_rx_cmd) = mpsc::channel::<FileSearchReplaceCmd>();
    let (replace_tx_done, replace_rx_done) = mpsc::channel::<FileSearchReplaceOut>();
    let native = spawn_workers(
        search_rx_cmd,
        search_tx_done,
        replace_rx_cmd,
        replace_tx_done,
        notify,
    );
    let driver = FileSearchDriver::new(
        search_tx_cmd,
        search_rx_done,
        replace_tx_cmd,
        replace_rx_done,
        trace,
    );
    (driver, native)
}

/// Two worker threads: one for the live-typing search feed, one for
/// replace-all. Kept separate so an in-flight replace (which does
/// disk writes) doesn't back up search responses. Both self-exit
/// when their command `Sender` hangs up.
pub fn spawn_workers(
    search_rx: Receiver<FileSearchCmd>,
    search_tx: Sender<FileSearchOut>,
    replace_rx: Receiver<FileSearchReplaceCmd>,
    replace_tx: Sender<FileSearchReplaceOut>,
    notify: Notifier,
) -> FileSearchNative {
    let notify_search = notify.clone();
    thread::Builder::new()
        .name("led-file-search".into())
        .spawn(move || search_worker_loop(search_rx, search_tx, notify_search))
        .expect("spawning file-search search worker should succeed");
    thread::Builder::new()
        .name("led-file-search-replace".into())
        .spawn(move || replace_worker_loop(replace_rx, replace_tx, notify))
        .expect("spawning file-search replace worker should succeed");
    FileSearchNative { _marker: () }
}

fn search_worker_loop(
    rx: Receiver<FileSearchCmd>,
    tx: Sender<FileSearchOut>,
    notify: Notifier,
) {
    while let Ok(cmd) = rx.recv() {
        let (groups, flat) = run_search(&cmd);
        let out = FileSearchOut {
            query: cmd.query,
            case_sensitive: cmd.case_sensitive,
            use_regex: cmd.use_regex,
            groups,
            flat,
        };
        if tx.send(out).is_err() {
            return;
        }
        notify.notify();
    }
}

fn replace_worker_loop(
    rx: Receiver<FileSearchReplaceCmd>,
    tx: Sender<FileSearchReplaceOut>,
    notify: Notifier,
) {
    while let Ok(cmd) = rx.recv() {
        let (files_changed, total_replacements) = run_replace(&cmd);
        let out = FileSearchReplaceOut {
            query: cmd.query,
            files_changed,
            total_replacements,
        };
        if tx.send(out).is_err() {
            return;
        }
        notify.notify();
    }
}

/// Walk the workspace independently of the search results — apply
/// `regex.replace_all` to each file not in `skip_paths`, rewrite
/// atomically (`<dir>/.led-replace-<pid>` → `rename`) when the
/// substitution changed anything. Returns
/// `(files_changed, total_replacements)`. Invalid regex short-
/// circuits to `(0, 0)`.
fn run_replace(cmd: &FileSearchReplaceCmd) -> (usize, usize) {
    let pattern = if cmd.use_regex {
        cmd.query.clone()
    } else {
        regex_syntax::escape(&cmd.query)
    };
    let re = match regex::RegexBuilder::new(&pattern)
        .case_insensitive(!cmd.case_sensitive)
        .build()
    {
        Ok(r) => r,
        Err(_) => return (0, 0),
    };

    let walker = WalkBuilder::new(cmd.root.as_path())
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut files_changed = 0usize;
    let mut total_replacements = 0usize;

    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = UserPath::new(entry.path()).canonicalize();
        if cmd.skip_paths.contains(&path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            // Binary / unreadable files are skipped silently — same
            // policy `grep-searcher` applies in the search path.
            continue;
        };
        let new_content = re.replace_all(&content, cmd.replacement.as_str());
        if new_content.as_ref() == content {
            continue;
        }
        let count = re.find_iter(&content).count();
        if write_atomic(entry.path(), new_content.as_ref()).is_ok() {
            files_changed += 1;
            total_replacements += count;
        }
    }

    (files_changed, total_replacements)
}

/// Write `content` to `path` atomically: stage into
/// `<dir>/.led-replace-<pid>-<n>` first, then `rename` into place.
/// Keeps readers / other processes from seeing a torn file.
fn write_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = path.parent().unwrap_or(path);
    let tmp = dir.join(format!(".led-replace-{}-{}", std::process::id(), n));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

fn run_search(cmd: &FileSearchCmd) -> (Vec<FileSearchGroup>, Vec<FileSearchHit>) {
    let pattern = if cmd.use_regex {
        cmd.query.clone()
    } else {
        regex_syntax::escape(&cmd.query)
    };
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(!cmd.case_sensitive)
        .build(&pattern)
    {
        Ok(m) => m,
        Err(_) => return (Vec::new(), Vec::new()),
    };

    let walker = WalkBuilder::new(cmd.root.as_path())
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut searcher = grep_searcher::SearcherBuilder::new()
        .binary_detection(grep_searcher::BinaryDetection::quit(0x00))
        .build();

    let mut groups: Vec<FileSearchGroup> = Vec::new();
    let mut total_hits: usize = 0;

    'walk: for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = UserPath::new(entry.path()).canonicalize();
        let mut hits: Vec<FileSearchHit> = Vec::new();
        let _ = searcher.search_path(
            &matcher,
            path.as_path(),
            UTF8(|line_num, line_text| {
                let line_text = line_text
                    .trim_end_matches('\n')
                    .trim_end_matches('\r');
                let mut byte_offset = 0usize;
                loop {
                    let hay = &line_text.as_bytes()[byte_offset..];
                    if hay.is_empty() {
                        break;
                    }
                    match matcher.find(hay) {
                        Ok(Some(mat)) => {
                            let abs_start = byte_offset + mat.start();
                            let abs_end = byte_offset + mat.end();
                            let col = line_text[..abs_start].chars().count() + 1;
                            hits.push(FileSearchHit {
                                path: path.clone(),
                                line: line_num as usize,
                                col,
                                preview: line_text.to_string(),
                                match_start: abs_start,
                                match_end: abs_end,
                            });
                            byte_offset = abs_end;
                            if mat.start() == mat.end() {
                                byte_offset += 1;
                            }
                        }
                        _ => break,
                    }
                }
                Ok(true)
            }),
        );

        if !hits.is_empty() {
            total_hits += hits.len();
            let relative = entry
                .path()
                .strip_prefix(cmd.root.as_path())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| entry.path().to_string_lossy().into_owned());
            groups.push(FileSearchGroup {
                path,
                relative,
                hits,
            });
            if total_hits >= MAX_HITS {
                break 'walk;
            }
        }
    }

    groups.sort_by(|a, b| a.relative.cmp(&b.relative));
    let flat: Vec<FileSearchHit> = groups
        .iter()
        .flat_map(|g| g.hits.iter().cloned())
        .collect();
    (groups, flat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::CanonPath;
    use std::fs as stdfs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let n = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = base.join(format!("led-file-search-test.{pid}.{n}"));
        stdfs::create_dir_all(&dir).unwrap();
        dir
    }

    fn canon(p: &std::path::Path) -> CanonPath {
        UserPath::new(p).canonicalize()
    }

    fn wait_for<F: FnMut() -> bool>(mut f: F, deadline: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    struct NoopTraceImpl;
    impl Trace for NoopTraceImpl {
        fn file_search_start(&self, _: &FileSearchCmd) {}
        fn file_search_done(&self, _: &str, _: bool) {}
        fn file_search_replace_start(&self, _: &FileSearchReplaceCmd) {}
        fn file_search_replace_done(&self, _: &str, _: usize, _: usize) {}
    }

    #[test]
    fn literal_match_groups_per_file_with_1indexed_line_and_col() {
        let dir = tempdir();
        stdfs::write(dir.join("a.txt"), b"no match here\nneedle is here\n").unwrap();
        stdfs::write(dir.join("b.txt"), b"first\nsecond needle\nneedle twice needle\n")
            .unwrap();

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute(std::iter::once(&FileSearchCmd {
            root: canon(&dir),
            query: "needle".into(),
            case_sensitive: false,
            use_regex: false,
        }));

        let mut collected: Vec<FileSearchOut> = Vec::new();
        assert!(
            wait_for(
                || {
                    collected.extend(drv.process());
                    !collected.is_empty()
                },
                Duration::from_secs(2),
            ),
            "expected a result within 2s",
        );
        let out = &collected[0];
        assert_eq!(out.groups.len(), 2);
        let a = &out.groups[0];
        assert_eq!(a.relative, "a.txt");
        assert_eq!(a.hits.len(), 1);
        assert_eq!(a.hits[0].line, 2);
        assert_eq!(a.hits[0].col, 1);
        assert_eq!(a.hits[0].preview, "needle is here");
        let b = &out.groups[1];
        assert_eq!(b.relative, "b.txt");
        assert_eq!(b.hits.len(), 3); // 1 on line 2, 2 on line 3
        assert_eq!(b.hits[0].line, 2);
        assert_eq!(b.hits[1].line, 3);
        assert_eq!(b.hits[2].line, 3);
        // flat matches groups[..].hits concatenated.
        assert_eq!(out.flat.len(), 4);
    }

    #[test]
    fn case_sensitive_toggle_filters_out_mismatched_case() {
        let dir = tempdir();
        stdfs::write(dir.join("x.txt"), b"Needle\nneedle\n").unwrap();

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute(std::iter::once(&FileSearchCmd {
            root: canon(&dir),
            query: "needle".into(),
            case_sensitive: true,
            use_regex: false,
        }));

        let mut collected: Vec<FileSearchOut> = Vec::new();
        wait_for(
            || {
                collected.extend(drv.process());
                !collected.is_empty()
            },
            Duration::from_secs(2),
        );
        assert_eq!(collected[0].flat.len(), 1);
        assert_eq!(collected[0].flat[0].line, 2);
    }

    #[test]
    fn regex_toggle_interprets_pattern_as_regex() {
        let dir = tempdir();
        stdfs::write(dir.join("x.txt"), b"foo123bar\nfoo456bar\nbaz\n").unwrap();

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute(std::iter::once(&FileSearchCmd {
            root: canon(&dir),
            query: "foo[0-9]+bar".into(),
            case_sensitive: false,
            use_regex: true,
        }));

        let mut collected: Vec<FileSearchOut> = Vec::new();
        wait_for(
            || {
                collected.extend(drv.process());
                !collected.is_empty()
            },
            Duration::from_secs(2),
        );
        assert_eq!(collected[0].flat.len(), 2);
    }

    #[test]
    fn invalid_regex_returns_empty_groups() {
        let dir = tempdir();
        stdfs::write(dir.join("x.txt"), b"hi\n").unwrap();

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute(std::iter::once(&FileSearchCmd {
            root: canon(&dir),
            query: "[invalid".into(),
            case_sensitive: false,
            use_regex: true,
        }));

        let mut collected: Vec<FileSearchOut> = Vec::new();
        wait_for(
            || {
                collected.extend(drv.process());
                !collected.is_empty()
            },
            Duration::from_secs(2),
        );
        assert!(collected[0].groups.is_empty());
    }

    #[test]
    fn gitignored_paths_are_skipped() {
        let dir = tempdir();
        // `ignore` only honours `.gitignore` when the tree looks
        // like a git repo — empty `.git/` is enough of a marker.
        stdfs::create_dir(dir.join(".git")).unwrap();
        stdfs::write(dir.join(".gitignore"), b"ignored.txt\n").unwrap();
        stdfs::write(dir.join("ignored.txt"), b"needle\n").unwrap();
        stdfs::write(dir.join("kept.txt"), b"needle\n").unwrap();

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute(std::iter::once(&FileSearchCmd {
            root: canon(&dir),
            query: "needle".into(),
            case_sensitive: false,
            use_regex: false,
        }));

        let mut collected: Vec<FileSearchOut> = Vec::new();
        wait_for(
            || {
                collected.extend(drv.process());
                !collected.is_empty()
            },
            Duration::from_secs(2),
        );
        let names: Vec<&str> = collected[0]
            .groups
            .iter()
            .map(|g| g.relative.as_str())
            .collect();
        assert_eq!(names, vec!["kept.txt"]);
    }

    fn wait_for_replace(
        drv: &FileSearchDriver,
        deadline: Duration,
    ) -> Option<FileSearchReplaceOut> {
        let start = Instant::now();
        while start.elapsed() < deadline {
            let mut batch = drv.process_replace();
            if let Some(first) = batch.drain(..).next() {
                return Some(first);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        None
    }

    #[test]
    fn replace_rewrites_matching_files_and_counts_replacements() {
        let dir = tempdir();
        stdfs::write(dir.join("a.txt"), b"foo bar\nbaz foo\n").unwrap();
        stdfs::write(dir.join("b.txt"), b"no match here\n").unwrap();
        stdfs::write(dir.join("c.txt"), b"triple foo foo foo\n").unwrap();

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute_replace(std::iter::once(&FileSearchReplaceCmd {
            root: canon(&dir),
            query: "foo".into(),
            replacement: "QUX".into(),
            case_sensitive: false,
            use_regex: false,
            skip_paths: Vec::new(),
        }));

        let out = wait_for_replace(&drv, Duration::from_secs(2))
            .expect("replace result within 2s");
        assert_eq!(out.files_changed, 2);
        assert_eq!(out.total_replacements, 5);
        // a.txt rewritten, b.txt untouched, c.txt rewritten.
        let a = stdfs::read_to_string(dir.join("a.txt")).unwrap();
        let b = stdfs::read_to_string(dir.join("b.txt")).unwrap();
        let c = stdfs::read_to_string(dir.join("c.txt")).unwrap();
        assert_eq!(a, "QUX bar\nbaz QUX\n");
        assert_eq!(b, "no match here\n");
        assert_eq!(c, "triple QUX QUX QUX\n");
    }

    #[test]
    fn replace_skips_paths_in_skip_list() {
        let dir = tempdir();
        stdfs::write(dir.join("a.txt"), b"foo\n").unwrap();
        stdfs::write(dir.join("b.txt"), b"foo\n").unwrap();

        let a_path = canon(&dir.join("a.txt"));

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute_replace(std::iter::once(&FileSearchReplaceCmd {
            root: canon(&dir),
            query: "foo".into(),
            replacement: "QUX".into(),
            case_sensitive: false,
            use_regex: false,
            skip_paths: vec![a_path],
        }));

        let out = wait_for_replace(&drv, Duration::from_secs(2))
            .expect("replace result within 2s");
        // Only b.txt got rewritten; a.txt stays on disk untouched
        // (the runtime would have applied the replace in-memory for
        // a.txt on its own).
        assert_eq!(out.files_changed, 1);
        assert_eq!(out.total_replacements, 1);
        assert_eq!(stdfs::read_to_string(dir.join("a.txt")).unwrap(), "foo\n");
        assert_eq!(
            stdfs::read_to_string(dir.join("b.txt")).unwrap(),
            "QUX\n"
        );
    }

    #[test]
    fn replace_invalid_regex_returns_zero_counts() {
        let dir = tempdir();
        stdfs::write(dir.join("a.txt"), b"foo\n").unwrap();

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute_replace(std::iter::once(&FileSearchReplaceCmd {
            root: canon(&dir),
            query: "[invalid".into(),
            replacement: "x".into(),
            case_sensitive: false,
            use_regex: true,
            skip_paths: Vec::new(),
        }));

        let out = wait_for_replace(&drv, Duration::from_secs(2))
            .expect("replace result within 2s");
        assert_eq!(out.files_changed, 0);
        assert_eq!(out.total_replacements, 0);
        assert_eq!(stdfs::read_to_string(dir.join("a.txt")).unwrap(), "foo\n");
    }

    #[test]
    fn replace_preserves_trailing_newline_and_content_outside_match() {
        let dir = tempdir();
        stdfs::write(
            dir.join("a.txt"),
            b"line one with foo\nline two\nfoo at end\n",
        )
        .unwrap();

        let (drv, _native) = spawn(Arc::new(NoopTraceImpl), Notifier::noop());
        drv.execute_replace(std::iter::once(&FileSearchReplaceCmd {
            root: canon(&dir),
            query: "foo".into(),
            replacement: "BAR".into(),
            case_sensitive: false,
            use_regex: false,
            skip_paths: Vec::new(),
        }));

        let out = wait_for_replace(&drv, Duration::from_secs(2))
            .expect("replace result within 2s");
        assert_eq!(out.files_changed, 1);
        assert_eq!(out.total_replacements, 2);
        assert_eq!(
            stdfs::read_to_string(dir.join("a.txt")).unwrap(),
            "line one with BAR\nline two\nBAR at end\n",
        );
    }
}
