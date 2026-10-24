//! Small nonmodal AppKit feedback panel for explicit tray actions.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSBackingStoreType, NSColor, NSFont, NSLineBreakMode, NSPanel,
    NSProgressIndicator, NSProgressIndicatorStyle, NSTextField, NSView, NSWindowStyleMask,
    NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURL};

const WIDTH: f64 = 440.0;
const HEIGHT: f64 = 220.0;

pub(super) struct NativeFeedback {
    panel: Retained<NSPanel>,
    heading: Retained<NSTextField>,
    body: Retained<NSTextField>,
    progress: Retained<NSProgressIndicator>,
    shown: bool,
    dismissed: bool,
}

impl NativeFeedback {
    /// Creates the retained panel and its controls. Call only from the AppKit main thread.
    pub(super) fn new() -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: `panel` is retained by this struct, so AppKit must not release it on close.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setTitle(&NSString::from_str("AgentSCM"));
        panel.setHidesOnDeactivate(false);

        let content = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT)),
        );
        let heading = NSTextField::labelWithString(&NSString::new(), mtm);
        heading.setFrame(NSRect::new(
            NSPoint::new(24.0, 162.0),
            NSSize::new(WIDTH - 48.0, 26.0),
        ));
        heading.setFont(Some(&NSFont::boldSystemFontOfSize(15.0)));
        heading.setTextColor(Some(&NSColor::labelColor()));
        heading.setAccessibilityLabel(Some(&NSString::from_str("AgentSCM action status")));

        let body = NSTextField::labelWithString(&NSString::new(), mtm);
        body.setFrame(NSRect::new(
            NSPoint::new(24.0, 52.0),
            NSSize::new(WIDTH - 48.0, 100.0),
        ));
        body.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        body.setTextColor(Some(&NSColor::labelColor()));
        body.setPreferredMaxLayoutWidth(WIDTH - 48.0);
        body.setMaximumNumberOfLines(0);
        body.setUsesSingleLineMode(false);
        body.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        body.setSelectable(true);

        let progress = NSProgressIndicator::initWithFrame(
            NSProgressIndicator::alloc(mtm),
            NSRect::new(NSPoint::new(24.0, 22.0), NSSize::new(20.0, 20.0)),
        );
        progress.setStyle(NSProgressIndicatorStyle::Spinning);
        progress.setIndeterminate(true);
        progress.setDisplayedWhenStopped(false);

        content.addSubview(&heading);
        content.addSubview(&body);
        content.addSubview(&progress);
        panel.setContentView(Some(&content));

        Some(Self {
            panel,
            heading,
            body,
            progress,
            shown: false,
            dismissed: false,
        })
    }

    /// Starts a new explicit action and brings its nonmodal progress panel forward.
    pub(super) fn show(&mut self, title: &str, body: &str, busy: bool) {
        self.dismissed = false;
        self.render(title, body, busy);
        if !self.shown {
            self.panel.center();
        }
        self.shown = true;
        self.panel.makeKeyAndOrderFront(None);
        self.panel.orderFrontRegardless();
    }

    /// Updates an action already shown. A user-closed panel remains closed until the next action.
    pub(super) fn update(&mut self, title: &str, body: &str, busy: bool) {
        if self.shown && !self.panel.isVisible() {
            self.dismissed = true;
        }
        if self.dismissed {
            return;
        }
        self.render(title, body, busy);
    }

    fn render(&self, title: &str, body: &str, busy: bool) {
        self.heading.setStringValue(&NSString::from_str(title));
        self.heading
            .setAccessibilityLabel(Some(&NSString::from_str(title)));
        self.body.setStringValue(&NSString::from_str(body));
        self.body
            .setAccessibilityLabel(Some(&NSString::from_str(body)));
        if busy {
            // SAFETY: AppKit requires animation messages on the main thread; construction and all
            // methods are main-thread-only through the retained AppKit object types.
            unsafe { self.progress.startAnimation(None) };
        } else {
            // SAFETY: same main-thread AppKit requirement as `startAnimation` above.
            unsafe { self.progress.stopAnimation(None) };
        }
    }
}

/// Opens a URL through Launch Services without exposing it in a child-process argument list.
pub(super) fn open_url(url: &str) -> Result<(), &'static str> {
    let url = NSURL::URLWithString(&NSString::from_str(url)).ok_or("invalid URL")?;
    if NSWorkspace::sharedWorkspace().openURL(&url) {
        Ok(())
    } else {
        Err("could not open URL")
    }
}
