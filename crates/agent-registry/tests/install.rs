//! The registry end to end, with a fake network and a fake Node: what is installed, what is
//! refused, and what is left on disk either way.

use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_registry::{
    AgentRegistry, CatalogOrigin, Fetch, NodeLocator, NodeToolchain, Parts, Redirects,
    RegistryError,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const CATALOG_URL: &str = "https://registry.test/registry.json";
const TARGET: &str = "test-target";

/// Serves canned bodies and records what was asked for.
#[derive(Default)]
struct Network {
    bodies: Mutex<HashMap<String, Vec<u8>>>,
    requests: Mutex<Vec<(String, Redirects)>>,
}

impl Network {
    fn serve(&self, url: &str, body: impl Into<Vec<u8>>) {
        self.bodies.lock().unwrap().insert(url.into(), body.into());
    }

    fn drop_all(&self) {
        self.bodies.lock().unwrap().clear();
    }

    fn requests(&self) -> Vec<(String, Redirects)> {
        self.requests.lock().unwrap().clone()
    }
}

impl Fetch for Network {
    fn open(&self, url: &str, redirects: Redirects) -> Result<Box<dyn Read + Send>, RegistryError> {
        self.requests.lock().unwrap().push((url.into(), redirects));
        match self.bodies.lock().unwrap().get(url) {
            Some(body) => Ok(Box::new(Cursor::new(body.clone()))),
            None => Err(RegistryError::Download(format!("{url} answered 404"))),
        }
    }
}

/// A Node that is `/bin/sh` and an npm that is a script of ours: it records its arguments and
/// writes the package the way npm would.
struct FakeNode {
    toolchain: Result<NodeToolchain, String>,
}

impl NodeLocator for FakeNode {
    fn locate(&self) -> Result<NodeToolchain, String> {
        self.toolchain.clone()
    }
}

struct World {
    dir: tempfile::TempDir,
    network: Arc<Network>,
    registry: AgentRegistry,
}

fn zip_of(program: &[u8]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            "./agent",
            zip::write::SimpleFileOptions::default().unix_permissions(0o644),
        )
        .unwrap();
    writer.write_all(program).unwrap();
    writer.finish().unwrap().into_inner()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn agent(id: &str, version: &str, distribution: Value) -> Value {
    json!({"id": id, "name": format!("Agent {id}"), "version": version, "description": "d", "distribution": distribution})
}

fn binary(url: &str, sha: Option<String>) -> Value {
    json!({"binary": {TARGET: {"archive": url, "cmd": "./agent", "sha256": sha, "args": ["acp"]}}})
}

fn world_with(node: Result<NodeToolchain, String>, agents: Vec<Value>) -> World {
    let dir = tempfile::tempdir().unwrap();
    let network = Arc::new(Network::default());
    network.serve(
        CATALOG_URL,
        json!({"version": "1.0.0", "agents": agents}).to_string(),
    );
    let registry = AgentRegistry::with_parts(
        dir.path().join("agents"),
        Parts {
            fetch: network.clone(),
            node: Arc::new(FakeNode { toolchain: node }),
            registry_url: CATALOG_URL.into(),
            target: Some(TARGET.into()),
        },
    )
    .unwrap();
    World {
        dir,
        network,
        registry,
    }
}

fn no_node() -> Result<NodeToolchain, String> {
    Err("Node.js 22 or newer (with npm) was not found. Install it from nodejs.org.".into())
}

impl World {
    fn packages(&self) -> PathBuf {
        self.dir.path().join("agents/packages")
    }

    /// Everything under packages/, as relative paths: what an install leaves behind.
    fn left_over(&self) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                out.push(
                    path.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
                if path.is_dir() {
                    walk(&path, base, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.packages(), &self.packages(), &mut out);
        out.sort();
        out
    }
}

fn plain_world() -> (World, Vec<u8>) {
    let archive = zip_of(b"#!/bin/sh\nexit 0\n");
    let url = "https://downloads.test/agent.zip";
    let world = world_with(
        no_node(),
        vec![agent("tool", "1.2.3", binary(url, Some(sha256(&archive))))],
    );
    world.network.serve(url, archive.clone());
    (world, archive)
}

#[test]
fn a_verified_binary_installs_and_can_be_started_and_removed() {
    let (world, _) = plain_world();
    let listing = world.registry.listing(true).unwrap();
    assert_eq!(listing.origin, CatalogOrigin::Network);
    assert_eq!(listing.agents[0].blocked, None);
    assert!(listing.installed.is_empty());

    let installed = world.registry.install("tool", "1.2.3").unwrap();
    assert_eq!(
        (installed.agent_id.as_str(), installed.version.as_str()),
        ("tool", "1.2.3")
    );
    assert_eq!(
        world.registry.listing(false).unwrap().installed,
        vec![installed.clone()]
    );

    let launch = world.registry.launch_command("tool").unwrap();
    assert_eq!(launch.args, vec!["acp"]);
    assert!(
        launch.program.ends_with("extract/agent"),
        "{:?}",
        launch.program
    );
    assert_eq!(
        launch.read_only_paths,
        vec![world.packages().join("tool/1.2.3")]
    );
    assert_eq!(fs::read(&launch.program).unwrap(), b"#!/bin/sh\nexit 0\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&launch.program).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o111,
            0o111,
            "the agent is executable even if the archive said 0644"
        );
    }
    // The download is gone; only the extracted agent stays.
    assert!(!world.packages().join("tool/1.2.3/download").exists());
    assert!(
        fs::read_dir(world.packages().join(".staging"))
            .unwrap()
            .next()
            .is_none(),
        "staging is empty"
    );

    world.registry.uninstall(&installed.id).unwrap();
    assert!(world.registry.listing(false).unwrap().installed.is_empty());
    assert_eq!(
        world.left_over(),
        vec![".staging".to_string()],
        "nothing of the agent is left"
    );
}

#[test]
fn a_download_that_does_not_match_its_checksum_installs_nothing() {
    let archive = zip_of(b"real");
    let url = "https://downloads.test/agent.zip";
    let world = world_with(
        no_node(),
        vec![agent("tool", "1.2.3", binary(url, Some(sha256(&archive))))],
    );
    world.network.serve(url, zip_of(b"tampered"));
    let error = world.registry.install("tool", "1.2.3").unwrap_err();
    assert!(matches!(error, RegistryError::ChecksumMismatch), "{error}");
    assert!(world.registry.listing(false).unwrap().installed.is_empty());
    assert!(!world.packages().join("tool").exists());
}

#[test]
fn a_download_without_a_published_checksum_is_never_fetched() {
    let url = "https://downloads.test/agent.zip";
    let world = world_with(no_node(), vec![agent("tool", "1.2.3", binary(url, None))]);
    world.network.serve(url, zip_of(b"anything"));
    let card = &world.registry.listing(true).unwrap().agents[0];
    assert!(card.blocked.as_deref().unwrap().contains("SHA-256"));
    let error = world.registry.install("tool", "1.2.3").unwrap_err();
    assert!(
        matches!(error, RegistryError::NotInstallable { .. }),
        "{error}"
    );
    assert!(
        world
            .network
            .requests()
            .iter()
            .all(|(url, _)| url == CATALOG_URL),
        "no archive was requested"
    );
}

#[test]
fn the_user_confirms_the_exact_version_that_gets_installed() {
    let (world, _) = plain_world();
    let error = world.registry.install("tool", "1.2.2").unwrap_err();
    assert!(
        matches!(error, RegistryError::VersionChanged { .. }),
        "{error}"
    );
    assert!(world.registry.install("missing", "1.2.3").is_err());
    let plan = world.registry.plan("tool", "1.2.3").unwrap();
    assert!(plan.summary.contains("agent.zip") && plan.summary.contains("SHA-256"));
    assert!(
        world.left_over().is_empty(),
        "refusing or planning leaves nothing on disk"
    );
}

#[test]
fn hidden_agents_cannot_be_installed_even_by_name() {
    let archive = zip_of(b"x");
    let url = "https://downloads.test/agent.zip";
    let world = world_with(
        no_node(),
        vec![agent(
            "gemini",
            "1.0.0",
            binary(url, Some(sha256(&archive))),
        )],
    );
    world.network.serve(url, archive);
    assert!(world.registry.listing(true).unwrap().agents.is_empty());
    assert!(matches!(
        world.registry.install("gemini", "1.0.0"),
        Err(RegistryError::UnknownAgent(_))
    ));
}

#[test]
fn the_catalog_is_read_from_one_address_without_redirects() {
    let (world, _) = plain_world();
    world.registry.listing(true).unwrap();
    assert_eq!(
        world.network.requests(),
        vec![(CATALOG_URL.to_string(), Redirects::Refuse)]
    );
    world.registry.install("tool", "1.2.3").unwrap();
    assert_eq!(
        world.network.requests().last().unwrap().1,
        Redirects::Follow,
        "archives may be redirected"
    );
}

#[test]
fn offline_the_saved_catalog_is_shown_and_says_so() {
    let (world, _) = plain_world();
    assert!(world.registry.listing(false).is_err(), "nothing saved yet");
    world.registry.listing(true).unwrap();
    world.network.drop_all();
    let listing = world.registry.listing(true).unwrap();
    assert_eq!(listing.origin, CatalogOrigin::Saved);
    assert!(listing.refresh_error.unwrap().contains("404"));
    assert_eq!(listing.agents.len(), 1);
}

#[test]
fn a_catalog_that_is_not_one_is_not_saved() {
    let (world, _) = plain_world();
    world
        .network
        .serve(CATALOG_URL, "<html>captive portal</html>");
    assert!(world.registry.listing(true).is_err());
    world
        .network
        .serve(CATALOG_URL, json!({"agents": "nope"}).to_string());
    assert!(world.registry.listing(true).is_err());
    assert!(world.registry.listing(false).is_err(), "neither was kept");
}

#[test]
fn a_new_version_replaces_the_old_one() {
    let first = zip_of(b"v1");
    let second = zip_of(b"v2");
    let (url1, url2) = (
        "https://downloads.test/1.zip",
        "https://downloads.test/2.zip",
    );
    let world = world_with(
        no_node(),
        vec![agent("tool", "1.0.0", binary(url1, Some(sha256(&first))))],
    );
    world.network.serve(url1, first);
    world.registry.install("tool", "1.0.0").unwrap();
    world.network.serve(
        CATALOG_URL,
        json!({"agents": [agent("tool", "2.0.0", binary(url2, Some(sha256(&second))))]})
            .to_string(),
    );
    world.network.serve(url2, second);
    world.registry.install("tool", "2.0.0").unwrap();
    let listing = world.registry.listing(false).unwrap();
    assert_eq!(listing.installed.len(), 1);
    assert_eq!(listing.installed[0].version, "2.0.0");
    assert!(!world.packages().join("tool/1.0.0").exists());
    assert_eq!(
        fs::read(world.registry.launch_command("tool").unwrap().program).unwrap(),
        b"v2"
    );
}

#[test]
fn an_archive_without_the_named_program_installs_nothing() {
    let archive = zip_of(b"x");
    let url = "https://downloads.test/agent.zip";
    let distribution =
        json!({"binary": {TARGET: {"archive": url, "cmd": "./other", "sha256": sha256(&archive)}}});
    let world = world_with(no_node(), vec![agent("tool", "1.0.0", distribution)]);
    world.network.serve(url, archive);
    let error = world.registry.install("tool", "1.0.0").unwrap_err();
    assert!(error.to_string().contains("does not contain"), "{error}");
    assert_eq!(world.left_over(), vec![".staging".to_string()]);
}

#[test]
fn removing_an_installation_whose_record_points_elsewhere_deletes_nothing_there() {
    let (world, _) = plain_world();
    let installed = world.registry.install("tool", "1.2.3").unwrap();
    let victim = world.dir.path().join("precious");
    fs::create_dir(&victim).unwrap();
    fs::write(victim.join("file"), "keep").unwrap();
    let path = world.dir.path().join("agents/installations.json");
    let mut records: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    records[0]["location"] = json!(victim);
    fs::write(&path, serde_json::to_vec(&records).unwrap()).unwrap();
    assert!(world.registry.uninstall(&installed.id).is_err());
    assert_eq!(fs::read(victim.join("file")).unwrap(), b"keep");
    assert!(matches!(
        world.registry.uninstall("nobody@1"),
        Err(RegistryError::UnknownInstallation(_))
    ));
}

// ---- npm agents (they need a shell to stand in for Node) -----------------------------------

#[cfg(unix)]
mod npm {
    use agent_registry::Launch;

    use super::*;

    /// An "npm" that records its arguments and lays out `node_modules/<name>` like the real one.
    fn fake_npm(
        dir: &Path,
        name: &str,
        version: &str,
        bin: &str,
        create_script: bool,
    ) -> NodeToolchain {
        let script = dir.join("npm-cli.sh");
        let args_log = dir.join("npm-args.txt");
        let package_dir = format!("node_modules/{name}");
        let create = if create_script {
            format!("mkdir -p {package_dir}/dist && echo 'console.log(1)' > {package_dir}/dist/index.js")
        } else {
            format!("mkdir -p {package_dir}")
        };
        let body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{log}'\n{create}\ncat > {package_dir}/package.json <<'EOF'\n{{\"name\":\"{name}\",\"version\":\"{version}\",\"bin\":{bin}}}\nEOF\n",
            log = args_log.display()
        );
        fs::write(&script, body).unwrap();
        NodeToolchain {
            node: "/bin/sh".into(),
            npm_cli: Some(script),
            npm_program: None,
        }
    }

    fn npm_world(name: &str, version: &str, bin: &str, create_script: bool) -> (World, PathBuf) {
        let scratch = tempfile::tempdir().unwrap().keep();
        let toolchain = fake_npm(&scratch, name, version, bin, create_script);
        let spec = format!("{name}@{version}");
        let world = world_with(
            Ok(toolchain),
            vec![agent(
                "codex",
                version,
                json!({"npx": {"package": spec, "args": ["--acp"]}}),
            )],
        );
        (world, scratch.join("npm-args.txt"))
    }

    #[test]
    fn an_npm_package_is_installed_without_running_its_scripts_and_started_with_node() {
        let (world, args_log) = npm_world(
            "@scope/codex-acp",
            "2.6.0",
            r#"{"codex-acp":"dist/index.js"}"#,
            true,
        );
        assert_eq!(
            world.registry.listing(true).unwrap().agents[0].blocked,
            None
        );
        world.registry.install("codex", "2.6.0").unwrap();

        let args = fs::read_to_string(args_log).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(args[0], "install");
        for flag in ["--ignore-scripts", "--omit=dev", "--save-exact"] {
            assert!(args.contains(&flag), "{flag} in {args:?}");
        }
        let registry = args.iter().position(|a| *a == "--registry").unwrap();
        assert_eq!(args[registry + 1], "https://registry.npmjs.org/");
        assert_eq!(args.last(), Some(&"@scope/codex-acp@2.6.0"));

        let launch = world.registry.launch_command("codex").unwrap();
        assert_eq!(launch.program, PathBuf::from("/bin/sh"));
        assert!(
            launch.args[0].ends_with("node_modules/@scope/codex-acp/dist/index.js"),
            "{:?}",
            launch.args
        );
        assert_eq!(launch.args[1], "--acp");
        assert_eq!(
            launch.read_only_paths,
            vec![world.packages().join("codex/2.6.0")]
        );
        let installed: Vec<_> = serde_json::from_slice::<Vec<Value>>(
            &fs::read(world.dir.path().join("agents/installations.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(installed[0]["launch"]["kind"], "node");
        assert!(matches!(
            serde_json::from_value::<Launch>(installed[0]["launch"].clone()),
            Ok(Launch::Node { .. })
        ));
    }

    #[test]
    fn a_package_that_installs_a_different_version_or_no_executable_is_refused() {
        let (world, _) = npm_world(
            "codex-acp",
            "2.6.0",
            r#"{"codex-acp":"dist/index.js"}"#,
            false,
        );
        let error = world.registry.install("codex", "2.6.0").unwrap_err();
        assert!(error.to_string().contains("no executable"), "{error}");
        assert_eq!(world.left_over(), vec![".staging".to_string()]);

        // npm writes version 2.6.1 although 2.6.0 was asked for.
        let scratch = tempfile::tempdir().unwrap().keep();
        let toolchain = fake_npm(
            &scratch,
            "codex-acp",
            "2.6.1",
            r#"{"codex-acp":"dist/index.js"}"#,
            true,
        );
        let world = world_with(
            Ok(toolchain),
            vec![agent(
                "codex",
                "2.6.0",
                json!({"npx": {"package": "codex-acp@2.6.0"}}),
            )],
        );
        let error = world.registry.install("codex", "2.6.0").unwrap_err();
        assert!(error.to_string().contains("different package"), "{error}");
    }

    #[test]
    fn an_executable_that_escapes_through_a_link_or_a_path_is_refused() {
        for bin in [
            r#""../../outside.js""#,
            r#""/etc/passwd""#,
            r#"{"a":"x.js","b":"y.js"}"#,
        ] {
            let (world, _) = npm_world("codex-acp", "2.6.0", bin, true);
            assert!(world.registry.install("codex", "2.6.0").is_err(), "{bin}");
            assert_eq!(world.left_over(), vec![".staging".to_string()], "{bin}");
        }
        // A link inside the package that points out of the install.
        let (world, _) = npm_world(
            "codex-acp",
            "2.6.0",
            r#"{"codex-acp":"dist/index.js"}"#,
            true,
        );
        let outside = world.dir.path().join("outside.js");
        fs::write(&outside, "x").unwrap();
        let scratch = tempfile::tempdir().unwrap().keep();
        let script = scratch.join("npm-cli.sh");
        let body = format!(
            "#!/bin/sh\nmkdir -p node_modules/codex-acp/dist\nln -s '{}' node_modules/codex-acp/dist/index.js\necho '{{\"name\":\"codex-acp\",\"version\":\"2.6.0\",\"bin\":\"dist/index.js\"}}' > node_modules/codex-acp/package.json\n",
            outside.display()
        );
        fs::write(&script, body).unwrap();
        let world = world_with(
            Ok(NodeToolchain {
                node: "/bin/sh".into(),
                npm_cli: Some(script),
                npm_program: None,
            }),
            vec![agent(
                "codex",
                "2.6.0",
                json!({"npx": {"package": "codex-acp@2.6.0"}}),
            )],
        );
        let error = world.registry.install("codex", "2.6.0").unwrap_err();
        assert!(error.to_string().contains("outside"), "{error}");
    }

    #[test]
    fn a_failing_npm_reports_its_reason_and_leaves_nothing() {
        let scratch = tempfile::tempdir().unwrap().keep();
        let script = scratch.join("npm-cli.sh");
        fs::write(
            &script,
            "#!/bin/sh\necho 'npm error 404 Not Found' >&2\nexit 1\n",
        )
        .unwrap();
        let world = world_with(
            Ok(NodeToolchain {
                node: "/bin/sh".into(),
                npm_cli: Some(script),
                npm_program: None,
            }),
            vec![agent(
                "codex",
                "2.6.0",
                json!({"npx": {"package": "codex-acp@2.6.0"}}),
            )],
        );
        let error = world.registry.install("codex", "2.6.0").unwrap_err();
        assert!(error.to_string().contains("404 Not Found"), "{error}");
        assert_eq!(world.left_over(), vec![".staging".to_string()]);
    }
}

#[test]
fn an_npm_agent_without_node_is_blocked_with_the_reason_and_not_installed() {
    let world = world_with(
        no_node(),
        vec![agent(
            "codex",
            "2.6.0",
            json!({"npx": {"package": "codex-acp@2.6.0"}}),
        )],
    );
    let card = &world.registry.listing(true).unwrap().agents[0];
    assert!(card.blocked.as_deref().unwrap().contains("Node.js 22"));
    let error = world.registry.install("codex", "2.6.0").unwrap_err();
    assert!(
        matches!(error, RegistryError::NotInstallable { .. }),
        "{error}"
    );
    assert!(world.left_over().is_empty(), "nothing was started");
}

/// The real registry, real downloads and this computer's Node.js. Run by hand:
/// `cargo test -p agent-registry --test install -- --ignored --nocapture`.
#[test]
#[ignore = "needs the network and Node.js; downloads about 100 MB"]
fn live_install_of_a_real_binary_and_a_real_npm_agent() {
    let dir = tempfile::tempdir().unwrap();
    let registry = AgentRegistry::open(dir.path()).unwrap();
    let listing = registry.listing(true).unwrap();
    assert_eq!(listing.origin, CatalogOrigin::Network);
    for card in &listing.agents {
        println!(
            "{:<22} {:<7} {:<7} {}",
            card.id,
            card.version,
            card.distribution,
            card.blocked.as_deref().unwrap_or("ok")
        );
    }
    for id in ["claude-acp", "codex-acp"] {
        let card = listing.agents.iter().find(|card| card.id == id).unwrap();
        if let Some(reason) = &card.blocked {
            panic!("{id} cannot be installed here: {reason}");
        }
        let installed = registry.install(id, &card.version).unwrap();
        let launch = registry.launch_command(id).unwrap();
        println!(
            "{} {} -> {} {:?}",
            installed.name,
            installed.version,
            launch.program.display(),
            launch.args
        );
        assert!(launch.program.exists());
        assert!(launch.args.iter().all(|arg| !arg.contains("..")));
    }
    for installed in registry.listing(false).unwrap().installed {
        registry.uninstall(&installed.id).unwrap();
    }
    assert!(registry.listing(false).unwrap().installed.is_empty());
}
