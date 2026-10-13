//! Translate supported terminal launch choices into a new shared thread.
//! Unknown CLI semantics are never discarded or forwarded to another runtime.
use serde_json::{json, Map, Value};

pub(super) struct LaunchConfig {
    pub params: Value,
    pub ui_args: Vec<String>,
}

pub(super) fn parse(cwd: &str, args: &[String]) -> Result<LaunchConfig, &'static str> {
    if !std::path::Path::new(cwd).is_absolute() {
        return Err("shared Codex startup requires an absolute working directory");
    }
    let mut params = json!({"cwd":cwd});
    let mut config = Map::new();
    let mut ui_args = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dangerously-bypass-approvals-and-sandbox" => {
                params["approvalPolicy"] = json!("never");
                params["sandbox"] = json!("danger-full-access");
            }
            "--no-alt-screen" => ui_args.push(arg.clone()),
            "-m" | "--model" => {
                params["model"] = json!(args.next().ok_or("Codex model argument is missing")?);
            }
            "-a" | "--ask-for-approval" => {
                let policy = args.next().ok_or("Codex approval argument is missing")?;
                if !matches!(
                    policy.as_str(),
                    "untrusted" | "on-failure" | "on-request" | "never"
                ) {
                    return Err("Codex approval policy is unsupported for shared startup");
                }
                params["approvalPolicy"] = json!(policy);
            }
            "-s" | "--sandbox" => {
                let sandbox = args.next().ok_or("Codex sandbox argument is missing")?;
                if !matches!(
                    sandbox.as_str(),
                    "read-only" | "workspace-write" | "danger-full-access"
                ) {
                    return Err("Codex sandbox policy is unsupported for shared startup");
                }
                params["sandbox"] = json!(sandbox);
            }
            "-c" | "--config" => {
                configuration(
                    args.next()
                        .ok_or("Codex configuration argument is missing")?,
                    &mut config,
                )?;
            }
            other if other.starts_with("--config=") => {
                configuration(&other[9..], &mut config)?;
            }
            _ => {
                return Err(
                    "configured Codex CLI options require the existing terminal startup path",
                )
            }
        }
    }
    // The official server otherwise persists trust for a write-capable new
    // thread. Preserve the launcher's existing per-invocation trust choice;
    // never turn a task launch into a global configuration write.
    if config
        .get("projects")
        .and_then(|projects| projects.get(cwd))
        .and_then(|project| project.get("trust_level"))
        != Some(&json!("trusted"))
    {
        return Err(
            "shared Codex startup requires the existing per-invocation folder trust override",
        );
    }
    if let Some(instructions) = config.remove("developer_instructions") {
        params["developerInstructions"] = instructions;
    }
    if !config.is_empty() {
        params["config"] = Value::Object(config);
    }
    Ok(LaunchConfig { params, ui_args })
}

fn configuration(raw: &str, config: &mut Map<String, Value>) -> Result<(), &'static str> {
    let (key, raw) = raw
        .split_once('=')
        .ok_or("Codex configuration requires key=value")?;
    // These settings are known per-thread inputs in the pinned official API.
    // Other CLI settings may select an executor, home, auth or global service.
    if !matches!(
        key,
        "projects"
            | "developer_instructions"
            | "model"
            | "model_provider"
            | "model_reasoning_effort"
            | "mcp_servers.muqun.url"
            | "mcp_servers.muqun.http_headers_helper"
    ) {
        return Err("configured Codex settings require the existing terminal startup path");
    }
    let value = toml::from_str::<toml::Table>(&format!("value = {raw}"))
        .ok()
        .and_then(|mut table| table.remove("value"))
        .unwrap_or_else(|| toml::Value::String(raw.to_owned()));
    let value = serde_json::to_value(value).map_err(|_| "invalid Codex configuration")?;
    if (key == "projects" && !value.is_object()) || (key != "projects" && !value.is_string()) {
        return Err("unsupported Codex configuration value type");
    }
    config.insert(key.into(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).into()).collect()
    }

    #[test]
    fn preserves_thread_instructions_mcp_trust_model_and_permissions_without_tui_config() {
        let launch = parse(
            "/synthetic/project",
            &args(&[
                "--dangerously-bypass-approvals-and-sandbox",
                "-c",
                "projects={ '/synthetic/project' = { trust_level = 'trusted' } }",
                "-c",
                "developer_instructions='Keep user rules. Complete the task.'",
                "-c",
                "mcp_servers.muqun.url='http://127.0.0.1:1234/mcp'",
                "-c",
                "mcp_servers.muqun.http_headers_helper='fixture-helper --task-id synthetic-task'",
                "--model",
                "fixture-model",
                "-c",
                "model_reasoning_effort='high'",
                "--no-alt-screen",
            ]),
        )
        .unwrap();
        assert_eq!(
            launch.params["developerInstructions"],
            "Keep user rules. Complete the task."
        );
        assert_eq!(
            launch.params["config"]["projects"]["/synthetic/project"]["trust_level"],
            "trusted"
        );
        assert_eq!(
            launch.params["config"]["mcp_servers.muqun.http_headers_helper"],
            "fixture-helper --task-id synthetic-task"
        );
        assert_eq!(launch.params["model"], "fixture-model");
        assert_eq!(launch.params["config"]["model_reasoning_effort"], "high");
        assert_eq!(launch.params["approvalPolicy"], "never");
        assert_eq!(launch.params["sandbox"], "danger-full-access");
        assert_eq!(launch.ui_args, ["--no-alt-screen"]);
    }

    #[test]
    fn unsupported_executor_and_configuration_are_not_silently_lost() {
        for values in [
            vec!["--no-daemon"],
            vec!["--oss"],
            vec!["--profile", "custom"],
            vec!["-c", "shell_environment_policy.inherit='none'"],
            vec!["--remote", "ws://unrelated"],
        ] {
            assert!(parse("/synthetic/project", &args(&values)).is_err());
        }
        assert!(parse("relative", &[]).is_err());
        assert!(parse("/synthetic/project", &[]).is_err());
    }
}
