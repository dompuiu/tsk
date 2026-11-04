//! Headless dispatch cleanup.

use std::path::PathBuf;

use crate::cli::parser::TaskAddress;
use crate::dispatch::{self, CleanupError, CleanupResult, DispatchHost, HostKind};
use crate::store::{default_state_dir, TaskStore};

pub fn run(target: TaskAddress, state_dir: Option<PathBuf>) -> Result<CleanupResult, CleanupError> {
    let (state_dir, mut host) = crate::cli::dispatch::system_host(state_dir);
    run_with_host(
        target,
        Some(state_dir),
        dispatch::HostKind::detect(),
        &mut host,
    )
}

/// Clean one dispatch, saving its marker only after host cleanup succeeds.
///
pub fn run_with_host(
    target: TaskAddress,
    state_dir: Option<PathBuf>,
    host_kind: HostKind,
    host: &mut impl DispatchHost,
) -> Result<CleanupResult, CleanupError> {
    let state_dir = state_dir.unwrap_or_else(default_state_dir);
    let store = TaskStore::new(&state_dir);
    let mut state = store
        .load()
        .map_err(|error| CleanupError::Store(error.to_string()))?;
    let id = state
        .tasks()
        .iter()
        .find(|task| target.matches(task))
        .map(|task| task.id)
        .ok_or(CleanupError::UnknownTask)?;
    let result = dispatch::clean_with_host(&mut state, id, host_kind, host)?;
    store
        .reload_merge_save(&mut state)
        .map_err(|error| CleanupError::Store(error.to_string()))?;
    Ok(result)
}

/// Why `--merge` merged nothing. The task, its dispatch and GitHub are unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeRefused {
    Cleanup(CleanupError),
    /// `[merge]` in `config.toml` is invalid.
    Config(String),
    /// `[workflow]` in `config.toml` is invalid.
    Workflow(String),
    Merge(dispatch::MergeError),
}

impl MergeRefused {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cleanup(error) => error.code(),
            Self::Config(_) => "merge-config",
            Self::Workflow(_) => "workflow-config",
            Self::Merge(error) => error.code(),
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Cleanup(error) => error.to_string(),
            Self::Config(reason) | Self::Workflow(reason) => reason.clone(),
            Self::Merge(error) => error.message(),
        }
    }

    pub fn is_store(&self) -> bool {
        matches!(self, Self::Cleanup(CleanupError::Store(_)))
    }
}

/// What `--merge` merged or landed, for the result line and `--json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    pub number: u64,
    pub record: crate::domain::Dispatch,
    pub outcome: dispatch::pr::Merged,
}

/// Land the task's work, the way its project lands: merge its open pull request, or merge its
/// branch into the recorded base. Runs before anything else: the done and the cleanup follow
/// only a merge that succeeded. Refuses a dispatch cleanup would refuse (none, cleaned, dirty).
pub fn merge_with_host(
    target: TaskAddress,
    state_dir: Option<PathBuf>,
    host_kind: HostKind,
    host: &mut impl DispatchHost,
) -> Result<Merged, MergeRefused> {
    let state_dir = state_dir.unwrap_or_else(default_state_dir);
    let store = TaskStore::new(&state_dir);
    let mut state = store
        .load()
        .map_err(|error| MergeRefused::Cleanup(CleanupError::Store(error.to_string())))?;
    let id = state
        .tasks()
        .iter()
        .find(|task| target.matches(task))
        .map(|task| task.id)
        .ok_or(MergeRefused::Cleanup(CleanupError::UnknownTask))?;
    let preview = dispatch::inspect_cleanup_cached_with_host(&state, id, host_kind, host)
        .map_err(MergeRefused::Cleanup)?;
    if preview.inspection.dirty {
        return Err(MergeRefused::Cleanup(CleanupError::DirtyWorktree));
    }
    let method = crate::agents::MergeMethod::load(&state_dir)
        .map_err(|error| MergeRefused::Config(error.to_string()))?;
    // A broken `[workflow]` must not silently land one way or the other.
    if let Some(error) = host.workflow().error() {
        return Err(MergeRefused::Workflow(error.to_string()));
    }
    let pull_requests = host.workflow().pull_requests(Some(&preview.project));
    let row = dispatch::MergeRow {
        task_id: id,
        number: preview.number,
        title: preview.title,
        project: preview.project,
        record: preview.record,
        pull_requests,
    };
    let outcome =
        dispatch::pr::merge_row_configured(host, &row, method).map_err(MergeRefused::Merge)?;
    // A local land is recorded before the done saves, so the cleanup after it may delete a
    // branch a squash or rebase left without ancestry.
    if let Some(landed) = outcome.landed() {
        store
            .reload_merge_save_then(&mut state, |state| state.record_dispatch_landed(id, landed))
            .map_err(|error| MergeRefused::Cleanup(CleanupError::Store(error.to_string())))?
            .map_err(|error| MergeRefused::Cleanup(CleanupError::Store(error.to_string())))?;
    }
    Ok(Merged {
        number: row.number,
        record: row.record,
        outcome,
    })
}
