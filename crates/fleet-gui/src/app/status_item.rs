//! The menu-bar status item and the activation policy, in AppKit (objc2;
//! the rest of the app's AppKit calls are in `native`). The menu is rebuilt from
//! [`status_menu`] each time it opens; a pick goes into a channel that a
//! foreground gpui task hands to [`super::resident::handle`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gpui::{App, Global};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSControlStateValueOff, NSControlStateValueOn,
    NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSObject, NSObjectProtocol, NSSize, NSString, ns_string};

use super::app_state::AppState;
use super::resident;
use crate::core::status::badge_label;
use crate::core::status_menu::{StatusEntry, StatusMenuState, status_menu};

/// The 18 pt template mark at 2x (rendered from `fleetd-mark.svg`).
const ICON: &[u8] = include_bytes!("../../../../assets/logo/fleetd-mark-18@2x.png");

struct TargetIvars {
    /// Kept current by an observer of [`AppState`].
    state: Rc<RefCell<StatusMenuState>>,
    /// The entries of the menu as last built; an item's tag indexes it.
    shown: RefCell<Vec<StatusEntry>>,
    picks: async_channel::Sender<StatusEntry>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `MenuTarget`
    // does not implement `Drop`. It is main-thread only, like the menu.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FleetStatusMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        // SAFETY: the selector takes one object argument (the sender) and
        // returns nothing, as the signature says.
        #[unsafe(method(pick:))]
        fn pick(&self, sender: Option<&NSMenuItem>) {
            let Some(item) = sender else { return };
            let entry = usize::try_from(item.tag())
                .ok()
                .and_then(|i| self.ivars().shown.borrow().get(i).cloned());
            if let Some(entry) = entry {
                // Unbounded: fails only once the app is shutting down.
                let _ = self.ivars().picks.try_send(entry);
            }
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}

    // SAFETY: `menuNeedsUpdate:` has the signature AppKit expects.
    unsafe impl NSMenuDelegate for MenuTarget {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.rebuild(menu);
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, ivars: TargetIvars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSObject's `init` on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }

    fn rebuild(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        let entries = status_menu(&self.ivars().state.borrow());
        menu.removeAllItems();
        for (i, entry) in entries.iter().enumerate() {
            let item = match entry {
                StatusEntry::Separator => NSMenuItem::separatorItem(mtm),
                StatusEntry::Header(text) => {
                    let item = menu_item(mtm, text, None);
                    item.setEnabled(false);
                    item
                }
                _ => {
                    let item = menu_item(mtm, entry.title(), Some(self));
                    item.setTag(i as isize);
                    if let StatusEntry::Notifications(on) = entry {
                        item.setState(if *on {
                            NSControlStateValueOn
                        } else {
                            NSControlStateValueOff
                        });
                    }
                    item
                }
            };
            menu.addItem(&item);
        }
        *self.ivars().shown.borrow_mut() = entries;
    }
}

/// An item calling `pick:` on `target` (none: a disabled line).
fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    target: Option<&MenuTarget>,
) -> Retained<NSMenuItem> {
    let action = target.map(|_| sel!(pick:));
    // SAFETY: `pick:` is implemented by `MenuTarget`, the item's target.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            ns_string!(""),
        )
    };
    if let Some(target) = target {
        let target: &AnyObject = target;
        // SAFETY: the target is retained by [`StatusItem`] for the life of
        // the app, so the item's unretained reference never dangles.
        unsafe { item.setTarget(Some(target)) };
    }
    item
}

/// Keeps the status item (and the menu's target) alive.
struct StatusItem {
    _item: Retained<NSStatusItem>,
    _menu: Retained<NSMenu>,
    _target: Retained<MenuTarget>,
}

impl Global for StatusItem {}

/// Puts the fleet mark in the menu bar with the status menu. Call on the
/// main thread, inside `Application::run`.
pub fn install_status_item(cx: &mut App) {
    let Some(mtm) = MainThreadMarker::new() else {
        debug_assert!(false, "install_status_item off the main thread");
        return;
    };
    let Some(app) = AppState::global(cx) else {
        return;
    };
    let state = Rc::new(RefCell::new(resident::menu_state(app.read(cx))));
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let shared = state.clone();
    let observed = item.clone();
    // The needs-you count, last shown (ADR 0042).
    let shown_count = Cell::new(0usize);
    cx.observe(&app, move |app, cx| {
        let a = app.read(cx);
        *shared.borrow_mut() = resident::menu_state(a);
        let count = a.needs_you_count();
        if shown_count.replace(count) != count {
            show_count(&observed, mtm, count);
        }
    })
    .detach();
    // Statuses age (working turns quiet) with no event: while no window's
    // ticker does it, this one does.
    let ticking = app.downgrade();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = ticking.update(cx, |a, cx| {
                if a.window.is_none() {
                    a.refresh_status(cx);
                }
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();

    let (picks, rx) = async_channel::unbounded();
    cx.spawn(async move |cx| {
        while let Ok(entry) = rx.recv().await {
            if cx.update(|cx| resident::handle(entry, cx)).is_err() {
                break;
            }
        }
    })
    .detach();

    let target = MenuTarget::new(
        mtm,
        TargetIvars {
            state,
            shown: RefCell::new(Vec::new()),
            picks,
        },
    );
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    if let Some(button) = item.button(mtm) {
        let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(ICON));
        if let Some(image) = image {
            image.setSize(NSSize::new(18.0, 18.0));
            image.setTemplate(true);
            button.setImage(Some(&image));
        }
        button.setToolTip(Some(ns_string!("fleet")));
    }
    item.setMenu(Some(&menu));
    cx.set_global(StatusItem {
        _item: item,
        _menu: menu,
        _target: target,
    });
}

/// The needs-you count as the status item's title beside the mark, and
/// as the Dock badge (none at 0).
fn show_count(item: &NSStatusItem, mtm: MainThreadMarker, count: usize) {
    let label = badge_label(count);
    if let Some(button) = item.button(mtm) {
        button.setTitle(&NSString::from_str(label.as_deref().unwrap_or("")));
    }
    super::attention::set_dock_badge(label.as_deref());
}

/// `Accessory` (no Dock icon, no app menu) while no window is open,
/// `Regular` while one is. Main thread only; elsewhere it does nothing.
pub fn set_accessory(accessory: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        debug_assert!(false, "set_accessory off the main thread");
        return;
    };
    let policy = if accessory {
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Regular
    };
    NSApplication::sharedApplication(mtm).setActivationPolicy(policy);
}
