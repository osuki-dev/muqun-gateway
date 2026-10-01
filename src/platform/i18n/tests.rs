use super::*;
use axum::http::HeaderValue;

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

#[test]
fn the_twelve_codes_are_the_literals_the_app_and_the_site_already_use() {
    // Not `zh-Hant`, not `zh-Hant-TW`, not `zh_TW`, not `zh-Hans` and not
    // `pt-BR`. The app names its catalog directories and persists its
    // setting with these exact strings, so a change here silently
    // un-localizes every client.
    let codes: Vec<&str> = Locale::ALL.iter().map(|locale| locale.as_str()).collect();
    assert_eq!(
        codes,
        ["en", "zh-TW", "zh-CN", "ja", "ko", "de", "fr", "es", "pt", "ru", "vi", "th"]
    );
    assert_eq!(Locale::default(), Locale::En);
}

#[test]
fn every_code_is_spelled_once_and_reads_back_as_itself() {
    // `as_str` writes and `from_code` reads; a language whose two halves
    // disagree answers in one code and is asked for in another.
    let mut seen = std::collections::HashSet::new();
    for &locale in Locale::ALL {
        let code = locale.as_str();
        assert!(seen.insert(code), "{code} is spelled by two variants");
        assert_eq!(
            Locale::from_code(code),
            Some(locale),
            "{code} does not read back"
        );
    }
}

#[test]
fn the_app_header_carries_one_exact_code_for_each_locale() {
    assert_eq!(
        Locale::from_headers(&headers(&[("x-muqun-locale", "en")])),
        Locale::En
    );
    assert_eq!(
        Locale::from_headers(&headers(&[("x-muqun-locale", "zh-TW")])),
        Locale::ZhTw
    );
    // Header names are case-insensitive on the wire and the app spells it
    // `X-Muqun-Locale`.
    assert_eq!(
        Locale::from_headers(&headers(&[("X-Muqun-Locale", "zh-TW")])),
        Locale::ZhTw
    );
    // The three codes the app added together, each as the app sends it.
    for (code, expected) in [
        ("zh-CN", Locale::ZhCn),
        ("ru", Locale::Ru),
        ("vi", Locale::Vi),
    ] {
        assert_eq!(
            Locale::from_headers(&headers(&[("X-Muqun-Locale", code)])),
            expected,
            "{code}"
        );
    }
}

#[test]
fn accept_language_answers_when_the_app_header_is_absent() {
    assert_eq!(
        Locale::from_headers(&headers(&[("accept-language", "zh-TW")])),
        Locale::ZhTw
    );
    // What a browser actually sends.
    assert_eq!(
        Locale::from_headers(&headers(&[("accept-language", "zh-TW,zh;q=0.9,en;q=0.8")])),
        Locale::ZhTw
    );
    // The highest weight the gateway *can serve* wins, not the first tag
    // and not the highest weight overall. `it` used to be `fr` here, which
    // stopped testing anything the day French became a language we have --
    // the unservable entry has to actually be unservable.
    assert_eq!(
        Locale::from_headers(&headers(&[(
            "accept-language",
            "it;q=1.0,en;q=0.4,zh-TW;q=0.9"
        )])),
        Locale::ZhTw
    );
    assert_eq!(
        Locale::from_headers(&headers(&[("accept-language", "zh-TW;q=0.2,en;q=0.7")])),
        Locale::En
    );
    // `q=0` means "not acceptable", so it may not win by being first.
    assert_eq!(
        Locale::from_headers(&headers(&[("accept-language", "zh-TW;q=0,en")])),
        Locale::En
    );
}

#[test]
fn the_app_header_wins_when_the_two_disagree() {
    assert_eq!(
        Locale::from_headers(&headers(&[
            ("x-muqun-locale", "zh-TW"),
            ("accept-language", "en-US,en;q=0.9"),
        ])),
        Locale::ZhTw
    );
    assert_eq!(
        Locale::from_headers(&headers(&[
            ("x-muqun-locale", "en"),
            ("accept-language", "zh-TW,zh;q=0.9"),
        ])),
        Locale::En
    );
}

#[test]
fn nothing_a_client_can_send_produces_anything_but_a_locale() {
    for value in [
        "",
        "   ",
        "-",
        ";;;",
        "klingon",
        "zh-TW;q=not-a-number",
        "en_US_POSIX_extra",
        "*",
        "q=1.0",
        "🙂",
    ] {
        let explicit = Locale::from_headers(&headers(&[("x-muqun-locale", value)]));
        let accepted = Locale::from_headers(&headers(&[("accept-language", value)]));
        assert_eq!(explicit, Locale::En, "{value:?} via X-Muqun-Locale");
        assert_eq!(accepted, Locale::En, "{value:?} via Accept-Language");
    }
    assert_eq!(Locale::from_headers(&HeaderMap::new()), Locale::En);
    assert_eq!(Locale::resolve(None, None), Locale::En);
}

#[test]
fn a_header_that_is_not_utf8_is_read_as_no_header_at_all() {
    let mut map = HeaderMap::new();
    map.insert(
        axum::http::HeaderName::from_static("x-muqun-locale"),
        HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
    );
    assert_eq!(Locale::from_headers(&map), Locale::En);
}

#[test]
fn traditional_tags_fold_onto_zh_tw_and_simplified_ones_onto_zh_cn() {
    for tag in [
        "zh-TW",
        "zh-tw",
        "zh_TW",
        "zh-Hant",
        "zh-hant",
        "zh-Hant-TW",
        "zh-HK",
        "zh-MO",
    ] {
        assert_eq!(Locale::from_code(tag), Some(Locale::ZhTw), "{tag}");
    }
    for tag in [
        "zh-CN",
        "zh-cn",
        "zh_CN",
        "zh-Hans",
        "zh-hans",
        "zh-Hans-CN",
        "zh-SG",
        "zh-MY",
    ] {
        assert_eq!(Locale::from_code(tag), Some(Locale::ZhCn), "{tag}");
    }
    // Bare `zh` is Simplified: it is what the tag expands to everywhere
    // else, and what a mainland device reports when the user never chose
    // a region. A region that says nothing about script gets the same
    // answer.
    assert_eq!(Locale::from_code("zh"), Some(Locale::ZhCn));
    assert_eq!(Locale::from_code("zh-XX"), Some(Locale::ZhCn));
    // But a mangled tag is not a bare `zh` with noise after it: it is not
    // a tag, and English is the answer for a header that is not one.
    for tag in ["zh-TW;q=0.9", "zh;q=1", "zh-🙂", "zh-T W"] {
        assert_eq!(Locale::from_code(tag), None, "{tag}");
    }
    assert_eq!(Locale::from_code("en-US"), Some(Locale::En));
    assert_eq!(Locale::from_code("en-GB"), Some(Locale::En));
}

#[test]
fn a_chinese_script_subtag_outranks_the_region_beside_it() {
    // The script is the thing the two tables differ by; the region is only
    // a hint about it, and a tag that names both has already answered.
    assert_eq!(Locale::from_code("zh-Hans-TW"), Some(Locale::ZhCn));
    assert_eq!(Locale::from_code("zh-Hans-HK"), Some(Locale::ZhCn));
    assert_eq!(Locale::from_code("zh-Hant-CN"), Some(Locale::ZhTw));
    assert_eq!(Locale::from_code("zh-Hant-SG"), Some(Locale::ZhTw));
    // Subtag order does not matter to the answer; the script still wins.
    assert_eq!(Locale::from_code("zh-TW-Hans"), Some(Locale::ZhCn));
}

#[test]
fn neither_chinese_table_ever_answers_for_the_other() {
    // The same key, two scripts, two different answers -- and each one is
    // its own script, not the other's. A shared entry would be a reader of
    // one script being served the other, which is the bug the split exists
    // to prevent.
    assert_eq!(t(Locale::ZhTw, "Deny"), "拒絕");
    assert_eq!(t(Locale::ZhCn, "Deny"), "拒绝");
    assert_eq!(t(Locale::ZhTw, "Approve"), "核准");
    assert_eq!(t(Locale::ZhCn, "Approve"), "批准");
    assert_eq!(t(Locale::ZhTw, "device not found"), "找不到這個裝置");
    assert_eq!(t(Locale::ZhCn, "device not found"), "找不到这个设备");
    assert_ne!(ZH_TW, ZH_CN);
}

#[test]
fn a_regional_tag_folds_onto_the_one_catalog_its_language_has() {
    // Chinese is the only language split by script here, so it is the only
    // one with a rule of its own. Everything else has exactly one table to
    // land in, and a Brazilian reader served the `pt` table is still being
    // served Portuguese -- whereas a Brazilian reader served English is
    // being served a bug.
    for (tag, expected) in [
        ("ja-JP", Locale::Ja),
        ("ja", Locale::Ja),
        ("ko-KR", Locale::Ko),
        ("de-AT", Locale::De),
        ("de-CH", Locale::De),
        ("de_DE", Locale::De),
        ("fr-CA", Locale::Fr),
        ("fr-BE", Locale::Fr),
        ("es-MX", Locale::Es),
        ("es-419", Locale::Es),
        ("es-ES", Locale::Es),
        ("pt-BR", Locale::Pt),
        ("pt-PT", Locale::Pt),
        ("PT", Locale::Pt),
        ("ru-RU", Locale::Ru),
        ("ru-BY", Locale::Ru),
        ("ru", Locale::Ru),
        ("vi-VN", Locale::Vi),
        ("vi", Locale::Vi),
        ("th-TH", Locale::Th),
        ("th", Locale::Th),
        ("TH", Locale::Th),
    ] {
        assert_eq!(Locale::from_code(tag), Some(expected), "{tag}");
    }
    // On the website but not here, which is the interesting negative: a
    // code existing somewhere in the product is not a table existing in it.
    // `ru` used to be on this list, and `th` after it -- each stopped being
    // a negative the day that language became one we have.
    for tag in ["it", "it-IT", "ar", "nl-NL", "uk", "gl", "ca"] {
        assert_eq!(Locale::from_code(tag), None, "{tag}");
    }
}

#[test]
fn a_weighted_accept_language_still_picks_among_twelve() {
    // A browser in Quebec, ranking French above English.
    assert_eq!(
        Locale::from_headers(&headers(&[("accept-language", "fr-CA,fr;q=0.9,en;q=0.8")])),
        Locale::Fr
    );
    // The highest weight the gateway can serve wins, not the first tag, and
    // a language it cannot serve does not block the ones it can.
    assert_eq!(
        Locale::from_headers(&headers(&[(
            "accept-language",
            "it-IT;q=1.0,en;q=0.3,pt-BR;q=0.9"
        )])),
        Locale::Pt
    );
    // A mainland browser: the exact code first, then the bare language,
    // and both land on the same table.
    assert_eq!(
        Locale::from_headers(&headers(&[("accept-language", "zh-CN,zh;q=0.9,en;q=0.8")])),
        Locale::ZhCn
    );
    // A Vietnamese browser ranking English above its own language still
    // gets English: the weights are the reader's, not ours.
    assert_eq!(
        Locale::from_headers(&headers(&[("accept-language", "en-US,en;q=0.9,vi;q=0.8")])),
        Locale::En
    );
    assert_eq!(
        Locale::from_headers(&headers(&[("accept-language", "ru-RU,ru;q=0.9,en;q=0.8")])),
        Locale::Ru
    );
}

#[test]
fn a_message_with_no_translation_falls_back_to_the_english_string() {
    assert_eq!(t(Locale::ZhTw, "Deny"), "拒絕");
    assert_eq!(t(Locale::ZhCn, "Deny"), "拒绝");
    assert_eq!(t(Locale::Ja, "Deny"), "拒否");
    assert_eq!(t(Locale::De, "Deny"), "Ablehnen");
    assert_eq!(t(Locale::Ru, "Deny"), "Отклонить");
    assert_eq!(t(Locale::Vi, "Deny"), "Từ chối");
    // Taken verbatim from the app's own Thai catalog, so one product does
    // not answer the same word two ways.
    assert_eq!(t(Locale::Th, "Deny"), "ปฏิเสธ");
    assert_eq!(t(Locale::Th, "Agent"), "เอเจนต์");
    // The failure mode a half-finished catalog should have: English, not a
    // blank and not a panic. Asserted for every language, because "we will
    // add the entry later" is a thing that happens in all of them.
    for &locale in Locale::ALL {
        assert_eq!(
            t(locale, "a sentence nobody has translated yet"),
            "a sentence nobody has translated yet",
            "{}",
            locale.as_str()
        );
    }
    // English is the key, so English is also its own answer.
    assert_eq!(t(Locale::En, "Deny"), "Deny");
}

#[test]
fn slots_are_filled_after_the_sentence_has_been_chosen() {
    assert_eq!(
        t_slots(Locale::En, "Option {index}", &[("index", "4")]),
        "Option 4"
    );
    assert_eq!(
        t_slots(Locale::ZhTw, "Option {index}", &[("index", "4")]),
        "選項 4"
    );
    assert_eq!(
        t_slots(
            Locale::ZhTw,
            "{name} needs your input.",
            &[("name", "Codex")]
        ),
        "Codex 需要你的輸入。"
    );
}

#[test]
fn every_catalog_translates_each_english_string_exactly_once() {
    for &locale in Locale::ALL {
        let catalog = locale.catalog();
        let code = locale.as_str();
        if locale == Locale::En {
            // English has no table: it is the key, and an entry mapping a
            // string to itself would be a row that can never be read.
            assert!(catalog.is_empty(), "en should have no table of its own");
            continue;
        }
        for (position, (english, translated)) in catalog.iter().enumerate() {
            assert!(!english.is_empty(), "{code}: an empty key matches nothing");
            assert_ne!(english, translated, "{code}: {english:?} is not translated");
            assert!(
                !catalog[..position]
                    .iter()
                    .any(|(earlier, _)| earlier == english),
                "{code}: {english:?} appears twice, so one of the two is dead"
            );
        }
    }
}

#[test]
fn every_catalog_covers_exactly_the_same_english_strings() {
    // The tables are hand-written and there are ten of them. Without this,
    // a language quietly missing four sentences is four screens that switch
    // back to English mid-paragraph, and nothing says which four.
    let reference: Vec<&str> = ZH_TW.iter().map(|(english, _)| *english).collect();
    for &locale in Locale::ALL {
        if locale == Locale::En {
            continue;
        }
        let code = locale.as_str();
        let keys: Vec<&str> = locale
            .catalog()
            .iter()
            .map(|(english, _)| *english)
            .collect();
        let missing: Vec<&&str> = reference.iter().filter(|k| !keys.contains(k)).collect();
        let extra: Vec<&&str> = keys.iter().filter(|k| !reference.contains(k)).collect();
        assert!(missing.is_empty(), "{code} is missing {missing:?}");
        assert!(extra.is_empty(), "{code} has {extra:?}, which nothing says");
    }
}

#[test]
fn every_slot_in_a_source_string_survives_into_its_translation() {
    // A translator who drops `{name}` produces a push with no agent in it,
    // which reads as a bug rather than as a typo.
    for &locale in Locale::ALL {
        let code = locale.as_str();
        for (english, translated) in locale.catalog() {
            for slot in ["{index}", "{action}", "{name}"] {
                assert_eq!(
                    english.contains(slot),
                    translated.contains(slot),
                    "{code}: {english:?} and its translation disagree about {slot}"
                );
            }
            // And no translation may invent a slot, which `t_slots` would
            // leave standing in the output as literal braces.
            assert_eq!(
                translated.matches('{').count(),
                english.matches('{').count(),
                "{code}: {english:?} and {translated:?} disagree about brace count"
            );
        }
    }
}

#[test]
fn api_vocabulary_inside_a_message_is_never_translated() {
    // These are values a client sends back to us, not words. A catalog that
    // "translated" `allow_always` would produce an error message telling the
    // reader to send a value the API rejects.
    for &locale in Locale::ALL {
        let code = locale.as_str();
        for (english, translated) in locale.catalog() {
            for token in [
                "allow_always",
                "recent-unwrapped",
                "branch_name",
                "device_name",
                "startup_timeout_ms",
                "workspace_label",
                "request_id",
                "repo_path",
                "GET /api/agents/catalog",
            ] {
                if english.contains(token) {
                    assert!(
                        translated.contains(token),
                        "{code}: {english:?} lost the API token {token:?}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn the_ambient_locale_is_english_outside_a_request_and_the_request_s_inside_one() {
    assert_eq!(current(), Locale::En);
    let inside = scope(Locale::ZhTw, async { current() }).await;
    assert_eq!(inside, Locale::ZhTw);
    assert_eq!(current(), Locale::En, "the scope does not leak");
}
