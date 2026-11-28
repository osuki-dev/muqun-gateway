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

fn surface_label(slot: PluginSlot, language: Language) -> &'static str {
    match slot {
        PluginSlot::Page => tr_ref(language, "Page"),
        PluginSlot::Drawer => tr_ref(language, "Dock"),
        PluginSlot::Tile => tr_ref(language, "Tile"),
        _ => "",
    }
}

fn card(s: &UiState, k: &Kit, plugin: &dyn DesktopPlugin, width: f32) -> El {
    let language = s.cfg.general.language;
    let id = plugin.id();
    let enabled = is_enabled(id, &s.cfg.plugins_disabled);
    let always = is_always_on(id);
    let category = plugin.category();
    let toggle_id = id.to_owned();
    let title = plugin.title(language);
    let version = s.plugins.managed_versions.get(id);

    let mut sub_parts = vec![category.label(language).to_owned()];
    if let Some(v) = version {
        sub_parts.push(format!("v{v}"));
    } else {
        sub_parts.push(id.to_owned());
    }
    let subtitle_line = sub_parts.join(" · ");

    let icon_badge = el()
        .size(34.0, 34.0)
        .radius(k.radius.md)
        .fill(if enabled {
            k.c.bg_accent.alpha(0.11)
        } else {
            k.c.layer_02
        })
        .border(
            1.0,
            if enabled {
                k.c.bg_accent.alpha(0.28)
            } else {
                k.c.border_weak
            },
        )
        .center()
        .child(plugin.render_icon(
            s,
            17.0,
            if enabled {
                k.c.text_accent
            } else {
                k.c.icon_muted
            },
        ));

    let header = row()
        .w_full()
        .min_w(0.0)
        .gap(10.0)
        .center_y()
        .child(icon_badge)
        .child(
            col()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(2.0)
                .child(
                    text(&title)
                        .font(k.ty.ui_medium)
                        .color(if enabled {
                            k.c.text_base
                        } else {
                            k.c.text_muted
                        })
                        .single_line()
                        .truncate()
                        .help_when_truncated(&title),
                )
                .child(
                    text(subtitle_line)
                        .font(k.ty.small)
                        .color(k.c.text_faint)
                        .single_line()
                        .truncate(),
                ),
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
        ));

    let desc_text = plugin
        .description(language)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_default();

    let mut tags = row().gap(4.0).center_y().flex_wrap().grow(1.0).min_w(0.0);
    let kw = plugin.keywords();
    if !kw.is_empty() {
        for (index, keyword) in kw.iter().take(2).enumerate() {
            tags = tags.child(
                el().key(format!("plugin-keyword-{id}-{index}"))
                    .px(6.0)
                    .py(2.0)
                    .radius(k.radius.sm)
                    .fill(k.c.layer_02)
                    .child(
                        text(keyword)
                            .font(k.ty.small)
                            .color(k.c.text_muted)
                            .single_line()
                            .truncate(),
                    ),
            );
        }
    } else {
        for slot in [PluginSlot::Page, PluginSlot::Drawer, PluginSlot::Tile] {
            if plugin.supports_surface(slot) {
                let lbl = surface_label(slot, language);
                if !lbl.is_empty() {
                    tags = tags.child(
                        el().px(6.0)
                            .py(2.0)
                            .radius(k.radius.sm)
                            .fill(k.c.layer_02)
                            .child(
                                text(lbl)
                                    .font(k.ty.small)
                                    .color(k.c.text_faint)
                                    .single_line(),
                            ),
                    );
                }
            }
        }
    }

    let mut actions = row().gap(4.0).center_y();
    if let Some(repository) = plugin.repository() {
        let url = repository.source_url();
        let help = repository
            .directory
            .as_ref()
            .map_or_else(|| url.clone(), |directory| format!("{url}\n{directory}"));
        actions = actions.child(
            super::super::button(tr_ref(language, "Source"))
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
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
                    .control_size(ControlSize::Small)
                    .disabled(s.plugin_packages.busy())
                    .on_press(move |s: &mut UiState| s.change_plugin_package(operation.clone()))
                    .build(k)
                    .key(format!("plugin-{}-{id}", label.to_lowercase())),
            );
        }
    }
    if let Some(slot) = default_surface(plugin) {
        let open_id = id.to_owned();
        actions = actions.child(
            super::super::button(tr_ref(language, "Open"))
                .variant(ButtonVariant::Neutral)
                .control_size(ControlSize::Small)
                .disabled(!enabled)
                .on_press(move |s: &mut UiState| s.switch_plugin_surface(&open_id, slot))
                .build(k)
                .key(format!("plugin-open-{id}")),
        );
    }

    let mut body = col()
        .key(format!("plugin-{id}"))
        .w(width)
        .min_w(0.0)
        .px(14.0)
        .py(13.0)
        .gap(10.0)
        .radius(k.radius.lg)
        .fill(if enabled {
            k.c.layer_01
        } else {
            k.c.layer_01.alpha(0.55)
        })
        .border(
            1.0,
            if enabled {
                k.c.border_weak
            } else {
                k.c.border_weak.alpha(0.6)
            },
        )
        .child(header);

    if !desc_text.is_empty() {
        body = body.child(
            text(desc_text)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .w_full()
                .min_h(32.0)
                .wrap(),
        );
    }
    if let Some(progress) = super::progress::package_progress(s, k, id) {
        body = body.child(progress);
    }
    body.child(el().grow(1.0))
        .child(el().w_full().h(1.0).fill(k.c.border_weak.alpha(0.55)))
        .child(
            row()
                .w_full()
                .min_w(0.0)
                .gap(8.0)
                .center_y()
                .child(tags)
                .child(actions),
        )
}

fn section_grid(
    s: &UiState,
    k: &Kit,
    key: &str,
    items: &[&std::sync::Arc<dyn DesktopPlugin>],
    columns: usize,
    card_width: f32,
) -> El {
    let mut grid = col().key(key).w_full().min_w(0.0).gap(GAP);
    for (index, line) in items.chunks(columns).enumerate() {
        grid = grid.child(
            row()
                .key(format!("{key}-row-{index}"))
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
    grid
}

pub(super) fn catalog(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let width = rune::memo::layout_with("plugins-grid", |rect| rect[2])
        .unwrap_or(super::super::INNER_W.get())
        .max(0.0);
    let columns = if width >= 1060.0 {
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
    let search_matched: Vec<_> = plugins
        .iter()
        .filter(|plugin| matches_search(plugin.as_ref(), language, &s.plugin_query.text))
        .collect();
    let visible: Vec<_> = search_matched
        .iter()
        .copied()
        .filter(|plugin| {
            s.plugin_category
                .is_none_or(|category| plugin.category() == category)
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

    let mut filters = row().gap(4.0).flex_wrap().center_y();
    for category in std::iter::once(None).chain(PluginCategory::ALL.into_iter().map(Some)) {
        let selected = s.plugin_category == category;
        let count = search_matched
            .iter()
            .filter(|p| category.is_none_or(|c| p.category() == c))
            .count();
        let base_label = category.map_or(tr_ref(language, "All"), |c| c.label(language));
        let label = if count > 0 {
            format!("{base_label} · {count}")
        } else {
            base_label.to_owned()
        };
        filters = filters.child(
            super::super::button(&label)
                .variant(if selected {
                    ButtonVariant::Neutral
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

    // Unified single-row filter & search toolbar.
    let toolbar = row()
        .w_full()
        .min_w(0.0)
        .gap(10.0)
        .center_y()
        .flex_wrap()
        .child(
            el().grow(1.0)
                .min_w(200.0)
                .max_w(320.0)
                .child(search.build(k).w_full().key("plugin-search")),
        )
        .child(filters);

    let mut content = col().key("plugins-grid").w_full().min_w(0.0).gap(16.0);
    if visible.is_empty() {
        content = content.child(super::super::empty(
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
    } else if s.plugin_category.is_none() {
        let extensions: Vec<_> = visible
            .iter()
            .copied()
            .filter(|p| {
                matches!(p.category(), PluginCategory::Extensions)
                    || s.plugins.managed_versions.contains_key(p.id())
            })
            .collect();
        let builtins: Vec<_> = visible
            .iter()
            .copied()
            .filter(|p| {
                !matches!(p.category(), PluginCategory::Extensions)
                    && !s.plugins.managed_versions.contains_key(p.id())
            })
            .collect();
        if !extensions.is_empty() {
            content = content
                .child(super::super::group_head(
                    k,
                    tr_ref(language, "Extensions"),
                    Some(
                        text(format!("{}", extensions.len()))
                            .font(k.ty.small)
                            .color(k.c.text_faint),
                    ),
                ))
                .child(section_grid(
                    s,
                    k,
                    "plugins-grid-ext",
                    &extensions,
                    columns,
                    card_width,
                ));
        }
        if !builtins.is_empty() {
            content = content
                .child(super::super::group_head(
                    k,
                    tr_ref(language, "Built-in modules"),
                    Some(
                        text(format!("{}", builtins.len()))
                            .font(k.ty.small)
                            .color(k.c.text_faint),
                    ),
                ))
                .child(section_grid(
                    s,
                    k,
                    "plugins-grid-builtin",
                    &builtins,
                    columns,
                    card_width,
                ));
        }
    } else {
        content = content.child(section_grid(
            s,
            k,
            "plugins-grid-cat",
            &visible,
            columns,
            card_width,
        ));
    }

    let enabled_count = plugins
        .iter()
        .filter(|p| is_enabled(p.id(), &s.cfg.plugins_disabled))
        .count();
    col()
        .key("plugins-list")
        .w_full()
        .min_w(0.0)
        .gap(14.0)
        .child(super::super::group_head(
            k,
            tr_ref(language, "Installed"),
            Some(
                text(format!(
                    "{enabled_count} / {} {}",
                    plugins.len(),
                    tr_ref(language, "Enabled")
                ))
                .font(k.ty.small)
                .color(k.c.text_muted),
            ),
        ))
        .child(toolbar)
        .child(content)
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
