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
///
/// Muqun itself is 牧群（ぼくぐん） here, the reading given once and dropped
/// after, matching the app catalog and osuki.dev.
pub(super) const JA: &[(&str, &str)] = &[
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
    ("Muqun push notifications are connected.", "牧群（ぼくぐん）のプッシュ通知が接続されました。"),
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
    ("cwd must be a directory inside a workspace this session has open", "cwd はこのセッションが開いているワークスペース内のディレクトリでなければなりません"),
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
    ("that tab has no pane to split", "このタブには分割できるパネルがありません"),
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
        "transport encryption is disabled on this gateway; scan its current QR code",
        "この Gateway では転送の暗号化が無効になっています。現在の QR コードをスキャンしてください",
    ),
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
