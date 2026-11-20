//! Selection and its menu belong to one rendered document. Replacing the view
//! or closing its pane drops that document and the menu snapshot with it.
use super::{native_view::Node, *};
use rune::{PointerPress, Tx};
use rune_kit::{context_menu, MenuItem};

pub(super) struct Document {
    pub root: Node,
    menu: Arc<Mutex<Option<SelectionMenu>>>,
}
#[derive(Clone)]
struct SelectionMenu {
    point: PointerPress,
    text: Option<String>,
}

impl Document {
    pub(super) fn new(root: Node) -> Self {
        Self {
            root,
            menu: Default::default(),
        }
    }

    pub(super) fn text_menu(
        &self,
        pane: El,
        scope: String,
        ctx: &PluginViewContext<'_>,
        layer: &mut Vec<El>,
    ) -> El {
        let menu = self.menu.clone();
        let menu_id = crate::ui::OpenMenu::PluginTextSelection(scope.clone());
        let open_menu = menu_id.clone();
        let selection_scope = format!("/{scope}/");
        let pane = pane.selection_group(true).on_secondary_press(
            move |s: &mut UiState, point, tx: &mut Tx| {
                s.menu = Some(open_menu.clone());
                let selected = tx.text_selection().filter(|s| {
                    s.texts()
                        .iter()
                        .any(|part| part.is_selected() && part.path().contains(&selection_scope))
                });
                *menu.lock().unwrap() = Some(SelectionMenu {
                    point,
                    text: selected.map(|s| s.text().to_owned()),
                });
            },
        );
        if ctx.s.menu.as_ref() != Some(&menu_id) {
            return pane;
        }
        let Some(snapshot) = self.menu.lock().unwrap().clone() else {
            return pane;
        };
        let language = ctx.s.cfg.general.language;
        let dismiss = self.menu.clone();
        let copy = self.menu.clone();
        let all = self.menu.clone();
        // Focusing a modal menu clears Rune's document selection, so keep
        // this transient menu in the same focus scope as its selected text.
        let mut overlays = vec![rune_kit::scrim(
            format!("{scope}-text-dismiss"),
            move |s: &mut UiState| {
                s.menu = None;
                *dismiss.lock().unwrap() = None;
            },
        )];
        let trigger = context_menu(
            format!("{scope}-text-menu"),
            el().size(0.0, 0.0),
            Some(snapshot.point),
        )
        .standard()
        .min_width(184.0)
        .item(
            MenuItem::new(tr_ref(language, "Copy"))
                .shortcut(crate::platform::keys::label("Primary+C"))
                .disabled(snapshot.text.is_none())
                .on_select_tx(move |s: &mut UiState, tx: &mut Tx| {
                    s.menu = None;
                    *copy.lock().unwrap() = None;
                    if let Some(text) = &snapshot.text {
                        tx.copy(text.to_owned());
                    }
                }),
        )
        .item(
            MenuItem::new(tr_ref(language, "Select all"))
                .shortcut(crate::platform::keys::label("Primary+A"))
                .on_select_tx(move |s: &mut UiState, tx: &mut Tx| {
                    s.menu = None;
                    *all.lock().unwrap() = None;
                    tx.select_all_text(scope.clone());
                }),
        )
        .build(ctx.k, &mut overlays);
        layer.extend(overlays.into_iter().map(El::keeps_text_selection));
        layer.push(trigger);
        pane
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document_views(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
        let k = rune_kit::Kit::neutral(true);
        let look = s.look();
        let plugin = s.plugins.get("notes").unwrap();
        let mut layer = vec![];
        let panes = ["left", "right"]
            .into_iter()
            .fold(row().h_full().w_full(), |r, instance| {
                let ctx = PluginViewContext::new(s, &k, &look, instance, 1.0, PluginSlot::Tile);
                r.child(
                    col()
                        .w(360.0)
                        .h_full()
                        .child(plugin.render_tile(&ctx, &mut layer).unwrap()),
                )
            });
        rune_kit::root(panes, layer)
    }

    #[test]
    fn selected_plugin_text_copies_by_menu_and_keyboard_and_select_all_stays_in_its_pane() {
        let manifest = PluginManifest::from_package(
            serde_json::json!({"name":"notes","version":"1","main":"index.ts","muqun":{}}),
        )
        .unwrap()
        .unwrap();
        let plugin = ExternalPlugin::new(manifest, PathBuf::new());
        let sentence = "Review these exact changes today";
        for (instance, title) in [("left", "Left document"), ("right", "Right document")] {
            let node = serde_json::from_value(serde_json::json!({"type":"column","children":[
                {"type":"heading","text":title},{"type":"markdown","text":sentence},
                {"type":"markdown","text":"Another **paragraph**"}
            ]}))
            .unwrap();
            plugin
                .native_views
                .lock()
                .unwrap()
                .insert(instance.into(), Document::new(node));
        }
        let (ch, _) = crate::model::commands::channel();
        let mut state = UiState::new(ch);
        state.plugins.register(Arc::new(plugin));
        let mut rt = rune::Runtime::new(
            state,
            document_views,
            (720.0, 430.0),
            rune::Env::new(true, 1.0),
        );
        rt.set_system_reduced_motion(true);
        let clipboard = rune::clipboard::memory();
        rt.use_clipboard(&clipboard);
        let settle = |rt: &mut rune::Runtime<UiState>| {
            for _ in 0..20 {
                rt.frame(0.016);
            }
        };
        settle(&mut rt);
        let rect = rt
            .elements()
            .into_iter()
            .find(|e| e.text.as_deref() == Some(sentence) && e.rect[0] < 300.0)
            .unwrap()
            .rect;
        let x = rect[0] + 5.0;
        let y = rect[1] + rect[3] * 0.5;
        rt.pointer_down(x, y);
        rt.frame(0.016);
        rt.pointer_move(x + 76.0, y);
        rt.frame(0.016);
        rt.pointer_up(x + 76.0, y);
        settle(&mut rt);
        let selected = rt.selection().unwrap();
        assert!(!selected.text().is_empty() && selected.text() != sentence);
        let key_copy = rune::KeyEvent {
            key: rune::Key::Command('c'),
            mods: rune::Mods {
                cmd: true,
                ctrl: !crate::platform::is_macos(),
                ..Default::default()
            },
        };
        rt.key(key_copy);
        rt.frame(0.016);
        assert_eq!(
            clipboard.text().as_deref().map(String::as_str),
            Some(selected.text())
        );

        let choose = |rt: &mut rune::Runtime<UiState>, index: usize| {
            rt.secondary_down(x + 20.0, y);
            rt.secondary_up(x + 20.0, y);
            settle(rt);
            let key = format!("native-view-notes-left-text-menu-item{index}");
            let [x, y, w, h] = rt
                .elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some(&key))
                .unwrap()
                .rect;
            rt.pointer_down(x + w / 2.0, y + h / 2.0);
            rt.pointer_up(x + w / 2.0, y + h / 2.0);
            settle(rt);
        };
        clipboard.set_text("sentinel");
        choose(&mut rt, 0);
        assert_eq!(
            clipboard.text().as_deref().map(String::as_str),
            Some(selected.text())
        );
        assert_eq!(rt.selection().as_ref(), Some(&selected));
        choose(&mut rt, 1);
        let all = rt.selection().unwrap();
        assert!(
            all.text().contains("Left document")
                && all.text().contains(sentence)
                && all.text().contains("Another paragraph")
        );
        assert!(!all.text().contains("Right document"));
    }
}
