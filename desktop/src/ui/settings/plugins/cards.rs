//! Installed plugin catalog, sized from its own rendered container.

use rune::{col, el, row, text, El};
use rune_kit::{ButtonVariant, ControlSize, Kit, TextInput};

use crate::config::{ConfigEdit, Language};
use crate::plugin::packages::Operation;
use crate::plugin::{is_always_on, is_enabled, DesktopPlugin, PluginCategory, PluginSlot};
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

const GAP: f32 = 14.0;

fn matches_search(plugin: &dyn DesktopPlugin, language: Language, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let mut terms = vec![
        plugin.id().to_owned(),
        plugin.title(language),
        plugin.description(language).unwrap_or_default(),
        plugin.category().id().to_owned(),
        plugin.category().label(language).to_owned(),
    ];
    terms.extend(plugin.keywords().iter().cloned());
    let searchable = terms.join(" ").to_lowercase();
    query
        .split_whitespace()
        .all(|term| searchable.contains(term))
}

fn default_surface(plugin: &dyn DesktopPlugin) -> Option<PluginSlot> {
    [PluginSlot::Page, PluginSlot::Drawer, PluginSlot::Tile]
        .into_iter()
        .find(|slot| plugin.supports_surface(*slot))
}

fn card(s: &UiState, k: &Kit, plugin: &dyn DesktopPlugin, width: f32) -> El {
    let language = s.cfg.general.language;
    let id = plugin.id();
    let enabled = is_enabled(id, &s.cfg.plugins_disabled);
    let always = is_always_on(id);
    let category = plugin.category();
    let toggle_id = id.to_owned();
    let title = plugin.title(language);
    let mut identity = row().w_full().min_w(0.0).gap(6.0).center_y().child(
        text(id.to_owned())
            .font(k.ty.mono)
            .color(k.c.text_faint)
            .grow(1.0)
            .shrink()
            .min_w(0.0)
            .truncate()
            .help_when_truncated(id),
    );
    if let Some(version) = s.plugins.managed_versions.get(id) {
        identity = identity.child(
            text(format!("v{version}"))
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    }
    let mut body = col()
        .key(format!("plugin-{id}"))
        .w(width)
        .min_w(0.0)
        .min_h(230.0)
        .px(16.0)
        .py(16.0)
        .gap(10.0)
        .radius(k.radius.lg)
        .fill(k.c.layer_01)
        .border(1.0, k.c.border_weak)
        .child(
            row()
                .w_full()
                .min_w(0.0)
                .gap(10.0)
                .items_start()
                .child(plugin.render_icon(s, 22.0, k.c.icon_muted))
                .child(
                    text(&title)
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .wrap(),
                )
                .child(super::super::flag_disabled(
                    k,
                    &format!("plugin-{id}"),
                    enabled,
                    &format!(
                        "{}: {}",
                        title,
                        tr_ref(language, if always { "Always on" } else { "Enabled" })
                    ),
                    always,
                    move |s| {
                        s.edit(ConfigEdit::PluginEnabled {
                            plugin: toggle_id.clone(),
                            on: !is_enabled(&toggle_id, &s.cfg.plugins_disabled),
                        });
                    },
                )),
        )
        .child(identity);
    if let Some(description) = plugin
        .description(language)
        .filter(|text| !text.trim().is_empty())
    {
        body = body.child(
            text(description)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .w_full()
                .wrap(),
        );
    }
    let mut metadata = row()
        .w_full()
        .min_w(0.0)
        .gap(4.0)
        .flex_wrap()
        .center_y()
        .child(
            super::super::button(category.label(language))
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.plugin_category = Some(category))
                .build(k)
                .key(format!("plugin-category-{id}")),
        );
    for (index, keyword) in plugin.keywords().iter().take(3).enumerate() {
        metadata = metadata.child(
            col()
                .key(format!("plugin-keyword-{id}-{index}"))
                .max_w((width - 32.0).clamp(0.0, 140.0))
                .px(6.0)
                .py(3.0)
                .radius(k.radius.sm)
                .fill(k.c.text_base.alpha(0.05))
                .child(
                    text(keyword)
                        .font(k.ty.small)
                        .color(k.c.text_muted)
                        .min_w(0.0)
                        .shrink()
                        .truncate()
                        .help_when_truncated(keyword),
                ),
        );
    }
    if plugin.keywords().len() > 3 {
        metadata = metadata.child(
            text(format!("+{}", plugin.keywords().len() - 3))
                .font(k.ty.small)
                .color(k.c.text_faint)
                .help(plugin.keywords()[3..].join(", ")),
        );
    }
    body = body.child(metadata).child(el().grow(1.0));
    let mut actions = row().w_full().min_w(0.0).gap(6.0).center_y().flex_wrap();
    if let Some(slot) = default_surface(plugin) {
        let open_id = id.to_owned();
        actions = actions.child(
            super::super::button(tr_ref(language, "Open"))
                .disabled(!enabled)
                .on_press(move |s: &mut UiState| s.switch_plugin_surface(&open_id, slot))
                .build(k)
                .key(format!("plugin-open-{id}")),
        );
    }
    if let Some(repository) = plugin.repository() {
        let url = repository.source_url();
        let help = repository
            .directory
            .as_ref()
            .map_or_else(|| url.clone(), |directory| format!("{url}\n{directory}"));
        actions = actions.child(
            super::super::button(tr_ref(language, "Source"))
                .variant(ButtonVariant::GhostMuted)
                .help(help)
                .on_press_tx(move |_: &mut UiState, tx| {
                    let _ = tx.open_url(&url);
                })
                .build(k)
                .key(format!("plugin-source-{id}")),
        );
    }
    if s.plugins.managed_versions.contains_key(id) {
        for (label, operation) in [
            ("Update", Operation::Update(id.to_owned())),
            ("Uninstall", Operation::Remove(id.to_owned())),
        ] {
            actions = actions.child(
                super::super::button(tr_ref(language, label))
                    .variant(ButtonVariant::GhostMuted)
                    .disabled(s.plugin_packages.busy())
                    .on_press(move |s: &mut UiState| s.change_plugin_package(operation.clone()))
                    .build(k)
                    .key(format!("plugin-{}-{id}", label.to_lowercase())),
            );
        }
    }
    if let Some(progress) = super::progress::package_progress(s, k, id) {
        body = body.child(progress);
    }
    body.child(actions)
}

pub(super) fn catalog(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let width = rune::memo::layout_with("plugins-grid", |rect| rect[2])
        .unwrap_or(super::super::INNER_W.get())
        .max(0.0);
    let columns = if width >= 1180.0 {
        3
    } else if width >= 620.0 {
        2
    } else {
        1
    };
    let card_width = ((width - GAP * (columns - 1) as f32) / columns as f32).max(0.0);
    let mut plugins = s.plugins.all();
    plugins.sort_by_key(|plugin| {
        (
            !matches!(plugin.category(), PluginCategory::Extensions),
            plugin.title(language).to_lowercase(),
        )
    });
    let visible: Vec<_> = plugins
        .iter()
        .filter(|plugin| {
            s.plugin_category
                .is_none_or(|category| plugin.category() == category)
                && matches_search(plugin.as_ref(), language, &s.plugin_query.text)
        })
        .collect();
    let mut search = TextInput::new(&s.plugin_query)
        .label(tr_ref(language, "Search plugins"))
        .placeholder(tr_ref(language, "Search plugins"))
        .leading_icon(&rune_kit::glyphs::SEARCH)
        .leading_icon_size(14.0)
        .width(None)
        .on_edit(|s: &mut UiState| &mut s.plugin_query);
    if !s.plugin_query.text.is_empty() {
        search = search.clear_button();
    }
    let mut filters = row().w_full().gap(4.0).flex_wrap().center_y();
    for category in std::iter::once(None).chain(PluginCategory::ALL.into_iter().map(Some)) {
        let selected = s.plugin_category == category;
        let label = category.map_or(tr_ref(language, "All"), |category| category.label(language));
        filters = filters.child(
            super::super::button(label)
                .variant(if selected {
                    ButtonVariant::Outline
                } else {
                    ButtonVariant::GhostMuted
                })
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.plugin_category = category)
                .build(k)
                .checked(selected)
                .key(format!(
                    "plugins-filter-{}",
                    category.map_or("all", PluginCategory::id)
                )),
        );
    }
    let mut grid = col().key("plugins-grid").w_full().min_w(0.0).gap(GAP);
    for (index, line) in visible.chunks(columns).enumerate() {
        grid = grid.child(
            row()
                .key(format!("plugins-grid-row-{index}"))
                .w_full()
                .min_w(0.0)
                .gap(GAP)
                .items_stretch()
                .children(
                    line.iter()
                        .map(|plugin| card(s, k, plugin.as_ref(), card_width)),
                ),
        );
    }
    if visible.is_empty() {
        grid = grid.child(super::super::empty(
            k,
            "plugins-empty",
            tr_ref(
                language,
                if plugins.is_empty() {
                    "No plugins"
                } else {
                    "No matching plugins"
                },
            ),
            tr_ref(
                language,
                if plugins.is_empty() {
                    "Built-in and loaded plugins appear here."
                } else {
                    "Try a different search or category."
                },
            ),
        ));
    }
    col()
        .key("plugins-list")
        .w_full()
        .min_w(0.0)
        .gap(14.0)
        .child(super::super::group_head(
            k,
            tr_ref(language, "Installed"),
            Some(
                text(format!("{} / {}", visible.len(), plugins.len()))
                    .font(k.ty.small)
                    .color(k.c.text_muted),
            ),
        ))
        .child(search.build(k).w_full().key("plugin-search"))
        .child(filters)
        .child(grid)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SearchPlugin {
        keywords: Vec<String>,
    }
    impl DesktopPlugin for SearchPlugin {
        fn id(&self) -> &'static str {
            "network-helper"
        }
        fn title(&self, _: Language) -> String {
            "Connection inspector".into()
        }
        fn icon(&self) -> &'static rune::Icon {
            &crate::ui::icons::GRID
        }
        fn description(&self, _: Language) -> Option<String> {
            Some("Inspect local requests".into())
        }
        fn category(&self) -> PluginCategory {
            PluginCategory::Diagnostics
        }
        fn keywords(&self) -> &[String] {
            &self.keywords
        }
        fn render_tile(
            &self,
            _: &crate::plugin::PluginViewContext<'_>,
            _: &mut Vec<El>,
        ) -> Option<El> {
            None
        }
    }

    #[test]
    fn search_matches_metadata_including_keywords_beyond_the_visible_chips() {
        let plugin = SearchPlugin {
            keywords: vec![
                "one".into(),
                "two".into(),
                "three".into(),
                "WebSocket".into(),
            ],
        };
        for query in [
            " CONNECTION ",
            "requests",
            "diagnostics",
            "network-helper",
            "websocket",
            "local WebSocket",
        ] {
            assert!(
                matches_search(&plugin, Language::En, query),
                "query {query:?}"
            );
        }
        assert!(!matches_search(&plugin, Language::En, "unrelated"));
        assert!(!matches_search(&plugin, Language::En, "requests unrelated"));
    }
}
