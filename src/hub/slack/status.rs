//! Private deterministic status queries and opt-in action reminders. No Slack message text is
//! retained. Jobs bind an exact configured identity and re-read their facts before every send.
use super::*;
use crate::hub::work::WorkState;
use crate::hub::work_http::actionable_status::{self, ActionableStatus, StatusState};

const SCAN_SECONDS: i64 = 60;
const JOB_TTL: i64 = 86400;
const EPISODE_LIMIT: i64 = 4096;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReminderPolicy {
    pub enabled: bool,
    pub interval_seconds: i64,
    pub max_reminders: i64,
    pub escalation_user: Option<String>,
    pub escalation_after_seconds: i64,
    pub send_resolution: bool,
}
impl Default for ReminderPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: 3600,
            max_reminders: 2,
            escalation_user: None,
            escalation_after_seconds: 7200,
            send_resolution: false,
        }
    }
}
impl ReminderPolicy {
    pub(super) fn validate(&self, binding: &BindingConfig, all: &[BindingConfig]) -> Result<()> {
        if !(900..=86400).contains(&self.interval_seconds)
            || !(1..=3).contains(&self.max_reminders)
            || !(self.interval_seconds..=7 * 86400).contains(&self.escalation_after_seconds)
        {
            return Err(invalid());
        }
        if let Some(user) = &self.escalation_user
            && (user == &binding.slack_user
                || !all.iter().any(|target| {
                    target.slack_user == *user
                        && target.provider == binding.provider
                        && target.host == binding.host
                        && target.repository_id == binding.repository_id
                        && target.repository == binding.repository
                }))
        {
            return Err(invalid());
        }
        Ok(())
    }
}

pub(super) fn binding_key(binding: &Binding) -> String {
    let c = &binding.config;
    hash(
        serde_json::to_string(&(
            &c.slack_user,
            &c.provider,
            &c.host,
            c.forge_user_id,
            &c.repository,
            c.repository_id,
            binding.digest.as_str(),
        ))
        .expect("binding identity serializes")
        .as_bytes(),
    )
}

pub(super) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS message_bindings (message TEXT PRIMARY KEY, binding TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS status_jobs (id TEXT PRIMARY KEY, binding TEXT NOT NULL, user TEXT NOT NULL, kind TEXT NOT NULL, subject TEXT, cycle INTEGER NOT NULL DEFAULT 0, state TEXT NOT NULL, created INTEGER NOT NULL, due INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS status_episodes (id TEXT PRIMARY KEY, binding TEXT NOT NULL, subject TEXT NOT NULL, cycle INTEGER NOT NULL, state TEXT NOT NULL, first_seen INTEGER NOT NULL, next_due INTEGER NOT NULL, delivered INTEGER NOT NULL DEFAULT 0, escalated INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS status_scans (binding TEXT PRIMARY KEY, next_due INTEGER NOT NULL);
        UPDATE status_jobs SET state='queued' WHERE state='sending';")?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Subject {
    Work {
        id: String,
        generation: i64,
        state: StatusState,
    },
    Request {
        id: String,
        work_id: String,
        generation: i64,
    },
}
impl Subject {
    fn key(&self) -> String {
        serde_json::to_string(self).expect("subject serializes")
    }
}
struct Outstanding {
    subject: Subject,
    text: String,
}
fn outstanding(status: &ActionableStatus) -> Vec<Outstanding> {
    let mut out = Vec::new();
    for work in &status.work {
        if work.needs_you
            && matches!(
                work.state,
                StatusState::Blocked | StatusState::AwaitingParty
            )
            && !work
                .limits
                .iter()
                .any(|l| l == "work_changed_during_read" || l == "reconciliation_work_changed")
        {
            out.push(Outstanding {
                subject: Subject::Work {
                    id: work.work_id.clone(),
                    generation: work.generation,
                    state: work.state,
                },
                text: format!(
                    "{}: {} (recipient {}; claim {:?}; reservation {:?}).",
                    sanitize(&work.title, 160),
                    work.action,
                    work.recipient.login.as_deref().unwrap_or("unknown"),
                    work.freshness,
                    work.operational.reservation.status
                ),
            });
        }
    }
    for action in &status.actions {
        if !action.needs_you || action.kind != "directed_request" {
            continue;
        }
        let Some(work) = action
            .work_ids
            .first()
            .and_then(|id| status.work.iter().find(|w| &w.work_id == id))
        else {
            continue;
        };
        out.push(Outstanding {
            subject: Subject::Request {
                id: action.id.clone(),
                work_id: work.work_id.clone(),
                generation: work.generation,
            },
            text: action.action.clone(),
        });
    }
    out
}

fn body(text: &str) -> Value {
    json!({"text": escape(&clip_plain_text(text, 3500)), "parse":"none", "mrkdwn":false, "unfurl_links":false,
        "unfurl_media":false, "blocks":[{"type":"section","text":plain_lines(text, 3000)}]})
}
fn render_status(status: &ActionableStatus) -> String {
    let mut copy = status.clone();
    copy.work.sort_by_key(|w| !w.needs_you);
    copy.work.truncate(4);
    copy.actions.retain(|a| a.needs_you);
    copy.actions.truncate(4);
    for work in &mut copy.work {
        work.title = sanitize(&work.title, 100);
        work.action = sanitize(&work.action, 180);
        work.recipient.login = work.recipient.login.as_deref().map(|s| sanitize(s, 64));
    }
    for action in &mut copy.actions {
        action.action = sanitize(&action.action, 180);
    }
    let mut text = actionable_status::render_text(&copy);
    if status.work.iter().any(|w| {
        w.limits
            .iter()
            .any(|l| l == "provider_coverage_unavailable")
    }) {
        text.push_str("\nProvider evidence is unavailable; this is not clear status.");
    }
    if status.work.len() > 4 || status.actions.len() > 4 {
        text.push_str("\nThis private reply lists at most four tasks and four personal actions; more evidence is available in the Hub.");
    }
    text
}

impl Slack {
    /// Only a small command grammar is accepted. The original body is never journaled.
    pub(super) fn enqueue_status_query(
        &self,
        state: &HubState,
        event: &Value,
        now: i64,
    ) -> Result<()> {
        self.active()?;
        let e = &event["event"];
        if event["type"] != "event_callback"
            || e.get("bot_id").is_some()
            || e.get("subtype").is_some()
        {
            return Ok(());
        }
        let user = e["user"]
            .as_str()
            .filter(|u| slack_id(u, 'U'))
            .ok_or_else(invalid)?;
        let id = event["event_id"]
            .as_str()
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
            .ok_or_else(invalid)?;
        let text = e["text"]
            .as_str()
            .filter(|s| s.len() <= 512)
            .ok_or_else(invalid)?;
        let mut words = text.split_whitespace();
        let mention = words.next().unwrap_or_default();
        if !mention.starts_with("<@") || !mention.ends_with('>') {
            return Ok(());
        }
        let command = words.next().unwrap_or_default().to_ascii_lowercase();
        if !matches!(command.as_str(), "status" | "needs" | "proceed") {
            return Ok(());
        }
        let selected = words.next();
        if words.next().is_some() {
            return Ok(());
        }
        let Some(binding) = self.bindings.iter().find(|b| b.config.slack_user == user) else {
            return Ok(());
        };
        if selected.is_some_and(|repo| repo != binding.config.repository) {
            return Ok(());
        }
        let _slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
        self.principal(state, binding)?;
        let key = binding_key(binding);
        let job = hash(format!("query:{}:{}:{id}:{key}", self.team, self.app).as_bytes());
        let mut db = self.database()?;
        let tx = db.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM status_jobs WHERE id=?1)",
            [&job],
            |r| r.get(0),
        )?;
        if exists {
            tx.commit()?;
            return Ok(());
        }
        let count: i64 = tx.query_row("SELECT count(*) FROM status_jobs", [], |r| r.get(0))?;
        if count >= CAPACITY {
            return Err(HubError::Busy);
        }
        tx.execute("INSERT OR IGNORE INTO status_jobs(id,binding,user,kind,state,created,due) VALUES(?1,?2,?3,'query','queued',?4,?4)", params![job,key,user,now])?;
        tx.commit()?;
        Ok(())
    }

    /// One binding per pass and at most one scan per minute keeps background reads bounded.
    pub(super) fn collect_status(&self, state: &HubState, now: i64) -> Result<()> {
        {
            let db = self.database()?;
            db.execute(
                "DELETE FROM status_jobs WHERE created<?1 AND state NOT IN ('queued','sending')",
                [now - JOB_TTL],
            )?;
            db.execute(
                "UPDATE status_jobs SET state='expired' WHERE created<?1 AND state='queued'",
                [now - JOB_TTL],
            )?;
            db.execute(
                "DELETE FROM message_bindings WHERE message NOT IN (SELECT id FROM messages)",
                [],
            )?;
            db.execute(
                "DELETE FROM status_scans WHERE next_due<?1",
                [now - 7 * JOB_TTL],
            )?;
            db.execute("DELETE FROM status_episodes WHERE state='resolved' AND first_seen<?1 AND id NOT IN (SELECT subject FROM status_jobs WHERE state IN ('queued','sending') AND subject IS NOT NULL)", [now-7*JOB_TTL])?;
            let configured: BTreeSet<String> = self.bindings.iter().map(binding_key).collect();
            let obsolete: Vec<(String, String)> = {
                let mut q = db.prepare(
                    "SELECT id,binding FROM status_episodes WHERE first_seen<?1 LIMIT 4096",
                )?;
                q.query_map([now - 7 * JOB_TTL], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?
            };
            for (id, binding) in obsolete {
                if !configured.contains(&binding) {
                    db.execute("DELETE FROM status_episodes WHERE id=?1", [id])?;
                }
            }
        }
        for binding in &self.bindings {
            if !binding.config.reminders.enabled {
                continue;
            }
            let key = binding_key(binding);
            let due: Option<i64> = self
                .database()?
                .query_row(
                    "SELECT next_due FROM status_scans WHERE binding=?1",
                    [&key],
                    |r| r.get(0),
                )
                .optional()?;
            if due.is_some_and(|d| d > now) {
                continue;
            }
            self.database()?.execute("INSERT INTO status_scans(binding,next_due) VALUES(?1,?2) ON CONFLICT(binding) DO UPDATE SET next_due=excluded.next_due", params![key,now+SCAN_SECONDS])?;
            return self.collect_status_binding(state, binding, now);
        }
        Ok(())
    }

    pub(super) fn collect_status_binding(
        &self,
        state: &HubState,
        binding: &Binding,
        now: i64,
    ) -> Result<()> {
        let _slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
        let principal = match self.principal(state, binding) {
            Ok(p) => p,
            Err(HubError::Unauthorized) => return Ok(()),
            Err(e) => return Err(e),
        };
        let status = actionable_status::read(&*state.store, &principal, now)?;
        let pending = outstanding(&status);
        let key = binding_key(binding);
        let policy = &binding.config.reminders;
        let mut db = self.database()?;
        let tx = db.transaction()?;
        let mut count: i64 =
            tx.query_row("SELECT count(*) FROM status_episodes", [], |r| r.get(0))?;
        for action in &pending {
            let subject = action.subject.key();
            let episode = hash(format!("{key}:{subject}").as_bytes());
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM status_episodes WHERE id=?1)",
                [&episode],
                |r| r.get(0),
            )?;
            if !exists && count >= EPISODE_LIMIT {
                return Err(HubError::Busy);
            }
            count += tx.execute("INSERT OR IGNORE INTO status_episodes(id,binding,subject,cycle,state,first_seen,next_due) VALUES(?1,?2,?3,1,'active',?4,?5)", params![episode,key,subject,now,now+policy.interval_seconds])? as i64;
            // A confirmed earlier resolution permits a new episode, not a retry of its old job.
            tx.execute("UPDATE status_episodes SET cycle=cycle+1,state='active',first_seen=?2,next_due=?3,delivered=0,escalated=0 WHERE id=?1 AND state='resolved'", params![episode,now,now+policy.interval_seconds])?;
            let (cycle, first, due, delivered, escalated): (i64,i64,i64,i64,bool) = tx.query_row("SELECT cycle,first_seen,next_due,delivered,escalated FROM status_episodes WHERE id=?1", [&episode], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
            if now >= due && delivered < policy.max_reminders {
                queue(
                    &tx,
                    &format!("{episode}:{cycle}:reminder:{}", delivered + 1),
                    &key,
                    &binding.config.slack_user,
                    "reminder",
                    (&episode, cycle),
                    now,
                )?;
            }
            if delivered > 0
                && !escalated
                && now >= first.saturating_add(policy.escalation_after_seconds)
                && let Some(user) = &policy.escalation_user
            {
                queue(
                    &tx,
                    &format!("{episode}:{cycle}:escalation"),
                    &key,
                    user,
                    "escalation",
                    (&episode, cycle),
                    now,
                )?;
            }
        }
        tx.commit()?;
        drop(db);
        // Absence from a bounded list alone is never resolution. Check each retained subject.
        let episodes: Vec<(String, String, i64, i64)> = {
            let db = self.database()?;
            let mut q = db.prepare("SELECT id,subject,cycle,delivered FROM status_episodes WHERE binding=?1 AND state='active' ORDER BY id LIMIT 100")?;
            q.query_map([&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<std::result::Result<_, _>>()?
        };
        for (episode, encoded, cycle, delivered) in episodes {
            let subject: Subject = serde_json::from_str(&encoded).map_err(|_| unavailable())?;
            if pending.iter().any(|a| a.subject == subject) {
                continue;
            }
            if confirmed_cleared(state, &principal, &subject, now)? {
                let mut db = self.database()?;
                let tx = db.transaction()?;
                tx.execute(
                    "UPDATE status_episodes SET state='resolved' WHERE id=?1 AND cycle=?2",
                    params![episode, cycle],
                )?;
                if delivered > 0 && policy.send_resolution {
                    queue(
                        &tx,
                        &format!("{episode}:{cycle}:resolution"),
                        &key,
                        &binding.config.slack_user,
                        "resolution",
                        (&episode, cycle),
                        now,
                    )?;
                }
                tx.commit()?;
            }
        }
        Ok(())
    }

    pub(super) async fn deliver_status(self: &Arc<Self>, state: &HubState) -> Result<bool> {
        let service = self.clone();
        let context = state.clone();
        let job = offload(move || service.prepare_status_job(&context, unix_now())).await?;
        let Some((id, user, message, attempts)) = job else {
            return Ok(false);
        };
        let result = self
            .api("chat.postMessage", {
                let mut b = message;
                b["channel"] = json!(user);
                b
            })
            .await;
        let service = self.clone();
        offload(move || {
            let now = unix_now();
            let mut db = service.database()?; let tx = db.transaction()?;
            match result {
                Ok(()) => {
                    let (kind, episode, cycle): (String,Option<String>,i64) = tx.query_row("SELECT kind,subject,cycle FROM status_jobs WHERE id=?1", [&id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
                    tx.execute("UPDATE status_jobs SET state='done' WHERE id=?1 AND state='sending'", [&id])?;
                    if let Some(episode) = episode {
                        if kind == "reminder" {
                            let key: String = tx.query_row("SELECT binding FROM status_jobs WHERE id=?1", [&id], |r| r.get(0))?;
                            let binding = service.bindings.iter().find(|b| binding_key(b) == key);
                            let interval = binding.map_or(3600,|b|b.config.reminders.interval_seconds);
                            tx.execute("UPDATE status_episodes SET delivered=delivered+1,next_due=?3 WHERE id=?1 AND cycle=?2", params![episode,cycle,now+interval])?;
                        } else if kind == "escalation" { tx.execute("UPDATE status_episodes SET escalated=1 WHERE id=?1 AND cycle=?2", params![episode,cycle])?; }
                    }
                }
                Err(delay) if delay > 0 && attempts + 1 < MAX_ATTEMPTS => { tx.execute("UPDATE status_jobs SET state='queued',due=?2 WHERE id=?1 AND state='sending'", params![id,now+delay as i64])?; }
                Err(_) => { tx.execute("UPDATE status_jobs SET state='failed' WHERE id=?1 AND state='sending'", [&id])?; }
            }
            tx.commit()?; Ok(true)
        }).await
    }

    pub(super) fn prepare_status_job(
        &self,
        state: &HubState,
        now: i64,
    ) -> Result<Option<(String, String, Value, i64)>> {
        self.active()?;
        type JobRow = (
            String,
            String,
            String,
            String,
            Option<String>,
            i64,
            i64,
            i64,
        );
        let job: Option<JobRow> = self.database()?.query_row(
            "SELECT id,binding,user,kind,subject,cycle,attempts,created FROM status_jobs WHERE state='queued' AND due<=?1 ORDER BY created,id LIMIT 1", [now],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
        let Some((id, key, user, kind, episode, cycle, attempts, created)) = job else {
            return Ok(None);
        };
        if attempts >= MAX_ATTEMPTS {
            self.database()?.execute(
                "UPDATE status_jobs SET state='failed' WHERE id=?1 AND state='queued'",
                [&id],
            )?;
            return Ok(None);
        }
        let Some(binding) = self.bindings.iter().find(|b| binding_key(b) == key) else {
            self.obsolete_status_job(&id)?;
            return Ok(None);
        };
        let _slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
        let principal = match self.principal(state, binding) {
            Ok(p) => p,
            Err(HubError::Unauthorized) => {
                self.obsolete_status_job(&id)?;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if now.saturating_sub(created) > JOB_TTL {
            self.obsolete_status_job(&id)?;
            return Ok(None);
        }
        if kind == "query" && user != binding.config.slack_user {
            return Err(HubError::Unauthorized);
        }
        let status = match actionable_status::read(&*state.store, &principal, now) {
            Ok(status) => status,
            Err(HubError::Unauthorized) => {
                self.obsolete_status_job(&id)?;
                return Ok(None);
            }
            Err(_) if kind == "query" => {
                let claimed = self.database()?.execute("UPDATE status_jobs SET state='sending',attempts=attempts+1 WHERE id=?1 AND state='queued'", [&id])?;
                return Ok((claimed == 1).then(||(id,user,body("Shared status is unavailable. No clear status was established; retry the status query or open the Hub for evidence."),attempts)));
            }
            Err(_) => {
                self.database()?.execute("UPDATE status_jobs SET due=?2,attempts=attempts+1 WHERE id=?1 AND state='queued'",params![id,now+SCAN_SECONDS])?;
                return Ok(None);
            }
        };
        let text = if kind == "query" {
            if user != binding.config.slack_user {
                return Err(HubError::Unauthorized);
            }
            render_status(&status)
        } else {
            if !binding.config.reminders.enabled {
                self.obsolete_status_job(&id)?;
                return Ok(None);
            }
            let Some(episode) = &episode else {
                return Err(unavailable());
            };
            let (encoded, current_cycle, episode_state): (String, i64, String) =
                self.database()?.query_row(
                    "SELECT subject,cycle,state FROM status_episodes WHERE id=?1",
                    [episode],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
            let subject: Subject = serde_json::from_str(&encoded).map_err(|_| unavailable())?;
            if current_cycle != cycle {
                self.obsolete_status_job(&id)?;
                return Ok(None);
            }
            if kind == "resolution" {
                if !binding.config.reminders.send_resolution
                    || user != binding.config.slack_user
                    || episode_state != "resolved"
                    || !confirmed_cleared(state, &principal, &subject, now)?
                {
                    self.obsolete_status_job(&id)?;
                    return Ok(None);
                }
                format!(
                    "An action previously sent to you for {} is no longer outstanding. This does not establish task success. Evidence {} (read {}; non-atomic).",
                    principal.repository, status.digest, now
                )
            } else {
                let pending = outstanding(&status);
                let Some(action) = pending.iter().find(|a| a.subject == subject) else {
                    self.obsolete_status_job(&id)?;
                    return Ok(None);
                };
                if episode_state != "active" {
                    self.obsolete_status_job(&id)?;
                    return Ok(None);
                }
                if kind == "escalation" {
                    if binding.config.reminders.escalation_user.as_deref() != Some(&user) {
                        self.obsolete_status_job(&id)?;
                        return Ok(None);
                    }
                    let Some(target) = self.bindings.iter().find(|b| {
                        b.config.slack_user == user
                            && b.config.provider == binding.config.provider
                            && b.config.host == binding.config.host
                            && b.config.repository_id == binding.config.repository_id
                            && b.config.repository == binding.config.repository
                    }) else {
                        self.obsolete_status_job(&id)?;
                        return Ok(None);
                    };
                    self.principal(state, target)?;
                } else if user != binding.config.slack_user {
                    return Err(HubError::Unauthorized);
                }
                format!(
                    "{} for {}: {}\nEvidence {} (read {}; non-atomic). {}",
                    if kind == "escalation" {
                        "Escalation requested by configured policy"
                    } else {
                        "Outstanding action reminder"
                    },
                    status.repository,
                    action.text,
                    status.digest,
                    now,
                    if status.complete {
                        "Observed coverage complete."
                    } else {
                        "Coverage incomplete; missing facts remain unknown."
                    }
                )
            }
        };
        let claimed = self.database()?.execute("UPDATE status_jobs SET state='sending',attempts=attempts+1 WHERE id=?1 AND state='queued'", [&id])?;
        if claimed != 1 {
            return Ok(None);
        }
        Ok(Some((id, user, body(&text), attempts)))
    }
    fn obsolete_status_job(&self, id: &str) -> Result<()> {
        self.database()?.execute(
            "UPDATE status_jobs SET state='suppressed' WHERE id=?1 AND state='queued'",
            [id],
        )?;
        Ok(())
    }
}
fn queue(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    binding: &str,
    user: &str,
    kind: &str,
    episode_cycle: (&str, i64),
    now: i64,
) -> Result<()> {
    let (episode, cycle) = episode_cycle;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM status_jobs WHERE id=?1)",
        [id],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(());
    }
    let count: i64 = tx.query_row("SELECT count(*) FROM status_jobs", [], |r| r.get(0))?;
    if count >= CAPACITY {
        return Err(HubError::Busy);
    }
    tx.execute("INSERT OR IGNORE INTO status_jobs(id,binding,user,kind,subject,cycle,state,created,due) VALUES(?1,?2,?3,?4,?5,?6,'queued',?7,?7)",params![id,binding,user,kind,episode,cycle,now])?;
    Ok(())
}
fn confirmed_cleared(
    state: &HubState,
    principal: &Principal,
    subject: &Subject,
    now: i64,
) -> Result<bool> {
    match subject {
        Subject::Work { id, generation, .. } => {
            let Some(item) = state.store.work_get(&principal.token_id, id, now)? else {
                return Ok(false);
            };
            let cleared = item.generation != *generation
                || matches!(item.state, WorkState::Active | WorkState::Finished);
            let same = state
                .store
                .work_get(&principal.token_id, id, now)?
                .is_some_and(|after| {
                    after.generation == item.generation && after.updated_seq == item.updated_seq
                });
            Ok(cleared && same)
        }
        Subject::Request {
            id,
            work_id,
            generation,
        } => {
            use crate::hub::coordination::{CoordinationListQuery, CoordinationStatus};
            let Some((request_id, encoded_work)) =
                id.strip_prefix("request:").and_then(|s| s.rsplit_once(':'))
            else {
                return Ok(false);
            };
            if encoded_work != work_id {
                return Ok(false);
            }
            let query = CoordinationListQuery {
                limit: 100,
                ..Default::default()
            };
            let page = state
                .store
                .coordination_list(&principal.token_id, &query, now)?;
            let Some(request) = page.requests.iter().find(|r| r.id == request_id) else {
                return Ok(false);
            };
            let terminal = matches!(
                request.status,
                CoordinationStatus::Resolved | CoordinationStatus::Withdrawn
            );
            let mut work_version = None;
            let answered = if terminal {
                true
            } else if request.responses_complete {
                let Some(item) = state.store.work_get(&principal.token_id, work_id, now)? else {
                    return Ok(false);
                };
                work_version = Some((item.updated_seq, item.generation));
                item.generation != *generation
                    || request
                        .targets
                        .iter()
                        .find(|t| t.work_id == *work_id)
                        .is_some_and(|target| {
                            target.generation == item.generation
                                && target.lead.token == item.lead_token
                                && target.lead.session == item.lead_session
                                && target.lead.agent == item.lead_agent
                                && request.responses.iter().any(|r| {
                                    r.target_work_id == *work_id
                                        && r.target_generation == target.generation
                                        && r.responder == target.lead
                                })
                        })
            } else {
                false
            };
            if !answered {
                return Ok(false);
            }
            if let Some((seq, generation)) = work_version
                && !state
                    .store
                    .work_get(&principal.token_id, work_id, now)?
                    .is_some_and(|w| w.updated_seq == seq && w.generation == generation)
            {
                return Ok(false);
            }
            let after = state
                .store
                .coordination_list(&principal.token_id, &query, now)?;
            Ok(after
                .requests
                .iter()
                .any(|r| r.id == request.id && r.updated_seq == request.updated_seq))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_status_uses_shared_counts_and_names_provider_failure_without_prose() {
        let mut status: ActionableStatus =
            serde_json::from_str(include_str!("../fixtures/actionable-status.json")).unwrap();
        status.work[0]
            .limits
            .push("provider_coverage_unavailable".into());
        status.work[0].title = "<@UOTHER> <https://example.test|unsafe>".into();
        let text = render_status(&status);
        assert!(text.contains(&format!("{} actions need you", status.counts.needs_you)));
        assert!(text.contains("Provider evidence is unavailable"));
        assert!(text.contains(&status.digest));
        let message = body(&text);
        assert!(!message["text"].as_str().unwrap().contains("<@"));
        assert_eq!(message["blocks"][0]["text"]["type"], "plain_text");
        assert_eq!(message["mrkdwn"], false);
    }
}
