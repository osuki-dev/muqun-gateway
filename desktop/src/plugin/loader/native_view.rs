use super::*;
use base64::Engine;
use rune_kit::{Badge, BadgeVariant, ScrollBar, ScrollBarStyle};
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Deserialize)]
pub(super) struct SelectOption {
    label: String,
    action: String,
    #[serde(default)]
    selected: bool,
}

#[derive(Deserialize)]
pub(super) struct Response {
    pub version: u16,
    pub view: Node,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(super) enum Node {
    Column {
        children: Vec<Node>,
        #[serde(flatten)]
        layout: Layout,
    },
    Row {
        children: Vec<Node>,
        #[serde(flatten)]
        layout: Layout,
    },
    Select {
        key: String,
        label: String,
        options: Vec<SelectOption>,
    },
    ScrollArea {
        key: String,
        axis: Axis,
        #[serde(default = "scroll_height")]
        height: Height,
        children: Vec<Node>,
    },
    Card {
        key: String,
        label: String,
        action: Option<String>,
        children: Vec<Node>,
        #[serde(default)]
        variant: CardVariant,
        #[serde(default)]
        tone: Tone,
    },
    Image {
        src: String,
        alt: String,
        width: Option<f32>,
        height: Option<f32>,
        #[serde(skip)]
        media: OnceLock<native_media::Picture>,
        #[serde(skip)]
        decoded: OnceLock<rune::Image>,
    },
    Avatar {
        name: String,
        image: Option<String>,
        #[serde(default = "avatar_size")]
        size: f32,
        #[serde(skip)]
        decoded: OnceLock<Option<rune::Image>>,
    },
    Badge {
        text: String,
        #[serde(default)]
        tone: Tone,
    },
    Text {
        text: String,
        #[serde(flatten)]
        presentation: TextPresentation,
    },
    Heading {
        text: String,
    },
    Markdown {
        text: String,
        anchor: Option<String>,
    },
    Button {
        label: String,
        action: String,
        #[serde(flatten)]
        presentation: ButtonPresentation,
    },
    WebView {
        entry: String,
    },
    TerminalButton {
        label: String,
        entry: String,
    },
    Form {
        action: String,
        #[serde(rename = "submitLabel")]
        submit_label: String,
        children: Vec<Field>,
        #[serde(default)]
        inline: bool,
        #[serde(default = "yes", rename = "showLabels")]
        show_labels: bool,
    },
}

#[derive(Default, Deserialize)]
pub(super) struct Layout {
    key: Option<String>,
    width: Option<f32>,
    height: Option<Height>,
    gap: Option<f32>,
    wrap: Option<bool>,
    align: Option<Alignment>,
    justify: Option<Alignment>,
    surface: Option<Surface>,
    tone: Option<Tone>,
    #[serde(rename = "paddingX")]
    padding_x: Option<f32>,
    #[serde(rename = "paddingY")]
    padding_y: Option<f32>,
    #[serde(default)]
    grow: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Surface {
    Subtle,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum CardVariant {
    #[default]
    Raised,
    Elevated,
    Ghost,
}
#[derive(Default, Deserialize)]
pub(super) struct TextPresentation {
    font: Option<Font>,
    tone: Option<Tone>,
    width: Option<f32>,
    height: Option<f32>,
    rotate: Option<f32>,
    #[serde(default)]
    truncate: bool,
    #[serde(default)]
    grow: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Font {
    Body,
    Ui,
    Small,
}
#[derive(Default, Deserialize)]
pub(super) struct ButtonPresentation {
    variant: Option<ButtonStyle>,
    size: Option<ButtonSize>,
    width: Option<f32>,
    height: Option<f32>,
    #[serde(rename = "ariaLabel")]
    aria_label: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ButtonStyle {
    Outline,
    Ghost,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ButtonSize {
    Regular,
    Small,
}
fn yes() -> bool {
    true
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Alignment {
    Start,
    Center,
    End,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Tone {
    #[default]
    Muted,
    Accent,
    Info,
    Success,
    Warning,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(untagged)]
pub(super) enum Height {
    Points(f32),
    Fill(Fill),
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Fill {
    Fill,
}

impl Height {
    fn apply(self, layout: El, in_row: bool) -> El {
        match self {
            Self::Points(value) => layout.h(value),
            Self::Fill(_) if in_row => layout.h_full(),
            Self::Fill(_) => layout.grow(1.0),
        }
        .min_h(0.0)
    }
    fn valid(self, min: f32, max: f32) -> bool {
        match self {
            Self::Points(value) => bounded(value, min, max),
            Self::Fill(_) => true,
        }
    }
}

fn scroll_height() -> Height {
    Height::Points(520.0)
}
fn avatar_size() -> f32 {
    25.0
}

fn tone_color(k: &rune_kit::Kit, tone: Tone) -> rune::Color {
    match tone {
        Tone::Muted => k.c.text_muted,
        Tone::Accent => k.c.text_accent,
        Tone::Info => k.c.info.icon,
        Tone::Success => k.c.success.icon,
        Tone::Warning => k.c.warning.icon,
    }
}

fn invalid(detail: &str) -> ScriptFailure {
    ScriptFailure {
        reason: Outcome::InvalidResponse,
        detail: detail.into(),
    }
}

fn valid_anchor(anchor: &str) -> bool {
    !anchor.is_empty()
        && anchor.len() <= 128
        && anchor
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn bounded(value: f32, minimum: f32, maximum: f32) -> bool {
    value.is_finite() && value >= minimum && value <= maximum
}

fn avatar_bytes(source: &str) -> Option<Vec<u8>> {
    if source.len() > 32_768 {
        return None;
    }
    let (prefix, encoded) = source.split_once(',')?;
    if !matches!(
        prefix,
        "data:image/png;base64" | "data:image/jpeg;base64" | "data:image/webp;base64"
    ) {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let format = image::guess_format(&bytes).ok()?;
    if !matches!(
        format,
        image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP
    ) {
        return None;
    }
    // Inspect headers before the asynchronous host decode; thumbnails cannot
    // request a large allocation despite a small compressed representation.
    let dimensions = image::ImageReader::with_format(std::io::Cursor::new(&bytes), format)
        .into_dimensions()
        .ok()?;
    (dimensions.0 <= 512 && dimensions.1 <= 512 && dimensions.0 > 0 && dimensions.1 > 0)
        .then_some(bytes)
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub(super) enum Field {
    #[serde(rename = "textfield")]
    Text {
        name: String,
        label: String,
        #[serde(default)]
        value: String,
        #[serde(default)]
        multiline: bool,
        #[serde(default)]
        placeholder: String,
    },
}

impl Node {
    pub(super) fn fields(
        &self,
        images: &native_media::Images,
    ) -> Result<HashMap<(String, String), rune::TextEdit>, ScriptFailure> {
        let mut fields = HashMap::new();
        let mut nodes = vec![self];
        let mut keys = std::collections::HashSet::new();
        let mut anchors = std::collections::HashSet::new();
        let mut count = 0;
        while let Some(node) = nodes.pop() {
            count += 1;
            if count > 4096 {
                return Err(invalid("Native view exceeds the node limit"));
            }
            let key = match node {
                Self::Column { layout, .. } | Self::Row { layout, .. } => {
                    if layout
                        .width
                        .is_some_and(|value| !bounded(value, 1.0, 4096.0))
                        || layout.height.is_some_and(|value| !value.valid(1.0, 4096.0))
                        || layout.gap.is_some_and(|value| !bounded(value, 0.0, 64.0))
                        || layout
                            .padding_x
                            .is_some_and(|value| !bounded(value, 0.0, 64.0))
                        || layout
                            .padding_y
                            .is_some_and(|value| !bounded(value, 0.0, 64.0))
                    {
                        return Err(invalid("Native layout dimensions exceed their limits"));
                    }
                    layout.key.as_deref()
                }
                Self::Select { key, options, .. } => {
                    if options.iter().filter(|o| o.selected).count() > 1 {
                        return Err(invalid("A select can only have one current option"));
                    }
                    Some(key.as_str())
                }
                Self::ScrollArea { key, height, .. } => {
                    if !height.valid(32.0, 2048.0) {
                        return Err(invalid("Native scroll height exceeds its limit"));
                    }
                    Some(key.as_str())
                }
                Self::Card { key, .. } => Some(key.as_str()),
                Self::Markdown {
                    anchor: Some(anchor),
                    ..
                } => {
                    if !valid_anchor(anchor) || !anchors.insert(anchor) {
                        return Err(invalid("Markdown anchors require distinct ASCII identifiers of up to 128 characters"));
                    }
                    None
                }
                Self::Image {
                    src,
                    width,
                    height,
                    media,
                    ..
                } => {
                    if width.is_some_and(|w| !bounded(w, 1.0, 4096.0))
                        || height.is_some_and(|h| !bounded(h, 1.0, 4096.0))
                    {
                        return Err(invalid("Native image dimensions exceed their limits"));
                    }
                    if let Some(picture) = images.get(src) {
                        let _ = media.set(picture.clone());
                    }
                    None
                }
                Self::Avatar { size, .. } => {
                    if !bounded(*size, 16.0, 96.0) {
                        return Err(invalid("Native avatar size exceeds its limit"));
                    }
                    None
                }
                Self::Text { presentation, .. } => {
                    if presentation
                        .width
                        .is_some_and(|value| !bounded(value, 1.0, 4096.0))
                        || presentation
                            .height
                            .is_some_and(|value| !bounded(value, 1.0, 4096.0))
                        || presentation
                            .rotate
                            .is_some_and(|value| !bounded(value, -360.0, 360.0))
                    {
                        return Err(invalid(
                            "Native text dimensions or rotation exceed their limits",
                        ));
                    }
                    None
                }
                Self::Button { presentation, .. } => {
                    if presentation
                        .width
                        .is_some_and(|value| !bounded(value, 1.0, 4096.0))
                        || presentation
                            .height
                            .is_some_and(|value| !bounded(value, 1.0, 4096.0))
                    {
                        return Err(invalid("Native button dimensions exceed their limits"));
                    }
                    None
                }
                _ => None,
            };
            if let Some(key) = key {
                if key.is_empty() || key.len() > 128 || !keys.insert(key) {
                    return Err(invalid(
                        "Native view keys require distinct, nonempty bounded names",
                    ));
                }
            }
            match node {
                Self::Column { children, .. }
                | Self::Row { children, .. }
                | Self::ScrollArea { children, .. }
                | Self::Card { children, .. } => nodes.extend(children),
                Self::WebView { .. } if !std::ptr::eq(self, node) => {
                    return Err(ScriptFailure {
                        reason: Outcome::InvalidResponse,
                        detail: "WebView must be the root of its plugin view".into(),
                    });
                }
                Self::Form {
                    action, children, ..
                } => {
                    for Field::Text { name, value, .. } in children {
                        if name.is_empty()
                            || fields
                                .insert(
                                    (action.clone(), name.clone()),
                                    rune::TextEdit::new(value.clone()),
                                )
                                .is_some()
                        {
                            return Err(ScriptFailure {
                                reason: Outcome::InvalidResponse,
                                detail: "Form fields require distinct, nonempty names".into(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(fields)
    }

    fn fills_height(&self) -> bool {
        match self {
            Self::Column { layout, .. } | Self::Row { layout, .. } => {
                matches!(layout.height, Some(Height::Fill(_)))
            }
            Self::ScrollArea { height, .. } => matches!(height, Height::Fill(_)),
            _ => false,
        }
    }

    #[cfg(test)]
    fn render(&self, ctx: &PluginViewContext<'_>, id: &'static str) -> El {
        self.render_child(ctx, id, false, &mut vec![])
    }
    fn render_child(
        &self,
        ctx: &PluginViewContext<'_>,
        id: &'static str,
        in_row: bool,
        layer: &mut Vec<El>,
    ) -> El {
        let k = ctx.k;
        match self {
            Self::Column { children, layout } | Self::Row { children, layout } => {
                let mut container = if matches!(self, Self::Row { .. }) {
                    let row = row().center_y();
                    if layout.wrap.unwrap_or(true) {
                        row.flex_wrap()
                    } else {
                        row
                    }
                } else {
                    col()
                };
                container = match layout.align {
                    Some(Alignment::Start) => container.items_start(),
                    Some(Alignment::Center) => container.items_center(),
                    Some(Alignment::End) => container.items_end(),
                    None => container,
                };
                container = match layout.justify {
                    Some(Alignment::Start) => container.justify_start(),
                    Some(Alignment::Center) => container.justify_center(),
                    Some(Alignment::End) => container.justify_end(),
                    None => container,
                };
                container = if let Some(width) = layout.width {
                    container.w(width).min_w(width)
                } else {
                    container.w_full().min_w(0.0)
                };
                if let Some(height) = layout.height {
                    container = height.apply(container, in_row);
                }
                if layout.grow {
                    container = container.grow(1.0).min_w(0.0).min_h(0.0);
                }
                if let Some(padding) = layout.padding_x {
                    container = container.px(padding);
                }
                if let Some(padding) = layout.padding_y {
                    container = container.py(padding);
                }
                if layout.surface.is_some() {
                    let color = tone_color(k, layout.tone.unwrap_or_default());
                    container = container
                        .fill(k.alpha(k.c.layer_01, if k.dark { 0.20 } else { 0.16 }))
                        .radius(k.radius.md)
                        .clip_rounded(k.radius.md)
                        .border(0.5, k.alpha(color, if k.dark { 0.16 } else { 0.10 }));
                }
                if let Some(key) = &layout.key {
                    container = container.key(format!("plugin-native-{id}-{}-{key}", ctx.tile_id));
                }
                children
                    .iter()
                    .fold(container.gap(layout.gap.unwrap_or(8.0)), |layout, node| {
                        layout.child(node.render_child(
                            ctx,
                            id,
                            matches!(self, Self::Row { .. }),
                            layer,
                        ))
                    })
            }
            Self::Select {
                key,
                label,
                options,
            } => {
                use crate::ui::OpenMenu;
                let owner = ctx.tile_id;
                let key = format!("plugin-select-{id}-{owner}-{key}");
                let state = OpenMenu::PluginSelect(key.clone());
                let open = ctx.s.menu.as_ref() == Some(&state);
                let current = options.iter().find(|option| option.selected);
                let trigger = rune_kit::Select::new::<&str>(format!("{key}-field"), &[])
                    .label(label.clone())
                    .placeholder(label.clone())
                    .current(current.map(|option| option.label.as_str()))
                    .disabled(options.is_empty())
                    .open(open)
                    .on_toggle(move |s: &mut UiState| s.menu = (!open).then(|| state.clone()))
                    .trigger(k);
                let mut menu = rune_kit::Menu::new(key)
                    .trigger(trigger)
                    .min_width(180.0)
                    .open(open)
                    .on_close(|s: &mut UiState| s.menu = None);
                if open {
                    for option in options {
                        let action = option.action.clone();
                        let instance = owner.to_owned();
                        menu = menu.item(
                            rune_kit::MenuItem::new(option.label.clone())
                                .checked(option.selected)
                                .on_select(move |s: &mut UiState| {
                                    s.menu = None;
                                    if let Some(plugin) =
                                        s.plugins.get_enabled(id, &s.cfg.plugins_disabled)
                                    {
                                        plugin.on_view_action(s, &instance, &action);
                                    }
                                }),
                        );
                    }
                }
                menu.build(k, layer)
            }
            Self::ScrollArea {
                key,
                axis,
                height,
                children,
            } => {
                let body = children
                    .iter()
                    .fold(col().min_w(0.0).gap(8.0), |body, node| {
                        body.child(node.render_child(ctx, id, false, layer))
                    });
                let body = if matches!((axis, height), (Axis::Horizontal, Height::Fill(_))) {
                    body.h_full()
                } else {
                    body
                };
                let viewport = col()
                    .key(format!("plugin-native-{id}-{}-{key}", ctx.tile_id))
                    .w_full()
                    .min_w(0.0)
                    .child(body);
                let viewport = height.apply(viewport, in_row);
                match axis {
                    Axis::Horizontal => viewport
                        .scroll_x()
                        .scroll_bar_styled(ScrollBarStyle::from_kit(k).horizontal()),
                    Axis::Vertical => viewport
                        .scroll(rune::ScrollSpec {
                            stick_bottom: false,
                        })
                        .scroll_bar(k),
                }
            }
            Self::Card {
                key,
                label,
                action,
                children,
                variant,
                tone,
            } => {
                let color = tone_color(k, *tone);
                let surface = children.iter().fold(
                    if matches!(variant, CardVariant::Ghost) {
                        col().w_full().min_w(0.0).gap(0.0)
                    } else {
                        col()
                            .w_full()
                            .min_w(0.0)
                            .padding(12.0)
                            .gap(8.0)
                            .fill(k.c.surface_raised_base)
                            .radius(k.radius.md)
                            .border(0.5, k.c.border_muted)
                    },
                    |body, node| body.child(node.render_child(ctx, id, false, layer)),
                );
                let surface = if matches!(variant, CardVariant::Elevated) {
                    ctx.look.accent_rim(
                        k,
                        surface.box_shadow(rune::Shadow {
                            x: 0.0,
                            y: 3.0,
                            blur: 10.0,
                            spread: -3.0,
                            color: k.c.black.alpha(if k.dark { 0.24 } else { 0.07 }),
                            inset: false,
                        }),
                        color,
                    )
                } else {
                    surface
                };
                let surface = if let Some(action) = action {
                    let action = action.clone();
                    let instance = ctx.tile_id.to_owned();
                    (if matches!(variant, CardVariant::Raised) {
                        k.interactive(surface)
                    } else {
                        k.interactive_with(surface, k.alpha(color, 0.025), k.alpha(color, 0.08))
                    })
                    .role("button")
                    .label(label)
                    .focusable(true)
                    .on_press(move |s: &mut UiState| {
                        if let Some(plugin) = s.plugins.get_enabled(id, &s.cfg.plugins_disabled) {
                            plugin.on_view_action(s, &instance, &action);
                        }
                    })
                } else {
                    surface.label(label)
                };
                surface.key(format!("plugin-native-{id}-{}-{key}", ctx.tile_id))
            }
            Self::Image {
                alt,
                width,
                height,
                media,
                decoded,
                ..
            } => {
                let Some((picture, cache)) = media.get().zip(ctx.s.images.as_ref()) else {
                    return text(alt).color(k.c.text_muted).wrap();
                };
                let w = width.unwrap_or(picture.width as f32).min(1200.0);
                let h = height
                    .unwrap_or(w * picture.height as f32 / picture.width as f32)
                    .min(1200.0);
                let tile = el()
                    .w_full()
                    .max_w(w)
                    .min_w(0.0)
                    .h(h)
                    .role("button")
                    .focusable(true)
                    .cursor(rune::Cursor::Pointer)
                    .label(alt);
                let bytes = picture.bytes.len() as u64;
                let picture = decoded
                    .get_or_init(|| cache.load(picture.bytes.clone()))
                    .clone();
                if picture.error().is_some() {
                    return text(alt).color(k.c.text_muted).wrap();
                }
                let name = if alt.trim().is_empty() {
                    tr_ref(ctx.s.cfg.general.language, "Image").to_owned()
                } else {
                    alt.clone()
                };
                tile.pixels_fit(picture.pixels(), rune::SurfaceFit::Fit)
                    .on_press(move |s: &mut UiState| {
                        s.open_plugin_image(name.clone(), picture.clone(), bytes)
                    })
            }
            Self::Avatar {
                name,
                image,
                size,
                decoded,
            } => {
                let slot = row()
                    .size(*size, *size)
                    .min_w(*size)
                    .center()
                    .radius(*size / 2.0)
                    .clip_rounded(*size / 2.0)
                    .fill(k.c.layer_02)
                    .role("image")
                    .label(name);
                if let Some(cache) = &ctx.s.images {
                    if let Some(picture) = decoded.get_or_init(|| {
                        image
                            .as_deref()
                            .and_then(avatar_bytes)
                            .map(|bytes| cache.load(bytes))
                    }) {
                        if picture.is_ready() && picture.error().is_none() {
                            return slot.pixels_fit(picture.pixels(), rune::SurfaceFit::Fill);
                        }
                    }
                }
                slot.child(
                    text(name.trim().chars().next().unwrap_or('·').to_string())
                        .font(k.ty.small_medium)
                        .color(k.c.text_muted),
                )
            }
            Self::Badge { text, tone } => {
                let color = tone_color(k, *tone);
                Badge::new(text.clone())
                    .variant(BadgeVariant::Subtle)
                    .build(k)
                    .fill(k.alpha(color, 0.12))
            }
            Self::Text {
                text: value,
                presentation,
            } => {
                let mut label = text(value.clone())
                    .selectable(true)
                    .color(
                        presentation
                            .tone
                            .map_or(k.c.text_base, |tone| tone_color(k, tone)),
                    )
                    .wrap();
                if let Some(font) = &presentation.font {
                    label = label.font(match font {
                        Font::Body => k.ty.body_medium,
                        Font::Ui => k.ty.ui_medium,
                        Font::Small => k.ty.small_medium,
                    });
                }
                if let Some(width) = presentation.width {
                    label = label.w(width).min_w(width);
                }
                if let Some(height) = presentation.height {
                    label = label.h(height);
                }
                if presentation.grow {
                    label = label.grow(1.0).shrink().min_w(0.0);
                }
                if presentation.truncate {
                    label = label.truncate();
                    // Truncation permits shrinking; an explicitly sized rotated
                    // label still needs its unrotated width for text layout.
                    if let Some(width) = presentation.width {
                        label = label.min_w(width);
                    }
                }
                if let Some(degrees) = presentation.rotate {
                    label = label.rotate(degrees.to_radians());
                }
                label
            }
            Self::Heading { text: value } => text(value.clone())
                .selectable(true)
                .font(k.ty.body_medium)
                .color(k.c.text_accent)
                .wrap(),
            Self::Markdown { text, anchor } => {
                let prefix = format!(
                    "plugin-anchor:{}:{}:",
                    hex::encode(id),
                    hex::encode(ctx.tile_id)
                );
                let key = anchor
                    .as_ref()
                    .map(|fragment| format!("{prefix}{fragment}"));
                let block = Markdown::new(text.clone())
                    .selectable(true)
                    .style(crate::ui::parts::message_style(k))
                    .on_link_tx(move |s: &mut UiState, url: &str, tx| {
                        if let Some(fragment) = url.strip_prefix('#') {
                            if valid_anchor(fragment) {
                                tx.scroll_key_to(format!("{prefix}{fragment}"), 0.0);
                            }
                        } else {
                            crate::ui::open_markdown_link(s, url, tx);
                        }
                    })
                    .build(k)
                    .selection_group(false);
                if let Some(key) = key {
                    block.key(key)
                } else {
                    block
                }
            }
            Self::Form {
                action,
                submit_label,
                children,
                inline,
                show_labels,
            } => {
                let owner = (id.to_owned(), ctx.tile_id.to_owned());
                let mut form = if *inline { row().center_y() } else { col() }
                    .w_full()
                    .min_w(0.0)
                    .gap(8.0);
                for Field::Text {
                    name,
                    label,
                    value,
                    multiline,
                    placeholder,
                } in children
                {
                    let owner = owner.clone();
                    let field = (action.clone(), name.clone());
                    let initial = value.clone();
                    let edit = ctx
                        .s
                        .plugin_fields
                        .get(&owner)
                        .and_then(|m| m.get(&field))
                        .cloned()
                        .unwrap_or_else(|| rune::TextEdit::new(initial.clone()));
                    let key = format!("plugin-input-{owner:?}-{field:?}");
                    let input = if *multiline {
                        rune_kit::TextArea::new(&edit)
                            .label(label.clone())
                            .placeholder(placeholder.clone())
                            .auto_grow(3, 12)
                            .on_edit(move |s: &mut UiState| {
                                s.plugin_fields
                                    .entry(owner.clone())
                                    .or_default()
                                    .entry(field.clone())
                                    .or_insert_with(|| rune::TextEdit::new(initial.clone()))
                            })
                            .build(k)
                    } else {
                        rune_kit::TextInput::new(&edit)
                            .label(label.clone())
                            .placeholder(placeholder.clone())
                            .on_edit(move |s: &mut UiState| {
                                s.plugin_fields
                                    .entry(owner.clone())
                                    .or_default()
                                    .entry(field.clone())
                                    .or_insert_with(|| rune::TextEdit::new(initial.clone()))
                            })
                            .build(k)
                    };
                    if *show_labels {
                        form = form.child(text(label.clone()).color(k.c.text_base));
                    }
                    form = form.child(if *inline {
                        input.key(key).grow(1.0).min_w(0.0)
                    } else {
                        input.key(key).w_full()
                    });
                }
                form.child(
                    Node::Button {
                        label: submit_label.clone(),
                        action: action.clone(),
                        presentation: ButtonPresentation::default(),
                    }
                    .render_child(ctx, id, false, layer),
                )
            }
            Self::WebView { .. } => {
                text(tr_ref(ctx.s.cfg.general.language, "Loading…")).color(k.c.text_muted)
            }
            Self::TerminalButton { label, entry } => {
                let entry = entry.clone();
                Button::new(label.clone())
                    .variant(ButtonVariant::Outline)
                    .on_press(move |s: &mut UiState| {
                        if let Some(plugin) = s.plugins.get_enabled(id, &s.cfg.plugins_disabled) {
                            plugin.open_terminal_entry(s, &entry);
                        }
                    })
                    .build(k)
            }
            Self::Button {
                label,
                action,
                presentation,
            } => {
                let instance = ctx.tile_id.to_owned();
                let action = action.clone();
                let mut button = Button::new(label.clone())
                    .variant(
                        if matches!(presentation.variant, Some(ButtonStyle::Ghost)) {
                            ButtonVariant::GhostMuted
                        } else {
                            ButtonVariant::Outline
                        },
                    )
                    .control_size(if matches!(presentation.size, Some(ButtonSize::Small)) {
                        ControlSize::Small
                    } else {
                        ControlSize::Regular
                    });
                if let Some(name) = &presentation.aria_label {
                    button = button.help(name);
                }
                let mut button = button
                    .on_press(move |s: &mut UiState| {
                        if let Some(plugin) = s.plugins.get_enabled(id, &s.cfg.plugins_disabled) {
                            plugin.on_view_action(s, &instance, &action);
                        }
                    })
                    .build(k);
                if let Some(name) = &presentation.aria_label {
                    button = button.label(name);
                }
                if let Some(width) = presentation.width {
                    button = button.w(width).min_w(width);
                }
                if let Some(height) = presentation.height {
                    button = button.h(height);
                }
                button
            }
        }
    }
}

impl ExternalPlugin {
    pub(super) fn render_native_view(
        &self,
        ctx: &PluginViewContext<'_>,
        layer: &mut Vec<El>,
    ) -> El {
        let views = self.native_views.lock().unwrap();
        let open = Node::Button {
            label: tr_ref(ctx.s.cfg.general.language, "Open").into(),
            action: "open".into(),
            presentation: ButtonPresentation::default(),
        };
        let mut body = col().w_full().px(16.0).py(12.0).gap(8.0);
        if self
            .jobs
            .lock()
            .unwrap()
            .contains_key(&Some(ctx.tile_id.to_owned()))
        {
            body = body.child(
                text(tr_ref(ctx.s.cfg.general.language, "Loading…")).color(ctx.k.c.text_muted),
            );
        }
        let doc = views.get(ctx.tile_id);
        let node = doc.map_or(&open, |document| &document.root);
        body = body.child(node.render_child(ctx, self.id, false, layer));
        let scope = format!("native-view-{}-{}", self.id, ctx.tile_id);
        let pane = if node.fills_height() {
            body.grow(1.0).min_h(0.0)
        } else {
            col()
                .grow(1.0)
                .min_h(0.0)
                .w_full()
                .child(body)
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_bar(ctx.k)
        }
        .key(&scope);
        match doc {
            Some(document) => document.text_menu(pane, scope, ctx, layer),
            None => pane,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ActionPlugin;
    impl DesktopPlugin for ActionPlugin {
        fn id(&self) -> &'static str {
            "layout-test"
        }
        fn title(&self, _: Language) -> String {
            "Layout test".into()
        }
        fn icon(&self) -> &'static Icon {
            &icons::TASK
        }
        fn render_tile(&self, _: &PluginViewContext<'_>, _: &mut Vec<El>) -> Option<El> {
            None
        }
        fn on_view_action(&self, s: &mut UiState, instance: &str, action: &str) -> bool {
            s.status = format!("{instance}:{action}");
            true
        }
    }

    fn layout_view(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
        let lanes: Vec<_> = (0..2).map(|lane| serde_json::json!({
            "type":"column","key":format!("lane-{lane}"),"width":280,
            "children":[{"type":"scrollarea","key":format!("scroll-{lane}"),"axis":"vertical","height":140,
            "children":(0..10).map(|card| serde_json::json!({"type":"card","key":format!("card-{lane}-{card}"),"label":"Open record","action":format!("open-{card}"),"children":[{"type":"text","text":format!("Record {card}")}]})).collect::<Vec<_>>()}]
        })).collect();
        let node: Node = serde_json::from_value(serde_json::json!({"type":"scrollarea","key":"rail","axis":"horizontal","height":180,"children":[{"type":"row","wrap":false,"children":lanes}]})).unwrap();
        let kit = rune_kit::Kit::neutral(true);
        let look = s.look();
        node.render(
            &PluginViewContext::new(
                s,
                &kit,
                &look,
                "instance",
                1.0,
                crate::plugin::PluginSlot::Page,
            ),
            "layout-test",
        )
    }

    fn rect(rt: &rune::Runtime<UiState>, suffix: &str) -> [f32; 4] {
        rt.elements()
            .into_iter()
            .find(|element| {
                element.key.as_deref()
                    == Some(&format!("plugin-native-layout-test-instance-{suffix}"))
            })
            .unwrap()
            .rect
    }

    #[test]
    fn clicking_a_markdown_fragment_aligns_its_heading_below_the_fixed_header_in_only_that_pane() {
        fn view(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let kit = rune_kit::Kit::neutral(true);
            let look = s.look();
            let tree: Node = serde_json::from_value(serde_json::json!({"type":"column","height":"fill","children":[
                {"type":"column","key":"header","height":40,"children":[{"type":"text","text":"Card 905"}]},
                {"type":"scrollarea","key":"body","axis":"vertical","height":"fill","children":[
                    {"type":"markdown","text":"[Jump](#setup)"},
                    {"type":"column","height":500,"children":[]},
                    {"type":"markdown","anchor":"setup","text":"## Destination"},
                    {"type":"column","height":500,"children":[]}
                ]}
            ]})).unwrap();
            ["left", "right"]
                .into_iter()
                .fold(row().w_full().h_full(), |r, tile| {
                    let ctx = PluginViewContext::new(
                        s,
                        &kit,
                        &look,
                        tile,
                        1.0,
                        crate::plugin::PluginSlot::Page,
                    );
                    r.child(
                        col()
                            .w(210.0)
                            .h_full()
                            .child(tree.render(&ctx, "layout-test")),
                    )
                })
        }
        let (commands, _) = crate::model::commands::channel();
        let mut rt = rune::Runtime::new(
            UiState::new(commands),
            view,
            (420.0, 240.0),
            rune::Env::new(true, 1.0),
        );
        rt.set_system_reduced_motion(true);
        rt.frame(0.0);
        let elements = rt.elements();
        let link = elements
            .iter()
            .find(|e| e.text.as_deref() == Some("Jump") && e.rect[0] > 180.0)
            .unwrap();
        let body = elements
            .iter()
            .find(|e| e.key.as_deref() == Some("plugin-native-layout-test-right-body"))
            .unwrap();
        let positions = |rt: &rune::Runtime<UiState>| -> Vec<_> {
            rt.elements()
                .into_iter()
                .filter(|e| e.text.as_deref() == Some("Destination"))
                .map(|e| e.rect[1])
                .collect()
        };
        let before = positions(&rt);
        assert_eq!(before.len(), 2);
        assert!(before[1] > 500.0);
        rt.pointer_down(link.rect[0] + 8.0, link.rect[1] + 7.0);
        rt.pointer_up(link.rect[0] + 8.0, link.rect[1] + 7.0);
        rt.frame(0.0);
        let after = positions(&rt);
        assert_eq!(before[0], after[0]);
        assert!(
            (after[1] - body.rect[1]).abs() < 2.0,
            "destination {after:?}, body {:?}",
            body.rect
        );
    }

    #[test]
    fn resizing_the_pane_gives_the_board_and_card_scroller_the_remaining_height() {
        fn board_tile(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let kit = rune_kit::Kit::neutral(true);
            let look = s.look();
            let ctx = PluginViewContext::new(
                s,
                &kit,
                &look,
                "instance",
                1.0,
                crate::plugin::PluginSlot::Page,
            );
            let plugin = s.plugins.get("layout-test").unwrap();
            col()
                .w_full()
                .h_full()
                .child(plugin.render_tile(&ctx, &mut vec![]).unwrap())
        }
        let manifest = PluginManifest::from_package(
            serde_json::json!({"name":"layout-test","version":"1","main":"index.ts","muqun":{}}),
        )
        .unwrap()
        .unwrap();
        let plugin = ExternalPlugin::new(manifest, PathBuf::new());
        let cards = (0..25)
            .map(|n| serde_json::json!({"type":"text","text":format!("Record {n}")}))
            .collect::<Vec<_>>();
        let board = serde_json::json!({"type":"column","height":"fill","children":[
            {"type":"column","key":"header","height":64,"children":[]},
            {"type":"column","height":"fill","children":[
                {"type":"scrollarea","key":"rail","axis":"horizontal","height":"fill","children":[
                    {"type":"row","align":"start","height":"fill","wrap":false,"children":[
                        {"type":"column","key":"lane","width":280,"height":"fill","children":[
                            {"type":"text","text":"In progress"},
                            {"type":"scrollarea","key":"cards","axis":"vertical","height":"fill","children":cards}
                        ]}
                    ]}
                ]}
            ]}
        ]});
        plugin.native_views.lock().unwrap().insert(
            "instance".into(),
            native_selection::Document::new(serde_json::from_value(board).unwrap()),
        );
        let (commands, _) = crate::model::commands::channel();
        let mut state = UiState::new(commands);
        state.plugins.register(Arc::new(plugin));
        let mut rt =
            rune::Runtime::new(state, board_tile, (420.0, 400.0), rune::Env::new(true, 1.0));
        rt.set_system_reduced_motion(true);
        rt.frame(0.0);
        let header = rect(&rt, "header");
        let before = rect(&rt, "cards");
        let rail = rect(&rt, "rail");
        assert!(
            before[3] > 240.0 && before[3] < rail[3],
            "card viewport {before:?}, rail {rail:?}"
        );
        rt.set_viewport((420.0, 880.0), 1.0);
        rt.frame(0.0);
        let after = rect(&rt, "cards");
        assert_eq!(rect(&rt, "header"), header);
        assert!(
            (after[3] - before[3] - 480.0).abs() < 1.0,
            "card viewport {before:?} -> {after:?}"
        );
        assert_eq!(after[1], before[1]);
    }

    #[test]
    fn rotated_truncated_text_keeps_its_explicit_unrotated_measurement() {
        fn vertical_view(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let node: Node=serde_json::from_value(serde_json::json!({"type":"row","width":28,"height":210,"align":"center","justify":"center","children":[{"type":"text","text":"INPROGRESS","width":190,"height":20,"rotate":90,"truncate":true}]})).unwrap();
            let kit = rune_kit::Kit::neutral(true);
            let look = s.look();
            node.render(
                &PluginViewContext::new(
                    s,
                    &kit,
                    &look,
                    "instance",
                    1.0,
                    crate::plugin::PluginSlot::Page,
                ),
                "layout-test",
            )
        }
        let (commands, _) = crate::model::commands::channel();
        let mut rt = rune::Runtime::new(
            UiState::new(commands),
            vertical_view,
            (100.0, 240.0),
            rune::Env::new(true, 1.0),
        );
        rt.frame(0.0);
        let label = rt
            .elements()
            .into_iter()
            .find(|element| element.role == "text")
            .unwrap();
        assert!(
            label.rect[3] > 180.0,
            "a whole rotated label must not truncate to its narrow rail width"
        );
        assert!(label.rect[2] < 25.0);
    }

    #[test]
    fn explicit_alignment_top_aligns_unequal_columns_without_changing_default_rows() {
        fn aligned_view(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let mut tree = serde_json::json!({"type":"row","height":180,"wrap":false,"children":[
                {"type":"column","key":"short","width":60,"height":44,"children":[]},
                {"type":"column","key":"tall","width":200,"height":140,"children":[]}
            ]});
            if !s.status.is_empty() {
                tree["align"] = serde_json::Value::String(s.status.clone());
            }
            let node: Node = serde_json::from_value(tree).unwrap();
            let kit = rune_kit::Kit::neutral(true);
            let look = s.look();
            node.render(
                &PluginViewContext::new(
                    s,
                    &kit,
                    &look,
                    "instance",
                    1.0,
                    crate::plugin::PluginSlot::Page,
                ),
                "layout-test",
            )
        }
        let (commands, _) = crate::model::commands::channel();
        let mut state = UiState::new(commands);
        state.status.clear();
        let mut rt = rune::Runtime::new(
            state,
            aligned_view,
            (400.0, 200.0),
            rune::Env::new(true, 1.0),
        );
        rt.set_system_reduced_motion(true);
        rt.frame(0.0);
        let original = rect(&rt, "short");
        assert!(original[1] > rect(&rt, "tall")[1] + 30.0);
        rt.update(|s, _| s.status = "start".into());
        rt.frame(0.0);
        assert_eq!(rect(&rt, "short")[1], rect(&rt, "tall")[1]);
        rt.update(|s, _| s.status = "center".into());
        rt.frame(0.0);
        assert_eq!(rect(&rt, "short")[1], original[1]);
        rt.update(|s, _| s.status = "end".into());
        rt.frame(0.0);
        let short = rect(&rt, "short");
        let tall = rect(&rt, "tall");
        assert_eq!(short[1] + short[3], tall[1] + tall[3]);
    }

    #[test]
    fn a_plugin_picker_selects_the_right_action_outside_its_clipped_container() {
        fn render_picker(s: &UiState, _: &mut rune::Cx<UiState>) -> El {
            let k = rune_kit::Kit::neutral(true);
            let look = s.look();
            let ctx = PluginViewContext::new(
                s,
                &k,
                &look,
                "instance",
                1.0,
                crate::plugin::PluginSlot::Page,
            );
            let picker: Node = serde_json::from_value(serde_json::json!({
                "type":"select","key":"boards","label":"Board","options":[
                    {"label":"Team","action":"first","selected":true},
                    {"label":"Team","action":"second"}
                ]
            }))
            .unwrap();
            let mut layer = vec![];
            let trigger = picker.render_child(&ctx, "layout-test", false, &mut layer);
            rune_kit::root(
                col()
                    .w_full()
                    .h_full()
                    .child(col().h(28.0).clip_rounded(2.0).child(trigger)),
                layer,
            )
        }
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.plugins.register(Arc::new(ActionPlugin));
        let mut rt =
            rune::Runtime::new(s, render_picker, (420.0, 260.0), rune::Env::new(true, 1.0));
        rt.set_system_reduced_motion(true);
        let mut click = |suffix: &str| {
            for _ in 0..30 {
                rt.frame(1.0 / 60.0);
            }
            let [x, y, w, h] = rt
                .elements()
                .into_iter()
                .find(|e| e.key.as_deref().is_some_and(|k| k.ends_with(suffix)))
                .unwrap()
                .rect;
            rt.pointer_down(x + w / 2.0, y + h / 2.0);
            rt.pointer_up(x + w / 2.0, y + h / 2.0);
            y
        };
        click("boards-field-trigger");
        assert!(click("boards-item1") >= 28.0);
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.status, "instance:second");
        assert!(rt.state.menu.is_none());
    }

    #[test]
    fn card_actions_keep_instance_and_nested_scroll_axes_independent() {
        let (commands, _) = crate::model::commands::channel();
        let mut state = UiState::new(commands);
        state.plugins.register(Arc::new(ActionPlugin));
        let mut rt = rune::Runtime::new(
            state,
            layout_view,
            (400.0, 200.0),
            rune::Env::new(true, 1.0),
        );
        rt.frame(0.0);
        let first = rect(&rt, "card-0-0");
        let other = rect(&rt, "card-1-0");
        assert!(other[0] > first[0] + 250.0);
        rt.pointer_down(first[0] + 10.0, first[1] + 10.0);
        rt.pointer_up(first[0] + 10.0, first[1] + 10.0);
        rt.frame(0.0);
        assert_eq!(rt.state.status, "instance:open-0");
        rt.wheel(40.0, 60.0, -80.0);
        rt.frame(0.0);
        let moved = rect(&rt, "card-0-0");
        assert!(moved[1] < first[1] - 20.0);
        assert_eq!(rect(&rt, "card-1-0")[1], other[1]);
        rt.wheel_xy(180.0, 170.0, -100.0, 0.0);
        rt.frame(0.0);
        assert!(rect(&rt, "card-1-0")[0] < other[0] - 20.0);
        assert_eq!(rect(&rt, "card-0-0")[1], moved[1]);
    }

    #[test]
    fn layout_validation_keeps_existing_trees_and_rejects_unbounded_or_ambiguous_layouts() {
        let old: Node = serde_json::from_str(r#"{"type":"column","children":[{"type":"row","children":[{"type":"text","text":"Existing"}]}]}"#).unwrap();
        assert!(old.fields(&HashMap::new()).is_ok());
        for view in [
            serde_json::json!({"type":"column","width":100000,"children":[]}),
            serde_json::json!({"type":"scrollarea","key":"scroll","axis":"vertical","height":0,"children":[]}),
            serde_json::json!({"type":"avatar","name":"Person","size":1000}),
            serde_json::json!({"type":"text","text":"Untrusted rotation","rotate":10000}),
            serde_json::json!({"type":"column","paddingX":100000,"children":[]}),
            serde_json::json!({"type":"column","children":[{"type":"card","key":"same","label":"One","children":[]},{"type":"card","key":"same","label":"Two","children":[]}]}),
        ] {
            assert!(serde_json::from_value::<Node>(view)
                .unwrap()
                .fields(&HashMap::new())
                .is_err());
        }
    }

    #[test]
    fn avatars_accept_only_bounded_inline_raster_thumbnails() {
        let thumbnail = |width| {
            let mut bytes = std::io::Cursor::new(Vec::new());
            image::RgbaImage::new(width, 2)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
            )
        };
        assert!(avatar_bytes(&thumbnail(2)).is_some());
        assert!(avatar_bytes(&thumbnail(513)).is_none());
        for source in [
            "https://example.test/avatar.png",
            "file:///private/avatar.png",
            "data:image/svg+xml;base64,PHN2Zy8+",
            "data:image/png;base64,bm90LWltYWdl",
        ] {
            assert!(avatar_bytes(source).is_none());
        }
        assert!(avatar_bytes(&format!("data:image/png;base64,{}", "A".repeat(32768))).is_none());
    }
}
