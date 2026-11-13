//! Managed external runtime discovery, probing, and atomic installation (`bun`, `herdr`).
//!
//! Resolves runtime binaries across three tiers in priority order:
//! 1. [`RuntimeSource::Configured`] — an explicit user-configured binary path.
//! 2. [`RuntimeSource::System`] — discovered on process `PATH`, login shell `PATH`,
//!    or well-known user/system binary directories (`~/.bun/bin`, `~/.local/bin`,
//!    `~/.cargo/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`).
//! 3. [`RuntimeSource::Managed`] — installed by Muqun under
//!    `<state_dir>/runtimes/<kind>/current/<binary>`.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use sha2::{Digest as _, Sha256};

/// Maximum allowed download size for a runtime archive or binary (256 MiB).
pub const MAX_RUNTIME_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

/// Bounded timeout for `probe_version` (`<binary> --version`).
pub const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum number of bytes read from `--version` stdout/stderr.
pub const VERSION_PROBE_MAX_BYTES: u64 = 1024;

/// External runtime kind managed or probed by Muqun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    /// JavaScript/TypeScript runtime for the plugin host (`bun`).
    Bun,
    /// Terminal multiplexer runtime (`herdr` or `herdr-core`).
    Herdr,
}

impl RuntimeKind {
    /// All supported external runtime kinds.
    pub const ALL: [RuntimeKind; 2] = [RuntimeKind::Bun, RuntimeKind::Herdr];

    /// Stable lowercase identifier used in directory paths and serialization.
    pub fn id(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Herdr => "herdr",
        }
    }

    /// Human-readable display name.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Bun => "Bun",
            Self::Herdr => "Herdr",
        }
    }

    /// Candidate executable names searched for this runtime, in preference order.
    pub fn binary_names(self) -> &'static [&'static str] {
        match self {
            Self::Bun => &["bun"],
            Self::Herdr => &["herdr", "herdr-core"],
        }
    }

    /// Primary canonical executable name installed in the managed `current/` directory.
    pub fn primary_binary_name(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Herdr => "herdr",
        }
    }

    /// Environment variable allowing operators or tests to override the release download URL.
    pub fn release_url_env_var(self) -> &'static str {
        match self {
            Self::Bun => "MUQUN_BUN_RELEASE_URL",
            Self::Herdr => "MUQUN_HERDR_RELEASE_URL",
        }
    }

    /// Official release download URL for the current platform and architecture.
    pub fn default_release_url(self) -> Result<String> {
        match self {
            Self::Bun => {
                let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
                    ("macos", "aarch64") => "bun-darwin-aarch64.zip",
                    ("macos", "x86_64") => "bun-darwin-x64.zip",
                    ("linux", "x86_64") => "bun-linux-x64.zip",
                    ("linux", "aarch64") => "bun-linux-aarch64.zip",
                    (os, arch) => {
                        anyhow::bail!("no official Bun release archive for {os}/{arch}")
                    }
                };
                Ok(format!(
                    "https://github.com/oven-sh/bun/releases/latest/download/{asset}"
                ))
            }
            Self::Herdr => {
                let target = match (std::env::consts::OS, std::env::consts::ARCH) {
                    ("macos", "aarch64") => "aarch64-apple-darwin",
                    ("macos", "x86_64") => "x86_64-apple-darwin",
                    ("linux", "x86_64") => "x86_64-unknown-linux-musl",
                    ("linux", "aarch64") => "aarch64-unknown-linux-musl",
                    (os, arch) => {
                        anyhow::bail!("no official Herdr release binary for {os}/{arch}")
                    }
                };
                Ok(format!(
                    "https://github.com/osuki-dev/herdr/releases/latest/download/herdr-{target}"
                ))
            }
        }
    }

    /// Resolve the effective download URL, preferring an explicit override argument,
    /// then the environment variable override, and finally the official platform URL.
    pub fn resolve_release_url(self, custom_url: Option<&str>) -> Result<String> {
        if let Some(url) = custom_url.map(str::trim).filter(|s| !s.is_empty()) {
            return Ok(url.to_owned());
        }
        if let Ok(env_url) = std::env::var(self.release_url_env_var()) {
            let trimmed = env_url.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_owned());
            }
        }
        self.default_release_url()
    }
}

impl fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl FromStr for RuntimeKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bun" => Ok(Self::Bun),
            "herdr" | "herdr-core" => Ok(Self::Herdr),
            other => anyhow::bail!("unknown runtime kind `{other}`"),
        }
    }
}

/// Where a resolved runtime executable was found.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RuntimeSource {
    /// Explicitly configured path supplied by caller/settings.
    Configured(PathBuf),
    /// Discovered on system `PATH`, login shell `PATH`, or standard user locations.
    System(PathBuf),
    /// Installed and managed under `<state_dir>/runtimes/<kind>/current/<binary>`.
    Managed(PathBuf),
}

impl RuntimeSource {
    /// Return the resolved executable path.
    pub fn path(&self) -> &Path {
        match self {
            Self::Configured(path) | Self::System(path) | Self::Managed(path) => path,
        }
    }

    /// Consume `self` and return the inner [`PathBuf`].
    pub fn into_path(self) -> PathBuf {
        match self {
            Self::Configured(path) | Self::System(path) | Self::Managed(path) => path,
        }
    }

    /// Short label identifying the source tier.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::Configured(_) => "configured",
            Self::System(_) => "system",
            Self::Managed(_) => "managed",
        }
    }

    pub fn is_configured(&self) -> bool {
        matches!(self, Self::Configured(_))
    }

    pub fn is_system(&self) -> bool {
        matches!(self, Self::System(_))
    }

    pub fn is_managed(&self) -> bool {
        matches!(self, Self::Managed(_))
    }
}

/// Complete availability and version status for a runtime.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeStatus {
    pub kind: RuntimeKind,
    pub available: bool,
    pub source: Option<RuntimeSource>,
    pub binary_path: Option<PathBuf>,
    pub version: Option<String>,
    pub managed_dir: PathBuf,
}

/// Manager for probing, resolving, staging, and installing external runtimes.
#[derive(Debug, Clone)]
pub struct RuntimeManager {
    state_dir: PathBuf,
    /// Optional explicit system search directories (used for hermetic tests).
    /// When `None`, probes process `PATH`, `login_env::login_path()`, and well-known user paths.
    system_search_dirs: Option<Vec<PathBuf>>,
}

impl RuntimeManager {
    /// Construct a `RuntimeManager` rooted at [`crate::platform::store::state_dir`].
    pub fn new() -> Result<Self> {
        Ok(Self {
            state_dir: crate::platform::store::state_dir()?,
            system_search_dirs: None,
        })
    }

    /// Construct a `RuntimeManager` with an explicit state directory.
    pub fn with_state_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            system_search_dirs: None,
        }
    }

    /// Construct a `RuntimeManager` with an explicit state directory and isolated system search paths.
    pub fn with_search_dirs(
        state_dir: impl Into<PathBuf>,
        system_search_dirs: Vec<PathBuf>,
    ) -> Self {
        Self {
            state_dir: state_dir.into(),
            system_search_dirs: Some(system_search_dirs),
        }
    }

    /// Base state directory used by this manager.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Root directory for all managed runtimes (`<state_dir>/runtimes`).
    pub fn runtimes_root(&self) -> PathBuf {
        self.state_dir.join("runtimes")
    }

    /// Managed directory for a specific runtime (`<state_dir>/runtimes/<kind>`).
    pub fn managed_dir(&self, kind: RuntimeKind) -> PathBuf {
        self.runtimes_root().join(kind.id())
    }

    /// Active directory/symlink for a specific runtime (`<state_dir>/runtimes/<kind>/current`).
    pub fn managed_current_dir(&self, kind: RuntimeKind) -> PathBuf {
        self.managed_dir(kind).join("current")
    }

    /// Canonical managed binary path (`<state_dir>/runtimes/<kind>/current/<primary_binary>`).
    pub fn managed_binary_path(&self, kind: RuntimeKind) -> PathBuf {
        self.managed_current_dir(kind)
            .join(kind.primary_binary_name())
    }

    /// Resolve the highest-priority executable source for `kind`:
    /// `Configured` > `System` > `Managed`.
    pub fn resolve_source(
        &self,
        kind: RuntimeKind,
        configured: Option<&Path>,
    ) -> Option<RuntimeSource> {
        // Priority 1: Explicitly configured binary path.
        if let Some(cfg_path) = configured {
            if !cfg_path.as_os_str().is_empty() {
                let expanded = expand_tilde(cfg_path);
                if is_executable(&expanded) {
                    return Some(RuntimeSource::Configured(expanded));
                }
                // If a bare command name was configured, search system dirs for it.
                if expanded.components().count() == 1 {
                    for dir in self.collect_system_dirs(kind) {
                        let candidate = dir.join(&expanded);
                        if is_executable(&candidate) {
                            return Some(RuntimeSource::Configured(candidate));
                        }
                    }
                }
            }
        }

        // Priority 2: System PATH, login shell PATH, and well-known user/system locations.
        for dir in self.collect_system_dirs(kind) {
            for bin_name in kind.binary_names() {
                let candidate = dir.join(bin_name);
                if is_executable(&candidate) {
                    return Some(RuntimeSource::System(candidate));
                }
            }
        }

        // Priority 3: Managed directory `<state_dir>/runtimes/<kind>/current/<bin_name>`.
        let current_dir = self.managed_current_dir(kind);
        for bin_name in kind.binary_names() {
            let candidate = current_dir.join(bin_name);
            if is_executable(&candidate) {
                return Some(RuntimeSource::Managed(candidate));
            }
        }

        None
    }

    /// Resolve the executable binary path for `kind`, if available from any source.
    pub fn resolve_binary(&self, kind: RuntimeKind, configured: Option<&Path>) -> Option<PathBuf> {
        self.resolve_source(kind, configured)
            .map(RuntimeSource::into_path)
    }

    /// Probe availability, source tier, binary path, and `--version` for `kind`.
    pub fn probe(&self, kind: RuntimeKind, configured: Option<&Path>) -> RuntimeStatus {
        let managed_dir = self.managed_dir(kind);
        let Some(source) = self.resolve_source(kind, configured) else {
            return RuntimeStatus {
                kind,
                available: false,
                source: None,
                binary_path: None,
                version: None,
                managed_dir,
            };
        };

        let binary_path = source.path().to_path_buf();
        let version = probe_version(&binary_path);

        RuntimeStatus {
            kind,
            available: true,
            source: Some(source),
            binary_path: Some(binary_path),
            version,
            managed_dir,
        }
    }

    /// Probe all supported external runtimes with no custom overrides.
    pub fn probe_all(&self) -> Vec<RuntimeStatus> {
        RuntimeKind::ALL
            .into_iter()
            .map(|kind| self.probe(kind, None))
            .collect()
    }

    /// Ensure `kind` is available; if already present (configured, system, or managed),
    /// returns its status immediately without downloading. Otherwise installs the managed runtime.
    pub async fn ensure_installed(
        &self,
        kind: RuntimeKind,
        configured: Option<&Path>,
    ) -> Result<RuntimeStatus> {
        let status = self.probe(kind, configured);
        if status.available {
            return Ok(status);
        }
        self.install_managed(kind, None, None).await
    }

    /// Download, verify, stage, and atomically activate a managed runtime under
    /// `<state_dir>/runtimes/<kind>/current/<binary>`.
    pub async fn install_managed(
        &self,
        kind: RuntimeKind,
        custom_url: Option<&str>,
        expected_sha256: Option<&str>,
    ) -> Result<RuntimeStatus> {
        let url = kind.resolve_release_url(custom_url)?;
        let has_override = custom_url.is_some()
            || std::env::var(kind.release_url_env_var())
                .ok()
                .is_some_and(|v| !v.trim().is_empty());
        let payload = fetch_release_payload(&url, has_override).await?;
        self.stage_and_activate(kind, &payload, expected_sha256)
    }

    /// Verify optional SHA-256 checksum, extract or write `payload` into a staging directory
    /// `<state_dir>/runtimes/<kind>/staging-<uuid>`, validate native binary headers and
    /// `--version`, set `0755` permissions, and atomically switch `current`.
    pub fn stage_and_activate(
        &self,
        kind: RuntimeKind,
        payload: &[u8],
        expected_sha256: Option<&str>,
    ) -> Result<RuntimeStatus> {
        anyhow::ensure!(!payload.is_empty(), "runtime payload is empty");
        anyhow::ensure!(
            payload.len() as u64 <= MAX_RUNTIME_ARCHIVE_BYTES,
            "runtime payload exceeds maximum size limit ({} bytes)",
            MAX_RUNTIME_ARCHIVE_BYTES
        );

        if let Some(expected) = expected_sha256.map(str::trim).filter(|s| !s.is_empty()) {
            verify_sha256_bytes(payload, expected)?;
        }

        let managed_root = self.managed_dir(kind);
        fs::create_dir_all(&managed_root).with_context(|| {
            format!(
                "failed to create managed runtime directory {}",
                managed_root.display()
            )
        })?;

        let install_id = uuid::Uuid::new_v4();
        let staging_dir = managed_root.join(format!("staging-{install_id}"));
        fs::create_dir_all(&staging_dir).with_context(|| {
            format!(
                "failed to create runtime staging directory {}",
                staging_dir.display()
            )
        })?;
        let mut staging_guard = StagingGuard::new(staging_dir.clone());

        let staged_binary = extract_or_write_payload(kind, payload, &staging_dir)?;
        set_executable_permissions(&staged_binary)?;
        verify_staged_binary_header(&staged_binary)?;

        // For Herdr, ensure both `herdr` and `herdr-core` resolve in the staged directory.
        #[cfg(unix)]
        if kind == RuntimeKind::Herdr {
            let alias = staging_dir.join("herdr-core");
            if !alias.exists() {
                let _ = std::os::unix::fs::symlink(kind.primary_binary_name(), &alias);
            }
        }

        let version = probe_version_strict(&staged_binary).with_context(|| {
            format!(
                "staged {} binary failed `--version` verification",
                kind.display_name()
            )
        })?;

        activate_staged_directory(&managed_root, &staging_dir, install_id)?;
        staging_guard.disarm();

        let final_binary = self.managed_binary_path(kind);
        Ok(RuntimeStatus {
            kind,
            available: true,
            source: Some(RuntimeSource::Managed(final_binary.clone())),
            binary_path: Some(final_binary),
            version: Some(version),
            managed_dir: managed_root,
        })
    }

    fn collect_system_dirs(&self, kind: RuntimeKind) -> Vec<PathBuf> {
        let managed_root = self.managed_dir(kind);
        if let Some(explicit_dirs) = &self.system_search_dirs {
            return explicit_dirs
                .iter()
                .filter(|dir| !dir.starts_with(&managed_root))
                .cloned()
                .collect();
        }

        let mut dirs = Vec::new();
        let mut seen = HashSet::new();
        let mut push_unique = |dir: PathBuf| {
            if !dir.as_os_str().is_empty()
                && !dir.starts_with(&managed_root)
                && seen.insert(dir.clone())
            {
                dirs.push(dir);
            }
        };

        if let Some(path_os) = std::env::var_os("PATH") {
            for entry in std::env::split_paths(&path_os) {
                push_unique(entry);
            }
        }

        if let Some(login_path) = crate::terminal::login_env::login_path() {
            for entry in std::env::split_paths(&login_path) {
                push_unique(entry);
            }
        }

        if let Some(home) = dirs::home_dir() {
            match kind {
                RuntimeKind::Bun => {
                    push_unique(home.join(".bun").join("bin"));
                    push_unique(home.join(".local").join("share").join("mise").join("shims"));
                    push_unique(home.join(".local").join("bin"));
                    push_unique(home.join(".cargo").join("bin"));
                }
                RuntimeKind::Herdr => {
                    push_unique(home.join(".cargo").join("bin"));
                    push_unique(home.join(".local").join("bin"));
                    push_unique(home.join(".local").join("share").join("mise").join("shims"));
                }
            }
        }

        for sys_dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
            push_unique(PathBuf::from(sys_dir));
        }

        dirs
    }
}

/// Return the managed directory for `kind` under the global [`crate::platform::store::state_dir`].
pub fn managed_dir(kind: RuntimeKind) -> Result<PathBuf> {
    Ok(RuntimeManager::new()?.managed_dir(kind))
}

/// Resolve the source for `kind` using the default [`RuntimeManager`].
pub fn resolve_source(kind: RuntimeKind, configured: Option<&Path>) -> Option<RuntimeSource> {
    RuntimeManager::new().ok()?.resolve_source(kind, configured)
}

/// Resolve the binary path for `kind` using the default [`RuntimeManager`].
pub fn resolve_binary(kind: RuntimeKind, configured: Option<&Path>) -> Option<PathBuf> {
    RuntimeManager::new().ok()?.resolve_binary(kind, configured)
}

/// Probe `kind` using the default [`RuntimeManager`].
pub fn probe(kind: RuntimeKind, configured: Option<&Path>) -> RuntimeStatus {
    match RuntimeManager::new() {
        Ok(manager) => manager.probe(kind, configured),
        Err(_) => {
            let fallback = RuntimeManager::with_state_dir(std::env::temp_dir().join("muqun-state"));
            fallback.probe(kind, configured)
        }
    }
}

/// Probe all supported external runtimes using the default [`RuntimeManager`].
pub fn probe_all() -> Vec<RuntimeStatus> {
    RuntimeKind::ALL
        .into_iter()
        .map(|kind| probe(kind, None))
        .collect()
}

/// Ensure `kind` is available using the default [`RuntimeManager`], installing it if absent.
pub async fn ensure_installed(
    kind: RuntimeKind,
    configured: Option<&Path>,
) -> Result<RuntimeStatus> {
    RuntimeManager::new()?
        .ensure_installed(kind, configured)
        .await
}

/// Download and install `kind` into the managed runtime directory using the default [`RuntimeManager`].
pub async fn install_managed(
    kind: RuntimeKind,
    custom_url: Option<&str>,
    expected_sha256: Option<&str>,
) -> Result<RuntimeStatus> {
    RuntimeManager::new()?
        .install_managed(kind, custom_url, expected_sha256)
        .await
}

/// Probe `<binary> --version` with a bounded timeout ([`VERSION_PROBE_TIMEOUT`])
/// and output cap ([`VERSION_PROBE_MAX_BYTES`]).
pub fn probe_version(binary_path: &Path) -> Option<String> {
    probe_version_strict(binary_path).ok()
}

/// Probe `<binary> --version` with a bounded timeout and output cap, returning an error on failure.
pub fn probe_version_strict(binary_path: &Path) -> Result<String> {
    let mut command = Command::new(binary_path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Handle transient ETXTBSY on Linux when a freshly written binary is executed immediately.
    let spawn_deadline = Instant::now() + Duration::from_millis(500);
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            #[cfg(unix)]
            Err(err)
                if err.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < spawn_deadline =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to spawn `{}` for version probe",
                        binary_path.display()
                    )
                });
            }
        }
    };

    let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "version probe for `{}` timed out after {:?}",
                    binary_path.display(),
                    VERSION_PROBE_TIMEOUT
                );
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err).with_context(|| {
                    format!(
                        "failed waiting on `{}` version probe",
                        binary_path.display()
                    )
                });
            }
        }
    };

    let mut stdout_bytes = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let _ = stdout
            .take(VERSION_PROBE_MAX_BYTES)
            .read_to_end(&mut stdout_bytes);
    }

    let mut stderr_bytes = Vec::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = stderr
            .take(VERSION_PROBE_MAX_BYTES)
            .read_to_end(&mut stderr_bytes);
    }

    anyhow::ensure!(
        status.success(),
        "`{} --version` exited with status {}",
        binary_path.display(),
        status
    );

    let stdout_text = String::from_utf8_lossy(&stdout_bytes);
    if let Some(parsed) = parse_version_output(&stdout_text) {
        return Ok(parsed);
    }

    let stderr_text = String::from_utf8_lossy(&stderr_bytes);
    if let Some(parsed) = parse_version_output(&stderr_text) {
        return Ok(parsed);
    }

    anyhow::bail!(
        "`{} --version` produced no recognizable version string",
        binary_path.display()
    )
}

/// Extract a clean version string from `--version` output.
pub fn parse_version_output(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut cleaned = line;
    for prefix in ["bun ", "herdr-core ", "herdr "] {
        if cleaned.len() > prefix.len() && cleaned[..prefix.len()].eq_ignore_ascii_case(prefix) {
            cleaned = cleaned[prefix.len()..].trim();
            break;
        }
    }
    if let Some(stripped) = cleaned
        .strip_prefix('v')
        .or_else(|| cleaned.strip_prefix('V'))
    {
        if stripped.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
            cleaned = stripped;
        }
    }
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned.chars().any(char::is_control) {
        return None;
    }
    Some(cleaned.to_owned())
}

/// Check whether `bytes` begins with a valid native executable header (ELF, Mach-O, or PE).
pub fn is_native_binary_header(bytes: &[u8]) -> bool {
    if bytes.len() >= 4 {
        // ELF magic: 0x7F 'E' 'L' 'F'
        if bytes[..4] == *b"\x7fELF" {
            return true;
        }
        // Mach-O 32-bit, 64-bit, and universal/fat magic values (both endiannesses)
        let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if matches!(
            magic,
            0xfeedface | 0xcefaedfe | 0xfeedfacf | 0xcffaedfe | 0xcafebabe | 0xbebafeca
        ) {
            return true;
        }
    }
    // Windows PE/COFF MZ header
    if bytes.len() >= 2 && bytes[..2] == *b"MZ" {
        return true;
    }
    false
}

/// Verify that `path` starts with a native binary executable header (ELF, Mach-O, or PE).
pub fn verify_native_binary_header(path: &Path) -> Result<()> {
    let mut header = [0_u8; 4];
    let mut file =
        File::open(path).with_context(|| format!("failed to open binary `{}`", path.display()))?;
    let read = file
        .read(&mut header)
        .with_context(|| format!("failed to read binary header from `{}`", path.display()))?;
    anyhow::ensure!(
        is_native_binary_header(&header[..read]),
        "`{}` does not have a valid native executable header (expected ELF, Mach-O, or PE)",
        path.display()
    );
    Ok(())
}

fn verify_staged_binary_header(path: &Path) -> Result<()> {
    #[cfg(test)]
    {
        let mut header = [0_u8; 4];
        if let Ok(mut file) = File::open(path) {
            if let Ok(n) = file.read(&mut header) {
                if n >= 2 && &header[..2] == b"#!" {
                    return Ok(());
                }
            }
        }
    }
    verify_native_binary_header(path)
}

fn verify_sha256_bytes(payload: &[u8], expected: &str) -> Result<()> {
    let token = expected
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(expected.trim())
        .split_whitespace()
        .next()
        .unwrap_or_default();
    anyhow::ensure!(
        token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid expected SHA-256 hex digest"
    );
    let actual = crate::hex(Sha256::digest(payload).as_slice());
    anyhow::ensure!(
        actual.eq_ignore_ascii_case(token),
        "SHA-256 checksum mismatch (expected {}, got {})",
        token.to_ascii_lowercase(),
        actual
    );
    Ok(())
}

async fn fetch_release_payload(url: &str, allow_local_override: bool) -> Result<Vec<u8>> {
    if allow_local_override {
        if let Some(file_path) = url.strip_prefix("file://") {
            let bytes = tokio::fs::read(file_path)
                .await
                .with_context(|| format!("failed to read local runtime payload {file_path}"))?;
            anyhow::ensure!(
                bytes.len() as u64 <= MAX_RUNTIME_ARCHIVE_BYTES,
                "local runtime payload exceeds size limit"
            );
            return Ok(bytes);
        }
        let candidate_path = Path::new(url);
        if candidate_path.is_absolute() && candidate_path.is_file() {
            let bytes = tokio::fs::read(candidate_path).await.with_context(|| {
                format!(
                    "failed to read local runtime payload {}",
                    candidate_path.display()
                )
            })?;
            anyhow::ensure!(
                bytes.len() as u64 <= MAX_RUNTIME_ARCHIVE_BYTES,
                "local runtime payload exceeds size limit"
            );
            return Ok(bytes);
        }
    }

    let parsed = reqwest::Url::parse(url).context("invalid runtime release URL")?;
    let is_loopback_http = allow_local_override
        && parsed.scheme() == "http"
        && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
    anyhow::ensure!(
        parsed.scheme() == "https" || is_loopback_http,
        "runtime release download requires HTTPS (got scheme `{}`)",
        parsed.scheme()
    );

    let client = reqwest::Client::builder()
        .https_only(!is_loopback_http)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .user_agent(concat!("muqun-gateway/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to initialize HTTP client for runtime installation")?;

    let mut response = client
        .get(parsed)
        .send()
        .await
        .context("runtime download request failed")?;
    anyhow::ensure!(
        response.status().is_success(),
        "runtime download server returned HTTP {}",
        response.status().as_u16()
    );
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|len| len <= MAX_RUNTIME_ARCHIVE_BYTES),
        "runtime download exceeds maximum size limit"
    );

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed reading runtime download stream")?
    {
        anyhow::ensure!(
            body.len() as u64 + chunk.len() as u64 <= MAX_RUNTIME_ARCHIVE_BYTES,
            "runtime download stream exceeded maximum size limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn extract_or_write_payload(
    kind: RuntimeKind,
    payload: &[u8],
    staging_dir: &Path,
) -> Result<PathBuf> {
    let target_bin = staging_dir.join(kind.primary_binary_name());

    // 1. ZIP archive (PK\x03\x04)
    if payload.len() >= 4 && payload[..4] == *b"PK\x03\x04" {
        let archive_path = staging_dir.join("archive.zip");
        let extract_dir = staging_dir.join("extracted");
        fs::create_dir_all(&extract_dir)?;
        fs::write(&archive_path, payload)?;

        extract_zip_archive(&archive_path, &extract_dir)?;
        let found =
            find_extracted_binary(&extract_dir, kind.binary_names()).with_context(|| {
                format!(
                    "ZIP archive did not contain expected {} executable",
                    kind.display_name()
                )
            })?;
        fs::copy(&found, &target_bin)?;
        let _ = fs::remove_file(&archive_path);
        let _ = fs::remove_dir_all(&extract_dir);
        return Ok(target_bin);
    }

    // 2. Gzipped tar archive (\x1f\x8b)
    if payload.len() >= 2 && payload[..2] == *b"\x1f\x8b" {
        let archive_path = staging_dir.join("archive.tar.gz");
        let extract_dir = staging_dir.join("extracted");
        fs::create_dir_all(&extract_dir)?;
        fs::write(&archive_path, payload)?;

        extract_tar_gz_archive(&archive_path, &extract_dir)?;
        let found =
            find_extracted_binary(&extract_dir, kind.binary_names()).with_context(|| {
                format!(
                    "tar.gz archive did not contain expected {} executable",
                    kind.display_name()
                )
            })?;
        fs::copy(&found, &target_bin)?;
        let _ = fs::remove_file(&archive_path);
        let _ = fs::remove_dir_all(&extract_dir);
        return Ok(target_bin);
    }

    // 3. Direct uncompressed binary
    write_synced_file(&target_bin, payload)?;
    Ok(target_bin)
}

fn extract_zip_archive(archive_path: &Path, extract_dir: &Path) -> Result<()> {
    let unzip_status = Command::new("unzip")
        .arg("-q")
        .arg("-o")
        .arg(archive_path)
        .arg("-d")
        .arg(extract_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if unzip_status.is_ok_and(|s| s.success()) {
        return Ok(());
    }

    // Fallback to bsdtar (`tar -xf`) when `unzip` is not installed.
    let tar_status = Command::new("tar")
        .arg("-xf")
        .arg(archive_path)
        .arg("-C")
        .arg(extract_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run `unzip` or `tar` to extract runtime ZIP archive")?;

    anyhow::ensure!(
        tar_status.success(),
        "failed to extract runtime ZIP archive {}",
        archive_path.display()
    );
    Ok(())
}

fn extract_tar_gz_archive(archive_path: &Path, extract_dir: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive_path)
        .arg("-C")
        .arg(extract_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to invoke `tar` to extract runtime archive")?;

    anyhow::ensure!(
        status.success(),
        "`tar -xzf` failed for runtime archive {}",
        archive_path.display()
    );
    Ok(())
}

fn find_extracted_binary(root: &Path, names: &[&str]) -> Option<PathBuf> {
    let canonical_root = fs::canonicalize(root).ok()?;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut sorted: Vec<_> = entries.flatten().collect();
        sorted.sort_by_key(|e| e.file_name());
        for entry in sorted {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if names.contains(&file_name) {
                    if let Ok(canon) = fs::canonicalize(&path) {
                        if canon.starts_with(&canonical_root) {
                            return Some(canon);
                        }
                    }
                }
            }
        }
    }
    None
}

fn write_synced_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path)
        .with_context(|| format!("failed to create staged file {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write staged file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync staged file {}", path.display()))?;
    Ok(())
}

fn set_executable_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).with_context(|| {
            format!(
                "failed to set 0755 executable permissions on {}",
                path.display()
            )
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn activate_staged_directory(
    managed_root: &Path,
    staging_dir: &Path,
    install_id: uuid::Uuid,
) -> Result<()> {
    let versions_dir = managed_root.join("versions");
    fs::create_dir_all(&versions_dir).with_context(|| {
        format!(
            "failed to create versions directory {}",
            versions_dir.display()
        )
    })?;

    let generation_name = format!("rel-{install_id}");
    let generation_dir = versions_dir.join(&generation_name);
    fs::rename(staging_dir, &generation_dir).with_context(|| {
        format!(
            "failed to move staged runtime {} to {}",
            staging_dir.display(),
            generation_dir.display()
        )
    })?;

    let current_path = managed_root.join("current");

    #[cfg(unix)]
    {
        // If `current` is an ordinary directory rather than a symlink, move it aside first
        // so `rename(temp_symlink, current)` can atomically replace `current`.
        if let Ok(meta) = fs::symlink_metadata(&current_path) {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                let displaced = managed_root.join(format!(".current-old-{install_id}"));
                fs::rename(&current_path, &displaced)?;
                let _ = fs::remove_dir_all(&displaced);
            }
        }

        let tmp_link = managed_root.join(format!(".current-tmp-{install_id}"));
        let _ = fs::remove_file(&tmp_link);
        let relative_target = Path::new("versions").join(&generation_name);
        std::os::unix::fs::symlink(&relative_target, &tmp_link).with_context(|| {
            format!(
                "failed to create staging symlink {} -> {}",
                tmp_link.display(),
                relative_target.display()
            )
        })?;
        fs::rename(&tmp_link, &current_path).with_context(|| {
            format!(
                "failed to atomically switch {} to {}",
                current_path.display(),
                relative_target.display()
            )
        })?;
    }

    #[cfg(not(unix))]
    {
        if current_path.exists() {
            let displaced = managed_root.join(format!(".current-old-{install_id}"));
            let _ = fs::rename(&current_path, &displaced);
            let _ = fs::remove_dir_all(&displaced);
        }
        fs::rename(&generation_dir, &current_path)?;
    }

    // Prune older generations under `versions/` on best-effort basis.
    if let Ok(entries) = fs::read_dir(&versions_dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_str() != Some(generation_name.as_str()) {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    Ok(())
}

struct StagingGuard {
    path: PathBuf,
    active: bool,
}

impl StagingGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, active: true }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    if let Some(raw) = path.to_str() {
        if raw == "~" {
            if let Some(home) = dirs::home_dir() {
                return home;
            }
        } else if let Some(rest) = raw.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest);
            }
        }
    }
    path.to_path_buf()
}

fn is_executable(candidate: &Path) -> bool {
    let Ok(metadata) = fs::metadata(candidate) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "muqun-runtimes-{prefix}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&dir).expect("failed to create temp test dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_mock_executable(path: &Path, script_body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let content = format!("#!/bin/sh\n{script_body}\n");
        fs::write(path, content.as_bytes()).unwrap();
        set_executable_permissions(path).unwrap();
    }

    #[test]
    fn discovery_priority_prefers_configured_then_system_then_managed() {
        let temp = TempDir::new("priority");
        let state_dir = temp.path().join("state");
        let system_dir = temp.path().join("system-bin");
        let custom_dir = temp.path().join("custom-bin");
        fs::create_dir_all(&system_dir).unwrap();
        fs::create_dir_all(&custom_dir).unwrap();

        let manager = RuntimeManager::with_search_dirs(state_dir.clone(), vec![system_dir.clone()]);

        // 0. Nothing installed anywhere -> unavailable.
        let status = manager.probe(RuntimeKind::Bun, None);
        assert!(!status.available);
        assert_eq!(status.source, None);
        assert_eq!(status.binary_path, None);
        assert_eq!(status.version, None);
        assert_eq!(status.managed_dir, state_dir.join("runtimes").join("bun"));

        // 1. Only Managed exists -> resolves Managed.
        let managed_bin = manager.managed_binary_path(RuntimeKind::Bun);
        write_mock_executable(&managed_bin, "echo '1.1.0-managed'");
        let status = manager.probe(RuntimeKind::Bun, None);
        assert!(status.available);
        assert_eq!(
            status.source,
            Some(RuntimeSource::Managed(managed_bin.clone()))
        );
        assert_eq!(status.binary_path, Some(managed_bin.clone()));
        assert_eq!(status.version.as_deref(), Some("1.1.0-managed"));

        // 2. System binary added -> System wins over Managed.
        let system_bin = system_dir.join("bun");
        write_mock_executable(&system_bin, "echo 'bun 1.2.0-system'");
        let status = manager.probe(RuntimeKind::Bun, None);
        assert!(status.available);
        assert_eq!(
            status.source,
            Some(RuntimeSource::System(system_bin.clone()))
        );
        assert_eq!(status.binary_path, Some(system_bin.clone()));
        assert_eq!(status.version.as_deref(), Some("1.2.0-system"));

        // 3. Configured binary provided -> Configured wins over System and Managed.
        let configured_bin = custom_dir.join("my-bun");
        write_mock_executable(&configured_bin, "echo 'v1.3.0-configured'");
        let status = manager.probe(RuntimeKind::Bun, Some(&configured_bin));
        assert!(status.available);
        assert_eq!(
            status.source,
            Some(RuntimeSource::Configured(configured_bin.clone()))
        );
        assert_eq!(status.binary_path, Some(configured_bin));
        assert_eq!(status.version.as_deref(), Some("1.3.0-configured"));

        // 4. Non-existent configured path falls back cleanly to System.
        let missing = custom_dir.join("does-not-exist");
        let fallback = manager.probe(RuntimeKind::Bun, Some(&missing));
        assert_eq!(fallback.source, Some(RuntimeSource::System(system_bin)));
    }

    #[test]
    fn herdr_discovery_recognizes_both_herdr_and_herdr_core() {
        let temp = TempDir::new("herdr-names");
        let state_dir = temp.path().join("state");
        let system_dir = temp.path().join("system-bin");
        fs::create_dir_all(&system_dir).unwrap();

        let manager = RuntimeManager::with_search_dirs(&state_dir, vec![system_dir.clone()]);

        // Only `herdr-core` on system PATH -> discovered as Herdr.
        let herdr_core = system_dir.join("herdr-core");
        write_mock_executable(&herdr_core, "echo 'herdr-core 0.9.5'");
        let status = manager.probe(RuntimeKind::Herdr, None);
        assert!(status.available);
        assert_eq!(status.source, Some(RuntimeSource::System(herdr_core)));
        assert_eq!(status.version.as_deref(), Some("0.9.5"));

        // When `herdr` is also present, `herdr` takes precedence over `herdr-core`.
        let herdr = system_dir.join("herdr");
        write_mock_executable(&herdr, "echo 'herdr 0.10.0'");
        let status = manager.probe(RuntimeKind::Herdr, None);
        assert!(status.available);
        assert_eq!(status.source, Some(RuntimeSource::System(herdr)));
        assert_eq!(status.version.as_deref(), Some("0.10.0"));
    }

    #[test]
    fn version_probing_extracts_clean_version_and_rejects_failures() {
        let temp = TempDir::new("version-probe");

        let ok_script = temp.path().join("mock-runtime");
        write_mock_executable(&ok_script, "printf 'herdr 0.14.2\\nextra line\\n'");
        assert_eq!(probe_version(&ok_script).as_deref(), Some("0.14.2"));

        let fail_script = temp.path().join("failing-runtime");
        write_mock_executable(&fail_script, "exit 1");
        assert_eq!(probe_version(&fail_script), None);

        let empty_script = temp.path().join("empty-runtime");
        write_mock_executable(&empty_script, "printf '\\n'");
        assert_eq!(probe_version(&empty_script), None);
    }

    #[test]
    fn native_binary_header_validation_checks_elf_macho_and_pe() {
        let temp = TempDir::new("headers");

        let elf = temp.path().join("elf-bin");
        fs::write(&elf, b"\x7fELF\x02\x01\x01\x00").unwrap();
        assert!(verify_native_binary_header(&elf).is_ok());

        let macho = temp.path().join("macho-bin");
        fs::write(&macho, [0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01]).unwrap();
        assert!(verify_native_binary_header(&macho).is_ok());

        let pe = temp.path().join("pe-bin");
        fs::write(&pe, b"MZ\x90\x00\x03\x00").unwrap();
        assert!(verify_native_binary_header(&pe).is_ok());

        let html = temp.path().join("error.html");
        fs::write(&html, b"<!DOCTYPE html><html>Not Found</html>").unwrap();
        assert!(verify_native_binary_header(&html).is_err());

        let script = temp.path().join("script.sh");
        fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
        assert!(
            verify_native_binary_header(&script).is_err(),
            "strict native header check must reject shell scripts"
        );
    }

    #[test]
    fn atomic_staging_and_activation_switches_current_and_cleans_up() {
        let temp = TempDir::new("staging");
        let state_dir = temp.path().join("state");
        let empty_sys = temp.path().join("empty-sys");
        fs::create_dir_all(&empty_sys).unwrap();

        let manager = RuntimeManager::with_search_dirs(&state_dir, vec![empty_sys]);

        // Stage v1 with SHA-256 verification.
        let v1_payload = b"#!/bin/sh\necho 'bun 1.1.40'\n";
        let v1_sha = crate::hex(Sha256::digest(v1_payload).as_slice());

        let status_v1 = manager
            .stage_and_activate(RuntimeKind::Bun, v1_payload, Some(&v1_sha))
            .expect("v1 staging should succeed");
        assert!(status_v1.available);
        assert_eq!(status_v1.version.as_deref(), Some("1.1.40"));
        let expected_bin = manager.managed_binary_path(RuntimeKind::Bun);
        assert_eq!(
            status_v1.source,
            Some(RuntimeSource::Managed(expected_bin.clone()))
        );
        assert!(expected_bin.is_file());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&expected_bin).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "installed binary must have 0755 permissions");
            let link_meta =
                fs::symlink_metadata(manager.managed_current_dir(RuntimeKind::Bun)).unwrap();
            assert!(
                link_meta.file_type().is_symlink(),
                "`current` must be an atomic symlink on Unix"
            );
        }

        // Upgrade to v2 and ensure `current` switches atomically and old versions are pruned.
        let v2_payload = b"#!/bin/sh\necho 'bun 1.2.5'\n";
        let status_v2 = manager
            .stage_and_activate(RuntimeKind::Bun, v2_payload, None)
            .expect("v2 upgrade should succeed");
        assert_eq!(status_v2.version.as_deref(), Some("1.2.5"));
        let probed = manager.probe(RuntimeKind::Bun, None);
        assert_eq!(probed.version.as_deref(), Some("1.2.5"));

        // Verify no leftover `staging-*` directories exist and only 1 generation remains in `versions/`.
        let managed_root = manager.managed_dir(RuntimeKind::Bun);
        let staging_leftovers: Vec<_> = fs::read_dir(&managed_root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("staging-"))
            .collect();
        assert!(
            staging_leftovers.is_empty(),
            "no staging directories should remain after activation"
        );
        let versions_count = fs::read_dir(managed_root.join("versions"))
            .unwrap()
            .flatten()
            .count();
        assert_eq!(
            versions_count, 1,
            "previous generation should be pruned after switching current"
        );

        // Verify a failed staging attempt (bad SHA-256 or broken binary) leaves v2 intact!
        let bad_sha = manager.stage_and_activate(
            RuntimeKind::Bun,
            b"#!/bin/sh\necho 'bun 9.9.9'\n",
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
        );
        assert!(bad_sha.is_err());

        let broken_bin = manager.stage_and_activate(RuntimeKind::Bun, b"#!/bin/sh\nexit 1\n", None);
        assert!(broken_bin.is_err());

        // v2 must still be active and untouched.
        let still_v2 = manager.probe(RuntimeKind::Bun, None);
        assert_eq!(still_v2.version.as_deref(), Some("1.2.5"));
    }

    #[tokio::test]
    async fn install_managed_supports_local_tar_gz_archive_via_override_url() {
        let temp = TempDir::new("archive-install");
        let state_dir = temp.path().join("state");
        let empty_sys = temp.path().join("empty-sys");
        fs::create_dir_all(&empty_sys).unwrap();

        // Build a nested directory structure like an official release archive (`herdr-release/herdr`)
        let archive_src = temp.path().join("archive-src").join("herdr-release");
        fs::create_dir_all(&archive_src).unwrap();
        let src_bin = archive_src.join("herdr");
        write_mock_executable(&src_bin, "echo 'herdr 0.15.0'");

        let tar_gz_path = temp.path().join("herdr-release.tar.gz");
        let tar_ok = Command::new("tar")
            .arg("-czf")
            .arg(&tar_gz_path)
            .arg("-C")
            .arg(temp.path().join("archive-src"))
            .arg("herdr-release")
            .status()
            .is_ok_and(|s| s.success());
        if !tar_ok {
            return;
        }

        let archive_bytes = fs::read(&tar_gz_path).unwrap();
        let sha = crate::hex(Sha256::digest(&archive_bytes).as_slice());
        let file_url = format!("file://{}", tar_gz_path.display());

        let manager = RuntimeManager::with_search_dirs(&state_dir, vec![empty_sys]);
        let status = manager
            .install_managed(RuntimeKind::Herdr, Some(&file_url), Some(&sha))
            .await
            .expect("installing from local tar.gz override should succeed");

        assert!(status.available);
        assert_eq!(status.version.as_deref(), Some("0.15.0"));
        assert!(manager.managed_binary_path(RuntimeKind::Herdr).is_file());
        #[cfg(unix)]
        assert!(manager
            .managed_current_dir(RuntimeKind::Herdr)
            .join("herdr-core")
            .exists());
    }
}
