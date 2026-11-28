//! Privileged IPC for a user-activated append to the opening conversation.
use gtk::{gio, prelude::*};
use serde_json::{Value, json};
use std::{cell::Cell, rc::Rc, sync::mpsc::SyncSender};
use webkit2gtk::{
    UserContentInjectedFrames, UserContentManagerExt, UserScript, UserScriptInjectionTime, WebView,
    WebViewExt,
};

pub fn reply(web: &WebView, id: &Value, error: &Value) {
    web.run_javascript(
        &format!("window.__muqunDraftReply?.({id},{error})"),
        None::<&gio::Cancellable>,
        |_| {},
    );
}

pub fn install(
    web: &WebView,
    origin: url::Origin,
    nonce: String,
    sink: SyncSender<Vec<u8>>,
    in_flight: Rc<Cell<bool>>,
) {
    let Some(manager) = web.user_content_manager() else {
        return;
    };
    let weak = web.downgrade();
    let expected = origin.clone();
    let token = nonce.clone();
    manager.connect_script_message_received(Some("muqunDraft"), move |_, result| {
        let Some(web) = weak.upgrade() else {
            return;
        };
        if web
            .uri()
            .and_then(|s| url::Url::parse(&s).ok())
            .is_none_or(|u| u.origin() != expected)
        {
            return;
        }
        let Some(raw) = result.js_value().map(|v| v.to_string()) else {
            return;
        };
        if raw.len() > 65536 {
            return;
        }
        let Ok(data) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        if data["nonce"].as_str() != Some(token.as_str()) {
            return;
        }
        let Some(id) = data["id"]
            .as_str()
            .filter(|id| gtk::glib::uuid_string_is_valid(id))
        else {
            return;
        };
        let Some(text) = data["text"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 8192)
        else {
            return;
        };
        let payload = json!({"draft":{"text":text,"id":id}}).to_string();
        let mut packet = vec![0; 4];
        packet.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        packet.extend_from_slice(payload.as_bytes());
        if in_flight.get() || sink.try_send(packet).is_err() {
            reply(&web, &data["id"], &json!("Desktop is busy. Try again."));
        } else {
            in_flight.set(true);
        }
    });
    if !manager.register_script_message_handler("muqunDraft") {
        return;
    }
    let source = format!(
        "({})({},{})",
        include_str!("draft.js"),
        json!(origin.ascii_serialization()),
        json!(nonce)
    );
    manager.add_script(&UserScript::new(
        &source,
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
}
