//! Installing one agent into a staging folder. The caller moves the result into place only if
//! this returns `Ok`.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::archive::{self, Limits};
use crate::catalog::{parse_exact_package, CatalogEntry, Source};
use crate::error::RegistryError;
use crate::http::{Fetch, Redirects};
use crate::model::{Installation, Launch};
use crate::node::NodeLocator;
use crate::process::run_bounded;
use crate::store::{now_ms, Store};

/// Largest archive downloaded.
pub(crate) const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
const NPM_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const NPM_OUTPUT_BYTES: usize = 64 * 1024;
/// npm packages come from here, whatever the user's npm configuration says.
const NPM_REGISTRY: &str = "https://registry.npmjs.org/";
const NPM_MANIFEST: &str = r#"{"name":"coloride-agent","private":true,"version":"0.0.0"}"#;

fn not_installable(entry: &CatalogEntry, reason: impl Into<String>) -> RegistryError {
    RegistryError::NotInstallable {
        name: entry.name.clone(),
        reason: reason.into(),
    }
}

/// Folder-relative path with `/` separators, as saved in an installation.
fn posix(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn install(
    store: &Store,
    fetch: &dyn Fetch,
    node: &dyn NodeLocator,
    entry: &CatalogEntry,
) -> Result<Installation, RegistryError> {
    let staging = store.new_staging(&entry.id)?;
    let launch = match &entry.source {
        Source::Binary {
            archive,
            sha256,
            command,
            args,
        } => binary(
            entry,
            fetch,
            archive,
            sha256.as_deref(),
            command,
            args,
            staging.path(),
        )?,
        Source::Npm { package, args } => npm(entry, node, package, args, staging.path())?,
        Source::Uv { .. } => {
            return Err(not_installable(
                entry,
                "Agents distributed through uv are not supported yet.",
            ))
        }
    };
    let location = store.commit(staging, &entry.id, &entry.version)?;
    Ok(Installation {
        id: format!("{}@{}", entry.id, entry.version),
        agent_id: entry.id.clone(),
        name: entry.name.clone(),
        version: entry.version.clone(),
        distribution: entry.source.label().into(),
        installed_at_ms: now_ms(),
        location,
        launch,
    })
}

fn binary(
    entry: &CatalogEntry,
    fetch: &dyn Fetch,
    url: &str,
    sha256: Option<&str>,
    command: &str,
    args: &[String],
    dir: &Path,
) -> Result<Launch, RegistryError> {
    // The catalog check is repeated here: nothing is downloaded unless it can be verified.
    let digest = sha256
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| {
            not_installable(entry, "the registry publishes no SHA-256 for this download")
        })?;
    let format = archive::format_of(url)
        .ok_or_else(|| not_installable(entry, "the download format is not supported"))?;
    let program = archive::safe_relative(command)
        .ok_or_else(|| not_installable(entry, "the registry gives an unsafe command path"))?;

    let download = dir.join("download");
    download_verified(fetch, url, digest, &download)?;
    let extracted = dir.join("extract");
    fs::create_dir(&extracted).map_err(RegistryError::io("Creating a folder for the agent"))?;
    archive::extract(&download, format, &extracted, Limits::default())?;
    fs::remove_file(&download).map_err(RegistryError::io("Removing the download"))?;

    let target = extracted.join(&program);
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_file() => {}
        _ => {
            return Err(RegistryError::Install(
                "the archive does not contain the program the registry names".into(),
            ))
        }
    }
    make_executable(&target)?;
    Ok(Launch::Executable {
        path: format!("extract/{}", posix(&program)),
        args: args.to_vec(),
    })
}

/// Streams `url` into `destination`, refusing anything that is not exactly the digest.
fn download_verified(
    fetch: &dyn Fetch,
    url: &str,
    sha256: &str,
    destination: &Path,
) -> Result<(), RegistryError> {
    let reader = fetch.open(url, Redirects::Follow)?;
    let mut reader = reader.take(MAX_DOWNLOAD_BYTES + 1);
    let mut file = File::create(destination).map_err(RegistryError::io("Creating the download"))?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| RegistryError::Download(error.to_string()))?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > MAX_DOWNLOAD_BYTES {
            return Err(RegistryError::Download(
                "the file is larger than 512 MiB".into(),
            ));
        }
        hasher.update(&buffer[..count]);
        file.write_all(&buffer[..count])
            .map_err(RegistryError::io("Writing the download"))?;
    }
    file.sync_all()
        .map_err(RegistryError::io("Writing the download"))?;
    if format!("{:x}", hasher.finalize()) != sha256.to_ascii_lowercase() {
        return Err(RegistryError::ChecksumMismatch);
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), RegistryError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(RegistryError::io("Making the agent executable"))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn make_executable(_path: &Path) -> Result<(), RegistryError> {
    Ok(())
}

fn npm(
    entry: &CatalogEntry,
    node: &dyn NodeLocator,
    package: &str,
    args: &[String],
    dir: &Path,
) -> Result<Launch, RegistryError> {
    let (name, version) = parse_exact_package(package).ok_or_else(|| {
        not_installable(entry, "the npm package is not pinned to an exact version")
    })?;
    let toolchain = node
        .locate()
        .map_err(|reason| not_installable(entry, reason))?;
    fs::write(dir.join("package.json"), NPM_MANIFEST)
        .map_err(RegistryError::io("Preparing the npm install"))?;
    let mut command = toolchain
        .npm_command()
        .map_err(|reason| not_installable(entry, reason))?;
    command
        .args([
            "install",
            // The point of an explicit install: fetch the code, but run none of it. Packages
            // whose scripts are needed fail the check below and are reported, not half-installed.
            "--ignore-scripts",
            "--omit=dev",
            "--no-audit",
            "--no-fund",
            "--no-progress",
            "--save-exact",
            "--loglevel=error",
            "--registry",
            NPM_REGISTRY,
            package,
        ])
        .current_dir(dir);
    let output = run_bounded(command, NPM_TIMEOUT, NPM_OUTPUT_BYTES)
        .map_err(|reason| RegistryError::Install(format!("npm: {reason}")))?;
    if !output.status.success() {
        return Err(RegistryError::Install(format!(
            "npm could not install {package}: {}",
            tail(&output.stderr)
        )));
    }

    let package_dir = dir.join("node_modules").join(name);
    let manifest = fs::read(package_dir.join("package.json")).map_err(|_| {
        RegistryError::Install(format!("npm finished but {name} is not in node_modules"))
    })?;
    let manifest: Value = serde_json::from_slice(&manifest).map_err(|error| {
        RegistryError::Install(format!("the package.json of {name} is not valid: {error}"))
    })?;
    if manifest["name"] != name || manifest["version"] != version {
        return Err(RegistryError::Install(format!(
            "npm installed a different package than {package}"
        )));
    }
    let script = entry_script(&manifest, name)?;
    let relative = archive::safe_relative(&script).ok_or_else(|| {
        RegistryError::Install(format!(
            "the executable {script:?} of {name} is not a safe path"
        ))
    })?;
    // Resolve links: the script must really be a file inside this install.
    let root = fs::canonicalize(dir).map_err(RegistryError::io("Reading the install folder"))?;
    let resolved = fs::canonicalize(package_dir.join(&relative))
        .map_err(|_| RegistryError::Install(format!("{name} has no executable at {script}")))?;
    let inside = resolved.strip_prefix(&root).map_err(|_| {
        RegistryError::Install(format!(
            "the executable of {name} points outside its folder"
        ))
    })?;
    if !resolved.is_file() {
        return Err(RegistryError::Install(format!(
            "{name} has no executable at {script}"
        )));
    }
    Ok(Launch::Node {
        script: posix(inside),
        args: args.to_vec(),
    })
}

/// The script an npm package runs: its `bin`, a file or the entry named like the package.
fn entry_script(manifest: &Value, name: &str) -> Result<String, RegistryError> {
    let missing = || RegistryError::Install(format!("{name} does not publish an executable"));
    match &manifest["bin"] {
        Value::String(path) => Ok(path.clone()),
        Value::Object(bins) => {
            let short = name.rsplit('/').next().unwrap_or(name);
            let only = (bins.len() == 1).then(|| bins.values().next()).flatten();
            bins.get(short)
                .or(only)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    RegistryError::Install(format!(
                        "{name} publishes several executables and none is named {short}"
                    ))
                })
        }
        _ => Err(missing()),
    }
}

/// The end of a message, which is where npm puts the reason.
fn tail(text: &str) -> String {
    let text = text.trim();
    let start = text.len().saturating_sub(1500);
    let start = (start..=text.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(text.len());
    text[start..].trim().to_owned()
}
