/// Taiwanese-normative Traditional Chinese, keyed by the English source.
///
/// The register is the island's, not Simplified Chinese transliterated: 設定,
/// 終端機, 檔案, 伺服器, 網路, 儲存, 預設, 程式碼, 連線. The app's own catalog
/// is the other half of this vocabulary and these agree with it: 核准, 拒絕,
/// 代理程式, 面板, 工作區, 工作階段, 配對 -- and Gateway, which stays in Latin
/// script because it is the product's name. Muqun itself is 牧群 here, the same
/// word the app and osuki.dev use in this language.
pub(super) const ZH_TW: &[(&str, &str)] = &[
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
        "牧群推播通知已連線。",
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
    ("cwd must be a directory inside a workspace this session has open", "cwd 必須是這個工作階段已開啟的工作區底下的目錄"),
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
    ("that tab has no pane to split", "這個分頁沒有可分割的面板"),
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
        "transport encryption is disabled on this gateway; scan its current QR code",
        "這個 Gateway 已停用傳輸加密，請掃描它目前的 QR Code",
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
