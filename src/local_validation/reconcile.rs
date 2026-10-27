//! Reconciles the `agentscm/local-validation` commit status of every open pull request on one
//! base branch from the signed attestation comments, replacing the shell verifier.
//!
//! Safety order: every opted-in head is first set to `pending`, so a crash can leave `pending`
//! but never a stale `success`; each later status write is preceded by a re-read of the pull
//! requests and comments, and is skipped when anything changed (a newer run owns the verdict).
//! When the policy has `exempt_paths`, then, after that invalidation, every open pull request whose
//! head is not shared is checked against Git: a head whose change is non-empty, touches no trust
//! input and only exempt paths gets `success` without an attestation, ahead of any attestation
//! verdict. An unmarked head that is not exempt gets nothing; a shared head gets no exemption.
//! Pull request code is never checked out, built or run; heads are fetched as Git data only.
//!
//! ponytail: invalidate-first writes two statuses per opted-in head per run, and GitHub keeps
//! at most 1000 statuses per sha and context, so a head hits the cap after about 500 runs. Upgrade
//! path: skip both writes when the head's latest status already equals the verdict and nothing changed.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde_json::Value;

use super::comment::MARKER_PREFIX;
use super::fetch::FetchContext;
use super::run::{observe, only_exempt, required_commands, trust_paths};
use super::{Observed, ValidationPolicy, ValidationRecipe, VerdictState, select};
use crate::evidence::EvaluationClock;
use crate::hub::github::{Api, CommitStatus, Gh, GhError, StatusState};

pub const CONTEXT: &str = "agentscm/local-validation";
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
}

impl IssueComment {
    fn parse(value: &Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        Some(Self {
            id: value["id"].as_i64()?,
            author_association: text("author_association").unwrap_or_default(),
            updated_at: text("updated_at")?,
            body: text("body")?,
        })
    }
}

/// Attestation comments a maintainer posted: trusted association and the marker. The signature
/// still decides; this limits what is read.
pub fn trusted(comments: &[IssueComment]) -> Vec<&IssueComment> {
    comments
        .iter()
        .filter(|c| {
            TRUSTED_ASSOCIATIONS.contains(&c.author_association.as_str())
                && c.body.starts_with(MARKER_PREFIX)
        })
        .collect()
}

/// Sorted `id:updated_at` of the trusted comments; any edit, deletion or addition changes it.
pub fn fingerprint(trusted: &[&IssueComment]) -> Vec<String> {
    let mut out: Vec<String> = trusted
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
}

struct Candidate {
    number: i64,
    head: String,
    fingerprint: Vec<String>,
    bodies: Vec<String>,
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
        let ours = paged.items.iter().any(|s| s["context"] == CONTEXT);
        if !ours && paged.unread > 0 {
            return Err(unread("statuses"));
        }
        Ok(ours)
    }

    /// Writes one status; true only when GitHub definitely applied it.
    async fn write(&self, head: &str, state: StatusState, reason: &str, tally: &mut Tally) -> bool {
        // The reason can derive from untrusted text: single line, bounded.
        let description: String = reason
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(140)
            .collect();
        let status = CommitStatus::new(head, state, CONTEXT, &description).and_then(|s| {
            self.target_url
                .map_or(Ok(s.clone()), |u| s.with_target_url(u))
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

    /// Re-reads the pull requests and the trusted comments just before a write.
    async fn recheck(&self, c: &Candidate) -> Gh<Recheck> {
        let pulls = self.open_pulls().await?;
        let Some(pull) = pulls.iter().find(|p| p.number == c.number) else {
            return Ok(Recheck::Changed("no longer open"));
        };
        if pull.head != c.head || pull.base != self.base {
            return Ok(Recheck::Changed("head or base changed"));
        }
        let comments = self.comments(c.number).await?;
        if fingerprint(&trusted(&comments)) != c.fingerprint {
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
    fn verdict(&self, c: &Candidate) -> Result<(StatusState, String), String> {
        let git = tokio::task::block_in_place(|| {
            self.fetch.fetch_pull(self.base, c.number, &c.head)?;
            observe(self.git_repo, &c.head, &format!("origin/{}", self.base))
        });
        let git = match git {
            Ok(git) if git.commit == c.head => git,
            Ok(_) => return Err("fetched commit differs from the head".into()),
            Err(error) => return Err(format!("{error:?}")),
        };
        let clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok())
            .map_or(EvaluationClock::Uncertain, |unix_ms| {
                EvaluationClock::Known { unix_ms }
            });
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
        let bodies: Vec<&str> = c.bodies.iter().map(String::as_str).collect();
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
        let state = match verdict.state {
            VerdictState::Success => StatusState::Success,
            VerdictState::Failure => StatusState::Failure,
            VerdictState::Error => StatusState::Error,
            VerdictState::Pending => StatusState::Pending,
        };
        Ok((state, verdict.reason.to_owned()))
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
            let attestations = trusted(&comments);
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
            if attempted.insert(c.head.clone())
                && self
                    .write(
                        &c.head,
                        StatusState::Pending,
                        "re-verifying local validation",
                        &mut tally,
                    )
                    .await
            {
                invalidated.insert(c.head.clone());
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
                    .write(&c.head, StatusState::Success, reason, &mut tally)
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
            let (state, reason) = outcome.unwrap_or((
                StatusState::Pending,
                "head shared by several open pull requests; unsupported".into(),
            ));
            let written = self.write(&c.head, state, &reason, &mut tally).await;
            if written {
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
