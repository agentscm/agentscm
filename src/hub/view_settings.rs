//! Switches for the inline MCP Apps views: a hub default plus an optional per-user override.
use serde::{Deserialize, Serialize};

use super::Result;
use super::store::Store;

const KEY: &str = "mcp_views";

fn override_key(owner_id: &str) -> String {
    format!("{KEY}:{owner_id}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewSettings {
    pub enabled: bool,
    pub work_overview: bool,
    pub coordination_inbox: bool,
}

/// A user's own choices; `None` inherits the hub default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewOverride {
    pub enabled: Option<bool>,
    pub work_overview: Option<bool>,
    pub coordination_inbox: Option<bool>,
}

impl ViewOverride {
    /// Fields set in `change` replace the ones here.
    pub fn merge(self, change: Self) -> Self {
        Self {
            enabled: change.enabled.or(self.enabled),
            work_overview: change.work_overview.or(self.work_overview),
            coordination_inbox: change.coordination_inbox.or(self.coordination_inbox),
        }
    }

    pub fn apply(self, base: ViewSettings) -> ViewSettings {
        ViewSettings {
            enabled: self.enabled.unwrap_or(base.enabled),
            work_overview: self.work_overview.unwrap_or(base.work_overview),
            coordination_inbox: self.coordination_inbox.unwrap_or(base.coordination_inbox),
        }
    }
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

    /// A missing or corrupt override is no override.
    pub fn load_override(store: &dyn Store, owner_id: &str) -> Result<Option<ViewOverride>> {
        Ok(store
            .get_setting(&override_key(owner_id))?
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .filter(|o| *o != ViewOverride::default()))
    }

    /// Saving the default (all fields unset) is a reset to the hub default.
    ///
    /// shortcut: a caller's read-modify-write is last-writer-wins; only one user's own agents
    /// and UI write this key, so a lost update is a lost toggle. Upgrade if that matters.
    pub fn save_override(store: &dyn Store, owner_id: &str, value: &ViewOverride) -> Result<()> {
        let raw = serde_json::to_string(value)
            .map_err(|_| super::HubError::Unavailable("view settings encoding failed".into()))?;
        store.set_setting(&override_key(owner_id), &raw, owner_id)
    }

    pub fn effective(store: &dyn Store, owner_id: &str) -> Result<Self> {
        let base = Self::load(store)?;
        Ok(Self::load_override(store, owner_id)?.map_or(base, |o| o.apply(base)))
    }

    /// The JSON shape shared by the tool and the HTTP routes.
    pub fn report(store: &dyn Store, owner_id: &str) -> Result<serde_json::Value> {
        let hub_default = Self::load(store)?;
        let user = Self::load_override(store, owner_id)?;
        Ok(serde_json::json!({
            "effective": user.map_or(hub_default, |o| o.apply(hub_default)),
            "hub_default": hub_default,
            "override": user,
        }))
    }
}
