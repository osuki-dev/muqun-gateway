//! Bounded read-only views of native Pi JSONL. Record offsets always refer to
//! the original bytes so pagination and native fork/copy can use them verbatim.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(super) enum Record<'a> {
    Raw(&'a [u8]),
    Large(Value),
    Malformed,
}

pub(super) fn scan(
    input: File,
    limit: usize,
    visit: impl FnMut(u64, u64, Record<'_>) -> bool,
) -> io::Result<()> {
    scan_from(input, 0, limit, visit)
}

pub(super) fn scan_from(
    input: File,
    start: u64,
    limit: usize,
    mut visit: impl FnMut(u64, u64, Record<'_>) -> bool,
) -> io::Result<()> {
    let snapshot_len = input.metadata()?.len();
    let mut reader = BufReader::new(input);
    let mut offset = start;
    reader.seek(SeekFrom::Start(start))?;
    let mut line = Vec::new();
    while offset < snapshot_len {
        line.clear();
        let mut bounded = (&mut reader).take((snapshot_len - offset).min(limit as u64 + 1));
        let mut len = bounded.read_until(b'\n', &mut line)? as u64;
        if len == 0 {
            break;
        }
        let record = if len <= limit as u64 {
            Record::Raw(&line)
        } else {
            if line.last() != Some(&b'\n') {
                len += (&mut reader)
                    .take(snapshot_len - offset - len)
                    .skip_until(b'\n')? as u64;
            }
            reader.seek(SeekFrom::Start(offset))?;
            let projected = project_large((&mut reader).take(len))?;
            reader.seek(SeekFrom::Start(offset + len))?;
            projected.map_or(Record::Malformed, Record::Large)
        };
        if !visit(offset, len, record) {
            break;
        }
        offset += len;
    }
    Ok(())
}

// Avoid allocating a native tool output, base64 image, or even a maliciously
// large object key. Only oversized records go through this adapter. Short strings
// (including JSON escapes) pass through verbatim; longer strings are replaced.
const STRING_LIMIT: usize = 8192;
const OMITTED: &[u8] = b"\"[Large Pi payload omitted]\"";

struct ShortStrings<R> {
    source: R,
    pending: Vec<u8>,
    offset: usize,
    string: Vec<u8>,
    in_string: bool,
    escaped: bool,
    protected_key: bool,
    protected_value: bool,
    abbreviated: bool,
    invalid: bool,
}

impl<R: BufRead> ShortStrings<R> {
    fn new(source: R) -> Self {
        Self {
            source,
            pending: Vec::new(),
            offset: 0,
            string: Vec::new(),
            in_string: false,
            escaped: false,
            protected_key: false,
            protected_value: false,
            abbreviated: false,
            invalid: false,
        }
    }
}

// Validate a bounded segment of a large JSON string, retaining an unfinished
// escape, surrogate pair or UTF-8 character for the next segment. At most 12
// bytes are needed for a surrogate pair. No source bytes are silently repaired.
fn validate_segment(bytes: &mut Vec<u8>) -> bool {
    let end = bytes.len();
    bytes.push(b'"');
    for cut in (end.saturating_sub(12)..=end).rev() {
        let saved = std::mem::replace(&mut bytes[cut], b'"');
        let valid = serde_json::from_slice::<String>(&bytes[..=cut]).is_ok();
        bytes[cut] = saved;
        if valid {
            bytes.copy_within(cut..end, 1);
            bytes.truncate(1 + end - cut);
            return true;
        }
    }
    false
}

// The byte spelling of an escaped key is not its identity. In particular,
// "\u0069d" and "id" must both protect the native branch identifier.
fn identity_key(raw: &[u8]) -> bool {
    let decoded;
    let key = if raw.contains(&b'\\') {
        let Ok(s) = serde_json::from_slice::<String>(raw) else {
            return true;
        };
        decoded = s;
        decoded.as_bytes()
    } else {
        &raw[1..raw.len() - 1]
    };
    matches!(
        key,
        b"id"
            | b"parentId"
            | b"type"
            | b"role"
            | b"provider"
            | b"modelId"
            | b"thinkingLevel"
            | b"name"
            | b"parentSession"
            | b"toolCallId"
            | b"toolName"
            | b"timestamp"
    )
}

impl<R: BufRead> Read for ShortStrings<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.offset == self.pending.len() {
            self.pending.clear();
            self.offset = 0;
            let chunk = self.source.fill_buf()?;
            if chunk.is_empty() {
                if self.in_string {
                    // Keep the record invalid for serde rather than reporting a disk error.
                    self.in_string = false;
                    out[0] = b'!';
                    return Ok(1);
                }
                return Ok(0);
            }
            let used = chunk.len().min(16 * 1024);
            for &byte in &chunk[..used] {
                if self.in_string {
                    if !self.invalid {
                        self.string.push(byte);
                    }
                    let closes = !self.escaped && byte == b'"';
                    self.escaped = !self.escaped && byte == b'\\';
                    if closes {
                        self.protected_key = !self.abbreviated && identity_key(&self.string);
                        if self.abbreviated && !self.invalid {
                            self.invalid = serde_json::from_slice::<String>(&self.string).is_err();
                        }
                        self.pending.extend_from_slice(if self.invalid {
                            b"!"
                        } else if self.abbreviated {
                            OMITTED
                        } else {
                            &self.string
                        });
                        self.in_string = false;
                        self.protected_value = false;
                    } else if !self.invalid && self.string.len() > STRING_LIMIT {
                        self.abbreviated = true;
                        self.invalid = self.protected_value || !validate_segment(&mut self.string);
                    }
                } else if byte == b'"' {
                    self.in_string = true;
                    self.abbreviated = false;
                    self.invalid = false;
                    self.string.clear();
                    self.string.push(byte);
                } else {
                    if !byte.is_ascii_whitespace() {
                        self.protected_value = byte == b':' && self.protected_key;
                        self.protected_key = false;
                    }
                    // A long object key must never become the shared display
                    // placeholder. A long array element has no such restriction.
                    if byte == b':' && self.abbreviated {
                        self.pending.push(b'!');
                    }
                    if !byte.is_ascii_whitespace() {
                        self.abbreviated = false;
                    }
                    self.pending.push(byte);
                }
            }
            self.source.consume(used);
        }
        let n = (self.pending.len() - self.offset).min(out.len());
        out[..n].copy_from_slice(&self.pending[self.offset..self.offset + n]);
        self.offset += n;
        Ok(n)
    }
}

// A message may also contain a large array of small blocks. Preserve IDs,
// ordinals and tool-call associations while discarding large block bodies.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    parent_id: Option<String>,
    timestamp: Option<String>,
    provider: Option<String>,
    model_id: Option<String>,
    thinking_level: Option<String>,
    name: Option<String>,
    parent_session: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    role: String,
    timestamp: Option<u64>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    is_error: Option<bool>,
    usage: Option<super::sessions::SummaryUsage>,
    #[serde(default, deserialize_with = "block_previews")]
    content: Value,
}

#[derive(Deserialize, Serialize)]
struct BlockPreview {
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<String>,
}

fn block_previews<'de, D: serde::Deserializer<'de>>(reader: D) -> Result<Value, D::Error> {
    struct Preview;
    impl<'de> serde::de::Visitor<'de> for Preview {
        type Value = Value;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("Pi message text or content blocks")
        }
        fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Value, E> {
            Ok(Value::String(text.to_owned()))
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
            let mut blocks = Vec::new();
            let mut retained = 0;
            while let Some(mut block) = seq.next_element::<BlockPreview>()? {
                for body in [&mut block.text, &mut block.thinking].into_iter().flatten() {
                    if body.len() > 256 {
                        *body = "[Large Pi payload omitted]".into();
                    }
                }
                let value = serde_json::to_value(block).map_err(serde::de::Error::custom)?;
                retained += super::mapper::value_bytes(&value);
                if retained > 2 * 1024 * 1024 || blocks.len() >= 8192 {
                    while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                    // Preserve the distinction between an empty turn and a turn
                    // whose block metadata exceeds the projection budget.
                    return Ok(Value::Null);
                }
                blocks.push(value);
            }
            Ok(Value::Array(blocks))
        }
    }
    reader.deserialize_any(Preview)
}

pub(super) fn project_large(input: impl BufRead) -> io::Result<Option<Value>> {
    let mut shortened = ShortStrings::new(input);
    let mut prefix = Vec::new();
    const PROJECT_LIMIT: u64 = 64 * 1024;
    (&mut shortened)
        .take(PROJECT_LIMIT + 1)
        .read_to_end(&mut prefix)?;
    if prefix.len() as u64 <= PROJECT_LIMIT {
        let Ok(mut entry) = serde_json::from_slice::<Value>(&prefix) else {
            return Ok(None);
        };
        if let Some(blocks) = entry
            .pointer_mut("/message/content")
            .and_then(Value::as_array_mut)
        {
            for block in blocks {
                if block["type"] == "image" && block["data"] == "[Large Pi payload omitted]" {
                    *block = serde_json::json!({"type":"text", "text":"[Large Pi image omitted]"});
                }
            }
        }
        return Ok(Some(entry));
    }
    let stream = prefix.as_slice().chain(shortened);
    let envelope = match serde_json::from_reader::<_, Envelope>(BufReader::new(stream)) {
        Ok(row) => row,
        Err(error) if error.is_io() => return Err(io::Error::other(error)),
        Err(_) => return Ok(None),
    };
    let mut projected = serde_json::to_value(envelope).map_err(io::Error::other)?;
    if projected["message"].is_object() && projected["message"]["content"].is_null() {
        projected["largeHistoryUnavailable"] = true.into();
        projected["message"]["content"] =
            serde_json::json!([{"type":"text","text":"[Large Pi message omitted]"}]);
    }
    Ok(Some(projected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_identity_and_malformed_payload_are_not_replaced_by_valid_records() {
        let payload = "x".repeat(STRING_LIMIT * 2);
        for line in [
            format!(r#"{{"id":"{payload}"}}"#),
            format!(r#"{{"\u0069d":"{payload}"}}"#),
            format!(r#"{{"{payload}":0}}"#),
            format!(r#"{{"content":"{payload}\x"}}"#),
            format!(r#"{{"content":"{payload}"#),
        ] {
            assert!(project_large(line.as_bytes()).unwrap().is_none());
        }
        let array_payload = format!(r#"{{"id":"a","extras":["{payload}"]}}"#);
        assert_eq!(
            project_large(array_payload.as_bytes()).unwrap().unwrap()["id"],
            "a"
        );
        let emoji = r"\ud83d\ude00".repeat(STRING_LIMIT);
        let line = format!(r#"{{"id":"leaf","message":{{"content":"{emoji}"}}}}"#);
        assert_eq!(
            project_large(line.as_bytes()).unwrap().unwrap()["id"],
            "leaf"
        );
    }

    #[test]
    fn over_budget_messages_remain_visible_in_history() {
        let blocks = vec![r#"{"type":"text","text":"block"}"#; 8193].join(",");
        let row = format!(
            r#"{{"type":"message","id":"oversized","message":{{"role":"assistant","content":[{blocks}]}}}}"#
        );
        let projected = project_large(row.as_bytes()).unwrap().unwrap();
        let timeline = super::super::mapper::timeline_from_entries(&[projected]);
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].id, "oversized");
    }

    #[test]
    fn many_small_content_blocks_keep_trailing_ancestry_and_usage() {
        let mut blocks = vec![r#"{"type":"text","text":"block"}"#; 512];
        blocks.push(r#"{"type":"toolCall","id":"call-1","name":"read","arguments":{}}"#);
        // Extra details force the fallback, while the active block list is small.
        let blocks = blocks.join(",");
        let extra = vec!["0"; 64000].join(",");
        let row = format!(
            r#"{{"type":"message","details":[{extra}],"message":{{"role":"assistant","content":[{blocks}],"usage":{{"input":7,"output":2}}}},"id":"leaf","parentId":"root"}}"#
        );
        let entry = project_large(row.as_bytes()).unwrap().unwrap();
        assert_eq!(entry["parentId"], "root");
        assert_eq!(entry["message"]["usage"]["input"], 7);
        assert_eq!(entry["message"]["content"][0]["type"], "text");
        assert_eq!(entry["message"]["content"][512]["id"], "call-1");
        assert!(project_large(&row.as_bytes()[..row.len() - 1])
            .unwrap()
            .is_none());
    }
}
