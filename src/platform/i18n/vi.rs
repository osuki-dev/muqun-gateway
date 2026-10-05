/// Vietnamese, keyed by the English source.
///
/// Standard Northern orthography and vocabulary, the register of a developer
/// tool: tệp for a file, thư mục, máy chủ, mã hóa, and a polite `hãy` in front
/// of anything that asks the reader to do something. The nouns are the app
/// catalog's: tác tử for an agent, ngăn for a pane, không gian làm việc, phiên,
/// ghép nối for pairing, thiết bị, and Chấp thuận / Từ chối for the approval
/// pair. Token, header, gateway, Muqun and the format names stay in Latin
/// script, as they do in the app.
pub(super) const VI: &[(&str, &str)] = &[
    // -- approval labels the gateway writes for itself -----------------------
    ("Approve", "Chấp thuận"),
    ("Approve and don't ask again", "Chấp thuận và không hỏi lại"),
    ("Deny", "Từ chối"),
    ("Option {index}", "Lựa chọn {index}"),
    ("Allow {action}?", "Cho phép {action}?"),
    // -- push notifications --------------------------------------------------
    ("Agent", "Tác tử"),
    ("Approval needed", "Cần chấp thuận"),
    ("Agent blocked", "Tác tử đang chờ"),
    ("Agent done", "Tác tử đã xong"),
    ("{name} is waiting for your approval.", "{name} đang chờ bạn chấp thuận."),
    ("{name} needs your input.", "{name} cần bạn nhập liệu."),
    ("{name} finished running.", "{name} đã chạy xong."),
    ("Muqun push notifications are connected.", "Thông báo đẩy của Muqun đã được kết nối."),
    // -- API error messages --------------------------------------------------
    ("Expo push service request failed", "yêu cầu tới dịch vụ thông báo đẩy của Expo thất bại"),
    ("Herdr did not return the created pane id", "Herdr không trả về id của ngăn vừa tạo"),
    ("Herdr is unavailable", "không kết nối được với Herdr"),
    (
        "agent is not one this gateway offers; see GET /api/agents/catalog",
        "agent không nằm trong số tác tử mà gateway này cung cấp; xem GET /api/agents/catalog",
    ),
    (
        "another pairing request is awaiting confirmation",
        "một yêu cầu ghép nối khác đang chờ xác nhận",
    ),
    (
        "answer with an option number or a decision",
        "hãy trả lời bằng số thứ tự của lựa chọn hoặc một quyết định",
    ),
    (
        "asset not found in a session workspace",
        "không tìm thấy tệp trong không gian làm việc của phiên này",
    ),
    (
        "cwd must be a directory inside a workspace this session has open",
        "cwd phải là một thư mục bên trong không gian làm việc mà phiên này đã mở",
    ),
    (
        "decision must be allow, allow_always, or deny",
        "decision phải là allow, allow_always hoặc deny",
    ),
    ("device not found", "không tìm thấy thiết bị"),
    (
        "device_name must be at most 80 characters and contain no control characters",
        "device_name tối đa 80 ký tự và không được chứa ký tự điều khiển",
    ),
    ("direction must be right or down", "direction phải là right hoặc down"),
    ("executables and scripts are not accepted", "không chấp nhận tệp thực thi và tập lệnh"),
    ("expected Bearer token", "cần token Bearer"),
    (
        "expected a multipart/form-data body with a file field",
        "cần phần thân multipart/form-data có trường file",
    ),
    ("failed to check pairing request limit", "không thể kiểm tra giới hạn yêu cầu ghép nối"),
    ("failed to lock device state", "không thể khóa trạng thái thiết bị"),
    ("failed to lock pending pairing state", "không thể khóa trạng thái ghép nối đang chờ"),
    ("failed to lock push token state", "không thể khóa trạng thái token thông báo đẩy"),
    ("failed to lock the asset index", "không thể khóa chỉ mục tệp"),
    ("failed to read recent agent activity", "không thể đọc hoạt động gần đây của tác tử"),
    ("failed to read the asset", "không thể đọc tệp"),
    (
        "failed to remove push notification registration",
        "không thể gỡ đăng ký thông báo đẩy",
    ),
    ("failed to revoke the device token", "không thể thu hồi token của thiết bị"),
    (
        "failed to save push notification registration",
        "không thể lưu đăng ký thông báo đẩy",
    ),
    ("failed to save the new device token", "không thể lưu token mới của thiết bị"),
    ("failed to store the upload", "không thể lưu tệp đã tải lên"),
    ("format must be text or ansi", "format phải là text hoặc ansi"),
    ("invalid Authorization header", "header Authorization không hợp lệ"),
    ("invalid pairing code", "mã ghép nối không hợp lệ"),
    ("invalid token", "token không hợp lệ"),
    ("keys must contain 1 to 32 entries", "keys phải chứa từ 1 đến 32 mục"),
    ("missing Authorization header", "thiếu header Authorization"),
    ("mode must be on, off, or toggle", "mode phải là on, off hoặc toggle"),
    ("no pending pairing request", "không có yêu cầu ghép nối nào đang chờ"),
    (
        "only png, jpeg, gif, webp, and heic images are accepted",
        "chỉ chấp nhận ảnh png, jpeg, gif, webp và heic",
    ),
    (
        "pairing code expired; request a new code",
        "mã ghép nối đã hết hạn; hãy yêu cầu mã mới",
    ),
    ("platform must be ios or android", "platform phải là ios hoặc android"),
    (
        "repo_path is not a git checkout, so a branch cannot be made in it",
        "repo_path không phải là một bản checkout git, nên không thể tạo nhánh trong đó",
    ),
    (
        "repo_path must be a directory inside a workspace this session has open",
        "repo_path phải là một thư mục bên trong không gian làm việc mà phiên này đã mở",
    ),
    (
        "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        "request_id phải dài từ 1 đến 80 ký tự và chỉ gồm chữ cái Latinh, chữ số, dấu chấm, gạch dưới hoặc gạch nối",
    ),
    ("session not found", "không tìm thấy phiên"),
    (
        "source must be visible, recent, recent-unwrapped, or detection",
        "source phải là visible, recent, recent-unwrapped hoặc detection",
    ),
    (
        "startup_timeout_ms must be between 3001 and 300000",
        "startup_timeout_ms phải nằm trong khoảng từ 3001 đến 300000",
    ),
    ("text must be at most 65536 bytes", "text tối đa 65536 byte"),
    ("that tab has no pane to split", "thẻ này không có ngăn nào để chia"),
    (
        "the agent no longer has that request pending",
        "tác tử không còn chờ yêu cầu đó nữa",
    ),
    ("the asset is larger than 10 MiB", "tệp lớn hơn 10 MiB"),
    ("the file field is empty", "trường file đang trống"),
    ("the file field must carry a filename", "trường file phải kèm tên tệp"),
    ("the pane is not waiting on an approval", "ngăn này không đang chờ chấp thuận"),
    ("the pane is waiting on a different approval", "ngăn này đang chờ một chấp thuận khác"),
    ("the upload must be at most 25 MiB", "tệp tải lên tối đa 25 MiB"),
    (
        "this approval has no option with that number",
        "yêu cầu chấp thuận này không có lựa chọn mang số đó",
    ),
    (
        "this approval offers no option with that meaning",
        "yêu cầu chấp thuận này không có lựa chọn mang ý nghĩa đó",
    ),
    ("token must be an Expo push token", "token phải là token thông báo đẩy của Expo"),
    (
        "too many pairing requests; try again later",
        "quá nhiều yêu cầu ghép nối; hãy thử lại sau",
    ),
    (
        "transport encryption is disabled on this gateway; scan its current QR code",
        "gateway này đã tắt mã hóa truyền tải; hãy quét mã QR hiện tại của nó",
    ),
    (
        "workspace_label must be at most 120 printable characters",
        "workspace_label tối đa 120 ký tự in được",
    ),
    // -- branch names --------------------------------------------------------
    ("branch_name must not be empty", "branch_name không được để trống"),
    ("branch_name must be at most 200 characters", "branch_name tối đa 200 ký tự"),
    (
        "branch_name may only contain letters, digits, dot, underscore, dash and slash",
        "branch_name chỉ được chứa chữ cái Latinh, chữ số, dấu chấm, gạch dưới, gạch nối và dấu gạch chéo",
    ),
    ("branch_name must not contain ..", "branch_name không được chứa .."),
    ("branch_name must not start with a dash", "branch_name không được bắt đầu bằng gạch nối"),
    (
        "branch_name must not have an empty path segment or a segment starting or ending with a dot",
        "branch_name không được có đoạn đường dẫn trống hoặc đoạn bắt đầu hay kết thúc bằng dấu chấm",
    ),
    ("branch_name must not end with .lock", "branch_name không được kết thúc bằng .lock"),
];
