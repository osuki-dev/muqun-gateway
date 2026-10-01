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
pub(super) const FR: &[(&str, &str)] = &[
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
    ("cwd must be a directory inside a workspace this session has open", "cwd doit être un répertoire situé dans un espace de travail que cette session a ouvert"),
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
    ("that tab has no pane to split", "cet onglet n’a aucun panneau à diviser"),
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
        "transport encryption is disabled on this gateway; scan its current QR code",
        "le chiffrement du transport est désactivé sur ce Gateway ; scannez son QR code actuel",
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
