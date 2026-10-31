//! The macOS tray's primary, read-only work surface and quick prompt.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use objc2::rc::Retained;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSApplication,
    NSBackingStoreType, NSBezelStyle, NSBox, NSBoxType, NSButton, NSButtonType, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSFloatingWindowLevel, NSFont, NSImage,
    NSImageView, NSLineBreakMode, NSNormalWindowLevel, NSPanel, NSScreen, NSScrollView, NSSwitch,
    NSTextField, NSView, NSWindowStyleMask,
};
use objc2_foundation::{NSData, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use winit::event_loop::EventLoopProxy;

use super::UserEvent;

const WIDTH: f64 = 620.0;
const HEIGHT: f64 = 780.0;
const REPOSITORY_LIMIT: usize = 50;
const HUB_ACTIVITY_LIMIT: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RepositoryRow {
    pub name: String,
    pub hub: Option<String>,
    pub local_agents: String,
    pub hub_state: String,
    pub attention: usize,
    pub work_note: String,
    pub local: Vec<LocalAgentRow>,
    pub work: Vec<WorkRow>,
    pub overflow: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LocalAgentRow {
    pub agent: String,
    pub detail: String,
    pub observed: bool,
    pub needs_you: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WorkRow {
    pub agent: String,
    pub title: String,
    pub detail: String,
    pub needs_you: bool,
}

#[derive(Debug)]
struct TargetIvars {
    proxy: EventLoopProxy<UserEvent>,
    prompt: Retained<NSTextField>,
    checkout_label: Retained<NSTextField>,
    agent_label: Retained<NSTextField>,
    option_label: Retained<NSTextField>,
    checkouts: RefCell<Vec<String>>,
    agents: RefCell<Vec<String>>,
    options: RefCell<Vec<String>>,
    repository_links: RefCell<Vec<Option<(String, String)>>>,
    tabs: RefCell<Vec<String>>,
    checkout_index: Cell<usize>,
    agent_index: Cell<usize>,
    option_index: Cell<usize>,
}

objc2::define_class!(
    #[name = "AgentSCMCompanionPanelTarget"]
    #[unsafe(super = objc2_foundation::NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetIvars]
    struct Target;

    unsafe impl NSObjectProtocol for Target {}

    impl Target {
        #[unsafe(method(submitPrompt:))]
        fn submit_prompt(&self, _sender: &NSButton) {
            let ivars = self.ivars();
            let prompt = ivars.prompt.stringValue().to_string();
            if prompt.trim().is_empty() {
                return;
            }
            let _ = ivars.proxy.send_event(UserEvent::CompanionSubmit {
                prompt,
                checkout_index: ivars.checkout_index.get(),
                agent_index: ivars.agent_index.get(),
                option_index: ivars.option_index.get(),
            });
        }

        #[unsafe(method(nextCheckout:))]
        fn next_checkout(&self, _sender: &NSButton) {
            let ivars = self.ivars();
            let options = ivars.checkouts.borrow();
            if options.is_empty() {
                return;
            }
            let index = (ivars.checkout_index.get() + 1) % options.len();
            ivars.checkout_index.set(index);
            ivars.checkout_label
                .setStringValue(&NSString::from_str(&format!("Checkout: {}", options[index])));
            let _ = ivars.proxy.send_event(UserEvent::CompanionSelection(index));
        }

        #[unsafe(method(nextAgent:))]
        fn next_agent(&self, _sender: &NSButton) {
            let ivars = self.ivars();
            let options = ivars.agents.borrow();
            if options.is_empty() {
                return;
            }
            let index = (ivars.agent_index.get() + 1) % options.len();
            ivars.agent_index.set(index);
            ivars.agent_label
                .setStringValue(&NSString::from_str(&format!("Agent: {}", options[index])));
            ivars.option_index.set(0);
            if let Some(default) = ivars.options.borrow().first() {
                ivars.option_label
                    .setStringValue(&NSString::from_str(&format!("Mode: {default}")));
            }
            ivars.prompt.setPlaceholderString(Some(&NSString::from_str(&format!(
                "What should {} do?",
                options[index]
            ))));
        }

        #[unsafe(method(nextOption:))]
        fn next_option(&self, _sender: &NSButton) {
            let ivars = self.ivars();
            let options = ivars.options.borrow();
            if options.is_empty() {
                return;
            }
            let allowed = if ivars.agent_index.get() == 0 { 1 } else { 2 };
            let index = if ivars.option_index.get() == 0 {
                allowed.min(options.len() - 1)
            } else {
                0
            };
            ivars.option_index.set(index);
            ivars.option_label
                .setStringValue(&NSString::from_str(&format!("Mode: {}", options[index])));
        }

        #[unsafe(method(pinChanged:))]
        fn pin_changed(&self, sender: &NSSwitch) {
            let _ = self
                .ivars()
                .proxy
                .send_event(UserEvent::CompanionPin(sender.state() == NSControlStateValueOn));
        }

        #[unsafe(method(pinButtonChanged:))]
        fn pin_button_changed(&self, sender: &NSButton) {
            let _ = self
                .ivars()
                .proxy
                .send_event(UserEvent::CompanionPin(sender.state() == NSControlStateValueOn));
        }

        #[unsafe(method(openSelectedHub:))]
        fn open_selected_hub(&self, _sender: &NSButton) {
            if let Some((hub, repository)) = self
                .ivars()
                .repository_links
                .borrow()
                .iter()
                .flatten()
                .next()
            {
                let _ = self.ivars().proxy.send_event(UserEvent::CompanionOpenRepository {
                    hub: hub.clone(),
                    repository: repository.clone(),
                });
            } else {
                let _ = self.ivars().proxy.send_event(UserEvent::CompanionOpenHub(
                    self.ivars().checkout_index.get(),
                ));
            }
        }

        #[unsafe(method(openRepository:))]
        fn open_repository(&self, sender: &NSButton) {
            let Ok(index) = usize::try_from(sender.tag()) else {
                return;
            };
            if let Some(Some((hub, repository))) = self.ivars().repository_links.borrow().get(index) {
                let _ = self.ivars().proxy.send_event(UserEvent::CompanionOpenRepository {
                    hub: hub.clone(),
                    repository: repository.clone(),
                });
            }
        }

        #[unsafe(method(selectTab:))]
        fn select_tab(&self, sender: &NSButton) {
            let Ok(index) = usize::try_from(sender.tag()) else {
                return;
            };
            if index < self.ivars().tabs.borrow().len() {
                let _ = self.ivars().proxy.send_event(UserEvent::CompanionTab(index));
            }
        }

        #[unsafe(method(expandRepository:))]
        fn expand_repository(&self, sender: &NSButton) {
            if let Ok(index) = usize::try_from(sender.tag()) {
                let _ = self
                    .ivars()
                    .proxy
                    .send_event(UserEvent::CompanionExpandRepository(index));
            }
        }

        #[unsafe(method(showMoreRepositories:))]
        fn show_more_repositories(&self, _sender: &NSButton) {
            let _ = self
                .ivars()
                .proxy
                .send_event(UserEvent::CompanionShowMoreRepositories);
        }
    }
);

impl Target {
    fn new(mtm: MainThreadMarker, ivars: TargetIvars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSObject's initializer has this signature and Target has no extra init work.
        unsafe { msg_send![super(this), init] }
    }
}

/// Reusable, nonmodal panel. All methods must run on AppKit's main thread.
pub(super) struct CompanionPanel {
    panel: Retained<NSPanel>,
    panel_scroll: Retained<NSScrollView>,
    target: Retained<Target>,
    status: Retained<NSTextField>,
    list_title: Retained<NSTextField>,
    attention_title: Retained<NSTextField>,
    attention: Retained<NSTextField>,
    work_scroll: Retained<NSScrollView>,
    now_group: Retained<NSView>,
    list_group: Retained<NSView>,
    prompt_group: Retained<NSView>,
    now_actions: [Retained<NSButton>; 2],
    action_backgrounds: [Retained<NSBox>; 2],
    footer_card: Retained<NSBox>,
    footer_title: Retained<NSTextField>,
    footer_hint: Retained<NSTextField>,
    tab_buttons: [Retained<NSButton>; 3],
    tab_backgrounds: [Retained<NSBox>; 3],
    tab_marker: Retained<NSBox>,
    tab: usize,
    all_rows: Vec<RepositoryRow>,
    rendered_rows: Vec<RepositoryRow>,
    rendered_omitted: usize,
    expanded: HashSet<(Option<String>, String)>,
    visible_limit: usize,
    prompt: Retained<NSTextField>,
    checkout_label: Retained<NSTextField>,
    agent_label: Retained<NSTextField>,
    option_label: Retained<NSTextField>,
    pin: PinControl,
    pinned: bool,
}

enum PinControl {
    Switch(Retained<NSSwitch>),
    Button(Retained<NSButton>),
}

impl CompanionPanel {
    pub(super) fn new(proxy: EventLoopProxy<UserEvent>) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let panel_height = NSScreen::mainScreen(mtm)
            .as_ref()
            .map_or(HEIGHT, |screen| fitted_height(screen));
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect(0.0, 0.0, WIDTH, panel_height),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: this struct owns the panel after a user closes its window.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setTitle(&NSString::from_str("AgentSCM"));
        panel.setHidesOnDeactivate(false);
        // The authored surfaces use a fixed light palette; keep semantic controls in that palette.
        let appearance = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameAqua })?;
        panel.setAppearance(Some(&appearance));
        panel.setBackgroundColor(Some(&color(0xfafafc)));

        let content = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, WIDTH, HEIGHT));
        let now_group = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 530.0, WIDTH, 82.0));
        let list_group = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, WIDTH, HEIGHT));
        let prompt_group = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, WIDTH, HEIGHT));
        let header = surface(mtm, rect(14.0, 676.0, WIDTH - 28.0, 90.0), 0x17142e, 20.0);
        let logo_bytes = include_bytes!("../../docs/assets/agentscm-icon.png");
        // SAFETY: include_bytes! provides a valid pointer for the duration of the copy.
        let logo_data =
            unsafe { NSData::dataWithBytes_length(logo_bytes.as_ptr().cast(), logo_bytes.len()) };
        let logo_image = NSImage::initWithData(NSImage::alloc(), &logo_data)?;
        let logo =
            NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(37.0, 704.0, 45.0, 45.0));
        logo.setImage(Some(&logo_image));
        logo.setAccessibilityLabel(Some(&NSString::from_str("AgentSCM logo")));
        let brand = label(mtm, "AgentSCM", 21.0, true);
        brand.setFrame(rect(100.0, 724.0, 280.0, 27.0));
        brand.setTextColor(Some(&color(0xf3f0ff)));
        let tagline = label(mtm, "Agents and work across your repositories", 12.0, false);
        tagline.setFrame(rect(101.0, 702.0, 360.0, 18.0));
        tagline.setTextColor(Some(&color(0xc2bce1)));
        let pin = if objc2::runtime::AnyClass::get(c"NSSwitch").is_some() {
            let control = NSSwitch::initWithFrame(
                NSSwitch::alloc(mtm),
                rect(WIDTH - 85.0, 685.0, 42.0, 22.0),
            );
            control.setAccessibilityLabel(Some(&NSString::from_str("Keep panel on top")));
            PinControl::Switch(control)
        } else {
            let control = button(mtm, "", rect(WIDTH - 85.0, 685.0, 42.0, 22.0));
            control.setButtonType(NSButtonType::Switch);
            control.setAccessibilityLabel(Some(&NSString::from_str("Keep panel on top")));
            PinControl::Button(control)
        };
        let pin_label = label(mtm, "PIN", 10.0, true);
        pin_label.setFrame(rect(WIDTH - 115.0, 689.0, 30.0, 14.0));
        pin_label.setTextColor(Some(&color(0xc2bce1)));

        let header_rule = surface(mtm, rect(34.0, 695.0, WIDTH - 68.0, 1.0), 0x3c3658, 0.0);
        let header_note = label(mtm, "Hub refreshes every 1–2 minutes", 12.0, false);
        header_note.setFrame(rect(35.0, 679.0, 330.0, 16.0));
        header_note.setTextColor(Some(&color(0xe2def3)));

        let tab_buttons = ["Work", "Needs you", "Start"].map(|title| {
            let control = button(mtm, title, rect(0.0, 0.0, 1.0, 1.0));
            control.setBordered(false);
            control.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
            control
        });
        let tab_backgrounds = [0, 1, 2].map(|index| {
            surface(
                mtm,
                rect(22.0 + index as f64 * 194.0, 631.0, 186.0, 34.0),
                if index == 0 { 0xe6e0fa } else { 0xf1eefd },
                9.0,
            )
        });
        for (index, control) in tab_buttons.iter().enumerate() {
            control.setFrame(rect(22.0 + index as f64 * 194.0, 631.0, 186.0, 34.0));
            control.setTag(index as isize);
            control.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                "{} view",
                ["Work", "Needs you", "Start"][index]
            ))));
        }
        let tab_marker = surface(mtm, rect(22.0, 626.0, 186.0, 3.0), 0x6d5ce0, 1.5);

        let activity_title = label(mtm, "Agents across your work", 18.0, true);
        activity_title.setFrame(rect(66.0, 488.0, 400.0, 28.0));
        let activity_icon = symbol(mtm, "square.stack.3d.up", rect(36.0, 491.0, 20.0, 20.0));
        let status = label(mtm, "Checking connected work…", 12.0, false);
        status.setFrame(rect(36.0, 464.0, WIDTH - 72.0, 18.0));
        status.setTextColor(Some(&color(0x656177)));
        status.setAccessibilityLabel(Some(&NSString::from_str("Local agent status")));
        let work_scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            rect(14.0, 84.0, WIDTH - 28.0, 372.0),
        );
        work_scroll.setHasVerticalScroller(true);
        work_scroll.setDrawsBackground(false);
        work_scroll.setAccessibilityLabel(Some(&NSString::from_str(
            "Connected repositories and agent work",
        )));

        let attention_card = surface(mtm, rect(14.0, 0.0, WIDTH - 28.0, 82.0), 0xf1eefd, 16.0);
        let attention_title = label(mtm, "Attention", 16.0, true);
        attention_title.setFrame(rect(66.0, 51.0, 270.0, 25.0));
        let attention_icon = symbol(mtm, "bell.badge", rect(36.0, 54.0, 20.0, 20.0));
        let attention = label(mtm, "Checking attention…", 13.0, false);
        attention.setFrame(rect(36.0, 10.0, WIDTH - 72.0, 38.0));
        attention.setPreferredMaxLayoutWidth(WIDTH - 68.0);
        attention.setMaximumNumberOfLines(0);
        attention.setUsesSingleLineMode(false);
        attention.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        attention.setTextColor(Some(&color(0x48415d)));
        attention.setAccessibilityLabel(Some(&NSString::from_str("Attention status")));
        let open_hub = button(
            mtm,
            "Review in Hub  ↗",
            rect(WIDTH - 162.0, 51.0, 126.0, 29.0),
        );
        open_hub.setAccessibilityLabel(Some(&NSString::from_str(
            "Open the first listed repository in Hub for Inbox and Dashboard",
        )));

        let composer_card = surface(mtm, rect(14.0, 85.0, WIDTH - 28.0, 535.0), 0xffffff, 18.0);
        let composer_title = label(mtm, "Start a coding agent", 20.0, true);
        composer_title.setFrame(rect(66.0, 572.0, 370.0, 32.0));
        let composer_icon = symbol(mtm, "terminal", rect(36.0, 578.0, 20.0, 20.0));
        let composer_hint = label(
            mtm,
            "Write once; your installed agent runs it in the selected checkout.",
            13.0,
            false,
        );
        composer_hint.setFrame(rect(36.0, 545.0, 470.0, 20.0));
        composer_hint.setTextColor(Some(&color(0x656177)));

        let prompt = NSTextField::textFieldWithString(&NSString::new(), mtm);
        prompt.setFrame(rect(36.0, 285.0, WIDTH - 72.0, 245.0));
        prompt.setFont(Some(&NSFont::systemFontOfSize(16.0)));
        prompt.setPlaceholderString(Some(&NSString::from_str("What should the agent do?")));
        prompt.setAccessibilityLabel(Some(&NSString::from_str("Prompt for the agent")));
        prompt.setUsesSingleLineMode(false);
        prompt.setMaximumNumberOfLines(0);
        prompt.setPreferredMaxLayoutWidth(WIDTH - 92.0);
        prompt.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        // NSTextField's default cell scrolls one line sideways even in a tall frame.
        if let Some(cell) = prompt.cell() {
            cell.setScrollable(false);
            cell.setWraps(true);
            cell.setUsesSingleLineMode(false);
        }

        let checkout_label = label(mtm, "Checkout: choose…", 13.0, true);
        checkout_label.setFrame(rect(36.0, 238.0, 410.0, 22.0));
        checkout_label.setAccessibilityLabel(Some(&NSString::from_str("Selected checkout")));
        let checkout_next = button(mtm, "Change ›", rect(WIDTH - 116.0, 236.0, 82.0, 27.0));
        checkout_next.setAccessibilityLabel(Some(&NSString::from_str("Choose next checkout")));
        let agent_label = label(mtm, "Agent: choose…", 13.0, true);
        agent_label.setFrame(rect(36.0, 195.0, 410.0, 22.0));
        agent_label.setAccessibilityLabel(Some(&NSString::from_str("Selected agent")));
        let agent_next = button(mtm, "Change ›", rect(WIDTH - 116.0, 193.0, 82.0, 27.0));
        agent_next.setAccessibilityLabel(Some(&NSString::from_str("Choose next agent")));
        let option_label = label(mtm, "Mode: default", 13.0, true);
        option_label.setFrame(rect(36.0, 152.0, 410.0, 22.0));
        option_label.setAccessibilityLabel(Some(&NSString::from_str("Selected agent mode")));
        let option_next = button(mtm, "Change ›", rect(WIDTH - 116.0, 150.0, 82.0, 27.0));
        option_next.setAccessibilityLabel(Some(&NSString::from_str("Choose next mode")));
        let submit = button(
            mtm,
            "Start agent  ↗",
            rect(WIDTH - 190.0, 100.0, 156.0, 40.0),
        );
        submit.setBezelStyle(NSBezelStyle::Push);
        submit.setBezelColor(Some(&color(0xc6bbff)));

        let target = Target::new(
            mtm,
            TargetIvars {
                proxy,
                prompt: prompt.clone(),
                checkout_label: checkout_label.clone(),
                agent_label: agent_label.clone(),
                option_label: option_label.clone(),
                checkouts: RefCell::new(Vec::new()),
                agents: RefCell::new(Vec::new()),
                options: RefCell::new(Vec::new()),
                repository_links: RefCell::new(Vec::new()),
                tabs: RefCell::new(vec!["Work".into(), "Needs you".into(), "Start".into()]),
                checkout_index: Cell::new(0),
                agent_index: Cell::new(0),
                option_index: Cell::new(0),
            },
        );
        // SAFETY: the retained target implements the selectors with matching sender types.
        unsafe {
            for control in &tab_buttons {
                control.setTarget(Some(&target));
                control.setAction(Some(sel!(selectTab:)));
            }
            submit.setTarget(Some(&target));
            submit.setAction(Some(sel!(submitPrompt:)));
            checkout_next.setTarget(Some(&target));
            checkout_next.setAction(Some(sel!(nextCheckout:)));
            agent_next.setTarget(Some(&target));
            agent_next.setAction(Some(sel!(nextAgent:)));
            option_next.setTarget(Some(&target));
            option_next.setAction(Some(sel!(nextOption:)));
            open_hub.setTarget(Some(&target));
            open_hub.setAction(Some(sel!(openSelectedHub:)));
            match &pin {
                PinControl::Switch(control) => {
                    control.setTarget(Some(&target));
                    control.setAction(Some(sel!(pinChanged:)));
                }
                PinControl::Button(control) => {
                    control.setTarget(Some(&target));
                    control.setAction(Some(sel!(pinButtonChanged:)));
                }
            }
        }

        now_group.addSubview(&attention_card);
        if let Some(icon) = attention_icon {
            now_group.addSubview(&icon);
        }
        now_group.addSubview(&attention_title);
        now_group.addSubview(&attention);
        now_group.addSubview(&open_hub);
        let start_cta = button(
            mtm,
            "Start a coding agent  ↗",
            rect(WIDTH - 250.0, 24.0, 216.0, 44.0),
        );
        start_cta.setTag(2);
        start_cta.setBordered(false);
        start_cta.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        start_cta.setContentTintColor(Some(&color(0x17142e)));
        // SAFETY: Target implements selectTab: for this retained button.
        unsafe {
            start_cta.setTarget(Some(&target));
            start_cta.setAction(Some(sel!(selectTab:)));
        }
        let all_cta = button(mtm, "See all work  →", rect(34.0, 24.0, 170.0, 44.0));
        all_cta.setTag(0);
        all_cta.setBordered(false);
        all_cta.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        all_cta.setContentTintColor(Some(&color(0x17142e)));
        // SAFETY: Target implements selectTab: for this retained button.
        unsafe {
            all_cta.setTarget(Some(&target));
            all_cta.setAction(Some(sel!(selectTab:)));
        }
        let footer_card = surface(mtm, rect(14.0, 13.0, WIDTH - 28.0, 65.0), 0xf1eefd, 16.0);
        let action_backgrounds = [
            surface(mtm, rect(34.0, 24.0, 170.0, 44.0), 0xffffff, 11.0),
            surface(mtm, rect(WIDTH - 250.0, 24.0, 216.0, 44.0), 0xd9d0fa, 11.0),
        ];
        let footer_title = label(mtm, "Start something new", 14.0, true);
        footer_title.setFrame(rect(34.0, 49.0, 280.0, 21.0));
        let footer_hint = label(
            mtm,
            "Send a prompt through your installed agent",
            11.0,
            false,
        );
        footer_hint.setFrame(rect(34.0, 30.0, 320.0, 18.0));
        footer_hint.setTextColor(Some(&color(0x656177)));
        if let Some(icon) = &activity_icon {
            list_group.addSubview(icon);
        }
        list_group.addSubview(&activity_title);
        list_group.addSubview(&status);
        list_group.addSubview(&work_scroll);
        prompt_group.addSubview(&composer_card);
        if let Some(icon) = composer_icon {
            prompt_group.addSubview(&icon);
        }
        prompt_group.addSubview(&composer_title);
        prompt_group.addSubview(&composer_hint);
        prompt_group.addSubview(&prompt);
        prompt_group.addSubview(&checkout_label);
        prompt_group.addSubview(&checkout_next);
        prompt_group.addSubview(&agent_label);
        prompt_group.addSubview(&agent_next);
        prompt_group.addSubview(&option_label);
        prompt_group.addSubview(&option_next);
        prompt_group.addSubview(&submit);
        content.addSubview(&list_group);
        content.addSubview(&now_group);
        content.addSubview(&prompt_group);
        content.addSubview(&footer_card);
        for background in &action_backgrounds {
            content.addSubview(background);
        }
        content.addSubview(&footer_title);
        content.addSubview(&footer_hint);
        // The two full-size view groups sit behind the controls. The attention group is
        // bounded to its card so it cannot intercept list or footer clicks.
        content.addSubview(&all_cta);
        content.addSubview(&start_cta);
        content.addSubview(&header);
        content.addSubview(&logo);
        content.addSubview(&brand);
        content.addSubview(&tagline);
        content.addSubview(&header_rule);
        content.addSubview(&header_note);
        match &pin {
            PinControl::Switch(control) => content.addSubview(control),
            PinControl::Button(control) => content.addSubview(control),
        }
        content.addSubview(&pin_label);
        for background in &tab_backgrounds {
            content.addSubview(background);
        }
        for control in &tab_buttons {
            content.addSubview(control);
        }
        content.addSubview(&tab_marker);
        prompt_group.setHidden(true);
        all_cta.setHidden(true);
        action_backgrounds[0].setHidden(true);
        let panel_scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            rect(0.0, 0.0, WIDTH, panel_height),
        );
        panel_scroll.setDrawsBackground(false);
        panel_scroll.setHasVerticalScroller(panel_height < HEIGHT);
        panel_scroll.setDocumentView(Some(&content));
        let clip = panel_scroll.contentView();
        clip.scrollToPoint(NSPoint::new(0.0, HEIGHT - panel_height));
        panel_scroll.reflectScrolledClipView(&clip);
        panel.setContentView(Some(&panel_scroll));

        Some(Self {
            panel,
            panel_scroll,
            target,
            status,
            list_title: activity_title,
            attention_title,
            attention,
            work_scroll,
            now_group,
            list_group,
            prompt_group,
            now_actions: [all_cta, start_cta],
            action_backgrounds,
            footer_card,
            footer_title,
            footer_hint,
            tab_buttons,
            tab_backgrounds,
            tab_marker,
            tab: 0,
            all_rows: Vec::new(),
            rendered_rows: Vec::new(),
            rendered_omitted: 0,
            expanded: HashSet::new(),
            visible_limit: REPOSITORY_LIMIT,
            prompt,
            checkout_label,
            agent_label,
            option_label,
            pin,
            pinned: false,
        })
    }

    pub(super) fn show(
        &mut self,
        status: &str,
        attention: Option<&str>,
        checkouts: &[String],
        agents: &[String],
        options: &[String],
        selected_checkout: usize,
    ) {
        self.fit_to_screen();
        self.attention_title
            .setStringValue(&NSString::from_str("Attention"));
        self.attention_title
            .setTextColor(Some(&NSColor::labelColor()));
        self.refresh_options(
            status,
            attention,
            checkouts,
            agents,
            options,
            selected_checkout,
        );
        self.set_tab(0);
        if !self.panel.isVisible() {
            self.panel.center();
        }
        if let Some(mtm) = MainThreadMarker::new() {
            // AgentSCM still supports macOS before NSApplication::activate was added.
            #[allow(deprecated)]
            NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        }
        self.panel.makeKeyAndOrderFront(None);
        self.panel.orderFrontRegardless();
    }

    pub(super) fn set_status(&mut self, status: &str, attention: Option<&str>) {
        self.update_summary(status);
        let text = attention
            .unwrap_or("No local setup alerts. Open Hub for Inbox and Dashboard attention.");
        self.attention.setStringValue(&NSString::from_str(text));
        self.attention
            .setAccessibilityLabel(Some(&NSString::from_str(if text.is_empty() {
                "No attention required"
            } else {
                text
            })));
    }

    fn update_summary(&self, status: &str) {
        let summary = if self.all_rows.is_empty() {
            status.to_owned()
        } else {
            let local = self
                .all_rows
                .iter()
                .map(|row| row.local.len())
                .sum::<usize>();
            let hub = self
                .all_rows
                .iter()
                .map(|row| row.work.len())
                .sum::<usize>();
            format!(
                "Repositories {} · Local agents {} · Hub activities {}",
                self.all_rows.len(),
                local,
                hub
            )
        };
        self.status.setStringValue(&NSString::from_str(&summary));
        self.status
            .setAccessibilityLabel(Some(&NSString::from_str(&summary)));
    }

    pub(super) fn mark_new_attention(&self) {
        self.attention_title
            .setStringValue(&NSString::from_str("New attention"));
        self.attention_title.setTextColor(Some(&color(0xa23c2e)));
    }

    pub(super) fn set_rows(&mut self, rows: &[RepositoryRow]) {
        if self.all_rows != rows {
            self.all_rows = rows.to_vec();
            self.update_summary("Checking connected work…");
            self.render_rows();
        }
    }

    pub(super) fn toggle_repository(&mut self, index: usize) {
        let Some(row) = self.rendered_rows.get(index) else {
            return;
        };
        let key = (row.hub.clone(), row.name.clone());
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        self.render_rows();
    }

    pub(super) fn show_more_repositories(&mut self) {
        self.visible_limit = self.visible_limit.saturating_add(REPOSITORY_LIMIT);
        self.render_rows();
    }

    pub(super) fn set_tab(&mut self, index: usize) {
        if index > 2 {
            return;
        }
        self.tab = index;
        self.now_group.setHidden(index == 2);
        self.now_actions[0].setHidden(index != 1);
        self.now_actions[1].setHidden(index == 2);
        self.action_backgrounds[0].setHidden(index != 1);
        self.action_backgrounds[1].setHidden(index == 2);
        self.footer_card.setHidden(index == 2);
        self.footer_title.setHidden(index != 0);
        self.footer_hint.setHidden(index != 0);
        self.list_group.setHidden(index == 2);
        self.prompt_group.setHidden(index != 2);
        if index == 1 {
            self.list_title
                .setStringValue(&NSString::from_str("What needs you"));
        } else {
            self.list_title
                .setStringValue(&NSString::from_str("Agents across your work"));
        }
        for (tab_index, control) in self.tab_buttons.iter().enumerate() {
            self.tab_backgrounds[tab_index].setFillColor(&color(if tab_index == index {
                0xe6e0fa
            } else {
                0xf1eefd
            }));
            control.setContentTintColor(Some(&color(if tab_index == index {
                0x17142e
            } else {
                0x555169
            })));
            control.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                "{} view{}",
                ["Work", "Needs you", "Start"][tab_index],
                if tab_index == index { ", selected" } else { "" }
            ))));
        }
        self.tab_marker
            .setFrame(rect(22.0 + index as f64 * 194.0, 626.0, 186.0, 3.0));
        self.scroll_outer_to_tab();
        self.rendered_rows.clear();
        self.rendered_omitted = usize::MAX;
        self.render_rows();
        if index == 2 && self.panel.isVisible() {
            let _ = self.panel.makeFirstResponder(Some(&self.prompt));
        }
    }

    fn render_rows(&mut self) {
        if self.tab == 2 {
            return;
        }
        let (rows, omitted_repositories) =
            selected_rows(&self.all_rows, self.tab, self.visible_limit, &self.expanded);
        if self.rendered_rows == rows && self.rendered_omitted == omitted_repositories {
            return;
        }
        let distance_from_top = if self.rendered_rows.is_empty() {
            None
        } else {
            self.work_scroll.documentView().map(|old| {
                let clip = self.work_scroll.contentView();
                (old.frame().size.height
                    - self.work_scroll.frame().size.height
                    - clip.bounds().origin.y)
                    .max(0.0)
            })
        };
        self.rendered_rows = rows.to_vec();
        self.rendered_omitted = omitted_repositories;
        *self.target.ivars().repository_links.borrow_mut() = rows
            .iter()
            .map(|row| row.hub.as_ref().map(|hub| (hub.clone(), row.name.clone())))
            .collect();
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let heights: Vec<f64> = rows
            .iter()
            .map(|row| {
                if is_quiet_row(row) {
                    return 126.0;
                }
                86.0 + if row.local.is_empty() {
                    0.0
                } else {
                    23.0 + 42.0 * row.local.len() as f64
                } + if row.work.is_empty() {
                    63.0
                } else {
                    23.0 + 52.0 * row.work.len() as f64
                } + if row.overflow > 0
                    || (row.work.len() > HUB_ACTIVITY_LIMIT
                        && self.expanded.contains(&(row.hub.clone(), row.name.clone())))
                {
                    30.0
                } else {
                    0.0
                }
            })
            .collect();
        let viewport_height = 372.0;
        let footer_height = if omitted_repositories > 0 { 44.0 } else { 0.0 };
        let total = (12.0 + heights.iter().sum::<f64>() + 12.0 * rows.len() as f64 + footer_height)
            .max(viewport_height);
        let document =
            NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, WIDTH - 44.0, total));
        let mut top = total - 6.0;
        if rows.is_empty() {
            let empty = label(
                mtm,
                if self.tab == 1 {
                    "Nothing is asking for you right now"
                } else {
                    "No connected agents yet"
                },
                16.0,
                true,
            );
            empty.setFrame(rect(24.0, total - 58.0, WIDTH - 88.0, 24.0));
            let help = label(
                mtm,
                if self.tab == 1 {
                    "Work and Inbox signals will appear here when they need a look."
                } else {
                    "Connect a checkout or sign in to Hub to see who is working."
                },
                12.0,
                false,
            );
            help.setFrame(rect(24.0, total - 82.0, WIDTH - 88.0, 18.0));
            help.setTextColor(Some(&color(0x656177)));
            document.addSubview(&empty);
            document.addSubview(&help);
        }
        for (index, (row, height)) in rows.iter().zip(heights).enumerate() {
            let bottom = top - height;
            let card = surface(mtm, rect(0.0, bottom, WIDTH - 44.0, height), 0xffffff, 16.0);
            document.addSubview(&card);

            let name = label(mtm, &row.name, 16.0, true);
            name.setFrame(rect(20.0, top - 34.0, 350.0, 25.0));
            name.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
            document.addSubview(&name);

            let hub = label(mtm, &row.hub_state, 11.0, false);
            hub.setFrame(rect(WIDTH - 230.0, top - 30.0, 105.0, 18.0));
            hub.setTextColor(Some(&color(if row.attention > 0 {
                0xc8432a
            } else {
                0x6c6b80
            })));
            document.addSubview(&hub);
            if row.hub.is_some() {
                let open = button(mtm, "Open ↗", rect(WIDTH - 118.0, top - 36.0, 65.0, 29.0));
                open.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                    "Open {} in Hub",
                    row.name
                ))));
                open.setTag(index as isize);
                // SAFETY: Target retains the selector for the life of each row button.
                unsafe {
                    open.setTarget(Some(&self.target));
                    open.setAction(Some(sel!(openRepository:)));
                }
                document.addSubview(&open);
            }

            let local = label(mtm, &row.local_agents, 12.0, false);
            local.setFrame(rect(20.0, top - 61.0, WIDTH - 82.0, 20.0));
            local.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
            local.setTextColor(Some(&color(0x6c6b80)));
            document.addSubview(&local);

            if is_quiet_row(row) {
                let agents = if row.local.is_empty() {
                    "No local agents".to_owned()
                } else {
                    format!(
                        "{} · no recent activity observed",
                        row.local
                            .iter()
                            .map(|agent| agent.agent.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                let agent_line = label(mtm, &agents, 12.0, true);
                agent_line.setFrame(rect(20.0, top - 84.0, WIDTH - 82.0, 20.0));
                agent_line.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                document.addSubview(&agent_line);
                let hub_line = label(mtm, &row.work_note, 11.0, false);
                hub_line.setFrame(rect(20.0, top - 106.0, WIDTH - 82.0, 18.0));
                hub_line.setTextColor(Some(&color(0x656177)));
                hub_line.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                document.addSubview(&hub_line);
                top = bottom - 12.0;
                continue;
            }

            let mut cursor = top - 86.0;
            if !row.local.is_empty() {
                let section = label(mtm, "ON THIS MAC", 10.0, true);
                section.setFrame(rect(20.0, cursor - 3.0, 160.0, 16.0));
                section.setTextColor(Some(&color(0x656177)));
                document.addSubview(&section);
                cursor -= 23.0;
                for agent in &row.local {
                    let dot = surface(
                        mtm,
                        rect(20.0, cursor - 10.0, 8.0, 8.0),
                        if agent.needs_you {
                            0xc8432a
                        } else if agent.observed {
                            0x127a52
                        } else {
                            0xb9b5c5
                        },
                        4.0,
                    );
                    document.addSubview(&dot);
                    let title = label(mtm, &agent.agent, 13.0, true);
                    title.setFrame(rect(36.0, cursor - 13.0, WIDTH - 100.0, 20.0));
                    document.addSubview(&title);
                    let detail = label(mtm, &agent.detail, 11.0, false);
                    detail.setFrame(rect(36.0, cursor - 30.0, WIDTH - 100.0, 18.0));
                    detail.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                    detail.setTextColor(Some(&color(0x656177)));
                    document.addSubview(&detail);
                    cursor -= 42.0;
                }
            }
            let section = label(mtm, "IN HUB", 10.0, true);
            section.setFrame(rect(20.0, cursor - 3.0, 160.0, 16.0));
            section.setTextColor(Some(&color(0x656177)));
            document.addSubview(&section);
            cursor -= 23.0;
            if row.work.is_empty() {
                let no_work = label(mtm, &row.work_note, 12.0, false);
                no_work.setFrame(rect(20.0, cursor - 21.0, WIDTH - 82.0, 20.0));
                no_work.setTextColor(Some(&color(0x6c6b80)));
                document.addSubview(&no_work);
            } else {
                for work in &row.work {
                    let y = cursor - 15.0;
                    let dot = surface(
                        mtm,
                        rect(20.0, y + 7.0, 8.0, 8.0),
                        if work.needs_you { 0xc8432a } else { 0x6d5ce0 },
                        4.0,
                    );
                    document.addSubview(&dot);
                    let title = label(mtm, &format!("{} · {}", work.agent, work.title), 13.0, true);
                    title.setFrame(rect(36.0, y, WIDTH - 100.0, 22.0));
                    title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                    document.addSubview(&title);
                    let detail = label(mtm, &work.detail, 11.0, false);
                    detail.setFrame(rect(36.0, y - 19.0, WIDTH - 100.0, 18.0));
                    detail.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                    detail.setTextColor(Some(&color(0x656177)));
                    document.addSubview(&detail);
                    cursor -= 52.0;
                }
            }
            let expanded = self.expanded.contains(&(row.hub.clone(), row.name.clone()));
            if row.overflow > 0 || (expanded && row.work.len() > HUB_ACTIVITY_LIMIT) {
                let title = if expanded {
                    "Show fewer activities".to_owned()
                } else {
                    format!("Show {} more Hub activities", row.overflow)
                };
                let more = button(mtm, &title, rect(31.0, bottom + 8.0, 230.0, 26.0));
                more.setTag(index as isize);
                // SAFETY: the retained target implements this selector for row buttons.
                unsafe {
                    more.setTarget(Some(&self.target));
                    more.setAction(Some(sel!(expandRepository:)));
                }
                document.addSubview(&more);
            }
            top = bottom - 12.0;
        }
        if omitted_repositories > 0 {
            let more = button(
                mtm,
                &format!(
                    "Show {} more repositories",
                    omitted_repositories.min(REPOSITORY_LIMIT)
                ),
                rect(20.0, top - 36.0, 230.0, 30.0),
            );
            // SAFETY: the retained target implements this selector for the paging button.
            unsafe {
                more.setTarget(Some(&self.target));
                more.setAction(Some(sel!(showMoreRepositories:)));
            }
            document.addSubview(&more);
        }
        self.work_scroll.setDocumentView(Some(&document));
        let clip = self.work_scroll.contentView();
        let top = (total - viewport_height).max(0.0);
        clip.scrollToPoint(NSPoint::new(
            0.0,
            (top - distance_from_top.unwrap_or(0.0)).clamp(0.0, top),
        ));
        self.work_scroll.reflectScrolledClipView(&clip);
    }

    pub(super) fn is_visible(&self) -> bool {
        self.panel.isVisible()
    }

    pub(super) fn hide(&self) {
        self.panel.orderOut(None);
    }

    /// Suppress a passive notice only while the aggregate card is fully on screen.
    pub(super) fn is_attention_visible(&self) -> bool {
        let visible = self.panel_scroll.contentView().bounds();
        self.tab != 2
            && self.panel.isVisible()
            && self.panel.isKeyWindow()
            && visible.origin.y <= 530.0
            && visible.origin.y + visible.size.height >= 612.0
    }

    pub(super) fn is_pinned(&self) -> bool {
        self.pinned
    }

    fn fit_to_screen(&self) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let screen = self.panel.screen().or_else(|| NSScreen::mainScreen(mtm));
        let height = screen
            .as_ref()
            .map_or(HEIGHT, |screen| fitted_height(screen));
        let resized = (self.panel_scroll.frame().size.height - height).abs() > 1.0;
        self.panel.setContentSize(NSSize::new(WIDTH, height));
        self.panel_scroll.setFrame(rect(0.0, 0.0, WIDTH, height));
        self.panel_scroll.setHasVerticalScroller(height < HEIGHT);
        self.scroll_outer_to_tab();
        if resized && self.panel.isVisible() {
            self.panel.center();
        }
    }

    fn scroll_outer_to_tab(&self) {
        let height = self.panel_scroll.frame().size.height;
        let clip = self.panel_scroll.contentView();
        clip.scrollToPoint(NSPoint::new(0.0, HEIGHT - height));
        self.panel_scroll.reflectScrolledClipView(&clip);
    }

    fn set_options(&mut self, checkouts: &[String], agents: &[String], options: &[String]) {
        let ivars = self.target.ivars();
        *ivars.checkouts.borrow_mut() = checkouts.to_vec();
        *ivars.agents.borrow_mut() = agents.to_vec();
        *ivars.options.borrow_mut() = options.to_vec();
        ivars.checkout_index.set(0);
        ivars.agent_index.set(0);
        ivars.option_index.set(0);
        self.checkout_label.setStringValue(&NSString::from_str(
            &checkouts
                .first()
                .map_or_else(|| "Checkout: none".to_owned(), |s| format!("Checkout: {s}")),
        ));
        self.agent_label.setStringValue(&NSString::from_str(
            &agents
                .first()
                .map_or_else(|| "Agent: none".to_owned(), |s| format!("Agent: {s}")),
        ));
        self.prompt
            .setPlaceholderString(Some(&NSString::from_str(&agents.first().map_or_else(
                || "What should the agent do?".to_owned(),
                |agent| format!("What should {agent} do?"),
            ))));
        self.option_label.setStringValue(&NSString::from_str(
            &options
                .first()
                .map_or_else(|| "Mode: default".to_owned(), |s| format!("Mode: {s}")),
        ));
    }

    pub(super) fn refresh_options(
        &mut self,
        status: &str,
        attention: Option<&str>,
        checkouts: &[String],
        agents: &[String],
        options: &[String],
        selected_checkout: usize,
    ) {
        let agent_index = self.target.ivars().agent_index.get();
        let option_index = self.target.ivars().option_index.get();
        self.set_options(checkouts, agents, options);
        let ivars = self.target.ivars();
        let checkout_index = selected_checkout.min(checkouts.len().saturating_sub(1));
        let agent_index = agent_index.min(agents.len().saturating_sub(1));
        let option_index = option_index.min(options.len().saturating_sub(1));
        ivars.checkout_index.set(checkout_index);
        ivars.agent_index.set(agent_index);
        ivars.option_index.set(option_index);
        if let Some(checkout) = checkouts.get(checkout_index) {
            self.checkout_label
                .setStringValue(&NSString::from_str(&format!("Checkout: {checkout}")));
        }
        if let Some(agent) = agents.get(agent_index) {
            self.agent_label
                .setStringValue(&NSString::from_str(&format!("Agent: {agent}")));
            self.prompt
                .setPlaceholderString(Some(&NSString::from_str(&format!(
                    "What should {agent} do?"
                ))));
        }
        if let Some(option) = options.get(option_index) {
            self.option_label
                .setStringValue(&NSString::from_str(&format!("Mode: {option}")));
        }
        self.set_status(status, attention);
    }
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

/// The work view keeps the whole bounded roster; the attention view only narrows it.
fn is_quiet_row(row: &RepositoryRow) -> bool {
    row.attention == 0
        && row.hub_state == "Hub current"
        && row.work.is_empty()
        && row
            .local
            .iter()
            .all(|agent| !agent.observed && !agent.needs_you)
}

fn selected_rows(
    all_rows: &[RepositoryRow],
    tab: usize,
    visible_limit: usize,
    expanded: &HashSet<(Option<String>, String)>,
) -> (Vec<RepositoryRow>, usize) {
    let candidates = all_rows
        .iter()
        .filter(|row| {
            tab == 0
                || row.attention > 0
                || row.local.iter().any(|agent| agent.needs_you)
                || row.work.iter().any(|work| work.needs_you)
        })
        .collect::<Vec<_>>();
    let omitted = candidates.len().saturating_sub(visible_limit);
    let rows = candidates
        .into_iter()
        .take(visible_limit)
        .map(|row| {
            let mut row = row.clone();
            if tab == 1 {
                row.local.retain(|agent| agent.needs_you);
                row.work.retain(|work| work.needs_you);
                if row.work.is_empty() && row.attention > 0 {
                    row.work_note = "More attention in Hub · open this repository".into();
                }
            }
            if !expanded.contains(&(row.hub.clone(), row.name.clone())) {
                row.overflow = row.work.len().saturating_sub(HUB_ACTIVITY_LIMIT);
                row.work.truncate(HUB_ACTIVITY_LIMIT);
            }
            row
        })
        .collect();
    (rows, omitted)
}

fn fitted_height(screen: &NSScreen) -> f64 {
    // Keep room for the title bar and a little separation from the menu bar and Dock.
    (screen.visibleFrame().size.height - 56.0).clamp(500.0, HEIGHT)
}

fn color(rgb: u32) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from((rgb >> 16) & 0xff) / 255.0,
        f64::from((rgb >> 8) & 0xff) / 255.0,
        f64::from(rgb & 0xff) / 255.0,
        1.0,
    )
}

fn surface(mtm: MainThreadMarker, frame: NSRect, fill: u32, radius: f64) -> Retained<NSBox> {
    let box_view = NSBox::initWithFrame(NSBox::alloc(mtm), frame);
    box_view.setBoxType(NSBoxType::Custom);
    box_view.setBorderWidth(0.0);
    box_view.setCornerRadius(radius);
    box_view.setFillColor(&color(fill));
    box_view
}

fn label(mtm: MainThreadMarker, text: &str, size: f64, bold: bool) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    let font = if bold {
        NSFont::boldSystemFontOfSize(size)
    } else {
        NSFont::systemFontOfSize(size)
    };
    label.setFont(Some(&font));
    label.setTextColor(Some(&NSColor::labelColor()));
    label
}

fn button(mtm: MainThreadMarker, title: &str, frame: NSRect) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    button.setTitle(&NSString::from_str(title));
    button.setAccessibilityLabel(Some(&NSString::from_str(title)));
    button
}

fn symbol(mtm: MainThreadMarker, name: &str, frame: NSRect) -> Option<Retained<NSImageView>> {
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        None,
    )?;
    image.setTemplate(true);
    let view = NSImageView::initWithFrame(NSImageView::alloc(mtm), frame);
    view.setImage(Some(&image));
    view.setContentTintColor(Some(&color(0x6d5ce0)));
    Some(view)
}

impl CompanionPanel {
    /// Applies an opt-in floating level to the same light-palette panel.
    pub(super) fn set_pinned(&mut self, pinned: bool) {
        self.pinned = pinned;
        let state = if pinned {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        };
        match &self.pin {
            PinControl::Switch(control) => control.setState(state),
            PinControl::Button(control) => control.setState(state),
        }
        self.panel.setLevel(if pinned {
            NSFloatingWindowLevel
        } else {
            NSNormalWindowLevel
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(name: &str) -> RepositoryRow {
        RepositoryRow {
            name: name.into(),
            hub: Some("https://hub.example".into()),
            local_agents: "Connected checkout on this Mac".into(),
            hub_state: "Hub current".into(),
            attention: 0,
            work_note: "No current Hub work reported".into(),
            local: vec![LocalAgentRow {
                agent: "Claude Code".into(),
                detail: "Connected on this Mac · no recent activity observed".into(),
                observed: false,
                needs_you: false,
            }],
            work: Vec::new(),
            overflow: 0,
        }
    }

    #[test]
    fn work_is_the_full_roster_and_needs_you_only_filters_actionable_rows() {
        let idle = repository("example/idle");
        let mut blocked = repository("example/blocked");
        blocked.work.push(WorkRow {
            agent: "Codex".into(),
            title: "Review API".into(),
            detail: "Needs your attention".into(),
            needs_you: true,
        });
        let mut local_issue = repository("example/local-issue");
        local_issue.local[0].needs_you = true;
        let all = vec![blocked, local_issue, idle];
        let (work, omitted) = selected_rows(&all, 0, REPOSITORY_LIMIT, &HashSet::new());
        assert_eq!(work.len(), 3);
        assert_eq!(omitted, 0);
        assert_eq!(work[2].name, "example/idle");

        let (attention, omitted) = selected_rows(&all, 1, REPOSITORY_LIMIT, &HashSet::new());
        assert_eq!(attention.len(), 2);
        assert_eq!(omitted, 0);
        assert_eq!(attention[0].work.len(), 1);
        assert_eq!(attention[1].local.len(), 1);
    }

    #[test]
    fn bounded_roster_explains_repository_and_hub_activity_overflow() {
        let mut all = (0..=REPOSITORY_LIMIT)
            .map(|index| repository(&format!("example/{index}")))
            .collect::<Vec<_>>();
        all[0].work = (0..=HUB_ACTIVITY_LIMIT)
            .map(|index| WorkRow {
                agent: format!("Agent {index}"),
                title: "Task".into(),
                detail: "Reported in Hub".into(),
                needs_you: false,
            })
            .collect();
        let (rows, omitted) = selected_rows(&all, 0, REPOSITORY_LIMIT, &HashSet::new());
        assert_eq!(rows.len(), REPOSITORY_LIMIT);
        assert_eq!(omitted, 1);
        assert_eq!(rows[0].work.len(), HUB_ACTIVITY_LIMIT);
        assert_eq!(rows[0].overflow, 1);

        let expanded = HashSet::from([(all[0].hub.clone(), all[0].name.clone())]);
        let (rows, omitted) = selected_rows(&all, 0, REPOSITORY_LIMIT * 2, &expanded);
        assert_eq!(rows.len(), REPOSITORY_LIMIT + 1);
        assert_eq!(omitted, 0);
        assert_eq!(rows[0].work.len(), HUB_ACTIVITY_LIMIT + 1);
        assert_eq!(rows[0].overflow, 0);
    }
}
