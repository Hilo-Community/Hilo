use super::protocol::{AcpEvent, ProposedEdit, MAX_SESSION_ITEMS, MAX_TEXT_BYTES};
use crate::{
    broker::{ExecutionError, ExecutionReport, ToolExecutor},
    budget::BudgetUsage,
    IntentOutcome,
};
use host_core::{TaskSpec, ToolArgValue, ToolCall};
use sandbox_runner::{
    ChildOutput, PathGuard, ResourceLimits, SandboxRunner, SandboxSpec, SandboxedChild,
};
use std::{
    collections::BTreeMap,
    io::Read,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// A trusted Host adapter. Open documents MUST come from host-core's Rope, including unsaved text.
pub trait WorkspaceText: Send + Sync {
    fn text(&self, relative_path: &str) -> Result<String, String>;
    fn query_local_mcp(&self, _request: &str) -> Result<String, String> {
        Err("No brokered local MCP adapter is configured".into())
    }
}

pub(crate) type Children = Arc<Mutex<Vec<Arc<Mutex<SandboxedChild>>>>>;
pub type EventSink = Arc<dyn Fn(AcpEvent) + Send + Sync>;

/// This executor's write schema stores proposals only. It has no document/disk write API.
pub struct AgentToolExecutor {
    root: PathBuf,
    reader: Arc<dyn WorkspaceText>,
    runner: Arc<dyn SandboxRunner>,
    limits: ResourceLimits,
    pub(crate) children: Children,
    pub(crate) cancelled: Arc<AtomicBool>,
    proposals: Mutex<BTreeMap<String, ProposedEdit>>,
    sink: EventSink,
}

impl AgentToolExecutor {
    pub fn new(
        root: PathBuf,
        reader: Arc<dyn WorkspaceText>,
        runner: Arc<dyn SandboxRunner>,
        limits: ResourceLimits,
        sink: EventSink,
    ) -> Self {
        Self {
            root,
            reader,
            runner,
            limits,
            children: Arc::new(Mutex::new(Vec::new())),
            cancelled: Arc::new(AtomicBool::new(false)),
            proposals: Mutex::new(BTreeMap::new()),
            sink,
        }
    }

    pub fn proposed_edits(&self) -> Vec<ProposedEdit> {
        self.proposals
            .lock()
            .expect("proposal lock")
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn stop(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        for child in self.children.lock().expect("children lock").iter() {
            child.lock().expect("child lock").kill();
        }
    }

    fn text_arg<'a>(&self, call: &'a ToolCall, key: &str) -> Result<&'a str, ExecutionError> {
        match call.validated_args.get(key) {
            Some(ToolArgValue::Text(text)) => Ok(text),
            _ => Err(ExecutionError::NotStarted("missing text argument".into())),
        }
    }

    fn base(&self, spec: &TaskSpec, call: &ToolCall) -> Result<(String, String), ExecutionError> {
        let path = self.text_arg(call, "path")?;
        let resolved = PathGuard::verify_within_worktree(&self.root, &self.root.join(path))
            .map_err(|e| ExecutionError::NotStarted(e.to_string()))?;
        let relative = resolved
            .strip_prefix(&self.root)
            .map_err(|e| ExecutionError::NotStarted(e.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        if !crate::policy::paths::permitted(
            &relative,
            &spec.policy.allowed_paths,
            &spec.policy.forbidden_paths,
        ) {
            return Err(ExecutionError::NotStarted(
                "resolved path is forbidden by policy".into(),
            ));
        }
        let text = self.reader.text(path).map_err(ExecutionError::NotStarted)?;
        if text.len() > MAX_TEXT_BYTES {
            return Err(ExecutionError::NotStarted("file size limit".into()));
        }
        Ok((path.to_string(), text))
    }

    fn command(&self, call: &ToolCall) -> Result<String, ExecutionError> {
        let mut spec = SandboxSpec::new(&self.root, self.text_arg(call, "program")?);
        spec.cwd = self.root.join(self.text_arg(call, "cwd")?);
        spec.args = match call.validated_args.get("args") {
            Some(ToolArgValue::StringList(args)) => args.iter().map(Into::into).collect(),
            _ => return Err(ExecutionError::NotStarted("missing args".into())),
        };
        // Until LOT-015 applies changes in a checkpoint, terminal tools cannot write source either.
        spec.worktree_read_only = true;
        spec.limits = self.limits;
        let child;
        let stdout;
        let stderr;
        {
            // Serialize the cancellation check, spawn and registration: stop cannot miss a child.
            let mut children = self.children.lock().expect("children lock");
            if self.cancelled.load(Ordering::SeqCst) {
                return Err(ExecutionError::NotStarted("session cancelled".into()));
            }
            let mut process = self.runner.spawn(&spec).map_err(spawn_error)?;
            stdout = process.take_stdout();
            stderr = process.take_stderr();
            child = Arc::new(Mutex::new(process));
            children.push(child.clone());
        }
        let read = |pipe: Option<ChildOutput>, child: Arc<Mutex<SandboxedChild>>| {
            thread::spawn(move || {
                let mut bytes = Vec::new();
                if let Some(pipe) = pipe {
                    pipe.take((MAX_TEXT_BYTES + 1) as u64)
                        .read_to_end(&mut bytes)?;
                }
                if bytes.len() > MAX_TEXT_BYTES {
                    child.lock().expect("child lock").kill();
                }
                Ok::<_, std::io::Error>(bytes)
            })
        };
        let out = read(stdout, child.clone());
        let err = read(stderr, child.clone());
        let status = loop {
            let status = child
                .lock()
                .expect("child lock")
                .try_wait()
                .map_err(|e| ExecutionError::Invalid(e.to_string()))?;
            if let Some(status) = status {
                break status;
            }
            thread::sleep(Duration::from_millis(5));
        };
        let output = out
            .join()
            .map_err(|_| ExecutionError::Invalid("stdout reader panicked".into()))?
            .map_err(|e| ExecutionError::Invalid(e.to_string()))?;
        let errors = err
            .join()
            .map_err(|_| ExecutionError::Invalid("stderr reader panicked".into()))?
            .map_err(|e| ExecutionError::Invalid(e.to_string()))?;
        self.children
            .lock()
            .expect("children lock")
            .retain(|c| !Arc::ptr_eq(c, &child));
        if output.len() + errors.len() > MAX_TEXT_BYTES {
            return Err(ExecutionError::Invalid("terminal output limit".into()));
        }
        Ok(serde_json::json!({"output": format!("{}{}", String::from_utf8_lossy(&output), String::from_utf8_lossy(&errors)), "truncated": false, "exitStatus": {"exitCode": status.code()}}).to_string())
    }
}

/// Watchdog/I/O failures may occur after a child started: keep their durable intent unknown.
pub(crate) fn spawn_error(error: sandbox_runner::SandboxError) -> ExecutionError {
    use sandbox_runner::SandboxError::*;
    match error {
        SandboxUnavailable { .. }
        | InvalidSpec(_)
        | Path(_)
        | LimitNotEnforceable { .. }
        | NetworkPolicyNotEnforceable { .. }
        | Spawn(_) => ExecutionError::NotStarted(error.to_string()),
        other => ExecutionError::Sandbox(other),
    }
}

impl ToolExecutor for AgentToolExecutor {
    fn estimate(&self, _spec: &TaskSpec, call: &ToolCall) -> Result<BudgetUsage, ExecutionError> {
        let cpu_ms = if call.tool_name == "run_sandbox_command" {
            self.limits
                .max_cpu_time
                .ok_or_else(|| ExecutionError::NotStarted("Host must set a CPU ceiling".into()))?
                .as_secs()
                .saturating_add(1)
                .saturating_mul(1000)
        } else {
            0
        };
        Ok(BudgetUsage {
            tool_calls: 1,
            cpu_ms,
            ..BudgetUsage::default()
        })
    }

    fn execute(
        &self,
        spec: &TaskSpec,
        call: &ToolCall,
        _reserved: BudgetUsage,
    ) -> Result<ExecutionReport, ExecutionError> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(ExecutionError::NotStarted("session cancelled".into()));
        }
        let summary = match call.tool_name.as_str() {
            "read_workspace" => self.base(spec, call)?.1,
            "write_worktree" => {
                let (path, old_text) = self.base(spec, call)?;
                let new_text = self.text_arg(call, "content")?.to_string();
                if new_text.len() > MAX_TEXT_BYTES {
                    return Err(ExecutionError::NotStarted("proposal size limit".into()));
                }
                let edit = ProposedEdit {
                    id: call.call_id.clone(),
                    path,
                    base_content_hash: blake3::hash(old_text.as_bytes()).to_hex().to_string(),
                    old_text,
                    new_text,
                };
                let mut proposals = self.proposals.lock().expect("proposal lock");
                if proposals.len() >= MAX_SESSION_ITEMS {
                    return Err(ExecutionError::NotStarted("proposal count limit".into()));
                }
                proposals.insert(call.idempotency_key.clone(), edit.clone());
                (self.sink)(AcpEvent::ProposedEdit { edit });
                "Proposal recorded for review; no file was written. Applying edits requires LOT-015.".into()
            }
            "run_sandbox_command" => self.command(call)?,
            "query_local_mcp" => {
                let result = self
                    .reader
                    .query_local_mcp(self.text_arg(call, "request")?)
                    .map_err(ExecutionError::NotStarted)?;
                if result.len() > MAX_TEXT_BYTES {
                    return Err(ExecutionError::NotStarted("MCP output limit".into()));
                }
                result
            }
            _ => return Err(ExecutionError::NotStarted("unsupported ACP tool".into())),
        };
        Ok(ExecutionReport {
            outcome: IntentOutcome {
                succeeded: true,
                summary,
            },
            actual: self.estimate(spec, call)?,
            failure_signature: None,
        })
    }
}
