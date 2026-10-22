//! Operator-pinned local workspace timeline operations shared by CLI and stdio MCP.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::authority::Authority;
use crate::context::ChangeId;
use crate::store::Engine;
use crate::workspace::Workspace;
use crate::workspace::{SnapshotTrigger, TimelineEntry};
use crate::{Error, Result};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

pub struct WorkspaceTimelineService {
    data: PathBuf,
    root: PathBuf,
    fork_root: Option<PathBuf>,
}

impl WorkspaceTimelineService {
    /// Paths are chosen by the operator, never by a tool request.
    pub fn new(data_dir: &Path, workspace: &Path, fork_root: Option<&Path>) -> Result<Self> {
        let data = data_dir.canonicalize()?;
        let root = workspace.canonicalize()?;
        if nested(&data, &root) || !data.join("state.sqlite").is_file() {
            return Err(Error::Invalid(
                "timeline requires an initialized store outside the workspace".into(),
            ));
        }
        let fork_root = fork_root.map(Path::canonicalize).transpose()?;
        if let Some(parent) = &fork_root
            && (!parent.is_dir() || parent.starts_with(&data) || parent.starts_with(&root))
        {
            return Err(Error::Invalid(
                "fork root must be a directory outside the store and workspace".into(),
            ));
        }
        // Read-only preflight also accepts legacy workspaces; only capture migrates.
        let authority = Authority::open_read_only(&data)?;
        let state = Workspace::open_read_only(&root)?.state()?;
        if state.change.repository != authority.repository_id()? {
            return Err(Error::Conflict(
                "workspace belongs to another repository".into(),
            ));
        }
        authority.change(&state.change)?;
        Ok(Self {
            data,
            root,
            fork_root,
        })
    }

    pub fn capture(&self, trigger: SnapshotTrigger) -> Result<Value> {
        self.capture_with_deadline(trigger, Duration::from_secs(60))
    }

    pub fn capture_with_deadline(
        &self,
        trigger: SnapshotTrigger,
        deadline: Duration,
    ) -> Result<Value> {
        let engine = Engine::open(&self.data)?;
        let workspace = self.open(true)?;
        let outcome = workspace.capture_with_deadline(&engine, trigger, deadline)?;
        bounded(json!({"version":1,"trigger":trigger,"outcome":outcome}))
    }

    pub fn timeline(&self, limit: usize, before: Option<u64>) -> Result<Value> {
        let page = self.open(false)?.timeline(limit, before)?;
        bounded(json!({"version":1,"timeline":page}))
    }

    pub fn entry(&self, id: u64) -> Result<Value> {
        let entry = self.open(false)?.timeline_entry(id)?;
        let evidence = match &entry {
            TimelineEntry::Snapshot(_) => json!({"state":"not_run"}),
            TimelineEntry::CheckLink(link) => {
                let engine = Engine::open(&self.data)?;
                match crate::skill_workflow_service::read(&self.data, &engine, &link.run_id) {
                    Ok(projection)
                        if projection.run.source.revision == link.tested_revision
                            && projection.run.source.repository == link.change.repository
                            && projection.run.source.policy_epoch == link.policy_epoch
                            && projection.present_landing_binding.as_ref().is_none_or(
                                |binding| {
                                    binding.change_id.as_deref() == Some(link.change.id.as_str())
                                        && binding.change_version == Some(link.change_version)
                                        && binding.context_digest.as_deref()
                                            == Some(link.context_digest.as_str())
                                },
                            ) =>
                    {
                        // The run is an observation about tested_revision only. Its applicability
                        // is evaluated independently against current authority by the run service.
                        json!({"state":"available","workflow":projection})
                    }
                    Ok(_) => json!({"state":"binding_mismatch"}),
                    Err(_) => json!({"state":"unavailable"}),
                }
            }
        };
        bounded(json!({"version":1,"entry":entry,"evidence":evidence}))
    }

    pub fn diff(
        &self,
        left: u64,
        right: u64,
        limit: usize,
        after: Option<String>,
    ) -> Result<Value> {
        if !(1..=100).contains(&limit) {
            return Err(Error::Limit("timeline diff supports 1..100 entries".into()));
        }
        let workspace = self.open(false)?;
        let left_entry = workspace.timeline_entry(left)?;
        let right_entry = workspace.timeline_entry(right)?;
        let engine = Engine::open(&self.data)?;
        let mut page = engine.diff_page(
            left_entry.revision(),
            right_entry.revision(),
            after.as_deref(),
            limit,
        )?;
        // Filter displayed entries to the intersection of the recorded projections.
        // The opaque cursor and needed_objects still describe complete-source traversal;
        // scope selects work, it is not an authorization boundary.
        page.entries.retain(|entry| {
            in_scope(&entry.path, left_entry.scope()) && in_scope(&entry.path, right_entry.scope())
        });
        bounded(json!({"version":1,"left":left,"right":right,"diff":page}))
    }

    pub fn fork(&self, id: u64, child_name: &str, change: &str) -> Result<Value> {
        let parent = self
            .fork_root
            .as_ref()
            .ok_or_else(|| Error::Invalid("fork is disabled; configure --fork-root".into()))?;
        let mut components = Path::new(child_name).components();
        if child_name.len() > 128
            || !matches!(components.next(), Some(Component::Normal(_)))
            || components.next().is_some()
            || child_name.contains(['/', '\\'])
        {
            return Err(Error::Invalid(
                "fork child must be a single directory name of at most 128 bytes".into(),
            ));
        }
        let destination = parent.join(child_name);
        if nested(&destination, &self.data) || nested(&destination, &self.root) {
            return Err(Error::Invalid(
                "fork destination must not overlap the source workspace or store".into(),
            ));
        }
        let workspace = self.open(false)?;
        let entry = workspace.timeline_entry(id)?;
        let authority = Authority::open_read_only(&self.data)?;
        let change = ChangeId {
            repository: authority.repository_id()?,
            id: change.to_owned(),
        };
        let state = authority.change(&change)?;
        let context = authority.review_context(&state.context_digest)?;
        if context.revision != entry.revision() {
            return Err(Error::Conflict(
                "fork Change context must match the selected source revision".into(),
            ));
        }
        let engine = Engine::open(&self.data)?;
        let fork = workspace.fork_from_entry(&engine, id, &destination, change)?;
        bounded(json!({"version":1,"path":destination,"workspace":fork.state()?,"source_entry":id}))
    }

    fn open(&self, writable: bool) -> Result<Workspace> {
        // Detect replacement of the configured paths between requests.
        if self.data.canonicalize()? != self.data || self.root.canonicalize()? != self.root {
            return Err(Error::Conflict("configured timeline path changed".into()));
        }
        if let Some(parent) = &self.fork_root
            && parent.canonicalize()? != *parent
        {
            return Err(Error::Conflict("configured fork root changed".into()));
        }
        let workspace = if writable {
            Workspace::open(&self.root)?
        } else {
            Workspace::open_read_only(&self.root)?
        };
        let authority = Authority::open_read_only(&self.data)?;
        let state = workspace.state()?;
        if state.change.repository != authority.repository_id()? {
            return Err(Error::Conflict(
                "workspace belongs to another repository".into(),
            ));
        }
        authority.change(&state.change)?;
        Ok(workspace)
    }
}

fn nested(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn in_scope(path: &str, scope: Option<&[String]>) -> bool {
    scope.is_none_or(|roots| roots.iter().any(|root| Path::new(path).starts_with(root)))
}

fn bounded(value: Value) -> Result<Value> {
    if serde_json::to_vec(&value)?.len() > MAX_RESPONSE_BYTES {
        return Err(Error::Limit("timeline response exceeds 1 MiB".into()));
    }
    Ok(value)
}
