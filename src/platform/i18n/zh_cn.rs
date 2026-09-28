/// Mainland-normative Simplified Chinese, keyed by the English source.
///
/// The register is the mainland's, not Traditional Chinese transcribed: 设置,
/// 终端, 文件, 服务器, 网络, 保存, 默认, 代码, 连接, 请求头. The app's own
/// `zh-CN` catalog is the other half of this vocabulary and these agree with
/// it: 批准, 拒绝, 代理, 面板, 工作区, 会话, 配对 -- and Gateway, which stays in
/// Latin script because it is the product's name. Muqun itself is 牧群 here,
/// the same word the app and osuki.dev use in this language.
///
/// This table and [`ZH_TW`] never stand in for each other: a Simplified reader
/// served 核准 and 檔案 is being served the wrong script, and the rule in
/// [`Locale::from_code`] exists so that neither table ever is.
pub(super) const ZH_CN: &[(&str, &str)] = &[
    // -- approval labels the gateway writes for itself -----------------------
    ("Approve", "批准"),
    ("Approve and don't ask again", "批准且不再询问"),
    ("Deny", "拒绝"),
    ("Option {index}", "选项 {index}"),
    ("Allow {action}?", "允许 {action}？"),
    // -- push notifications --------------------------------------------------
    ("Agent", "代理"),
    ("Approval needed", "需要批准"),
    ("Agent blocked", "代理等待中"),
    ("Agent done", "代理已完成"),
    ("{name} is waiting for your approval.", "{name} 正在等待你的批准。"),
    ("{name} needs your input.", "{name} 需要你的输入。"),
    ("{name} finished running.", "{name} 已运行完毕。"),
    ("Muqun push notifications are connected.", "牧群推送通知已连接。"),
    // -- API error messages --------------------------------------------------
    ("Expo push service request failed", "Expo 推送服务请求失败"),
    ("Herdr did not return the created pane id", "Herdr 没有返回所创建面板的 id"),
    ("Herdr is unavailable", "无法连接到 Herdr"),
    (
        "agent is not one this gateway offers; see GET /api/agents/catalog",
        "这个 Gateway 未提供该代理；请参阅 GET /api/agents/catalog",
    ),
    (
        "another pairing request is awaiting confirmation",
        "已有另一个配对请求正在等待确认",
    ),
    ("answer with an option number or a decision", "请以选项编号或决定作答"),
    (
        "asset not found in a session workspace",
        "在这个会话的工作区中找不到该文件",
    ),
    (
        "cwd must be a directory inside a workspace this session has open",
        "cwd 必须是这个会话已打开的工作区下的目录",
    ),
    (
        "decision must be allow, allow_always, or deny",
        "decision 必须是 allow、allow_always 或 deny",
    ),
    ("device not found", "找不到这个设备"),
    (
        "device_name must be at most 80 characters and contain no control characters",
        "device_name 最多 80 个字符，且不得包含控制字符",
    ),
    ("direction must be right or down", "direction 必须是 right 或 down"),
    ("executables and scripts are not accepted", "不接受可执行文件和脚本"),
    ("expected Bearer token", "需要 Bearer token"),
    (
        "expected a multipart/form-data body with a file field",
        "需要含有 file 字段的 multipart/form-data 请求体",
    ),
    ("failed to check pairing request limit", "无法检查配对请求的次数上限"),
    ("failed to lock device state", "无法锁定设备状态"),
    ("failed to lock pending pairing state", "无法锁定待处理的配对状态"),
    ("failed to lock push token state", "无法锁定推送 token 状态"),
    ("failed to lock the asset index", "无法锁定文件索引"),
    ("failed to read recent agent activity", "无法读取最近的代理活动"),
    ("failed to read the asset", "无法读取这个文件"),
    (
        "failed to remove push notification registration",
        "无法移除推送通知的注册",
    ),
    ("failed to revoke the device token", "无法吊销这个设备的 token"),
    (
        "failed to save push notification registration",
        "无法保存推送通知的注册",
    ),
    ("failed to save the new device token", "无法保存新的设备 token"),
    ("failed to store the upload", "无法保存上传的文件"),
    ("format must be text or ansi", "format 必须是 text 或 ansi"),
    ("invalid Authorization header", "Authorization 请求头无效"),
    ("invalid pairing code", "配对码无效"),
    ("invalid token", "token 无效"),
    ("keys must contain 1 to 32 entries", "keys 必须包含 1 到 32 个条目"),
    ("missing Authorization header", "缺少 Authorization 请求头"),
    ("mode must be on, off, or toggle", "mode 必须是 on、off 或 toggle"),
    ("no pending pairing request", "没有待处理的配对请求"),
    (
        "only png, jpeg, gif, webp, and heic images are accepted",
        "只接受 png、jpeg、gif、webp 和 heic 图片",
    ),
    (
        "pairing code expired; request a new code",
        "配对码已过期，请重新获取新的配对码",
    ),
    ("platform must be ios or android", "platform 必须是 ios 或 android"),
    (
        "repo_path is not a git checkout, so a branch cannot be made in it",
        "repo_path 不是 git 工作目录，因此无法在其中创建分支",
    ),
    (
        "repo_path must be a directory inside a workspace this session has open",
        "repo_path 必须是这个会话已打开的工作区下的目录",
    ),
    (
        "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        "request_id 必须是 1 到 80 个字符，且只能使用英文字母、数字、点、下划线或连字符",
    ),
    ("session not found", "找不到这个会话"),
    (
        "source must be visible, recent, recent-unwrapped, or detection",
        "source 必须是 visible、recent、recent-unwrapped 或 detection",
    ),
    (
        "startup_timeout_ms must be between 3001 and 300000",
        "startup_timeout_ms 必须介于 3001 和 300000 之间",
    ),
    ("text must be at most 65536 bytes", "text 最多 65536 个字节"),
    ("that tab has no pane to split", "这个标签页没有可分割的面板"),
    (
        "the agent no longer has that request pending",
        "代理已不再等待这个请求",
    ),
    ("the asset is larger than 10 MiB", "这个文件超过 10 MiB"),
    ("the file field is empty", "file 字段为空"),
    ("the file field must carry a filename", "file 字段必须带有文件名"),
    ("the pane is not waiting on an approval", "这个面板并未在等待批准"),
    (
        "the pane is waiting on a different approval",
        "这个面板正在等待的是另一个批准",
    ),
    ("the upload must be at most 25 MiB", "上传的文件最多 25 MiB"),
    (
        "this approval has no option with that number",
        "这个批准没有该编号的选项",
    ),
    (
        "this approval offers no option with that meaning",
        "这个批准没有代表该决定的选项",
    ),
    ("token must be an Expo push token", "token 必须是 Expo 推送 token"),
    (
        "too many pairing requests; try again later",
        "配对请求次数过多，请稍后再试",
    ),
    (
        "transport encryption is disabled on this gateway; scan its current QR code",
        "这个 Gateway 已禁用传输加密，请扫描它当前的二维码",
    ),
    (
        "workspace_label must be at most 120 printable characters",
        "workspace_label 最多 120 个可打印字符",
    ),
    // -- branch names --------------------------------------------------------
    ("branch_name must not be empty", "branch_name 不能为空"),
    ("branch_name must be at most 200 characters", "branch_name 最多 200 个字符"),
    (
        "branch_name may only contain letters, digits, dot, underscore, dash and slash",
        "branch_name 只能包含英文字母、数字、点、下划线、连字符和斜杠",
    ),
    ("branch_name must not contain ..", "branch_name 不能包含 .."),
    ("branch_name must not start with a dash", "branch_name 不能以连字符开头"),
    (
        "branch_name must not have an empty path segment or a segment starting or ending with a dot",
        "branch_name 不能有空的路径段，路径段也不能以点开头或结尾",
    ),
    ("branch_name must not end with .lock", "branch_name 不能以 .lock 结尾"),
];
