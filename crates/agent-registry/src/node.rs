//! The Node.js of this computer, which npm agents run with. Coloride neither downloads nor
//! bundles one.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::process::run_bounded;

/// Lowest major version agents are expected to run with.
pub const MIN_NODE_MAJOR: u32 = 22;

/// Whether npm agents can be installed on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeStatus {
    Ready,
    /// Why not, in a sentence the user can act on.
    Missing(String),
}

/// A usable Node.js and the way to run npm with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeToolchain {
    pub node: PathBuf,
    /// `npm-cli.js`, run by `node` itself: the `npm` launcher is a script that looks `node` up in
    /// `PATH`, which a desktop app started from the Dock or Start menu may not have.
    pub npm_cli: Option<PathBuf>,
    /// A standalone `npm` program, when `npm-cli.js` is not found next to Node.
    pub npm_program: Option<PathBuf>,
}

impl NodeToolchain {
    /// `npm` ready to take its arguments, and the `PATH` entry its children need.
    pub fn npm_command(&self) -> Result<Command, String> {
        let node = plain_path(self.node.clone());
        let mut command = if let Some(cli) = &self.npm_cli {
            let mut command = Command::new(&node);
            command.arg(plain_path(cli.clone()));
            command
        } else if let Some(program) = &self.npm_program {
            Command::new(plain_path(program.clone()))
        } else {
            return Err("npm was not found next to this Node.js".into());
        };
        command.env_clear();
        for (key, value) in clean_environment(node.parent()) {
            command.env(key, value);
        }
        Ok(command)
    }
}

/// `\\?\C:\dir` to `C:\dir`: Node.js on Windows fails in `fs.realpathSync` (`EISDIR, lstat 'C:'`)
/// when its entry script argument uses the verbatim `\\?\` prefix returned by `canonicalize`.
pub(crate) fn plain_path(path: PathBuf) -> PathBuf {
    if let Some(text) = path.to_str() {
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            let bytes = rest.as_bytes();
            if bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && bytes[2] == b'\\'
            {
                return PathBuf::from(rest);
            }
        }
    }
    path
}

/// The environment of this process without what would redirect Node, with Node's folder first
/// in `PATH`.
fn clean_environment(node_dir: Option<&Path>) -> Vec<(OsString, OsString)> {
    let mut path: Vec<PathBuf> = node_dir.map(Path::to_path_buf).into_iter().collect();
    path.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let joined = env::join_paths(path).unwrap_or_default();
    env::vars_os()
        .filter(|(key, _)| {
            let key = key.to_string_lossy().to_ascii_uppercase();
            !matches!(
                key.as_str(),
                "NODE_OPTIONS" | "NODE_PATH" | "ELECTRON_RUN_AS_NODE" | "PATH"
            ) && !key.starts_with("NPM_CONFIG_")
        })
        .chain([(OsString::from("PATH"), joined)])
        .collect()
}

pub trait NodeLocator: Send + Sync {
    /// A usable Node.js, or why there is none.
    fn locate(&self) -> Result<NodeToolchain, String>;

    fn status(&self) -> NodeStatus {
        match self.locate() {
            Ok(_) => NodeStatus::Ready,
            Err(reason) => NodeStatus::Missing(reason),
        }
    }
}

/// Looks for Node.js where a developer is likely to have put it.
pub struct SystemNode;

fn executable(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

/// `PATH`, then the places a GUI app does not see in its own `PATH`.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        env::split_paths(&env::var_os("PATH").unwrap_or_default()).collect();
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    if cfg!(windows) {
        for variable in ["ProgramFiles", "LOCALAPPDATA"] {
            if let Some(base) = env::var_os(variable) {
                dirs.push(PathBuf::from(&base).join("nodejs"));
                dirs.push(PathBuf::from(base).join("Programs").join("nodejs"));
            }
        }
    } else {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    }
    if let Some(home) = home {
        dirs.push(home.join(".volta").join("bin"));
        dirs.extend(newest_nvm_bin(&home));
    }
    dirs
}

/// The `bin` of the highest Node installed with nvm.
fn newest_nvm_bin(home: &Path) -> Option<PathBuf> {
    let versions = home.join(".nvm").join("versions").join("node");
    let mut found: Vec<(Vec<u32>, PathBuf)> = std::fs::read_dir(versions)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let numbers = name
                .strip_prefix('v')?
                .split('.')
                .map(|part| part.parse().ok())
                .collect::<Option<Vec<u32>>>()?;
            Some((numbers, entry.path().join("bin")))
        })
        .collect();
    found.sort();
    found.pop().map(|(_, bin)| bin)
}

/// `v24.11.1` -> 24.
pub(crate) fn major_of(version: &str) -> Option<u32> {
    version
        .trim()
        .strip_prefix('v')?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn probe(node: &Path) -> Result<u32, String> {
    let mut command = Command::new(node);
    command.arg("--version");
    let output = run_bounded(command, Duration::from_secs(10), 256)?;
    if !output.status.success() {
        return Err("it did not report a version".into());
    }
    major_of(&output.stdout).ok_or_else(|| format!("unexpected version {:?}", output.stdout.trim()))
}

/// Where npm lives for the Node at `node`.
fn npm_for(node: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let real = std::fs::canonicalize(node)
        .map(plain_path)
        .unwrap_or_else(|_| plain_path(node.to_path_buf()));
    let real_dir = real.parent().map(Path::to_path_buf).unwrap_or_default();
    let cli = [
        // Unix: <prefix>/bin/node and <prefix>/lib/node_modules/npm
        real_dir
            .join("..")
            .join("lib")
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js"),
        // Windows: node.exe and node_modules\npm side by side
        real_dir
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
    .and_then(|candidate| std::fs::canonicalize(candidate).ok())
    .map(plain_path);
    let dir = plain_path(node.to_path_buf())
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let programs = if cfg!(windows) {
        [dir.join("npm.cmd"), dir.join("npm.exe")]
    } else {
        [dir.join("npm"), dir.join("npm.cmd")]
    };
    let program = programs.into_iter().find(|candidate| candidate.is_file());
    (cli, program)
}

impl NodeLocator for SystemNode {
    fn locate(&self) -> Result<NodeToolchain, String> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        // An explicit choice wins, and is never replaced by another Node when it is wrong.
        if let Some(forced) = env::var_os("COLORIDE_NODE").filter(|value| !value.is_empty()) {
            candidates.push(PathBuf::from(forced));
        } else {
            candidates.extend(
                search_dirs()
                    .into_iter()
                    .map(|dir| dir.join(executable("node"))),
            );
        }
        let mut too_old: Option<(PathBuf, u32)> = None;
        for node in candidates.into_iter().filter(|node| node.is_file()) {
            match probe(&node) {
                Ok(major) if major >= MIN_NODE_MAJOR => {
                    let node = plain_path(node);
                    let (npm_cli, npm_program) = npm_for(&node);
                    if npm_cli.is_none() && npm_program.is_none() {
                        continue;
                    }
                    return Ok(NodeToolchain {
                        node,
                        npm_cli,
                        npm_program,
                    });
                }
                Ok(major) => too_old = too_old.or(Some((node, major))),
                Err(_) => {}
            }
        }
        Err(match too_old {
            Some((node, major)) => format!(
                "Node.js {MIN_NODE_MAJOR} or newer is required; {} is version {major}. Install a newer one from nodejs.org.",
                node.display()
            ),
            None => format!(
                "Node.js {MIN_NODE_MAJOR} or newer (with npm) was not found. Install it from nodejs.org."
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_versions_are_read_from_node_output() {
        assert_eq!(major_of("v24.11.1\n"), Some(24));
        assert_eq!(major_of("v22.0.0"), Some(22));
        assert_eq!(major_of("24.1.0"), None);
        assert_eq!(major_of("vx.1.0"), None);
        assert_eq!(major_of(""), None);
    }

    #[test]
    fn the_environment_of_npm_loses_what_would_redirect_node_and_keeps_node_first_in_path() {
        let environment = clean_environment(Some(Path::new("/node/bin")));
        let keys: Vec<String> = environment
            .iter()
            .map(|(key, _)| key.to_string_lossy().to_ascii_uppercase())
            .collect();
        assert!(!keys.iter().any(|key| key == "NODE_OPTIONS"
            || key == "NODE_PATH"
            || key.starts_with("NPM_CONFIG_")));
        let path = environment
            .iter()
            .find(|(key, _)| key == "PATH")
            .unwrap()
            .1
            .clone();
        assert_eq!(
            env::split_paths(&path).next(),
            Some(PathBuf::from("/node/bin"))
        );
        assert_eq!(keys.iter().filter(|key| *key == "PATH").count(), 1);
    }

    #[test]
    fn nothing_is_run_without_a_toolchain_that_has_npm() {
        let toolchain = NodeToolchain {
            node: "/x/node".into(),
            npm_cli: None,
            npm_program: None,
        };
        assert!(toolchain.npm_command().is_err());
    }

    #[test]
    fn windows_verbatim_drive_prefixes_are_stripped_for_node() {
        assert_eq!(
            plain_path(PathBuf::from(
                r"\\?\C:\Program Files\nodejs\node_modules\npm\bin\npm-cli.js"
            )),
            PathBuf::from(r"C:\Program Files\nodejs\node_modules\npm\bin\npm-cli.js")
        );
        assert_eq!(
            plain_path(PathBuf::from(r"C:\Program Files\nodejs\node.exe")),
            PathBuf::from(r"C:\Program Files\nodejs\node.exe")
        );
        assert_eq!(
            plain_path(PathBuf::from(r"\\server\share\node.exe")),
            PathBuf::from(r"\\server\share\node.exe")
        );
        let toolchain = NodeToolchain {
            node: PathBuf::from(r"\\?\C:\Program Files\nodejs\node.exe"),
            npm_cli: Some(PathBuf::from(
                r"\\?\C:\Program Files\nodejs\node_modules\npm\bin\npm-cli.js",
            )),
            npm_program: None,
        };
        let command = toolchain.npm_command().unwrap();
        assert_eq!(command.get_program(), r"C:\Program Files\nodejs\node.exe");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [std::ffi::OsStr::new(
                r"C:\Program Files\nodejs\node_modules\npm\bin\npm-cli.js"
            )]
        );
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("node.exe");
        let cli = dir
            .path()
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(&node, "").unwrap();
        std::fs::write(&cli, "").unwrap();
        let (found_cli, _) = npm_for(&node);
        let found_cli = found_cli.unwrap();
        assert!(
            !found_cli.to_string_lossy().starts_with(r"\\?\"),
            "npm_cli must not carry a verbatim prefix: {}",
            found_cli.display()
        );
    }
}
