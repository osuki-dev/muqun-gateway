//! An embedded service may append to the conversation that opened it.
use crate::{config::ConfigEdit, ui::UiState};

#[derive(Clone)]
pub(crate) struct Target {
    computer: String,
    session: String,
}

impl Target {
    pub(crate) fn capture(s: &UiState) -> Option<Self> {
        Some(Self {
            computer: s.current_computer().to_string(),
            session: s.current()?.asid.0.clone(),
        })
    }

    pub(crate) fn append(&self, s: &mut UiState, text: &str) -> Result<(), &'static str> {
        if s.current_computer() != self.computer || s.session_by_id(&self.session).is_none() {
            return Err("The original conversation is unavailable.");
        }
        let key = s.session_draft_key(&self.session);
        if !s.cfg.general.drafts.contains_key(&key)
            && s.cfg.general.drafts.len() >= crate::config::MAX_DRAFTS
        {
            return Err("The saved draft limit has been reached.");
        }
        let active = s.selected.as_ref() == Some(&self.session);
        let editor = if active {
            Some(&mut s.draft)
        } else {
            s.tiles
                .iter_mut()
                .find(|t| t.asid.as_ref() == Some(&self.session))
                .map(|t| &mut t.slot.draft)
        };
        let existing = editor
            .as_ref()
            .map(|edit| edit.text.as_str())
            .or_else(|| s.cfg.general.drafts.get(&key).map(String::as_str))
            .unwrap_or("");
        let updated = append_text(existing, text)?;
        if let Some(editor) = editor {
            editor.text.clone_from(&updated);
            editor.set_caret(updated.len());
        }
        s.edit(ConfigEdit::Draft { key, text: updated });
        Ok(())
    }
}

fn append_text(existing: &str, text: &str) -> Result<String, &'static str> {
    if text.trim().is_empty() || text.len() > 8192 {
        return Err("Enter 1 to 8192 bytes of text.");
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    if existing.len() + separator.len() + text.len() > 256 * 1024 {
        return Err("The conversation draft is full.");
    }
    Ok(format!("{existing}{separator}{text}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn switching_conversations_does_not_redirect_plugin_drafts() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.cfg.current_computer = "local".to_owned();
        s.sessions = ["original", "new"].map(|id| serde_json::from_value(serde_json::json!({
            "asid": id, "agent_id": "codex", "title":id, "status":"idle", "directory":"/project", "updated_ms":1
        })).unwrap()).to_vec();
        s.selected = Some("original".into());
        let origin = Target::capture(&s).unwrap();
        s.selected = Some("new".into());
        s.draft.text = "other draft".into();
        let mut tile = crate::ui::Tile::session("original");
        tile.slot.draft.text = "unsent edits".into();
        s.tiles.push(tile);
        origin.append(&mut s, "annotation").unwrap();
        assert_eq!(s.tiles[0].slot.draft.text, "unsent edits\nannotation");
        assert_eq!(s.cfg.general.drafts["original"], "unsent edits\nannotation");
        assert_eq!(s.draft.text, "other draft");
        s.cfg.current_computer = "mini".into();
        assert!(origin.append(&mut s, "wrong machine").is_err());
        s.cfg.current_computer = "local".into();
        s.sessions.clear();
        assert!(origin.append(&mut s, "removed session").is_err());
        assert_eq!(s.cfg.general.drafts["original"], "unsent edits\nannotation");
    }

    #[test]
    fn appending_retains_existing_text_and_checks_the_combined_byte_limit() {
        assert_eq!(append_text("unsent\n", "  🧪\n").unwrap(), "unsent\n  🧪\n");
        assert!(append_text(&"x".repeat(256 * 1024), "x").is_err());
        assert!(append_text("", &"🧪".repeat(2049)).is_err());
    }
}
