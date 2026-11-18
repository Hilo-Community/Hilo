use super::{
    auth::{self, AgentCredentials, AuthMethod, AuthenticationPlan},
    executor::{AgentToolExecutor, EventSink, WorkspaceText},
    protocol::*,
};
use crate::{
    broker::{BrokerError, ExecutionError, ExecutionReport, ToolBroker, ToolExecutor},
    budget::BudgetUsage,
    policy::{approval_inputs, Decision, HostAuthority, PolicyEngine, SignedTaskSpec},
    IntentOutcome, Lease, Millis, TaskStateRepository,
};
use host_core::{
    Approval, ApprovalActionKind, GatedCapability, TaskSpec, TaskState, ToolArgValue, ToolCall,
    ToolCapability, ToolPrecondition, WorkspaceTrustGate, CONTRACT_SCHEMA_VERSION,
};
use sandbox_runner::{ChildInput, ResourceLimits, SandboxRunner, SandboxSpec, SandboxedChild};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    sync::{
        atomic::Ordering,
        mpsc::{self, Receiver, SyncSender, TryRecvError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub enum AgentNetwork {
    Offline,
    ProviderOnly { destination: String },
    Proxy { address: std::net::SocketAddr },
}

pub struct AcpLaunch {
    pub identity: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Host-resolved installation files, never supplied by the webview.
    pub read_only_paths: Vec<PathBuf>,
    /// Explicit Host-built environment. Contains session tokens, never provider API keys.
    pub environment: BTreeMap<OsString, OsString>,
    pub writable_paths: Vec<PathBuf>,
    /// macOS preference domains the agent may read (from its reviewed profile).
    pub preference_domains: Vec<String>,
    pub credentials: AgentCredentials,
    pub network: AgentNetwork,
    pub limits: ResourceLimits,
}

pub struct AcpHost {
    /// Host-owned resources (egress listener, home lease) dropped when the session worker ends.
    pub session_resources: Vec<Box<dyn Send>>,
    pub gate: WorkspaceTrustGate,
    /// Host-provisioned task snapshot, separate from the workspace whose trust gate is held.
    pub worktree_root: PathBuf,
    pub repository: Arc<TaskStateRepository>,
    pub lease: Lease,
    pub signed: SignedTaskSpec,
    pub authority: Arc<HostAuthority>,
    pub runner: Arc<dyn SandboxRunner>,
    pub text: Arc<dyn WorkspaceText>,
    pub clock: Arc<dyn Fn() -> Millis + Send + Sync>,
    pub events: EventSink,
}

enum Control {
    TerminalResize(u16, u16),
    StartTerminalAuth,
    TerminalInput(String),
    Prompt(String),
    Permission(String, bool),
    Elicitation(String, String),
    CancelTurn,
}

pub struct AcpSessionSupervisor {
    login_terminal: Arc<Mutex<Option<Box<dyn sandbox_runner::SandboxedTerminal>>>>,
    identity: String,
    controls: SyncSender<Control>,
    executor: Arc<AgentToolExecutor>,
    thread: Option<JoinHandle<()>>,
    permissions: Arc<Mutex<BTreeMap<String, String>>>,
    elicitation_urls: Arc<Mutex<BTreeMap<String, String>>>,
}

impl AcpSessionSupervisor {
    pub fn start(mut host: AcpHost, launch: AcpLaunch) -> Result<Self, AcpError> {
        host.gate
            .authorize(GatedCapability::WorkspaceLocalBinary)
            .map_err(|e| AcpError::Launch(e.to_string()))?;
        if matches!(launch.credentials, AgentCredentials::HostMediated { .. }) {
            return Err(AcpError::Launch(
                "Host-mediated authentication requires a session proxy".into(),
            ));
        }
        if let AgentNetwork::ProviderOnly { destination } = launch.network {
            return Err(AcpError::Launch(format!("provider-only egress to {destination} cannot be enforced: sandbox-runner accepts only Denied or LocalProxyOnly; a Host destination-filtering proxy is missing")));
        }
        let root = host
            .worktree_root
            .canonicalize()
            .map_err(|e| AcpError::Launch(e.to_string()))?;
        // The agent sees this exact path (its cwd and the prefix of the absolute paths it sends).
        host.worktree_root.clone_from(&root);
        let executor = Arc::new(AgentToolExecutor::new(
            root.clone(),
            host.text.clone(),
            host.runner.clone(),
            launch.limits,
            host.events.clone(),
        ));
        let program = launch
            .program
            .canonicalize()
            .map_err(|e| AcpError::Launch(e.to_string()))?;
        let mut spec = SandboxSpec::new(root, &program);
        spec.worktree_read_only = true;
        // Installed agents execute their own read tools against the disposable task snapshot.
        // Kernel-enforced read-only access keeps every direct write a refused operation.
        spec.mediate_worktree_reads = false;
        if !host
            .signed
            .spec
            .policy
            .allowed_paths
            .iter()
            .any(|path| crate::policy::paths::normalize(path).as_deref() == Some(""))
        {
            return Err(AcpError::Launch(
                "Direct agent reads require an explicitly authorized whole-worktree scope".into(),
            ));
        }
        spec.denied_read_paths = forbidden_read_paths(
            &spec.worktree_root,
            &host.signed.spec.policy.forbidden_paths,
        )?;
        spec.args = launch.args.iter().map(Into::into).collect();
        spec.read_only_paths = launch.read_only_paths;
        spec.writable_paths = launch.writable_paths;
        spec.preference_domains = launch.preference_domains;
        spec.env = launch.environment;
        if let AgentNetwork::Proxy { address } = launch.network {
            spec.network = sandbox_runner::NetworkPolicy::LocalProxyOnly(address);
        }
        spec.limits = launch.limits;
        let base_spec = spec.clone();
        let launch_executor = Arc::new(LaunchExecutor {
            interactive: false,
            terminal: Mutex::new(None),
            spec,
            runner: host.runner.clone(),
            child: Mutex::new(None),
        });
        let launch_broker = ToolBroker::new(
            host.repository.clone(),
            PolicyEngine::new(host.authority.clone()),
            launch_executor.clone(),
            host.clock.clone(),
        );
        let call = tool_call(
            &host,
            "launch",
            "run_sandbox_command",
            ToolCapability::RunSandboxCommand,
            BTreeMap::from([
                (
                    "program".into(),
                    ToolArgValue::Text(program.to_string_lossy().into_owned()),
                ),
                ("cwd".into(), ToolArgValue::Text(".".into())),
                ("args".into(), ToolArgValue::StringList(launch.args)),
            ]),
            false,
        );
        launch_broker
            .execute(&host.lease, &host.signed, &call, &[], (host.clock)())
            .map_err(|e| AcpError::Launch(e.to_string()))?;
        let mut child = launch_executor
            .child
            .lock()
            .expect("launch lock")
            .take()
            .ok_or_else(|| AcpError::Launch("launch replay refused: no new process".into()))?;
        let input = child.take_stdin().ok_or_else(|| bad("stdin unavailable"))?;
        let output = child
            .take_stdout()
            .ok_or_else(|| bad("stdout unavailable"))?;
        let errors = child
            .take_stderr()
            .ok_or_else(|| bad("stderr unavailable"))?;
        let child = Arc::new(Mutex::new(child));
        *executor.agent.lock().expect("agent lock") = Some(child.clone());
        let (messages_tx, messages_rx) = mpsc::sync_channel(MAX_PENDING_REQUESTS);
        let output_limit = launch.limits.max_output_bytes;
        let output_child = child.clone();
        let reader = thread::spawn(move || {
            let mut input = BufReader::new(OutputBudget::new(output, output_limit));
            loop {
                let item = read_message(&mut input);
                let ended = !matches!(&item, Ok(Some(_)));
                // Break a concurrent blocked stdin write as soon as stdout is invalid/closed.
                if ended {
                    output_child.lock().expect("child lock").kill();
                }
                if messages_tx.send(item).is_err() || ended {
                    break;
                }
            }
        });
        let error_child = child.clone();
        let error_limit = launch.limits.max_output_bytes;
        let error_reader = thread::spawn(move || {
            let mut bytes = 0u64;
            let mut errors = errors;
            let mut buffer = [0u8; 4096];
            while let Ok(count) = errors.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                bytes = bytes.saturating_add(count as u64);
                if bytes > error_limit {
                    error_child.lock().expect("child lock").kill();
                    break;
                }
            }
        });
        let (controls, control_rx) = mpsc::sync_channel(MAX_PENDING_REQUESTS);
        let permissions = Arc::new(Mutex::new(BTreeMap::new()));
        let worker_permissions = permissions.clone();
        let elicitation_urls = Arc::new(Mutex::new(BTreeMap::new()));
        let worker_urls = elicitation_urls.clone();
        let worker_executor = executor.clone();
        let identity = launch.identity;
        let login_terminal = Arc::new(Mutex::new(None));
        let worker_terminal = login_terminal.clone();
        let thread = thread::spawn(move || {
            let mut session = Session::new(
                host,
                worker_executor.clone(),
                input,
                SessionUi {
                    permissions: worker_permissions,
                    elicitation_urls: worker_urls,
                    login_terminal: worker_terminal,
                },
                launch.credentials,
                base_spec,
            );
            let result = session.run(messages_rx, control_rx);
            session.cancel_pending();
            let cancelled = worker_executor.cancelled.load(Ordering::SeqCst);
            worker_executor.stop();
            session.stop_terminal();
            for reader in session.restart_readers.drain(..) {
                let _ = reader.join();
            }
            // Every intent was recorded by the broker. Unknown outcomes stay recorded for
            // reconciliation; we never invent a success when cancellation interrupts a tool.
            let snapshot = session
                .host
                .repository
                .snapshot(session.host.lease.task_id());
            let target = if snapshot
                .as_ref()
                .is_some_and(|s| !s.pending_intents.is_empty())
            {
                TaskState::NeedsReconciliation
            } else if cancelled {
                TaskState::Cancelled
            } else if result.is_err() {
                TaskState::Failed
            } else {
                TaskState::Cancelled
            };
            let reason = result.err().map_or_else(
                || {
                    if cancelled {
                        "Session stopped".into()
                    } else {
                        "Agent exited".into()
                    }
                },
                |e| e.to_string(),
            );
            if let Err(error) = session.host.repository.transition(
                &session.host.lease,
                target,
                &reason,
                (session.host.clock)(),
            ) {
                (session.host.events)(AcpEvent::Error {
                    reason: format!("Lifecycle requires recovery: {error}"),
                });
            }
            (session.host.events)(AcpEvent::Stopped { reason });
            drop(session);
            let _ = reader.join();
            let _ = error_reader.join();
        });
        Ok(Self {
            login_terminal,
            identity,
            controls,
            executor,
            thread: Some(thread),
            permissions,
            elicitation_urls,
        })
    }

    pub fn prompt(&self, text: String) -> Result<(), AcpError> {
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err(bad("prompt size limit"));
        }
        self.controls
            .try_send(Control::Prompt(text))
            .map_err(|_| bad("session closed or pending request limit"))
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn start_terminal_auth(&self) -> Result<(), AcpError> {
        self.controls
            .try_send(Control::StartTerminalAuth)
            .map_err(|_| bad("session closed or pending request limit"))
    }
    pub fn resize_terminal_auth(&self, cols: u16, rows: u16) -> Result<(), AcpError> {
        if !(2..=500).contains(&cols) || !(1..=200).contains(&rows) {
            return Err(bad("terminal size limit"));
        }
        self.controls
            .try_send(Control::TerminalResize(cols, rows))
            .map_err(|_| bad("session closed or pending request limit"))
    }
    pub fn write_terminal_auth(&self, text: String) -> Result<(), AcpError> {
        if text.is_empty() || text.len() > 8192 {
            return Err(bad("terminal input size limit"));
        }
        self.controls
            .try_send(Control::TerminalInput(text))
            .map_err(|_| bad("session closed or pending request limit"))
    }
    /// Trusted Host UI entry point only. The agent transport cannot call it or sign approvals.
    pub fn answer_permission(&self, id: String, allow: bool) -> Result<(), AcpError> {
        self.controls
            .try_send(Control::Permission(id, allow))
            .map_err(|_| bad("session closed or pending request limit"))
    }
    pub fn elicitation_url(&self, id: &str) -> Option<String> {
        self.elicitation_urls
            .lock()
            .expect("elicitation lock")
            .get(id)
            .cloned()
    }
    pub fn answer_elicitation(&self, id: String, action: String) -> Result<(), AcpError> {
        if !matches!(action.as_str(), "accept" | "decline" | "cancel") {
            return Err(bad("invalid elicitation action"));
        }
        self.controls
            .try_send(Control::Elicitation(id, action))
            .map_err(|_| bad("session closed or pending request limit"))
    }
    /// Cancels the current turn (ACP `session/cancel`): running commands are killed and pending
    /// requests answered as cancelled. The agent keeps running and the session stays open.
    pub fn cancel(&self) -> Result<(), AcpError> {
        self.executor.kill_commands();
        self.controls
            .try_send(Control::CancelTurn)
            .map_err(|_| bad("session closed or pending request limit"))
    }
    /// Ends the session: the agent and every command are killed.
    pub fn stop(&mut self) {
        if let Some(terminal) = self
            .login_terminal
            .lock()
            .expect("login terminal lock")
            .as_mut()
        {
            terminal.kill();
        }
        self.executor.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
    pub fn proposed_edits(&self) -> Vec<ProposedEdit> {
        self.executor.proposed_edits()
    }
    pub fn permission_description(&self, id: &str) -> Option<String> {
        self.permissions
            .lock()
            .expect("permissions lock")
            .get(id)
            .cloned()
    }
    pub fn process_ids(&self) -> Vec<u32> {
        self.executor.process_ids()
    }
}
impl Drop for AcpSessionSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn forbidden_read_paths(root: &Path, forbidden: &[String]) -> Result<Vec<PathBuf>, AcpError> {
    let mut denied = Vec::new();
    for relative in forbidden {
        let normalized = crate::policy::paths::normalize(relative)
            .ok_or_else(|| bad("invalid forbidden read scope"))?;
        if normalized.is_empty() {
            return Err(bad("the entire worktree is forbidden"));
        }
        // This runner-owned directory did not exist in the snapshot. It holds only this
        // run's private scratch, which is an explicit runtime write allowance.
        if normalized == sandbox_runner::TMP_DIR_NAME {
            continue;
        }
        let mut candidates = vec![root.to_path_buf()];
        for component in normalized.split('/') {
            let mut next = Vec::new();
            for parent in candidates {
                if !parent.is_dir() {
                    continue;
                }
                for entry in std::fs::read_dir(parent)? {
                    let entry = entry?;
                    if entry.file_name().to_string_lossy().to_lowercase()
                        == component.to_lowercase()
                    {
                        next.push(
                            sandbox_runner::PathGuard::verify_within_worktree(root, &entry.path())
                                .map_err(|e| AcpError::Launch(e.to_string()))?,
                        );
                    }
                }
            }
            candidates = next;
        }
        denied.extend(candidates);
    }
    denied.sort();
    denied.dedup();
    Ok(denied)
}

struct LaunchExecutor {
    interactive: bool,
    terminal: Mutex<Option<Box<dyn sandbox_runner::SandboxedTerminal>>>,
    spec: SandboxSpec,
    runner: Arc<dyn SandboxRunner>,
    child: Mutex<Option<SandboxedChild>>,
}
impl ToolExecutor for LaunchExecutor {
    fn estimate(&self, _spec: &TaskSpec, _call: &ToolCall) -> Result<BudgetUsage, ExecutionError> {
        Ok(BudgetUsage {
            tool_calls: 1,
            cpu_ms: self
                .spec
                .limits
                .max_cpu_time
                .ok_or_else(|| ExecutionError::NotStarted("Host must supply a CPU ceiling".into()))?
                .as_secs()
                .saturating_add(1)
                .saturating_mul(1000),
            ..BudgetUsage::default()
        })
    }
    fn execute(
        &self,
        spec: &TaskSpec,
        call: &ToolCall,
        _reserved: BudgetUsage,
    ) -> Result<ExecutionReport, ExecutionError> {
        if self.interactive {
            let terminal = self
                .runner
                .spawn_terminal(&self.spec)
                .map_err(super::executor::spawn_error)?;
            *self.terminal.lock().expect("terminal launch lock") = Some(terminal);
        } else {
            let child = self
                .runner
                .spawn(&self.spec)
                .map_err(super::executor::spawn_error)?;
            *self.child.lock().expect("launch lock") = Some(child);
        }
        Ok(ExecutionReport {
            outcome: IntentOutcome {
                succeeded: true,
                summary: "Sandboxed ACP process started".into(),
            },
            actual: self.estimate(spec, call)?,
            failure_signature: None,
        })
    }
}

struct Pending {
    request: Value,
    call: Option<ToolCall>,
    options: Value,
}
struct UrlElicitation {
    request: Value,
    elicitation_id: String,
    answered: bool,
}
struct SessionUi {
    permissions: Arc<Mutex<BTreeMap<String, String>>>,
    elicitation_urls: Arc<Mutex<BTreeMap<String, String>>>,
    login_terminal: Arc<Mutex<Option<Box<dyn sandbox_runner::SandboxedTerminal>>>>,
}
struct Session {
    auth_methods: Vec<AuthMethod>,
    login_attempted: bool,
    base_spec: SandboxSpec,
    login_method: Option<AuthMethod>,
    login_terminal: Arc<Mutex<Option<Box<dyn sandbox_runner::SandboxedTerminal>>>>,
    login_input: Option<ChildInput>,
    restart_readers: Vec<JoinHandle<()>>,
    terminal_authenticated: bool,
    host: AcpHost,
    executor: Arc<AgentToolExecutor>,
    broker: ToolBroker,
    input: ChildInput,
    session_id: Option<String>,
    outgoing: BTreeMap<u64, String>,
    next_id: u64,
    seen: BTreeSet<String>,
    pending: BTreeMap<String, Pending>,
    terminals: BTreeMap<String, (Value, ToolCall, Vec<crate::policy::SignedApproval>)>,
    prompt_active: bool,
    permissions: Arc<Mutex<BTreeMap<String, String>>>,
    elicitation_urls: Arc<Mutex<BTreeMap<String, String>>>,
    elicitations: BTreeMap<String, UrlElicitation>,
    credentials: AgentCredentials,
}
impl Session {
    fn new(
        host: AcpHost,
        executor: Arc<AgentToolExecutor>,
        input: ChildInput,
        ui: SessionUi,
        credentials: AgentCredentials,
        base_spec: SandboxSpec,
    ) -> Self {
        let broker = ToolBroker::new(
            host.repository.clone(),
            PolicyEngine::new(host.authority.clone()),
            executor.clone(),
            host.clock.clone(),
        );
        Self {
            auth_methods: vec![],
            login_attempted: false,
            base_spec,
            login_method: None,
            login_terminal: ui.login_terminal,
            login_input: None,
            restart_readers: vec![],
            terminal_authenticated: matches!(
                &credentials,
                AgentCredentials::Account {
                    use_stored_credentials: true,
                    ..
                }
            ),
            host,
            executor,
            broker,
            input,
            session_id: None,
            outgoing: BTreeMap::new(),
            next_id: 0,
            seen: BTreeSet::new(),
            pending: BTreeMap::new(),
            terminals: BTreeMap::new(),
            prompt_active: false,
            permissions: ui.permissions,
            elicitation_urls: ui.elicitation_urls,
            elicitations: BTreeMap::new(),
            credentials,
        }
    }
    fn request(&mut self, method: &str, params: Value) -> Result<(), AcpError> {
        if self.outgoing.len() >= MAX_PENDING_REQUESTS {
            return Err(bad("outgoing request limit"));
        }
        let id = self.next_id;
        self.next_id += 1;
        send(
            &mut self.input,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        self.outgoing.insert(id, method.into());
        Ok(())
    }
    fn reply(&mut self, request: &Value, result: Result<Value, String>) -> Result<(), AcpError> {
        let value = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
            Err(reason) => {
                json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":reason}})
            }
        };
        send(&mut self.input, &value)
    }
    fn run(
        &mut self,
        mut messages: Receiver<Result<Option<Value>, AcpError>>,
        controls: Receiver<Control>,
    ) -> Result<(), AcpError> {
        self.initialize()?;
        let timeout = Duration::from_secs(
            if matches!(self.credentials, AgentCredentials::Account { .. }) {
                600
            } else {
                10
            },
        );
        let mut deadline = Instant::now() + timeout;
        loop {
            if self.executor.cancelled.load(Ordering::SeqCst)
                || self
                    .host
                    .gate
                    .authorize(GatedCapability::WorkspaceLocalBinary)
                    .is_err()
                || (self.host.clock)() >= self.host.lease.expires_at()
            {
                self.cancel_pending();
                return Ok(());
            }
            if self.session_id.is_none() && Instant::now() > deadline {
                return Err(bad("handshake timeout"));
            }
            match controls.try_recv() {
                Ok(Control::TerminalResize(cols, rows)) => {
                    if let Some(terminal) = self
                        .login_terminal
                        .lock()
                        .expect("login terminal lock")
                        .as_ref()
                    {
                        terminal.resize(cols, rows)?;
                    }
                }
                Ok(Control::StartTerminalAuth) => self.start_terminal()?,
                Ok(Control::TerminalInput(text)) => {
                    use std::io::Write;
                    let input = self
                        .login_input
                        .as_mut()
                        .ok_or_else(|| bad("no interactive login is running"))?;
                    input.write_all(text.as_bytes())?;
                    input.flush()?;
                }
                Ok(Control::Prompt(text)) => {
                    if self.session_id.is_none() || self.prompt_active {
                        (self.host.events)(AcpEvent::Error {
                            reason: "Session is not ready or a prompt is already running".into(),
                        });
                    } else {
                        self.request("session/prompt", json!({"sessionId":self.session_id,"prompt":[{"type":"text","text":text}]}))?;
                        self.prompt_active = true;
                        (self.host.events)(AcpEvent::Message {
                            role: "user".into(),
                            text,
                        });
                    }
                }
                Ok(Control::Permission(id, allow)) => self.permission(&id, allow)?,
                Ok(Control::Elicitation(id, action)) => {
                    if let Some(elicitation) = self.elicitations.get_mut(&id) {
                        if !elicitation.answered {
                            elicitation.answered = true;
                            let request = elicitation.request.clone();
                            // After an accept the agent is still waiting for the user to finish
                            // in the browser: the page can be opened again until the agent says
                            // it is complete (`elicitation/complete`) or the turn is cancelled.
                            if action != "accept" {
                                self.elicitation_urls
                                    .lock()
                                    .expect("elicitation lock")
                                    .remove(&id);
                            }
                            self.reply(&request, Ok(json!({"action":action})))?;
                        }
                    }
                }
                Ok(Control::CancelTurn) => self.cancel_pending(),
                Err(TryRecvError::Disconnected) => return Ok(()),
                Err(TryRecvError::Empty) => {}
            }
            let terminal_status = self
                .login_terminal
                .lock()
                .expect("login terminal lock")
                .as_mut()
                .map(|terminal| terminal.try_wait())
                .transpose()?
                .flatten();
            if let Some(status) = terminal_status {
                self.stop_terminal();
                (self.host.events)(AcpEvent::TerminalAuthComplete {
                    success: status.success,
                });
                if !status.success {
                    return Err(bad("interactive account login exited without success"));
                }
                self.terminal_authenticated = true;
                messages = self.reconnect()?;
                self.initialize()?;
                deadline = Instant::now() + timeout;
            }
            match messages.recv_timeout(Duration::from_millis(10)) {
                Ok(Ok(Some(message))) => {
                    self.message(message)?;
                    deadline = Instant::now() + timeout;
                }
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
    fn initialize(&mut self) -> Result<(), AcpError> {
        self.request("initialize", json!({"protocolVersion":1,"clientInfo":{"name":"Coloride","version":"0.1.0"},"clientCapabilities":{"auth":{"terminal":self.host.runner.supports_terminal(),"_meta":{"gateway":true}},"elicitation":{"url":{}},"fs":{"readTextFile":true,"writeTextFile":true},"terminal":true,"_meta":{"writesAreProposals":true}}}))
    }
    fn stop_terminal(&mut self) {
        self.login_input.take();
        if let Some(mut terminal) = self
            .login_terminal
            .lock()
            .expect("login terminal lock")
            .take()
        {
            terminal.kill();
        }
    }
    fn start_terminal(&mut self) -> Result<(), AcpError> {
        self.login_attempted = true;
        let method = self
            .login_method
            .take()
            .ok_or_else(|| bad("no terminal login is pending"))?;
        let spec = auth::terminal_spec(&self.base_spec, &method)?;
        let launch = Arc::new(LaunchExecutor {
            spec: spec.clone(),
            runner: self.host.runner.clone(),
            child: Mutex::new(None),
            interactive: true,
            terminal: Mutex::new(None),
        });
        let broker = ToolBroker::new(
            self.host.repository.clone(),
            PolicyEngine::new(self.host.authority.clone()),
            launch.clone(),
            self.host.clock.clone(),
        );
        let call = tool_call(
            &self.host,
            "interactive-login",
            "run_sandbox_command",
            ToolCapability::RunSandboxCommand,
            BTreeMap::from([
                (
                    "program".into(),
                    ToolArgValue::Text(spec.program.to_string_lossy().into_owned()),
                ),
                ("cwd".into(), ToolArgValue::Text(".".into())),
                (
                    "args".into(),
                    ToolArgValue::StringList(
                        spec.args
                            .iter()
                            .map(|arg| arg.to_string_lossy().into_owned())
                            .collect(),
                    ),
                ),
            ]),
            false,
        );
        broker
            .execute(
                &self.host.lease,
                &self.host.signed,
                &call,
                &[],
                (self.host.clock)(),
            )
            .map_err(|e| AcpError::Launch(e.to_string()))?;
        let mut terminal = launch
            .terminal
            .lock()
            .expect("terminal launch lock")
            .take()
            .ok_or_else(|| bad("login launch replay refused"))?;
        self.login_input = Some(
            terminal
                .take_stdin()
                .ok_or_else(|| bad("login stdin unavailable"))?,
        );
        let mut output = terminal
            .take_stdout()
            .ok_or_else(|| bad("login stdout unavailable"))?;
        *self.login_terminal.lock().expect("login terminal lock") = Some(terminal);
        let terminal = self.login_terminal.clone();
        let events = self.host.events.clone();
        let limit = self.base_spec.limits.max_output_bytes;
        self.restart_readers.push(thread::spawn(move || {
            let mut bytes = 0u64;
            let mut buffer = [0u8; 4096];
            while let Ok(count) = output.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                bytes = bytes.saturating_add(count as u64);
                if bytes > limit {
                    if let Some(terminal) = terminal.lock().expect("login terminal lock").as_mut() {
                        terminal.kill();
                    }
                    break;
                }
                events(AcpEvent::TerminalAuthOutput {
                    data: buffer[..count].to_vec(),
                });
            }
        }));
        Ok(())
    }
    fn reconnect(&mut self) -> Result<Receiver<Result<Option<Value>, AcpError>>, AcpError> {
        if let Some(previous) = self.executor.agent.lock().expect("agent lock").take() {
            previous.lock().expect("child lock").kill();
        }
        let launch = Arc::new(LaunchExecutor {
            spec: self.base_spec.clone(),
            runner: self.host.runner.clone(),
            child: Mutex::new(None),
            interactive: false,
            terminal: Mutex::new(None),
        });
        let broker = ToolBroker::new(
            self.host.repository.clone(),
            PolicyEngine::new(self.host.authority.clone()),
            launch.clone(),
            self.host.clock.clone(),
        );
        let call = tool_call(
            &self.host,
            "launch-after-login",
            "run_sandbox_command",
            ToolCapability::RunSandboxCommand,
            BTreeMap::from([
                (
                    "program".into(),
                    ToolArgValue::Text(self.base_spec.program.to_string_lossy().into_owned()),
                ),
                ("cwd".into(), ToolArgValue::Text(".".into())),
                (
                    "args".into(),
                    ToolArgValue::StringList(
                        self.base_spec
                            .args
                            .iter()
                            .map(|arg| arg.to_string_lossy().into_owned())
                            .collect(),
                    ),
                ),
            ]),
            false,
        );
        broker
            .execute(
                &self.host.lease,
                &self.host.signed,
                &call,
                &[],
                (self.host.clock)(),
            )
            .map_err(|e| AcpError::Launch(e.to_string()))?;
        let mut child = launch
            .child
            .lock()
            .expect("launch lock")
            .take()
            .ok_or_else(|| bad("reconnect launch replay refused"))?;
        self.input = child.take_stdin().ok_or_else(|| bad("stdin unavailable"))?;
        let output = child
            .take_stdout()
            .ok_or_else(|| bad("stdout unavailable"))?;
        let mut errors = child
            .take_stderr()
            .ok_or_else(|| bad("stderr unavailable"))?;
        let child = Arc::new(Mutex::new(child));
        *self.executor.agent.lock().expect("agent lock") = Some(child.clone());
        let (tx, rx) = mpsc::sync_channel(MAX_PENDING_REQUESTS);
        let limit = self.base_spec.limits.max_output_bytes;
        let output_child = child.clone();
        self.restart_readers.push(thread::spawn(move || {
            let mut input = BufReader::new(OutputBudget::new(output, limit));
            loop {
                let item = read_message(&mut input);
                let ended = !matches!(&item, Ok(Some(_)));
                if ended {
                    output_child.lock().expect("child lock").kill();
                }
                if tx.send(item).is_err() || ended {
                    break;
                }
            }
        }));
        self.restart_readers.push(thread::spawn(move || {
            let mut bytes = 0u64;
            let mut buffer = [0u8; 4096];
            while let Ok(count) = errors.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                bytes = bytes.saturating_add(count as u64);
                if bytes > limit {
                    child.lock().expect("child lock").kill();
                    break;
                }
            }
        }));
        self.outgoing.clear();
        self.next_id = 0;
        self.seen.clear();
        Ok(rx)
    }
    fn choose_authentication(&mut self) -> Result<(), AcpError> {
        match auth::authentication(&self.credentials, &self.auth_methods)? {
            AuthenticationPlan::Skip => self.new_session(),
            AuthenticationPlan::Request(params) => self.request("authenticate", params),
            AuthenticationPlan::Terminal(method) => {
                if !self.host.runner.supports_terminal() {
                    return Err(bad(
                        "interactive sandboxed login is unavailable on this engine",
                    ));
                }
                auth::terminal_spec(&self.base_spec, &method)?;
                (self.host.events)(AcpEvent::TerminalAuth {
                    name: method.name.clone(),
                    args: method.args.clone(),
                    env: method.env.clone(),
                });
                self.login_method = Some(method);
                Ok(())
            }
        }
    }
    fn new_session(&mut self) -> Result<(), AcpError> {
        self.request(
            "session/new",
            json!({"cwd":self.host.worktree_root,"mcpServers":[]}),
        )
    }
    fn elicit(&mut self, message: Value) -> Result<(), AcpError> {
        if message.get("id").is_none() {
            return Err(bad("elicitation must be a request"));
        }
        let key = message["id"].to_string();
        if self.seen.len() >= MAX_SESSION_ITEMS
            || !self.seen.insert(key.clone())
            || self.elicitations.len() >= MAX_PENDING_REQUESTS
        {
            return Err(bad("elicitation request limit or duplicate id"));
        }
        let params = &message["params"];
        let in_session = self.session_id.is_some() && params["sessionId"] == json!(self.session_id);
        let in_auth = params["requestId"]
            .as_u64()
            .is_some_and(|id| self.outgoing.get(&id).is_some_and(|m| m == "authenticate"));
        if !in_session && !in_auth {
            return Err(bad("elicitation has no active request or session"));
        }
        if params["mode"] != "url" {
            return self.reply(&message, Err("Unsupported elicitation mode".into()));
        }
        let url = params["url"]
            .as_str()
            .filter(|url| url.len() <= 8192)
            .ok_or_else(|| bad("invalid elicitation URL"))?
            .to_owned();
        auth::browser_url(&self.credentials, &url)?;
        let elicitation_id = params["elicitationId"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or_else(|| bad("invalid elicitation id"))?
            .to_owned();
        if self
            .elicitations
            .values()
            .any(|e| e.elicitation_id == elicitation_id)
        {
            return Err(bad("duplicate elicitation id"));
        }
        let text = params["message"]
            .as_str()
            .filter(|text| text.len() <= MAX_TEXT_BYTES)
            .ok_or_else(|| bad("invalid elicitation message"))?
            .to_owned();
        self.elicitation_urls
            .lock()
            .expect("elicitation lock")
            .insert(key.clone(), url.clone());
        self.elicitations.insert(
            key.clone(),
            UrlElicitation {
                request: message,
                elicitation_id,
                answered: false,
            },
        );
        (self.host.events)(AcpEvent::Elicitation {
            id: key,
            url,
            message: text,
        });
        Ok(())
    }
    fn cancel_pending(&mut self) {
        self.elicitation_urls
            .lock()
            .expect("elicitation lock")
            .clear();
        for (id, elicitation) in std::mem::take(&mut self.elicitations) {
            if !elicitation.answered {
                let _ = self.reply(&elicitation.request, Ok(json!({"action":"cancel"})));
            }
            (self.host.events)(AcpEvent::ElicitationComplete { id });
        }
        self.permissions.lock().expect("permissions lock").clear();
        let pending = std::mem::take(&mut self.pending);
        for (id, item) in pending {
            let result = if item.call.is_none() {
                Ok(json!({"outcome":{"outcome":"cancelled"}}))
            } else {
                Err("Session cancelled before authorization".into())
            };
            let _ = self.reply(&item.request, result);
            (self.host.events)(AcpEvent::Tool {
                id,
                kind: "permission".into(),
                status: "denied".into(),
                locations: json!([]),
                reason: Some("Session cancelled".into()),
            });
        }
        let _ = send(
            &mut self.input,
            &json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":self.session_id}}),
        );
    }
    fn message(&mut self, message: Value) -> Result<(), AcpError> {
        let Some(method) = message["method"].as_str() else {
            let id = message["id"]
                .as_u64()
                .ok_or_else(|| bad("unknown response id"))?;
            let method = self
                .outgoing
                .remove(&id)
                .ok_or_else(|| bad("unsolicited response"))?;
            if let Some(error) = message.get("error") {
                // A saved successful terminal login is only a hint. An expired/revoked login
                // must go through the agent's advertised flow again, once, without a fallback.
                if method == "session/new"
                    && self.terminal_authenticated
                    && !self.login_attempted
                    && error["code"] == -32000
                {
                    self.terminal_authenticated = false;
                    return self.choose_authentication();
                }
                return Err(AcpError::Protocol(format!("{method}: {error}")));
            }
            match method.as_str() {
                "initialize" => {
                    if message["result"]["protocolVersion"] != 1 {
                        return Err(bad("unsupported protocol version"));
                    }
                    let methods = auth::methods(&message["result"]["authMethods"])?;
                    (self.host.events)(AcpEvent::AuthMethods {
                        methods: methods.clone(),
                    });
                    self.auth_methods = methods;
                    if self.terminal_authenticated {
                        self.new_session()?;
                    } else {
                        self.choose_authentication()?;
                    }
                }
                "authenticate" => self.new_session()?,
                "session/new" => {
                    let id = message["result"]["sessionId"]
                        .as_str()
                        .filter(|s| !s.is_empty() && s.len() < 256)
                        .ok_or_else(|| bad("invalid session id"))?
                        .to_string();
                    self.session_id = Some(id.clone());
                    (self.host.events)(AcpEvent::Ready { session_id: id });
                }
                "session/prompt" => {
                    self.prompt_active = false;
                    (self.host.events)(AcpEvent::Update {
                        update: json!({"sessionUpdate":"turn_complete","stopReason":message["result"]["stopReason"]}),
                    });
                }
                _ => return Err(bad("unexpected response")),
            }
            return Ok(());
        };
        // ACP extension notifications are optional. They may describe connection-level
        // state before session/new; ignoring an unknown one never grants a capability.
        if (method.starts_with('_') || method == "auth/status_update")
            && message.get("id").is_none()
        {
            return Ok(());
        }
        if method == "elicitation/create" {
            return self.elicit(message);
        }
        if method == "elicitation/complete" && message.get("id").is_none() {
            let elicitation_id = message["params"]["elicitationId"]
                .as_str()
                .unwrap_or_default();
            let key = self
                .elicitations
                .iter()
                .find(|(_, e)| e.elicitation_id == elicitation_id)
                .map(|(id, _)| id.clone());
            if let Some(id) = key {
                self.elicitations.remove(&id);
                self.elicitation_urls
                    .lock()
                    .expect("elicitation lock")
                    .remove(&id);
                (self.host.events)(AcpEvent::ElicitationComplete { id });
            }
            return Ok(());
        }
        if self.session_id.is_none() || message["params"]["sessionId"] != json!(self.session_id) {
            return Err(bad(&format!("wrong session for {method}")));
        }
        if method == "session/update" && message.get("id").is_none() {
            let mut update = message["params"]["update"].clone();
            if !update.is_object() {
                return Err(bad("invalid session update"));
            }
            // Agents report the absolute paths ACP uses; the Host UI opens worktree-relative ones.
            if let Some(locations) = update.get_mut("locations").and_then(Value::as_array_mut) {
                for location in locations {
                    if let Some(path) = location["path"].as_str() {
                        location["path"] = json!(self.relative(path));
                    }
                }
            }
            (self.host.events)(AcpEvent::Update { update });
            return Ok(());
        }
        if message.get("id").is_none() {
            return Err(bad("unknown notification"));
        }
        let key = message["id"].to_string();
        if self.seen.len() >= MAX_SESSION_ITEMS || !self.seen.insert(key.clone()) {
            return Err(bad("request count limit or duplicate id"));
        }
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            return Err(bad("pending request limit"));
        }
        if method == "session/request_permission" {
            let options = message["params"]["options"].clone();
            if !options.is_array() || options.as_array().is_some_and(|v| v.len() > 16) {
                return Err(bad("invalid permission options"));
            }
            self.permissions
                .lock()
                .expect("permissions lock")
                .insert(key.clone(), message["params"].to_string());
            (self.host.events)(AcpEvent::Permission {
                id: key.clone(),
                reason: format!(
                    "Agent requests permission: {}",
                    message["params"]["toolCall"]
                ),
                options: options.clone(),
            });
            self.pending.insert(
                key,
                Pending {
                    request: message,
                    call: None,
                    options,
                },
            );
            return Ok(());
        }
        if method.starts_with("terminal/") && method != "terminal/create" {
            let id = message["params"]["terminalId"].as_str().unwrap_or("");
            let result = match self.terminals.get(id).cloned() {
                Some((output, call, approvals)) => {
                    if let Err(error) = self.broker.execute(
                        &self.host.lease,
                        &self.host.signed,
                        &call,
                        &approvals,
                        (self.host.clock)(),
                    ) {
                        return self.reply(&message, Err(error.to_string()));
                    }
                    match method {
                        "terminal/output" => Ok(output),
                        "terminal/wait_for_exit" => Ok(output["exitStatus"].clone()),
                        "terminal/kill" => Ok(json!({})), // run-to-completion: already exited
                        "terminal/release" => {
                            self.terminals.remove(id);
                            Ok(json!({}))
                        }
                        _ => Err("Unknown ACP method".into()),
                    }
                }
                None => Err("Unknown terminal".into()),
            };
            return self.reply(&message, result);
        }
        match self.translate(&message, &key) {
            Ok(call) => self.execute(message, call, &[]),
            Err(reason) => self.reply(&message, Err(reason)),
        }
    }
    fn translate(&self, message: &Value, key: &str) -> Result<ToolCall, String> {
        let params = &message["params"];
        let text = |key: &str| {
            params[key]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("Missing {key}"))
        };
        let arg = |key: &str| text(key).map(ToolArgValue::Text);
        let path = |key: &str| text(key).map(|p| ToolArgValue::Text(self.relative(&p)));
        let (name, cap, args, approval) = match message["method"].as_str().unwrap_or("") {
            "fs/read_text_file" => (
                "read_workspace",
                ToolCapability::ReadWorkspace,
                BTreeMap::from([("path".into(), path("path")?)]),
                false,
            ),
            "fs/write_text_file" => (
                "write_worktree",
                ToolCapability::WriteWorktree,
                BTreeMap::from([
                    ("path".into(), path("path")?),
                    ("content".into(), arg("content")?),
                ]),
                true,
            ),
            "terminal/create" => {
                if params
                    .get("env")
                    .and_then(Value::as_array)
                    .is_some_and(|v| !v.is_empty())
                {
                    return Err("Agent environment variables are not accepted".into());
                }
                let args: Vec<String> =
                    serde_json::from_value(params.get("args").cloned().unwrap_or(json!([])))
                        .map_err(|_| "Invalid args")?;
                (
                    "run_sandbox_command",
                    ToolCapability::RunSandboxCommand,
                    BTreeMap::from([
                        ("program".into(), arg("command")?),
                        (
                            "cwd".into(),
                            ToolArgValue::Text(
                                self.relative(params["cwd"].as_str().unwrap_or(".")),
                            ),
                        ),
                        ("args".into(), ToolArgValue::StringList(args)),
                    ]),
                    true,
                )
            }
            "_coloride/query_local_mcp" => (
                "query_local_mcp",
                ToolCapability::QueryLocalMcp,
                BTreeMap::from([("request".into(), arg("request")?)]),
                false,
            ),
            _ => return Err("Unknown ACP method".into()),
        };
        Ok(tool_call(&self.host, key, name, cap, args, approval))
    }
    /// ACP paths are absolute. One inside the worktree becomes the relative path the policy
    /// checks; any other path is passed on unchanged, for the policy to refuse.
    fn relative(&self, path: &str) -> String {
        match std::path::Path::new(path).strip_prefix(&self.host.worktree_root) {
            Ok(rest) if rest.as_os_str().is_empty() => ".".into(),
            Ok(rest) => rest.to_string_lossy().replace('\\', "/"),
            Err(_) => path.into(),
        }
    }
    fn execute(
        &mut self,
        request: Value,
        call: ToolCall,
        approvals: &[crate::policy::SignedApproval],
    ) -> Result<(), AcpError> {
        let id = request["id"].to_string();
        let locations = call
            .validated_args
            .get("path")
            .and_then(|v| {
                if let ToolArgValue::Text(p) = v {
                    Some(json!([{"path":p,"line":1}]))
                } else {
                    None
                }
            })
            .unwrap_or(json!([]));
        (self.host.events)(AcpEvent::Tool {
            id: id.clone(),
            kind: call.tool_name.clone(),
            status: "running".into(),
            locations: locations.clone(),
            reason: None,
        });
        match self.broker.execute(
            &self.host.lease,
            &self.host.signed,
            &call,
            approvals,
            (self.host.clock)(),
        ) {
            Err(BrokerError::Policy(Decision::RequiresApproval(_, reason))) => {
                let reason = format!(
                    "{reason}\nTool: {}\nArguments: {:?}",
                    call.tool_name, call.validated_args
                );
                self.permissions
                    .lock()
                    .expect("permissions lock")
                    .insert(id.clone(), reason.clone());
                let options = json!([{"optionId":"allow","name":"Allow once","kind":"allow_once"},{"optionId":"reject","name":"Reject","kind":"reject_once"}]);
                (self.host.events)(AcpEvent::Permission {
                    id: id.clone(),
                    reason,
                    options: options.clone(),
                });
                (self.host.events)(AcpEvent::Tool {
                    id: id.clone(),
                    kind: call.tool_name.clone(),
                    status: "pending".into(),
                    locations,
                    reason: None,
                });
                self.pending.insert(
                    id,
                    Pending {
                        request,
                        call: Some(call),
                        options,
                    },
                );
                Ok(())
            }
            Err(error) => {
                let reason = error.to_string();
                (self.host.events)(AcpEvent::Tool {
                    id,
                    kind: call.tool_name,
                    status: "denied".into(),
                    locations,
                    reason: Some(reason.clone()),
                });
                self.reply(&request, Err(reason))
            }
            Ok(outcome) => {
                let result = if !outcome.succeeded {
                    Err(outcome.summary)
                } else {
                    match call.tool_name.as_str() {
                        "read_workspace" => {
                            let line = request["params"]["line"]
                                .as_u64()
                                .unwrap_or(1)
                                .saturating_sub(1) as usize;
                            let limit = request["params"]["limit"]
                                .as_u64()
                                .unwrap_or(u64::MAX)
                                .min(MAX_TEXT_BYTES as u64)
                                as usize;
                            let content = if line == 0 && request["params"].get("limit").is_none() {
                                outcome.summary
                            } else {
                                outcome
                                    .summary
                                    .split_inclusive('\n')
                                    .skip(line)
                                    .take(limit)
                                    .collect::<String>()
                            };
                            Ok(json!({"content":content}))
                        }
                        "run_sandbox_command" => {
                            let output = serde_json::from_str(&outcome.summary)?;
                            self.terminals
                                .insert(id.clone(), (output, call.clone(), approvals.to_vec()));
                            Ok(json!({"terminalId":id}))
                        }
                        "query_local_mcp" => Ok(json!({"content":outcome.summary})),
                        _ => Ok(json!({})),
                    }
                };
                (self.host.events)(AcpEvent::Tool {
                    id,
                    kind: call.tool_name,
                    status: if result.is_ok() { "done" } else { "failed" }.into(),
                    locations,
                    reason: result.as_ref().err().cloned(),
                });
                self.reply(&request, result)
            }
        }
    }
    fn permission(&mut self, id: &str, allow: bool) -> Result<(), AcpError> {
        self.permissions
            .lock()
            .expect("permissions lock")
            .remove(id);
        let Some(item) = self.pending.remove(id) else {
            return Ok(());
        };
        if let Some(call) = item.call {
            if !allow {
                (self.host.events)(AcpEvent::Tool {
                    id: id.into(),
                    kind: call.tool_name,
                    status: "denied".into(),
                    locations: json!([]),
                    reason: Some("Rejected by Host user".into()),
                });
                return self.reply(&item.request, Err("Rejected by Host user".into()));
            }
            let (call_hash, plan_hash) =
                approval_inputs(&self.host.signed, &call).map_err(|e| bad(&e.to_string()))?;
            let kind = ApprovalActionKind::ExecuteToolCall;
            let approval = Approval {
                schema_version: CONTRACT_SCHEMA_VERSION,
                approval_id: format!("acp:{id}"),
                actor_identity: "Host user".into(),
                target_env: self.host.signed.spec.target_env.clone(),
                action_kind: kind,
                approved_digest: Approval::compute_action_digest(
                    &call_hash,
                    &plan_hash,
                    &self.host.signed.spec.target_env,
                    &kind,
                ),
                expires_at: (self.host.clock)()
                    .saturating_add(60_000)
                    .min(self.host.lease.expires_at()),
            };
            let signed = self
                .host
                .authority
                .sign_approval(approval)
                .map_err(|e| bad(&e.to_string()))?;
            self.execute(item.request, call, &[signed])
        } else {
            let kind = if allow { "allow_once" } else { "reject_once" };
            let option = item
                .options
                .as_array()
                .and_then(|v| v.iter().find(|o| o["kind"] == kind))
                .and_then(|o| o["optionId"].as_str());
            let outcome = option.map_or(
                json!({"outcome":"cancelled"}),
                |id| json!({"outcome":"selected","optionId":id}),
            );
            self.reply(&item.request, Ok(json!({"outcome":outcome})))
        }
    }
}

fn tool_call(
    host: &AcpHost,
    id: &str,
    name: &str,
    cap: ToolCapability,
    args: BTreeMap<String, ToolArgValue>,
    approval: bool,
) -> ToolCall {
    ToolCall {
        schema_version: CONTRACT_SCHEMA_VERSION,
        call_id: id.into(),
        task_id: host.lease.task_id().into(),
        tool_name: name.into(),
        validated_args: args,
        required_capabilities: vec![cap],
        preconditions: if approval {
            vec![ToolPrecondition {
                description: "Host consent for one exact ACP call".into(),
                required_snapshot_hash: None,
                requires_human_approval: true,
            }]
        } else {
            vec![]
        },
        idempotency_key: format!("acp:{}:{id}", host.lease.generation()),
    }
}
