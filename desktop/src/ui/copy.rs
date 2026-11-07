//! Short bilingual UI copy.

use crate::config::Language;

/// Pick English or 简体中文 for the owner's language.
pub fn tr(language: Language, en: &str, zh: &str) -> String {
    match language {
        Language::En => en.to_string(),
        Language::ZhCn => zh.to_string(),
    }
}

pub fn tr_ref(language: Language, en: &'static str, zh: &'static str) -> &'static str {
    match language {
        Language::En => en,
        Language::ZhCn => zh,
    }
}
