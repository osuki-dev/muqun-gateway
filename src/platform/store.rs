//! Local configuration, identity, device, pairing, and process storage.

use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::Context as _;
use base64::Engine as _;

use super::manage::{revoke_managed_device, truncate};
use super::state_lock;
use crate::authority::DeviceRecord;
use crate::{
    transport, Config, PairingFile, PushTokenRecord, CONFIG_FILE, DEVICES_BACKUP_FILE,
    DEVICES_FILE, HERDR_PLUGIN_IMPORT_MARKER, MAX_DEVICE_NAME_CHARS, PAIRING_CODE_CHARACTER_COUNT,
    PAIRING_FILE, PID_FILE, PUSH_TOKENS_FILE,
};

pub(crate) fn load_config(config_path: Option<String>) -> anyhow::Result<Config> {
    // Resolved lazily on purpose: `config_dir` consults the rename migration,
    // which loads the pre-rename config by explicit path. An eagerly evaluated
    // default would recurse between the two until the stack ran out.
    let path = match config_path {
        Some(path) => PathBuf::from(path),
        None => config_dir()?.join(CONFIG_FILE),
    };
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn config_dir() -> anyhow::Result<std::path::PathBuf> {
    let standalone = standalone_config_dir()?;
    if !standalone.join(HERDR_PLUGIN_IMPORT_MARKER).exists() {
        if let Ok(path) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
            return Ok(path.into());
        }
    }
    Ok(standalone)
}

/// Where a test run keeps the state a handler persists.
///
/// The handlers under test call the same `write_devices` a running gateway
/// does, and that resolved to the state directory of whatever gateway is
/// installed for the current user -- so running the checks on a machine that
/// also runs this gateway overwrote its real `devices.json` with a test
/// fixture, unpairing every device its owner had. A test binary gets its own
/// directory instead, once per process, and never learns the real one.
#[cfg(test)]
pub(crate) fn test_state_dir() -> std::path::PathBuf {
    static TEST_STATE_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    TEST_STATE_DIR
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!(
                "muqun-gateway-test-state-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&dir).expect("failed to create the test state directory");
            dir
        })
        .clone()
}

#[cfg(test)]
pub(crate) fn state_dir() -> anyhow::Result<std::path::PathBuf> {
    Ok(test_state_dir())
}

/// A unique socket path short enough to bind.
///
/// `sockaddr_un.sun_path` is 104 bytes on macOS and 108 on Linux, and the
/// limit is on the whole path. `std::env::temp_dir()` on macOS is a per-user
/// `/var/folders/<...>/T/` directory 49 characters long, so a name with a UUID
/// in it landed at about 112 -- and every integration test that binds a tmux
/// or herdr socket failed with "File name too long" on the maintainer's own
/// platform. Those are the tests that exist to prove the adapters work against
/// a real server, so they are exactly the ones that must be runnable there.
#[cfg(test)]
pub(crate) fn short_test_socket(prefix: &str) -> std::path::PathBuf {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    std::path::PathBuf::from("/tmp").join(format!("{prefix}-{}.sock", &unique[..12]))
}

#[cfg(not(test))]
pub(crate) fn state_dir() -> anyhow::Result<std::path::PathBuf> {
    let standalone_config = standalone_config_dir()?;
    if !standalone_config.join(HERDR_PLUGIN_IMPORT_MARKER).exists() {
        if let Ok(path) = std::env::var("HERDR_PLUGIN_STATE_DIR") {
            return Ok(path.into());
        }
    }
    standalone_state_dir()
}

/// The gateway's standalone directory name before the muqun-gateway rename.
/// Kept only so an existing install's config and state can be found once and
/// carried forward; nothing else should reference it.
pub(crate) const PRE_RENAME_STANDALONE_DIR_NAME: &str = "herdr-gateway";

/// Move an install's directory from the pre-rename name to the current one,
/// the first time it is asked for after upgrading.
///
/// A same-filesystem `rename` is atomic at the directory-entry level -- there
/// is no window in which both names exist with half the files in each -- so
/// this step itself cannot leave an install half-migrated. What can still
/// split state across both names is a gateway built under the pre-rename
/// name that is *still running* against this directory: it does not know the
/// directory was just renamed out from under it, and the next thing it
/// writes -- a push token, an upload, its own pid file -- recreates the old
/// directory fresh, right next to the one this just moved. So this refuses
/// to migrate at all while such a process is found listening on the old
/// config's port, rather than migrate and let the two diverge; the caller
/// gets a plain error naming the pid to stop instead of a silently split
/// install. Once the new name exists, the old one is never inspected again:
/// a directory an owner later recreates under the old name is left alone
/// rather than merged in.
pub(crate) fn migrate_renamed_standalone_dir(parent: &std::path::Path) -> anyhow::Result<PathBuf> {
    let new_dir = parent.join("muqun-gateway");
    let old_dir = parent.join(PRE_RENAME_STANDALONE_DIR_NAME);
    if new_dir.exists() || !old_dir.exists() {
        return Ok(new_dir);
    }
    let old_config_path = old_dir.join(CONFIG_FILE).to_string_lossy().into_owned();
    if let Ok(old_config) = load_config(Some(old_config_path)) {
        if let Ok(old_pids) = listener_pids_named(old_config.port(), PRE_RENAME_STANDALONE_DIR_NAME)
        {
            if let Some(&pid) = old_pids.first() {
                anyhow::bail!(
                    "a gateway is still running under the pre-rename name {} (pid {pid}); stop \
                     it before this directory can be migrated to muqun-gateway -- migrating \
                     underneath a still-running process would split its data across both names",
                    PRE_RENAME_STANDALONE_DIR_NAME
                );
            }
        }
    }
    std::fs::rename(&old_dir, &new_dir).with_context(|| {
        format!(
            "failed to migrate {} to {}",
            old_dir.display(),
            new_dir.display()
        )
    })?;
    eprintln!("migrated {} to {}", old_dir.display(), new_dir.display());
    Ok(new_dir)
}

pub(crate) fn standalone_config_dir() -> anyhow::Result<PathBuf> {
    let parent = dirs::config_dir().context("failed to locate config directory")?;
    migrate_renamed_standalone_dir(&parent)
}

pub(crate) fn standalone_state_dir() -> anyhow::Result<PathBuf> {
    let parent = dirs::data_dir()
        .or_else(dirs::config_dir)
        .context("failed to locate state directory")?;
    migrate_renamed_standalone_dir(&parent)
}

pub(crate) fn default_herdr_plugin_config_dir() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
        return Ok(path.into());
    }
    Ok(PathBuf::from(default_socket_path())
        .parent()
        .context("failed to locate Herdr config directory")?
        .join("plugins/config/herdr.gateway"))
}

pub(crate) fn default_herdr_plugin_state_dir() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("HERDR_PLUGIN_STATE_DIR") {
        return Ok(path.into());
    }
    Ok(std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
        .context("failed to locate state directory")?
        .join("herdr/plugins/herdr.gateway"))
}

pub(crate) fn read_push_tokens() -> anyhow::Result<Vec<PushTokenRecord>> {
    let path = state_dir()?.join(PUSH_TOKENS_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read push token file {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn write_push_tokens(tokens: &[PushTokenRecord]) -> anyhow::Result<()> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create state dir {}", dir.display()))?;
    let path = dir.join(PUSH_TOKENS_FILE);
    write_secret_file(&path, &serde_json::to_vec_pretty(tokens)?)
}

pub(crate) fn read_devices() -> anyhow::Result<Vec<DeviceRecord>> {
    read_devices_at(&state_dir()?.join(DEVICES_FILE))
}

/// Load the push-token list for a process that will later persist it.
///
/// Same shape as the device file -- held in memory, rewritten whole -- but not
/// the same stakes. A device re-registers its push token on the next app
/// launch, so a list lost here refills itself, while a lost pairing has to be
/// re-established by hand on the device. So this starts anyway rather than
/// refusing the way `load_devices_for_service` does, and says on stderr what
/// it dropped instead of starting silently empty.
pub(crate) fn load_push_tokens_for_service() -> Vec<PushTokenRecord> {
    match read_push_tokens() {
        Ok(tokens) => tokens,
        Err(error) => {
            eprintln!(
                "could not read the push token file ({error:#}); starting with none -- devices \
                 will re-register on their next app launch"
            );
            Vec::new()
        }
    }
}

/// Read the paired-device list, keeping "nothing is paired yet" and "the
/// pairings are there but I cannot read them" as two different answers.
///
/// Collapsing them is what makes this file dangerous. The list is held in
/// memory for the life of the process and every change writes the whole list
/// back, so a caller that turns an unreadable file into an empty list has
/// armed the next pairing to overwrite every record that file still held. An
/// absent file is genuinely empty; a present one that will not parse is an
/// error the caller has to handle.
pub(crate) fn read_devices_at(path: &std::path::Path) -> anyhow::Result<Vec<DeviceRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read device file {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse device file {}", path.display()))
}

/// Load the device list for a process that will later persist it.
///
/// Starting a gateway with an empty list because its device file could not be
/// read is unrecoverable: the first pairing writes that empty list back over
/// the file. Refusing to start leaves the file, and the backup beside it,
/// exactly as they are so an operator can restore them.
pub(crate) fn load_devices_for_service() -> anyhow::Result<Vec<DeviceRecord>> {
    let dir = state_dir()?;
    read_devices_at(&dir.join(DEVICES_FILE)).with_context(|| {
        format!(
            "refusing to start with an unreadable device file -- starting would overwrite it \
             with an empty list and permanently lose every pairing. Restore it from {} and \
             start again.",
            dir.join(DEVICES_BACKUP_FILE).display()
        )
    })
}

pub(crate) fn write_devices(devices: &[DeviceRecord]) -> anyhow::Result<()> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create state dir {}", dir.display()))?;
    write_devices_at(&dir, devices)
}

/// Persist the device list, keeping the generation it replaces beside it.
///
/// The backup is what turns a bad list into an inconvenience instead of a
/// loss. Writing it costs one more small file per change, which is nothing
/// next to re-pairing every device the owner has.
pub(crate) fn write_devices_at(
    dir: &std::path::Path,
    devices: &[DeviceRecord],
) -> anyhow::Result<()> {
    let path = dir.join(DEVICES_FILE);
    if let Ok(previous) = std::fs::read(&path) {
        if !previous.is_empty() {
            write_secret_file(&dir.join(DEVICES_BACKUP_FILE), &previous)?;
        }
    }
    write_secret_file(&path, &serde_json::to_vec_pretty(devices)?)
}

pub(crate) fn list_devices() -> anyhow::Result<()> {
    let devices = read_devices()?;
    if devices.is_empty() {
        println!("no paired devices");
        return Ok(());
    }
    for device in devices {
        println!(
            "{}  {}  paired {}  last seen {}",
            device.id,
            truncate(&device.name, 32),
            format_unix_ms(device.paired_unix_ms),
            format_unix_ms(device.last_seen_unix_ms)
        );
    }
    Ok(())
}

pub(crate) fn revoke_device(device_id: Option<String>, all: bool) -> anyhow::Result<()> {
    if all {
        let devices = read_devices()?;
        let mut count = 0_usize;
        for device in devices {
            count += usize::from(revoke_managed_device(&device.id)?);
        }
        println!("revoked {count} device token(s)");
        return Ok(());
    }
    let Some(device_id) = device_id else {
        anyhow::bail!("pass a device id from `gateway devices`, or --all");
    };
    // A running gateway owns the in-memory device list. Revoke through its
    // narrow manager API so a later pairing write cannot resurrect a token
    // removed only from disk. The helper falls back to disk when it is stopped.
    if !revoke_managed_device(&device_id)? {
        anyhow::bail!("device {device_id} not found");
    }
    println!("revoked device {device_id}");
    Ok(())
}

/// Drop one device straight from the file, with no gateway running.
///
/// The whole read-modify-write happens under the state-directory lock, not
/// just the write. Locking the write alone would still lose records: another
/// writer's change can land between the read below and the write that follows
/// it, and the write puts back a list that never contained it. Holding the
/// lock across both also means that when a gateway *is* running -- it owns the
/// directory for its lifetime -- this refuses instead of editing the list
/// behind its back, which is right, because the running gateway would rewrite
/// its own in-memory list over this one at the next pairing anyway.
pub(crate) fn revoke_device_by_id(device_id: &str) -> anyhow::Result<bool> {
    revoke_device_at(&state_dir()?, device_id)
}

pub(crate) fn revoke_device_at(dir: &std::path::Path, device_id: &str) -> anyhow::Result<bool> {
    let removed = update_devices_at(dir, |devices| {
        let previous_len = devices.len();
        devices.retain(|device| device.id != device_id);
        (devices.len() != previous_len).then_some(())
    })?;
    Ok(removed.is_some())
}

/// Load the device list, change it, and store it back, owning the state
/// directory from the first byte read to the last byte written.
///
/// The lock has to span the read as well as the write. A change that locked
/// only its write would read the list, let another writer's pairing land in
/// the gap, and then store a list that never contained it -- the whole-file
/// overwrite this module exists to prevent, just with a smaller window. So
/// `change` runs inside the critical section, and must not block: everything
/// else that wants to touch this directory is refused while it runs.
///
/// `change` returns `None` when it decided not to change anything, which
/// leaves the file -- and the backup generation beside it -- untouched.
pub(crate) fn update_devices_at<T>(
    dir: &std::path::Path,
    change: impl FnOnce(&mut Vec<DeviceRecord>) -> Option<T>,
) -> anyhow::Result<Option<T>> {
    let _lock = state_lock::StateLock::acquire(dir)
        .context("cannot edit the device list on disk while a gateway owns it")?;
    let mut devices = read_devices_at(&dir.join(DEVICES_FILE))?;
    let Some(outcome) = change(&mut devices) else {
        return Ok(None);
    };
    write_devices_at(dir, &devices)?;
    Ok(Some(outcome))
}

pub(crate) fn format_unix_ms(value: u128) -> String {
    if value == 0 {
        return String::from("never");
    }
    let seconds_ago = now_unix_ms().saturating_sub(value) / 1000;
    match seconds_ago {
        0..=59 => String::from("just now"),
        60..=3599 => format!("{}m ago", seconds_ago / 60),
        3600..=86399 => format!("{}h ago", seconds_ago / 3600),
        _ => format!("{}d ago", seconds_ago / 86400),
    }
}

pub(crate) fn pid_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(state_dir()?.join(PID_FILE))
}

pub(crate) fn read_pid() -> anyhow::Result<Option<u32>> {
    let path = pid_path()?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    Ok(text.trim().parse::<u32>().ok())
}

/// Whether `config.json` has been rewritten more recently than the running
/// gateway process started.
///
/// `run` reads `config.json` exactly once, at startup, into a plain `Config`
/// value that lives for the process's lifetime (see `AppState`); nothing
/// re-reads the file afterward. So once the process is up, the file on disk
/// and the settings it is actually enforcing (transport encryption, the
/// backend list, the default backend, the public URL) can only drift apart,
/// never resync, until the next restart.
///
/// There is no live channel to ask the running process what it loaded, so
/// this compares two mtimes as a proxy: the pid file, written immediately
/// after the process is spawned (see `start_background_inner`), stands in
/// for "when the running process started". If `config.json` was modified
/// after that, the running process cannot have seen the change.
pub(crate) fn config_changed_since_start() -> bool {
    let Ok(config_path) = config_dir().map(|dir| dir.join(CONFIG_FILE)) else {
        return false;
    };
    let Ok(pid_file) = pid_path() else {
        return false;
    };
    let (Ok(config_meta), Ok(pid_meta)) = (
        std::fs::metadata(&config_path),
        std::fs::metadata(&pid_file),
    ) else {
        return false;
    };
    let (Ok(config_mtime), Ok(pid_mtime)) = (config_meta.modified(), pid_meta.modified()) else {
        return false;
    };
    config_mtime > pid_mtime
}

pub(crate) fn read_pairing_file() -> anyhow::Result<PairingFile> {
    let bytes = std::fs::read(config_dir()?.join(PAIRING_FILE))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn ensure_pairing_transport_key() -> anyhow::Result<()> {
    let path = config_dir()?.join(PAIRING_FILE);
    let mut pairing: PairingFile = serde_json::from_slice(&std::fs::read(&path)?)?;
    if transport::decode_key(&pairing.payload.transport_key).is_ok_and(|key| key.len() == 32) {
        return Ok(());
    }
    pairing.payload.transport_key = generate_token();
    write_secret_file(&path, &serde_json::to_vec_pretty(&pairing)?)
}

pub(crate) fn rotate_pairing_transport_key() -> anyhow::Result<()> {
    let path = config_dir()?.join(PAIRING_FILE);
    let mut pairing: PairingFile = serde_json::from_slice(&std::fs::read(&path)?)?;
    pairing.payload.transport_key = generate_token();
    write_secret_file(&path, &serde_json::to_vec_pretty(&pairing)?)
}

pub(crate) fn remove_pid_file() -> anyhow::Result<()> {
    let path = pid_path()?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub(crate) fn process_running(pid: u32) -> bool {
    ProcessCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub(crate) fn stop_pid(pid: u32) -> anyhow::Result<()> {
    let status = ProcessCommand::new("kill")
        .arg(pid.to_string())
        .status()
        .context("failed to run kill")?;
    if !status.success() {
        anyhow::bail!("failed to stop pid {pid}");
    }
    Ok(())
}

pub(crate) const GATEWAY_PROCESS_NAME: &str = "muqun-gateway";

/// Find gateway processes listening on `port`. Anything that is not the gateway
/// is filtered out by process name: `stop` must never kill an unrelated service
/// that happens to hold the port.
pub(crate) fn gateway_listener_pids(port: u16) -> anyhow::Result<Vec<u32>> {
    listener_pids_named(port, GATEWAY_PROCESS_NAME)
}

/// Same search, but for an arbitrary process name rather than this build's own
/// [`GATEWAY_PROCESS_NAME`]. Used to detect a gateway still running under the
/// pre-rename binary name, which this build's own name-filtered search would
/// never find.
pub(crate) fn listener_pids_named(port: u16, process_name: &str) -> anyhow::Result<Vec<u32>> {
    #[cfg(target_os = "macos")]
    let mut pids = {
        let output = ProcessCommand::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
            .output()
            .context("failed to inspect listening sockets")?;
        if !output.status.success() && output.status.code() != Some(1) {
            anyhow::bail!("failed to inspect listening sockets");
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|value| value.trim().parse::<u32>().ok())
            .filter(|pid| process_matches_name(*pid, process_name))
            .collect::<Vec<_>>()
    };

    #[cfg(not(target_os = "macos"))]
    let mut pids = {
        let output = ProcessCommand::new("ss")
            .args(["-ltnp"])
            .output()
            .context("failed to inspect listening sockets")?;
        let text = String::from_utf8_lossy(&output.stdout);
        let port_suffix = format!(":{port}");
        let mut pids = Vec::new();
        for line in text.lines() {
            if !line.contains(&port_suffix) || !line.contains(process_name) {
                continue;
            }
            let Some(pid_start) = line.find("pid=") else {
                continue;
            };
            let pid_text = line[pid_start + 4..]
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            if let Ok(pid) = pid_text.parse::<u32>() {
                pids.push(pid);
            }
        }
        pids
    };

    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

/// Confirm a pid really belongs to a process named `process_name` before
/// trusting it (e.g. before signalling it, or before refusing a directory
/// migration on its account).
#[cfg(target_os = "macos")]
pub(crate) fn process_matches_name(pid: u32, process_name: &str) -> bool {
    ProcessCommand::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with(process_name))
        })
}

/// Marks a directory as never-to-be-committed.
///
/// Config directories are routinely symlinked into a dotfiles repository, and a
/// habitual `git add -A` there would publish the admin token and the paired
/// device list. File mode 0600 does not survive a commit; a `.gitignore` does.
/// Written next to the secrets themselves so it travels with them wherever
/// `HERDR_PLUGIN_CONFIG_DIR` points.
pub(crate) fn write_secret_dir_gitignore(dir: &std::path::Path) {
    let path = dir.join(".gitignore");
    if path.exists() {
        return;
    }
    let body = "# The gateway keeps tokens and paired-device records here.\n\
                # Never let them reach a repository.\n\
                *\n";
    if let Err(err) = std::fs::write(&path, body) {
        eprintln!("could not write {}: {err}", path.display());
    }
}

/// Narrow the directory the secrets sit in, not only the secrets.
///
/// The files have been 0600 for a while, so nothing in here was readable by
/// another account. The directory was left at whatever the umask gave it,
/// which on a normal machine is world-listable -- and a listing of it is the
/// names of the paired devices' record file, the pairing file, and whether a
/// gateway is configured on this account at all. Free to fix, and it removes
/// the last thing a second account on a shared machine could learn.
///
/// A failure is reported and not fatal: a directory the owner has deliberately
/// re-permissioned, or one on a filesystem with no modes at all, must not stop
/// the gateway writing its own config.
#[cfg(unix)]
pub(crate) fn lock_down_secret_dir(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(metadata) = std::fs::metadata(dir) else {
        return;
    };
    if metadata.permissions().mode() & 0o077 == 0 {
        return;
    }
    if let Err(err) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
        eprintln!("could not restrict {}: {err}", dir.display());
    }
}

#[cfg(not(unix))]
pub(crate) fn lock_down_secret_dir(_dir: &std::path::Path) {}

/// Replace a secret file's contents in one step, never leaving it short.
///
/// Truncating the real file and writing into it is what loses pairings. It
/// puts the file through a window -- open, chmod, write -- in which it is
/// zero bytes on disk, so a process killed mid-write leaves nothing behind,
/// and a reader that lands in that window sees an empty file rather than the
/// records that are still supposed to be there. Writing a complete temporary
/// file first and renaming it over the target closes that window: `rename` is
/// atomic within a directory, so every reader sees either the whole previous
/// generation or the whole new one, and a crash at any point leaves one of
/// the two rather than a stump.
pub(crate) fn write_secret_file(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        write_secret_dir_gitignore(dir);
        lock_down_secret_dir(dir);
    }
    let directory = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("secret file path has no file name")?;
    // The sequence number matters as much as the pid: two threads of one
    // process writing the same file would otherwise pick the same temporary
    // and rename each other's half-written copy into place.
    static TEMPORARY_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = directory.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        TEMPORARY_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));

    let write_temporary = || -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            // `mode` only applies when the file is created, so a temporary left
            // by an older build keeps its old permissions until they are set
            // explicitly.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            std::io::Write::write_all(&mut file, bytes)?;
            // The rename is only worth anything if the bytes reached the disk
            // before the name started pointing at them.
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&temporary, bytes)?;
        }
        std::fs::rename(&temporary, path)?;
        Ok(())
    };

    match write_temporary() {
        Ok(()) => Ok(()),
        Err(error) => {
            // A half-written temporary must not be left to be mistaken for
            // state, and must not block the next attempt.
            std::fs::remove_file(&temporary).ok();
            Err(error.context(format!("failed to write {}", path.display())))
        }
    }
}

pub(crate) fn default_socket_path() -> String {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("herdr")
        .join("herdr.sock")
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn hostname_label() -> String {
    // HOSTNAME is often unset on macOS, which left every server named the
    // generic "Server". Fall back to the real machine name so a fresh
    // install is at least recognisable before the user renames it.
    if let Ok(value) = std::env::var("HOSTNAME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(output) = std::process::Command::new("hostname").output() {
        if let Ok(name) = String::from_utf8(output.stdout) {
            let name = name.trim().trim_end_matches(".local").trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    "Server".into()
}

/// Persist a new display label into the config (and the pairing payload), so a
/// name set from the app shows up in push notifications. Mirrors
/// `update_public_url`.
pub(crate) fn update_server_label(label: &str) -> anyhow::Result<String> {
    let label = label.trim();
    anyhow::ensure!(
        !label.is_empty()
            && label.chars().count() <= MAX_DEVICE_NAME_CHARS
            && !label.chars().any(char::is_control),
        "label must be non-empty, at most {MAX_DEVICE_NAME_CHARS} characters, and free of control characters"
    );
    let config_path = config_dir()?.join(CONFIG_FILE);
    let mut config = load_config(None)?;
    config.label = label.to_string();
    write_secret_file(&config_path, &serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("failed to write config {}", config_path.display()))?;

    if let Ok(mut pairing) = read_pairing_file() {
        pairing.payload.label = label.to_string();
        let _ = write_secret_file(
            &config_dir()?.join(PAIRING_FILE),
            &serde_json::to_vec_pretty(&pairing)?,
        );
    }
    Ok(label.to_string())
}

/// The label as it stands on disk right now, so a rename from the app takes
/// effect for notifications without restarting the gateway.
pub(crate) fn current_server_label(fallback: &str) -> String {
    load_config(None)
        .map(|config| config.label)
        .unwrap_or_else(|_| fallback.to_string())
}

pub(crate) fn generate_token() -> String {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The glyphs a pairing code is drawn from: no `0`/`O` and no `1`/`I`/`L`, so a
/// person reading one off a screen cannot mistype it into a different code.
pub(crate) const PAIRING_CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// Eight glyphs of this alphabet is a shade under forty bits, and that number
/// is only true if every glyph is equally likely at every position. Two things
/// were making it false.
///
/// Folding a byte with `%` biases the fold: 256 is not a multiple of 31, so the
/// first eight glyphs came up a ninth more often than the other twenty-three.
/// And a v4 UUID is not sixteen random bytes -- four bits of byte 6 say
/// "version 4" and two of byte 8 say "RFC variant" -- so the position that
/// happened to land on byte 6 could only ever produce sixteen of the
/// thirty-one glyphs, costing that character most of its entropy.
///
/// Neither was reachable: eight wrong answers burn the code and six requests
/// burn ten minutes, so nothing gets near enough guesses for a fraction of a
/// bit to matter. It is fixed because the number the code is worth should be
/// the number it says it is.
pub(crate) fn generate_pairing_code() -> String {
    // The last whole multiple of the alphabet below 256. A byte at or above it
    // would fold unevenly, so it is redrawn rather than folded.
    let ceiling = (256 / PAIRING_CODE_ALPHABET.len()) * PAIRING_CODE_ALPHABET.len();
    let mut source = RandomBytes::default();
    let mut characters = String::with_capacity(PAIRING_CODE_CHARACTER_COUNT);
    while characters.len() < PAIRING_CODE_CHARACTER_COUNT {
        let byte = usize::from(source.next_byte());
        if byte >= ceiling {
            continue;
        }
        characters.push(PAIRING_CODE_ALPHABET[byte % PAIRING_CODE_ALPHABET.len()] as char);
    }
    format!("{}-{}", &characters[..4], &characters[4..])
}

/// Random bytes from the CSPRNG behind `Uuid::new_v4`, minus the bytes of a v4
/// UUID that are not random. Keeps the gateway's entropy on one source instead
/// of adding a second dependency for eight characters.
#[derive(Default)]
pub(crate) struct RandomBytes {
    pub(crate) buffer: Vec<u8>,
}

impl RandomBytes {
    pub(crate) fn next_byte(&mut self) -> u8 {
        if self.buffer.is_empty() {
            let bytes = *uuid::Uuid::new_v4().as_bytes();
            // Byte 6 carries the version nibble and byte 8 the variant bits.
            // Neither is random, so neither is spent.
            self.buffer = bytes
                .into_iter()
                .enumerate()
                .filter(|(index, _)| !matches!(index, 6 | 8))
                .map(|(_, byte)| byte)
                .collect();
        }
        // The refill puts fourteen bytes here, so this is never the default.
        self.buffer.pop().unwrap_or_default()
    }
}

/// Device names are echoed into the manage UI's terminal box, so control
/// characters (ANSI escapes in particular) are rejected outright.
/// A client install identifier: an opaque token the app generates once and
/// keeps. Bounded and control-character-free like every other client string.
pub(crate) fn valid_install_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub(crate) fn valid_device_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= MAX_DEVICE_NAME_CHARS
        && !value.chars().any(char::is_control)
}

pub(crate) fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub(crate) fn now_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

pub(crate) fn write_config(path: &std::path::Path, config: &Config) -> anyhow::Result<()> {
    write_secret_file(path, &serde_json::to_vec_pretty(config)?)
        .with_context(|| format!("failed to write config {}", path.display()))
}
