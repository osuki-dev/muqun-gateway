//! Human labels for catalogue ids — never leave raw ids like `default[]` on a pill.

use muqun_gateway::agents::{ModelInfo, ModelRef};

/// Strip a trailing `[…]` suffix (`default[]` → `default`).
pub fn bare_catalog_id(id: &str) -> &str {
    let bytes = id.as_bytes();
    if let Some(open) = id.rfind('[') {
        if open > 0 && bytes.last() == Some(&b']') {
            let head = id[..open].trim();
            if !head.is_empty() {
                return head;
            }
        }
    }
    id
}

/// camelCase / kebab ids → a short title (`bypassPermissions` → `Bypass permissions`).
pub fn readable_catalog_id(id: &str) -> String {
    let bare = bare_catalog_id(id);
    let mut spaced = String::with_capacity(bare.len() + 4);
    let mut prev = '\0';
    for ch in bare.chars() {
        if ch == '-' || ch == '_' || ch.is_whitespace() {
            if !spaced.ends_with(' ') && !spaced.is_empty() {
                spaced.push(' ');
            }
        } else if prev.is_ascii_lowercase() || prev.is_ascii_digit() {
            if ch.is_ascii_uppercase() {
                spaced.push(' ');
            }
            spaced.push(ch);
        } else {
            spaced.push(ch);
        }
        prev = ch;
    }
    let words: Vec<String> = spaced
        .split_whitespace()
        .map(|word| {
            if word.len() > 1 && word.chars().all(|c| c.is_ascii_uppercase()) {
                word.to_string()
            } else {
                word.to_ascii_lowercase()
            }
        })
        .collect();
    let sentence = words.join(" ");
    let mut chars = sentence.chars();
    match chars.next() {
        Some(c) => format!("{}{}", c.to_ascii_uppercase(), chars.as_str()),
        None => id.to_string(),
    }
}

fn known_model_name(id: &str) -> Option<&'static str> {
    Some(match bare_catalog_id(id) {
        "gemini-3.8-flash" => "Gemini 3.8 Flash",
        "deepseek-v4.1-flash" => "DeepSeek V4.1 Flash",
        "deepseek-v4-flash-free" => "DeepSeek V4 Flash",
        "gpt-5.6-sol" => "GPT-5.6 Sol",
        "gpt-5.6-luna" => "GPT-5.6 Luna",
        "gpt-6-astra" => "GPT-6 Astra",
        "gpt-6-astra-fast" => "GPT-6 Astra Fast",
        "muse-spark-1.3-contributor-free" => "Muse Spark 1.3",
        "nemotron-3.5-lightning-free" => "Nemotron 3.5 Lightning",
        "ling-3.0-flash-fin-free" => "Ling 3.0 Flash",
        _ => return None,
    })
}

/// App `formatModelName` without folding variant into the chip (effort owns that).
pub fn format_model_name(model: &ModelRef, fallback: &str, catalog_name: Option<&str>) -> String {
    if model.model_id.is_empty() {
        return fallback.to_string();
    }
    let id = bare_catalog_id(&model.model_id);
    if let Some(name) = catalog_name.map(str::trim).filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    if let Some(known) = known_model_name(id) {
        return known.to_string();
    }
    if id.contains('-') || id.contains('_') || id.contains('.') {
        return id
            .split(['-', '_', '.'])
            .filter(|w| !w.is_empty())
            .map(|word| {
                if word.len() <= 3 {
                    word.to_ascii_uppercase()
                } else {
                    let mut chars = word.chars();
                    match chars.next() {
                        Some(c) => format!("{}{}", c.to_ascii_uppercase(), chars.as_str()),
                        None => String::new(),
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
    }
    readable_catalog_id(id)
}

fn catalog_entry<'a>(
    models: &[&'a ModelInfo],
    id: &str,
    default_id: Option<&str>,
) -> Option<&'a ModelInfo> {
    let find = |wanted: &str| {
        models
            .iter()
            .find(|m| m.id == wanted)
            .or_else(|| {
                models
                    .iter()
                    .find(|m| bare_catalog_id(&m.id) == bare_catalog_id(wanted))
            })
            .copied()
    };
    if let Some(hit) = find(id) {
        return Some(hit);
    }
    let default_id = default_id?;
    if bare_catalog_id(id).eq_ignore_ascii_case("default") {
        find(default_id)
    } else {
        None
    }
}

/// Exact provider match first, then bare-id / default.
pub fn catalog_model_info<'a>(
    models: &[&'a ModelInfo],
    model: &ModelRef,
    default_model: Option<&ModelRef>,
) -> Option<&'a ModelInfo> {
    if model.model_id.is_empty() {
        return None;
    }
    models
        .iter()
        .find(|m| {
            m.id == model.model_id
                && (model.provider_id.is_empty() || m.provider_id == model.provider_id)
        })
        .copied()
        .or_else(|| {
            catalog_entry(
                models,
                &model.model_id,
                default_model.map(|d| d.model_id.as_str()),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_brackets_become_default() {
        assert_eq!(bare_catalog_id("default[]"), "default");
        assert_eq!(readable_catalog_id("default[]"), "Default");
        let model = ModelRef {
            provider_id: String::new(),
            model_id: "default[]".into(),
            variant: None,
        };
        assert_eq!(format_model_name(&model, "Model", None), "Default");
    }

    #[test]
    fn known_and_kebab_ids() {
        let model = ModelRef {
            provider_id: "openai".into(),
            model_id: "gpt-6-astra".into(),
            variant: None,
        };
        assert_eq!(format_model_name(&model, "Model", None), "GPT-6 Astra");
        assert_eq!(
            format_model_name(&model, "Model", Some("Sonnet 4")),
            "Sonnet 4"
        );
    }
}
