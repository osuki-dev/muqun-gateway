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
pub(super) const ES: &[(&str, &str)] = &[
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
    ("cwd must be a directory inside a workspace this session has open", "cwd debe ser un directorio dentro de un espacio de trabajo que esta sesión tenga abierto"),
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
    ("that tab has no pane to split", "esta pestaña no tiene ningún panel que dividir"),
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
        "transport encryption is disabled on this gateway; scan its current QR code",
        "el cifrado de transporte está desactivado en este gateway; escanea su código QR actual",
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
