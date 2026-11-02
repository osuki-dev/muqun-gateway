//! The two P0 string sets. Where the gateway's i18n catalogue already has
//! the wording (approval buttons) it is used instead, so the desktop and the
//! phone say the same thing.

use muqun_gateway::agents::AgentSessionStatus;
use muqun_gateway::platform::i18n;

use crate::config::Language;

#[derive(Debug, Clone, Copy)]
pub struct Strings {
    pub language: Language,
}

macro_rules! strings {
    ($($name:ident => $en:literal, $zh:literal;)*) => {
        impl Strings {
            $(pub fn $name(&self) -> &'static str {
                match self.language {
                    Language::En => $en,
                    Language::ZhCn => $zh,
                }
            })*
        }
    };
}

strings! {
    sessions => "Sessions", "会话";
    new_session => "New session", "新建会话";
    no_sessions => "No OpenCode sessions yet.", "还没有 OpenCode 会话。";
    untitled => "Untitled", "未命名";
    pick_session => "Pick a session on the left, or start a new one.", "从左侧选择一个会话，或新建一个。";
    empty_timeline => "Nothing here yet.", "这里还没有内容。";
    composer_placeholder => "Message OpenCode — Enter to send, Shift+Enter for a new line", "给 OpenCode 发消息 — 回车发送，Shift+回车换行";
    send => "Send", "发送";
    stop => "Stop", "停止";
    thinking => "Thinking", "思考";
    dismiss => "Dismiss", "关闭";
    choose_folder => "Choose a folder for the new session", "为新会话选择文件夹";
    light => "Light", "浅色";
    dark => "Dark", "深色";
    permission_needed => "Permission needed", "需要授权";
    home => "Home", "首页";
    all => "All", "全部";
    agents => "Agents", "Agents";
    today => "Today", "今天";
    yesterday => "Yesterday", "昨天";
    this_week => "This week", "本周";
    earlier => "Earlier", "更早";
    launch => "Start a session", "开始会话";
    continue_working => "Continue working", "继续工作";
    needs_attention => "Needs attention", "需要你处理";
    agent => "Agent", "Agent";
    more_agents => "More", "更多";
    folder => "Folder", "文件夹";
    browse => "Browse…", "浏览…";
    create => "Create", "创建";
    cancel => "Cancel", "取消";
    new_session_hint => "Enter creates it with these choices; Esc cancels.", "回车按当前选择创建；Esc 取消。";
}

impl Strings {
    pub fn approve(&self) -> &'static str {
        i18n::t(self.language.locale(), "Approve")
    }

    pub fn deny(&self) -> &'static str {
        i18n::t(self.language.locale(), "Deny")
    }

    pub fn status(&self, status: AgentSessionStatus) -> &'static str {
        use AgentSessionStatus::*;
        match (self.language, status) {
            (Language::En, Idle) => "Idle",
            (Language::En, Busy) => "Working",
            (Language::En, Retry) => "Retrying",
            (Language::En, Failed) => "Failed",
            (Language::En, Interrupted) => "Stopped",
            (Language::En, Unknown) => "Unknown",
            (Language::ZhCn, Idle) => "空闲",
            (Language::ZhCn, Busy) => "运行中",
            (Language::ZhCn, Retry) => "重试中",
            (Language::ZhCn, Failed) => "失败",
            (Language::ZhCn, Interrupted) => "已停止",
            (Language::ZhCn, Unknown) => "未知",
        }
    }
}
