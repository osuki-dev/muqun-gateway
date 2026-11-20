//! Bun owns dependency resolution. Desktop activates a validated installation atomically.
use super::manifest::PluginManifest;
use crate::diagnostics::Outcome;
use anyhow::{ensure, Context, Result};
use muqun_gateway::platform::runtimes::{resolve_binary, RuntimeKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub enum Operation {
    Install(String),
    Update(String),
    Remove(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageStage {
    Preparing,
    Resolving,
    Validating,
    Activating,
    Removing,
}

#[derive(Serialize, Deserialize)]
struct Installation {
    generation: Uuid,
    source: String,
    // Older generations used the plugin ID as their dependency key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dependency: Option<String>,
}

type Installed = BTreeMap<String, Installation>;

fn store() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("Desktop data directory unavailable")?
        .join("muqun-desktop/plugin-packages"))
}

fn installed(root: &Path) -> Result<Installed> {
    match fs::read(root.join("installed.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Installed::new()),
        Err(e) => Err(e.into()),
    }
}

fn valid_name(id: &str) -> bool {
    let valid = |s: &str| {
        !s.is_empty()
            && !matches!(s, "." | ".." | ".bin" | "node_modules")
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
    };
    if let Some((scope, name)) = id.strip_prefix('@').and_then(|s| s.split_once('/')) {
        valid(scope) && valid(name)
    } else {
        !id.starts_with('-') && valid(id)
    }
}

fn location(root: &Path, id: &str, slot: &Installation) -> PathBuf {
    root.join(slot.generation.to_string())
        .join("node_modules")
        .join(slot.dependency.as_deref().unwrap_or(id))
}

pub fn entries() -> Result<Vec<(String, PathBuf)>> {
    let root = store()?;
    Ok(installed(&root)?
        .into_iter()
        .filter(|(id, slot)| valid_name(id) && slot.dependency.as_deref().is_none_or(valid_name))
        .map(|(id, slot)| {
            let path = location(&root, &id, &slot);
            (id, path)
        })
        .collect())
}

pub fn apply_with_progress(
    operation: Operation,
    reserved: &[String],
    mut progress: impl FnMut(PackageStage),
) -> Result<String> {
    progress(PackageStage::Preparing);
    let bun = resolve_binary(RuntimeKind::Bun, None).context(Outcome::RuntimeMissing)?;
    change(&store()?, &bun, operation, reserved, progress)
}

fn run(bun: &Path, dir: &Path, command: &str, arg: &str) -> Result<()> {
    let mut cmd = Command::new(bun);
    cmd.current_dir(dir).args([command, "--ignore-scripts"]);
    if command == "add" {
        cmd.arg("--no-cache");
    }
    let output = cmd.args(["--", arg]).output()?;
    ensure!(
        output.status.success(),
        "bun {command} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn install_source(input: &str) -> Result<String> {
    let input = input.trim();
    ensure!(
        !input.is_empty() && !input.starts_with('-') && !input.chars().any(char::is_control),
        "Enter a package source"
    );
    // Bun runs in an isolated project. Anchor existing local sources before changing cwd.
    let alias = input
        .match_indices('@')
        .find(|(i, _)| *i > 0)
        .and_then(|(i, _)| valid_name(&input[..i]).then(|| input.split_at(i + 1)));
    for (name, spec) in std::iter::once(("", input)).chain(alias) {
        let (prefix, local) = ["file:", "link:"]
            .into_iter()
            .find_map(|prefix| spec.strip_prefix(prefix).map(|local| (prefix, local)))
            .unwrap_or(("", spec));
        if !local.starts_with("//") && Path::new(local).exists() {
            let path = std::path::absolute(local)?;
            return Ok(format!(
                "{name}{prefix}{}",
                path.to_str().context("Invalid source path")?
            ));
        }
    }
    Ok(input.into())
}

const SDK_NAME: &str = "@muqun/desktop";
const PROJECT: &str = r#"{"private":true,"dependencies":{"@muqun/desktop":"file:./sdk"}}"#;

fn write_sdk(stage: &Path) -> Result<()> {
    let sdk = stage.join("sdk");
    fs::create_dir(&sdk)?;
    fs::write(sdk.join("index.ts"), include_bytes!("../../sdk/index.ts"))?;
    fs::write(
        sdk.join("webview-context.ts"),
        include_bytes!("../../sdk/webview-context.ts"),
    )?;
    let mut manifest: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(include_bytes!("../../sdk/package.json"))?;
    manifest.remove("devDependencies");
    manifest.remove("scripts");
    fs::write(sdk.join("package.json"), serde_json::to_vec(&manifest)?)?;
    Ok(())
}

fn change(
    root: &Path,
    bun: &Path,
    operation: Operation,
    reserved: &[String],
    mut progress: impl FnMut(PackageStage),
) -> Result<String> {
    fs::create_dir_all(root)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("lock"))?;
    lock.try_lock()
        .context("Another plugin operation is in progress")?;
    let mut active = installed(root)?;
    let generation = Uuid::new_v4();
    let stage = root.join(generation.to_string());
    fs::create_dir(&stage)?;
    let result = (|| -> Result<String> {
        write_sdk(&stage)?;
        let expected = match &operation {
            Operation::Install(_) => None,
            Operation::Update(id) | Operation::Remove(id) => {
                ensure!(valid_name(id), "Invalid plugin ID");
                Some((
                    id.as_str(),
                    active.get(id).context("Plugin is not installed")?,
                ))
            }
        };
        let remove = matches!(operation, Operation::Remove(_));
        let (id, install) = if let Some((id, slot)) = expected.filter(|_| remove) {
            progress(PackageStage::Removing);
            let previous = root.join(slot.generation.to_string());
            fs::copy(previous.join("package.json"), stage.join("package.json"))?;
            fs::copy(previous.join("bun.lock"), stage.join("bun.lock"))?;
            let dependency = slot.dependency.as_deref().unwrap_or(id);
            ensure!(valid_name(dependency), "Invalid installed dependency name");
            run(bun, &stage, "remove", dependency).context(Outcome::PackageManagerFailed)?;
            (id.to_owned(), None)
        } else {
            let source = match &operation {
                Operation::Install(spec) => install_source(spec)?,
                _ => expected.context("Missing plugin source")?.1.source.clone(),
            };
            fs::write(stage.join("package.json"), PROJECT)?;
            progress(PackageStage::Resolving);
            // Resolve the original input afresh so pins stay pinned and moving refs can advance.
            run(bun, &stage, "add", &source).context(Outcome::PackageManagerFailed)?;
            progress(PackageStage::Validating);
            let project: serde_json::Value =
                serde_json::from_slice(&fs::read(stage.join("package.json"))?)?;
            let deps = project["dependencies"]
                .as_object()
                .context("Bun did not install a plugin")?;
            ensure!(
                deps.len() == 2 && deps.contains_key(SDK_NAME),
                "Expected one plugin package"
            );
            let dependency = deps
                .keys()
                .find(|name| *name != SDK_NAME)
                .context("Missing plugin dependency")?;
            ensure!(valid_name(dependency), "Invalid installed dependency name");
            let package = stage.join("node_modules").join(dependency);
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(package.join("package.json"))?)?;
            if let Some(range) = value["peerDependencies"][SDK_NAME].as_str() {
                let sdk: serde_json::Value =
                    serde_json::from_slice(include_bytes!("../../sdk/package.json"))?;
                let status = Command::new(bun).current_dir(stage.join("sdk"))
                    .args(["--eval", "process.exit(Bun.semver.satisfies(process.argv[1],process.argv[2]) ? 0 : 1)", "--", sdk["version"].as_str().context("Missing SDK version")?, range]).status()?;
                ensure!(status.success(), Outcome::SdkIncompatible);
            }
            let manifest = PluginManifest::from_package(value)?
                .context("This package is not a Muqun plugin")?;
            let id = manifest.id;
            ensure!(
                valid_name(&id) && expected.is_none_or(|(previous, _)| previous == id),
                "Unexpected plugin identity: {id}"
            );
            ensure!(
                id != SDK_NAME && !reserved.contains(&id),
                "This ID is already provided by Desktop or a linked plugin"
            );
            let entry = package
                .join(manifest.entry.context("Missing entry point")?)
                .canonicalize()?;
            ensure!(
                entry.starts_with(package.canonicalize()?) && entry.is_file(),
                "Invalid plugin entry point"
            );
            let dependency = (id != *dependency).then(|| dependency.clone());
            (
                id,
                Some(Installation {
                    generation,
                    source,
                    dependency,
                }),
            )
        };

        let previous = active.remove(&id);
        if let Some(slot) = install {
            progress(PackageStage::Activating);
            active.insert(id.clone(), slot);
        }
        let next = stage.join("installed.next");
        fs::write(&next, serde_json::to_vec(&active)?)?;
        fs::rename(next, root.join("installed.json"))?;
        if let Some(old) = previous {
            let _ = fs::remove_dir_all(root.join(old.generation.to_string()));
        }
        Ok(id)
    })();
    if result.is_err() || matches!(operation, Operation::Remove(_)) {
        let _ = fs::remove_dir_all(stage);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_input_cannot_supply_bun_options_or_control_characters() {
        for source in ["", " --global", "--ignore-scripts=false", "source\nextra"] {
            assert!(install_source(source).is_err());
        }
    }

    #[test]
    fn local_alias_resolves_before_entering_the_installation_stage() {
        let alias = "my-local-plugin";
        let source = install_source(&format!("{alias}@file:.")).unwrap();
        let location = source.strip_prefix("my-local-plugin@file:").unwrap();
        assert!(Path::new(location).is_absolute());
        assert_eq!(
            fs::canonicalize(location).unwrap(),
            fs::canonicalize(".").unwrap()
        );
    }
}
