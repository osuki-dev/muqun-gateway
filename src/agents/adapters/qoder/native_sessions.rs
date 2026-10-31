//! Recover metadata omitted by Qoder's global ACP listing. Only native-listed
//! identities are looked up under Qoder's own transcript root. Project bucket
//! names are lossy and are never decoded into workspace paths.
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agents::ports::agent::AgentError;

const MAX_ROWS: usize = 4096;
const MAX_PROJECTS: usize = 1024;
const MAX_ENTRIES: usize = 16384;
const PREFIX_BYTES: usize = 8 * 1024 * 1024;
const TAIL_BYTES: usize = 64 * 1024;
const TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_RECORDS: usize = 65536;

#[derive(Default)]
struct Metadata {
    cwd: Option<String>,
    title: Option<String>,
    id_seen: bool,
    invalid: bool,
    metadata_only: bool,
    empty_leaf_seen: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    session_id: Option<String>,
    cwd: Option<String>,
    custom_title: Option<String>,
    leaf_uuid: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, serde::de::IgnoredAny>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

pub(super) async fn recover(
    rows: Vec<Value>,
    stop: &CancellationToken,
) -> Result<Vec<Value>, AgentError> {
    let Some(home) = dirs::home_dir() else {
        return Ok(rows);
    };
    let root = home.join(".qoder").join("projects");
    let stop = stop.clone();
    tokio::task::spawn_blocking(move || recover_in(rows, &root, &stop))
        .await
        .map_err(|_| AgentError::NotAvailable("Qoder session metadata reader failed".into()))?
}

/// Changes when Qoder adds, removes or appends to a saved session.
pub(super) fn store_fingerprint() -> Option<u64> {
    store_fingerprint_in(&dirs::home_dir()?.join(".qoder").join("projects"))
}

fn store_fingerprint_in(root: &Path) -> Option<u64> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let projects = match std::fs::read_dir(root) {
        Ok(projects) => projects,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(0),
        Err(_) => return None,
    };
    // Order-independent: directory iteration order is unspecified.
    let mut fingerprint = 0_u64;
    let mut entries = 0;
    for project in projects {
        let project = project.ok()?;
        if !project.file_type().ok()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(project.path()).ok()? {
            entries += 1;
            if entries > MAX_ENTRIES {
                return None;
            }
            let entry = entry.ok()?;
            let metadata = entry.metadata().ok()?;
            let mut hasher = DefaultHasher::new();
            (project.file_name(), entry.file_name()).hash(&mut hasher);
            (metadata.len(), metadata.modified().ok()?).hash(&mut hasher);
            fingerprint = fingerprint.wrapping_add(hasher.finish());
        }
    }
    Some(fingerprint)
}

fn cancelled(stop: &CancellationToken) -> Result<(), AgentError> {
    if stop.is_cancelled() {
        Err(AgentError::NotAvailable("Qoder runtime stopped".into()))
    } else {
        Ok(())
    }
}

fn records(bytes: &[u8], whole: bool, skip_first: bool, id: &str, metadata: &mut Metadata) {
    let mut lines = bytes.split(|byte| *byte == b'\n');
    if skip_first {
        lines.next();
    }
    for (count, line) in lines.enumerate() {
        if count >= MAX_RECORDS {
            metadata.invalid = true;
            break;
        }
        if line.is_empty() {
            continue;
        }
        // A bounded prefix/tail can end mid-append. Never accept a partial row.
        if !whole && !bytes.ends_with(b"\n") && line.as_ptr_range().end == bytes.as_ptr_range().end
        {
            break;
        }
        let Ok(record) = serde_json::from_slice::<Record>(line) else {
            metadata.invalid = true;
            continue;
        };
        let identified = record.session_id.as_deref() == Some(id);
        if let Some(session_id) = record.session_id.as_deref() {
            if session_id != id {
                metadata.invalid = true;
                continue;
            }
            metadata.id_seen = true;
        }
        let allowed_extra: &[&str] = match record.kind.as_deref() {
            Some("active-leaf") => &["timestamp", "explicit"],
            Some("runtime-config") => &[
                "timestamp",
                "contextWindow",
                "generation",
                "model",
                "reasoningEffort",
            ],
            Some("custom-title") => &["timestamp"],
            _ => &[],
        };
        let known_metadata = matches!(
            record.kind.as_deref(),
            Some("active-leaf" | "runtime-config" | "custom-title")
        ) && record
            .extra
            .keys()
            .all(|key| allowed_extra.contains(&key.as_str()));
        if !identified || !known_metadata || record.leaf_uuid.is_some() {
            metadata.metadata_only = false;
        }
        if identified
            && known_metadata
            && record.kind.as_deref() == Some("active-leaf")
            && record.leaf_uuid.is_none()
        {
            metadata.empty_leaf_seen = true;
        }
        if let Some(cwd) = record.cwd.filter(|cwd| {
            cwd.len() <= 8192 && Path::new(cwd).is_absolute() && !cwd.chars().any(char::is_control)
        }) {
            metadata.cwd = Some(cwd);
        }
        if record.kind.as_deref() == Some("custom-title") {
            if let Some(title) = record.custom_title.filter(|title| {
                !title.trim().is_empty() && title.len() <= 4096 && !title.contains('\0')
            }) {
                metadata.title = Some(title);
            }
        }
    }
}

fn read_metadata(path: &Path, id: &str, budget: &mut usize) -> Option<Metadata> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).ok()?;
    let stat = file.metadata().ok()?;
    if !stat.is_file() {
        return None;
    }
    let length = usize::try_from(stat.len()).ok()?;
    let prefix = length.min(PREFIX_BYTES);
    let tail = if length > PREFIX_BYTES {
        length.min(TAIL_BYTES)
    } else {
        0
    };
    let charge = prefix.checked_add(tail)?;
    *budget = budget.checked_sub(charge)?;
    let mut bytes = Vec::with_capacity(prefix);
    (&mut file)
        .take(prefix as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let mut metadata = Metadata {
        metadata_only: length <= PREFIX_BYTES,
        ..Default::default()
    };
    let complete_lines = bytes.len() == length && bytes.ends_with(b"\n");
    records(&bytes, length <= PREFIX_BYTES, false, id, &mut metadata);
    if tail > 0 {
        file.seek(SeekFrom::Start((length - tail) as u64)).ok()?;
        bytes.clear();
        (&mut file).take(tail as u64).read_to_end(&mut bytes).ok()?;
        records(&bytes, false, true, id, &mut metadata);
    }
    if metadata.metadata_only {
        let stable = file
            .metadata()
            .ok()
            .zip(std::fs::symlink_metadata(path).ok())
            .is_some_and(|(after, named)| {
                let unchanged = after.len() == stat.len()
                    && named.len() == stat.len()
                    && stat
                        .modified()
                        .ok()
                        .zip(after.modified().ok())
                        .is_some_and(|(a, b)| a == b)
                    && !named.file_type().is_symlink()
                    && named.is_file();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt as _;
                    unchanged && stat.dev() == named.dev() && stat.ino() == named.ino()
                }
                #[cfg(not(unix))]
                {
                    let _ = unchanged;
                    false
                }
            });
        metadata.metadata_only = metadata.empty_leaf_seen && complete_lines && stable;
    }
    (metadata.id_seen && !metadata.invalid).then_some(metadata)
}

fn recover_in(
    mut rows: Vec<Value>,
    root: &Path,
    stop: &CancellationToken,
) -> Result<Vec<Value>, AgentError> {
    cancelled(stop)?;
    if rows.len() > MAX_ROWS {
        return Err(AgentError::Protocol(
            "Qoder session metadata exceeds its row budget".into(),
        ));
    }
    let mut wanted = HashSet::new();
    for row in &mut rows {
        let id = row["sessionId"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or_else(|| AgentError::Protocol("Qoder native session identity is invalid".into()))?
            .to_owned();
        if !wanted.insert(id) {
            return Err(AgentError::Protocol(
                "Qoder native session identities are ambiguous".into(),
            ));
        }
        if let Some(row) = row.as_object_mut() {
            row.remove("_muqunEmptySession");
        }
    }
    let Ok(stat) = std::fs::symlink_metadata(root) else {
        return Ok(rows);
    };
    if stat.file_type().is_symlink() || !stat.is_dir() {
        return Ok(rows);
    }
    let Ok(projects) = std::fs::read_dir(root) else {
        return Ok(rows);
    };
    let mut found: HashMap<String, Option<Metadata>> = HashMap::new();
    let mut budget = TOTAL_BYTES;
    let mut entries = 0;
    for (project_count, project) in projects.enumerate() {
        cancelled(stop)?;
        if project_count >= MAX_PROJECTS {
            break;
        }
        let Ok(project) = project else {
            continue;
        };
        if !project.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files {
            cancelled(stop)?;
            entries += 1;
            if entries > MAX_ENTRIES || budget == 0 {
                break;
            }
            let Ok(file) = file else {
                continue;
            };
            if !file.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let name = file.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id) = name
                .strip_suffix(".jsonl")
                .filter(|id| wanted.contains(*id))
            else {
                continue;
            };
            if found.contains_key(id) {
                found.insert(id.into(), None);
                continue;
            }
            found.insert(id.into(), read_metadata(&file.path(), id, &mut budget));
        }
        if entries > MAX_ENTRIES || budget == 0 {
            break;
        }
    }
    for row in &mut rows {
        let id = row["sessionId"].as_str().unwrap_or_default();
        let Some(Some(metadata)) = found.get(id) else {
            continue;
        };
        // Keep an explicit native cwd; recover only the upstream's empty field.
        if !row["cwd"]
            .as_str()
            .is_some_and(|cwd| Path::new(cwd).is_absolute())
        {
            if let Some(cwd) = &metadata.cwd {
                row["cwd"] = Value::String(cwd.clone());
            }
        }
        if let Some(title) = &metadata.title {
            row["title"] = Value::String(title.clone());
        }
        if metadata.metadata_only {
            row["_muqunEmptySession"] = Value::Bool(true);
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn scratch() -> Scratch {
        let path =
            std::env::temp_dir().join(format!("muqun-qoder-metadata-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }
    fn write(root: &Path, bucket: &str, id: &str, records: &[Value]) -> std::path::PathBuf {
        let dir = root.join(bucket);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let content = records
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, content).unwrap();
        path
    }
    #[test]
    fn store_fingerprint_moves_only_when_saved_sessions_change() {
        let root = scratch();
        assert_eq!(store_fingerprint_in(&root.0.join("absent")), Some(0));
        let path = write(&root.0, "bucket", "one", &[json!({"sessionId":"one"})]);
        let initial = store_fingerprint_in(&root.0).unwrap();
        assert_eq!(store_fingerprint_in(&root.0), Some(initial));
        let mut seen = vec![initial];
        let changes: [&dyn Fn(); 3] = [
            &|| {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                std::io::Write::write_all(&mut file, b"{}\n").unwrap();
            },
            &|| {
                drop(write(
                    &root.0,
                    "other",
                    "two",
                    &[json!({"sessionId":"two"})],
                ))
            },
            &|| std::fs::remove_file(&path).unwrap(),
        ];
        for change in changes {
            change();
            let next = store_fingerprint_in(&root.0).unwrap();
            assert!(!seen.contains(&next));
            seen.push(next);
        }
    }
    #[test]
    #[cfg_attr(windows, ignore = "not yet ported to Windows; see docs/windows.md")]
    fn global_empty_cwd_recovers_exact_transcript_metadata_without_decoding_bucket() {
        let root = scratch();
        write(
            &root.0,
            "ambiguous-native-bucket",
            "opaque",
            &[
                json!({"type":"active-leaf","sessionId":"opaque"}),
                json!({"type":"runtime-config","sessionId":"opaque"}),
                json!({"type":"user","sessionId":"opaque","cwd":"/actual/project-with-hyphens","message":{"content":"private content is not returned"}}),
                json!({"type":"custom-title","sessionId":"opaque","customTitle":"Renamed QA"}),
            ],
        );
        let result = recover_in(
            vec![json!({"sessionId":"opaque","cwd":"","title":"Empty conversation"})],
            &root.0,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result[0]["cwd"], "/actual/project-with-hyphens");
        assert_eq!(result[0]["title"], "Renamed QA");
        assert!(result[0].get("message").is_none());
        assert!(result[0].get("_muqunEmptySession").is_none());
    }
    #[test]
    #[cfg_attr(windows, ignore = "not yet ported to Windows; see docs/windows.md")]
    fn genuinely_empty_metadata_never_guesses_a_workspace() {
        let root = scratch();
        write(
            &root.0,
            "not-a-decodable-path",
            "empty",
            &[
                json!({"type":"active-leaf","sessionId":"empty"}),
                json!({"type":"runtime-config","sessionId":"empty"}),
            ],
        );
        let result = recover_in(
            vec![json!({"sessionId":"empty","cwd":""})],
            &root.0,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result[0]["cwd"], "");
        assert_eq!(result[0]["_muqunEmptySession"], true);
    }
    #[test]
    fn identity_mismatch_and_duplicate_files_do_not_supply_unproven_cwd() {
        let root = scratch();
        write(
            &root.0,
            "one",
            "same",
            &[json!({"type":"user","sessionId":"other","cwd":"/wrong"})],
        );
        let result = recover_in(
            vec![json!({"sessionId":"same","cwd":""})],
            &root.0,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result[0]["cwd"], "");
        write(
            &root.0,
            "one",
            "same",
            &[json!({"type":"user","sessionId":"same","cwd":"/one"})],
        );
        write(
            &root.0,
            "two",
            "same",
            &[json!({"type":"user","sessionId":"same","cwd":"/two"})],
        );
        let result = recover_in(
            vec![json!({"sessionId":"same","cwd":""})],
            &root.0,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result[0]["cwd"], "");
        assert!(recover_in(
            vec![json!({"sessionId":"../escape","cwd":""})],
            &root.0,
            &CancellationToken::new()
        )
        .is_err());
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_buckets_and_transcripts_cannot_escape_native_root() {
        let root = scratch();
        let outside = scratch();
        let target = write(
            &outside.0,
            "one",
            "opaque",
            &[json!({"type":"user","sessionId":"opaque","cwd":"/secret"})],
        );
        std::os::unix::fs::symlink(outside.0.join("one"), root.0.join("linked-bucket")).unwrap();
        std::fs::create_dir(root.0.join("real")).unwrap();
        std::os::unix::fs::symlink(target, root.0.join("real/opaque.jsonl")).unwrap();
        let result = recover_in(
            vec![json!({"sessionId":"opaque","cwd":""})],
            &root.0,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result[0]["cwd"], "");
    }
    #[test]
    fn empty_proof_refuses_unresolved_leaf_fork_references_missing_identity_and_partial_rows() {
        for records in [
            vec![
                json!({"type":"active-leaf","sessionId":"opaque","leafUuid":"unresolved-history"}),
            ],
            vec![
                json!({"type":"active-leaf","sessionId":"opaque","leafUuid":null,"parentSessionId":"source"}),
            ],
            vec![
                json!({"type":"active-leaf","sessionId":"opaque"}),
                json!({"type":"runtime-config"}),
            ],
            vec![json!({"type":"runtime-config","sessionId":"opaque"})],
        ] {
            let root = scratch();
            write(&root.0, "bucket", "opaque", &records);
            let result = recover_in(
                vec![json!({"sessionId":"opaque","cwd":""})],
                &root.0,
                &CancellationToken::new(),
            )
            .unwrap();
            assert_ne!(
                result[0].get("_muqunEmptySession"),
                Some(&Value::Bool(true))
            );
        }
        let root = scratch();
        let path = write(
            &root.0,
            "bucket",
            "opaque",
            &[json!({"type":"active-leaf","sessionId":"opaque"})],
        );
        let mut incomplete = std::fs::read(&path).unwrap();
        incomplete.extend_from_slice(b"{\"type\":\"user\"");
        std::fs::write(path, incomplete).unwrap();
        let result = recover_in(
            vec![json!({"sessionId":"opaque","cwd":""})],
            &root.0,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_ne!(
            result[0].get("_muqunEmptySession"),
            Some(&Value::Bool(true))
        );
    }
}
