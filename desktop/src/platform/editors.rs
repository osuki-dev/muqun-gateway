//! "Open in …" (architecture §6.0‴ A): a project folder in an editor, a
//! terminal or the file manager, the way each platform starts them.

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
    Terminal,
    FileManager,
}

impl Editor {
    pub const ALL: [Editor; 5] = [
        Self::VsCode,
        Self::Zed,
        Self::Cursor,
        Self::Terminal,
        Self::FileManager,
    ];

    /// The product name (the same in every language) or `None` for the two
    /// the UI words itself (terminal, file manager).
    pub fn product(self) -> Option<&'static str> {
        match self {
            Self::VsCode => Some("VS Code"),
            Self::Zed => Some("Zed"),
            Self::Cursor => Some("Cursor"),
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
            Editor::Terminal => app("Terminal"),
            Editor::FileManager => Vec::new(),
        };
    }
    if cfg!(target_os = "windows") {
        return match editor {
            Editor::VsCode => vec![run("code.cmd", &["{}"])],
            Editor::Zed => vec![run("zed", &["{}"])],
            Editor::Cursor => vec![run("cursor.cmd", &["{}"])],
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
        let back: Editor = serde_json::from_str("\"file-manager\"").unwrap();
        assert_eq!(back, Editor::FileManager);
    }
}
