# Range-Addressed Pane Output Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a client ask for an absolute line range of a pane's scrollback and be told exactly which lines it got, so paging back is one request per page and reaching the top is a fact rather than a guess.

**Architecture:** `ReadPane` grows an optional range and `PaneOutput` grows a `PaneRange` describing what was served. tmux maps the range onto `capture-pane -S/-E`; herdr ignores the request but reports its actual tail in the same shape, so one response type covers both. Muqun then reads `range.start > 0` instead of measuring whether a larger request came back longer.

**Tech Stack:** Rust (axum 0.8, tokio), TypeScript (React Native / Expo, bun test).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-02-tmux-range-paging-design.md`
- `MAX_OUTPUT_LINES` stays `5000` (`src/main.rs:80`). It is a per-request ceiling, not a scrollback horizon.
- Absolute line 0 is the oldest retained line. Ranges are half-open `[start, end)`. `total` is one past the newest.
- tmux coordinates are relative to the top of the *visible* pane, and `-E` is **inclusive**. Verified live at `history_size=463, pane_height=41`: `-S -` → 504 lines, `-S -448` → 489 lines, `-S -448 -E -448` → exactly 1 line, `-S 0 -E 0` → first visible row.
- `BackendKind`'s `#[serde(default)]` stays `Herdr`. It is the read-contract for configs written before the field existed. Do not change it.
- Gateway verification, from `CONTEXT.md`: `cargo fmt --check`, `cargo test --offline`, `cargo clippy --offline --all-targets -- -D warnings`, `cargo build --release --offline`.
- Muqun verification: `npx tsc --noEmit` and `bun test`.
- Do **not** touch the pane-id percent-encoding defect. It is being fixed in fizzyx separately.

## Branch Hygiene (do this before Task 1)

`feature/encrypted-transport` carries codex's finished-but-uncommitted transport work, including ~693 modified lines of `src/main.rs`. This plan also modifies `src/main.rs`. Commit that work first so the two are separable.

- [ ] **Step 0.1: Verify the transport work is green**

```bash
cd ~/.osuki/herdr-gateway
cargo fmt --check && cargo test --offline 2>&1 | tail -3
```

Expected: fmt silent, `324 passed; 0 failed`.

- [ ] **Step 0.2: Commit it on its own**

```bash
git add -A
git commit -m "feat: per-device encrypted application transport"
```

- [ ] **Step 0.3: Branch for this work**

```bash
git checkout -b feature/range-paging
```

## File Structure

| File | Responsibility |
| --- | --- |
| `src/backend/model.rs` | `PaneRange` type; range fields on `ReadPane` and `PaneOutput` |
| `src/backend/tmux.rs` | absolute↔tmux coordinate mapping; range-aware `read_pane`; per-pane metrics |
| `src/backend/herdr.rs` | fill `PaneRange` from the tail actually returned |
| `src/backend/compat.rs` | serialize `range` into the `pane_read` envelope |
| `src/main.rs` | `start`/`end` on `OutputQuery`; validation and clamping; tmux-first ordering |
| `Muqun/src/terminal/history.ts` | read `range`; prepend disjoint pages; keep the measured path as fallback |

---

### Task 1: Coordinate mapping

The mapping is the part that is easy to get wrong by one and hard to notice, so it lands first as a pure function with no tmux process involved.

**Files:**
- Modify: `src/backend/model.rs` (add `PaneRange`)
- Modify: `src/backend/tmux.rs` (add `capture_bounds`, tests)

**Interfaces:**
- Produces: `PaneRange { start: u32, end: u32, total: u32 }` in `backend::model`, re-exported through `backend`.
- Produces: `fn capture_bounds(start: u32, end: u32, history_size: u32) -> (i64, i64)` in `backend::tmux`, returning the inclusive `-S`/`-E` pair.

- [ ] **Step 1.1: Add the `PaneRange` type**

In `src/backend/model.rs`, after `PaneOutput`:

```rust
/// Which absolute lines of a pane a read actually returned.
///
/// Line 0 is the oldest line the pane still holds and `total` is one past the
/// newest, so `start == 0` means the reader has reached the top. This always
/// describes what was served, never what was asked for: a backend that cannot
/// honour a requested range still reports the tail it did return, which is what
/// lets one response shape cover both backends without a capability flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneRange {
    pub start: u32,
    pub end: u32,
    pub total: u32,
}
```

Export it alongside the other model types in `src/backend/mod.rs`.

- [ ] **Step 1.2: Write the failing mapping test**

In `src/backend/tmux.rs` `mod tests`:

```rust
#[test]
fn capture_bounds_map_absolute_lines_onto_tmux_coordinates() {
    // Measured live: history_size 463, pane_height 41, total 504.
    // tmux counts from the top of the visible pane, and -E is inclusive.
    assert_eq!(capture_bounds(0, 504, 463), (-463, 40));
    // The oldest line alone.
    assert_eq!(capture_bounds(0, 1, 463), (-463, -463));
    // The first visible row alone.
    assert_eq!(capture_bounds(463, 464, 463), (0, 0));
    // A pane with no scrollback at all.
    assert_eq!(capture_bounds(0, 41, 0), (0, 40));
}
```

- [ ] **Step 1.3: Run it and watch it fail**

Run: `cargo test --offline capture_bounds`
Expected: FAIL, `cannot find function 'capture_bounds'`.

- [ ] **Step 1.4: Implement it**

In `src/backend/tmux.rs`, next to `tail_lines`:

```rust
/// Absolute `[start, end)` to the inclusive `-S`/`-E` pair tmux wants.
///
/// tmux numbers rows from the top of the *visible* pane: 0 is the first visible
/// row and negatives reach back into scrollback, so absolute line `i` sits at
/// `i - history_size`. `-E` is inclusive, hence the extra `- 1`.
fn capture_bounds(start: u32, end: u32, history_size: u32) -> (i64, i64) {
    let origin = i64::from(history_size);
    (
        i64::from(start) - origin,
        i64::from(end.max(start + 1)) - 1 - origin,
    )
}
```

- [ ] **Step 1.5: Run it and watch it pass**

Run: `cargo test --offline capture_bounds`
Expected: PASS.

- [ ] **Step 1.6: Commit**

```bash
git add src/backend/model.rs src/backend/mod.rs src/backend/tmux.rs
git commit -m "feat: map absolute pane lines onto tmux capture coordinates"
```

---

### Task 2: tmux serves ranges

**Files:**
- Modify: `src/backend/model.rs` (range fields on `ReadPane`, `range` on `PaneOutput`)
- Modify: `src/backend/tmux.rs` (`pane_metrics`, range-aware `read_pane`)
- Modify: `src/backend/herdr.rs`, `src/main.rs` (construct the new fields so the crate builds)

**Interfaces:**
- Consumes: `PaneRange`, `capture_bounds` from Task 1.
- Produces: `ReadPane.start: Option<u32>`, `ReadPane.end: Option<u32>`, `PaneOutput.range: Option<PaneRange>`.
- Produces: `async fn pane_metrics(&self, pane: &PaneId) -> Result<(u32, u32), BackendError>` on the tmux backend, returning `(history_size, pane_height)`.

- [ ] **Step 2.1: Add the fields**

In `src/backend/model.rs`:

```rust
pub struct ReadPane {
    pub pane_id: PaneId,
    pub source: OutputSource,
    pub format: OutputFormat,
    pub lines: u32,
    /// Absolute half-open range to read. `None` keeps the tail-of-`lines`
    /// behaviour every existing caller relies on.
    pub start: Option<u32>,
    pub end: Option<u32>,
}

pub struct PaneOutput {
    pub text: String,
    pub revision: Option<u64>,
    /// Which absolute lines this text is, where the backend can say.
    pub range: Option<PaneRange>,
}
```

Every existing construction site must gain `start: None, end: None` or `range: None`. Compile to find them:

Run: `cargo build --offline 2>&1 | grep -c "missing field"`

- [ ] **Step 2.2: Write the failing metrics test**

In `src/backend/tmux.rs` `mod tests` — this one is a contract test because it needs a real server:

```rust
#[tokio::test]
#[ignore = "requires a tmux server"]
async fn pane_metrics_report_history_and_height() {
    let (backend, workspace) = contract_workspace().await;
    let panes = backend.list_panes().await.unwrap();
    let pane = panes.iter().find(|p| p.workspace_id == workspace.id).unwrap();
    let (history, height) = backend.pane_metrics(&pane.id).await.unwrap();
    assert_eq!(Some(height), pane.viewport_rows);
    assert_eq!(Some(history), pane.max_offset_from_bottom);
    backend.close_workspace(&workspace.id).await.unwrap();
}
```

If `contract_workspace()` does not already exist, factor it out of the existing
`#[ignore]` contract test rather than duplicating its setup.

- [ ] **Step 2.3: Run it and watch it fail**

Run: `cargo test --offline pane_metrics -- --ignored`
Expected: FAIL, no method `pane_metrics`.

- [ ] **Step 2.4: Implement `pane_metrics`**

`list_panes` already reads both numbers (`#{pane_height}` at field 9, `#{history_size}` at field 11), but a read should not pay for every pane on the server:

```rust
/// `(history_size, pane_height)` for one pane.
///
/// `list_panes` carries both, but a single read has no reason to query every
/// pane on the server to learn about one of them.
async fn pane_metrics(&self, pane: &PaneId) -> Result<(u32, u32), BackendError> {
    validate_tmux_id(pane.as_str(), '%', "pane")?;
    let output = self
        .output(&[
            "display-message".to_owned(),
            "-p".to_owned(),
            "-t".to_owned(),
            pane.as_str().to_owned(),
            "-F".to_owned(),
            "#{history_size}\t#{pane_height}".to_owned(),
        ])
        .await?;
    let line = output.lines().next().unwrap_or_default();
    let mut parts = line.split('\t');
    let history = parse_u32(parts.next().unwrap_or_default(), "pane")?;
    let height = parse_u32(parts.next().unwrap_or_default(), "pane")?;
    Ok((history, height))
}
```

- [ ] **Step 2.5: Run it and watch it pass**

Run: `cargo test --offline pane_metrics -- --ignored`
Expected: PASS.

- [ ] **Step 2.6: Make `read_pane` honour the range**

Replace the body of `read_pane` in `src/backend/tmux.rs`. The non-range path must keep producing byte-identical output to today:

```rust
fn read_pane<'a>(&'a self, request: &'a ReadPane) -> BackendFuture<'a, PaneOutput> {
    Box::pin(async move {
        validate_tmux_id(request.pane_id.as_str(), '%', "pane")?;
        let (history_size, pane_height) = self.pane_metrics(&request.pane_id).await?;
        let total = history_size.saturating_add(pane_height);

        let mut args = vec!["capture-pane".to_owned(), "-p".to_owned()];
        if request.format == OutputFormat::Ansi {
            args.push("-e".to_owned());
        }
        if request.source == OutputSource::RecentUnwrapped {
            args.push("-J".to_owned());
        }

        let range = match (request.start, request.end) {
            // `visible` is the live screen, which no range addresses.
            (Some(start), Some(end)) if request.source != OutputSource::Visible => {
                let start = start.min(total.saturating_sub(1));
                let end = end.min(total).max(start + 1);
                let end = end.min(start + MAX_CAPTURE_LINES);
                let (s, e) = capture_bounds(start, end, history_size);
                args.push("-S".to_owned());
                args.push(s.to_string());
                args.push("-E".to_owned());
                args.push(e.to_string());
                Some(PaneRange { start, end, total })
            }
            _ => {
                if request.source != OutputSource::Visible {
                    args.push("-S".to_owned());
                    args.push(format!("-{}", request.lines));
                }
                None
            }
        };

        args.push("-t".to_owned());
        args.push(request.pane_id.as_str().to_owned());
        let captured = self.output(&args).await?;

        let (text, range) = match range {
            Some(range) => (captured, Some(range)),
            None => {
                let text = tail_lines(&captured, request.lines as usize);
                let served = text.lines().count() as u32;
                let range = PaneRange {
                    start: total.saturating_sub(served),
                    end: total,
                    total,
                };
                (text, Some(range))
            }
        };

        Ok(PaneOutput {
            revision: Some(text_revision(&text)),
            text,
            range,
        })
    })
}
```

Add near the top of `src/backend/tmux.rs`:

```rust
/// Mirrors `MAX_OUTPUT_LINES` in main. The backend clamps too so a direct
/// caller cannot ask tmux for a hundred thousand rows.
const MAX_CAPTURE_LINES: u32 = 5000;
```

- [ ] **Step 2.7: Verify the no-range path is unchanged**

Run: `cargo test --offline`
Expected: `324 passed; 0 failed`. Any failure here means the tail path changed behaviour — fix it rather than updating the assertion.

- [ ] **Step 2.8: Commit**

```bash
git add src/backend/
git commit -m "feat: read an absolute line range from a tmux pane"
```

---

### Task 3: herdr reports what it served

**Files:**
- Modify: `src/backend/herdr.rs`

**Interfaces:**
- Consumes: `PaneRange` from Task 1, `PaneOutput.range` from Task 2.

- [ ] **Step 3.1: Write the failing test**

In `src/backend/herdr.rs` `mod tests`, alongside the existing response-parsing tests:

```rust
#[test]
fn herdr_range_describes_the_tail_it_returned_not_the_one_requested() {
    // The daemon has no range parameter, so a range request is answered with a
    // tail. Reporting that honestly is what lets the client stop asking.
    let output = herdr_pane_output(
        "line1\nline2\nline3",
        Some(900),
    );
    let range = output.range.unwrap();
    assert_eq!(range.total, 900);
    assert_eq!(range.end, 900);
    assert_eq!(range.start, 897);
}

#[test]
fn herdr_range_falls_back_to_the_line_count_when_the_daemon_reports_no_total() {
    let output = herdr_pane_output("line1\nline2", None);
    let range = output.range.unwrap();
    assert_eq!(range, PaneRange { start: 0, end: 2, total: 2 });
}
```

- [ ] **Step 3.2: Run them and watch them fail**

Run: `cargo test --offline herdr_range`
Expected: FAIL, `cannot find function 'herdr_pane_output'`.

- [ ] **Step 3.3: Implement the helper and wire it in**

In `src/backend/herdr.rs`:

```rust
/// The tail a herdr read returned, expressed as an absolute range.
///
/// `scrollback_rows` is herdr's own count where it reports one. Muqun has
/// measured that count disagreeing with what `pane.read` hands over (a pane
/// claiming 2765 rows yielding 992 lines), so this does not treat it as
/// authoritative — it only places it next to the line count it failed to
/// predict, in one response, where a client can compare them.
fn herdr_pane_output(text: &str, scrollback_rows: Option<u32>) -> PaneOutput {
    let served = text.lines().count() as u32;
    let total = scrollback_rows.unwrap_or(served).max(served);
    PaneOutput {
        text: text.to_owned(),
        revision: None,
        range: Some(PaneRange {
            start: total.saturating_sub(served),
            end: total,
            total,
        }),
    }
}
```

In `read_pane`, after extracting `text` and `revision`, build the output through
this helper and then restore the parsed `revision`, reading the row count from
the same pane metadata that feeds `max_offset_from_bottom`.

- [ ] **Step 3.4: Run them and watch them pass**

Run: `cargo test --offline herdr_range`
Expected: PASS.

- [ ] **Step 3.5: Commit**

```bash
git add src/backend/herdr.rs
git commit -m "feat: report the range a herdr read actually served"
```

---

### Task 4: The HTTP surface

**Files:**
- Modify: `src/main.rs` (`OutputQuery`, `pane_output`, `stream_read_request`)
- Modify: `src/backend/compat.rs` (`pane_read`)

**Interfaces:**
- Consumes: everything from Tasks 1–3.
- Produces: `fn validate_output_range(start: Option<u32>, end: Option<u32>) -> Result<Option<(u32, u32)>, (StatusCode, Json<Value>)>` in `src/main.rs`.

- [ ] **Step 4.1: Write the failing validation tests**

In `src/main.rs` `mod tests`:

```rust
#[test]
fn an_output_range_needs_both_ends_or_neither() {
    assert!(validate_output_range(None, None).unwrap().is_none());
    assert_eq!(validate_output_range(Some(10), Some(20)).unwrap(), Some((10, 20)));
    assert!(validate_output_range(Some(10), None).is_err());
    assert!(validate_output_range(None, Some(20)).is_err());
}

#[test]
fn an_output_range_must_run_forwards() {
    assert!(validate_output_range(Some(20), Some(20)).is_err());
    assert!(validate_output_range(Some(21), Some(20)).is_err());
}

#[test]
fn an_oversized_output_range_is_trimmed_from_its_start_rather_than_refused() {
    // A reader scrolling toward the top always eventually overreaches. That is
    // an arrival at the top, not a client mistake, so it clamps.
    let (start, end) = validate_output_range(Some(0), Some(50_000)).unwrap().unwrap();
    assert_eq!(start, 0);
    assert_eq!(end, MAX_OUTPUT_LINES);
}
```

- [ ] **Step 4.2: Run them and watch them fail**

Run: `cargo test --offline output_range`
Expected: FAIL, `cannot find function 'validate_output_range'`.

- [ ] **Step 4.3: Implement**

Add the fields to `OutputQuery` in `src/main.rs`:

```rust
#[derive(Deserialize)]
struct OutputQuery {
    source: Option<String>,
    lines: Option<u32>,
    format: Option<String>,
    /// Absolute half-open range. Both or neither; the range wins over `lines`.
    start: Option<u32>,
    end: Option<u32>,
}
```

And the validator:

```rust
/// A requested range, clamped to what one response may carry.
///
/// Out-of-bounds clamps instead of failing: a reader paging toward the top will
/// always eventually ask for more than the pane holds, and that is how reaching
/// the top looks from outside, not a mistake worth an error for.
fn validate_output_range(
    start: Option<u32>,
    end: Option<u32>,
) -> ApiResult<Option<(u32, u32)>> {
    match (start, end) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) if start < end => {
            Ok(Some((start, end.min(start.saturating_add(MAX_OUTPUT_LINES)))))
        }
        (Some(_), Some(_)) => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_range",
            "start must be less than end",
        )),
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_range",
            "start and end must be given together",
        )),
    }
}
```

Call it at the top of `pane_output` and pass the result into the `BackendReadPane`
it builds.

- [ ] **Step 4.4: Run them and watch them pass**

Run: `cargo test --offline output_range`
Expected: PASS.

- [ ] **Step 4.5: Serialize `range` into the envelope**

In `src/backend/compat.rs`, replace `pane_read`:

```rust
pub fn pane_read(output: PaneOutput) -> Value {
    let mut read = json!({
        "output": output.text,
        "revision": output.revision,
    });
    if let Some(range) = output.range {
        read["range"] = json!({
            "start": range.start,
            "end": range.end,
            "total": range.total,
        });
    }
    json!({ "result": { "type": "pane_read", "read": read } })
}
```

- [ ] **Step 4.6: Document it in the OpenAPI spec**

`src/main.rs:9559` documents `lines`. Add beside it:

```rust
query_param("start", "First absolute line to read, 0 being the oldest the pane holds. Requires end."),
query_param("end", "One past the last absolute line to read. Requires start. A range wins over lines, and is clamped to 5000 lines and to what the pane holds rather than refused."),
```

Then extend the existing `openapi_spec_contains_docs_routes_and_auth` test to
assert both names appear.

- [ ] **Step 4.7: Full verification**

```bash
cargo fmt --check
cargo test --offline 2>&1 | tail -3
cargo clippy --offline --all-targets -- -D warnings
```

Expected: all clean.

- [ ] **Step 4.8: Commit**

```bash
git add src/main.rs src/backend/compat.rs
git commit -m "feat: address pane output by absolute line range"
```

---

### Task 5: tmux sorts first

Muqun takes `sessions[0]` and offers no picker (`src/stores/session-control.ts:96`), so order is what decides which backend the app talks to.

**Files:**
- Modify: `src/main.rs` (`enable_managed_backend`, around line 2789)

- [ ] **Step 5.1: Write the failing test**

In `src/main.rs` `mod tests`:

```rust
#[test]
fn tmux_sessions_sort_ahead_of_herdr_whatever_order_they_were_added() {
    let mut config = test_config("token");
    config.sessions.clear();
    upsert_backend_session(&mut config, BackendKind::Herdr, None, None, None).unwrap();
    upsert_backend_session(&mut config, BackendKind::Tmux, None, None, None).unwrap();
    // The app reads sessions[0] and shows no picker, so this ordering is the
    // whole of "tmux is the default backend" as far as a client can tell.
    assert_eq!(config.sessions[0].backend, BackendKind::Tmux);
    assert_eq!(config.sessions[1].backend, BackendKind::Herdr);
}
```

- [ ] **Step 5.2: Run it and watch it fail**

Run: `cargo test --offline tmux_sessions_sort_ahead`
Expected: FAIL — herdr was added first and stays first.

- [ ] **Step 5.3: Implement**

At the end of `upsert_backend_session`, after the `push`:

```rust
    // Muqun selects sessions[0] and exposes no picker, so ordering is what
    // makes tmux the backend a client actually reaches. Stable, so two
    // sessions of the same backend keep the order they were added in.
    config
        .sessions
        .sort_by_key(|session| match session.backend {
            BackendKind::Tmux => 0,
            BackendKind::Herdr => 1,
        });
```

- [ ] **Step 5.4: Run it and watch it pass**

Run: `cargo test --offline tmux_sessions_sort_ahead`
Expected: PASS.

- [ ] **Step 5.5: Confirm no existing test assumed the old order**

Run: `cargo test --offline`
Expected: `324 passed` plus the new tests. `main.rs:12529` and `main.rs:12747`
assert on `sessions[N].backend` — read them and update only if the assertion was
about ordering rather than about routing.

- [ ] **Step 5.6: Commit**

```bash
git add src/main.rs
git commit -m "feat: order tmux sessions ahead of herdr"
```

---

### Task 6: Muqun reads the range

**Files:**
- Modify: `Muqun/src/terminal/history.ts`
- Test: `Muqun/src/terminal/__tests__/history.test.ts` (exists; append)

**Interfaces:**
- Consumes: the `range` object from Task 4's envelope.
- Produces: `export function paneReadRange(read: unknown): { start: number; end: number; total: number } | null`

- [ ] **Step 6.1: Write the failing test**

```ts
import { paneReadRange, hasEarlierAfterPage } from '@/terminal/history';

test('a range at the top says there is nothing above it', () => {
  const read = { range: { start: 0, end: 500, total: 500 } };
  expect(paneReadRange(read)).toEqual({ start: 0, end: 500, total: 500 });
});

test('a malformed range is no range at all', () => {
  expect(paneReadRange({ range: { start: 'x', end: 1, total: 2 } })).toBeNull();
  expect(paneReadRange({})).toBeNull();
  expect(paneReadRange(null)).toBeNull();
});
```

- [ ] **Step 6.2: Run it and watch it fail**

Run: `cd ~/.osuki/Muqun && bun test history`
Expected: FAIL, `paneReadRange is not a function`.

- [ ] **Step 6.3: Implement**

```ts
/**
 * The absolute range a read came back with, or null where the gateway did not
 * say — herdr panes and gateways older than range addressing.
 *
 * Null is the signal to fall back to measuring; a present range is a fact and
 * replaces the measurement entirely.
 */
export function paneReadRange(
  read: unknown
): { start: number; end: number; total: number } | null {
  if (typeof read !== 'object' || read === null) return null;
  const range = (read as { range?: unknown }).range;
  if (typeof range !== 'object' || range === null) return null;
  const { start, end, total } = range as Record<string, unknown>;
  if (
    typeof start !== 'number' ||
    typeof end !== 'number' ||
    typeof total !== 'number'
  ) {
    return null;
  }
  return { start, end, total };
}
```

- [ ] **Step 6.4: Run it and watch it pass**

Run: `bun test history`
Expected: PASS.

- [ ] **Step 6.5: Commit**

```bash
cd ~/.osuki/Muqun
git add src/terminal/history.ts src/terminal/__tests__/history.test.ts
git commit -m "feat: read the absolute range a pane read reports"
```

---

### Task 7: Stop measuring when the range is there

**Files:**
- Modify: `Muqun/src/terminal/history.ts` (`hasEarlierAfterPage`)
- Test: `Muqun/src/terminal/__tests__/history.test.ts` (exists; append)

**Interfaces:**
- Consumes: `paneReadRange` from Task 6.

- [ ] **Step 7.1: Write the failing test**

```ts
test('a range decides "more above" without consulting line counts', () => {
  // The measured path would say no here: the page came back no longer than the
  // previous one. The range says the top has not been reached, and it is right.
  const read = { range: { start: 400, end: 900, total: 900 } };
  expect(hasEarlierAfterPage('a\nb', 500, 5000, null, 2, read)).toBe(true);
});

test('start at zero is the top', () => {
  const read = { range: { start: 0, end: 500, total: 900 } };
  expect(hasEarlierAfterPage('a\nb', 500, 5000, null, 0, read)).toBe(false);
});

test('without a range it still measures', () => {
  // herdr, and gateways older than range addressing.
  expect(hasEarlierAfterPage('a\nb', 500, 5000, null, 2, null)).toBe(false);
});
```

- [ ] **Step 7.2: Run it and watch it fail**

Run: `bun test history`
Expected: FAIL — `hasEarlierAfterPage` takes five arguments.

- [ ] **Step 7.3: Implement**

Add a trailing optional parameter so no existing call site breaks:

```ts
export function hasEarlierAfterPage(
  output: string,
  requestedLines: number,
  maximumLines: number,
  scroll: unknown,
  previousRows: number,
  read?: unknown
): boolean {
  // A reported range answers the question outright. The measuring below exists
  // because herdr's row count could not be trusted and there was no other
  // signal; where there is one, it is not a tiebreaker, it is the answer.
  const range = paneReadRange(read);
  if (range) return range.start > 0;

  if (terminalOutputLineCount(output) <= previousRows) return false;
  return hasEarlierTerminalOutput(output, requestedLines, maximumLines, scroll);
}
```

- [ ] **Step 7.4: Run it and watch it pass**

Run: `bun test history && npx tsc --noEmit`
Expected: PASS, no type errors.

- [ ] **Step 7.5: Commit**

```bash
git add src/terminal/history.ts src/terminal/__tests__/history.test.ts
git commit -m "feat: let a reported range answer whether history remains"
```

---

### Task 8: Page by range

**Files:**
- Modify: `Muqun/src/terminal/history.ts` (add `nextPageRange`)
- Modify: `Muqun/src/app/servers/[serverId].tsx:1430-1445` — the reader that
  issues the follow-up request and folds the result
- Test: `Muqun/src/terminal/__tests__/history.test.ts` (exists; append)

**Interfaces:**
- Consumes: `paneReadRange` from Task 6.
- Produces: `export function nextPageRange(range: {start: number}, pageSize: number): { start: number; end: number } | null`
- Existing, do not change its shape:
  `foldPaneRead(currentOutput: string, latestOutput: string, origin: PaneReadOrigin, maximumLines: number): string`

- [ ] **Step 8.1: Write the failing tests**

```ts
test('the next page is the one above the page held, and they do not overlap', () => {
  expect(nextPageRange({ start: 900 }, 500)).toEqual({ start: 400, end: 900 });
});

test('the next page stops at the top instead of going negative', () => {
  expect(nextPageRange({ start: 300 }, 500)).toEqual({ start: 0, end: 300 });
});

test('there is no page above the top', () => {
  expect(nextPageRange({ start: 0 }, 500)).toBeNull();
});

test('a page above the window is folded in ahead of it, losing no row', () => {
  // foldPaneRead takes (currentOutput, latestOutput, origin, maximumLines) and
  // 'page' is the one origin allowed to claim depth, so an older page folded
  // into a newer window lands above it.
  const window = 'l4\nl5';
  const olderPage = 'l1\nl2\nl3';
  expect(foldPaneRead(window, olderPage, 'page', 100)).toBe('l1\nl2\nl3\nl4\nl5');
});
```

- [ ] **Step 8.2: Run them and watch them fail**

Run: `bun test history`
Expected: FAIL, `nextPageRange is not a function`.

- [ ] **Step 8.3: Implement**

```ts
/**
 * The page immediately above the one already held.
 *
 * Pages are disjoint: the previous page starts where this one ends, so a page
 * costs its own lines rather than every line beneath it. Null means the held
 * page already begins at the oldest line.
 */
export function nextPageRange(
  range: { start: number },
  pageSize: number
): { start: number; end: number } | null {
  if (range.start <= 0) return null;
  return { start: Math.max(0, range.start - pageSize), end: range.start };
}
```

Then wire it into `src/app/servers/[serverId].tsx` around line 1430. That block
currently widens `nextLimit` and re-reads the tail. Change it to branch:

```tsx
      const lastRange = paneReadRange(lastReadRef.current);
      const page = lastRange ? nextPageRange(lastRange, PANE_PAGE_ROWS) : null;
      // A range-addressed page is disjoint from the window, so it costs its own
      // rows. Without one this stays the widening tail read it has always been.
      const value = page
        ? await readPaneRange(requestPaneId, page.start, page.end)
        : await readPaneTail(requestPaneId, nextLimit);

      setOutput((current) => foldPaneRead(current, value, 'page', nextLimit));
      setCanLoadEarlierOutput(
        hasEarlierAfterPage(
          value,
          nextLimit,
          MAX_PANE_OUTPUT_LINES,
          scroll,
          reachedRows,
          lastReadRef.current
        )
      );
```

`readPaneRange` / `readPaneTail` are thin wrappers over the generated
`getApiSessionsBySessionIdPanesByPaneIdOutput` call, differing only in whether
they pass `start`/`end` or `lines`. `lastReadRef` holds the whole parsed `read`
object of the previous response so `paneReadRange` can see its `range`; today
the code keeps only the output string.

Leave the `nextLimit` bookkeeping in place — it still bounds the window that
`foldPaneRead` trims to.

- [ ] **Step 8.4: Run them and watch them pass**

Run: `bun test && npx tsc --noEmit`
Expected: PASS, no type errors.

- [ ] **Step 8.5: Commit**

```bash
git add src/terminal/history.ts src/terminal/__tests__/history.test.ts
git commit -m "feat: page pane scrollback by disjoint absolute ranges"
```

---

### Task 9: End-to-end against the live gateway

**Files:**
- Modify: `src/backend/tmux.rs` (extend the existing `#[ignore]` contract test)

- [ ] **Step 9.1: Write the failing contract assertion**

Extend the contract test codex added, after its existing 240/720 assertions:

```rust
    let metrics = backend.pane_metrics(&split.id).await.unwrap();
    let total = metrics.0 + metrics.1;
    let lower = backend
        .read_pane(&ReadPane {
            pane_id: split.id.clone(),
            source: OutputSource::RecentUnwrapped,
            format: OutputFormat::Text,
            lines: 0,
            start: Some(total - 200),
            end: Some(total - 100),
        })
        .await
        .unwrap();
    let upper = backend
        .read_pane(&ReadPane {
            pane_id: split.id.clone(),
            source: OutputSource::RecentUnwrapped,
            format: OutputFormat::Text,
            lines: 0,
            start: Some(total - 100),
            end: Some(total),
        })
        .await
        .unwrap();
    let spanning = backend
        .read_pane(&ReadPane {
            pane_id: split.id.clone(),
            source: OutputSource::RecentUnwrapped,
            format: OutputFormat::Text,
            lines: 0,
            start: Some(total - 200),
            end: Some(total),
        })
        .await
        .unwrap();
    // Disjoint pages tile the span exactly: this is the property the whole
    // change exists for, and the one a growing tail could never have.
    assert_eq!(format!("{}\n{}", lower.text, upper.text), spanning.text);
    assert_eq!(lower.range.unwrap().end, upper.range.unwrap().start);
```

- [ ] **Step 9.2: Run it**

Run: `cargo test --offline -- --ignored 2>&1 | tail -5`
Expected: PASS. A failure here is a real off-by-one in `capture_bounds` — fix
the mapping, not the assertion.

- [ ] **Step 9.3: Full verification, both repos**

```bash
cd ~/.osuki/herdr-gateway
cargo fmt --check && cargo clippy --offline --all-targets -- -D warnings && cargo test --offline 2>&1 | tail -3
cargo build --release --offline
cd ~/.osuki/Muqun && npx tsc --noEmit && bun test 2>&1 | tail -3
```

- [ ] **Step 9.4: Update CONTEXT.md**

The verification section describes the contract tests. Add one line noting that
the tmux contract test now covers range tiling and needs a real tmux server.

- [ ] **Step 9.5: Commit**

```bash
cd ~/.osuki/herdr-gateway
git add src/backend/tmux.rs CONTEXT.md
git commit -m "test: assert disjoint ranges tile a tmux pane exactly"
```
