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
pub(super) const DE: &[(&str, &str)] = &[
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
    ("cwd must be a directory inside a workspace this session has open", "cwd muss ein Verzeichnis in einem Workspace sein, den diese Sitzung geöffnet hat"),
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
    ("that tab has no pane to split", "dieser Tab hat kein Panel, das geteilt werden könnte"),
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
        "transport encryption is disabled on this gateway; scan its current QR code",
        "die Übertragungsverschlüsselung ist auf diesem Gateway deaktiviert; scanne seinen aktuellen QR-Code",
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
