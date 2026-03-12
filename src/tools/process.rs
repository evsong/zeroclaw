use super::process_sessions::{
    ProcessExitState, ProcessPollSnapshot, ProcessSessionRegistry, ProcessSessionSnapshot,
};
use super::traits::{Tool, ToolExecutionContext, ToolResult};
use crate::runtime::RuntimeAdapter;
use crate::security::policy::ToolOperation;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::process::Stdio;
use std::sync::Arc;

const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];

pub struct ProcessTool {
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
    registry: Arc<ProcessSessionRegistry>,
}

impl ProcessTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        runtime: Arc<dyn RuntimeAdapter>,
        registry: Arc<ProcessSessionRegistry>,
    ) -> Self {
        Self {
            security,
            runtime,
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
            .ok_or_else(|| anyhow::anyhow!("process tool requires a conversation context"))
    }

    fn parse_session_id<'a>(&self, args: &'a Value) -> anyhow::Result<&'a str> {
        args.get("session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'session_id' parameter"))
    }

    fn parse_offset(args: &Value, field: &str) -> anyhow::Result<usize> {
        let Some(value) = args.get(field) else {
            return Ok(0);
        };

        let raw = value
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("'{field}' must be an integer >= 0"))?;
        usize::try_from(raw).map_err(|_| anyhow::anyhow!("'{field}' is too large"))
    }

    fn summarize_command(command: &str) -> String {
        const MAX_LEN: usize = 120;
        let trimmed = command.trim();
        if trimmed.chars().count() <= MAX_LEN {
            return trimmed.to_string();
        }

        let mut out = String::with_capacity(MAX_LEN + 1);
        for (idx, ch) in trimmed.chars().enumerate() {
            if idx >= MAX_LEN.saturating_sub(1) {
                break;
            }
            out.push(ch);
        }
        out.push('…');
        out
    }

    fn format_system_time(value: std::time::SystemTime) -> String {
        DateTime::<Utc>::from(value).to_rfc3339()
    }

    fn exit_state_json(state: &ProcessExitState) -> Value {
        match state {
            ProcessExitState::Running => json!({
                "state": "running",
            }),
            ProcessExitState::Exited { success, code } => json!({
                "state": "exited",
                "success": success,
                "code": code,
            }),
            ProcessExitState::Failed { reason } => json!({
                "state": "failed",
                "reason": reason,
            }),
        }
    }

    fn snapshot_json(snapshot: &ProcessSessionSnapshot) -> Value {
        json!({
            "session_id": snapshot.session_id,
            "owner_key": snapshot.owner_key,
            "command_summary": snapshot.command_summary,
            "started_at": Self::format_system_time(snapshot.started_at),
            "last_activity_at": Self::format_system_time(snapshot.last_activity_at),
            "completed_at": snapshot.completed_at.map(Self::format_system_time),
            "exit_state": Self::exit_state_json(&snapshot.exit_state),
            "stdout": {
                "content": snapshot.stdout.content,
                "truncated": snapshot.stdout.truncated,
                "total_bytes": snapshot.stdout.total_bytes,
            },
            "stderr": {
                "content": snapshot.stderr.content,
                "truncated": snapshot.stderr.truncated,
                "total_bytes": snapshot.stderr.total_bytes,
            },
            "has_stdin": snapshot.has_stdin,
        })
    }

    fn poll_json(snapshot: &ProcessPollSnapshot) -> Value {
        json!({
            "session_id": snapshot.session_id,
            "exit_state": Self::exit_state_json(&snapshot.exit_state),
            "stdout": {
                "content": snapshot.stdout.content,
                "next_offset": snapshot.stdout.next_offset,
                "total_bytes": snapshot.stdout.total_bytes,
                "missed_output": snapshot.stdout.missed_output,
                "truncated": snapshot.stdout.truncated,
            },
            "stderr": {
                "content": snapshot.stderr.content,
                "next_offset": snapshot.stderr.next_offset,
                "total_bytes": snapshot.stderr.total_bytes,
                "missed_output": snapshot.stderr.missed_output,
                "truncated": snapshot.stderr.truncated,
            },
        })
    }

    fn is_valid_env_var_name(name: &str) -> bool {
        let mut chars = name.chars();
        match chars.next() {
            Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
            _ => return false,
        }
        chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    }

    fn collect_allowed_shell_env_vars(security: &SecurityPolicy) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for key in SAFE_ENV_VARS
            .iter()
            .copied()
            .chain(security.shell_env_passthrough.iter().map(|s| s.as_str()))
        {
            let candidate = key.trim();
            if candidate.is_empty() || !Self::is_valid_env_var_name(candidate) {
                continue;
            }
            if seen.insert(candidate.to_string()) {
                out.push(candidate.to_string());
            }
        }
        out
    }

    fn prepare_child_command_env(
        &self,
        command: &mut tokio::process::Command,
    ) -> anyhow::Result<()> {
        command.env_clear();
        for var in Self::collect_allowed_shell_env_vars(&self.security) {
            if let Ok(val) = std::env::var(&var) {
                command.env(&var, val);
            }
        }
        Ok(())
    }

    async fn handle_start(
        &self,
        args: Value,
        context: Option<ToolExecutionContext>,
    ) -> anyhow::Result<ToolResult> {
        let owner_key = self.owner_key(context.as_ref())?;
        if let Err(error) = self.registry.prepare_for_new_session().await {
            return Ok(Self::tool_error(error.to_string()));
        }
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;
        let approved = args
            .get("approved")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if !self.runtime.supports_long_running() {
            return Ok(Self::tool_error(format!(
                "Runtime '{}' does not support long-running process sessions",
                self.runtime.name()
            )));
        }

        if self.security.is_rate_limited() {
            return Ok(Self::tool_error(
                "Rate limit exceeded: too many actions in the last hour",
            ));
        }

        if let Err(reason) = self.security.validate_command_execution(command, approved) {
            return Ok(Self::tool_error(reason));
        }

        if let Some(path) = self.security.forbidden_path_argument(command) {
            return Ok(Self::tool_error(format!(
                "Path blocked by security policy: {path}"
            )));
        }

        if !self.security.record_action() {
            return Ok(Self::tool_error(
                "Rate limit exceeded: action budget exhausted",
            ));
        }

        let mut child_command = match self
            .runtime
            .build_shell_command(command, &self.security.workspace_dir)
        {
            Ok(command_builder) => command_builder,
            Err(error) => {
                return Ok(Self::tool_error(format!(
                    "Failed to build runtime command: {error}"
                )));
            }
        };
        self.prepare_child_command_env(&mut child_command)?;
        child_command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = match child_command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Ok(Self::tool_error(format!(
                    "Failed to spawn background process: {error}"
                )));
            }
        };

        let session = self.registry.register_child(
            owner_key.to_string(),
            Self::summarize_command(command),
            child,
        );
        let snapshot = session.snapshot().await;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "action": "start",
                "session": Self::snapshot_json(&snapshot),
            }))?,
            error: None,
        })
    }

    async fn handle_status(
        &self,
        args: Value,
        context: Option<ToolExecutionContext>,
    ) -> anyhow::Result<ToolResult> {
        let owner_key = self.owner_key(context.as_ref())?;
        let _ = self.registry.cleanup_expired().await;
        let session_id = self.parse_session_id(&args)?;
        let session = self.registry.get_owned(owner_key, session_id)?;
        let _ = session.refresh_exit_state().await;
        let snapshot = session.snapshot().await;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "action": "status",
                "session": {
                    "session_id": snapshot.session_id,
                    "owner_key": snapshot.owner_key,
                    "command_summary": snapshot.command_summary,
                    "started_at": Self::format_system_time(snapshot.started_at),
                    "last_activity_at": Self::format_system_time(snapshot.last_activity_at),
                    "completed_at": snapshot.completed_at.map(Self::format_system_time),
                    "exit_state": Self::exit_state_json(&snapshot.exit_state),
                    "stdout_total_bytes": snapshot.stdout.total_bytes,
                    "stderr_total_bytes": snapshot.stderr.total_bytes,
                    "has_stdin": snapshot.has_stdin,
                }
            }))?,
            error: None,
        })
    }

    async fn handle_log(
        &self,
        args: Value,
        context: Option<ToolExecutionContext>,
    ) -> anyhow::Result<ToolResult> {
        let owner_key = self.owner_key(context.as_ref())?;
        let _ = self.registry.cleanup_expired().await;
        let session_id = self.parse_session_id(&args)?;
        let session = self.registry.get_owned(owner_key, session_id)?;
        let _ = session.refresh_exit_state().await;
        let snapshot = session.snapshot().await;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "action": "log",
                "session": Self::snapshot_json(&snapshot),
            }))?,
            error: None,
        })
    }

    async fn handle_poll(
        &self,
        args: Value,
        context: Option<ToolExecutionContext>,
    ) -> anyhow::Result<ToolResult> {
        let owner_key = self.owner_key(context.as_ref())?;
        let _ = self.registry.cleanup_expired().await;
        let session_id = self.parse_session_id(&args)?;
        let stdout_offset = Self::parse_offset(&args, "stdout_offset")?;
        let stderr_offset = Self::parse_offset(&args, "stderr_offset")?;
        let session = self.registry.get_owned(owner_key, session_id)?;
        let snapshot = session.poll_output(stdout_offset, stderr_offset).await?;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "action": "poll",
                "session": Self::poll_json(&snapshot),
            }))?,
            error: None,
        })
    }

    async fn handle_write(
        &self,
        args: Value,
        context: Option<ToolExecutionContext>,
    ) -> anyhow::Result<ToolResult> {
        let owner_key = self.owner_key(context.as_ref())?;
        let _ = self.registry.cleanup_expired().await;
        let session_id = self.parse_session_id(&args)?;
        let input = args.get("input").and_then(Value::as_str).unwrap_or("");
        let close_stdin = args
            .get("close_stdin")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if input.is_empty() && !close_stdin {
            return Ok(Self::tool_error(
                "write action requires non-empty 'input' or close_stdin=true",
            ));
        }

        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "process.write")
        {
            return Ok(Self::tool_error(error));
        }

        let session = self.registry.get_owned(owner_key, session_id)?;
        if !input.is_empty() {
            session.write_stdin(input.as_bytes()).await?;
        }
        if close_stdin {
            session.close_stdin().await;
        }
        let snapshot = session.snapshot().await;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "action": "write",
                "session_id": snapshot.session_id,
                "has_stdin": snapshot.has_stdin,
                "stdout_total_bytes": snapshot.stdout.total_bytes,
                "stderr_total_bytes": snapshot.stderr.total_bytes,
            }))?,
            error: None,
        })
    }

    async fn handle_kill(
        &self,
        args: Value,
        context: Option<ToolExecutionContext>,
    ) -> anyhow::Result<ToolResult> {
        let owner_key = self.owner_key(context.as_ref())?;
        let _ = self.registry.cleanup_expired().await;
        let session_id = self.parse_session_id(&args)?;
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "process.kill")
        {
            return Ok(Self::tool_error(error));
        }

        let session = self.registry.get_owned(owner_key, session_id)?;
        let state = session.kill().await?;
        let snapshot = session.snapshot().await;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "action": "kill",
                "session_id": session_id,
                "exit_state": Self::exit_state_json(&state),
                "completed_at": snapshot.completed_at.map(Self::format_system_time),
            }))?,
            error: None,
        })
    }
}

#[async_trait]
impl Tool for ProcessTool {
    fn name(&self) -> &str {
        "process"
    }

    fn description(&self) -> &str {
        "Manage background shell processes. Actions: start, status, poll, log, write, kill."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["start", "status", "poll", "log", "write", "kill"],
                    "description": "Action to perform"
                },
                "command": {
                    "type": "string",
                    "description": "Shell command to start in the workspace. Required for action=start."
                },
                "approved": {
                    "type": "boolean",
                    "description": "Set true to explicitly approve medium/high-risk commands in supervised mode",
                    "default": false
                },
                "session_id": {
                    "type": "string",
                    "description": "Existing process session id. Required for status/poll/log/write/kill."
                },
                "stdout_offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Byte cursor from a prior poll for stdout."
                },
                "stderr_offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Byte cursor from a prior poll for stderr."
                },
                "input": {
                    "type": "string",
                    "description": "Data to write to stdin for action=write."
                },
                "close_stdin": {
                    "type": "boolean",
                    "description": "Close stdin after an optional write for action=write.",
                    "default": false
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
            "start" => self.handle_start(args, context).await,
            "status" => self.handle_status(args, context).await,
            "poll" => self.handle_poll(args, context).await,
            "log" => self.handle_log(args, context).await,
            "write" => self.handle_write(args, context).await,
            "kill" => self.handle_kill(args, context).await,
            other => Ok(Self::tool_error(format!(
                "Unknown action '{other}'. Use start/status/poll/log/write/kill."
            ))),
        };

        Ok(match result {
            Ok(tool_result) => tool_result,
            Err(error) => Self::tool_error(error.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeAdapter;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;
    use tokio::time::{sleep, Duration};

    struct TestRuntime {
        supports_long_running: bool,
    }

    impl RuntimeAdapter for TestRuntime {
        fn name(&self) -> &str {
            "test-runtime"
        }

        fn has_shell_access(&self) -> bool {
            true
        }

        fn has_filesystem_access(&self) -> bool {
            true
        }

        fn storage_path(&self) -> PathBuf {
            std::env::temp_dir().join("zeroclaw-test-runtime")
        }

        fn supports_long_running(&self) -> bool {
            self.supports_long_running
        }

        fn build_shell_command(
            &self,
            command: &str,
            workspace_dir: &Path,
        ) -> anyhow::Result<tokio::process::Command> {
            let mut process = tokio::process::Command::new("sh");
            process.arg("-c").arg(command).current_dir(workspace_dir);
            Ok(process)
        }
    }

    fn test_security(workspace_dir: PathBuf) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir,
            workspace_only: true,
            allowed_commands: vec!["python3".into(), "sh".into()],
            max_actions_per_hour: 50,
            ..SecurityPolicy::default()
        })
    }

    fn test_context(owner: &str) -> ToolExecutionContext {
        ToolExecutionContext {
            conversation_key: Some(owner.into()),
            channel_name: Some("cli".into()),
            reply_target: Some("stdout".into()),
        }
    }

    fn parse_output(result: &ToolResult) -> Value {
        serde_json::from_str(&result.output).expect("tool output should be valid JSON")
    }

    #[tokio::test]
    async fn process_tool_supports_start_status_poll_write_and_kill() {
        let tmp = TempDir::new().unwrap();
        let tool = ProcessTool::new(
            test_security(tmp.path().to_path_buf()),
            Arc::new(TestRuntime {
                supports_long_running: true,
            }),
            Arc::new(ProcessSessionRegistry::default()),
        );
        let context = test_context("conversation-a");

        let start = tool
            .execute_with_context(
                json!({
                    "action": "start",
                    "command": "python3 -c 'import sys,time; print(\"ready\", flush=True); line = sys.stdin.readline().strip(); print(f\"got:{line}\", flush=True); time.sleep(30)'"
                }),
                Some(context.clone()),
            )
            .await
            .unwrap();
        assert!(start.success, "start should succeed: {:?}", start.error);
        let start_json = parse_output(&start);
        let session_id = start_json["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        sleep(Duration::from_millis(120)).await;
        let status = tool
            .execute_with_context(
                json!({
                    "action": "status",
                    "session_id": session_id,
                }),
                Some(context.clone()),
            )
            .await
            .unwrap();
        let status_json = parse_output(&status);
        assert_eq!(status_json["session"]["exit_state"]["state"], "running");
        assert!(
            status_json["session"]["stdout_total_bytes"]
                .as_u64()
                .unwrap()
                >= 6
        );

        let first_poll = tool
            .execute_with_context(
                json!({
                    "action": "poll",
                    "session_id": session_id,
                    "stdout_offset": 0,
                    "stderr_offset": 0,
                }),
                Some(context.clone()),
            )
            .await
            .unwrap();
        let first_poll_json = parse_output(&first_poll);
        assert_eq!(first_poll_json["session"]["stdout"]["content"], "ready\n");
        let stdout_offset = first_poll_json["session"]["stdout"]["next_offset"]
            .as_u64()
            .unwrap();

        let write = tool
            .execute_with_context(
                json!({
                    "action": "write",
                    "session_id": session_id,
                    "input": "ping\n",
                }),
                Some(context.clone()),
            )
            .await
            .unwrap();
        assert!(write.success, "write should succeed: {:?}", write.error);

        sleep(Duration::from_millis(150)).await;
        let second_poll = tool
            .execute_with_context(
                json!({
                    "action": "poll",
                    "session_id": session_id,
                    "stdout_offset": stdout_offset,
                    "stderr_offset": 0,
                }),
                Some(context.clone()),
            )
            .await
            .unwrap();
        let second_poll_json = parse_output(&second_poll);
        assert_eq!(
            second_poll_json["session"]["stdout"]["content"],
            "got:ping\n"
        );

        let kill = tool
            .execute_with_context(
                json!({
                    "action": "kill",
                    "session_id": session_id,
                }),
                Some(context.clone()),
            )
            .await
            .unwrap();
        assert!(kill.success, "kill should succeed: {:?}", kill.error);
        let kill_json = parse_output(&kill);
        assert_eq!(kill_json["exit_state"]["state"], "exited");
        assert_eq!(kill_json["exit_state"]["success"], false);
    }

    #[tokio::test]
    async fn process_tool_rejects_runtime_without_long_running_support() {
        let tmp = TempDir::new().unwrap();
        let tool = ProcessTool::new(
            test_security(tmp.path().to_path_buf()),
            Arc::new(TestRuntime {
                supports_long_running: false,
            }),
            Arc::new(ProcessSessionRegistry::default()),
        );

        let result = tool
            .execute_with_context(
                json!({
                    "action": "start",
                    "command": "python3 -c 'print(\"hello\")'"
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
            .contains("does not support long-running process sessions"));
    }

    #[tokio::test]
    async fn process_tool_denies_cross_conversation_access() {
        let tmp = TempDir::new().unwrap();
        let tool = ProcessTool::new(
            test_security(tmp.path().to_path_buf()),
            Arc::new(TestRuntime {
                supports_long_running: true,
            }),
            Arc::new(ProcessSessionRegistry::default()),
        );

        let start = tool
            .execute_with_context(
                json!({
                    "action": "start",
                    "command": "python3 -c 'import time; print(\"hello\", flush=True); time.sleep(30)'"
                }),
                Some(test_context("conversation-a")),
            )
            .await
            .unwrap();
        let start_json = parse_output(&start);
        let session_id = start_json["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        let denied = tool
            .execute_with_context(
                json!({
                    "action": "status",
                    "session_id": session_id,
                }),
                Some(test_context("conversation-b")),
            )
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(denied.error.as_deref().unwrap_or("").contains("not found"));
    }

    #[tokio::test]
    async fn process_tool_rejects_start_when_running_session_limit_is_reached() {
        let tmp = TempDir::new().unwrap();
        let tool = ProcessTool::new(
            test_security(tmp.path().to_path_buf()),
            Arc::new(TestRuntime {
                supports_long_running: true,
            }),
            Arc::new(ProcessSessionRegistry::with_limits(
                64 * 1024,
                32 * 1024,
                crate::tools::process_sessions::ProcessSessionRegistryLimits {
                    max_running_sessions: 1,
                    completed_retention: Duration::from_secs(60),
                    idle_retention: Duration::from_secs(60),
                },
            )),
        );
        let context = test_context("conversation-a");

        let first = tool
            .execute_with_context(
                json!({
                    "action": "start",
                    "command": "python3 -c 'import time; print(\"first\", flush=True); time.sleep(30)'"
                }),
                Some(context.clone()),
            )
            .await
            .unwrap();
        assert!(first.success);

        let second = tool
            .execute_with_context(
                json!({
                    "action": "start",
                    "command": "python3 -c 'import time; print(\"second\", flush=True); time.sleep(30)'"
                }),
                Some(context),
            )
            .await
            .unwrap();
        assert!(!second.success);
        assert!(second
            .error
            .as_deref()
            .unwrap_or("")
            .contains("capacity exceeded"));
    }
}
