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
pub(super) const PT: &[(&str, &str)] = &[
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
    ("cwd must be a directory inside a workspace this session has open", "cwd tem de ser um diretório dentro de uma área de trabalho que esta sessão tenha aberta"),
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
    ("that tab has no pane to split", "este separador não tem nenhum painel para dividir"),
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
        "transport encryption is disabled on this gateway; scan its current QR code",
        "a encriptação de transporte está desativada neste gateway; leia o respetivo código QR atual",
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
