//! The two P0 string sets. Where the gateway's i18n catalogue already has
//! the wording (approval buttons) it is used instead, so the desktop and the
//! phone say the same thing.

use std::sync::atomic::{AtomicU8, Ordering};

use muqun_gateway::agents::AgentSessionStatus;
use muqun_gateway::platform::i18n;

use crate::config::Language;

/// The language every string is read in, process-wide: the settings page
/// switches it live, and every view reads it fresh on its next build (the
/// switch asks for a full rebuild), so no view holds a stale copy.
static LANGUAGE: AtomicU8 = AtomicU8::new(0);

/// A handle on the current language's strings. Zero-sized and `Copy`: views
/// carry one in [`App`](super::App) and call `app.strings.x()`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Strings;

impl Strings {
    pub fn set_language(language: Language) {
        LANGUAGE.store(
            match language {
                Language::En => 0,
                Language::ZhCn => 1,
            },
            Ordering::Relaxed,
        );
    }

    pub fn language(&self) -> Language {
        match LANGUAGE.load(Ordering::Relaxed) {
            1 => Language::ZhCn,
            _ => Language::En,
        }
    }
}

macro_rules! strings {
    ($($name:ident => $en:literal, $zh:literal;)*) => {
        // Entries may wait here for the view that will use them.
        #[allow(dead_code)]
        impl Strings {
            $(pub fn $name(&self) -> &'static str {
                match self.language() {
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
    composer_no_session => "Open or start a session to write to it", "打开或新建一个会话后再输入";
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
    no_usable_agent => "No agent is available on this computer yet.", "这台电脑上还没有可用的 agent。";
    settings => "Settings", "设置";
    no_folder => "No folder", "无文件夹";
    recent => "Recent", "最近";
    folder_missing => "folder not found", "文件夹不存在";
    folder_placeholder => "Type a path, or pick a recent folder", "输入路径，或选择最近的文件夹";
    new_session_hint => "Enter creates it with these choices; Esc cancels.", "回车按当前选择创建；Esc 取消。";
}

impl Strings {
    /// "N agents available on this computer".
    pub fn agents_available(&self, n: usize) -> String {
        match self.language() {
            Language::En if n == 1 => "1 agent available on this computer".to_string(),
            Language::En => format!("{n} agents available on this computer"),
            Language::ZhCn => format!("这台电脑上有 {n} 个可用的 agent"),
        }
    }

    /// "Message Claude Code — Enter to send…": names the session's own agent.
    pub fn composer_placeholder(&self, agent: &str) -> String {
        match self.language() {
            Language::En => {
                format!("Message {agent} — Enter to send")
            }
            Language::ZhCn => format!("给 {agent} 发消息 — 回车发送"),
        }
    }

    pub fn group_by(&self, grouping: crate::config::Grouping) -> &'static str {
        use crate::config::Grouping::*;
        match (self.language(), grouping) {
            (Language::En, Time) => "Group: by time",
            (Language::En, Agent) => "Group: by agent",
            (Language::En, Folder) => "Group: by folder",
            (Language::ZhCn, Time) => "分组：按时间",
            (Language::ZhCn, Agent) => "分组：按 Agent",
            (Language::ZhCn, Folder) => "分组：按文件夹",
        }
    }

    pub fn folder_unusable(&self, raw: &str) -> String {
        match self.language() {
            Language::En => format!("Can't use \"{raw}\" as a folder: give a path, or ~/…"),
            Language::ZhCn => format!("无法把“{raw}”作为文件夹：请输入路径或 ~/…"),
        }
    }

    pub fn approve(&self) -> &'static str {
        i18n::t(self.language().locale(), "Approve")
    }

    pub fn deny(&self) -> &'static str {
        i18n::t(self.language().locale(), "Deny")
    }

    #[allow(dead_code)] // the timeline renders status words
    pub fn status(&self, status: AgentSessionStatus) -> &'static str {
        use AgentSessionStatus::*;
        match (self.language(), status) {
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
