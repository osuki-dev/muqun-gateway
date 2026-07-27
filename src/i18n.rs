//! Which language the gateway is answering in, and the table it answers from.
//!
//! Two locales exist and their codes are literals, not a naming scheme:
//! `en` and `zh-TW`. Lowercase primary tag, hyphen, uppercase region. The app
//! already uses those exact strings as the name of its catalog directory, as
//! the value it persists when the user picks a language, and as the value it
//! puts on the wire -- so a gateway that answered `zh-Hant` or `zh_TW` would be
//! answering with a code no client has a catalog for. There is deliberately no
//! alias for either code anywhere in this file's *output*; tolerance lives only
//! in [`Locale::from_code`], which reads what a client sent and never writes.
//!
//! Resolution order is `X-Muqun-Locale`, then `Accept-Language`, then `en`. The
//! app sends both headers with the same single code on every request including
//! the long-lived SSE stream, so the first hop answers in practice; the second
//! exists for browsers, which is also why the parser handles a weighted list.
//! Nothing in here can fail: a header that is absent, empty, mangled, not UTF-8
//! or simply about a language the gateway does not have falls back to `en`. An
//! error would be a worse answer than English to every one of those.
//!
//! **English is the key.** A message is looked up by its own English text, so a
//! call site that has not been translated yet still compiles, still runs, and
//! still says something true -- it just says it in English. That is the failure
//! mode a half-finished catalog should have.

use std::future::Future;

use axum::http::header::ACCEPT_LANGUAGE;
use axum::http::HeaderMap;

/// The app's own header: one exact code, no q-values, no lists. It is preferred
/// over `Accept-Language` because it carries the language the user *chose* in
/// the app, which is not always the one the operating system reports.
pub const LOCALE_HEADER: &str = "x-muqun-locale";

/// A language the gateway can answer in.
///
/// The set is closed on purpose. Adding a variant means adding a column to the
/// table below, and [`Locale::as_str`] is the only place a code is ever spelled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Locale {
    #[default]
    En,
    ZhTw,
}

impl Locale {
    /// The wire spelling. These two strings are shared verbatim with the app
    /// and the marketing site; nothing may normalize, case-fold or "improve"
    /// them.
    pub fn as_str(self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::ZhTw => "zh-TW",
        }
    }

    /// The locale a single BCP-47 tag asks for, if it is one the gateway can
    /// serve.
    ///
    /// Traditional Chinese is served to the tags that mean Traditional Chinese:
    /// `zh-Hant` in any of its regional spellings, plus `zh-HK` and `zh-MO`,
    /// whose readers read Traditional. `zh-Hans`, `zh-CN` and `zh-SG` are
    /// deliberately *not* folded onto it -- serving Traditional to a Simplified
    /// reader is a worse answer than serving English, so they fall through.
    /// Bare `zh` says only "Chinese" and is left to fall through for the same
    /// reason: the script is exactly the thing it does not say.
    pub fn from_code(code: &str) -> Option<Self> {
        let mut subtags = code
            .trim()
            .split(['-', '_'])
            .filter(|subtag| !subtag.is_empty())
            .map(str::to_ascii_lowercase);
        let primary = subtags.next()?;
        let rest: Vec<String> = subtags.collect();
        let has = |names: &[&str]| rest.iter().any(|subtag| names.contains(&subtag.as_str()));
        match primary.as_str() {
            "en" => Some(Locale::En),
            "zh" if has(&["hans", "cn", "sg"]) => None,
            "zh" if has(&["hant", "tw", "hk", "mo"]) => Some(Locale::ZhTw),
            _ => None,
        }
    }

    /// The best servable locale in an `Accept-Language` list.
    ///
    /// A browser sends `zh-TW,zh;q=0.9,en;q=0.8`, so the highest-weighted tag
    /// the gateway can actually serve wins rather than simply the first one.
    /// Ties go to the earlier entry, which is what the header's own ordering
    /// means. A `q` the client mangled drops that entry instead of the request.
    pub fn from_accept_language(header: &str) -> Option<Self> {
        let mut best: Option<(f32, Locale)> = None;
        for entry in header.split(',') {
            let mut pieces = entry.split(';');
            let tag = pieces.next().unwrap_or_default().trim();
            let mut quality = 1.0_f32;
            for parameter in pieces {
                let parameter = parameter.trim();
                let value = parameter
                    .strip_prefix("q=")
                    .or_else(|| parameter.strip_prefix("Q="));
                if let Some(value) = value {
                    quality = value.trim().parse::<f32>().unwrap_or(0.0);
                }
            }
            // `q=0` means "not acceptable", and a `q` that parsed into
            // something that is not a number is no answer at all.
            if !quality.is_finite() || quality <= 0.0 {
                continue;
            }
            let Some(locale) = Locale::from_code(tag) else {
                continue;
            };
            if best.is_none_or(|(seen, _)| quality > seen) {
                best = Some((quality, locale));
            }
        }
        best.map(|(_, locale)| locale)
    }

    /// `X-Muqun-Locale`, then `Accept-Language`, then English.
    ///
    /// An `X-Muqun-Locale` the gateway cannot serve does not short-circuit to
    /// English -- it falls through to `Accept-Language`, which is the more
    /// specific answer of the two remaining ones. Either way the walk ends at
    /// `en` and never at an error.
    pub fn resolve(explicit: Option<&str>, accept_language: Option<&str>) -> Self {
        explicit
            .and_then(Locale::from_code)
            .or_else(|| accept_language.and_then(Locale::from_accept_language))
            .unwrap_or_default()
    }

    /// The locale a request asks for. A header that is not UTF-8 is read as no
    /// header at all, which is the same fallback every other malformed value
    /// gets.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let text = |name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        Locale::resolve(
            text(LOCALE_HEADER).as_deref(),
            headers
                .get(ACCEPT_LANGUAGE)
                .and_then(|value| value.to_str().ok()),
        )
    }
}

tokio::task_local! {
    static REQUEST_LOCALE: Locale;
}

/// Run a request with its locale in scope.
///
/// The alternative was threading a `Locale` argument through all seventy-nine
/// `api_error` call sites and every helper between them and a handler --
/// including ones like `find_session` that have no business knowing a request
/// exists. A task-local is scoped to exactly the same lifetime a request has,
/// costs nothing when nobody reads it, and cannot be forgotten at a call site.
pub async fn scope<F: Future>(locale: Locale, future: F) -> F::Output {
    REQUEST_LOCALE.scope(locale, future).await
}

/// The locale of the request being served, or English outside one.
///
/// Background work -- the approval and agent-status watchers, the CLI, tests --
/// has no request to read, and English is the right answer there rather than a
/// panic. Pushes do not rely on this: they carry the locale the device
/// registered with, because the watcher that builds them is not serving anyone.
pub fn current() -> Locale {
    REQUEST_LOCALE
        .try_with(|locale| *locale)
        .unwrap_or_default()
}

/// The reader's wording for an English source string, or the English itself
/// when the catalog has no entry for it.
pub fn t(locale: Locale, source: &str) -> &str {
    match locale {
        Locale::En => source,
        Locale::ZhTw => ZH_TW
            .iter()
            .find(|(english, _)| *english == source)
            .map_or(source, |(_, translated)| *translated),
    }
}

/// The same lookup for a message with named slots in it.
///
/// Word order is the reason these are format strings rather than concatenated
/// fragments: "Codex needs your input." and "Codex 需要你的輸入。" happen to put
/// the name first, but nothing guarantees the next language will, and a
/// `format!("{name} {tail}")` gives a translator no way to move it.
pub fn t_slots(locale: Locale, source: &str, slots: &[(&str, &str)]) -> String {
    let mut text = t(locale, source).to_owned();
    for (name, value) in slots {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

/// Taiwanese-normative Traditional Chinese, keyed by the English source.
///
/// The register is the island's, not Simplified Chinese transliterated: 設定,
/// 終端機, 檔案, 伺服器, 網路, 儲存, 預設, 程式碼, 連線. The app's own catalog
/// is the other half of this vocabulary and these agree with it: 核准, 拒絕,
/// 代理程式, 面板, 工作區, 工作階段, 配對 -- and Gateway, which stays in Latin
/// script because it is the product's name.
///
/// API vocabulary embedded in a message is *not* translated: `allow`,
/// `allow_always`, `deny`, `visible`, `recent`, `recent-unwrapped`,
/// `detection`, field names, and the route `GET /api/agents/catalog` are values
/// a client sends back, so only the sentence around them moves.
const ZH_TW: &[(&str, &str)] = &[
    // -- approval labels the gateway writes for itself -----------------------
    ("Approve", "核准"),
    ("Approve and don't ask again", "核准，且不再詢問"),
    ("Deny", "拒絕"),
    ("Option {index}", "選項 {index}"),
    ("Allow {action}?", "允許 {action}？"),
    // -- push notifications --------------------------------------------------
    ("Agent", "代理程式"),
    ("Approval needed", "需要核准"),
    ("Agent blocked", "代理程式等待中"),
    ("Agent done", "代理程式已完成"),
    ("{name} is waiting for your approval.", "{name} 正在等待你的核准。"),
    ("{name} needs your input.", "{name} 需要你的輸入。"),
    ("{name} finished running.", "{name} 已執行完畢。"),
    (
        "Muqun push notifications are connected.",
        "Muqun 推播通知已連線。",
    ),
    // -- API error messages --------------------------------------------------
    ("Expo push service request failed", "Expo 推播服務請求失敗"),
    (
        "Herdr did not return the created pane id",
        "Herdr 沒有回傳所建立面板的 id",
    ),
    ("Herdr is unavailable", "無法連線到 Herdr"),
    (
        "agent is not one this gateway offers; see GET /api/agents/catalog",
        "這個 Gateway 未提供該代理程式；請參閱 GET /api/agents/catalog",
    ),
    (
        "another pairing request is awaiting confirmation",
        "已有另一個配對請求正在等待確認",
    ),
    (
        "answer with an option number or a decision",
        "請以選項編號或決定作答",
    ),
    (
        "asset not found in a session workspace",
        "在這個工作階段的工作區中找不到該檔案",
    ),
    (
        "decision must be allow, allow_always, or deny",
        "decision 必須是 allow、allow_always 或 deny",
    ),
    ("device not found", "找不到這個裝置"),
    (
        "device_name must be at most 80 characters and contain no control characters",
        "device_name 最多 80 個字元，且不得包含控制字元",
    ),
    ("direction must be right or down", "direction 必須是 right 或 down"),
    (
        "executables and scripts are not accepted",
        "不接受可執行檔與指令碼",
    ),
    ("expected Bearer token", "需要 Bearer token"),
    (
        "expected a multipart/form-data body with a file field",
        "需要含有 file 欄位的 multipart/form-data 內容",
    ),
    (
        "failed to check pairing request limit",
        "無法檢查配對請求的次數上限",
    ),
    ("failed to lock device state", "無法鎖定裝置狀態"),
    (
        "failed to lock pending pairing state",
        "無法鎖定待處理的配對狀態",
    ),
    ("failed to lock push token state", "無法鎖定推播 token 狀態"),
    ("failed to lock the asset index", "無法鎖定檔案索引"),
    ("failed to read the asset", "無法讀取這個檔案"),
    (
        "failed to remove push notification registration",
        "無法移除推播通知的註冊",
    ),
    ("failed to revoke the device token", "無法撤銷這個裝置的 token"),
    (
        "failed to save push notification registration",
        "無法儲存推播通知的註冊",
    ),
    ("failed to save the new device token", "無法儲存新的裝置 token"),
    ("failed to store the upload", "無法儲存上傳的檔案"),
    ("format must be text or ansi", "format 必須是 text 或 ansi"),
    ("invalid Authorization header", "Authorization 標頭無效"),
    ("invalid pairing code", "配對碼無效"),
    ("invalid token", "token 無效"),
    ("keys must contain 1 to 32 entries", "keys 必須包含 1 到 32 個項目"),
    ("missing Authorization header", "缺少 Authorization 標頭"),
    ("mode must be on, off, or toggle", "mode 必須是 on、off 或 toggle"),
    ("no pending pairing request", "沒有待處理的配對請求"),
    (
        "only png, jpeg, gif, webp, and heic images are accepted",
        "只接受 png、jpeg、gif、webp 與 heic 圖片",
    ),
    (
        "pairing code expired; request a new code",
        "配對碼已過期，請重新索取新的配對碼",
    ),
    ("platform must be ios or android", "platform 必須是 ios 或 android"),
    (
        "repo_path is not a git checkout, so a branch cannot be made in it",
        "repo_path 不是 git 工作目錄，因此無法在其中建立分支",
    ),
    (
        "repo_path must be a directory inside a workspace this session has open",
        "repo_path 必須是這個工作階段已開啟的工作區底下的目錄",
    ),
    (
        "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        "request_id 必須是 1 到 80 個字元，且只能使用英文字母、數字、點、底線或連字號",
    ),
    ("session not found", "找不到這個工作階段"),
    (
        "source must be visible, recent, recent-unwrapped, or detection",
        "source 必須是 visible、recent、recent-unwrapped 或 detection",
    ),
    (
        "startup_timeout_ms must be between 3001 and 300000",
        "startup_timeout_ms 必須介於 3001 與 300000 之間",
    ),
    ("text must be at most 65536 bytes", "text 最多 65536 個位元組"),
    (
        "the agent no longer has that request pending",
        "代理程式已不再等待這個請求",
    ),
    ("the asset is larger than 10 MiB", "這個檔案超過 10 MiB"),
    ("the file field is empty", "file 欄位是空的"),
    ("the file field must carry a filename", "file 欄位必須帶有檔名"),
    ("the pane is not waiting on an approval", "這個面板並未在等待核准"),
    (
        "the pane is waiting on a different approval",
        "這個面板正在等待的是另一個核准",
    ),
    ("the upload must be at most 25 MiB", "上傳的檔案最多 25 MiB"),
    (
        "this approval has no option with that number",
        "這個核准沒有該編號的選項",
    ),
    (
        "this approval offers no option with that meaning",
        "這個核准沒有代表該決定的選項",
    ),
    ("token must be an Expo push token", "token 必須是 Expo 推播 token"),
    (
        "too many pairing requests; try again later",
        "配對請求次數過多，請稍後再試",
    ),
    (
        "workspace_label must be at most 120 printable characters",
        "workspace_label 最多 120 個可列印字元",
    ),
    // -- branch names --------------------------------------------------------
    ("branch_name must not be empty", "branch_name 不得為空"),
    (
        "branch_name must be at most 200 characters",
        "branch_name 最多 200 個字元",
    ),
    (
        "branch_name may only contain letters, digits, dot, underscore, dash and slash",
        "branch_name 只能包含英文字母、數字、點、底線、連字號與斜線",
    ),
    ("branch_name must not contain ..", "branch_name 不得包含 .."),
    (
        "branch_name must not start with a dash",
        "branch_name 不得以連字號開頭",
    ),
    (
        "branch_name must not have an empty path segment or a segment starting or ending with a dot",
        "branch_name 不得有空的路徑片段，片段也不得以點開頭或結尾",
    ),
    (
        "branch_name must not end with .lock",
        "branch_name 不得以 .lock 結尾",
    ),
];

#[cfg(test)]
mod tests {
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
    fn the_two_codes_are_the_literals_the_app_and_the_site_already_use() {
        // Not `zh-Hant`, not `zh-Hant-TW`, not `zh_TW`. The app names its
        // catalog directory and persists its setting with these exact strings,
        // so a change here silently un-localizes every client.
        assert_eq!(Locale::En.as_str(), "en");
        assert_eq!(Locale::ZhTw.as_str(), "zh-TW");
        assert_eq!(Locale::default(), Locale::En);
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
        // The highest weight the gateway can serve wins, not the first tag.
        assert_eq!(
            Locale::from_headers(&headers(&[(
                "accept-language",
                "fr;q=1.0,en;q=0.4,zh-TW;q=0.9"
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
    fn traditional_tags_fold_onto_zh_tw_and_simplified_ones_never_do() {
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
        // Serving Traditional to a Simplified reader is a worse answer than
        // serving English, so these fall through rather than folding. Bare `zh`
        // does not say which script it wants, which is the whole question.
        for tag in ["zh-Hans", "zh-CN", "zh-cn", "zh-SG", "zh-Hans-CN", "zh"] {
            assert_eq!(Locale::from_code(tag), None, "{tag}");
        }
        assert_eq!(Locale::from_code("en-US"), Some(Locale::En));
        assert_eq!(Locale::from_code("en-GB"), Some(Locale::En));
    }

    #[test]
    fn a_message_with_no_translation_falls_back_to_the_english_string() {
        assert_eq!(t(Locale::ZhTw, "Deny"), "拒絕");
        assert_eq!(
            t(Locale::ZhTw, "a sentence nobody has translated yet"),
            "a sentence nobody has translated yet"
        );
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
    fn the_catalog_translates_each_english_string_exactly_once() {
        for (position, (english, translated)) in ZH_TW.iter().enumerate() {
            assert!(!english.is_empty(), "an empty key matches nothing");
            assert_ne!(english, translated, "{english:?} is not translated");
            assert!(
                !ZH_TW[..position]
                    .iter()
                    .any(|(earlier, _)| earlier == english),
                "{english:?} appears twice, so one of the two is dead"
            );
        }
    }

    #[test]
    fn every_slot_in_a_source_string_survives_into_its_translation() {
        // A translator who drops `{name}` produces a push with no agent in it,
        // which reads as a bug rather than as a typo.
        for (english, translated) in ZH_TW {
            for slot in ["{index}", "{action}", "{name}"] {
                assert_eq!(
                    english.contains(slot),
                    translated.contains(slot),
                    "{english:?} and its translation disagree about {slot}"
                );
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
}
