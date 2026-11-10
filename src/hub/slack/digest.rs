//! The weekly digest as a direct message: the bound person's own stale unfinished Work, each with
//! buttons that release or complete it as that person, and the repository's repeated collisions.
//! It is queued as a status job once a week for each binding whose repository has the digest
//! switched on and whose person has not switched theirs off, and is read again just before it is
//! sent. It goes to the bound person's direct message only, never to a channel.
use super::status::binding_key;
use super::*;
use crate::hub::digest::{self, DigestQuery, StaleItem, WorkDigestV1};
use crate::hub::digest_http;
use crate::hub::work_transition::{
    WorkLifecycleReader, WorkTransitionActor, WorkTransitionKind, WorkTransitionRequest,
};

pub(super) const RELEASE: &str = "agentscm_digest_release";
pub(super) const COMPLETE: &str = "agentscm_digest_complete";
pub(super) const STOP: &str = "agentscm_digest_stop";

const WEEK: i64 = 7 * 86_400;
/// Weeks start on Monday at 09:00 UTC; the Unix epoch was a Thursday.
const WEEK_START: i64 = 4 * 86_400 + 9 * 3600;
/// How often the worker looks for people whose digest of the week is not queued yet.
const SCAN_SECONDS: i64 = 300;
/// A failed read is tried again after this long.
const RETRY_SECONDS: i64 = 60;
/// Stale Work rows one message shows, each with its own buttons.
const ROWS: usize = 5;
/// Blocks of a clicked message that are kept; Slack accepts 50.
const MAX_BLOCKS: usize = 40;

pub(super) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS digest_weeks (binding TEXT PRIMARY KEY, week INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS digest_scan (id INTEGER PRIMARY KEY CHECK(id=1), next_due INTEGER NOT NULL);",
    )?;
    Ok(())
}

/// The number of the week `now` falls in.
fn week(now: i64) -> i64 {
    (now - WEEK_START).div_euclid(WEEK)
}

/// Whether `action_id` is one of the digest's buttons.
pub(super) fn is_button(action_id: &str) -> bool {
    button_label(action_id).is_some()
}

fn button_label(action_id: &str) -> Option<&'static str> {
    match action_id {
        RELEASE => Some("Release"),
        COMPLETE => Some("Mark complete"),
        STOP => Some("Stop these digests"),
        _ => None,
    }
}

fn button(action_id: &str, label: &str, value: Option<&str>) -> Value {
    let mut button = json!({"type":"button","action_id":action_id,"text":plain_text(label,30)});
    if let Some(value) = value {
        button["value"] = json!(value);
    }
    button
}

fn section(text: &str) -> Value {
    json!({"type":"section","text":plain_lines(text, 2900)})
}

fn plural(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The buttons of one stale Work row: the transitions its owner may make now, and its Hub page.
/// The value carries the fences the digest read, so a click on Work that changed since is refused.
fn row_buttons(item: &StaleItem, link: &str) -> Vec<Value> {
    let value = json!({"w": item.work_id, "g": item.generation, "u": item.updated_seq}).to_string();
    let mut elements = Vec::new();
    // Both transitions look alike: neither is the better choice for every task.
    for (allowed, action_id) in [(item.can_complete, COMPLETE), (item.can_release, RELEASE)] {
        if let (true, Some(label)) = (allowed, button_label(action_id)) {
            elements.push(button(action_id, label, Some(&value)));
        }
    }
    elements.push(open_button(link));
    elements
}

impl Slack {
    /// The digest as one direct message, all plain text.
    fn digest_message(&self, digest: &WorkDigestV1) -> Value {
        let stale = &digest.stale;
        let collisions = &digest.collisions;
        let mut blocks = vec![
            json!({"type":"header","text":plain_text("Your weekly digest",150)}),
            section(&format!(
                "{}, {} to {} (UTC)",
                digest.repository,
                digest::day(digest.since),
                digest::day(digest.until)
            )),
        ];
        if !stale.items.is_empty() {
            let shown = stale.items.len().min(ROWS);
            let mut heading = format!(
                "Your unfinished work, quiet for {} days or more: {}",
                stale.quiet_days, stale.total
            );
            if stale.total > shown as u64 {
                heading.push_str(&format!(" ({shown} shown; the rest are in the Hub)"));
            }
            if !stale.complete {
                heading.push_str("\nYou have more unfinished work than one digest reads; this list may miss some.");
            }
            blocks.push(section(&heading));
            for item in stale.items.iter().take(ROWS) {
                blocks.push(section(&format!(
                    "{}\n{}",
                    item.title,
                    digest::stale_detail(item, digest.until)
                )));
                let buttons = row_buttons(item, &self.work_link(&item.work_id));
                blocks.push(json!({"type":"actions","elements":buttons}));
            }
        }
        if !collisions.hot_paths.is_empty() {
            let lines: Vec<String> = collisions
                .hot_paths
                .iter()
                .map(digest::hot_path_line)
                .collect();
            blocks.push(section(&format!(
                "Paths with the most refused or repeated edits\n{}",
                listed(&lines)
            )));
        }
        if !collisions.pairs.is_empty() {
            let lines: Vec<String> = collisions.pairs.iter().map(digest::pair_line).collect();
            blocks.push(section(&format!(
                "People whose agents collided on one path {} or more times\n{}",
                digest::PAIR_COLLISIONS,
                listed(&lines)
            )));
        }
        let mut guidance = String::from(
            "Release frees the files a task still holds and leaves it unfinished. Mark complete ends it.",
        );
        if let Some(coverage) = digest::coverage_line(collisions) {
            guidance.push(' ');
            guidance.push_str(&coverage);
        }
        blocks.push(json!({"type":"context","elements":[plain_text(&guidance,500)]}));
        let stop = button(STOP, button_label(STOP).unwrap_or_default(), None);
        blocks.push(json!({"type":"actions","elements":[stop]}));
        let summary = format!(
            "Your weekly digest for {}: {}, {}",
            digest.repository,
            plural(stale.total, "stale task", "stale tasks"),
            plural(
                collisions.hot_paths.len() as u64,
                "path with collisions",
                "paths with collisions"
            )
        );
        json!({"text": escape(&clip_plain_text(&summary, 300)), "blocks": blocks})
    }

    /// Queues this week's digest for every binding it is switched on for that has none yet.
    /// A binding it was off for gets the current week's digest when it is switched on.
    pub(super) fn queue_digests(&self, state: &HubState, now: i64) -> Result<()> {
        let due: Option<i64> = self
            .database()?
            .query_row("SELECT next_due FROM digest_scan WHERE id=1", [], |r| {
                r.get(0)
            })
            .optional()?;
        if due.is_some_and(|due| due > now) {
            return Ok(());
        }
        let week = week(now);
        // A binding left out of the configuration is forgotten once its row is two months old.
        self.database()?
            .execute("DELETE FROM digest_weeks WHERE week<?1", [week - 8])?;
        for binding in &self.bindings {
            let key = binding_key(binding);
            let last: Option<i64> = self
                .database()?
                .query_row(
                    "SELECT week FROM digest_weeks WHERE binding=?1",
                    [&key],
                    |r| r.get(0),
                )
                .optional()?;
            if last.is_some_and(|last| last >= week) {
                continue;
            }
            let delivering = {
                let _slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
                match self.principal(state, binding) {
                    Ok(who) => self.delivering(state, &who)?,
                    Err(HubError::Unauthorized) => false,
                    Err(error) => return Err(error),
                }
            };
            if !delivering {
                continue;
            }
            let job = hash(format!("digest:{}:{}:{key}:{week}", self.team, self.app).as_bytes());
            let mut db = self.database()?;
            let tx = db.transaction()?;
            let count: i64 = tx.query_row("SELECT count(*) FROM status_jobs", [], |r| r.get(0))?;
            if count >= CAPACITY {
                return Err(HubError::Busy);
            }
            tx.execute(
                "INSERT OR IGNORE INTO status_jobs(id,binding,user,kind,cycle,state,created,due) VALUES(?1,?2,?3,'digest',?4,'queued',?5,?5)",
                params![job, key, binding.config.slack_user, week, now],
            )?;
            tx.execute(
                "INSERT INTO digest_weeks(binding,week) VALUES(?1,?2) ON CONFLICT(binding) DO UPDATE SET week=excluded.week",
                params![key, week],
            )?;
            tx.commit()?;
        }
        self.database()?.execute(
            "INSERT INTO digest_scan(id,next_due) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET next_due=excluded.next_due",
            [now + SCAN_SECONDS],
        )?;
        Ok(())
    }

    /// Whether the repository has the digest on and this person has not switched theirs off.
    fn delivering(&self, state: &HubState, who: &Principal) -> Result<bool> {
        let settings = digest_http::settings(
            &*state.store,
            &who.repository,
            who.repository_id,
            &who.owner.id,
        )?;
        Ok(settings.delivering)
    }

    /// The message of a queued digest job (id, Slack user, attempts so far), read now. A job is
    /// dropped when it is not for the bound person, the digest was switched off since, or it
    /// would say nothing. The caller holds a store slot.
    pub(super) fn prepare_digest_job(
        &self,
        state: &HubState,
        binding: &Binding,
        who: &Principal,
        job: (String, String, i64),
        now: i64,
    ) -> Result<Option<(String, String, Value, i64)>> {
        let (id, user, attempts) = job;
        if user != binding.config.slack_user || !self.delivering(state, who)? {
            self.obsolete_status_job(&id)?;
            return Ok(None);
        }
        let digest = match digest::read(&*state.store, who, &DigestQuery::default(), now) {
            Ok(digest) => digest,
            Err(HubError::Unauthorized) => {
                self.obsolete_status_job(&id)?;
                return Ok(None);
            }
            Err(_) => {
                self.database()?.execute(
                    "UPDATE status_jobs SET due=?2,attempts=attempts+1 WHERE id=?1 AND state='queued'",
                    params![id, now + RETRY_SECONDS],
                )?;
                return Ok(None);
            }
        };
        if digest.is_empty() {
            self.obsolete_status_job(&id)?;
            return Ok(None);
        }
        let payload = self.digest_message(&digest);
        let claimed = self.database()?.execute(
            "UPDATE status_jobs SET state='sending',attempts=attempts+1 WHERE id=?1 AND state='queued'",
            [&id],
        )?;
        Ok((claimed == 1).then_some((id, user, payload, attempts)))
    }

    /// A digest button. The clicking Slack user must be bound and the Hub must still authorize
    /// the binding. A Work button changes only Work that person owns, through the same fenced
    /// transition the Hub's own pages use, as that person. `Ok` says what happened.
    fn digest_click(
        &self,
        state: &HubState,
        user: &str,
        action_id: &str,
        value: Option<&str>,
    ) -> std::result::Result<String, Refusal> {
        self.active()?;
        let _store_slot = state.permits.try_acquire().map_err(|_| HubError::Busy)?;
        let binding = self
            .bindings
            .iter()
            .find(|b| b.config.slack_user == user)
            .ok_or(HubError::Unauthorized)?;
        let who = self.principal(state, binding)?;
        let repository_id = who.repository_id.ok_or(HubError::Unauthorized)?;
        if action_id == STOP {
            digest_http::set_opted_out(&*state.store, &who.owner.id, repository_id, true)?;
            return Ok(format!(
                "Weekly digests for {} are off for you. Run agentscm digest opt-in in a checkout of it to get them again.",
                sanitize(&who.repository, TEXT_CHARS)
            ));
        }
        let transition = if action_id == RELEASE {
            WorkTransitionKind::Release
        } else {
            WorkTransitionKind::Complete
        };
        let button: Value =
            serde_json::from_str(value.ok_or_else(invalid)?).map_err(|_| invalid())?;
        let (Some(work_id), Some(generation), Some(updated_seq)) = (
            button["w"].as_str(),
            button["g"].as_i64(),
            button["u"].as_i64(),
        ) else {
            return Err(invalid().into());
        };
        let now = unix_now();
        let reader = WorkLifecycleReader::Person {
            owner: &who.owner,
            repository: &who.repository,
            repository_id,
        };
        let item = state
            .store
            .work_get_scoped(&reader, work_id, now)?
            .filter(|item| item.owner == who.owner.id)
            .ok_or(Refusal::NotYourWork)?;
        let title = sanitize(&item.title, TEXT_CHARS);
        if item.generation != generation || item.updated_seq != updated_seq {
            return Ok(format!(
                "Nothing changed. '{title}' changed after this digest was sent; open the Hub to see where it stands."
            ));
        }
        let (reason, outcome) = match transition {
            WorkTransitionKind::Release => (
                "Released by its owner from the weekly digest in Slack.",
                format!(
                    "Released '{title}'. Its files are free for other work; it stays unfinished until someone resumes or completes it."
                ),
            ),
            _ => (
                "Marked complete by its owner from the weekly digest in Slack.",
                format!("Marked '{title}' complete."),
            ),
        };
        let request = WorkTransitionRequest {
            work_id: work_id.to_owned(),
            operation_id: format!(
                "slack-digest:{work_id}:{generation}:{updated_seq}:{}",
                transition.as_str()
            ),
            generation,
            expected_updated_seq: updated_seq,
            session: None,
            transition,
            reason: reason.to_owned(),
            superseded_by: None,
        };
        let actor = WorkTransitionActor::Person {
            user: &who.owner,
            repository: &who.repository,
            repository_id,
            is_admin: false,
        };
        state.store.work_transition(&actor, &request, now)?;
        Ok(outcome)
    }
}

/// A clicked digest rebuilt as plain text for its update. Every block stays except the buttons of
/// the row that was clicked: the other rows keep theirs, so one message settles several tasks.
/// Only the digest's own buttons and links into this Hub are kept.
fn rebuilt(blocks: &[Value], clicked: (&str, Option<&str>), hub_url: &str) -> Vec<Value> {
    blocks
        .iter()
        .take(MAX_BLOCKS)
        .filter_map(|block| match block["type"].as_str()? {
            "actions" => {
                let elements = block["elements"].as_array()?;
                let is_clicked = elements.iter().any(|element| {
                    element["action_id"] == clicked.0 && element["value"].as_str() == clicked.1
                });
                let buttons: Vec<Value> = elements
                    .iter()
                    .filter_map(|element| {
                        let id = element["action_id"].as_str()?;
                        if id == OPEN {
                            let url = element["url"]
                                .as_str()
                                .filter(|url| url.starts_with(hub_url));
                            return Some(open_button(url.unwrap_or(hub_url)));
                        }
                        let value = element["value"].as_str().filter(|v| v.len() <= 2000);
                        button_label(id)
                            .filter(|_| !is_clicked)
                            .map(|label| button(id, label, value))
                    })
                    .collect();
                (!buttons.is_empty()).then(|| json!({"type":"actions","elements":buttons}))
            }
            "context" => {
                let texts: Vec<Value> = block["elements"]
                    .as_array()?
                    .iter()
                    .filter_map(|element| element["text"].as_str())
                    .take(10)
                    .map(|text| plain_text(text, 500))
                    .collect();
                (!texts.is_empty()).then(|| json!({"type":"context","elements":texts}))
            }
            // The parent's rebuild puts a section on one line; a digest section is a list.
            "section" => block["text"]["text"].as_str().map(section),
            _ => kept(block, hub_url),
        })
        .collect()
}

/// What a click on a digest button asks for: the outcome under the message, or a notice to the
/// person who clicked when the Hub refused them.
pub(super) fn prepare_click(
    state: &HubState,
    slack: &Slack,
    payload: &Value,
    user: &str,
    action: &Value,
) -> Result<Prepared> {
    let action_id = action["action_id"].as_str().ok_or_else(invalid)?;
    let value = action["value"].as_str().filter(|v| v.len() <= 2000);
    let channel = payload.pointer("/channel/id").and_then(Value::as_str);
    let outcome = match slack.digest_click(state, user, action_id, value) {
        Ok(outcome) => outcome,
        Err(refusal) => {
            return Ok(Prepared::Notice {
                channel: channel.unwrap_or(user).to_owned(),
                user: user.to_owned(),
                text: refusal.sentence(),
            });
        }
    };
    let (Some(channel), Some(ts)) = (
        channel,
        payload.pointer("/message/ts").and_then(Value::as_str),
    ) else {
        return Ok(Prepared::Response(json!({})));
    };
    let original = payload
        .pointer("/message/blocks")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |blocks| {
            rebuilt(blocks, (action_id, value), &slack.hub_url)
        });
    Ok(Prepared::Update {
        channel: channel.to_owned(),
        ts: ts.to_owned(),
        user: user.to_owned(),
        original,
        outcome,
    })
}
