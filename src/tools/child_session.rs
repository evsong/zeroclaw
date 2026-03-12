use super::traits::{Tool, ToolExecutionContext, ToolResult};
use crate::config::DelegateAgentConfig;
use crate::security::policy::ToolOperation;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use thiserror::Error;
use tokio::sync::Notify;
use tokio::task::AbortHandle;

const DEFAULT_CHILD_TIMEOUT_SECS: u64 = 300;
const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 30;
const MAX_CHILD_TIMEOUT_SECS: u64 = 900;
const MAX_WAIT_TIMEOUT_SECS: u64 = 300;
const PROMPT_SUMMARY_LIMIT_CHARS: usize = 160;
const COMPLETION_SUMMARY_LIMIT_CHARS: usize = 1200;
const MAX_ARTIFACT_PATHS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildSessionState {
    Running,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSessionCompletion {
    pub success: bool,
    pub summary: String,
    pub error: Option<String>,
    pub artifacts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSessionSnapshot {
    pub session_id: String,
    pub owner_key: String,
    pub agent_name: String,
    pub provider: String,
    pub model: String,
    pub allowed_tools: Vec<String>,
    pub max_depth: u32,
    pub max_iterations: usize,
    pub timeout_secs: u64,
    pub prompt_summary: String,
    pub channel_name: Option<String>,
    pub reply_target: Option<String>,
    pub started_at: SystemTime,
    pub last_update_at: SystemTime,
    pub completed_at: Option<SystemTime>,
    pub state: ChildSessionState,
    pub completion: Option<ChildSessionCompletion>,
}

#[derive(Debug, Error)]
pub enum ChildSessionError {
    #[error("child session '{session_id}' not found")]
    SessionNotFound { session_id: String },
}

pub struct ChildSession {
    session_id: String,
    owner_key: String,
    agent_name: String,
    provider: String,
    model: String,
    allowed_tools: Vec<String>,
    max_depth: u32,
    max_iterations: usize,
    timeout_secs: u64,
    prompt_summary: String,
    channel_name: Option<String>,
    reply_target: Option<String>,
    started_at: SystemTime,
    last_update_at: RwLock<SystemTime>,
    completed_at: RwLock<Option<SystemTime>>,
    state: RwLock<ChildSessionState>,
    completion: RwLock<Option<ChildSessionCompletion>>,
    abort_handle: Mutex<Option<AbortHandle>>,
    notify: Notify,
}

impl ChildSession {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_id: String,
        owner_key: String,
        channel_name: Option<String>,
        reply_target: Option<String>,
        agent_name: String,
        agent_config: &DelegateAgentConfig,
        timeout_secs: u64,
        prompt_summary: String,
    ) -> Self {
        Self {
            session_id,
            owner_key,
            agent_name,
            provider: agent_config.provider.clone(),
            model: agent_config.model.clone(),
            allowed_tools: agent_config.allowed_tools.clone(),
            max_depth: agent_config.max_depth,
            max_iterations: agent_config.max_iterations,
            timeout_secs,
            prompt_summary,
            channel_name,
            reply_target,
            started_at: SystemTime::now(),
            last_update_at: RwLock::new(SystemTime::now()),
            completed_at: RwLock::new(None),
            state: RwLock::new(ChildSessionState::Running),
            completion: RwLock::new(None),
            abort_handle: Mutex::new(None),
            notify: Notify::new(),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn owner_key(&self) -> &str {
        &self.owner_key
    }

    pub fn snapshot(&self) -> ChildSessionSnapshot {
        ChildSessionSnapshot {
            session_id: self.session_id.clone(),
            owner_key: self.owner_key.clone(),
            agent_name: self.agent_name.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            allowed_tools: self.allowed_tools.clone(),
            max_depth: self.max_depth,
            max_iterations: self.max_iterations,
            timeout_secs: self.timeout_secs,
            prompt_summary: self.prompt_summary.clone(),
            channel_name: self.channel_name.clone(),
            reply_target: self.reply_target.clone(),
            started_at: self.started_at,
            last_update_at: *self.last_update_at.read(),
            completed_at: *self.completed_at.read(),
            state: self.state.read().clone(),
            completion: self.completion.read().clone(),
        }
    }

    pub async fn wait_for_completion(&self, timeout: Duration) -> ChildSessionSnapshot {
        if !matches!(*self.state.read(), ChildSessionState::Running) {
            return self.snapshot();
        }

        let _ = tokio::time::timeout(timeout, self.notify.notified()).await;
        self.snapshot()
    }

    pub fn set_abort_handle(&self, abort_handle: AbortHandle) {
        *self.abort_handle.lock() = Some(abort_handle);
    }

    pub fn cancel(&self, reason: &str) {
        if let Some(handle) = self.abort_handle.lock().take() {
            handle.abort();
        }
        self.mark_terminal(
            ChildSessionState::Cancelled,
            ChildSessionCompletion {
                success: false,
                summary: summarize_text(reason, COMPLETION_SUMMARY_LIMIT_CHARS),
                error: Some(reason.to_string()),
                artifacts: Vec::new(),
            },
        );
    }

    pub fn mark_completed(&self, result: ToolResult, workspace_dir: &std::path::Path) {
        let source = if result.success {
            result.output
        } else {
            result
                .error
                .clone()
                .unwrap_or_else(|| "child session failed".to_string())
        };
        let completion = ChildSessionCompletion {
            success: result.success,
            summary: summarize_text(&source, COMPLETION_SUMMARY_LIMIT_CHARS),
            error: result.error,
            artifacts: extract_workspace_paths(&source, workspace_dir),
        };
        let state = if completion.success {
            ChildSessionState::Completed
        } else {
            ChildSessionState::Failed
        };
        self.mark_terminal(state, completion);
    }

    pub fn mark_timeout(&self, timeout_secs: u64) {
        self.mark_terminal(
            ChildSessionState::TimedOut,
            ChildSessionCompletion {
                success: false,
                summary: format!("Child session timed out after {timeout_secs}s"),
                error: Some(format!("timed out after {timeout_secs}s")),
                artifacts: Vec::new(),
            },
        );
    }

    fn mark_terminal(&self, state: ChildSessionState, completion: ChildSessionCompletion) {
        let mut state_guard = self.state.write();
        if !matches!(*state_guard, ChildSessionState::Running) {
            return;
        }

        let now = SystemTime::now();
        *state_guard = state;
        *self.completion.write() = Some(completion);
        *self.completed_at.write() = Some(now);
        *self.last_update_at.write() = now;
        self.abort_handle.lock().take();
        self.notify.notify_waiters();
    }

    fn best_effort_cancel_on_drop(&self) {
        if let Some(handle) = self.abort_handle.lock().take() {
            handle.abort();
        }
    }
}

#[derive(Default)]
pub struct ChildSessionRegistry {
    next_id: AtomicU64,
    sessions: RwLock<HashMap<String, Arc<ChildSession>>>,
}

impl ChildSessionRegistry {
    pub fn register(
        &self,
        owner_key: impl Into<String>,
        context: Option<&ToolExecutionContext>,
        agent_name: &str,
        agent_config: &DelegateAgentConfig,
        timeout_secs: u64,
        prompt_summary: String,
    ) -> Arc<ChildSession> {
        let session_id = format!(
            "child-{:06}",
            self.next_id.fetch_add(1, Ordering::Relaxed) + 1
        );
        let session = Arc::new(ChildSession::new(
            session_id.clone(),
            owner_key.into(),
            context.and_then(|ctx| ctx.channel_name.clone()),
            context.and_then(|ctx| ctx.reply_target.clone()),
            agent_name.to_string(),
            agent_config,
            timeout_secs,
            prompt_summary,
        ));
        self.sessions
            .write()
            .insert(session_id, Arc::clone(&session));
        session
    }

    pub fn get_owned(
        &self,
        owner_key: &str,
        session_id: &str,
    ) -> Result<Arc<ChildSession>, ChildSessionError> {
        let session = self
            .sessions
            .read()
            .get(session_id)
            .cloned()
            .ok_or_else(|| ChildSessionError::SessionNotFound {
                session_id: session_id.to_string(),
            })?;
        if session.owner_key() != owner_key {
            return Err(ChildSessionError::SessionNotFound {
                session_id: session_id.to_string(),
            });
        }
        Ok(session)
    }
}

impl Drop for ChildSessionRegistry {
    fn drop(&mut self) {
        for session in self.sessions.get_mut().values() {
            session.best_effort_cancel_on_drop();
        }
    }
}

pub struct ChildSessionTool {
    agents: Arc<HashMap<String, DelegateAgentConfig>>,
    delegate: Arc<dyn Tool>,
    security: Arc<SecurityPolicy>,
    registry: Arc<ChildSessionRegistry>,
}

impl ChildSessionTool {
    pub fn new(
        agents: HashMap<String, DelegateAgentConfig>,
        delegate: Arc<dyn Tool>,
        security: Arc<SecurityPolicy>,
        registry: Arc<ChildSessionRegistry>,
    ) -> Self {
        Self {
            agents: Arc::new(agents),
            delegate,
            security,
            registry,
        }
    }

    fn tool_error(message: impl Into<String>) -> ToolResult {
        ToolResult {
            success: false,
            output: String::new(),
            error: Some(message.into()),
        }
    }

    fn owner_key<'a>(&self, context: Option<&'a ToolExecutionContext>) -> anyhow::Result<&'a str> {
        context
            .and_then(ToolExecutionContext::owner_key)
            .ok_or_else(|| anyhow::anyhow!("child_session requires a conversation context"))
    }

    fn parse_session_id<'a>(&self, args: &'a Value) -> anyhow::Result<&'a str> {
        args.get("session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'session_id' parameter"))
    }

    fn parse_bounded_secs(
        args: &Value,
        field: &str,
        default: u64,
        max: u64,
    ) -> anyhow::Result<u64> {
        let Some(value) = args.get(field) else {
            return Ok(default);
        };
        let parsed = value
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("'{field}' must be an integer >= 1"))?;
        if parsed == 0 || parsed > max {
            anyhow::bail!("'{field}' must be between 1 and {max}");
        }
        Ok(parsed)
    }

    fn validate_agent<'a>(&'a self, agent_name: &str) -> anyhow::Result<&'a DelegateAgentConfig> {
        let agent = self
            .agents
            .get(agent_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown agent '{agent_name}'"))?;
        if agent.max_depth == 0 {
            anyhow::bail!("Agent '{agent_name}' must have max_depth > 0");
        }
        if agent.max_iterations == 0 {
            anyhow::bail!("Agent '{agent_name}' must have max_iterations > 0");
        }
        if agent.agentic && agent.allowed_tools.is_empty() {
            anyhow::bail!(
                "Agent '{agent_name}' has agentic=true but no allowed_tools, so child sessions would be unbounded"
            );
        }
        Ok(agent)
    }

    fn session_json(snapshot: ChildSessionSnapshot) -> Value {
        json!({
            "session_id": snapshot.session_id,
            "owner_key": snapshot.owner_key,
            "agent_name": snapshot.agent_name,
            "provider": snapshot.provider,
            "model": snapshot.model,
            "allowed_tools": snapshot.allowed_tools,
            "max_depth": snapshot.max_depth,
            "max_iterations": snapshot.max_iterations,
            "timeout_secs": snapshot.timeout_secs,
            "prompt_summary": snapshot.prompt_summary,
            "channel_name": snapshot.channel_name,
            "reply_target": snapshot.reply_target,
            "started_at": format_system_time(snapshot.started_at),
            "last_update_at": format_system_time(snapshot.last_update_at),
            "completed_at": snapshot.completed_at.map(format_system_time),
            "state": child_state_name(&snapshot.state),
            "completion": snapshot.completion.map(|completion| {
                json!({
                    "success": completion.success,
                    "summary": completion.summary,
                    "error": completion.error,
                    "artifacts": completion.artifacts,
                })
            }),
        })
    }

    fn handle_spawn(
        &self,
        args: Value,
        context: Option<ToolExecutionContext>,
    ) -> anyhow::Result<ToolResult> {
        let owner_key = self.owner_key(context.as_ref())?;
        let agent_name = args
            .get("agent")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'agent' parameter"))?;
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'prompt' parameter"))?;
        let child_context_text = args
            .get("context")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let timeout_secs = Self::parse_bounded_secs(
            &args,
            "timeout_secs",
            DEFAULT_CHILD_TIMEOUT_SECS,
            MAX_CHILD_TIMEOUT_SECS,
        )?;
        let agent_config = self.validate_agent(agent_name)?;

        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "child_session.spawn")
        {
            return Ok(Self::tool_error(error));
        }

        let session = self.registry.register(
            owner_key.to_string(),
            context.as_ref(),
            agent_name,
            agent_config,
            timeout_secs,
            summarize_text(prompt, PROMPT_SUMMARY_LIMIT_CHARS),
        );
        let delegate = Arc::clone(&self.delegate);
        let workspace_dir = self.security.workspace_dir.clone();
        let delegate_args = json!({
            "agent": agent_name,
            "prompt": prompt,
            "context": child_context_text,
        });
        let session_for_task = Arc::clone(&session);
        let task_context = context.clone();
        let join = tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                delegate.execute_with_context(delegate_args, task_context),
            )
            .await;

            match result {
                Ok(Ok(tool_result)) => session_for_task.mark_completed(tool_result, &workspace_dir),
                Ok(Err(error)) => session_for_task.mark_completed(
                    ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(error.to_string()),
                    },
                    &workspace_dir,
                ),
                Err(_) => session_for_task.mark_timeout(timeout_secs),
            }
        });
        session.set_abort_handle(join.abort_handle());

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "action": "spawn",
                "session": Self::session_json(session.snapshot()),
            }))?,
            error: None,
        })
    }

    fn handle_status(
        &self,
        args: Value,
        context: Option<ToolExecutionContext>,
    ) -> anyhow::Result<ToolResult> {
        let owner_key = self.owner_key(context.as_ref())?;
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Read, "child_session.status")
        {
            return Ok(Self::tool_error(error));
        }

        let session = self
            .registry
            .get_owned(owner_key, self.parse_session_id(&args)?)?;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "action": "status",
                "session": Self::session_json(session.snapshot()),
            }))?,
            error: None,
        })
    }

    async fn handle_wait(
        &self,
        args: Value,
        context: Option<ToolExecutionContext>,
    ) -> anyhow::Result<ToolResult> {
        let owner_key = self.owner_key(context.as_ref())?;
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Read, "child_session.wait")
        {
            return Ok(Self::tool_error(error));
        }

        let wait_timeout = Self::parse_bounded_secs(
            &args,
            "wait_timeout_secs",
            DEFAULT_WAIT_TIMEOUT_SECS,
            MAX_WAIT_TIMEOUT_SECS,
        )?;
        let session = self
            .registry
            .get_owned(owner_key, self.parse_session_id(&args)?)?;
        let snapshot = session
            .wait_for_completion(Duration::from_secs(wait_timeout))
            .await;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "action": "wait",
                "session": Self::session_json(snapshot),
            }))?,
            error: None,
        })
    }

    fn handle_cancel(
        &self,
        args: Value,
        context: Option<ToolExecutionContext>,
    ) -> anyhow::Result<ToolResult> {
        let owner_key = self.owner_key(context.as_ref())?;
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "child_session.cancel")
        {
            return Ok(Self::tool_error(error));
        }

        let session = self
            .registry
            .get_owned(owner_key, self.parse_session_id(&args)?)?;
        session.cancel("Child session cancelled by parent request");
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "action": "cancel",
                "session": Self::session_json(session.snapshot()),
            }))?,
            error: None,
        })
    }
}

#[async_trait]
impl Tool for ChildSessionTool {
    fn name(&self) -> &str {
        "child_session"
    }

    fn description(&self) -> &str {
        "Spawn and manage bounded delegated child sessions for asynchronous work. Actions: spawn, status, wait, cancel."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["spawn", "status", "wait", "cancel"],
                    "description": "Action to perform"
                },
                "agent": {
                    "type": "string",
                    "description": "Delegate agent name. Required for action=spawn."
                },
                "prompt": {
                    "type": "string",
                    "description": "Task prompt for the child agent. Required for action=spawn."
                },
                "context": {
                    "type": "string",
                    "description": "Optional context to prepend for action=spawn."
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Per-child execution timeout in seconds for action=spawn."
                },
                "session_id": {
                    "type": "string",
                    "description": "Existing child session id. Required for status/wait/cancel."
                },
                "wait_timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "How long wait should block before returning the latest status."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        self.execute_with_context(args, None).await
    }

    async fn execute_with_context(
        &self,
        args: Value,
        context: Option<ToolExecutionContext>,
    ) -> anyhow::Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing 'action' parameter"))?;

        let result = match action {
            "spawn" => self.handle_spawn(args, context),
            "status" => self.handle_status(args, context),
            "wait" => self.handle_wait(args, context).await,
            "cancel" => self.handle_cancel(args, context),
            other => Ok(Self::tool_error(format!(
                "Unknown action '{other}'. Use spawn/status/wait/cancel."
            ))),
        };

        Ok(match result {
            Ok(tool_result) => tool_result,
            Err(error) => Self::tool_error(error.to_string()),
        })
    }
}

fn child_state_name(state: &ChildSessionState) -> &'static str {
    match state {
        ChildSessionState::Running => "running",
        ChildSessionState::Completed => "completed",
        ChildSessionState::Failed => "failed",
        ChildSessionState::TimedOut => "timed_out",
        ChildSessionState::Cancelled => "cancelled",
    }
}

fn format_system_time(value: SystemTime) -> String {
    DateTime::<Utc>::from(value).to_rfc3339()
}

fn summarize_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let mut out = String::with_capacity(max_chars + 1);
    for (index, ch) in trimmed.chars().enumerate() {
        if index >= max_chars.saturating_sub(1) {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

fn extract_workspace_paths(text: &str, workspace_dir: &std::path::Path) -> Vec<String> {
    let workspace = workspace_dir.to_string_lossy();
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for token in text.split_whitespace() {
        let candidate = token
            .trim_matches(|ch: char| {
                ch.is_ascii_whitespace()
                    || matches!(
                        ch,
                        '`' | '"' | '\'' | ',' | ':' | ';' | '(' | ')' | '[' | ']'
                    )
            })
            .trim_end_matches(['.', '!', '?']);
        if candidate.starts_with(workspace.as_ref()) && seen.insert(candidate.to_string()) {
            out.push(candidate.to_string());
            if out.len() >= MAX_ARTIFACT_PATHS {
                break;
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::time::sleep;

    #[derive(Clone)]
    struct StubDelegateTool {
        seen_contexts: Arc<Mutex<Vec<Option<ToolExecutionContext>>>>,
        artifact_path: Option<String>,
    }

    #[async_trait]
    impl Tool for StubDelegateTool {
        fn name(&self) -> &str {
            "delegate"
        }

        fn description(&self) -> &str {
            "stub delegate"
        }

        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
            self.execute_with_context(args, None).await
        }

        async fn execute_with_context(
            &self,
            args: Value,
            context: Option<ToolExecutionContext>,
        ) -> anyhow::Result<ToolResult> {
            self.seen_contexts.lock().push(context);
            let agent = args
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match agent {
                "worker" => {
                    sleep(Duration::from_millis(30)).await;
                    Ok(ToolResult {
                        success: true,
                        output: match self.artifact_path.as_ref() {
                            Some(path) => format!("child finished and wrote {path}"),
                            None => "child finished".to_string(),
                        },
                        error: None,
                    })
                }
                "slow" => {
                    sleep(Duration::from_millis(1200)).await;
                    Ok(ToolResult {
                        success: true,
                        output: "slow child finished".to_string(),
                        error: None,
                    })
                }
                "failing" => {
                    sleep(Duration::from_millis(20)).await;
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("delegate failure".to_string()),
                    })
                }
                _ => Ok(ToolResult {
                    success: true,
                    output: "ok".to_string(),
                    error: None,
                }),
            }
        }
    }

    fn test_security(workspace_dir: PathBuf) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir,
            workspace_only: true,
            ..SecurityPolicy::default()
        })
    }

    fn test_context(owner: &str) -> ToolExecutionContext {
        ToolExecutionContext {
            conversation_key: Some(owner.to_string()),
            channel_name: Some("feishu".to_string()),
            reply_target: Some("thread-1".to_string()),
        }
    }

    fn sample_agents() -> HashMap<String, DelegateAgentConfig> {
        let mut agents = HashMap::new();
        agents.insert(
            "worker".to_string(),
            DelegateAgentConfig {
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                system_prompt: None,
                api_key: None,
                temperature: Some(0.2),
                max_depth: 2,
                agentic: true,
                allowed_tools: vec!["echo_tool".to_string()],
                max_iterations: 4,
            },
        );
        agents.insert(
            "slow".to_string(),
            DelegateAgentConfig {
                provider: "test-provider".to_string(),
                model: "slow-model".to_string(),
                system_prompt: None,
                api_key: None,
                temperature: None,
                max_depth: 2,
                agentic: true,
                allowed_tools: vec!["echo_tool".to_string()],
                max_iterations: 4,
            },
        );
        agents.insert(
            "unbounded".to_string(),
            DelegateAgentConfig {
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                system_prompt: None,
                api_key: None,
                temperature: None,
                max_depth: 2,
                agentic: true,
                allowed_tools: Vec::new(),
                max_iterations: 4,
            },
        );
        agents
    }

    fn parse_output(result: &ToolResult) -> Value {
        serde_json::from_str(&result.output).expect("tool output should be valid JSON")
    }

    #[tokio::test]
    async fn child_session_spawn_and_wait_returns_completion_summary() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let stub = Arc::new(StubDelegateTool {
            seen_contexts: Arc::new(Mutex::new(Vec::new())),
            artifact_path: Some(workspace.join("out.txt").display().to_string()),
        });
        let tool = ChildSessionTool::new(
            sample_agents(),
            stub.clone(),
            test_security(workspace),
            Arc::new(ChildSessionRegistry::default()),
        );
        let context = test_context("conversation-a");

        let spawned = tool
            .execute_with_context(
                json!({
                    "action": "spawn",
                    "agent": "worker",
                    "prompt": "inspect the codebase and report back"
                }),
                Some(context.clone()),
            )
            .await
            .unwrap();
        assert!(spawned.success);
        let spawned_json = parse_output(&spawned);
        let session_id = spawned_json["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        let waited = tool
            .execute_with_context(
                json!({
                    "action": "wait",
                    "session_id": session_id,
                    "wait_timeout_secs": 1
                }),
                Some(context),
            )
            .await
            .unwrap();
        assert!(waited.success);
        let waited_json = parse_output(&waited);
        assert_eq!(waited_json["session"]["state"], "completed");
        assert!(waited_json["session"]["completion"]["summary"]
            .as_str()
            .unwrap()
            .contains("child finished"));
        assert!(waited_json["session"]["completion"]["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path.as_str().unwrap().ends_with("/out.txt")));
        assert_eq!(
            stub.seen_contexts.lock()[0]
                .as_ref()
                .and_then(ToolExecutionContext::owner_key),
            Some("conversation-a")
        );
    }

    #[tokio::test]
    async fn child_session_reports_timeout_handoff() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tool = ChildSessionTool::new(
            sample_agents(),
            Arc::new(StubDelegateTool {
                seen_contexts: Arc::new(Mutex::new(Vec::new())),
                artifact_path: None,
            }),
            test_security(workspace),
            Arc::new(ChildSessionRegistry::default()),
        );
        let context = test_context("conversation-a");

        let spawned = tool
            .execute_with_context(
                json!({
                    "action": "spawn",
                    "agent": "slow",
                    "prompt": "take your time",
                    "timeout_secs": 1
                }),
                Some(context.clone()),
            )
            .await
            .unwrap();
        let session_id = parse_output(&spawned)["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        let waited = tool
            .execute_with_context(
                json!({
                    "action": "wait",
                    "session_id": session_id,
                    "wait_timeout_secs": 2
                }),
                Some(context),
            )
            .await
            .unwrap();
        let waited_json = parse_output(&waited);
        assert_eq!(waited_json["session"]["state"], "timed_out");
        assert!(waited_json["session"]["completion"]["error"]
            .as_str()
            .unwrap()
            .contains("timed out"));
    }

    #[tokio::test]
    async fn child_session_rejects_unbounded_agent_configuration() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tool = ChildSessionTool::new(
            sample_agents(),
            Arc::new(StubDelegateTool {
                seen_contexts: Arc::new(Mutex::new(Vec::new())),
                artifact_path: None,
            }),
            test_security(workspace),
            Arc::new(ChildSessionRegistry::default()),
        );

        let result = tool
            .execute_with_context(
                json!({
                    "action": "spawn",
                    "agent": "unbounded",
                    "prompt": "this should fail"
                }),
                Some(test_context("conversation-a")),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("no allowed_tools"));
    }

    #[tokio::test]
    async fn child_session_denies_foreign_owner_access() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tool = ChildSessionTool::new(
            sample_agents(),
            Arc::new(StubDelegateTool {
                seen_contexts: Arc::new(Mutex::new(Vec::new())),
                artifact_path: None,
            }),
            test_security(workspace),
            Arc::new(ChildSessionRegistry::default()),
        );

        let spawned = tool
            .execute_with_context(
                json!({
                    "action": "spawn",
                    "agent": "worker",
                    "prompt": "run"
                }),
                Some(test_context("conversation-a")),
            )
            .await
            .unwrap();
        let session_id = parse_output(&spawned)["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        let foreign = tool
            .execute_with_context(
                json!({
                    "action": "status",
                    "session_id": session_id
                }),
                Some(test_context("conversation-b")),
            )
            .await
            .unwrap();
        assert!(!foreign.success);
        assert!(foreign.error.as_deref().unwrap_or("").contains("not found"));
    }
}
