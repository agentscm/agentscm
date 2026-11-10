//! Store limits that refuse agent writes (`HubError::WorkCapacity`). The refused agent is told
//! in its reply; the record here tells the operator, on the Health page and in the admin view,
//! which repository's limit refused and what to run. It is never a per-person obligation.

/// A refusal counts as recent on the Health page for this long.
pub const RECENT_SECONDS: i64 = 24 * 60 * 60;
/// Refusal rows are kept this long, one per repository and limit, at most `MAX_REFUSALS`.
pub const RETAIN_SECONDS: i64 = 30 * 24 * 60 * 60;
pub const MAX_REFUSALS: i64 = 200;

/// Which bounded store limit refused a write. Each has its own remedy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityLimit {
    /// The repository's retained stand-up payload budget.
    WorkStorage,
    /// Retained merge observations for catch-up.
    MergeObservations,
    /// Unacknowledged catch-up deliveries, by bytes or by count.
    CatchupDeliveries,
    /// Change-record events, actions and operations of one Work or session.
    ChangeRecords,
    /// Active coordination requests per repository, person or agent token.
    CoordinationRequests,
    /// Acknowledgements or responses on one coordination request.
    CoordinationUpdates,
}

const LIMITS: [CapacityLimit; 6] = [
    CapacityLimit::WorkStorage,
    CapacityLimit::MergeObservations,
    CapacityLimit::CatchupDeliveries,
    CapacityLimit::ChangeRecords,
    CapacityLimit::CoordinationRequests,
    CapacityLimit::CoordinationUpdates,
];

impl CapacityLimit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkStorage => "work_storage",
            Self::MergeObservations => "merge_observations",
            Self::CatchupDeliveries => "catchup_deliveries",
            Self::ChangeRecords => "change_records",
            Self::CoordinationRequests => "coordination_requests",
            Self::CoordinationUpdates => "coordination_updates",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        LIMITS.into_iter().find(|limit| limit.as_str() == text)
    }

    /// One plain sentence: what is full.
    pub fn what(self) -> &'static str {
        match self {
            Self::WorkStorage => "the repository's retained stand-up storage budget is full",
            Self::MergeObservations => "the repository's retained merge observations are full",
            Self::CatchupDeliveries => {
                "too many catch-up deliveries are waiting to be acknowledged"
            }
            Self::ChangeRecords => "one Work's change records reached their cap",
            Self::CoordinationRequests => "too many coordination requests are open",
            Self::CoordinationUpdates => "one coordination request reached its update cap",
        }
    }

    /// What the refused agent can do itself, or `None` when only the operator can help.
    pub fn agent_remedy(self) -> Option<String> {
        use crate::hub::coordination::{
            MAX_ACTIVE_COORDINATION_REQUESTS, MAX_ACTIVE_COORDINATION_REQUESTS_PER_OWNER,
            MAX_ACTIVE_COORDINATION_REQUESTS_PER_TOKEN,
        };
        match self {
            Self::WorkStorage | Self::MergeObservations => None,
            Self::CatchupDeliveries => Some("finish and acknowledge your open catch-up deliveries with agentscm_work_catchup_ack, or wait for them to expire".into()),
            Self::ChangeRecords => Some("finish this Work with agentscm_done and outcome complete, then start new Work for further changes".into()),
            Self::CoordinationRequests => Some(format!(
                "at most {MAX_ACTIVE_COORDINATION_REQUESTS_PER_TOKEN} are open per agent token, {MAX_ACTIVE_COORDINATION_REQUESTS_PER_OWNER} per person and {MAX_ACTIVE_COORDINATION_REQUESTS} per repository; resolve or withdraw your own open requests (agentscm_coordination_inbox lists them; agentscm_coordination_resolve or agentscm_coordination_withdraw ends one), then retry"
            )),
            Self::CoordinationUpdates => Some("resolve this coordination request with agentscm_coordination_resolve and open a new one if more coordination is needed".into()),
        }
    }
}

/// The repository whose limit refused, as `agentscm work compact` names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapacityRepository {
    pub provider: String,
    pub host: String,
    pub repository: String,
    pub repository_id: Option<i64>,
}

/// What refused. A refusal without a repository (a generic budget check whose caller names
/// none, such as the operator's own rename repair) is logged but not recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub limit: CapacityLimit,
    pub repository: Option<CapacityRepository>,
}

/// One retained row: the latest refusal of a repository's limit and how many there were.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapacityRefusal {
    pub limit: CapacityLimit,
    pub repository: CapacityRepository,
    pub at: i64,
    pub count: i64,
}

impl crate::hub::HubError {
    pub fn capacity(limit: CapacityLimit, repository: Option<CapacityRepository>) -> Self {
        Self::WorkCapacity(Box::new(Capacity { limit, repository }))
    }
}

impl CapacityRefusal {
    /// What the operator runs or asks for. The database and cutoff stay placeholders: the Hub
    /// never prints its own database credential.
    pub fn remedy(&self) -> String {
        use crate::hub::coordination::{
            MAX_ACTIVE_COORDINATION_REQUESTS, MAX_ACTIVE_COORDINATION_REQUESTS_PER_OWNER,
            MAX_ACTIVE_COORDINATION_REQUESTS_PER_TOKEN,
        };
        let r = &self.repository;
        match self.limit {
            CapacityLimit::WorkStorage | CapacityLimit::MergeObservations => format!(
                "agentscm work compact --database <the Hub's database> --provider {} --host {} --repository {}{} --before <a Unix time at least 24 hours ago>; inspect the dry run, then repeat it with --apply",
                r.provider,
                r.host,
                r.repository,
                r.repository_id
                    .map(|id| format!(" --repository-id {id}"))
                    .unwrap_or_default()
            ),
            CapacityLimit::CatchupDeliveries => "Ask the repository's agents to acknowledge their pending catch-up deliveries; unacknowledged deliveries expire on their own.".into(),
            CapacityLimit::ChangeRecords => "Ask the agent to finish that Work and start new Work for further changes; change records per Work are capped.".into(),
            CapacityLimit::CoordinationRequests => format!(
                "Resolve or withdraw open coordination requests from the Inbox; at most {MAX_ACTIVE_COORDINATION_REQUESTS} are open per repository, {MAX_ACTIVE_COORDINATION_REQUESTS_PER_OWNER} per person and {MAX_ACTIVE_COORDINATION_REQUESTS_PER_TOKEN} per agent token."
            ),
            CapacityLimit::CoordinationUpdates => "Resolve the coordination request from the Inbox; each request keeps a bounded number of acknowledgements and responses.".into(),
        }
    }
}

/// Records a refusal for the operator. A repository-less refusal and a failed write are only
/// logged: the refused caller already has its error, and recording must not change it.
pub fn record(store: &dyn crate::hub::store::Store, capacity: &Capacity, now: i64) {
    let Some(repository) = &capacity.repository else {
        eprintln!("hub: capacity refused a write: {}", capacity.limit.what());
        return;
    };
    let refusal = CapacityRefusal {
        limit: capacity.limit,
        repository: repository.clone(),
        at: now,
        count: 1,
    };
    if let Err(error) = store.record_capacity_refusal(&refusal) {
        eprintln!("hub: capacity refusal not recorded: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_round_trip_and_name_their_remedy() {
        for limit in LIMITS {
            assert_eq!(CapacityLimit::parse(limit.as_str()), Some(limit));
            let refusal = CapacityRefusal {
                limit,
                repository: CapacityRepository {
                    provider: "github".into(),
                    host: "github.com".into(),
                    repository: "acme/widgets".into(),
                    repository_id: Some(101),
                },
                at: 1,
                count: 1,
            };
            let remedy = refusal.remedy();
            let compacts = matches!(
                limit,
                CapacityLimit::WorkStorage | CapacityLimit::MergeObservations
            );
            assert_eq!(remedy.contains("agentscm work compact"), compacts);
            if compacts {
                assert!(remedy.contains(
                    "--provider github --host github.com --repository acme/widgets --repository-id 101"
                ));
            }
        }
        assert_eq!(CapacityLimit::parse("future_limit"), None);
    }
}
