pub mod events;
pub mod form;
pub mod model;
pub mod permission;
pub mod session;
pub mod timeline;

pub use events::AgentDomainEvent;
pub use form::{FormField, FormOption, FormRequest, FormWhen};
pub use model::{
    AgentCatalog, AgentInfo, CatalogDefaults, CommandInfo, McpServerInfo, ModelInfo,
    ModelVariantInfo, ProviderInfo, ProviderModelInfo, SkillInfo,
};
pub use permission::{PermissionDecision, PermissionOption, PermissionRequest};
pub use session::{
    AgentErrorInfo, AgentProject, AgentSessionId, AgentSessionInfo, AgentSessionStatus, ModelRef,
    SessionForkInfo, SessionQuery, SessionRevertInfo, TokensUsage,
};
pub use timeline::{
    part_item_id, reasoning_item_id, text_item_id, tool_item_id, AgentPart, CompactionStatus,
    TimelineItem, TimelineRole, TodoItem, ToolCall, ToolCallStatus, ToolTime,
};


#[cfg(test)]
mod contract_tests {
    //! The field names `docs/agent-api.md` promises the app team. A rename
    //! that does not also update the document fails here.
    use super::*;
    use serde_json::{json, Value};

    fn keys(value: &Value) -> Vec<String> {
        value
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }

    #[test]
    fn session_info_keys_match_the_contract() {
        let info = AgentSessionInfo {
            asid: AgentSessionId("ses_1".into()),
            backend_session_id: "ses_1".into(),
            title: "t".into(),
            agent: Some("build".into()),
            model: Some(ModelRef {
                provider_id: "opencode".into(),
                model_id: "m".into(),
                variant: None,
            }),
            status: AgentSessionStatus::Busy,
            directory: Some("/d".into()),
            cost: Some(0.5),
            tokens: Some(TokensUsage::default()),
            limit: None,
            parent_id: Some("ses_0".into()),
            project_id: Some("p".into()),
            outcome: Some("succeeded".into()),
            error: None,
            revert: Some(SessionRevertInfo {
                message_id: "msg_1".into(),
                part_id: None,
                snapshot: None,
                files: None,
            }),
            fork: None,
            time_idle: Some(2),
            time_viewed: Some(1),
            deleted: false,
            updated_ms: 3,
        };
        let value = serde_json::to_value(&info).expect("serializes");
        for key in [
            "asid",
            "backend_session_id",
            "title",
            "agent",
            "model",
            "status",
            "directory",
            "parent_id",
            "project_id",
            "outcome",
            "revert",
            "time_idle",
            "time_viewed",
            "updated_ms",
        ] {
            assert!(keys(&value).contains(&key.to_string()), "missing {key}");
        }
        assert_eq!(value["status"], "busy");
        assert_eq!(value["asid"], "ses_1", "the session id is a bare string");
        assert_eq!(value["model"]["provider_id"], "opencode");
        assert_eq!(value["revert"]["message_id"], "msg_1");
        assert!(
            !keys(&value).contains(&"deleted".to_string()),
            "`deleted` is omitted unless the session is gone"
        );
    }

    #[test]
    fn a_tool_part_keeps_both_state_and_status() {
        let part = AgentPart::Tool(ToolCall {
            id: "call_1".into(),
            name: "glob".into(),
            title: Some("**/*".into()),
            input: json!({ "pattern": "**/*" }),
            output: Some(json!("a.rs")),
            content: Some(json!([{ "type": "text", "text": "a.rs" }])),
            metadata: Some(json!({ "sessionID": "ses_c", "truncated": false })),
            state: ToolCallStatus::Completed,
            status: ToolCallStatus::Completed,
            error: None,
            child_session_id: Some("ses_c".into()),
            background: true,
            truncated: false,
            time: ToolTime {
                created: Some(1),
                ran: Some(2),
                completed: Some(3),
            },
        });
        let value = serde_json::to_value(&part).expect("serializes");
        assert_eq!(value["type"], "tool");
        assert_eq!(value["state"], "completed");
        assert_eq!(value["status"], "completed", "the legacy name carries the same value");
        assert_eq!(value["child_session_id"], "ses_c");
        assert_eq!(value["background"], true);
        assert_eq!(value["metadata"]["sessionID"], "ses_c", "metadata is verbatim");
        assert_eq!(value["time"]["ran"], 2);
        assert!(
            !keys(&value).contains(&"truncated".to_string()),
            "`truncated` is omitted while false"
        );
    }

    #[test]
    fn the_error_tool_state_is_accepted_as_an_alias_for_failed() {
        let from_failed: ToolCallStatus = serde_json::from_value(json!("failed")).expect("failed");
        let from_error: ToolCallStatus = serde_json::from_value(json!("error")).expect("error");
        assert_eq!(from_failed, ToolCallStatus::Failed);
        assert_eq!(from_error, ToolCallStatus::Failed);
        assert_eq!(
            serde_json::to_value(ToolCallStatus::Failed).expect("serializes"),
            json!("failed")
        );
    }

    #[test]
    fn event_names_match_their_serialized_type() {
        let asid = AgentSessionId("ses_1".into());
        let events = vec![
            AgentDomainEvent::StatusChanged {
                asid: asid.clone(),
                status: AgentSessionStatus::Interrupted,
                error: None,
                seq: 1,
            },
            AgentDomainEvent::CompactionChanged {
                asid: asid.clone(),
                status: CompactionStatus::Started,
                reason: Some("manual".into()),
                delta: None,
                seq: 2,
            },
            AgentDomainEvent::InboxChanged {
                asid: asid.clone(),
                items: vec![],
                seq: 3,
            },
            AgentDomainEvent::Resync {
                asid,
                reason: "event_backlog_overflow".into(),
            },
        ];
        for event in &events {
            let value = serde_json::to_value(event).expect("serializes");
            assert_eq!(
                value["type"], event.event_name(),
                "the SSE event name and the payload's own type must agree"
            );
        }

        let compaction = serde_json::to_value(&events[1]).expect("serializes");
        assert_eq!(
            compaction["session_id"], "ses_1",
            "the compaction and inbox events name the session `session_id`"
        );
        let status = serde_json::to_value(&events[0]).expect("serializes");
        assert_eq!(status["status"], "interrupted");
    }

    #[test]
    fn a_permission_request_exposes_save_and_its_source() {
        let request = PermissionRequest {
            id: "per_1".into(),
            asid: AgentSessionId("ses_1".into()),
            action: "shell".into(),
            resources: vec!["ls".into()],
            save: vec!["ls *".into()],
            prompt: "p".into(),
            tool: None,
            source_message_id: Some("msg_1".into()),
            source_tool_call_id: Some("call_1".into()),
            metadata: None,
            message: None,
            options: vec![],
        };
        let value = serde_json::to_value(&request).expect("serializes");
        assert_eq!(value["save"][0], "ls *");
        assert_eq!(value["source_tool_call_id"], "call_1");
        assert_eq!(value["source_message_id"], "msg_1");
    }
}
