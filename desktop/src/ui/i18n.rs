//! Multi-language UI catalogs powered by `rune-i18n`.
//!
//! Generated at compile-time by `rune_i18n::codegen::generate` from `desktop/locales/`.
//! Covers all 12 supported locales: `en`, `zh-TW`, `zh-CN`, `ja`, `ko`, `de`, `fr`,
//! `es`, `pt`, `ru`, `vi`, `th`.

use crate::config::Language;

include!(concat!(env!("OUT_DIR"), "/i18n.rs"));

pub static I18N: rune_i18n::I18n = rune_i18n::I18n::new(&CATALOG);

/// Map [`Language`] to its locale index in [`I18N::locales`].
pub fn locale_index(language: Language) -> usize {
    match language {
        Language::En => 0,
        Language::ZhTw => 1,
        Language::ZhCn => 2,
        Language::Ja => 3,
        Language::Ko => 4,
        Language::De => 5,
        Language::Fr => 6,
        Language::Es => 7,
        Language::Pt => 8,
        Language::Ru => 9,
        Language::Vi => 10,
        Language::Th => 11,
    }
}

/// Look up an English UI string in the catalog for `language`.
pub fn lookup(language: Language, key: &str) -> Option<&'static str> {
    let loc = locale_index(language);
    if loc == 0 {
        return None;
    }
    let key_idx = rune_i18n::find(KEYS, key)?;
    let translated = I18N.text_in(loc, rune_i18n::Key::named(KEYS, KEYS[key_idx]));
    if translated.is_empty() {
        None
    } else {
        Some(translated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rune_i18n_locales_match_order() {
        assert_eq!(I18N.locales().len(), 12);
        for (i, &lang) in Language::ALL.iter().enumerate() {
            assert_eq!(locale_index(lang), i);
            assert_eq!(I18N.locales()[i].id(), lang.as_str());
        }
    }

    #[test]
    fn rune_i18n_lookup_works() {
        assert_eq!(lookup(Language::Ja, "Commands"), Some("コマンド"));
        assert_eq!(lookup(Language::De, "Commands"), Some("Befehle"));
        assert_eq!(lookup(Language::Fr, "Commands"), Some("Commandes"));
        assert_eq!(lookup(Language::Ko, "Commands"), Some("명령어"));
        assert!(lookup(Language::ZhCn, "Commands").is_some());
        assert!(lookup(Language::ZhTw, "Commands").is_some());
        assert_eq!(lookup(Language::En, "Commands"), None);
    }

    #[test]
    fn tr_translates_across_all_twelve_languages() {
        use crate::ui::copy::{tr, tr_ref};

        for &lang in Language::ALL {
            let res = tr(lang, "Settings");
            assert!(!res.is_empty(), "empty tr for {:?}", lang);

            let res_ref = tr_ref(lang, "Settings");
            assert!(!res_ref.is_empty(), "empty tr_ref for {:?}", lang);
        }

        assert_eq!(tr_ref(Language::En, "Settings"), "Settings");
        assert_eq!(
            tr_ref(Language::ZhCn, "Settings"),
            lookup(Language::ZhCn, "Settings").unwrap()
        );
        assert_eq!(
            tr_ref(Language::ZhTw, "Settings"),
            lookup(Language::ZhTw, "Settings").unwrap()
        );
        assert_eq!(
            tr_ref(Language::Ja, "Settings"),
            lookup(Language::Ja, "Settings").unwrap()
        );
        assert_eq!(tr_ref(Language::Ko, "Settings"), "설정");
        assert_eq!(tr_ref(Language::De, "Settings"), "Einstellungen");
        assert_eq!(tr_ref(Language::Fr, "Settings"), "Paramètres");
        assert_eq!(tr_ref(Language::Es, "Settings"), "Ajustes");
        assert_eq!(tr_ref(Language::Pt, "Settings"), "Definições");
        assert_eq!(tr_ref(Language::Ru, "Settings"), "Настройки");
        assert_eq!(tr_ref(Language::Th, "Settings"), "การตั้งค่า");
        assert_eq!(tr_ref(Language::Vi, "Settings"), "Cài đặt");
    }
}
