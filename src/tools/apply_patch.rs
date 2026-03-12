use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::fs;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchDocument {
    pub operations: Vec<PatchOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOperation {
    Add(AddFileOperation),
    Delete(DeleteFileOperation),
    Update(UpdateFileOperation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddFileOperation {
    pub path: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteFileOperation {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFileOperation {
    pub path: String,
    pub move_to: Option<String>,
    pub hunks: Vec<PatchHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchHunk {
    pub header: Option<String>,
    pub lines: Vec<PatchHunkLine>,
    pub has_end_of_file_marker: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchHunkLine {
    pub kind: PatchLineKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchLineKind {
    Context,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PatchParseError {
    #[error("patch must start with '*** Begin Patch'")]
    MissingBegin,
    #[error("patch must end with '*** End Patch'")]
    MissingEnd,
    #[error("line {line_no}: unsupported patch operation '{operation}'")]
    UnsupportedOperation { line_no: usize, operation: String },
    #[error("line {line_no}: {message}")]
    InvalidLine { line_no: usize, message: String },
    #[error("line {line_no}: duplicate or conflicting patch target '{path}'")]
    ConflictingPath { line_no: usize, path: String },
}

pub struct ApplyPatchTool {
    security: Arc<SecurityPolicy>,
}

impl ApplyPatchTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchExecutionSummary {
    pub affected_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PatchExecutionError {
    #[error("action blocked: autonomy is read-only")]
    ReadOnly,
    #[error("rate limit exceeded: too many actions in the last hour")]
    RateLimited,
    #[error("rate limit exceeded: action budget exhausted")]
    ActionBudgetExhausted,
    #[error("path not allowed by security policy: {path}")]
    DisallowedPath { path: String },
    #[error("resolved path escapes workspace or allowed_roots: {path}")]
    ResolvedPathDenied { path: String },
    #[error("invalid patch target '{path}': {reason}")]
    InvalidPath { path: String, reason: String },
    #[error("refusing to modify through symlink: {path}")]
    SymlinkDenied { path: String },
    #[error("patch target must be a file: {path}")]
    NotAFile { path: String },
    #[error("patch target does not exist: {path}")]
    MissingTarget { path: String },
    #[error("add-file target already exists: {path}")]
    TargetExists { path: String },
    #[error("move destination already exists: {path}")]
    MoveDestinationExists { path: String },
    #[error("failed to read {path}: {reason}")]
    ReadFailed { path: String, reason: String },
    #[error("failed to resolve path {path}: {reason}")]
    ResolveFailed { path: String, reason: String },
    #[error("failed to create parent directories for {path}: {reason}")]
    CreateParentFailed { path: String, reason: String },
    #[error("failed to write {path}: {reason}")]
    WriteFailed { path: String, reason: String },
    #[error("failed to delete {path}: {reason}")]
    DeleteFailed { path: String, reason: String },
    #[error("patch hunk could not be applied to {path}: {reason}")]
    HunkApplyFailed { path: String, reason: String },
    #[error("patch hunk is ambiguous in {path}: {reason}")]
    HunkAmbiguous { path: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidatedPatchPath {
    requested_path: String,
    full_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlannedOperation {
    Add {
        path: ValidatedPatchPath,
        content: String,
    },
    Delete {
        path: ValidatedPatchPath,
        resolved_target: PathBuf,
    },
    Update {
        source_path: ValidatedPatchPath,
        destination_path: ValidatedPatchPath,
        resolved_source: PathBuf,
        content: String,
        remove_source_after_write: bool,
    },
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a structured multi-file patch with add, update, move, and delete operations"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "Structured patch document using *** Begin Patch / *** End Patch grammar"
                }
            },
            "required": ["patch"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let patch = args
            .get("patch")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'patch' parameter"))?;

        let result = execute_patch_text(self.security.as_ref(), patch).await;
        Ok(match result {
            Ok(summary) => ToolResult {
                success: true,
                output: format_patch_summary(&summary),
                error: None,
            },
            Err(error) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            },
        })
    }
}

pub fn parse_patch_document(input: &str) -> Result<PatchDocument, PatchParseError> {
    let lines: Vec<&str> = input.lines().collect();
    if lines.first().copied() != Some("*** Begin Patch") {
        return Err(PatchParseError::MissingBegin);
    }

    let mut operations = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();
    let mut index = 1usize;

    while index < lines.len() {
        let line = lines[index];
        if line == "*** End Patch" {
            if lines[index + 1..]
                .iter()
                .any(|line| !line.trim().is_empty())
            {
                return Err(PatchParseError::InvalidLine {
                    line_no: index + 2,
                    message: "unexpected trailing content after '*** End Patch'".into(),
                });
            }
            return Ok(PatchDocument { operations });
        }

        let (operation, next_index) = parse_operation(&lines, index)?;
        register_operation_paths(&operation, index + 1, &mut seen_paths)?;
        operations.push(operation);
        index = next_index;
    }

    Err(PatchParseError::MissingEnd)
}

fn parse_operation(
    lines: &[&str],
    start: usize,
) -> Result<(PatchOperation, usize), PatchParseError> {
    let line = lines[start];

    if let Some(path) = line.strip_prefix("*** Add File: ") {
        parse_add_file(lines, start, normalize_patch_path(path, start + 1)?)
    } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
        Ok((
            PatchOperation::Delete(DeleteFileOperation {
                path: normalize_patch_path(path, start + 1)?,
            }),
            start + 1,
        ))
    } else if let Some(path) = line.strip_prefix("*** Update File: ") {
        parse_update_file(lines, start, normalize_patch_path(path, start + 1)?)
    } else if let Some(operation) = line.strip_prefix("*** ").and_then(|value| {
        value
            .split_once(':')
            .map(|(prefix, _)| prefix)
            .or(Some(value))
    }) {
        Err(PatchParseError::UnsupportedOperation {
            line_no: start + 1,
            operation: operation.to_string(),
        })
    } else {
        Err(PatchParseError::InvalidLine {
            line_no: start + 1,
            message: "expected patch operation header".into(),
        })
    }
}

fn parse_add_file(
    lines: &[&str],
    start: usize,
    path: String,
) -> Result<(PatchOperation, usize), PatchParseError> {
    let mut index = start + 1;
    let mut added_lines = Vec::new();

    while index < lines.len() {
        let line = lines[index];
        if is_operation_boundary(line) {
            break;
        }

        let Some(content) = line.strip_prefix('+') else {
            return Err(PatchParseError::InvalidLine {
                line_no: index + 1,
                message: "add-file hunks must contain only '+' lines".into(),
            });
        };
        added_lines.push(content.to_string());
        index += 1;
    }

    if added_lines.is_empty() {
        return Err(PatchParseError::InvalidLine {
            line_no: start + 1,
            message: "add-file operation must contain at least one '+' line".into(),
        });
    }

    Ok((
        PatchOperation::Add(AddFileOperation {
            path,
            lines: added_lines,
        }),
        index,
    ))
}

fn parse_update_file(
    lines: &[&str],
    start: usize,
    path: String,
) -> Result<(PatchOperation, usize), PatchParseError> {
    let mut index = start + 1;
    let mut move_to = None;
    let mut hunks = Vec::new();
    let mut current_hunk: Option<(usize, PatchHunk)> = None;

    while index < lines.len() {
        let line = lines[index];
        if is_operation_boundary(line) {
            break;
        }

        if let Some(target) = line.strip_prefix("*** Move to: ") {
            if move_to.is_some() {
                return Err(PatchParseError::InvalidLine {
                    line_no: index + 1,
                    message: "update-file operation may contain only one '*** Move to:' header"
                        .into(),
                });
            }
            if current_hunk.is_some() || !hunks.is_empty() {
                return Err(PatchParseError::InvalidLine {
                    line_no: index + 1,
                    message: "'*** Move to:' must appear before any hunks".into(),
                });
            }
            let target = normalize_patch_path(target, index + 1)?;
            if target == path {
                return Err(PatchParseError::InvalidLine {
                    line_no: index + 1,
                    message: "move target must differ from source path".into(),
                });
            }
            move_to = Some(target);
            index += 1;
            continue;
        }

        if let Some(header) = line.strip_prefix("@@") {
            finalize_hunk(&mut current_hunk, &mut hunks, index)?;
            let header = header.strip_prefix(' ').unwrap_or(header).trim();
            current_hunk = Some((
                index + 1,
                PatchHunk {
                    header: (!header.is_empty()).then(|| header.to_string()),
                    lines: Vec::new(),
                    has_end_of_file_marker: false,
                },
            ));
            index += 1;
            continue;
        }

        if line == "*** End of File" {
            let Some((_, hunk)) = current_hunk.as_mut() else {
                return Err(PatchParseError::InvalidLine {
                    line_no: index + 1,
                    message: "'*** End of File' requires an active hunk".into(),
                });
            };
            if hunk.has_end_of_file_marker {
                return Err(PatchParseError::InvalidLine {
                    line_no: index + 1,
                    message: "duplicate '*** End of File' marker in the same hunk".into(),
                });
            }
            hunk.has_end_of_file_marker = true;
            index += 1;
            continue;
        }

        let Some(hunk_line) = parse_hunk_line(line) else {
            if let Some(operation) = line.strip_prefix("*** ").and_then(|value| {
                value
                    .split_once(':')
                    .map(|(prefix, _)| prefix)
                    .or(Some(value))
            }) {
                return Err(PatchParseError::UnsupportedOperation {
                    line_no: index + 1,
                    operation: operation.to_string(),
                });
            }
            return Err(PatchParseError::InvalidLine {
                line_no: index + 1,
                message: "update-file hunks must use '@@', ' ', '+', '-', or '*** End of File'"
                    .into(),
            });
        };

        let Some((_, hunk)) = current_hunk.as_mut() else {
            return Err(PatchParseError::InvalidLine {
                line_no: index + 1,
                message: "update-file operation requires a hunk header before patch lines".into(),
            });
        };
        hunk.lines.push(hunk_line);
        index += 1;
    }

    finalize_hunk(&mut current_hunk, &mut hunks, index)?;

    if hunks.is_empty() {
        return Err(PatchParseError::InvalidLine {
            line_no: start + 1,
            message: "update-file operation must contain at least one hunk".into(),
        });
    }

    Ok((
        PatchOperation::Update(UpdateFileOperation {
            path,
            move_to,
            hunks,
        }),
        index,
    ))
}

fn finalize_hunk(
    current_hunk: &mut Option<(usize, PatchHunk)>,
    hunks: &mut Vec<PatchHunk>,
    _boundary_index: usize,
) -> Result<(), PatchParseError> {
    let Some((hunk_line_no, hunk)) = current_hunk.take() else {
        return Ok(());
    };

    if hunk.lines.is_empty() {
        return Err(PatchParseError::InvalidLine {
            line_no: hunk_line_no,
            message: "patch hunks must contain at least one context/add/remove line".into(),
        });
    }

    hunks.push(hunk);
    Ok(())
}

fn parse_hunk_line(line: &str) -> Option<PatchHunkLine> {
    let (kind, text) = if let Some(text) = line.strip_prefix(' ') {
        (PatchLineKind::Context, text)
    } else if let Some(text) = line.strip_prefix('+') {
        (PatchLineKind::Add, text)
    } else if let Some(text) = line.strip_prefix('-') {
        (PatchLineKind::Remove, text)
    } else {
        return None;
    };

    Some(PatchHunkLine {
        kind,
        text: text.to_string(),
    })
}

fn normalize_patch_path(path: &str, line_no: usize) -> Result<String, PatchParseError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(PatchParseError::InvalidLine {
            line_no,
            message: "patch path must not be empty".into(),
        });
    }
    Ok(trimmed.to_string())
}

fn register_operation_paths(
    operation: &PatchOperation,
    line_no: usize,
    seen_paths: &mut std::collections::HashSet<String>,
) -> Result<(), PatchParseError> {
    let paths: Vec<&str> = match operation {
        PatchOperation::Add(op) => vec![op.path.as_str()],
        PatchOperation::Delete(op) => vec![op.path.as_str()],
        PatchOperation::Update(op) => {
            let mut values = vec![op.path.as_str()];
            if let Some(move_to) = op.move_to.as_deref() {
                values.push(move_to);
            }
            values
        }
    };

    for path in paths {
        if !seen_paths.insert(path.to_string()) {
            return Err(PatchParseError::ConflictingPath {
                line_no,
                path: path.to_string(),
            });
        }
    }

    Ok(())
}

fn is_operation_boundary(line: &str) -> bool {
    line == "*** End Patch"
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}

async fn execute_patch_text(
    security: &SecurityPolicy,
    patch: &str,
) -> Result<PatchExecutionSummary, String> {
    let document = parse_patch_document(patch).map_err(|error| error.to_string())?;
    execute_patch_document(security, &document)
        .await
        .map_err(|error| error.to_string())
}

async fn execute_patch_document(
    security: &SecurityPolicy,
    document: &PatchDocument,
) -> Result<PatchExecutionSummary, PatchExecutionError> {
    if !security.can_act() {
        return Err(PatchExecutionError::ReadOnly);
    }

    if security.is_rate_limited() {
        return Err(PatchExecutionError::RateLimited);
    }

    let plan = build_execution_plan(security, document).await?;

    if !security.record_action() {
        return Err(PatchExecutionError::ActionBudgetExhausted);
    }

    apply_execution_plan(security, plan).await
}

async fn build_execution_plan(
    security: &SecurityPolicy,
    document: &PatchDocument,
) -> Result<Vec<PlannedOperation>, PatchExecutionError> {
    let mut plan = Vec::with_capacity(document.operations.len());

    for operation in &document.operations {
        match operation {
            PatchOperation::Add(operation) => {
                let validated = validate_future_patch_target(security, &operation.path).await?;
                if path_exists(&validated.full_path).await? {
                    return Err(PatchExecutionError::TargetExists {
                        path: operation.path.clone(),
                    });
                }

                plan.push(PlannedOperation::Add {
                    path: validated,
                    content: operation.lines.join("\n"),
                });
            }
            PatchOperation::Delete(operation) => {
                let validated = validate_existing_patch_target(security, &operation.path).await?;
                let resolved_target = resolve_existing_file_target(security, &validated).await?;
                plan.push(PlannedOperation::Delete {
                    path: validated,
                    resolved_target,
                });
            }
            PatchOperation::Update(operation) => {
                let validated_source =
                    validate_existing_patch_target(security, &operation.path).await?;
                let resolved_source =
                    resolve_existing_file_target(security, &validated_source).await?;
                let source_content =
                    fs::read_to_string(&resolved_source)
                        .await
                        .map_err(|error| PatchExecutionError::ReadFailed {
                            path: operation.path.clone(),
                            reason: error.to_string(),
                        })?;
                let next_content =
                    apply_hunks_to_text(&source_content, &operation.hunks, &operation.path)?;

                let (destination_path, remove_source_after_write) =
                    if let Some(move_to) = operation.move_to.as_deref() {
                        let destination = validate_future_patch_target(security, move_to).await?;
                        if path_exists(&destination.full_path).await? {
                            return Err(PatchExecutionError::MoveDestinationExists {
                                path: move_to.to_string(),
                            });
                        }
                        (destination, true)
                    } else {
                        (validated_source.clone(), false)
                    };

                plan.push(PlannedOperation::Update {
                    source_path: validated_source,
                    destination_path,
                    resolved_source,
                    content: next_content,
                    remove_source_after_write,
                });
            }
        }
    }

    Ok(plan)
}

async fn apply_execution_plan(
    security: &SecurityPolicy,
    plan: Vec<PlannedOperation>,
) -> Result<PatchExecutionSummary, PatchExecutionError> {
    let mut affected_paths = Vec::with_capacity(plan.len());

    for operation in plan {
        match operation {
            PlannedOperation::Add { path, content } => {
                let resolved_target =
                    materialize_writable_target(security, &path.requested_path, &path.full_path)
                        .await?;
                fs::write(&resolved_target, content)
                    .await
                    .map_err(|error| PatchExecutionError::WriteFailed {
                        path: path.requested_path.clone(),
                        reason: error.to_string(),
                    })?;
                affected_paths.push(path.requested_path);
            }
            PlannedOperation::Delete {
                path,
                resolved_target,
            } => {
                fs::remove_file(&resolved_target).await.map_err(|error| {
                    PatchExecutionError::DeleteFailed {
                        path: path.requested_path.clone(),
                        reason: error.to_string(),
                    }
                })?;
                affected_paths.push(path.requested_path);
            }
            PlannedOperation::Update {
                source_path,
                destination_path,
                resolved_source,
                content,
                remove_source_after_write,
            } => {
                let resolved_destination = if remove_source_after_write {
                    materialize_writable_target(
                        security,
                        &destination_path.requested_path,
                        &destination_path.full_path,
                    )
                    .await?
                } else {
                    resolved_source.clone()
                };

                fs::write(&resolved_destination, content)
                    .await
                    .map_err(|error| PatchExecutionError::WriteFailed {
                        path: destination_path.requested_path.clone(),
                        reason: error.to_string(),
                    })?;

                if remove_source_after_write {
                    fs::remove_file(&resolved_source).await.map_err(|error| {
                        PatchExecutionError::DeleteFailed {
                            path: source_path.requested_path.clone(),
                            reason: error.to_string(),
                        }
                    })?;
                    affected_paths.push(format!(
                        "{} -> {}",
                        source_path.requested_path, destination_path.requested_path
                    ));
                } else {
                    affected_paths.push(destination_path.requested_path);
                }
            }
        }
    }

    Ok(PatchExecutionSummary { affected_paths })
}

async fn validate_existing_patch_target(
    security: &SecurityPolicy,
    path: &str,
) -> Result<ValidatedPatchPath, PatchExecutionError> {
    validate_requested_patch_path(security, path)?;
    let full_path = security.workspace_dir.join(path);
    let metadata = fs::symlink_metadata(&full_path)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => PatchExecutionError::MissingTarget {
                path: path.to_string(),
            },
            _ => PatchExecutionError::ResolveFailed {
                path: path.to_string(),
                reason: error.to_string(),
            },
        })?;

    if metadata.file_type().is_symlink() {
        return Err(PatchExecutionError::SymlinkDenied {
            path: path.to_string(),
        });
    }

    if !metadata.is_file() {
        return Err(PatchExecutionError::NotAFile {
            path: path.to_string(),
        });
    }

    let resolved =
        fs::canonicalize(&full_path)
            .await
            .map_err(|error| PatchExecutionError::ResolveFailed {
                path: path.to_string(),
                reason: error.to_string(),
            })?;
    if !security.is_resolved_path_allowed(&resolved) {
        return Err(PatchExecutionError::ResolvedPathDenied {
            path: resolved.display().to_string(),
        });
    }

    Ok(ValidatedPatchPath {
        requested_path: path.to_string(),
        full_path,
    })
}

async fn resolve_existing_file_target(
    security: &SecurityPolicy,
    path: &ValidatedPatchPath,
) -> Result<PathBuf, PatchExecutionError> {
    let resolved = fs::canonicalize(&path.full_path).await.map_err(|error| {
        PatchExecutionError::ResolveFailed {
            path: path.requested_path.clone(),
            reason: error.to_string(),
        }
    })?;
    if !security.is_resolved_path_allowed(&resolved) {
        return Err(PatchExecutionError::ResolvedPathDenied {
            path: resolved.display().to_string(),
        });
    }
    Ok(resolved)
}

async fn validate_future_patch_target(
    security: &SecurityPolicy,
    path: &str,
) -> Result<ValidatedPatchPath, PatchExecutionError> {
    validate_requested_patch_path(security, path)?;
    let full_path = security.workspace_dir.join(path);
    let parent = full_path
        .parent()
        .ok_or_else(|| PatchExecutionError::InvalidPath {
            path: path.to_string(),
            reason: "missing parent directory".into(),
        })?;

    validate_existing_ancestor_for_parent(security, path, parent).await?;

    if let Ok(metadata) = fs::symlink_metadata(&full_path).await {
        if metadata.file_type().is_symlink() {
            return Err(PatchExecutionError::SymlinkDenied {
                path: path.to_string(),
            });
        }
        if metadata.is_dir() {
            return Err(PatchExecutionError::NotAFile {
                path: path.to_string(),
            });
        }
    }

    Ok(ValidatedPatchPath {
        requested_path: path.to_string(),
        full_path,
    })
}

fn validate_requested_patch_path(
    security: &SecurityPolicy,
    path: &str,
) -> Result<(), PatchExecutionError> {
    if !security.is_path_allowed(path) {
        return Err(PatchExecutionError::DisallowedPath {
            path: path.to_string(),
        });
    }
    Ok(())
}

async fn validate_existing_ancestor_for_parent(
    security: &SecurityPolicy,
    requested_path: &str,
    parent: &Path,
) -> Result<(), PatchExecutionError> {
    let nearest_existing = nearest_existing_ancestor(parent).await.ok_or_else(|| {
        PatchExecutionError::InvalidPath {
            path: requested_path.to_string(),
            reason: "unable to find an existing ancestor for the target path".into(),
        }
    })?;

    let resolved_ancestor = fs::canonicalize(&nearest_existing).await.map_err(|error| {
        PatchExecutionError::ResolveFailed {
            path: requested_path.to_string(),
            reason: error.to_string(),
        }
    })?;
    if !security.is_resolved_path_allowed(&resolved_ancestor) {
        return Err(PatchExecutionError::ResolvedPathDenied {
            path: resolved_ancestor.display().to_string(),
        });
    }

    if let Ok(parent_meta) = fs::symlink_metadata(parent).await {
        if parent_meta.file_type().is_symlink() {
            return Err(PatchExecutionError::SymlinkDenied {
                path: requested_path.to_string(),
            });
        }
        let resolved_parent =
            fs::canonicalize(parent)
                .await
                .map_err(|error| PatchExecutionError::ResolveFailed {
                    path: requested_path.to_string(),
                    reason: error.to_string(),
                })?;
        if !security.is_resolved_path_allowed(&resolved_parent) {
            return Err(PatchExecutionError::ResolvedPathDenied {
                path: resolved_parent.display().to_string(),
            });
        }
    }

    Ok(())
}

async fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path.to_path_buf());

    while let Some(candidate) = current {
        match fs::metadata(&candidate).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                current = candidate.parent().map(Path::to_path_buf);
            }
            // Both Ok and non-NotFound errors return the candidate
            _ => return Some(candidate),
        }
    }

    None
}

async fn materialize_writable_target(
    security: &SecurityPolicy,
    requested_path: &str,
    full_path: &Path,
) -> Result<PathBuf, PatchExecutionError> {
    let parent = full_path
        .parent()
        .ok_or_else(|| PatchExecutionError::InvalidPath {
            path: requested_path.to_string(),
            reason: "missing parent directory".into(),
        })?;

    fs::create_dir_all(parent)
        .await
        .map_err(|error| PatchExecutionError::CreateParentFailed {
            path: requested_path.to_string(),
            reason: error.to_string(),
        })?;

    let resolved_parent =
        fs::canonicalize(parent)
            .await
            .map_err(|error| PatchExecutionError::ResolveFailed {
                path: requested_path.to_string(),
                reason: error.to_string(),
            })?;
    if !security.is_resolved_path_allowed(&resolved_parent) {
        return Err(PatchExecutionError::ResolvedPathDenied {
            path: resolved_parent.display().to_string(),
        });
    }

    let file_name = full_path
        .file_name()
        .ok_or_else(|| PatchExecutionError::InvalidPath {
            path: requested_path.to_string(),
            reason: "missing file name".into(),
        })?;

    let resolved_target = resolved_parent.join(file_name);
    if let Ok(meta) = fs::symlink_metadata(&resolved_target).await {
        if meta.file_type().is_symlink() {
            return Err(PatchExecutionError::SymlinkDenied {
                path: requested_path.to_string(),
            });
        }
    }

    Ok(resolved_target)
}

async fn path_exists(path: &Path) -> Result<bool, PatchExecutionError> {
    match fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(PatchExecutionError::ResolveFailed {
            path: path.display().to_string(),
            reason: error.to_string(),
        }),
    }
}

fn apply_hunks_to_text(
    content: &str,
    hunks: &[PatchHunk],
    path: &str,
) -> Result<String, PatchExecutionError> {
    let (mut lines, had_trailing_newline) = split_text_lines(content);
    let mut search_start = 0usize;

    for hunk in hunks {
        let before_lines: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|line| line.kind != PatchLineKind::Add)
            .map(|line| line.text.as_str())
            .collect();
        let after_lines: Vec<String> = hunk
            .lines
            .iter()
            .filter(|line| line.kind != PatchLineKind::Remove)
            .map(|line| line.text.clone())
            .collect();

        if before_lines.is_empty() {
            if hunk.has_end_of_file_marker {
                let insert_at = lines.len();
                lines.splice(insert_at..insert_at, after_lines);
                search_start = lines.len();
                continue;
            }

            return Err(PatchExecutionError::HunkApplyFailed {
                path: path.to_string(),
                reason: "add-only hunks require either context/removal lines or '*** End of File'"
                    .into(),
            });
        }

        let matches = find_hunk_matches(
            &lines,
            &before_lines,
            search_start,
            hunk.has_end_of_file_marker,
        );
        let Some(start) = matches.first().copied() else {
            return Err(PatchExecutionError::HunkApplyFailed {
                path: path.to_string(),
                reason: format!(
                    "could not match hunk anchored by '{}'",
                    before_lines.first().copied().unwrap_or_default()
                ),
            });
        };
        if matches.len() > 1 {
            return Err(PatchExecutionError::HunkAmbiguous {
                path: path.to_string(),
                reason: format!("matched {} locations", matches.len()),
            });
        }

        let end = start + before_lines.len();
        lines.splice(start..end, after_lines);
        search_start = start.saturating_add(1);
    }

    Ok(join_text_lines(&lines, had_trailing_newline))
}

fn split_text_lines(content: &str) -> (Vec<String>, bool) {
    let had_trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
    if had_trailing_newline {
        let _ = lines.pop();
    }
    (lines, had_trailing_newline)
}

fn join_text_lines(lines: &[String], had_trailing_newline: bool) -> String {
    let mut output = lines.join("\n");
    if had_trailing_newline && (!lines.is_empty() || output.is_empty()) {
        output.push('\n');
    }
    output
}

fn find_hunk_matches(
    lines: &[String],
    before_lines: &[&str],
    search_start: usize,
    must_end_at_eof: bool,
) -> Vec<usize> {
    if before_lines.len() > lines.len() {
        return Vec::new();
    }

    let max_start = lines.len().saturating_sub(before_lines.len());
    let mut matches = Vec::new();

    for start in search_start.min(lines.len())..=max_start {
        let end = start + before_lines.len();
        if must_end_at_eof && end != lines.len() {
            continue;
        }
        if lines[start..end]
            .iter()
            .map(String::as_str)
            .eq(before_lines.iter().copied())
        {
            matches.push(start);
        }
    }

    matches
}

fn format_patch_summary(summary: &PatchExecutionSummary) -> String {
    match summary.affected_paths.len() {
        0 => "Applied patch with no file changes.".to_string(),
        1 => format!("Applied patch touching: {}", summary.affected_paths[0]),
        count => format!(
            "Applied patch touching {count} paths: {}",
            summary.affected_paths.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use tempfile::TempDir;

    fn test_security(workspace: PathBuf) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn parse_valid_multi_file_patch_with_move_and_multi_hunk_update() {
        let input = "\
*** Begin Patch
*** Add File: notes.txt
+hello
+world
*** Update File: src/app.rs
*** Move to: src/app_v2.rs
@@ fn main()
 line one
-old value
+new value
@@
 line two
+line three
*** Delete File: old.txt
*** End Patch";

        let document = parse_patch_document(input).expect("patch should parse");
        assert_eq!(document.operations.len(), 3);

        assert_eq!(
            document.operations[0],
            PatchOperation::Add(AddFileOperation {
                path: "notes.txt".into(),
                lines: vec!["hello".into(), "world".into()],
            })
        );

        let PatchOperation::Update(update) = &document.operations[1] else {
            panic!("expected update operation");
        };
        assert_eq!(update.path, "src/app.rs");
        assert_eq!(update.move_to.as_deref(), Some("src/app_v2.rs"));
        assert_eq!(update.hunks.len(), 2);
        assert_eq!(update.hunks[0].header.as_deref(), Some("fn main()"));
        assert_eq!(
            update.hunks[0].lines,
            vec![
                PatchHunkLine {
                    kind: PatchLineKind::Context,
                    text: "line one".into(),
                },
                PatchHunkLine {
                    kind: PatchLineKind::Remove,
                    text: "old value".into(),
                },
                PatchHunkLine {
                    kind: PatchLineKind::Add,
                    text: "new value".into(),
                },
            ]
        );
        assert_eq!(update.hunks[1].header, None);
        assert_eq!(
            update.hunks[1].lines,
            vec![
                PatchHunkLine {
                    kind: PatchLineKind::Context,
                    text: "line two".into(),
                },
                PatchHunkLine {
                    kind: PatchLineKind::Add,
                    text: "line three".into(),
                },
            ]
        );

        assert_eq!(
            document.operations[2],
            PatchOperation::Delete(DeleteFileOperation {
                path: "old.txt".into(),
            })
        );
    }

    #[test]
    fn parse_rejects_missing_begin_marker() {
        let err = parse_patch_document("*** Add File: notes.txt\n+hello\n*** End Patch")
            .expect_err("patch without begin marker must fail");
        assert_eq!(err, PatchParseError::MissingBegin);
    }

    #[test]
    fn parse_rejects_invalid_add_file_grammar_without_mutation_opcodes() {
        let input = "\
*** Begin Patch
*** Add File: notes.txt
hello
*** End Patch";

        let err = parse_patch_document(input).expect_err("invalid add grammar must fail");
        assert_eq!(
            err,
            PatchParseError::InvalidLine {
                line_no: 3,
                message: "add-file hunks must contain only '+' lines".into(),
            }
        );
    }

    #[test]
    fn parse_rejects_unsupported_operation() {
        let input = "\
*** Begin Patch
*** Copy File: from.txt
*** End Patch";

        let err = parse_patch_document(input).expect_err("unsupported op must fail");
        assert_eq!(
            err,
            PatchParseError::UnsupportedOperation {
                line_no: 2,
                operation: "Copy File".into(),
            }
        );
    }

    #[test]
    fn parse_rejects_duplicate_file_operations() {
        let input = "\
*** Begin Patch
*** Update File: src/app.rs
@@
-old
+new
*** Update File: src/app.rs
@@
-again
+again new
*** End Patch";

        let err = parse_patch_document(input).expect_err("duplicate target must fail");
        assert_eq!(
            err,
            PatchParseError::ConflictingPath {
                line_no: 6,
                path: "src/app.rs".into(),
            }
        );
    }

    #[test]
    fn parse_rejects_conflicting_move_destination() {
        let input = "\
*** Begin Patch
*** Update File: src/app.rs
*** Move to: src/app_v2.rs
@@
-old
+new
*** Add File: src/app_v2.rs
+hello
*** End Patch";

        let err = parse_patch_document(input).expect_err("move target conflict must fail");
        assert_eq!(
            err,
            PatchParseError::ConflictingPath {
                line_no: 7,
                path: "src/app_v2.rs".into(),
            }
        );
    }

    #[test]
    fn parse_supports_end_of_file_marker() {
        let input = "\
*** Begin Patch
*** Update File: src/app.rs
@@
 line one
-line two
+line three
*** End of File
*** End Patch";

        let document = parse_patch_document(input).expect("patch with EOF marker should parse");
        let PatchOperation::Update(update) = &document.operations[0] else {
            panic!("expected update operation");
        };
        assert_eq!(update.hunks.len(), 1);
        assert!(update.hunks[0].has_end_of_file_marker);
    }

    #[test]
    fn parse_rejects_missing_end_patch_marker() {
        let input = "\
*** Begin Patch
*** Delete File: old.txt";

        let err = parse_patch_document(input).expect_err("missing end marker must fail");
        assert_eq!(err, PatchParseError::MissingEnd);
    }

    #[test]
    fn parse_rejects_hunk_without_patch_lines() {
        let input = "\
*** Begin Patch
*** Update File: src/app.rs
@@
*** End Patch";

        let err = parse_patch_document(input).expect_err("empty hunk must fail");
        assert_eq!(
            err,
            PatchParseError::InvalidLine {
                line_no: 3,
                message: "patch hunks must contain at least one context/add/remove line".into(),
            }
        );
    }

    #[tokio::test]
    async fn apply_patch_tool_applies_multi_file_patch_with_move_and_delete() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(workspace.join("src")).await.unwrap();
        fs::write(
            workspace.join("src/app.rs"),
            "line one\nold value\nline two\n",
        )
        .await
        .unwrap();
        fs::write(workspace.join("old.txt"), "remove me")
            .await
            .unwrap();

        let tool = ApplyPatchTool::new(test_security(workspace.clone()));
        let patch = "\
*** Begin Patch
*** Add File: notes.txt
+hello
+world
*** Update File: src/app.rs
*** Move to: src/app_v2.rs
@@
 line one
-old value
+new value
 line two
*** Delete File: old.txt
*** End Patch";

        let result = tool.execute(json!({ "patch": patch })).await.unwrap();
        assert!(result.success, "patch should succeed: {:?}", result.error);
        assert!(result.output.contains("notes.txt"));
        assert!(result.output.contains("src/app.rs -> src/app_v2.rs"));
        assert!(result.output.contains("old.txt"));

        let notes = fs::read_to_string(workspace.join("notes.txt"))
            .await
            .unwrap();
        assert_eq!(notes, "hello\nworld");

        let moved = fs::read_to_string(workspace.join("src/app_v2.rs"))
            .await
            .unwrap();
        assert_eq!(moved, "line one\nnew value\nline two\n");
        assert!(!workspace.join("src/app.rs").exists());
        assert!(!workspace.join("old.txt").exists());
    }

    #[tokio::test]
    async fn apply_patch_tool_handles_realistic_rust_module_refactor() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(workspace.join("src")).await.unwrap();
        fs::write(
            workspace.join("src/lib.rs"),
            "mod worker;\n\npub fn run() -> String {\n    worker::handle(\"ready\")\n}\n",
        )
        .await
        .unwrap();
        fs::write(
            workspace.join("src/worker.rs"),
            "pub fn handle(input: &str) -> String {\n    format!(\"handled:{input}\")\n}\n",
        )
        .await
        .unwrap();

        let tool = ApplyPatchTool::new(test_security(workspace.clone()));
        let patch = "\
*** Begin Patch
*** Update File: src/lib.rs
@@
-mod worker;
+mod processor;
 
 pub fn run() -> String {
-    worker::handle(\"ready\")
+    processor::format_status(\"ready\")
 }
*** Update File: src/worker.rs
*** Move to: src/processor.rs
@@
-pub fn handle(input: &str) -> String {
-    format!(\"handled:{input}\")
+pub fn format_status(input: &str) -> String {
+    format!(\"status:{input}\")
 }
*** End Patch";

        let result = tool.execute(json!({ "patch": patch })).await.unwrap();
        assert!(result.success, "patch should succeed: {:?}", result.error);
        assert!(result.output.contains("src/lib.rs"));
        assert!(result.output.contains("src/worker.rs -> src/processor.rs"));

        let lib_rs = fs::read_to_string(workspace.join("src/lib.rs"))
            .await
            .unwrap();
        assert_eq!(
            lib_rs,
            "mod processor;\n\npub fn run() -> String {\n    processor::format_status(\"ready\")\n}\n"
        );

        let processor_rs = fs::read_to_string(workspace.join("src/processor.rs"))
            .await
            .unwrap();
        assert_eq!(
            processor_rs,
            "pub fn format_status(input: &str) -> String {\n    format!(\"status:{input}\")\n}\n"
        );
        assert!(!workspace.join("src/worker.rs").exists());
    }

    #[tokio::test]
    async fn apply_patch_tool_rejects_invalid_grammar_without_mutation() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).await.unwrap();
        fs::write(workspace.join("keep.txt"), "keep me")
            .await
            .unwrap();

        let tool = ApplyPatchTool::new(test_security(workspace.clone()));
        let patch = "\
*** Begin Patch
*** Add File: broken.txt
hello
*** End Patch";

        let result = tool.execute(json!({ "patch": patch })).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("add-file hunks must contain only '+' lines"));
        assert_eq!(
            fs::read_to_string(workspace.join("keep.txt"))
                .await
                .unwrap(),
            "keep me"
        );
        assert!(!workspace.join("broken.txt").exists());
    }

    #[tokio::test]
    async fn apply_patch_tool_denies_forbidden_path() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).await.unwrap();

        let tool = ApplyPatchTool::new(test_security(workspace));
        let patch = "\
*** Begin Patch
*** Add File: ../../etc/evil.txt
+nope
*** End Patch";

        let result = tool.execute(json!({ "patch": patch })).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("path not allowed by security policy"));
    }

    #[tokio::test]
    async fn apply_patch_tool_fails_closed_when_hunk_does_not_match() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).await.unwrap();
        fs::write(workspace.join("sample.txt"), "line one\nline two\n")
            .await
            .unwrap();

        let tool = ApplyPatchTool::new(test_security(workspace.clone()));
        let patch = "\
*** Begin Patch
*** Update File: sample.txt
@@
 line one
-missing line
+new line
*** End Patch";

        let result = tool.execute(json!({ "patch": patch })).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("could not match hunk"));
        assert_eq!(
            fs::read_to_string(workspace.join("sample.txt"))
                .await
                .unwrap(),
            "line one\nline two\n"
        );
    }
}
