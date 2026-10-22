use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value as TomlValue, value};

use super::{ClientAgent, PinnedWorkspace, invalid};
use crate::{Error, Result};

const MAX_SETTINGS_BYTES: u64 = 256 * 1024;
#[derive(Clone, Debug)]
pub struct InstallRequest {
    pub agent: ClientAgent,
    pub hook_settings: PathBuf,
    pub mcp_settings: PathBuf,
    pub executable: PathBuf,
    pub pinned: PinnedWorkspace,
    pub apply: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct FilePreview {
    pub path: PathBuf,
    pub action: &'static str,
    pub before: Option<String>,
    pub after: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstallPreview {
    pub applied: bool,
    pub files: Vec<FilePreview>,
}

fn safe_literal(path: &Path, label: &str) -> Result<String> {
    let value = path
        .to_str()
        .filter(|value| {
            !value
                .chars()
                .any(|character| character.is_control() || matches!(character, '$' | '%' | '`'))
        })
        .ok_or_else(|| invalid(format!("{label} is not a safe UTF-8 path")))?;
    if !path.is_absolute() {
        return Err(invalid(format!("{label} must be absolute")));
    }
    Ok(value.to_owned())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn args(request: &InstallRequest, subcommand: &str) -> Result<Vec<String>> {
    let mut args = vec![
        "--no-telemetry".to_owned(),
        "--data-dir".to_owned(),
        safe_literal(&request.pinned.data_dir, "data directory")?,
        "workspace-client".to_owned(),
        subcommand.to_owned(),
        "--workspace".to_owned(),
        safe_literal(&request.pinned.workspace_root, "workspace root")?,
    ];
    if let Some(root) = &request.pinned.fork_root {
        args.push("--fork-root".to_owned());
        args.push(safe_literal(root, "fork root")?);
    }
    Ok(args)
}

fn command(request: &InstallRequest) -> Result<String> {
    let mut words = vec![shell_quote(&safe_literal(
        &request.executable,
        "executable",
    )?)];
    words.extend(
        args(request, "hook")?
            .into_iter()
            .map(|word| shell_quote(&word)),
    );
    words.extend(["--agent".to_owned(), shell_quote(request.agent.name())]);
    Ok(words.join(" "))
}

fn hook_entry(request: &InstallRequest) -> Result<(&'static str, Value)> {
    Ok(match request.agent {
        ClientAgent::Claude | ClientAgent::Codex => (
            "Stop",
            json!({"hooks":[{"type":"command","command":command(request)?,"timeout":30}]}),
        ),
        ClientAgent::Cursor => (
            "stop",
            json!({"command":command(request)?,"timeout":30,"loop_limit":null,"failClosed":false}),
        ),
        ClientAgent::Copilot => {
            let mut arguments = args(request, "hook")?;
            arguments.extend(["--agent".to_owned(), request.agent.name().to_owned()]);
            (
                "agentStop",
                json!({"type":"command","exec":safe_literal(&request.executable,"executable")?,"args":arguments,"timeoutSec":30}),
            )
        }
        ClientAgent::Gemini => (
            "AfterAgent",
            json!({"sequential":true,"hooks":[{"type":"command","command":command(request)?,"name":"AgentSCM workspace capture","timeout":30000}]}),
        ),
    })
}

fn own_capture(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(own_capture),
        Value::Object(values) => values.values().any(own_capture),
        Value::String(value) => {
            value.contains("workspace-client")
                && value.contains("hook")
                && value.contains("agentscm")
        }
        _ => false,
    }
}

fn workflow_stop(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(workflow_stop),
        Value::Object(values) => values.values().any(workflow_stop),
        Value::String(value) => value.contains("skill-workflow") && value.contains("stop"),
        _ => false,
    }
}

fn add_hook(root: &mut Value, request: &InstallRequest) -> Result<()> {
    let top = root
        .as_object_mut()
        .ok_or_else(|| invalid("hook settings must be a JSON object"))?;
    if matches!(request.agent, ClientAgent::Cursor | ClientAgent::Copilot) {
        if top.get("version").is_some_and(|version| version != 1) {
            return Err(invalid("hook settings use an unsupported version"));
        }
        top.insert("version".into(), json!(1));
    }
    let hooks = top
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid("hooks must be a JSON object"))?;
    let (event, entry) = hook_entry(request)?;
    let entries = hooks
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| invalid("completion hooks must be an array"))?;
    if entries.iter().any(|existing| existing == &entry) {
        return Ok(());
    }
    if entries.iter().any(own_capture) {
        return Err(invalid(
            "another AgentSCM workspace capture hook exists; remove or migrate it explicitly",
        ));
    }
    if matches!(request.agent, ClientAgent::Claude | ClientAgent::Codex)
        && entries.iter().any(workflow_stop)
    {
        return Err(Error::Conflict(
            "an AgentSCM workflow Stop hook already exists and records its own timeline check links; use explicit MCP capture when an additional observation is needed".into(),
        ));
    }
    entries.push(entry);
    Ok(())
}

fn mcp_value(request: &InstallRequest) -> Result<Value> {
    let executable = safe_literal(&request.executable, "executable")?;
    let args = args(request, "mcp")?;
    let mut value = json!({"command":executable,"args":args});
    match request.agent {
        ClientAgent::Gemini => {}
        ClientAgent::Copilot => value["type"] = json!("local"),
        _ => value["type"] = json!("stdio"),
    }
    Ok(value)
}

fn add_mcp(root: &mut Value, request: &InstallRequest) -> Result<()> {
    let key = "mcpServers";
    let top = root
        .as_object_mut()
        .ok_or_else(|| invalid("MCP settings must be a JSON object"))?;
    let servers = top
        .entry(key)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid("MCP servers must be a JSON object"))?;
    let wanted = mcp_value(request)?;
    if let Some(existing) = servers.get("agentscm-workspace")
        && existing != &wanted
    {
        return Err(invalid(
            "an existing agentscm-workspace MCP entry differs; review it before migration",
        ));
    }
    servers.insert("agentscm-workspace".into(), wanted);
    Ok(())
}

fn read(path: &Path) -> Result<Option<String>> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(invalid("settings path is not a regular file"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(Error::Limit("settings file exceeds 256 KiB".into()));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| invalid("settings file is not UTF-8"))
}

fn normalized_settings_path(path: &Path, pinned: &PinnedWorkspace) -> Result<PathBuf> {
    if !path.is_absolute() || path.to_str().is_none() {
        return Err(invalid("settings path must be absolute UTF-8"));
    }
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(invalid("settings path is a symbolic link"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("settings path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("settings path has no file name"))?;
    let parent =
        fs::canonicalize(parent).map_err(|_| invalid("settings parent must already exist"))?;
    if parent.starts_with(&pinned.workspace_root)
        || pinned
            .fork_root
            .as_ref()
            .is_some_and(|root| parent.starts_with(root))
    {
        return Err(invalid(
            "settings must be outside the workspace and fork root",
        ));
    }
    Ok(parent.join(name))
}

fn json_text(old: Option<&str>) -> Result<Value> {
    match old {
        Some(text) => serde_json::from_str(text)
            .map_err(|_| invalid("settings must be strict JSON before applying changes")),
        None => Ok(json!({})),
    }
}

fn preview(path: PathBuf, before: Option<String>, after: String) -> FilePreview {
    let action = if before.as_deref() == Some(&after) {
        "unchanged"
    } else if before.is_some() {
        "update"
    } else {
        "create"
    };
    FilePreview {
        path,
        action,
        before,
        after,
    }
}

fn codex_toml(old: Option<&str>, request: &InstallRequest) -> Result<String> {
    let old = old.unwrap_or_default();
    let executable = safe_literal(&request.executable, "executable")?;
    let arguments = args(request, "mcp")?;
    let mut document = old
        .parse::<DocumentMut>()
        .map_err(|_| invalid("Codex MCP settings must be valid TOML"))?;
    let servers = document
        .entry("mcp_servers")
        .or_insert_with(|| Item::Table(Table::new()));
    if let Some(existing) = servers
        .as_table_like()
        .and_then(|servers| servers.get("agentscm-workspace"))
    {
        if codex_server_matches(existing, &executable, &arguments) {
            return Ok(old.to_owned());
        }
        return Err(invalid(
            "an existing agentscm-workspace MCP entry differs; review it before migration",
        ));
    }
    if let Some(inline) = servers.as_inline_table_mut() {
        inline.insert(
            "agentscm-workspace",
            TomlValue::InlineTable(codex_inline_server(&executable, &arguments)),
        );
    } else {
        let servers = servers
            .as_table_like_mut()
            .ok_or_else(|| invalid("mcp_servers must be a TOML table"))?;
        servers.insert(
            "agentscm-workspace",
            Item::Table(codex_server_table(&executable, &arguments)),
        );
    }
    let rendered = document.to_string();
    rendered
        .parse::<DocumentMut>()
        .map_err(|_| invalid("generated Codex MCP settings are invalid"))?;
    Ok(rendered)
}

fn codex_args(arguments: &[String]) -> Array {
    let mut array = Array::new();
    for argument in arguments {
        array.push(argument.as_str());
    }
    array
}

fn codex_server_table(executable: &str, arguments: &[String]) -> Table {
    let mut table = Table::new();
    table.insert("command", value(executable));
    table.insert("args", value(codex_args(arguments)));
    table.insert("startup_timeout_sec", value(10));
    table.insert("tool_timeout_sec", value(30));
    table
}

fn codex_inline_server(executable: &str, arguments: &[String]) -> InlineTable {
    let mut table = InlineTable::new();
    table.insert("command", TomlValue::from(executable));
    table.insert("args", TomlValue::Array(codex_args(arguments)));
    table.insert("startup_timeout_sec", TomlValue::from(10));
    table.insert("tool_timeout_sec", TomlValue::from(30));
    table
}

fn codex_server_matches(existing: &Item, executable: &str, arguments: &[String]) -> bool {
    let Some(table) = existing.as_table_like() else {
        return false;
    };
    if table.iter().count() != 4
        || table.get("command").and_then(Item::as_str) != Some(executable)
        || table.get("startup_timeout_sec").and_then(Item::as_integer) != Some(10)
        || table.get("tool_timeout_sec").and_then(Item::as_integer) != Some(30)
    {
        return false;
    }
    let Some(actual) = table.get("args").and_then(Item::as_array) else {
        return false;
    };
    actual.len() == arguments.len()
        && actual
            .iter()
            .zip(arguments)
            .all(|(actual, expected)| actual.as_str() == Some(expected))
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("settings path has no parent"))?;
    let temporary = parent.join(format!(
        ".agentscm-workspace-client-{}.tmp",
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary)?;
    if let Err(error) = (|| -> std::io::Result<()> {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })() {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// Preview by default and apply only when explicitly requested.
pub fn install(request: &InstallRequest) -> Result<InstallPreview> {
    let hook_settings = normalized_settings_path(&request.hook_settings, &request.pinned)?;
    let mcp_settings = normalized_settings_path(&request.mcp_settings, &request.pinned)?;
    // Refuse ambiguous aliases on case-insensitive filesystems, including new files.
    // A shared Gemini file must use exactly the same spelling for both arguments.
    if hook_settings != mcp_settings
        && hook_settings.to_string_lossy().to_lowercase()
            == mcp_settings.to_string_lossy().to_lowercase()
    {
        return Err(invalid(
            "settings paths must not differ only by letter case",
        ));
    }
    if hook_settings == mcp_settings && request.agent != ClientAgent::Gemini {
        return Err(invalid(
            "hook and MCP configuration require different settings files for this client",
        ));
    }
    let executable =
        fs::canonicalize(&request.executable).map_err(|_| invalid("executable does not exist"))?;
    if !fs::metadata(&executable)?.is_file() {
        return Err(invalid("executable is not a file"));
    }
    let mut request = request.clone();
    request.hook_settings = hook_settings;
    request.mcp_settings = mcp_settings;
    request.executable = executable;

    let mut files = Vec::new();
    if request.agent != ClientAgent::Codex && request.hook_settings == request.mcp_settings {
        let before = read(&request.hook_settings)?;
        let mut value = json_text(before.as_deref())?;
        add_hook(&mut value, &request)?;
        add_mcp(&mut value, &request)?;
        let after = serde_json::to_string_pretty(&value)? + "\n";
        files.push(preview(request.hook_settings.clone(), before, after));
    } else {
        let before = read(&request.hook_settings)?;
        let mut value = json_text(before.as_deref())?;
        add_hook(&mut value, &request)?;
        let after = serde_json::to_string_pretty(&value)? + "\n";
        files.push(preview(request.hook_settings.clone(), before, after));

        let before = read(&request.mcp_settings)?;
        let after = if request.agent == ClientAgent::Codex {
            codex_toml(before.as_deref(), &request)?
        } else {
            let mut value = json_text(before.as_deref())?;
            add_mcp(&mut value, &request)?;
            serde_json::to_string_pretty(&value)? + "\n"
        };
        files.push(preview(request.mcp_settings.clone(), before, after));
    }
    if request.apply {
        for file in &files {
            if read(&file.path)? != file.before {
                return Err(Error::Conflict(
                    "settings changed after preview; retry installation".into(),
                ));
            }
        }
        for file in &files {
            if file.action != "unchanged" {
                write_atomic(&file.path, &file.after)?;
            }
        }
    }
    Ok(InstallPreview {
        applied: request.apply && files.iter().any(|file| file.action != "unchanged"),
        files,
    })
}
