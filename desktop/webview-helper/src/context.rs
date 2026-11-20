//! Document-start context is confined to the plugin origin's top frame.
//! Replace the stored script on updates so later navigations start with fresh values.
use gtk::gio;
use serde_json::Value;
use webkit2gtk::{
    UserContentInjectedFrames, UserContentManagerExt, UserScript, UserScriptInjectionTime, WebView,
    WebViewExt,
};

pub struct Bridge {
    origin: String,
    installed: Option<UserScript>,
}

impl Bridge {
    pub fn new(origin: &url::Origin) -> Self {
        Self {
            origin: origin.ascii_serialization(),
            installed: None,
        }
    }

    pub fn set(&mut self, web: &WebView, value: &Value, live: bool) {
        if !value.is_object() {
            return;
        }
        let Some(manager) = web.user_content_manager() else {
            return;
        };
        let source = format!(
            "({})({},{})",
            include_str!("context.js"),
            serde_json::json!(self.origin),
            value
        );
        if let Some(old) = self.installed.take() {
            manager.remove_script(&old);
        }
        let script = UserScript::new(
            &source,
            UserContentInjectedFrames::TopFrame,
            UserScriptInjectionTime::Start,
            &[],
            &[],
        );
        manager.add_script(&script);
        self.installed = Some(script);
        if live {
            web.run_javascript(&source, None::<&gio::Cancellable>, |_| {});
        }
    }
}
