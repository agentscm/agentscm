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

use agentscm::change_record::CaptureModeV1;
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

#[path = "hub_open.rs"]
mod hub_open;
#[path = "native_feedback.rs"]
mod native_feedback;
#[path = "native_menu.rs"]
mod native_menu;

const READY: &str = "AGENTSCM_TRAY_READY";
const ALREADY_RUNNING: &str = "AGENTSCM_TRAY_ALREADY_RUNNING";
const RECENT_HUB_CHECK_SECONDS: i64 = 60;

enum UserEvent {
    Menu(String),
    LaunchLogin(bool),
    LoginResult(bool, std::result::Result<(), String>),
    MenuTrackingEnded,
    Snapshot(std::result::Result<Snapshot, String>, ConfigStamp),
    Integration(crate::client_setup::Agent, std::result::Result<(), String>),
    ShareConversation(ShareRequest, std::result::Result<(), String>),
    Connected(
        crate::client_setup::Agent,
        String,
        std::result::Result<String, String>,
    ),
    HubOpen(std::result::Result<Box<hub_open::PreparedOpen>, hub_open::OpenError>),
    /// Open Hub opened this computer's own Hub page (`agentscm open`).
    HubOpenedLocally,
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

    let login_enabled = super::login::enabled();
    let menu = build_menu(
        &initial,
        &activity,
        Some("Loading connected checkouts…"),
        login_enabled,
        None,
    )?;
    let refreshing = Arc::new(AtomicBool::new(false));
    let installing = Arc::new(AtomicBool::new(false));
    let refresh_proxy = event_loop.create_proxy();
    refreshing.store(true, Ordering::Release);
    spawn_refresh(refresh_proxy.clone(), true);
    let now = std::time::Instant::now();
    let menu_key = menu_projection_key(
        &initial,
        &activity,
        Some("Loading connected checkouts…"),
        login_enabled,
        None,
        agentscm::hub::store::unix_now(),
        now,
    );
    let native_menu =
        native_menu::NativeMenuBridge::attach(&menu, refresh_proxy.clone(), login_enabled)
            .filter(native_menu::NativeMenuBridge::is_installed)
            .ok_or_else(|| Error::Invalid("cannot install native tray menu controls".into()))?;
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
        menu_key,
        observed_config: config_stamp(),
        pending_config: None,
        initial_menu: Some(menu),
        initial_icon: Some(icon()?),
        tray: None,
        native_menu: Some(native_menu),
        feedback: native_feedback::NativeFeedback::new()
            .ok_or_else(|| Error::Invalid("cannot create tray action feedback".into()))?,
        opening_hub: false,
        login_changing: false,
        menu_dirty: false,
        notice: None,
        pending_share: None,
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
    menu_key: String,
    observed_config: ConfigStamp,
    pending_config: Option<(ConfigStamp, std::time::Instant)>,
    initial_menu: Option<Menu>,
    initial_icon: Option<Icon>,
    tray: Option<tray_icon::TrayIcon>,
    native_menu: Option<native_menu::NativeMenuBridge>,
    feedback: native_feedback::NativeFeedback,
    opening_hub: bool,
    login_changing: bool,
    menu_dirty: bool,
    notice: Option<Notice>,
    /// The checkout whose sharing the person was asked to confirm. Sharing turns on only from the
    /// explicit confirm item this makes the menu show, never from the first click.
    pending_share: Option<PendingShare>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Notice {
    Action(String),
    Refresh(String),
}

impl Notice {
    fn as_str(&self) -> &str {
        match self {
            Self::Action(message) | Self::Refresh(message) => message,
        }
    }
}

fn update_refresh_notice(persistent: &mut Option<Notice>, notice: Option<&str>) {
    if let Some(notice) = notice {
        *persistent = Some(Notice::Refresh(notice.to_owned()));
    }
}

fn clear_refresh_notice(notice: &mut Option<Notice>) {
    if matches!(notice, Some(Notice::Refresh(_))) {
        *notice = None;
    }
}

impl TrayApp {
    fn replace_menu(&mut self, notice: Option<&str>) {
        update_refresh_notice(&mut self.notice, notice);
        self.pending_share = self.pending_share.take().filter(|pending| {
            pending_share_current(pending, &self.snapshot, std::time::Instant::now())
        });
        let confirming = self
            .pending_share
            .as_ref()
            .map(|pending| pending.request.identity.as_str());
        let notice = projected_notice(
            None,
            self.notice.as_ref().map(Notice::as_str),
            self.refreshing.load(Ordering::Acquire) || self.pending_config.is_some(),
            self.snapshot.connections.is_empty(),
        );
        let login_enabled = super::login::enabled();
        let key = menu_projection_key(
            &self.snapshot,
            &self.activity,
            notice,
            login_enabled,
            confirming,
            agentscm::hub::store::unix_now(),
            std::time::Instant::now(),
        );
        let tracking = self
            .native_menu
            .as_ref()
            .is_some_and(native_menu::NativeMenuBridge::is_tracking);
        match menu_swap_decision(&self.menu_key, &key, tracking) {
            MenuSwapDecision::Unchanged => {
                self.menu_dirty = false;
                return;
            }
            MenuSwapDecision::Defer => {
                self.menu_dirty = true;
                return;
            }
            MenuSwapDecision::Replace => {}
        }
        if let (Some(tray), Ok(menu)) = (
            &self.tray,
            build_menu(
                &self.snapshot,
                &self.activity,
                notice,
                login_enabled,
                confirming,
            ),
        ) {
            let Some(native_menu) = native_menu::NativeMenuBridge::attach(
                &menu,
                self.refresh_proxy.clone(),
                login_enabled,
            ) else {
                self.notice = Some(Notice::Action(
                    "Could not update native tray controls".into(),
                ));
                return;
            };
            if self.login_changing {
                native_menu.set_login_state(native_menu::NativeLoginState::Pending(login_enabled));
            }
            tray.set_menu(Some(Box::new(menu)));
            self.menu_key = key;
            self.native_menu = Some(native_menu);
            self.menu_dirty = false;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuSwapDecision {
    Unchanged,
    Defer,
    Replace,
}

fn menu_swap_decision(current: &str, candidate: &str, tracking: bool) -> MenuSwapDecision {
    if current == candidate {
        MenuSwapDecision::Unchanged
    } else if tracking {
        MenuSwapDecision::Defer
    } else {
        MenuSwapDecision::Replace
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
                discard_stdout();
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
                let desired = !super::login::enabled();
                let _ = self
                    .refresh_proxy
                    .send_event(UserEvent::LaunchLogin(desired));
            }
            UserEvent::LaunchLogin(desired) => {
                self.notice = None;
                if self.login_changing {
                    self.notice = Some(Notice::Action(
                        "A launch-at-login change is still in progress".into(),
                    ));
                    if let Some(native_menu) = &self.native_menu {
                        native_menu.set_enabled(super::login::enabled());
                    }
                    self.replace_menu(None);
                } else {
                    self.login_changing = true;
                    if let Some(native_menu) = &self.native_menu {
                        native_menu.set_login_state(native_menu::NativeLoginState::Pending(
                            super::login::enabled(),
                        ));
                    }
                    spawn_login_change(
                        self.refresh_proxy.clone(),
                        desired,
                        self.executable.clone(),
                    );
                }
            }
            UserEvent::LoginResult(desired, result) => {
                self.login_changing = false;
                let notice = match result {
                    Err(error) => error,
                    Ok(()) if desired => "Launch at login enabled for the next login".into(),
                    Ok(()) => "Launch at login disabled".into(),
                };
                if let Some(native_menu) = &self.native_menu {
                    native_menu.set_enabled(super::login::enabled());
                }
                self.notice = Some(Notice::Action(notice));
                self.replace_menu(None);
            }
            UserEvent::MenuTrackingEnded => {
                if self.menu_dirty {
                    self.replace_menu(None);
                }
            }
            UserEvent::Menu(id) if id.starts_with("setup:") => {
                self.notice = None;
                if self.opening_hub || self.installing.load(Ordering::Acquire) {
                    self.notice = Some(Notice::Action(
                        "An action is still running; wait for its result".into(),
                    ));
                    self.replace_menu(None);
                } else if let Some(request) =
                    integration_request(&id, &self.snapshot, &self.executable)
                {
                    self.installing.store(true, Ordering::Release);
                    self.feedback.show(
                        &format!("Setting up {}…", request.label),
                        "Configuring AgentSCM hooks and MCP for this checkout. Your coding client will ask for its own approvals when you open it.",
                        true,
                    );
                    spawn_integration(self.refresh_proxy.clone(), request);
                    self.notice = Some(Notice::Action("Setting up agent integration…".into()));
                    self.replace_menu(None);
                } else {
                    self.feedback.show(
                        "Setup could not start",
                        "The checkout connection changed. Refresh connections and try again.",
                        false,
                    );
                    self.notice = Some(Notice::Action(
                        "The checkout connection changed; refresh and try again".into(),
                    ));
                    self.replace_menu(None);
                }
            }
            UserEvent::Menu(id)
                if id.starts_with("share-conv:") || id.starts_with("share-confirm:") =>
            {
                self.notice = None;
                if self.opening_hub || self.installing.load(Ordering::Acquire) {
                    self.notice = Some(Notice::Action(
                        "An action is still running; wait for its result".into(),
                    ));
                } else {
                    match share_step(
                        &id,
                        &self.snapshot,
                        self.pending_share.as_ref(),
                        std::time::Instant::now(),
                    ) {
                        ShareStep::AskConfirmation(request) => {
                            let (title, body) = share_confirmation(&request);
                            self.feedback.show(&title, &body, false);
                            self.notice = Some(Notice::Action(title));
                            self.pending_share = Some(PendingShare {
                                request,
                                asked_at: std::time::Instant::now(),
                            });
                        }
                        ShareStep::Change(request) => {
                            self.pending_share = None;
                            self.installing.store(true, Ordering::Release);
                            self.feedback.show(
                                "Updating conversation sharing…",
                                &share_progress(&request),
                                true,
                            );
                            self.notice =
                                Some(Notice::Action("Updating conversation sharing…".into()));
                            spawn_share_change(self.refresh_proxy.clone(), request);
                        }
                        ShareStep::Unavailable => {
                            self.pending_share = None;
                            self.feedback.show(
                                "Conversation sharing unavailable",
                                "The checkout connection changed. Refresh connections and try again.",
                                false,
                            );
                            self.notice = Some(Notice::Action(
                                "Conversation sharing is unavailable; refresh and try again".into(),
                            ));
                        }
                    }
                }
                self.replace_menu(None);
            }
            UserEvent::ShareConversation(request, result) => {
                self.installing.store(false, Ordering::Release);
                let (title, body) = share_outcome(&request, &result);
                self.feedback.update(&title, &body, false);
                if result.is_ok()
                    && let Some(connection) = self
                        .snapshot
                        .connections
                        .iter_mut()
                        .find(|connection| connection_identity(connection) == request.identity)
                {
                    connection.capture_mode = Some(request.desired);
                }
                self.notice = Some(Notice::Action(title));
                self.replace_menu(None);
                if !self.refreshing.swap(true, Ordering::AcqRel) {
                    spawn_refresh(self.refresh_proxy.clone(), false);
                }
            }
            UserEvent::Menu(id) if id.starts_with("connect:") => {
                self.notice = None;
                let request = connect_request(
                    &id,
                    &self.snapshot,
                    &self.activity,
                    std::time::Instant::now(),
                );
                let hub = request
                    .as_ref()
                    .and_then(|request| target_hub(&request.host, &request.repo));
                if self.opening_hub || self.installing.load(Ordering::Acquire) {
                    self.notice = Some(Notice::Action(
                        "An action is still running; wait for its result".into(),
                    ));
                } else if let (Some(request), Some(hub)) = (&request, &hub) {
                    self.installing.store(true, Ordering::Release);
                    self.feedback.show(
                        &format!("Connecting {}…", request.repo),
                        &format!(
                            "Connecting this checkout to {hub} with your saved sign-in, then configuring {}. Your coding client will ask for its own approvals when you open it.",
                            request.label
                        ),
                        true,
                    );
                    spawn_connect(
                        self.refresh_proxy.clone(),
                        request.clone(),
                        hub.clone(),
                        self.executable.clone(),
                    );
                    self.notice = Some(Notice::Action("Connecting checkout…".into()));
                } else if request.is_some() {
                    self.feedback.show(
                        "No saved Hub sign-in",
                        "Copy Terminal Setup and run it in a terminal to choose a Hub and sign in.",
                        false,
                    );
                    self.notice = Some(Notice::Action("No saved Hub sign-in".into()));
                } else {
                    self.feedback.show(
                        "Connect could not start",
                        "This checkout is no longer listed as not connected. Check Now and try again.",
                        false,
                    );
                }
                self.replace_menu(None);
            }
            UserEvent::Menu(id) if id.starts_with("copy-quickstart:") => {
                self.notice = None;
                if let Some(row) =
                    unconnected_rows(&self.snapshot, &self.activity, std::time::Instant::now())
                        .into_iter()
                        .find(|row| id == format!("copy-quickstart:{}", unconnected_identity(row)))
                {
                    copy_text(&quickstart_command_for(&self.executable, &row));
                }
                self.replace_menu(None);
            }
            UserEvent::Menu(id) if id.starts_with("hub:") => {
                if self.opening_hub || self.installing.load(Ordering::Acquire) {
                    self.notice = Some(Notice::Action(
                        "An action is still running; wait for its result".into(),
                    ));
                } else {
                    let request = self
                        .snapshot
                        .connections
                        .iter()
                        .find(|connection| id == format!("hub:{}", connection_identity(connection)))
                        .ok_or(hub_open::OpenError::InvalidConnection)
                        .and_then(hub_open::OpenRequest::from_status);
                    match request {
                        Ok(request) => {
                            self.opening_hub = true;
                            self.feedback.show(
                                "Opening Hub…",
                                "Preparing a read-only browser session for this repository.",
                                true,
                            );
                            self.notice = Some(Notice::Action("Opening Hub…".into()));
                            spawn_hub_open(self.refresh_proxy.clone(), request);
                        }
                        Err(error) => {
                            self.feedback
                                .show("Could not open Hub", &error.to_string(), false);
                            self.notice =
                                Some(Notice::Action(format!("Could not open Hub: {error}")));
                        }
                    }
                }
                self.replace_menu(None);
            }
            UserEvent::Menu(id) => {
                self.notice = None;
                handle_action(&id, &self.snapshot, &self.executable);
                self.replace_menu(None);
            }
            UserEvent::Snapshot(Ok(updated), dispatched) => {
                self.refreshing.store(false, Ordering::Release);
                if dispatched == config_stamp() {
                    for connection in &updated.connections {
                        self.activity.forget_unconnected(&connection.root);
                    }
                    self.snapshot = updated;
                    clear_refresh_notice(&mut self.notice);
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
            UserEvent::Integration(agent, result) => {
                self.installing.store(false, Ordering::Release);
                self.notice = Some(Notice::Action(match result {
                    Ok(()) => {
                        let label = agent_choices()
                            .into_iter()
                            .find(|(candidate, _)| *candidate == agent)
                            .expect("known agent")
                            .1;
                        self.feedback.update(
                            &format!("{label} integration configured"),
                            setup_next_step(agent),
                            false,
                        );
                        format!("{label} integration configured; open a fresh client session")
                    }
                    Err(error) => {
                        self.feedback
                            .update("Integration setup failed", &error, false);
                        format!("Integration setup failed: {error}")
                    }
                }));
                self.replace_menu(None);
                if !self.refreshing.swap(true, Ordering::AcqRel) {
                    spawn_refresh(self.refresh_proxy.clone(), false);
                }
            }
            UserEvent::HubOpenedLocally => {
                self.opening_hub = false;
                self.feedback.update("Hub opened", "Your browser opened this computer's Hub page, connected to your files, saved conversations and your team's work.", false);
                self.notice = Some(Notice::Action("Hub opened in your browser".into()));
                self.replace_menu(None);
            }
            UserEvent::Connected(agent, repo, result) => {
                self.installing.store(false, Ordering::Release);
                self.notice = Some(Notice::Action(match result {
                    Ok(hub) => {
                        self.feedback.update(
                            &format!("{repo} connected"),
                            &format!("Connected to {hub}. {}", setup_next_step(agent)),
                            false,
                        );
                        format!("{repo} connected; open a fresh client session")
                    }
                    Err(error) => {
                        self.feedback.update("Connect failed", &error, false);
                        format!("Connect failed: {error}")
                    }
                }));
                self.replace_menu(None);
                if !self.refreshing.swap(true, Ordering::AcqRel) {
                    spawn_refresh(self.refresh_proxy.clone(), false);
                }
            }
            UserEvent::HubOpen(result) => {
                self.opening_hub = false;
                let opened = result
                    .map(|prepared| *prepared)
                    .map_err(|error| error.to_string())
                    .and_then(|prepared| {
                        finish_hub_open(
                            prepared,
                            |prepared| {
                                prepared
                                    .validate_current()
                                    .map_err(|error| error.to_string())
                            },
                            |prepared| {
                                native_feedback::open_url(prepared.url()).map_err(str::to_owned)
                            },
                            revoke_hub_open,
                        )
                    });
                match opened {
                    Ok(()) => {
                        self.feedback.update("Hub opened", "A read-only session was sent to your browser. An existing browser sign-in is kept; sign out there first to switch repositories.", false);
                        self.notice = Some(Notice::Action("Hub opened in your browser".into()));
                    }
                    Err(error) => {
                        self.feedback.update("Could not open Hub", &error, false);
                        self.notice = Some(Notice::Action(format!("Could not open Hub: {error}")));
                    }
                }
                self.replace_menu(None);
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

fn spawn_login_change(
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    desired: bool,
    executable: std::path::PathBuf,
) {
    std::thread::spawn(move || {
        let result = super::set_login_enabled(desired, &executable)
            .map_err(|error| bounded_error(&error.to_string()));
        let _ = proxy.send_event(UserEvent::LoginResult(desired, result));
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
        .and_then(|result| result.map_err(|error| bounded_error(&error.to_string())));
        let _ = proxy.send_event(UserEvent::Integration(request.agent, result));
    });
}

#[derive(Clone, Debug)]
struct ShareRequest {
    identity: String,
    root: std::path::PathBuf,
    repository: String,
    hub: String,
    label: String,
    desired: CaptureModeV1,
}

/// What turning sharing on does, in the words every surface uses.
fn share_consequences(hub: &str) -> String {
    format!(
        "Conversations are redacted on this computer, uploaded to {hub}, kept 30 days by default, and readable by you and Hub admins. Redaction is best effort. Turning sharing off stops future uploads."
    )
}

fn share_progress(request: &ShareRequest) -> String {
    if request.desired == CaptureModeV1::SharedConversation {
        format!(
            "Setting up capture for {} if needed, then turning sharing on. {}",
            request.label,
            share_consequences(&request.hub)
        )
    } else {
        "Switching this checkout to metadata only. Local data is not touched.".into()
    }
}

fn share_menu_id(identity: &str) -> String {
    format!("share-conv:{identity}")
}

fn share_confirm_id(identity: &str) -> String {
    format!("share-confirm:{identity}")
}

fn share_confirm_label(hub: &str) -> String {
    format!(
        "Confirm: share conversations with {}",
        super::clean_label(hub)
    )
}

/// What a click on a sharing item does. Turning sharing on is two clicks: the toggle only asks,
/// and the confirm item that then appears for that checkout is the one that changes anything.
enum ShareStep {
    /// Turning sharing on needs the person to confirm what it does first.
    AskConfirmation(ShareRequest),
    /// Turn sharing off, or on after the explicit confirmation.
    Change(ShareRequest),
    Unavailable,
}

fn share_step(
    id: &str,
    snapshot: &Snapshot,
    pending: Option<&PendingShare>,
    now: std::time::Instant,
) -> ShareStep {
    if let Some(identity) = id.strip_prefix("share-confirm:") {
        return pending
            .filter(|pending| pending.request.identity == identity)
            .filter(|pending| pending_share_current(pending, snapshot, now))
            .and_then(|_| share_request(&share_menu_id(identity), snapshot))
            .filter(|request| request.desired == CaptureModeV1::SharedConversation)
            .map_or(ShareStep::Unavailable, ShareStep::Change);
    }
    match share_request(id, snapshot) {
        Some(request) if request.desired == CaptureModeV1::SharedConversation => {
            ShareStep::AskConfirmation(request)
        }
        Some(request) => ShareStep::Change(request),
        None => ShareStep::Unavailable,
    }
}

/// How long a share confirmation stays available after the person was asked.
const SHARE_CONFIRM_TTL: Duration = Duration::from_secs(120);

/// A share request the person was asked to confirm, and when.
struct PendingShare {
    request: ShareRequest,
    asked_at: std::time::Instant,
}

/// Whether a pending confirmation still describes what the menu shows: asked within
/// [`SHARE_CONFIRM_TTL`], the same checkout and Hub (the identity covers both), not yet shared.
fn pending_share_current(
    pending: &PendingShare,
    snapshot: &Snapshot,
    now: std::time::Instant,
) -> bool {
    now.saturating_duration_since(pending.asked_at) < SHARE_CONFIRM_TTL
        && share_request(&share_menu_id(&pending.request.identity), snapshot)
            .is_some_and(|request| request.desired == CaptureModeV1::SharedConversation)
}

fn share_confirmation(request: &ShareRequest) -> (String, String) {
    (
        format!("Share conversations from {}?", request.label),
        format!(
            "{} Nothing is shared yet. Open this checkout's menu and choose \"{}\" to turn sharing on.",
            share_consequences(&request.hub),
            share_confirm_label(&request.hub)
        ),
    )
}

fn share_mismatch_note(saved: &crate::client_status::CaptureMismatch, connected: &str) -> String {
    format!(
        "Sharing was set for {}; this checkout is now connected to {}.\nTurn sharing off here, or run `agentscm change-capture forget --root <checkout>` after unconfigure to start fresh.",
        super::clean_label(&saved.hub),
        super::clean_label(connected)
    )
}

/// The toggle flips the mode in the snapshot, not the checkmark AppKit just drew, so a stale or
/// failed click can never request a change the user did not see. A checkout without an enabled
/// binding is unchecked, and turning sharing on creates the binding.
fn share_request(id: &str, snapshot: &Snapshot) -> Option<ShareRequest> {
    let identity = id.strip_prefix("share-conv:")?;
    let connection = snapshot
        .connections
        .iter()
        .find(|connection| connection_identity(connection) == identity)?;
    let desired = match connection.capture_mode {
        Some(CaptureModeV1::SharedConversation) => CaptureModeV1::MetadataOnly,
        _ => CaptureModeV1::SharedConversation,
    };
    Some(ShareRequest {
        identity: identity.to_owned(),
        root: connection.root.clone(),
        repository: connection.repository.clone(),
        hub: connection.hub.clone(),
        label: connection
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .map_or_else(|| connection.repository.clone(), super::clean_label),
        desired,
    })
}

/// Whether the share toggle is checked. It is always enabled: without a binding, a click sets
/// capture up.
fn share_toggle_checked(mode: Option<CaptureModeV1>) -> bool {
    mode == Some(CaptureModeV1::SharedConversation)
}

fn share_outcome(
    request: &ShareRequest,
    result: &std::result::Result<(), String>,
) -> (String, String) {
    match (result, request.desired) {
        (Err(error), _) => (
            "Could not change conversation sharing".into(),
            error.clone(),
        ),
        (Ok(()), CaptureModeV1::SharedConversation) => (
            format!("Sharing on for {}", request.label),
            format!(
                "Sharing on for {}. {}",
                request.label,
                share_consequences(&request.hub)
            ),
        ),
        (Ok(()), _) => (
            format!("Sharing off for {}", request.label),
            format!(
                "Sharing off for {}: new conversations are no longer uploaded; already-uploaded copies remain until they expire or you delete them",
                request.label
            ),
        ),
    }
}

fn spawn_share_change(proxy: winit::event_loop::EventLoopProxy<UserEvent>, request: ShareRequest) {
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if request.desired == CaptureModeV1::SharedConversation {
                crate::change_capture_client::enable_capture(
                    &request.root,
                    request.desired,
                    crate::change_capture_client::ExpectedTarget {
                        repository: &request.repository,
                        hub: &request.hub,
                    },
                )
            } else {
                crate::change_capture_client::set_capture_mode(&request.root, request.desired, None)
            }
        }))
        .map_err(|_| "conversation sharing change stopped unexpectedly".to_owned())
        .and_then(|result| {
            result
                .map(|_| ())
                .map_err(|error| bounded_error(&error.to_string()))
        });
        let _ = proxy.send_event(UserEvent::ShareConversation(request, result));
    });
}

#[derive(Clone)]
struct ConnectRequest {
    agent: crate::client_setup::Agent,
    label: &'static str,
    root: std::path::PathBuf,
    host: String,
    repo: String,
}

fn connect_request(
    id: &str,
    snapshot: &Snapshot,
    activity: &crate::client_activity::ActivityStore,
    now: std::time::Instant,
) -> Option<ConnectRequest> {
    let mut parts = id.split(':');
    (parts.next()? == "connect").then_some(())?;
    let identity = parts.next()?;
    let agent_name = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let row = unconnected_rows(snapshot, activity, now)
        .into_iter()
        .find(|row| unconnected_identity(row) == identity)?;
    row.agents
        .iter()
        .any(|agent| agent == agent_name)
        .then_some(())?;
    let (agent, label) = agent_choices()
        .into_iter()
        .find(|(agent, _)| agent.name() == agent_name)?;
    Some(ConnectRequest {
        agent,
        label,
        root: row.root,
        host: row.host,
        repo: row.repo,
    })
}

/// The Hub a tray Connect uses for `host`/`repo`, with the saved sign-in it needs.
fn target_hub(host: &str, repo: &str) -> Option<String> {
    let connections = crate::client_config::load().unwrap_or_default();
    connect_hub(host, repo, &connections, |hub| {
        crate::hub_cli::load_session(hub).ok().map(|(hub, _)| hub)
    })
}

/// A Hub another checkout of the same forge repository is connected to and still signed in to,
/// so its agents meet the others; else the default saved sign-in.
fn connect_hub(
    host: &str,
    repo: &str,
    connections: &[crate::client_config::Connection],
    signed_in: impl Fn(Option<&str>) -> Option<String>,
) -> Option<String> {
    connections
        .iter()
        .filter(|connection| {
            connection.origin_host.eq_ignore_ascii_case(host)
                && connection.repo.eq_ignore_ascii_case(repo)
        })
        .find_map(|connection| signed_in(Some(&connection.hub)))
        .or_else(|| signed_in(None))
}

fn spawn_connect(
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    request: ConnectRequest,
    hub: String,
    program: std::path::PathBuf,
) {
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime
                .block_on(crate::client_cli::setup_unconnected(
                    request.agent,
                    &request.root,
                    &hub,
                    &program,
                ))
                .map_err(|error| error.to_string())
        }))
        .map_err(|_| "connect stopped unexpectedly".to_owned())
        .and_then(|result| result.map_err(|error| bounded_error(&error)))
        .map(|()| hub);
        let _ = proxy.send_event(UserEvent::Connected(request.agent, request.repo, result));
    });
}

/// Unconnected checkouts still worth offering: fresh, and not connected since they were seen.
fn unconnected_rows(
    snapshot: &Snapshot,
    activity: &crate::client_activity::ActivityStore,
    now: std::time::Instant,
) -> Vec<crate::client_activity::UnconnectedCheckout> {
    let mut rows = activity.unconnected(now);
    rows.retain(|row| {
        !snapshot
            .connections
            .iter()
            .any(|connection| connection.root == row.root)
    });
    rows
}

fn unconnected_identity(row: &crate::client_activity::UnconnectedCheckout) -> String {
    let digest = Sha256::digest(row.root.as_os_str().as_encoded_bytes());
    format!("{digest:x}")[..24].to_owned()
}

fn unconnected_details(
    row: &crate::client_activity::UnconnectedCheckout,
    now: std::time::Instant,
    hub: Option<&str>,
) -> String {
    let elapsed = now.saturating_duration_since(row.seen_at);
    let age = if elapsed < Duration::from_secs(60) {
        "less than 1m ago".to_owned()
    } else {
        format!("{}m ago", elapsed.as_secs() / 60)
    };
    let agents = row
        .agents
        .iter()
        .rev()
        .map(|agent| agent_label(agent))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Agent  {agents}\nLast seen  {age}\nForge  {}\nPath  {}\nConnects to  {}",
        full_menu_value(&row.host),
        full_menu_value(&row.root.to_string_lossy()),
        hub.map_or_else(|| "no saved Hub sign-in".to_owned(), full_menu_value),
    )
}

fn agent_label(agent: &str) -> &'static str {
    agent_choices()
        .into_iter()
        .find(|(choice, _)| choice.name() == agent)
        .map_or("Agent", |(_, label)| label)
}

fn quickstart_command_for(
    executable: &Path,
    row: &crate::client_activity::UnconnectedCheckout,
) -> String {
    let agent = row.agents.last().map_or("claude", String::as_str);
    format!(
        "cd {} && {} quickstart --agent {}",
        shell_quote(&row.root.to_string_lossy()),
        shell_quote(&executable.to_string_lossy()),
        shell_quote(agent)
    )
}

/// `client setup` prints progress, and once the launcher has read the ready line, stdout is a
/// closed pipe on which `println!` would panic.
fn discard_stdout() {
    use std::os::fd::AsRawFd;
    if let Ok(null) = OpenOptions::new().write(true).open("/dev/null") {
        // SAFETY: both descriptors are open for the call; dup2 only replaces descriptor 1.
        unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) };
    }
}

fn setup_next_step(agent: crate::client_setup::Agent) -> &'static str {
    use crate::client_setup::Agent;
    match agent {
        Agent::Claude => {
            "Open Claude Code in this checkout, review its plugin and MCP approvals, then start a fresh session. Setup configures AgentSCM; it does not install Claude Code."
        }
        Agent::Codex => {
            "Open Codex in this checkout, approve the AgentSCM plugin and MCP server, reload hooks, then start a fresh chat. Setup configures AgentSCM; it does not install Codex."
        }
        Agent::Cursor => {
            "Reopen this checkout in Cursor and review its hook and MCP approvals. Start a fresh agent session. Setup configures AgentSCM; it does not install Cursor."
        }
        Agent::Copilot => {
            "Open Copilot CLI in this checkout, review its hook and MCP approvals, then start a fresh session. Setup configures AgentSCM; it does not install Copilot CLI."
        }
        Agent::Gemini => {
            "Open Gemini CLI in this checkout, trust its project settings, and approve AgentSCM hooks and MCP. Start a fresh session. If Gemini CLI is not installed, install it first."
        }
    }
}

fn spawn_hub_open(
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    request: hub_open::OpenRequest,
) {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        // This computer's own page first; the hosted Hub's read-only link only if it cannot start.
        if let Ok(runtime) = &runtime
            && runtime
                .block_on(crate::open_cli::open_checkout(request.root()))
                .is_ok()
        {
            let _ = proxy.send_event(UserEvent::HubOpenedLocally);
            return;
        }
        let result = match &runtime {
            Ok(runtime) => runtime.block_on(hub_open::prepare_open(request)),
            Err(_) => Err(hub_open::OpenError::Unavailable),
        };
        if let Err(winit::event_loop::EventLoopClosed(UserEvent::HubOpen(Ok(prepared)))) =
            proxy.send_event(UserEvent::HubOpen(result.map(Box::new)))
            && let Ok(runtime) = runtime
        {
            runtime.block_on((*prepared).revoke());
        }
    });
}

fn revoke_hub_open(prepared: hub_open::PreparedOpen) {
    std::thread::spawn(move || {
        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            runtime.block_on(prepared.revoke());
        }
    });
}

/// Keep the last binding check and browser handoff in one main-thread operation.
fn finish_hub_open<T>(
    prepared: T,
    validate: impl FnOnce(&T) -> std::result::Result<(), String>,
    open: impl FnOnce(&T) -> std::result::Result<(), String>,
    revoke: impl FnOnce(T),
) -> std::result::Result<(), String> {
    let result = validate(&prepared).and_then(|()| open(&prepared));
    if result.is_err() {
        revoke(prepared);
    }
    result
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
    login_enabled: bool,
    confirming: Option<&str>,
) -> Result<Menu> {
    let now = agentscm::hub::store::unix_now();
    let menu = Menu::new();
    menu.append(&PredefinedMenuItem::section_header("AgentSCM"))
        .map_err(menu_error)?;
    let monotonic_now = std::time::Instant::now();
    let unconnected = unconnected_rows(snapshot, activity, monotonic_now);
    if snapshot.connections.is_empty() && unconnected.is_empty() && notice.is_none() {
        menu.append(&native_text_item(
            "empty",
            "No connected checkouts\nRun npx agentscm in a checkout to begin.",
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
        let indicator = connection_indicator(connection, now);
        submenu
            .append(&native_text_item(
                "summary",
                &format!(
                    "{}\n{}",
                    full_menu_value(&connection.repository),
                    indicator.label()
                ),
            ))
            .map_err(menu_error)?;
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

        if let Some(mismatch) = &connection.capture_mismatch {
            submenu
                .append(&native_text_item(
                    "sharing-mismatch",
                    &share_mismatch_note(mismatch, &connection.hub),
                ))
                .map_err(menu_error)?;
        }
        let sharing_on = share_toggle_checked(connection.capture_mode);
        submenu
            .append(&CheckMenuItem::with_id(
                share_menu_id(&identity),
                "Share agent conversations with Hub",
                // After a reconnect to another Hub, sharing can only be turned off.
                sharing_on || connection.capture_mismatch.is_none(),
                sharing_on,
                None,
            ))
            .map_err(menu_error)?;
        if confirming == Some(identity.as_str()) {
            submenu
                .append(&MenuItem::with_id(
                    share_confirm_id(&identity),
                    share_confirm_label(&connection.hub),
                    true,
                    None,
                ))
                .map_err(menu_error)?;
        }

        let integrations = Submenu::new("Agent Integrations", true);
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
        }
        let terminal = Submenu::new("Copy Terminal Setup", true);
        terminal
            .append(&native_text_item(
                "note",
                "Guided setup may sign in or renew this connection.",
            ))
            .map_err(menu_error)?;
        for (agent, label) in agent_choices() {
            terminal
                .append(&MenuItem::with_id(
                    format!("copy-setup:{identity}:{}", agent.name()),
                    label,
                    true,
                    None,
                ))
                .map_err(menu_error)?;
        }
        integrations.append(&terminal).map_err(menu_error)?;
        submenu.append(&integrations).map_err(menu_error)?;

        let details = Submenu::new("Connection Details", true);
        details
            .append(&native_text_item(
                "details",
                &format!(
                    "Repository  {}\nPath  {}\nHub  {}\nBinding  {}\nCredential  {}\nAuthentication  {}",
                    full_menu_value(&connection.repository),
                    full_menu_value(&connection.root.to_string_lossy()),
                    full_menu_value(&connection.hub),
                    binding_label(&connection.binding.state),
                    credential_label(&connection.credential, now),
                    hub_label(&connection.hub_check, now),
                ),
            ))
            .map_err(menu_error)?;
        submenu.append(&details).map_err(menu_error)?;

        let recent = Submenu::new("Recent Activity", true);
        let mut recent_lines = Vec::new();
        for participation in connection
            .recent_participation
            .iter()
            .filter(|participation| participation_current(participation.expires_at, now))
        {
            recent_lines.push(format!(
                "{}: {} — {}",
                full_menu_value(&participation.agent),
                full_menu_value(&participation.state),
                full_menu_value(&participation.next_action)
            ));
        }
        if let Some(binding) = connection.binding.activity_generation.as_deref() {
            for row in activity
                .snapshot(binding, std::time::Instant::now())
                .into_iter()
                .rev()
                .take(5)
            {
                recent_lines.push(activity_label(&row));
            }
        }
        if recent_lines.is_empty() {
            recent
                .append(&native_text_item(
                    "empty",
                    "No recent AgentSCM activity observed.",
                ))
                .map_err(menu_error)?;
        } else {
            recent
                .append(&native_text_item("activity", &recent_lines.join("\n")))
                .map_err(menu_error)?;
        }
        submenu.append(&recent).map_err(menu_error)?;

        let troubleshooting = Submenu::new("Troubleshooting", true);
        let sign_in = Submenu::new("Hub Sign-in", true);
        for (provider, label) in [
            ("github", "Copy GitHub Sign-in"),
            ("gitlab", "Copy GitLab Sign-in"),
        ] {
            sign_in
                .append(&MenuItem::with_id(
                    format!("copy-login:{identity}:{provider}"),
                    label,
                    true,
                    None,
                ))
                .map_err(menu_error)?;
        }
        troubleshooting.append(&sign_in).map_err(menu_error)?;
        troubleshooting
            .append(&MenuItem::with_id(
                format!("diagnostics:{identity}"),
                "Copy Diagnostics",
                true,
                None,
            ))
            .map_err(menu_error)?;
        troubleshooting
            .append(&MenuItem::with_id(
                format!("reconnect:{identity}"),
                "Copy Reconnect",
                true,
                None,
            ))
            .map_err(menu_error)?;
        troubleshooting
            .append(&MenuItem::with_id(
                format!("guidance:{identity}"),
                "Copy Guidance Preview",
                true,
                None,
            ))
            .map_err(menu_error)?;
        submenu.append(&troubleshooting).map_err(menu_error)?;
        menu.append(&submenu).map_err(menu_error)?;
    }
    if !unconnected.is_empty() {
        menu.append(&PredefinedMenuItem::section_header("Not connected"))
            .map_err(menu_error)?;
    }
    for row in &unconnected {
        let identity = unconnected_identity(row);
        let name = row
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&row.repo);
        let submenu = Submenu::new(super::clean_label(name), true);
        submenu
            .append(&native_text_item(
                "summary",
                &format!("{}\nNot connected", full_menu_value(&row.repo)),
            ))
            .map_err(menu_error)?;
        submenu
            .append(&native_text_item(
                "details",
                &unconnected_details(
                    row,
                    monotonic_now,
                    target_hub(&row.host, &row.repo).as_deref(),
                ),
            ))
            .map_err(menu_error)?;
        submenu
            .append(&PredefinedMenuItem::separator())
            .map_err(menu_error)?;
        for agent in row.agents.iter().rev() {
            submenu
                .append(&MenuItem::with_id(
                    format!("connect:{identity}:{agent}"),
                    format!("Connect with {}", agent_label(agent)),
                    true,
                    None,
                ))
                .map_err(menu_error)?;
        }
        submenu
            .append(&MenuItem::with_id(
                format!("copy-quickstart:{identity}"),
                "Copy Terminal Setup",
                true,
                None,
            ))
            .map_err(menu_error)?;
        menu.append(&submenu).map_err(menu_error)?;
    }
    menu.append(&PredefinedMenuItem::separator())
        .map_err(menu_error)?;
    if let Some(notice) = notice {
        menu.append(&native_text_item("notice", notice))
            .map_err(menu_error)?;
    }
    menu.append(&MenuItem::with_id("refresh", "Check Now", true, None))
        .map_err(menu_error)?;
    menu.append(&CheckMenuItem::with_id(
        "launch-login",
        "Start at Login (configured)",
        true,
        login_enabled,
        None,
    ))
    .map_err(menu_error)?;
    menu.append(&MenuItem::with_id("quit", "Quit AgentSCM Tray", true, None))
        .map_err(menu_error)?;
    Ok(menu)
}

fn native_text_item(role: &str, text: &str) -> MenuItem {
    MenuItem::new(native_text_title(role, text), false, None)
}

fn native_text_title(role: &str, text: &str) -> String {
    format!(
        "__AGENTSCM_TEXT_{role}__{}",
        text.replace('\0', "").replace('&', "&&")
    )
}

fn full_menu_value(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

/// Key only the content users can see. The event loop may evaluate this every few seconds, but
/// replacing an unchanged NSMenu while AppKit is tracking it dismisses the open tray menu.
fn menu_projection_key(
    snapshot: &Snapshot,
    activity: &crate::client_activity::ActivityStore,
    notice: Option<&str>,
    login_enabled: bool,
    confirming: Option<&str>,
    now: i64,
    monotonic_now: std::time::Instant,
) -> String {
    let mut key = serde_json::to_string(&snapshot.connections).unwrap_or_default();
    key.push_str("\nconfirming:");
    key.push_str(confirming.unwrap_or_default());
    key.push_str("\nnotice:");
    key.push_str(notice.unwrap_or_default());
    key.push_str("\nlogin:");
    key.push(if login_enabled { '1' } else { '0' });
    for connection in &snapshot.connections {
        key.push_str("\nindicator:");
        key.push_str(connection_indicator(connection, now).label());
        key.push_str("\ncredential:");
        key.push_str(credential_label(&connection.credential, now));
        key.push_str("\nhub:");
        key.push_str(&hub_label(&connection.hub_check, now));
        for participation in &connection.recent_participation {
            key.push(if participation_current(participation.expires_at, now) {
                '1'
            } else {
                '0'
            });
        }
        if let Some(binding) = connection.binding.activity_generation.as_deref() {
            for row in activity
                .snapshot(binding, monotonic_now)
                .into_iter()
                .rev()
                .take(5)
            {
                key.push_str("\nactivity:");
                key.push_str(&activity_label_at(&row, monotonic_now));
            }
        }
    }
    for row in unconnected_rows(snapshot, activity, monotonic_now) {
        key.push_str("\nunconnected:");
        key.push_str(&row.repo);
        key.push_str(&unconnected_details(
            &row,
            monotonic_now,
            target_hub(&row.host, &row.repo).as_deref(),
        ));
    }
    key
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionIndicator {
    Connected,
    Attention,
    Unknown,
}

impl ConnectionIndicator {
    fn label(self) -> &'static str {
        match self {
            Self::Connected => "Hub recently reachable",
            Self::Attention => "Connection: attention needed",
            Self::Unknown => "Connection: unknown",
        }
    }
}

fn connection_indicator(
    connection: &crate::client_status::ConnectionStatus,
    now: i64,
) -> ConnectionIndicator {
    indicator_state(
        &connection.binding.state,
        &connection.credential.state,
        connection.credential.expires_at,
        &connection.hub_check,
        now,
    )
}

fn indicator_state(
    binding: &BindingState,
    credential: &CredentialState,
    credential_expires_at: Option<i64>,
    hub_check: &HubCheck,
    now: i64,
) -> ConnectionIndicator {
    let credential_expired = matches!(credential, CredentialState::Expired)
        || credential_expires_at.is_some_and(|expires_at| expires_at <= now);
    let hub_expired = matches!(
        hub_check,
        HubCheck::Valid { expires_at, .. } if *expires_at <= now
    );
    if matches!(binding, BindingState::Mismatch)
        || credential_expired
        || hub_expired
        || matches!(
            hub_check,
            HubCheck::Rejected { checked_at } if hub_check_recent(*checked_at, now)
        )
    {
        return ConnectionIndicator::Attention;
    }
    if matches!(binding, BindingState::Current)
        && matches!(credential, CredentialState::Available)
        && hub_recently_verified(hub_check, now)
    {
        return ConnectionIndicator::Connected;
    }
    ConnectionIndicator::Unknown
}

fn hub_recently_verified(check: &HubCheck, now: i64) -> bool {
    matches!(
        check,
        HubCheck::Valid {
            checked_at,
            expires_at,
            ..
        } if *expires_at > now && hub_check_recent(*checked_at, now)
    )
}

fn hub_check_recent(checked_at: i64, now: i64) -> bool {
    checked_at <= now && now.saturating_sub(checked_at) <= RECENT_HUB_CHECK_SECONDS
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
    activity_label_at(row, std::time::Instant::now())
}

fn activity_label_at(
    row: &crate::client_activity::ActivityObservation,
    now: std::time::Instant,
) -> String {
    let elapsed = now.saturating_duration_since(row.observed_at);
    let age = if elapsed < Duration::from_secs(60) {
        "less than 1m ago".into()
    } else {
        format!("{}m ago", elapsed.as_secs() / 60)
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

fn handle_action(id: &str, snapshot: &Snapshot, executable: &Path) {
    if let Some(command) = terminal_action_command(id, snapshot, executable) {
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
        "diagnostics" => copy_text(&diagnostics(connection)),
        "reconnect" => copy_text(&reconnect_command(connection, executable)),
        "guidance" => copy_text(&guidance_command(connection, executable)),
        _ => {}
    }
}

fn terminal_action_command(id: &str, snapshot: &Snapshot, executable: &Path) -> Option<String> {
    let mut parts = id.split(':');
    let kind = parts.next()?;
    let identity = parts.next()?;
    let agent = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let connection = snapshot
        .connections
        .iter()
        .find(|connection| connection_identity(connection) == identity)?;
    if kind == "copy-login" {
        return hub_login_command_for(executable, agent, &connection.hub);
    }
    (kind == "copy-setup" && agent_choices().iter().any(|(item, _)| item.name() == agent))
        .then(|| terminal_setup_command_for(executable, agent, &connection.hub, &connection.root))
}

fn hub_login_command_for(executable: &Path, provider: &str, hub: &str) -> Option<String> {
    matches!(provider, "github" | "gitlab").then(|| {
        format!(
            "{} hub login --hub {} --provider {}",
            shell_quote(&executable.to_string_lossy()),
            shell_quote(hub),
            provider,
        )
    })
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
fn reconnect_command(
    connection: &crate::client_status::ConnectionStatus,
    executable: &Path,
) -> String {
    format!(
        "{} client connect --hub {} --repo {} --root {}",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(&connection.hub),
        shell_quote(&connection.repository),
        shell_quote(&connection.root.to_string_lossy())
    )
}
fn guidance_command(
    connection: &crate::client_status::ConnectionStatus,
    executable: &Path,
) -> String {
    format!(
        "cd {} && {} init-repo --hub-url {}",
        shell_quote(&connection.root.to_string_lossy()),
        shell_quote(&executable.to_string_lossy()),
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
        crate::hub_cli::config_dir()
            .unwrap_or_default()
            .join("agentscm/change-capture.json"),
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
    fn copied_hub_login_uses_the_exact_hub_and_explicit_provider() {
        let executable = Path::new("/Applications/AgentSCM versions/agentscm");
        assert_eq!(hub_login_command_for(executable, "github", "https://hub.example/a'b"),
            Some("'/Applications/AgentSCM versions/agentscm' hub login --hub 'https://hub.example/a'\\''b' --provider github".into()));
        assert!(
            hub_login_command_for(executable, "gitlab", "https://hub.example")
                .unwrap()
                .ends_with("--provider gitlab")
        );
        assert!(
            hub_login_command_for(executable, "github; injected", "https://hub.example").is_none()
        );
    }

    #[test]
    fn changed_hub_binding_never_reaches_the_browser_and_is_revoked() {
        let revoked = std::cell::Cell::new(false);
        let result = finish_hub_open(
            "prepared credential",
            |_| Err("binding changed".into()),
            |_| panic!("a stale credential must never reach the opener"),
            |_| revoked.set(true),
        );
        assert_eq!(result, Err("binding changed".into()));
        assert!(revoked.get());
    }

    #[test]
    fn failed_browser_handoff_revokes_after_validation() {
        let calls = std::cell::RefCell::new(Vec::new());
        let result = finish_hub_open(
            (),
            |_| {
                calls.borrow_mut().push("validate");
                Ok(())
            },
            |_| {
                calls.borrow_mut().push("open");
                Err("no browser".into())
            },
            |_| calls.borrow_mut().push("revoke"),
        );
        assert_eq!(result, Err("no browser".into()));
        assert_eq!(*calls.borrow(), ["validate", "open", "revoke"]);
    }

    #[test]
    fn successful_browser_handoff_keeps_the_validated_token() {
        let validated = std::cell::Cell::new(false);
        finish_hub_open(
            (),
            |_| {
                validated.set(true);
                Ok(())
            },
            |_| {
                assert!(validated.get());
                Ok(())
            },
            |_| panic!("the browser now owns this token"),
        )
        .unwrap();
    }

    fn share_connection(
        root: &str,
        mode: Option<CaptureModeV1>,
    ) -> crate::client_status::ConnectionStatus {
        crate::client_status::ConnectionStatus {
            root: root.into(),
            repository: "example/project".into(),
            hub: "https://hub.example".into(),
            agents: vec![],
            binding: crate::client_status::BindingStatus {
                state: BindingState::Current,
                generation: Some("generation".into()),
                activity_generation: None,
            },
            credential: crate::client_status::CredentialStatus {
                kind: crate::client_status::CredentialKind::External,
                state: CredentialState::Available,
                expires_at: None,
            },
            hub_check: HubCheck::Skipped,
            recent_participation: vec![],
            capture_mode: mode,
            capture_mismatch: None,
        }
    }

    #[test]
    fn share_toggle_flips_the_mode_and_a_click_without_a_binding_turns_sharing_on() {
        let snapshot = Snapshot {
            version: 1,
            snapshot_at: 0,
            connections: vec![
                share_connection("/synthetic/off", Some(CaptureModeV1::LocalConversation)),
                share_connection("/synthetic/on", Some(CaptureModeV1::SharedConversation)),
                share_connection("/synthetic/unbound", None),
            ],
        };
        let id = |index: usize| share_menu_id(&connection_identity(&snapshot.connections[index]));
        let off = share_request(&id(0), &snapshot).unwrap();
        assert_eq!(off.desired, CaptureModeV1::SharedConversation);
        assert_eq!(off.label, "off");
        assert_eq!(off.root, Path::new("/synthetic/off"));
        let on = share_request(&id(1), &snapshot).unwrap();
        assert_eq!(on.desired, CaptureModeV1::MetadataOnly);
        let unbound = share_request(&id(2), &snapshot).unwrap();
        assert_eq!(unbound.desired, CaptureModeV1::SharedConversation);
        assert_eq!(unbound.hub, "https://hub.example");
        assert!(share_request("share-conv:unknown", &snapshot).is_none());
        assert!(share_request("hub:unknown", &snapshot).is_none());

        assert!(!share_toggle_checked(None));
        assert!(!share_toggle_checked(Some(CaptureModeV1::MetadataOnly)));
        assert!(share_toggle_checked(Some(
            CaptureModeV1::SharedConversation
        )));
    }

    #[test]
    fn turning_sharing_on_takes_two_clicks_and_the_first_never_changes_anything() {
        let mut snapshot = Snapshot {
            version: 1,
            snapshot_at: 0,
            connections: vec![
                share_connection("/synthetic/unbound", None),
                share_connection("/synthetic/on", Some(CaptureModeV1::SharedConversation)),
            ],
        };
        let identity =
            |snapshot: &Snapshot, index: usize| connection_identity(&snapshot.connections[index]);
        let toggle = share_menu_id(&identity(&snapshot, 0));
        let confirm = share_confirm_id(&identity(&snapshot, 0));

        // The toggle only asks, and says what sharing does and which item confirms it.
        let now = std::time::Instant::now();
        let ShareStep::AskConfirmation(asked) = share_step(&toggle, &snapshot, None, now) else {
            panic!("the first click must only ask");
        };
        let pending = PendingShare {
            request: asked.clone(),
            asked_at: now,
        };
        let (title, body) = share_confirmation(&asked);
        assert_eq!(title, "Share conversations from unbound?");
        assert!(body.contains(&share_consequences("https://hub.example")));
        assert!(body.contains(&share_confirm_label("https://hub.example")));
        assert!(body.contains("Nothing is shared yet"));
        assert_eq!(
            share_confirm_label("https://hub.example"),
            "Confirm: share conversations with https://hub.example"
        );
        // Clicking the toggle again asks again; it never turns anything on.
        assert!(matches!(
            share_step(&toggle, &snapshot, Some(&pending), now),
            ShareStep::AskConfirmation(_)
        ));

        // The confirm item alone changes the setting, and only after the person was asked.
        assert!(matches!(
            share_step(&confirm, &snapshot, None, now),
            ShareStep::Unavailable
        ));
        let ShareStep::Change(request) = share_step(&confirm, &snapshot, Some(&pending), now)
        else {
            panic!("the confirm item must turn sharing on");
        };
        assert_eq!(request.desired, CaptureModeV1::SharedConversation);
        assert_eq!(request.repository, "example/project");
        assert_eq!(request.hub, "https://hub.example");

        // A confirmation asked for another checkout does not confirm this one.
        let other = share_request(&share_menu_id(&identity(&snapshot, 1)), &snapshot).unwrap();
        let other_pending = PendingShare {
            request: other,
            asked_at: now,
        };
        assert!(matches!(
            share_step(&confirm, &snapshot, Some(&other_pending), now),
            ShareStep::Unavailable
        ));

        // Turning sharing off needs no confirmation.
        let off = share_menu_id(&identity(&snapshot, 1));
        assert!(matches!(
            share_step(&off, &snapshot, None, now),
            ShareStep::Change(request) if request.desired == CaptureModeV1::MetadataOnly
        ));

        // The menu shows the confirm item only while the asked checkout is unchanged: another Hub
        // is another identity, and a checkout that now shares has nothing left to confirm.
        assert!(pending_share_current(&pending, &snapshot, now));
        snapshot.connections[0].hub = "https://other.example".into();
        assert!(!pending_share_current(&pending, &snapshot, now));
        assert!(matches!(
            share_step(&confirm, &snapshot, Some(&pending), now),
            ShareStep::Unavailable
        ));
        snapshot.connections[0].hub = "https://hub.example".into();
        snapshot.connections[0].capture_mode = Some(CaptureModeV1::SharedConversation);
        assert!(!pending_share_current(&pending, &snapshot, now));

        // The confirmation lapses after two minutes: the menu drops it and the item does nothing.
        snapshot.connections[0].capture_mode = None;
        let late = now + SHARE_CONFIRM_TTL;
        assert!(pending_share_current(
            &pending,
            &snapshot,
            late - Duration::from_secs(1)
        ));
        assert!(!pending_share_current(&pending, &snapshot, late));
        assert!(matches!(
            share_step(&confirm, &snapshot, Some(&pending), late),
            ShareStep::Unavailable
        ));
        // Asking again after it lapsed starts a fresh window.
        assert!(matches!(
            share_step(&toggle, &snapshot, Some(&pending), late),
            ShareStep::AskConfirmation(_)
        ));
        snapshot.connections[0].capture_mode = Some(CaptureModeV1::SharedConversation);

        let activity = crate::client_activity::ActivityStore::default();
        let now = std::time::Instant::now();
        assert_ne!(
            menu_projection_key(&snapshot, &activity, None, false, None, 100, now),
            menu_projection_key(&snapshot, &activity, None, false, Some("x"), 100, now)
        );
    }

    #[test]
    fn a_reconnected_checkout_explains_the_saved_hub_and_can_only_turn_sharing_off() {
        let mut connection =
            share_connection("/synthetic/moved", Some(CaptureModeV1::SharedConversation));
        connection.capture_mismatch = Some(crate::client_status::CaptureMismatch {
            repository: "example/project".into(),
            hub: "https://old.example".into(),
        });
        let note = share_mismatch_note(
            connection.capture_mismatch.as_ref().unwrap(),
            &connection.hub,
        );
        assert!(note.starts_with(
            "Sharing was set for https://old.example; this checkout is now connected to https://hub.example."
        ));
        assert!(note.contains("change-capture forget"));
        let snapshot = Snapshot {
            version: 1,
            snapshot_at: 0,
            connections: vec![connection],
        };
        let id = share_menu_id(&connection_identity(&snapshot.connections[0]));
        assert!(matches!(
            share_step(&id, &snapshot, None, std::time::Instant::now()),
            ShareStep::Change(request) if request.desired == CaptureModeV1::MetadataOnly
        ));
        let activity = crate::client_activity::ActivityStore::default();
        let now = std::time::Instant::now();
        let mut snapshot = snapshot;
        let with = menu_projection_key(&snapshot, &activity, None, false, None, 100, now);
        snapshot.connections[0].capture_mismatch = None;
        assert_ne!(
            with,
            menu_projection_key(&snapshot, &activity, None, false, None, 100, now)
        );
    }

    #[test]
    fn share_outcome_states_what_the_change_does_and_does_not_do() {
        let mut request = ShareRequest {
            identity: "x".into(),
            root: "/synthetic/project".into(),
            repository: "example/project".into(),
            hub: "https://hub.example.test".into(),
            label: "project".into(),
            desired: CaptureModeV1::SharedConversation,
        };
        let (_, body) = share_outcome(&request, &Ok(()));
        assert!(body.starts_with("Sharing on for project."));
        assert!(body.contains("redacted on this computer"));
        assert!(body.contains("uploaded to https://hub.example.test"));
        assert!(body.contains("30 days") && body.contains("Hub admins"));
        assert!(share_progress(&request).contains("30 days"));
        request.desired = CaptureModeV1::MetadataOnly;
        let (_, body) = share_outcome(&request, &Ok(()));
        assert!(body.starts_with("Sharing off for project:"));
        assert!(body.contains("no longer uploaded") && body.contains("remain until"));
        assert!(!share_progress(&request).contains("uploaded"));
        let (title, body) = share_outcome(
            &request,
            &Err("no client connection for this checkout".into()),
        );
        assert_eq!(title, "Could not change conversation sharing");
        assert_eq!(body, "no client connection for this checkout");
    }

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
    fn connect_prefers_the_hub_the_repository_already_uses() {
        let saved =
            |root: &str, host: &str, repo: &str, hub: &str| crate::client_config::Connection {
                root: root.into(),
                origin_host: host.into(),
                hub: hub.into(),
                repo: repo.into(),
                token_file: "/synthetic/token".into(),
                report_completion: false,
                owned_token_id: None,
                owned_token_expires_at: None,
                agents: Vec::new(),
            };
        let connections = [
            saved(
                "/synthetic/a",
                "gitlab.com",
                "acme/widgets",
                "https://other.example.test",
            ),
            saved(
                "/synthetic/b",
                "github.com",
                "Acme/Widgets",
                "https://team.example.test",
            ),
            saved(
                "/synthetic/c",
                "github.com",
                "acme/widgets",
                "https://stale.example.test",
            ),
        ];
        let signed_in = |hubs: &'static [&'static str]| {
            move |hub: Option<&str>| match hub {
                Some(hub) => hubs.contains(&hub).then(|| hub.to_owned()),
                None => Some("https://default.example.test".to_owned()),
            }
        };
        assert_eq!(
            connect_hub(
                "github.com",
                "acme/widgets",
                &connections,
                signed_in(&["https://team.example.test"])
            ),
            Some("https://team.example.test".to_owned()),
            "same forge repository, case-insensitive"
        );
        assert_eq!(
            connect_hub("github.com", "acme/widgets", &connections, signed_in(&[])),
            Some("https://default.example.test".to_owned()),
            "no sign-in for that Hub any more"
        );
        assert_eq!(
            connect_hub(
                "github.com",
                "acme/other",
                &connections,
                signed_in(&["https://team.example.test"])
            ),
            Some("https://default.example.test".to_owned()),
            "another repository"
        );
        assert_eq!(connect_hub("github.com", "acme/other", &[], |_| None), None);
    }

    #[test]
    fn unconnected_rows_offer_seen_agents_until_the_checkout_is_connected() {
        let now = std::time::Instant::now();
        let mut activity = crate::client_activity::ActivityStore::default();
        for agent in ["claude", "codex"] {
            activity.push_unconnected(crate::client_activity::UnconnectedCheckout {
                root: "/synthetic/widgets".into(),
                host: "github.com".into(),
                repo: "acme/widgets".into(),
                agents: vec![agent.into()],
                seen_at: now,
            });
        }
        let mut snapshot = Snapshot {
            version: 1,
            snapshot_at: 0,
            connections: Vec::new(),
        };
        let rows = unconnected_rows(&snapshot, &activity, now);
        assert_eq!(rows.len(), 1);
        let identity = unconnected_identity(&rows[0]);
        let request = connect_request(
            &format!("connect:{identity}:codex"),
            &snapshot,
            &activity,
            now,
        )
        .unwrap();
        assert_eq!(request.agent, crate::client_setup::Agent::Codex);
        assert_eq!(request.root, Path::new("/synthetic/widgets"));
        for id in [
            format!("connect:{identity}:gemini"),
            format!("connect:{identity}:codex:extra"),
            "connect:unknown:codex".to_owned(),
        ] {
            assert!(
                connect_request(&id, &snapshot, &activity, now).is_none(),
                "{id}"
            );
        }
        assert_eq!(
            quickstart_command_for(Path::new("/opt/agent scm"), &rows[0]),
            "cd '/synthetic/widgets' && '/opt/agent scm' quickstart --agent 'codex'"
        );
        let projected = menu_projection_key(&snapshot, &activity, None, false, None, 100, now);
        assert!(projected.contains("unconnected:acme/widgets"));

        snapshot
            .connections
            .push(crate::client_status::ConnectionStatus {
                root: "/synthetic/widgets".into(),
                repository: "acme/widgets".into(),
                hub: "https://hub.example.test".into(),
                agents: vec!["codex".into()],
                binding: crate::client_status::BindingStatus {
                    state: BindingState::Current,
                    generation: None,
                    activity_generation: None,
                },
                credential: crate::client_status::CredentialStatus {
                    kind: crate::client_status::CredentialKind::Managed,
                    state: CredentialState::Available,
                    expires_at: None,
                },
                hub_check: HubCheck::Skipped,
                recent_participation: vec![],
                capture_mode: None,
                capture_mismatch: None,
            });
        assert!(unconnected_rows(&snapshot, &activity, now).is_empty());
        assert!(
            connect_request(
                &format!("connect:{identity}:codex"),
                &snapshot,
                &activity,
                now
            )
            .is_none()
        );
    }

    #[test]
    fn copied_recovery_commands_work_without_path_and_keep_arguments_literal() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("AgentSCM 'current;$(touch injected)");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$(pwd -P)\" \"$@\" > \"$CAPTURE\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = temp.path().join("checkout 'quote';$(touch injected)");
        std::fs::create_dir(&root).unwrap();
        let connection = crate::client_status::ConnectionStatus {
            root: root.clone(),
            repository: "example/project".into(),
            hub: "https://hub.example/a'b;$(touch injected)".into(),
            agents: vec![],
            binding: crate::client_status::BindingStatus {
                state: BindingState::Current,
                generation: None,
                activity_generation: None,
            },
            credential: crate::client_status::CredentialStatus {
                kind: crate::client_status::CredentialKind::External,
                state: CredentialState::Available,
                expires_at: None,
            },
            hub_check: HubCheck::Skipped,
            recent_participation: vec![],
            capture_mode: None,
            capture_mismatch: None,
        };
        let capture = temp.path().join("arguments");
        for (command, directory, arguments) in [
            (
                reconnect_command(&connection, &executable),
                temp.path(),
                vec![
                    "client",
                    "connect",
                    "--hub",
                    &connection.hub,
                    "--repo",
                    &connection.repository,
                    "--root",
                    root.to_str().unwrap(),
                ],
            ),
            (
                guidance_command(&connection, &executable),
                root.as_path(),
                vec!["init-repo", "--hub-url", &connection.hub],
            ),
        ] {
            let output = Command::new("/bin/sh")
                .args(["-c", &command])
                .current_dir(temp.path())
                .env_clear()
                .env("PATH", "")
                .env("CAPTURE", &capture)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            let directory = directory.canonicalize().unwrap();
            let expected = std::iter::once(directory.to_str().unwrap())
                .chain(arguments)
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(std::fs::read_to_string(&capture).unwrap(), expected + "\n");
        }
        assert!(!temp.path().join("injected").exists());
        assert!(!root.join("injected").exists());
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
    fn unchanged_projection_does_not_request_a_native_menu_swap() {
        let mut snapshot = Snapshot {
            version: 1,
            snapshot_at: 42,
            connections: Vec::new(),
        };
        let activity = crate::client_activity::ActivityStore::default();
        let monotonic = std::time::Instant::now();
        let first = menu_projection_key(&snapshot, &activity, None, false, None, 100, monotonic);
        snapshot.snapshot_at = 47;
        let periodic = menu_projection_key(
            &snapshot,
            &activity,
            None,
            false,
            None,
            105,
            monotonic + Duration::from_secs(5),
        );
        assert_eq!(first, periodic);
        assert_ne!(
            first,
            menu_projection_key(
                &snapshot,
                &activity,
                Some("Checking…"),
                false,
                None,
                105,
                monotonic,
            )
        );
    }

    #[test]
    fn login_configuration_participates_in_empty_menu_projection() {
        let snapshot = Snapshot {
            version: 1,
            snapshot_at: 42,
            connections: Vec::new(),
        };
        let activity = crate::client_activity::ActivityStore::default();
        let monotonic = std::time::Instant::now();
        assert_ne!(
            menu_projection_key(&snapshot, &activity, None, false, None, 100, monotonic),
            menu_projection_key(&snapshot, &activity, None, true, None, 100, monotonic),
        );
    }

    #[test]
    fn explicit_notice_survives_tracking_until_catch_up_replacement() {
        let mut persistent = None;
        update_refresh_notice(&mut persistent, Some("Check failed: unavailable"));
        let notice = projected_notice(None, persistent.as_ref().map(Notice::as_str), false, true);
        assert_eq!(
            menu_swap_decision("old", notice.unwrap(), true),
            MenuSwapDecision::Defer
        );

        update_refresh_notice(&mut persistent, None);
        let catch_up = projected_notice(None, persistent.as_ref().map(Notice::as_str), false, true);
        assert_eq!(catch_up, Some("Check failed: unavailable"));
        assert_eq!(
            menu_swap_decision("old", catch_up.unwrap(), false),
            MenuSwapDecision::Replace
        );
        clear_refresh_notice(&mut persistent);
        assert_eq!(
            projected_notice(None, persistent.as_ref().map(Notice::as_str), false, true,),
            None
        );
    }

    #[test]
    fn successful_refresh_preserves_action_results_only() {
        for message in [
            "Integration setup failed: client unavailable",
            "Launch at login enabled for the next login",
        ] {
            let mut notice = Some(Notice::Action(message.into()));
            clear_refresh_notice(&mut notice);
            assert_eq!(notice.as_ref().map(Notice::as_str), Some(message));
        }

        let mut notice = Some(Notice::Refresh("Check failed: unavailable".into()));
        clear_refresh_notice(&mut notice);
        assert_eq!(notice, None);
    }

    #[test]
    fn changed_menu_defers_while_open_and_replaces_after_close() {
        assert_eq!(
            menu_swap_decision("old", "old", true),
            MenuSwapDecision::Unchanged
        );
        assert_eq!(
            menu_swap_decision("old", "new", true),
            MenuSwapDecision::Defer
        );
        assert_eq!(
            menu_swap_decision("old", "new", false),
            MenuSwapDecision::Replace
        );
    }

    #[test]
    fn connection_indicators_keep_unknown_neutral() {
        assert_eq!(
            ConnectionIndicator::Connected.label(),
            "Hub recently reachable"
        );
        assert_eq!(
            ConnectionIndicator::Attention.label(),
            "Connection: attention needed"
        );
        assert_eq!(ConnectionIndicator::Unknown.label(), "Connection: unknown");
    }

    #[test]
    fn connection_detail_values_keep_full_printable_text() {
        let value = "https://hub.example.test/a/very/long/checkout/value?one=two\nignored";
        assert_eq!(
            full_menu_value(value),
            "https://hub.example.test/a/very/long/checkout/value?one=twoignored"
        );
        assert_eq!(
            native_text_title("details", "A&B"),
            "__AGENTSCM_TEXT_details__A&&B"
        );
    }

    #[test]
    fn hub_reachability_indicator_has_a_bounded_freshness_window() {
        let check = |checked_at, expires_at| HubCheck::Valid {
            checked_at,
            provider: "github".into(),
            host: "github.com".into(),
            login: "member".into(),
            expires_at,
        };
        assert!(hub_recently_verified(&check(40, 200), 100));
        assert!(!hub_recently_verified(&check(39, 200), 100));
        assert!(!hub_recently_verified(&check(101, 200), 100));
        assert!(!hub_recently_verified(&check(40, 100), 100));
        assert_eq!(
            indicator_state(
                &BindingState::Current,
                &CredentialState::Available,
                Some(200),
                &check(39, 200),
                100,
            ),
            ConnectionIndicator::Unknown
        );
        assert_eq!(
            indicator_state(
                &BindingState::Current,
                &CredentialState::Available,
                Some(200),
                &check(39, 100),
                100,
            ),
            ConnectionIndicator::Attention
        );
        let rejected = |checked_at| HubCheck::Rejected { checked_at };
        assert_eq!(
            indicator_state(
                &BindingState::Current,
                &CredentialState::Available,
                Some(200),
                &rejected(40),
                100,
            ),
            ConnectionIndicator::Attention
        );
        assert_eq!(
            indicator_state(
                &BindingState::Current,
                &CredentialState::Available,
                Some(200),
                &rejected(39),
                100,
            ),
            ConnectionIndicator::Unknown
        );
        assert_eq!(
            indicator_state(
                &BindingState::Current,
                &CredentialState::Available,
                Some(200),
                &rejected(101),
                100,
            ),
            ConnectionIndicator::Unknown
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
