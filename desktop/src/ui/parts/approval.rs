//! A permission the agent asks for (App `agent-permission-card`): what it
//! wants to do as a phrase, the one thing it acts on, the change it would
//! make, a warning when the command would do whole-system damage, and the
//! answers in the order the engine gave them.

use muqun_gateway::agents::{PermissionDecision, PermissionOption, PermissionRequest};
use rune::{col, icon, row, text, El};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, Markdown};

use super::glyphs;
use crate::config::Language;
use crate::model::commands::Command;
use crate::model::tool_view as tv;
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

pub fn reply(
    request: &PermissionRequest,
    decision: PermissionDecision,
) -> impl Fn(&mut UiState) + 'static {
    let (asid, id) = (request.asid.clone(), request.id.clone());
    move |s: &mut UiState| {
        if let Some(list) = s.waiting_perms.get_mut(&asid.0) {
            list.retain(|p| p.id != id);
        }
        s.send(Command::ReplyPermission {
            asid: asid.clone(),
            id: id.clone(),
            decision,
        });
    }
}

pub fn pending_permission(s: &UiState, request: &PermissionRequest) -> bool {
    s.waiting_perms
        .get(&request.asid.0)
        .is_some_and(|l| l.iter().any(|p| p.id == request.id))
}

/// App `permissionActionPhrase`; an unknown key spelled out.
pub fn action_phrase(language: Language, action: &str) -> String {
    let t = |en, zh| tr_ref(language, en, zh).to_string();
    match action {
        "external_directory" => t("Read outside the workspace", "读取工作区之外的内容"),
        "external_write" => t("Write outside the workspace", "写入工作区之外的位置"),
        "bash" | "shell" | "command" | "execute" => t("Run a command", "执行命令"),
        "read" => t("Read a file", "读取文件"),
        "write" => t("Write a file", "写入文件"),
        "edit" => t("Edit a file", "编辑文件"),
        "patch" => t("Apply a patch", "应用补丁"),
        "webfetch" | "fetch" => t("Fetch a web page", "获取网页"),
        "network" => t("Reach the network", "访问网络"),
        "install" => t("Install a package", "安装软件包"),
        "task" => t("Start a subagent", "启动子代理"),
        "question" => t("Ask you a question", "向你提出一个问题"),
        "" => t("Permission required", "需要权限"),
        other => crate::ui::labels::readable_catalog_id(other),
    }
}

/// App `permissionOptionLabel`: known labels in the app's words, a
/// provider's own scope as it wrote it.
pub fn option_label(language: Language, option: &PermissionOption) -> String {
    let label = option.label.trim();
    let lower = label.to_ascii_lowercase();
    let t = |en, zh| tr_ref(language, en, zh).to_string();
    match option.decision {
        PermissionDecision::Allow if lower == "allow once" => t("Allow once", "允许一次"),
        PermissionDecision::Allow if lower == "allow for this turn" => {
            t("Allow for this turn", "允许本轮使用")
        }
        PermissionDecision::Allow if label.is_empty() || lower == "allow" => t("Allow", "允许"),
        PermissionDecision::AllowAlways
            if label.is_empty() || lower == "allow_always" || lower == "always allow" =>
        {
            t("Always allow", "始终允许")
        }
        PermissionDecision::Deny if label.is_empty() || lower == "deny" => t("Deny", "拒绝"),
        _ => label.replace('`', ""),
    }
}

pub fn danger_reason(language: Language, reason: &str) -> &'static str {
    let t = |en, zh| tr_ref(language, en, zh);
    match reason {
        "rm-system-root" => t(
            "Deletes a system folder or your whole home folder.",
            "会删除系统目录或整个主目录。",
        ),
        "fork-bomb" => t(
            "Starts processes until the computer stalls.",
            "会不断创建进程直到电脑卡死。",
        ),
        "block-device-write" => t("Writes straight onto a disk.", "会直接写入磁盘设备。"),
        "disk-format" => t("Formats a disk.", "会格式化磁盘。"),
        "chmod-system-root" => t(
            "Changes permissions of the whole filesystem.",
            "会更改整个文件系统的权限。",
        ),
        "remote-pipe-shell" => t(
            "Runs a script downloaded from the internet.",
            "会运行从网上下载的脚本。",
        ),
        _ => t(
            "This command can do serious damage.",
            "此命令可能造成严重破坏。",
        ),
    }
}

/// The diff an edit permission carries, drawn read-only before it is
/// approved (a tinted `diff` fence: the same rows, no state to keep).
pub fn permission_diff(k: &Kit, request: &PermissionRequest) -> Option<El> {
    let files = tv::diff_files(request.metadata.as_ref().and_then(|m| m.as_object()));
    if files.is_empty() {
        return None;
    }
    let mut block = col().key("perm-diff").w_full().gap(6.0);
    for (i, f) in files.iter().take(6).enumerate() {
        let (shown, _) = super::cap_lines(&f.patch, 120);
        block = block
            .child(
                text(f.path.clone())
                    .key(format!("p{i}"))
                    .font(k.ty.small_medium)
                    .color(k.c.text_muted)
                    .single_line()
                    .truncate_head()
                    .w_full(),
            )
            .child(
                Markdown::new(format!("```diff\n{shown}\n```"))
                    .build(k)
                    .key(format!("d{i}")),
            );
    }
    Some(block)
}

/// A permission as a card with its answers (or "Answered" once it has been).
pub fn permission_card(s: &UiState, k: &Kit, request: &PermissionRequest) -> El {
    let language = s.cfg.general.language;
    let pending = pending_permission(s, request);
    let danger = tv::dangerous_permission(request);
    let tone = if danger.is_some() {
        k.c.danger.icon
    } else {
        k.c.warning.icon
    };
    let subject = tv::permission_subject(request);
    let head = row()
        .key("head")
        .w_full()
        .gap(10.0)
        .center_y()
        .child(
            row()
                .key("mark")
                .size(28.0, 28.0)
                .center()
                .radius(k.radius.md)
                .fill(k.alpha(tone, if pending { 0.14 } else { 0.08 }))
                .child(icon(&glyphs::TRUST_WARNING, 15.0).color(tone)),
        )
        .child(
            col()
                .gap(1.0)
                .shrink()
                .grow(1.0)
                .child(
                    text(if pending {
                        tr_ref(language, "Permission required", "需要权限")
                    } else {
                        tr_ref(language, "Permission answered", "权限已答复")
                    })
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base),
                )
                .child(
                    text(action_phrase(language, &request.action))
                        .font(k.ty.small)
                        .color(k.c.text_muted),
                ),
        );
    let mut card = col()
        .key(format!("perm-{}", request.id))
        .w_full()
        .gap(10.0)
        .px(14.0)
        .py(12.0)
        .radius(k.radius.lg)
        .clip_rounded(k.radius.lg)
        .border(1.0, k.c.border_weak)
        .fill(k.c.surface_raised_base)
        .child(head);
    // Waiting on the owner: lit from its top edge in its tone, not outlined.
    if pending {
        card = card.child(s.look().top_light("perm-light", tone, 0.9));
    }
    if !pending {
        if !subject.is_empty() {
            card = card.child(
                text(subject)
                    .font(k.ty.mono)
                    .color(k.c.text_muted)
                    .single_line()
                    .truncate()
                    .w_full(),
            );
        }
        return card;
    }
    if let Some(reason) = danger {
        card = card.child(
            row()
                .key("danger")
                .w_full()
                .gap(8.0)
                .px(10.0)
                .py(7.0)
                .radius(k.radius.md)
                .fill(k.alpha(k.c.danger.icon, 0.10))
                .child(icon(&glyphs::WARNING, 14.0).color(k.c.danger.icon).mt(2.0))
                .child(
                    text(danger_reason(language, reason))
                        .font(k.ty.small_medium)
                        .color(k.c.danger.fg)
                        .wrap()
                        .shrink(),
                ),
        );
    }
    if let Some(what) = what_block(k, request, &subject) {
        card = card.child(what);
    }
    if let Some(diff) = permission_diff(k, request) {
        card = card.child(diff);
    }
    let mut options = options(request);
    let stacked = options.len() > 3 || options.iter().any(|o| o.label.chars().count() > 24);
    if stacked {
        options.sort_by_key(|o| o.decision != PermissionDecision::Allow);
    }
    let mut answers = if stacked {
        col().key("answers").w_full().gap(6.0)
    } else {
        row().key("answers").w_full().gap(8.0).flex_wrap()
    };
    for (i, option) in options.iter().enumerate() {
        let variant = match option.decision {
            PermissionDecision::Allow if danger.is_none() => ButtonVariant::Submit,
            PermissionDecision::Allow | PermissionDecision::AllowAlways => ButtonVariant::Outline,
            PermissionDecision::Deny if danger.is_some() => ButtonVariant::Submit,
            PermissionDecision::Deny => ButtonVariant::Ghost,
        };
        let mut button = Button::new(option_label(language, option))
            .variant(variant)
            .control_size(ControlSize::Small)
            .on_press(reply(request, option.decision))
            .build(k)
            .key(format!("perm-{}-{i}", request.id));
        if stacked {
            button = button.w_full();
        }
        answers = answers.child(button);
    }
    card = card.child(answers);
    if !request.save.is_empty() {
        card = card.child(
            text(format!(
                "{} {}",
                tr_ref(language, "Always allow covers", "始终允许将涵盖"),
                request.save.join(", ")
            ))
            .font(k.ty.small)
            .color(k.c.text_faint)
            .wrap(),
        );
    }
    card
}

/// What the permission acts on, the other resources and the agent's note,
/// in one quiet block; `None` when it names nothing.
pub fn what_block(k: &Kit, request: &PermissionRequest, subject: &str) -> Option<El> {
    let mut what = col()
        .key("what")
        .w_full()
        .gap(6.0)
        .px(10.0)
        .py(8.0)
        .radius(k.radius.md)
        .fill(k.c.surface_base);
    let mut any = false;
    if !subject.is_empty() {
        what = what.child(
            text(subject.to_string())
                .font(k.ty.mono)
                .color(k.c.text_base)
                .wrap_anywhere()
                .selectable(true)
                .w_full(),
        );
        any = true;
    }
    for res in request
        .resources
        .iter()
        .filter(|r| **r != subject && !request.save.contains(r))
        .take(8)
    {
        what = what.child(
            text(format!("• {res}"))
                .font(k.ty.mono)
                .color(k.c.text_muted)
                .wrap_anywhere()
                .w_full(),
        );
        any = true;
    }
    if let Some(message) = request.message.as_ref().filter(|m| !m.trim().is_empty()) {
        what = what.child(Markdown::new(message).build(k));
        any = true;
    }
    any.then_some(what)
}

/// The engine's answers in its order; the three every permission has when
/// it listed none (Always only when the engine offers something to save).
pub fn options(request: &PermissionRequest) -> Vec<PermissionOption> {
    if !request.options.is_empty() {
        return request.options.clone();
    }
    let mut base = vec![PermissionOption {
        index: 0,
        label: String::new(),
        decision: PermissionDecision::Allow,
    }];
    if !request.save.is_empty() {
        base.push(PermissionOption {
            index: 1,
            label: String::new(),
            decision: PermissionDecision::AllowAlways,
        });
    }
    base.push(PermissionOption {
        index: 2,
        label: String::new(),
        decision: PermissionDecision::Deny,
    });
    base
}
