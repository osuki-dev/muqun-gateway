//! Which language the gateway is answering in, and the table it answers from.
//!
//! Eight locales exist and their codes are literals, not a naming scheme:
//! `en`, `zh-TW`, `ja`, `ko`, `de`, `fr`, `es`, `pt`. Lowercase primary tag,
//! and where there is a region, a hyphen and an uppercase region. The app
//! already uses those exact strings as the names of its catalog directories, as
//! the value it persists when the user picks a language, and as the value it
//! puts on the wire -- so a gateway that answered `zh-Hant`, `zh_TW` or `pt-BR`
//! would be answering with a code no client has a catalog for. There is
//! deliberately no alias for any code anywhere in this file's *output*;
//! tolerance lives only in [`Locale::from_code`], which reads what a client
//! sent and never writes.
//!
//! Chinese is the only language split by script, and it is therefore the only
//! one with a rule of its own below. The other six are one catalog per
//! language: `pt` answers Brazil and Portugal, `es` answers Spain and Latin
//! America.
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
    Ja,
    Ko,
    De,
    Fr,
    Es,
    Pt,
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
        Locale::Ja,
        Locale::Ko,
        Locale::De,
        Locale::Fr,
        Locale::Es,
        Locale::Pt,
    ];

    /// The wire spelling. These strings are shared verbatim with the app and
    /// the marketing site; nothing may normalize, case-fold or "improve" them.
    pub fn as_str(self) -> &'static str {
        match self {
            Locale::En => "en",
            Locale::ZhTw => "zh-TW",
            Locale::Ja => "ja",
            Locale::Ko => "ko",
            Locale::De => "de",
            Locale::Fr => "fr",
            Locale::Es => "es",
            Locale::Pt => "pt",
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
            Locale::Ja => JA,
            Locale::Ko => KO,
            Locale::De => DE,
            Locale::Fr => FR,
            Locale::Es => ES,
            Locale::Pt => PT,
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
    ///
    /// Every other language is served on its primary subtag alone, whatever
    /// region follows: `de-AT`, `fr-CA`, `pt-BR`, `pt-PT`, `es-419` and
    /// `es-MX` all have exactly one catalog here to land in, and a Brazilian
    /// reader served the `pt` table is still being served Portuguese. Splitting
    /// any of them would mean emitting a code the app has no catalog directory
    /// for.
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
            "ja" => Some(Locale::Ja),
            "ko" => Some(Locale::Ko),
            "de" => Some(Locale::De),
            "fr" => Some(Locale::Fr),
            "es" => Some(Locale::Es),
            "pt" => Some(Locale::Pt),
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
// carrying the same seventy-five entries in the same order, so that a diff
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
//  * **Product names stay in Latin script.** Muqun, Herdr, Gateway and Expo are
//    names, not words.
//  * **Each table agrees with the app catalog of the same language.** The two
//    halves are read by the same person on the same screen: the gateway writes
//    the approval prompt, the app writes the button under it.
//
/// Taiwanese-normative Traditional Chinese, keyed by the English source.
///
/// The register is the island's, not Simplified Chinese transliterated: 設定,
/// 終端機, 檔案, 伺服器, 網路, 儲存, 預設, 程式碼, 連線. The app's own catalog
/// is the other half of this vocabulary and these agree with it: 核准, 拒絕,
/// 代理程式, 面板, 工作區, 工作階段, 配對 -- and Gateway, which stays in Latin
/// script because it is the product's name.
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
    ("failed to read recent agent activity", "無法讀取最近的代理程式活動"),
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

/// Japanese, keyed by the English source.
///
/// The register is a Japanese developer tool: noun phrases for labels and
/// ですます for whole sentences, never mixed within one string. Katakana
/// loanwords where a Japanese developer uses them -- パネル, セッション,
/// ターミナル, エージェント, サーバー -- rather than kanji calques nobody says.
///
/// `pane` and `panel` are one word here, パネル, because they are one thing in
/// the product. The app catalog makes the same call, so the two halves of a
/// sentence a reader sees split across the two surfaces agree.
const JA: &[(&str, &str)] = &[
    // -- approval labels the gateway writes for itself -----------------------
    ("Approve", "承認"),
    ("Approve and don't ask again", "承認して今後は確認しない"),
    ("Deny", "拒否"),
    ("Option {index}", "選択肢 {index}"),
    ("Allow {action}?", "{action} を許可しますか？"),
    // -- push notifications --------------------------------------------------
    ("Agent", "エージェント"),
    ("Approval needed", "承認が必要"),
    ("Agent blocked", "エージェントが待機中"),
    ("Agent done", "エージェントが完了"),
    ("{name} is waiting for your approval.", "{name} が承認を待っています。"),
    ("{name} needs your input.", "{name} が入力を待っています。"),
    ("{name} finished running.", "{name} の実行が完了しました。"),
    ("Muqun push notifications are connected.", "Muqun のプッシュ通知が接続されました。"),
    // -- API error messages --------------------------------------------------
    ("Expo push service request failed", "Expo プッシュサービスへのリクエストが失敗しました"),
    ("Herdr did not return the created pane id", "Herdr が作成したパネルの id を返しませんでした"),
    ("Herdr is unavailable", "Herdr に接続できません"),
    (
        "agent is not one this gateway offers; see GET /api/agents/catalog",
        "agent はこの Gateway が提供していないものです。GET /api/agents/catalog を参照してください",
    ),
    ("another pairing request is awaiting confirmation", "別のペアリング要求が確認待ちです"),
    ("answer with an option number or a decision", "選択肢の番号か決定のいずれかで回答してください"),
    ("asset not found in a session workspace", "セッションのワークスペースにそのファイルが見つかりません"),
    (
        "decision must be allow, allow_always, or deny",
        "decision は allow、allow_always、deny のいずれかにしてください",
    ),
    ("device not found", "デバイスが見つかりません"),
    (
        "device_name must be at most 80 characters and contain no control characters",
        "device_name は 80 文字以内で、制御文字を含められません",
    ),
    ("direction must be right or down", "direction は right または down にしてください"),
    ("executables and scripts are not accepted", "実行ファイルとスクリプトは受け付けません"),
    ("expected Bearer token", "Bearer トークンが必要です"),
    (
        "expected a multipart/form-data body with a file field",
        "file フィールドを含む multipart/form-data のボディが必要です",
    ),
    ("failed to check pairing request limit", "ペアリング要求の上限を確認できませんでした"),
    ("failed to lock device state", "デバイスの状態をロックできませんでした"),
    ("failed to lock pending pairing state", "保留中のペアリング状態をロックできませんでした"),
    ("failed to lock push token state", "プッシュトークンの状態をロックできませんでした"),
    ("failed to lock the asset index", "ファイルインデックスをロックできませんでした"),
    ("failed to read recent agent activity", "最近のエージェントの動きを読み取れませんでした"),
    ("failed to read the asset", "ファイルを読み取れませんでした"),
    ("failed to remove push notification registration", "プッシュ通知の登録を削除できませんでした"),
    ("failed to revoke the device token", "デバイストークンを失効できませんでした"),
    ("failed to save push notification registration", "プッシュ通知の登録を保存できませんでした"),
    ("failed to save the new device token", "新しいデバイストークンを保存できませんでした"),
    ("failed to store the upload", "アップロードされたファイルを保存できませんでした"),
    ("format must be text or ansi", "format は text または ansi にしてください"),
    ("invalid Authorization header", "Authorization ヘッダーが不正です"),
    ("invalid pairing code", "ペアリングコードが不正です"),
    ("invalid token", "トークンが不正です"),
    ("keys must contain 1 to 32 entries", "keys は 1〜32 個の項目を含めてください"),
    ("missing Authorization header", "Authorization ヘッダーがありません"),
    ("mode must be on, off, or toggle", "mode は on、off、toggle のいずれかにしてください"),
    ("no pending pairing request", "保留中のペアリング要求はありません"),
    (
        "only png, jpeg, gif, webp, and heic images are accepted",
        "png、jpeg、gif、webp、heic の画像のみ受け付けます",
    ),
    ("pairing code expired; request a new code", "ペアリングコードの有効期限が切れました。新しいコードを取得してください"),
    ("platform must be ios or android", "platform は ios または android にしてください"),
    (
        "repo_path is not a git checkout, so a branch cannot be made in it",
        "repo_path は git のチェックアウトではないため、その中にブランチを作成できません",
    ),
    (
        "repo_path must be a directory inside a workspace this session has open",
        "repo_path は、このセッションが開いているワークスペース内のディレクトリにしてください",
    ),
    (
        "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        "request_id は英字、数字、ドット、アンダースコア、ハイフンを使った 1〜80 文字にしてください",
    ),
    ("session not found", "セッションが見つかりません"),
    (
        "source must be visible, recent, recent-unwrapped, or detection",
        "source は visible、recent、recent-unwrapped、detection のいずれかにしてください",
    ),
    (
        "startup_timeout_ms must be between 3001 and 300000",
        "startup_timeout_ms は 3001 以上 300000 以下にしてください",
    ),
    ("text must be at most 65536 bytes", "text は 65536 バイト以内にしてください"),
    ("the agent no longer has that request pending", "エージェントはその要求をすでに待っていません"),
    ("the asset is larger than 10 MiB", "このファイルは 10 MiB を超えています"),
    ("the file field is empty", "file フィールドが空です"),
    ("the file field must carry a filename", "file フィールドにはファイル名が必要です"),
    ("the pane is not waiting on an approval", "このパネルは承認を待っていません"),
    ("the pane is waiting on a different approval", "このパネルは別の承認を待っています"),
    ("the upload must be at most 25 MiB", "アップロードは 25 MiB 以内にしてください"),
    ("this approval has no option with that number", "この承認にその番号の選択肢はありません"),
    ("this approval offers no option with that meaning", "この承認にその意味の選択肢はありません"),
    ("token must be an Expo push token", "token は Expo のプッシュトークンにしてください"),
    ("too many pairing requests; try again later", "ペアリング要求が多すぎます。しばらくしてからお試しください"),
    (
        "workspace_label must be at most 120 printable characters",
        "workspace_label は表示可能な文字で 120 文字以内にしてください",
    ),
    // -- branch names --------------------------------------------------------
    ("branch_name must not be empty", "branch_name は空にできません"),
    ("branch_name must be at most 200 characters", "branch_name は 200 文字以内にしてください"),
    (
        "branch_name may only contain letters, digits, dot, underscore, dash and slash",
        "branch_name には英字、数字、ドット、アンダースコア、ダッシュ、スラッシュのみ使えます",
    ),
    ("branch_name must not contain ..", "branch_name に .. は使えません"),
    ("branch_name must not start with a dash", "branch_name はダッシュで始められません"),
    (
        "branch_name must not have an empty path segment or a segment starting or ending with a dot",
        "branch_name に空のパスセグメントや、ドットで始まる・終わるセグメントは使えません",
    ),
    ("branch_name must not end with .lock", "branch_name の末尾に .lock は使えません"),
];

/// Korean, keyed by the English source.
///
/// 명사형 for buttons, labels and headings; 합니다체 for whole sentences and
/// for every API error. The first thirteen entries are notification and UI
/// labels and are therefore noun-final; the rest are errors and are not.
///
/// Where a slot lands in front of a particle the particle is written in the
/// `을(를)` form, because the final consonant of the substituted word is not
/// knowable here and guessing it is how a localised string starts reading as
/// a machine wrote it.
const KO: &[(&str, &str)] = &[
    // -- approval labels the gateway writes for itself -----------------------
    ("Approve", "승인"),
    ("Approve and don't ask again", "승인하고 다시 묻지 않기"),
    ("Deny", "거부"),
    ("Option {index}", "옵션 {index}"),
    ("Allow {action}?", "{action}을(를) 허용할까요?"),
    // -- push notifications --------------------------------------------------
    ("Agent", "에이전트"),
    ("Approval needed", "승인 필요"),
    ("Agent blocked", "에이전트 대기 중"),
    ("Agent done", "에이전트 완료"),
    ("{name} is waiting for your approval.", "{name}이(가) 승인을 기다리고 있습니다."),
    ("{name} needs your input.", "{name}에게 입력이 필요합니다."),
    ("{name} finished running.", "{name} 실행이 끝났습니다."),
    ("Muqun push notifications are connected.", "Muqun 푸시 알림이 연결되었습니다."),
    // -- API error messages --------------------------------------------------
    ("Expo push service request failed", "Expo 푸시 서비스 요청에 실패했습니다"),
    ("Herdr did not return the created pane id", "Herdr가 생성된 패널의 id를 반환하지 않았습니다"),
    ("Herdr is unavailable", "Herdr를 사용할 수 없습니다"),
    (
        "agent is not one this gateway offers; see GET /api/agents/catalog",
        "이 Gateway가 제공하는 agent가 아닙니다. GET /api/agents/catalog 참조",
    ),
    ("another pairing request is awaiting confirmation", "다른 페어링 요청이 확인을 기다리고 있습니다"),
    ("answer with an option number or a decision", "옵션 번호나 결정으로 응답하세요"),
    ("asset not found in a session workspace", "세션 워크스페이스에서 해당 파일을 찾을 수 없습니다"),
    (
        "decision must be allow, allow_always, or deny",
        "decision은 allow, allow_always 또는 deny여야 합니다",
    ),
    ("device not found", "기기를 찾을 수 없습니다"),
    (
        "device_name must be at most 80 characters and contain no control characters",
        "device_name은 최대 80자이며 제어 문자를 포함할 수 없습니다",
    ),
    ("direction must be right or down", "direction은 right 또는 down이어야 합니다"),
    ("executables and scripts are not accepted", "실행 파일과 스크립트는 받지 않습니다"),
    ("expected Bearer token", "Bearer 토큰이 필요합니다"),
    (
        "expected a multipart/form-data body with a file field",
        "file 필드가 있는 multipart/form-data 본문이 필요합니다",
    ),
    ("failed to check pairing request limit", "페어링 요청 한도를 확인하지 못했습니다"),
    ("failed to lock device state", "기기 상태를 잠그지 못했습니다"),
    ("failed to lock pending pairing state", "대기 중인 페어링 상태를 잠그지 못했습니다"),
    ("failed to lock push token state", "푸시 토큰 상태를 잠그지 못했습니다"),
    ("failed to lock the asset index", "파일 색인을 잠그지 못했습니다"),
    ("failed to read recent agent activity", "최근 에이전트 활동을 읽지 못했습니다"),
    ("failed to read the asset", "파일을 읽지 못했습니다"),
    ("failed to remove push notification registration", "푸시 알림 등록을 삭제하지 못했습니다"),
    ("failed to revoke the device token", "기기 토큰을 폐기하지 못했습니다"),
    ("failed to save push notification registration", "푸시 알림 등록을 저장하지 못했습니다"),
    ("failed to save the new device token", "새 기기 토큰을 저장하지 못했습니다"),
    ("failed to store the upload", "업로드한 파일을 저장하지 못했습니다"),
    ("format must be text or ansi", "format은 text 또는 ansi여야 합니다"),
    ("invalid Authorization header", "Authorization 헤더가 올바르지 않습니다"),
    ("invalid pairing code", "페어링 코드가 올바르지 않습니다"),
    ("invalid token", "토큰이 올바르지 않습니다"),
    ("keys must contain 1 to 32 entries", "keys에는 항목이 1개에서 32개까지 있어야 합니다"),
    ("missing Authorization header", "Authorization 헤더가 없습니다"),
    ("mode must be on, off, or toggle", "mode는 on, off 또는 toggle이어야 합니다"),
    ("no pending pairing request", "대기 중인 페어링 요청이 없습니다"),
    (
        "only png, jpeg, gif, webp, and heic images are accepted",
        "png, jpeg, gif, webp, heic 이미지만 받습니다",
    ),
    ("pairing code expired; request a new code", "페어링 코드가 만료되었습니다. 새 코드를 요청하세요"),
    ("platform must be ios or android", "platform은 ios 또는 android여야 합니다"),
    (
        "repo_path is not a git checkout, so a branch cannot be made in it",
        "repo_path가 git 체크아웃이 아니어서 그 안에 브랜치를 만들 수 없습니다",
    ),
    (
        "repo_path must be a directory inside a workspace this session has open",
        "repo_path는 이 세션이 열어 둔 워크스페이스 안의 디렉터리여야 합니다",
    ),
    (
        "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        "request_id는 영문자, 숫자, 점, 밑줄, 하이픈으로 이루어진 1~80자여야 합니다",
    ),
    ("session not found", "세션을 찾을 수 없습니다"),
    (
        "source must be visible, recent, recent-unwrapped, or detection",
        "source는 visible, recent, recent-unwrapped 또는 detection이어야 합니다",
    ),
    (
        "startup_timeout_ms must be between 3001 and 300000",
        "startup_timeout_ms는 3001에서 300000 사이여야 합니다",
    ),
    ("text must be at most 65536 bytes", "text는 최대 65536바이트여야 합니다"),
    ("the agent no longer has that request pending", "에이전트가 더 이상 그 요청을 기다리고 있지 않습니다"),
    ("the asset is larger than 10 MiB", "파일이 10 MiB를 넘습니다"),
    ("the file field is empty", "file 필드가 비어 있습니다"),
    ("the file field must carry a filename", "file 필드에 파일 이름이 있어야 합니다"),
    ("the pane is not waiting on an approval", "이 패널은 승인을 기다리고 있지 않습니다"),
    ("the pane is waiting on a different approval", "이 패널이 기다리는 것은 다른 승인입니다"),
    ("the upload must be at most 25 MiB", "업로드는 최대 25 MiB까지 가능합니다"),
    ("this approval has no option with that number", "이 승인에는 그 번호의 옵션이 없습니다"),
    ("this approval offers no option with that meaning", "이 승인에는 그 뜻에 해당하는 옵션이 없습니다"),
    ("token must be an Expo push token", "token은 Expo 푸시 토큰이어야 합니다"),
    ("too many pairing requests; try again later", "페어링 요청이 너무 많습니다. 잠시 후 다시 시도하세요"),
    (
        "workspace_label must be at most 120 printable characters",
        "workspace_label은 출력 가능한 문자로 최대 120자여야 합니다",
    ),
    // -- branch names --------------------------------------------------------
    ("branch_name must not be empty", "branch_name은 비워 둘 수 없습니다"),
    ("branch_name must be at most 200 characters", "branch_name은 최대 200자여야 합니다"),
    (
        "branch_name may only contain letters, digits, dot, underscore, dash and slash",
        "branch_name에는 영문자, 숫자, 점, 밑줄, 대시, 슬래시만 쓸 수 있습니다",
    ),
    ("branch_name must not contain ..", "branch_name에는 ..을 넣을 수 없습니다"),
    ("branch_name must not start with a dash", "branch_name은 대시로 시작할 수 없습니다"),
    (
        "branch_name must not have an empty path segment or a segment starting or ending with a dot",
        "branch_name에는 빈 경로 세그먼트나 점으로 시작 또는 끝나는 세그먼트가 있을 수 없습니다",
    ),
    ("branch_name must not end with .lock", "branch_name은 .lock으로 끝날 수 없습니다"),
];

/// German, keyed by the English source.
///
/// du throughout, which is what German developer tools use, and infinitives
/// rather than imperatives for anything button-shaped. Nouns are capitalised
/// even inside the lower-case-initial API errors, because that is German and
/// not a style choice.
///
/// One deliberate asymmetry: the verb pair is zulassen / ablehnen, so an
/// inline permission prompt reads the way a German permission prompt reads,
/// while the noun is Freigabe. Genehmigen/Genehmigung would have matched
/// verb to noun and made the prompt sound like a municipal form.
const DE: &[(&str, &str)] = &[
    // -- approval labels the gateway writes for itself -----------------------
    ("Approve", "Zulassen"),
    ("Approve and don't ask again", "Zulassen und nicht mehr fragen"),
    ("Deny", "Ablehnen"),
    ("Option {index}", "Option Nr. {index}"),
    ("Allow {action}?", "{action} zulassen?"),
    // -- push notifications --------------------------------------------------
    ("Agent", "Der Agent"),
    ("Approval needed", "Freigabe erforderlich"),
    ("Agent blocked", "Agent wartet"),
    ("Agent done", "Agent fertig"),
    ("{name} is waiting for your approval.", "{name} wartet auf deine Freigabe."),
    ("{name} needs your input.", "{name} braucht deine Eingabe."),
    ("{name} finished running.", "{name} hat die Ausführung beendet."),
    ("Muqun push notifications are connected.", "Muqun-Push-Benachrichtigungen sind verbunden."),
    // -- API error messages --------------------------------------------------
    ("Expo push service request failed", "Anfrage an den Expo-Push-Dienst fehlgeschlagen"),
    (
        "Herdr did not return the created pane id",
        "Herdr hat die id des erstellten Panels nicht zurückgegeben",
    ),
    ("Herdr is unavailable", "Herdr ist nicht erreichbar"),
    (
        "agent is not one this gateway offers; see GET /api/agents/catalog",
        "agent ist keiner, den dieses Gateway anbietet; siehe GET /api/agents/catalog",
    ),
    (
        "another pairing request is awaiting confirmation",
        "eine andere Kopplungsanfrage wartet auf Bestätigung",
    ),
    (
        "answer with an option number or a decision",
        "antworte mit einer Optionsnummer oder einer Entscheidung",
    ),
    ("asset not found in a session workspace", "Datei in keinem Workspace einer Session gefunden"),
    (
        "decision must be allow, allow_always, or deny",
        "decision muss allow, allow_always oder deny sein",
    ),
    ("device not found", "Gerät nicht gefunden"),
    (
        "device_name must be at most 80 characters and contain no control characters",
        "device_name darf höchstens 80 Zeichen lang sein und keine Steuerzeichen enthalten",
    ),
    ("direction must be right or down", "direction muss right oder down sein"),
    (
        "executables and scripts are not accepted",
        "ausführbare Dateien und Skripte werden nicht akzeptiert",
    ),
    ("expected Bearer token", "Bearer-Token erwartet"),
    (
        "expected a multipart/form-data body with a file field",
        "erwartet wurde ein multipart/form-data-Body mit einem file-Feld",
    ),
    (
        "failed to check pairing request limit",
        "das Limit für Kopplungsanfragen konnte nicht geprüft werden",
    ),
    ("failed to lock device state", "der Gerätestatus konnte nicht gesperrt werden"),
    (
        "failed to lock pending pairing state",
        "der Status der ausstehenden Kopplung konnte nicht gesperrt werden",
    ),
    ("failed to lock push token state", "der Status des Push-Tokens konnte nicht gesperrt werden"),
    ("failed to lock the asset index", "der Dateiindex konnte nicht gesperrt werden"),
    ("failed to read recent agent activity", "die letzten Agent-Aktivitäten konnten nicht gelesen werden"),
    ("failed to read the asset", "die Datei konnte nicht gelesen werden"),
    (
        "failed to remove push notification registration",
        "die Registrierung für Push-Benachrichtigungen konnte nicht entfernt werden",
    ),
    ("failed to revoke the device token", "das Token des Geräts konnte nicht widerrufen werden"),
    (
        "failed to save push notification registration",
        "die Registrierung für Push-Benachrichtigungen konnte nicht gespeichert werden",
    ),
    ("failed to save the new device token", "das neue Gerätetoken konnte nicht gespeichert werden"),
    ("failed to store the upload", "der Upload konnte nicht gespeichert werden"),
    ("format must be text or ansi", "format muss text oder ansi sein"),
    ("invalid Authorization header", "ungültiger Authorization-Header"),
    ("invalid pairing code", "ungültiger Kopplungscode"),
    ("invalid token", "ungültiges Token"),
    ("keys must contain 1 to 32 entries", "keys muss 1 bis 32 Einträge enthalten"),
    ("missing Authorization header", "fehlender Authorization-Header"),
    ("mode must be on, off, or toggle", "mode muss on, off oder toggle sein"),
    ("no pending pairing request", "keine ausstehende Kopplungsanfrage"),
    (
        "only png, jpeg, gif, webp, and heic images are accepted",
        "nur png-, jpeg-, gif-, webp- und heic-Bilder werden akzeptiert",
    ),
    (
        "pairing code expired; request a new code",
        "Kopplungscode abgelaufen; fordere einen neuen Code an",
    ),
    ("platform must be ios or android", "platform muss ios oder android sein"),
    (
        "repo_path is not a git checkout, so a branch cannot be made in it",
        "repo_path ist kein git-Checkout, daher lässt sich darin kein Branch anlegen",
    ),
    (
        "repo_path must be a directory inside a workspace this session has open",
        "repo_path muss ein Verzeichnis in einem Workspace sein, den diese Session geöffnet hat",
    ),
    (
        "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        "request_id muss 1-80 Zeichen lang sein und darf nur Buchstaben, Ziffern, Punkt, Unterstrich oder Bindestrich enthalten",
    ),
    ("session not found", "Session nicht gefunden"),
    (
        "source must be visible, recent, recent-unwrapped, or detection",
        "source muss visible, recent, recent-unwrapped oder detection sein",
    ),
    (
        "startup_timeout_ms must be between 3001 and 300000",
        "startup_timeout_ms muss zwischen 3001 und 300000 liegen",
    ),
    ("text must be at most 65536 bytes", "text darf höchstens 65536 Bytes groß sein"),
    (
        "the agent no longer has that request pending",
        "beim Agenten steht diese Anfrage nicht mehr aus",
    ),
    ("the asset is larger than 10 MiB", "die Datei ist größer als 10 MiB"),
    ("the file field is empty", "das file-Feld ist leer"),
    ("the file field must carry a filename", "das file-Feld muss einen Dateinamen enthalten"),
    ("the pane is not waiting on an approval", "das Panel wartet nicht auf eine Freigabe"),
    ("the pane is waiting on a different approval", "das Panel wartet auf eine andere Freigabe"),
    ("the upload must be at most 25 MiB", "der Upload darf höchstens 25 MiB groß sein"),
    (
        "this approval has no option with that number",
        "diese Freigabe hat keine Option mit dieser Nummer",
    ),
    (
        "this approval offers no option with that meaning",
        "diese Freigabe bietet keine Option mit dieser Bedeutung",
    ),
    ("token must be an Expo push token", "token muss ein Expo-Push-Token sein"),
    (
        "too many pairing requests; try again later",
        "zu viele Kopplungsanfragen; versuche es später erneut",
    ),
    (
        "workspace_label must be at most 120 printable characters",
        "workspace_label darf höchstens 120 druckbare Zeichen lang sein",
    ),
    // -- branch names --------------------------------------------------------
    ("branch_name must not be empty", "branch_name darf nicht leer sein"),
    (
        "branch_name must be at most 200 characters",
        "branch_name darf höchstens 200 Zeichen lang sein",
    ),
    (
        "branch_name may only contain letters, digits, dot, underscore, dash and slash",
        "branch_name darf nur Buchstaben, Ziffern, Punkt, Unterstrich, Bindestrich und Schrägstrich enthalten",
    ),
    ("branch_name must not contain ..", "branch_name darf .. nicht enthalten"),
    (
        "branch_name must not start with a dash",
        "branch_name darf nicht mit einem Bindestrich beginnen",
    ),
    (
        "branch_name must not have an empty path segment or a segment starting or ending with a dot",
        "branch_name darf kein leeres Pfadsegment haben und kein Segment, das mit einem Punkt beginnt oder endet",
    ),
    ("branch_name must not end with .lock", "branch_name darf nicht auf .lock enden"),
];

/// French, keyed by the English source.
///
/// vous for sentences, bare infinitives for buttons. French typography is
/// load-bearing and is written out properly here: U+202F narrow no-break
/// space before `: ; ? !`, and typographic apostrophes rather than straight
/// ones.
///
/// pane and panel are both « panneau ». « volet » reads as a collapsible
/// sidebar and « sous-fenêtre » is Apple-only vocabulary; neither is what
/// this is. Pairing is appairer / appairage, and its opposite is dissocier,
/// because « désappairer » is not a word anyone says.
const FR: &[(&str, &str)] = &[
    // -- approval labels the gateway writes for itself -----------------------
    ("Approve", "Approuver"),
    ("Approve and don't ask again", "Approuver et ne plus demander"),
    ("Deny", "Refuser"),
    ("Option {index}", "Option n° {index}"),
    ("Allow {action}?", "Autoriser {action} ?"),
    // -- push notifications --------------------------------------------------
    ("Agent", "Agent de codage"),
    ("Approval needed", "Approbation requise"),
    ("Agent blocked", "Agent bloqué"),
    ("Agent done", "Exécution terminée"),
    ("{name} is waiting for your approval.", "{name} attend votre approbation."),
    ("{name} needs your input.", "{name} attend votre réponse."),
    ("{name} finished running.", "{name} a fini de s’exécuter."),
    ("Muqun push notifications are connected.", "Les notifications push de Muqun sont connectées."),
    // -- API error messages --------------------------------------------------
    ("Expo push service request failed", "Échec de la requête au service push Expo"),
    ("Herdr did not return the created pane id", "Herdr n’a pas renvoyé l’id du panneau créé"),
    ("Herdr is unavailable", "Herdr est indisponible"),
    (
        "agent is not one this gateway offers; see GET /api/agents/catalog",
        "cet agent n’est pas proposé par ce Gateway ; voir GET /api/agents/catalog",
    ),
    (
        "another pairing request is awaiting confirmation",
        "une autre demande d’appairage attend une confirmation",
    ),
    (
        "answer with an option number or a decision",
        "répondez avec un numéro d’option ou une décision",
    ),
    (
        "asset not found in a session workspace",
        "fichier introuvable dans un espace de travail de session",
    ),
    (
        "decision must be allow, allow_always, or deny",
        "decision doit être allow, allow_always ou deny",
    ),
    ("device not found", "appareil introuvable"),
    (
        "device_name must be at most 80 characters and contain no control characters",
        "device_name doit faire au plus 80 caractères et ne contenir aucun caractère de contrôle",
    ),
    ("direction must be right or down", "direction doit être right ou down"),
    (
        "executables and scripts are not accepted",
        "les exécutables et les scripts ne sont pas acceptés",
    ),
    ("expected Bearer token", "un token Bearer est attendu"),
    (
        "expected a multipart/form-data body with a file field",
        "un corps multipart/form-data avec un champ file est attendu",
    ),
    (
        "failed to check pairing request limit",
        "impossible de vérifier la limite de demandes d’appairage",
    ),
    ("failed to lock device state", "impossible de verrouiller l’état de l’appareil"),
    (
        "failed to lock pending pairing state",
        "impossible de verrouiller l’état de l’appairage en attente",
    ),
    ("failed to lock push token state", "impossible de verrouiller l’état du token push"),
    ("failed to lock the asset index", "impossible de verrouiller l’index des fichiers"),
    ("failed to read recent agent activity", "impossible de lire l’activité récente des agents"),
    ("failed to read the asset", "impossible de lire le fichier"),
    (
        "failed to remove push notification registration",
        "impossible de supprimer l’inscription aux notifications push",
    ),
    ("failed to revoke the device token", "impossible de révoquer le token de l’appareil"),
    (
        "failed to save push notification registration",
        "impossible d’enregistrer l’inscription aux notifications push",
    ),
    (
        "failed to save the new device token",
        "impossible d’enregistrer le nouveau token de l’appareil",
    ),
    ("failed to store the upload", "impossible de stocker le fichier envoyé"),
    ("format must be text or ansi", "format doit être text ou ansi"),
    ("invalid Authorization header", "en-tête Authorization invalide"),
    ("invalid pairing code", "code d’appairage invalide"),
    ("invalid token", "token invalide"),
    ("keys must contain 1 to 32 entries", "keys doit contenir de 1 à 32 entrées"),
    ("missing Authorization header", "en-tête Authorization manquant"),
    ("mode must be on, off, or toggle", "mode doit être on, off ou toggle"),
    ("no pending pairing request", "aucune demande d’appairage en attente"),
    (
        "only png, jpeg, gif, webp, and heic images are accepted",
        "seules les images png, jpeg, gif, webp et heic sont acceptées",
    ),
    (
        "pairing code expired; request a new code",
        "code d’appairage expiré ; demandez-en un nouveau",
    ),
    ("platform must be ios or android", "platform doit être ios ou android"),
    (
        "repo_path is not a git checkout, so a branch cannot be made in it",
        "repo_path n’est pas une copie de travail git, il est donc impossible d’y créer une branche",
    ),
    (
        "repo_path must be a directory inside a workspace this session has open",
        "repo_path doit être un dossier situé dans un espace de travail ouvert par cette session",
    ),
    (
        "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        "request_id doit faire de 1 à 80 caractères, avec uniquement des lettres, des chiffres, un point, un tiret bas ou un trait d’union",
    ),
    ("session not found", "session introuvable"),
    (
        "source must be visible, recent, recent-unwrapped, or detection",
        "source doit être visible, recent, recent-unwrapped ou detection",
    ),
    (
        "startup_timeout_ms must be between 3001 and 300000",
        "startup_timeout_ms doit être compris entre 3001 et 300000",
    ),
    ("text must be at most 65536 bytes", "text doit faire au plus 65536 octets"),
    ("the agent no longer has that request pending", "l’agent n’a plus cette demande en attente"),
    ("the asset is larger than 10 MiB", "le fichier dépasse 10 MiB"),
    ("the file field is empty", "le champ file est vide"),
    ("the file field must carry a filename", "le champ file doit comporter un nom de fichier"),
    ("the pane is not waiting on an approval", "ce panneau n’attend aucune approbation"),
    ("the pane is waiting on a different approval", "ce panneau attend une autre approbation"),
    ("the upload must be at most 25 MiB", "le fichier envoyé ne doit pas dépasser 25 MiB"),
    (
        "this approval has no option with that number",
        "cette approbation n’a aucune option portant ce numéro",
    ),
    (
        "this approval offers no option with that meaning",
        "cette approbation ne propose aucune option ayant ce sens",
    ),
    ("token must be an Expo push token", "token doit être un token push Expo"),
    (
        "too many pairing requests; try again later",
        "trop de demandes d’appairage ; réessayez plus tard",
    ),
    (
        "workspace_label must be at most 120 printable characters",
        "workspace_label doit faire au plus 120 caractères imprimables",
    ),
    // -- branch names --------------------------------------------------------
    ("branch_name must not be empty", "branch_name ne doit pas être vide"),
    ("branch_name must be at most 200 characters", "branch_name doit faire au plus 200 caractères"),
    (
        "branch_name may only contain letters, digits, dot, underscore, dash and slash",
        "branch_name ne peut contenir que des lettres, des chiffres, un point, un tiret bas, un tiret et une barre oblique",
    ),
    ("branch_name must not contain ..", "branch_name ne doit pas contenir .."),
    ("branch_name must not start with a dash", "branch_name ne doit pas commencer par un tiret"),
    (
        "branch_name must not have an empty path segment or a segment starting or ending with a dot",
        "branch_name ne doit pas comporter de segment de chemin vide, ni de segment commençant ou finissant par un point",
    ),
    ("branch_name must not end with .lock", "branch_name ne doit pas se terminer par .lock"),
];

/// Spanish, keyed by the English source.
///
/// One catalog for Spain and Latin America both, so the wording is neutral
/// by construction: tú and never vosotros, `archivo` rather than `fichero`,
/// `agregar` rather than `añadir`, and phrasing that sidesteps
/// ordenador/computadora entirely (`tu máquina`, `este dispositivo`).
///
/// `header Authorization` keeps the English noun on purpose -- `cabecera` is
/// peninsular and `encabezado` is American, and the loanword is what a
/// developer on either side actually says.
const ES: &[(&str, &str)] = &[
    // -- approval labels the gateway writes for itself -----------------------
    ("Approve", "Aprobar"),
    ("Approve and don't ask again", "Aprobar y no volver a preguntar"),
    ("Deny", "Denegar"),
    ("Option {index}", "Opción {index}"),
    ("Allow {action}?", "¿Permitir {action}?"),
    // -- push notifications --------------------------------------------------
    ("Agent", "Agente"),
    ("Approval needed", "Se necesita aprobación"),
    ("Agent blocked", "El agente está bloqueado"),
    ("Agent done", "El agente terminó"),
    ("{name} is waiting for your approval.", "{name} está esperando tu aprobación."),
    ("{name} needs your input.", "{name} necesita tu respuesta."),
    ("{name} finished running.", "{name} terminó de ejecutarse."),
    (
        "Muqun push notifications are connected.",
        "Las notificaciones push de Muqun están conectadas.",
    ),
    // -- API error messages --------------------------------------------------
    ("Expo push service request failed", "La solicitud al servicio push de Expo falló"),
    ("Herdr did not return the created pane id", "Herdr no devolvió el id del panel creado"),
    ("Herdr is unavailable", "Herdr no está disponible"),
    (
        "agent is not one this gateway offers; see GET /api/agents/catalog",
        "el agente indicado no está entre los que ofrece este gateway; consulta GET /api/agents/catalog",
    ),
    (
        "another pairing request is awaiting confirmation",
        "ya hay otra solicitud de vinculación esperando confirmación",
    ),
    (
        "answer with an option number or a decision",
        "responde con un número de opción o con una decisión",
    ),
    (
        "asset not found in a session workspace",
        "el archivo no está en ningún espacio de trabajo de la sesión",
    ),
    (
        "decision must be allow, allow_always, or deny",
        "decision debe ser allow, allow_always o deny",
    ),
    ("device not found", "dispositivo no encontrado"),
    (
        "device_name must be at most 80 characters and contain no control characters",
        "device_name debe tener 80 caracteres como máximo y no contener caracteres de control",
    ),
    ("direction must be right or down", "direction debe ser right o down"),
    ("executables and scripts are not accepted", "no se aceptan ejecutables ni scripts"),
    ("expected Bearer token", "se esperaba un token Bearer"),
    (
        "expected a multipart/form-data body with a file field",
        "se esperaba un cuerpo multipart/form-data con un campo file",
    ),
    (
        "failed to check pairing request limit",
        "no se pudo verificar el límite de solicitudes de vinculación",
    ),
    ("failed to lock device state", "no se pudo bloquear el estado del dispositivo"),
    (
        "failed to lock pending pairing state",
        "no se pudo bloquear el estado de la vinculación pendiente",
    ),
    ("failed to lock push token state", "no se pudo bloquear el estado del token push"),
    ("failed to lock the asset index", "no se pudo bloquear el índice de archivos"),
    ("failed to read recent agent activity", "no se pudo leer la actividad reciente de los agentes"),
    ("failed to read the asset", "no se pudo leer el archivo"),
    (
        "failed to remove push notification registration",
        "no se pudo eliminar el registro de notificaciones push",
    ),
    ("failed to revoke the device token", "no se pudo revocar el token del dispositivo"),
    (
        "failed to save push notification registration",
        "no se pudo guardar el registro de notificaciones push",
    ),
    ("failed to save the new device token", "no se pudo guardar el nuevo token del dispositivo"),
    ("failed to store the upload", "no se pudo almacenar el archivo subido"),
    ("format must be text or ansi", "format debe ser text o ansi"),
    ("invalid Authorization header", "header Authorization no válido"),
    ("invalid pairing code", "código de vinculación no válido"),
    ("invalid token", "token no válido"),
    ("keys must contain 1 to 32 entries", "keys debe contener entre 1 y 32 elementos"),
    ("missing Authorization header", "falta el header Authorization"),
    ("mode must be on, off, or toggle", "mode debe ser on, off o toggle"),
    ("no pending pairing request", "no hay ninguna solicitud de vinculación pendiente"),
    (
        "only png, jpeg, gif, webp, and heic images are accepted",
        "solo se aceptan imágenes png, jpeg, gif, webp y heic",
    ),
    ("pairing code expired; request a new code", "el código de vinculación expiró; pide uno nuevo"),
    ("platform must be ios or android", "platform debe ser ios o android"),
    (
        "repo_path is not a git checkout, so a branch cannot be made in it",
        "repo_path no es un checkout de git, así que no se puede crear una rama en él",
    ),
    (
        "repo_path must be a directory inside a workspace this session has open",
        "repo_path debe ser un directorio dentro de un espacio de trabajo que esta sesión tenga abierto",
    ),
    (
        "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        "request_id debe tener entre 1 y 80 caracteres, con letras, dígitos, punto, guion bajo o guion",
    ),
    ("session not found", "sesión no encontrada"),
    (
        "source must be visible, recent, recent-unwrapped, or detection",
        "source debe ser visible, recent, recent-unwrapped o detection",
    ),
    (
        "startup_timeout_ms must be between 3001 and 300000",
        "startup_timeout_ms debe estar entre 3001 y 300000",
    ),
    ("text must be at most 65536 bytes", "text debe tener 65536 bytes como máximo"),
    (
        "the agent no longer has that request pending",
        "el agente ya no tiene esa solicitud pendiente",
    ),
    ("the asset is larger than 10 MiB", "el archivo supera los 10 MiB"),
    ("the file field is empty", "el campo file está vacío"),
    ("the file field must carry a filename", "el campo file debe llevar un nombre de archivo"),
    ("the pane is not waiting on an approval", "el panel no está esperando ninguna aprobación"),
    ("the pane is waiting on a different approval", "el panel está esperando otra aprobación"),
    ("the upload must be at most 25 MiB", "el archivo subido debe ocupar 25 MiB como máximo"),
    (
        "this approval has no option with that number",
        "esta aprobación no tiene ninguna opción con ese número",
    ),
    (
        "this approval offers no option with that meaning",
        "esta aprobación no ofrece ninguna opción con ese significado",
    ),
    ("token must be an Expo push token", "token debe ser un token push de Expo"),
    (
        "too many pairing requests; try again later",
        "demasiadas solicitudes de vinculación; inténtalo más tarde",
    ),
    (
        "workspace_label must be at most 120 printable characters",
        "workspace_label debe tener 120 caracteres imprimibles como máximo",
    ),
    // -- branch names --------------------------------------------------------
    ("branch_name must not be empty", "branch_name no puede estar vacío"),
    (
        "branch_name must be at most 200 characters",
        "branch_name debe tener 200 caracteres como máximo",
    ),
    (
        "branch_name may only contain letters, digits, dot, underscore, dash and slash",
        "branch_name solo puede contener letras, dígitos, punto, guion bajo, guion y barra",
    ),
    ("branch_name must not contain ..", "branch_name no puede contener .."),
    ("branch_name must not start with a dash", "branch_name no puede empezar con un guion"),
    (
        "branch_name must not have an empty path segment or a segment starting or ending with a dot",
        "branch_name no puede tener un segmento de ruta vacío ni un segmento que empiece o termine con un punto",
    ),
    ("branch_name must not end with .lock", "branch_name no puede terminar en .lock"),
];

/// European Portuguese, keyed by the English source.
///
/// One catalog for Brazil and Portugal, written in the European variant to
/// match the marketing site, which is unambiguously European: `ficheiro`,
/// `utilizador`, `palavra-passe`, and "a + infinitive" where Brazil uses the
/// gerund. Choosing the variant the other surface already chose matters more
/// than which of the two it was.
///
/// Pairing is emparelhar / desemparelhar, the Portuguese Bluetooth
/// convention, not the Brazilian `parear`. `ligação`, not `conexão`.
const PT: &[(&str, &str)] = &[
    // -- approval labels the gateway writes for itself -----------------------
    ("Approve", "Aprovar"),
    ("Approve and don't ask again", "Aprovar e não perguntar novamente"),
    ("Deny", "Recusar"),
    ("Option {index}", "Opção {index}"),
    ("Allow {action}?", "Permitir {action}?"),
    // -- push notifications --------------------------------------------------
    ("Agent", "Agente"),
    ("Approval needed", "Aprovação necessária"),
    ("Agent blocked", "Agente bloqueado"),
    ("Agent done", "Agente terminou"),
    ("{name} is waiting for your approval.", "{name} está à espera da sua aprovação."),
    ("{name} needs your input.", "{name} precisa da sua resposta."),
    ("{name} finished running.", "{name} terminou a execução."),
    ("Muqun push notifications are connected.", "As notificações push do Muqun estão ligadas."),
    // -- API error messages --------------------------------------------------
    ("Expo push service request failed", "o pedido ao serviço de push da Expo falhou"),
    ("Herdr did not return the created pane id", "o Herdr não devolveu o id do painel criado"),
    ("Herdr is unavailable", "o Herdr está indisponível"),
    (
        "agent is not one this gateway offers; see GET /api/agents/catalog",
        "agent não é um dos que este gateway oferece; consulte GET /api/agents/catalog",
    ),
    (
        "another pairing request is awaiting confirmation",
        "outro pedido de emparelhamento aguarda confirmação",
    ),
    (
        "answer with an option number or a decision",
        "responda com um número de opção ou com uma decisão",
    ),
    (
        "asset not found in a session workspace",
        "ficheiro não encontrado num espaço de trabalho da sessão",
    ),
    (
        "decision must be allow, allow_always, or deny",
        "decision tem de ser allow, allow_always ou deny",
    ),
    ("device not found", "dispositivo não encontrado"),
    (
        "device_name must be at most 80 characters and contain no control characters",
        "device_name tem de ter no máximo 80 caracteres e não pode conter caracteres de controlo",
    ),
    ("direction must be right or down", "direction tem de ser right ou down"),
    ("executables and scripts are not accepted", "não são aceites executáveis nem scripts"),
    ("expected Bearer token", "esperava-se um token Bearer"),
    (
        "expected a multipart/form-data body with a file field",
        "esperava-se um corpo multipart/form-data com um campo file",
    ),
    (
        "failed to check pairing request limit",
        "não foi possível verificar o limite de pedidos de emparelhamento",
    ),
    ("failed to lock device state", "não foi possível bloquear o estado do dispositivo"),
    (
        "failed to lock pending pairing state",
        "não foi possível bloquear o estado do emparelhamento pendente",
    ),
    ("failed to lock push token state", "não foi possível bloquear o estado do token de push"),
    ("failed to lock the asset index", "não foi possível bloquear o índice de ficheiros"),
    ("failed to read recent agent activity", "não foi possível ler a atividade recente dos agentes"),
    ("failed to read the asset", "não foi possível ler o ficheiro"),
    (
        "failed to remove push notification registration",
        "não foi possível remover o registo das notificações push",
    ),
    ("failed to revoke the device token", "não foi possível revogar o token do dispositivo"),
    (
        "failed to save push notification registration",
        "não foi possível guardar o registo das notificações push",
    ),
    ("failed to save the new device token", "não foi possível guardar o novo token do dispositivo"),
    ("failed to store the upload", "não foi possível guardar o ficheiro carregado"),
    ("format must be text or ansi", "format tem de ser text ou ansi"),
    ("invalid Authorization header", "cabeçalho Authorization inválido"),
    ("invalid pairing code", "código de emparelhamento inválido"),
    ("invalid token", "token inválido"),
    ("keys must contain 1 to 32 entries", "keys tem de conter entre 1 e 32 entradas"),
    ("missing Authorization header", "falta o cabeçalho Authorization"),
    ("mode must be on, off, or toggle", "mode tem de ser on, off ou toggle"),
    ("no pending pairing request", "não há nenhum pedido de emparelhamento pendente"),
    (
        "only png, jpeg, gif, webp, and heic images are accepted",
        "só são aceites imagens png, jpeg, gif, webp e heic",
    ),
    (
        "pairing code expired; request a new code",
        "o código de emparelhamento expirou; peça um novo código",
    ),
    ("platform must be ios or android", "platform tem de ser ios ou android"),
    (
        "repo_path is not a git checkout, so a branch cannot be made in it",
        "repo_path não é um checkout git, por isso não é possível criar nele uma branch",
    ),
    (
        "repo_path must be a directory inside a workspace this session has open",
        "repo_path tem de ser um diretório dentro de um espaço de trabalho que esta sessão tenha aberto",
    ),
    (
        "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        "request_id tem de ter entre 1 e 80 caracteres, usando letras, algarismos, ponto, sublinhado ou hífen",
    ),
    ("session not found", "sessão não encontrada"),
    (
        "source must be visible, recent, recent-unwrapped, or detection",
        "source tem de ser visible, recent, recent-unwrapped ou detection",
    ),
    (
        "startup_timeout_ms must be between 3001 and 300000",
        "startup_timeout_ms tem de estar entre 3001 e 300000",
    ),
    ("text must be at most 65536 bytes", "text tem de ter no máximo 65536 bytes"),
    ("the agent no longer has that request pending", "o agente já não tem esse pedido pendente"),
    ("the asset is larger than 10 MiB", "o ficheiro é maior do que 10 MiB"),
    ("the file field is empty", "o campo file está vazio"),
    ("the file field must carry a filename", "o campo file tem de incluir um nome de ficheiro"),
    ("the pane is not waiting on an approval", "o painel não está à espera de nenhuma aprovação"),
    ("the pane is waiting on a different approval", "o painel está à espera de outra aprovação"),
    ("the upload must be at most 25 MiB", "o ficheiro carregado tem de ter no máximo 25 MiB"),
    (
        "this approval has no option with that number",
        "esta aprovação não tem nenhuma opção com esse número",
    ),
    (
        "this approval offers no option with that meaning",
        "esta aprovação não oferece nenhuma opção com esse significado",
    ),
    ("token must be an Expo push token", "token tem de ser um token de push da Expo"),
    (
        "too many pairing requests; try again later",
        "demasiados pedidos de emparelhamento; tente novamente mais tarde",
    ),
    (
        "workspace_label must be at most 120 printable characters",
        "workspace_label tem de ter no máximo 120 caracteres imprimíveis",
    ),
    // -- branch names --------------------------------------------------------
    ("branch_name must not be empty", "branch_name não pode estar vazio"),
    (
        "branch_name must be at most 200 characters",
        "branch_name tem de ter no máximo 200 caracteres",
    ),
    (
        "branch_name may only contain letters, digits, dot, underscore, dash and slash",
        "branch_name só pode conter letras, algarismos, ponto, sublinhado, traço e barra",
    ),
    ("branch_name must not contain ..", "branch_name não pode conter .."),
    ("branch_name must not start with a dash", "branch_name não pode começar por um traço"),
    (
        "branch_name must not have an empty path segment or a segment starting or ending with a dot",
        "branch_name não pode ter um segmento de caminho vazio nem um segmento que comece ou termine por ponto",
    ),
    ("branch_name must not end with .lock", "branch_name não pode terminar em .lock"),
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
    fn the_eight_codes_are_the_literals_the_app_and_the_site_already_use() {
        // Not `zh-Hant`, not `zh-Hant-TW`, not `zh_TW`, and not `pt-BR`. The app
        // names its catalog directories and persists its setting with these
        // exact strings, so a change here silently un-localizes every client.
        let codes: Vec<&str> = Locale::ALL.iter().map(|locale| locale.as_str()).collect();
        assert_eq!(codes, ["en", "zh-TW", "ja", "ko", "de", "fr", "es", "pt"]);
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
        ] {
            assert_eq!(Locale::from_code(tag), Some(expected), "{tag}");
        }
        // On the website but not here, which is the interesting negative: a
        // code existing somewhere in the product is not a table existing in it.
        for tag in ["it", "it-IT", "ar", "nl-NL", "ru", "gl", "ca"] {
            assert_eq!(Locale::from_code(tag), None, "{tag}");
        }
    }

    #[test]
    fn a_weighted_accept_language_still_picks_among_eight() {
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
    }

    #[test]
    fn a_message_with_no_translation_falls_back_to_the_english_string() {
        assert_eq!(t(Locale::ZhTw, "Deny"), "拒絕");
        assert_eq!(t(Locale::Ja, "Deny"), "拒否");
        assert_eq!(t(Locale::De, "Deny"), "Ablehnen");
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
        // The tables are hand-written and there are seven of them. Without this,
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
}
