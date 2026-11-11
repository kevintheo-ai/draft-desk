//! Notifications through UserNotifications. A process without a bundle identifier (the test binaries, the bare
//! binary started from a terminal) gets none: `currentNotificationCenter` raises there.

use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2_foundation::{NSBundle, NSDictionary, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
    UNNotificationResponse, UNUserNotificationCenter,
};

use crate::app::Delegate;
use crate::log::log;

/// The user-info key that names the draft a notification is about.
const ITEM_KEY: &str = "item_id";

static ALLOWED: AtomicBool = AtomicBool::new(false);

/// A line for a notification or a hint: no invisible characters, at most 200 characters (as in the Python version).
pub fn clean(s: &str) -> String {
    draftdesk_core::text::strip_forbidden(s)
        .chars()
        .take(200)
        .collect()
}

/// Notifications need an app bundle.
pub fn available() -> bool {
    NSBundle::mainBundle().bundleIdentifier().is_some()
}

/// Whether the person allowed notifications; false until the answer to `request` has arrived.
pub fn allowed() -> bool {
    ALLOWED.load(Ordering::SeqCst)
}

/// A notification would be shown now.
pub fn works() -> bool {
    available() && allowed()
}

/// The app hears of clicks on its notifications. Set before the launch ends, so a click that started the app is
/// heard too.
pub fn set_delegate(delegate: &Delegate) {
    if available() {
        UNUserNotificationCenter::currentNotificationCenter()
            .setDelegate(Some(ProtocolObject::from_ref(delegate)));
    }
}

/// The draft a clicked notification is about.
pub fn item_of(response: &UNNotificationResponse) -> Option<String> {
    let info = response.notification().request().content().userInfo();
    let value = info.objectForKey(&NSString::from_str(ITEM_KEY))?;
    value.downcast::<NSString>().ok().map(|s| s.to_string())
}

fn describe(err: *mut NSError) -> String {
    if err.is_null() {
        return String::new();
    }
    // SAFETY: a non-null NSError handed to a completion handler is valid for the call.
    unsafe { (*err).localizedDescription().to_string() }
}

/// Asks for permission; the system shows its dialog the first time only. The answer arrives later, on another
/// thread.
pub fn request() {
    if !available() {
        return;
    }
    let done = RcBlock::new(|granted: Bool, err: *mut NSError| {
        ALLOWED.store(granted.as_bool(), Ordering::SeqCst);
        log(&format!(
            "notifications allowed: {} {}",
            granted.as_bool(),
            describe(err)
        ));
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert, &done);
}

/// Shows a notification; `key` replaces an earlier one with the same key; a click on it selects `item`.
pub fn post(title: &str, body: &str, key: &str, item: Option<&str>) {
    if !available() {
        return;
    }
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    if let Some(id) = item {
        let value = NSString::from_str(id);
        let info = NSDictionary::<NSString, AnyObject>::from_slices(
            &[&*NSString::from_str(ITEM_KEY)],
            &[value.as_ref()],
        );
        // SAFETY: the same dictionary, only its key and value types widened; strings are property-list values, as user
        // info must hold.
        let info: Retained<NSDictionary> = unsafe { Retained::cast_unchecked(info) };
        // SAFETY: as above.
        unsafe { content.setUserInfo(&info) };
    }
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(key),
        &content,
        None,
    );
    let done = RcBlock::new(|err: *mut NSError| {
        if !err.is_null() {
            log(&format!("notification not shown: {}", describe(err)));
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, Some(&done));
}
