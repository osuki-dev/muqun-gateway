//! Turning a pane's plain text into the unified content model's parts.
//!
//! Agents draw their transcript with line-start markers -- Claude opens a block
//! with `⏺` and continues its result under `⎿`, Qoder opens with `▪` and
//! continues under `└` -- and those markers are the only structure the terminal
//! preserves. The blocks themselves are the same shape across agents (a marker
//! line naming a tool and its input, then indented result lines), so this module
//! is one state machine plus a per-agent marker dictionary, not one parser per
//! agent. Adding an agent is a new [`Dictionary`], never a new part type.
//!
//! Two rules from `docs/content-model.md` are enforced here rather than left to
//! callers:
//!
//! - **Every part carries `fallback_text`, the source lines verbatim.** A client
//!   that does not know a part's `type` renders that instead, so a marker the
//!   dictionary misses costs structure and never costs content.
//! - **Nothing is dropped.** Every non-blank source line ends up inside exactly
//!   one part's `fallback_text`, in order. That is asserted by a test over the
//!   fixtures, and it is what makes a dictionary that drifts out of date merely
//!   degrade into prose.
//!
//! An agent with no dictionary is not an error: the whole transcript degrades to
//! `text` parts, which is the same thing a client would render for an unknown
//! part type.

use serde_json::{json, Value};

/// What a line-start marker means. The dictionary maps an agent's glyphs onto
/// these, and the state machine below only ever speaks in these terms.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Marker {
    /// Opens a block: a tool call when the rest of the line parses as
    /// `Tool(input)`, otherwise a paragraph of the agent's own prose.
    Block,
    /// Opens a block that is always a tool call: the first word names the tool
    /// and the rest is its input. An agent that gives tool calls their own glyph
    /// needs no other signal, and it prints the outcome on the call line itself,
    /// so such a block has finished rather than being in flight.
    Call,
    /// Opens a result group under the block above it.
    Result,
    /// A continued quote or reasoning line, e.g. Qoder's thinking tree.
    Quote,
    /// The user's own input line.
    Prompt,
    /// One frame of the agent's animated status line.
    Status,
}

/// One agent family's line-start markers.
///
/// Only the glyphs differ between agents; the block grammar does not. Every
/// glyph field is a set because agents rotate spinner frames and print more than
/// one kind of block opener.
pub struct Dictionary {
    /// Stable id on the wire, so a client can tell which dictionary produced a
    /// transcript without the wire format naming an agent-specific construct.
    pub id: &'static str,
    block: &'static [char],
    /// Block openers that are tool calls by construction; see [`Marker::Call`].
    call: &'static [char],
    result: &'static [char],
    quote: &'static [char],
    prompt: &'static [char],
    status: &'static [char],
    /// Head words that name a tool on an ordinary [`Marker::Block`] line, for
    /// agents that write `Ran cargo test` where Claude writes `Bash(cargo
    /// test)`. Only consulted when the block actually produced a result group,
    /// which is what keeps a sentence that opens with the same word as prose.
    verbs: &'static [&'static str],
    /// Whether the user's message is drawn as a gutter -- the prompt marker
    /// repeated on every line at one indent -- rather than as a single marker
    /// line with its wrapping indented under it.
    prompt_gutter: bool,
}

/// The fields every dictionary leaves empty unless the agent needs them, so a
/// new table lists only the glyphs that agent actually draws.
const EMPTY: Dictionary = Dictionary {
    id: "",
    block: &[],
    call: &[],
    result: &[],
    quote: &[],
    prompt: &[],
    status: &[],
    verbs: &[],
    prompt_gutter: false,
};

/// Claude Code. `⏺` opens a block, `⎿` opens each result group under it, `❯` is
/// the user's line, and the spinner frames rotate through a handful of glyphs.
const CLAUDE: Dictionary = Dictionary {
    id: "claude",
    block: &['⏺'],
    result: &['⎿'],
    prompt: &['❯'],
    status: &['✻', '✽', '✳', '✢', '∗', '·'],
    ..EMPTY
};

/// Qoder CLI. `▪` opens a block and `●` opens a background-task notice; `└`
/// opens the result, and `│` continues the thinking tree. Qoder draws its input
/// as a boxed widget rather than as a prompt line, so it has no prompt marker
/// and those rows degrade to text.
const QODER: Dictionary = Dictionary {
    id: "qoder",
    block: &['▪', '●'],
    result: &['└'],
    quote: &['│'],
    ..EMPTY
};

/// OpenAI Codex CLI. `•` opens a block -- prose, or a tool call written as a
/// verb -- `⚠` opens a startup notice drawn the same way, `└` opens the result,
/// and `›` is the user's line. `│` continues a command that carried a newline,
/// and is also how Codex draws the sides of its `/status` box; both are chrome
/// around text, which is what the quote marker means.
///
/// `Ran` and `Explored` are the only heads promoted to tool calls. Codex writes
/// its prose in the first person, so the risk is small, and the result-group
/// requirement removes the rest of it.
///
/// The status glyph is `◦`, not `•`: Herdr's `codex.toml` detection manifest
/// matches the live line as `^[•◦]\s+Working \(…esc to interrupt\)`, and `•` is
/// already the block opener. A `• Working (…)` line therefore lands as text --
/// the head names no tool -- which costs a status part and no content.
const CODEX: Dictionary = Dictionary {
    id: "codex",
    block: &['•', '⚠'],
    result: &['└'],
    quote: &['│'],
    prompt: &['›'],
    status: &['◦'],
    verbs: &["Ran", "Explored"],
    ..EMPTY
};

/// opencode. `→` opens a tool call and nothing else, so it is a call glyph: the
/// first word is the tool and the rest is its input. opencode prints the outcome
/// on that same line, which is why a call block with no result group is done
/// rather than running. `↳` opens the occasional note under one, `+` opens a
/// reasoning line (`+ Thought: 480ms`), and `┃` is the gutter opencode draws
/// down the left of the user's message and of its own editor box.
///
/// No status glyph: opencode's `▣ Build · … · 20.7s` footer is drawn inside that
/// gutter rather than at the margin, and the margin is what tells a status line
/// apart from a bullet in somebody's output. It degrades to text.
const OPENCODE: Dictionary = Dictionary {
    id: "opencode",
    call: &['→'],
    result: &['↳'],
    quote: &['+'],
    prompt: &['┃'],
    prompt_gutter: true,
    ..EMPTY
};

/// Matched as a substring against the agent name Herdr reports, the same
/// discipline `shortcuts.rs` uses: "Claude Code", "claude-code" and "claude" all
/// resolve to one dictionary, and "qodercli" to the other. The aliases mirror
/// the `id` and `aliases` of Herdr's agent-detection manifests
/// (`~/.local/state/herdr/agent-detection/`), which is the ecosystem-maintained
/// list this table tracks.
const DICTIONARIES: &[(&[&str], &Dictionary)] = &[
    (&["claude"], &CLAUDE),
    (&["qoder"], &QODER),
    (&["codex"], &CODEX),
    (&["opencode", "open-code"], &OPENCODE),
];

/// Which dictionary, if any, normalizes this agent's output.
///
/// `None` is a supported answer, not a failure: the caller still gets parts, all
/// of them `text`.
pub fn dictionary_for(agent: Option<&str>) -> Option<&'static Dictionary> {
    let agent = agent?.trim().to_ascii_lowercase();
    if agent.is_empty() {
        return None;
    }
    DICTIONARIES
        .iter()
        .find(|(names, _)| names.iter().any(|name| agent.contains(name)))
        .map(|(_, dictionary)| *dictionary)
}

/// One item of a `todo` part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    text: String,
    done: bool,
}

impl TodoItem {
    pub fn new(text: impl Into<String>, done: bool) -> Self {
        TodoItem {
            text: text.into(),
            done,
        }
    }
}

/// One answer offered by an `approval` part.
///
/// `decision` is the closed vocabulary `approvals.rs` already reads off a drawn
/// menu (`allow`, `allow_always`, `deny`, `other`), so a client dispatches on
/// one set of words whichever source raised the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalChoice {
    pub index: u32,
    pub label: String,
    pub decision: &'static str,
}

/// A permission request an agent is blocked on, carried inside the transcript.
///
/// Only a source that *reports* approval state can fill this in; a marker
/// dictionary reads a drawn menu off the screen instead, and that keeps riding
/// the approvals endpoint (see `docs/content-model.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// The id to answer with. Opaque to the client and to this module.
    pub id: String,
    /// The question. Written by the gateway from the protocol's own action
    /// name, never quoted out of a terminal.
    pub prompt: String,
    /// The action or tool the request is about, as the protocol named it.
    pub tool: Option<String>,
    /// What is being asked for -- the command, the path, the host -- verbatim
    /// from the protocol.
    pub context: Vec<String>,
    pub options: Vec<ApprovalChoice>,
}

/// A tool block's outcome, decided from its result rather than from any exit
/// code: the terminal never carried one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Ok,
    Error,
    Running,
}

impl ToolStatus {
    fn as_str(self) -> &'static str {
        match self {
            ToolStatus::Ok => "ok",
            ToolStatus::Error => "error",
            ToolStatus::Running => "running",
        }
    }
}

/// The typed payload of a part. The set is closed on purpose: a client that
/// knows these renders them, and anything the dictionary could not type arrives
/// as `Text`, never as a new type it has to guess at.
#[derive(Debug, Clone, PartialEq)]
enum Body {
    Text {
        markdown: String,
    },
    ToolBlock {
        tool: String,
        input: String,
        result: Vec<String>,
        status: ToolStatus,
        truncated: bool,
    },
    Diff {
        file: Option<String>,
        hunks: Vec<String>,
    },
    Todo {
        items: Vec<TodoItem>,
    },
    Status {
        text: String,
        spinner: bool,
    },
    Prompt {
        text: String,
    },
    /// v2 (schema 1.4.0). A closed-set addition: an old client does not know
    /// the type and renders `fallback_text`, which is the request drawn as the
    /// numbered menu it would have seen on the terminal anyway.
    Approval {
        request: ApprovalRequest,
    },
}

/// One normalized part, plus the source rows it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    body: Body,
    /// The source lines verbatim. The contract's fallback path: a client that
    /// does not know `type` renders this.
    fallback_text: String,
    /// Inclusive 0-based source row span, so a client can correlate a part with
    /// the raw terminal view. Spans never overlap and never run backwards, which
    /// is what lets a client map a terminal row to the part that owns it.
    start: usize,
    end: usize,
}

impl Part {
    /// The wire's `type`. Only the tests read it back off a part -- the gateway
    /// itself hands the JSON straight to the client.
    #[cfg(test)]
    pub fn kind(&self) -> &'static str {
        match self.body {
            Body::Text { .. } => "text",
            Body::ToolBlock { .. } => "tool-block",
            Body::Diff { .. } => "diff",
            Body::Todo { .. } => "todo",
            Body::Status { .. } => "status",
            Body::Prompt { .. } => "prompt",
            Body::Approval { .. } => "approval",
        }
    }

    #[cfg(test)]
    pub fn fallback_text(&self) -> &str {
        &self.fallback_text
    }

    pub fn to_json(&self) -> Value {
        let mut value = match &self.body {
            Body::Text { markdown } => json!({ "type": "text", "markdown": markdown }),
            Body::ToolBlock {
                tool,
                input,
                result,
                status,
                truncated,
            } => json!({
                "type": "tool-block",
                "tool": tool,
                "input": input,
                "result": result,
                "status": status.as_str(),
                "truncated": truncated,
            }),
            Body::Diff { file, hunks } => json!({
                "type": "diff",
                "file": file,
                "hunks": hunks,
            }),
            Body::Todo { items } => json!({
                "type": "todo",
                "items": items
                    .iter()
                    .map(|item| json!({ "text": item.text, "done": item.done }))
                    .collect::<Vec<Value>>(),
            }),
            Body::Status { text, spinner } => json!({
                "type": "status",
                "text": text,
                "spinner": spinner,
            }),
            Body::Prompt { text } => json!({ "type": "prompt", "text": text }),
            Body::Approval { request } => json!({
                "type": "approval",
                "approval_id": request.id,
                "prompt": request.prompt,
                "tool": request.tool,
                "context": request.context,
                "options": request
                    .options
                    .iter()
                    .map(|option| json!({
                        "index": option.index,
                        "label": option.label,
                        "decision": option.decision,
                    }))
                    .collect::<Vec<Value>>(),
            }),
        };
        let map = value.as_object_mut().expect("part serializes to an object");
        map.insert("fallback_text".into(), json!(self.fallback_text));
        map.insert(
            "range".into(),
            json!({ "start": self.start, "end": self.end }),
        );
        value
    }
}

/// Builds parts from a source that is not a terminal.
///
/// A native protocol hands structure over directly, so there are no source rows
/// to span and no drawn lines to fall back to -- and both invariants in this
/// module's header are stated in exactly those terms. Rather than exempt the
/// native path from them, an adapter *renders* each part into lines and this
/// type makes that rendering the source: the lines go into one transcript, the
/// part's span is pinned to the rows they landed on, and its `fallback_text` is
/// those rows verbatim. Both invariants then hold by construction, and the same
/// tests that check a dictionary check an adapter unchanged.
///
/// The rendering is deliberately the shape the dictionaries read back off a
/// terminal -- `Tool(input)` over indented results, `☐`/`☑` for a checklist --
/// so a client showing `fallback_text` cannot tell which source answered.
#[derive(Default)]
pub struct Assembler {
    lines: Vec<String>,
    parts: Vec<Part>,
}

impl Assembler {
    pub fn new() -> Self {
        Assembler::default()
    }

    /// Append a part and the rows it renders to. A part that renders to nothing
    /// is dropped rather than pinned to an empty span.
    fn push(&mut self, body: Body, rendered: Vec<String>) {
        if rendered.iter().all(|line| line.trim().is_empty()) {
            return;
        }
        // One blank row between parts, so the transcript a client would see if
        // it pasted every `fallback_text` together reads the way a pane does.
        // Blank rows belong to no part, which is what the coverage invariant
        // says about a terminal read too.
        if !self.lines.is_empty() {
            self.lines.push(String::new());
        }
        let start = self.lines.len();
        self.lines.extend(rendered);
        let end = self.lines.len() - 1;
        self.parts.push(Part {
            body,
            fallback_text: self.lines[start..=end].join("\n"),
            start,
            end,
        });
    }

    pub fn text(&mut self, markdown: &str) {
        let markdown = markdown.trim_end().to_owned();
        let rendered = markdown.lines().map(str::to_owned).collect();
        self.push(Body::Text { markdown }, rendered);
    }

    pub fn prompt(&mut self, text: &str) {
        let text = text.trim_end().to_owned();
        let rendered = text.lines().map(str::to_owned).collect();
        self.push(Body::Prompt { text }, rendered);
    }

    pub fn tool_block(
        &mut self,
        tool: &str,
        input: &str,
        result: Vec<String>,
        status: ToolStatus,
        truncated: bool,
    ) {
        let mut rendered: Vec<String> = format!("{tool}({input})")
            .lines()
            .map(str::to_owned)
            .collect();
        rendered.extend(result.iter().map(|line| format!("  {line}")));
        self.push(
            Body::ToolBlock {
                tool: tool.to_owned(),
                input: input.to_owned(),
                result,
                status,
                truncated,
            },
            rendered,
        );
    }

    pub fn diff(&mut self, file: Option<String>, hunks: Vec<String>) {
        let rendered = hunks.clone();
        self.push(Body::Diff { file, hunks }, rendered);
    }

    pub fn todo(&mut self, items: Vec<TodoItem>) {
        let rendered = items
            .iter()
            .map(|item| {
                let box_glyph = if item.done { '☑' } else { '☐' };
                format!("{box_glyph} {}", item.text)
            })
            .collect();
        self.push(Body::Todo { items }, rendered);
    }

    /// A pending permission request. Rendered as the numbered menu an agent
    /// would have drawn, so a client that does not know the type still shows
    /// the user what is being asked and what the answers are.
    pub fn approval(&mut self, request: ApprovalRequest) {
        let mut rendered = vec![request.prompt.clone()];
        rendered.extend(request.context.iter().map(|line| format!("  {line}")));
        rendered.extend(
            request
                .options
                .iter()
                .map(|option| format!("{}. {}", option.index, option.label)),
        );
        self.push(Body::Approval { request }, rendered);
    }

    /// The rows every part was pinned to. The native path's answer to "the raw
    /// terminal view", and what the invariant tests read.
    pub fn transcript(&self) -> String {
        self.lines.join("\n")
    }

    pub fn finish(self) -> Vec<Part> {
        self.parts
    }
}

/// Normalize a pane's text into parts, or into plain paragraphs when the agent
/// has no dictionary.
pub fn normalize(text: &str, dictionary: Option<&Dictionary>) -> Vec<Part> {
    let Some(dictionary) = dictionary else {
        return paragraphs(text);
    };
    let rows: Vec<Row> = text
        .lines()
        .map(|line| classify(line, dictionary))
        .collect();
    let mut parts = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        if rows[index].blank {
            index += 1;
            continue;
        }
        index = match rows[index].marker {
            Some(Marker::Block) | Some(Marker::Call) => {
                read_block(&rows, index, dictionary, &mut parts)
            }
            Some(Marker::Prompt) => read_prompt(&rows, index, dictionary, &mut parts),
            Some(Marker::Status) => {
                parts.push(status_part(&rows, index));
                index + 1
            }
            Some(Marker::Result) | Some(Marker::Quote) | None => {
                read_text(&rows, index, &mut parts)
            }
        };
    }
    parts
}

/// Parts as they go on the wire.
pub fn normalize_json(text: &str, dictionary: Option<&Dictionary>) -> Vec<Value> {
    normalize(text, dictionary)
        .iter()
        .map(Part::to_json)
        .collect()
}

/// One source line, pre-classified so the state machine never re-scans it.
struct Row<'a> {
    raw: &'a str,
    /// Leading whitespace in columns. Terminal indentation is ASCII, so bytes
    /// and columns agree here.
    indent: usize,
    marker: Option<Marker>,
    blank: bool,
}

fn classify<'a>(raw: &'a str, dictionary: &Dictionary) -> Row<'a> {
    let trimmed = raw.trim_start();
    let indent = raw.len() - trimmed.len();
    let mut chars = trimmed.chars();
    let first = chars.next();
    // A marker glyph only marks when it stands alone at the head of the line.
    // Requiring the space keeps a glyph that happens to open a word -- or a box
    // rule drawn out of the same code page -- from being read as structure.
    let standalone = chars.next().is_none_or(char::is_whitespace);
    let marker = first.filter(|_| standalone).and_then(|glyph| {
        if dictionary.block.contains(&glyph) {
            Some(Marker::Block)
        } else if dictionary.call.contains(&glyph) {
            Some(Marker::Call)
        } else if dictionary.result.contains(&glyph) {
            Some(Marker::Result)
        } else if dictionary.quote.contains(&glyph) {
            Some(Marker::Quote)
        } else if dictionary.prompt.contains(&glyph) {
            Some(Marker::Prompt)
        } else if dictionary.status.contains(&glyph) && indent == 0 {
            // The status line is drawn at the left margin; the same glyph
            // indented is a bullet in someone's output.
            Some(Marker::Status)
        } else {
            None
        }
    });
    Row {
        raw,
        indent,
        marker,
        blank: trimmed.is_empty(),
    }
}

/// The next row that continues a construct opened at `indent`, skipping blank
/// rows. A blank line does not end a block on its own -- agents put blank lines
/// between the paragraphs of one message -- but a line back at or left of the
/// opening indent does.
fn next_continuation(rows: &[Row], from: usize, indent: usize) -> Option<usize> {
    let next = (from..rows.len()).find(|&index| !rows[index].blank)?;
    (rows[next].indent > indent).then_some(next)
}

/// Everything after the marker glyph and the padding that follows it. Trimmed
/// rather than sliced by a fixed width because agents pad with more than one
/// space, and Claude pads with a non-breaking one.
fn strip_marker(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    let mut chars = trimmed.chars();
    chars.next();
    chars.as_str().trim_start()
}

/// Remove `columns` of leading whitespace, keeping any indentation beyond it:
/// the result lines of a block are padded to clear its marker, and the padding
/// is chrome while the indentation inside them is content.
fn dedent(raw: &str, columns: usize) -> &str {
    if raw.len() >= columns && raw[..columns].bytes().all(|byte| byte == b' ') {
        &raw[columns..]
    } else {
        raw.trim_start()
    }
}

/// The narrowest indentation across the given rows, which is the padding they
/// all share and therefore the padding to remove.
fn shared_indent(rows: &[Row], indices: &[usize]) -> usize {
    indices
        .iter()
        .filter(|&&index| !rows[index].blank)
        .map(|&index| rows[index].indent)
        .min()
        .unwrap_or(0)
}

fn raw_text(rows: &[Row], indices: &[usize]) -> String {
    indices
        .iter()
        .map(|&index| rows[index].raw)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A head line plus its continuation, with the marker and the shared padding
/// removed: what a client should render as markdown.
fn body_lines(rows: &[Row], indices: &[usize]) -> Vec<String> {
    let (&head, tail) = match indices.split_first() {
        Some(split) => split,
        None => return Vec::new(),
    };
    let padding = shared_indent(rows, tail);
    let mut lines = vec![strip_marker(rows[head].raw).to_owned()];
    lines.extend(tail.iter().map(|&index| {
        let line = dedent(rows[index].raw, padding);
        // A quote glyph continuing a head is the agent's own gutter -- Codex
        // draws one down the left of a command that carried a newline -- so it
        // is chrome here just as it is around prose.
        match rows[index].marker {
            Some(Marker::Quote) => strip_marker(line),
            _ => line,
        }
        .to_owned()
    }));
    lines
}

/// The rows of one result group under a block.
struct Group {
    rows: Vec<usize>,
}

/// Read a block: the marker line, whatever continues it, and every result group
/// underneath. Tool blocks and prose blocks share this shape, which is why one
/// reader serves both.
fn read_block(rows: &[Row], start: usize, dictionary: &Dictionary, out: &mut Vec<Part>) -> usize {
    let base = rows[start].indent;
    let call = rows[start].marker == Some(Marker::Call);
    let mut head: Vec<usize> = vec![start];
    let mut groups: Vec<Group> = Vec::new();
    let mut index = start + 1;

    while let Some(next) = next_continuation(rows, index, base) {
        if rows[next].marker == Some(Marker::Result) {
            let mut group = Group { rows: vec![next] };
            let mut inner = next + 1;
            while let Some(line) = next_continuation(rows, inner, rows[next].indent) {
                group.rows.extend(inner..line);
                group.rows.push(line);
                inner = line + 1;
            }
            index = inner;
            groups.push(group);
        } else {
            // A continuation that is not a result line extends whatever is open:
            // the tool's input or the prose before any result arrived, and
            // otherwise the result group it trails.
            let target = match groups.last_mut() {
                Some(group) => &mut group.rows,
                None => &mut head,
            };
            target.extend(index..next);
            target.push(next);
            index = next + 1;
        }
    }

    // A block yields one part per shape, in source order: the block itself, then
    // whichever of its result groups turned out to be a checklist or a diff.
    // Groups are walked in order and the block is flushed the moment the first
    // one splits off, so a part's rows stay contiguous and reading the parts in
    // order reads the transcript in order. A plain group that trails a split is
    // its own text part rather than jumping back into the block above it.
    // Which of the three head shapes named a tool is only decidable now: the
    // verb form is trusted only when the block produced a result group.
    let head_text = strip_marker(rows[start].raw);
    let tool = parse_tool_head(head_text).or_else(|| {
        let verb = !groups.is_empty()
            && dictionary
                .verbs
                .iter()
                .any(|&verb| heads_with(head_text, verb));
        (call || verb).then(|| word_tool_head(head_text)).flatten()
    });

    // The file a split-out diff belongs to is whatever the tool was called with.
    let edited = tool.as_ref().map(|head| head.input.clone());
    let mut tool = tool;
    let mut base_rows = head.clone();
    let mut result: Vec<String> = Vec::new();
    let mut split: Vec<Part> = Vec::new();
    for group in &groups {
        let lines = body_lines(rows, &group.rows);
        let end = *group.rows.last().unwrap_or(&group.rows[0]);
        let body = match classify_group(&lines) {
            GroupKind::Todo(items) => Body::Todo { items },
            GroupKind::Diff(hunks) => Body::Diff {
                file: edited.clone(),
                hunks,
            },
            GroupKind::Plain if !split.is_empty() => Body::Text {
                markdown: lines.join("\n"),
            },
            GroupKind::Plain => {
                result.extend(lines);
                base_rows.extend(group.rows.iter().copied());
                continue;
            }
        };
        if split.is_empty() {
            let head_tool = tool.take();
            split.push(block_part(
                rows,
                &head,
                &base_rows,
                head_tool,
                &mut result,
                groups.is_empty() && !call,
            ));
        }
        split.push(Part {
            body,
            fallback_text: raw_text(rows, &group.rows),
            start: group.rows[0],
            end,
        });
    }
    if split.is_empty() {
        split.push(block_part(
            rows,
            &head,
            &base_rows,
            tool,
            &mut result,
            groups.is_empty() && !call,
        ));
    }
    // Built base-first within the loop; source order is by first row.
    split.sort_by_key(|part| part.start);
    out.append(&mut split);
    index
}

/// The block itself, once its result groups have been sorted into what stays
/// with it and what splits off.
fn block_part(
    rows: &[Row],
    head: &[usize],
    base_rows: &[usize],
    tool: Option<ToolHead>,
    result: &mut Vec<String>,
    unanswered: bool,
) -> Part {
    let fallback_text = raw_text(rows, base_rows);
    let truncated = fallback_text.contains('…');
    let result = std::mem::take(result);
    let body = match tool {
        Some(head_tool) => {
            // Everything the head collected past its own line is the rest of a
            // command that carried a newline, and the paren that closes it is
            // down there rather than on the marker line.
            let mut input = vec![head_tool.input];
            input.extend(
                body_lines(rows, head)
                    .into_iter()
                    .skip(1)
                    .filter(|line| !line.is_empty()),
            );
            let mut input = input.join("\n");
            if !head_tool.closed {
                input = input.strip_suffix(')').unwrap_or(&input).to_owned();
            }
            Body::ToolBlock {
                tool: head_tool.name,
                input,
                status: tool_status(&result, unanswered),
                result,
                truncated,
            }
        }
        None => {
            let mut markdown = body_lines(rows, head);
            markdown.extend(result);
            Body::Text {
                markdown: markdown.join("\n"),
            }
        }
    };
    Part {
        body,
        fallback_text,
        start: base_rows[0],
        end: *base_rows.last().unwrap_or(&base_rows[0]),
    }
}

/// The user's own line, and the lines of a message that wrapped onto more of
/// them. A result group under a prompt belongs to the command it ran, not to
/// what the user typed, so it is left for the caller to read as text.
fn read_prompt(rows: &[Row], start: usize, dictionary: &Dictionary, out: &mut Vec<Part>) -> usize {
    let base = rows[start].indent;
    let mut used = vec![start];
    let mut index = start + 1;
    let text = if dictionary.prompt_gutter {
        // Every row of the message repeats the marker at one indent, so the run
        // ends at the first row that does not -- there is no indented wrapping
        // to look for, and a blank row is the end of the widget rather than a
        // paragraph break inside it.
        while index < rows.len()
            && rows[index].marker == Some(Marker::Prompt)
            && rows[index].indent == base
        {
            used.push(index);
            index += 1;
        }
        let lines: Vec<&str> = used
            .iter()
            .map(|&row| strip_marker(rows[row].raw))
            .collect();
        // The gutter is drawn a row taller than the message on both sides.
        let body = lines
            .iter()
            .position(|line| !line.is_empty())
            .map(|first| {
                let last = lines
                    .iter()
                    .rposition(|line| !line.is_empty())
                    .unwrap_or(first);
                &lines[first..=last]
            })
            .unwrap_or(&[]);
        body.join("\n")
    } else {
        while let Some(next) = next_continuation(rows, index, base) {
            if rows[next].marker == Some(Marker::Result) {
                break;
            }
            used.extend(index..next);
            used.push(next);
            index = next + 1;
        }
        body_lines(rows, &used).join("\n")
    };
    let end = *used.last().unwrap_or(&start);
    out.push(Part {
        body: Body::Prompt { text },
        fallback_text: raw_text(rows, &used),
        start,
        end,
    });
    end + 1
}

fn status_part(rows: &[Row], index: usize) -> Part {
    Part {
        body: Body::Status {
            text: strip_marker(rows[index].raw).to_owned(),
            // The dictionary's status glyphs are the frames of the animated
            // line, so a line that matched one is by construction a live one.
            spinner: true,
        },
        fallback_text: rows[index].raw.to_owned(),
        start: index,
        end: index,
    }
}

/// A run of lines the dictionary could not type. Merged into one `text` part up
/// to the next blank line or the next block, which keeps a paragraph together
/// without swallowing the whole transcript when a dictionary drifts.
fn read_text(rows: &[Row], start: usize, out: &mut Vec<Part>) -> usize {
    if checkbox(rows[start].raw.trim_start()).is_some() {
        let mut used = vec![start];
        let mut index = start + 1;
        while index < rows.len() && checkbox(rows[index].raw.trim_start()).is_some() {
            used.push(index);
            index += 1;
        }
        // No marker to strip here: the checkbox itself opens every line.
        let items = used
            .iter()
            .filter_map(|&row| todo_item(rows[row].raw))
            .collect();
        out.push(Part {
            body: Body::Todo { items },
            fallback_text: raw_text(rows, &used),
            start,
            end: index - 1,
        });
        return index;
    }

    let mut used = vec![start];
    let mut index = start + 1;
    while index < rows.len()
        && !rows[index].blank
        && !matches!(
            rows[index].marker,
            Some(Marker::Block) | Some(Marker::Prompt) | Some(Marker::Status)
        )
    {
        used.push(index);
        index += 1;
    }
    let padding = shared_indent(rows, &used);
    let lines: Vec<String> = used
        .iter()
        .map(|&row| {
            let line = dedent(rows[row].raw, padding);
            match rows[row].marker {
                // A quote or an orphaned result marker is chrome around prose.
                Some(Marker::Quote) | Some(Marker::Result) => strip_marker(line),
                _ => line,
            }
            .to_owned()
        })
        .collect();
    // A read that starts part-way down the scrollback catches diffs whose block
    // header has already scrolled off. Those rows are still a diff, and typing
    // them costs nothing: the same climbing-gutter test that rejects a listing
    // rejects prose. A checklist is not treated the same way, because a run that
    // is only partly checkboxes would lose the prose around them.
    let body = match classify_group(&lines) {
        GroupKind::Diff(hunks) => Body::Diff { file: None, hunks },
        _ => Body::Text {
            markdown: lines.join("\n"),
        },
    };
    out.push(Part {
        body,
        fallback_text: raw_text(rows, &used),
        start,
        end: index - 1,
    });
    index
}

/// What a block's marker line said, when it said a tool ran.
struct ToolHead {
    name: String,
    input: String,
    /// Whether the input closed on this line. A command with an embedded newline
    /// does not, and the rest of it arrives as continuation lines.
    closed: bool,
}

/// A block head reads as a tool call when it names one and opens a paren:
/// `Bash(cargo test)`. Anything else -- "Update Todos", a sentence, a line of
/// Chinese prose -- is the agent talking, and becomes text.
fn parse_tool_head(head: &str) -> Option<ToolHead> {
    let open = head.find('(')?;
    let name = &head[..open];
    if name.is_empty() || name.len() > 40 {
        return None;
    }
    if !name.starts_with(|glyph: char| glyph.is_ascii_alphabetic()) {
        return None;
    }
    if !name
        .chars()
        .all(|glyph| glyph.is_ascii_alphanumeric() || matches!(glyph, '_' | '-' | '.'))
    {
        return None;
    }
    let body = &head[open + 1..];
    // A truncated input has no closing paren at all, which is not an error: the
    // ellipsis the agent printed is what sets `truncated`.
    let closed = body.ends_with(')');
    Some(ToolHead {
        name: name.to_owned(),
        input: body.strip_suffix(')').unwrap_or(body).to_owned(),
        closed,
    })
}

/// Whether the head opens with exactly this word. `Ran cargo test` heads with
/// `Ran`; `Rank the files` does not, and neither does a head that is the word
/// alone with nothing after it in an agent that always prints an argument --
/// that case is left to the caller, since `Explored` legitimately stands alone.
fn heads_with(head: &str, verb: &str) -> bool {
    head.strip_prefix(verb)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// A head written as `Verb argument` rather than as `Tool(argument)`: the first
/// word is the tool and the rest is its input. The name is held to the same
/// shape [`parse_tool_head`] holds `Tool(` to, so a head that opens with prose
/// -- or with a path, or with a glyph -- still reads as text.
fn word_tool_head(head: &str) -> Option<ToolHead> {
    let (name, input) = match head.find(char::is_whitespace) {
        Some(split) => (&head[..split], head[split..].trim_start()),
        None => (head, ""),
    };
    if name.is_empty() || name.len() > 40 {
        return None;
    }
    if !name.starts_with(|glyph: char| glyph.is_ascii_alphabetic()) {
        return None;
    }
    if !name
        .chars()
        .all(|glyph| glyph.is_ascii_alphanumeric() || matches!(glyph, '_' | '-' | '.'))
    {
        return None;
    }
    Some(ToolHead {
        name: name.to_owned(),
        input: input.to_owned(),
        // Nothing here is closed by a paren, so the trailing-paren repair that
        // `Tool(` needs must not run.
        closed: true,
    })
}

/// Read the outcome off the result, since the terminal never carried an exit
/// code. A block still waiting for its first result line is running.
fn tool_status(result: &[String], unanswered: bool) -> ToolStatus {
    if unanswered {
        return ToolStatus::Running;
    }
    let first = result.iter().find(|line| !line.trim().is_empty());
    match first {
        Some(line) if is_error_line(line) => ToolStatus::Error,
        _ => ToolStatus::Ok,
    }
}

fn is_error_line(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with('✗')
        || line.starts_with('✘')
        || line.starts_with('✖')
        || line
            .get(..5)
            .is_some_and(|head| head.eq_ignore_ascii_case("error"))
}

enum GroupKind {
    Todo(Vec<TodoItem>),
    Diff(Vec<String>),
    Plain,
}

/// Decide whether a result group is really a checklist or a diff. Both are
/// printed as ordinary result lines, so the only signal is that most of the
/// group's lines share one shape; a majority rather than every line, because
/// agents head these groups with a summary ("Added 363 lines").
fn classify_group(lines: &[String]) -> GroupKind {
    let filled: Vec<&String> = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if filled.is_empty() {
        return GroupKind::Plain;
    }
    let checkboxes = filled
        .iter()
        .filter(|line| checkbox(line.trim_start()).is_some())
        .count();
    if checkboxes >= 1 && checkboxes * 5 >= filled.len() * 3 {
        return GroupKind::Todo(filled.iter().filter_map(|line| todo_item(line)).collect());
    }
    let numbered: Vec<(u32, &&String)> = filled
        .iter()
        .filter_map(|line| Some((diff_line_number(line)?, line)))
        .collect();
    // Line numbers only ever climb. Requiring that is what separates a diff from
    // a result that merely opens with counts ("13 warnings", "0 errors").
    let climbing = numbered.windows(2).all(|pair| pair[0].0 <= pair[1].0);
    if numbered.len() >= 2 && climbing && numbered.len() * 5 >= filled.len() * 3 {
        return GroupKind::Diff(numbered.iter().map(|(_, line)| (**line).clone()).collect());
    }
    GroupKind::Plain
}

/// `☐`/`☑` and the variants agents use for a cancelled or completed item.
///
/// Ballot boxes only. A bare check mark is what half the world prints in front
/// of a success line -- "✓ started #590" is a result, not a checklist -- and
/// accepting it turned ordinary tool output into todo parts.
fn checkbox(line: &str) -> Option<bool> {
    match line.chars().next()? {
        '☐' | '⬚' | '□' => Some(false),
        '☑' | '☒' | '■' => Some(true),
        _ => None,
    }
}

fn todo_item(line: &str) -> Option<TodoItem> {
    let line = line.trim_start();
    let done = checkbox(line)?;
    let mut chars = line.chars();
    chars.next();
    Some(TodoItem {
        text: chars.as_str().trim().to_owned(),
        done,
    })
}

/// The line number off a diff line, if this is one.
///
/// Agents print the number as a gutter: it is followed by the padding that
/// aligns the source, or by the `+`/`-` of a changed line, and never by a word.
/// Insisting on that keeps a date ("2026-07-21") and a counted result line
/// ("13 warnings") out of the diff.
fn diff_line_number(line: &str) -> Option<u32> {
    let line = line.trim_start();
    let digits = line
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if !(1..=7).contains(&digits) {
        return None;
    }
    let rest = &line[digits..];
    let gutter = rest.is_empty()
        || rest.starts_with("  ")
        || rest.starts_with(" +")
        || rest.starts_with(" -");
    gutter.then(|| line[..digits].parse().ok())?
}

/// The no-dictionary path: paragraphs of text, so an agent nobody has written a
/// dictionary for still renders, and renders in the same envelope.
fn paragraphs(text: &str) -> Vec<Part> {
    let lines: Vec<&str> = text.lines().collect();
    let mut parts = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim().is_empty() {
            index += 1;
            continue;
        }
        let start = index;
        while index < lines.len() && !lines[index].trim().is_empty() {
            index += 1;
        }
        let block = &lines[start..index];
        let padding = block
            .iter()
            .map(|line| line.len() - line.trim_start().len())
            .min()
            .unwrap_or(0);
        parts.push(Part {
            body: Body::Text {
                markdown: block
                    .iter()
                    .map(|line| dedent(line, padding))
                    .collect::<Vec<_>>()
                    .join("\n"),
            },
            fallback_text: block.join("\n"),
            start,
            end: index - 1,
        });
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_FIXTURE: &str = include_str!("../tests/fixtures/claude-transcript.txt");
    const CLAUDE_SNAPSHOT: &str = include_str!("../tests/fixtures/claude-parts.json");
    const QODER_FIXTURE: &str = include_str!("../tests/fixtures/qoder-transcript.txt");
    const QODER_SNAPSHOT: &str = include_str!("../tests/fixtures/qoder-parts.json");
    const CODEX_FIXTURE: &str = include_str!("../tests/fixtures/codex-transcript.txt");
    const CODEX_SNAPSHOT: &str = include_str!("../tests/fixtures/codex-parts.json");
    const OPENCODE_FIXTURE: &str = include_str!("../tests/fixtures/opencode-transcript.txt");
    const OPENCODE_SNAPSHOT: &str = include_str!("../tests/fixtures/opencode-parts.json");

    /// Every fixture, with the agent name Herdr reports for the pane it came
    /// from. The two invariants are asserted across all of them, so a new
    /// dictionary is covered by them the moment its fixture lands here.
    const FIXTURES: &[(&str, &str)] = &[
        (CLAUDE_FIXTURE, "claude"),
        (QODER_FIXTURE, "qodercli"),
        (CODEX_FIXTURE, "codex"),
        (OPENCODE_FIXTURE, "opencode"),
    ];

    fn claude(text: &str) -> Vec<Part> {
        normalize(text, dictionary_for(Some("claude")))
    }

    fn qoder(text: &str) -> Vec<Part> {
        normalize(text, dictionary_for(Some("qodercli")))
    }

    fn codex(text: &str) -> Vec<Part> {
        normalize(text, dictionary_for(Some("codex")))
    }

    fn opencode(text: &str) -> Vec<Part> {
        normalize(text, dictionary_for(Some("opencode")))
    }

    fn kinds(parts: &[Part]) -> Vec<&'static str> {
        parts.iter().map(Part::kind).collect()
    }

    #[test]
    fn an_agent_resolves_to_a_dictionary_by_substring() {
        assert_eq!(dictionary_for(Some("claude")).unwrap().id, "claude");
        assert_eq!(dictionary_for(Some("Claude Code")).unwrap().id, "claude");
        assert_eq!(dictionary_for(Some("qodercli")).unwrap().id, "qoder");
        // The names Herdr reports for the v1.2 additions, and the aliases its
        // own detection manifests list beside them.
        assert_eq!(dictionary_for(Some("codex")).unwrap().id, "codex");
        assert_eq!(dictionary_for(Some("OpenAI Codex")).unwrap().id, "codex");
        assert_eq!(dictionary_for(Some("opencode")).unwrap().id, "opencode");
        assert_eq!(dictionary_for(Some("open-code")).unwrap().id, "opencode");
        assert_eq!(
            dictionary_for(Some("herdr:opencode")).unwrap().id,
            "opencode"
        );
        // Still an answer we support: an agent nobody has written a table for.
        assert!(dictionary_for(Some("aider")).is_none());
        assert!(dictionary_for(Some("")).is_none());
        assert!(dictionary_for(None).is_none());
    }

    /// The dictionary ids are what a client sees, so two tables may not claim
    /// one id and no table may be reachable only through another's alias.
    #[test]
    fn every_dictionary_has_its_own_id_and_is_reachable_by_every_alias() {
        let mut ids: Vec<&str> = DICTIONARIES.iter().map(|(_, table)| table.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two dictionaries share an id");
        for (aliases, table) in DICTIONARIES {
            for alias in *aliases {
                assert_eq!(
                    dictionary_for(Some(alias)).map(|found| found.id),
                    Some(table.id),
                    "{alias} is shadowed by an earlier dictionary"
                );
            }
        }
    }

    #[test]
    fn an_agent_with_no_dictionary_degrades_to_text_paragraphs() {
        let parts = normalize("first line\nstill first\n\nsecond\n", None);
        assert_eq!(kinds(&parts), ["text", "text"]);
        assert_eq!(parts[0].fallback_text(), "first line\nstill first");
        assert_eq!(parts[1].to_json()["markdown"], "second");
        // Even here nothing is dropped, which is the whole point of the path.
        assert_eq!(parts[1].to_json()["range"], json!({ "start": 3, "end": 3 }));
    }

    #[test]
    fn a_claude_tool_block_reconstructs_from_its_markers() {
        let parts = claude("⏺ Bash(cargo test)\n  ⎿  test result: ok. 60 passed\n     finished\n");
        assert_eq!(kinds(&parts), ["tool-block"]);
        let value = parts[0].to_json();
        assert_eq!(value["tool"], "Bash");
        assert_eq!(value["input"], "cargo test");
        assert_eq!(value["result"][0], "test result: ok. 60 passed");
        assert_eq!(value["result"][1], "finished");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["truncated"], false);
        assert_eq!(
            value["fallback_text"],
            "⏺ Bash(cargo test)\n  ⎿  test result: ok. 60 passed\n     finished"
        );
    }

    #[test]
    fn a_tool_block_takes_its_status_from_the_first_result_line() {
        let failed = claude("⏺ Bash(cargo test)\n  ⎿  Error: 2 tests failed\n     see above\n");
        assert_eq!(failed[0].to_json()["status"], "error");
        // "error" has to open the line: an error mentioned in passing is not one.
        let mentioned = claude("⏺ Bash(cargo test)\n  ⎿  handled the error path\n");
        assert_eq!(mentioned[0].to_json()["status"], "ok");
        // No result line yet means the tool has not answered.
        let pending = claude("⏺ Bash(sleep 30)\n");
        assert_eq!(pending[0].to_json()["status"], "running");
    }

    #[test]
    fn an_ellipsis_anywhere_in_a_block_marks_it_truncated() {
        let parts = claude("⏺ Bash(cargo test)\n  ⎿  ok\n     … +12 lines (ctrl+o to expand)\n");
        assert_eq!(parts[0].to_json()["truncated"], true);
        let whole = claude("⏺ Bash(cargo test)\n  ⎿  ok\n");
        assert_eq!(whole[0].to_json()["truncated"], false);
    }

    #[test]
    fn several_result_groups_stay_with_one_block() {
        let parts = claude("⏺ Bash(cargo test)\n  ⎿  ok\n  ⎿  (timeout 7m)\n");
        assert_eq!(kinds(&parts), ["tool-block"]);
        let value = parts[0].to_json();
        assert_eq!(value["result"][0], "ok");
        assert_eq!(value["result"][1], "(timeout 7m)");
    }

    #[test]
    fn a_block_head_that_is_not_a_tool_call_is_prose() {
        // A sentence, a tool-ish name with no parens, and a non-ASCII opener all
        // have to stay text, or the transcript fills with imaginary tools.
        for head in [
            "⏺ Now the tests:",
            "⏺ Update Todos",
            "⏺ 合批 APK 出炉，静默验证上架。",
            "⏺ Background command \"build\" completed (exit code 0)",
        ] {
            assert_eq!(kinds(&claude(head)), ["text"], "{head}");
        }
    }

    #[test]
    fn prose_keeps_its_wrapped_and_blank_separated_lines_together() {
        let parts = claude("⏺ Shipped it:\n\n  - one\n  - two\n\n  Tell me when done.\n");
        assert_eq!(kinds(&parts), ["text"]);
        assert_eq!(
            parts[0].to_json()["markdown"],
            "Shipped it:\n\n- one\n- two\n\nTell me when done."
        );
    }

    #[test]
    fn a_numbered_result_group_becomes_a_diff_beside_its_block() {
        let parts = claude(concat!(
            "⏺ Update(src/main.rs)\n",
            "  ⎿  Added 2 lines\n",
            "      6135          let existing = 1;\n",
            "      6136 +        let added = 2;\n",
            "      6137 -        let removed = 3;\n",
        ));
        assert_eq!(kinds(&parts), ["tool-block", "diff"]);
        let diff = parts[1].to_json();
        assert_eq!(diff["file"], "src/main.rs");
        assert_eq!(diff["hunks"][1], "6136 +        let added = 2;");
        assert_eq!(diff["hunks"].as_array().unwrap().len(), 3);
        // The summary line is not a hunk, but it is still somebody's fallback.
        assert!(diff["fallback_text"]
            .as_str()
            .unwrap()
            .starts_with("  ⎿  Added 2 lines"));
    }

    #[test]
    fn a_date_or_a_counted_result_line_is_not_a_diff() {
        // A number followed by a word is a sentence, not a gutter.
        let dated = claude(concat!(
            "⏺ Bash(git log)\n",
            "  ⎿  2026-07-21 first\n",
            "     2026-07-22 second\n",
            "     2026-07-23 third\n",
        ));
        assert_eq!(kinds(&dated), ["tool-block"]);
        let counted = claude("⏺ Bash(pnpm lint)\n  ⎿  13 warnings\n     0 errors\n");
        assert_eq!(kinds(&counted), ["tool-block"]);
        // Line numbers climb; a listing that happens to be padded does not.
        let unsorted = claude(concat!(
            "⏺ Bash(wc -l *)\n",
            "  ⎿  120   src/main.rs\n",
            "     42    src/parts.rs\n",
            "     9     README.md\n",
        ));
        assert_eq!(kinds(&unsorted), ["tool-block"]);
    }

    #[test]
    fn a_checkbox_result_group_becomes_a_todo() {
        let parts =
            claude("⏺ Update Todos\n  ⎿  ☒ Write the dictionary\n     ☐ Wire the endpoint\n");
        assert_eq!(kinds(&parts), ["text", "todo"]);
        let todo = parts[1].to_json();
        assert_eq!(
            todo["items"][0],
            json!({ "text": "Write the dictionary", "done": true })
        );
        assert_eq!(
            todo["items"][1],
            json!({ "text": "Wire the endpoint", "done": false })
        );
    }

    #[test]
    fn a_diff_whose_block_scrolled_off_is_still_a_diff() {
        // What a read that starts part-way down the scrollback sees.
        let parts = claude(concat!(
            "      6204 +    #[cfg(unix)]\n",
            "      6205 +    #[test]\n",
            "      6206 +    fn a_symlink_cannot_be_read() {\n",
        ));
        assert_eq!(kinds(&parts), ["diff"]);
        let value = parts[0].to_json();
        assert_eq!(value["file"], Value::Null);
        assert_eq!(value["hunks"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_bare_checkbox_run_is_a_todo_without_a_block() {
        let parts = claude("☐ one\n☑ two\n");
        assert_eq!(kinds(&parts), ["todo"]);
        assert_eq!(parts[0].to_json()["items"][1]["done"], true);
    }

    #[test]
    fn a_result_that_ticks_off_what_it_did_is_not_a_checklist() {
        let parts =
            claude("⏺ Bash(fizzyx flow start 590)\n  ⎿  ✓ started #590\n     ✓ started #594\n");
        assert_eq!(kinds(&parts), ["tool-block"]);
    }

    #[test]
    fn a_group_that_trails_a_split_keeps_its_place_in_the_transcript() {
        // Claude puts its auto-mode note after the diff it approved. Reading the
        // parts in order has to read the pane in order.
        let parts = claude(concat!(
            "⏺ Update(src/main.rs)\n",
            "  ⎿  Added 2 lines\n",
            "      10          before\n",
            "      11 +        after\n",
            "  ⎿  Allowed by auto mode classifier\n",
        ));
        assert_eq!(kinds(&parts), ["tool-block", "diff", "text"]);
        let rebuilt = parts
            .iter()
            .flat_map(|part| part.fallback_text().lines())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            rebuilt,
            concat!(
                "⏺ Update(src/main.rs)\n",
                "  ⎿  Added 2 lines\n",
                "      10          before\n",
                "      11 +        after\n",
                "  ⎿  Allowed by auto mode classifier"
            )
        );
    }

    #[test]
    fn a_prompt_keeps_the_lines_the_user_typed_and_not_the_command_output() {
        let parts = claude("❯ /usage\n  ⎿  Settings dialog dismissed\n");
        assert_eq!(kinds(&parts), ["prompt", "text"]);
        assert_eq!(parts[0].to_json()["text"], "/usage");
        assert_eq!(parts[1].to_json()["markdown"], "Settings dialog dismissed");

        let wrapped = claude("❯ 1 first ask\n  2 second ask\n");
        assert_eq!(kinds(&wrapped), ["prompt"]);
        assert_eq!(wrapped[0].to_json()["text"], "1 first ask\n2 second ask");
    }

    #[test]
    fn a_spinner_frame_at_the_margin_is_status_and_indented_is_not() {
        let parts = claude("✻ Waiting for 3 background agents to finish\n");
        assert_eq!(kinds(&parts), ["status"]);
        let value = parts[0].to_json();
        assert_eq!(value["text"], "Waiting for 3 background agents to finish");
        assert_eq!(value["spinner"], true);
        // The same glyph inside somebody's output is not the status line.
        assert_eq!(kinds(&claude("  · not the spinner\n")), ["text"]);
    }

    #[test]
    fn a_qoder_block_reconstructs_from_its_own_markers() {
        let parts = qoder(concat!(
            " ▪ Bash(pnpm run lint 2>&1 | tail -3)\n",
            "   └ 13 warnings\n",
            "     0 errors\n",
        ));
        assert_eq!(kinds(&parts), ["tool-block"]);
        let value = parts[0].to_json();
        assert_eq!(value["tool"], "Bash");
        assert_eq!(value["input"], "pnpm run lint 2>&1 | tail -3");
        assert_eq!(value["result"][1], "0 errors");
    }

    #[test]
    fn a_qoder_command_that_ran_onto_more_lines_keeps_one_input() {
        let parts = qoder(concat!(
            " ▪ Bash(cd /srv/api && go run ./cmd/api 2>&1 &\n",
            "   echo \"api pid=$!\"…)\n",
            "   └ Background process started with PID 77129.\n",
        ));
        assert_eq!(kinds(&parts), ["tool-block"]);
        let value = parts[0].to_json();
        // The paren that closes a command with a newline in it lands on the last
        // of its lines, and is chrome there just as it is on the first.
        assert_eq!(
            value["input"],
            "cd /srv/api && go run ./cmd/api 2>&1 &\necho \"api pid=$!\"…"
        );
        assert_eq!(
            value["result"][0],
            "Background process started with PID 77129."
        );
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn a_qoder_thinking_tree_is_text_with_its_tree_drawing_removed() {
        let parts = qoder(" Thinking\n │ First I check the table.\n │ Then I edit it.\n");
        assert_eq!(kinds(&parts), ["text"]);
        assert_eq!(
            parts[0].to_json()["markdown"],
            "Thinking\nFirst I check the table.\nThen I edit it."
        );
    }

    #[test]
    fn a_qoder_background_notice_is_prose_not_a_tool() {
        let parts = qoder(" ● Background command \"go run ./cmd/api\" completed (exit code 0)\n");
        assert_eq!(kinds(&parts), ["text"]);
    }

    #[test]
    fn a_codex_verb_head_with_a_result_is_a_tool_and_without_one_is_prose() {
        // Codex writes `Ran cargo test` where Claude writes `Bash(cargo test)`.
        let parts = codex(concat!(
            "\u{2022} Ran pwd && ls\n",
            "  \u{2514} /Users/okk/.osuki\n",
            "    admin\n",
            "    \u{2026} +15 lines (ctrl + t to view transcript)\n",
        ));
        assert_eq!(kinds(&parts), ["tool-block"]);
        let value = parts[0].to_json();
        assert_eq!(value["tool"], "Ran");
        assert_eq!(value["input"], "pwd && ls");
        assert_eq!(value["result"][0], "/Users/okk/.osuki");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["truncated"], true);
        // The same word opening a sentence is prose: no result group, no tool.
        // This is what keeps Codex's first-person narration out of the tool set.
        assert_eq!(
            kinds(&codex("\u{2022} Ran into a wall on the way there\n")),
            ["text"]
        );
        // And a head that is not a verb the table knows stays prose even when a
        // result does follow it.
        let notice = codex("\u{2022} Model changed to gpt-5.3-codex-spark\n  \u{2514} done\n");
        assert_eq!(kinds(&notice), ["text"]);
    }

    #[test]
    fn a_codex_command_that_wrapped_keeps_its_gutter_out_of_the_input() {
        let parts = codex(concat!(
            "\u{2022} Ran cd /srv/api && (bun run dev > /tmp/dev.log 2>&1 &) ; pid=$!; kill $pid\n",
            "  \u{2502} >/dev/null 2>&1 || true; sleep 1; tail -n 80 /tmp/dev.log\n",
            "  \u{2514} (no output)\n",
        ));
        assert_eq!(kinds(&parts), ["tool-block"]);
        let value = parts[0].to_json();
        assert_eq!(
            value["input"],
            concat!(
                "cd /srv/api && (bun run dev > /tmp/dev.log 2>&1 &) ; pid=$!; kill $pid\n",
                ">/dev/null 2>&1 || true; sleep 1; tail -n 80 /tmp/dev.log"
            )
        );
        assert_eq!(value["result"][0], "(no output)");
    }

    #[test]
    fn a_codex_startup_warning_is_one_text_part_with_its_wrapping() {
        let parts = codex(concat!(
            "\u{26a0} MCP client for `shopify-storefront` failed to start: MCP startup failed:\n",
            "  message error Transport channel closed, when send initialized notification\n",
        ));
        assert_eq!(kinds(&parts), ["text"]);
        assert_eq!(
            parts[0].to_json()["markdown"],
            concat!(
                "MCP client for `shopify-storefront` failed to start: MCP startup failed:\n",
                "message error Transport channel closed, when send initialized notification"
            )
        );
    }

    #[test]
    fn a_codex_status_box_keeps_its_rows_as_one_text_part() {
        // `/status` draws a box whose sides are the same glyph a wrapped command
        // continues with. Both are chrome around text.
        let parts = codex(concat!(
            "\u{256d}\u{2500}\u{2500}\u{2500}\u{256e}\n",
            "\u{2502}  Model:      gpt-5.3-codex-spark\n",
            "\u{2502}  Directory:  ~/.osuki\n",
            "\u{2570}\u{2500}\u{2500}\u{2500}\u{256f}\n",
        ));
        assert_eq!(kinds(&parts), ["text"]);
        let markdown = parts[0].to_json();
        let markdown = markdown["markdown"].as_str().unwrap();
        assert!(
            markdown.contains("Model:      gpt-5.3-codex-spark"),
            "{markdown}"
        );
        assert!(!markdown.contains("\u{2502}  Model"), "{markdown}");
    }

    #[test]
    fn a_codex_prompt_is_the_users_line_and_the_working_line_is_status() {
        let parts = codex(
            "\u{203a} \u{770b}\u{770b} kit-pro \u{662f}\u{5426}\u{80fd}\u{591f}\u{8fd0}\u{884c}\n",
        );
        assert_eq!(kinds(&parts), ["prompt"]);
        assert_eq!(
            parts[0].to_json()["text"],
            "\u{770b}\u{770b} kit-pro \u{662f}\u{5426}\u{80fd}\u{591f}\u{8fd0}\u{884c}"
        );
        // Herdr's codex manifest matches the live line as `[•◦] Working (…)`.
        // `◦` is the status glyph here; `•` is already the block opener, so that
        // spelling degrades to text rather than being read as a tool.
        let spinning = codex("\u{25e6} Working (12s \u{b7} esc to interrupt)\n");
        assert_eq!(kinds(&spinning), ["status"]);
        assert_eq!(
            spinning[0].to_json()["text"],
            "Working (12s \u{b7} esc to interrupt)"
        );
        assert_eq!(
            kinds(&codex("\u{2022} Working (12s \u{b7} esc to interrupt)\n")),
            ["text"]
        );
    }

    #[test]
    fn an_opencode_call_glyph_names_the_tool_and_needs_no_result_group() {
        // opencode gives tool calls their own glyph and prints the outcome on
        // that line, so the block is finished rather than in flight.
        let parts = opencode(concat!(
            "     \u{2192} Read kit/packages/ui/src/index.ts\n",
            "     \u{2192} Grep --glob **/*.tsx button\n",
        ));
        assert_eq!(kinds(&parts), ["tool-block", "tool-block"]);
        let value = parts[0].to_json();
        assert_eq!(value["tool"], "Read");
        assert_eq!(value["input"], "kit/packages/ui/src/index.ts");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["result"].as_array().unwrap().len(), 0);
        assert_eq!(parts[1].to_json()["tool"], "Grep");
        // Claude's blocks are unaffected: no result there still means running.
        assert_eq!(
            claude("\u{23fa} Bash(sleep 30)\n")[0].to_json()["status"],
            "running"
        );
    }

    #[test]
    fn an_opencode_gutter_is_one_prompt_and_not_a_prompt_per_row() {
        let parts = opencode(concat!(
            "  \u{2503}\n",
            "  \u{2503}  kit \u{7ec4}\u{4ef6}\u{5462}\u{6709}\u{54ea}\u{4e9b}？\n",
            "  \u{2503}  \u{662f}\u{5426}\u{5b8c}\u{6574}\u{4e86}\n",
            "  \u{2503}\n",
        ));
        assert_eq!(kinds(&parts), ["prompt"]);
        assert_eq!(
            parts[0].to_json()["text"],
            "kit \u{7ec4}\u{4ef6}\u{5462}\u{6709}\u{54ea}\u{4e9b}？\n\u{662f}\u{5426}\u{5b8c}\u{6574}\u{4e86}"
        );
        // The gutter rows the widget drew are still somebody's fallback.
        assert_eq!(parts[0].fallback_text().lines().count(), 4);
        // Claude's prompt is one marker line with its wrapping indented under
        // it, and two of them stay two prompts.
        assert_eq!(
            kinds(&claude("\u{276f} first\n\u{276f} second\n")),
            ["prompt", "prompt"]
        );
    }

    #[test]
    fn an_opencode_thought_and_note_line_degrade_to_text() {
        assert_eq!(kinds(&opencode("     + Thought: 480ms\n")), ["text"]);
        assert_eq!(
            opencode("     + Thought: 480ms\n")[0].to_json()["markdown"],
            "Thought: 480ms"
        );
        let note = opencode("     \u{21b3} Loaded kit/AGENTS.md\n");
        assert_eq!(kinds(&note), ["text"]);
        assert_eq!(note[0].to_json()["markdown"], "Loaded kit/AGENTS.md");
    }

    #[test]
    fn part_spans_are_ordered_and_never_overlap() {
        for &(text, agent) in FIXTURES {
            assert_spans(text, agent);
        }
    }

    /// A part's span names the source rows it owns, and its `fallback_text` is
    /// those rows byte for byte. Both halves matter: the span is how a client
    /// maps a terminal row to a part, and the verbatim text is what it renders
    /// when it does not know the part's type.
    fn assert_spans(text: &str, agent: &str) {
        let source: Vec<&str> = text.lines().collect();
        let parts = normalize(text, dictionary_for(Some(agent)));
        let mut previous = None;
        for part in &parts {
            assert!(part.start <= part.end, "{agent}");
            assert!(part.end < source.len(), "{agent} span runs past the source");
            if let Some(previous) = previous {
                assert!(part.start > previous, "{agent} span runs backwards");
            }
            assert_eq!(
                part.fallback_text(),
                source[part.start..=part.end].join("\n"),
                "{agent} fallback_text is not its own source rows verbatim"
            );
            previous = Some(part.end);
        }
    }

    /// The same two invariants against whatever a live pane is showing right
    /// now, rather than against a fixture captured once. Point
    /// `PARTS_LIVE_TRANSCRIPTS` at a directory of `<agent>.txt` files -- a
    /// `recent-unwrapped` read per pane, saved verbatim -- and re-run. Unset,
    /// which is the normal case and every CI run, the check is skipped: it
    /// exists to be pointed at a real machine, not to gate the build on one.
    #[test]
    fn a_live_pane_read_holds_both_invariants() {
        let Some(dir) = std::env::var_os("PARTS_LIVE_TRANSCRIPTS") else {
            return;
        };
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir)
            .expect("live transcript directory")
            .flatten()
        {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("txt") {
                continue;
            }
            let agent = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("agent name from file stem")
                .to_owned();
            let text = std::fs::read_to_string(&path).expect("live transcript");
            assert_spans(&text, &agent);
            assert_covers_every_line(&text, &agent);
            let parts = normalize(&text, dictionary_for(Some(&agent)));
            eprintln!(
                "{agent}: {} rows -> {} parts, {} typed",
                text.lines().count(),
                parts.len(),
                parts.iter().filter(|part| part.kind() != "text").count()
            );
            checked += 1;
        }
        assert!(checked > 0, "PARTS_LIVE_TRANSCRIPTS held no .txt reads");
    }

    fn assert_covers_every_line(text: &str, agent: &str) {
        let parts = normalize(text, dictionary_for(Some(agent)));
        let source: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let covered: Vec<&str> = parts
            .iter()
            .flat_map(|part| part.fallback_text().lines())
            .filter(|line| !line.trim().is_empty())
            .collect();
        assert_eq!(covered, source, "{agent} lost or reordered a line");
    }

    #[test]
    fn every_non_blank_source_line_lands_in_exactly_one_fallback_text() {
        // The contract's floor: structure may be missed, content may not be.
        // Every fixture is also read through the wrong dictionary and through no
        // dictionary at all, because the floor has to hold for a table that has
        // drifted as much as for one that fits.
        let wrong: Vec<(&str, &str)> = FIXTURES
            .iter()
            .flat_map(|&(text, _)| [(text, "qodercli"), (text, "codex"), (text, "aider")])
            .collect();
        for &(text, agent) in FIXTURES.iter().chain(wrong.iter()) {
            assert_covers_every_line(text, agent);
        }
    }

    /// Snapshots are pinned per agent version: an agent that moves its markers
    /// has to show up as a diff in review rather than as a quiet loss of
    /// structure. Re-pin deliberately, after reading the diff, with
    /// `UPDATE_PART_SNAPSHOTS=1 cargo test`.
    fn assert_snapshot(name: &str, agent: &str, fixture: &str, pinned: &str) {
        let parts = normalize_json(fixture, dictionary_for(Some(agent)));
        let actual = serde_json::to_string_pretty(&parts).unwrap();
        if std::env::var_os("UPDATE_PART_SNAPSHOTS").is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name);
            std::fs::write(&path, format!("{actual}\n")).unwrap();
            return;
        }
        assert_eq!(actual.trim(), pinned.trim(), "{name} drifted");
    }

    #[test]
    fn the_claude_fixture_normalizes_to_its_pinned_snapshot() {
        assert_snapshot(
            "claude-parts.json",
            "claude",
            CLAUDE_FIXTURE,
            CLAUDE_SNAPSHOT,
        );
    }

    #[test]
    fn the_qoder_fixture_normalizes_to_its_pinned_snapshot() {
        assert_snapshot(
            "qoder-parts.json",
            "qodercli",
            QODER_FIXTURE,
            QODER_SNAPSHOT,
        );
    }

    #[test]
    fn the_codex_fixture_normalizes_to_its_pinned_snapshot() {
        assert_snapshot("codex-parts.json", "codex", CODEX_FIXTURE, CODEX_SNAPSHOT);
    }

    #[test]
    fn the_opencode_fixture_normalizes_to_its_pinned_snapshot() {
        assert_snapshot(
            "opencode-parts.json",
            "opencode",
            OPENCODE_FIXTURE,
            OPENCODE_SNAPSHOT,
        );
    }

    #[test]
    fn the_fixtures_cover_every_part_type_the_dictionaries_can_produce() {
        let mut seen: Vec<&str> = FIXTURES
            .iter()
            .flat_map(|&(text, agent)| normalize(text, dictionary_for(Some(agent))))
            .map(|part| part.kind())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen,
            ["diff", "prompt", "status", "text", "todo", "tool-block"]
        );
    }

    /// Herdr keeps the ecosystem's agent list as detection manifests it updates
    /// out of band (bundled, remote-refreshed, and locally overridable). This
    /// table tracks that list, so the drift worth catching is a dictionary keyed
    /// on a name Herdr does not know -- that pane would never reach us.
    ///
    /// The manifests live on the machine Herdr runs on, so the check is skipped
    /// where they are absent rather than making the build depend on them. The
    /// other direction -- a manifest with no dictionary yet -- is the roadmap,
    /// not a failure, so it is reported and not asserted. Run with
    /// `cargo test -- --nocapture` to read it.
    #[test]
    fn no_dictionary_is_keyed_on_an_agent_herdr_does_not_know() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let root = std::path::Path::new(&home).join(".local/state/herdr/agent-detection/remote");
        let Ok(entries) = std::fs::read_dir(&root) else {
            eprintln!("herdr manifests not on this machine; drift check skipped");
            return;
        };
        // `id = "codex"` and `aliases = ["open-code", …]`, which is all of the
        // manifest this check needs and the reason it reads no TOML crate.
        let mut known: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            // The agent's own `id` and `aliases` head the file; every `id`
            // below the first `[[rules]]` names a detection rule instead.
            for line in text.lines().take_while(|line| !line.starts_with("[[")) {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                if !matches!(key.trim(), "id" | "aliases") {
                    continue;
                }
                known.extend(
                    value
                        .split('"')
                        .skip(1)
                        .step_by(2)
                        .map(|name| name.to_ascii_lowercase()),
                );
            }
        }
        if known.is_empty() {
            eprintln!("herdr manifests unreadable; drift check skipped");
            return;
        }
        for (aliases, table) in DICTIONARIES {
            for alias in *aliases {
                assert!(
                    known.iter().any(|name| name.contains(alias)),
                    "dictionary {} is keyed on {alias}, which no herdr manifest reports",
                    table.id
                );
            }
        }
        let uncovered: Vec<&String> = known
            .iter()
            .filter(|name| dictionary_for(Some(name)).is_none())
            .collect();
        eprintln!("herdr agents with no dictionary yet: {uncovered:?}");
    }

    /// Each dictionary has to buy its keep: a table that types nothing is a
    /// table that has drifted, and the fixture is the only place that shows it.
    /// The floor is deliberately well under what the fixtures type today, so
    /// that ordinary transcript variation does not fail the build.
    #[test]
    fn every_dictionary_types_a_real_share_of_its_own_fixture() {
        for &(text, agent) in FIXTURES {
            let parts = normalize(text, dictionary_for(Some(agent)));
            let typed = parts.iter().filter(|part| part.kind() != "text").count();
            assert!(
                typed * 4 >= parts.len(),
                "{agent} typed only {typed} of {} parts",
                parts.len()
            );
        }
    }
}
