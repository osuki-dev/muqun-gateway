//! Local `@` file search when the agent cannot answer (`find_files`
//! unsupported or empty). Same bounded walk as the gateway terminal pane.

use std::path::{Path, PathBuf};

use muqun_gateway::filesearch;
use serde_json::{json, Value};

/// Hits shaped like the agent's `{path, type}` objects, paths relative to
/// `directory`. Runs on a blocking pool: the walk is sync and IO-bound.
pub async fn find_files(directory: &str, query: &str, limit: usize) -> Vec<Value> {
    let root = PathBuf::from(directory);
    if !root.is_absolute() || !root.is_dir() {
        return Vec::new();
    }
    let needle = query.to_string();
    let hits = tokio::task::spawn_blocking(move || filesearch::search_files(&root, &needle, limit))
        .await
        .unwrap_or_default();
    hits.into_iter()
        .map(|hit| {
            json!({
                "path": hit.path,
                "type": "file",
                "name": hit.name,
            })
        })
        .collect()
}

/// True when `path` is under `directory` (after normalizing separators).
#[cfg(test)]
fn under_root(directory: &Path, path: &str) -> bool {
    let candidate = directory.join(path);
    candidate.starts_with(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mq-desktop-filesearch-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(&dir).unwrap()
    }

    #[tokio::test]
    async fn empty_query_lists_shallow_files() {
        let root = workspace("empty");
        fs::write(root.join("readme.md"), "hi").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();

        let hits = find_files(root.to_str().unwrap(), "", 10).await;
        assert!(!hits.is_empty());
        let paths: Vec<&str> = hits
            .iter()
            .filter_map(|h| h.get("path").and_then(Value::as_str))
            .collect();
        assert!(paths.iter().any(|p| *p == "readme.md"));
        for path in &paths {
            assert!(under_root(&root, path));
            assert!(!path.starts_with('/'));
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn query_ranks_name_matches() {
        let root = workspace("query");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(root.join("README.md"), "").unwrap();

        let hits = find_files(root.to_str().unwrap(), "main", 5).await;
        let first = hits[0].get("path").and_then(Value::as_str).unwrap();
        assert!(first.contains("main"), "{first}");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn refuses_relative_or_missing_roots() {
        assert!(find_files("relative", "x", 5).await.is_empty());
        assert!(find_files("/no/such/mq-desktop-root", "x", 5).await.is_empty());
    }
}
