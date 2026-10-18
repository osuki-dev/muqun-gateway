use crate::agents::adapters::t3::client::{new_id, ModelSelection};
use serde_json::{json, Value};

pub fn project_create(id: &str, title: &str, root: &str) -> Value {
    json!({"type":"project.create","commandId":new_id(),"projectId":id,"title":title,"workspaceRoot":root})
}

pub fn thread_create(
    id: &str,
    project: &str,
    title: &str,
    model: &ModelSelection,
    runtime: &str,
    interaction: &str,
) -> Value {
    json!({"type":"thread.create","createdBy":"user","creationSource":"mobile","commandId":new_id(),"threadId":id,"projectId":project,"title":title,"modelSelection":model.to_value(),"runtimeMode":runtime,"interactionMode":interaction,"branch":null,"worktreePath":null})
}

pub fn message_dispatch(
    id: &str,
    message: &str,
    text: &str,
    model: Option<&ModelSelection>,
    active: bool,
) -> Value {
    let mut command = json!({"type":"message.dispatch","createdBy":"user","creationSource":"mobile","commandId":new_id(),"threadId":id,"messageId":message,"text":text,"attachments":[],"dispatchMode":{"type":if active {"queue_after_active"} else {"start_immediately"}}});
    if let Some(model) = model {
        command["modelSelection"] = model.to_value();
    }
    command
}

pub fn interrupt(id: &str, run: &str) -> Value {
    json!({"type":"run.interrupt","commandId":new_id(),"threadId":id,"runId":run})
}

pub fn respond(id: &str, request: &str, decision: Option<&str>, answers: Option<Value>) -> Value {
    let mut command = json!({"type":"runtime-request.respond","commandId":new_id(),"threadId":id,"requestId":request});
    if let Some(decision) = decision {
        command["decision"] = json!(decision);
    }
    if let Some(answers) = answers {
        command["answers"] = answers;
    }
    command
}

pub fn set_model(id: &str, model: &ModelSelection) -> Value {
    json!({"type":"thread.model-selection.set","commandId":new_id(),"threadId":id,"modelSelection":model.to_value()})
}

pub fn rename(id: &str, title: &str) -> Value {
    json!({"type":"thread.metadata.update","commandId":new_id(),"threadId":id,"title":title})
}
