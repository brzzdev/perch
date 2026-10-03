//! The warning-to-outcome lifecycle for branch and worktree Removal.
//!
//! Every destructive flow enters through [`assess`]. An [`Assessment`] owns the
//! safety facts and display-ready offers, [`Assessment::choose`] issues the
//! narrow License earned by the user's choice, and [`Pending::finish`] alone may
//! mutate Git state. Outcome wording, Hook eligibility, partial failures, and a
//! current-worktree Handoff stay behind the same seam.

use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

use console::style;

use super::{display_path, hook, marker, picker, reclamation, shell_quote};
use crate::{AppResult, Error, git};

mod reporting;

/// What Removal would irreversibly destroy. It remains private to the Removal
/// module's implementation once callers have been migrated to display-ready
/// offers.
#[derive(Default, Clone, Copy)]
struct Risk {
    dirty: bool,
    unmerged: Option<git::Unmerged>,
}

impl Risk {
    /// The picker markers for this indivisible safety judgement. Keeping the
    /// rendering here stops dirtiness and mergedness travelling as loose facts.
    fn markers(self) -> String {
        let mut markers = Vec::new();
        if self.dirty {
            markers.push(marker::Marker::Dirty);
        }
        match self.unmerged {
            Some(git::Unmerged::Ahead(n)) => markers.push(marker::Marker::Unmerged(Some(n))),
            Some(git::Unmerged::NoUpstream) => markers.push(marker::Marker::Unmerged(None)),
            None => {}
        }
        marker::join(&markers)
    }
}

/// One Removal request, before any warning has licensed mutation.
pub(crate) enum Request {
    Branches(BranchRequest),
    Stale(StaleRequest),
    Worktrees(WorktreeRequest),
}

pub(crate) struct StaleRequest {
    stale: Vec<git::StaleBranch>,
    worktrees: Vec<git::Worktree>,
    remote: String,
    old_branch: Option<String>,
    destination: Option<String>,
}

impl StaleRequest {
    pub(crate) fn new(
        stale: Vec<git::StaleBranch>,
        worktrees: Vec<git::Worktree>,
        remote: &str,
        old_branch: Option<&str>,
        destination: Option<&str>,
    ) -> Self {
        Self {
            stale,
            worktrees,
            remote: remote.to_string(),
            old_branch: old_branch.map(str::to_string),
            destination: destination.map(str::to_string),
        }
    }
}

/// Whether branch Removal should ignore upstreams, offer them, or require that
/// every eligible upstream is put before the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpstreamInterest {
    None,
    Offer,
    Requested,
}

pub(crate) struct BranchRequest {
    branches: Vec<String>,
    worktrees: Vec<git::Worktree>,
    current: Option<String>,
    upstream: UpstreamInterest,
}

impl BranchRequest {
    pub(crate) fn new(
        branches: Vec<String>,
        worktrees: Vec<git::Worktree>,
        current: Option<&str>,
        upstream: UpstreamInterest,
    ) -> Self {
        Self {
            branches,
            worktrees,
            current: current.map(str::to_string),
            upstream,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Forcing {
    Forced,
    Unforced,
}

impl From<bool> for Forcing {
    fn from(force: bool) -> Self {
        if force { Self::Forced } else { Self::Unforced }
    }
}

pub(crate) struct WorktreeRequest {
    worktrees: Vec<git::Worktree>,
    cwd: Option<PathBuf>,
    target: Option<String>,
    forcing: Forcing,
}

impl WorktreeRequest {
    pub(crate) fn new(
        worktrees: Vec<git::Worktree>,
        cwd: Option<PathBuf>,
        target: Option<&str>,
        forcing: Forcing,
    ) -> Self {
        Self {
            worktrees,
            cwd,
            target: target.map(str::to_string),
            forcing,
        }
    }
}

/// An opaque identity returned with an offer and accepted back as a choice.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct LocalId(usize);

/// A display-ready local Removal offer. The caller renders it and returns only
/// its opaque identity; it never reconstructs the safety facts behind the row.
pub(crate) struct Offer {
    id: LocalId,
    name: String,
    label: String,
    selected: bool,
    disabled: bool,
}

impl Offer {
    pub(crate) fn id(&self) -> LocalId {
        self.id
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

impl From<&Offer> for picker::MultiItem {
    fn from(offer: &Offer) -> Self {
        Self {
            label: offer.label.clone(),
            selected: offer.selected,
            disabled: offer.disabled,
        }
    }
}

/// The warning material for a named target. An empty warning list means the
/// target is safe enough to choose without confirmation.
pub(crate) struct NamedOffer {
    id: LocalId,
    warnings: Vec<String>,
    refusal: String,
    question: String,
}

impl NamedOffer {
    pub(crate) fn id(&self) -> LocalId {
        self.id
    }

    pub(crate) fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(crate) fn refusal(&self) -> &str {
        &self.refusal
    }

    pub(crate) fn question(&self) -> &str {
        &self.question
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestKind {
    Branches,
    Stale,
    Worktrees,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Reclamation {
    Background,
    Synchronous,
}

#[derive(Clone, Debug)]
enum OwnedTarget {
    Branch { name: String },
    Held { name: String, path: PathBuf },
    Worktree { path: PathBuf },
}

impl OwnedTarget {
    fn borrowed(&self) -> Target<'_> {
        match self {
            Self::Branch { name } => Target::Branch { name },
            Self::Held { name, path } => Target::Held { name, path },
            Self::Worktree { path } => Target::Worktree { path },
        }
    }

    fn name(&self) -> Option<&str> {
        match self {
            Self::Branch { name } | Self::Held { name, .. } => Some(name),
            Self::Worktree { .. } => None,
        }
    }

    fn path(&self) -> Option<&Path> {
        match self {
            Self::Branch { .. } => None,
            Self::Held { path, .. } | Self::Worktree { path } => Some(path),
        }
    }

    fn offer_name(&self) -> String {
        match self {
            Self::Branch { name } | Self::Held { name, .. } => name.clone(),
            Self::Worktree { path } => display_path(path),
        }
    }
}

struct AssessedLocal {
    target: OwnedTarget,
    risk: Risk,
    proof: Option<git::Proof>,
    /// Why naming this target is an error, where it is one. It is also the
    /// whole of why a picker row is inert: the picker offers what a name would
    /// reach, so the two answers cannot differ.
    named_error: Option<NamedError>,
    contains_cwd: bool,
}

impl AssessedLocal {
    fn disabled(&self) -> bool {
        self.named_error.is_some()
    }
}

enum NamedError {
    Held(git::Worktree),
}

struct WorktreeTargetResolution {
    target: String,
    id: Option<LocalId>,
}

/// A stable safety snapshot and its display-ready offers. It cannot mutate the
/// repository; choosing consumes it and prepares a [`Pending`] Removal.
pub(crate) struct Assessment {
    kind: RequestKind,
    offers: Vec<Offer>,
    locals: Vec<AssessedLocal>,
    legend: Option<String>,
    main: Option<PathBuf>,
    worktree_target: Option<WorktreeTargetResolution>,
    upstream: UpstreamInterest,
}

impl Assessment {
    pub(crate) fn offers(&self) -> &[Offer] {
        &self.offers
    }

    pub(crate) fn offer(&self, id: LocalId) -> &Offer {
        &self.offers[id.0]
    }

    pub(crate) fn legend(&self) -> Option<&str> {
        self.legend.as_deref()
    }

    pub(crate) fn named(&self, name: &str) -> AppResult<NamedOffer> {
        let id = match self.kind {
            RequestKind::Branches | RequestKind::Stale => self
                .locals
                .iter()
                .position(|local| local.target.name() == Some(name)),
            RequestKind::Worktrees => match &self.worktree_target {
                Some(requested) if requested.target == name => requested.id.map(|id| id.0),
                Some(_) | None => None,
            },
        };
        let Some(index) = id else {
            return Err(match self.kind {
                RequestKind::Branches | RequestKind::Stale => Error::LocalBranchNotFound {
                    branch: name.to_string(),
                },
                RequestKind::Worktrees if name == "." => Error::Git {
                    command: "worktree remove".into(),
                    message: "the main worktree cannot be removed".into(),
                },
                RequestKind::Worktrees => Error::Git {
                    command: "worktree remove".into(),
                    message: format!("no worktree matching '{name}'"),
                },
            });
        };
        let local = &self.locals[index];
        if let Some(NamedError::Held(holder)) = &local.named_error {
            let branch = local.target.name().unwrap_or(name);
            let hint = if holder.is_main {
                "check out another branch in the main worktree first".into()
            } else {
                format!(
                    "remove that worktree with `perch wt rm {}`",
                    shell_quote(branch)
                )
            };
            return Err(Error::HeldForRemoval {
                branch: branch.to_string(),
                path: display_path(&holder.path),
                hint,
            });
        }

        let (subject, path, separator, question) = match (&self.kind, &local.target) {
            (RequestKind::Branches, OwnedTarget::Branch { name }) => (
                name.as_str(),
                self.main.as_deref().unwrap_or_else(|| Path::new(".")),
                "; ",
                format!("Delete {name} anyway?"),
            ),
            (RequestKind::Worktrees, OwnedTarget::Held { name, path }) => (
                name.as_str(),
                path.as_path(),
                " and ",
                format!("Remove the worktree and delete {name} anyway?"),
            ),
            (RequestKind::Worktrees, OwnedTarget::Worktree { path }) => (
                "this worktree",
                path.as_path(),
                " and ",
                "Remove the worktree anyway?".to_string(),
            ),
            _ => unreachable!("named confirmation is not used for stale Removal"),
        };
        let warnings = reporting::warnings(local.risk, subject, path);
        let reason = reporting::describe(local.risk, subject, path).join(separator);
        let refusal =
            format!("{reason}; not removing. Rerun in a terminal to confirm, or pass --force.");
        Ok(NamedOffer {
            id: LocalId(index),
            warnings,
            refusal,
            question,
        })
    }

    pub(crate) fn choose(self, choice: LocalChoice) -> AppResult<Pending> {
        let (ids, authority, source) = choice.into_parts();
        let mut locals = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(local) = self.locals.get(id.0) else {
                continue;
            };
            if source == ChoiceSource::Picker && local.disabled() {
                continue;
            }
            let license = match authority {
                Authority::Forced => License::forced(),
                Authority::Shown => match &local.proof {
                    Some(proof) => License::proven(local.risk, proof),
                    None => License::shown(local.risk),
                },
            };
            locals.push(PlannedLocal {
                target: local.target.clone(),
                license,
                preparation_failure: None,
                contains_cwd: local.contains_cwd,
                upstream: None,
            });
        }

        let mut pending = Pending {
            kind: self.kind,
            locals,
            main: self.main,
            upstream_offers: Vec::new(),
            notices: Vec::new(),
        };
        if self.kind == RequestKind::Branches && self.upstream != UpstreamInterest::None {
            pending.prepare_upstreams(self.upstream, source)?;
        }
        Ok(pending)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Authority {
    Forced,
    Shown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChoiceSource {
    Named,
    Picker,
}

/// The single choice vocabulary used by named, picker, and forced Removal.
pub(crate) struct LocalChoice {
    ids: Vec<LocalId>,
    authority: Authority,
    source: ChoiceSource,
}

impl LocalChoice {
    pub(crate) fn picked(ids: Vec<LocalId>) -> Self {
        Self {
            ids,
            authority: Authority::Shown,
            source: ChoiceSource::Picker,
        }
    }

    pub(crate) fn forced_picked(ids: Vec<LocalId>) -> Self {
        Self {
            ids,
            authority: Authority::Forced,
            source: ChoiceSource::Picker,
        }
    }

    pub(crate) fn named(id: LocalId) -> Self {
        Self {
            ids: vec![id],
            authority: Authority::Shown,
            source: ChoiceSource::Named,
        }
    }

    pub(crate) fn forced(id: LocalId) -> Self {
        Self {
            ids: vec![id],
            authority: Authority::Forced,
            source: ChoiceSource::Named,
        }
    }

    fn into_parts(self) -> (Vec<LocalId>, Authority, ChoiceSource) {
        (self.ids, self.authority, self.source)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct UpstreamId(usize);

pub(crate) struct UpstreamOffer {
    id: UpstreamId,
    label: String,
    warning: String,
    question: String,
}

impl UpstreamOffer {
    pub(crate) fn id(&self) -> UpstreamId {
        self.id
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn warning(&self) -> &str {
        &self.warning
    }

    pub(crate) fn question(&self) -> &str {
        &self.question
    }
}

struct PlannedLocal {
    target: OwnedTarget,
    license: License,
    preparation_failure: Option<String>,
    contains_cwd: bool,
    upstream: Option<(UpstreamId, git::RemoteBranch)>,
}

pub(crate) trait Reporter {
    fn emit(&mut self, line: String);

    fn removed(&mut self, hook: RemovedHook<'_>) {
        hook.fire();
    }
}

/// A successful worktree Removal owes the outside world one Hook. A reporter
/// may choose where that Hook is drawn, but dropping the token still fires it,
/// so presentation cannot suppress the consequence.
pub(crate) struct RemovedHook<'a> {
    path: &'a Path,
    branch: Option<&'a str>,
    main: &'a Path,
    fired: bool,
}

impl RemovedHook<'_> {
    pub(crate) fn fire(mut self) {
        self.fired = true;
        hook::fire(hook::Event::Removed, self.path, self.branch, self.main);
    }
}

impl Drop for RemovedHook<'_> {
    fn drop(&mut self) {
        if !self.fired {
            hook::fire(hook::Event::Removed, self.path, self.branch, self.main);
        }
    }
}

pub(crate) struct StderrReporter;

impl Reporter for StderrReporter {
    fn emit(&mut self, line: String) {
        eprintln!("{line}");
    }
}

/// A fully chosen Removal. Dropping it is cancellation; only [`Self::finish`]
/// can cross the mutation point.
pub(crate) struct Pending {
    kind: RequestKind,
    locals: Vec<PlannedLocal>,
    main: Option<PathBuf>,
    upstream_offers: Vec<UpstreamOffer>,
    notices: Vec<String>,
}

enum UpstreamPreparation {
    Candidate(git::RemoteBranch),
    Failure(String),
    None,
    Notice(String),
}

fn prepare_upstream(
    branch: &str,
    interest: UpstreamInterest,
    source: ChoiceSource,
) -> AppResult<UpstreamPreparation> {
    let requested = interest == UpstreamInterest::Requested;
    let named = source == ChoiceSource::Named;
    let upstream = match git::same_named_upstream(branch) {
        Ok(upstream) => upstream,
        Err(error) if requested && named => return Err(error),
        Err(error) if requested => return Ok(UpstreamPreparation::Failure(error.to_string())),
        Err(error) => {
            return Ok(UpstreamPreparation::Notice(format!(
                "{} could not read the upstream of {branch}: {error}; offering local removal only",
                super::warn(),
            )));
        }
    };
    let Some(upstream) = upstream else {
        if requested && named {
            return Err(Error::NoRemovableUpstream {
                branch: branch.to_string(),
                reason: "it has no explicit same-named upstream".into(),
            });
        }
        return Ok(UpstreamPreparation::None);
    };

    match git::inspect_upstream(&upstream) {
        Ok(git::UpstreamInspection::Removable(upstream)) => {
            Ok(UpstreamPreparation::Candidate(upstream))
        }
        Ok(git::UpstreamInspection::Absent(upstream)) if named => {
            Ok(UpstreamPreparation::Notice(format!(
                "{} upstream {}/{} is already absent",
                super::warn(),
                upstream.remote,
                upstream.branch,
            )))
        }
        Ok(git::UpstreamInspection::Default(upstream)) if requested && named => {
            Err(Error::NoRemovableUpstream {
                branch: branch.to_string(),
                reason: format!(
                    "{}/{} is the remote's default branch",
                    upstream.remote, upstream.branch
                ),
            })
        }
        Ok(git::UpstreamInspection::Absent(_) | git::UpstreamInspection::Default(_)) => {
            Ok(UpstreamPreparation::None)
        }
        Ok(git::UpstreamInspection::DefaultUnknown(upstream)) => {
            let reason = format!(
                "could not establish the default branch of {}",
                upstream.remote
            );
            let error = Error::NoRemovableUpstream {
                branch: branch.to_string(),
                reason: reason.clone(),
            };
            if requested && named {
                Err(error)
            } else if requested {
                Ok(UpstreamPreparation::Failure(error.to_string()))
            } else {
                Ok(UpstreamPreparation::Notice(format!(
                    "{} {reason}; offering local removal only",
                    super::warn()
                )))
            }
        }
        Err(error) if requested && named => Err(error),
        Err(error) if requested => Ok(UpstreamPreparation::Failure(error.to_string())),
        Err(error) => Ok(UpstreamPreparation::Notice(format!(
            "{} could not inspect the upstream of {branch}: {error}; offering local removal only",
            super::warn(),
        ))),
    }
}

impl Pending {
    pub(crate) fn notices(&self) -> &[String] {
        &self.notices
    }

    pub(crate) fn upstream_offers(&self) -> &[UpstreamOffer] {
        &self.upstream_offers
    }

    fn prepare_upstreams(
        &mut self,
        interest: UpstreamInterest,
        source: ChoiceSource,
    ) -> AppResult<()> {
        for local in &mut self.locals {
            let Some(branch) = local.target.name() else {
                continue;
            };
            match prepare_upstream(branch, interest, source)? {
                UpstreamPreparation::Candidate(upstream) => {
                    let id = UpstreamId(self.upstream_offers.len());
                    self.upstream_offers.push(UpstreamOffer {
                        id,
                        label: format!("{}/{}", upstream.remote, upstream.branch),
                        warning: reporting::upstream_warning(&upstream),
                        question: format!(
                            "Delete upstream {}/{} too?",
                            upstream.remote, upstream.branch
                        ),
                    });
                    local.upstream = Some((id, upstream));
                }
                UpstreamPreparation::Failure(error) => {
                    local.preparation_failure = Some(error);
                }
                UpstreamPreparation::None => {}
                UpstreamPreparation::Notice(notice) => self.notices.push(notice),
            }
        }
        Ok(())
    }

    pub(crate) fn finish(
        self,
        upstream: UpstreamChoice,
        mut reporter: impl Reporter,
    ) -> Result<Outcome, FinishFailure> {
        let mut steps = GitSteps::at_main(self.main.as_deref());
        self.finish_with_steps(upstream, &mut steps, &mut reporter)
    }

    fn finish_with_steps(
        self,
        upstream: UpstreamChoice,
        steps: &mut impl Steps,
        reporter: &mut impl Reporter,
    ) -> Result<Outcome, FinishFailure> {
        let selected: HashSet<UpstreamId> = upstream.ids.into_iter().collect();
        let handoff = self
            .locals
            .iter()
            .any(|local| local.contains_cwd)
            .then(|| self.main.clone())
            .flatten();
        if let Some(main) = &handoff {
            env::set_current_dir(main).map_err(|error| FinishFailure::new(error.into(), None))?;
        }

        let mut outcome = Outcome {
            failed: false,
            handoff,
        };
        let reclamation = if self.kind == RequestKind::Worktrees {
            Reclamation::Background
        } else {
            Reclamation::Synchronous
        };
        for local in self.locals {
            let display_name = local.target.name().unwrap_or("this worktree");
            if let Some(error) = local.preparation_failure {
                reporter.emit(format!(
                    "{} could not prepare upstream removal for {display_name}: {error}; kept the local branch",
                    super::warn(),
                ));
                outcome.failed = true;
                continue;
            }

            let report = match remove(local.target.borrowed(), &local.license, reclamation, steps) {
                Ok(report) => report,
                Err(error) if self.kind != RequestKind::Branches => {
                    // The process moved before removal began. Move its pending
                    // handoff into the error so the caller emits it exactly once.
                    let handoff = outcome.handoff.take();
                    return Err(FinishFailure::new(error, handoff));
                }
                Err(error) => {
                    reporter.emit(format!(
                        "{} could not remove {display_name}: {error}",
                        super::warn(),
                    ));
                    outcome.failed = true;
                    continue;
                }
            };
            for line in reporting::removal_outcome(&report) {
                reporter.emit(line);
            }
            if report.worktree_removed()
                && let (Some(path), Some(main)) = (local.target.path(), self.main.as_deref())
            {
                reporter.removed(RemovedHook {
                    path,
                    branch: local.target.name(),
                    main,
                    fired: false,
                });
            }

            let local_deleted = report.branch_removed();
            let selected_upstream = local
                .upstream
                .as_ref()
                .filter(|(id, _)| selected.contains(id))
                .map(|(_, candidate)| candidate);
            let Some(upstream) = selected_upstream else {
                if self.kind == RequestKind::Branches && !local_deleted {
                    outcome.failed = true;
                }
                continue;
            };
            let local_ref = format!("refs/heads/{display_name}");
            if !local_deleted || git::resolve(None, &local_ref).is_some() {
                reporter.emit(reporting::upstream_kept_local(upstream));
                outcome.failed = true;
                continue;
            }
            let remote_outcome = match git::delete_remote_branch(upstream) {
                Ok(remote_outcome) => remote_outcome,
                Err(error) => {
                    reporter.emit(format!(
                        "{} could not delete upstream {}/{}: {error}",
                        super::warn(),
                        upstream.remote,
                        upstream.branch,
                    ));
                    outcome.failed = true;
                    continue;
                }
            };
            reporter.emit(reporting::upstream_outcome(upstream, &remote_outcome));
            if !matches!(
                remote_outcome,
                git::RemoteBranchDeleteOutcome::Deleted
                    | git::RemoteBranchDeleteOutcome::AlreadyAbsent
            ) {
                outcome.failed = true;
            }
        }
        Ok(outcome)
    }
}

pub(crate) struct UpstreamChoice {
    ids: Vec<UpstreamId>,
}

impl UpstreamChoice {
    pub(crate) fn keep() -> Self {
        Self { ids: Vec::new() }
    }

    pub(crate) fn selected(ids: Vec<UpstreamId>) -> Self {
        Self { ids }
    }
}

/// A fatal removal error plus the shell handoff earned before it occurred.
#[derive(Debug)]
pub(crate) struct FinishFailure {
    error: Error,
    handoff: Option<PathBuf>,
}

impl FinishFailure {
    fn new(error: Error, handoff: Option<PathBuf>) -> Self {
        Self { error, handoff }
    }

    pub(crate) fn handoff(&self) -> Option<&Path> {
        self.handoff.as_deref()
    }

    pub(crate) fn into_error(self) -> Error {
        self.error
    }
}

/// Everything the caller must render or perform after mutation.
pub(crate) struct Outcome {
    failed: bool,
    handoff: Option<PathBuf>,
}

impl Outcome {
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }

    pub(crate) fn handoff(&self) -> Option<&Path> {
        self.handoff.as_deref()
    }
}

pub(crate) fn assess(request: Request) -> AppResult<Assessment> {
    match request {
        Request::Branches(request) => assess_branches(request),
        Request::Stale(request) => Ok(assess_stale(request)),
        Request::Worktrees(request) => assess_worktrees(request),
    }
}

fn risk_legend(risks: impl IntoIterator<Item = Risk>) -> Option<String> {
    let (has_dirty, has_unmerged) =
        risks
            .into_iter()
            .fold((false, false), |(has_dirty, has_unmerged), risk| {
                (
                    has_dirty || risk.dirty,
                    has_unmerged || risk.unmerged.is_some(),
                )
            });
    let mut parts = Vec::new();
    if has_dirty {
        parts.push(format!("{} uncommitted changes", marker::Marker::Dirty));
    }
    if has_unmerged {
        parts.push(format!(
            "{} unmerged commits",
            marker::Marker::Unmerged(None)
        ));
    }
    (!parts.is_empty()).then(|| parts.join("   "))
}

fn assess_stale(request: StaleRequest) -> Assessment {
    let main = request
        .worktrees
        .iter()
        .find(|worktree| worktree.is_main)
        .map(|worktree| worktree.path.clone());
    let unmerged = git::unmerged_branches(main.as_deref()).unwrap_or_default();
    let candidates: Vec<&str> = request
        .stale
        .iter()
        .map(|branch| branch.name.as_str())
        .filter(|name| {
            unmerged.contains_key(*name) && request.destination.as_deref() != Some(*name)
        })
        .collect();
    let equivalent = git::equivalent_branches(main.as_deref(), &request.remote, &candidates);
    build_stale_assessment(request, main, &unmerged, &equivalent, git::worktree_dirty)
}

fn build_stale_assessment(
    request: StaleRequest,
    main: Option<PathBuf>,
    unmerged: &HashMap<String, git::Unmerged>,
    equivalent: &HashMap<String, git::Proof>,
    dirty: impl Fn(&Path) -> bool,
) -> Assessment {
    let mut raw = Vec::new();
    let mut locals = Vec::new();
    let mut stale = request.stale;
    stale.retain(|branch| request.destination.as_deref() != Some(branch.name.as_str()));
    let ground_width = stale
        .iter()
        .map(|branch| stale_ground_label(branch.ground).len())
        .max()
        .unwrap_or_default();
    for stale in stale {
        let worktree = git::worktree_for_branch(&request.worktrees, &stale.name);
        let proof = equivalent.get(&stale.name).cloned();
        let risk = Risk {
            dirty: worktree
                .as_ref()
                .is_some_and(|worktree| !worktree.prunable && dirty(&worktree.path)),
            unmerged: proof
                .is_none()
                .then(|| unmerged.get(&stale.name).copied())
                .flatten(),
        };
        let worktree_label = match &worktree {
            None => String::new(),
            Some(worktree) if worktree.prunable => "(+ worktree, missing)".to_string(),
            Some(_) if risk.dirty => format!("(+ worktree {})", marker::Marker::Dirty),
            Some(_) => "(+ worktree)".to_string(),
        };
        let branch_risk = Risk {
            dirty: false,
            unmerged: risk.unmerged,
        }
        .markers();
        let ground = stale_ground_label(stale.ground);
        let follows = !worktree_label.is_empty() || !branch_risk.is_empty();
        let ground = style(format!(
            "{ground:<width$}",
            width = if follows { ground_width } else { 0 }
        ))
        .dim()
        .to_string();
        let annotation = [ground, worktree_label, branch_risk]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        raw.push((stale.name.clone(), annotation));
        let target = match worktree {
            Some(worktree) => OwnedTarget::Held {
                name: stale.name,
                path: worktree.path,
            },
            None => OwnedTarget::Branch { name: stale.name },
        };
        locals.push(AssessedLocal {
            target,
            risk,
            proof,
            named_error: None,
            contains_cwd: false,
        });
    }
    let labels = picker::align_labels(&raw);
    let offers: Vec<Offer> = labels
        .into_iter()
        .enumerate()
        .map(|(index, label)| Offer {
            id: LocalId(index),
            name: locals[index].target.offer_name(),
            selected: request.old_branch.as_deref() == locals[index].target.name(),
            disabled: false,
            label,
        })
        .collect();
    let legend = risk_legend(locals.iter().map(|local| local.risk));
    Assessment {
        kind: RequestKind::Stale,
        offers,
        locals,
        legend,
        main,
        worktree_target: None,
        upstream: UpstreamInterest::None,
    }
}

fn stale_ground_label(ground: git::Ground) -> &'static str {
    match ground {
        git::Ground::Gone => "gone",
        git::Ground::TracksAnchorAhead => "tracks anchor, ahead",
        git::Ground::UntrackedTipInAnchor => "untracked, tip in anchor",
    }
}

fn assess_branches(request: BranchRequest) -> AppResult<Assessment> {
    let main = request
        .worktrees
        .iter()
        .find(|worktree| worktree.is_main)
        .ok_or_else(|| Error::Git {
            command: "worktree list".into(),
            message: "main worktree not found".into(),
        })?
        .path
        .clone();
    let unmerged = git::unmerged_branches(Some(&main)).unwrap_or_default();
    let mut raw = Vec::new();
    let mut locals = Vec::new();
    for name in request.branches {
        // Being held is the whole of why a branch row is inert, so the row is
        // drawn from the error naming it would raise rather than from a second
        // reading of the same fact.
        let named_error = git::worktree_for_branch(&request.worktrees, &name).map(NamedError::Held);
        let risk = Risk {
            dirty: false,
            unmerged: unmerged.get(&name).copied(),
        };
        let annotation = if request.current.as_deref() == Some(&name) {
            "current".to_string()
        } else if let Some(NamedError::Held(holder)) = &named_error {
            let path = display_path(&holder.path);
            if holder.is_main {
                format!("main worktree at {path}")
            } else if holder.prunable {
                format!("missing worktree at {path}; use wt rm")
            } else {
                format!("{path}; use wt rm")
            }
        } else {
            String::new()
        };
        let markers = if named_error.is_some() {
            String::new()
        } else {
            risk.markers()
        };
        let detail = [annotation, markers]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        raw.push((name.clone(), detail));
        locals.push(AssessedLocal {
            target: OwnedTarget::Branch { name },
            risk,
            proof: None,
            named_error,
            contains_cwd: false,
        });
    }
    let labels = picker::align_labels(&raw);
    let offers: Vec<Offer> = labels
        .into_iter()
        .enumerate()
        .map(|(index, label)| Offer {
            id: LocalId(index),
            name: locals[index].target.offer_name(),
            label,
            selected: false,
            disabled: locals[index].disabled(),
        })
        .collect();
    let legend = risk_legend(
        locals
            .iter()
            .filter(|local| !local.disabled())
            .map(|local| local.risk),
    );
    Ok(Assessment {
        kind: RequestKind::Branches,
        offers,
        locals,
        legend,
        main: Some(main),
        worktree_target: None,
        upstream: request.upstream,
    })
}

fn assess_worktrees(request: WorktreeRequest) -> AppResult<Assessment> {
    build_worktree_assessment(request, git::worktree_dirty)
}

/// Every name `wt rm` accepts for one worktree: its branch, and the final
/// component of its path. The path name lets callers target a detached or
/// Missing worktree. A renamed worktree, or a nested branch such as `feat/x`
/// in a directory named `x`, answers to both names.
///
/// Named assessment resolves this rule once and stores the matching identity.
/// Shell completion uses the same accepted names without resolving a target.
pub(crate) fn worktree_names<'a>(
    branch: Option<&'a str>,
    path: &'a Path,
) -> impl Iterator<Item = &'a str> {
    branch
        .into_iter()
        .chain(path.file_name().and_then(|part| part.to_str()))
}

fn resolve_worktree_target(
    removable: &[(git::Worktree, bool)],
    current: Option<LocalId>,
    target: Option<&str>,
) -> Option<WorktreeTargetResolution> {
    let target = target?;
    let id = if target == "." {
        current
    } else {
        removable
            .iter()
            .position(|(worktree, _)| {
                worktree_names(worktree.branch.as_deref(), &worktree.path)
                    .any(|name| name == target)
            })
            .map(LocalId)
    };
    Some(WorktreeTargetResolution {
        target: target.to_string(),
        id,
    })
}

fn build_worktree_assessment(
    request: WorktreeRequest,
    mut worktree_dirty: impl FnMut(&Path) -> bool,
) -> AppResult<Assessment> {
    let main = request
        .worktrees
        .iter()
        .find(|worktree| worktree.is_main)
        .ok_or_else(|| Error::Git {
            command: "worktree list".into(),
            message: "no main worktree found".into(),
        })?
        .path
        .clone();
    let contains_cwd = |worktree: &git::Worktree| {
        let path = worktree
            .path
            .canonicalize()
            .unwrap_or_else(|_| worktree.path.clone());
        request
            .cwd
            .as_ref()
            .is_some_and(|cwd| cwd.starts_with(path))
    };
    let removable: Vec<(git::Worktree, bool)> = request
        .worktrees
        .into_iter()
        .filter(|worktree| !worktree.is_main)
        .map(|worktree| {
            let contains_cwd = contains_cwd(&worktree);
            (worktree, contains_cwd)
        })
        .collect();
    let current = removable
        .iter()
        .enumerate()
        .filter(|(_, (_, contains_cwd))| *contains_cwd)
        .max_by_key(|(_, (worktree, _))| worktree.path.as_os_str().len())
        .map(|(index, _)| LocalId(index));
    let worktree_target = resolve_worktree_target(&removable, current, request.target.as_deref());
    let unmerged = git::unmerged_branches(Some(&main)).unwrap_or_default();
    let (mut raw, mut locals) = (Vec::new(), Vec::new());
    for (index, (worktree, contains_cwd)) in removable.into_iter().enumerate() {
        let assess_dirtiness = request.target.is_none()
            || (request.forcing == Forcing::Unforced
                && worktree_target.as_ref().and_then(|resolved| resolved.id)
                    == Some(LocalId(index)));
        let risk = Risk {
            // INVARIANT: Named assessments never render unassessed dirtiness.
            // Non-target rows are unreachable, and --force consumes no warnings.
            dirty: assess_dirtiness && !worktree.prunable && worktree_dirty(&worktree.path),
            unmerged: worktree
                .branch
                .as_deref()
                .and_then(|branch| unmerged.get(branch).copied()),
        };
        let mut name = worktree
            .branch
            .clone()
            .unwrap_or_else(|| display_path(&worktree.path));
        if worktree.prunable {
            name.push_str(" (missing)");
        }
        if current == Some(LocalId(index)) {
            name.push_str(" (current)");
        }
        raw.push((name, risk.markers()));
        let target = match worktree.branch {
            Some(name) => OwnedTarget::Held {
                name,
                path: worktree.path,
            },
            None => OwnedTarget::Worktree {
                path: worktree.path,
            },
        };
        locals.push(AssessedLocal {
            target,
            risk,
            proof: None,
            named_error: None,
            contains_cwd,
        });
    }
    let labels = picker::align_labels(&raw);
    let offers = labels
        .into_iter()
        .enumerate()
        .map(|(index, label)| Offer {
            id: LocalId(index),
            name: locals[index].target.offer_name(),
            label,
            selected: false,
            disabled: false,
        })
        .collect();
    let legend = risk_legend(locals.iter().map(|local| local.risk));
    Ok(Assessment {
        kind: RequestKind::Worktrees,
        offers,
        locals,
        legend,
        main: Some(main),
        worktree_target,
        upstream: UpstreamInterest::None,
    })
}

/// What is being removed. Every case names something real, so "neither a branch
/// nor a worktree" cannot be asked for. It borrows from the row or worktree the
/// caller is already holding to render the outcome from, and travels on in the
/// [`Report`] so the wording never has to be told twice which steps could run.
#[derive(Clone, Copy, Debug)]
enum Target<'a> {
    /// A branch no worktree holds.
    Branch { name: &'a str },
    /// A branch and the worktree *holding* it, which go together.
    Held { name: &'a str, path: &'a Path },
    /// A worktree with no branch to delete alongside it (a detached one).
    Worktree { path: &'a Path },
}

impl<'a> Target<'a> {
    fn branch(self) -> Option<&'a str> {
        match self {
            Target::Branch { name } | Target::Held { name, .. } => Some(name),
            Target::Worktree { .. } => None,
        }
    }

    fn path(self) -> Option<&'a Path> {
        match self {
            Target::Branch { .. } => None,
            Target::Held { path, .. } | Target::Worktree { path } => Some(path),
        }
    }
}

/// The worktree half of a [`License`]: on what authority the delete may discard
/// files. A shown dirty marker is deliberately distinct from explicit force:
/// both cover dirty files, but only the latter may override an unrelated git
/// guard such as an initialized submodule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorktreeLicense {
    None,
    Marked,
    Forced,
}

impl WorktreeLicense {
    fn permits_file_discard(self) -> bool {
        !matches!(self, Self::None)
    }

    fn permits_guard_override(self) -> bool {
        matches!(self, Self::Forced)
    }
}

/// The branch half of a [`License`]: on what authority the delete may discard
/// commits.
#[derive(Clone, Debug, Eq, PartialEq)]
enum BranchLicense {
    /// Nothing licenses it, so git's own guard decides.
    None,
    /// Licensed outright, on something already settled before the delete began:
    /// a *Marker* the user has seen, or an explicit `--force`. The two are
    /// distinct sources of *License* in the glossary and this does not conflate
    /// them — it says only that neither is conditional on anything still true.
    Outright,
    /// Proof that the branch is *Equivalent*, which is conditional: per [ADR
    /// 0005](../../../docs/adr/0005-proof-of-equivalence-is-a-license.md) a license
    /// covers what was established and nothing more, so it lapses the moment
    /// either the branch or the anchor moves off what was proven.
    Proven(git::Proof),
}

/// What licenses forcing: a warning the user has already seen (ADR 0001), proof
/// that a branch is *Equivalent* (ADR 0005), or an explicit `--force`. It has no
/// public fields and exactly three constructors, so forcing something nobody was
/// warned about and nothing was proven of is unrepresentable rather than merely
/// against convention.
struct License {
    worktree: WorktreeLicense,
    branch: BranchLicense,
}

impl License {
    /// The risk the user was warned about — as row markers in a picker, or as
    /// the confirmation that stands in for them where a target was named on the
    /// command line. A *dirty* worktree licenses discarding its files, an
    /// *unmerged* branch its commits, and nothing licenses anything else: a risk
    /// that arose after the warning was given is absent here, so git's own guard
    /// refuses instead.
    pub(crate) fn shown(risk: Risk) -> Self {
        Self {
            worktree: if risk.dirty {
                WorktreeLicense::Marked
            } else {
                WorktreeLicense::None
            },
            branch: if risk.unmerged.is_some() {
                BranchLicense::Outright
            } else {
                BranchLicense::None
            },
        }
    }

    /// A branch proven *Equivalent*, with the worktree half still answering to
    /// the risk shown. Nothing was warned of about the branch — that is the
    /// point of the proof — so the proof stands in for the marker, and only for
    /// as long as what it was established on still holds.
    pub(crate) fn proven(risk: Risk, proof: &git::Proof) -> Self {
        Self {
            branch: BranchLicense::Proven(proof.clone()),
            ..Self::shown(risk)
        }
    }

    /// An explicit removal `--force`, blanket over both local steps.
    pub(crate) fn forced() -> Self {
        Self {
            worktree: WorktreeLicense::Forced,
            branch: BranchLicense::Outright,
        }
    }
}

/// What happened, one field per step. `None` means the step never ran: because
/// the target had nothing for it to act on, or — for the branch — because the
/// worktree refused to go and left it alone. It carries the [`Target`] so
/// the reporting code can word the outcome from the report alone.
#[derive(Debug)]
struct Report<'a> {
    target: Target<'a>,
    worktree: Option<git::WorktreeRemoveOutcome>,
    branch: Option<git::BranchDeleteOutcome>,
}

impl Report<'_> {
    /// Whether the worktree itself went. False covers both a target that never
    /// had one and one git refused to remove — in either case there is no
    /// directory to report gone.
    pub(crate) fn worktree_removed(&self) -> bool {
        matches!(self.worktree, Some(git::WorktreeRemoveOutcome::Removed))
    }

    pub(crate) fn branch_removed(&self) -> bool {
        matches!(
            self.branch,
            Some(
                git::BranchDeleteOutcome::Deleted
                    | git::BranchDeleteOutcome::DeletedLeavingConfig(_)
                    | git::BranchDeleteOutcome::DeletedConfigUnverified(_)
            )
        )
    }
}

/// What a removal asks of git: the two operations it performs, and the one
/// question it asks before deciding to force. Putting them behind a trait lets
/// the ordering and licensing rules be driven by scripted outcomes in tests,
/// exactly as the key source drives the interactive pickers; [`GitSteps`] is the
/// real implementation.
trait Steps {
    /// What `refname` points at *now* — asked to check a proof still covers what
    /// it was established on, so it must read the repo rather than anything
    /// remembered.
    fn resolve(&mut self, refname: &str) -> Option<String>;

    fn delete_branch(&mut self, branch: &str, force: bool) -> AppResult<git::BranchDeleteOutcome>;

    /// Delete `branch` only while it still stands at `expected`, in one
    /// operation. `None` means it had moved and nothing was deleted.
    fn delete_branch_at(
        &mut self,
        branch: &str,
        expected: &str,
    ) -> AppResult<Option<git::BranchDeleteOutcome>>;

    fn remove_worktree(
        &mut self,
        path: &Path,
        force: bool,
    ) -> AppResult<git::WorktreeRemoveOutcome>;

    fn worktree_state(&mut self, path: &Path) -> FreshWorktree;

    fn stage_worktree(&mut self, path: &Path) -> AppResult<reclamation::Staged>;

    fn restore_worktree(&mut self, staged: &reclamation::Staged) -> AppResult<()>;

    fn start_reclamation(&mut self, staged: &reclamation::Staged) -> AppResult<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FreshWorktree {
    Clean,
    Dirty,
    DirtinessUnreadable,
    Guarded,
    Missing,
    SubmodulesUnreadable,
}

/// The real steps, run against the repo on disk.
///
/// It carries the main worktree so no call site has to remember to: `git branch
/// -d` judges merged-ness against the HEAD it runs under, and the main worktree
/// is both where the markers were measured from and the one worktree that can
/// never be the one being removed.
struct GitSteps {
    main: Option<PathBuf>,
}

impl GitSteps {
    fn at_main(main: Option<&Path>) -> Self {
        Self {
            main: main.map(Path::to_path_buf),
        }
    }
}

impl Steps for GitSteps {
    fn resolve(&mut self, refname: &str) -> Option<String> {
        git::resolve(self.main.as_deref(), refname)
    }

    fn delete_branch(&mut self, branch: &str, force: bool) -> AppResult<git::BranchDeleteOutcome> {
        let dir = self.main.as_deref();
        if force {
            git::force_delete_branch(dir, branch)
        } else {
            git::delete_branch_if_merged(dir, branch)
        }
    }

    fn delete_branch_at(
        &mut self,
        branch: &str,
        expected: &str,
    ) -> AppResult<Option<git::BranchDeleteOutcome>> {
        git::delete_branch_at(self.main.as_deref(), branch, expected)
    }

    fn remove_worktree(
        &mut self,
        path: &Path,
        force: bool,
    ) -> AppResult<git::WorktreeRemoveOutcome> {
        let outcome = git::worktree_remove(path, force)?;
        // A staged worktree's trash still fills its parent, so this prunes
        // nothing until the reclamation worker has deleted it.
        if let (git::WorktreeRemoveOutcome::Removed, Some(main)) = (&outcome, &self.main) {
            super::wt::prune_empty_parents(path, main);
        }
        Ok(outcome)
    }

    fn worktree_state(&mut self, path: &Path) -> FreshWorktree {
        if path.exists() {
            match git::worktree_has_initialized_submodules(path) {
                Some(true) => return FreshWorktree::Guarded,
                Some(false) => {}
                None => return FreshWorktree::SubmodulesUnreadable,
            }
            match git::worktree_dirtiness(path) {
                Some(true) => FreshWorktree::Dirty,
                Some(false) => FreshWorktree::Clean,
                None => FreshWorktree::DirtinessUnreadable,
            }
        } else {
            FreshWorktree::Missing
        }
    }

    fn stage_worktree(&mut self, path: &Path) -> AppResult<reclamation::Staged> {
        reclamation::stage(path)
    }

    fn restore_worktree(&mut self, staged: &reclamation::Staged) -> AppResult<()> {
        reclamation::restore(staged)
    }

    fn start_reclamation(&mut self, staged: &reclamation::Staged) -> AppResult<()> {
        reclamation::start(staged)
    }
}

/// Removes `target`, forcing only what `license` covers.
///
/// The worktree goes first — git will not delete a branch something still holds
/// — and one that refuses to go leaves its branch alone, which the returned
/// [`Report`] shows as an absent branch step. Git refusing is a value either
/// way; only a git process that cannot be spawned is an error.
fn remove<'a>(
    target: Target<'a>,
    license: &License,
    reclamation: Reclamation,
    steps: &mut impl Steps,
) -> AppResult<Report<'a>> {
    let mut report = Report {
        target,
        worktree: None,
        branch: None,
    };

    if let Some(path) = target.path() {
        let worktree = match reclamation {
            Reclamation::Synchronous => {
                let state = steps.worktree_state(path);
                steps.remove_worktree(path, worktree_force(state, license.worktree))?
            }
            Reclamation::Background => remove_worktree_in_background(path, license, steps)?,
        };
        report.worktree = Some(worktree);
        if !report.worktree_removed() {
            return Ok(report);
        }
    }

    if let Some(branch) = target.branch() {
        // A proof is re-checked rather than trusted. It was established on two
        // things — where the branch stood and what the anchor held — and a
        // license covers both or neither: a branch that moved has work nobody
        // proved, and an anchor rewound out from under it (by a removal hook on
        // an earlier row, say) no longer holds the content that made the branch
        // safe to discard. Either lapse drops to `-d`, to meet git's own guard
        // exactly as an unmarked worktree does.
        //
        // The branch half is checked *by* the delete rather than before it:
        // `delete_branch_at` compares and deletes in one operation, so a branch
        // that grows a commit in between is not discarded unwarned. The anchor
        // is checked here, since no single git command can speak for two refs.
        let outcome = match &license.branch {
            BranchLicense::Outright => steps.delete_branch(branch, true),
            BranchLicense::Proven(proof)
                if steps.resolve(&proof.anchor_ref).as_ref() == Some(&proof.anchor_tip) =>
            {
                match steps.delete_branch_at(branch, &proof.tip)? {
                    Some(outcome) => Ok(outcome),
                    None => steps.delete_branch(branch, false),
                }
            }
            BranchLicense::None | BranchLicense::Proven(_) => steps.delete_branch(branch, false),
        };
        report.branch = Some(outcome?);
    }

    Ok(report)
}

fn worktree_force(state: FreshWorktree, license: WorktreeLicense) -> bool {
    license.permits_guard_override()
        || matches!(
            state,
            FreshWorktree::Dirty | FreshWorktree::DirtinessUnreadable
        ) && license.permits_file_discard()
}

fn remove_worktree_in_background(
    path: &Path,
    license: &License,
    steps: &mut impl Steps,
) -> AppResult<git::WorktreeRemoveOutcome> {
    let state = steps.worktree_state(path);
    let fallback_force = worktree_force(state, license.worktree);
    let may_stage = match state {
        FreshWorktree::Clean => true,
        FreshWorktree::Dirty | FreshWorktree::DirtinessUnreadable => {
            license.worktree.permits_file_discard()
        }
        FreshWorktree::Guarded | FreshWorktree::Missing | FreshWorktree::SubmodulesUnreadable => {
            false
        }
    };
    if !may_stage {
        return steps.remove_worktree(path, fallback_force);
    }

    let staged = steps.stage_worktree(path)?;
    let removal = steps.remove_worktree(path, false);
    match removal {
        Ok(git::WorktreeRemoveOutcome::Removed) => {
            // Deregistration is the Removal boundary. The durable staged record
            // lets a later `wt` retry if the detached worker cannot start.
            let _ = steps.start_reclamation(&staged);
            Ok(git::WorktreeRemoveOutcome::Removed)
        }
        Ok(failed @ git::WorktreeRemoveOutcome::Failed(_)) => {
            steps.restore_worktree(&staged)?;
            Ok(failed)
        }
        Err(error) => {
            steps.restore_worktree(&staged)?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::collections::HashMap;
    use std::env;
    use std::fs;
    use std::process::Command;
    use std::sync::{Mutex, PoisonError};

    use super::*;
    use tempfile::TempDir;

    static REPO_LOCK: Mutex<()> = Mutex::new(());

    struct Repo {
        root: TempDir,
        previous: PathBuf,
    }

    impl Repo {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temporary repository");
            run_git(root.path(), &["init", "-b", "main"]);
            run_git(root.path(), &["config", "user.email", "perch@example.test"]);
            run_git(root.path(), &["config", "user.name", "Perch Tests"]);
            run_git(root.path(), &["config", "perch.hook.removed", ""]);
            fs::write(root.path().join("base.txt"), "base\n").expect("write base file");
            run_git(root.path(), &["add", "base.txt"]);
            run_git(root.path(), &["commit", "-m", "base"]);
            let previous = env::current_dir().expect("current directory");
            env::set_current_dir(root.path()).expect("enter temporary repository");
            Self { root, previous }
        }

        fn path(&self) -> &Path {
            self.root.path()
        }

        fn add_feature(&self) {
            run_git(self.path(), &["checkout", "-b", "feature"]);
            fs::write(self.path().join("feature.txt"), "feature\n").expect("write feature file");
            run_git(self.path(), &["add", "feature.txt"]);
            run_git(self.path(), &["commit", "-m", "feature"]);
            run_git(self.path(), &["checkout", "main"]);
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            env::set_current_dir(&self.previous).expect("restore current directory");
        }
    }

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|line| console::strip_ansi_codes(line).into_owned())
            .collect()
    }

    fn plain_marker(risk: Risk) -> String {
        console::strip_ansi_codes(&risk.markers()).into_owned()
    }

    #[test]
    fn risk_markers_keep_dirty_and_unmerged_facts_together() {
        assert_eq!(
            plain_marker(Risk {
                dirty: true,
                unmerged: Some(git::Unmerged::Ahead(2)),
            }),
            "● ↑2"
        );
        assert_eq!(
            plain_marker(Risk {
                dirty: false,
                unmerged: Some(git::Unmerged::NoUpstream),
            }),
            "↑"
        );
        assert_eq!(Risk::default().markers(), "");
    }

    #[test]
    fn stale_removal_never_offers_the_handoff_destination() {
        let assessment = build_stale_assessment(
            StaleRequest::new(
                vec![
                    git::StaleBranch {
                        ground: git::Ground::UntrackedTipInAnchor,
                        name: "feature".to_string(),
                    },
                    git::StaleBranch {
                        ground: git::Ground::UntrackedTipInAnchor,
                        name: "fix/typo".to_string(),
                    },
                ],
                Vec::new(),
                "origin",
                None,
                Some("feature"),
            ),
            None,
            &HashMap::new(),
            &HashMap::new(),
            |_| false,
        );

        let names: Vec<&str> = assessment
            .locals
            .iter()
            .filter_map(|local| local.target.name())
            .collect();
        assert_eq!(names, ["fix/typo"]);
    }

    fn test_worktree(branch: &str, path: &str) -> git::Worktree {
        git::Worktree {
            path: PathBuf::from(path),
            branch: Some(branch.to_string()),
            is_main: false,
            prunable: false,
        }
    }

    fn with_main_worktree(mut worktrees: Vec<git::Worktree>) -> Vec<git::Worktree> {
        worktrees.insert(
            0,
            git::Worktree {
                path: PathBuf::from("/tmp/main"),
                branch: Some("main".to_string()),
                is_main: true,
                prunable: false,
            },
        );
        worktrees
    }

    fn test_worktree_assessment(
        worktrees: Vec<git::Worktree>,
        cwd: Option<&str>,
        target: &str,
    ) -> Assessment {
        assess_worktrees(WorktreeRequest::new(
            with_main_worktree(worktrees),
            cwd.map(PathBuf::from),
            Some(target),
            Forcing::Unforced,
        ))
        .expect("worktree assessment")
    }

    #[test]
    fn forced_named_assessment_does_not_read_worktree_dirtiness() {
        let worktrees = with_main_worktree(vec![
            test_worktree("feature", "/tmp/worktrees/feature"),
            test_worktree("other", "/tmp/worktrees/other"),
        ]);
        let mut dirty_paths = Vec::new();

        let assessment = build_worktree_assessment(
            WorktreeRequest::new(worktrees, None, Some("feature"), Forcing::Forced),
            |path| {
                dirty_paths.push(path.to_path_buf());
                true
            },
        )
        .expect("worktree assessment");
        let named = assessment.named("feature").expect("named worktree");
        assessment
            .choose(LocalChoice::forced(named.id()))
            .expect("forced Removal");

        assert_eq!(dirty_paths, Vec::<PathBuf>::new());
    }

    #[test]
    fn dot_assessment_reads_only_the_deepest_containing_worktrees_dirtiness() {
        let worktrees = with_main_worktree(vec![
            test_worktree("outer", "/tmp/worktrees/outer"),
            test_worktree("inner", "/tmp/worktrees/outer/inner"),
        ]);
        let mut dirty_paths = Vec::new();

        let assessment = build_worktree_assessment(
            WorktreeRequest::new(
                worktrees,
                Some(PathBuf::from("/tmp/worktrees/outer/inner/src")),
                Some("."),
                Forcing::Unforced,
            ),
            |path| {
                dirty_paths.push(path.to_path_buf());
                false
            },
        )
        .expect("worktree assessment");
        assessment.named(".").expect("current worktree");

        assert_eq!(dirty_paths, [PathBuf::from("/tmp/worktrees/outer/inner")]);
    }

    #[test]
    fn branch_target_resolves_when_the_directory_name_differs() {
        let assessment = test_worktree_assessment(
            vec![test_worktree(
                "feat/login",
                "/tmp/worktrees/repo/differently-named",
            )],
            None,
            "feat/login",
        );

        let named = assessment.named("feat/login").expect("branch target");

        assert_eq!(assessment.offer(named.id()).name(), "feat/login");
    }

    #[test]
    fn directory_target_reports_the_dirtiness_of_the_resolved_worktree() {
        let worktrees = with_main_worktree(vec![
            test_worktree("feat/login", "/tmp/worktrees/repo/login"),
            test_worktree("login", "/tmp/worktrees/repo/other"),
        ]);
        let mut dirty_paths = Vec::new();

        let assessment = build_worktree_assessment(
            WorktreeRequest::new(worktrees, None, Some("login"), Forcing::Unforced),
            |path| {
                dirty_paths.push(path.to_path_buf());
                true
            },
        )
        .expect("worktree assessment");
        let named = assessment.named("login").expect("directory target");

        assert_eq!(dirty_paths, [PathBuf::from("/tmp/worktrees/repo/login")]);
        assert_eq!(
            plain(named.warnings()),
            ["! /tmp/worktrees/repo/login has uncommitted changes"]
        );
    }

    #[test]
    fn picker_assessment_reads_every_non_missing_worktrees_dirtiness() {
        let mut missing = test_worktree("missing", "/tmp/worktrees/missing");
        missing.prunable = true;
        let worktrees = with_main_worktree(vec![
            test_worktree("feature", "/tmp/worktrees/feature"),
            test_worktree("other", "/tmp/worktrees/other"),
            missing,
        ]);
        let mut dirty_paths = Vec::new();

        build_worktree_assessment(
            WorktreeRequest::new(worktrees, None, None, Forcing::Unforced),
            |path| {
                dirty_paths.push(path.to_path_buf());
                false
            },
        )
        .expect("worktree assessment");

        assert_eq!(
            dirty_paths,
            [
                PathBuf::from("/tmp/worktrees/feature"),
                PathBuf::from("/tmp/worktrees/other"),
            ]
        );
    }

    #[test]
    fn picker_assessment_keeps_every_marker_and_the_legend_for_both_forcing_states() {
        for forcing in [Forcing::Forced, Forcing::Unforced] {
            let worktrees = with_main_worktree(vec![
                test_worktree("feature", "/tmp/worktrees/feature"),
                test_worktree("other", "/tmp/worktrees/other"),
            ]);

            let assessment = build_worktree_assessment(
                WorktreeRequest::new(worktrees, None, None, forcing),
                |path| path.ends_with("feature"),
            )
            .expect("worktree assessment");
            let labels: Vec<String> = assessment
                .offers()
                .iter()
                .map(|offer| console::strip_ansi_codes(&offer.label).into_owned())
                .collect();
            let legend = assessment
                .legend()
                .map(console::strip_ansi_codes)
                .map(std::borrow::Cow::into_owned);

            assert_eq!(
                (labels, legend),
                (
                    vec!["feature  ●".to_string(), "other".to_string()],
                    Some("● uncommitted changes".to_string()),
                ),
                "{forcing:?}",
            );
        }
    }

    #[test]
    fn dot_names_the_deepest_worktree_containing_the_cwd() {
        let assessment = test_worktree_assessment(
            vec![
                test_worktree("outer", "/tmp/worktrees/outer"),
                test_worktree("inner", "/tmp/worktrees/outer/inner"),
            ],
            Some("/tmp/worktrees/outer/inner/src"),
            ".",
        );

        let named = assessment.named(".").expect("current worktree");

        assert_eq!(assessment.offer(named.id()).name(), "inner");
    }

    #[test]
    fn directory_target_keeps_the_assessed_branch_name() {
        let assessment = test_worktree_assessment(
            vec![test_worktree("feat/login", "/tmp/worktrees/repo/login")],
            None,
            "login",
        );

        let named = assessment.named("login").expect("directory target");

        assert_eq!(assessment.offer(named.id()).name(), "feat/login");
    }

    #[test]
    fn detached_worktree_offer_uses_its_display_path_as_the_name() {
        let path = PathBuf::from("/tmp/worktrees/repo/detached");
        let assessment = test_worktree_assessment(
            vec![git::Worktree {
                path: path.clone(),
                branch: None,
                is_main: false,
                prunable: false,
            }],
            None,
            "detached",
        );

        let named = assessment.named("detached").expect("detached worktree");

        assert_eq!(assessment.offer(named.id()).name(), display_path(&path));
    }

    #[test]
    fn stale_ground_without_following_detail_has_no_trailing_padding() {
        let assessment = build_stale_assessment(
            StaleRequest::new(
                vec![git::StaleBranch {
                    ground: git::Ground::UntrackedTipInAnchor,
                    name: "fix/typo".to_string(),
                }],
                Vec::new(),
                "origin",
                None,
                None,
            ),
            None,
            &HashMap::new(),
            &HashMap::new(),
            |_| false,
        );

        let label = console::strip_ansi_codes(&assessment.offers[0].label).into_owned();
        assert!(
            label.ends_with("untracked, tip in anchor"),
            "ground-only row: {label:?}"
        );
        assert_eq!(label.trim_end(), label);
    }

    #[test]
    fn stale_label_marks_a_missing_worktree() {
        let mut missing = test_worktree("old/thing", "/tmp/missing");
        missing.prunable = true;
        let assessment = build_stale_assessment(
            StaleRequest::new(
                vec![git::StaleBranch {
                    ground: git::Ground::UntrackedTipInAnchor,
                    name: "old/thing".to_string(),
                }],
                vec![missing],
                "origin",
                None,
                None,
            ),
            None,
            &HashMap::new(),
            &HashMap::new(),
            |_| false,
        );

        let label = console::strip_ansi_codes(&assessment.offers[0].label).into_owned();
        assert!(
            label.contains("untracked, tip in anchor (+ worktree, missing)"),
            "missing worktree label: {label:?}"
        );
    }

    #[test]
    fn stale_labels_align_each_ground_before_the_worktree_detail() {
        let assessment = build_stale_assessment(
            StaleRequest::new(
                vec![
                    git::StaleBranch {
                        ground: git::Ground::Gone,
                        name: "gone-held".to_string(),
                    },
                    git::StaleBranch {
                        ground: git::Ground::TracksAnchorAhead,
                        name: "ahead-held".to_string(),
                    },
                    git::StaleBranch {
                        ground: git::Ground::UntrackedTipInAnchor,
                        name: "untracked-held".to_string(),
                    },
                ],
                vec![
                    test_worktree("gone-held", "/tmp/gone"),
                    test_worktree("ahead-held", "/tmp/ahead"),
                    test_worktree("untracked-held", "/tmp/untracked"),
                ],
                "origin",
                None,
                None,
            ),
            None,
            &HashMap::new(),
            &HashMap::new(),
            |_| false,
        );

        let labels: Vec<String> = assessment
            .offers
            .iter()
            .map(|offer| console::strip_ansi_codes(&offer.label).into_owned())
            .collect();
        assert!(
            labels[0].contains("gone                     (+ worktree)")
                && labels[1].contains("tracks anchor, ahead     (+ worktree)")
                && labels[2].contains("untracked, tip in anchor (+ worktree)"),
            "grounds should align their following detail: {labels:?}"
        );
    }

    #[test]
    fn stale_label_places_worktree_and_branch_risks_after_the_ground() {
        let assessment = build_stale_assessment(
            StaleRequest::new(
                vec![git::StaleBranch {
                    ground: git::Ground::UntrackedTipInAnchor,
                    name: "feature".to_string(),
                }],
                vec![test_worktree("feature", "/tmp/feature")],
                "origin",
                None,
                None,
            ),
            None,
            &HashMap::from([("feature".to_string(), git::Unmerged::Ahead(2))]),
            &HashMap::new(),
            |path| path == Path::new("/tmp/feature"),
        );

        let label = console::strip_ansi_codes(&assessment.offers[0].label).into_owned();
        assert!(
            label.contains("untracked, tip in anchor (+ worktree ●) ↑2"),
            "ground and risks should keep their established order: {label}"
        );
    }

    #[test]
    fn risk_legend_names_only_the_markers_present() {
        let dirty = Risk {
            dirty: true,
            unmerged: None,
        };
        let unmerged = Risk {
            dirty: false,
            unmerged: Some(git::Unmerged::NoUpstream),
        };
        assert_eq!(
            console::strip_ansi_codes(&risk_legend([dirty]).expect("dirty risk has a legend")),
            "● uncommitted changes"
        );
        assert_eq!(
            console::strip_ansi_codes(
                &risk_legend([unmerged]).expect("unmerged risk has a legend")
            ),
            "↑ unmerged commits"
        );

        let legend = risk_legend([dirty, unmerged]).expect("visible risks have a legend");

        assert_eq!(
            console::strip_ansi_codes(&legend),
            "● uncommitted changes   ↑ unmerged commits"
        );
    }

    #[test]
    fn risk_legend_is_absent_when_no_row_is_at_risk() {
        assert!(risk_legend([Risk::default(), Risk::default()]).is_none());
    }

    #[test]
    fn equivalent_stale_branch_keeps_its_proof_without_an_unmerged_risk() {
        let proof = git::Proof {
            anchor_ref: "refs/heads/main".to_string(),
            anchor_tip: "def".to_string(),
            tip: "abc".to_string(),
        };
        let assessment = build_stale_assessment(
            StaleRequest::new(
                vec![git::StaleBranch {
                    ground: git::Ground::UntrackedTipInAnchor,
                    name: "shipped".to_string(),
                }],
                Vec::new(),
                "origin",
                None,
                None,
            ),
            None,
            &HashMap::from([("shipped".to_string(), git::Unmerged::NoUpstream)]),
            &HashMap::from([("shipped".to_string(), proof.clone())]),
            |_| false,
        );

        let local = &assessment.locals[0];
        assert_eq!(
            (local.proof.as_ref(), local.risk.unmerged),
            (Some(&proof), None)
        );
    }

    #[test]
    fn worktree_target_does_not_match_a_partial_branch_path() {
        let worktrees = with_main_worktree(vec![test_worktree(
            "feat/login",
            "/tmp/worktrees/repo/feat/login",
        )]);
        let assessment = build_worktree_assessment(
            WorktreeRequest::new(worktrees, None, Some("feat"), Forcing::Unforced),
            |_| false,
        )
        .expect("worktree assessment");

        let Err(error) = assessment.named("feat") else {
            panic!("a path segment is not a complete worktree target");
        };
        assert_eq!(
            error.to_string(),
            "git worktree remove: no worktree matching 'feat'"
        );
    }

    /// One recorded call, as the rules care about it: which step, and whether it
    /// was forced.
    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        DeleteBranch {
            force: bool,
        },
        /// The pinned delete, and whether the branch was still there to take it.
        DeleteBranchAt {
            hit: bool,
        },
        RemoveWorktree {
            force: bool,
        },
        ReadWorktree(FreshWorktree),
        StageWorktree,
        RestoreWorktree,
        StartReclamation,
    }

    /// Runs the steps from scripted outcomes and records what it was asked to
    /// do, so ordering and licensing can be asserted without a repo on disk.
    struct FakeSteps {
        worktree: git::WorktreeRemoveOutcome,
        worktree_error: bool,
        worktree_state: FreshWorktree,
        reclamation_error: bool,
        branch: git::BranchDeleteOutcome,
        branch_error: bool,
        /// What the refs read when [`remove`] asks. The proof tests move one out
        /// from under the license and leave the other where it was.
        refs: HashMap<String, String>,
        calls: Vec<Call>,
    }

    impl FakeSteps {
        /// Both steps succeed and both refs sit where [`proof`] says; tests that
        /// care about an outcome, or about something moving, override it.
        fn new() -> Self {
            Self {
                worktree: git::WorktreeRemoveOutcome::Removed,
                worktree_error: false,
                worktree_state: FreshWorktree::Clean,
                reclamation_error: false,
                branch: git::BranchDeleteOutcome::Deleted,
                branch_error: false,
                refs: HashMap::from([
                    ("refs/heads/feature".to_string(), PROVEN_TIP.to_string()),
                    (ANCHOR_REF.to_string(), PROVEN_ANCHOR.to_string()),
                ]),
                calls: Vec::new(),
            }
        }

        /// Move a ref off what the proof was established on.
        fn moved(mut self, refname: &str) -> Self {
            self.refs.insert(refname.to_string(), "moved".to_string());
            self
        }
    }

    const ANCHOR_REF: &str = "refs/heads/main";
    /// What the proof tests establish their license on.
    const PROVEN_TIP: &str = "abc123";
    const PROVEN_ANCHOR: &str = "def456";

    fn proof() -> git::Proof {
        git::Proof {
            anchor_ref: ANCHOR_REF.to_string(),
            anchor_tip: PROVEN_ANCHOR.to_string(),
            tip: PROVEN_TIP.to_string(),
        }
    }

    impl Steps for FakeSteps {
        fn resolve(&mut self, refname: &str) -> Option<String> {
            self.refs.get(refname).cloned()
        }

        fn delete_branch(
            &mut self,
            _branch: &str,
            force: bool,
        ) -> AppResult<git::BranchDeleteOutcome> {
            self.calls.push(Call::DeleteBranch { force });
            if self.branch_error {
                return Err(Error::Git {
                    command: "branch delete".to_string(),
                    message: "could not start git".to_string(),
                });
            }
            Ok(self.branch.clone())
        }

        /// Answers as git's pinned delete does: it goes through only while the
        /// branch still stands where the caller expects.
        fn delete_branch_at(
            &mut self,
            branch: &str,
            expected: &str,
        ) -> AppResult<Option<git::BranchDeleteOutcome>> {
            let hit = self.resolve(&format!("refs/heads/{branch}")).as_deref() == Some(expected);
            self.calls.push(Call::DeleteBranchAt { hit });
            Ok(hit.then(|| self.branch.clone()))
        }

        fn remove_worktree(
            &mut self,
            _path: &Path,
            force: bool,
        ) -> AppResult<git::WorktreeRemoveOutcome> {
            self.calls.push(Call::RemoveWorktree { force });
            if self.worktree_error {
                return Err(Error::Git {
                    command: "worktree remove".to_string(),
                    message: "could not start git".to_string(),
                });
            }
            Ok(self.worktree.clone())
        }

        fn worktree_state(&mut self, _path: &Path) -> FreshWorktree {
            self.calls.push(Call::ReadWorktree(self.worktree_state));
            self.worktree_state
        }

        fn stage_worktree(&mut self, path: &Path) -> AppResult<reclamation::Staged> {
            self.calls.push(Call::StageWorktree);
            Ok(reclamation::Staged {
                config: PathBuf::from("/tmp/config"),
                original: path.to_path_buf(),
                trash: PathBuf::from("/tmp/.perch-trash.wt.1"),
                _lock: None,
            })
        }

        fn restore_worktree(&mut self, _staged: &reclamation::Staged) -> AppResult<()> {
            self.calls.push(Call::RestoreWorktree);
            Ok(())
        }

        fn start_reclamation(&mut self, _staged: &reclamation::Staged) -> AppResult<()> {
            self.calls.push(Call::StartReclamation);
            if self.reclamation_error {
                return Err(Error::Reclamation(
                    "could not start background reclamation".to_string(),
                ));
            }
            Ok(())
        }
    }

    fn held() -> Target<'static> {
        Target::Held {
            name: "feature",
            path: Path::new("/tmp/wt"),
        }
    }

    fn pending_with_local(
        kind: RequestKind,
        target: OwnedTarget,
        contains_cwd: bool,
        main: Option<PathBuf>,
    ) -> Pending {
        Pending {
            kind,
            locals: vec![PlannedLocal {
                target,
                license: License::forced(),
                preparation_failure: None,
                contains_cwd,
                upstream: None,
            }],
            main,
            upstream_offers: Vec::new(),
            notices: Vec::new(),
        }
    }

    struct LineReporter<'a>(&'a mut Vec<String>);

    impl Reporter for LineReporter<'_> {
        fn emit(&mut self, line: String) {
            self.0.push(line);
        }
    }

    struct SilentReporter;

    impl Reporter for SilentReporter {
        fn emit(&mut self, _line: String) {}
    }

    #[derive(Default)]
    struct EventReporter {
        events: Vec<&'static str>,
    }

    impl Reporter for EventReporter {
        fn emit(&mut self, _line: String) {
            self.events.push("outcome");
        }

        fn removed(&mut self, hook: RemovedHook<'_>) {
            self.events.push("hook");
            hook.fire();
        }
    }

    #[test]
    fn branch_removal_crosses_the_staged_interface() {
        let _lock = REPO_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let repo = Repo::new();
        repo.add_feature();
        let worktrees = git::worktree_list().expect("worktrees");
        let branches = git::local_branches().expect("branches");
        let assessment = assess(Request::Branches(BranchRequest::new(
            branches,
            worktrees,
            Some("main"),
            UpstreamInterest::None,
        )))
        .expect("assessment");
        let named = assessment.named("feature").expect("named offer");
        assert_eq!(
            plain(named.warnings()),
            ["! feature has unmerged commits and no upstream"]
        );

        let pending = assessment
            .choose(LocalChoice::forced(named.id()))
            .expect("pending Removal");
        let mut lines = Vec::new();
        let outcome = pending
            .finish(UpstreamChoice::keep(), LineReporter(&mut lines))
            .expect("Removal outcome");

        assert_eq!(plain(&lines), ["✓ deleted feature"]);
        assert!(!outcome.failed());
        assert!(git::resolve(None, "refs/heads/feature").is_none());
    }

    #[test]
    fn forced_picker_worktree_removal_returns_one_complete_outcome() {
        let _lock = REPO_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let repo = Repo::new();
        repo.add_feature();
        let worktree_path = repo.path().join("feature-worktree");
        let worktree_arg = worktree_path.to_string_lossy();
        run_git(repo.path(), &["worktree", "add", &worktree_arg, "feature"]);
        fs::write(worktree_path.join("dirty.txt"), "dirty\n").expect("dirty worktree");
        let displayed_path = worktree_path
            .canonicalize()
            .expect("canonical worktree path");
        let assessment = assess(Request::Worktrees(WorktreeRequest::new(
            git::worktree_list().expect("worktrees"),
            env::current_dir()
                .ok()
                .and_then(|dir| dir.canonicalize().ok()),
            None,
            Forcing::Forced,
        )))
        .expect("assessment");
        let id = assessment
            .offers()
            .iter()
            .find(|offer| console::strip_ansi_codes(&offer.label).starts_with("feature"))
            .map(Offer::id)
            .expect("feature offer");

        let pending = assessment
            .choose(LocalChoice::forced_picked(vec![id]))
            .expect("pending Removal");
        let mut lines = Vec::new();
        let outcome = pending
            .finish(UpstreamChoice::keep(), LineReporter(&mut lines))
            .expect("Removal outcome");

        assert_eq!(
            plain(&lines),
            [format!(
                "✓ removed worktree at {}, deleted feature",
                display_path(&displayed_path)
            )]
        );
        assert!(!outcome.failed());
        assert!(!worktree_path.exists());
        assert!(git::resolve(None, "refs/heads/feature").is_none());
    }

    #[test]
    fn worktree_removal_emits_its_outcome_before_firing_the_hook() {
        let _lock = REPO_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let repo = Repo::new();
        let pending = pending_with_local(
            RequestKind::Worktrees,
            OwnedTarget::Held {
                name: "feature".to_string(),
                path: PathBuf::from("/tmp/wt"),
            },
            false,
            Some(repo.path().to_path_buf()),
        );
        let mut steps = FakeSteps::new();
        let mut reporter = EventReporter::default();

        pending
            .finish_with_steps(UpstreamChoice::keep(), &mut steps, &mut reporter)
            .expect("Removal outcome");

        assert_eq!(reporter.events, ["outcome", "hook"]);
    }

    /// Git will not delete a branch a worktree still holds, so the order is not
    /// a preference.
    #[test]
    fn the_worktree_goes_before_the_branch() {
        let mut steps = FakeSteps::new();
        remove(
            held(),
            &License::shown(Risk::default()),
            Reclamation::Synchronous,
            &mut steps,
        )
        .expect("no git to fail");
        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Clean),
                Call::RemoveWorktree { force: false },
                Call::DeleteBranch { force: false },
            ]
        );
    }

    #[test]
    fn a_clean_worktree_is_staged_deregistered_and_detached_before_its_branch_goes() {
        let mut steps = FakeSteps::new();

        remove(
            held(),
            &License::shown(Risk::default()),
            Reclamation::Background,
            &mut steps,
        )
        .expect("no step to fail");

        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Clean),
                Call::StageWorktree,
                Call::RemoveWorktree { force: false },
                Call::StartReclamation,
                Call::DeleteBranch { force: false },
            ]
        );
    }

    #[test]
    fn a_freshly_dirty_worktree_with_a_license_takes_the_background_path() {
        let mut steps = FakeSteps::new();
        steps.worktree_state = FreshWorktree::Dirty;
        let risk = Risk {
            dirty: true,
            unmerged: None,
        };

        remove(
            held(),
            &License::shown(risk),
            Reclamation::Background,
            &mut steps,
        )
        .expect("no step to fail");

        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Dirty),
                Call::StageWorktree,
                Call::RemoveWorktree { force: false },
                Call::StartReclamation,
                Call::DeleteBranch { force: false },
            ]
        );
    }

    #[test]
    fn an_unlicensed_freshly_dirty_worktree_meets_gits_unforced_guard() {
        let mut steps = FakeSteps::new();
        steps.worktree_state = FreshWorktree::Dirty;
        steps.worktree = git::WorktreeRemoveOutcome::Failed("dirty".to_string());

        let report = remove(
            held(),
            &License::shown(Risk::default()),
            Reclamation::Background,
            &mut steps,
        )
        .expect("git refusal is an outcome");

        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Dirty),
                Call::RemoveWorktree { force: false },
            ]
        );
        assert!(report.branch.is_none());
    }

    #[test]
    fn an_unlicensed_unreadable_fresh_state_meets_gits_unforced_guard() {
        let mut steps = FakeSteps::new();
        steps.worktree_state = FreshWorktree::DirtinessUnreadable;
        steps.worktree = git::WorktreeRemoveOutcome::Failed("unreadable".to_string());

        remove(
            held(),
            &License::shown(Risk::default()),
            Reclamation::Background,
            &mut steps,
        )
        .expect("git refusal is an outcome");

        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::DirtinessUnreadable),
                Call::RemoveWorktree { force: false },
            ]
        );
    }

    #[test]
    fn an_incomplete_safety_read_meets_gits_unforced_guard() {
        let mut steps = FakeSteps::new();
        steps.worktree_state = FreshWorktree::SubmodulesUnreadable;
        steps.worktree = git::WorktreeRemoveOutcome::Failed("uninspectable".to_string());
        let risk = Risk {
            dirty: true,
            unmerged: None,
        };

        remove(
            held(),
            &License::shown(risk),
            Reclamation::Background,
            &mut steps,
        )
        .expect("git refusal is an outcome");

        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::SubmodulesUnreadable),
                Call::RemoveWorktree { force: false },
            ]
        );
    }

    #[test]
    fn a_force_license_covers_an_unreadable_fresh_state() {
        let mut steps = FakeSteps::new();
        steps.worktree_state = FreshWorktree::DirtinessUnreadable;

        remove(
            held(),
            &License::forced(),
            Reclamation::Background,
            &mut steps,
        )
        .expect("the force license covers fresh uncertainty");

        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::DirtinessUnreadable),
                Call::StageWorktree,
                Call::RemoveWorktree { force: false },
                Call::StartReclamation,
                Call::DeleteBranch { force: true },
            ]
        );
    }

    #[test]
    fn a_dirty_marker_does_not_override_an_initialized_submodule_guard() {
        let mut steps = FakeSteps::new();
        steps.worktree_state = FreshWorktree::Guarded;
        steps.worktree = git::WorktreeRemoveOutcome::Failed("submodule".to_string());
        let risk = Risk {
            dirty: true,
            unmerged: None,
        };

        let report = remove(
            held(),
            &License::shown(risk),
            Reclamation::Background,
            &mut steps,
        )
        .expect("git refusal is an outcome");

        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Guarded),
                Call::RemoveWorktree { force: false },
            ]
        );
        assert!(report.branch.is_none());
    }

    #[test]
    fn stale_removal_does_not_let_a_dirty_marker_override_a_submodule_guard() {
        let mut steps = FakeSteps::new();
        steps.worktree_state = FreshWorktree::Guarded;
        steps.worktree = git::WorktreeRemoveOutcome::Failed("submodule".to_string());
        let risk = Risk {
            dirty: true,
            unmerged: None,
        };

        let report = remove(
            held(),
            &License::shown(risk),
            Reclamation::Synchronous,
            &mut steps,
        )
        .expect("git refusal is an outcome");

        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Guarded),
                Call::RemoveWorktree { force: false },
            ]
        );
        assert!(report.branch.is_none());
    }

    #[test]
    fn explicit_force_overrides_an_initialized_submodule_guard() {
        let mut steps = FakeSteps::new();
        steps.worktree_state = FreshWorktree::Guarded;

        remove(
            held(),
            &License::forced(),
            Reclamation::Background,
            &mut steps,
        )
        .expect("explicit force covers the guard");

        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Guarded),
                Call::RemoveWorktree { force: true },
                Call::DeleteBranch { force: true },
            ]
        );
    }

    #[test]
    fn failed_deregistration_restores_the_staged_worktree_and_keeps_the_branch() {
        let mut steps = FakeSteps::new();
        steps.worktree = git::WorktreeRemoveOutcome::Failed("locked".to_string());

        let report = remove(
            held(),
            &License::forced(),
            Reclamation::Background,
            &mut steps,
        )
        .expect("git refusal is an outcome");

        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Clean),
                Call::StageWorktree,
                Call::RemoveWorktree { force: false },
                Call::RestoreWorktree,
            ]
        );
        assert!(report.branch.is_none());
    }

    #[test]
    fn failed_detached_launch_does_not_hide_removal_or_skip_the_branch() {
        let mut steps = FakeSteps::new();
        steps.reclamation_error = true;

        let report = remove(
            held(),
            &License::forced(),
            Reclamation::Background,
            &mut steps,
        )
        .expect("the durable record makes launch failure recoverable");

        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Clean),
                Call::StageWorktree,
                Call::RemoveWorktree { force: false },
                Call::StartReclamation,
                Call::DeleteBranch { force: true },
            ]
        );
        assert!(report.worktree_removed());
        assert!(report.branch_removed());
    }

    #[test]
    fn a_fatal_worktree_error_escapes_the_staged_removal() {
        let pending = pending_with_local(
            RequestKind::Worktrees,
            OwnedTarget::Held {
                name: "feature".to_string(),
                path: PathBuf::from("/tmp/wt"),
            },
            false,
            None,
        );
        let mut steps = FakeSteps::new();
        steps.worktree_error = true;

        let Err(error) =
            pending.finish_with_steps(UpstreamChoice::keep(), &mut steps, &mut SilentReporter)
        else {
            panic!("process-level git errors stay fatal");
        };

        assert_eq!(
            error.into_error().to_string(),
            "git worktree remove: could not start git"
        );
    }

    #[test]
    fn a_fatal_stale_error_escapes_the_staged_removal() {
        let pending = pending_with_local(
            RequestKind::Stale,
            OwnedTarget::Branch {
                name: "feature".to_string(),
            },
            false,
            None,
        );
        let mut steps = FakeSteps::new();
        steps.branch_error = true;

        let Err(error) =
            pending.finish_with_steps(UpstreamChoice::keep(), &mut steps, &mut SilentReporter)
        else {
            panic!("process-level git errors stay fatal during stale Removal");
        };

        assert_eq!(
            error.into_error().to_string(),
            "git branch delete: could not start git"
        );
    }

    #[test]
    fn a_fatal_error_after_removing_the_cwd_worktree_preserves_the_handoff() {
        let _lock = REPO_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let repo = Repo::new();
        let main = repo.path().to_path_buf();
        let pending = pending_with_local(
            RequestKind::Worktrees,
            OwnedTarget::Held {
                name: "feature".to_string(),
                path: PathBuf::from("/tmp/wt"),
            },
            true,
            Some(main.clone()),
        );
        let mut steps = FakeSteps::new();
        steps.branch_error = true;

        let Err(failure) =
            pending.finish_with_steps(UpstreamChoice::keep(), &mut steps, &mut SilentReporter)
        else {
            panic!("the branch process failure should stay fatal");
        };

        assert_eq!(failure.handoff(), Some(main.as_path()));
        assert_eq!(
            failure.into_error().to_string(),
            "git branch delete: could not start git"
        );
    }

    /// A locked worktree, say: deleting its branch would leave a directory with
    /// nothing behind it.
    #[test]
    fn a_worktree_that_refuses_leaves_its_branch_alone() {
        let mut steps = FakeSteps::new();
        steps.worktree = git::WorktreeRemoveOutcome::Failed("locked".to_string());
        let report = remove(
            held(),
            &License::forced(),
            Reclamation::Synchronous,
            &mut steps,
        )
        .expect("no git to fail");
        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Clean),
                Call::RemoveWorktree { force: true },
            ]
        );
        assert_matches!(report.worktree, Some(git::WorktreeRemoveOutcome::Failed(_)));
        assert!(
            report.branch.is_none(),
            "no branch step ran, got: {:?}",
            report.branch
        );
    }

    /// ADR 0001 made structural: a `●` licenses discarding files and an `↑N`
    /// licenses discarding commits, each on its own.
    #[test]
    fn a_license_from_markers_forces_only_what_was_marked() {
        let mut dirty_only = FakeSteps::new();
        dirty_only.worktree_state = FreshWorktree::Dirty;
        let risk = Risk {
            dirty: true,
            unmerged: None,
        };
        remove(
            held(),
            &License::shown(risk),
            Reclamation::Synchronous,
            &mut dirty_only,
        )
        .expect("no git to fail");
        assert_eq!(
            dirty_only.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Dirty),
                Call::RemoveWorktree { force: true },
                Call::DeleteBranch { force: false },
            ]
        );

        let mut unmerged_only = FakeSteps::new();
        let risk = Risk {
            dirty: false,
            unmerged: Some(git::Unmerged::Ahead(2)),
        };
        remove(
            held(),
            &License::shown(risk),
            Reclamation::Synchronous,
            &mut unmerged_only,
        )
        .expect("no git to fail");
        assert_eq!(
            unmerged_only.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Clean),
                Call::RemoveWorktree { force: false },
                Call::DeleteBranch { force: true },
            ]
        );
    }

    /// `wt rm --force` stays blanket over both steps.
    #[test]
    fn an_explicit_force_covers_both_steps() {
        let mut steps = FakeSteps::new();
        remove(
            held(),
            &License::forced(),
            Reclamation::Synchronous,
            &mut steps,
        )
        .expect("no git to fail");
        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Clean),
                Call::RemoveWorktree { force: true },
                Call::DeleteBranch { force: true },
            ]
        );
    }

    /// Proof is the third source of license (ADR 0005): an equivalent branch
    /// draws no marker, so nothing else in its license would force anything, and
    /// the proof alone is what discards the commits git would refuse. It never
    /// reaches `branch -D`, since what it licenses is pinned to a commit.
    #[test]
    fn a_proof_takes_the_pinned_delete_and_not_a_blanket_force() {
        let mut steps = FakeSteps::new();
        remove(
            held(),
            &License::proven(Risk::default(), &proof()),
            Reclamation::Synchronous,
            &mut steps,
        )
        .expect("no git to fail");
        assert_eq!(
            steps.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Clean),
                Call::RemoveWorktree { force: false },
                Call::DeleteBranchAt { hit: true },
            ]
        );
    }

    /// A license covers what was established and nothing more, and equivalence
    /// was established on two things at once. Move either — the branch grows a
    /// commit nobody proved, or the anchor is rewound and no longer holds the
    /// content that made the branch safe to discard — and the delete meets
    /// git's own guard instead.
    ///
    /// The two lapse differently, and deliberately: a branch that moved is
    /// caught *by* the pinned delete, which is what closes the window between
    /// checking and deleting, while an anchor that moved is caught before it,
    /// because no one git command can speak for two refs.
    #[test]
    fn a_proof_lapses_when_either_the_branch_or_the_anchor_moves() {
        let mut branch_moved = FakeSteps::new().moved("refs/heads/feature");
        remove(
            held(),
            &License::proven(Risk::default(), &proof()),
            Reclamation::Synchronous,
            &mut branch_moved,
        )
        .expect("no git to fail");
        assert_eq!(
            branch_moved.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Clean),
                Call::RemoveWorktree { force: false },
                Call::DeleteBranchAt { hit: false },
                Call::DeleteBranch { force: false },
            ],
            "the pinned delete declines, and git's own guard decides instead"
        );

        let mut anchor_moved = FakeSteps::new().moved(ANCHOR_REF);
        remove(
            held(),
            &License::proven(Risk::default(), &proof()),
            Reclamation::Synchronous,
            &mut anchor_moved,
        )
        .expect("no git to fail");
        assert_eq!(
            anchor_moved.calls,
            vec![
                Call::ReadWorktree(FreshWorktree::Clean),
                Call::RemoveWorktree { force: false },
                Call::DeleteBranch { force: false },
            ],
            "the content is no longer on the anchor, so nothing is pinned at all"
        );
    }
}
