//! Which language the gateway is answering in, and the table it answers from.
//!
//! Eleven locales exist and their codes are literals, not a naming scheme:
//! `en`, `zh-TW`, `zh-CN`, `ja`, `ko`, `de`, `fr`, `es`, `pt`, `ru`, `vi`.
//! Lowercase primary tag, and where there is a region, a hyphen and an
//! uppercase region. The app already uses those exact strings as the names of
//! its catalog directories, as the value it persists when the user picks a
//! language, and as the value it puts on the wire -- so a gateway that answered
//! `zh-Hant`, `zh-Hans`, `zh_TW` or `pt-BR` would be answering with a code no
//! client has a catalog for. There is deliberately no alias for any code
//! anywhere in this file's *output*; tolerance lives only in
//! [`Locale::from_code`], which reads what a client sent and never writes.
//!
//! Chinese is the only language split by script, and it is therefore the only
//! one with a rule of its own below: two tables, `zh-TW` and `zh-CN`, and
//! neither ever stands in for the other. The other eight are one catalog per
//! language: `pt` answers Brazil and Portugal, `es` answers Spain and Latin
//! America, `vi` answers every Vietnamese reader.
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
/// The set is closed on purpose. Adding a variant means adding a table below
/// and naming it in [`Locale::catalog`], and [`Locale::as_str`] is the only
/// place a code is ever spelled. [`Locale::ALL`] exists so that the tests can
/// hold every language to the same invariants without a list of their own that
/// could fall behind this one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Locale {
    #[default]
    En,
    ZhTw,
    ZhCn,
    Ja,
    Ko,
    De,
    Fr,
    Es,
    Pt,
    Ru,
    Vi,
    Th,
}

impl Locale {
    /// Every language, in the order the app's picker lists them.
    ///
    /// `cfg(test)` because the tests are, for now, its only reader: nothing the
    /// gateway serves enumerates its languages, it only answers in one of them.
    /// Drop the attribute the day a capabilities endpoint wants to advertise
    /// the list -- the constant is the right shape for it either way.
    #[cfg(test)]
    pub const ALL: &'static [Locale] = &[
        Locale::En,
        Locale::ZhTw,
        Locale::ZhCn,
        Locale::Ja,
        Locale::Ko,
        Locale::De,
        Locale::Fr,
        Locale::Es,
        Locale::Pt,
        Locale::Ru,
        Locale::Vi,
        Locale::Th,
    ];

    /// The wire spelling. These strings are shared verbatim with the app and
    /// the marketing site; nothing may normalize, case-fold or "improve" them.
    pub fn as_str(self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::ZhTw => "zh-TW",
            Locale::ZhCn => "zh-CN",
            Locale::Ja => "ja",
            Locale::Ko => "ko",
            Locale::De => "de",
            Locale::Fr => "fr",
            Locale::Es => "es",
            Locale::Pt => "pt",
            Locale::Ru => "ru",
            Locale::Vi => "vi",
            Locale::Th => "th",
        }
    }

    /// The table this locale is answered from, empty for the source language.
    ///
    /// English has no table because English is the key: `t` returns the source
    /// string unchanged, which is both the translation and the fallback.
    fn catalog(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Locale::En => &[],
            Locale::ZhTw => ZH_TW,
            Locale::ZhCn => ZH_CN,
            Locale::Ja => JA,
            Locale::Ko => KO,
            Locale::De => DE,
            Locale::Fr => FR,
            Locale::Es => ES,
            Locale::Pt => PT,
            Locale::Ru => RU,
            Locale::Vi => VI,
            Locale::Th => TH,
        }
    }

    /// The locale a single BCP-47 tag asks for, if it is one the gateway can
    /// serve.
    ///
    /// Chinese is split by script, so a `zh` tag is read by
    /// [`Locale::chinese`]: a script subtag decides outright, a region decides
    /// when there is no script, and a tag that says neither is Simplified.
    /// The two Chinese tables never substitute for each other -- serving
    /// Traditional to a Simplified reader, or the reverse, is a worse answer
    /// than serving English would be, which is why the rule is spelled out
    /// instead of "whichever Chinese we have".
    ///
    /// Every other language is served on its primary subtag alone, whatever
    /// region follows: `de-AT`, `fr-CA`, `pt-BR`, `pt-PT`, `es-419`, `es-MX`
    /// and `vi-VN` all have exactly one catalog here to land in, and a
    /// Brazilian reader served the `pt` table is still being served
    /// Portuguese. Splitting any of them would mean emitting a code the app
    /// has no catalog directory for.
    ///
    /// A subtag is letters and digits, nothing else. A value with anything
    /// else in it -- `zh-TW;q=0.9` pasted whole into the app header, a stray
    /// emoji -- is not a language tag and is read as none, which matters now
    /// that bare `zh` has an answer: the junk after a mangled `zh-` would
    /// otherwise be swallowed and the request served Simplified on the
    /// strength of two letters.
    pub fn from_code(code: &str) -> Option<Self> {
        let subtags: Vec<String> = code
            .trim()
            .split(['-', '_'])
            .filter(|subtag| !subtag.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        let (primary, rest) = subtags.split_first()?;
        if !subtags
            .iter()
            .all(|subtag| subtag.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        {
            return None;
        }
        match primary.as_str() {
            "en" => Some(Locale::En),
            "zh" => Some(Locale::chinese(rest)),
            "ja" => Some(Locale::Ja),
            "ko" => Some(Locale::Ko),
            "de" => Some(Locale::De),
            "fr" => Some(Locale::Fr),
            "es" => Some(Locale::Es),
            "pt" => Some(Locale::Pt),
            "ru" => Some(Locale::Ru),
            "vi" => Some(Locale::Vi),
            "th" => Some(Locale::Th),
            _ => None,
        }
    }

    /// Which Chinese table the subtags after `zh` ask for.
    ///
    /// Script outranks region, because the script is the thing the two tables
    /// differ by and a region is only a hint about it: `zh-Hans-TW` is a
    /// Simplified reader who happens to be in Taiwan and `zh-Hant-CN` a
    /// Traditional reader on the mainland, and both get the script they named.
    /// With no script, the regions that read Traditional -- `TW`, `HK`, `MO`
    /// -- select `zh-TW`, and `CN`, `SG` and `MY` select `zh-CN`.
    ///
    /// Bare `zh`, and `zh` with a region that says nothing about script, is
    /// Simplified. That is what the tag means in practice: it is where the
    /// large majority of Chinese readers are, it is what every likely-subtags
    /// table expands `zh` to, and it is what an operating system reports for
    /// a mainland user who never picked anything more specific.
    fn chinese(subtags: &[String]) -> Self {
        let by_script = subtags.iter().find_map(|subtag| match subtag.as_str() {
            "hant" => Some(Locale::ZhTw),
            "hans" => Some(Locale::ZhCn),
            _ => None,
        });
        let by_region = subtags.iter().find_map(|subtag| match subtag.as_str() {
            "tw" | "hk" | "mo" => Some(Locale::ZhTw),
            "cn" | "sg" | "my" => Some(Locale::ZhCn),
            _ => None,
        });
        by_script.or(by_region).unwrap_or(Locale::ZhCn)
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
    locale
        .catalog()
        .iter()
        .find(|(english, _)| *english == source)
        .map_or(source, |(_, translated)| *translated)
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

// The catalogs.
//
// One table per language, all keyed by the same English source strings and all
// carrying the same seventy-nine entries in the same order, so that a diff
// between two of them is a diff of wording and nothing else. The tests hold
// every table to the same invariants -- no duplicate key, no entry left as its
// English source, every `{slot}` surviving into the translation -- by walking
// `Locale::ALL`, so a table added without being wired into `Locale::catalog`
// simply never gets checked, and one wired in badly fails immediately.
//
// Three rules apply to all of them:
//
//  * **API vocabulary embedded in a message is not translated.** `allow`,
//    `allow_always`, `deny`, `visible`, `recent`, `recent-unwrapped`,
//    `detection`, every field name, and the route `GET /api/agents/catalog` are
//    values a client sends back to us. Only the sentence around them moves.
//  * **Product names stay in Latin script.** Herdr, Gateway and Expo are names,
//    not words. Muqun is the exception in ZH_TW, ZH_CN and JA: the app is
//    called 牧群 there, matching the localisation already published on
//    osuki.dev, so those three tables spell it that way instead of leaving it
//    Latin.
//  * **Each table agrees with the app catalog of the same language.** The two
//    halves are read by the same person on the same screen: the gateway writes
//    the approval prompt, the app writes the button under it.
//
// Each language lives in its own file below, so a translation diff is a diff
// of one language. To add one: add the variant, its `as_str`, its `catalog()`
// arm, its entry in `Locale::ALL`, and a table file here; the coverage tests
// fail until the new table carries exactly the same English keys as the rest.

mod de;
mod es;
mod fr;
mod ja;
mod ko;
mod pt;
mod ru;
mod th;
mod vi;
mod zh_cn;
mod zh_tw;

use de::DE;
use es::ES;
use fr::FR;
use ja::JA;
use ko::KO;
use pt::PT;
use ru::RU;
use th::TH;
use vi::VI;
use zh_cn::ZH_CN;
use zh_tw::ZH_TW;

#[cfg(test)]
mod tests;
