//! Getting your attention from outside the window (ADR 0042): the Dock
//! badge and macOS notifications (`UNUserNotificationCenter`). A click on
//! a notification shows its agent. Main thread only; elsewhere, and for a
//! run outside the app bundle (which UserNotifications refuses), they do
//! nothing.

use std::cell::OnceCell;

use block2::RcBlock;
use gpui::{App, Global};
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

use super::app_state::AppState;
use crate::core::status::{alert_agent, alert_id};

/// Sets (or, with `None`, clears) the Dock icon's badge.
pub fn set_dock_badge(label: Option<&str>) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let tile = NSApplication::sharedApplication(mtm).dockTile();
    tile.setBadgeLabel(label.map(NSString::from_str).as_deref());
}

/// Notifications work only from a bundle (`fleetd.app`); a bare binary
/// (`cargo run`) has no bundle identifier and UserNotifications would
/// raise.
fn bundled() -> bool {
    NSBundle::mainBundle().bundleIdentifier().is_some()
}

struct DelegateIvars {
    clicks: async_channel::Sender<String>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `Delegate` does
    // not implement `Drop`. UserNotifications calls it on the main thread
    // (the app's main queue).
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FleetNotificationDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    // SAFETY: the methods have the signatures the protocol declares.
    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        /// Shown even while the app is frontmost: they are only sent while
        /// the window is not focused.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &block2::DynBlock<dyn Fn()>,
        ) {
            let id = response.notification().request().identifier().to_string();
            // Unbounded: fails only once the app is shutting down.
            let _ = self.ivars().clicks.try_send(id);
            completion_handler.call(());
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker, ivars: DelegateIvars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSObject's `init` on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

/// Keeps the notification delegate alive (the centre holds it weakly).
struct Notifier {
    _delegate: Retained<Delegate>,
    /// Authorization was asked for this run.
    asked: OnceCell<()>,
}

impl Global for Notifier {}

/// Sets the notification centre's delegate, whose clicks show the agent.
/// Call once on the main thread, inside `Application::run`.
pub fn install(cx: &mut App) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    if !bundled() {
        return;
    }
    let (clicks, rx) = async_channel::unbounded::<String>();
    cx.spawn(async move |cx| {
        while let Ok(id) = rx.recv().await {
            if cx.update(|cx| clicked(&id, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
    let delegate = Delegate::new(mtm, DelegateIvars { clicks });
    UNUserNotificationCenter::currentNotificationCenter()
        .setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    cx.set_global(Notifier {
        _delegate: delegate,
        asked: OnceCell::new(),
    });
}

/// Shows the clicked notification's agent, if it is of this session.
fn clicked(identifier: &str, cx: &mut App) {
    let Some((epoch, agent_id)) = alert_agent(identifier) else {
        return;
    };
    let Some(app) = AppState::global(cx) else {
        return;
    };
    if app.read(cx).session_epoch() != epoch {
        return;
    }
    super::resident::show_agent(agent_id, cx);
}

/// Asks macOS for permission to notify (once per run; macOS asks the user
/// only the first time). Without the bundle it says why there will be none.
pub fn request_permission(cx: &App) {
    if MainThreadMarker::new().is_none() {
        return;
    }
    let Some(notifier) = cx.try_global::<Notifier>() else {
        eprintln!("fleet-gui: notifications need the app bundle (fleetd.app)");
        return;
    };
    if notifier.asked.set(()).is_err() {
        return;
    }
    let done = RcBlock::new(|granted: Bool, _error: *mut NSError| {
        if !granted.as_bool() {
            eprintln!("fleet-gui: notifications are not allowed (System Settings › Notifications)");
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &done,
        );
}

/// Posts a notification about agent `agent_id` of session `epoch`,
/// replacing the agent's previous one.
pub fn post(epoch: u64, agent_id: u64, title: &str, body: &str, cx: &App) {
    if MainThreadMarker::new().is_none() || cx.try_global::<Notifier>().is_none() {
        return;
    }
    request_permission(cx);
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&alert_id(epoch, agent_id)),
        &content,
        None,
    );
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, None);
}
