/// A fake `gh` CLI for integration tests.
///
/// Reads `.fake-gh.json` from the current directory (which the gh-pr driver
/// sets to the workspace root) and returns canned responses based on
/// the subcommand.
///
/// Config format:
/// ```json
/// {
///     "pr_view": { "number": 42, "state": "OPEN", "url": "...", "reviewThreads": { "nodes": [] } },
///     "pr_diff": "diff --git a/file.txt b/file.txt\n..."
/// }
/// ```
///
/// Behaviour:
/// - `gh pr view --json ...` → prints `pr_view` object as JSON
/// - `gh pr diff` → prints `pr_diff` string
/// - Missing config or missing key → exit 1 (simulates "no PR")
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let config_str = match fs::read_to_string(".fake-gh.json") {
        Ok(s) => s,
        Err(_) => process::exit(1),
    };

    let config: serde_json::Value = match serde_json::from_str(&config_str) {
        Ok(v) => v,
        Err(_) => process::exit(1),
    };

    // Match: pr view --json ...
    if args.len() >= 2 && args[0] == "pr" && args[1] == "view" {
        match config.get("pr_view") {
            Some(v) => {
                println!("{}", v);
                process::exit(0);
            }
            None => process::exit(1),
        }
    }

    // Match: pr diff
    if args.len() >= 2 && args[0] == "pr" && args[1] == "diff" {
        match config.get("pr_diff").and_then(|v| v.as_str()) {
            Some(s) => {
                print!("{}", s);
                process::exit(0);
            }
            None => process::exit(1),
        }
    }

    // Match: api graphql -f query=...
    if args.len() >= 2 && args[0] == "api" && args[1] == "graphql" {
        match config.get("graphql") {
            Some(v) => {
                println!("{}", v);
                process::exit(0);
            }
            None => process::exit(1),
        }
    }

    // Unknown subcommand
    process::exit(1);
}
