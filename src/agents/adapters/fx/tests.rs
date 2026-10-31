use serde_json::json;

use super::mapper;
use crate::agents::domain::{AgentPart, PermissionDecision, TimelineRole, ToolCallStatus};

#[test]
fn chunks_preserve_native_identity_roles_and_tool_updates() {
    let mut projection = mapper::Projection::default();
    for update in [
        json!({"sessionUpdate":"user_message_chunk","messageId":"user-z","content":{"type":"text","text":"Question"}}),
        json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Thinking"}}),
        json!({"sessionUpdate":"agent_message_chunk","messageId":"answer-a","content":{"type":"text","text":"One"}}),
        json!({"sessionUpdate":"agent_message_chunk","messageId":"answer-a","content":{"type":"text","text":" two"}}),
        json!({"sessionUpdate":"tool_call","toolCallId":"opaque-tool","name":"shell","title":"Run command","status":"in_progress","rawInput":{"command":"pwd"}}),
        json!({"sessionUpdate":"tool_call_update","toolCallId":"opaque-tool","status":"completed","content":[{"type":"content","content":{"type":"text","text":"/workspace"}}]}),
        json!({"sessionUpdate":"tool_call_update","toolCallId":"opaque-tool","status":"in_progress"}),
    ] {
        projection.update(&update).unwrap();
    }
    let rows = projection.timeline();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].role, TimelineRole::User);
    assert!(matches!(&rows[1].part, AgentPart::Reasoning {text,..} if text == "Thinking"));
    assert!(matches!(&rows[2].part, AgentPart::Text {text} if text == "One two"));
    let AgentPart::Tool(tool) = &rows[3].part else {
        panic!("tool row")
    };
    assert_eq!(tool.status, ToolCallStatus::Completed);
    assert_eq!(tool.output, Some(json!("/workspace")));
    assert_eq!(tool.input, json!({"command":"pwd"}));
}

#[test]
fn identical_stream_chunks_are_stable_and_missing_ids_are_refused() {
    let updates = [
        json!({"sessionUpdate":"user_message_chunk","messageId":"u","content":{"type":"text","text":"Q"}}),
        json!({"sessionUpdate":"agent_message_chunk","messageId":"a","content":{"type":"text","text":"A"}}),
        json!({"sessionUpdate":"agent_message_chunk","messageId":"a","content":{"type":"text","text":"B"}}),
    ];
    let replay = || {
        let mut projection = mapper::Projection::default();
        for update in &updates {
            projection.update(update).unwrap();
        }
        projection.timeline()
    };
    let first = replay();
    let second = replay();
    assert_eq!(
        first
            .iter()
            .map(|row| (&row.id, &row.message_id, &row.part))
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|row| (&row.id, &row.message_id, &row.part))
            .collect::<Vec<_>>()
    );
    assert!(mapper::Projection::default()
        .update(
            &json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"no-id"}})
        )
        .is_err());
}

#[test]
fn permission_uses_offered_opaque_ids_and_session_only_label() {
    let permission = mapper::permission(json!(42), &json!({"sessionId":"s","toolCall":{"toolCallId":"t","name":"shell","title":"Run command","rawInput":{"action":"run","command":"pwd"}},"options":[
        {"optionId":"opaque-1","name":"Allow once","kind":"allow_once"},
        {"optionId":"opaque-2","name":"Allow for this session","kind":"allow_always"},
        {"optionId":"opaque-3","name":"Reject forever","kind":"reject_always"}
    ]})).unwrap();
    assert_eq!(permission.rpc_id, json!(42));
    assert_eq!(
        permission.options,
        vec![
            (PermissionDecision::Allow, "opaque-1".into()),
            (PermissionDecision::AllowAlways, "opaque-2".into())
        ]
    );
    assert_eq!(
        permission.request.options[1].label,
        "Allow for this session"
    );
    assert!(permission.request.save.is_empty());
    assert_eq!(permission.request.source_tool_call_id.as_deref(), Some("t"));
}

#[test]
fn config_options_supply_actual_model_and_context_is_not_usage() {
    let value = json!({"configOptions":[
        {"id":"provider","currentValue":"official-provider"},
        {"id":"model","currentValue":"model-x","options":[{"value":"model-x","name":"Model X"}]},
        {"id":"effort","currentValue":"high"}
    ],"modes":{"currentModeId":"default","availableModes":[{"id":"default","name":"Default"}]},"updatedAt":"2026-10-30T03:00:00.000Z"});
    let info = mapper::session("s", "/workspace", &value);
    let catalog = mapper::catalog(&value);
    assert_eq!(
        info.model.as_ref().unwrap().provider_id,
        "official-provider"
    );
    assert_eq!(
        info.model.as_ref().unwrap().variant.as_deref(),
        Some("high")
    );
    assert_eq!(catalog.models[0].id, "model-x");
    assert!(info.tokens.is_none());
    assert_eq!(info.updated_ms, 1_793_329_200_000);
}

#[test]
fn oversized_history_is_explicitly_unavailable() {
    let mut projection = mapper::Projection::default();
    let update = json!({"sessionUpdate":"agent_message_chunk","messageId":"a","content":{"type":"text","text":"x".repeat(4*1024*1024)}});
    for _ in 0..7 {
        projection.update(&update).unwrap();
    }
    let error = projection.update(&update).unwrap_err();
    assert!(error.to_string().contains("history budget"));
    assert_eq!(projection.timeline().len(), 1);
}

#[cfg(unix)]
pub(crate) mod integration {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::broadcast;

    use crate::agents::adapters::fx::FxDriver;
    use crate::agents::adapters::memory_mirror::MemoryMirror;
    use crate::agents::domain::{AgentSessionStatus, SessionQuery};
    use crate::agents::ports::agent::{AgentError, AgentPort};
    use crate::agents::ports::mirror::SessionMirrorPort;

    pub(crate) struct Fixture {
        pub(crate) directory: PathBuf,
        pub(crate) driver: Arc<FxDriver>,
        mirror: Arc<MemoryMirror>,
        _permit: tokio::sync::OwnedSemaphorePermit,
    }
    impl Fixture {
        pub(crate) async fn new() -> Self {
            static FIXTURES: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
                std::sync::OnceLock::new();
            let permit = FIXTURES
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
                .clone()
                .acquire_owned()
                .await
                .unwrap();
            use std::os::unix::fs::PermissionsExt;
            let directory =
                std::env::temp_dir().join(format!("muqun-fx-driver-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let binary = directory.join("mock-fx");
            std::fs::write(&binary, r##"#!/usr/bin/env python3
import json, os, sys, time
store = 'sessions.json'
def read():
    try:
        with open(store) as f: return json.load(f)
    except FileNotFoundError: return {}
def send(value):
    print(json.dumps(value), flush=True)
def reply(id, result): send({'jsonrpc':'2.0','id':id,'result':result})
def update(sid, value): send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':sid,'update':value}})
def text(sid, mid, content, user=False):
    value={'sessionUpdate':'user_message_chunk' if user else 'agent_message_chunk','content':{'type':'text','text':content}}
    if '--acp' not in sys.argv:value['messageId']=mid
    update(sid,value)
config={'configOptions':[{'id':'provider','currentValue':'mock-provider','options':[{'value':'mock-provider','name':'Mock provider'},{'value':'other-provider','name':'Other provider'}]},{'id':'model','currentValue':'mock-model','options':[{'value':'mock-model','name':'Mock model'},{'value':'other-model','name':'Other model'}]},{'id':'effort','currentValue':'low','options':[{'value':'low','name':'Low'},{'value':'high','name':'High'}]}]}
if '--acp' in sys.argv:
    config['configOptions']=[option for option in config['configOptions'] if option['id']!='provider']
    for option in config['configOptions']:
        if option['id']=='effort':option['id']='reasoning_effort'
active=None
pending=None
turn=0
notices=False
for line in sys.stdin:
    message=json.loads(line)
    method=message.get('method')
    params=message.get('params',{})
    with open('methods.log','a') as f: f.write((method or 'response')+'\n')
    if method=='initialize':
        if os.path.exists('hang-init'):continue
        caps=params.get('clientCapabilities',{})
        with open('init-capabilities.json','w') as f:json.dump(caps,f)
        notices=isinstance(caps.get('session',{}).get('notices'),dict)
        reply(message['id'],{'protocolVersion':1,'agentCapabilities':{'loadSession':True,'promptCapabilities':{'image':True,'embeddedContext':True},'sessionCapabilities':{'list':{},'fork':{},'delete':{}} if '--acp' in sys.argv else {'list':{}}}})
    elif method=='session/list':
        rows=[{'sessionId':sid,'cwd':os.getcwd(),'title':'Mock','updatedAt':'2026-10-30T03:00:00Z'} for sid in read()]
        if os.path.exists('list-delay'):
            with open('list-started','w') as f:f.write('ready')
            time.sleep(0.15)
        if os.path.exists('fail-list'):
            send({'jsonrpc':'2.0','id':message['id'],'error':{'code':-32603,'message':'metadata unavailable'}});continue
        if params.get('cursor') and os.path.exists('fail-list-page'):
            send({'jsonrpc':'2.0','id':message['id'],'error':{'code':-32603,'message':'next page unavailable'}});continue
        reply(message['id'],dict(sessions=rows,**({'nextCursor':'page-2'} if os.path.exists('fail-list-page') else {})))
    elif method=='session/new':
        active='session-'+str(os.getpid())
        data=read();data[active]=[]
        with open(store,'w') as f:json.dump(data,f)
        reply(message['id'],dict(config,sessionId=active))
        if '--acp' in sys.argv:
            time.sleep(0.03)
            update(active,{'sessionUpdate':'available_commands_update','availableCommands':[{'name':'goal','description':'Set goal'}]})
    elif method=='session/load':
        if os.path.exists('busy-load'):
            send({'jsonrpc':'2.0','id':message['id'],'error':{'code':-32603,'message':'Session is busy'}});continue
        active=params['sessionId']
        if active not in read():send({'jsonrpc':'2.0','id':message['id'],'error':{'code':-32602,'message':'not found'}});continue
        for index,entry in enumerate(read()[active]):text(active,'replay-'+str(os.getpid())+'-'+str(index),entry[1],entry[2])
        reply(message['id'],config)
    elif method=='session/fork':
        data=read();source=params['sessionId']
        if not data.get(source):send({'jsonrpc':'2.0','id':message['id'],'error':{'code':500,'message':'No conversation to branch'}});continue
        cloned='fork-'+str(os.getpid());data[cloned]=data[source]
        with open(store,'w') as f:json.dump(data,f)
        reply(message['id'],dict(config,sessionId=cloned))
    elif method=='session/delete':
        if active!=params['sessionId']:
            send({'jsonrpc':'2.0','id':message['id'],'error':{'code':-32602,'message':'Deletion requires a bound native session'}});continue
        data=read();data.pop(params['sessionId'],None)
        with open(store,'w') as f:json.dump(data,f)
        reply(message['id'],{})
    elif method=='session/set_config_option':
        for option in config['configOptions']:
            if option['id']==params['configId']:option['currentValue']=params['value']
        if os.path.exists('normalize-model') and params['configId']=='model':
            for option in config['configOptions']:
                if option['id']=='model':option['currentValue']='canonical-model'
        if os.path.exists('wrong-provider') and params['configId']=='model':
            for option in config['configOptions']:
                if option['id']=='provider':option['currentValue']='other-provider'
        reply(message['id'],config)
    elif method=='session/prompt':
        turn+=1
        content=params['prompt'][0]['text']
        if '--acp' in sys.argv:text(active,'echo-'+str(turn),content,True)
        if content=='native notice':
            diagnostic='[context] synthetic skill catalog diagnostic'
            if notices:update(active,{'sessionUpdate':'notice','severity':'warning','title':diagnostic})
            else:text(active,'notice-'+str(turn),diagnostic)
        if content=='disconnect':sys.exit(0)
        data=read(); data[active].append(['user-'+str(turn),content,True])
        with open(store,'w') as f:json.dump(data,f)
        if content in ('approval','hold'):
            pending=message['id']
            if content=='approval':
                update(active,{'sessionUpdate':'tool_call','toolCallId':'tool-1','name':'shell','title':'Run mock tool','status':'pending','rawInput':{'command':'pwd'}})
                send({'jsonrpc':'2.0','id':'permission-opaque','method':'session/request_permission','params':{'sessionId':active,'toolCall':{'toolCallId':'tool-1','name':'shell','title':'Run mock tool'},'options':[{'optionId':'custom-permit','kind':'allow_once','name':'Allow once'}]}})
        else:
            text(active,'assistant-'+str(turn),'A');text(active,'assistant-'+str(turn),'B')
            data=read();data[active].append(['assistant-'+str(turn),'AB',False])
            with open(store,'w') as f:json.dump(data,f)
            reply(message['id'],{'stopReason':'end_turn','usage':{'inputTokens':17,'outputTokens':7,'cacheReadTokens':5,'cacheWriteTokens':2,'reasoningTokens':3}})
    elif method=='session/cancel':
        if pending is not None:reply(pending,{'stopReason':'cancelled'});pending=None
    elif method is None and message.get('id')=='permission-opaque':
        if message.get('result',{}).get('outcome',{}).get('optionId')!='custom-permit':sys.exit(9)
        update(active,{'sessionUpdate':'tool_call_update','toolCallId':'tool-1','status':'completed','content':[{'type':'content','content':{'type':'text','text':'approved'}}]})
        text(active,'assistant-'+str(turn),'Permission answered')
        reply(pending,{'stopReason':'end_turn'});pending=None
"##).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mirror = Arc::new(MemoryMirror::for_agent("fx"));
            let (tx, _) = broadcast::channel(256);
            let driver = FxDriver::new(
                binary.to_string_lossy().into_owned(),
                directory.to_string_lossy().into_owned(),
                mirror.clone(),
                tx,
            );
            Self {
                directory,
                driver,
                mirror,
                _permit: permit,
            }
        }
        pub(crate) fn qoder_driver(&self) -> Arc<crate::agents::adapters::qoder::QoderDriver> {
            let (tx, _) = broadcast::channel(256);
            let driver = crate::agents::adapters::qoder::QoderDriver::new(
                self.directory
                    .join("mock-fx")
                    .to_string_lossy()
                    .into_owned(),
                self.directory.to_string_lossy().into_owned(),
                Arc::new(MemoryMirror::for_agent("qoder")),
                tx,
            );
            // Never fingerprint the real ~/.qoder store from tests.
            driver.set_session_store(|| None);
            driver
        }
        pub(crate) fn methods(&self) -> Vec<String> {
            std::fs::read_to_string(self.directory.join("methods.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
        async fn status(&self, id: &str, expected: AgentSessionStatus) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if self.driver.get_session(id).await.unwrap().status == expected {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("status transition");
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.driver.stop();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[tokio::test]
    async fn native_notices_are_negotiated_without_reclassifying_fallback_text() {
        let fixture = Fixture::new().await;
        let info = fixture
            .driver
            .create_session(None, None, None)
            .await
            .unwrap();
        let capabilities: Value = serde_json::from_slice(
            &std::fs::read(fixture.directory.join("init-capabilities.json")).unwrap(),
        )
        .unwrap();
        assert!(capabilities
            .pointer("/session/notices")
            .is_some_and(Value::is_object));
        assert_eq!(capabilities["terminal"], false);
        assert_eq!(capabilities["fs"]["readTextFile"], false);
        fixture
            .driver
            .send_prompt(&info.asid.0, "native notice", &[], None)
            .await
            .unwrap();
        fixture.status(&info.asid.0, AgentSessionStatus::Idle).await;
        let rows = fixture
            .driver
            .get_timeline(&info.asid.0, 100)
            .await
            .unwrap();
        assert!(rows.iter().any(|row| row.role == TimelineRole::System
            && matches!(&row.part,AgentPart::System{text,..} if text.contains("synthetic skill catalog diagnostic"))));
        assert!(rows.iter().any(|row| row.role == TimelineRole::Assistant
            && matches!(&row.part,AgentPart::Text{text} if text == "AB")));

        // Older/native providers may still emit diagnostics as ordinary text.
        // Without exact notice metadata the Gateway must retain it unchanged.
        let legacy = fixture.qoder_driver();
        let info = legacy.create_session(None, None, None).await.unwrap();
        let capabilities: Value = serde_json::from_slice(
            &std::fs::read(fixture.directory.join("init-capabilities.json")).unwrap(),
        )
        .unwrap();
        assert!(capabilities.pointer("/session/notices").is_none());
        legacy
            .send_prompt(&info.asid.0, "native notice", &[], None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while legacy.get_session(&info.asid.0).await.unwrap().status != AgentSessionStatus::Idle
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let rows = legacy.get_timeline(&info.asid.0, 100).await.unwrap();
        assert!(!rows.iter().any(|row| row.role == TimelineRole::System));
        assert!(rows
            .iter()
            .any(|row| matches!(&row.part,AgentPart::Text{text}
            if text == "[context] synthetic skill catalog diagnosticAB")));
        legacy.shutdown().await;
        fixture.driver.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_cancels_initializing_private_children() {
        let fixture = Fixture::new().await;
        std::fs::write(fixture.directory.join("hang-init"), b"hang").unwrap();
        let driver = fixture.driver.clone();
        let create = tokio::spawn(async move { driver.create_session(None, None, None).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if fixture.methods().contains(&"initialize".into()) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), fixture.driver.shutdown())
            .await
            .unwrap();
        assert!(create.await.unwrap().is_err());
        assert!(!fixture.driver.is_connected());
        assert!(!fixture.driver.has_live_stream());
        assert!(!fixture.methods().contains(&"session/new".into()));
    }

    #[tokio::test]
    async fn prompt_stream_follow_up_and_cancel_use_existing_domain_contract() {
        let fixture = Fixture::new().await;
        let session = fixture
            .driver
            .create_session(None, None, None)
            .await
            .unwrap();
        let id = &session.asid.0;
        assert_eq!(session.model.as_ref().unwrap().model_id, "mock-model");
        tokio::time::timeout(
            Duration::from_secs(1),
            fixture.driver.send_prompt(id, "first", &[], None),
        )
        .await
        .unwrap()
        .unwrap();
        fixture.status(id, AgentSessionStatus::Idle).await;
        let rows = fixture.driver.get_timeline(id, 100).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[1].part,AgentPart::Text{text} if text=="AB"));
        fixture
            .driver
            .send_prompt(id, "follow-up", &[], None)
            .await
            .unwrap();
        fixture.status(id, AgentSessionStatus::Idle).await;
        assert_eq!(fixture.driver.get_timeline(id, 100).await.unwrap().len(), 4);
        fixture
            .driver
            .send_prompt(id, "hold", &[], None)
            .await
            .unwrap();
        assert!(matches!(
            fixture.driver.send_prompt(id, "duplicate", &[], None).await,
            Err(AgentError::NotAvailable(_))
        ));
        fixture.driver.interrupt(id).await.unwrap();
        fixture.status(id, AgentSessionStatus::Interrupted).await;
        let snapshot = fixture.mirror.get_snapshot(&session.asid).await.unwrap();
        assert_eq!(snapshot.info.agent_id, "fx");
        assert_eq!(snapshot.info.status, AgentSessionStatus::Interrupted);
        assert!(snapshot.timeline.iter().all(|row| row.seq > 0));
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|method| *method == "session/prompt")
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn usage_replies_publish_verified_totals_and_full_history_pages_from_cache() {
        use crate::agents::use_cases::SessionService;
        let fixture = Fixture::new().await;
        let info = fixture
            .driver
            .create_session(None, None, None)
            .await
            .unwrap();
        for text in ["first", "second"] {
            fixture
                .driver
                .send_prompt(&info.asid.0, text, &[], None)
                .await
                .unwrap();
            fixture.status(&info.asid.0, AgentSessionStatus::Idle).await;
        }
        let tokens = fixture
            .driver
            .get_session(&info.asid.0)
            .await
            .unwrap()
            .tokens
            .unwrap();
        assert_eq!(
            (
                tokens.input,
                tokens.output,
                tokens.cache_read,
                tokens.reasoning
            ),
            (20, 8, Some(10), Some(6))
        );
        fixture.driver.shutdown().await;
        let history: Vec<_> = (0..150)
            .flat_map(|index| {
                [
                    json!([format!("u{index}"), format!("question {index}"), true]),
                    json!([format!("a{index}"), format!("answer {index}"), false]),
                ]
            })
            .collect();
        std::fs::write(
            fixture.directory.join("sessions.json"),
            serde_json::to_vec(&json!({info.asid.0.clone():history})).unwrap(),
        )
        .unwrap();
        let mirror = Arc::new(MemoryMirror::for_agent("fx"));
        let (tx, _) = broadcast::channel(256);
        let driver = FxDriver::new(
            fixture
                .directory
                .join("mock-fx")
                .to_string_lossy()
                .into_owned(),
            fixture.directory.to_string_lossy().into_owned(),
            mirror.clone(),
            tx,
        );
        let service = SessionService::with_memory_mirror(driver.clone(), mirror);
        service.prepare_session(&info.asid).await.unwrap();
        let opening = service
            .history_window(&info.asid, None, false)
            .await
            .unwrap();
        assert_eq!(opening.snapshot.timeline.len(), 100);
        assert!(opening.snapshot.info.tokens.is_none());
        let mut ids: std::collections::HashSet<_> = opening
            .snapshot
            .timeline
            .iter()
            .map(|row| row.id.clone())
            .collect();
        let mut cursor = opening.history_cursor.unwrap();
        loop {
            let page = service.history_page(&info.asid, &cursor).await.unwrap();
            for row in page.items {
                assert!(ids.insert(row.id));
            }
            if page.exhausted {
                break;
            }
            cursor = page.history_cursor.unwrap();
        }
        assert_eq!(ids.len(), 300);
        assert!(driver.load_earlier_timeline("not-owned", 80).await.is_err());
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn permissions_wait_for_explicit_supported_user_choice() {
        let fixture = Fixture::new().await;
        let session = fixture
            .driver
            .create_session(None, None, None)
            .await
            .unwrap();
        let id = &session.asid.0;
        fixture
            .driver
            .send_prompt(id, "approval", &[], None)
            .await
            .unwrap();
        let pending = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let pending = fixture.driver.get_pending_permissions(id).await.unwrap();
                if !pending.is_empty() {
                    break pending;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            fixture.driver.get_session(id).await.unwrap().status,
            AgentSessionStatus::Busy
        );
        assert!(matches!(
            fixture
                .driver
                .reply_permission(id, &pending[0].id, PermissionDecision::Deny, None)
                .await,
            Err(AgentError::Unsupported(_))
        ));
        assert!(!fixture.methods().contains(&"response".into()));
        fixture
            .driver
            .reply_permission(id, &pending[0].id, PermissionDecision::Allow, None)
            .await
            .unwrap();
        fixture.status(id, AgentSessionStatus::Idle).await;
        assert!(fixture
            .driver
            .get_pending_permissions(id)
            .await
            .unwrap()
            .is_empty());
        assert!(fixture
            .driver
            .reply_permission(id, &pending[0].id, PermissionDecision::Allow, None)
            .await
            .is_err());
        let rows = fixture.driver.get_timeline(id, 100).await.unwrap();
        assert!(rows.iter().any(|row| matches!(&row.part,AgentPart::Tool(tool) if tool.status==ToolCallStatus::Completed)));
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|method| *method == "response")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn model_and_effort_selection_use_native_options_and_reject_busy_changes() {
        use crate::agents::domain::ModelRef;
        let fixture = Fixture::new().await;
        assert!(fixture.driver.observed_catalog().await.models.is_empty());
        let requested = ModelRef {
            provider_id: "other-provider".into(),
            model_id: "other-model".into(),
            variant: Some("high".into()),
        };
        let session = fixture
            .driver
            .create_session(None, Some(&requested), None)
            .await
            .unwrap();
        assert_eq!(session.model.as_ref(), Some(&requested));
        let id = &session.asid.0;
        let catalog = fixture.driver.observed_catalog().await;
        assert_eq!(catalog.models.len(), 2);
        let model = catalog
            .models
            .iter()
            .find(|model| model.id == "other-model")
            .unwrap();
        assert_eq!(model.provider_id, "other-provider");
        assert_eq!(model.variants.as_ref().unwrap().len(), 2);
        assert!(catalog.modes.is_empty());
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|method| *method == "session/set_config_option")
                .count(),
            3
        );
        fixture
            .driver
            .send_prompt(id, "hold", &[], None)
            .await
            .unwrap();
        let original = ModelRef {
            provider_id: "mock-provider".into(),
            model_id: "mock-model".into(),
            variant: Some("low".into()),
        };
        assert!(matches!(
            fixture.driver.switch_model(id, &original).await,
            Err(AgentError::NotAvailable(_))
        ));
        fixture.driver.interrupt(id).await.unwrap();
        fixture.status(id, AgentSessionStatus::Interrupted).await;
        fixture.driver.switch_model(id, &original).await.unwrap();
        assert_eq!(
            fixture.driver.get_session(id).await.unwrap().model.as_ref(),
            Some(&original)
        );
        let not_offered = ModelRef {
            model_id: "invented-model".into(),
            ..original.clone()
        };
        assert!(matches!(
            fixture.driver.switch_model(id, &not_offered).await,
            Err(AgentError::InvalidRequest(_))
        ));
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|method| *method == "session/set_config_option")
                .count(),
            6
        );
        assert!(matches!(
            fixture.driver.switch_mode(id, "permissive").await,
            Err(AgentError::Unsupported(_))
        ));
        let within_provider = ModelRef {
            provider_id: "mock-provider".into(),
            ..requested.clone()
        };
        fixture
            .driver
            .create_session(None, Some(&within_provider), None)
            .await
            .unwrap();
        let catalog = fixture.driver.observed_catalog().await;
        assert!(catalog
            .models
            .iter()
            .filter(|model| model.provider_id == "mock-provider")
            .any(|model| model.id == "mock-model"
                && model
                    .variants
                    .as_ref()
                    .is_some_and(|variants| variants.len() == 2)));
        assert!(catalog.models.iter().any(|model| model.id == "other-model"
            && model
                .variants
                .as_ref()
                .is_some_and(|variants| variants.len() == 2)));
    }

    #[tokio::test]
    async fn selection_retains_authoritative_native_config_and_refuses_silent_provider_changes() {
        use crate::agents::domain::ModelRef;
        let fixture = Fixture::new().await;
        let session = fixture
            .driver
            .create_session(None, None, None)
            .await
            .unwrap();
        let requested = ModelRef {
            provider_id: "mock-provider".into(),
            model_id: "other-model".into(),
            variant: None,
        };
        std::fs::write(fixture.directory.join("normalize-model"), b"normalize").unwrap();
        assert!(matches!(
            fixture
                .driver
                .switch_model(&session.asid.0, &requested)
                .await,
            Err(AgentError::Protocol(_))
        ));
        assert_eq!(
            fixture
                .driver
                .get_session(&session.asid.0)
                .await
                .unwrap()
                .model
                .unwrap()
                .model_id,
            "canonical-model"
        );
        std::fs::remove_file(fixture.directory.join("normalize-model")).unwrap();
        std::fs::write(fixture.directory.join("wrong-provider"), b"wrong").unwrap();
        let error = fixture
            .driver
            .switch_model(&session.asid.0, &requested)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed the provider"));
        assert_eq!(
            fixture
                .driver
                .get_session(&session.asid.0)
                .await
                .unwrap()
                .model
                .unwrap()
                .provider_id,
            "other-provider"
        );
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|method| *method == "session/set_config_option")
                .count(),
            2
        );
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|method| *method == "session/new")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn create_with_unavailable_model_reports_one_created_session_without_retry() {
        use crate::agents::domain::ModelRef;
        let fixture = Fixture::new().await;
        let requested = ModelRef {
            provider_id: "mock-provider".into(),
            model_id: "not-offered".into(),
            variant: None,
        };
        let error = fixture
            .driver
            .create_session(None, Some(&requested), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("was created"), "{error}");
        assert!(error.to_string().contains("do not retry creation"));
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|method| *method == "session/new")
                .count(),
            1
        );
        assert_eq!(
            fixture
                .driver
                .list_sessions(&SessionQuery::default())
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn native_history_load_is_explicit_metadata_listing_never_loads() {
        let fixture = Fixture::new().await;
        let session = fixture
            .driver
            .create_session(None, None, None)
            .await
            .unwrap();
        let id = &session.asid.0;
        fixture
            .driver
            .send_prompt(id, "first", &[], None)
            .await
            .unwrap();
        fixture.status(id, AgentSessionStatus::Idle).await;
        fixture.driver.shutdown().await;
        let second_mirror = Arc::new(MemoryMirror::for_agent("fx"));
        let (tx, _) = broadcast::channel(32);
        let second = FxDriver::new(
            fixture
                .directory
                .join("mock-fx")
                .to_string_lossy()
                .into_owned(),
            fixture.directory.to_string_lossy().into_owned(),
            second_mirror,
            tx,
        );
        assert_eq!(
            second
                .list_sessions(&SessionQuery::default())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            second.get_session(id).await.unwrap().status,
            AgentSessionStatus::Unknown
        );
        assert!(!fixture.methods().contains(&"session/load".into()));
        assert!(matches!(
            second.get_session("foreign-id").await,
            Err(AgentError::SessionNotFound(_))
        ));
        assert!(second.get_timeline(id, 100).await.is_err());
        assert!(!fixture.methods().contains(&"session/load".into()));
        second.prepare_selected_session(id).await.unwrap();
        assert_eq!(second.get_timeline(id, 100).await.unwrap().len(), 2);
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|method| *method == "session/load")
                .count(),
            1
        );
        assert_eq!(second.get_timeline(id, 100).await.unwrap().len(), 2);
        second.stop();
    }

    #[tokio::test]
    async fn explicit_reload_replaces_random_history_ids_and_failed_load_preserves_snapshot() {
        let fixture = Fixture::new().await;
        let session = fixture
            .driver
            .create_session(None, None, None)
            .await
            .unwrap();
        let id = &session.asid.0;
        fixture
            .driver
            .send_prompt(id, "first", &[], None)
            .await
            .unwrap();
        fixture.status(id, AgentSessionStatus::Idle).await;
        let original = fixture.mirror.get_snapshot(&session.asid).await.unwrap();
        assert_eq!(
            original.timeline.len(),
            2,
            "live FX omits user echo, so flushed input needs one local row"
        );
        assert_eq!(original.timeline[0].role, TimelineRole::User);
        fixture
            .driver
            .send_prompt(id, "disconnect", &[], None)
            .await
            .unwrap();
        fixture.status(id, AgentSessionStatus::Unknown).await;
        // Wait for the old generation's final runtime observation, then prove
        // a rejected explicit load cannot change that settled snapshot.
        let before_failed_load = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = fixture.mirror.get_snapshot(&session.asid).await.unwrap();
                if snapshot.info.runtime.as_ref().is_some_and(|runtime| {
                    runtime.interaction == crate::agents::domain::RuntimeInteraction::Unavailable
                }) {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        std::fs::write(fixture.directory.join("busy-load"), b"busy").unwrap();
        let error = fixture
            .driver
            .prepare_selected_session(id)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Session is busy"));
        let after_failed_load = fixture.mirror.get_snapshot(&session.asid).await.unwrap();
        assert_eq!(before_failed_load.seq, after_failed_load.seq);
        assert_eq!(before_failed_load.timeline, after_failed_load.timeline);
        std::fs::remove_file(fixture.directory.join("busy-load")).unwrap();
        fixture.driver.prepare_selected_session(id).await.unwrap();
        let reloaded = fixture.mirror.get_snapshot(&session.asid).await.unwrap();
        assert_eq!(
            reloaded.timeline.len(),
            2,
            "a full native replay replaces prior random ids and provisional input"
        );
        assert_ne!(original.timeline[0].id, reloaded.timeline[0].id);
        assert!(
            reloaded.timeline.iter().all(|row| row.seq == 0),
            "loaded history must not become live delta output"
        );
        assert!(reloaded.seq > before_failed_load.seq);
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|method| *method == "session/prompt")
                .count(),
            2,
            "reload never resubmits the ambiguous prompt"
        );
        fixture
            .driver
            .send_prompt(id, "disconnect", &[], None)
            .await
            .unwrap();
        fixture.status(id, AgentSessionStatus::Unknown).await;
        fixture.driver.prepare_selected_session(id).await.unwrap();
        let again = fixture.mirror.get_snapshot(&session.asid).await.unwrap();
        assert_eq!(again.timeline.len(), 2);
        assert_ne!(
            reloaded.timeline[0].id, again.timeline[0].id,
            "fixture generates new ids for every load"
        );
    }

    #[tokio::test]
    async fn disconnection_does_not_retry_or_report_completion() {
        let fixture = Fixture::new().await;
        let session = fixture
            .driver
            .create_session(None, None, None)
            .await
            .unwrap();
        let id = &session.asid.0;
        fixture
            .driver
            .send_prompt(id, "disconnect", &[], None)
            .await
            .unwrap();
        fixture.status(id, AgentSessionStatus::Unknown).await;
        assert!(fixture
            .driver
            .send_prompt(id, "never resend", &[], None)
            .await
            .is_err());
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|method| *method == "session/prompt")
                .count(),
            1
        );
        assert!(fixture
            .driver
            .get_session(id)
            .await
            .unwrap()
            .outcome
            .is_none());
    }
}
