//! The key a connected repository's Hub rows are stored under, for tests that read the store.

use agentscm::hub::store::{self, Store};

/// The scope of the repository connected as `full_name`; panics when none is connected.
pub(crate) fn connected(store: &dyn Store, full_name: &str) -> String {
    store
        .connections()
        .unwrap()
        .iter()
        .flat_map(|connection| store.repositories(&connection.id).unwrap())
        .find(|repository| repository.full_name.eq_ignore_ascii_case(full_name))
        .expect("a connected repository of that name")
        .repository_scope()
}

/// The scope of `repository` on github.com; see `store::repository_scope`.
pub(crate) fn github(repository: &str, repository_id: Option<i64>) -> String {
    store::repository_scope("github.com", repository, repository_id)
}
