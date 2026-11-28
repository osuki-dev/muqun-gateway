//! Link detection in terminal text: URLs and file paths with line:column.

use std::path::{Path, PathBuf};

/// A detected link target in terminal output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalLink {
    /// An HTTP/HTTPS or file URI.
    Url(String),
    /// A local filesystem path, optionally targeting a specific line and column.
    FilePath {
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
    },
}

/// A link found in a terminal row, with its character cell span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedLink {
    pub link: TerminalLink,
    pub col_start: u16,
    pub col_end: u16,
    pub raw: String,
}

/// Look for a clickable URL or file path in `line` at character column `col`.
///
/// `cwd` is the active terminal tab's working directory, used to resolve
/// relative file paths.
pub fn find_link_at(line: &str, col: usize, cwd: &Path) -> Option<DetectedLink> {
    if line.is_empty() {
        return None;
    }

    // 1. First, check for explicit URLs (http://, https://, file://).
    if let Some(link) = find_url_at(line, col) {
        return Some(link);
    }

    // 2. Otherwise, check for local file paths at `col`.
    find_file_path_at(line, col, cwd)
}

/// Find a URL under `target_col`.
fn find_url_at(line: &str, target_col: usize) -> Option<DetectedLink> {
    let prefixes = ["https://", "http://", "file://"];
    for prefix in prefixes {
        let mut search_from = 0;
        while let Some(rel_start) = line[search_from..].find(prefix) {
            let start = search_from + rel_start;
            let rest = &line[start..];
            let end_offset = rest
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>'))
                .unwrap_or(rest.len());
            let mut raw = &rest[..end_offset];
            // Trim trailing punctuation like `.` or `,` or unmatched `)` / `]`
            while raw.ends_with('.') || raw.ends_with(',') || raw.ends_with(';') {
                raw = &raw[..raw.len() - 1];
            }
            if raw.ends_with(')') && !raw.contains('(') {
                raw = &raw[..raw.len() - 1];
            }
            if raw.ends_with(']') && !raw.contains('[') {
                raw = &raw[..raw.len() - 1];
            }

            let start_char_col = line[..start].chars().count();
            let end_char_col = start_char_col + raw.chars().count();

            if target_col >= start_char_col && target_col < end_char_col {
                return Some(DetectedLink {
                    link: TerminalLink::Url(raw.to_string()),
                    col_start: start_char_col as u16,
                    col_end: end_char_col as u16,
                    raw: raw.to_string(),
                });
            }

            search_from = start + prefix.len();
        }
    }
    None
}

/// Delimiters that separate terminal command tokens or paths.
fn is_path_delimiter(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '|' | '='
        )
}

/// Find a file path (with optional :line[:col]) under `target_col`.
fn find_file_path_at(line: &str, target_col: usize, cwd: &Path) -> Option<DetectedLink> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    if target_col >= chars.len() {
        return None;
    }

    // If the character under target_col is a delimiter, no link.
    if is_path_delimiter(chars[target_col].1) {
        return None;
    }

    // Scan backwards for token start.
    let mut start_idx = target_col;
    while start_idx > 0 && !is_path_delimiter(chars[start_idx - 1].1) {
        start_idx -= 1;
    }

    // Scan forwards for token end.
    let mut end_idx = target_col + 1;
    while end_idx < chars.len() && !is_path_delimiter(chars[end_idx].1) {
        end_idx += 1;
    }

    let byte_start = chars[start_idx].0;
    let byte_end = if end_idx < chars.len() {
        chars[end_idx].0
    } else {
        line.len()
    };

    let mut candidate = &line[byte_start..byte_end];

    // Strip leading punctuation like `-->`, `(`, etc.
    while candidate.starts_with("-->") {
        candidate = &candidate[3..];
        start_idx += 3;
    }
    while candidate.starts_with(':') || candidate.starts_with(';') {
        candidate = &candidate[1..];
        start_idx += 1;
    }

    // Strip trailing punctuation like `,`, `;`, `.`.
    while candidate.ends_with(',')
        || candidate.ends_with(';')
        || (candidate.ends_with('.') && !candidate.ends_with(".."))
    {
        candidate = &candidate[..candidate.len() - 1];
        end_idx = end_idx.saturating_sub(1);
    }

    if candidate.trim().is_empty() {
        return None;
    }

    let (path_str, line_num, col_num) = parse_line_col(candidate);
    if path_str.is_empty() || path_str == "-" || path_str == "--" || path_str == "..." {
        return None;
    }

    // Resolve the path candidate against cwd and home directory.
    let resolved = resolve_path(path_str, cwd)?;

    Some(DetectedLink {
        link: TerminalLink::FilePath {
            path: resolved,
            line: line_num,
            col: col_num,
        },
        col_start: start_idx as u16,
        col_end: end_idx as u16,
        raw: candidate.to_string(),
    })
}

/// Splits trailing `:line[:col]` from a path string.
fn parse_line_col(candidate: &str) -> (&str, Option<u32>, Option<u32>) {
    let parts: Vec<&str> = candidate.split(':').collect();
    if parts.len() >= 3 {
        let p_line = parts[parts.len() - 2];
        let p_col = parts[parts.len() - 1];
        if let (Ok(line), Ok(col)) = (p_line.parse::<u32>(), p_col.parse::<u32>()) {
            let cut = candidate.len() - p_line.len() - p_col.len() - 2;
            return (&candidate[..cut], Some(line), Some(col));
        }
    }
    if parts.len() >= 2 {
        let p_line = parts[parts.len() - 1];
        if let Ok(line) = p_line.parse::<u32>() {
            let cut = candidate.len() - p_line.len() - 1;
            return (&candidate[..cut], Some(line), None);
        }
    }
    (candidate, None, None)
}

/// Resolves `path_str` to an existing file/directory or plausible path.
fn resolve_path(path_str: &str, cwd: &Path) -> Option<PathBuf> {
    let path = if let Some(stripped) = path_str.strip_prefix("~/") {
        dirs::home_dir().map(|h| h.join(stripped))?
    } else {
        PathBuf::from(path_str)
    };

    if path.is_absolute() {
        if path.exists() || path.parent().is_some_and(|p| p.is_dir()) {
            return Some(path);
        }
        return None;
    }

    // Try relative to cwd
    let in_cwd = cwd.join(&path);
    if in_cwd.exists() {
        return Some(in_cwd);
    }

    // Try relative to cwd's parent (e.g. if cwd is a subdirectory like desktop/ and path is desktop/src/...)
    if let Some(parent) = cwd.parent() {
        let in_parent = parent.join(&path);
        if in_parent.exists() {
            return Some(in_parent);
        }
    }

    // If it contains a directory separator or standard extension and parent dir exists in cwd:
    if (path_str.contains('/') || path_str.contains('\\'))
        && in_cwd.parent().is_some_and(|p| p.is_dir())
    {
        return Some(in_cwd);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_urls_in_terminal_output() {
        let cwd = Path::new("/tmp");
        let line = "Listening on http://localhost:8080/api/v1 (ready)";
        let col = 18; // inside http://localhost:8080/api/v1
        let detected = find_link_at(line, col, cwd).unwrap();
        assert_eq!(
            detected.link,
            TerminalLink::Url("http://localhost:8080/api/v1".into())
        );
        assert_eq!(detected.col_start, 13);
        assert_eq!(detected.col_end, 41);
    }

    #[test]
    fn detects_file_path_with_line_and_column() {
        let cwd = std::env::current_dir().unwrap();
        let line = "  --> desktop/src/main.rs:12:34";
        let col = 15; // inside desktop/src/main.rs
        let detected = find_link_at(line, col, &cwd).unwrap();
        match detected.link {
            TerminalLink::FilePath { path, line, col } => {
                assert!(path.ends_with("desktop/src/main.rs"));
                assert_eq!(line, Some(12));
                assert_eq!(col, Some(34));
            }
            _ => panic!("expected FilePath"),
        }
    }

    #[test]
    fn detects_relative_file_path() {
        let cwd = std::env::current_dir().unwrap();
        let line = "Check Cargo.toml for dependencies";
        let col = 8; // inside Cargo.toml
        let detected = find_link_at(line, col, &cwd).unwrap();
        match detected.link {
            TerminalLink::FilePath { path, line, col } => {
                assert!(path.ends_with("Cargo.toml"));
                assert_eq!(line, None);
                assert_eq!(col, None);
            }
            _ => panic!("expected FilePath"),
        }
    }
}
