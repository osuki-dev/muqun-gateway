//! Secrets the owner typed (the voice service's API key): in the system
//! keyring when there is one, else a file only the owner can read.
//!
//! Linux speaks the freedesktop Secret Service (GNOME Keyring, KWallet's
//! compatibility service, KeePassXC) over the session bus, into the default
//! collection, and never raises an unlock prompt: a locked or missing keyring
//! falls back to `<config>/secrets/<name>` with mode 0600. Other platforms use
//! that file for now. Every call blocks briefly; call it off the UI thread.

use std::path::PathBuf;

/// Where a secret ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    Keyring,
    File,
}

fn file(name: &str) -> Option<PathBuf> {
    let (config, _) = super::directories().ok()?;
    Some(config.join("secrets").join(name))
}

/// Keep `secret` under `name`, replacing what was there. An empty secret
/// removes it.
pub fn save(name: &str, secret: &str) -> Result<Store, String> {
    if secret.is_empty() {
        delete(name);
        return Ok(Store::File);
    }
    if keyring::save(name, secret) {
        // One place only: a stale file copy must not outlive the keyring's.
        if let Some(path) = file(name) {
            let _ = std::fs::remove_file(path);
        }
        return Ok(Store::Keyring);
    }
    let path = file(name).ok_or("no configuration directory")?;
    write_private(&path, secret).map_err(|err| err.to_string())?;
    Ok(Store::File)
}

/// The secret under `name`, from the keyring, else the file.
pub fn load(name: &str) -> Option<String> {
    keyring::load(name).or_else(|| {
        std::fs::read_to_string(file(name)?)
            .ok()
            .map(|s| s.trim_end_matches('\n').to_string())
            .filter(|s| !s.is_empty())
    })
}

/// Where the secret under `name` is kept, if anywhere.
pub fn locate(name: &str) -> Option<Store> {
    if keyring::load(name).is_some() {
        return Some(Store::Keyring);
    }
    file(name).filter(|p| p.is_file()).map(|_| Store::File)
}

pub fn delete(name: &str) {
    keyring::delete(name);
    if let Some(path) = file(name) {
        let _ = std::fs::remove_file(path);
    }
}

fn write_private(path: &std::path::Path, secret: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut f = options.open(&tmp)?;
    f.write_all(secret.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

#[cfg(target_os = "linux")]
mod keyring {
    use std::collections::HashMap;
    use std::time::Duration;

    use zbus::{Connection, Proxy};
    use zbus::{ObjectPath, OwnedObjectPath, Variant as Value};

    const SERVICE: &str = "org.freedesktop.secrets";
    const ROOT: &str = "/org/freedesktop/secrets";
    const ROOT_IFACE: &str = "org.freedesktop.Secret.Service";
    const APP: &str = "muqun-desktop";

    type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

    fn block<T>(job: impl std::future::Future<Output = zbus::Result<T>>) -> Option<T> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async {
            match tokio::time::timeout(Duration::from_secs(3), job).await {
                Ok(Ok(value)) => Some(value),
                Ok(Err(err)) => {
                    tracing::debug!("secret service: {err}");
                    None
                }
                Err(_) => None,
            }
        })
    }

    fn attributes(name: &str) -> HashMap<&str, &str> {
        HashMap::from([("application", APP), ("name", name)])
    }

    async fn session(conn: &Connection) -> zbus::Result<(Proxy<'static>, OwnedObjectPath)> {
        let service = Proxy::new(conn, SERVICE, ROOT, ROOT_IFACE).await?;
        // "plain": the session bus is the owner's own; nothing to negotiate.
        let (_, session): (zbus::OwnedVariant, OwnedObjectPath) = service
            .call("OpenSession", &("plain", Value::from("")))
            .await?;
        Ok((service, session))
    }

    pub fn save(name: &str, secret: &str) -> bool {
        block(async {
            let conn = Connection::session().await?;
            let (service, session) = session(&conn).await?;
            let collection: OwnedObjectPath = service.call("ReadAlias", &("default",)).await?;
            if collection.as_str() == "/" {
                return Err(zbus::Error::Failure("no default keyring".into()));
            }
            let props = Proxy::new(
                &conn,
                SERVICE,
                collection.as_str().to_owned(),
                "org.freedesktop.Secret.Collection",
            )
            .await?;
            if props.get_property::<bool>("Locked").await.unwrap_or(true) {
                return Err(zbus::Error::Failure("the keyring is locked".into()));
            }
            let mut properties: HashMap<&str, Value<'_>> = HashMap::new();
            properties.insert(
                "org.freedesktop.Secret.Item.Label",
                Value::from(format!("Muqun Desktop: {name}")),
            );
            properties.insert(
                "org.freedesktop.Secret.Item.Attributes",
                Value::from(attributes(name)),
            );
            let value: Secret = (
                session,
                Vec::new(),
                secret.as_bytes().to_vec(),
                "text/plain".into(),
            );
            let (_item, prompt): (OwnedObjectPath, OwnedObjectPath) =
                props.call("CreateItem", &(properties, value, true)).await?;
            if prompt.as_str() != "/" {
                return Err(zbus::Error::Failure("the keyring wants a prompt".into()));
            }
            Ok(())
        })
        .is_some()
    }

    async fn items(service: &Proxy<'_>, name: &str) -> zbus::Result<Vec<OwnedObjectPath>> {
        let (unlocked, _locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
            service.call("SearchItems", &(attributes(name),)).await?;
        Ok(unlocked)
    }

    pub fn load(name: &str) -> Option<String> {
        block(async {
            let conn = Connection::session().await?;
            let (service, session) = session(&conn).await?;
            let found = items(&service, name).await?;
            let Some(first) = found.first() else {
                return Ok(None);
            };
            let paths: Vec<ObjectPath<'_>> = vec![first.as_ref()];
            let secrets: HashMap<OwnedObjectPath, Secret> = service
                .call("GetSecrets", &(paths, session.as_ref()))
                .await?;
            Ok(secrets
                .into_values()
                .next()
                .and_then(|(_, _, value, _)| String::from_utf8(value).ok()))
        })
        .flatten()
    }

    pub fn delete(name: &str) {
        block(async {
            let conn = Connection::session().await?;
            let (service, _) = session(&conn).await?;
            for item in items(&service, name).await? {
                let item = Proxy::new(
                    &conn,
                    SERVICE,
                    item.as_str().to_owned(),
                    "org.freedesktop.Secret.Item",
                )
                .await?;
                let _: OwnedObjectPath = item.call("Delete", &()).await?;
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
mod keyring {
    pub fn save(_: &str, _: &str) -> bool {
        false
    }
    pub fn load(_: &str) -> Option<String> {
        None
    }
    pub fn delete(_: &str) {}
}
