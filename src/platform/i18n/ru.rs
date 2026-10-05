/// Russian, keyed by the English source.
///
/// The register is formal: вы, never ты, and the imperative in its polite
/// plural (укажите, повторите, отсканируйте). API error messages start in
/// lowercase like the English they translate. The nouns are the app catalog's:
/// агент, панель, пространство for a workspace, сеанс, привязка for pairing,
/// токен -- and Разрешить / Отклонить for the approval pair, so "approval" as
/// a noun is разрешение throughout. Gateway and Muqun stay in Latin script
/// because they are names.
pub(super) const RU: &[(&str, &str)] = &[
    // -- approval labels the gateway writes for itself -----------------------
    ("Approve", "Разрешить"),
    ("Approve and don't ask again", "Разрешить и больше не спрашивать"),
    ("Deny", "Отклонить"),
    ("Option {index}", "Вариант {index}"),
    ("Allow {action}?", "Разрешить {action}?"),
    // -- push notifications --------------------------------------------------
    ("Agent", "Агент"),
    ("Approval needed", "Требуется разрешение"),
    ("Agent blocked", "Агент ожидает"),
    ("Agent done", "Агент завершил работу"),
    ("{name} is waiting for your approval.", "{name} ожидает вашего разрешения."),
    ("{name} needs your input.", "{name} ожидает вашего ввода."),
    ("{name} finished running.", "{name} завершил выполнение."),
    ("Muqun push notifications are connected.", "Push-уведомления Muqun подключены."),
    // -- API error messages --------------------------------------------------
    ("Expo push service request failed", "запрос к push-сервису Expo не выполнен"),
    ("Herdr did not return the created pane id", "Herdr не вернул id созданной панели"),
    ("Herdr is unavailable", "Herdr недоступен"),
    (
        "agent is not one this gateway offers; see GET /api/agents/catalog",
        "agent не входит в число агентов, которые предлагает этот Gateway; см. GET /api/agents/catalog",
    ),
    (
        "another pairing request is awaiting confirmation",
        "другой запрос на привязку уже ожидает подтверждения",
    ),
    ("answer with an option number or a decision", "укажите номер варианта или решение"),
    ("asset not found in a session workspace", "файл не найден в пространстве этого сеанса"),
    (
        "cwd must be a directory inside a workspace this session has open",
        "cwd должен быть каталогом внутри пространства, открытого в этом сеансе",
    ),
    (
        "decision must be allow, allow_always, or deny",
        "decision должен быть allow, allow_always или deny",
    ),
    ("device not found", "устройство не найдено"),
    (
        "device_name must be at most 80 characters and contain no control characters",
        "device_name должен содержать не более 80 символов и не содержать управляющих символов",
    ),
    ("direction must be right or down", "direction должен быть right или down"),
    ("executables and scripts are not accepted", "исполняемые файлы и скрипты не принимаются"),
    ("expected Bearer token", "ожидался токен Bearer"),
    (
        "expected a multipart/form-data body with a file field",
        "ожидалось тело multipart/form-data с полем file",
    ),
    ("failed to check pairing request limit", "не удалось проверить лимит запросов на привязку"),
    ("failed to lock device state", "не удалось заблокировать состояние устройств"),
    (
        "failed to lock pending pairing state",
        "не удалось заблокировать состояние ожидающей привязки",
    ),
    ("failed to lock push token state", "не удалось заблокировать состояние push-токенов"),
    ("failed to lock the asset index", "не удалось заблокировать индекс файлов"),
    ("failed to read recent agent activity", "не удалось прочитать недавнюю активность агентов"),
    ("failed to read the asset", "не удалось прочитать файл"),
    (
        "failed to remove push notification registration",
        "не удалось удалить регистрацию push-уведомлений",
    ),
    ("failed to revoke the device token", "не удалось отозвать токен устройства"),
    (
        "failed to save push notification registration",
        "не удалось сохранить регистрацию push-уведомлений",
    ),
    ("failed to save the new device token", "не удалось сохранить новый токен устройства"),
    ("failed to store the upload", "не удалось сохранить загруженный файл"),
    ("format must be text or ansi", "format должен быть text или ansi"),
    ("invalid Authorization header", "недопустимый заголовок Authorization"),
    ("invalid pairing code", "недопустимый код привязки"),
    ("invalid token", "недопустимый токен"),
    ("keys must contain 1 to 32 entries", "keys должен содержать от 1 до 32 элементов"),
    ("missing Authorization header", "отсутствует заголовок Authorization"),
    ("mode must be on, off, or toggle", "mode должен быть on, off или toggle"),
    ("no pending pairing request", "нет ожидающего запроса на привязку"),
    (
        "only png, jpeg, gif, webp, and heic images are accepted",
        "принимаются только изображения png, jpeg, gif, webp и heic",
    ),
    (
        "pairing code expired; request a new code",
        "срок действия кода привязки истёк; запросите новый код",
    ),
    ("platform must be ios or android", "platform должен быть ios или android"),
    (
        "repo_path is not a git checkout, so a branch cannot be made in it",
        "repo_path не является рабочей копией git, поэтому создать в нём ветку нельзя",
    ),
    (
        "repo_path must be a directory inside a workspace this session has open",
        "repo_path должен быть каталогом внутри пространства, открытого в этом сеансе",
    ),
    (
        "request_id must be 1-80 chars using letters, digits, dot, underscore, or hyphen",
        "request_id должен содержать от 1 до 80 символов: латинские буквы, цифры, точка, подчёркивание или дефис",
    ),
    ("session not found", "сеанс не найден"),
    (
        "source must be visible, recent, recent-unwrapped, or detection",
        "source должен быть visible, recent, recent-unwrapped или detection",
    ),
    (
        "startup_timeout_ms must be between 3001 and 300000",
        "startup_timeout_ms должен быть в диапазоне от 3001 до 300000",
    ),
    ("text must be at most 65536 bytes", "text должен занимать не более 65536 байт"),
    ("that tab has no pane to split", "в этой вкладке нет панели, которую можно разделить"),
    (
        "the agent no longer has that request pending",
        "агент больше не ожидает ответа на этот запрос",
    ),
    ("the asset is larger than 10 MiB", "файл больше 10 МиБ"),
    ("the file field is empty", "поле file пустое"),
    ("the file field must carry a filename", "поле file должно содержать имя файла"),
    ("the pane is not waiting on an approval", "панель не ожидает разрешения"),
    ("the pane is waiting on a different approval", "панель ожидает другого разрешения"),
    ("the upload must be at most 25 MiB", "размер загружаемого файла не должен превышать 25 МиБ"),
    (
        "this approval has no option with that number",
        "в этом запросе на разрешение нет варианта с таким номером",
    ),
    (
        "this approval offers no option with that meaning",
        "в этом запросе на разрешение нет варианта с таким значением",
    ),
    ("token must be an Expo push token", "token должен быть push-токеном Expo"),
    (
        "too many pairing requests; try again later",
        "слишком много запросов на привязку; повторите попытку позже",
    ),
    (
        "transport encryption is disabled on this gateway; scan its current QR code",
        "на этом Gateway отключено транспортное шифрование; отсканируйте его текущий QR-код",
    ),
    (
        "workspace_label must be at most 120 printable characters",
        "workspace_label должен содержать не более 120 печатных символов",
    ),
    // -- branch names --------------------------------------------------------
    ("branch_name must not be empty", "branch_name не должен быть пустым"),
    ("branch_name must be at most 200 characters", "branch_name должен содержать не более 200 символов"),
    (
        "branch_name may only contain letters, digits, dot, underscore, dash and slash",
        "branch_name может содержать только латинские буквы, цифры, точку, подчёркивание, дефис и косую черту",
    ),
    ("branch_name must not contain ..", "branch_name не должен содержать .."),
    ("branch_name must not start with a dash", "branch_name не должен начинаться с дефиса"),
    (
        "branch_name must not have an empty path segment or a segment starting or ending with a dot",
        "branch_name не должен содержать пустой сегмент пути или сегмент, начинающийся или заканчивающийся точкой",
    ),
    ("branch_name must not end with .lock", "branch_name не должен заканчиваться на .lock"),
];
