//! Linux plugin pickers distinguish cancelling a dialog from a picker that failed to open.
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use zbus::{Connection, MatchRule, MessageStream, OwnedObjectPath, OwnedVariant, Proxy, Variant};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const ROOT: &str = "/org/freedesktop/portal/desktop";

/// Called on the desktop's dialog worker thread.
pub(super) fn pick(directory: bool, title: &str, start: Option<&Path>) -> Result<Option<PathBuf>> {
    let start = start.filter(|path| path.is_absolute() && path.is_dir());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let opened = async {
            let connection = Connection::session().await?;
            let picker = Proxy::new(
                &connection,
                PORTAL,
                ROOT,
                "org.freedesktop.portal.FileChooser",
            )
            .await?;
            // Subscribe first: a fast response can arrive before OpenFile returns.
            let rule = MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(PORTAL)
                .interface("org.freedesktop.portal.Request")
                .member("Response")
                .build()?;
            let responses = MessageStream::for_match_rule(rule, &connection, None).await?;
            let owner = picker.receive_owner_changed().await?;
            let token = format!("muqun_{}", uuid::Uuid::new_v4().simple());
            let mut options = HashMap::from([
                ("handle_token", Variant::from(token)),
                ("directory", Variant::from(directory)),
                ("multiple", Variant::from(false)),
            ]);
            if let Some(dir) = start {
                let mut bytes = dir.as_os_str().as_bytes().to_vec();
                bytes.push(0);
                options.insert("current_folder", Variant::new(bytes));
            }
            let handle: OwnedObjectPath = picker.call("OpenFile", &("", title, options)).await?;
            Ok::<_, zbus::Error>((handle, responses, owner))
        }
        .await;
        let (handle, mut responses, mut owner) = match opened {
            Ok(request) => request,
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.portal.Error.Cancelled" =>
            {
                return Ok(None)
            }
            Err(error) if unavailable(&error) => {
                return fallback(directory, title, start)
                    .with_context(|| format!("The file chooser portal could not open ({error})"));
            }
            Err(error) => {
                return Err(error).context("The file chooser portal rejected the request")
            }
        };
        // After a request is accepted, a cancellation or failure must not create a second dialog.
        loop {
            tokio::select! {
                message = responses.next() => {
                    let message = message.context("File chooser portal disconnected")??;
                    if message.header().path().map(|path| path.as_str()) != Some(handle.as_str()) {
                        continue;
                    }
                    let (status, values) = message.body().deserialize()?;
                    return decode_response(status, values);
                }
                changed = owner.next() => {
                    if !matches!(changed, Some(Some(_))) {
                        bail!("File chooser portal disconnected");
                    }
                }
            }
        }
    })
}

fn unavailable(error: &zbus::Error) -> bool {
    use zbus::fdo::Error as Fdo;
    match error {
        zbus::Error::FDO(error) => matches!(
            error.as_ref(),
            Fdo::ServiceUnknown(_)
                | Fdo::NameHasNoOwner(_)
                | Fdo::NotSupported(_)
                | Fdo::UnknownInterface(_)
                | Fdo::UnknownObject(_)
                | Fdo::UnknownMethod(_)
        ),
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.portal.Error.NotFound" | "org.freedesktop.portal.Error.NotSupported"
        ),
        zbus::Error::Address(_) => true,
        zbus::Error::InputOutput(error) => matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::NotFound
                | std::io::ErrorKind::NotConnected
        ),
        _ => false,
    }
}

fn decode_response(
    status: u32,
    mut values: HashMap<String, OwnedVariant>,
) -> Result<Option<PathBuf>> {
    match status {
        0 => {
            let uris = Vec::<String>::try_from(
                values
                    .remove("uris")
                    .context("File chooser did not return a selection")?,
            )?;
            let [uri] = uris.as_slice() else {
                bail!("Expected one local file or directory");
            };
            let path = reqwest::Url::parse(uri)?
                .to_file_path()
                .map_err(|_| anyhow!("The file chooser returned a non-local URI"))?;
            Ok(Some(path))
        }
        1 => Ok(None),
        other => bail!("File chooser ended without a selection (portal response {other})"),
    }
}

fn fallback(directory: bool, title: &str, start: Option<&Path>) -> Result<Option<PathBuf>> {
    for program in ["zenity", "kdialog"] {
        let mut cmd = Command::new(program);
        cmd.arg("--title").arg(title);
        if program == "zenity" {
            cmd.env("GTK_USE_PORTAL", "0").arg("--file-selection");
            if directory {
                cmd.arg("--directory");
            }
            if let Some(start) = start {
                cmd.arg("--filename").arg(start.join(""));
            }
        } else {
            cmd.arg(if directory {
                "--getexistingdirectory"
            } else {
                "--getopenfilename"
            });
            if let Some(start) = start {
                cmd.arg(start);
            }
        }
        let output = match cmd.output() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            other => other.with_context(|| format!("Could not start {program}"))?,
        };
        match output.status.code() {
            Some(0) => {
                let bytes = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
                let path = PathBuf::from(OsString::from_vec(bytes.to_vec()));
                if !path.is_absolute() {
                    bail!("{program} did not return an absolute path");
                }
                return Ok(Some(path));
            }
            Some(1) => return Ok(None),
            _ => bail!(
                "{program} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }
    bail!("No system file picker is available (tried the desktop portal, zenity and kdialog)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(uri: &str) -> HashMap<String, OwnedVariant> {
        HashMap::from([(
            "uris".into(),
            Variant::new(vec![uri.to_owned()]).try_into_owned().unwrap(),
        )])
    }

    #[test]
    fn cancel_is_distinct_from_failure_or_malformed_success() {
        assert_eq!(decode_response(1, HashMap::new()).unwrap(), None);
        assert!(decode_response(2, HashMap::new()).is_err());
        assert!(decode_response(0, HashMap::new()).is_err());
    }

    #[test]
    fn local_selection_preserves_escaped_names_and_refuses_network_uris() {
        assert_eq!(
            decode_response(0, selection("file:///tmp/my%20folder/")).unwrap(),
            Some(PathBuf::from("/tmp/my folder/"))
        );
        assert!(decode_response(0, selection("https://example.org/plugin")).is_err());
    }
}
