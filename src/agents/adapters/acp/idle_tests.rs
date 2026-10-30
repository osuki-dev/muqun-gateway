use super::*;
use crate::agents::adapters::fx::tests::integration::Fixture;

struct RecoverableFixture;
impl AcpProvider for RecoverableFixture {
    const KIND: &'static str = "fx";
    const LABEL: &'static str = "Recoverable fixture";
    const ARGS: &'static [&'static str] = &["acp"];
    const REQUIRE_MESSAGE_ID: bool = true;
    const MODEL_SELECTION: bool = true;
    const IDLE_RETIREMENT: bool = true;
    fn recovery_snapshot<'a>(
        _: &'a str,
        directory: &'a str,
        id: &'a str,
        _: &'a CancellationToken,
    ) -> AgentFuture<'a, NativeSnapshot> {
        Box::pin(async move {
            let directory = std::path::Path::new(directory);
            if directory.join("block-inspection").exists() {
                std::fs::write(directory.join("inspection-entered"), "ready").unwrap();
                while directory.join("block-inspection").exists() {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
            let data: Value = serde_json::from_slice(
                &tokio::fs::read(directory.join("sessions.json"))
                    .await
                    .map_err(|_| protocol("fixture history missing"))?,
            )
            .unwrap();
            let rows = data[id]
                .as_array()
                .ok_or_else(|| protocol("fixture session missing"))?;
            if rows.is_empty() {
                return Err(protocol("unsaved empty fixture"));
            }
            let mut entries = Vec::new();
            let mut last_user = None;
            let mut last_assistant = None;
            for pair in rows.chunks(2) {
                if pair.len() != 2 || pair[0][2] != true || pair[1][2] != false {
                    return Err(protocol("fixture turn unfinished"));
                }
                entries.push(json!(pair));
                last_user = pair[0][1].as_str().map(str::to_owned);
                last_assistant = pair[1][1].as_str().map(str::to_owned);
            }
            Ok(NativeSnapshot {
                session_id: id.into(),
                entries,
                last_user,
                last_assistant,
            })
        })
    }
}

fn driver(fixture: &Fixture) -> Arc<AcpDriver<RecoverableFixture>> {
    let driver = AcpDriver::new(
        fixture.driver.binary.clone(),
        fixture.driver.default_directory.clone(),
        Arc::new(MemoryMirror::for_agent("fx")),
        broadcast::channel(256).0,
    );
    driver.set_idle_timeout(Duration::from_secs(1));
    driver
}

async fn idle<P: AcpProvider>(driver: &AcpDriver<P>, id: &str) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let session = driver.retained(id).ok_or("native generation ended")?;
            let state = session.state.lock().await;
            if !state.permissions.is_empty() {
                return Err("native approval requires the owner; no answer was sent".into());
            }
            if !session.busy.load(Ordering::Acquire) {
                return if state.info.status == AgentSessionStatus::Idle {
                    Ok(())
                } else {
                    Err("native turn did not complete normally".into())
                };
            }
            drop(state);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|_| "native turn timed out; no retry".to_owned())?
}

async fn expire<P: AcpProvider>(driver: &AcpDriver<P>, id: &str) {
    driver
        .retained(id)
        .unwrap()
        .state
        .lock()
        .await
        .last_activity = tokio::time::Instant::now() - Duration::from_secs(2);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
}

async fn dormant<P: AcpProvider>(driver: &AcpDriver<P>, id: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !driver.dormant.lock().unwrap().contains_key(id) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

// A detached OS process models the native guard's EOF cleanup hazard. The
// completed tool record is intentionally absent from the fixture's journal:
// later history/ordinary turns must never clear live execution ownership.
#[cfg(unix)]
#[tokio::test]
async fn observed_execution_keeps_detached_background_work_alive_across_idle_sweeps() {
    let fixture = Fixture::new().await;
    let binary = fixture.directory.join("mock-fx");
    let script = std::fs::read_to_string(&binary).unwrap()
        .replace("import json, os, sys, time", "import json, os, sys, time, subprocess, signal\nbackground=None")
        .replace("        turn+=1", r#"        turn+=1
        if turn==1:
            background=subprocess.Popen([sys.executable,'background-sentinel.py'],start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            with open('background-pid','w') as f:f.write(str(background.pid))
            update(active,{'sessionUpdate':'tool_call','toolCallId':'background','title':'Start background work','status':'completed'})"#);
    std::fs::write(&binary, format!("{script}\nif background is not None:\n    with open('eof-guard-ran','w') as f:f.write('cleanup')\n    os.killpg(background.pid,signal.SIGKILL)\n    background.wait()\n")).unwrap();
    std::fs::write(
        fixture.directory.join("background-sentinel.py"),
        r#"
import time
while True:
    with open('heartbeat','w') as f:f.write(str(time.time_ns()))
    time.sleep(0.02)
"#,
    )
    .unwrap();
    let driver = driver(&fixture);
    let info = driver.create_session(None, None, None).await.unwrap();
    driver
        .send_prompt(&info.asid.0, "start", &[], None)
        .await
        .unwrap();
    idle(&driver, &info.asid.0).await.unwrap();
    let pid: i32 = std::fs::read_to_string(fixture.directory.join("background-pid"))
        .unwrap()
        .parse()
        .unwrap();
    // The still-live fixture owns the unreaped detached group leader, keeping
    // its identity reserved until this guard cleans up our own sentinel.
    struct Sentinel(i32);
    impl Drop for Sentinel {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
    let sentinel = Sentinel(pid);
    for prompt in [None, Some("a later ordinary turn")] {
        if let Some(prompt) = prompt {
            driver
                .send_prompt(&info.asid.0, prompt, &[], None)
                .await
                .unwrap();
            idle(&driver, &info.asid.0).await.unwrap();
        }
        expire(&driver, &info.asid.0).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !fixture.directory.join("eof-guard-ran").exists(),
            "idle EOF would kill the background job"
        );
        assert!(driver.retained(&info.asid.0).unwrap().client.is_running());
        assert!(!driver.dormant.lock().unwrap().contains_key(&info.asid.0));
        let before = std::fs::read_to_string(fixture.directory.join("heartbeat")).unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let after = std::fs::read_to_string(fixture.directory.join("heartbeat")).unwrap();
        assert_ne!(
            before, after,
            "the detached background process must keep working"
        );
    }
    drop(sentinel);
    driver.shutdown().await;
}

#[tokio::test]
async fn direct_slash_delivery_pins_before_write_and_later_plain_turn_cannot_clear_it() {
    // Native extension commands can be absent from, or newer than, the last
    // advertised catalog. Both routes still deliver the same prompt bytes.
    for command in [
        "/extension-command arguments",
        " \t /late-command arguments",
    ] {
        let fixture = Fixture::new().await;
        let driver = driver(&fixture);
        let info = driver.create_session(None, None, None).await.unwrap();
        driver
            .send_prompt(&info.asid.0, command, &[], None)
            .await
            .unwrap();
        assert!(driver
            .retained(&info.asid.0)
            .unwrap()
            .client
            .has_unverified_live_work());
        idle(&driver, &info.asid.0).await.unwrap();
        driver
            .send_prompt(&info.asid.0, "a later plain turn", &[], None)
            .await
            .unwrap();
        idle(&driver, &info.asid.0).await.unwrap();
        expire(&driver, &info.asid.0).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(driver.retained(&info.asid.0).unwrap().client.is_running());
        assert!(!driver.dormant.lock().unwrap().contains_key(&info.asid.0));
        driver.shutdown().await;
    }
}

#[tokio::test]
async fn parked_observation_drops_live_projection_and_concurrent_foreground_prepares_load_once() {
    let fixture = Fixture::new().await;
    let driver = driver(&fixture);
    let info = driver.create_session(None, None, None).await.unwrap();
    driver
        .send_prompt(&info.asid.0, "hello", &[], None)
        .await
        .unwrap();
    idle(&driver, &info.asid.0).await.unwrap();
    let weak = Arc::downgrade(&driver.retained(&info.asid.0).unwrap());
    let before = driver.get_timeline(&info.asid.0, 100).await.unwrap();
    expire(&driver, &info.asid.0).await;
    dormant(&driver, &info.asid.0).await;
    assert!(
        weak.upgrade().is_none(),
        "retirement must release the full live projection and client"
    );
    assert!(driver.sessions.lock().unwrap().is_empty());
    assert_eq!(
        driver.dormant.lock().unwrap()[&info.asid.0].kind,
        DormantKind::Parked
    );
    assert_eq!(
        driver
            .get_session(&info.asid.0)
            .await
            .unwrap()
            .runtime
            .unwrap()
            .interaction,
        RuntimeInteraction::Native
    );
    let methods = fixture.methods();
    driver.prepare_observed_session(&info.asid.0).await.unwrap();
    assert_eq!(
        driver.get_timeline(&info.asid.0, 100).await.unwrap().len(),
        before.len()
    );
    driver.get_pending_permissions(&info.asid.0).await.unwrap();
    driver
        .load_earlier_timeline(&info.asid.0, 100)
        .await
        .unwrap();
    driver
        .get_catalog_for_session(Some(&info.asid.0), None)
        .await
        .unwrap();
    driver.get_session(&info.asid.0).await.unwrap();
    assert_eq!(
        fixture.methods(),
        methods,
        "background reads must not load or prompt"
    );
    let (first, second) = tokio::join!(
        driver.prepare_selected_session(&info.asid.0),
        driver.prepare_selected_session(&info.asid.0)
    );
    first.unwrap();
    second.unwrap();
    assert!(driver
        .retained(&info.asid.0)
        .unwrap()
        .connected
        .load(Ordering::Acquire));
    assert!(!driver.dormant.lock().unwrap().contains_key(&info.asid.0));
    assert_eq!(
        fixture
            .methods()
            .iter()
            .filter(|method| *method == "session/load")
            .count(),
        1
    );
    assert_eq!(
        fixture
            .methods()
            .iter()
            .filter(|method| *method == "session/prompt")
            .count(),
        1
    );
    driver.shutdown().await;
}

#[tokio::test]
async fn foreground_send_waits_for_retiring_generation_then_submits_one_new_prompt() {
    let fixture = Fixture::new().await;
    let driver = driver(&fixture);
    let info = driver.create_session(None, None, None).await.unwrap();
    driver
        .send_prompt(&info.asid.0, "first", &[], None)
        .await
        .unwrap();
    idle(&driver, &info.asid.0).await.unwrap();
    let directory = std::path::Path::new(&driver.default_directory);
    std::fs::write(directory.join("block-inspection"), "wait").unwrap();
    expire(&driver, &info.asid.0).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !directory.join("inspection-entered").exists() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    let submitting = driver.clone();
    let id = info.asid.0.clone();
    let send = tokio::spawn(async move { submitting.send_prompt(&id, "second", &[], None).await });
    tokio::task::yield_now().await;
    assert_eq!(
        fixture
            .methods()
            .iter()
            .filter(|method| *method == "session/prompt")
            .count(),
        1
    );
    std::fs::remove_file(directory.join("block-inspection")).unwrap();
    send.await.unwrap().unwrap();
    idle(&driver, &info.asid.0).await.unwrap();
    assert_eq!(
        fixture
            .methods()
            .iter()
            .filter(|method| *method == "session/load")
            .count(),
        1
    );
    assert_eq!(
        fixture
            .methods()
            .iter()
            .filter(|method| *method == "session/prompt")
            .count(),
        2
    );
    driver.shutdown().await;
}

#[tokio::test]
async fn unsaved_blank_busy_permission_and_cancelled_turns_are_not_retired() {
    let fixture = Fixture::new().await;
    let driver = driver(&fixture);
    let info = driver.create_session(None, None, None).await.unwrap();
    expire(&driver, &info.asid.0).await;
    tokio::task::yield_now().await;
    assert!(driver.retained(&info.asid.0).is_some());
    driver
        .send_prompt(&info.asid.0, "approval", &[], None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while driver
            .get_pending_permissions(&info.asid.0)
            .await
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    expire(&driver, &info.asid.0).await;
    tokio::task::yield_now().await;
    let session = driver.retained(&info.asid.0).unwrap();
    assert!(session.busy.load(Ordering::Acquire));
    assert!(session.client.is_running());
    driver.interrupt(&info.asid.0).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while session.busy.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(session.state.lock().await.recovery.is_none());
    expire(&driver, &info.asid.0).await;
    assert!(driver.retained(&info.asid.0).is_some());
    driver.shutdown().await;
}

#[tokio::test]
async fn unexpected_disconnect_never_replays_and_nonzero_retirement_is_unknown() {
    let fixture = Fixture::new().await;
    let driver = driver(&fixture);
    let info = driver.create_session(None, None, None).await.unwrap();
    driver
        .send_prompt(&info.asid.0, "disconnect", &[], None)
        .await
        .unwrap();
    dormant(&driver, &info.asid.0).await;
    assert_eq!(
        driver.dormant.lock().unwrap()[&info.asid.0].kind,
        DormantKind::Unexpected
    );
    let methods = fixture.methods();
    assert!(driver
        .send_prompt(&info.asid.0, "do not replay", &[], None)
        .await
        .is_err());
    assert_eq!(fixture.methods(), methods);
    driver.shutdown().await;
    // A distinct fixture phase fails only after EOF; it has a valid saved turn.
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&fixture.driver.binary)
        .unwrap()
        .write_all(b"\nsys.exit(7)\n")
        .unwrap();
    let failed = driver_for_failure(&fixture);
    let info = failed.create_session(None, None, None).await.unwrap();
    failed
        .send_prompt(&info.asid.0, "hello", &[], None)
        .await
        .unwrap();
    idle(&failed, &info.asid.0).await.unwrap();
    let before = failed
        .mirror
        .get_snapshot(&info.asid)
        .await
        .unwrap()
        .timeline;
    expire(&failed, &info.asid.0).await;
    dormant(&failed, &info.asid.0).await;
    assert_eq!(
        failed.dormant.lock().unwrap()[&info.asid.0].kind,
        DormantKind::Unexpected
    );
    assert_eq!(
        failed.get_session(&info.asid.0).await.unwrap().status,
        AgentSessionStatus::Unknown
    );
    assert_eq!(
        failed
            .mirror
            .get_snapshot(&info.asid)
            .await
            .unwrap()
            .timeline,
        before
    );
    failed.shutdown().await;
}
fn driver_for_failure(fixture: &Fixture) -> Arc<AcpDriver<RecoverableFixture>> {
    driver(fixture)
}

#[tokio::test]
async fn oversized_metadata_protects_live_retirement_and_disconnect_compaction_is_bounded() {
    let fixture = Fixture::new().await;
    let driver = driver(&fixture);
    let info = driver.create_session(None, None, None).await.unwrap();
    driver
        .send_prompt(&info.asid.0, "hello", &[], None)
        .await
        .unwrap();
    idle(&driver, &info.asid.0).await.unwrap();
    let title = "native title ".repeat(MAX_DORMANT_INFO_BYTES);
    let session = driver.retained(&info.asid.0).unwrap();
    let mut state = session.state.lock().await;
    state.info.title = title.clone();
    driver.mirror.update_session(state.info.clone()).await;
    assert!(!dormant_metadata_fits(&state.info));
    drop(state);
    expire(&driver, &info.asid.0).await;
    tokio::task::yield_now().await;
    assert!(
        session.client.is_running(),
        "oversized metadata must be refused before EOF"
    );
    session.client.shutdown().await;
    dormant(&driver, &info.asid.0).await;
    let compact = driver.dormant.lock().unwrap()[&info.asid.0].clone();
    assert!(!compact.complete_metadata);
    assert!(dormant_metadata_fits(&compact.info));
    assert_eq!(
        driver.get_session(&info.asid.0).await.unwrap().title,
        title,
        "display metadata must not be silently truncated"
    );
    assert_eq!(
        compact.info.runtime.unwrap().interaction,
        RuntimeInteraction::Unavailable
    );
    driver.shutdown().await;
}

#[tokio::test]
#[ignore = "requires explicit ACP_IDLE_NATIVE_PROVIDER=fx|qoder|qoder-empty and installed CLI; isolated QA, no approval answers"]
async fn real_native_idle_pid_replacement_and_context_recall() {
    let provider = std::env::var("ACP_IDLE_NATIVE_PROVIDER")
        .expect("explicit own native QA provider required");
    let binary = std::env::var("ACP_IDLE_NATIVE_BINARY").unwrap_or_else(|_| {
        if provider.starts_with("qoder") {
            "qodercli".into()
        } else {
            "fx".into()
        }
    });
    match provider.as_str() {
        "fx" => native_round_trip::<crate::agents::adapters::fx::driver::FxProvider>(binary).await,
        "qoder" => {
            native_round_trip::<crate::agents::adapters::qoder::driver::QoderProvider>(binary).await
        }
        "qoder-empty" => native_empty_round_trip(binary).await,
        _ => panic!("unsupported explicit native QA provider"),
    }
}

async fn native_empty_round_trip(binary: String) {
    use crate::agents::adapters::qoder::driver::QoderProvider;
    let root = dirs::cache_dir()
        .unwrap()
        .join("muqun-native-idle-qa")
        .join(format!("qoder-empty-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let driver = AcpDriver::<QoderProvider>::new(
        binary,
        root.to_string_lossy().into_owned(),
        Arc::new(MemoryMirror::for_agent("qoder")),
        broadcast::channel(256).0,
    );
    driver.set_idle_timeout(Duration::from_secs(1));
    let outcome: Result<Value, String> = async {
        let first = driver.create_session(None, None, None).await.map_err(|error| error.to_string())?;
        let old_pid = driver.retained(&first.asid.0).unwrap().client.process_id().ok_or("missing old PID")?;
        let weak = Arc::downgrade(&driver.retained(&first.asid.0).unwrap());
        tokio::time::timeout(Duration::from_secs(65), async {
            while !driver.dormant.lock().unwrap().get(&first.asid.0).is_some_and(|entry| entry.kind == DormantKind::Parked) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await.map_err(|_| "verified empty Qoder did not retire".to_owned())?;
        if weak.upgrade().is_some() { return Err("empty live projection was retained".into()); }
        let gone = unsafe { libc::kill(old_pid as i32, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if !gone || !driver.get_timeline(&first.asid.0, 100).await.map_err(|error| error.to_string())?.is_empty() {
            return Err("empty PID/history retirement failed".into());
        }
        driver.prepare_selected_session(&first.asid.0).await.map_err(|error| error.to_string())?;
        let resumed = driver.get_session(&first.asid.0).await.map_err(|error| error.to_string())?;
        let new_pid = driver.retained(&first.asid.0).unwrap().client.process_id().ok_or("missing new PID")?;
        if resumed.asid != first.asid || new_pid == old_pid || resumed.model != first.model
            || !driver.get_timeline(&first.asid.0, 100).await.map_err(|error| error.to_string())?.is_empty() {
            return Err("empty same-ID/model/history restoration failed".into());
        }
        Ok(json!({"provider":"qoder","empty":true,"session_id":first.asid.0,"first_pid":old_pid,"second_pid":new_pid,
            "old_pid_gone":gone,"same_id":true,"live_projection_released":true,"model_preserved":true,"model_prompts":0}))
    }.await;
    driver.shutdown().await;
    let report = outcome.expect("isolated empty Qoder acceptance");
    std::fs::write(
        root.join("acceptance.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("native empty idle acceptance: {report}");
}

async fn native_round_trip<P: AcpProvider>(binary: String) {
    let root = dirs::cache_dir()
        .unwrap()
        .join("muqun-native-idle-qa")
        .join(format!("{}-{}", P::KIND, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let driver = AcpDriver::<P>::new(
        binary,
        root.to_string_lossy().into_owned(),
        Arc::new(MemoryMirror::for_agent(P::KIND)),
        broadcast::channel(256).0,
    );
    driver.set_idle_timeout(Duration::from_secs(1));
    let token = format!("ACP_IDLE_RECALL_{}", uuid::Uuid::new_v4().simple());
    let outcome: Result<Value, String> = async {
        let info = driver.create_session(None, None, None).await.map_err(|error| error.to_string())?;
        let first_pid = driver.retained(&info.asid.0).unwrap().client.process_id().ok_or("missing first PID")?;
        driver.send_prompt(&info.asid.0, &format!("Remember this conversation token for later: {token}. Reply with only ACK."), &[], None).await.map_err(|error| error.to_string())?;
        idle(&driver, &info.asid.0).await?;
        let weak = Arc::downgrade(&driver.retained(&info.asid.0).unwrap());
        tokio::time::timeout(Duration::from_secs(65), async {
            loop {
                if driver.dormant.lock().unwrap().get(&info.asid.0).is_some_and(|entry| entry.kind == DormantKind::Parked) { break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await.map_err(|_| "native process did not reach confirmed Parked".to_owned())?;
        if weak.upgrade().is_some() { return Err("retired native projection was retained".into()); }
        // Signal zero only inspects the captured owned PID; it sends no signal.
        let gone = unsafe { libc::kill(first_pid as i32, 0) } != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if !gone { return Err("old owned PID is still present".into()); }
        driver.prepare_observed_session(&info.asid.0).await.map_err(|error| error.to_string())?;
        driver.get_timeline(&info.asid.0, 100).await.map_err(|error| error.to_string())?;
        if driver.retained(&info.asid.0).is_some() { return Err("background observation woke native process".into()); }
        driver.prepare_selected_session(&info.asid.0).await.map_err(|error| error.to_string())?;
        let second_pid = driver.retained(&info.asid.0).unwrap().client.process_id().ok_or("missing second PID")?;
        if first_pid == second_pid || driver.get_session(&info.asid.0).await.map_err(|error| error.to_string())?.asid != info.asid { return Err("native same-ID/new-PID restoration failed".into()); }
        driver.send_prompt(&info.asid.0, "What exact conversation token did I ask you to remember earlier? Reply with only that token.", &[], None).await.map_err(|error| error.to_string())?;
        idle(&driver, &info.asid.0).await?;
        let recalled = driver.retained(&info.asid.0).unwrap().state.lock().await.projection.latest_text(TimelineRole::Assistant).is_some_and(|text| text.contains(&token));
        if !recalled { return Err("native model did not recall the earlier token".into()); }
        Ok(json!({"provider":P::KIND,"session_id":info.asid.0,"first_pid":first_pid,"second_pid":second_pid,"old_pid_gone":gone,"same_id":true,"live_projection_released":true,"background_did_not_wake":true,"context_recall":true,"token_digest":recovery::text_digest(&token)}))
    }.await;
    driver.shutdown().await;
    match outcome {
        Ok(report) => {
            std::fs::write(
                root.join("acceptance.json"),
                serde_json::to_vec_pretty(&report).unwrap(),
            )
            .unwrap();
            println!("native idle acceptance: {}", report);
        }
        Err(error) => panic!("isolated native idle acceptance failed: {error}"),
    }
}
