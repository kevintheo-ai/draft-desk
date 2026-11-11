//! The window's toolbar: Check, Fix, Discard, a flexible space, Send (prominent). No item has a key equivalent:
//! sending is a click. The app enables the items from the core's actions.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSToolbar, NSToolbarFlexibleSpaceItemIdentifier, NSToolbarItem, NSToolbarItemIdentifier,
    NSToolbarItemStyle, NSWindow,
};
use objc2_foundation::{NSArray, NSCopying, NSString};

use crate::app::Delegate;
use crate::i18n::tr;
use crate::ui::{ns, symbol};

pub const CHECK: &str = "check";
pub const FIX: &str = "fix";
pub const DISCARD: &str = "discard";
pub const SEND: &str = "send";

/// The items in their order.
pub fn identifiers() -> Retained<NSArray<NSToolbarItemIdentifier>> {
    // SAFETY: a static AppKit constant.
    let flexible: &NSString = unsafe { NSToolbarFlexibleSpaceItemIdentifier };
    let ids = [ns(CHECK), ns(FIX), ns(DISCARD), flexible.copy(), ns(SEND)];
    NSArray::from_retained_slice(&ids)
}

pub fn new(mtm: MainThreadMarker, delegate: &Delegate) -> Retained<NSToolbar> {
    let toolbar = NSToolbar::initWithIdentifier(NSToolbar::alloc(mtm), &ns("DraftDeskToolbar"));
    toolbar.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(delegate)));
    toolbar
}

/// The item for an identifier the toolbar asks for; `None` for unknown ones.
pub fn item(
    mtm: MainThreadMarker,
    delegate: &Delegate,
    id: &NSToolbarItemIdentifier,
) -> Option<Retained<NSToolbarItem>> {
    let (label, image, action): (&str, &str, Sel) = match id.to_string().as_str() {
        CHECK => (tr("Prüfen", "Check"), "checkmark.seal", sel!(checkDraft:)),
        FIX => (tr("Korrigieren", "Fix"), "wand.and.stars", sel!(fixDraft:)),
        DISCARD => (tr("Verwerfen", "Discard"), "trash", sel!(discardDraft:)),
        SEND => (tr("Senden", "Send"), "paperplane", sel!(sendDraft:)),
        _ => return None,
    };
    let item = NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(mtm), id);
    item.setLabel(&ns(label));
    item.setToolTip(Some(&ns(label)));
    item.setImage(symbol(image, label).as_deref());
    let target: &AnyObject = delegate.as_ref();
    // SAFETY: the target is the app delegate, which lives as long as the app.
    unsafe {
        item.setTarget(Some(target));
        item.setAction(Some(action));
    }
    // The app enables the items itself (the core's actions); AppKit's validation would enable them again.
    item.setAutovalidates(false);
    if id.to_string() == SEND {
        item.setStyle(NSToolbarItemStyle::Prominent);
    }
    Some(item)
}

/// Enables the items by identifier.
pub fn set_enabled(window: &NSWindow, states: &[(&str, bool)]) {
    let Some(toolbar) = window.toolbar() else {
        return;
    };
    for item in toolbar.items().iter() {
        let id = item.itemIdentifier().to_string();
        if let Some((_, on)) = states.iter().find(|(s, _)| *s == id) {
            item.setEnabled(*on);
        }
    }
}

/// Whether the item is enabled; `None` without it (tests).
pub fn enabled(window: &NSWindow, id: &str) -> Option<bool> {
    let toolbar = window.toolbar()?;
    toolbar
        .items()
        .iter()
        .find(|i| i.itemIdentifier().to_string() == id)
        .map(|i| i.isEnabled())
}
