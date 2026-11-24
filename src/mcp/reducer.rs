//! High-signal result reduction and secret redaction for MCP tool outputs
//! and inter-agent orchestration summaries.
//!
//! Principles:
//! 1. **Private by default**: Strip bearer tokens, provider API keys, and
//!    credential assignments before output crosses into an LLM context or a
//!    secondary reviewer/advisor agent.
//! 2. **Signal over noise**: Fold repetitive build/test boilerplate lines
//!    (`Compiling ...`, `Downloading ...`, `test ... ok`) while preserving
//!    errors, panics, failed tests, and final summary lines.

use serde::Serialize;

const REDACTED: &str = "[REDACTED]";

/// Output of [`reduce_command_output`]: compact, redacted, error-preserving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReducedOutput {
    pub stdout_tail: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub key_errors: Vec<String>,
    pub original_bytes: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub omitted_lines: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Redact common API keys, bearer tokens, and secret key=value assignments.
/// Returns `(redacted_text, changed)`.
pub(crate) fn redact_secrets(input: &str) -> (String, bool) {
    if input.is_empty() {
        return (String::new(), false);
    }
    let mut out = String::with_capacity(input.len());
    let mut changed = false;
    for (idx, line) in input.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let (clean, line_changed) = redact_line(line);
        changed |= line_changed;
        out.push_str(&clean);
    }
    (out, changed)
}

fn redact_line(line: &str) -> (String, bool) {
    let mut out = String::with_capacity(line.len());
    let mut changed = false;
    let bytes = line.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // 1. Authorization: Bearer <token> / "Bearer <token>"
        if starts_with_ascii_ci(&line[i..], "bearer ")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        {
            out.push_str(&line[i..i + 7]);
            i += 7;
            while i < bytes.len() && bytes[i] == b' ' {
                out.push(' ');
                i += 1;
            }
            let start = i;
            while i < bytes.len() && is_token_char(bytes[i]) {
                i += 1;
            }
            if i.saturating_sub(start) >= 8 {
                out.push_str(REDACTED);
                changed = true;
            } else {
                out.push_str(&line[start..i]);
            }
            continue;
        }

        // 2. Well-known provider token prefixes (at a token boundary, including after `=`, `:`, or quotes).
        if (i == 0
            || (!bytes[i - 1].is_ascii_alphanumeric()
                && bytes[i - 1] != b'_'
                && bytes[i - 1] != b'-'))
            && is_known_secret_prefix(&line[i..])
        {
            let start = i;
            while i < bytes.len() && is_token_char(bytes[i]) {
                i += 1;
            }
            if i.saturating_sub(start) >= 16 {
                out.push_str(REDACTED);
                changed = true;
                continue;
            }
            out.push_str(&line[start..i]);
            continue;
        }

        // 3. Key=value or Key: value credential assignments (e.g. API_KEY=..., password="...").
        if i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_') {
            if let Some(key_len) = secret_key_prefix_len(&line[i..]) {
                let after_key = i + key_len;
                let rest = &line[after_key..];
                if let Some((sep_len, quote)) = assignment_separator(rest) {
                    let val_start = after_key + sep_len;
                    let mut val_end = val_start;
                    while val_end < bytes.len() {
                        let b = bytes[val_end];
                        if let Some(q) = quote {
                            if b == q {
                                break;
                            }
                        } else if b.is_ascii_whitespace()
                            || matches!(b, b',' | b';' | b')' | b']' | b'}' | b'"' | b'\'')
                        {
                            break;
                        }
                        val_end += 1;
                    }
                    let val = &line[val_start..val_end];
                    if val.len() >= 10
                        && !val.starts_with('$')
                        && !val.starts_with('[')
                        && !val.eq_ignore_ascii_case("redacted")
                        && !val.eq_ignore_ascii_case("placeholder")
                        && !val.eq_ignore_ascii_case("changeme")
                    {
                        out.push_str(&line[i..val_start]);
                        out.push_str(REDACTED);
                        i = val_end;
                        changed = true;
                        continue;
                    }
                }
            }
        }

        let ch = line[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    (out, changed)
}

fn starts_with_ascii_ci(haystack: &str, needle: &str) -> bool {
    haystack
        .get(..needle.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(needle))
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+' | b'/' | b'=')
}

fn is_known_secret_prefix(s: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-ant-",
        "sk-proj-",
        "sk-",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "github_pat_",
        "AIza",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "AKIA",
        "ASIA",
        "glpat-",
        "npm_",
        "hf_",
    ];
    PREFIXES.iter().any(|p| s.starts_with(p))
}

fn secret_key_prefix_len(s: &str) -> Option<usize> {
    // Match identifier up to 40 chars containing a sensitive token suffix.
    let len = s
        .bytes()
        .take(48)
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        .count();
    if len < 5 {
        return None;
    }
    let lower = s[..len].to_ascii_lowercase();
    const SUFFIXES: &[&str] = &[
        "api_key",
        "apikey",
        "api-key",
        "secret_key",
        "secret",
        "access_token",
        "auth_token",
        "refresh_token",
        "private_key",
        "password",
        "passwd",
    ];
    SUFFIXES
        .iter()
        .any(|suf| {
            lower == *suf
                || lower.ends_with(&format!("_{suf}"))
                || lower.ends_with(&format!("-{suf}"))
        })
        .then_some(len)
}

fn assignment_separator(rest: &str) -> Option<(usize, Option<u8>)> {
    let bytes = rest.as_bytes();
    let mut i = 0;
    // Allow optional closing quote on JSON keys (`"api_key": "..."`).
    if bytes.get(i) == Some(&b'"') || bytes.get(i) == Some(&b'\'') {
        i += 1;
    }
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if !matches!(bytes.get(i), Some(b'=' | b':')) {
        return None;
    }
    i += 1;
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    let quote = match bytes.get(i) {
        Some(q @ (b'"' | b'\'')) => {
            i += 1;
            Some(*q)
        }
        _ => None,
    };
    Some((i, quote))
}

/// True for high-volume build/test boilerplate lines that add little value
/// to an LLM once a command finishes.
fn is_boilerplate_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    t.starts_with("Compiling ")
        || t.starts_with("Downloading ")
        || t.starts_with("Downloaded ")
        || t.starts_with("Checking ")
        || t.starts_with("Fresh ")
        || t.starts_with("Updating ")
        || t.starts_with("Locking ")
        || t.starts_with("Packaging ")
        || t.starts_with("Archiving ")
        || (t.starts_with("test ") && t.ends_with("... ok"))
}

/// True for high-signal failure/error lines worth extracting into `key_errors`.
pub(crate) fn is_error_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    t.starts_with("error[")
        || t.starts_with("error:")
        || t.starts_with("fatal:")
        || t.starts_with("FAILED")
        || t.starts_with("FAIL ")
        || t.starts_with("npm ERR!")
        || t.starts_with("AssertionError")
        || t.starts_with("Traceback (most recent call last)")
        || t.contains("panicked at ")
        || (t.starts_with("test ") && t.ends_with("... FAILED"))
        || t.contains("error TS")
}

/// Reduce and redact command output for LLM consumption while preserving
/// key errors and the final summary tail.
pub(crate) fn reduce_command_output(
    raw: &str,
    exit_code: Option<i32>,
    max_bytes: usize,
) -> ReducedOutput {
    let original_bytes = raw.len();
    // Collapse carriage-return progress rewrites (`foo\rbar` -> `bar`).
    let normalized = raw
        .lines()
        .map(|line| line.rsplit('\r').next().unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    let (redacted_text, redacted) = redact_secrets(&normalized);

    let failed = exit_code.is_some_and(|c| c != 0);
    let mut key_errors = Vec::new();
    let mut kept = Vec::new();
    let mut omitted_lines = 0usize;

    for line in redacted_text.lines() {
        if is_error_line(line) && key_errors.len() < 12 {
            let trimmed = line.trim();
            let clipped = if trimmed.len() > 240 {
                let mut end = 240;
                while end > 0 && !trimmed.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &trimmed[..end])
            } else {
                trimmed.to_string()
            };
            if !key_errors.contains(&clipped) {
                key_errors.push(clipped);
            }
        }
        if is_boilerplate_line(line) {
            omitted_lines += 1;
            continue;
        }
        kept.push(line);
    }

    // If everything was boilerplate (e.g. a quiet build), keep the last 10 lines.
    let mut stdout_tail = if kept.is_empty() && !redacted_text.is_empty() {
        let all: Vec<&str> = redacted_text.lines().collect();
        let start = all.len().saturating_sub(10);
        omitted_lines = start;
        all[start..].join("\n")
    } else {
        kept.join("\n")
    };

    // Trim cleanly to `max_bytes`, keeping the tail (which has failure summaries
    // and test results) and prepending key errors if a failed run got trimmed.
    if stdout_tail.len() > max_bytes && max_bytes > 64 {
        let mut start = stdout_tail.len() - (max_bytes - 32);
        while start < stdout_tail.len() && !stdout_tail.is_char_boundary(start) {
            start += 1;
        }
        if let Some(nl) = stdout_tail[start..].find('\n') {
            start += nl + 1;
        }
        let dropped = stdout_tail[..start].lines().count();
        omitted_lines += dropped;
        let tail = &stdout_tail[start..];
        stdout_tail = if failed && !key_errors.is_empty() && !tail.contains(&key_errors[0]) {
            let header = format!(
                "[key errors]\n{}\n[… {} earlier lines omitted …]\n",
                key_errors
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
                omitted_lines
            );
            let remain = max_bytes.saturating_sub(header.len());
            let mut tstart = tail.len().saturating_sub(remain);
            while tstart < tail.len() && !tail.is_char_boundary(tstart) {
                tstart += 1;
            }
            format!("{header}{}", &tail[tstart..])
        } else {
            format!("[… {omitted_lines} earlier lines omitted …]\n{tail}")
        };
    }

    ReducedOutput {
        stdout_tail,
        key_errors,
        original_bytes,
        omitted_lines,
        redacted,
    }
}

/// Extract up to `limit` concise error lines from arbitrary text (e.g. task answer/summary).
pub(crate) fn extract_key_errors(text: &str, limit: usize) -> Vec<String> {
    let (clean, _) = redact_secrets(text);
    let mut out = Vec::new();
    for line in clean.lines() {
        if is_error_line(line) {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !out.iter().any(|e| e == trimmed) {
                let clipped = if trimmed.len() > 220 {
                    let mut end = 220;
                    while end > 0 && !trimmed.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}…", &trimmed[..end])
                } else {
                    trimmed.to_string()
                };
                out.push(clipped);
                if out.len() >= limit {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_tokens_and_credentials_without_mangling_normal_code() {
        let raw = "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abcdef123456\n\
                   OPENAI_API_KEY=sk-proj-1234567890abcdef1234567890\n\
                   GITHUB_TOKEN=ghp_1234567890abcdefghij1234567890\n\
                   {\"password\": \"supersecret999\", \"safe\": \"hello\"}\n\
                   let count = 42;";
        let (clean, changed) = redact_secrets(raw);
        assert!(changed);
        assert!(!clean.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(!clean.contains("sk-proj-1234567890"));
        assert!(!clean.contains("ghp_1234567890"));
        assert!(!clean.contains("supersecret999"));
        assert!(clean.contains("Bearer [REDACTED]"));
        assert!(clean.contains("OPENAI_API_KEY=[REDACTED]"));
        assert!(clean.contains("\"safe\": \"hello\""));
        assert!(clean.contains("let count = 42;"));
    }

    #[test]
    fn reduces_cargo_noise_and_keeps_errors_and_summary() {
        let mut raw = String::new();
        for i in 0..80 {
            raw.push_str(&format!("   Compiling crate-{i} v0.1.0\n"));
        }
        raw.push_str("error[E0308]: mismatched types\n");
        raw.push_str("  --> src/lib.rs:42:10\n");
        for i in 0..50 {
            raw.push_str(&format!("test suite::case_{i} ... ok\n"));
        }
        raw.push_str("test suite::failing_case ... FAILED\n");
        raw.push_str("test result: FAILED. 50 passed; 1 failed;\n");

        let reduced = reduce_command_output(&raw, Some(1), 2048);
        assert_eq!(reduced.omitted_lines, 130);
        assert!(reduced
            .key_errors
            .iter()
            .any(|e| e.contains("error[E0308]: mismatched types")));
        assert!(reduced
            .key_errors
            .iter()
            .any(|e| e.contains("test suite::failing_case ... FAILED")));
        assert!(!reduced.stdout_tail.contains("Compiling crate-0"));
        assert!(!reduced.stdout_tail.contains("case_0 ... ok"));
        assert!(reduced
            .stdout_tail
            .contains("test result: FAILED. 50 passed; 1 failed;"));
    }
}
