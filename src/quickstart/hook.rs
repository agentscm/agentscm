//! Merging the three AgentSCM hooks into a Claude Code settings file without
//! disturbing anything else in it.
use std::path::Path;

use serde_json::{Value, json};

use super::manifest::{Added, sha_hex};

/// The environment variable that holds the agent token for the HTTP hooks.
pub(crate) const TOKEN_VAR: &str = "AGENTSCM_HUB_TOKEN";

/// The hooks of hooks/claude-code.example.json, kept here because container builds copy only
/// `src/`; a test keeps the two equal.
const EXAMPLE: &str = r#"{"hooks": {
  "SessionStart": [{"hooks": [{"type": "command", "command": "agentscm intent start --hook claude"}]}],
  "PreToolUse": [{"matcher": "Edit|Write|MultiEdit",
    "hooks": [{"type": "command", "command": "agentscm intent check --hook claude", "timeout": 10}]}],
  "SessionEnd": [{"hooks": [{"type": "command", "command": "agentscm intent done --hook claude", "timeout": 5}]}]
}}"#;

/// The words of `command` from ` intent ` on, when its program is `agentscm`.
fn agentscm_tail(command: &str) -> Option<&str> {
    let at = command.find(" intent ")?;
    let program = command[..at].trim().trim_matches(['\'', '"']);
    (Path::new(program).file_name()? == "agentscm").then(|| &command[at..])
}

/// The same hook, whether `agentscm` was written bare or as a path.
fn same_hook(existing: &str, ours: &str) -> bool {
    existing == ours
        || agentscm_tail(existing).is_some_and(|tail| Some(tail) == agentscm_tail(ours))
}

/// What identifies a hook: its command, or for an HTTP hook its URL.
fn ident(hook: &Value) -> Option<&str> {
    hook["command"].as_str().or_else(|| hook["url"].as_str())
}

fn commands(group: &Value) -> impl Iterator<Item = &str> {
    group["hooks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(ident)
}

/// The HTTP form of the hooks for the hub at `hub` and, when known, `repo`. SessionStart is
/// absent because Claude Code does not support HTTP hooks for it. With `?repo=owner/name`
/// the hub ignores a token that belongs to another repository; the repo has passed
/// `valid_repo`, so it needs no URL encoding.
pub(crate) fn http_entries(hub: &str, repo: Option<&str>) -> Vec<Added> {
    let guard = repo.map_or_else(String::new, |r| format!("?repo={r}"));
    let hook = json!({
        "type": "http",
        "url": format!("{hub}/v1/hub/hook/claude{guard}"),
        "headers": {"Authorization": format!("Bearer ${TOKEN_VAR}")},
        "allowedEnvVars": [TOKEN_VAR],
        "timeout": 5,
    });
    [
        (
            "PreToolUse",
            json!({"matcher": "Edit|Write|MultiEdit", "hooks": [hook]}),
        ),
        ("SessionEnd", json!({"hooks": [hook]})),
    ]
    .into_iter()
    .map(|(event, entry)| Added {
        event: event.to_owned(),
        entry,
    })
    .collect()
}

/// The three hook groups of hooks/claude-code.example.json, calling `program`.
pub(crate) fn entries(program: &str) -> Vec<Added> {
    let example: Value = serde_json::from_str(EXAMPLE).unwrap_or_default();
    let mut out = Vec::new();
    for (event, groups) in example["hooks"].as_object().into_iter().flatten() {
        for group in groups.as_array().into_iter().flatten() {
            let mut group = group.clone();
            for hook in group["hooks"].as_array_mut().into_iter().flatten() {
                if let Some(rest) = hook["command"]
                    .as_str()
                    .and_then(|c| c.strip_prefix("agentscm"))
                {
                    hook["command"] = json!(format!("{program}{rest}"));
                }
            }
            out.push(Added {
                event: event.clone(),
                entry: group,
            });
        }
    }
    out
}

fn shape() -> String {
    "its \"hooks\" entry is not the shape Claude Code uses".into()
}

/// Adds each entry whose command is not already hooked to the same event; returns
/// what was added. `Err` leaves `settings` unchanged.
pub(crate) fn merge(settings: &mut Value, wanted: &[Added]) -> Result<Vec<Added>, String> {
    let mut next = settings.clone();
    let root = next.as_object_mut().ok_or("it is not a JSON object")?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or_else(shape)?;
    let mut added = Vec::new();
    for want in wanted {
        let groups = hooks.entry(want.event.clone()).or_insert_with(|| json!([]));
        let groups = groups.as_array_mut().ok_or_else(shape)?;
        let present = commands(&want.entry).all(|ours| {
            groups
                .iter()
                .any(|g| commands(g).any(|c| same_hook(c, ours)))
        });
        if !present {
            groups.push(want.entry.clone());
            added.push(want.clone());
        }
    }
    *settings = next;
    Ok(added)
}

/// Takes back entries quickstart added: the identical group, else just the hook with the
/// same command inside a group the user has since edited. A group, event or `hooks` object
/// is dropped only when taking our entry out is what emptied it. Returns how many hooks
/// were removed.
pub(crate) fn remove(settings: &mut Value, added: &[Added]) -> usize {
    remove_matching(settings, added, |a, b| a == b)
}

/// Takes out the quickstart command-form hooks (`agentscm … intent start|check|done --hook
/// claude`, whatever path runs `agentscm`) so the HTTP form does not announce edits twice.
pub(crate) fn remove_command_form(settings: &mut Value) -> usize {
    remove_matching(settings, &entries("agentscm"), same_hook)
}

fn remove_matching(
    settings: &mut Value,
    added: &[Added],
    same: impl Fn(&str, &str) -> bool,
) -> usize {
    let mut removed = 0;
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut emptied = Vec::new();
    for add in added {
        let Some(groups) = hooks.get_mut(&add.event).and_then(Value::as_array_mut) else {
            continue;
        };
        let before = removed;
        if let Some(at) = groups.iter().position(|g| *g == add.entry) {
            groups.remove(at);
            removed += 1;
        } else {
            let ours: Vec<String> = commands(&add.entry).map(str::to_owned).collect();
            let mut i = 0;
            while i < groups.len() {
                let mut drop = false;
                if let Some(list) = groups[i]["hooks"].as_array_mut() {
                    let had = list.len();
                    list.retain(|h| !ident(h).is_some_and(|c| ours.iter().any(|o| same(c, o))));
                    removed += had - list.len();
                    drop = had > list.len() && list.is_empty();
                }
                if drop {
                    groups.remove(i);
                } else {
                    i += 1;
                }
            }
        }
        if removed > before && groups.is_empty() {
            emptied.push(add.event.clone());
        }
    }
    for event in emptied {
        hooks.remove(&event);
    }
    if removed > 0
        && hooks.is_empty()
        && let Some(root) = settings.as_object_mut()
    {
        root.remove("hooks");
    }
    removed
}

/// What `set_env` did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Env {
    Set,
    /// Already holds this token, written by quickstart.
    Same,
    /// Holds a value quickstart did not write; left alone.
    Foreign,
}

/// Puts `token` in `env.AGENTSCM_HUB_TOKEN`, unless that holds a value other than the one
/// quickstart recorded (by its SHA-256). `Err` leaves `settings` unchanged.
pub(crate) fn set_env(
    settings: &mut Value,
    token: &str,
    recorded_sha: Option<&str>,
) -> Result<Env, String> {
    let mut next = settings.clone();
    let root = next.as_object_mut().ok_or("it is not a JSON object")?;
    let env = root.entry("env").or_insert_with(|| json!({}));
    let env = env
        .as_object_mut()
        .ok_or("its \"env\" entry is not a JSON object")?;
    let ours = |v: &Value| {
        v.as_str()
            .is_some_and(|s| recorded_sha == Some(sha_hex(s.as_bytes()).as_str()))
    };
    let result = match env.get(TOKEN_VAR) {
        Some(v) if !ours(v) => return Ok(Env::Foreign),
        Some(v) if v.as_str() == Some(token) => Env::Same,
        _ => Env::Set,
    };
    env.insert(TOKEN_VAR.to_owned(), json!(token));
    *settings = next;
    Ok(result)
}

/// Takes `env.AGENTSCM_HUB_TOKEN` out when its value hashes to `sha`, and `env` when that
/// empties it.
pub(crate) fn remove_env(settings: &mut Value, sha: &str) {
    let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) else {
        return;
    };
    if env
        .get(TOKEN_VAR)
        .and_then(Value::as_str)
        .is_some_and(|v| sha_hex(v.as_bytes()) == sha)
    {
        env.remove(TOKEN_VAR);
        if env.is_empty()
            && let Some(root) = settings.as_object_mut()
        {
            root.remove("env");
        }
    }
}

/// What quickstart is about to add, as it will appear in the file. The token itself is
/// never shown: `with_token` stands a placeholder in for it.
pub(crate) fn preview(added: &[Added], with_token: bool) -> String {
    let mut hooks = serde_json::Map::new();
    for add in added {
        if let Some(groups) = hooks
            .entry(add.event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()
        {
            groups.push(add.entry.clone());
        }
    }
    let mut shown = serde_json::Map::new();
    if with_token {
        shown.insert("env".to_owned(), json!({TOKEN_VAR: "<your agent token>"}));
    }
    if !hooks.is_empty() {
        shown.insert("hooks".to_owned(), Value::Object(hooks));
    }
    serde_json::to_string_pretty(&shown).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_match_the_example_file() {
        let file: Value =
            serde_json::from_str(include_str!("../../hooks/claude-code.example.json")).unwrap();
        assert_eq!(serde_json::from_str::<Value>(EXAMPLE).unwrap(), file);
    }

    #[test]
    fn merges_beside_unrelated_hooks_and_is_idempotent() {
        let wanted = entries("/opt/agentscm");
        assert_eq!(wanted.len(), 3);
        let mut settings = json!({
            "model": "x",
            "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "other"}]}]}
        });
        let added = merge(&mut settings, &wanted).unwrap();
        assert_eq!(added.len(), 3);
        assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(settings["model"], "x");
        assert!(merge(&mut settings, &wanted).unwrap().is_empty());
        // A bare `agentscm` hook already there counts as present.
        let mut bare = json!({"hooks": {}});
        merge(&mut bare, &entries("agentscm")).unwrap();
        assert!(merge(&mut bare, &wanted).unwrap().is_empty());
        remove(&mut settings, &added);
        assert_eq!(
            settings,
            json!({"model": "x", "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "other"}]}]}})
        );
    }

    #[test]
    fn a_missing_hooks_key_is_created_and_removed_again() {
        let mut settings = json!({"model": "x"});
        let added = merge(&mut settings, &entries("agentscm")).unwrap();
        assert_eq!(added.len(), 3);
        assert_eq!(remove(&mut settings, &added), 3);
        assert_eq!(settings, json!({"model": "x"}));
    }

    #[test]
    fn a_user_hook_with_the_same_matcher_does_not_count_as_ours() {
        let mut settings = json!({"hooks": {"PreToolUse": [
            {"matcher": "Edit|Write|MultiEdit", "hooks": [{"type": "command", "command": "lint.sh"}]}]}});
        let added = merge(&mut settings, &entries("/p/agentscm")).unwrap();
        assert_eq!(added.len(), 3);
        let groups = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1]["matcher"], "Edit|Write|MultiEdit");
        assert_eq!(groups[1]["hooks"][0]["timeout"], 10);
    }

    #[test]
    fn removal_keeps_what_the_user_had_and_follows_edits() {
        let mut settings = json!({"hooks": {"Stop": [], "SessionEnd": [{"hooks": []}]}});
        let added = merge(&mut settings, &entries("agentscm")).unwrap();
        // The user merged an extra hook into our PreToolUse group.
        settings["hooks"]["PreToolUse"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": "mine"}));
        assert_eq!(remove(&mut settings, &added), 3);
        assert_eq!(
            settings,
            json!({"hooks": {
                "Stop": [],
                "SessionEnd": [{"hooks": []}],
                "PreToolUse": [{"matcher": "Edit|Write|MultiEdit", "hooks": [{"type": "command", "command": "mine"}]}]
            }})
        );
    }

    #[test]
    fn refuses_odd_shapes_without_changing_them() {
        let wanted = entries("agentscm");
        for text in [
            r#"[]"#,
            r#"{"hooks": []}"#,
            r#"{"hooks": {"SessionEnd": {}}}"#,
        ] {
            let mut value: Value = serde_json::from_str(text).unwrap();
            let before = value.clone();
            assert!(merge(&mut value, &wanted).is_err());
            assert_eq!(value, before);
        }
    }

    const HUB: &str = "https://hub.example.com";

    #[test]
    fn http_hooks_match_the_example_file() {
        let file: Value =
            serde_json::from_str(include_str!("../../hooks/claude-code.http.example.json"))
                .unwrap();
        let made: Value =
            serde_json::from_str(&preview(&http_entries(HUB, Some("acme/widgets")), false))
                .unwrap();
        assert_eq!(made, file);
    }

    #[test]
    fn http_hooks_merge_beside_unrelated_hooks_are_idempotent_and_removable() {
        let wanted = http_entries(HUB, None);
        assert_eq!(wanted.len(), 2);
        let original = json!({
            "model": "x",
            "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "http", "url": "https://other.example.com/h"}]}]}
        });
        let mut settings = original.clone();
        let added = merge(&mut settings, &wanted).unwrap();
        assert_eq!(added.len(), 2);
        assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert!(merge(&mut settings, &wanted).unwrap().is_empty());
        // Another hub's hook is not ours.
        let other = http_entries("https://hub.example.org", None);
        assert_eq!(merge(&mut settings, &other).unwrap().len(), 2);
        assert_eq!(remove(&mut settings, &other), 2);
        assert_eq!(remove(&mut settings, &added), 2);
        assert_eq!(settings, original);
    }

    #[test]
    fn http_removal_follows_user_edits_by_url() {
        let mut settings = json!({});
        let added = merge(&mut settings, &http_entries(HUB, None)).unwrap();
        settings["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"] = json!(9);
        settings["hooks"]["SessionEnd"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "command", "command": "mine"}));
        assert_eq!(remove(&mut settings, &added), 2);
        assert_eq!(
            settings,
            json!({"hooks": {"SessionEnd": [{"hooks": [{"type": "command", "command": "mine"}]}]}})
        );
    }

    #[test]
    fn the_older_command_form_is_replaced_whatever_the_program_path() {
        let mut settings = json!({"model": "x"});
        merge(&mut settings, &entries("'/home/me/.agentscm/bin/agentscm'")).unwrap();
        settings["hooks"]["PreToolUse"]
            .as_array_mut()
            .unwrap()
            .push(json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "mine intent check --hook claude"}]}));
        assert_eq!(remove_command_form(&mut settings), 3);
        let added = merge(&mut settings, &http_entries(HUB, None)).unwrap();
        assert_eq!(added.len(), 2);
        assert!(settings["hooks"].get("SessionStart").is_none());
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(
            pre[0]["hooks"][0]["command"],
            "mine intent check --hook claude"
        );
    }

    #[test]
    fn the_token_env_is_set_restored_and_a_foreign_value_is_kept() {
        let mut settings = json!({"env": {"OTHER": "1"}});
        let sha = sha_hex(b"tok1");
        assert_eq!(set_env(&mut settings, "tok1", None), Ok(Env::Set));
        assert_eq!(settings["env"][TOKEN_VAR], "tok1");
        // Recorded and unchanged.
        assert_eq!(set_env(&mut settings, "tok1", Some(&sha)), Ok(Env::Same));
        // Recorded, and the token has been replaced since: ours to update.
        assert_eq!(set_env(&mut settings, "tok2", Some(&sha)), Ok(Env::Set));
        assert_eq!(settings["env"][TOKEN_VAR], "tok2");
        // A value nobody recorded is left alone, even when it is the same token.
        let before = settings.clone();
        assert_eq!(set_env(&mut settings, "tok3", Some(&sha)), Ok(Env::Foreign));
        assert_eq!(set_env(&mut settings, "tok2", None), Ok(Env::Foreign));
        assert_eq!(settings, before);
        // Removal needs the recorded hash, and drops `env` only when it empties.
        remove_env(&mut settings, &sha);
        assert_eq!(settings, before);
        remove_env(&mut settings, &sha_hex(b"tok2"));
        assert_eq!(settings, json!({"env": {"OTHER": "1"}}));
        let mut only = json!({"env": {TOKEN_VAR: "tok1"}});
        remove_env(&mut only, &sha);
        assert_eq!(only, json!({}));
        assert!(set_env(&mut json!({"env": []}), "t", None).is_err());
    }

    #[test]
    fn the_preview_never_shows_a_token() {
        let text = preview(&http_entries(HUB, None), true);
        assert!(text.contains("<your agent token>"));
        assert!(!text.contains("tok-secret"));
        let mut settings = json!({});
        set_env(&mut settings, "tok-secret", None).unwrap();
        assert!(!preview(&http_entries(HUB, None), true).contains("tok-secret"));
    }
}
