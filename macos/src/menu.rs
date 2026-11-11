//! The main menu: app, Edit (without it copy and paste do not work in a window built in code), Draft, Window.
//! Send has no key equivalent anywhere: a click is the only way to send.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::NSString;

use crate::app::Delegate;
use crate::i18n::tr;

/// Enables the items of the app's menu bar with these actions (the Draft menu has no automatic validation).
pub fn set_enabled(mtm: MainThreadMarker, states: &[(Sel, bool)]) {
    fn walk(m: &NSMenu, states: &[(Sel, bool)]) {
        for i in m.itemArray().iter() {
            if let Some(on) = states
                .iter()
                .find(|(s, _)| i.action() == Some(*s))
                .map(|(_, on)| *on)
            {
                i.setEnabled(on);
            }
            if let Some(sub) = i.submenu() {
                walk(&sub, states);
            }
        }
    }
    if let Some(bar) = NSApplication::sharedApplication(mtm).mainMenu() {
        walk(&bar, states);
    }
}

/// One item. `target` = `None` sends the action up the responder chain (text editing, window handling).
fn item(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    key: &str,
    target: Option<&AnyObject>,
) -> Retained<NSMenuItem> {
    // SAFETY: designated initializer on a fresh allocation.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    // SAFETY: the target is the app delegate, which lives as long as the app.
    unsafe { item.setTarget(target) };
    item
}

/// A top-level menu and its item in the menu bar.
fn menu(mtm: MainThreadMarker, bar: &NSMenu, title: &str) -> Retained<NSMenu> {
    let title = NSString::from_str(title);
    let m = NSMenu::initWithTitle(NSMenu::alloc(mtm), &title);
    // Items stay as enabled as the app sets them; AppKit's validation is off (it re-enables items with a target).
    m.setAutoenablesItems(false);
    let holder = NSMenuItem::new(mtm);
    holder.setSubmenu(Some(&m));
    bar.addItem(&holder);
    m
}

fn separator(mtm: MainThreadMarker, m: &NSMenu) {
    m.addItem(&NSMenuItem::separatorItem(mtm));
}

/// The menu bar. The window menu becomes the app's windows menu.
pub fn main_menu(mtm: MainThreadMarker, delegate: &Delegate) -> Retained<NSMenu> {
    let me: &AnyObject = delegate.as_ref();
    let bar = NSMenu::new(mtm);
    let add = |m: &NSMenu, title: &str, action: Sel, key: &str, target: Option<&AnyObject>| {
        let i = item(mtm, title, action, key, target);
        m.addItem(&i);
        i
    };

    let app = menu(mtm, &bar, "Draft Desk");
    app.setAutoenablesItems(true);
    add(
        &app,
        tr("Über Draft Desk", "About Draft Desk"),
        sel!(orderFrontStandardAboutPanel:),
        "",
        None,
    );
    separator(mtm, &app);
    add(
        &app,
        tr("Einstellungen …", "Settings…"),
        sel!(showSettings:),
        ",",
        Some(me),
    );
    separator(mtm, &app);
    add(
        &app,
        tr("Draft Desk ausblenden", "Hide Draft Desk"),
        sel!(hide:),
        "h",
        None,
    );
    let others = add(
        &app,
        tr("Andere ausblenden", "Hide Others"),
        sel!(hideOtherApplications:),
        "h",
        None,
    );
    others
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Option | NSEventModifierFlags::Command);
    add(
        &app,
        tr("Alle einblenden", "Show All"),
        sel!(unhideAllApplications:),
        "",
        None,
    );
    separator(mtm, &app);
    add(
        &app,
        tr("Draft Desk beenden", "Quit Draft Desk"),
        sel!(terminate:),
        "q",
        None,
    );

    // Edit: standard actions of the text views, resolved through the responder chain, so AppKit validates them.
    let edit = menu(mtm, &bar, tr("Bearbeiten", "Edit"));
    edit.setAutoenablesItems(true);
    add(&edit, tr("Widerrufen", "Undo"), sel!(undo:), "z", None);
    add(&edit, tr("Wiederholen", "Redo"), sel!(redo:), "Z", None);
    separator(mtm, &edit);
    add(&edit, tr("Ausschneiden", "Cut"), sel!(cut:), "x", None);
    add(&edit, tr("Kopieren", "Copy"), sel!(copy:), "c", None);
    add(&edit, tr("Einsetzen", "Paste"), sel!(paste:), "v", None);
    let plain = add(
        &edit,
        tr("Einsetzen und Stil anpassen", "Paste and Match Style"),
        sel!(pasteAsPlainText:),
        "V",
        None,
    );
    plain.setKeyEquivalentModifierMask(
        NSEventModifierFlags::Option | NSEventModifierFlags::Shift | NSEventModifierFlags::Command,
    );
    add(&edit, tr("Löschen", "Delete"), sel!(delete:), "", None);
    add(
        &edit,
        tr("Alles auswählen", "Select All"),
        sel!(selectAll:),
        "a",
        None,
    );
    separator(mtm, &edit);
    let find = NSMenu::initWithTitle(
        NSMenu::alloc(mtm),
        &NSString::from_str(tr("Suchen", "Find")),
    );
    for (title, tag, key) in [
        (tr("Suchen …", "Find…"), 1, "f"),
        (tr("Weitersuchen", "Find Next"), 2, "g"),
        (tr("Rückwärts suchen", "Find Previous"), 3, "G"),
        (tr("Auswahl suchen", "Use Selection for Find"), 7, "e"),
    ] {
        add(&find, title, sel!(performFindPanelAction:), key, None).setTag(tag);
    }
    let find_holder = add(&edit, tr("Suchen", "Find"), sel!(submenuAction:), "", None);
    find_holder.setSubmenu(Some(&find));

    // Draft: the app's own actions, enabled by the app.
    let draft = menu(mtm, &bar, tr("Entwurf", "Draft"));
    add(
        &draft,
        tr("Prüfen", "Check"),
        sel!(checkDraft:),
        "P",
        Some(me),
    );
    add(
        &draft,
        tr("Korrigieren", "Fix"),
        sel!(fixDraft:),
        "K",
        Some(me),
    );
    add(
        &draft,
        tr("Vorschau", "Preview"),
        sel!(togglePreview:),
        "V",
        Some(me),
    );
    separator(mtm, &draft);
    // No key equivalent: Cmd+Backspace deletes to the line start while typing.
    add(
        &draft,
        tr("Verwerfen …", "Discard…"),
        sel!(discardDraft:),
        "",
        Some(me),
    );
    // No key equivalent, on purpose: sending is a click.
    add(&draft, tr("Senden", "Send"), sel!(sendDraft:), "", Some(me));

    let window = menu(mtm, &bar, tr("Fenster", "Window"));
    window.setAutoenablesItems(true);
    add(
        &window,
        tr("Im Dock ablegen", "Minimize"),
        sel!(performMiniaturize:),
        "m",
        None,
    );
    add(&window, tr("Zoomen", "Zoom"), sel!(performZoom:), "", None);
    separator(mtm, &window);
    add(&window, "Draft Desk", sel!(showMainWindow:), "0", Some(me));
    separator(mtm, &window);
    add(
        &window,
        tr("Alle nach vorne bringen", "Bring All to Front"),
        sel!(arrangeInFront:),
        "",
        None,
    );
    NSApplication::sharedApplication(mtm).setWindowsMenu(Some(&window));
    bar
}
