use std::sync::Arc;
use std::time::Duration;

use super::QoderDriver;
use crate::agents::adapters::fx::tests::integration::Fixture;
use crate::agents::domain::{
    AgentPart, AgentSessionStatus, PermissionDecision, TimelineRole, ToolCallStatus,
};
use crate::agents::ports::agent::{AgentError, AgentPort};

async fn status(driver: &Arc<QoderDriver>, id: &str, expected: AgentSessionStatus) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if driver.get_session(id).await.unwrap().status == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Qoder status transition");
}

#[tokio::test]
async fn idless_echo_stream_followup_cancel_and_history_reuse_shared_lifecycle() {
    let fixture = Fixture::new().await;
    let driver = fixture.qoder_driver();
    assert_eq!(driver.kind(), "qoder");
    let info = driver.create_session(None, None, None).await.unwrap();
    let id = &info.asid.0;
    for text in ["first", "follow up"] {
        driver.send_prompt(id, text, &[], None).await.unwrap();
        status(&driver, id, AgentSessionStatus::Idle).await;
    }
    let rows = driver.get_timeline(id, 100).await.unwrap();
    assert_eq!(
        rows.len(),
        4,
        "native user echoes must not duplicate flushed submissions"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.role == TimelineRole::User)
            .count(),
        2
    );
    assert!(matches!(&rows[1].part, AgentPart::Text{text} if text == "AB"));
    assert!(matches!(&rows[3].part, AgentPart::Text{text} if text == "AB"));
    let catalog = driver
        .get_catalog_for_session(Some(id), None)
        .await
        .unwrap();
    assert!(catalog.models.is_empty());
    assert!(catalog.modes.is_empty());
    driver.shutdown().await;
    let resumed = fixture.qoder_driver();
    resumed.prepare_selected_session(id).await.unwrap();
    assert_eq!(resumed.get_timeline(id, 100).await.unwrap().len(), 4);
    resumed.send_prompt(id, "hold", &[], None).await.unwrap();
    assert!(matches!(
        resumed.send_prompt(id, "duplicate", &[], None).await,
        Err(AgentError::NotAvailable(_))
    ));
    resumed.interrupt(id).await.unwrap();
    status(&resumed, id, AgentSessionStatus::Interrupted).await;
    assert_eq!(
        fixture
            .methods()
            .iter()
            .filter(|method| *method == "session/prompt")
            .count(),
        3
    );
    resumed.shutdown().await;
}

#[tokio::test]
async fn permission_requires_explicit_offered_choice_and_publishes_completed_tool() {
    let fixture = Fixture::new().await;
    let driver = fixture.qoder_driver();
    let info = driver.create_session(None, None, None).await.unwrap();
    let id = &info.asid.0;
    driver.send_prompt(id, "approval", &[], None).await.unwrap();
    let permission = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let pending = driver.get_pending_permissions(id).await.unwrap();
            if let Some(permission) = pending.first() {
                break permission.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!fixture.methods().contains(&"response".into()));
    assert!(driver
        .reply_permission(id, &permission.id, PermissionDecision::Deny, None)
        .await
        .is_err());
    driver
        .reply_permission(id, &permission.id, PermissionDecision::Allow, None)
        .await
        .unwrap();
    status(&driver, id, AgentSessionStatus::Idle).await;
    assert!(driver
        .reply_permission(id, &permission.id, PermissionDecision::Allow, None)
        .await
        .is_err());
    let rows = driver.get_timeline(id, 100).await.unwrap();
    assert!(rows.iter().any(|row| matches!(&row.part, AgentPart::Tool(tool) if tool.status == ToolCallStatus::Completed && tool.output == Some(serde_json::json!("approved")))));
    assert!(rows
        .iter()
        .any(|row| matches!(&row.part, AgentPart::Text{text} if text == "Permission answered")));
    driver.shutdown().await;
}
