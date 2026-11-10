//! "Open in …" (architecture §6.0‴ A): a project folder in an editor, a
//! terminal or the file manager, the way each platform starts them.

// Available to the UI; wired as those surfaces land.
#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Editor {
    #[default]
    VsCode,
    Zed,
    Cursor,
    Neovim,
    Terminal,
    FileManager,
}

impl Editor {
    pub const ALL: [Editor; 6] = [
        Self::VsCode,
        Self::Zed,
        Self::Cursor,
        Self::Neovim,
        Self::Terminal,
        Self::FileManager,
    ];

    pub const LABELS: [&'static str; 6] =
        ["VS Code", "Zed", "Cursor", "Neovim", "Terminal", "Files"];

    /// The product name (the same in every language) or `None` for the two
    /// the UI words itself (terminal, file manager).
    pub fn product(self) -> Option<&'static str> {
        match self {
            Self::VsCode => Some("VS Code"),
            Self::Zed => Some("Zed"),
            Self::Cursor => Some("Cursor"),
            Self::Neovim => Some("Neovim"),
            Self::Terminal | Self::FileManager => None,
        }
    }
}

/// Open `folder` with `editor`. Fire and forget: the program runs detached
/// and a failure to start it is logged and returned.
pub fn open(editor: Editor, folder: &Path) -> std::io::Result<()> {
    if editor == Editor::FileManager {
        super::shell::open_folder(folder);
        return Ok(());
    }
    let mut last = None;
    for (program, args) in commands(editor, &folder.to_string_lossy()) {
        let mut command = Command::new(&program);
        command
            .args(&args)
            .current_dir(folder)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(mut child) => {
                // Reap it off-thread so it never lingers as a zombie.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            Err(err) => last = Some(err),
        }
    }
    let err = last.unwrap_or_else(|| std::io::Error::other("no command for this platform"));
    tracing::warn!(?editor, "could not open {}: {err}", folder.display());
    Err(err)
}

/// Open `path` with an editor or appropriate system app.
pub fn open_file(
    path: &Path,
    custom_command: Option<&str>,
    choice: Option<Editor>,
) -> std::io::Result<()> {
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    if is_text_or_code(&abs_path) {
        if let Some(cmd) = custom_command.filter(|c| !c.trim().is_empty()) {
            if try_custom_command(cmd, &abs_path)? {
                return Ok(());
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("command failed: {cmd}"),
            ));
        }

        if let Some(editor) = choice {
            match editor {
                Editor::VsCode | Editor::Zed | Editor::Cursor => {
                    if try_editor_choice(editor, &abs_path)? {
                        return Ok(());
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("{} editor not found", editor.product().unwrap_or("GUI")),
                    ));
                }
                Editor::Neovim => {
                    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                    {
                        let p = abs_path.to_string_lossy().to_string();
                        if launch_in_terminal("nvim", &[p], abs_path.parent())? {
                            return Ok(());
                        }
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "nvim not found",
                    ));
                }
                Editor::Terminal => {
                    let term_ed = detect_default_terminal_editor().unwrap_or_else(|| "nvim".into());
                    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                    {
                        let p = abs_path.to_string_lossy().to_string();
                        if launch_in_terminal(&term_ed, &[p], abs_path.parent())? {
                            return Ok(());
                        }
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("{term_ed} not found"),
                    ));
                }
                Editor::FileManager => {
                    super::shell::reveal(&abs_path);
                    return Ok(());
                }
            }
        }
    }

    super::shell::open_path(&abs_path);
    Ok(())
}

/// Try opening a code/text file with the system's default editor (Omarchy launcher, $EDITOR, or terminal).
pub fn try_open_editor(path: &Path) -> std::io::Result<bool> {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if try_omarchy_launcher(path)? {
            return Ok(true);
        }
        if try_env_editor(path)? {
            return Ok(true);
        }
        if try_xdg_mime_editor(path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn is_text_or_code(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        return matches!(
            ext.to_ascii_lowercase().as_str(),
            "rs" | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "py"
                | "c"
                | "cpp"
                | "cc"
                | "cxx"
                | "h"
                | "hpp"
                | "go"
                | "rb"
                | "php"
                | "java"
                | "kt"
                | "swift"
                | "sh"
                | "bash"
                | "zsh"
                | "fish"
                | "lua"
                | "zig"
                | "nim"
                | "json"
                | "jsonc"
                | "json5"
                | "toml"
                | "yaml"
                | "yml"
                | "xml"
                | "html"
                | "htm"
                | "css"
                | "scss"
                | "sass"
                | "less"
                | "sql"
                | "md"
                | "markdown"
                | "txt"
                | "log"
                | "conf"
                | "cfg"
                | "ini"
                | "env"
                | "diff"
                | "patch"
                | "lock"
                | "dockerfile"
                | "graphql"
                | "proto"
                | "svg"
        );
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(
        name.to_ascii_lowercase().as_str(),
        "makefile" | "dockerfile" | "license" | "readme" | "gemfile" | "rakefile" | "vagrantfile"
    )
}

pub fn is_terminal_editor(program: &str) -> bool {
    let name = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "nvim" | "vim" | "vi" | "nano" | "micro" | "hx" | "helix" | "fresh" | "emacs"
    )
}

/// Detect the system's default terminal editor, if any (Omarchy defaults, $EDITOR, or nvim).
pub fn detect_default_terminal_editor() -> Option<String> {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // 1. Omarchy Linux default editor
        if let Some(home) = dirs::home_dir() {
            let omarchy_file = home.join(".local/state/omarchy/defaults/editor");
            if let Ok(content) = std::fs::read_to_string(omarchy_file) {
                let name = content.trim();
                if is_terminal_editor(name) {
                    return Some(name.to_string());
                }
            }
        }
        // 2. $EDITOR environment variable
        if let Ok(ed) = std::env::var("EDITOR") {
            let first = ed.split_whitespace().next().unwrap_or("");
            let name = Path::new(first)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if is_terminal_editor(name) {
                return Some(name.to_string());
            }
            if name == "omarchy-launch-editor" {
                if let Some(home) = dirs::home_dir() {
                    let omarchy_file = home.join(".local/state/omarchy/defaults/editor");
                    if let Ok(content) = std::fs::read_to_string(omarchy_file) {
                        let ed_name = content.trim();
                        if is_terminal_editor(ed_name) {
                            return Some(ed_name.to_string());
                        }
                    }
                }
                return Some("nvim".into());
            }
        }
        // 3. $VISUAL environment variable
        if let Ok(vis) = std::env::var("VISUAL") {
            let first = vis.split_whitespace().next().unwrap_or("");
            let name = Path::new(first)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if is_terminal_editor(name) {
                return Some(name.to_string());
            }
        }
        // 4. Default to nvim if installed on PATH
        if Command::new("which")
            .arg("nvim")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return Some("nvim".into());
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(ed) = std::env::var("EDITOR") {
            let first = ed.split_whitespace().next().unwrap_or("");
            let name = Path::new(first)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if is_terminal_editor(name) {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Determine if the configured or default editor is a terminal editor (e.g. nvim).
pub fn resolve_terminal_editor(
    custom_command: Option<&str>,
    choice: Option<Editor>,
) -> Option<String> {
    // 1. Custom command has highest priority
    if let Some(cmd) = custom_command.filter(|c| !c.trim().is_empty()) {
        let first = cmd.split_whitespace().next().unwrap_or("");
        if is_terminal_editor(first) {
            return Some(first.to_string());
        }
        return None;
    }

    // 2. Explicit choice of editor
    if let Some(c) = choice {
        match c {
            Editor::Neovim => return Some("nvim".into()),
            Editor::Terminal => {
                return detect_default_terminal_editor().or_else(|| Some("nvim".into()));
            }
            Editor::VsCode | Editor::Zed | Editor::Cursor | Editor::FileManager => {
                return None;
            }
        }
    }

    None
}

pub fn try_custom_command(cmd: &str, path: &Path) -> std::io::Result<bool> {
    let path_str = path.to_string_lossy();
    let full = if cmd.contains("{}") {
        cmd.replace("{}", &path_str)
    } else {
        format!("{cmd} {path_str}")
    };
    let parts: Vec<&str> = full.split_whitespace().collect();
    let Some(prog) = parts.first() else {
        return Ok(false);
    };
    let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    if is_terminal_editor(prog) {
        return launch_in_terminal(prog, &args, path.parent());
    }

    let mut command = Command::new(prog);
    command.args(&args);
    if let Some(dir) = path.parent() {
        command.current_dir(dir);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

pub fn try_editor_choice(editor: Editor, path: &Path) -> std::io::Result<bool> {
    let p = path.to_string_lossy();
    let cmds: Vec<(&str, Vec<&str>)> = if cfg!(target_os = "macos") {
        match editor {
            Editor::VsCode => vec![("open", vec!["-a", "Visual Studio Code", p.as_ref()])],
            Editor::Zed => vec![("open", vec!["-a", "Zed", p.as_ref()])],
            Editor::Cursor => vec![("open", vec!["-a", "Cursor", p.as_ref()])],
            Editor::Neovim => vec![("nvim", vec![p.as_ref()])],
            _ => Vec::new(),
        }
    } else if cfg!(target_os = "windows") {
        match editor {
            Editor::VsCode => vec![("code.cmd", vec![p.as_ref()])],
            Editor::Zed => vec![("zed", vec![p.as_ref()])],
            Editor::Cursor => vec![("cursor.cmd", vec![p.as_ref()])],
            Editor::Neovim => vec![("nvim", vec![p.as_ref()])],
            _ => Vec::new(),
        }
    } else {
        match editor {
            Editor::VsCode => vec![("code", vec![p.as_ref()])],
            Editor::Zed => vec![("zed", vec![p.as_ref()]), ("zeditor", vec![p.as_ref()])],
            Editor::Cursor => vec![("cursor", vec![p.as_ref()])],
            Editor::Neovim => vec![("nvim", vec![p.as_ref()])],
            _ => Vec::new(),
        }
    };

    for (bin, args) in cmds {
        let mut cmd = Command::new(bin);
        cmd.args(&args);
        if let Some(dir) = path.parent() {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = cmd.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn try_omarchy_launcher(path: &Path) -> std::io::Result<bool> {
    let mut cmd = Command::new("omarchy-launch-editor");
    cmd.arg(path);
    if let Some(dir) = path.parent() {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn try_env_editor(path: &Path) -> std::io::Result<bool> {
    let env_ed = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok();
    let Some(raw) = env_ed.filter(|e| !e.trim().is_empty()) else {
        return Ok(false);
    };

    if raw.contains("omarchy-launch-editor") {
        return try_omarchy_launcher(path);
    }

    let parts: Vec<&str> = raw.split_whitespace().collect();
    let Some(prog) = parts.first() else {
        return Ok(false);
    };
    let mut args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
    args.push(path.to_string_lossy().to_string());

    if is_terminal_editor(prog) {
        launch_in_terminal(prog, &args, path.parent())
    } else {
        let mut cmd = Command::new(prog);
        cmd.args(&args);
        if let Some(dir) = path.parent() {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match cmd.spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn try_xdg_mime_editor(path: &Path) -> std::io::Result<bool> {
    let Ok(out) = Command::new("xdg-mime")
        .args(["query", "default", "text/plain"])
        .output()
    else {
        return Ok(false);
    };
    let desktop = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if desktop.is_empty() {
        return Ok(false);
    }

    let lower = desktop.to_ascii_lowercase();
    let prog = if lower.contains("nvim") {
        Some("nvim")
    } else if lower.contains("vim") {
        Some("vim")
    } else if lower.contains("nano") {
        Some("nano")
    } else if lower.contains("helix") || lower.contains("hx") {
        Some("hx")
    } else {
        None
    };

    if let Some(editor) = prog {
        let p = path.to_string_lossy().to_string();
        return launch_in_terminal(editor, &[p], path.parent());
    }

    Ok(false)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn launch_in_terminal(program: &str, args: &[String], cwd: Option<&Path>) -> std::io::Result<bool> {
    // 1. Try omarchy-launch-tui
    let mut cmd = Command::new("omarchy-launch-tui");
    cmd.arg(program).args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        return Ok(true);
    }

    // 2. Try xdg-terminal-exec
    let mut cmd = Command::new("xdg-terminal-exec");
    cmd.arg("-e").arg(program).args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        return Ok(true);
    }

    // 3. Try $TERMINAL
    if let Ok(term) = std::env::var("TERMINAL") {
        if !term.trim().is_empty() {
            let mut cmd = Command::new(&term);
            cmd.arg("-e").arg(program).args(args);
            if let Some(dir) = cwd {
                cmd.current_dir(dir);
            }
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Ok(mut child) = cmd.spawn() {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(true);
            }
        }
    }

    // 4. Try common terminals
    for term in [
        "ghostty",
        "kitty",
        "alacritty",
        "foot",
        "wezterm",
        "gnome-terminal",
        "konsole",
        "x-terminal-emulator",
    ] {
        let mut cmd = Command::new(term);
        match term {
            "kitty" | "foot" => {
                cmd.arg(program).args(args);
            }
            "wezterm" => {
                cmd.arg("start").arg("--").arg(program).args(args);
            }
            "gnome-terminal" => {
                cmd.arg("--").arg(program).args(args);
            }
            _ => {
                cmd.arg("-e").arg(program).args(args);
            }
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = cmd.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(true);
        }
    }

    Ok(false)
}

/// The commands to try for `folder`, the first that starts wins.
fn commands(editor: Editor, folder: &str) -> Vec<(String, Vec<String>)> {
    let run = |program: &str, args: &[&str]| {
        (
            program.to_string(),
            args.iter().map(|a| a.replace("{}", folder)).collect(),
        )
    };
    if cfg!(target_os = "macos") {
        let app = |name: &str| vec![run("open", &["-a", name, "{}"])];
        return match editor {
            Editor::VsCode => app("Visual Studio Code"),
            Editor::Zed => app("Zed"),
            Editor::Cursor => app("Cursor"),
            Editor::Neovim => vec![run("nvim", &["{}"])],
            Editor::Terminal => app("Terminal"),
            Editor::FileManager => Vec::new(),
        };
    }
    if cfg!(target_os = "windows") {
        return match editor {
            Editor::VsCode => vec![run("code.cmd", &["{}"])],
            Editor::Zed => vec![run("zed", &["{}"])],
            Editor::Cursor => vec![run("cursor.cmd", &["{}"])],
            Editor::Neovim => vec![run("nvim", &["{}"])],
            Editor::Terminal => vec![
                run("wt", &["-d", "{}"]),
                run("cmd", &["/c", "start", "cmd", "/k", "cd", "/d", "{}"]),
            ],
            Editor::FileManager => Vec::new(),
        };
    }
    match editor {
        Editor::VsCode => vec![run("code", &["{}"])],
        // Arch packages Zed's CLI as `zeditor`.
        Editor::Zed => vec![run("zed", &["{}"]), run("zeditor", &["{}"])],
        Editor::Cursor => vec![run("cursor", &["{}"])],
        Editor::Neovim => vec![run("nvim", &["{}"])],
        // Every terminal also starts in the folder (`current_dir`); the flag
        // is for those that would otherwise open in home.
        Editor::Terminal => {
            let mut list = Vec::new();
            if let Some(term) = std::env::var("TERMINAL").ok().filter(|t| !t.is_empty()) {
                list.push(run(&term, &[]));
            }
            list.extend([
                run("ghostty", &["--working-directory={}"]),
                run("kitty", &["--directory", "{}"]),
                run("alacritty", &["--working-directory", "{}"]),
                run("foot", &["--working-directory={}"]),
                run("wezterm", &["start", "--cwd", "{}"]),
                run("gnome-terminal", &["--working-directory={}"]),
                run("konsole", &["--workdir", "{}"]),
                run("x-terminal-emulator", &[]),
            ]);
            list
        }
        Editor::FileManager => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_editor_but_the_file_manager_has_a_command() {
        for editor in Editor::ALL {
            assert_eq!(
                commands(editor, "/tmp").is_empty(),
                editor == Editor::FileManager,
                "{editor:?}"
            );
        }
    }

    #[test]
    fn the_choice_round_trips_through_the_config() {
        let json = serde_json::to_string(&Editor::VsCode).unwrap();
        assert_eq!(json, "\"vs-code\"");
        let nvim: Editor = serde_json::from_str("\"neovim\"").unwrap();
        assert_eq!(nvim, Editor::Neovim);
        let back: Editor = serde_json::from_str("\"file-manager\"").unwrap();
        assert_eq!(back, Editor::FileManager);
    }

    #[test]
    fn test_is_text_or_code() {
        assert!(is_text_or_code(Path::new("main.rs")));
        assert!(is_text_or_code(Path::new("src/ui/pane.rs")));
        assert!(is_text_or_code(Path::new("config.toml")));
        assert!(is_text_or_code(Path::new("Dockerfile")));
        assert!(is_text_or_code(Path::new("Makefile")));
        assert!(!is_text_or_code(Path::new("photo.png")));
        assert!(!is_text_or_code(Path::new("video.mp4")));
    }

    #[test]
    fn test_is_terminal_editor() {
        assert!(is_terminal_editor("nvim"));
        assert!(is_terminal_editor("/usr/bin/nvim"));
        assert!(is_terminal_editor("vim"));
        assert!(is_terminal_editor("nano"));
        assert!(is_terminal_editor("hx"));
        assert!(is_terminal_editor("helix"));
        assert!(!is_terminal_editor("code"));
        assert!(!is_terminal_editor("zed"));
        assert!(!is_terminal_editor("cursor"));
    }

    #[test]
    fn test_resolve_terminal_editor() {
        assert_eq!(
            resolve_terminal_editor(Some("nvim"), None),
            Some("nvim".into())
        );
        assert_eq!(resolve_terminal_editor(Some("code"), None), None);
        assert_eq!(resolve_terminal_editor(None, Some(Editor::VsCode)), None);
        assert_eq!(resolve_terminal_editor(None, Some(Editor::Cursor)), None);
        assert_eq!(resolve_terminal_editor(None, Some(Editor::Zed)), None);
        assert_eq!(
            resolve_terminal_editor(None, Some(Editor::Neovim)),
            Some("nvim".into())
        );
        assert_eq!(
            resolve_terminal_editor(None, Some(Editor::Terminal)),
            detect_default_terminal_editor().or_else(|| Some("nvim".into()))
        );
    }
}
