use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex as AsyncMutex;

const DEFAULT_STDOUT_CAPACITY_BYTES: usize = 64 * 1024;
const DEFAULT_STDERR_CAPACITY_BYTES: usize = 32 * 1024;
const STREAM_READ_CHUNK_BYTES: usize = 4096;
const DEFAULT_MAX_RUNNING_SESSIONS: usize = 8;
const DEFAULT_COMPLETED_RETENTION_SECS: u64 = 15 * 60;
const DEFAULT_IDLE_RETENTION_SECS: u64 = 30 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSessionRegistryLimits {
    pub max_running_sessions: usize,
    pub completed_retention: Duration,
    pub idle_retention: Duration,
}

impl Default for ProcessSessionRegistryLimits {
    fn default() -> Self {
        Self {
            max_running_sessions: DEFAULT_MAX_RUNNING_SESSIONS,
            completed_retention: Duration::from_secs(DEFAULT_COMPLETED_RETENTION_SECS),
            idle_retention: Duration::from_secs(DEFAULT_IDLE_RETENTION_SECS),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSessionCleanupReport {
    pub removed_completed_sessions: usize,
    pub killed_idle_sessions: usize,
    pub remaining_running_sessions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessExitState {
    Running,
    Exited { success: bool, code: Option<i32> },
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutputSnapshot {
    pub content: String,
    pub truncated: bool,
    pub total_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutputDelta {
    pub content: String,
    pub next_offset: usize,
    pub total_bytes: usize,
    pub missed_output: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessPollSnapshot {
    pub session_id: String,
    pub exit_state: ProcessExitState,
    pub stdout: ProcessOutputDelta,
    pub stderr: ProcessOutputDelta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSessionSnapshot {
    pub session_id: String,
    pub owner_key: String,
    pub command_summary: String,
    pub started_at: SystemTime,
    pub last_activity_at: SystemTime,
    pub completed_at: Option<SystemTime>,
    pub exit_state: ProcessExitState,
    pub stdout: ProcessOutputSnapshot,
    pub stderr: ProcessOutputSnapshot,
    pub has_stdin: bool,
}

#[derive(Debug, Error)]
pub enum ProcessSessionError {
    #[error("process session '{session_id}' not found")]
    SessionNotFound { session_id: String },
    #[error(
        "process session capacity exceeded: {running_sessions} running sessions already active (limit {max_running_sessions})"
    )]
    SessionLimitExceeded {
        running_sessions: usize,
        max_running_sessions: usize,
    },
    #[error("failed to inspect process session '{session_id}': {reason}")]
    InspectFailed { session_id: String, reason: String },
    #[error("process session '{session_id}' does not expose stdin")]
    StdinUnavailable { session_id: String },
    #[error("failed to write to process session '{session_id}': {reason}")]
    StdinWriteFailed { session_id: String, reason: String },
    #[error("failed to kill process session '{session_id}': {reason}")]
    KillFailed { session_id: String, reason: String },
}

#[derive(Debug)]
struct BoundedOutputBuffer {
    max_bytes: usize,
    bytes: Vec<u8>,
    total_bytes: usize,
    truncated: bool,
}

impl BoundedOutputBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            bytes: Vec::new(),
            total_bytes: 0,
            truncated: false,
        }
    }

    fn append(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }

        self.total_bytes += chunk.len();
        self.bytes.extend_from_slice(chunk);

        if self.bytes.len() > self.max_bytes {
            let overflow = self.bytes.len() - self.max_bytes;
            self.bytes.drain(0..overflow);
            self.truncated = true;
        }
    }

    fn snapshot(&self) -> ProcessOutputSnapshot {
        ProcessOutputSnapshot {
            content: String::from_utf8_lossy(&self.bytes).into_owned(),
            truncated: self.truncated,
            total_bytes: self.total_bytes,
        }
    }

    fn delta_since(&self, offset: usize) -> ProcessOutputDelta {
        let earliest_offset = self.total_bytes.saturating_sub(self.bytes.len());
        let missed_output = offset < earliest_offset;
        let effective_offset = offset.max(earliest_offset).min(self.total_bytes);
        let start_idx = effective_offset.saturating_sub(earliest_offset);

        ProcessOutputDelta {
            content: String::from_utf8_lossy(&self.bytes[start_idx..]).into_owned(),
            next_offset: self.total_bytes,
            total_bytes: self.total_bytes,
            missed_output,
            truncated: self.truncated,
        }
    }
}

pub struct ProcessSession {
    session_id: String,
    owner_key: String,
    command_summary: String,
    started_at: SystemTime,
    last_activity_at: RwLock<SystemTime>,
    completed_at: RwLock<Option<SystemTime>>,
    exit_state: RwLock<ProcessExitState>,
    stdout: Mutex<BoundedOutputBuffer>,
    stderr: Mutex<BoundedOutputBuffer>,
    stdin: AsyncMutex<Option<ChildStdin>>,
    child: AsyncMutex<Option<Child>>,
}

impl ProcessSession {
    fn new(
        session_id: String,
        owner_key: String,
        command_summary: String,
        stdin: Option<ChildStdin>,
        child: Child,
        stdout_capacity_bytes: usize,
        stderr_capacity_bytes: usize,
    ) -> Self {
        Self {
            session_id,
            owner_key,
            command_summary,
            started_at: SystemTime::now(),
            last_activity_at: RwLock::new(SystemTime::now()),
            completed_at: RwLock::new(None),
            exit_state: RwLock::new(ProcessExitState::Running),
            stdout: Mutex::new(BoundedOutputBuffer::new(stdout_capacity_bytes)),
            stderr: Mutex::new(BoundedOutputBuffer::new(stderr_capacity_bytes)),
            stdin: AsyncMutex::new(stdin),
            child: AsyncMutex::new(Some(child)),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn owner_key(&self) -> &str {
        &self.owner_key
    }

    pub fn command_summary(&self) -> &str {
        &self.command_summary
    }

    pub async fn refresh_exit_state(&self) -> Result<ProcessExitState, ProcessSessionError> {
        let mut child_guard = self.child.lock().await;
        let Some(child) = child_guard.as_mut() else {
            return Ok(self.exit_state.read().clone());
        };

        match child.try_wait() {
            Ok(Some(status)) => {
                let state = ProcessExitState::Exited {
                    success: status.success(),
                    code: status.code(),
                };
                child_guard.take();
                self.mark_exit_state(state.clone());
                Ok(state)
            }
            Ok(None) => Ok(self.exit_state.read().clone()),
            Err(error) => {
                let state = ProcessExitState::Failed {
                    reason: error.to_string(),
                };
                child_guard.take();
                self.mark_exit_state(state.clone());
                Err(ProcessSessionError::InspectFailed {
                    session_id: self.session_id.clone(),
                    reason: error.to_string(),
                })
            }
        }
    }

    pub async fn write_stdin(&self, data: &[u8]) -> Result<(), ProcessSessionError> {
        let mut stdin_guard = self.stdin.lock().await;
        let Some(stdin) = stdin_guard.as_mut() else {
            return Err(ProcessSessionError::StdinUnavailable {
                session_id: self.session_id.clone(),
            });
        };

        stdin
            .write_all(data)
            .await
            .map_err(|error| ProcessSessionError::StdinWriteFailed {
                session_id: self.session_id.clone(),
                reason: error.to_string(),
            })?;
        stdin
            .flush()
            .await
            .map_err(|error| ProcessSessionError::StdinWriteFailed {
                session_id: self.session_id.clone(),
                reason: error.to_string(),
            })?;
        *self.last_activity_at.write() = SystemTime::now();
        Ok(())
    }

    pub async fn close_stdin(&self) {
        let mut stdin_guard = self.stdin.lock().await;
        stdin_guard.take();
        *self.last_activity_at.write() = SystemTime::now();
    }

    pub async fn has_stdin(&self) -> bool {
        self.stdin.lock().await.is_some()
    }

    pub async fn snapshot(&self) -> ProcessSessionSnapshot {
        let has_stdin = self.has_stdin().await;
        ProcessSessionSnapshot {
            session_id: self.session_id.clone(),
            owner_key: self.owner_key.clone(),
            command_summary: self.command_summary.clone(),
            started_at: self.started_at,
            last_activity_at: *self.last_activity_at.read(),
            completed_at: *self.completed_at.read(),
            exit_state: self.exit_state.read().clone(),
            stdout: self.stdout.lock().snapshot(),
            stderr: self.stderr.lock().snapshot(),
            has_stdin,
        }
    }

    pub async fn poll_output(
        &self,
        stdout_offset: usize,
        stderr_offset: usize,
    ) -> Result<ProcessPollSnapshot, ProcessSessionError> {
        let exit_state = self.refresh_exit_state().await?;
        Ok(ProcessPollSnapshot {
            session_id: self.session_id.clone(),
            exit_state,
            stdout: self.stdout.lock().delta_since(stdout_offset),
            stderr: self.stderr.lock().delta_since(stderr_offset),
        })
    }

    pub async fn kill(&self) -> Result<ProcessExitState, ProcessSessionError> {
        self.close_stdin().await;

        let mut child_guard = self.child.lock().await;
        let Some(mut child) = child_guard.take() else {
            return Ok(self.exit_state.read().clone());
        };

        let status = match child.try_wait() {
            Ok(Some(status)) => status,
            Ok(None) => {
                child
                    .kill()
                    .await
                    .map_err(|error| ProcessSessionError::KillFailed {
                        session_id: self.session_id.clone(),
                        reason: error.to_string(),
                    })?;
                child
                    .wait()
                    .await
                    .map_err(|error| ProcessSessionError::KillFailed {
                        session_id: self.session_id.clone(),
                        reason: error.to_string(),
                    })?
            }
            Err(error) => {
                let state = ProcessExitState::Failed {
                    reason: error.to_string(),
                };
                self.mark_exit_state(state.clone());
                return Err(ProcessSessionError::KillFailed {
                    session_id: self.session_id.clone(),
                    reason: error.to_string(),
                });
            }
        };

        let state = ProcessExitState::Exited {
            success: status.success(),
            code: status.code(),
        };
        self.mark_exit_state(state.clone());
        Ok(state)
    }

    fn append_stdout(&self, chunk: &[u8]) {
        self.stdout.lock().append(chunk);
        *self.last_activity_at.write() = SystemTime::now();
    }

    fn append_stderr(&self, chunk: &[u8]) {
        self.stderr.lock().append(chunk);
        *self.last_activity_at.write() = SystemTime::now();
    }

    fn mark_exit_state(&self, state: ProcessExitState) {
        let now = SystemTime::now();
        *self.exit_state.write() = state;
        *self.completed_at.write() = Some(now);
        *self.last_activity_at.write() = now;
    }

    fn is_running(&self) -> bool {
        matches!(*self.exit_state.read(), ProcessExitState::Running)
    }

    fn completed_at(&self) -> Option<SystemTime> {
        *self.completed_at.read()
    }

    fn last_activity_at(&self) -> SystemTime {
        *self.last_activity_at.read()
    }

    fn best_effort_shutdown_on_drop(&self) {
        if let Ok(mut stdin_guard) = self.stdin.try_lock() {
            stdin_guard.take();
        }

        if let Ok(mut child_guard) = self.child.try_lock() {
            if let Some(child) = child_guard.as_mut() {
                let _ = child.start_kill();
            }
        }
    }
}

pub struct ProcessSessionRegistry {
    next_id: AtomicU64,
    stdout_capacity_bytes: usize,
    stderr_capacity_bytes: usize,
    limits: ProcessSessionRegistryLimits,
    sessions: RwLock<HashMap<String, Arc<ProcessSession>>>,
}

impl Default for ProcessSessionRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_STDOUT_CAPACITY_BYTES, DEFAULT_STDERR_CAPACITY_BYTES)
    }
}

impl ProcessSessionRegistry {
    pub fn new(stdout_capacity_bytes: usize, stderr_capacity_bytes: usize) -> Self {
        Self::with_limits(
            stdout_capacity_bytes,
            stderr_capacity_bytes,
            ProcessSessionRegistryLimits::default(),
        )
    }

    pub fn with_limits(
        stdout_capacity_bytes: usize,
        stderr_capacity_bytes: usize,
        limits: ProcessSessionRegistryLimits,
    ) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            stdout_capacity_bytes,
            stderr_capacity_bytes,
            limits,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub fn register_child(
        &self,
        owner_key: impl Into<String>,
        command_summary: impl Into<String>,
        mut child: Child,
    ) -> Arc<ProcessSession> {
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();

        let session_id = format!("proc-{:06}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let session = Arc::new(ProcessSession::new(
            session_id.clone(),
            owner_key.into(),
            command_summary.into(),
            stdin,
            child,
            self.stdout_capacity_bytes,
            self.stderr_capacity_bytes,
        ));

        self.sessions
            .write()
            .insert(session_id, Arc::clone(&session));

        if let Some(stdout) = stdout {
            spawn_stream_task(Arc::clone(&session), stdout, StreamKind::Stdout);
        }
        if let Some(stderr) = stderr {
            spawn_stream_task(Arc::clone(&session), stderr, StreamKind::Stderr);
        }

        session
    }

    pub fn get(&self, session_id: &str) -> Option<Arc<ProcessSession>> {
        self.sessions.read().get(session_id).cloned()
    }

    pub fn get_owned(
        &self,
        owner_key: &str,
        session_id: &str,
    ) -> Result<Arc<ProcessSession>, ProcessSessionError> {
        let session = self
            .get(session_id)
            .ok_or_else(|| ProcessSessionError::SessionNotFound {
                session_id: session_id.to_string(),
            })?;
        if session.owner_key() != owner_key {
            return Err(ProcessSessionError::SessionNotFound {
                session_id: session_id.to_string(),
            });
        }
        Ok(session)
    }

    pub fn list_owned(&self, owner_key: &str) -> Vec<Arc<ProcessSession>> {
        self.sessions
            .read()
            .values()
            .filter(|session| session.owner_key() == owner_key)
            .cloned()
            .collect()
    }

    pub async fn prepare_for_new_session(
        &self,
    ) -> Result<ProcessSessionCleanupReport, ProcessSessionError> {
        let report = self.cleanup_expired().await;
        if report.remaining_running_sessions >= self.limits.max_running_sessions {
            return Err(ProcessSessionError::SessionLimitExceeded {
                running_sessions: report.remaining_running_sessions,
                max_running_sessions: self.limits.max_running_sessions,
            });
        }
        Ok(report)
    }

    pub async fn cleanup_expired(&self) -> ProcessSessionCleanupReport {
        let sessions: Vec<Arc<ProcessSession>> = self.sessions.read().values().cloned().collect();
        let now = SystemTime::now();
        let mut remove_ids = Vec::new();
        let mut removed_completed_sessions = 0usize;
        let mut killed_idle_sessions = 0usize;
        let mut remaining_running_sessions = 0usize;

        for session in sessions {
            let _ = session.refresh_exit_state().await;

            if let Some(completed_at) = session.completed_at() {
                if exceeded_retention(now, completed_at, self.limits.completed_retention) {
                    remove_ids.push(session.session_id().to_string());
                    removed_completed_sessions += 1;
                }
                continue;
            }

            if exceeded_retention(now, session.last_activity_at(), self.limits.idle_retention) {
                let _ = session.kill().await;
                remove_ids.push(session.session_id().to_string());
                killed_idle_sessions += 1;
            } else if session.is_running() {
                remaining_running_sessions += 1;
            }
        }

        if !remove_ids.is_empty() {
            let mut sessions = self.sessions.write();
            for session_id in remove_ids {
                sessions.remove(&session_id);
            }
        }

        ProcessSessionCleanupReport {
            removed_completed_sessions,
            killed_idle_sessions,
            remaining_running_sessions,
        }
    }

    pub async fn shutdown_all(&self) -> usize {
        let sessions: Vec<Arc<ProcessSession>> = self.sessions.read().values().cloned().collect();
        let count = sessions.len();

        for session in sessions {
            let _ = session.kill().await;
        }

        self.sessions.write().clear();
        count
    }
}

impl Drop for ProcessSessionRegistry {
    fn drop(&mut self) {
        for session in self.sessions.get_mut().values() {
            session.best_effort_shutdown_on_drop();
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

fn spawn_stream_task<R>(session: Arc<ProcessSession>, mut reader: R, stream_kind: StreamKind)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = [0u8; STREAM_READ_CHUNK_BYTES];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => match stream_kind {
                    StreamKind::Stdout => session.append_stdout(&buffer[..read]),
                    StreamKind::Stderr => session.append_stderr(&buffer[..read]),
                },
                Err(error) => {
                    let message = format!("\n[process stream read error: {}]\n", error);
                    match stream_kind {
                        StreamKind::Stdout => session.append_stdout(message.as_bytes()),
                        StreamKind::Stderr => session.append_stderr(message.as_bytes()),
                    }
                    break;
                }
            }
        }
    });
}

fn exceeded_retention(now: SystemTime, at: SystemTime, retention: Duration) -> bool {
    if retention.is_zero() {
        return true;
    }

    match now.duration_since(at) {
        Ok(elapsed) => elapsed >= retention,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::process::Command;
    use tokio::time::{sleep, Duration};

    async fn wait_for_exit(session: &ProcessSession) -> ProcessExitState {
        for _ in 0..100 {
            let state = session.refresh_exit_state().await.unwrap();
            if !matches!(state, ProcessExitState::Running) {
                return state;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("process session did not exit in time");
    }

    async fn wait_for_stdout_delta(
        session: &ProcessSession,
        stdout_offset: usize,
        expected: &str,
    ) -> ProcessPollSnapshot {
        for _ in 0..50 {
            let poll = session.poll_output(stdout_offset, 0).await.unwrap();
            if poll.stdout.content == expected {
                return poll;
            }
            sleep(Duration::from_millis(20)).await;
        }

        let snapshot = session.snapshot().await;
        panic!(
            "stdout delta did not reach expected content {:?}; latest stdout was {:?}",
            expected, snapshot.stdout.content
        );
    }

    #[tokio::test]
    async fn registry_assigns_stable_ids_and_owner_scoping() {
        let registry = ProcessSessionRegistry::default();

        let mut first = Command::new("sh");
        first.arg("-c").arg("printf first");
        first.stdout(Stdio::piped()).stderr(Stdio::piped());
        let first = registry.register_child(
            "conversation-a",
            "printf first",
            first.spawn().expect("spawn first child"),
        );

        let mut second = Command::new("sh");
        second.arg("-c").arg("printf second");
        second.stdout(Stdio::piped()).stderr(Stdio::piped());
        let second = registry.register_child(
            "conversation-b",
            "printf second",
            second.spawn().expect("spawn second child"),
        );

        assert_eq!(first.session_id(), "proc-000001");
        assert_eq!(second.session_id(), "proc-000002");
        assert_eq!(registry.list_owned("conversation-a").len(), 1);
        assert!(registry
            .get_owned("conversation-a", second.session_id())
            .is_err());
    }

    #[tokio::test]
    async fn registry_tracks_bounded_stdout_and_stderr_buffers() {
        let registry = ProcessSessionRegistry::new(12, 10);

        let mut command = Command::new("python3");
        command.arg("-c").arg(
            "import sys; sys.stdout.write('abcdefghijklmnop'); sys.stderr.write('uvwxyz012345')",
        );
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        let session = registry.register_child(
            "conversation-a",
            "python3 -c tail-test",
            command.spawn().expect("spawn output child"),
        );
        let state = wait_for_exit(&session).await;
        assert_eq!(
            state,
            ProcessExitState::Exited {
                success: true,
                code: Some(0)
            }
        );

        sleep(Duration::from_millis(50)).await;
        let snapshot = session.snapshot().await;
        assert_eq!(snapshot.stdout.content, "efghijklmnop");
        assert!(snapshot.stdout.truncated);
        assert_eq!(snapshot.stdout.total_bytes, 16);
        assert_eq!(snapshot.stderr.content, "wxyz012345");
        assert!(snapshot.stderr.truncated);
        assert_eq!(snapshot.stderr.total_bytes, 12);
        assert!(snapshot.completed_at.is_some());
    }

    #[tokio::test]
    async fn registry_preserves_stdin_handle_until_closed() {
        let registry = ProcessSessionRegistry::default();

        let mut command = Command::new("python3");
        command
            .arg("-c")
            .arg("import sys; data = sys.stdin.read(); sys.stdout.write(data.upper())");
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        let session = registry.register_child(
            "conversation-a",
            "python3 uppercase",
            command.spawn().expect("spawn stdin child"),
        );

        assert!(session.has_stdin().await);
        session.write_stdin(b"hello process").await.unwrap();
        session.close_stdin().await;
        assert!(!session.has_stdin().await);

        let state = wait_for_exit(&session).await;
        assert_eq!(
            state,
            ProcessExitState::Exited {
                success: true,
                code: Some(0)
            }
        );

        sleep(Duration::from_millis(50)).await;
        let snapshot = session.snapshot().await;
        assert_eq!(snapshot.stdout.content, "HELLO PROCESS");
    }

    #[tokio::test]
    async fn poll_output_returns_incremental_chunks_and_offsets() {
        let registry = ProcessSessionRegistry::default();

        let mut command = Command::new("python3");
        command.arg("-c").arg(
            "import sys,time; sys.stdout.write('one\\n'); sys.stdout.flush(); time.sleep(0.1); sys.stdout.write('two\\n'); sys.stdout.flush()",
        );
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        let session = registry.register_child(
            "conversation-a",
            "python3 incremental",
            command.spawn().expect("spawn incremental child"),
        );

        let first_poll = wait_for_stdout_delta(&session, 0, "one\n").await;
        assert_eq!(first_poll.stdout.content, "one\n");
        assert_eq!(first_poll.stdout.next_offset, 4);

        let second_poll =
            wait_for_stdout_delta(&session, first_poll.stdout.next_offset, "two\n").await;
        assert_eq!(second_poll.stdout.content, "two\n");
        assert_eq!(second_poll.stdout.next_offset, 8);
    }

    #[tokio::test]
    async fn cleanup_reaps_completed_and_idle_sessions() {
        let registry = ProcessSessionRegistry::with_limits(
            DEFAULT_STDOUT_CAPACITY_BYTES,
            DEFAULT_STDERR_CAPACITY_BYTES,
            ProcessSessionRegistryLimits {
                max_running_sessions: 4,
                completed_retention: Duration::from_millis(30),
                idle_retention: Duration::from_millis(80),
            },
        );

        let mut completed = Command::new("sh");
        completed.arg("-c").arg("printf done");
        completed.stdout(Stdio::piped()).stderr(Stdio::piped());
        let completed = registry.register_child(
            "conversation-a",
            "printf done",
            completed.spawn().expect("spawn completed child"),
        );
        let completed_id = completed.session_id().to_string();
        wait_for_exit(&completed).await;
        sleep(Duration::from_millis(40)).await;

        let mut idle = Command::new("python3");
        idle.arg("-c")
            .arg("import time; print('hi', flush=True); time.sleep(30)");
        idle.stdout(Stdio::piped()).stderr(Stdio::piped());
        let idle = registry.register_child(
            "conversation-a",
            "python3 idle",
            idle.spawn().expect("spawn idle child"),
        );
        let idle_id = idle.session_id().to_string();
        sleep(Duration::from_millis(120)).await;

        let report = registry.cleanup_expired().await;
        assert_eq!(report.removed_completed_sessions, 1);
        assert_eq!(report.killed_idle_sessions, 1);
        assert_eq!(report.remaining_running_sessions, 0);
        assert!(registry.get(&completed_id).is_none());
        assert!(registry.get(&idle_id).is_none());
    }

    #[tokio::test]
    async fn cleanup_enforces_running_session_limit_after_reaping_completed_sessions() {
        let registry = ProcessSessionRegistry::with_limits(
            DEFAULT_STDOUT_CAPACITY_BYTES,
            DEFAULT_STDERR_CAPACITY_BYTES,
            ProcessSessionRegistryLimits {
                max_running_sessions: 1,
                completed_retention: Duration::ZERO,
                idle_retention: Duration::from_secs(60),
            },
        );

        let mut command = Command::new("python3");
        command
            .arg("-c")
            .arg("import time; print('live', flush=True); time.sleep(30)");
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let session = registry.register_child(
            "conversation-a",
            "python3 live",
            command.spawn().expect("spawn live child"),
        );

        let error = registry.prepare_for_new_session().await.unwrap_err();
        assert!(matches!(
            error,
            ProcessSessionError::SessionLimitExceeded {
                running_sessions: 1,
                max_running_sessions: 1
            }
        ));

        session.kill().await.unwrap();
        let report = registry.prepare_for_new_session().await.unwrap();
        assert_eq!(report.removed_completed_sessions, 1);
        assert_eq!(report.remaining_running_sessions, 0);
    }

    #[tokio::test]
    async fn shutdown_all_terminates_sessions_and_clears_registry() {
        let registry = ProcessSessionRegistry::default();

        let mut first = Command::new("python3");
        first.arg("-c").arg("import time; time.sleep(30)");
        first.stdin(Stdio::piped());
        first.stdout(Stdio::piped()).stderr(Stdio::piped());
        registry.register_child(
            "conversation-a",
            "python3 sleep first",
            first.spawn().expect("spawn first sleeper"),
        );

        let mut second = Command::new("python3");
        second.arg("-c").arg("import time; time.sleep(30)");
        second.stdin(Stdio::piped());
        second.stdout(Stdio::piped()).stderr(Stdio::piped());
        registry.register_child(
            "conversation-b",
            "python3 sleep second",
            second.spawn().expect("spawn second sleeper"),
        );

        let terminated = registry.shutdown_all().await;
        assert_eq!(terminated, 2);
        assert!(registry.sessions.read().is_empty());
    }

    #[tokio::test]
    async fn kill_marks_process_as_exited() {
        let registry = ProcessSessionRegistry::default();

        let mut command = Command::new("python3");
        command.arg("-c").arg("import time; time.sleep(30)");
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        let session = registry.register_child(
            "conversation-a",
            "python3 sleep",
            command.spawn().expect("spawn sleep child"),
        );

        let state = session.kill().await.unwrap();
        assert!(matches!(
            state,
            ProcessExitState::Exited {
                success: false,
                code: _
            }
        ));
        let snapshot = session.snapshot().await;
        assert!(snapshot.completed_at.is_some());
    }

    #[tokio::test]
    async fn refresh_exit_state_reports_failed_wait_attempts() {
        let registry = ProcessSessionRegistry::default();

        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 7");
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        let session = registry.register_child(
            "conversation-a",
            "exit 7",
            command.spawn().expect("spawn failing child"),
        );
        let state = wait_for_exit(&session).await;
        assert_eq!(
            state,
            ProcessExitState::Exited {
                success: false,
                code: Some(7)
            }
        );
    }
}
