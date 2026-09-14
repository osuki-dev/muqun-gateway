//! Finite MCP stdio adapter over the existing scoped Unix reporting authority.
//! MCP 2025-11-25 lifecycle/tools and newline JSON-RPC transport; stdout is protocol only.
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
const MAX_INPUT: u64 = 256 * 1024;
const MAX_OUTPUT: usize = 6 * 1024 * 1024;

pub(super) async fn run() -> anyhow::Result<()> {
    serve(tokio::io::stdin(), tokio::io::stdout(), |name, arguments| async move {
        crate::work_local::reporting_request(&name, arguments).await
    }).await
}
fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
fn tools() -> Value {
    let string = json!({"type":"string"});
    let tool = |name: &str, description: &str, schema: Value, read_only: bool| json!({"name":name,"description":description,"inputSchema":schema,"annotations":{"readOnlyHint":read_only,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}});
    json!({"tools":[
        tool("context","Read the current assigned task, attempt and revision. No credentials are returned.",json!({"type":"object","properties":{},"additionalProperties":false}),true),
        tool("submit_result","Register an immutable result for this assigned attempt. Use one stable request_key for the intended submission. Human acceptance is separate; this does not complete the task.",json!({"type":"object","properties":{"request_key":{"type":"string","minLength":1,"maxLength":128},"expected_revision":{"type":"integer","minimum":0,"maximum":9007199254740991_u64},"input":{"type":"object","properties":{"attempt_id":string,"summary":string,"artifacts":{"type":"array","items":{"type":"object","properties":{"path":string,"sha256":string,"size_bytes":{"type":"integer","minimum":0}},"required":["path","sha256","size_bytes"],"additionalProperties":false}},"evidence":{"type":"array","items":string}},"required":["attempt_id","summary","artifacts","evidence"],"additionalProperties":false}},"required":["request_key","expected_revision","input"],"additionalProperties":false}),false),
        tool("result_receipt","Read this attempt's original result submission by request_key. Missing receipt does not prove that an in-flight submission failed and does not authorize resending.",json!({"type":"object","properties":{"request_key":{"type":"string","minLength":1,"maxLength":128}},"required":["request_key"],"additionalProperties":false}),true)
    ]})
}
async fn serve<R, W, C, F>(read: R, mut write: W, mut call: C) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    C: FnMut(String, Value) -> F,
    F: std::future::Future<Output = anyhow::Result<Value>>,
{
    let mut reader = BufReader::new(read);
    let mut initialized = false;
    let mut ready = false;
    loop {
        let mut bytes = Vec::new();
        let count = (&mut reader)
            .take(MAX_INPUT + 1)
            .read_until(b'\n', &mut bytes)
            .await?;
        if count == 0 {
            return Ok(());
        }
        if count as u64 > MAX_INPUT || bytes.last() != Some(&b'\n') {
            send(
                &mut write,
                error(Value::Null, -32700, "Invalid or oversized frame"),
            )
            .await?;
            return Ok(());
        }
        let request: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                send(&mut write, error(Value::Null, -32700, "Invalid JSON")).await?;
                continue;
            }
        };
        let id = request.get("id").cloned();
        let valid_id = id.as_ref().is_none_or(|id| {
            id.as_str().is_some_and(|s| s.len() <= 256)
                || id
                    .as_i64()
                    .is_some_and(|n| n.unsigned_abs() <= 9_007_199_254_740_991)
        });
        let method = request.get("method").and_then(Value::as_str);
        if !request.is_object() || request["jsonrpc"] != "2.0" || method.is_none() || !valid_id {
            send(&mut write, error(Value::Null, -32600, "Invalid request")).await?;
            continue;
        }
        let method = method.unwrap_or_default();
        if id.is_none() {
            if method == "notifications/initialized" && initialized {
                ready = true;
            }
            // Notifications cannot invoke a tool or trigger a scoped action.
            continue;
        }
        let id = id.unwrap_or(Value::Null);
        let params = request.get("params").cloned().unwrap_or(json!({}));
        let response = match method {
            "initialize" if !initialized => {
                if !params["protocolVersion"].is_string()
                    || !params["capabilities"].is_object()
                    || !params["clientInfo"].is_object()
                {
                    error(id, -32602, "Invalid initialization parameters")
                } else {
                    initialized = true;
                    let requested = params["protocolVersion"].as_str().unwrap_or_default();
                    let version = if matches!(
                        requested,
                        "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
                    ) {
                        requested
                    } else {
                        "2025-11-25"
                    };
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":version,"capabilities":{"tools":{}},"serverInfo":{"name":"muqun_reporting","version":env!("CARGO_PKG_VERSION")},"instructions":"Use context to obtain your assigned attempt and revision, submit_result to register work, and result_receipt for read-only recovery. These tools cannot approve, publish, execute commands, or mark human acceptance."}})
                }
            }
            "ping" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
            _ if !ready => error(id, -32000, "Initialization required"),
            "tools/list" if params.as_object().is_some_and(|p| p.is_empty()) => {
                json!({"jsonrpc":"2.0","id":id,"result":tools()})
            }
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                if !matches!(name, "context" | "submit_result" | "result_receipt")
                    || !arguments.is_object()
                    || params.as_object().is_none_or(|p| {
                        p.keys()
                            .any(|key| !matches!(key.as_str(), "name" | "arguments" | "_meta"))
                    })
                {
                    error(id, -32602, "Unknown tool or invalid arguments")
                } else {
                    let result = call(name.into(), arguments).await;
                    let (value, is_error) = match result {
                        Ok(value) => {
                            let failed = value.get("error").is_some();
                            (value, failed)
                        }
                        Err(_) => (
                            json!({"error":"reporting_unavailable","message":"Reporting was not confirmed. Check scoped context or recover the original request_key; do not automatically resend."}),
                            true,
                        ),
                    };
                    json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":serde_json::to_string(&value)?}],"isError":is_error}})
                }
            }
            "tools/list" => error(id, -32602, "Invalid list parameters"),
            _ => error(id, -32601, "Method not found"),
        };
        send(&mut write, response).await?;
    }
}
async fn send<W: AsyncWrite + Unpin>(write: &mut W, value: Value) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(&value)?;
    if bytes.len() > MAX_OUTPUT {
        bytes = serde_json::to_vec(&error(
            value["id"].clone(),
            -32000,
            "Reporting response exceeds size limit; recover by request key",
        ))?;
    }
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    write.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn exchange(lines: Vec<Value>) -> (Vec<Value>, Vec<(String, Value)>) {
        let input = lines
            .into_iter()
            .map(|line| format!("{}\n", line))
            .collect::<String>();
        let mut output = Vec::new();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let observed = calls.clone();
        serve(input.as_bytes(), &mut output, move |name, args| {
            observed.lock().unwrap().push((name, args));
            async { Ok(json!({"value":{"id":"receipt"}})) }
        })
        .await
        .unwrap();
        let values = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let calls = calls.lock().unwrap().clone();
        (values, calls)
    }
    fn init() -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})
    }
    fn ready() -> Value {
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    }
    #[tokio::test]
    async fn reporting_mcp_negotiates_and_exposes_only_finite_tools() {
        let(result,calls)=exchange(vec![init(),ready(),json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"context","arguments":{}}}),json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"execute","arguments":{"command":"secret"}}})]).await;
        assert_eq!(result[0]["result"]["serverInfo"]["name"], "muqun_reporting");
        assert_eq!(result[1]["result"]["tools"].as_array().unwrap().len(), 3);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "context");
        assert_eq!(result[3]["error"]["code"], -32602);
    }
    #[tokio::test]
    async fn reporting_mcp_notifications_and_uninitialized_calls_never_act() {
        let (result, calls) = exchange(vec![
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"submit_result"}}),
            init(),
            ready(),
            json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"submit_result"}}),
        ])
        .await;
        assert!(calls.is_empty());
        assert_eq!(result[0]["error"]["code"], -32000);
    }
    #[tokio::test]
    async fn reporting_mcp_bounds_frames_and_sanitizes_transport_errors() {
        let mut out = vec![];
        let input = vec![b'x'; MAX_INPUT as usize + 1];
        serve(input.as_slice(), &mut out, |_, _| async {
            panic!("no tool")
        })
        .await
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 1);
        let input = format!(
            "{}\n{}\n{}\n",
            init(),
            ready(),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"context"}})
        );
        let mut out = vec![];
        serve(input.as_bytes(), &mut out, |_, _| async {
            anyhow::bail!("secret-token-private-path")
        })
        .await
        .unwrap();
        let output = String::from_utf8(out).unwrap();
        assert!(!output.contains("secret-token"));
        assert!(output.contains("reporting_unavailable"));
    }
}
