/// Extract a language name from editor modelines in the first few lines.
///
/// Supports:
/// - Emacs: `-*- mode: ruby -*-` or `-*- ruby -*-`
/// - Vim/Vi: `vim: set ft=ruby :` or `vi: ft=ruby` or `vim: filetype=ruby`
pub(crate) fn detect_language_from_modeline<F>(line_fn: F, line_count: usize) -> Option<String>
where
    F: Fn(usize) -> String,
{
    let scan_lines = line_count.min(5);
    for i in 0..scan_lines {
        let line = line_fn(i);
        if let Some(lang) = parse_emacs_modeline(&line) {
            return Some(lang);
        }
        if let Some(lang) = parse_vim_modeline(&line) {
            return Some(lang);
        }
    }
    None
}

/// Parse an Emacs-style modeline.
///
/// Two forms:
/// - `# -*- mode: ruby -*-`          (explicit mode key)
/// - `# -*- ruby -*-`                (shorthand: bare name)
/// - `# -*- mode: ruby; coding: utf-8 -*-`  (mode among other variables)
fn parse_emacs_modeline(line: &str) -> Option<String> {
    let start = line.find("-*-")?;
    let rest = &line[start + 3..];
    let end = rest.find("-*-")?;
    let content = rest[..end].trim();

    // Check for "mode: X" among semicolon-separated variables.
    for part in content.split(';') {
        let part = part.trim();
        if let Some((key, val)) = part.split_once(':') {
            if key.trim().eq_ignore_ascii_case("mode") {
                return Some(val.trim().to_lowercase());
            }
        }
    }

    // Shorthand form: bare name with no colons or semicolons.
    if !content.contains(':') && !content.contains(';') && !content.is_empty() {
        return Some(content.to_lowercase());
    }

    None
}

/// Parse a Vim/Vi-style modeline.
///
/// Forms:
/// - `# vim: set ft=ruby :`
/// - `# vi: ft=ruby`
/// - `# vim: filetype=ruby`
/// - `# ex: set ft=ruby :`
fn parse_vim_modeline(line: &str) -> Option<String> {
    let markers = ["vim:", "vi:", "ex:"];
    let marker_pos = markers
        .iter()
        .find_map(|m| line.find(m).map(|pos| pos + m.len()))?;
    let rest = line[marker_pos..].trim_start();

    // Strip optional "set " or "se " prefix.
    let rest = rest
        .strip_prefix("set ")
        .or_else(|| rest.strip_prefix("se "))
        .unwrap_or(rest);

    // Look for ft= or filetype= among space-separated options.
    for option in rest.split_whitespace() {
        let option = option.trim_end_matches(':');
        if let Some(val) = option
            .strip_prefix("ft=")
            .or_else(|| option.strip_prefix("filetype="))
        {
            if !val.is_empty() {
                return Some(val.to_lowercase());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Emacs modeline ──

    #[test]
    fn emacs_mode_explicit() {
        assert_eq!(
            parse_emacs_modeline("# -*- mode: ruby -*-"),
            Some("ruby".into())
        );
    }

    #[test]
    fn emacs_mode_shorthand() {
        assert_eq!(
            parse_emacs_modeline("# -*- python -*-"),
            Some("python".into())
        );
    }

    #[test]
    fn emacs_mode_with_other_vars() {
        assert_eq!(
            parse_emacs_modeline("# -*- mode: ruby; coding: utf-8 -*-"),
            Some("ruby".into())
        );
    }

    #[test]
    fn emacs_mode_case_insensitive_key() {
        assert_eq!(
            parse_emacs_modeline("# -*- Mode: Ruby -*-"),
            Some("ruby".into())
        );
    }

    #[test]
    fn emacs_no_modeline() {
        assert_eq!(parse_emacs_modeline("# just a comment"), None);
    }

    #[test]
    fn emacs_unmatched_marker() {
        assert_eq!(parse_emacs_modeline("# -*- no closing marker"), None);
    }

    // ── Vim modeline ──

    #[test]
    fn vim_set_ft() {
        assert_eq!(
            parse_vim_modeline("# vim: set ft=ruby :"),
            Some("ruby".into())
        );
    }

    #[test]
    fn vim_filetype() {
        assert_eq!(
            parse_vim_modeline("# vim: filetype=python"),
            Some("python".into())
        );
    }

    #[test]
    fn vi_ft() {
        assert_eq!(
            parse_vim_modeline("# vi: set ft=bash :"),
            Some("bash".into())
        );
    }

    #[test]
    fn ex_ft() {
        assert_eq!(parse_vim_modeline("# ex: set ft=c :"), Some("c".into()));
    }

    #[test]
    fn vim_ft_among_other_options() {
        assert_eq!(
            parse_vim_modeline("# vim: set ts=4 ft=ruby sw=2 :"),
            Some("ruby".into())
        );
    }

    #[test]
    fn vim_no_modeline() {
        assert_eq!(parse_vim_modeline("# just a comment"), None);
    }

    // ── detect_language_from_modeline ──

    #[test]
    fn detect_from_first_line() {
        let lines = vec![
            "# -*- mode: ruby -*-".to_string(),
            "puts 'hello'".to_string(),
        ];
        let result = detect_language_from_modeline(|i| lines[i].clone(), lines.len());
        assert_eq!(result, Some("ruby".into()));
    }

    #[test]
    fn detect_from_second_line() {
        let lines = vec![
            "#!/usr/bin/env ruby".to_string(),
            "# vim: set ft=ruby :".to_string(),
            "".to_string(),
        ];
        let result = detect_language_from_modeline(|i| lines[i].clone(), lines.len());
        assert_eq!(result, Some("ruby".into()));
    }

    #[test]
    fn detect_none_when_absent() {
        let lines = vec![
            "#!/usr/bin/env ruby".to_string(),
            "puts 'hello'".to_string(),
        ];
        let result = detect_language_from_modeline(|i| lines[i].clone(), lines.len());
        assert_eq!(result, None);
    }

    #[test]
    fn detect_scans_at_most_5_lines() {
        let mut lines: Vec<String> = (0..10).map(|_| "# nothing".to_string()).collect();
        lines[6] = "# vim: set ft=ruby :".to_string();
        let result = detect_language_from_modeline(|i| lines[i].clone(), lines.len());
        assert_eq!(result, None);
    }
}
