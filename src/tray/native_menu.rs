use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSColor, NSControlStateValueOff, NSControlStateValueOn, NSFont,
    NSFontWeightSemibold, NSImage, NSImageView, NSLineBreakMode, NSMenu,
    NSMenuDidBeginTrackingNotification, NSMenuDidEndTrackingNotification, NSMenuItem, NSSwitch,
    NSTextField, NSView,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString,
};
use winit::event_loop::EventLoopProxy;

use super::UserEvent;
use tray_icon::menu::ContextMenu;

#[derive(Debug)]
struct TargetIvars {
    proxy: EventLoopProxy<UserEvent>,
    tracking: Arc<AtomicBool>,
}

objc2::define_class!(
    #[name = "AgentSCMTrayMenuTarget"]
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = TargetIvars]
    struct Target;

    unsafe impl NSObjectProtocol for Target {}

    impl Target {
        #[unsafe(method(loginSwitchChanged:))]
        fn login_switch_changed(&self, sender: &NSSwitch) {
            let desired = sender.state() == NSControlStateValueOn;
            let _ = self.ivars().proxy.send_event(UserEvent::LaunchLogin(desired));
        }

        #[unsafe(method(menuBeganTracking:))]
        fn menu_began_tracking(&self, _notification: &NSNotification) {
            set_tracking(&self.ivars().tracking, true);
        }

        #[unsafe(method(menuEndedTracking:))]
        fn menu_ended_tracking(&self, _notification: &NSNotification) {
            set_tracking(&self.ivars().tracking, false);
            let _ = self.ivars().proxy.send_event(UserEvent::MenuTrackingEnded);
        }
    }
);

fn set_tracking(tracking: &AtomicBool, value: bool) {
    tracking.store(value, Ordering::Release);
}

impl Target {
    fn new(
        mtm: MainThreadMarker,
        proxy: EventLoopProxy<UserEvent>,
        tracking: Arc<AtomicBool>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { proxy, tracking });
        // SAFETY: NSObject's initializer has this signature and Target adds no init requirements.
        unsafe { msg_send![super(this), init] }
    }
}

pub(super) struct NativeMenuBridge {
    menu: Retained<NSMenu>,
    item: Retained<NSMenuItem>,
    target: Retained<Target>,
    switch: Option<Retained<NSSwitch>>,
    login_state: Option<Retained<NSTextField>>,
    _view: Option<Retained<NSView>>,
    _text_views: Vec<Retained<NSView>>,
    tracking: Arc<AtomicBool>,
}

impl NativeMenuBridge {
    pub(super) fn attach(
        source: &tray_icon::menu::Menu,
        proxy: EventLoopProxy<UserEvent>,
        enabled: bool,
    ) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let pointer = source.ns_menu().cast::<NSMenu>();
        // SAFETY: Muda documents this pointer as its live NSMenu for as long as `source` exists.
        // Retaining it extends that exact menu's lifetime after `source` moves into tray-icon.
        let menu = unsafe { Retained::retain(pointer) }?;
        let item = menu.itemWithTitle(&NSString::from_str("Start at Login (configured)"))?;
        let tracking = Arc::new(AtomicBool::new(false));
        let target = Target::new(mtm, proxy, tracking.clone());

        // NSSwitch was introduced in macOS 10.15, while AgentSCM still supports older macOS.
        // Looking up the class first avoids messaging an unavailable class; the existing Muda
        // CheckMenuItem remains the accessible legacy control when the lookup fails.
        let (switch, login_state, view) = if AnyClass::get(c"NSSwitch").is_some() {
            let view = NSView::initWithFrame(
                NSView::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(280.0, 32.0)),
            );
            let label = NSTextField::labelWithString(&NSString::from_str("Start at Login"), mtm);
            label.setFrame(NSRect::new(
                NSPoint::new(16.0, 7.0),
                NSSize::new(130.0, 18.0),
            ));
            let login_state = NSTextField::labelWithString(&NSString::new(), mtm);
            login_state.setFrame(NSRect::new(
                NSPoint::new(148.0, 7.0),
                NSSize::new(70.0, 18.0),
            ));
            login_state.setFont(Some(&NSFont::systemFontOfSize(12.0)));
            let switch = NSSwitch::initWithFrame(
                NSSwitch::alloc(mtm),
                NSRect::new(NSPoint::new(224.0, 4.0), NSSize::new(44.0, 24.0)),
            );
            switch.setAccessibilityLabel(Some(&NSString::from_str("Start at Login")));
            apply_login_state(&switch, &login_state, NativeLoginState::Confirmed(enabled));
            // SAFETY: Target implements loginSwitchChanged: with the NSSwitch sender signature and
            // is retained by this bridge for at least as long as the switch.
            unsafe {
                switch.setTarget(Some(&target));
                switch.setAction(Some(sel!(loginSwitchChanged:)));
                view.addSubview(&label);
                view.addSubview(&login_state);
                view.addSubview(&switch);
            }
            item.setView(Some(&view));
            item.view()?;
            (Some(switch), Some(login_state), Some(view))
        } else {
            (None, None, None)
        };
        let mut text_views = Vec::new();
        customize_menu(&menu, mtm, &mut text_views);

        let center = NSNotificationCenter::defaultCenter();
        // SAFETY: Target implements both selectors with NSNotification arguments, and observer,
        // menu, and notification names remain valid until Drop unregisters the observer.
        unsafe {
            center.addObserver_selector_name_object(
                &target,
                sel!(menuBeganTracking:),
                Some(NSMenuDidBeginTrackingNotification),
                Some(&menu),
            );
            center.addObserver_selector_name_object(
                &target,
                sel!(menuEndedTracking:),
                Some(NSMenuDidEndTrackingNotification),
                Some(&menu),
            );
        }
        Some(Self {
            menu,
            item,
            target,
            switch,
            login_state,
            _view: view,
            _text_views: text_views,
            tracking,
        })
    }

    pub(super) fn is_tracking(&self) -> bool {
        self.tracking.load(Ordering::Acquire)
    }

    pub(super) fn set_enabled(&self, enabled: bool) {
        self.set_login_state(NativeLoginState::Confirmed(enabled));
    }

    pub(super) fn set_login_state(&self, state: NativeLoginState) {
        if let (Some(switch), Some(label)) = (&self.switch, &self.login_state) {
            apply_login_state(switch, label, state);
        }
    }

    pub(super) fn is_installed(&self) -> bool {
        self.switch.is_none() || self.item.view().is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NativeLoginState {
    Confirmed(bool),
    Pending(bool),
}

fn apply_login_state(switch: &NSSwitch, label: &NSTextField, state: NativeLoginState) {
    let (enabled, checked, text, color) = match state {
        NativeLoginState::Confirmed(true) => (true, true, "On", NSColor::controlAccentColor()),
        NativeLoginState::Confirmed(false) => (true, false, "Off", NSColor::secondaryLabelColor()),
        NativeLoginState::Pending(checked) => {
            (false, checked, "Updating…", NSColor::secondaryLabelColor())
        }
    };
    switch.setEnabled(enabled);
    switch.setState(if checked {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    switch.setAccessibilityValueDescription(Some(&NSString::from_str(text)));
    label.setStringValue(&NSString::from_str(text));
    label.setTextColor(Some(&color));
    label.setAccessibilityLabel(Some(&NSString::from_str(&format!(
        "Start at Login: {text}"
    ))));
}

const TEXT_MARKER: &str = "__AGENTSCM_TEXT_";

fn customize_menu(menu: &NSMenu, mtm: MainThreadMarker, views: &mut Vec<Retained<NSView>>) {
    for index in 0..menu.numberOfItems() {
        let Some(item) = menu.itemAtIndex(index) else {
            continue;
        };
        let title = item.title().to_string();
        let submenu = item.submenu();
        if let Some((role, text)) = text_item(&title, item.isEnabled(), submenu.is_some()) {
            let view = text_view(role, text, mtm);
            item.setView(Some(&view));
            item.setAccessibilityLabel(Some(&NSString::from_str(text)));
            views.push(view);
        } else if let Some(symbol) = menu_item_symbol(
            &title,
            submenu.as_deref().and_then(submenu_status_symbol),
            submenu.is_some(),
        ) {
            set_symbol(&item, symbol);
        }
        if let Some(submenu) = submenu {
            customize_menu(&submenu, mtm, views);
        }
    }
}

fn parse_text_marker(title: &str) -> Option<(&str, &str)> {
    let rest = title.strip_prefix(TEXT_MARKER)?;
    rest.split_once("__")
}

fn text_item(title: &str, enabled: bool, has_submenu: bool) -> Option<(&str, &str)> {
    (!enabled && !has_submenu)
        .then(|| parse_text_marker(title))
        .flatten()
}

fn text_view(role: &str, text: &str, mtm: MainThreadMarker) -> Retained<NSView> {
    let width = if role == "details" { 344.0 } else { 328.0 };
    if role == "summary" {
        return summary_view(text, width, mtm);
    }
    let font = NSFont::systemFontOfSize(12.0);
    let label_width = width - 28.0;
    let (label, label_height) = measured_label(text, &font, label_width, mtm);
    let height = label_height + 14.0;
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)),
    );
    label.setFrame(NSRect::new(
        NSPoint::new(14.0, 7.0),
        NSSize::new(label_width, label_height),
    ));
    label.setSelectable(role == "details");
    label.setAccessibilityLabel(Some(&NSString::from_str(text)));
    view.addSubview(&label);
    view
}

fn measured_label(
    text: &str,
    font: &NSFont,
    width: f64,
    mtm: MainThreadMarker,
) -> (Retained<NSTextField>, f64) {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setPreferredMaxLayoutWidth(width);
    label.setMaximumNumberOfLines(0);
    label.setUsesSingleLineMode(false);
    label.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    label.setTextColor(Some(&NSColor::labelColor()));
    label.setFont(Some(font));
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, f64::MAX));
    let height = label
        .cell()
        .map(|cell| cell.cellSizeForBounds(bounds).height.max(17.0))
        .unwrap_or(17.0);
    (label, height)
}

fn summary_view(text: &str, width: f64, mtm: MainThreadMarker) -> Retained<NSView> {
    let (repository, status) = text
        .split_once('\n')
        .unwrap_or((text, "Connection: unknown"));
    // SAFETY: AppKit exports this immutable process-lifetime font-weight constant.
    let heading_font = NSFont::systemFontOfSize_weight(13.0, unsafe { NSFontWeightSemibold });
    let status_font = NSFont::systemFontOfSize(12.0);
    let label_width = width - 54.0;
    let (heading, heading_height) = measured_label(repository, &heading_font, label_width, mtm);
    let (status_label, status_height) = measured_label(status, &status_font, label_width, mtm);
    let height = heading_height + status_height + 14.0;
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)),
    );
    status_label.setFrame(NSRect::new(
        NSPoint::new(40.0, 7.0),
        NSSize::new(label_width, status_height),
    ));
    heading.setFrame(NSRect::new(
        NSPoint::new(40.0, 7.0 + status_height),
        NSSize::new(label_width, heading_height),
    ));
    heading.setAccessibilityLabel(Some(&NSString::from_str(repository)));
    status_label.setAccessibilityLabel(Some(&NSString::from_str(status)));
    view.addSubview(&heading);
    view.addSubview(&status_label);
    if let Some(image) = status_symbol(status) {
        let image_view = NSImageView::initWithFrame(
            NSImageView::alloc(mtm),
            NSRect::new(
                NSPoint::new(16.0, (height - 16.0) / 2.0),
                NSSize::new(16.0, 16.0),
            ),
        );
        image_view.setImage(Some(&image));
        view.addSubview(&image_view);
    }
    view.setAccessibilityLabel(Some(&NSString::from_str(text)));
    view
}

fn status_symbol(status: &str) -> Option<Retained<NSImage>> {
    system_symbol(status_symbol_name(status))
}

fn status_symbol_name(status: &str) -> &'static str {
    if status == "Hub recently reachable" {
        "checkmark.circle"
    } else if status == "Connection: attention needed" {
        "exclamationmark.triangle"
    } else {
        "questionmark.circle"
    }
}

fn submenu_status_symbol(menu: &NSMenu) -> Option<&'static str> {
    let summary = menu.itemAtIndex(0)?;
    let title = summary.title().to_string();
    let (role, text) = text_item(&title, summary.isEnabled(), summary.submenu().is_some())?;
    (role == "summary")
        .then(|| {
            text.split_once('\n')
                .map(|(_, status)| status_symbol_name(status))
        })
        .flatten()
}

fn symbol_for_title(title: &str) -> Option<&'static str> {
    match title {
        "Open Checkout" => Some("folder"),
        "Open Hub" => Some("network"),
        "Agent Integrations" => Some("puzzlepiece.extension"),
        "Copy Terminal Setup" => Some("terminal"),
        "Connection Details" => Some("info.circle"),
        "Recent Activity" => Some("clock"),
        "Troubleshooting" => Some("wrench.and.screwdriver"),
        "Hub Sign-in" => Some("key"),
        "Copy GitHub Sign-in" | "Copy GitLab Sign-in" => Some("terminal"),
        "Copy Diagnostics" => Some("doc.on.doc"),
        "Copy Reconnect" => Some("arrow.triangle.2.circlepath"),
        "Copy Guidance Preview" => Some("text.book.closed"),
        "Check Now" => Some("arrow.clockwise"),
        "Quit AgentSCM Tray" => Some("power"),
        _ if title.starts_with("Set Up ") => Some("plus.app"),
        _ if title.starts_with("Reapply ") => Some("arrow.clockwise"),
        _ => None,
    }
}

fn menu_item_symbol(
    title: &str,
    submenu_status: Option<&'static str>,
    has_submenu: bool,
) -> Option<&'static str> {
    submenu_status
        .or_else(|| symbol_for_title(title))
        .or_else(|| has_submenu.then_some("folder"))
}

fn set_symbol(item: &NSMenuItem, symbol: &str) {
    if let Some(image) = system_symbol(symbol) {
        item.setImage(Some(&image));
    }
}

fn system_symbol(symbol: &str) -> Option<Retained<NSImage>> {
    let available = AnyClass::get(c"NSImage").is_some_and(|class| {
        class
            .metaclass()
            .responds_to(sel!(imageWithSystemSymbolName:accessibilityDescription:))
    });
    if !available {
        return None;
    }
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(symbol),
        None,
    )?;
    image.setTemplate(true);
    image.setSize(NSSize::new(16.0, 16.0));
    Some(image)
}

impl Drop for NativeMenuBridge {
    fn drop(&mut self) {
        // SAFETY: target was registered with this center and is still retained here.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(self.target.as_ref()) };
        let _ = &self.menu;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_callbacks_share_the_tracking_state_used_by_menu_gating() {
        let tracking = AtomicBool::new(false);
        set_tracking(&tracking, true);
        assert!(tracking.load(Ordering::Acquire));
        set_tracking(&tracking, false);
        assert!(!tracking.load(Ordering::Acquire));
    }

    #[test]
    fn native_text_markers_and_symbols_use_stable_menu_copy() {
        assert_eq!(
            parse_text_marker("__AGENTSCM_TEXT_details__Path  /tmp/repo"),
            Some(("details", "Path  /tmp/repo"))
        );
        assert_eq!(symbol_for_title("Open Checkout"), Some("folder"));
        assert_eq!(symbol_for_title("Connection: unknown"), None);
        assert_eq!(
            menu_item_symbol("Open Hub", Some("exclamationmark.triangle"), true),
            Some("exclamationmark.triangle"),
            "checkout health wins even when its name collides with an action"
        );
        assert_eq!(menu_item_symbol("Open Hub", None, false), Some("network"));
        assert_eq!(
            menu_item_symbol("Other Submenu", None, true),
            Some("folder")
        );
        assert_eq!(
            status_symbol_name("Hub recently reachable"),
            "checkmark.circle"
        );
        assert_eq!(
            status_symbol_name("Connection: attention needed"),
            "exclamationmark.triangle"
        );
        assert_eq!(
            status_symbol_name("Connection: unknown"),
            "questionmark.circle"
        );
        assert_eq!(
            text_item("__AGENTSCM_TEXT_details__Path  /tmp/repo", true, true),
            None,
            "an enabled checkout submenu title is never reinterpreted"
        );
    }
}
