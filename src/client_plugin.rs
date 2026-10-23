//! Reproducible local native-client packages. Installation and hook trust remain host-managed.
use std::path::{Path, PathBuf};

use agentscm::{Error, Result};
use clap::Args;
use serde_json::{Value, json};

#[derive(Debug, Args)]
pub struct PluginArgs {
    /// Local marketplace directory (default: a stable directory in user configuration).
    #[arg(long)]
    output: Option<PathBuf>,
    /// Install through the supported Codex CLI. Hook trust still requires the user.
    #[arg(long)]
    install: bool,
}

const SKILL: &str = crate::client_setup::SKILL;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Engine {
    Codex,
    Claude,
}

impl Engine {
    fn marketplace(self) -> &'static str {
        match self {
            Self::Codex => "agentscm-client",
            Self::Claude => "agentscm-claude-client",
        }
    }

    fn output_dir(self) -> &'static str {
        match self {
            Self::Codex => "codex-marketplace",
            Self::Claude => "claude-marketplace",
        }
    }
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn metadata() -> Value {
    json!({
        "displayName": "AgentSCM", "shortDescription": "Coordinate agent edits",
        "longDescription": "Check native file edits against the checkout's configured coordination Hub. Hooks require explicit host trust and fail open when unavailable.",
        "developerName": "DUVATL, Inc.", "category": "Developer tools",
        "capabilities": ["Lifecycle hooks", "Instructions"],
        "logo": "./assets/icon.png", "composerIcon": "./assets/icon.png",
        "defaultPrompt": "Check AgentSCM coordination before editing shared files."
    })
}

fn codex_package_files(program: &str) -> Vec<(&'static str, Vec<u8>)> {
    let command = format!(
        "{} --no-telemetry hook codex --client-connection",
        shell_quote(program)
    );
    let hook = |message: &str, timeout| {
        json!({"type":"command", "command":command,
        "timeout":timeout, "statusMessage":message})
    };
    let hooks = json!({"hooks": {
        "SessionStart":[{"hooks":[hook("AgentSCM: connect this chat", 5)]}],
        "PreToolUse":[{"matcher":"^apply_patch$|^Edit$|^Write$", "hooks":[hook("AgentSCM: check overlapping edits", 5)]}],
        "SessionEnd":[{"hooks":[hook("AgentSCM: release file intents", 3)]}]
    }});
    let common = json!({"name":"agentscm", "version":env!("CARGO_PKG_VERSION"),
        "description":"Coordination hooks for configured AgentSCM checkouts.",
        "author":{"name":"DUVATL, Inc."},
        "homepage":"https://github.com/counterbranch/agentscm",
        "repository":"https://github.com/counterbranch/agentscm"});
    // This compatibility format is understood by installed Codex desktop versions.
    // A full portable manifest can shadow the compatibility hook declaration there.
    let portable = json!({"name":"agentscm"});
    let mut compat = common;
    compat["hooks"] = json!("./hooks/hooks.json");
    compat["skills"] = json!("./skills/");
    compat["interface"] = metadata();
    let json_file = |path, value| {
        (
            path,
            serde_json::to_vec_pretty(&value).expect("static JSON"),
        )
    };
    vec![
        json_file(
            ".claude-plugin/marketplace.json",
            json!({"name":"agentscm-client",
            "owner":{"name":"DUVATL, Inc."},
            "plugins":[{"name":"agentscm", "source":"./agentscm", "category":"development"}]}),
        ),
        json_file("agentscm/plugin.json", portable),
        json_file("agentscm/.codex-plugin/plugin.json", compat),
        json_file("agentscm/hooks/hooks.json", hooks),
        (
            "agentscm/skills/coordinate-edits/SKILL.md",
            SKILL.as_bytes().to_vec(),
        ),
        (
            "agentscm/assets/icon.png",
            include_bytes!("assets/agentscm-icon.png").to_vec(),
        ),
    ]
}

fn claude_files_with_version(program: &str, version: &str) -> Vec<(&'static str, Vec<u8>)> {
    let hook = |timeout| {
        json!({
            "type":"command", "command":program,
            "args":["--no-telemetry", "hook", "claude", "--client-connection"],
            "timeout":timeout
        })
    };
    let mut pre = json!({"hooks":[hook(5)]});
    pre["matcher"] = json!("^(Edit|Write|MultiEdit|NotebookEdit)$");
    let hooks = json!({"hooks": {
        "SessionStart":[{"hooks":[hook(5)]}],
        "PreToolUse":[pre],
        "SessionEnd":[{"hooks":[hook(3)]}]
    }});
    let manifest = json!({
        "name":"agentscm", "displayName":"AgentSCM", "version":version,
        "description":"Coordinate agent edits in checkouts connected to AgentSCM Hub.",
        "author":{"name":"DUVATL, Inc."},
        "homepage":"https://github.com/counterbranch/agentscm",
        "repository":"https://github.com/counterbranch/agentscm",
        "icon":"./assets/icon.png"
    });
    let json_file = |path, value| {
        (
            path,
            serde_json::to_vec_pretty(&value).expect("static JSON"),
        )
    };
    vec![
        json_file("agentscm/.claude-plugin/plugin.json", manifest),
        json_file("agentscm/hooks/hooks.json", hooks),
        (
            "agentscm/skills/coordinate-edits/SKILL.md",
            SKILL.as_bytes().to_vec(),
        ),
        (
            "agentscm/assets/icon.png",
            include_bytes!("assets/agentscm-icon.png").to_vec(),
        ),
    ]
}

fn claude_package_files(program: &str) -> Vec<(&'static str, Vec<u8>)> {
    let seed = claude_files_with_version(program, env!("CARGO_PKG_VERSION"));
    let digest = files_digest(&seed);
    let base = env!("CARGO_PKG_VERSION");
    let version = if base.contains('+') {
        format!("{base}.agentscm.{}", &digest[..16])
    } else {
        format!("{base}+agentscm.{}", &digest[..16])
    };
    claude_files_with_version(program, &version)
}

fn package_files(engine: Engine, program: &str) -> Vec<(&'static str, Vec<u8>)> {
    match engine {
        Engine::Codex => codex_package_files(program),
        Engine::Claude => claude_package_files(program),
    }
}

const OWNERSHIP_FILE: &str = ".agentscm-client";
const OWNERSHIP: &str = "AgentSCM local marketplace format 1\n";

fn program_text(program: &Path) -> Result<&str> {
    let text = program
        .to_str()
        .ok_or_else(|| invalid("executable path is not UTF-8"))?;
    if !program.is_absolute() || text.chars().any(char::is_control) {
        return Err(invalid(
            "executable path must be absolute without control characters",
        ));
    }
    Ok(text)
}

fn files_digest(files: &[(&str, Vec<u8>)]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for (name, bytes) in files {
        digest.update(name.as_bytes());
        digest.update([0]);
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn changed_package() -> Error {
    invalid("local plugin differs from the generated package; inspect it before retrying setup")
}

fn check_tree(output: &Path, files: &[(&str, Vec<u8>)]) -> Result<()> {
    let mut pending = vec![output.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = path.symlink_metadata()?;
        if metadata.file_type().is_symlink() {
            return Err(changed_package());
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                let path = entry?.path();
                let relative = path.strip_prefix(output).map_err(|_| changed_package())?;
                if !files
                    .iter()
                    .any(|(name, _)| Path::new(name).starts_with(relative))
                {
                    return Err(changed_package());
                }
                pending.push(path);
            }
        } else if !metadata.is_file() {
            return Err(changed_package());
        }
    }
    for (name, expected) in files {
        if std::fs::read(output.join(name))? != *expected {
            return Err(changed_package());
        }
    }
    Ok(())
}

fn stage_dir(parent: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let mut nonce = [0_u8; 12];
    getrandom::fill(&mut nonce).map_err(|_| invalid("random source unavailable"))?;
    let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = parent.join(format!(".agentscm-stage-{suffix}"));
    std::fs::DirBuilder::new().mode(0o700).create(&path)?;
    Ok(path)
}

fn publish_files(output: &Path, files: &[(&str, Vec<u8>)]) -> Result<()> {
    if output.try_exists()? {
        return check_tree(output, files);
    }
    let parent = output
        .parent()
        .ok_or_else(|| invalid("bad plugin directory"))?;
    std::fs::create_dir_all(parent)?;
    let stage = stage_dir(parent)?;
    let result = (|| {
        for (relative, bytes) in files {
            let path = stage.join(relative);
            std::fs::create_dir_all(path.parent().expect("package parent"))?;
            std::fs::write(path, bytes)?;
        }
        match std::fs::rename(&stage, output) {
            Ok(()) => Ok(()),
            Err(_) if output.try_exists()? => check_tree(output, files),
            Err(error) => Err(error.into()),
        }
    })();
    // Only this invocation's exclusive random staging directory is removed.
    let _ = std::fs::remove_dir_all(&stage);
    result
}

fn marketplace(engine: Engine, source: &str) -> Value {
    json!({"name":engine.marketplace(), "owner":{"name":"DUVATL, Inc."},
        "plugins":[{"name":"agentscm", "source":source, "category":"development"}]})
}

fn source_for(digest: &str) -> String {
    format!("./versions/{digest}/agentscm")
}

fn verify_marketplace_for(engine: Engine, output: &Path) -> Result<()> {
    for directory in [
        output.to_path_buf(),
        output.join(".claude-plugin"),
        output.join("versions"),
    ] {
        let metadata = directory.symlink_metadata()?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(changed_package());
        }
    }
    if output
        .join(OWNERSHIP_FILE)
        .symlink_metadata()?
        .file_type()
        .is_symlink()
        || std::fs::read(output.join(OWNERSHIP_FILE))? != OWNERSHIP.as_bytes()
    {
        return Err(changed_package());
    }
    let path = output.join(".claude-plugin/marketplace.json");
    if path.symlink_metadata()?.file_type().is_symlink() {
        return Err(changed_package());
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() > 16 * 1024 {
        return Err(changed_package());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| changed_package())?;
    let source = value["plugins"][0]["source"]
        .as_str()
        .ok_or_else(changed_package)?;
    let digest = source
        .strip_prefix("./versions/")
        .and_then(|value| value.strip_suffix("/agentscm"))
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(changed_package)?;
    if value != marketplace(engine, source) {
        return Err(changed_package());
    }
    let version_root = output.join("versions").join(digest);
    let mut files = Vec::new();
    for (name, _) in package_files(engine, "/placeholder") {
        let path = version_root.join(name);
        let metadata = path.symlink_metadata()?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            return Err(changed_package());
        }
        files.push((name, std::fs::read(path)?));
    }
    check_tree(&version_root, &files)?;
    if files_digest(&files) != digest {
        return Err(changed_package());
    }
    Ok(())
}

fn generate_for(engine: Engine, output: &Path, program: &Path) -> Result<()> {
    let files = package_files(engine, program_text(program)?);
    let digest = files_digest(&files);
    let parent = output
        .parent()
        .ok_or_else(|| invalid("bad plugin directory"))?;
    std::fs::create_dir_all(parent)?;
    let stage = stage_dir(parent)?;
    let result = (|| {
        publish_files(&stage.join("versions").join(&digest), &files)?;
        std::fs::create_dir(stage.join(".claude-plugin"))?;
        std::fs::write(
            stage.join(".claude-plugin/marketplace.json"),
            serde_json::to_vec_pretty(&marketplace(engine, &source_for(&digest)))?,
        )?;
        std::fs::write(stage.join(OWNERSHIP_FILE), OWNERSHIP)?;
        // The complete marketplace appears at its stable path only after every write succeeds.
        std::fs::rename(&stage, output)?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

fn default_output(engine: Engine) -> Result<PathBuf> {
    Ok(crate::hub_cli::config_dir()?
        .join("agentscm")
        .join(engine.output_dir()))
}

fn prepare_for(engine: Engine, output: &Path, program: &Path) -> Result<()> {
    program_text(program)?;
    if !output.try_exists()? {
        match generate_for(engine, output, program) {
            Ok(()) => return Ok(()),
            // Another setup may have published the complete stable marketplace first.
            // Validate it below before adding this invocation's version.
            Err(_) if output.try_exists()? => {}
            Err(error) => return Err(error),
        }
    }
    verify_marketplace_for(engine, output)?;
    let files = package_files(engine, program_text(program)?);
    let digest = files_digest(&files);
    publish_files(&output.join("versions").join(&digest), &files)?;
    // Only this small pointer changes. Existing complete versions and installed caches survive.
    crate::hub_cli::write_private(
        &output.join(".claude-plugin/marketplace.json"),
        &serde_json::to_string_pretty(&marketplace(engine, &source_for(&digest)))?,
    )
}

#[cfg(test)]
fn generate(output: &Path, program: &Path) -> Result<()> {
    generate_for(Engine::Codex, output, program)
}

fn prepare(output: &Path, program: &Path) -> Result<()> {
    prepare_for(Engine::Codex, output, program)
}

#[cfg(test)]
fn verify_marketplace(output: &Path) -> Result<()> {
    verify_marketplace_for(Engine::Codex, output)
}

fn check_installed_plugins() -> Result<()> {
    let listed = std::process::Command::new("codex")
        .args(["plugin", "list", "--json"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|_| invalid("could not run codex; install Codex CLI and rerun client setup"))?;
    if !listed.status.success() {
        return Err(invalid(
            "could not inspect installed Codex plugins; resolve the CLI error and rerun client setup",
        ));
    }
    let listed: Value = serde_json::from_slice(&listed.stdout)
        .map_err(|_| invalid("Codex returned an unsupported plugin list"))?;
    if another_agentscm_plugin(&listed)? {
        return Err(invalid(
            "another AgentSCM Codex plugin is enabled; remove the old AgentSCM plugin through Codex before rerunning setup to avoid duplicate hooks",
        ));
    }
    Ok(())
}

fn install(path: &Path) -> Result<()> {
    check_installed_plugins()?;
    for args in [
        vec![
            "plugin",
            "marketplace",
            "add",
            path.to_str()
                .ok_or_else(|| invalid("plugin path is not UTF-8"))?,
        ],
        vec!["plugin", "add", "agentscm@agentscm-client"],
    ] {
        let status = std::process::Command::new("codex")
            .args(args)
            .stdin(std::process::Stdio::null())
            .status()
            .map_err(|_| {
                invalid("could not run codex; install Codex CLI and rerun client setup")
            })?;
        if !status.success() {
            return Err(invalid(
                "Codex plugin installation failed; resolve the CLI error and rerun setup. Use client check to inspect any saved checkout binding",
            ));
        }
    }
    Ok(())
}

fn another_agentscm_plugin(listed: &Value) -> Result<bool> {
    let installed = listed["installed"]
        .as_array()
        .ok_or_else(|| invalid("Codex returned an unsupported plugin list"))?;
    Ok(installed.iter().any(|plugin| {
        plugin["name"] == "agentscm"
            && plugin["pluginId"] != "agentscm@agentscm-client"
            && plugin["enabled"] == true
    }))
}

fn has_agentscm_hooks(path: &Path) -> Result<bool> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err(invalid(
            "existing hook configuration is too large; inspect it before installing the plugin",
        ));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        invalid("existing hook configuration is invalid; inspect it before installing the plugin")
    })?;
    Ok(value["hooks"]
        .as_object()
        .into_iter()
        .flat_map(|events| events.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|group| group["hooks"].as_array())
        .flatten()
        .any(|hook| {
            hook["url"]
                .as_str()
                .is_some_and(|url| url.contains("/v1/hub/hook/"))
                || ["command", "command_windows"].iter().any(|key| {
                    hook[*key]
                        .as_str()
                        .is_some_and(|command| command.to_ascii_lowercase().contains("agentscm"))
                })
        }))
}

fn check_existing_hooks(root: &Path) -> Result<()> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .ok_or_else(|| invalid("HOME is not set"))?;
    for path in [
        root.join(".codex/hooks.json"),
        codex_home.join("hooks.json"),
    ] {
        if has_agentscm_hooks(&path)? {
            return Err(invalid(
                "existing AgentSCM Codex command hooks were found; review and remove only their old entries before rerunning client setup, or keep using them instead of the plugin",
            ));
        }
    }
    Ok(())
}

fn claude_config_dir(root: &Path) -> Result<PathBuf> {
    let path = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .ok_or_else(|| invalid("HOME and CLAUDE_CONFIG_DIR are not set"))?;
    Ok(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn check_existing_claude_hooks(root: &Path) -> Result<()> {
    let config = claude_config_dir(root)?;
    for path in [
        config.join("settings.json"),
        root.join(".claude/settings.json"),
        root.join(".claude/settings.local.json"),
    ] {
        if has_agentscm_hooks(&path)? {
            return Err(invalid(
                "existing AgentSCM Claude command hooks were found; review and remove only their old entries before installing the plugin to avoid duplicate hooks",
            ));
        }
    }
    Ok(())
}

fn claude_output(root: &Path, args: &[&str], failure: &str) -> Result<std::process::Output> {
    let output = std::process::Command::new("claude")
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|_| invalid("could not run claude; install Claude Code and rerun client setup"))?;
    if !output.status.success() {
        return Err(invalid(failure));
    }
    Ok(output)
}

fn claude_json(root: &Path, args: &[&str], failure: &str) -> Result<Value> {
    let output = claude_output(root, args, failure)?;
    serde_json::from_slice(&output.stdout)
        .map_err(|_| invalid("Claude Code returned unsupported plugin state"))
}

fn claude_plugins(root: &Path) -> Result<Vec<Value>> {
    claude_json(
        root,
        &["plugin", "list", "--json"],
        "could not inspect installed Claude Code plugins; resolve the CLI error and rerun client setup",
    )?
    .as_array()
    .cloned()
    .ok_or_else(|| invalid("Claude Code returned unsupported plugin state"))
}

fn is_agentscm_plugin(plugin: &Value) -> bool {
    plugin["id"]
        .as_str()
        .and_then(|id| id.split_once('@'))
        .is_some_and(|(name, _)| name == "agentscm")
}

fn claude_plugin(plugins: &[Value]) -> Result<Option<&Value>> {
    let mut ours = plugins.iter().filter(|plugin| {
        plugin["id"] == "agentscm@agentscm-claude-client" && plugin["scope"] == "user"
    });
    let installed = ours.next();
    if ours.next().is_some()
        || plugins.iter().any(|plugin| {
            plugin["enabled"] == true
                && is_agentscm_plugin(plugin)
                && !(plugin["id"] == "agentscm@agentscm-claude-client" && plugin["scope"] == "user")
        })
    {
        return Err(invalid(
            "another AgentSCM Claude Code plugin is enabled; remove the old plugin through Claude Code before rerunning setup to avoid duplicate hooks",
        ));
    }
    Ok(installed)
}

fn check_installed_claude_plugins(root: &Path) -> Result<()> {
    let plugins = claude_plugins(root)?;
    claude_plugin(&plugins).map(|_| ())
}

fn claude_marketplaces(root: &Path) -> Result<Vec<Value>> {
    claude_json(
        root,
        &["plugin", "marketplace", "list", "--json"],
        "could not inspect Claude Code marketplaces; resolve the CLI error and rerun client setup",
    )?
    .as_array()
    .cloned()
    .ok_or_else(|| invalid("Claude Code returned unsupported marketplace state"))
}

fn claude_mutation_result(stdout: &[u8]) -> Result<Value> {
    let line = stdout
        .split(|byte| *byte == b'\n')
        .rev()
        .find(|line| !line.iter().all(u8::is_ascii_whitespace))
        .ok_or_else(|| invalid("Claude Code returned unsupported plugin state"))?;
    serde_json::from_slice(line)
        .map_err(|_| invalid("Claude Code returned unsupported plugin state"))
}

fn run_claude(root: &Path, args: &[&str], failure: &str) -> Result<()> {
    let output = claude_output(root, args, failure)?;
    let value = claude_mutation_result(&output.stdout)?;
    if value["outcome"] != "ok" {
        return Err(invalid(failure));
    }
    Ok(())
}

fn register_claude_marketplace(root: &Path, path: &Path) -> Result<()> {
    let path = path
        .to_str()
        .ok_or_else(|| invalid("plugin path is not UTF-8"))?;
    let marketplaces = claude_marketplaces(root)?;
    match marketplaces
        .iter()
        .find(|marketplace| marketplace["name"] == "agentscm-claude-client")
    {
        None => run_claude(
            root,
            &[
                "plugin",
                "marketplace",
                "add",
                "--scope",
                "user",
                "--json",
                path,
            ],
            "Claude Code marketplace installation failed; resolve the CLI error and rerun setup",
        )?,
        Some(marketplace)
            if marketplace["path"].as_str() == Some(path)
                || marketplace["installLocation"].as_str() == Some(path) => {}
        Some(_) => {
            return Err(invalid(
                "the AgentSCM Claude marketplace name already points to another location; remove it through Claude Code before rerunning setup",
            ));
        }
    }
    run_claude(
        root,
        &[
            "plugin",
            "marketplace",
            "update",
            "agentscm-claude-client",
            "--json",
        ],
        "Claude Code marketplace update failed; resolve the CLI error and rerun setup",
    )
}

fn claude_version(files: &[(&str, Vec<u8>)]) -> Result<String> {
    let manifest = files
        .iter()
        .find(|(name, _)| *name == "agentscm/.claude-plugin/plugin.json")
        .ok_or_else(changed_package)?;
    let value: Value = serde_json::from_slice(&manifest.1).map_err(|_| changed_package())?;
    value["version"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(changed_package)
}

fn claude_cache_files(files: &[(&'static str, Vec<u8>)]) -> Result<Vec<(&'static str, Vec<u8>)>> {
    files
        .iter()
        .map(|(name, bytes)| {
            (*name)
                .strip_prefix("agentscm/")
                .map(|name| (name, bytes.clone()))
                .ok_or_else(changed_package)
        })
        .collect()
}

fn verify_claude_install(plugin: &Value, files: &[(&'static str, Vec<u8>)]) -> Result<()> {
    let expected_version = claude_version(files)?;
    if plugin["id"] != "agentscm@agentscm-claude-client"
        || plugin["scope"] != "user"
        || plugin["enabled"] != true
        || plugin["version"].as_str() != Some(expected_version.as_str())
    {
        return Err(invalid(
            "Claude Code did not activate the expected AgentSCM plugin version; retry setup",
        ));
    }
    let install_path = plugin["installPath"]
        .as_str()
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| invalid("Claude Code returned an unsupported plugin cache path"))?;
    let cache_files = claude_cache_files(files)?;
    check_tree(install_path, &cache_files).map_err(|_| {
        invalid(
            "Claude Code plugin cache does not match the generated AgentSCM package; retry setup",
        )
    })
}

fn install_claude(root: &Path, path: &Path, files: &[(&'static str, Vec<u8>)]) -> Result<()> {
    register_claude_marketplace(root, path)?;
    let plugins = claude_plugins(root)?;
    let installed = claude_plugin(&plugins)?.is_some();
    if installed {
        run_claude(
            root,
            &[
                "plugin",
                "update",
                "--scope",
                "user",
                "--json",
                "agentscm@agentscm-claude-client",
            ],
            "Claude Code plugin update failed; resolve the CLI error and rerun setup",
        )?;
    } else {
        run_claude(
            root,
            &[
                "plugin",
                "install",
                "--scope",
                "user",
                "--json",
                "agentscm@agentscm-claude-client",
            ],
            "Claude Code plugin installation failed; resolve the CLI error and rerun setup",
        )?;
    }
    let plugins = claude_plugins(root)?;
    let plugin = claude_plugin(&plugins)?.ok_or_else(|| {
        invalid("Claude Code did not report the installed AgentSCM plugin; retry setup")
    })?;
    if plugin["enabled"] != true {
        run_claude(
            root,
            &[
                "plugin",
                "enable",
                "--scope",
                "user",
                "--json",
                "agentscm@agentscm-claude-client",
            ],
            "Claude Code could not enable the AgentSCM plugin; resolve the CLI error and rerun setup",
        )?;
    }
    let plugins = claude_plugins(root)?;
    let plugin = claude_plugin(&plugins)?.ok_or_else(|| {
        invalid("Claude Code did not report the installed AgentSCM plugin; retry setup")
    })?;
    verify_claude_install(plugin, files)
}

pub(crate) fn install_for_root(root: &Path, program: &Path) -> Result<()> {
    run_for_root(
        PluginArgs {
            output: None,
            install: true,
        },
        root,
        program,
    )
}

pub(crate) fn preflight_for_root(root: &Path) -> Result<()> {
    check_existing_hooks(root)?;
    check_installed_plugins()
}

pub(crate) fn preflight_claude_for_root(root: &Path) -> Result<()> {
    check_existing_claude_hooks(root)?;
    check_installed_claude_plugins(root)
}

pub(crate) fn install_claude_for_root(root: &Path, program: &Path) -> Result<()> {
    // Preflight precedes package generation so a duplicate hook cannot mint a token first.
    preflight_claude_for_root(root)?;
    let output = default_output(Engine::Claude)?;
    prepare_for(Engine::Claude, &output, program)?;
    let path = std::fs::canonicalize(&output)?;
    let files = package_files(Engine::Claude, program_text(program)?);
    install_claude(root, &path, &files)
}

/// Runs a plugin CLI for removal from a directory that always exists; `None` when the CLI is
/// gone, since it then has no plugin left to remove.
fn removal_output(program: &str, args: &[&str]) -> Result<Option<std::process::Output>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| invalid("HOME is not set"))?;
    match std::process::Command::new(program)
        .args(args)
        .current_dir(home)
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(output) => Ok(Some(output)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn removed(program: &str, args: &[&str], failure: &str) -> Result<()> {
    match removal_output(program, args)? {
        Some(output) if !output.status.success() => Err(invalid(failure)),
        _ => Ok(()),
    }
}

/// Takes out the user-scope plugin, its marketplace and the generated package once no
/// connected checkout uses them.
pub(crate) fn remove(claude: bool) -> Result<()> {
    let engine = if claude {
        Engine::Claude
    } else {
        Engine::Codex
    };
    let (program, plugin) = if claude {
        ("claude", "agentscm@agentscm-claude-client")
    } else {
        ("codex", "agentscm@agentscm-client")
    };
    let Some(listed) = removal_output(program, &["plugin", "list", "--json"])? else {
        println!("{program} is not installed, so there is no AgentSCM plugin to remove from it.");
        return Ok(());
    };
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap_or(Value::Null);
    let plugins = if claude {
        &listed
    } else {
        &listed["installed"]
    };
    let installed = plugins
        .as_array()
        .into_iter()
        .flatten()
        .any(|entry| entry["id"] == plugin || entry["pluginId"] == plugin);
    if installed {
        let args: &[&str] = if claude {
            &["plugin", "uninstall", "--scope", "user", plugin]
        } else {
            &["plugin", "remove", plugin]
        };
        removed(
            program,
            args,
            &format!("{program} could not uninstall {plugin}; remove it through {program}"),
        )?;
        println!("Uninstalled the {plugin} plugin.");
    }
    let marketplaces = if claude {
        removal_output(program, &["plugin", "marketplace", "list", "--json"])?
    } else {
        None
    };
    let registered = !claude
        || marketplaces
            .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
            .and_then(|value| value.as_array().cloned())
            .is_some_and(|list| {
                list.iter()
                    .any(|entry| entry["name"] == engine.marketplace())
            });
    if registered {
        // Codex has no machine-readable marketplace list; its remove fails only when absent.
        let result = removed(
            program,
            &["plugin", "marketplace", "remove", engine.marketplace()],
            &format!(
                "{program} could not remove the {} marketplace; remove it through {program}",
                engine.marketplace()
            ),
        );
        if claude {
            result?;
        }
    }
    let output = default_output(engine)?;
    match std::fs::remove_dir_all(&output) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    Ok(())
}

pub fn run(args: PluginArgs) -> Result<()> {
    run_for_root(args, &std::env::current_dir()?, &std::env::current_exe()?)
}

fn run_for_root(args: PluginArgs, root: &Path, program: &Path) -> Result<()> {
    if args.install {
        check_existing_hooks(root)?;
    }
    let output = match args.output {
        Some(output) => output,
        None => default_output(Engine::Codex)?,
    };
    prepare(&output, program)?;
    let path = std::fs::canonicalize(&output)?;
    println!("Prepared local Codex marketplace at {}", path.display());
    if args.install {
        install(&path)?;
    }
    println!(
        "codex plugin marketplace add {}",
        shell_quote(&path.to_string_lossy())
    );
    println!("codex plugin add agentscm@agentscm-client");
    println!("In Codex Settings > Hooks, select Reload hooks, then review and trust AgentSCM.");
    println!(
        "Use either this plugin or repository Codex hooks; remove duplicate AgentSCM hooks explicitly."
    );
    println!(
        "Check each checkout with agentscm client check. Installation is not runtime verification."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version_root(output: &Path) -> PathBuf {
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(output.join(".claude-plugin/marketplace.json")).unwrap(),
        )
        .unwrap();
        output
            .join(manifest["plugins"][0]["source"].as_str().unwrap())
            .parent()
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn package_is_branded_credential_free_and_uses_only_the_installed_binary() {
        let dir = tempfile::TempDir::new().unwrap();
        let output = dir.path().join("marketplace");
        generate(&output, Path::new("/opt/team's bin/agentscm")).unwrap();
        let version = version_root(&output);
        let read =
            |p| serde_json::from_slice::<Value>(&std::fs::read(version.join(p)).unwrap()).unwrap();
        let portable = read("agentscm/plugin.json");
        let compat = read("agentscm/.codex-plugin/plugin.json");
        assert_eq!(portable, json!({"name":"agentscm"}));
        assert_eq!(compat["interface"]["displayName"], "AgentSCM");
        let hooks = read("agentscm/hooks/hooks.json");
        assert_eq!(hooks["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3);
        for groups in hooks["hooks"].as_object().unwrap().values() {
            for group in groups.as_array().unwrap() {
                let command = group["hooks"][0]["command"].as_str().unwrap();
                assert_eq!(
                    command,
                    "'/opt/team'\\''s bin/agentscm' --no-telemetry hook codex --client-connection"
                );
                assert!(!command.contains("token"));
                assert!(!command.contains("hub-url"));
            }
        }
        assert!(generate(&output, Path::new("/opt/agentscm")).is_err());
        assert_eq!(read("agentscm/plugin.json"), portable);
    }

    #[test]
    fn claude_package_uses_native_defaults_and_content_versioned_exec_hooks() {
        let old = package_files(Engine::Claude, "/opt/old agentscm");
        let repeated = package_files(Engine::Claude, "/opt/old agentscm");
        let new = package_files(Engine::Claude, "/opt/new agentscm");
        assert_eq!(old, repeated);
        let read = |files: &[(&str, Vec<u8>)], name| {
            serde_json::from_slice::<Value>(
                &files.iter().find(|(path, _)| *path == name).unwrap().1,
            )
            .unwrap()
        };
        let old_manifest = read(&old, "agentscm/.claude-plugin/plugin.json");
        let new_manifest = read(&new, "agentscm/.claude-plugin/plugin.json");
        assert_eq!(old_manifest["name"], "agentscm");
        assert_eq!(old_manifest["displayName"], "AgentSCM");
        assert!(old_manifest.get("hooks").is_none());
        assert!(old_manifest.get("skills").is_none());
        assert_ne!(old_manifest["version"], new_manifest["version"]);
        assert!(
            old_manifest["version"]
                .as_str()
                .unwrap()
                .contains("+agentscm.")
        );
        let hooks = read(&old, "agentscm/hooks/hooks.json");
        for groups in hooks["hooks"].as_object().unwrap().values() {
            for group in groups.as_array().unwrap() {
                let hook = &group["hooks"][0];
                assert_eq!(hook["command"], "/opt/old agentscm");
                assert_eq!(
                    hook["args"],
                    json!(["--no-telemetry", "hook", "claude", "--client-connection"])
                );
                assert!(!hook.to_string().contains("token"));
                assert!(!hook.to_string().contains("hub-url"));
            }
        }
        assert_eq!(
            old.iter()
                .find(|(path, _)| *path == "agentscm/skills/coordinate-edits/SKILL.md")
                .unwrap()
                .1,
            SKILL.as_bytes()
        );
    }

    #[test]
    fn claude_plugin_preflight_allows_only_our_single_user_install() {
        let ours = json!({"id":"agentscm@agentscm-claude-client", "scope":"user",
            "enabled":true, "version":"1.0.0", "installPath":"/tmp/agentscm"});
        assert_eq!(claude_plugin(&[]).unwrap(), None);
        assert_eq!(
            claude_plugin(std::slice::from_ref(&ours)).unwrap(),
            Some(&ours)
        );
        for other in [
            json!({"id":"agentscm@old", "scope":"user", "enabled":true}),
            json!({"id":"agentscm@agentscm-claude-client", "scope":"project", "enabled":true}),
        ] {
            assert!(claude_plugin(&[ours.clone(), other]).is_err());
        }
        let disabled = json!({"id":"agentscm@old", "scope":"user", "enabled":false});
        assert_eq!(
            claude_plugin(&[ours.clone(), disabled]).unwrap(),
            Some(&ours)
        );
    }

    #[test]
    fn claude_mutations_use_the_documented_last_json_line() {
        let output = b"Validating local marketplace\n{\"command\":\"marketplace-update\",\"outcome\":\"ok\"}\n";
        assert_eq!(claude_mutation_result(output).unwrap()["outcome"], "ok");
        assert!(claude_mutation_result(b"progress only\n").is_err());
    }

    #[test]
    fn claude_cache_must_match_the_expected_version_and_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = dir.path().join("cache");
        let files = package_files(Engine::Claude, "/opt/agentscm");
        let cache_files = claude_cache_files(&files).unwrap();
        publish_files(&cache, &cache_files).unwrap();
        let version = claude_version(&files).unwrap();
        let plugin = json!({"id":"agentscm@agentscm-claude-client", "scope":"user",
            "enabled":true, "version":version, "installPath":cache});
        verify_claude_install(&plugin, &files).unwrap();
        assert!(cache.join(".claude-plugin/plugin.json").is_file());
        assert!(!cache.join("agentscm").exists());
        std::fs::write(cache.join("hooks/hooks.json"), b"{}").unwrap();
        assert!(verify_claude_install(&plugin, &files).is_err());
    }

    #[test]
    fn unsafe_binary_path_never_creates_a_package() {
        let dir = tempfile::TempDir::new().unwrap();
        for program in ["relative/agentscm", "/opt/bad\nprogram"] {
            let output = dir.path().join("marketplace");
            assert!(generate(&output, Path::new(program)).is_err());
            assert!(!output.exists());
        }
    }

    #[test]
    fn repeat_setup_accepts_only_an_unchanged_complete_package() {
        let dir = tempfile::TempDir::new().unwrap();
        let output = dir.path().join("marketplace");
        let program = Path::new("/opt/agentscm");
        prepare(&output, program).unwrap();
        prepare(&output, program).unwrap();
        let version = version_root(&output);
        let extra = version.join("agentscm/skills/unexpected");
        std::fs::create_dir(&extra).unwrap();
        assert!(prepare(&output, program).is_err());
        std::fs::remove_dir(extra).unwrap();
        std::fs::write(version.join("agentscm/hooks/hooks.json"), "{}").unwrap();
        assert!(prepare(&output, program).is_err());
    }

    #[test]
    fn existing_package_symlinks_are_not_reused() {
        let dir = tempfile::TempDir::new().unwrap();
        let output = dir.path().join("marketplace");
        let link = dir.path().join("link");
        let program = Path::new("/opt/agentscm");
        prepare(&output, program).unwrap();
        std::os::unix::fs::symlink(&output, &link).unwrap();
        assert!(prepare(&link, program).is_err());
    }

    #[test]
    fn legacy_hook_guard_preserves_existing_files_and_detects_our_commands() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("hooks.json");
        assert!(!has_agentscm_hooks(&path).unwrap());
        let unrelated = br#"{"hooks":{"PreToolUse":[{"hooks":[{"command":"other-check"}]}]}}"#;
        std::fs::write(&path, unrelated).unwrap();
        assert!(!has_agentscm_hooks(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), unrelated);
        let legacy = br#"{"hooks":{"PreToolUse":[{"hooks":[{"command":"agentscm hook codex --hub-url https://hub.example.test"}]}]}}"#;
        std::fs::write(&path, legacy).unwrap();
        assert!(has_agentscm_hooks(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), legacy);
        let claude = br#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"/opt/agentscm","args":["--no-telemetry","hook","claude","--client-connection"]}]}]}}"#;
        std::fs::write(&path, claude).unwrap();
        assert!(has_agentscm_hooks(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), claude);
        let http = br#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"http","url":"https://hub.example.test/v1/hub/hook/claude","headers":{"authorization":"Bearer ${AGENTSCM_HOOK_TOKEN}"}}]}]}}"#;
        std::fs::write(&path, http).unwrap();
        assert!(has_agentscm_hooks(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), http);
    }

    #[test]
    fn other_enabled_agentscm_plugin_requires_explicit_migration() {
        let list = |id, enabled| json!({"installed":[{"name":"agentscm", "pluginId":id, "enabled":enabled}]});
        assert!(!another_agentscm_plugin(&list("agentscm@agentscm-client", true)).unwrap());
        assert!(another_agentscm_plugin(&list("agentscm@old-source", true)).unwrap());
        assert!(!another_agentscm_plugin(&list("agentscm@old-source", false)).unwrap());
        assert!(another_agentscm_plugin(&json!({})).is_err());
    }

    #[test]
    fn upgrades_keep_marketplace_source_stable_and_publish_complete_versions() {
        let dir = tempfile::TempDir::new().unwrap();
        let output = dir.path().join("marketplace");
        prepare(&output, Path::new("/opt/old/agentscm")).unwrap();
        let old = version_root(&output);
        let old_hook = std::fs::read(old.join("agentscm/hooks/hooks.json")).unwrap();
        prepare(&output, Path::new("/opt/new/agentscm")).unwrap();
        let new = version_root(&output);
        assert_ne!(old, new);
        assert_eq!(
            std::fs::read(old.join("agentscm/hooks/hooks.json")).unwrap(),
            old_hook
        );
        assert!(
            String::from_utf8(std::fs::read(new.join("agentscm/hooks/hooks.json")).unwrap())
                .unwrap()
                .contains("/opt/new/agentscm")
        );
        verify_marketplace(&output).unwrap();
        // An interrupted, unpublished staging directory is never selected and does not block retry.
        let interrupted = output.join("versions/.agentscm-stage-interrupted");
        std::fs::create_dir(&interrupted).unwrap();
        std::fs::write(interrupted.join("partial"), b"incomplete").unwrap();
        prepare(&output, Path::new("/opt/new/agentscm")).unwrap();
        assert_eq!(version_root(&output), new);
    }

    #[test]
    fn failed_package_writes_publish_nothing_and_leave_retry_possible() {
        let dir = tempfile::TempDir::new().unwrap();
        let output = dir.path().join("package");
        let bad = vec![
            ("file", b"first write".to_vec()),
            ("file/child", b"fails".to_vec()),
        ];
        assert!(publish_files(&output, &bad).is_err());
        assert!(!output.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        publish_files(&output, &[("file", b"complete".to_vec())]).unwrap();
    }

    #[test]
    fn parallel_first_setups_keep_a_complete_marketplace() {
        let dir = tempfile::TempDir::new().unwrap();
        let output = dir.path().join("marketplace");
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            let mut jobs = Vec::new();
            for index in 0..4 {
                let output = &output;
                let barrier = &barrier;
                jobs.push(scope.spawn(move || {
                    barrier.wait();
                    prepare(
                        output,
                        &PathBuf::from(format!("/opt/version-{index}/agentscm")),
                    )
                }));
            }
            for job in jobs {
                job.join().unwrap().unwrap();
            }
        });
        verify_marketplace(&output).unwrap();
        assert_eq!(
            std::fs::read_dir(output.join("versions")).unwrap().count(),
            4
        );
    }
}
