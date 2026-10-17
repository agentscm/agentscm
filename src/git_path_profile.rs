#[cfg(unix)]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::collections::btree_map::Entry;

pub(crate) const RESERVED_NAMES: [&str; 4] =
    [".git", ".agentscm", ".agentscm-workspace", "agentscm-data"];

pub(crate) fn uses_reserved_namespace(path: &str) -> bool {
    path.split('/').any(|part| {
        RESERVED_NAMES
            .iter()
            .any(|name| part.eq_ignore_ascii_case(name))
    })
}

#[cfg(unix)]
#[derive(Default)]
pub(crate) struct PortableSpellings {
    prefixes: BTreeMap<String, String>,
}

#[cfg(unix)]
impl PortableSpellings {
    pub(crate) fn insert(&mut self, path: &str) -> Option<String> {
        let mut alias = None;
        for end in path
            .match_indices('/')
            .map(|(index, _)| index)
            .chain(std::iter::once(path.len()))
        {
            let prefix = &path[..end];
            match self.prefixes.entry(prefix.to_ascii_lowercase()) {
                Entry::Vacant(entry) => {
                    entry.insert(prefix.to_owned());
                }
                Entry::Occupied(entry) if entry.get() != prefix && alias.is_none() => {
                    alias = Some(entry.get().clone());
                }
                Entry::Occupied(_) => {}
            }
        }
        alias
    }

    pub(crate) fn len(&self) -> usize {
        self.prefixes.len()
    }
}
