//! Landing a dispatch's branch without pull requests: merge, squash, or rebase it into the
//! task's recorded base in the project clone, and never push.
//!
//! The base is the local branch the record names: `origin/main` and `refs/remotes/origin/main`
//! are both `main`. A land updates whichever checkout holds that branch — your project clone
//! when the base is checked out there, a temporary detached worktree when no worktree has it.
//! A base checked out in another worktree, a base checkout with uncommitted work, a base that
//! is behind the recorded one, and a conflict all refuse with nothing changed.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::pr::record_base_branch;
use super::{
    canonical_cleanup_path, cleanup_query, command_failure, git_worktree_entries, same_path,
    BranchDeletion, BranchRetentionReason, DispatchHost,
};
use crate::agents::MergeMethod;
use crate::domain::Dispatch;

static LAND_SEQ: AtomicU64 = AtomicU64::new(0);

/// What a land does: which branch, into which local base, with which squash subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandRequest {
    pub branch: String,
    /// The local base branch (`main`), without a remote.
    pub base: String,
    /// The commit the dispatch started from: a base that does not contain it is behind.
    pub base_commit: Option<String>,
    /// The squash commit's subject, `T12 fix the login timeout`.
    pub subject: String,
    pub method: MergeMethod,
}

/// A landed branch: what it was, and where the base stands now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandOutcome {
    pub base: String,
    pub method: MergeMethod,
    /// The branch tip that was landed. Cleanup deletes the branch only while this is the tip.
    pub tip: String,
    /// The base's tip after the land. Cleanup deletes the branch only while the base still
    /// contains this commit.
    pub base_commit: String,
}

/// Why a land refused. Nothing in the repository changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandError {
    /// The dispatch records no base.
    NoBase,
    /// The project clone has no local branch of the recorded base's name.
    BaseMissing(String),
    /// The local base does not contain the commit the dispatch started from: pull first.
    BaseBehind(String),
    /// The checkout holding the base has uncommitted work, untracked files included.
    BaseDirty(String),
    /// Another worktree has the base checked out.
    BaseCheckedOut(String),
    BranchMissing,
    /// The base already has every commit on the branch.
    NothingToLand(String),
    /// The merge, squash, or rebase conflicted; the abort left the base as it was.
    Conflict(String),
    /// git refused or failed: its reason.
    Failed(String),
    /// The host cannot land branches (a host without git).
    Unsupported,
}

impl LandError {
    /// The stable code a refusal prints.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoBase | Self::BaseMissing(_) => "base-missing",
            Self::BaseBehind(_) => "base-behind",
            Self::BaseDirty(_) => "base-dirty",
            Self::BaseCheckedOut(_) => "base-checked-out",
            Self::BranchMissing => "branch-missing",
            Self::NothingToLand(_) => "nothing-to-land",
            Self::Conflict(_) => "land-conflict",
            Self::Failed(_) | Self::Unsupported => "land-failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NoBase => "the dispatch records no base; nothing to merge into".into(),
            Self::BaseMissing(base) => {
                format!("no local {base} branch in the project clone; create it first")
            }
            Self::BaseBehind(base) => {
                format!("{base} is behind the recorded base; pull or merge it first")
            }
            Self::BaseDirty(base) => format!(
                "{base} has uncommitted changes in your checkout; commit or discard them first"
            ),
            Self::BaseCheckedOut(base) => format!("{base} is checked out in another worktree"),
            Self::BranchMissing => "branch no longer available".into(),
            Self::NothingToLand(base) => {
                format!("nothing to merge: {base} already has every commit on the branch")
            }
            Self::Conflict(reason) => format!("conflict, nothing changed: {reason}"),
            Self::Failed(reason) => reason.clone(),
            Self::Unsupported => "this host cannot merge branches".into(),
        }
    }
}

/// What the card needs to say whether a branch can land, from local refs only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BaseState {
    /// The local base branch's name, when the dispatch records one.
    pub base: Option<String>,
    /// A local branch of that name exists.
    pub local: bool,
    /// The local base does not contain the dispatch's starting commit.
    pub behind: bool,
    /// The checkout holding the base has uncommitted work.
    pub dirty: bool,
    /// Another worktree has the base checked out.
    pub checked_out_elsewhere: bool,
    /// Commits on the branch that the base does not have.
    pub ahead: Option<u64>,
    /// Why the state could not be read: git failed, and the card says so instead of guessing.
    pub error: Option<String>,
}

impl BaseState {
    /// Whether `m` can land the branch now.
    pub fn landable(&self) -> bool {
        self.local
            && !self.behind
            && !self.dirty
            && !self.checked_out_elsewhere
            && self.ahead.is_some_and(|ahead| ahead > 0)
    }

    /// Why `m` cannot land the branch, in a few words, or `None` when it can.
    pub fn blocker(&self) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(format!("can't read the base: {error}"));
        }
        let Some(base) = self.base.as_deref() else {
            return Some("no recorded base".into());
        };
        if !self.local {
            return Some(format!("no local {base} branch"));
        }
        if self.checked_out_elsewhere {
            return Some(format!("{base} is checked out in another worktree"));
        }
        if self.behind {
            return Some(format!("{base} is behind the recorded base"));
        }
        if self.dirty {
            return Some(format!("{base} has uncommitted changes"));
        }
        if self.ahead == Some(0) {
            return Some(format!("{base} already has every commit on the branch"));
        }
        None
    }
}

/// Read the base's local state for the card: local refs only, never the network.
pub fn system_base_state(project: &Path, record: &Dispatch) -> Result<BaseState, String> {
    let Some(base) = record_base_branch(record) else {
        return Ok(BaseState::default());
    };
    let base_ref = format!("refs/heads/{base}");
    let branch_ref = format!("refs/heads/{}", record.branch);
    let mut state = BaseState {
        base: Some(base),
        ..BaseState::default()
    };
    if !ref_exists(project, &base_ref)? {
        return Ok(state);
    }
    state.local = true;
    state.ahead = Some(ahead_count(project, &base_ref, &branch_ref)?);
    if let Some(base_commit) = record.base_commit.as_deref() {
        state.behind = !is_ancestor(project, base_commit, &base_ref)?;
    }
    let root = canonical_cleanup_path(project)?;
    for (path, branch) in git_worktree_entries(project)? {
        if branch.as_deref() != Some(base_ref.as_str()) {
            continue;
        }
        match path {
            Some(path) if same_path(&path, &root) => state.dirty = !status_clean(project)?,
            // A listing that could not canonicalize its path still holds the branch somewhere.
            Some(_) | None => state.checked_out_elsewhere = true,
        }
    }
    Ok(state)
}

/// Land one branch in the project clone. Every refusal leaves the repository as it was.
pub fn system_land(project: &Path, request: &LandRequest) -> Result<LandOutcome, LandError> {
    let base_ref = format!("refs/heads/{}", request.base);
    let branch_ref = format!("refs/heads/{}", request.branch);
    if rev_parse(project, &branch_ref)
        .map_err(LandError::Failed)?
        .is_none()
    {
        return Err(LandError::BranchMissing);
    }
    if !ref_exists(project, &base_ref).map_err(LandError::Failed)? {
        return Err(LandError::BaseMissing(request.base.clone()));
    }
    if let Some(base_commit) = request.base_commit.as_deref() {
        if !is_ancestor(project, base_commit, &base_ref).map_err(LandError::Failed)? {
            return Err(LandError::BaseBehind(request.base.clone()));
        }
    }
    if ahead_count(project, &base_ref, &branch_ref).map_err(LandError::Failed)? == 0 {
        return Err(LandError::NothingToLand(request.base.clone()));
    }
    let root = canonical_cleanup_path(project).map_err(LandError::Failed)?;
    let old_base = rev_parse(project, &base_ref)
        .map_err(LandError::Failed)?
        .ok_or_else(|| LandError::BaseMissing(request.base.clone()))?;
    let holder = git_worktree_entries(project)
        .map_err(LandError::Failed)?
        .into_iter()
        .find(|(_, branch)| branch.as_deref() == Some(base_ref.as_str()));
    match holder {
        Some((Some(path), _)) if same_path(&path, &root) => {
            if !status_clean(project).map_err(LandError::Failed)? {
                return Err(LandError::BaseDirty(request.base.clone()));
            }
            land_in(project, request, &base_ref, &branch_ref)
        }
        // A listing whose path could not be canonicalized still holds the base somewhere.
        Some(_) => Err(LandError::BaseCheckedOut(request.base.clone())),
        None => land_in_temporary_worktree(project, request, &base_ref, &branch_ref, &old_base),
    }
}

/// Land in a throwaway detached worktree on the base, then move the base ref to it: the
/// project clone's own checkout is never touched.
fn land_in_temporary_worktree(
    project: &Path,
    request: &LandRequest,
    base_ref: &str,
    branch_ref: &str,
    old_base: &str,
) -> Result<LandOutcome, LandError> {
    let temp = std::env::temp_dir().join(format!(
        "tsk-land-{}-{}",
        std::process::id(),
        LAND_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    // A leftover from a killed run: git refuses a registered path, so clear a plain directory.
    let _ = std::fs::remove_dir_all(&temp);
    let temp_text = temp.to_string_lossy().into_owned();
    run(
        project,
        &["worktree", "add", "--detach", &temp_text, base_ref],
    )
    .map_err(|error| LandError::Failed(error.reason))?;
    let landed = land_in(&temp, request, base_ref, branch_ref);
    // Read the result before the worktree goes: the land is only real once the base moves.
    let head = rev_parse(&temp, "HEAD").ok().flatten();
    let removed = run(project, &["worktree", "remove", "--force", &temp_text])
        .map_err(|error| LandError::Failed(error.reason));
    let outcome = match landed {
        Ok(outcome) => outcome,
        Err(error) => {
            // The conflict abort already ran inside the worktree; drop it either way.
            let _ = removed;
            let _ = std::fs::remove_dir_all(&temp);
            return Err(error);
        }
    };
    removed.inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&temp);
    })?;
    let Some(head) = head else {
        return Err(LandError::Failed(
            "the land left no commit to move the base to".into(),
        ));
    };
    // The old value makes a base that moved meanwhile a refusal rather than a lost commit.
    run(project, &["update-ref", base_ref, &head, old_base])
        .map_err(|error| LandError::Failed(error.reason))?;
    Ok(LandOutcome {
        base_commit: head,
        ..outcome
    })
}

/// Merge, squash, or rebase the branch into whatever `dir` has checked out.
fn land_in(
    dir: &Path,
    request: &LandRequest,
    base_ref: &str,
    branch_ref: &str,
) -> Result<LandOutcome, LandError> {
    let range = format!("{base_ref}..{branch_ref}");
    match request.method {
        MergeMethod::Merge => {
            if let Err(error) = run(dir, &["merge", "--no-ff", "--no-edit", branch_ref]) {
                abort(dir, &["merge", "--abort"]);
                return Err(error.into_land_error());
            }
        }
        MergeMethod::Squash => {
            if let Err(error) = run(dir, &["merge", "--squash", branch_ref]) {
                abort(dir, &["merge", "--abort"]);
                return Err(error.into_land_error());
            }
            // A branch of commits that change nothing stages nothing: there is no squash
            // commit to make, and nothing to land.
            if run(dir, &["diff", "--cached", "--quiet"]).is_ok() {
                abort(dir, &["merge", "--abort"]);
                return Err(LandError::NothingToLand(request.base.clone()));
            }
            if let Err(error) = run(dir, &["commit", "-m", &request.subject]) {
                abort(dir, &["merge", "--abort"]);
                return Err(LandError::Failed(error.reason));
            }
        }
        MergeMethod::Rebase => {
            // A rebase merge replays the branch's commits onto the base. cherry-pick reads
            // them without checking the branch out, which the task's worktree holds.
            if let Err(error) = run(dir, &["cherry-pick", &range]) {
                abort(dir, &["cherry-pick", "--abort"]);
                return Err(error.into_land_error());
            }
        }
    }
    let base_commit = rev_parse(dir, "HEAD")
        .ok()
        .flatten()
        .ok_or_else(|| LandError::Failed("the land left no commit".into()))?;
    Ok(LandOutcome {
        base: request.base.clone(),
        method: request.method,
        tip: rev_parse(dir, branch_ref)
            .ok()
            .flatten()
            .unwrap_or_default(),
        base_commit,
    })
}

/// Land one row's branch: the base the dispatch recorded, its starting commit, and the task
/// title as a squash subject.
pub fn land_row(
    host: &mut impl DispatchHost,
    row: &super::pr::MergeRow,
    method: MergeMethod,
) -> Result<LandOutcome, LandError> {
    let base = record_base_branch(&row.record).ok_or(LandError::NoBase)?;
    let request = LandRequest {
        branch: row.record.branch.clone(),
        base,
        base_commit: row.record.base_commit.clone(),
        subject: format!("T{} {}", row.number, row.title),
        method,
    };
    host.land_branch(&row.project, &request)
}

/// A branch the dispatch already landed: delete it while nothing moved since. The land is
/// its own merge evidence, so a squash or rebase land deletes the branch though ancestry
/// reads unmerged.
pub fn landed_deletion(
    host: &mut impl DispatchHost,
    project: &Path,
    record: &Dispatch,
    landed: &crate::domain::Landed,
) -> Result<BranchDeletion, String> {
    let Some(base) = record_base_branch(record) else {
        return Ok(BranchDeletion::Kept(BranchRetentionReason::NoRecordedBase));
    };
    match host.branch_tip(project, &record.branch)? {
        None => Ok(BranchDeletion::Kept(
            BranchRetentionReason::BranchUnavailable,
        )),
        Some(tip) if tip != landed.tip => Ok(BranchDeletion::Kept(
            BranchRetentionReason::LandedNewerCommits,
        )),
        Some(tip) => match host.branch_tip(project, &base)? {
            None => Ok(BranchDeletion::Kept(BranchRetentionReason::BaseUnavailable)),
            Some(base_tip) if base_tip != landed.base_commit => {
                Ok(BranchDeletion::Kept(BranchRetentionReason::BaseMoved))
            }
            Some(_) => host.delete_branch_at_tip(project, &record.branch, &tip),
        },
    }
}

/// A git command that failed: what it said, and whether it was a conflict.
struct GitFailure {
    reason: String,
    conflict: bool,
}

impl GitFailure {
    fn into_land_error(self) -> LandError {
        if self.conflict {
            LandError::Conflict(self.reason)
        } else {
            LandError::Failed(self.reason)
        }
    }
}

fn run(project: &Path, args: &[&str]) -> Result<(), GitFailure> {
    let output = cleanup_query(project, args).map_err(|reason| GitFailure {
        reason,
        conflict: false,
    })?;
    if output.status.success() {
        return Ok(());
    }
    // `git merge` prints its conflict lines on stdout, `cherry-pick` on stderr.
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let conflict = said.lines().any(|line| {
        let line = line.trim();
        line.starts_with("CONFLICT")
            || line.starts_with("error: could not apply")
            || line.starts_with("error: Your local changes")
    });
    Err(GitFailure {
        reason: failure_reason(&output),
        conflict,
    })
}

/// Undo a half-made land. A squash leaves no `MERGE_HEAD`, so `merge --abort` cannot help
/// there: the abort's fallback restores the commit the clean checkout started from.
fn abort(dir: &Path, args: &[&str]) {
    if run(dir, args).is_err() {
        let _ = run(dir, &["reset", "--hard", "HEAD"]);
    }
}

fn failure_reason(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Some git failures say everything on stdout, such as `nothing to commit`; a conflict
    // says CONFLICT on stdout for a merge and on stderr for a cherry-pick.
    stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| line.starts_with("CONFLICT") || line.starts_with("error:"))
        .or_else(|| stderr.lines().map(str::trim).find(|line| !line.is_empty()))
        // Some git failures say everything on stdout, such as `nothing to commit`.
        .or_else(|| stdout.lines().map(str::trim).find(|line| !line.is_empty()))
        .map_or_else(
            || command_failure("git", output),
            |line| line.trim_start_matches("error: ").to_string(),
        )
}

fn ref_exists(project: &Path, reference: &str) -> Result<bool, String> {
    let output = cleanup_query(project, &["show-ref", "--quiet", "--verify", reference])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(command_failure("git show-ref", &output)),
    }
}

fn rev_parse(project: &Path, reference: &str) -> Result<Option<String>, String> {
    let output = cleanup_query(project, &["rev-parse", "--verify", "--quiet", reference])?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string()))
}

fn is_ancestor(project: &Path, commit: &str, reference: &str) -> Result<bool, String> {
    let output = cleanup_query(project, &["merge-base", "--is-ancestor", commit, reference])
        .map_err(|error| super::cleanup_inspection_error("ancestry check", error))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(command_failure("git merge-base", &output)),
    }
}

fn ahead_count(project: &Path, base_ref: &str, branch_ref: &str) -> Result<u64, String> {
    let range = format!("{base_ref}..{branch_ref}");
    let output = cleanup_query(project, &["rev-list", "--count", &range])?;
    if !output.status.success() {
        return Err(command_failure("git rev-list", &output));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| "git rev-list printed no count".to_string())
}

fn status_clean(project: &Path) -> Result<bool, String> {
    let output = cleanup_query(project, &["status", "--porcelain", "--untracked-files=all"])?;
    if !output.status.success() {
        return Err(command_failure("git status", &output));
    }
    Ok(output.stdout.is_empty())
}
