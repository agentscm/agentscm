use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSControlStateValueOff, NSControlStateValueOn, NSMenu,
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
    _view: Option<Retained<NSView>>,
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
        let (switch, view) = if AnyClass::get(c"NSSwitch").is_some() {
            let view = NSView::initWithFrame(
                NSView::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(280.0, 32.0)),
            );
            let label = NSTextField::labelWithString(&NSString::from_str("Start at Login"), mtm);
            label.setFrame(NSRect::new(
                NSPoint::new(16.0, 7.0),
                NSSize::new(200.0, 18.0),
            ));
            let switch = NSSwitch::initWithFrame(
                NSSwitch::alloc(mtm),
                NSRect::new(NSPoint::new(224.0, 4.0), NSSize::new(44.0, 24.0)),
            );
            switch.setState(if enabled {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            switch.setAccessibilityLabel(Some(&NSString::from_str("Start at Login")));
            // SAFETY: Target implements loginSwitchChanged: with the NSSwitch sender signature and
            // is retained by this bridge for at least as long as the switch.
            unsafe {
                switch.setTarget(Some(&target));
                switch.setAction(Some(sel!(loginSwitchChanged:)));
                view.addSubview(&label);
                view.addSubview(&switch);
            }
            item.setView(Some(&view));
            item.view()?;
            (Some(switch), Some(view))
        } else {
            (None, None)
        };

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
            _view: view,
            tracking,
        })
    }

    pub(super) fn is_tracking(&self) -> bool {
        self.tracking.load(Ordering::Acquire)
    }

    pub(super) fn set_enabled(&self, enabled: bool) {
        if let Some(switch) = &self.switch {
            switch.setState(if enabled {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
    }

    pub(super) fn is_installed(&self) -> bool {
        self.switch.is_none() || self.item.view().is_some()
    }
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
}
