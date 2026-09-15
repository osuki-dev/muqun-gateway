//! OpenAPI descriptions for the implemented durable-work wire contract.
//! Kept beside the HTTP/domain boundary rather than extending main's inline schemas.
use serde_json::{json, Map, Value};

fn reference(name: &str) -> Value {
    json!({"$ref": format!("#/components/schemas/{name}")})
}
fn text(max_bytes: usize) -> Value {
    json!({"type":"string","minLength":1,"maxLength":max_bytes,"pattern":"^[^\\u0000]+$","x-maxUtf8Bytes":max_bytes,"description":"Nonblank, NUL-free UTF-8. The limit is measured in encoded bytes, not Unicode characters."})
}
fn nullable(schema: Value) -> Value {
    json!({"anyOf":[schema,{"type":"null"}]})
}
fn uint() -> Value {
    json!({"type":"integer","minimum":0,"maximum":u64::MAX})
}
fn array(item: Value) -> Value {
    json!({"type":"array","items":item})
}
fn object(properties: Value, optional: &[&str], strict: bool) -> Value {
    let required: Vec<_> = properties
        .as_object()
        .unwrap()
        .keys()
        .filter(|key| !optional.contains(&key.as_str()))
        .cloned()
        .collect();
    let mut schema = json!({"type":"object","properties":properties,"required":required});
    if strict {
        schema["additionalProperties"] = json!(false);
    }
    schema
}
fn mutation(name: &str) -> Value {
    object(
        json!({"value":reference(name),"replayed":{"type":"boolean"}}),
        &[],
        false,
    )
}
fn revision_request(properties: Value, optional: &[&str]) -> Value {
    let mut properties = properties;
    properties["request_key"] = text(128);
    properties["expected_revision"] = uint();
    object(properties, optional, true)
}
fn schemas() -> Value {
    let role = json!({"type":"string","enum":["lead","worker"]});
    let policy = object(
        json!({"allowed_agents":{"type":"array","minItems":1,"maxItems":32,"items":text(64)},"max_workers":{"type":"integer","minimum":0,"maximum":16,"description":"Maximum managed workers; zero allows only the lead."}}),
        &[],
        true,
    );
    let mut task_properties = json!({"repo_path":text(4096),"title":text(240),"brief":text(65536),"parent_task_id":nullable(text(256)),"policy":reference("WorkPolicy")});
    task_properties["repo_path"]["description"] = json!(
        "Absolute repository path, canonicalized within the selected session's allowed repository roots; max 4096 UTF-8 bytes. A parent task must share this session and repository."
    );
    let mut create_properties = task_properties.clone();
    create_properties["request_key"] = text(128);
    task_properties["id"] = text(256);
    task_properties["session_id"] = text(256);
    task_properties["revision"] = uint();
    task_properties["paused"] = json!({"type":"boolean"});
    for key in ["created_at_ms", "updated_at_ms"] {
        task_properties[key] =
            json!({"type":"integer","format":"int64","description":"Unix time in milliseconds"});
    }
    let mut binding = Map::new();
    for key in [
        "workspace_id",
        "tab_id",
        "instance_id",
        "target",
        "pane_id",
        "worktree_path",
    ] {
        binding.insert(key.into(), nullable(text(4096)));
    }
    let mut attempt = Value::Object(binding.clone());
    for (key, value) in [
        ("id", text(256)),
        ("task_id", text(256)),
        ("agent_kind", text(64)),
        ("role", role.clone()),
        ("created_at_ms", json!({"type":"integer","format":"int64"})),
    ] {
        attempt[key] = value;
    }
    let failure = json!({"type":"string","enum":["invalid_input","not_found","scope_mismatch","revision_conflict","request_key_conflict","capability_unavailable","instance_changed","not_ready","approval_required","delivery_unconfirmed","artifact_changed","artifact_missing","resource_limit","storage_unavailable"]});
    let operation = object(
        json!({"bootstrap_version":nullable(text(128)),"id":text(256),"task_id":text(256),"attempt_id":nullable(text(256)),"kind":{"type":"string","enum":["create_task","start_attempt","deliver_prompt","submit_result","review_result","pause_task"]},"state":{"type":"string","enum":["prepared","submitting","acknowledged","refused","unconfirmed"]},"resources":reference("WorkBinding"),"failure_code":nullable(reference("WorkFailureCode")),"created_at_ms":{"type":"integer","format":"int64"},"updated_at_ms":{"type":"integer","format":"int64"}}),
        &["bootstrap_version"],
        false,
    );
    let artifact = object(
        json!({"path":text(4096),"sha256":{"type":"string","pattern":"^[0-9a-f]{64}$","description":"Lowercase SHA-256 of captured bytes"},"size_bytes":{"type":"integer","minimum":0,"maximum":crate::work_artifacts::MAX_ARTIFACT_BYTES}}),
        &[],
        true,
    );
    let result_properties = json!({"attempt_id":text(256),"summary":text(16384),"artifacts":{"type":"array","maxItems":32,"items":reference("WorkArtifact"),"description":"Regular files inside the task repository, no symlink traversal. Total declared size at most 100 MiB. Sources are captured and digest-verified before submission commits."},"evidence":{"type":"array","maxItems":32,"items":text(4096)}});
    let mut result = result_properties.clone();
    result["id"] = text(256);
    result["task_id"] = text(256);
    result["created_at_ms"] = json!({"type":"integer","format":"int64"});
    let review_properties = json!({"submission_id":text(256),"decision":{"type":"string","enum":["accepted","changes_requested"]},"message":nullable(text(16384))});
    let mut review = review_properties.clone();
    review["id"] = text(256);
    review["task_id"] = text(256);
    review["actor_id"] = text(256);
    review["created_at_ms"] = json!({"type":"integer","format":"int64"});
    let mut start = revision_request(
        json!({"agent_kind":text(64),"role":role,"branch_name":nullable(json!({"type":"string","minLength":1,"maxLength":200,"pattern":"^[A-Za-z0-9._/-]+$","description":"Dedicated worktree branch. No leading dash, .., empty, dot-leading or dot-ending segments, or .lock suffix. Required and non-null for worker attempts; optional for the lead."}))}),
        &["branch_name"],
    );
    start["allOf"] = json!([{"if":{"properties":{"role":{"const":"worker"}},"required":["role"]},"then":{"required":["branch_name"],"properties":{"branch_name":{"type":"string"}}}}]);
    let mut definitions = json!({
        "WorkPolicy":policy,
        "WorkCreateRequest":object(create_properties,&["parent_task_id"],true),
        "WorkTask":object(task_properties,&[],false),
        "WorkBinding":object(Value::Object(binding),&[],false),
        "WorkAttempt":object(attempt,&[],false),
        "WorkFailureCode":failure,
        "WorkOperation":operation,
        "WorkArtifact":artifact,
        "WorkResult":object(result,&[],false),
        "WorkSubmitResultRequest":revision_request(result_properties,&[]),
        "WorkReview":object(review,&[],false),
        "WorkReviewRequest":revision_request(review_properties,&["message"]),
        "WorkPauseRequest":revision_request(json!({"paused":{"type":"boolean"}}),&[]),
        "WorkStartRequest":start,
        "WorkDeliveryRequest":revision_request(json!({"expected_instance_id":text(512),"text":text(65536)}),&[]),
        "WorkTaskMutation":mutation("WorkTask"),
        "WorkOperationMutation":mutation("WorkOperation"),
        "WorkResultMutation":mutation("WorkResult"),
        "WorkReviewMutation":mutation("WorkReview"),
        "WorkRequestReceipt":{"oneOf":[
            object(json!({"kind":{"enum":["create_task","pause_task","configure_delegation","set_dependencies"]},"value":reference("WorkTask")}),&[],false),
            object(json!({"kind":{"enum":["start_attempt","deliver_prompt","interrupt_attempt"]},"value":reference("WorkOperation")}),&[],false),
            object(json!({"kind":{"const":"submit_result"},"value":reference("WorkResult")}),&[],false),
            object(json!({"kind":{"const":"review_result"},"value":reference("WorkReview")}),&[],false)
        ]},
        "WorkDetail":object(json!({"task":reference("WorkTask"),"attempts":array(reference("WorkAttempt")),"operations":array(reference("WorkOperation")),"results":array(reference("WorkResult")),"reviews":array(reference("WorkReview")),"cursor":uint()}),&[],false),
        "WorkTaskSummary":object(json!({
            "task_id":text(256),"session_id":text(256),"parent_task_id":nullable(text(256)),"task_revision":json!({"type":"integer","minimum":0,"maximum":9007199254740991u64}),
            "title":text(240),"repo_path":text(4096),"paused":{"type":"boolean"},
            "last_activity":nullable(object(json!({"cursor":json!({"type":"integer","minimum":0,"maximum":9007199254740991u64}),"kind":text(256),"entity_id":text(256)}),&[],false)),
            "reserved_attempts":{"type":"integer","minimum":0,"maximum":1024},
            "unresolved_native_operations":{"type":"integer","minimum":0,"maximum":1024},
            "unreviewed_results":{"type":"integer","minimum":0,"maximum":1024},
            "latest_result":nullable(object(json!({"submission_id":text(256),"review":nullable(object(json!({"review_id":text(256),"decision":{"type":"string","enum":["accepted","changes_requested"]}}),&[],false))}),&[],false))
        }),&[],false),
        "WorkTaskSummaryPage":object(json!({"items":{"type":"array","maxItems":20,"items":reference("WorkTaskSummary")},"snapshot_cursor":json!({"type":"integer","minimum":0,"maximum":9007199254740991u64}),"next_after_id":nullable(text(256))}),&[],false),
        "WorkTaskPage":object(json!({"tasks":array(reference("WorkTask")),"next_after_id":nullable(text(256))}),&[],false),
        "WorkChange":object(json!({"cursor":uint(),"task_id":text(256),"revision":uint(),"kind":text(256),"entity_id":text(256)}),&[],false),
        "WorkChangePage":object(json!({"changes":array(reference("WorkChange")),"cursor":uint(),"reset_required":{"type":"boolean","description":"Discard the expired/future cursor and recover by loading a task snapshot. No command is replayed."}}),&[],false),
        "WorkError":object(json!({"error":object(json!({"code":{"type":"string","description":"Stable domain or authentication/session error code. Domain values are listed in WorkFailureCode."},"message":{"type":"string"}}),&[],false)}),&[],false)
    });
    definitions["WorkPage"] = object(
        json!({"snapshot_revision":uint(),"after_id":nullable(text(256)),"next_after_id":nullable(text(256)),"has_more":{"type":"boolean"}}),
        &[],
        false,
    );
    definitions["WorkDetailPages"] = object(
        json!({"attempts":reference("WorkPage"),"operations":reference("WorkPage"),"results":reference("WorkPage"),"reviews":reference("WorkPage")}),
        &[],
        false,
    );
    definitions["WorkDetail"]["properties"]["pages"] = reference("WorkDetailPages");
    definitions["WorkDetail"]["required"]
        .as_array_mut()
        .unwrap()
        .push(json!("pages"));
    definitions["WorkRecordPage"] = object(
        json!({"items":{"type":"array","maxItems":20,"items":{"oneOf":[reference("WorkAttempt"),reference("WorkOperation"),reference("WorkResult"),reference("WorkReview")]}},"page":reference("WorkPage")}),
        &[],
        false,
    );
    let evidence = json!({"oneOf":[
        object(json!({"kind":{"const":"gateway_dispatch_fence"},"start_operation_id":text(256)}),&[],false),
        object(json!({"kind":{"const":"native_start_refusal"},"start_operation_id":text(256),"native_owner_epoch":text(256),"native_receipt_id":text(256)}),&[],false),
        object(json!({"kind":{"const":"native_exit_tombstone"},"instance_id":text(256),"native_owner_epoch":text(256),"native_receipt_id":text(256)}),&[],false)
    ]});
    definitions["WorkAttemptRelease"] = object(
        json!({"reason":{"enum":["startup_not_dispatched","startup_refused_without_process","owned_process_exited"]},"evidence":evidence,"reconciliation_operation_id":text(256),"released_at_ms":{"type":"integer"}}),
        &[],
        false,
    );
    definitions["WorkAttemptLifecycle"] = object(
        json!({"launch_phase":{"enum":["not_dispatched","dispatch_claimed","launch_confirmed","legacy_unknown"]},"reservation":{"enum":["reserved","released"]},"native_owner_epoch":nullable(text(256)),"release":nullable(reference("WorkAttemptRelease"))}),
        &[],
        false,
    );
    definitions["WorkAttempt"]["properties"]["lifecycle"] = reference("WorkAttemptLifecycle");
    definitions["WorkOperation"]["properties"]["kind"]["enum"]
        .as_array_mut()
        .unwrap()
        .push(json!("reconcile_attempt"));
    definitions["WorkReconcileRequest"] = revision_request(
        json!({"expected_instance_id":nullable(text(256)),"expected_native_owner_epoch":nullable(text(256))}),
        &[],
    );
    definitions["WorkReconciliation"] = object(
        json!({"operation_id":text(256),"attempt_id":text(256),"observation":{"enum":["not_started","live","exited","unknown","already_released"]},"reservation":{"enum":["reserved","released"]},"release":nullable(reference("WorkAttemptRelease")),"task_revision":uint()}),
        &[],
        false,
    );
    definitions["WorkReconciliationMutation"] = mutation("WorkReconciliation");
    definitions["WorkRequestReceipt"]["oneOf"].as_array_mut().unwrap().push(object(json!({"kind":{"const":"reconcile_attempt"},"value":{"oneOf":[
        object(json!({"receipt_type":{"const":"operation"},"operation":reference("WorkOperation")}),&[],false),
        object(json!({"receipt_type":{"const":"reconciliation"},"receipt":reference("WorkReconciliation")}),&[],false)
    ]}}),&[],false));
    let input_use =
        json!({"type":"string","enum":["reference-only","may-include"],"default":"reference-only"});
    let caption = json!({"type":"string","maxLength":4096,"x-maxUtf8Bytes":4096,"pattern":"^[^\\u0000]*$","default":""});
    let input_ref = object(
        json!({"input_id":text(256),"caption":caption,"use":input_use}),
        &["caption", "use"],
        true,
    );
    let mut frozen = input_ref.clone();
    frozen["additionalProperties"] = json!(true);
    frozen["properties"]["name"] = text(480);
    frozen["properties"]["name"]["maxLength"] = json!(120);
    frozen["properties"]["mime"] = text(128);
    frozen["properties"]["sha256"] = json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
    frozen["properties"]["size_bytes"] =
        json!({"type":"integer","minimum":0,"maximum":10*1024*1024});
    frozen["required"] = json!([
        "input_id",
        "caption",
        "use",
        "name",
        "mime",
        "sha256",
        "size_bytes"
    ]);
    definitions["WorkInputRef"] = input_ref;
    definitions["WorkFrozenInputRef"] = frozen;
    definitions["WorkInputReceipt"] = object(
        json!({"input_id":text(256),"session_id":text(256),"repo_path":text(4096),"name":text(480),"mime":text(256),"size_bytes":{"type":"integer","minimum":0,"maximum":10*1024*1024},"sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"},"created_at_ms":{"type":"integer"},"expires_at_ms":{"type":"integer"}}),
        &[],
        false,
    );
    definitions["WorkInputReceipt"]["properties"]["name"]["maxLength"] = json!(120);
    definitions["WorkInputReceipt"]["properties"]["mime"] = text(128);
    for name in ["WorkCreateRequest", "WorkDeliveryRequest"] {
        definitions[name]["properties"]["input_refs"] = json!({"type":"array","maxItems":9,"items":reference("WorkInputRef"),"default":[],"description":"Ordered explicit references. Duplicate IDs are refused. No implicit historical references are appended."});
    }
    for name in ["WorkTask", "WorkOperation"] {
        definitions[name]["properties"]["input_refs"] =
            json!({"type":"array","maxItems":9,"items":reference("WorkFrozenInputRef")});
    }
    definitions["WorkFailureCode"]["enum"]
        .as_array_mut()
        .unwrap()
        .push(json!("input_expired"));
    definitions["WorkOperation"]["properties"]["kind"]["enum"]
        .as_array_mut()
        .unwrap()
        .extend([json!("configure_delegation"), json!("set_dependencies")]);
    definitions["WorkDelegationPolicy"] = object(
        json!({"enabled":{"type":"boolean","default":false},"max_children":{"type":"integer","minimum":0,"maximum":64,"default":16},"max_depth":{"type":"integer","minimum":0,"maximum":1,"default":1,"description":"This increment permits direct children only. Zero forbids creating child tasks."},"dependency_requirement":{"type":"string","enum":["result_available","human_accepted"],"default":"result_available"}}),
        &[],
        false,
    );
    definitions["WorkDelegationState"] = object(
        json!({"policy":reference("WorkDelegationPolicy"),"coordinator_attempt_id":nullable(text(256)),"coordinator_epoch":uint()}),
        &[],
        false,
    );
    definitions["WorkDelegationConfigRequest"] = object(
        json!({"request_key":text(128),"expected_revision":uint(),"input":object(json!({"policy":reference("WorkDelegationPolicy"),"coordinator_attempt_id":nullable(text(256))}),&[],true)}),
        &[],
        true,
    );
    definitions["WorkDelegationState"]["description"] = json!(
        "Legacy tasks default to disabled delegation, no coordinator and epoch zero. Only root tasks may enable delegation. Configuration does not grant native execution capability or create an assistant."
    );
    definitions["WorkTaskDependency"] = object(
        json!({"prerequisite_task_id":text(256),"submission_id":nullable(text(256))}),
        &[],
        false,
    );
    definitions["WorkTaskDependency"]["description"] = json!(
        "A prerequisite in the same session, project and direct-child group. Cycles, duplicate prerequisites and self-dependencies are refused. A null submission remains waiting until an immutable result is explicitly selected; admission records that exact version in dependency_snapshot. Agent idle/exit is never a dependency result."
    );
    definitions["WorkDependencyUpdateRequest"] = object(
        json!({"request_key":text(128),"expected_revision":uint(),"dependencies":{"type":"array","maxItems":16,"items":reference("WorkTaskDependency")}}),
        &[],
        true,
    );
    definitions["WorkDependencyUpdateRequest"]["description"] = json!(
        "Replace dependencies on this existing task before dispatch. Select exact immutable submission IDs; null remains waiting. No child replacement, native startup, delivery or automatic retry occurs. Local configured leads use `work set-child-dependencies` with JSON stdin containing task_id plus these same fields; the existing bounded private local transport supplies authority. Recover with child-receipt kind set_dependencies and the original request_key."
    );
    definitions["WorkDependencySnapshot"] = object(
        json!({"prerequisite_task_id":text(256),"submission_id":text(256),"review_id":nullable(text(256))}),
        &[],
        false,
    );
    definitions["WorkDelegationFence"] = object(
        json!({"coordinator_task_id":text(256),"coordinator_attempt_id":text(256),"coordinator_epoch":uint(),"instance_id":text(256),"native_owner_epoch":text(256)}),
        &[],
        false,
    );
    definitions["WorkDelegationFence"]["description"] = json!(
        "Server-constructed authority snapshot, not a client-supplied permission. Exact coordinator attempt/epoch and native generation must remain valid at dispatch."
    );
    definitions["WorkTask"]["properties"]["delegation"] = reference("WorkDelegationState");
    definitions["WorkTask"]["properties"]["dependencies"] =
        json!({"type":"array","maxItems":16,"items":reference("WorkTaskDependency"),"default":[]});
    definitions["WorkOperation"]["properties"]["delegation_fence"] =
        nullable(reference("WorkDelegationFence"));
    definitions["WorkOperation"]["properties"]["dependency_snapshot"] = json!({"type":"array","maxItems":16,"items":reference("WorkDependencySnapshot"),"default":[]});
    definitions["WorkInterruptionReceipt"] = object(
        json!({"operation_id":text(256),"launch_id":text(256),"owner_epoch":text(256),"receipt_id":text(256),"key":text(64),"bytes_written":uint(),"input_disposition":text(64)}),
        &[],
        false,
    );
    definitions["WorkInterruptionReceipt"]["properties"]["key"] =
        json!({"type":"string","enum":["Escape"]});
    definitions["WorkInterruptionReceipt"]["properties"]["bytes_written"] =
        json!({"type":"integer","enum":[1]});
    definitions["WorkInterruptionReceipt"]["properties"]["input_disposition"] =
        json!({"type":"string","enum":["written"]});
    definitions["WorkInterruptRequest"] = revision_request(
        json!({"expected_instance_id":text(256),"expected_native_owner_epoch":text(256)}),
        &[],
    );
    definitions["WorkOperation"]["properties"]["interruption_receipt"] =
        nullable(reference("WorkInterruptionReceipt"));
    definitions["WorkOperation"]["properties"]["interruption_owner_epoch"] = nullable(text(256));
    definitions["WorkOperation"]["properties"]["kind"]["enum"]
        .as_array_mut()
        .unwrap()
        .push(json!("interrupt_attempt"));
    definitions
}
fn parameter(name: &str, location: &str, schema: Value) -> Value {
    json!({"name":name,"in":location,"required":location=="path","schema":schema})
}
fn endpoint(
    summary: &str,
    params: Vec<Value>,
    response: &str,
    request: Option<&str>,
    execution: bool,
    details: &str,
) -> Value {
    let mut responses = json!({"200":{"description":"Recorded response. Read the operation state; HTTP success does not prove task completion.","content":{"application/json":{"schema":reference(response)}}}});
    for (code, description) in [
        (
            "400",
            "Invalid request or pagination; JSON/query extractor failures may be plain text.",
        ),
        ("401", "Missing or invalid authentication."),
        (
            "403",
            "A paired device is required (manager/admin bearer is refused), or repository scope was refused.",
        ),
        ("404", "Session or scoped record not found."),
        (
            "409",
            "Revision/request-key conflict, instance/readiness refusal, ambiguous delivery, or unavailable/changed artifact.",
        ),
        ("413", "Request body exceeds the server's configured limit."),
        ("415", "Expected application/json."),
        (
            "422",
            "JSON field shape rejected by the extractor; response may be plain text.",
        ),
        ("429", "Resource limit reached."),
        ("501", "Selected session lacks the required capability."),
        ("503", "Durable work storage unavailable."),
    ] {
        responses[code] = json!({"description":description,"content":{"application/json":{"schema":reference("WorkError")},"text/plain":{"schema":{"type":"string"}}}});
    }
    let mut operation = json!({"summary":summary,"tags":["Work"],"security":[{"pairedDeviceBearer":[]}],"parameters":params,"responses":responses,"x-required-capability":if execution {"work_execution_v1"}else{"work_tasks_v1"},"description":format!("{details} Paired-device authority only; the local manager/admin bearer is not accepted. Capability names describe prerequisites, not availability: inspect the selected session before use.")});
    if let Some(request) = request {
        operation["x-maxRequestBodyBytes"] = json!(crate::MAX_REQUEST_BODY_BYTES);
        operation["requestBody"] =
            json!({"required":true,"content":{"application/json":{"schema":reference(request)}}});
        let description = operation["description"].as_str().unwrap().to_owned();
        operation["description"] = json!(format!(
            "{description} The full JSON body is limited to 128 KiB including encoding overhead. request_key is scoped to paired actor, session and operation kind. Identical replay returns its durable receipt without repeating native effects; a changed payload with that key conflicts. expected_revision, where present, binds the mutation to the task revision displayed by the caller. An unconfirmed operation must be inspected, never automatically retried. Startup and initial prompt delivery are separate operations with separate request keys."
        ));
    }
    operation
}

pub(super) fn extend(spec: &mut Value) {
    spec["components"]["securitySchemes"]["pairedDeviceBearer"] = json!({"type":"http","scheme":"bearer","description":"Token issued to a paired device. Manager/admin bearer credentials cannot access work routes."});
    if spec["components"]["schemas"].is_null() {
        spec["components"]["schemas"] = json!({});
    }
    spec["components"]["schemas"]
        .as_object_mut()
        .unwrap()
        .extend(schemas().as_object().unwrap().clone());
    let session = || parameter("session_id", "path", text(256));
    let task = || parameter("task_id", "path", text(256));
    let limit = || {
        parameter(
            "limit",
            "query",
            json!({"type":"integer","minimum":1,"maximum":100,"default":50}),
        )
    };
    let paths = &mut spec["paths"];
    paths["/api/sessions/{session_id}/work/tasks/{task_id}/dependencies"] = json!({"post":endpoint("Select immutable dependency versions",vec![session(),task()],"WorkTaskMutation",Some("WorkDependencyUpdateRequest"),true,"Paired controller only. Update the existing task with an expected revision and exact prerequisite submission IDs before dispatch. Project, sibling-group, cycle, result and human-acceptance policy checks remain authoritative in the store. Null pins remain waiting. This changes metadata only: no assistant is started or sent input. Identical key replay returns its original receipt; changed payload conflicts. Local lead CLI authority is separately fenced to configured direct children.")});
    paths["/api/sessions/{session_id}/work/tasks/{task_id}/delegation-config"] = json!({"post":endpoint("Configure the current local lead",vec![session(),task()],"WorkTaskMutation",Some("WorkDelegationConfigRequest"),true,"Paired control only. Enabling requires an existing unexpired local reporting grant, a confirmed reserved lead and an exact native LIVE observation. Device, transport, grant and store state are revalidated at commit. A changed configuration increments coordinator_epoch and invalidates old control bindings. Replay never reactivates runtime authority; restart requires explicit regrant. Disable preserves worker reporting and existing native processes. Configuration alone does not advertise local delegation capability.")});
    let mut upload = endpoint(
        "Upload one scoped immutable task input",
        vec![session()],
        "WorkInputReceipt",
        None,
        false,
        "Authenticate before multipart parsing. Exactly one request_key, repo_path and file in any order; reject duplicate/unknown fields. Canonical project authorization precedes publication. Content detection controls MIME; executables refused and names sanitized. File limit 10 MiB, multipart plaintext limit 25 MiB, shared immutable blob quota 1 GiB/16384 entries. Actor/session/key replay returns the original receipt; changed content conflicts. Unclaimed inputs expire after 48 hours; claimed task inputs remain retained. No task is created or assistant dispatched.",
    );
    upload["x-required-capability"] = json!("work_inputs_v1");
    upload["requestBody"] = json!({"required":true,"content":{"multipart/form-data":{"schema":object(json!({"request_key":text(128),"repo_path":text(4096),"file":{"type":"string","format":"binary","description":"Exactly one named file; max 10 MiB."}}),&[],true)}}});
    upload["x-maxRequestBodyBytes"] = json!(25 * 1024 * 1024);
    paths["/api/sessions/{session_id}/work/inputs"] = json!({"post":upload});
    let mut input_key = parameter("request_key", "query", text(128));
    input_key["required"] = json!(true);
    let mut lookup = endpoint(
        "Read a scoped input upload receipt",
        vec![session(), input_key],
        "WorkInputReceipt",
        None,
        false,
        "Read-only recovery scoped to authenticated paired actor/session. An expired receipt remains readable but cannot be newly claimed. Missing receipt does not prove no upload is in flight; no automatic upload or task replay.",
    );
    lookup["x-required-capability"] = json!("work_inputs_v1");
    paths["/api/sessions/{session_id}/work/input-receipts"] = json!({"get":lookup});
    let mut receipt_kind = parameter(
        "kind",
        "query",
        json!({"type":"string","enum":["create_task","start_attempt","deliver_prompt","submit_result","review_result","pause_task"]}),
    );
    receipt_kind["required"] = json!(true);
    receipt_kind["schema"]["enum"]
        .as_array_mut()
        .unwrap()
        .extend([
            json!("reconcile_attempt"),
            json!("configure_delegation"),
            json!("set_dependencies"),
            json!("interrupt_attempt"),
        ]);
    let mut request_key = parameter("request_key", "query", text(128));
    request_key["required"] = json!(true);
    paths["/api/sessions/{session_id}/work/receipts"] = json!({"get":endpoint("Reconcile a committed request key",vec![session(),receipt_kind,request_key],"WorkRequestReceipt",None,false,"Read-only lookup scoped to the exact paired actor, session and operation kind. Start/delivery returns the current durable operation. A 404 means no committed receipt was found; it does not prove that no effects occurred or that no request is in flight. Never automatically resend after an absent or ambiguous receipt.")});
    let mut reconcile = endpoint(
        "Check an exact assistant lifecycle",
        vec![
            session(),
            task(),
            parameter("attempt_id", "path", text(256)),
        ],
        "WorkReconciliationMutation",
        Some("WorkReconcileRequest"),
        false,
        "Explicit check only. The expected launch and owner epoch must match stored identities including null. Evidence comes from the dispatch fence or exact native supervisor receipt, never a client claim, pane status or timestamp. Live/unknown retains reservation. Release preserves history and resource references; it does not stop, start, delete or replay anything. Exact replay returns its immutable check result. Local assistant grants cannot use this route. Native lookup occurs outside database/grant locks; commit and authority revocation are serialized.",
    );
    reconcile["x-required-capability"] = json!("work_attempt_reconciliation_v1");
    paths
        ["/api/sessions/{session_id}/work/tasks/{task_id}/attempts/{attempt_id}/reconciliations"] =
        json!({"post":reconcile});
    let mut summaries = endpoint(
        "List compact task facts in one session snapshot",
        vec![session(), parameter("after_id","query",text(256)), parameter("snapshot_cursor","query",uint()), parameter("limit","query",json!({"type":"integer","minimum":1,"maximum":20,"default":20}))],
        "WorkTaskSummaryPage", None, false,
        "First page omits both after_id and snapshot_cursor; subsequent pages require both. A changed session watermark returns revision_conflict: retain displayed rows and offer explicit Refresh. At most 20 rows and 128 KiB encoded; no native reads or per-row detail calls. Facts are not task completion: reservations may outlive processes, acceptance applies only to the exact latest submission, and unknown activity kinds require neutral labels. Absent activity/result/review is explicit null. No result text, brief, artifacts, prompt or terminal output is included. Projection unavailable keeps this capability absent and the original task list usable.",
    );
    summaries["x-required-capability"] = json!("work_task_summaries_v1");
    summaries["responses"]["200"]["x-maxEncodedBytes"] = json!(131072);
    paths["/api/sessions/{session_id}/work/task-summaries"] = json!({"get":summaries});
    paths["/api/sessions/{session_id}/work/tasks"] = json!({
        "get":endpoint("List scoped tasks",vec![session(),parameter("after_id","query",text(256)),limit()],"WorkTaskPage",None,false,"Pass next_after_id to continue. A full final page may require one more empty request."),
        "post":endpoint("Create a durable task",vec![session()],"WorkTaskMutation",Some("WorkCreateRequest"),false,"Creating a task records intent and policy; it does not start an assistant.")
    });
    paths["/api/sessions/{session_id}/work/tasks/{task_id}"] = json!({"get":endpoint("Read a task snapshot",vec![session(),task()],"WorkDetail",None,false,"Separate attempts, operations, immutable result submissions and reviews. Idle is not task completion.")});
    let mut record_kind = parameter(
        "kind",
        "query",
        json!({"type":"string","enum":["attempt","operation","result","review"]}),
    );
    record_kind["required"] = json!(true);
    let mut snapshot_revision = parameter("snapshot_revision", "query", uint());
    snapshot_revision["required"] = json!(true);
    paths["/api/sessions/{session_id}/work/tasks/{task_id}/records"] = json!({"get":endpoint("Continue one task record collection",vec![session(),task(),record_kind,snapshot_revision,parameter("after_id","query",text(256)),parameter("limit","query",json!({"type":"integer","minimum":1,"maximum":20,"default":20}))],"WorkRecordPage",None,false,"Records are ordered by ID. Continue each collection using its own page.next_after_id and snapshot_revision from task detail. A changed revision returns 409: explicitly refresh, never merge different revisions. Pages stop at 20 records or the serialized item byte budget of 2 MiB. has_more distinguishes a bounded prefix from complete history. References may point into unseen pages.")});
    for (suffix, id, schema) in [
        ("results", "submission_id", "WorkResult"),
        ("reviews", "review_id", "WorkReview"),
    ] {
        paths[format!("/api/sessions/{{session_id}}/work/tasks/{{task_id}}/{suffix}/{{{id}}}")] = json!({"get":endpoint("Read an exact immutable record",vec![session(),task(),parameter(id,"path",text(256))],schema,None,false,"Returns only the requested result or review within its task and session. Never substitutes a newer submission.")});
    }
    for (suffix, summary, request, response, execution, details) in [
        (
            "results",
            "Submit an immutable result",
            "WorkSubmitResultRequest",
            "WorkResultMutation",
            false,
            "The submission belongs to one attempt. Files are captured into digest-addressed storage; later source edits never substitute bytes in this version.",
        ),
        (
            "reviews",
            "Review an exact result submission",
            "WorkReviewRequest",
            "WorkReviewMutation",
            false,
            "submission_id selects an immutable result version. Acceptance neither merges/publishes code nor stops assistants.",
        ),
        (
            "delegation",
            "Pause or resume new delegation",
            "WorkPauseRequest",
            "WorkTaskMutation",
            false,
            "Pausing applies to future managed launches, including descendants. Existing assistants continue; this is not an OS sandbox or an interrupt.",
        ),
        (
            "attempts",
            "Start a recorded assistant attempt",
            "WorkStartRequest",
            "WorkOperationMutation",
            true,
            "Reserves an attempt and records partial resources. Workers require a branch/worktree. No initial prompt is sent by this request.",
        ),
    ] {
        paths[format!("/api/sessions/{{session_id}}/work/tasks/{{task_id}}/{suffix}")] = json!({"post":endpoint(summary,vec![session(),task()],response,Some(request),execution,details)});
    }
    paths["/api/sessions/{session_id}/work/tasks/{task_id}/attempts/{attempt_id}/deliveries"] = json!({"post":endpoint("Deliver to an exact assistant instance",vec![session(),task(),parameter("attempt_id","path",text(256))],"WorkOperationMutation",Some("WorkDeliveryRequest"),true,"The attempt and expected_instance_id must still match the current native assistant. Unknown/busy/approval states refuse delivery. A lost acknowledgment remains unconfirmed; no automatic resend.")});
    let mut interrupt = endpoint(
        "Request interruption of an exact assistant instance",
        vec![
            session(),
            task(),
            parameter("attempt_id", "path", text(256)),
        ],
        "WorkOperationMutation",
        Some("WorkInterruptRequest"),
        true,
        "One explicit cancellation key to the recorded launch and owner epoch. A validated acknowledgement proves one Escape byte was written, not that the assistant stopped or the task completed. Uncertain prompt delivery may coexist with this operation; neither receipt rewrites the other. No automatic retry, pane fallback, reservation release or result acceptance.",
    );
    interrupt["x-required-capability"] = json!("work_interrupt_v1");
    paths["/api/sessions/{session_id}/work/tasks/{task_id}/attempts/{attempt_id}/interruptions"] =
        json!({"post":interrupt});
    paths["/api/sessions/{session_id}/work/operations/{operation_id}"] = json!({"get":endpoint("Inspect a durable operation receipt",vec![session(),parameter("operation_id","path",text(256))],"WorkOperation",None,false,"Read-only recovery after reconnect. Never infer task completion from acknowledgment.")});
    paths["/api/sessions/{session_id}/work/changes"] = json!({"get":endpoint("Read work changes",vec![session(),parameter("after_cursor","query",uint()),limit()],"WorkChangePage",None,false,"A session-scoped cursor feed over persisted records, not a native execution queue. reset_required asks the caller to reload a snapshot.")});
    let mut after_cursor = parameter("after_cursor", "query", uint());
    after_cursor["required"] = json!(true);
    let mut events = endpoint(
        "Observe durable work changes",
        vec![session(), after_cursor],
        "WorkChangePage",
        None,
        false,
        "Explicit after_cursor is mandatory; Last-Event-ID is not used for replay. Paired authority is revalidated before every page. Backlog pages contain at most 100 changes and drain immediately; otherwise work.changes is emitted every two seconds, including empty pages. work.reset emits a ChangePage with reset_required=true then closes: load a fresh snapshot before reconnecting. work.unavailable emits {} then closes. Revocation closes without a further page. Uses the existing encrypted SSE transport when negotiated: muqun.encrypted carries {v:1,sid,seq,ciphertext}; decrypted records contain event plus data (a JSON string). No cache.",
    );
    events["responses"]["200"] = json!({"description":"SSE notifications over durable state, not execution requests. Keep-alive comments may occur every 15 seconds.","headers":{"Cache-Control":{"schema":{"type":"string","const":"private, no-store"}}},"content":{"text/event-stream":{"schema":{"type":"string"}}},"x-events":{"work.changes":reference("WorkChangePage"),"work.reset":reference("WorkChangePage"),"work.unavailable":{"type":"object","maxProperties":0}}});
    paths["/api/sessions/{session_id}/work/events"] = json!({"get":events});
    let mut artifact = endpoint(
        "Read an immutable artifact",
        vec![
            session(),
            task(),
            parameter("submission_id", "path", text(256)),
            parameter("index", "path", uint()),
        ],
        "WorkError",
        None,
        false,
        "index is zero-based within the submission's artifacts. Authorization checks task/submission membership before blob access. Stored bytes are checked against the recorded size and digest. Never falls back to the live repository path.",
    );
    artifact["responses"]["200"] = json!({"description":"Exact digest-verified bytes captured for this result version; download as untrusted content.","headers":{"Content-Disposition":{"schema":{"type":"string","const":"attachment; filename=artifact.bin"}},"X-Content-Type-Options":{"schema":{"type":"string","const":"nosniff"}},"Cache-Control":{"schema":{"type":"string","const":"private, no-store"}}},"content":{"application/octet-stream":{"schema":{"type":"string","format":"binary"}}}});
    paths["/api/sessions/{session_id}/work/tasks/{task_id}/results/{submission_id}/artifacts/{index}"] =
        json!({"get":artifact});
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::model::*;

    #[test]
    fn summary_schema_is_compact_nullable_and_bounded() {
        let definitions = schemas();
        let summary = &definitions["WorkTaskSummary"];
        for absent in ["brief", "artifacts", "prompt", "output", "status"] {
            assert!(summary["properties"].get(absent).is_none());
        }
        for field in ["last_activity", "latest_result", "parent_task_id"] {
            assert_eq!(summary["properties"][field]["anyOf"][1]["type"], "null");
            assert!(summary["required"]
                .as_array()
                .unwrap()
                .contains(&json!(field)));
        }
        assert_eq!(
            definitions["WorkTaskSummaryPage"]["properties"]["items"]["maxItems"],
            20
        );
        assert_eq!(
            definitions["WorkTaskSummaryPage"]["properties"]["snapshot_cursor"]["maximum"],
            9007199254740991u64
        );
    }

    #[test]
    fn receipt_and_pagination_routes_preserve_read_only_recovery_contract() {
        let mut spec = json!({"paths":{},"components":{"schemas":{}}});
        extend(&mut spec);
        let receipt = &spec["paths"]["/api/sessions/{session_id}/work/receipts"];
        assert!(receipt.get("post").is_none());
        assert!(receipt["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "request_key" && p["required"] == true));
        assert!(receipt["get"]["description"]
            .as_str()
            .unwrap()
            .contains("does not prove"));
        let records =
            &spec["paths"]["/api/sessions/{session_id}/work/tasks/{task_id}/records"]["get"];
        assert!(records["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "snapshot_revision" && p["required"] == true));
        assert!(records["responses"].get("409").is_some());
        assert_eq!(
            spec["components"]["schemas"]["WorkRecordPage"]["properties"]["items"]["maxItems"],
            20
        );
        assert_eq!(
            spec["components"]["schemas"]["WorkRequestReceipt"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
    }

    // Walk serialized fields rather than a hand-maintained expected-key list: a new
    // Rust field or changed serde flatten/null behavior must update the public contract.
    fn assert_shape(value: &Value, schema: &Value, definitions: &Value) {
        if let Some(path) = schema["$ref"].as_str() {
            return assert_shape(
                value,
                &definitions[path.rsplit('/').next().unwrap()],
                definitions,
            );
        }
        if let Some(variants) = schema["anyOf"].as_array() {
            if value.is_null() {
                assert!(variants.iter().any(|variant| variant["type"] == "null"));
            } else {
                assert_shape(value, &variants[0], definitions);
            }
            return;
        }
        if let Some(choices) = schema["enum"].as_array() {
            assert!(choices.contains(value), "{value} outside {choices:?}");
            if schema.get("type").is_none() {
                return;
            }
        }
        match schema["type"].as_str().unwrap() {
            "object" => {
                let fields = value.as_object().unwrap();
                let properties = schema["properties"].as_object().unwrap();
                assert_eq!(
                    fields.len(),
                    properties.len(),
                    "serialized fields diverge: {fields:?}"
                );
                for required in schema["required"].as_array().unwrap() {
                    assert!(fields.contains_key(required.as_str().unwrap()));
                }
                for (key, value) in fields {
                    assert_shape(value, &properties[key], definitions);
                }
            }
            "array" => {
                for item in value.as_array().unwrap() {
                    assert_shape(item, &schema["items"], definitions);
                }
            }
            "string" => assert!(value.is_string()),
            "integer" => assert!(value.is_i64() || value.is_u64()),
            "boolean" => assert!(value.is_boolean()),
            other => panic!("unexpected schema type {other}"),
        }
    }

    #[test]
    fn delegation_defaults_are_disabled_and_published_without_enabling_routes() {
        let definitions = schemas();
        let state = DelegationState::default();
        assert!(!state.policy.enabled);
        assert_eq!(state.coordinator_epoch, 0);
        assert!(state.coordinator_attempt_id.is_none());
        assert_shape(
            &serde_json::to_value(state).unwrap(),
            &definitions["WorkDelegationState"],
            &definitions,
        );
        for kind in [
            OperationKind::ConfigureDelegation,
            OperationKind::SetDependencies,
        ] {
            let serialized = serde_json::to_value(kind).unwrap();
            assert!(definitions["WorkOperation"]["properties"]["kind"]["enum"]
                .as_array()
                .unwrap()
                .contains(&serialized));
        }
        assert_eq!(
            definitions["WorkDelegationPolicy"]["properties"]["max_depth"]["maximum"],
            1
        );
    }

    #[test]
    fn serialized_detail_matches_all_published_record_fields() {
        let task = Task {
            delegation: Default::default(),
            dependencies: vec![],
            input_refs: vec![],
            id: "task".into(),
            session_id: "session".into(),
            repo_path: "/repo".into(),
            title: "Task".into(),
            brief: "Work".into(),
            parent_task_id: None,
            revision: 3,
            created_at_ms: 1,
            updated_at_ms: 2,
            paused: false,
            policy: TaskPolicy {
                allowed_agents: vec!["codex".into()],
                max_workers: 1,
            },
        };
        let detail = TaskDetail {
            task,
            attempts: vec![Attempt {
                lifecycle: AttemptLifecycle::default(),
                id: "attempt".into(),
                task_id: "task".into(),
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
                binding: NativeBinding::default(),
                created_at_ms: 1,
            }],
            operations: vec![Operation {
                interruption_receipt: None,
                interruption_owner_epoch: None,
                delegation_fence: None,
                dependency_snapshot: vec![],
                input_refs: vec![],
                bootstrap_version: Some(crate::work_prompt::INSTRUCTIONS_VERSION.into()),
                id: "operation".into(),
                task_id: "task".into(),
                attempt_id: Some("attempt".into()),
                kind: OperationKind::DeliverPrompt,
                state: OperationState::Unconfirmed,
                resources: NativeBinding::default(),
                failure_code: Some(FailureCode::DeliveryUnconfirmed),
                created_at_ms: 1,
                updated_at_ms: 2,
            }],
            results: vec![ResultSubmission {
                id: "result".into(),
                task_id: "task".into(),
                result: ResultInput {
                    attempt_id: "attempt".into(),
                    summary: "Done".into(),
                    artifacts: vec![ArtifactRef {
                        path: "result.txt".into(),
                        sha256: "a".repeat(64),
                        size_bytes: 1,
                    }],
                    evidence: vec!["Tests passed".into()],
                },
                created_at_ms: 2,
            }],
            reviews: vec![Review {
                id: "review".into(),
                task_id: "task".into(),
                actor_id: "phone".into(),
                review: ReviewInput {
                    submission_id: "result".into(),
                    decision: ReviewDecision::Accepted,
                    message: None,
                },
                created_at_ms: 3,
            }],
            cursor: 3,
        };
        let definitions = schemas();
        let page = crate::work::store::pagination::Page {
            snapshot_revision: detail.task.revision,
            after_id: None,
            next_after_id: None,
            has_more: false,
        };
        let detail = crate::work::store::pagination::PagedDetail {
            detail,
            pages: crate::work::store::pagination::DetailPages {
                attempts: page.clone(),
                operations: page.clone(),
                results: page.clone(),
                reviews: page,
            },
        };
        assert_shape(
            &serde_json::to_value(detail).unwrap(),
            &definitions["WorkDetail"],
            &definitions,
        );
        let page = ChangePage {
            changes: vec![TaskChange {
                cursor: 1,
                task_id: "task".into(),
                revision: 1,
                kind: "created".into(),
                entity_id: "task".into(),
            }],
            cursor: 1,
            reset_required: false,
        };
        assert_shape(
            &serde_json::to_value(page).unwrap(),
            &definitions["WorkChangePage"],
            &definitions,
        );
    }

    #[test]
    fn legacy_operation_without_bootstrap_metadata_still_deserializes() {
        let operation = json!({"id":"operation","task_id":"task","attempt_id":null,"kind":"deliver_prompt","state":"acknowledged","resources":{},"failure_code":null,"created_at_ms":1,"updated_at_ms":2});
        let decoded: Operation = serde_json::from_value(operation).unwrap();
        assert!(decoded.bootstrap_version.is_none());
        assert!(serde_json::to_value(decoded).unwrap()["bootstrap_version"].is_null());
    }

    #[test]
    fn execution_requests_match_serde_and_keep_start_separate_from_prompt() {
        use crate::work_execution::{DeliveryRequest, StartRequest};
        let definitions = schemas();
        let start = StartRequest {
            request_key: "start".into(),
            expected_revision: 1,
            agent_kind: "codex".into(),
            role: AttemptRole::Lead,
            branch_name: None,
        };
        assert_shape(
            &serde_json::to_value(start).unwrap(),
            &definitions["WorkStartRequest"],
            &definitions,
        );
        let delivery = DeliveryRequest {
            input_refs: vec![],
            request_key: "send".into(),
            expected_revision: 2,
            expected_instance_id: "instance".into(),
            text: "Work".into(),
        };
        assert_shape(
            &serde_json::to_value(delivery).unwrap(),
            &definitions["WorkDeliveryRequest"],
            &definitions,
        );
        assert!(definitions["WorkStartRequest"]["properties"]["prompt"].is_null());
        assert_eq!(
            definitions["WorkDeliveryRequest"]["properties"]["text"]["x-maxUtf8Bytes"],
            65536
        );
        assert_eq!(
            definitions["WorkStartRequest"]["additionalProperties"],
            false
        );
        assert!(!definitions["WorkReviewRequest"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("message")));
        assert!(definitions["WorkReview"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("message")));
    }

    #[test]
    fn installed_routes_require_paired_authority_and_binary_artifacts_do_not_become_json() {
        let spec = crate::openapi_spec();
        let paths = spec["paths"].as_object().unwrap();
        let router_source = include_str!("main.rs");
        let work_paths: Vec<_> = paths
            .iter()
            .filter(|(path, _)| path.contains("/work/"))
            .collect();
        assert_eq!(work_paths.len(), 22);
        for (path, item) in work_paths {
            assert!(
                router_source.contains(&format!("\"{path}\"")),
                "documented route is not mounted: {path}"
            );
            for method in ["get", "post"] {
                if let Some(operation) = item.get(method) {
                    assert_eq!(operation["security"], json!([{"pairedDeviceBearer":[]}]));
                    assert!(operation["responses"]["409"].is_object());
                    assert!(operation["description"]
                        .as_str()
                        .unwrap()
                        .contains("manager/admin bearer is not accepted"));
                }
            }
        }
        let stream = &paths["/api/sessions/{session_id}/work/events"]["get"];
        assert_eq!(stream["parameters"][1]["name"], "after_cursor");
        assert_eq!(stream["parameters"][1]["required"], true);
        assert!(stream["responses"]["200"]["content"]["text/event-stream"].is_object());
        assert_eq!(
            stream["responses"]["200"]["x-events"]["work.reset"],
            reference("WorkChangePage")
        );
        let artifact = &paths["/api/sessions/{session_id}/work/tasks/{task_id}/results/{submission_id}/artifacts/{index}"]
            ["get"]["responses"]["200"];
        assert!(artifact["content"]["application/json"].is_null());
        assert_eq!(
            artifact["content"]["application/octet-stream"]["schema"]["format"],
            "binary"
        );
        assert_eq!(
            artifact["headers"]["Cache-Control"]["schema"]["const"],
            "private, no-store"
        );
    }
}
