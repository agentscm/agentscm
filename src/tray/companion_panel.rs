//! Compact native prompt surface for the macOS tray companion.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSApplication,
    NSBackingStoreType, NSBezelStyle, NSBox, NSBoxType, NSButton, NSButtonType, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSFloatingWindowLevel, NSFont, NSImage,
    NSImageView, NSLineBreakMode, NSNormalWindowLevel, NSPanel, NSSwitch, NSTextField, NSView,
    NSWindowStyleMask,
};
use objc2_foundation::{NSData, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use winit::event_loop::EventLoopProxy;

use super::UserEvent;

const WIDTH: f64 = 500.0;
const HEIGHT: f64 = 680.0;

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
            let _ = self.ivars().proxy.send_event(UserEvent::CompanionOpenHub(
                self.ivars().checkout_index.get(),
            ));
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
    target: Retained<Target>,
    status: Retained<NSTextField>,
    attention: Retained<NSTextField>,
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
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect(0.0, 0.0, WIDTH, HEIGHT),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: this struct owns the panel after a user closes its window.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setTitle(&NSString::from_str("AgentSCM Companion"));
        panel.setHidesOnDeactivate(false);
        // The authored surfaces use a fixed light palette; keep semantic controls in that palette.
        let appearance = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameAqua })?;
        panel.setAppearance(Some(&appearance));
        panel.setBackgroundColor(Some(&color(0xfafafc)));

        let content = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, WIDTH, HEIGHT));
        let header = surface(mtm, rect(14.0, 555.0, WIDTH - 28.0, 111.0), 0x17142e, 22.0);
        let logo_bytes = include_bytes!("../../docs/assets/agentscm-icon.png");
        // SAFETY: include_bytes! provides a valid pointer for the duration of the copy.
        let logo_data =
            unsafe { NSData::dataWithBytes_length(logo_bytes.as_ptr().cast(), logo_bytes.len()) };
        let logo_image = NSImage::initWithData(NSImage::alloc(), &logo_data)?;
        let logo =
            NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(37.0, 607.0, 45.0, 45.0));
        logo.setImage(Some(&logo_image));
        logo.setAccessibilityLabel(Some(&NSString::from_str("AgentSCM logo")));
        let brand = label(mtm, "AgentSCM", 21.0, true);
        brand.setFrame(rect(100.0, 622.0, 250.0, 27.0));
        brand.setTextColor(Some(&color(0xf3f0ff)));
        let tagline = label(mtm, "Your work, within reach", 12.0, false);
        tagline.setFrame(rect(101.0, 602.0, 280.0, 18.0));
        tagline.setTextColor(Some(&color(0xc2bce1)));
        let pin = if objc2::runtime::AnyClass::get(c"NSSwitch").is_some() {
            let control =
                NSSwitch::initWithFrame(NSSwitch::alloc(mtm), rect(403.0, 568.0, 42.0, 22.0));
            control.setAccessibilityLabel(Some(&NSString::from_str("Keep panel on top")));
            PinControl::Switch(control)
        } else {
            let control = button(mtm, "", rect(403.0, 568.0, 42.0, 22.0));
            control.setButtonType(NSButtonType::Switch);
            control.setAccessibilityLabel(Some(&NSString::from_str("Keep panel on top")));
            PinControl::Button(control)
        };
        let pin_label = label(mtm, "PIN", 10.0, true);
        pin_label.setFrame(rect(373.0, 572.0, 30.0, 14.0));
        pin_label.setTextColor(Some(&color(0xc2bce1)));

        let header_rule = surface(mtm, rect(34.0, 594.0, WIDTH - 68.0, 1.0), 0x3c3658, 0.0);
        let header_note = label(mtm, "Work + Inbox signals, about every minute", 12.0, false);
        header_note.setFrame(rect(35.0, 568.0, 300.0, 19.0));
        header_note.setTextColor(Some(&color(0xe2def3)));

        let activity_title = label(mtm, "On this computer", 14.0, true);
        activity_title.setFrame(rect(66.0, 393.0, 250.0, 22.0));
        let activity_icon = symbol(mtm, "desktopcomputer", rect(36.0, 394.0, 19.0, 19.0));
        let status = label(mtm, "Loading local activity…", 12.0, false);
        status.setFrame(rect(36.0, 343.0, WIDTH - 72.0, 46.0));
        status.setPreferredMaxLayoutWidth(WIDTH - 68.0);
        status.setMaximumNumberOfLines(0);
        status.setUsesSingleLineMode(false);
        status.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        status.setAccessibilityLabel(Some(&NSString::from_str("Companion status")));

        let attention_card = surface(mtm, rect(14.0, 428.0, WIDTH - 28.0, 115.0), 0xf1eefd, 18.0);
        let attention_title = label(mtm, "Needs you", 18.0, true);
        attention_title.setFrame(rect(66.0, 508.0, 230.0, 26.0));
        let attention_icon = symbol(mtm, "bell.badge", rect(36.0, 512.0, 20.0, 20.0));
        let attention = label(mtm, "Loading attention…", 12.0, false);
        attention.setFrame(rect(36.0, 443.0, WIDTH - 72.0, 60.0));
        attention.setPreferredMaxLayoutWidth(WIDTH - 68.0);
        attention.setMaximumNumberOfLines(0);
        attention.setUsesSingleLineMode(false);
        attention.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        attention.setTextColor(Some(&color(0x48415d)));
        attention.setAccessibilityLabel(Some(&NSString::from_str("Attention status")));
        let open_hub = button(mtm, "Open Hub  ↗", rect(360.0, 507.0, 106.0, 29.0));
        open_hub.setAccessibilityLabel(Some(&NSString::from_str(
            "Open selected checkout in Hub for Inbox and Dashboard",
        )));

        let composer_card = surface(mtm, rect(14.0, 154.0, WIDTH - 28.0, 177.0), 0xffffff, 18.0);
        let composer_title = label(mtm, "Start a coding agent", 16.0, true);
        composer_title.setFrame(rect(66.0, 305.0, 270.0, 24.0));
        let composer_icon = symbol(mtm, "terminal", rect(36.0, 307.0, 20.0, 20.0));
        let composer_hint = label(mtm, "One prompt through your installed CLI", 11.0, false);
        composer_hint.setFrame(rect(36.0, 282.0, 380.0, 18.0));
        composer_hint.setTextColor(Some(&color(0x656177)));

        let prompt = NSTextField::textFieldWithString(&NSString::new(), mtm);
        prompt.setFrame(rect(36.0, 171.0, WIDTH - 72.0, 103.0));
        prompt.setFont(Some(&NSFont::systemFontOfSize(15.0)));
        prompt.setPlaceholderString(Some(&NSString::from_str("What should the agent do?")));
        prompt.setAccessibilityLabel(Some(&NSString::from_str("Prompt for the agent")));
        prompt.setUsesSingleLineMode(false);
        prompt.setMaximumNumberOfLines(0);
        prompt.setPreferredMaxLayoutWidth(WIDTH - 92.0);

        let checkout_label = label(mtm, "Checkout: choose…", 12.0, true);
        checkout_label.setFrame(rect(36.0, 126.0, 330.0, 22.0));
        checkout_label.setAccessibilityLabel(Some(&NSString::from_str("Selected checkout")));
        let checkout_next = button(mtm, "Next ›", rect(384.0, 124.0, 82.0, 27.0));
        checkout_next.setAccessibilityLabel(Some(&NSString::from_str("Choose next checkout")));
        let agent_label = label(mtm, "Agent: choose…", 12.0, true);
        agent_label.setFrame(rect(36.0, 90.0, 330.0, 22.0));
        agent_label.setAccessibilityLabel(Some(&NSString::from_str("Selected agent")));
        let agent_next = button(mtm, "Next ›", rect(384.0, 88.0, 82.0, 27.0));
        agent_next.setAccessibilityLabel(Some(&NSString::from_str("Choose next agent")));
        let option_label = label(mtm, "Mode: default", 12.0, true);
        option_label.setFrame(rect(36.0, 54.0, 330.0, 22.0));
        option_label.setAccessibilityLabel(Some(&NSString::from_str("Selected agent mode")));
        let option_next = button(mtm, "Next ›", rect(384.0, 52.0, 82.0, 27.0));
        option_next.setAccessibilityLabel(Some(&NSString::from_str("Choose next mode")));
        let submit = button(mtm, "Start agent  ↗", rect(338.0, 8.0, 128.0, 38.0));
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
                checkout_index: Cell::new(0),
                agent_index: Cell::new(0),
                option_index: Cell::new(0),
            },
        );
        // SAFETY: the retained target implements the selectors with matching sender types.
        unsafe {
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
        content.addSubview(&attention_card);
        if let Some(icon) = attention_icon {
            content.addSubview(&icon);
        }
        content.addSubview(&attention_title);
        content.addSubview(&attention);
        content.addSubview(&open_hub);
        if let Some(icon) = activity_icon {
            content.addSubview(&icon);
        }
        content.addSubview(&activity_title);
        content.addSubview(&status);
        content.addSubview(&composer_card);
        if let Some(icon) = composer_icon {
            content.addSubview(&icon);
        }
        content.addSubview(&composer_title);
        content.addSubview(&composer_hint);
        content.addSubview(&prompt);
        content.addSubview(&checkout_label);
        content.addSubview(&checkout_next);
        content.addSubview(&agent_label);
        content.addSubview(&agent_next);
        content.addSubview(&option_label);
        content.addSubview(&option_next);
        content.addSubview(&submit);
        panel.setContentView(Some(&content));

        Some(Self {
            panel,
            target,
            status,
            attention,
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
    ) {
        self.set_options(checkouts, agents, options);
        self.set_status(status, attention);
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
        let _ = self.panel.makeFirstResponder(Some(&self.prompt));
    }

    pub(super) fn set_status(&mut self, status: &str, attention: Option<&str>) {
        self.status.setStringValue(&NSString::from_str(status));
        self.status
            .setAccessibilityLabel(Some(&NSString::from_str(status)));
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

    pub(super) fn is_visible(&self) -> bool {
        self.panel.isVisible()
    }

    pub(super) fn is_attention_visible(&self) -> bool {
        self.panel.isVisible() && self.panel.isKeyWindow()
    }

    pub(super) fn is_pinned(&self) -> bool {
        self.pinned
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
