use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use agentscm::{Error, Result};
use sha2::{Digest, Sha256};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
use winit::window::WindowId;

use crate::client_activity::{ActivityOutcome, ActivityPath};
use crate::client_status::{BindingState, CredentialState, HubCheck, Snapshot};

const READY: &str = "AGENTSCM_TRAY_READY";
const ALREADY_RUNNING: &str = "AGENTSCM_TRAY_ALREADY_RUNNING";

#[derive(Debug)]
enum UserEvent {
    Menu(String),
    Snapshot(std::result::Result<Snapshot, String>, ConfigStamp),
    Integration(std::result::Result<String, String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConfigStamp(Vec<(std::path::PathBuf, Option<FileSignature>)>);

type FileSignature = (u64, u64, i64, i64, u64);

pub(super) async fn run(foreground: bool) -> Result<()> {
    if foreground {
        let initial = Snapshot {
            version: 1,
            snapshot_at: 0,
            connections: Vec::new(),
        };
        run_foreground(initial)
    } else {
        launch()
    }
}

fn launch() -> Result<()> {
    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable)
        .args(super::foreground_argv())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Invalid("cannot observe tray startup".into()))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout.take(128))
            .read_line(&mut line)
            .map(|_| line);
        let _ = sender.send(result);
    });
    match receiver.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok(line)) if matches!(line.trim(), READY | ALREADY_RUNNING) => Ok(()),
        failure => {
            let _ = child.kill();
            let _ = child.wait();
            match failure {
                Ok(Ok(_)) => Err(Error::Invalid(
                    "tray startup returned an invalid response".into(),
                )),
                Ok(Err(error)) => Err(error.into()),
                Err(_) => Err(Error::Invalid(
                    "timed out starting the AgentSCM tray".into(),
                )),
            }
        }
    }
}

fn run_foreground(initial: Snapshot) -> Result<()> {
    let _instance = match InstanceLock::acquire()? {
        Some(lock) => lock,
        None => {
            println!("{ALREADY_RUNNING}");
            std::io::stdout().flush()?;
            return Ok(());
        }
    };
    let activity_receiver = crate::client_activity::ActivityReceiver::bind()?;
    let activity = crate::client_activity::ActivityStore::default();
    let mut builder = EventLoop::<UserEvent>::with_user_event();
    builder
        .with_activation_policy(ActivationPolicy::Accessory)
        .with_default_menu(false)
        .with_activate_ignoring_other_apps(false);
    let event_loop = builder
        .build()
        .map_err(|error| Error::Invalid(format!("cannot create tray event loop: {error}")))?;
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(event.id.0));
    }));

    let menu = build_menu(&initial, &activity, Some("Loading connected checkouts…"))?;
    let refreshing = Arc::new(AtomicBool::new(false));
    let installing = Arc::new(AtomicBool::new(false));
    let refresh_proxy = event_loop.create_proxy();
    refreshing.store(true, Ordering::Release);
    spawn_refresh(refresh_proxy.clone(), true);
    let now = std::time::Instant::now();
    let mut app = TrayApp {
        activity_receiver,
        activity,
        executable: std::env::current_exe()?,
        snapshot: initial,
        refreshing,
        installing,
        refresh_proxy,
        last_tick: now,
        last_wall: std::time::SystemTime::now(),
        last_menu: now,
        observed_config: config_stamp(),
        pending_config: None,
        initial_menu: Some(menu),
        initial_icon: Some(icon()?),
        tray: None,
        notice: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|error| Error::Invalid(format!("tray event loop failed: {error}")))
}

struct TrayApp {
    activity_receiver: crate::client_activity::ActivityReceiver,
    activity: crate::client_activity::ActivityStore,
    executable: std::path::PathBuf,
    snapshot: Snapshot,
    refreshing: Arc<AtomicBool>,
    installing: Arc<AtomicBool>,
    refresh_proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    last_tick: std::time::Instant,
    last_wall: std::time::SystemTime,
    last_menu: std::time::Instant,
    observed_config: ConfigStamp,
    pending_config: Option<(ConfigStamp, std::time::Instant)>,
    initial_menu: Option<Menu>,
    initial_icon: Option<Icon>,
    tray: Option<tray_icon::TrayIcon>,
    notice: Option<String>,
}

impl TrayApp {
    fn replace_menu(&self, notice: Option<&str>) {
        let notice = projected_notice(
            notice,
            self.notice.as_deref(),
            self.refreshing.load(Ordering::Acquire) || self.pending_config.is_some(),
            self.snapshot.connections.is_empty(),
        );
        if let (Some(tray), Ok(menu)) = (
            &self.tray,
            build_menu(&self.snapshot, &self.activity, notice),
        ) {
            tray.set_menu(Some(Box::new(menu)));
        }
    }
}

impl ApplicationHandler<UserEvent> for TrayApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }
        let built = TrayIconBuilder::new()
            .with_tooltip("AgentSCM")
            .with_icon_templated(
                self.initial_icon
                    .take()
                    .expect("tray icon is initialized once"),
            )
            .with_autosave_name("com.counterbranch.agentscm.tray")
            .with_menu(Box::new(
                self.initial_menu
                    .take()
                    .expect("tray menu is initialized once"),
            ))
            .build();
        match built {
            Ok(icon) => {
                self.tray = Some(icon);
                println!("{READY}");
                let _ = std::io::stdout().flush();
            }
            Err(error) => {
                println!("AGENTSCM_TRAY_ERROR:{error}");
                let _ = std::io::stdout().flush();
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Menu(id) if id == "refresh" => {
                self.notice = None;
                if !self.refreshing.swap(true, Ordering::AcqRel) {
                    spawn_refresh(self.refresh_proxy.clone(), true);
                }
                self.replace_menu(None);
            }
            UserEvent::Menu(id) if id == "quit" => event_loop.exit(),
            UserEvent::Menu(id) if id == "launch-login" => {
                self.notice = None;
                let desired = !super::login::enabled();
                let notice =
                    if let Err(error) = super::login::set_enabled(desired, &self.executable) {
                        error.to_string()
                    } else {
                        if desired {
                            "Launch at login enabled for the next login".into()
                        } else {
                            "Launch at login disabled".into()
                        }
                    };
                self.notice = Some(notice);
                self.replace_menu(None);
            }
            UserEvent::Menu(id) if id.starts_with("setup:") => {
                self.notice = None;
                if self.installing.swap(true, Ordering::AcqRel) {
                    self.notice = Some("Another agent integration setup is still running".into());
                    self.replace_menu(None);
                } else if let Some(request) =
                    integration_request(&id, &self.snapshot, &self.executable)
                {
                    spawn_integration(self.refresh_proxy.clone(), request);
                    self.notice = Some("Setting up agent integration…".into());
                    self.replace_menu(None);
                } else {
                    self.installing.store(false, Ordering::Release);
                    self.notice =
                        Some("The checkout connection changed; refresh and try again".into());
                    self.replace_menu(None);
                }
            }
            UserEvent::Menu(id) => {
                self.notice = None;
                handle_action(&id, &self.snapshot, &self.executable);
                self.replace_menu(None);
            }
            UserEvent::Snapshot(Ok(updated), dispatched) => {
                self.refreshing.store(false, Ordering::Release);
                if dispatched == config_stamp() {
                    self.snapshot = updated;
                }
                self.replace_menu(None);
            }
            UserEvent::Snapshot(Err(error), dispatched) => {
                self.refreshing.store(false, Ordering::Release);
                let notice = if dispatched == config_stamp() {
                    format!("Check failed: {error}")
                } else {
                    "Configuration changed; checking again…".into()
                };
                self.replace_menu(Some(&notice));
            }
            UserEvent::Integration(result) => {
                self.installing.store(false, Ordering::Release);
                self.notice = Some(match result {
                    Ok(agent) => format!("{agent} integration configured"),
                    Err(error) => format!("Integration setup failed: {error}"),
                });
                self.replace_menu(None);
                if !self.refreshing.swap(true, Ordering::AcqRel) {
                    spawn_refresh(self.refresh_proxy.clone(), false);
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = std::time::Instant::now();
        let wall = std::time::SystemTime::now();
        if clock_discontinuity(
            self.last_wall,
            wall,
            now.saturating_duration_since(self.last_tick),
        ) {
            self.activity.clear();
            if !self.refreshing.swap(true, Ordering::AcqRel) {
                spawn_refresh(self.refresh_proxy.clone(), true);
            }
        }
        self.last_tick = now;
        self.last_wall = wall;
        let current_config = config_stamp();
        if current_config != self.observed_config {
            if self.pending_config.is_none() {
                self.snapshot.connections.clear();
                self.replace_menu(Some("Configuration changed; checking…"));
            }
            match &self.pending_config {
                Some((pending, since))
                    if pending == &current_config
                        && now.saturating_duration_since(*since) >= Duration::from_secs(2) =>
                {
                    if !self.refreshing.swap(true, Ordering::AcqRel) {
                        self.observed_config = current_config;
                        self.pending_config = None;
                        spawn_refresh(self.refresh_proxy.clone(), true);
                    }
                }
                Some((pending, _)) if pending == &current_config => {}
                _ => self.pending_config = Some((current_config, now)),
            }
        } else {
            self.pending_config = None;
        }
        self.activity_receiver.drain(&mut self.activity);
        let _ = self.activity.len();
        if now.saturating_duration_since(self.last_menu) >= Duration::from_secs(5) {
            self.last_menu = now;
            self.replace_menu(None);
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_secs(2)));
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

fn spawn_refresh(proxy: winit::event_loop::EventLoopProxy<UserEvent>, check: bool) {
    std::thread::spawn(move || {
        let stamp = config_stamp();
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
            .and_then(|runtime| {
                runtime
                    .block_on(crate::client_status::snapshot(check))
                    .map_err(|error| error.to_string())
            });
        let _ = proxy.send_event(UserEvent::Snapshot(result, stamp));
    });
}

struct IntegrationRequest {
    agent: crate::client_setup::Agent,
    label: &'static str,
    root: std::path::PathBuf,
    hub: String,
    repo: String,
    generation: String,
    program: std::path::PathBuf,
}

fn integration_request(
    id: &str,
    snapshot: &Snapshot,
    program: &Path,
) -> Option<IntegrationRequest> {
    let mut parts = id.split(':');
    (parts.next()? == "setup").then_some(())?;
    let identity = parts.next()?;
    let agent_name = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let connection = snapshot
        .connections
        .iter()
        .find(|connection| connection_identity(connection) == identity)?;
    let (agent, label) = agent_choices()
        .into_iter()
        .find(|(agent, _)| agent.name() == agent_name)?;
    Some(IntegrationRequest {
        agent,
        label,
        root: connection.root.clone(),
        hub: connection.hub.clone(),
        repo: connection.repository.clone(),
        generation: connection.binding.generation.clone()?,
        program: program.to_owned(),
    })
}

fn spawn_integration(
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    request: IntegrationRequest,
) {
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::client_cli::setup_existing_agent(
                request.agent,
                &request.root,
                &request.hub,
                &request.repo,
                &request.generation,
                &request.program,
            )
        }))
        .map_err(|_| "integration setup stopped unexpectedly".to_owned())
        .and_then(|result| result.map_err(|error| bounded_error(&error.to_string())))
        .map(|()| request.label.to_owned());
        let _ = proxy.send_event(UserEvent::Integration(result));
    });
}

fn bounded_error(error: &str) -> String {
    error
        .strip_prefix("invalid: ")
        .unwrap_or(error)
        .chars()
        .filter(|character| !character.is_control())
        .take(180)
        .collect()
}

fn agent_choices() -> [(crate::client_setup::Agent, &'static str); 5] {
    use crate::client_setup::Agent;
    [
        (Agent::Claude, "Claude Code"),
        (Agent::Codex, "Codex"),
        (Agent::Cursor, "Cursor"),
        (Agent::Copilot, "Copilot"),
        (Agent::Gemini, "Gemini"),
    ]
}

fn build_menu(
    snapshot: &Snapshot,
    activity: &crate::client_activity::ActivityStore,
    notice: Option<&str>,
) -> Result<Menu> {
    let now = agentscm::hub::store::unix_now();
    let menu = Menu::new();
    menu.append(&PredefinedMenuItem::section_header("AgentSCM"))
        .map_err(menu_error)?;
    if snapshot.connections.is_empty() && notice.is_none() {
        menu.append(&MenuItem::new(
            "No connected checkouts — run agentscm quickstart in a checkout",
            false,
            None,
        ))
        .map_err(menu_error)?;
    }
    for connection in &snapshot.connections {
        let identity = connection_identity(connection);
        let name = connection
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&connection.repository);
        let submenu = Submenu::new(super::clean_label(name), true);
        submenu
            .append(&MenuItem::new(
                format!("Repository: {}", super::clean_label(&connection.repository)),
                false,
                None,
            ))
            .map_err(menu_error)?;
        submenu
            .append(&MenuItem::new(
                format!(
                    "Path: {}",
                    exact_menu_label(&connection.root.to_string_lossy())
                ),
                false,
                None,
            ))
            .map_err(menu_error)?;
        submenu
            .append(&MenuItem::new(
                format!("URL: {}", super::clean_label(&connection.hub)),
                false,
                None,
            ))
            .map_err(menu_error)?;
        submenu
            .append(&MenuItem::new(
                format!("Binding: {}", binding_label(&connection.binding.state)),
                false,
                None,
            ))
            .map_err(menu_error)?;
        submenu
            .append(&MenuItem::new(
                format!(
                    "Credential: {}",
                    credential_label(&connection.credential, now)
                ),
                false,
                None,
            ))
            .map_err(menu_error)?;
        submenu
            .append(&MenuItem::new(
                format!("Hub: {}", hub_label(&connection.hub_check, now)),
                false,
                None,
            ))
            .map_err(menu_error)?;
        let integrations = Submenu::new("Set Up Agent Integration", true);
        integrations
            .append(&MenuItem::new(
                "Client CLI unavailable? Use guided Terminal setup.",
                false,
                None,
            ))
            .map_err(menu_error)?;
        integrations
            .append(&MenuItem::new(
                "Guided setup may sign in or renew the connection.",
                false,
                None,
            ))
            .map_err(menu_error)?;
        for (agent, label) in agent_choices() {
            let configured = connection.agents.iter().any(|saved| saved == agent.name());
            integrations
                .append(&MenuItem::with_id(
                    format!("setup:{identity}:{}", agent.name()),
                    format!("{} {label}", if configured { "Reapply" } else { "Set Up" }),
                    true,
                    None,
                ))
                .map_err(menu_error)?;
            integrations
                .append(&MenuItem::with_id(
                    format!("copy-setup:{identity}:{}", agent.name()),
                    format!("Copy Guided Terminal Setup for {label}"),
                    true,
                    None,
                ))
                .map_err(menu_error)?;
        }
        submenu.append(&integrations).map_err(menu_error)?;
        for participation in connection
            .recent_participation
            .iter()
            .filter(|participation| participation_current(participation.expires_at, now))
        {
            submenu
                .append(&MenuItem::new(
                    format!(
                        "{}: {} — {}",
                        super::clean_label(&participation.agent),
                        super::clean_label(&participation.state),
                        super::clean_label(&participation.next_action)
                    ),
                    false,
                    None,
                ))
                .map_err(menu_error)?;
        }
        if let Some(binding) = connection.binding.activity_generation.as_deref() {
            for row in activity
                .snapshot(binding, std::time::Instant::now())
                .into_iter()
                .rev()
                .take(5)
            {
                submenu
                    .append(&MenuItem::new(activity_label(&row), false, None))
                    .map_err(menu_error)?;
            }
        }
        submenu
            .append(&PredefinedMenuItem::separator())
            .map_err(menu_error)?;
        submenu
            .append(&MenuItem::with_id(
                format!("root:{identity}"),
                "Open Checkout",
                true,
                None,
            ))
            .map_err(menu_error)?;
        submenu
            .append(&MenuItem::with_id(
                format!("hub:{identity}"),
                "Open Hub",
                true,
                None,
            ))
            .map_err(menu_error)?;
        submenu
            .append(&MenuItem::with_id(
                format!("diagnostics:{identity}"),
                "Copy Diagnostics",
                true,
                None,
            ))
            .map_err(menu_error)?;
        submenu
            .append(&MenuItem::with_id(
                format!("reconnect:{identity}"),
                "Copy Reconnect Command",
                true,
                None,
            ))
            .map_err(menu_error)?;
        submenu
            .append(&MenuItem::with_id(
                format!("guidance:{identity}"),
                "Copy Repository Guidance Preview Command",
                true,
                None,
            ))
            .map_err(menu_error)?;
        menu.append(&submenu).map_err(menu_error)?;
    }
    menu.append(&PredefinedMenuItem::separator())
        .map_err(menu_error)?;
    if let Some(notice) = notice {
        menu.append(&MenuItem::new(super::clean_label(notice), false, None))
            .map_err(menu_error)?;
    }
    menu.append(&MenuItem::with_id("refresh", "Check Now", true, None))
        .map_err(menu_error)?;
    menu.append(&CheckMenuItem::with_id(
        "launch-login",
        "Start at Login (configured)",
        true,
        super::login::enabled(),
        None,
    ))
    .map_err(menu_error)?;
    menu.append(&MenuItem::with_id("quit", "Quit AgentSCM Tray", true, None))
        .map_err(menu_error)?;
    Ok(menu)
}

fn hub_label(check: &HubCheck, now: i64) -> String {
    match check {
        HubCheck::Skipped => "not checked".into(),
        HubCheck::Valid {
            checked_at,
            login,
            host,
            expires_at,
            ..
        } => {
            if *expires_at <= now {
                return format!(
                    "sign-in expired — checked {}",
                    unix_age_at(*checked_at, now)
                );
            }
            format!(
                "authenticated as {} — checked {} ({})",
                super::clean_label(login),
                unix_age_at(*checked_at, now),
                super::clean_label(host)
            )
        }
        HubCheck::Rejected { checked_at } => {
            format!(
                "sign-in rejected — checked {}",
                unix_age_at(*checked_at, now)
            )
        }
        HubCheck::Unavailable { checked_at } => {
            format!(
                "check unconfirmed — checked {}",
                unix_age_at(*checked_at, now)
            )
        }
        HubCheck::InvalidResponse { checked_at } => {
            format!("invalid reply — checked {}", unix_age_at(*checked_at, now))
        }
    }
}

fn binding_label(state: &BindingState) -> &'static str {
    match state {
        BindingState::Current => "checkout matches",
        BindingState::Mismatch => "checkout changed — reconnect",
        BindingState::Unknown => "not checked",
    }
}

fn credential_label(credential: &crate::client_status::CredentialStatus, now: i64) -> &'static str {
    if credential.expires_at.is_some_and(|expiry| expiry <= now) {
        return "sign-in expired";
    }
    match &credential.state {
        CredentialState::Available => "stored",
        CredentialState::Expired => "sign-in expired",
        CredentialState::Unknown => "unknown",
    }
}

fn participation_current(expires_at: i64, now: i64) -> bool {
    expires_at > now
}

fn projected_notice<'a>(
    explicit: Option<&'a str>,
    persistent: Option<&'a str>,
    checking: bool,
    empty: bool,
) -> Option<&'a str> {
    explicit
        .or(persistent)
        .or((checking && empty).then_some("Loading connected checkouts…"))
}

fn unix_age_at(timestamp: i64, now: i64) -> String {
    let seconds = now.saturating_sub(timestamp).max(0) as u64;
    if seconds < 60 {
        "less than 1m ago".into()
    } else {
        format!("{}m ago", seconds / 60)
    }
}

fn activity_label(row: &crate::client_activity::ActivityObservation) -> String {
    let age = if row.observed_at.elapsed() < Duration::from_secs(60) {
        "less than 1m ago".into()
    } else {
        format!("{}m ago", row.observed_at.elapsed().as_secs() / 60)
    };
    let subject = match row.path {
        ActivityPath::Mcp => "MCP transport reply observed".to_owned(),
        ActivityPath::Hook => format!(
            "{} hook reply observed",
            row.agent.as_deref().map(title_agent).unwrap_or("Agent")
        ),
    };
    let text = match row.outcome {
        ActivityOutcome::Acknowledged => subject,
        ActivityOutcome::TokenRejected => "sign-in rejected".into(),
        ActivityOutcome::Unavailable => "request unconfirmed".into(),
        ActivityOutcome::InvalidResponse => "invalid reply".into(),
        ActivityOutcome::BindingChanged => "binding changed".into(),
        ActivityOutcome::SessionEnded => "session-end hook observed".into(),
    };
    format!("{text} — {age}")
}

fn title_agent(agent: &str) -> &str {
    match agent {
        "claude" => "Claude",
        "codex" => "Codex",
        "cursor" => "Cursor",
        "copilot" => "Copilot",
        "gemini" => "Gemini",
        _ => "Agent",
    }
}

fn exact_menu_label(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .replace('&', "&&")
}

fn handle_action(id: &str, snapshot: &Snapshot, executable: &Path) {
    if let Some(command) = terminal_setup_command(id, snapshot, executable) {
        copy_text(&command);
        return;
    }
    let Some((kind, index)) = id.split_once(':') else {
        return;
    };
    let Some(connection) = snapshot
        .connections
        .iter()
        .find(|connection| connection_identity(connection) == index)
    else {
        return;
    };
    match kind {
        "root" => {
            let _ = Command::new("/usr/bin/open")
                .args(super::open_argv(&connection.root))
                .spawn();
        }
        "hub" => {
            let _ = Command::new("/usr/bin/open")
                .args(super::open_argv(Path::new(&connection.hub)))
                .spawn();
        }
        "diagnostics" => copy_text(&diagnostics(connection)),
        "reconnect" => copy_text(&reconnect_command(connection)),
        "guidance" => copy_text(&guidance_command(connection)),
        _ => {}
    }
}

fn terminal_setup_command(id: &str, snapshot: &Snapshot, executable: &Path) -> Option<String> {
    let mut parts = id.split(':');
    (parts.next()? == "copy-setup").then_some(())?;
    let identity = parts.next()?;
    let agent = parts.next()?;
    if parts.next().is_some() || !agent_choices().iter().any(|(item, _)| item.name() == agent) {
        return None;
    }
    let connection = snapshot
        .connections
        .iter()
        .find(|connection| connection_identity(connection) == identity)?;
    Some(terminal_setup_command_for(
        executable,
        agent,
        &connection.hub,
        &connection.root,
    ))
}

fn terminal_setup_command_for(executable: &Path, agent: &str, hub: &str, root: &Path) -> String {
    format!(
        "{} client setup --agent {} --hub {} --root {}",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(agent),
        shell_quote(hub),
        shell_quote(&root.to_string_lossy())
    )
}

fn copy_text(text: &str) {
    let text = text.to_owned();
    std::thread::spawn(move || {
        if let Ok(mut child) = Command::new("/usr/bin/pbcopy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    });
}

fn diagnostics(connection: &crate::client_status::ConnectionStatus) -> String {
    format!(
        "AgentSCM tray diagnostics\nbinding={:?}\ncredential={:?}\nhub_check={}\nagents={}\nparticipation={}\n",
        connection.binding.state,
        connection.credential.state,
        hub_kind(&connection.hub_check),
        connection.agents.len(),
        connection.recent_participation.len()
    )
}

fn hub_kind(check: &HubCheck) -> &'static str {
    match check {
        HubCheck::Skipped => "skipped",
        HubCheck::Valid { .. } => "valid",
        HubCheck::Rejected { .. } => "rejected",
        HubCheck::Unavailable { .. } => "unavailable",
        HubCheck::InvalidResponse { .. } => "invalid_response",
    }
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn reconnect_command(connection: &crate::client_status::ConnectionStatus) -> String {
    format!(
        "agentscm client connect --hub {} --repo {} --root {}",
        shell_quote(&connection.hub),
        shell_quote(&connection.repository),
        shell_quote(&connection.root.to_string_lossy())
    )
}
fn guidance_command(connection: &crate::client_status::ConnectionStatus) -> String {
    format!(
        "cd {} && agentscm init-repo --hub-url {}",
        shell_quote(&connection.root.to_string_lossy()),
        shell_quote(&connection.hub)
    )
}

fn connection_identity(connection: &crate::client_status::ConnectionStatus) -> String {
    let mut digest = Sha256::new();
    digest.update(connection.root.as_os_str().as_encoded_bytes());
    digest.update([0]);
    digest.update(connection.hub.as_bytes());
    digest.update([0]);
    if let Some(generation) = &connection.binding.generation {
        digest.update(generation.as_bytes());
    }
    format!("{:x}", digest.finalize())[..24].to_owned()
}

fn clock_discontinuity(
    previous: std::time::SystemTime,
    current: std::time::SystemTime,
    monotonic: Duration,
) -> bool {
    let wall = current.duration_since(previous).ok();
    match wall {
        Some(wall) => wall.abs_diff(monotonic) > Duration::from_secs(5),
        None => true,
    }
}

fn config_stamp() -> ConfigStamp {
    let mut paths = vec![
        crate::hub_cli::config_dir()
            .unwrap_or_default()
            .join("agentscm/connections.json"),
    ];
    if let Ok(connections) = crate::client_config::load() {
        paths.extend(
            connections
                .into_iter()
                .map(|connection| connection.token_file),
        );
    }
    paths.sort();
    paths.dedup();
    ConfigStamp(
        paths
            .into_iter()
            .map(|path| {
                let stamp = std::fs::symlink_metadata(&path).ok().map(|metadata| {
                    (
                        metadata.dev(),
                        metadata.ino(),
                        metadata.mtime(),
                        metadata.mtime_nsec(),
                        metadata.len(),
                    )
                });
                (path, stamp)
            })
            .collect(),
    )
}

fn icon() -> Result<Icon> {
    // Generated from docs/assets/agentscm-icon.png: Lanczos resize to 38 px centered in a
    // 44 px canvas, with source luminance × source alpha as the template mask. Using the PNG's
    // alpha alone would flatten its opaque dark robot and white details into one silhouette.
    let rgba = include_bytes!("agentscm-icon-44.rgba").to_vec();
    Icon::from_rgba(rgba, 44, 44)
        .map_err(|error| Error::Invalid(format!("cannot create tray icon: {error}")))
}

fn menu_error(error: tray_icon::menu::Error) -> Error {
    Error::Invalid(format!("cannot create tray menu: {error}"))
}

struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    fn acquire() -> Result<Option<Self>> {
        let path = crate::hub_cli::config_dir()?
            .join("agentscm")
            .join("tray.lock");
        Self::acquire_at(&path)
    }

    fn acquire_at(path: &Path) -> Result<Option<Self>> {
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
        {
            return Err(Error::Invalid(
                "tray lock is not an owner-controlled regular file".into(),
            ));
        }
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_commands_quote_shell_metacharacters_as_data() {
        assert_eq!(shell_quote("a'b;$(touch nope)"), "'a'\\''b;$(touch nope)'");
        assert_eq!(
            terminal_setup_command_for(
                Path::new("/Applications/AgentSCM versions/agentscm'current"),
                "codex",
                "https://hub.example.test/a'b",
                Path::new("/tmp/a b;$(touch nope)"),
            ),
            "'/Applications/AgentSCM versions/agentscm'\\''current' client setup --agent 'codex' --hub 'https://hub.example.test/a'\\''b' --root '/tmp/a b;$(touch nope)'"
        );
    }

    #[test]
    fn diagnostics_hub_kinds_do_not_include_remote_identity() {
        assert_eq!(hub_kind(&HubCheck::Rejected { checked_at: 7 }), "rejected");
        assert_eq!(
            hub_kind(&HubCheck::Unavailable { checked_at: 8 }),
            "unavailable"
        );
    }

    #[test]
    fn clock_projection_detects_suspend_and_backward_wall_clock() {
        let start = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        assert!(!clock_discontinuity(
            start,
            start + Duration::from_secs(2),
            Duration::from_secs(2)
        ));
        assert!(clock_discontinuity(
            start,
            start + Duration::from_secs(20),
            Duration::from_secs(2)
        ));
        assert!(clock_discontinuity(
            start,
            start - Duration::from_secs(1),
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn empty_refresh_keeps_loading_notice_until_snapshot_arrives() {
        assert_eq!(
            projected_notice(None, None, true, true),
            Some("Loading connected checkouts…")
        );
        assert_eq!(projected_notice(None, None, false, true), None);
        assert_eq!(
            projected_notice(Some("Configuration changed; checking…"), None, true, true),
            Some("Configuration changed; checking…")
        );
        assert_eq!(
            projected_notice(None, Some("Setup failed"), true, true),
            Some("Setup failed")
        );
    }

    #[test]
    fn singleton_lock_is_nonblocking_released_and_refuses_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tray.lock");
        let first = InstanceLock::acquire_at(&path).unwrap().unwrap();
        assert!(InstanceLock::acquire_at(&path).unwrap().is_none());
        drop(first);
        assert!(InstanceLock::acquire_at(&path).unwrap().is_some());
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(temp.path().join("target"), &path).unwrap();
        assert!(InstanceLock::acquire_at(&path).is_err());
    }

    #[test]
    fn status_labels_are_neutral_plain_language() {
        let now = agentscm::hub::store::unix_now();
        assert_eq!(
            binding_label(&BindingState::Mismatch),
            "checkout changed — reconnect"
        );
        let credential = crate::client_status::CredentialStatus {
            kind: crate::client_status::CredentialKind::External,
            state: CredentialState::Unknown,
            expires_at: None,
        };
        assert_eq!(credential_label(&credential, now), "unknown");
        assert_eq!(
            hub_label(&HubCheck::Unavailable { checked_at: now }, now),
            "check unconfirmed — checked less than 1m ago"
        );
    }

    #[test]
    fn expiry_crossings_reproject_credentials_and_hub_checks() {
        let credential = crate::client_status::CredentialStatus {
            kind: crate::client_status::CredentialKind::Managed,
            state: CredentialState::Available,
            expires_at: Some(101),
        };
        assert_eq!(credential_label(&credential, 100), "stored");
        assert_eq!(credential_label(&credential, 101), "sign-in expired");
        assert!(participation_current(101, 100));
        assert!(!participation_current(101, 101));
        let check = HubCheck::Valid {
            checked_at: 90,
            provider: "github".into(),
            host: "github.com".into(),
            login: "member".into(),
            expires_at: 101,
        };
        assert!(hub_label(&check, 100).starts_with("authenticated as member"));
        assert_eq!(
            hub_label(&check, 101),
            "sign-in expired — checked less than 1m ago"
        );
    }

    #[test]
    fn integration_menu_covers_supported_agents_with_user_labels() {
        assert_eq!(
            agent_choices().map(|(agent, label)| (agent.name(), label)),
            [
                ("claude", "Claude Code"),
                ("codex", "Codex"),
                ("cursor", "Cursor"),
                ("copilot", "Copilot"),
                ("gemini", "Gemini"),
            ]
        );
    }
}
