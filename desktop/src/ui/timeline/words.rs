//! The timeline's strings, beside the shared set in `ui/strings.rs`.

use crate::config::Language;
use crate::ui::strings::Strings;

macro_rules! words {
    ($($name:ident => $en:literal, $zh:literal;)*) => {
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

words! {
    copy => "Copy", "复制";
    thinking_live => "Thinking…", "思考中…";
    show_all => "Expand all", "展开全部";
    show_less => "Show less", "收起";
    loading_earlier => "Loading earlier messages…", "正在加载更早的消息…";
    opening_session => "Opening session…", "正在打开会话…";
    start_of_history => "Beginning of the conversation", "对话开始";
    earlier_available => "Load earlier messages", "加载更早的消息";
    input => "Input", "输入";
    output => "Output", "输出";
    tool_failed => "failed", "失败";
    waiting_approval => "waiting for approval", "等待批准";
    remember_session => "Remember for this session", "本会话记住";
    open_in_editor => "Open in editor", "在编辑器打开";
    allow => "Allow", "允许";
    revert => "Revert", "撤回";
    diff => "Diff", "Diff";
    jump_latest => "↓ Jump to latest", "↓ 回到最新";
    unknown_agent => "The agent", "Agent";
    todo_title => "Tasks", "任务";
    answer_needed => "Answer needed", "需要回答";
    submit => "Submit", "提交";
    answered => "Answered", "已回答";
    resolved => "Resolved", "已处理";
    compacted => "Context compacted", "上下文已压缩";
    compacting => "Compacting context…", "正在压缩上下文…";
    compaction_failed => "Compaction failed", "压缩失败";
    skill_used => "Skill", "技能";
    model_switched => "Model", "模型";
    agent_switched => "Agent mode", "Agent 模式";
    location_switched => "Moved to", "已移至";
    system => "System", "系统";
    synthetic => "Injected", "注入";
    yes => "Yes", "是";
    no => "No", "否";
    open_link => "Open", "打开";
    teach_title => "Start the conversation", "开始对话";
    teach_send => "Type below and press Enter to send · Shift+Enter starts a new line", "在下方输入，回车发送 · Shift+回车换行";
    teach_stop => "Replies stream in here · Esc stops a running turn", "回复会在这里流式出现 · Esc 停止正在进行的回合";
    teach_approve => "When the agent asks for permission, a card appears here · Y allows, N denies", "agent 请求授权时这里会出现卡片 · Y 允许，N 拒绝";
    unsupported_field => "This field can only be answered in the agent's own app.", "这个字段只能在 agent 自己的应用里回答。";
}

impl Strings {
    /// "3 files".
    pub fn files(&self, n: usize) -> String {
        match self.language() {
            Language::En if n == 1 => "1 file".to_string(),
            Language::En => format!("{n} files"),
            Language::ZhCn => format!("{n} 个文件"),
        }
    }

    /// "Claude Code wants to run": the start of an approval's one line.
    pub fn wants_to(&self, agent: &str, action: &str) -> String {
        let a = action.to_lowercase();
        let en = match a.as_str() {
            "bash" | "shell" | "exec" | "command" => "run".to_string(),
            "edit" | "write" | "patch" | "apply_patch" | "multiedit" => "edit".to_string(),
            "read" => "read".to_string(),
            "webfetch" | "fetch" => "fetch".to_string(),
            "glob" | "grep" | "list" | "search" => "search".to_string(),
            "task" | "subagent" => "start a subtask".to_string(),
            other => format!("use {other}"),
        };
        match self.language() {
            Language::En => format!("{agent} wants to {en}"),
            Language::ZhCn => {
                let zh = match a.as_str() {
                    "bash" | "shell" | "exec" | "command" => "运行".to_string(),
                    "edit" | "write" | "patch" | "apply_patch" | "multiedit" => "修改".to_string(),
                    "read" => "读取".to_string(),
                    "webfetch" | "fetch" => "访问".to_string(),
                    "glob" | "grep" | "list" | "search" => "搜索".to_string(),
                    "task" | "subagent" => "启动子任务".to_string(),
                    other => format!("使用 {other}"),
                };
                format!("{agent} 想要{zh}")
            }
        }
    }

    /// "↓ 3 new messages".
    pub fn new_messages(&self, n: usize) -> String {
        match self.language() {
            Language::En if n == 1 => "↓ 1 new message".to_string(),
            Language::En => format!("↓ {n} new messages"),
            Language::ZhCn => format!("↓ {n} 条新消息"),
        }
    }

    /// "Thinking · 4.2 s".
    pub fn thought_for(&self, ms: u64) -> String {
        let secs = ms as f64 / 1000.0;
        let label = self.thinking();
        if secs < 10.0 {
            format!("{label} · {secs:.1} s")
        } else {
            format!("{label} · {secs:.0} s")
        }
    }

    /// "+12 more lines".
    pub fn more_lines(&self, n: usize) -> String {
        match self.language() {
            Language::En => format!("{n} more lines"),
            Language::ZhCn => format!("还有 {n} 行"),
        }
    }

    /// "2 attachments". Kept for callers that only have a count.
    #[allow(dead_code)]
    pub fn attachments(&self, n: usize) -> String {
        match self.language() {
            Language::En if n == 1 => "1 attachment".to_string(),
            Language::En => format!("{n} attachments"),
            Language::ZhCn => format!("{n} 个附件"),
        }
    }

    /// "exit 1".
    pub fn exit_code(&self, code: f64) -> String {
        match self.language() {
            Language::En => format!("exit {code}"),
            Language::ZhCn => format!("退出码 {code}"),
        }
    }
}
