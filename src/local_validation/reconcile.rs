//! Reconciles the commit statuses of one local-validation check (the aggregate
//! `agentscm/local-validation[/<check>]` context plus one context per constituent) on every open
//! pull request on one base branch from the signed attestation comments, replacing the shell
//! verifier. [`reconcile_all`] sweeps several independent checks serially; each sweep owns only
//! its own contexts, and a head opted into any check's family shows `pending` on the others.
//!
//! Safety order: every opted-in head is first set to `pending` on every context of the check, so a
//! crash can leave `pending` but never a stale `success`; a head counts as invalidated only when
//! all of those writes succeeded; each later status write is preceded by a re-read of the pull
//! requests and comments, and is skipped when anything changed (a newer run owns the verdict).
//! When the policy has `exempt_paths`, then, after that invalidation, every open pull request whose
//! head is not shared is checked against Git: a head whose change is non-empty, touches no trust
//! input and only exempt paths gets `success` without an attestation, ahead of any attestation
//! verdict. An unmarked head that is not exempt gets nothing; a shared head gets no exemption.
//! Pull request code is never checked out, built or run; heads are fetched as Git data only.
//!
//! ponytail: `reconcile_all` repeats the pull request, comment and fetch reads once per check, so
//! the API budget grows linearly with the checks (at most 20). Upgrade path: share the reads
//! across sweeps when that budget matters. A serial sweep in one process is used because the
//! workflow's job-level concurrency keeps only the newest queued run, so one caller per check
//! would lose runs.
//!
//! ponytail: invalidate-first writes two statuses per context per opted-in head per run, and GitHub keeps
//! at most 1000 statuses per sha and context, so a head hits the cap after about 500 runs. Upgrade
//! path: skip both writes when the head's latest status already equals the verdict and nothing changed.
//! The workflow's pre-setup invalidation step writes `pending` first, so the latest status is usually
//! `pending` when reconcile starts.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde_json::Value;

use super::comment::MARKER_PREFIX;
use super::fetch::FetchContext;
use super::run::{observe, only_exempt, required_commands, trust_paths};
use super::{
    Observed, ValidationPolicy, ValidationRecipe, ValidationRecord, VerdictState, assess_lifecycle,
    select,
};
use crate::evidence::EvaluationClock;
use crate::hub::github::{Api, CommitStatus, Gh, GhError, StatusState};

use super::STATUS_CONTEXT;
const TRUSTED_ASSOCIATIONS: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];
const MAX_PULL_PAGES: u32 = 30;
const MAX_COMMENT_PAGES: u32 = 10;
const MAX_STATUS_PAGES: u32 = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueComment {
    pub id: i64,
    pub author_association: String,
    pub updated_at: String,
    pub body: String,
    /// The comment's web URL as the API reports it; display only.
    pub html_url: Option<String>,
}

/// GitHub's limit on a status description, in characters.
const DESCRIPTION_CHARS: usize = 140;

/// A single-line status description. The reason is the useful part, so it stays whole (up to the
/// limit) and the check and constituent labels give way to it, truncated by characters.
fn describe(check: Option<&str>, constituent: Option<&str>, reason: &str) -> String {
    let single_line = |text: &str, limit: usize| -> String {
        text.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(limit)
            .collect()
    };
    let reason = single_line(reason, DESCRIPTION_CHARS);
    let labelled = [check, constituent]
        .into_iter()
        .flatten()
        .fold(String::new(), |text, name| format!("{text}{name}: "));
    let room = DESCRIPTION_CHARS - reason.chars().count();
    format!("{}{reason}", single_line(&labelled, room))
}

#[cfg(test)]
mod description_tests {
    use super::describe;

    #[test]
    fn long_names_never_cut_off_the_reason() {
        let (check, constituent) = ("é".repeat(64), "ü".repeat(64));
        let reason = "issuer not trusted: Revoked";
        let text = describe(Some(&check), Some(&constituent), reason);
        assert!(text.ends_with(reason), "{text}");
        assert!(text.chars().count() <= 140, "{text}");
        assert_eq!(describe(Some("Unit"), None, "ok"), "Unit: ok");
        assert_eq!(describe(None, None, "a\nb"), "a b");
        let long = "r".repeat(300);
        assert_eq!(describe(Some("Unit"), None, &long), "r".repeat(140));
    }
}

impl IssueComment {
    fn parse(value: &Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        Some(Self {
            id: value["id"].as_i64()?,
            author_association: text("author_association").unwrap_or_default(),
            updated_at: text("updated_at")?,
            body: text("body")?,
            html_url: text("html_url"),
        })
    }
}

/// Association-only discovery hints. Reconciliation also authenticates enrolled signatures
/// when association visibility is weaker. Neither discovery route replaces the final verdict.
pub fn trusted(comments: &[IssueComment]) -> Vec<&IssueComment> {
    comments
        .iter()
        .filter(|c| {
            TRUSTED_ASSOCIATIONS.contains(&c.author_association.as_str())
                && c.body.starts_with(MARKER_PREFIX)
        })
        .collect()
}

fn clock() -> EvaluationClock {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .map_or(EvaluationClock::Uncertain, |unix_ms| {
            EvaluationClock::Known { unix_ms }
        })
}

/// Sorted `id:updated_at` of the eligible comments; any edit, deletion or addition changes it.
pub fn fingerprint(eligible: &[&IssueComment]) -> Vec<String> {
    let mut out: Vec<String> = eligible
        .iter()
        .map(|c| format!("{}:{}", c.id, c.updated_at))
        .collect();
    out.sort();
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenPull {
    pub number: i64,
    pub head: String,
    pub base: String,
}

fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Open pull requests (any base) per head sha.
fn by_head(pulls: &[OpenPull]) -> BTreeMap<&str, Vec<i64>> {
    let mut map: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
    for pull in pulls {
        map.entry(&pull.head).or_default().push(pull.number);
    }
    map
}

fn shared(pulls: &[OpenPull], head: &str) -> bool {
    pulls.iter().filter(|p| p.head == head).count() > 1
}

pub struct Context<'a> {
    pub api: &'a Api,
    pub token: &'a str,
    /// OWNER/NAME.
    pub github_repo: &'a str,
    pub base: &'a str,
    /// Repository-relative directory of the policy, recipe and registry; its tree at the checkout
    /// `HEAD` must equal `origin/<base>`'s before a verdict is written.
    pub config_dir: &'a str,
    /// Verifier workflow path relative to the checkout; with `config_dir` it is a trust input.
    pub verifier_workflow: &'a str,
    /// Local checkout whose `origin` is the repository.
    pub git_repo: &'a Path,
    /// One isolated transport context for every base, pull and freshness fetch in this run.
    pub fetch: &'a FetchContext<'a>,
    /// The policy's repository identity.
    pub repository: &'a str,
    pub policy: &'a ValidationPolicy,
    pub recipe: &'a ValidationRecipe,
    pub registry: &'a crate::policy::issuers::IssuerRegistrySnapshot,
    pub target_url: Option<&'a str>,
    /// The trusted display name of the check from `checks.json`; `None` for the legacy check.
    /// The check's status context and constituents come from `policy` and `recipe`.
    pub check_name: Option<&'a str>,
}

/// Sweeps the checks serially in order and returns the worst exit code. A failing sweep never
/// stops the later ones.
pub async fn reconcile_all(checks: &[Context<'_>]) -> i32 {
    let mut worst = 0;
    for check in checks {
        worst = worst.max(check.reconcile().await);
    }
    worst
}

/// One commit-status context of the check being reconciled.
struct Slot {
    context: String,
    /// Id and display name of the constituent; `None` for the aggregate.
    constituent: Option<(String, String)>,
}

/// A decided set of writes for one head: the aggregate first, then each constituent.
struct Decided {
    writes: Vec<(Slot, StatusState, String)>,
    /// Details link of the deciding comment, when known.
    link: Option<String>,
}

fn status_state(state: VerdictState) -> StatusState {
    match state {
        VerdictState::Success => StatusState::Success,
        VerdictState::Failure => StatusState::Failure,
        VerdictState::Error => StatusState::Error,
        VerdictState::Pending => StatusState::Pending,
    }
}

struct Candidate {
    number: i64,
    head: String,
    fingerprint: Vec<String>,
    bodies: Vec<String>,
    /// Lifecycle evidence from unrecognized authors: used only to choose a pending reason.
    hints: Vec<String>,
    /// `(envelope digest, html_url)` of each eligible comment that supplied a URL.
    links: Vec<(String, String)>,
}

enum Recheck {
    Current,
    Shared,
    Changed(&'static str),
}

#[derive(Default)]
struct Tally {
    errors: u32,
    /// Auth or rate limit: nothing more is attempted.
    stop: bool,
}

impl Tally {
    fn fail(&mut self, what: &str, error: &GhError) {
        self.errors += 1;
        self.stop |= matches!(error, GhError::Auth(_) | GhError::Limited(_));
        eprintln!("{what}: {error:?}");
    }
}

fn read_error(what: &str, error: &GhError) -> i32 {
    eprintln!("{what}: {error:?}; nothing written");
    1
}

fn unread(what: &str) -> GhError {
    GhError::Failed(format!("too many {what} to read completely"))
}

impl Context<'_> {
    /// Association visibility depends on the API viewer. An otherwise unrecognized author
    /// can opt in only with fresh, currently trusted signed evidence for this repository,
    /// head and base. The record's tree and merge-base are used here only for discovery;
    /// `verdict` still fetches Git and independently checks every source binding. Evidence
    /// from such an author that is authentic for this check but revoked, stale or expired is
    /// returned separately as a hint: it never opts a head in or joins the fingerprint, and
    /// `select` still verifies it fully before it can choose a pending reason.
    fn eligible<'a>(
        &self,
        comments: &'a [IssueComment],
        pull: &OpenPull,
    ) -> (Vec<&'a IssueComment>, Vec<&'a IssueComment>) {
        let required: Vec<String> = self.recipe.commands.iter().map(|c| c.id.clone()).collect();
        let now = clock();
        let (mut eligible, mut hints) = (Vec::new(), Vec::new());
        for comment in comments {
            if !comment.body.starts_with(MARKER_PREFIX) {
                continue;
            }
            if TRUSTED_ASSOCIATIONS.contains(&comment.author_association.as_str()) {
                eligible.push(comment);
                continue;
            }
            let Ok((envelope, bytes)) = super::comment::parse(&comment.body) else {
                continue;
            };
            let Ok(record) = ValidationRecord::from_slice(&bytes) else {
                continue;
            };
            let (verdict, lifecycle) = assess_lifecycle(
                &envelope,
                &bytes,
                self.recipe,
                self.registry,
                self.policy,
                Observed {
                    repository: self.repository,
                    commit: &pull.head,
                    tree: &record.subject.tree,
                    base_ref: &pull.base,
                    merge_base: &record.context.merge_base,
                    required: &required,
                },
                now,
            );
            if verdict.applies_to_head {
                eligible.push(comment);
            } else if lifecycle {
                hints.push(comment);
            }
        }
        (eligible, hints)
    }

    async fn open_pulls(&self) -> Gh<Vec<OpenPull>> {
        let path = format!("/repos/{}/pulls?state=open", self.github_repo);
        let paged = self.api.get_all(self.token, &path, MAX_PULL_PAGES).await?;
        if paged.unread > 0 {
            return Err(unread("open pull requests"));
        }
        paged
            .items
            .iter()
            .map(|p| {
                Some(OpenPull {
                    number: p["number"].as_i64()?,
                    head: p["head"]["sha"].as_str()?.to_owned(),
                    base: p["base"]["ref"].as_str()?.to_owned(),
                })
            })
            .collect::<Option<_>>()
            .ok_or_else(|| GhError::Failed("unreadable pull request".into()))
    }

    async fn comments(&self, number: i64) -> Gh<Vec<IssueComment>> {
        let path = format!("/repos/{}/issues/{number}/comments", self.github_repo);
        let paged = self
            .api
            .get_all(self.token, &path, MAX_COMMENT_PAGES)
            .await?;
        if paged.unread > 0 {
            return Err(unread("comments"));
        }
        paged
            .items
            .iter()
            .map(IssueComment::parse)
            .collect::<Option<_>>()
            .ok_or_else(|| GhError::Failed("unreadable comment".into()))
    }

    async fn has_status(&self, head: &str) -> Gh<bool> {
        let path = format!("/repos/{}/commits/{head}/statuses", self.github_repo);
        let paged = self
            .api
            .get_all(self.token, &path, MAX_STATUS_PAGES)
            .await?;
        // Family-wide: a head opted into any check shows pending on the others. Limit: a head
        // known only by a signed comment for a later check opts the earlier ones in on the next
        // sweep, not this one.
        let family = format!("{STATUS_CONTEXT}/");
        let ours = paged.items.iter().any(|s| {
            s["context"]
                .as_str()
                .is_some_and(|c| c == STATUS_CONTEXT || c.starts_with(&family))
        });
        if !ours && paged.unread > 0 {
            return Err(unread("statuses"));
        }
        Ok(ours)
    }

    /// The aggregate context first, then one per constituent in recipe order.
    fn slots(&self) -> Vec<Slot> {
        let mut slots = vec![Slot {
            context: self.policy.status_context(),
            constituent: None,
        }];
        for group in self.recipe.constituents.iter().flatten() {
            slots.push(Slot {
                context: self.policy.constituent_status_context(&group.id),
                constituent: Some((group.id.clone(), group.name.clone())),
            });
        }
        slots
    }

    /// Writes one status; true only when GitHub definitely applied it.
    async fn write(
        &self,
        head: &str,
        slot: &Slot,
        state: StatusState,
        reason: &str,
        link: Option<&str>,
        tally: &mut Tally,
    ) -> bool {
        // The names come from trusted configuration; the reason can derive from untrusted text.
        // Single line, bounded.
        let description = describe(
            self.check_name,
            slot.constituent.as_ref().map(|(_, name)| name.as_str()),
            reason,
        );
        let status = CommitStatus::new(head, state, &slot.context, &description).and_then(|s| {
            // An unusable API-supplied link falls back to the configured one.
            link.and_then(|u| s.clone().with_target_url(u).ok())
                .map_or_else(
                    || {
                        self.target_url
                            .map_or(Ok(s.clone()), |u| s.clone().with_target_url(u))
                    },
                    Ok,
                )
        });
        let status = match status {
            Ok(status) => status,
            Err(error) => return self.refused(tally, head, &error.to_string()),
        };
        match self
            .api
            .post_commit_status(self.token, self.github_repo, &status)
            .await
        {
            Ok(Ok(())) => true,
            Ok(Err(code)) => self.refused(tally, head, &format!("status refused with {code}")),
            Err(error) => {
                tally.fail(
                    &format!("head {head}: status write uncertain or failed"),
                    &error,
                );
                false
            }
        }
    }

    fn refused(&self, tally: &mut Tally, head: &str, why: &str) -> bool {
        tally.errors += 1;
        eprintln!("head {head}: {why}");
        false
    }

    /// Re-reads the pull requests and the eligible comments just before a write.
    async fn recheck(&self, c: &Candidate) -> Gh<Recheck> {
        let pulls = self.open_pulls().await?;
        let Some(pull) = pulls.iter().find(|p| p.number == c.number) else {
            return Ok(Recheck::Changed("no longer open"));
        };
        if pull.head != c.head || pull.base != self.base {
            return Ok(Recheck::Changed("head or base changed"));
        }
        let comments = self.comments(c.number).await?;
        if fingerprint(&self.eligible(&comments, pull).0) != c.fingerprint {
            return Ok(Recheck::Changed("attestation comments changed"));
        }
        Ok(if shared(&pulls, &c.head) {
            Recheck::Shared
        } else {
            Recheck::Current
        })
    }

    /// Whether the verifier inputs on the default branch are unchanged since the checkout.
    fn config_current(&self) -> crate::Result<bool> {
        tokio::task::block_in_place(|| self.fetch.config_dir_unchanged(self.base, self.config_dir))
    }

    /// Whether the head's change is only exempt paths; Err when it could not be fetched or read.
    fn exempt(&self, c: &Candidate, exempt: &[String]) -> Result<bool, String> {
        tokio::task::block_in_place(|| {
            self.fetch.fetch_pull(self.base, c.number, &c.head)?;
            let git = observe(self.git_repo, &c.head, &format!("origin/{}", self.base))?;
            if git.commit != c.head {
                return Ok(Err("fetched commit differs from the head".to_owned()));
            }
            let trust = trust_paths(self.config_dir, self.verifier_workflow);
            only_exempt(self.git_repo, &git.merge_base, &git.commit, exempt, &trust).map(Ok)
        })
        .map_err(|error| format!("{error:?}"))?
    }

    /// Err when the head could not be fetched or observed: nothing is written for it.
    fn verdict(&self, c: &Candidate) -> Result<Decided, String> {
        let git = tokio::task::block_in_place(|| {
            self.fetch.fetch_pull(self.base, c.number, &c.head)?;
            observe(self.git_repo, &c.head, &format!("origin/{}", self.base))
        });
        let git = match git {
            Ok(git) if git.commit == c.head => git,
            Ok(_) => return Err("fetched commit differs from the head".into()),
            Err(error) => return Err(format!("{error:?}")),
        };
        let clock = clock();
        let trust = trust_paths(self.config_dir, self.verifier_workflow);
        let required = match tokio::task::block_in_place(|| {
            required_commands(
                self.git_repo,
                &git.merge_base,
                &git.commit,
                self.recipe,
                &trust,
            )
        }) {
            Ok(required) => required,
            Err(error) => return Err(format!("{error:?}")),
        };
        let bodies: Vec<&str> = c
            .bodies
            .iter()
            .chain(&c.hints)
            .map(String::as_str)
            .collect();
        let verdict = select(
            &bodies,
            self.recipe,
            self.registry,
            self.policy,
            Observed {
                repository: self.repository,
                commit: &git.commit,
                tree: &git.tree,
                base_ref: self.base,
                merge_base: &git.merge_base,
                required: &required,
            },
            clock,
        );
        // Reasons are only the fixed verdict and constituent reasons, never comment text. A
        // structural problem leaves `verdict.constituents` empty: every context inherits it.
        let mut writes = Vec::new();
        for slot in self.slots() {
            let own = slot
                .constituent
                .as_ref()
                .and_then(|(id, _)| verdict.constituents.iter().find(|k| &k.id == id));
            let (state, reason) =
                own.map_or((verdict.state, verdict.reason), |k| (k.state, k.reason));
            writes.push((slot, status_state(state), reason.to_owned()));
        }
        let link = (verdict.state != VerdictState::Pending && !verdict.envelope_sha256.is_empty())
            .then(|| {
                c.links
                    .iter()
                    .find(|(digest, _)| *digest == verdict.envelope_sha256)
                    .map(|(_, url)| url.clone())
            })
            .flatten();
        Ok(Decided { writes, link })
    }

    /// Every context of the check at one state and reason.
    fn uniform(&self, state: StatusState, reason: &str) -> Decided {
        Decided {
            writes: self
                .slots()
                .into_iter()
                .map(|slot| (slot, state, reason.to_owned()))
                .collect(),
            link: None,
        }
    }

    /// Writes every context; true when all were applied. Stops early only on `tally.stop`.
    async fn write_all(&self, head: &str, decided: &Decided, tally: &mut Tally) -> bool {
        let mut all = true;
        for (slot, state, reason) in &decided.writes {
            all &= self
                .write(head, slot, *state, reason, decided.link.as_deref(), tally)
                .await;
            if tally.stop {
                return false;
            }
        }
        all
    }

    /// Exit code: 0 when every pull request was handled, 1 on any error or unread input.
    pub async fn reconcile(&self) -> i32 {
        let pulls = match self.open_pulls().await {
            Ok(pulls) => pulls,
            Err(error) => return read_error("open pull requests", &error),
        };
        let mut tally = Tally::default();
        let mut candidates = Vec::new();
        for pull in pulls.iter().filter(|p| p.base == self.base) {
            if !is_sha(&pull.head) {
                println!("PR #{}: unexpected head; skipped", pull.number);
                tally.errors += 1;
                continue;
            }
            let comments = match self.comments(pull.number).await {
                Ok(comments) => comments,
                Err(error) => return read_error("comments", &error),
            };
            let (attestations, hints) = self.eligible(&comments, pull);
            let opted_in = if attestations.is_empty() {
                match self.has_status(&pull.head).await {
                    Ok(has) => has,
                    Err(error) => return read_error("statuses", &error),
                }
            } else {
                true
            };
            let candidate = Candidate {
                number: pull.number,
                head: pull.head.clone(),
                fingerprint: fingerprint(&attestations),
                bodies: attestations.iter().map(|c| c.body.clone()).collect(),
                hints: hints.iter().map(|c| c.body.clone()).collect(),
                links: attestations
                    .iter()
                    .filter_map(|c| {
                        let (envelope, _) = super::comment::parse(&c.body).ok()?;
                        Some((
                            super::comment::envelope_sha256(&envelope),
                            c.html_url.clone()?,
                        ))
                    })
                    .collect(),
            };
            if opted_in {
                candidates.push((candidate, true));
            } else if self.policy.exempt_paths.is_some() {
                candidates.push((candidate, false));
            } else {
                println!("PR #{}: not opted in; nothing written", pull.number);
            }
        }

        // Invalidate first.
        let mut invalidated = HashSet::new();
        let mut attempted = HashSet::new();
        for (c, _) in candidates.iter().filter(|(_, opted_in)| *opted_in) {
            if attempted.insert(c.head.clone()) {
                let pending = self.uniform(StatusState::Pending, "re-verifying local validation");
                // Only a fully invalidated head may receive a verdict.
                if self.write_all(&c.head, &pending, &mut tally).await {
                    invalidated.insert(c.head.clone());
                }
            }
            if tally.stop {
                return 1;
            }
        }

        // Exempt-only heads (not shared) get success; an opted-in head was set pending above.
        let mut exempted = HashSet::new();
        if let Some(paths) = &self.policy.exempt_paths {
            for (c, opted_in) in &candidates {
                if shared(&pulls, &c.head) {
                    continue;
                }
                match self.exempt(c, paths) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    // An opted-in head goes on to its own verdict, which fails the same way.
                    Err(why) => {
                        if !opted_in {
                            // Not opted in: a warning, not an error an outside author can force.
                            eprintln!(
                                "warning: PR #{} {}: head could not be checked for exempt paths ({why}); nothing written",
                                c.number, c.head
                            );
                        }
                        continue;
                    }
                }
                exempted.insert(c.head.clone());
                match self.recheck(c).await {
                    Err(error) => {
                        tally.fail(
                            &format!(
                                "PR #{} {}: re-check failed; nothing written",
                                c.number, c.head
                            ),
                            &error,
                        );
                        if tally.stop {
                            return 1;
                        }
                        continue;
                    }
                    Ok(Recheck::Changed(why)) => {
                        println!(
                            "PR #{} {}: {why}; nothing written (a newer event owns the verdict)",
                            c.number, c.head
                        );
                        continue;
                    }
                    Ok(Recheck::Shared) => {
                        println!(
                            "PR #{} {}: shared head; exemption not applied",
                            c.number, c.head
                        );
                        continue;
                    }
                    Ok(Recheck::Current) => {}
                }
                match self.config_current() {
                    Ok(true) => {}
                    Ok(false) => {
                        println!(
                            "PR #{} {}: default-branch verifier configuration changed since checkout; a newer run owns the verdict",
                            c.number, c.head
                        );
                        break;
                    }
                    Err(error) => {
                        tally.errors += 1;
                        eprintln!(
                            "PR #{} {}: verifier configuration could not be checked ({error:?}); nothing written",
                            c.number, c.head
                        );
                        continue;
                    }
                }
                let reason = "not required: only exempt paths changed";
                if self
                    .write_all(
                        &c.head,
                        &self.uniform(StatusState::Success, reason),
                        &mut tally,
                    )
                    .await
                {
                    println!("PR #{} {}: success ({reason})", c.number, c.head);
                }
                if tally.stop {
                    return 1;
                }
            }
        }

        let work: Vec<&Candidate> = candidates
            .iter()
            .filter(|(c, opted_in)| *opted_in && !exempted.contains(&c.head))
            .map(|(c, _)| c)
            .collect();
        let heads = by_head(&pulls);
        let mut done = HashSet::new();
        for c in work {
            if !invalidated.contains(&c.head) {
                println!("PR #{} {}: not invalidated; not verified", c.number, c.head);
                continue;
            }
            let multiple = heads.get(c.head.as_str()).is_some_and(|n| n.len() > 1);
            if multiple && !done.insert(c.head.clone()) {
                println!("PR #{} {}: shared head; handled", c.number, c.head);
                continue;
            }
            let decided = if multiple {
                None
            } else {
                match self.verdict(c) {
                    Ok(decided) => Some(decided),
                    Err(why) => {
                        // Stays pending from the invalidate step. A head that moved mid-run is
                        // not an error; anything else fails the run.
                        match self.recheck(c).await {
                            Ok(Recheck::Changed(moved)) => println!(
                                "PR #{} {}: {moved}; left pending (a newer event owns the verdict)",
                                c.number, c.head
                            ),
                            Ok(_) => {
                                tally.errors += 1;
                                eprintln!(
                                    "PR #{} {}: head could not be fetched ({why}); stays pending",
                                    c.number, c.head
                                );
                            }
                            Err(error) => tally.fail(
                                &format!(
                                    "PR #{} {}: head could not be fetched ({why}) and re-check failed; stays pending",
                                    c.number, c.head
                                ),
                                &error,
                            ),
                        }
                        if tally.stop {
                            return 1;
                        }
                        continue;
                    }
                }
            };
            let outcome = match self.recheck(c).await {
                Err(error) => {
                    tally.fail(
                        &format!(
                            "PR #{} {}: re-check failed; head stays pending",
                            c.number, c.head
                        ),
                        &error,
                    );
                    if tally.stop {
                        return 1;
                    }
                    continue;
                }
                Ok(Recheck::Changed(why)) => {
                    println!(
                        "PR #{} {}: {why}; left pending (a newer event owns the verdict)",
                        c.number, c.head
                    );
                    continue;
                }
                Ok(Recheck::Shared) => None,
                Ok(Recheck::Current) => decided,
            };
            match self.config_current() {
                Ok(true) => {}
                Ok(false) => {
                    println!(
                        "PR #{} {}: default-branch verifier configuration changed since checkout; a newer run owns the verdict",
                        c.number, c.head
                    );
                    // Remaining heads stay pending; the newer run owns them.
                    break;
                }
                Err(error) => {
                    tally.errors += 1;
                    eprintln!(
                        "PR #{} {}: verifier configuration could not be checked ({error:?}); stays pending",
                        c.number, c.head
                    );
                    continue;
                }
            }
            let decided = outcome.unwrap_or_else(|| {
                self.uniform(
                    StatusState::Pending,
                    "head shared by several open pull requests; unsupported",
                )
            });
            let state = decided.writes[0].1;
            if self.write_all(&c.head, &decided, &mut tally).await {
                println!("PR #{} {}: {}", c.number, c.head, state.as_str());
            }
            if tally.stop {
                return 1;
            }
        }
        i32::from(tally.errors > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: i64, who: &str, body: &str) -> IssueComment {
        IssueComment {
            id,
            author_association: who.into(),
            updated_at: format!("t{id}"),
            body: body.into(),
            html_url: None,
        }
    }

    #[test]
    fn only_trusted_marked_comments_count() {
        let marked = format!("{MARKER_PREFIX}abc -->");
        let all = [
            comment(2, "OWNER", &marked),
            comment(1, "COLLABORATOR", &marked),
            comment(3, "NONE", &marked),
            comment(4, "MEMBER", "no marker"),
            comment(5, "CONTRIBUTOR", &marked),
        ];
        assert_eq!(fingerprint(&trusted(&all)), ["1:t1", "2:t2"]);
    }

    #[test]
    fn heads_shared_across_any_base() {
        let pull = |number, head: &str, base: &str| OpenPull {
            number,
            head: head.into(),
            base: base.into(),
        };
        let pulls = [
            pull(1, "a", "main"),
            pull(2, "a", "dev"),
            pull(3, "b", "main"),
        ];
        assert!(shared(&pulls, "a"));
        assert!(!shared(&pulls, "b"));
        assert_eq!(by_head(&pulls)["a"], [1, 2]);
        assert!(is_sha(&"a".repeat(40)) && !is_sha(&"A".repeat(40)));
    }
}
