//! One ACP agent session: the supervisor the Host drives, and the worker loop that speaks the
//! protocol. Signing in lives in `sign_in`, tool calls and permissions in `tools`.
mod launch;
mod sign_in;
mod tools;

use launch::{sandbox_spec, spawn_agent, LaunchExecutor};

use super::{
    auth::{self, AgentCredentials, AuthMethod, AuthenticationPlan},
    executor::{AgentToolExecutor, EventSink, WorkspaceText},
    options::{self, AgentOption},
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
    collections::{BTreeMap, BTreeSet, VecDeque},
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
    /// macOS only, see `SandboxSpec::loopback_listen_ports`.
    pub loopback_listen_ports: Vec<u16>,
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
    /// The user's dependency folders (`node_modules`) that the task snapshot links to. The agent
    /// and the commands it asks for may read them, never write: without this the links resolve
    /// outside the sandbox and a build or a test cannot find its packages.
    pub shared_read_only_paths: Vec<PathBuf>,
    pub clock: Arc<dyn Fn() -> Millis + Send + Sync>,
    pub events: EventSink,
}

enum Control {
    TerminalResize(u16, u16),
    StartTerminalAuth,
    ChooseAuth(String),
    TerminalInput(String),
    SetOption(String, String),
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
            host.shared_read_only_paths.clone(),
            host.events.clone(),
        ));
        let program = launch
            .program
            .canonicalize()
            .map_err(|e| AcpError::Launch(e.to_string()))?;
        let base_spec = sandbox_spec(&host, &launch, root, &program)?;
        let spawned = spawn_agent(&host, &base_spec, "launch")?;
        *executor.agent.lock().expect("agent lock") = Some(spawned.child.clone());
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
                worker_executor,
                spawned.input,
                SessionUi {
                    permissions: worker_permissions,
                    elicitation_urls: worker_urls,
                    login_terminal: worker_terminal,
                },
                launch.credentials,
                base_spec,
            );
            let result = session.run(spawned.messages, control_rx);
            session.finish(result);
            for reader in spawned.readers {
                let _ = reader.join();
            }
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
    /// Trusted Host UI entry point: the user's pick for one of the options the agent offered.
    pub fn set_option(&self, option_id: String, value: String) -> Result<(), AcpError> {
        if option_id.is_empty() || option_id.len() > 256 || value.is_empty() || value.len() > 256 {
            return Err(bad("option size limit"));
        }
        self.controls
            .try_send(Control::SetOption(option_id, value))
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
    /// Trusted Host UI entry point: the user's pick among the methods offered by `AuthChoice`.
    pub fn choose_authentication(&self, method_id: String) -> Result<(), AcpError> {
        self.controls
            .try_send(Control::ChooseAuth(method_id))
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
    /// An account `authenticate` just succeeded: reported once the session is ready.
    signed_in: bool,
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
    /// What the agent offers for this session, and the picks sent but not yet answered.
    options: Vec<AgentOption>,
    pending_options: VecDeque<(String, String)>,
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
            signed_in: false,
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
            options: Vec::new(),
            pending_options: VecDeque::new(),
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
    /// Ends the session after its loop returned: stops what is still running, then records how
    /// the task ended and tells the panel.
    fn finish(mut self, result: Result<(), AcpError>) {
        self.cancel_pending();
        let cancelled = self.executor.cancelled.load(Ordering::SeqCst);
        self.executor.stop();
        self.stop_terminal();
        for reader in self.restart_readers.drain(..) {
            let _ = reader.join();
        }
        // Every intent was recorded by the broker. Unknown outcomes stay recorded for
        // reconciliation; we never invent a success when cancellation interrupts a tool.
        let snapshot = self.host.repository.snapshot(self.host.lease.task_id());
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
        if let Err(error) =
            self.host
                .repository
                .transition(&self.host.lease, target, &reason, (self.host.clock)())
        {
            (self.host.events)(AcpEvent::Error {
                reason: format!("Lifecycle requires recovery: {error}"),
            });
        }
        (self.host.events)(AcpEvent::Stopped { reason });
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
                Ok(Control::ChooseAuth(id)) => self.choose_method(&id)?,
                Ok(Control::TerminalInput(text)) => {
                    use std::io::Write;
                    let input = self
                        .login_input
                        .as_mut()
                        .ok_or_else(|| bad("no interactive login is running"))?;
                    input.write_all(text.as_bytes())?;
                    input.flush()?;
                }
                Ok(Control::SetOption(id, value)) => self.set_option(&id, &value)?,
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
    fn publish_options(&self) {
        (self.host.events)(AcpEvent::Options {
            options: self.options.clone(),
        });
    }
    /// Sends the user's pick to the agent, but only a value the agent itself offered, and only
    /// between turns. Anything else leaves the panel showing what is really in force.
    fn set_option(&mut self, id: &str, value: &str) -> Result<(), AcpError> {
        let request = match (&self.session_id, self.prompt_active) {
            (Some(session), false) => self
                .options
                .iter()
                .find(|option| option.id == id)
                .filter(|option| option.choices.iter().any(|choice| choice.value == value))
                .map(|option| options::change_request(option, session, value)),
            _ => None,
        };
        let Some((method, params)) = request else {
            self.publish_options();
            return Ok(());
        };
        self.request(method, params)?;
        self.pending_options.push_back((id.into(), value.into()));
        Ok(())
    }
    fn new_session(&mut self) -> Result<(), AcpError> {
        let cwd = super::session::tools::plain_path(&self.host.worktree_root);
        self.request("session/new", json!({"cwd":cwd,"mcpServers":[]}))
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
    /// One message from the agent: a response to a request of ours, a notification, or a
    /// request the Host must answer.
    fn message(&mut self, message: Value) -> Result<(), AcpError> {
        let Some(method) = message["method"].as_str().map(str::to_owned) else {
            return self.response(message);
        };
        if method == "elicitation/create" {
            return self.elicit(message);
        }
        if message.get("id").is_none() {
            return self.notification(&method, message);
        }
        self.agent_request(&method, message)
    }

    /// The agent answered `initialize`, `authenticate`, `session/new` or `session/prompt`.
    fn response(&mut self, message: Value) -> Result<(), AcpError> {
        let id = message["id"]
            .as_u64()
            .ok_or_else(|| bad("unknown response id"))?;
        let method = self
            .outgoing
            .remove(&id)
            .ok_or_else(|| bad("unsolicited response"))?;
        if let Some(error) = message.get("error") {
            // A refused model or mode change is not a broken session: show what is in force.
            if method.starts_with("session/set_") {
                self.pending_options.pop_front();
                self.publish_options();
                return Ok(());
            }
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
            "authenticate" => {
                self.signed_in = matches!(self.credentials, AgentCredentials::Account { .. });
                self.new_session()?
            }
            "session/new" => {
                let id = message["result"]["sessionId"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() < 256)
                    .ok_or_else(|| bad("invalid session id"))?
                    .to_string();
                self.session_id = Some(id.clone());
                self.options = options::parse(&message["result"]);
                self.pending_options.clear();
                (self.host.events)(AcpEvent::Ready { session_id: id });
                if !self.options.is_empty() {
                    self.publish_options();
                }
                if std::mem::take(&mut self.signed_in) {
                    (self.host.events)(AcpEvent::Authenticated);
                }
            }
            "session/set_config_option" | "session/set_mode" | "session/set_model" => {
                if let Some((id, value)) = self.pending_options.pop_front() {
                    // A config change answers with the whole list: it can move other options.
                    let all = options::parse(
                        &json!({"configOptions":message["result"]["configOptions"]}),
                    );
                    if method == "session/set_config_option" && !all.is_empty() {
                        self.options = all;
                    } else {
                        options::select(&mut self.options, &id, &value);
                    }
                }
                self.publish_options();
            }
            "session/prompt" => {
                self.prompt_active = false;
                (self.host.events)(AcpEvent::Update {
                    update: json!({"sessionUpdate":"turn_complete","stopReason":message["result"]["stopReason"]}),
                });
            }
            _ => return Err(bad("unexpected response")),
        }
        Ok(())
    }

    /// A notification from the agent (no answer expected).
    fn notification(&mut self, method: &str, message: Value) -> Result<(), AcpError> {
        // ACP extension notifications are optional. They may describe connection-level
        // state before session/new; ignoring an unknown one never grants a capability.
        if method.starts_with('_') || method == "auth/status_update" {
            return Ok(());
        }
        if method == "elicitation/complete" {
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
        self.check_session(method, &message)?;
        if method != "session/update" {
            return Err(bad("unknown notification"));
        }
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
        // The same for the files of an edit: the card shows `src/a.ts`, not the snapshot's folder.
        if let Some(blocks) = update.get_mut("content").and_then(Value::as_array_mut) {
            for block in blocks {
                if block["type"] == "diff" {
                    if let Some(path) = block["path"].as_str() {
                        block["path"] = json!(self.relative(path));
                    }
                }
            }
        }
        // The agent changed its own mode or options (it can, for instance, leave plan mode).
        match update["sessionUpdate"].as_str() {
            Some("current_mode_update") => {
                if let Some(mode) = update["currentModeId"].as_str() {
                    if options::select(&mut self.options, "mode", mode) {
                        self.publish_options();
                    }
                }
            }
            Some("config_option_update") => {
                let all = options::parse(&json!({"configOptions":update["configOptions"]}));
                if !all.is_empty() {
                    self.options = all;
                    self.publish_options();
                }
            }
            _ => {}
        }
        (self.host.events)(AcpEvent::Update { update });
        Ok(())
    }

    /// A request from the agent in this session: a permission, one of its terminals, or a tool
    /// call the broker must authorize.
    fn agent_request(&mut self, method: &str, message: Value) -> Result<(), AcpError> {
        self.check_session(method, &message)?;
        let key = message["id"].to_string();
        if self.seen.len() >= MAX_SESSION_ITEMS || !self.seen.insert(key.clone()) {
            return Err(bad("request count limit or duplicate id"));
        }
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            return Err(bad("pending request limit"));
        }
        if method == "session/request_permission" {
            return self.permission_request(key, message);
        }
        if method.starts_with("terminal/") && method != "terminal/create" {
            return self.terminal_request(method, message);
        }
        match self.translate(&message, &key) {
            Ok(call) => self.execute(message, call, &[]),
            Err(reason) => self.reply(&message, Err(reason)),
        }
    }

    /// Everything but the handshake and sign-in belongs to the session the Host opened.
    fn check_session(&self, method: &str, message: &Value) -> Result<(), AcpError> {
        if self.session_id.is_none() || message["params"]["sessionId"] != json!(self.session_id) {
            return Err(bad(&format!("wrong session for {method}")));
        }
        Ok(())
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
