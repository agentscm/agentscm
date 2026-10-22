//! Hub-wide switches for the inline MCP Apps views, stored as JSON under one settings key.
use serde::{Deserialize, Serialize};

use super::Result;
use super::store::Store;

const KEY: &str = "mcp_views";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewSettings {
    pub enabled: bool,
    pub work_overview: bool,
    pub coordination_inbox: bool,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            work_overview: true,
            coordination_inbox: true,
        }
    }
}

impl ViewSettings {
    /// A missing or corrupt stored value falls back to the default.
    pub fn load(store: &dyn Store) -> Result<Self> {
        Ok(store
            .get_setting(KEY)?
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default())
    }

    pub fn save(store: &dyn Store, settings: &Self, actor: &str) -> Result<()> {
        let raw = serde_json::to_string(settings)
            .map_err(|_| super::HubError::Unavailable("view settings encoding failed".into()))?;
        store.set_setting(KEY, &raw, actor)
    }
}
