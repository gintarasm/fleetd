//! Native context menus (`NSMenu`, objc2). gpui 0.2.2 has none: it only
//! draws its own popovers.
//!
//! A menu is built at the right-click but shown a run-loop turn later
//! (`performSelector:withObject:afterDelay:`), outside gpui's event
//! dispatch: AppKit runs its own tracking loop while the menu is up, and
//! must not do that while gpui is mid-update (its frame callbacks would
//! re-enter the app). A pick goes through a channel to a foreground task,
//! which calls the callback given with the menu.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{App, Global};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSControlStateValueOn, NSEvent, NSEventModifierFlags, NSEventType, NSMenu,
    NSMenuItem, NSView,
};
use objc2_foundation::{NSObject, NSObjectNSDelayedPerforming, NSObjectProtocol, NSString};

use crate::core::menus::{MenuEntry, Shortcut};

/// One line of a menu as shown.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuLine {
    pub title: String,
    pub enabled: bool,
    pub shortcut: Option<Shortcut>,
    /// Drawn with a check mark (the current choice).
    pub checked: bool,
}

/// A menu waiting for its run-loop turn.
struct Pending {
    menu: Retained<NSMenu>,
    event: Retained<NSEvent>,
    view: Retained<NSView>,
}

struct TargetIvars {
    /// Set by [`show`], taken by `popUp:`.
    pending: RefCell<Option<Pending>>,
    /// The menu last shown; a pick names it, so a late pick of an older
    /// menu is dropped.
    token: Cell<u64>,
    picks: async_channel::Sender<(u64, usize)>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the target
    // does not implement `Drop`. It is main-thread only, like its menus.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FleetContextMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        // SAFETY: one object argument (the sender, unused), no return.
        #[unsafe(method(popUp:))]
        fn pop_up(&self, _sender: Option<&AnyObject>) {
            let Some(p) = self.ivars().pending.borrow_mut().take() else {
                return;
            };
            // Runs AppKit's tracking loop until the menu closes.
            NSMenu::popUpContextMenu_withEvent_forView(&p.menu, &p.event, &p.view);
        }

        // SAFETY: one object argument (the sender), no return.
        #[unsafe(method(pick:))]
        fn pick(&self, sender: Option<&NSMenuItem>) {
            let Some(item) = sender else { return };
            if let Ok(index) = usize::try_from(item.tag()) {
                // Unbounded: fails only once the app is shutting down.
                let _ = self.ivars().picks.try_send((self.ivars().token.get(), index));
            }
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, ivars: TargetIvars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSObject's `init` on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

type OnPick = Box<dyn FnOnce(usize, &mut App)>;

/// The app's menu target and the callback of the menu last shown.
struct ContextMenus {
    target: Retained<MenuTarget>,
    on_pick: Rc<RefCell<Option<(u64, OnPick)>>>,
}

impl Global for ContextMenus {}

/// Sets up native context menus. Call once, on the main thread, inside
/// `Application::run`; without it (tests) [`show`] shows nothing.
pub fn install(cx: &mut App) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let (picks, rx) = async_channel::unbounded::<(u64, usize)>();
    let on_pick: Rc<RefCell<Option<(u64, OnPick)>>> = Rc::default();
    let slot = on_pick.clone();
    cx.spawn(async move |cx| {
        while let Ok((token, index)) = rx.recv().await {
            let callback = slot.borrow_mut().take_if(|(t, _)| *t == token);
            if let Some((_, callback)) = callback
                && cx.update(|cx| callback(index, cx)).is_err()
            {
                break;
            }
        }
    })
    .detach();
    let target = MenuTarget::new(
        mtm,
        TargetIvars {
            pending: RefCell::new(None),
            token: Cell::new(0),
            picks,
        },
    );
    cx.set_global(ContextMenus { target, on_pick });
}

/// The lines of a menu of `entries`, titled and enabled by `line`.
pub fn lines<T: Copy>(
    entries: &[MenuEntry<T>],
    line: impl Fn(T) -> MenuLine,
) -> Vec<Option<MenuLine>> {
    entries
        .iter()
        .map(|e| match e {
            MenuEntry::Item(item) => Some(line(*item)),
            MenuEntry::Separator => None,
        })
        .collect()
}

/// Shows a context menu of `lines` (`None`: a separator) at the right
/// click being handled, and calls `on_pick` with the index of the line
/// picked, if one is. Returns false if no menu could be shown (no native
/// menus installed, or no mouse event to place it by).
pub fn show(
    lines: Vec<Option<MenuLine>>,
    cx: &mut App,
    on_pick: impl FnOnce(usize, &mut App) + 'static,
) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let Some(menus) = cx.try_global::<ContextMenus>() else {
        return false;
    };
    let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else {
        return false;
    };
    let placed_by = [
        NSEventType::RightMouseDown,
        NSEventType::LeftMouseDown,
        NSEventType::OtherMouseDown,
    ];
    if !placed_by.contains(&event.r#type()) {
        return false;
    }
    let Some(view) = event.window(mtm).and_then(|w| w.contentView()) else {
        return false;
    };
    let target = &menus.target;
    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    for (i, line) in lines.iter().enumerate() {
        let item = match line {
            None => NSMenuItem::separatorItem(mtm),
            Some(line) => {
                let (key, mods) = key_equivalent(line.shortcut);
                // SAFETY: `pick:` is implemented by `MenuTarget`, the
                // item's target.
                let item = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str(&line.title),
                        Some(sel!(pick:)),
                        &NSString::from_str(key),
                    )
                };
                item.setKeyEquivalentModifierMask(mods);
                let receiver: &AnyObject = target;
                // SAFETY: the target lives in a global for the life of
                // the app, so the item's unretained reference never
                // dangles.
                unsafe { item.setTarget(Some(receiver)) };
                item.setTag(i as isize);
                item.setEnabled(line.enabled);
                if line.checked {
                    item.setState(NSControlStateValueOn);
                }
                item
            }
        };
        menu.addItem(&item);
    }
    let token = target.ivars().token.get() + 1;
    target.ivars().token.set(token);
    *menus.on_pick.borrow_mut() = Some((token, Box::new(on_pick)));
    *target.ivars().pending.borrow_mut() = Some(Pending { menu, event, view });
    // SAFETY: `popUp:` takes one object argument, given as nil.
    unsafe { target.performSelector_withObject_afterDelay(sel!(popUp:), None, 0.0) };
    true
}

/// AppKit's key equivalent for a shortcut: the key and its modifiers.
fn key_equivalent(shortcut: Option<Shortcut>) -> (&'static str, NSEventModifierFlags) {
    let Some(s) = shortcut else {
        return ("", NSEventModifierFlags::empty());
    };
    let key = match s.key {
        "enter" => "\r",
        // NSUpArrowFunctionKey, NSDownArrowFunctionKey.
        "up" => "\u{F700}",
        "down" => "\u{F701}",
        k => k,
    };
    let mut mods = NSEventModifierFlags::Command;
    if s.shift {
        mods |= NSEventModifierFlags::Shift;
    }
    (key, mods)
}
