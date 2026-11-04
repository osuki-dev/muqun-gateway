//! Files and folders dropped on the window: what each becomes.
//!
//! Files are attached, as the paperclip does; a `.muqun-theme` is imported;
//! a folder is not uploaded -- its absolute path goes into the draft, where
//! an `@` mention would put a path.

use std::path::{Path, PathBuf};

/// A drop, sorted by what each path is.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Routed {
    pub themes: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
    pub folders: Vec<PathBuf>,
}

/// Sort `paths` by looking at the disk; anything that is neither a file nor
/// a folder (gone, a socket) is left out.
pub fn route(paths: Vec<PathBuf>) -> Routed {
    let mut routed = Routed::default();
    for path in paths {
        if path.is_dir() {
            routed.folders.push(path);
        } else if path.is_file() {
            if path.extension().and_then(|e| e.to_str()) == Some("muqun-theme") {
                routed.themes.push(path);
            } else {
                routed.files.push(path);
            }
        }
    }
    routed
}

/// The draft and caret once the folders' paths are inserted at `caret`, as
/// the `@` popup inserts a pick: the path as is, then one space; a space
/// first when the caret sits against a word.
pub fn insert_folders(text: &str, caret: usize, folders: &[PathBuf]) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let at = caret.min(chars.len());
    let head: String = chars[..at].iter().collect();
    let tail: String = chars[at..].iter().collect();
    let mut inserted = String::new();
    if chars[..at].last().is_some_and(|c| !c.is_whitespace()) {
        inserted.push(' ');
    }
    let paths: Vec<String> = folders.iter().map(|f| mention(f)).collect();
    inserted.push_str(&paths.join(" "));
    // One space after, unless whitespace already follows (then step over it).
    let spaced = !tail.starts_with([' ', '\t', '\n']);
    if spaced {
        inserted.push(' ');
    }
    let caret = at + inserted.chars().count() + usize::from(!spaced);
    (format!("{head}{inserted}{tail}"), caret)
}

fn mention(folder: &Path) -> String {
    folder.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drop_is_sorted_into_files_folders_and_themes() {
        let dir = std::env::temp_dir().join(format!("mq-drop-{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let file = dir.join("a.txt");
        let theme = dir.join("x.muqun-theme");
        std::fs::write(&file, b"a").unwrap();
        std::fs::write(&theme, b"t").unwrap();
        let gone = dir.join("gone");
        let routed = route(vec![file.clone(), sub.clone(), theme.clone(), gone]);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            routed,
            Routed {
                themes: vec![theme],
                files: vec![file],
                folders: vec![sub],
            }
        );
    }

    #[test]
    fn folders_are_inserted_as_paths_at_the_caret() {
        let a = PathBuf::from("/work/a");
        let b = PathBuf::from("/work/b");
        // Empty draft.
        assert_eq!(
            insert_folders("", 0, std::slice::from_ref(&a)),
            ("/work/a ".into(), 8)
        );
        // Against a word: a space first. Text after the caret is kept.
        assert_eq!(
            insert_folders("look in tail", 7, &[a.clone(), b]),
            ("look in /work/a /work/b tail".into(), 24)
        );
        // After whitespace: none.
        assert_eq!(insert_folders("see ", 4, &[a]), ("see /work/a ".into(), 12));
    }
}
