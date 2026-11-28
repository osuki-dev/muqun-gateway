use std::sync::Arc;
use std::time::Duration;

use super::CopilotDriver;
use crate::agents::adapters::fx::tests::integration::Fixture;
use crate::agents::adapters::memory_mirror::MemoryMirror;
use crate::agents::domain::{AgentSessionStatus, ModelRef};
use crate::agents::ports::agent::AgentPort;

async fn idle(driver: &CopilotDriver, id: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while driver.get_session(id).await.unwrap().status != AgentSessionStatus::Idle {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn native_selection_permissions_and_usage_keep_their_independent_meanings() {
    let fixture = Fixture::new().await;
    let (events, _) = tokio::sync::broadcast::channel(64);
    let driver = CopilotDriver::new(
        fixture
            .directory
            .join("mock-fx")
            .to_string_lossy()
            .into_owned(),
        fixture.directory.to_string_lossy().into_owned(),
        Arc::new(MemoryMirror::for_agent("copilot")),
        events,
    );
    let info = driver.create_session(None, None, None).await.unwrap();
    let id = &info.asid.0;
    let mode = info.mode;
    let model = ModelRef {
        provider_id: "copilot".into(),
        model_id: "other-model".into(),
        variant: Some("high".into()),
    };
    driver.switch_model(id, &model).await.unwrap();
    assert_eq!(driver.get_session(id).await.unwrap().model, Some(model));
    assert_eq!(driver.auto_approve(id).await.unwrap(), Some(false));
    driver.set_auto_approve(id, true).await.unwrap();
    assert_eq!(driver.auto_approve(id).await.unwrap(), Some(true));
    assert_eq!(driver.get_session(id).await.unwrap().mode, mode);
    driver.set_auto_approve(id, false).await.unwrap();
    std::fs::write(fixture.directory.join("normalize-allow-all"), "1").unwrap();
    assert!(driver.set_auto_approve(id, true).await.is_err());
    assert_eq!(driver.auto_approve(id).await.unwrap(), Some(false));
    assert!(!fixture.methods().contains(&"session/set_mode".into()));
    assert!(!fixture.methods().contains(&"response".into()));

    tokio::time::timeout(Duration::from_secs(3), async {
        while !driver.slash_commands_listed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    driver
        .send_prompt(id, "accounting", &[], None)
        .await
        .unwrap();
    idle(&driver, id).await;
    let info = driver.get_session(id).await.unwrap();
    let usage = info.tokens.unwrap();
    // Copilot's parent counters include cached input and reasoning output.
    assert_eq!((usage.input, usage.output), (10, 20));
    assert_eq!(usage.cache_write, Some(21598));
    assert_eq!(usage.reasoning, Some(53));
    let context = info.context_usage.unwrap();
    assert_eq!(context.total_tokens, 20482);
    assert_eq!(context.limit, Some(serde_json::json!({"context":128000})));
    driver.run_command(id, "context", "", None).await.unwrap();
    idle(&driver, id).await;
    assert_eq!(driver.get_session(id).await.unwrap().tokens, Some(usage));
    driver.shutdown().await;

    let (events, _) = tokio::sync::broadcast::channel(64);
    let resumed = CopilotDriver::new(
        fixture
            .directory
            .join("mock-fx")
            .to_string_lossy()
            .into_owned(),
        fixture.directory.to_string_lossy().into_owned(),
        Arc::new(MemoryMirror::for_agent("copilot")),
        events,
    );
    // The first scoped read loads the selected session and must return its
    // native defaults, rather than a global aggregate with no current model.
    let catalog = resumed
        .get_catalog_for_session(Some(id), None)
        .await
        .unwrap();
    assert!(catalog.defaults.model.is_some());
    assert_eq!(
        catalog.defaults.model,
        resumed.get_session(id).await.unwrap().model
    );
    resumed.shutdown().await;
}
