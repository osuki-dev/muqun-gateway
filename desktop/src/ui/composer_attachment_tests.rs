use super::*;
use crate::model::compose::StagedFile;

fn uploaded() -> StagedFile {
    StagedFile {
        draft_token: None,
        path: "/uploads/photo.png".into(),
        name: "photo.png".into(),
        size: 16,
        image: true,
    }
}

fn state() -> UiState {
    let (tx, _) = crate::model::commands::channel();
    let mut s = UiState::new(tx);
    s.draft = fresh("");
    insert_image_pill(&mut s.draft, 42);
    s.pending_image_uploads.insert(42);
    s
}

#[test]
fn deleting_inline_image_removes_only_its_attachment() {
    let mut s = state();
    finish_staging(&mut s, 42, Ok(uploaded()));
    let mut unrelated = uploaded();
    unrelated.path = "/uploads/other.png".into();
    s.staged.push(unrelated.clone());
    s.draft.set_selection(0, s.draft.text.len());
    s.draft.insert("");
    on_draft_change(&mut s, Slot::Session);
    assert_eq!(s.staged, vec![unrelated]);
}

#[test]
fn deleted_inline_image_does_not_reappear_after_upload() {
    let mut s = state();
    s.draft.set_selection(0, s.draft.text.len());
    s.draft.insert("");
    finish_staging(&mut s, 42, Ok(uploaded()));
    assert!(s.staged.is_empty());
    assert!(s.pending_image_uploads.is_empty());
}

#[test]
fn removing_image_pill_preserves_text_and_other_images() {
    let mut draft = fresh("before ");
    draft.set_caret(draft.text.len());
    insert_image_pill(&mut draft, 42);
    draft.insert("after ");
    insert_image_pill(&mut draft, 43);
    remove_image_pill(&mut draft, 42);
    assert!(!has_image_pill(&draft, 42));
    assert!(has_image_pill(&draft, 43));
    assert!(draft.text.contains("before "));
    assert!(draft.text.contains("after "));
}

#[test]
fn image_upload_finishes_in_its_original_split_pane() {
    let (tx, _) = crate::model::commands::channel();
    let mut s = UiState::new(tx);
    let (_, scene) = crate::ui::scenes::scenes()
        .iter()
        .find(|(name, _)| *name == "split2")
        .unwrap();
    scene(&mut s);
    let original = s.focus;
    insert_image_pill(&mut s.draft, 42);
    s.pending_image_uploads.insert(42);
    s.focus_tile(if original == 0 { 1 } else { 0 });
    finish_staging(&mut s, 42, Ok(uploaded()));
    assert!(!s.staged.iter().any(|file| file.draft_token == Some(42)));
    assert!(s.tiles[original]
        .slot
        .staged
        .iter()
        .any(|file| file.draft_token == Some(42)));
}

#[test]
fn document_upload_waits_for_its_original_home_or_session() {
    for from_home in [false, true] {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.selected = Some("original-session".into());
        s.mode = if from_home {
            super::super::Mode::Home
        } else {
            super::super::Mode::Session
        };
        let id = s.next_token;
        stage_path(&mut s, "/tmp/report.pdf".into());
        s.go(if from_home {
            super::super::Mode::Session
        } else {
            super::super::Mode::Home
        });
        let file = StagedFile {
            image: false,
            name: "report.pdf".into(),
            path: "/uploads/report.pdf".into(),
            ..uploaded()
        };
        finish_staging(&mut s, id, Ok(file.clone()));
        assert!(
            s.staged.is_empty(),
            "the other composer must not receive this file"
        );
        s.go(if from_home {
            super::super::Mode::Home
        } else {
            super::super::Mode::Session
        });
        assert_eq!(s.staged, vec![file]);
        assert!(s.deferred_attachments.is_empty());
    }
}

#[test]
fn document_upload_never_targets_another_computer_even_with_same_session_id() {
    for home in [false, true] {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.cfg.current_computer = "local".into();
        s.selected = Some("same-session".into());
        s.mode = if home {
            super::super::Mode::Home
        } else {
            super::super::Mode::Session
        };
        let id = s.next_token;
        stage_path(&mut s, "/tmp/report.pdf".into());
        s.switch_computer("mini".into());
        s.selected = Some("same-session".into());
        s.mode = if home {
            super::super::Mode::Home
        } else {
            super::super::Mode::Session
        };
        let file = StagedFile {
            image: false,
            name: "report.pdf".into(),
            path: "/uploads/report.pdf".into(),
            ..uploaded()
        };
        finish_staging(&mut s, id, Ok(file.clone()));
        assert!(s.staged.is_empty());
        s.switch_computer("local".into());
        assert_eq!(s.staged, vec![file]);
        assert!(s.deferred_attachments.is_empty());
    }
}
