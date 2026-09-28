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
pub(super) const KO: &[(&str, &str)] = &[
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
    ("cwd must be a directory inside a workspace this session has open", "cwd는 이 세션이 열어 둔 작업 공간 안의 디렉터리여야 합니다"),
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
    ("that tab has no pane to split", "이 탭에는 분할할 패널이 없습니다"),
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
        "transport encryption is disabled on this gateway; scan its current QR code",
        "이 Gateway는 전송 암호화가 비활성화되어 있습니다. 현재 QR 코드를 스캔하세요",
    ),
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
