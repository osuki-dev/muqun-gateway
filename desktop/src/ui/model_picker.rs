//! Searchable, provider-grouped catalog shared by Home and session composers.
use super::{copy::tr_ref, OpenMenu, UiState};
use rune::{col, icon, row, text, El, ScrollSpec, TextEdit};
use rune_kit::{Kit, Placement, Popover, TextInput};

impl UiState {
    pub(super) fn toggle_composer_menu(&mut self, kind: OpenMenu) {
        let opening = self.menu.as_ref() != Some(&kind);
        if opening && matches!(kind, OpenMenu::Model | OpenMenu::StartModel) {
            self.model_query = TextEdit::new("");
        }
        self.menu = opening.then_some(kind);
    }
}

fn close(s: &mut UiState) {
    s.menu = None;
    s.model_query = TextEdit::new("");
}

fn group(k: &Kit, name: &str) -> El {
    text(name.to_owned())
        .px(8.0)
        .py(4.0)
        .font(k.ty.small_medium)
        .color(k.c.text_faint)
}

fn choice(
    k: &Kit,
    key: String,
    label: &str,
    selected: bool,
    pick: impl Fn(&mut UiState) + 'static,
) -> El {
    let mut entry = row()
        .key(key)
        .w_full()
        .h(32.0)
        .radius(k.radius.md)
        .px(8.0)
        .gap(8.0)
        .center_y()
        .focusable(true)
        .role("menuitemradio")
        .checked(selected)
        .label(label.to_owned())
        .help_when_truncated(label.to_owned())
        .child(
            text(label.to_owned())
                .grow(1.0)
                .min_w(0.0)
                .font(k.ty.ui)
                .color(k.c.text_base)
                .truncate(),
        );
    if selected {
        entry = entry.child(
            icon(&super::parts::glyphs::DONE, 14.0)
                .color(k.c.icon_accent)
                .a11y_hidden(),
        );
    }
    k.interactive(entry).on_press(pick)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn popup(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    key: &str,
    trigger: El,
    open: bool,
    models: &[(String, String, String)],
    current: Option<(&str, &str)>,
    efforts: Option<(&[String], Option<&str>)>,
    on_pick: impl Fn(&mut UiState, String, String) + Clone + 'static,
) -> El {
    let lang = s.cfg.general.language;
    let search_label = tr_ref(lang, "Search models");
    let mut pop = Popover::new(key)
        .placement(Placement::TopStart)
        .trigger(trigger)
        .label(search_label)
        .padding(5.0)
        .open(open)
        .on_close(close);
    if !open {
        return pop.build(k, layer);
    }
    let terms = s.model_query.text.to_lowercase();
    let mut matches = models
        .iter()
        .filter(|(provider, id, name)| {
            if terms.trim().is_empty() {
                return true;
            }
            let searchable = format!("{provider} {id} {name}").to_lowercase();
            terms.split_whitespace().all(|t| searchable.contains(t))
        })
        .peekable();
    // Enter with an empty search keeps the existing choice if it is still available.
    let first = if terms.trim().is_empty() {
        current.and_then(|(p, id)| models.iter().find(|m| m.0 == p && m.1 == id))
    } else {
        None
    }
    .or_else(|| matches.peek().copied())
    .map(|(p, id, _)| (p.clone(), id.clone()));
    let first_key = matches
        .peek()
        .map(|(provider, id, _)| format!("model-option-{provider}-{id}"));
    let select = move |s: &mut UiState, provider: String, id: String| {
        close(s);
        on_pick(s, provider, id);
    };
    let mut body = col()
        .w(266.0)
        .min_h(0.0)
        .shrink()
        .gap(3.0)
        .modal(true, close)
        .focus_return(format!("{key}-trigger"));
    if !models.is_empty() {
        let submit = select.clone();
        body = body.child(
            TextInput::new(&s.model_query)
                .placeholder(search_label)
                .label(search_label)
                .leading_icon(&rune_kit::glyphs::SEARCH)
                .leading_icon_size(14.0)
                .width(Some(266.0))
                .on_edit(|s: &mut UiState| &mut s.model_query)
                .on_key(move |_: &mut UiState, event, tx| {
                    if event.key == rune::Key::Down {
                        if let Some(key) = &first_key {
                            tx.focus(key, true);
                            return true;
                        }
                    }
                    false
                })
                .on_submit(move |s: &mut UiState, _| {
                    if let Some((p, id)) = &first {
                        submit(s, p.clone(), id.clone());
                    }
                })
                .build(k)
                .key("model-filter"),
        );
    }
    let mut list = col()
        .w_full()
        .role("menu")
        .focus_group()
        .label(tr_ref(lang, "Model"));
    let mut last = None;
    for (p, id, name) in matches {
        if last != Some(p) {
            list = list.child(group(k, p));
            last = Some(p);
        }
        let on = current == Some((p.as_str(), id.as_str()));
        let (provider, model, pick) = (p.clone(), id.clone(), select.clone());
        list = list.child(choice(
            k,
            format!("model-option-{p}-{id}"),
            name,
            on,
            move |s| pick(s, provider.clone(), model.clone()),
        ));
    }
    if last.is_none() && !models.is_empty() {
        list = list.child(
            text(tr_ref(lang, "No matching models"))
                .padding(8.0)
                .color(k.c.text_muted)
                .font(k.ty.small),
        );
    }
    if let Some((levels, variant)) = efforts.filter(|(levels, _)| !levels.is_empty()) {
        list = list.child(group(k, tr_ref(lang, "Effort")));
        for level in std::iter::once(None).chain(levels.iter().map(Some)) {
            let label = level
                .map(|v| super::labels::readable_catalog_id(v))
                .unwrap_or_else(|| tr_ref(lang, "Default").to_owned());
            let selected = level.map(String::as_str) == variant;
            let value = level.cloned();
            list = list.child(choice(
                k,
                format!("model-effort-{}", level.map(String::as_str).unwrap_or("")),
                &label,
                selected,
                move |s| super::composer::switch_effort(s, super::Slot::Session, value.clone()),
            ));
        }
    }
    pop = pop.child(
        body.child(
            col()
                .key(format!("model-results-{terms}"))
                .w_full()
                .max_h(334.0)
                .min_h(0.0)
                .shrink()
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(list),
        ),
    );
    pop.build(k, layer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{el, Cx, Env, Key, KeyEvent, Mods, Runtime};
    #[test]
    fn searching_identical_names_by_provider_selects_the_matching_id_and_resets_on_reopen() {
        fn view(s: &UiState, _: &mut Cx<UiState>) -> El {
            let k = Kit::neutral(true);
            let mut layer = Vec::new();
            let models = [
                ("a".into(), "same".into(), "Alpha".into()),
                ("other".into(), "same".into(), "Alpha".into()),
            ];
            let trigger = super::super::composer::model_menu(
                s,
                &k,
                &mut layer,
                OpenMenu::StartModel,
                "home",
                s.menu == Some(OpenMenu::StartModel),
                "Model".into(),
                &models,
                None,
                |s, p, id| {
                    s.start_model = Some(super::super::composer::choose_model(None, None, p, id));
                    close(s)
                },
            );
            rune_kit::root(el().w_full().h_full().center().child(trigger), layer)
        }
        let (commands, _) = crate::model::commands::channel();
        let mut s = UiState::new(commands);
        s.toggle_composer_menu(OpenMenu::StartModel);
        let mut rt = Runtime::new(s, view, (800.0, 600.0), Env::new(true, 1.0));
        for i in 0..20 {
            rt.frame(i as f64 / 60.0);
        }
        rt.key(KeyEvent {
            key: Key::Text("OTHER al".into()),
            mods: Mods::default(),
        });
        rt.frame(0.4);
        assert_eq!(rt.state.model_query.text, "OTHER al");
        assert!(rt.find("model-option-a-same").is_none());
        assert!(rt.find("model-option-other-same").is_some());
        rt.key(KeyEvent {
            key: Key::Enter,
            mods: Mods::default(),
        });
        rt.frame(0.5);
        let model = rt.state.start_model.as_ref().expect("submitted result");
        assert_eq!((&*model.provider_id, &*model.model_id), ("other", "same"));
        rt.state.toggle_composer_menu(OpenMenu::StartModel);
        rt.invalidate();
        rt.frame(0.6);
        assert!(rt.find("model-option-a-same").is_some());
    }
}
