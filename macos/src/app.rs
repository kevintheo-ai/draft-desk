//! The app: the main-thread state, the one delegate object that AppKit calls back, the window, start and quit.
//!
//! The state lives in a main-thread slot (`with_app`); every AppKit callback reaches it through that slot, as in
//! Lookout. Closing the window hides it, quitting quits.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibilityAnnouncementKey, NSAccessibilityAnnouncementRequestedNotification,
    NSAccessibilityPostNotificationWithUserInfo, NSAccessibilityPriorityKey,
    NSAccessibilityPriorityLevel, NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn,
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply, NSBackingStoreType, NSControl, NSControlTextEditingDelegate,
    NSModalResponse, NSModalResponseOK, NSOpenPanel, NSSplitViewController, NSSplitViewItem,
    NSTableColumn, NSTableView, NSTableViewDataSource, NSTableViewDelegate, NSTextDelegate,
    NSTextFieldDelegate, NSTextViewDelegate, NSTokenFieldDelegate, NSToolbar, NSToolbarDelegate,
    NSToolbarItem, NSToolbarItemIdentifier, NSView, NSViewController, NSWindow, NSWindowDelegate,
    NSWindowStyleMask, NSWindowToolbarStyle,
};
use objc2_foundation::{
    NSActivityOptions, NSArray, NSDictionary, NSIndexSet, NSInteger, NSNotification, NSNumber,
    NSObject, NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSRunLoop, NSRunLoopCommonModes,
    NSSize, NSString, NSTimer, NSURL, NSUserDefaults,
};
use objc2_quick_look_ui::{QLPreviewItem, QLPreviewPanel, QLPreviewPanelDataSource};
use objc2_user_notifications::{
    UNNotificationResponse, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use objc2_web_kit::{
    WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKWebView,
};

use draftdesk_core::checks::is_stale;
use draftdesk_core::config::Kind;
use draftdesk_core::desk::{
    Actions, Confirmation, Desk, Note, OpenError, ScheduleError, SharedTransports, actions,
    confirmation,
};
use draftdesk_core::edit::{add_attachments, diff, remove_attachment, save_edits};
use draftdesk_core::item::{Item, ItemRead, State};
use draftdesk_core::sender::{FireResult, SystemClock, default_transport};
use draftdesk_core::text::{body_problems, has_forbidden};
use draftdesk_core::transport::CleanupResult;

use crate::attachments::{self, DropView};
use crate::editor::Editor;
use crate::i18n::{error_text, fire_text, note_text, quoted, tr};
use crate::log::log;
use crate::settings::{self, Settings};
use crate::{menu, notify, on_main, sidebar, spawn, toolbar};

/// What the Draft menu, the toolbar and the keyboard ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Check,
    Fix,
    TogglePreview,
    Discard,
    Send,
    Settings,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Delegate` implements no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DraftDeskDelegate"]
    pub struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            with_app(App::start);
        }

        #[unsafe(method(applicationDidBecomeActive:))]
        fn did_become_active(&self, _notification: &NSNotification) {
            with_app(App::first_activation);
        }

        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            with_app(|a| a.show_window(true));
            true
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _app: &NSApplication) -> bool {
            false
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            with_app_now(App::should_terminate).unwrap_or(NSApplicationTerminateReply::TerminateCancel)
        }
    }

    unsafe impl NSWindowDelegate for Delegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            with_app_now(App::window_should_close).unwrap_or(false)
        }
    }

    unsafe impl NSControlTextEditingDelegate for Delegate {
        /// Typing in a header field.
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _notification: &NSNotification) {
            with_app(App::mark_dirty);
        }
    }

    unsafe impl NSTextFieldDelegate for Delegate {}

    unsafe impl NSTokenFieldDelegate for Delegate {}

    unsafe impl NSTextDelegate for Delegate {
        /// Typing in the text (also an undo or a replacement made by the app).
        #[unsafe(method(textDidChange:))]
        fn text_did_change(&self, _notification: &NSNotification) {
            with_app(App::mark_dirty);
        }
    }

    unsafe impl NSTextViewDelegate for Delegate {}

    /// The preview goes nowhere: only its own page (`about:blank`) loads.
    unsafe impl WKNavigationDelegate for Delegate {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_navigation(
            &self,
            _view: &WKWebView,
            action: &WKNavigationAction,
            handler: &DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            // SAFETY: plain getter of the action WebKit hands over, main thread.
            let url = unsafe { action.request() }
                .URL()
                .and_then(|u| u.absoluteString())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let allow = crate::preview::allow_navigation(&url);
            if !allow {
                log("preview: a navigation was refused");
            }
            handler.call((if allow {
                WKNavigationActionPolicy::Allow
            } else {
                WKNavigationActionPolicy::Cancel
            },));
        }
    }

    // Two tables: the sidebar and the accounts in the settings. Each reads its own slot (`sidebar::ROWS`,
    // `settings::ROWS`), never the app: AppKit asks during a reload inside `with_app`.
    unsafe impl NSTableViewDataSource for Delegate {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, table: &NSTableView) -> NSInteger {
            if settings::is_accounts(table) {
                settings::row_count() as NSInteger
            } else {
                sidebar::row_count() as NSInteger
            }
        }
    }

    unsafe impl NSTableViewDelegate for Delegate {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for_row(
            &self,
            table: &NSTableView,
            column: Option<&NSTableColumn>,
            row: NSInteger,
        ) -> Option<Retained<NSView>> {
            // No `?` here: the macro rewrites the function body.
            let row = usize::try_from(row).ok();
            if settings::is_accounts(table) {
                let id = column.map(|c| c.identifier().to_string()).unwrap_or_default();
                row.and_then(|r| settings::cell(self.mtm(), r, &id))
            } else {
                row.and_then(sidebar::row_at).map(|r| sidebar::cell(self.mtm(), &r))
            }
        }

        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_did_change(&self, notification: &NSNotification) {
            let accounts = notification
                .object()
                .and_then(|o| o.downcast::<NSTableView>().ok())
                .is_some_and(|t| settings::is_accounts(&t));
            if accounts {
                with_app(App::settings_selection_changed);
            } else {
                with_app(App::selection_changed);
            }
        }
    }

    impl Delegate {
        #[unsafe(method(showSettings:))]
        fn show_settings(&self, _sender: Option<&AnyObject>) {
            with_app(|a| a.on_action(Action::Settings));
        }

        #[unsafe(method(showMainWindow:))]
        fn show_main_window(&self, _sender: Option<&AnyObject>) {
            with_app(|a| a.show_window(true));
        }

        #[unsafe(method(checkDraft:))]
        fn check_draft(&self, _sender: Option<&AnyObject>) {
            with_app(|a| a.on_action(Action::Check));
        }

        #[unsafe(method(fixDraft:))]
        fn fix_draft(&self, _sender: Option<&AnyObject>) {
            with_app(|a| a.on_action(Action::Fix));
        }

        #[unsafe(method(togglePreview:))]
        fn toggle_preview(&self, _sender: Option<&AnyObject>) {
            with_app(|a| a.on_action(Action::TogglePreview));
        }

        #[unsafe(method(discardDraft:))]
        fn discard_draft(&self, _sender: Option<&AnyObject>) {
            with_app(|a| a.on_action(Action::Discard));
        }

        #[unsafe(method(sendDraft:))]
        fn send_draft(&self, _sender: Option<&AnyObject>) {
            with_app(|a| a.on_action(Action::Send));
        }

        #[unsafe(method(fromChanged:))]
        fn from_changed(&self, _sender: Option<&AnyObject>) {
            with_app(|a| {
                a.editor.from_changed();
                a.mark_dirty();
            });
        }

        #[unsafe(method(bannerAction:))]
        fn banner_action(&self, _sender: Option<&AnyObject>) {
            with_app(App::banner_action);
        }

        #[unsafe(method(dismissBanner:))]
        fn dismiss_banner(&self, _sender: Option<&AnyObject>) {
            with_app(|a| a.hint(None, None));
        }

        /// The switch "Text | Preview".
        #[unsafe(method(previewToggled:))]
        fn preview_toggled(&self, _sender: Option<&AnyObject>) {
            with_app(|a| a.editor.set_preview(a.editor.preview_on()));
        }

        #[unsafe(method(openAttachment:))]
        fn open_attachment(&self, sender: Option<&AnyObject>) {
            let i = tag_of(sender);
            with_app(move |a| a.open_attachment(i));
        }

        #[unsafe(method(removeAttachment:))]
        fn remove_attachment(&self, sender: Option<&AnyObject>) {
            let i = tag_of(sender);
            with_app(move |a| a.remove_attachment_at(i));
        }

        #[unsafe(method(addAttachment:))]
        fn add_attachment(&self, _sender: Option<&AnyObject>) {
            with_app(App::choose_files);
        }

        /// The disclosure button of the conversation.
        #[unsafe(method(toggleThread:))]
        fn toggle_thread(&self, _sender: Option<&AnyObject>) {
            with_app(|a| a.editor.sync_thread());
        }

        /// "Undo" in the countdown bar.
        #[unsafe(method(undoSend:))]
        fn undo_send(&self, _sender: Option<&AnyObject>) {
            with_app(App::undo_send);
        }

        // ----- Settings (tags: 0 SMTP, 1 Google, 2 command; 0 check, 1 fix; 0 send, 1 discard program) -----

        #[unsafe(method(settingsAdd:))]
        fn settings_add(&self, sender: Option<&AnyObject>) {
            let kind = match tag_of(sender) {
                0 => Kind::Smtp,
                1 => Kind::Gmail,
                _ => Kind::Command,
            };
            with_app(move |a| a.settings_add(kind));
        }

        #[unsafe(method(settingsRemove:))]
        fn settings_remove(&self, _sender: Option<&AnyObject>) {
            with_app(App::settings_remove);
        }

        #[unsafe(method(settingsChooseCommand:))]
        fn settings_choose_command(&self, sender: Option<&AnyObject>) {
            let tag = tag_of(sender) as isize;
            with_app(move |a| a.settings_choose_command(tag));
        }

        #[unsafe(method(settingsClearCommand:))]
        fn settings_clear_command(&self, sender: Option<&AnyObject>) {
            let tag = tag_of(sender) as isize;
            with_app(move |a| a.settings_clear_command(tag));
        }

        #[unsafe(method(settingsTimeout:))]
        fn settings_timeout(&self, sender: Option<&AnyObject>) {
            let tag = tag_of(sender) as isize;
            with_app(move |a| a.settings_timeout(tag));
        }

        #[unsafe(method(sheetConfirm:))]
        fn sheet_confirm(&self, _sender: Option<&AnyObject>) {
            with_app(App::sheet_confirm);
        }

        #[unsafe(method(sheetCancel:))]
        fn sheet_cancel(&self, _sender: Option<&AnyObject>) {
            with_app(App::sheet_cancel);
        }

        #[unsafe(method(sheetChoose:))]
        fn sheet_choose(&self, sender: Option<&AnyObject>) {
            let tag = tag_of(sender) as isize;
            with_app(move |a| a.sheet_choose(tag));
        }

        // Quick Look looks for its controller in the responder chain of the key window; the window's delegate is
        // part of it (Apple: "your window controller or delegate").
        #[unsafe(method(acceptsPreviewPanelControl:))]
        fn accepts_preview_panel_control(&self, _panel: Option<&QLPreviewPanel>) -> bool {
            true
        }

        #[unsafe(method(beginPreviewPanelControl:))]
        fn begin_preview_panel_control(&self, panel: Option<&QLPreviewPanel>) {
            if let Some(p) = panel {
                // SAFETY: the delegate lives as long as the app.
                unsafe { p.setDataSource(Some(ProtocolObject::from_ref(self))) };
            }
        }

        #[unsafe(method(endPreviewPanelControl:))]
        fn end_preview_panel_control(&self, panel: Option<&QLPreviewPanel>) {
            if let Some(p) = panel {
                // SAFETY: plain setter.
                unsafe { p.setDataSource(None) };
            }
        }
    }

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        /// A click on a notification: the window comes and shows the draft it is about.
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn notification_clicked(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            done: &DynBlock<dyn Fn()>,
        ) {
            let id = notify::item_of(response);
            // UserNotifications does not promise the main thread for this call; the app lives there.
            on_main(move || with_app(move |a| a.notification_clicked(id)));
            done.call(());
        }
    }

    unsafe impl NSToolbarDelegate for Delegate {
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn toolbar_item(
            &self,
            _toolbar: &NSToolbar,
            id: &NSToolbarItemIdentifier,
            _inserted: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            toolbar::item(self.mtm(), self, id)
        }

        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn toolbar_default(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSToolbarItemIdentifier>> {
            toolbar::identifiers()
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn toolbar_allowed(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSToolbarItemIdentifier>> {
            toolbar::identifiers()
        }
    }

    // Reads its own slot (`attachments::PREVIEW`): Quick Look asks while the app may be busy.
    unsafe impl QLPreviewPanelDataSource for Delegate {
        #[unsafe(method(numberOfPreviewItemsInPreviewPanel:))]
        fn number_of_preview_items(&self, _panel: Option<&QLPreviewPanel>) -> NSInteger {
            attachments::preview_count() as NSInteger
        }

        #[unsafe(method_id(previewPanel:previewItemAtIndex:))]
        fn preview_item_at(
            &self,
            _panel: Option<&QLPreviewPanel>,
            index: NSInteger,
        ) -> Option<Retained<ProtocolObject<dyn QLPreviewItem>>> {
            usize::try_from(index)
                .ok()
                .and_then(attachments::preview_item)
                .map(ProtocolObject::from_retained)
        }
    }
);

/// The tag of the button that sent an action (the attachment's index).
fn tag_of(sender: Option<&AnyObject>) -> usize {
    sender
        .and_then(|s| s.downcast_ref::<NSControl>())
        .and_then(|c| usize::try_from(c.tag()).ok())
        .unwrap_or(usize::MAX)
}

impl Delegate {
    pub fn new(mtm: MainThreadMarker) -> Retained<Delegate> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

// ---------- The app slot (as in Lookout) ----------

type Job = Box<dyn FnOnce(&mut App)>;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static PENDING: RefCell<Vec<Job>> = const { RefCell::new(Vec::new()) };
}

/// Puts the app into the main-thread slot.
pub fn install(app: App) {
    APP.with(|slot| *slot.borrow_mut() = Some(app));
}

/// Takes the app out of the slot (tests).
pub fn uninstall() -> Option<App> {
    APP.with(|slot| slot.borrow_mut().take())
}

/// Runs `f` on the app (main thread). If AppKit called back while the app is busy, `f` waits and runs right after
/// the next call that gets through (the poll comes every second) instead of being lost.
pub fn with_app(f: impl FnOnce(&mut App) + 'static) {
    let mut f = Some(f);
    APP.with(|slot| {
        // `f` is taken only once the app is there: taken first, it was lost when the slot was still empty.
        if let Ok(mut slot) = slot.try_borrow_mut()
            && let Some(app) = slot.as_mut()
            && let Some(f) = f.take()
        {
            f(app);
        }
    });
    if let Some(f) = f {
        PENDING.with(|p| p.borrow_mut().push(Box::new(f)));
        on_main(drain_pending);
    } else if PENDING.with(|p| !p.borrow().is_empty()) {
        on_main(drain_pending);
    }
}

/// Runs `f` on the app now, for callbacks that must answer at once. `None` while the app is busy or not installed.
pub fn with_app_now<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|slot| slot.try_borrow_mut().ok()?.as_mut().map(f))
}

fn drain_pending() {
    // Still busy (a modal dialog runs inside a callback) or not installed yet: the jobs wait for the next call that
    // gets through. Trying again at once spun the main queue for as long as the dialog stayed open.
    if !APP.with(|slot| slot.try_borrow().is_ok_and(|s| s.is_some())) {
        return;
    }
    let jobs = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    for job in jobs {
        with_app(job);
    }
}

// ---------- Start ----------

/// Starts the app on the main thread and runs it until Quit.
pub fn run() {
    let mtm = MainThreadMarker::new().expect("Draft Desk must start on the main thread");
    crate::i18n::set_from_system();
    // macOS's "Add period with double-space" would change the text without a word (found on the Mac 2026-11-07):
    // off for this app (its own preference, above the system-wide one).
    NSUserDefaults::standardUserDefaults().setBool_forKey(
        false,
        &NSString::from_str("NSAutomaticPeriodSubstitutionEnabled"),
    );
    let ns_app = NSApplication::sharedApplication(mtm);
    ns_app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let app = App::new(mtm);
    ns_app.setMainMenu(Some(&menu::main_menu(mtm, &app.delegate)));
    ns_app.setDelegate(Some(ProtocolObject::from_ref(&*app.delegate)));
    notify::set_delegate(&app.delegate);
    install(app);
    ns_app.run();
}

/// The main window: hidden until asked for; closing it only hides it.
fn main_window(mtm: MainThreadMarker, delegate: &Delegate) -> Retained<NSWindow> {
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1100.0, 760.0));
    // SAFETY: designated initializer on a fresh allocation.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the `Retained` in `App` is the window's owner; AppKit must not release it on close as well.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("Draft Desk"));
    window.setContentMinSize(NSSize::new(820.0, 560.0));
    window.setRestorable(false);
    window.center();
    window.setFrameAutosaveName(&NSString::from_str("DraftDeskMain"));
    window.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    window.setToolbar(Some(&toolbar::new(mtm, delegate)));
    window.setToolbarStyle(NSWindowToolbarStyle::Unified);
    // Tab walks every field in reading order, also the ones that appear later (strip, findings, conversation).
    window.setAutorecalculatesKeyViewLoop(true);
    window
}

/// A view controller around a view built in code (no nib).
fn controller(mtm: MainThreadMarker, view: &NSView) -> Retained<NSViewController> {
    let vc = NSViewController::new(mtm);
    vc.setView(view);
    vc
}

pub struct App {
    pub mtm: MainThreadMarker,
    pub delegate: Retained<Delegate>,
    pub window: Retained<NSWindow>,
    pub split: Retained<NSSplitViewController>,
    pub table: Retained<NSTableView>,
    /// The right side; the editor lives here.
    pub content: Retained<NSView>,
    pub desk: Option<Arc<Desk>>,
    /// The queue as the last poll saw it, newest first.
    pub items: Vec<ItemRead>,
    pub selected: Option<String>,
    pub editor: Editor,
    /// The draft in the editor as last loaded or saved: the "shown" state a send click approves.
    pub shown: Option<Item>,
    /// The fields hold changes not yet saved.
    pub dirty: bool,
    /// Saves quietly 1.5 s after the last change.
    autosave: Option<Retained<NSTimer>>,
    /// What the hint's button does.
    hint_action: Option<HintAction>,
    /// The window was shown once since the start (the first activation shows it).
    shown_once: bool,
    /// Drafts whose check runs now (started here): their findings say so.
    checking: BTreeSet<String>,
    /// The scheduled sends of this run, by draft; each countdown runs on its own thread.
    pending: BTreeMap<String, Pending>,
    /// Numbers the send clicks: the late outcome of an undone send never touches a newer one.
    tickets: u64,
    /// Drafts being discarded (the mailbox step runs on its own thread).
    discarding: BTreeSet<String>,
    /// The sheet on the window, one at a time.
    pub alert: Option<Retained<NSAlert>>,
    /// Redraws the countdown four times a second while one runs.
    undo_timer: Option<Retained<NSTimer>>,
    /// The poll every second, from the end of the start-up repair on.
    tick: Option<Retained<NSTimer>>,
    /// A notification named this draft before the first poll filled the list: shown once the list is there.
    wanted: Option<String>,
    /// Whether a notification would be shown (tests replace it); without, a new draft brings the window to the front
    /// instead.
    pub notifications_work: fn() -> bool,
    /// The settings window, made on first use.
    pub settings: Option<Settings>,
}

/// One scheduled send.
struct Pending {
    token: String,
    ticket: u64,
    deadline: Instant,
    undo: Duration,
    subject: String,
    recipients: usize,
    /// Keeps App Nap, timer coalescing and sleep away during the countdown: a late wake-up would miss the window in
    /// which the core still takes the approval.
    activity: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

/// What the person confirmed in the sheet before sending: this draft, in this version, with these findings.
#[derive(Clone)]
struct Approved {
    id: String,
    revision: u64,
    what: Confirmation,
}

/// "Send Anyway" counts only for what the sheet showed.
fn changed_while_asked() -> &'static str {
    tr(
        "Der Entwurf oder seine Befunde haben sich geändert, während die Frage offen war. Nichts wurde gesendet; bitte erneut auf Senden klicken.",
        "The draft or its findings changed while the question was open. Nothing was sent; please click Send again.",
    )
}

/// The pause after the last change before the quiet save.
const AUTOSAVE_SECONDS: f64 = 1.5;

/// The button of the hint bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintAction {
    /// Moves an invalid draft out of the queue.
    ArchiveInvalid(String),
    /// Removes the invisible characters from text and subject.
    StripInvisible,
    /// Drops the changes that could not be saved: the saved state again.
    Revert,
}

impl App {
    pub fn new(mtm: MainThreadMarker) -> App {
        let delegate = Delegate::new(mtm);
        let window = main_window(mtm, &delegate);
        let (scroll, table) = sidebar::table(mtm, &delegate);
        // Files dropped anywhere on the right side become attachments.
        let content: Retained<NSView> = Retained::into_super(DropView::new(mtm));
        let editor = Editor::new(mtm, &delegate, &content);
        let split = NSSplitViewController::new(mtm);
        let side = NSSplitViewItem::sidebarWithViewController(&controller(mtm, &scroll));
        side.setMinimumThickness(220.0);
        side.setMaximumThickness(420.0);
        split.addSplitViewItem(&side);
        split.addSplitViewItem(&NSSplitViewItem::splitViewItemWithViewController(
            &controller(mtm, &content),
        ));
        window.setContentViewController(Some(&split));
        window.setContentSize(NSSize::new(1100.0, 760.0));
        // The first focus is the text.
        window.setInitialFirstResponder(Some(&editor.body));
        App {
            mtm,
            delegate,
            window,
            split,
            table,
            content,
            desk: None,
            items: Vec::new(),
            selected: None,
            editor,
            shown: None,
            dirty: false,
            autosave: None,
            hint_action: None,
            shown_once: false,
            checking: BTreeSet::new(),
            pending: BTreeMap::new(),
            tickets: 0,
            discarding: BTreeSet::new(),
            alert: None,
            undo_timer: None,
            tick: None,
            wanted: None,
            notifications_work: notify::works,
            settings: None,
        }
    }

    /// After the desk opened: notifications are asked for once, the start-up repair runs on its own thread, then the
    /// poll starts. The window stays hidden: a click on the app, the Dock or a notification shows it.
    pub fn begin(&mut self) {
        notify::request();
        let Some(desk) = self.desk.clone() else {
            return;
        };
        spawn("draftdesk-boot", move || {
            let notes = desk.boot();
            on_main(move || with_app(move |a| a.booted(notes)));
        });
    }

    fn booted(&mut self, notes: Vec<Note>) {
        self.refresh();
        if let Some(id) = self.wanted.take() {
            self.go_to(&id);
        }
        self.show_notes(&notes);
        let block = RcBlock::new(|_: NonNull<NSTimer>| with_app(App::refresh));
        // SAFETY: a repeating timer on the main run loop; the block only reaches the app through the main-thread slot.
        let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(1.0, true, &block) };
        // SAFETY: the main run loop and a static Foundation constant.
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        self.tick = Some(timer);
    }

    /// One poll into the window (every second): the sidebar and the selected draft; a new draft brings a
    /// notification (without notifications the window comes to the front, without the keyboard); a shown draft its
    /// agent replaced gives way to the new version; the mailbox cleanup of a discard on the command line starts;
    /// notes become the hint.
    pub fn refresh(&mut self) {
        let Some(desk) = self.desk.clone() else {
            return;
        };
        let p = desk.poll(std::time::SystemTime::now());
        // Only a draft replaced while it is selected: the selection can fall on one the last poll already saw
        // discarded (its neighbor left the queue), and that replacement is old news.
        let was_live = |id: &str| {
            self.items.iter().any(|r| {
                r.id() == id && !matches!(r, ItemRead::Valid(i) if i.state == State::Discarded)
            })
        };
        let replaced_by = self
            .selected
            .as_deref()
            .filter(|id| was_live(id))
            .and_then(|id| p.items.iter().find(|r| r.id() == id))
            .and_then(|r| match r {
                ItemRead::Valid(i) if i.state == State::Discarded => i
                    .history
                    .last()
                    .filter(|h| h.event == "superseded")
                    .map(|h| h.detail.clone()),
                _ => None,
            });
        let nothing_selected = self.selected.is_none();
        // Typing not saved yet cannot follow a draft that gives way (replaced, gone): the person is told.
        let unsaved = self.selected.clone().filter(|_| self.dirty);
        let lost = if unsaved.is_some() {
            tr(
                " Deine letzte Änderung war noch nicht gespeichert und ist nicht übernommen.",
                " Your last change was not saved yet and is not kept.",
            )
        } else {
            ""
        };
        self.set_items(p.items);
        if let Some(new) = replaced_by.filter(|n| sidebar::index_of(n).is_some()) {
            self.select(Some(&new));
            let replaced = tr(
                "Der Agent hat den Entwurf durch eine neue Fassung ersetzt.",
                "The agent replaced the draft with a new version.",
            );
            self.hint(Some(&format!("{replaced}{lost}")), None);
        } else if unsaved.is_some() && self.selected != unsaved {
            let gone = tr(
                "Der Entwurf ist nicht mehr in der Warteschlange.",
                "The draft left the queue.",
            );
            self.hint(Some(&format!("{gone}{lost}")), None);
        }
        for id in &p.new_ready {
            self.tell_new(id);
        }
        if nothing_selected && let Some(first) = p.new_ready.first() {
            self.select(Some(first));
        }
        for id in p.cleanup_due {
            let desk = desk.clone();
            spawn("draftdesk-cleanup", move || {
                let r = desk.cleanup_discarded(&id);
                on_main(move || with_app(move |a| a.cleanup_done(&id, r)));
            });
        }
        self.show_notes(&p.notes);
    }

    /// A new draft: a notification, a click on it shows the draft. Without notifications (not allowed, or not
    /// possible with this signature) the window comes to the front instead, without taking the keyboard.
    fn tell_new(&mut self, id: &str) {
        let Some(ItemRead::Valid(i)) = self.items.iter().find(|r| r.id() == id) else {
            return;
        };
        let n = i.to.len() + i.cc.len() + i.bcc.len();
        let subject = notify::clean(&i.subject);
        let text = match (crate::i18n::is_german(), n) {
            (true, _) => format!("Neuer Entwurf: {subject} ({n} Empfänger)"),
            (false, 1) => format!("New draft: {subject} (1 recipient)"),
            (false, _) => format!("New draft: {subject} ({n} recipients)"),
        };
        if (self.notifications_work)() {
            notify::post("Draft Desk", &text, &format!("new-{id}"), Some(id));
        } else {
            self.show_window(false);
        }
    }

    fn cleanup_done(&mut self, id: &str, r: draftdesk_core::Result<bool>) {
        match r {
            Ok(done) => log(&format!("mailbox cleanup of {id}: {done}")),
            Err(e) => self.show_notes(&[Note::Problem(id.to_string(), e.0)]),
        }
    }

    /// Notes of the start-up repair or the poll, one line each in the hint; a draft is named by its subject.
    fn show_notes(&mut self, notes: &[Note]) {
        if notes.is_empty() {
            return;
        }
        let sender = self.desk.as_ref().map(|d| d.sender());
        let lines: Vec<String> = notes
            .iter()
            .map(|n| {
                let id = match n {
                    Note::Unscheduled(i)
                    | Note::Interrupted(i)
                    | Note::Finalized(i)
                    | Note::CleanedUp(i)
                    | Note::Problem(i, _) => i.as_str(),
                    Note::ConfigInvalid(_) | Note::NoSigningKey(_) => "",
                };
                let label = match sender.as_ref().map(|s| s.store().read_any(id)) {
                    Some(ItemRead::Valid(i)) if !id.is_empty() => i.subject.clone(),
                    _ => id.to_string(),
                };
                note_text(n, &label)
            })
            .collect();
        self.hint(Some(&lines.join("\n")), None);
    }

    /// A click on a notification: the window comes, with the draft it is about.
    pub fn notification_clicked(&mut self, id: Option<String>) {
        self.show_window(true);
        if let Some(id) = id {
            self.go_to(&id);
        }
    }

    /// Selects a draft as a click in the sidebar would: unsaved changes are saved first.
    fn go_to(&mut self, id: &str) {
        // A notification can name a draft the list does not hold: before the first poll it is kept for then (a click
        // started the app); later it has left the queue (sent, discarded, replaced) and the selection stays.
        if sidebar::index_of(id).is_none() {
            if self.tick.is_none() {
                self.wanted = Some(id.to_string());
            }
            return;
        }
        if self.selected.as_deref() == Some(id) || !self.flush(true) {
            return;
        }
        self.select(Some(id));
    }

    /// Shows a hint (`None`: hides it); `action` puts a button next to it.
    pub fn hint(&mut self, text: Option<&str>, action: Option<(&str, HintAction)>) {
        match text {
            Some(t) => {
                self.editor
                    .banner
                    .show(t, action.as_ref().map(|(title, _)| *title));
                self.hint_action = action.map(|(_, a)| a);
            }
            None => {
                self.editor.banner.hide();
                self.hint_action = None;
            }
        }
    }

    fn banner_action(&mut self) {
        let Some(action) = self.hint_action.take() else {
            return;
        };
        match action {
            HintAction::ArchiveInvalid(id) => self.archive_invalid(&id),
            HintAction::StripInvisible => {
                self.hint(None, None);
                self.editor.strip_invisible();
                self.mark_dirty();
            }
            HintAction::Revert => self.load_selected(),
        }
    }

    /// The fields changed (typing, another sender, a replacement by the app): unsaved until the quiet save 1.5 s
    /// after the last change.
    pub fn mark_dirty(&mut self) {
        if self.shown.is_none() || !self.editable() {
            return;
        }
        self.dirty = true;
        if let Some(t) = self.autosave.take() {
            t.invalidate();
        }
        let block = RcBlock::new(|_: NonNull<NSTimer>| with_app(App::autosave));
        // SAFETY: the timer runs on the main run loop; the block only reaches the app through the main-thread slot.
        self.autosave = Some(unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(AUTOSAVE_SECONDS, false, &block)
        });
        self.update_invisible();
    }

    fn autosave(&mut self) {
        self.autosave = None;
        self.flush(false);
    }

    /// Saves what the fields hold. Quiet (`loud = false`, the timer): a failure keeps the changes unsaved and says
    /// nothing. Loud (a checkpoint: send, check, fix, attachment, another draft, closing, quitting): on success the
    /// fields show the saved form, which is what gets sent; on failure the hint says why and offers to drop the
    /// changes, and `false` stops the action.
    pub fn flush(&mut self, loud: bool) -> bool {
        if let Some(t) = self.autosave.take() {
            t.invalidate();
        }
        let (Some(shown), Some(desk)) = (self.shown.clone(), self.desk.clone()) else {
            return true;
        };
        if !self.editable() {
            return true;
        }
        let edit = diff(&shown, &self.editor.form());
        if edit.is_empty() {
            self.dirty = false;
            return true;
        }
        let sender = desk.sender();
        match save_edits(
            sender.store(),
            &shown.item_id,
            shown.content_revision,
            &edit,
        ) {
            Ok(item) => {
                self.dirty = false;
                if loud {
                    self.editor.redraw(&item);
                }
                self.shown = Some(item);
                // A new text makes the findings stale.
                self.show_findings();
                true
            }
            Err(e) => {
                // Only the id: the reason can hold an address.
                log(&format!("save of {} failed", shown.item_id));
                if loud {
                    let action = (
                        tr("Änderungen verwerfen", "Discard Changes"),
                        HintAction::Revert,
                    );
                    self.hint(Some(&error_text(&e.0)), Some(action));
                }
                false
            }
        }
    }

    /// The count of invisible characters in the hint, with the button that removes them.
    fn update_invisible(&mut self) {
        let n = self.editor.highlight_invisible();
        if n > 0 && self.editable() {
            let text = format!(
                "{} {n} {}",
                tr(
                    "Unsichtbare Zeichen im Text oder Betreff:",
                    "Invisible characters in the text or subject:"
                ),
                tr(
                    "(rot markiert). Gesendet wird erst ohne sie.",
                    "(marked red). The draft cannot be sent with them."
                )
            );
            let action = (tr("Entfernen", "Remove"), HintAction::StripInvisible);
            self.hint(Some(&text), Some(action));
        } else if self.hint_action == Some(HintAction::StripInvisible) {
            self.hint(None, None);
        }
    }

    /// Only a `ready` draft is editable, and not while a check, fix, countdown or discard runs for it.
    fn editable(&self) -> bool {
        self.shown
            .as_ref()
            .is_some_and(|i| i.state == State::Ready && !self.locked(&i.item_id))
    }

    fn locked(&self, id: &str) -> bool {
        self.busy(id) || self.pending.contains_key(id)
    }

    /// A check, fix or discard runs for the draft.
    fn busy(&self, id: &str) -> bool {
        self.discarding.contains(id) || self.desk.as_ref().is_some_and(|d| d.is_busy(id))
    }

    /// What the buttons and the Draft menu may do now for the selected draft.
    pub fn current_actions(&self) -> Actions {
        let (Some(desk), Some(id)) = (&self.desk, self.selected.as_deref()) else {
            return Actions::default();
        };
        let read = match &self.shown {
            Some(i) if i.item_id == id => ItemRead::Valid(Box::new(i.clone())),
            _ => match self.items.iter().find(|r| r.id() == id) {
                Some(r) => r.clone(),
                None => return Actions::default(),
            },
        };
        let sender = desk.sender();
        let pending = self.pending.contains_key(id);
        let mut a = actions(&read, sender.store().config(), self.busy(id), pending);
        // A config.json that cannot be read makes every draft look invalid: none goes to the archive then.
        a.archive &= desk.config_ok();
        a
    }

    /// The editor after a change of the shown draft or of what it allows: editability, the invisible-character
    /// hint, the attachment strip, the findings, the Draft menu.
    fn sync_editor(&mut self) {
        let editable = self.editable();
        self.editor.set_editable(editable);
        self.update_invisible();
        let dir = match (&self.shown, &self.desk) {
            (Some(i), Some(d)) => d.sender().store().item_dir(&i.item_id).ok(),
            _ => None,
        };
        self.editor.strip.show(
            self.mtm,
            &self.delegate,
            self.shown.as_ref(),
            dir.as_deref(),
            editable,
        );
        self.show_findings();
        let a = self.current_actions();
        menu::set_enabled(
            self.mtm,
            &[
                (sel!(checkDraft:), a.check),
                (sel!(fixDraft:), a.fix),
                (sel!(discardDraft:), a.discard || a.archive),
                (sel!(sendDraft:), a.send),
            ],
        );
        toolbar::set_enabled(
            &self.window,
            &[
                (toolbar::CHECK, a.check),
                (toolbar::FIX, a.fix),
                (toolbar::DISCARD, a.discard || a.archive),
                (toolbar::SEND, a.send),
            ],
        );
    }

    pub fn pending_ids(&self) -> Vec<String> {
        self.pending.keys().cloned().collect()
    }

    /// The send click. Saves first; invisible characters stop it with their hint; open findings ask first. Then the
    /// countdown runs on its own thread and the bar offers Undo; the editor is locked until the outcome.
    /// `approved`: the click on "Send Anyway", valid only for the draft, version and findings the sheet showed.
    fn send(&mut self, approved: Option<Approved>) {
        if self.alert.is_some() {
            return;
        }
        if !self.current_actions().send {
            if approved.is_some() {
                self.hint(Some(changed_while_asked()), None);
            }
            return;
        }
        if !self.flush(true) {
            return;
        }
        let (Some(shown), Some(desk)) = (self.shown.clone(), self.desk.clone()) else {
            return;
        };
        if has_forbidden(&shown.subject) || !body_problems(&shown.body).is_empty() {
            self.update_invisible();
            return;
        }
        if let Some(a) = &approved
            && (a.id != shown.item_id
                || a.revision != shown.content_revision
                || confirmation(&shown).as_ref() != Some(&a.what))
        {
            self.hint(Some(changed_while_asked()), None);
            return;
        }
        let confirmed = approved.is_some();
        self.tickets += 1;
        let (id, ticket) = (shown.item_id.clone(), self.tickets);
        let back = id.clone();
        let done = move |r| on_main(move || with_app(move |a| a.send_done(&back, ticket, r)));
        match desk.schedule(&shown, confirmed, done) {
            Ok(s) => {
                let options = NSActivityOptions::UserInitiated | NSActivityOptions::LatencyCritical;
                let reason = NSString::from_str("Draft Desk: a send countdown runs");
                let activity =
                    NSProcessInfo::processInfo().beginActivityWithOptions_reason(options, &reason);
                self.pending.insert(
                    id,
                    Pending {
                        token: s.token,
                        ticket,
                        deadline: Instant::now() + s.undo,
                        undo: s.undo,
                        subject: shown.subject.clone(),
                        recipients: shown.to.len() + shown.cc.len() + shown.bcc.len(),
                        activity,
                    },
                );
                self.sync_editor();
                self.tick_undo();
                self.start_undo_timer();
                if let Some(text) = self.editor.undo.text() {
                    self.announce(&text);
                }
            }
            Err(ScheduleError::NeedsConfirmation(c)) => self.confirm_send(&c),
            Err(ScheduleError::Busy) => self.hint(Some(&error_text("busy")), None),
            Err(ScheduleError::Refused(e)) => {
                self.refresh();
                self.hint(Some(&error_text(&e)), None);
            }
        }
    }

    /// Open findings: the person confirms them in a sheet first.
    fn confirm_send(&mut self, c: &Confirmation) {
        let Some(shown) = &self.shown else {
            return;
        };
        let approved = Approved {
            id: shown.item_id.clone(),
            revision: shown.content_revision,
            what: c.clone(),
        };
        let mut lines: Vec<String> = c
            .findings
            .iter()
            .map(|f| match f.detail.is_empty() {
                true => format!("• {}", f.title),
                false => format!("• {}: {}", f.title, f.detail),
            })
            .collect();
        if c.verdict_block {
            lines.push(
                tr(
                    "Die Prüfung hat den Entwurf blockiert.",
                    "The check blocked the draft.",
                )
                .into(),
            );
        }
        if c.stale {
            lines.push(
                tr(
                    "Die Befunde gelten für einen früheren Text.",
                    "The findings were made for an earlier text.",
                )
                .into(),
            );
        }
        let window = self.window.clone();
        self.ask(
            &window,
            tr("Vor dem Senden bestätigen", "Confirm before sending"),
            &lines.join("\n"),
            tr("Trotzdem senden", "Send Anyway"),
            move |a| a.send(Some(approved.clone())),
        );
    }

    /// A sheet on `on` with "Cancel" first (Escape) and `action` second, without a key: Enter does nothing, the
    /// action needs a click. `then` runs after the click on `action`.
    pub(crate) fn ask(
        &mut self,
        on: &NSWindow,
        message: &str,
        info: &str,
        action: &str,
        then: impl Fn(&mut App) + 'static,
    ) {
        let alert = NSAlert::new(self.mtm);
        alert.setMessageText(&NSString::from_str(message));
        alert.setInformativeText(&NSString::from_str(info));
        let cancel = alert.addButtonWithTitle(&NSString::from_str(tr("Abbrechen", "Cancel")));
        cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
        let go = alert.addButtonWithTitle(&NSString::from_str(action));
        go.setKeyEquivalent(&NSString::from_str(""));
        let then = Rc::new(then);
        let done = RcBlock::new(move |response: NSModalResponse| {
            let (then, chosen) = (Rc::clone(&then), response == NSAlertSecondButtonReturn);
            with_app(move |a| {
                a.alert = None;
                if chosen {
                    then(a);
                }
            });
        });
        alert.beginSheetModalForWindow_completionHandler(on, Some(&done));
        self.alert = Some(alert);
    }

    /// "Undo": cancels the latest scheduled send. Too late once its transport runs.
    pub fn undo_send(&mut self) {
        let (Some(id), Some(desk)) = (self.latest_pending(), self.desk.clone()) else {
            return;
        };
        let token = self.pending[&id].token.clone();
        match desk.cancel(&id, &token) {
            Ok(true) => {
                self.end_pending(&id);
                self.refresh();
                self.sync_editor();
                self.tick_undo();
            }
            Ok(false) => self.hint(
                Some(tr(
                    "Zu spät, der Versand läuft bereits.",
                    "Too late, the send is under way.",
                )),
                None,
            ),
            Err(e) => self.hint(Some(&error_text(&e.0)), None),
        }
    }

    /// The bar shows the send scheduled last.
    fn latest_pending(&self) -> Option<String> {
        self.pending
            .iter()
            .max_by_key(|(_, p)| p.deadline)
            .map(|(id, _)| id.clone())
    }

    fn end_pending(&mut self, id: &str) {
        if let Some(p) = self.pending.remove(id) {
            // SAFETY: the activity this app began for the countdown.
            unsafe { NSProcessInfo::processInfo().endActivity(&p.activity) };
        }
    }

    /// The outcome of a countdown, on the main thread. Only the send of this ticket ends its countdown: an old
    /// countdown after an undo finds no approval and reports nothing.
    fn send_done(&mut self, id: &str, ticket: u64, r: FireResult) {
        let mine = self.pending.get(id).filter(|p| p.ticket == ticket);
        let told = mine.map(|p| (p.subject.clone(), p.recipients));
        if told.is_some() {
            self.end_pending(id);
        }
        self.refresh();
        self.sync_editor();
        self.tick_undo();
        let Some((subject, recipients)) = told else {
            return;
        };
        let Some(text) = fire_text(&r, recipients) else {
            return;
        };
        let line = format!("{}: {text}", quoted(&notify::clean(&subject)));
        log(&format!("send {id}: {}", fire_code(&r)));
        self.hint(Some(&line), None);
        if !matches!(r, FireResult::NotSent(_)) {
            notify::post("Draft Desk", &line, &format!("sent-{id}"), Some(id));
        }
    }

    fn start_undo_timer(&mut self) {
        if self.undo_timer.is_some() {
            return;
        }
        let block = RcBlock::new(|_: NonNull<NSTimer>| with_app(App::tick_undo));
        // SAFETY: a repeating timer on the main run loop; the block only reaches the app through the main-thread slot.
        let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(0.25, true, &block) };
        // Common modes: the countdown keeps moving while a menu is open or the window is resized.
        // SAFETY: the main run loop and a static Foundation constant.
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        self.undo_timer = Some(timer);
    }

    /// Redraws the bar: the send scheduled last and its seconds left; hides it (and stops the timer) when none runs.
    fn tick_undo(&mut self) {
        let Some(id) = self.latest_pending() else {
            self.editor.undo.hide();
            if let Some(t) = self.undo_timer.take() {
                t.invalidate();
            }
            return;
        };
        let p = &self.pending[&id];
        let left = p.deadline.saturating_duration_since(Instant::now());
        let secs = left.as_secs_f64().ceil() as u64;
        let more = match self.pending.len() {
            1 => String::new(),
            n => format!("({n} {}) ", tr("geplant", "scheduled")),
        };
        let s = &p.subject;
        let text = match (secs, crate::i18n::is_german()) {
            (0, true) => format!("{more}„{s}“ wird gesendet …"),
            (0, false) => format!("{more}“{s}” is being sent…"),
            (n, true) => format!("{more}„{s}“ wird in {n} s gesendet …"),
            (n, false) => format!("{more}“{s}” is sent in {n} s…"),
        };
        let total = p.undo.as_secs_f64().max(1.0);
        self.editor.undo.show(&text, left.as_secs_f64() / total);
    }

    /// Tells VoiceOver at once, without moving the focus.
    fn announce(&self, text: &str) {
        let app = NSApplication::sharedApplication(self.mtm);
        let message = NSString::from_str(text);
        let priority = NSNumber::new_isize(NSAccessibilityPriorityLevel::High.0);
        // SAFETY: static AppKit constants.
        let (key, level, name) = unsafe {
            (
                NSAccessibilityAnnouncementKey,
                NSAccessibilityPriorityKey,
                NSAccessibilityAnnouncementRequestedNotification,
            )
        };
        let info = NSDictionary::<NSString, AnyObject>::from_slices(
            &[key, level],
            &[message.as_ref(), priority.as_ref()],
        );
        let element: &AnyObject = app.as_ref();
        // SAFETY: the app as element, a valid notification name and user info of the documented keys.
        unsafe { NSAccessibilityPostNotificationWithUserInfo(element, name, Some(&info)) };
    }

    /// "Discard": a sheet first (with what happens to the mailbox draft), then the core discards on its own thread.
    /// An invalid draft goes to the archive instead, without a sheet: it can never be sent.
    fn discard(&mut self) {
        let a = self.current_actions();
        if a.archive {
            if let Some(id) = self.selected.clone() {
                self.archive_invalid(&id);
            }
            return;
        }
        if self.alert.is_some() || !a.discard {
            return;
        }
        let (Some(item), Some(desk)) = (self.shown.clone(), self.desk.clone()) else {
            return;
        };
        let sender = desk.sender();
        let kind = sender
            .store()
            .config()
            .account(&item.account)
            .map(|acc| acc.kind);
        let mailbox = match kind {
            Some(Kind::Smtp) => tr(
                " Der Postfach-Entwurf wandert in den Papierkorb, wenn er noch zu diesem Entwurf passt.",
                " The mailbox draft moves to the trash if it still matches this draft.",
            ),
            Some(Kind::Gmail) => tr(
                " Der Gmail-Entwurf wird mitgelöscht, sofern er unverändert ist.",
                " The Gmail draft is deleted too if it is unchanged.",
            ),
            _ => "",
        };
        let info = format!(
            "{}{mailbox}",
            tr(
                "Der Entwurf verschwindet aus der Warteschlange.",
                "The draft leaves the queue."
            )
        );
        let id = item.item_id;
        let window = self.window.clone();
        self.ask(
            &window,
            tr("Wirklich verwerfen?", "Discard this draft?"),
            &info,
            tr("Verwerfen", "Discard"),
            move |a| a.discard_confirmed(id.clone()),
        );
    }

    fn discard_confirmed(&mut self, id: String) {
        let Some(desk) = self.desk.clone() else {
            return;
        };
        // Typing not saved yet goes with the draft: no save, and no hint about it once the draft left the list.
        self.dirty = false;
        if let Some(t) = self.autosave.take() {
            t.invalidate();
        }
        self.discarding.insert(id.clone());
        self.sync_editor();
        spawn("draftdesk-discard", move || {
            let r = desk.discard(&id);
            on_main(move || with_app(move |a| a.discard_done(&id, r)));
        });
    }

    fn discard_done(&mut self, id: &str, r: draftdesk_core::Result<CleanupResult>) {
        self.discarding.remove(id);
        self.refresh();
        self.sync_editor();
        match r {
            Ok(CleanupResult::Failed(why)) => {
                let text = tr(
                    "Verworfen. Der Postfach-Entwurf konnte nicht aufgeräumt werden; bitte im Postfach löschen.",
                    "Discarded. The mailbox draft could not be cleaned up; please delete it in the mailbox.",
                );
                self.hint(Some(&format!("{text} ({why})")), None);
            }
            Ok(_) => {}
            Err(e) => self.hint(Some(&error_text(&e.0)), None),
        }
    }

    /// Moves an invalid draft out of the queue; nothing is sent.
    fn archive_invalid(&mut self, id: &str) {
        let result = self.desk.as_ref().map(|d| d.archive_invalid(id));
        self.refresh();
        if let Some(Err(e)) = result {
            self.hint(Some(&error_text(&e.0)), None);
        }
    }

    fn show_findings(&self) {
        let (checks, body, stale, running) = match &self.shown {
            Some(i) => (
                i.checks.as_ref(),
                i.body.as_str(),
                i.checks.as_ref().is_some_and(|c| is_stale(c, i)),
                self.checking.contains(&i.item_id),
            ),
            None => (None, "", false, false),
        };
        self.editor
            .findings
            .show(self.mtm, checks, body, stale, running);
    }

    /// "Check": saves, then runs the check command on its own thread; the draft is locked until the findings are
    /// back.
    fn check(&mut self) {
        let Some(id) = self.shown.as_ref().map(|i| i.item_id.clone()) else {
            return;
        };
        if !self.current_actions().check || !self.flush(true) {
            return;
        }
        let Some(desk) = self.desk.clone() else {
            return;
        };
        let back = id.clone();
        let done = move |r| on_main(move || with_app(move |a| a.check_done(&back, r)));
        match desk.check(&id, done) {
            Ok(()) => {
                self.checking.insert(id);
                self.sync_editor();
            }
            Err(e) => self.hint(Some(&error_text(&e.0)), None),
        }
    }

    fn check_done(&mut self, id: &str, r: draftdesk_core::Result<Item>) {
        self.checking.remove(id);
        let here = self.shown.as_ref().is_some_and(|i| i.item_id == id);
        match r {
            // Checks are no content: the fields still show the same text.
            Ok(item) if here => self.shown = Some(item),
            Ok(_) => {}
            Err(e) if here => self.hint(Some(&error_text(&e.0)), None),
            Err(_) => log(&format!("check of {id} failed")),
        }
        if here {
            self.sync_editor();
        }
        self.refresh();
    }

    /// "Fix": saves, then asks the fix command for a new text on its own thread. The answer replaces the text only
    /// if the editor still shows what was sent off, as one undo step, and is saved like typing; nothing is sent.
    fn fix(&mut self) {
        let Some(id) = self.shown.as_ref().map(|i| i.item_id.clone()) else {
            return;
        };
        if !self.current_actions().fix || !self.flush(true) {
            return;
        }
        let Some(desk) = self.desk.clone() else {
            return;
        };
        let (back, before) = (id.clone(), self.editor.body.string().to_string());
        let done = move |r| on_main(move || with_app(move |a| a.fix_done(&back, &before, r)));
        match desk.fix(&id, done) {
            Ok(()) => {
                self.hint(Some(tr("Korrektur läuft …", "Fixing…")), None);
                self.sync_editor();
            }
            Err(e) => self.hint(Some(&error_text(&e.0)), None),
        }
    }

    fn fix_done(&mut self, id: &str, before: &str, r: draftdesk_core::Result<String>) {
        let here = self.shown.as_ref().is_some_and(|i| i.item_id == id);
        let unchanged = here && self.editor.body.string().to_string() == before;
        // Unlocks the editor first: a read-only text view takes no replacement.
        self.sync_editor();
        match r {
            Ok(text) if unchanged => {
                self.editor.replace_body(&text);
                self.mark_dirty();
                self.hint(
                    Some(tr(
                        "Korrektur eingesetzt, Cmd+Z nimmt sie zurück.",
                        "Fix inserted; Cmd+Z takes it back.",
                    )),
                    None,
                );
            }
            Ok(_) if here => self.hint(
                Some(tr(
                    "Korrektur verworfen: der Text wurde inzwischen geändert.",
                    "Fix dropped: the text changed meanwhile.",
                )),
                None,
            ),
            Ok(_) => {
                log(&format!("fix of {id} dropped: another draft is shown"));
                self.hint(
                    Some(tr(
                        "Korrektur nicht übernommen: inzwischen war ein anderer Entwurf offen.",
                        "Fix not applied: another draft was open by then.",
                    )),
                    None,
                );
            }
            Err(e) if here => self.hint(Some(&error_text(&e.0)), None),
            Err(_) => log(&format!("fix of {id} failed")),
        }
    }

    /// Files may be dropped on an editable draft only.
    pub fn accepts_drop(&self) -> bool {
        self.editable()
    }

    /// Files from a drop or the file dialog: the fields are saved first, then the files copied into the draft; files
    /// that break a rule are named in the hint with the reason.
    pub fn add_files(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() || !self.editable() || !self.flush(true) {
            return;
        }
        let (Some(shown), Some(desk)) = (self.shown.clone(), self.desk.clone()) else {
            return;
        };
        let sender = desk.sender();
        match add_attachments(
            sender.store(),
            &shown.item_id,
            shown.content_revision,
            &paths,
        ) {
            Ok(att) => {
                self.shown = Some(att.item);
                self.sync_editor();
                if !att.skipped.is_empty() {
                    let list: Vec<String> = att
                        .skipped
                        .iter()
                        .map(|(p, why)| {
                            let name = Path::new(p).file_name().map(|n| n.to_string_lossy());
                            format!("{} ({why})", name.unwrap_or_default())
                        })
                        .collect();
                    let text = format!(
                        "{} {}",
                        tr("Nicht angehängt:", "Not attached:"),
                        list.join("; ")
                    );
                    self.hint(Some(&text), None);
                }
            }
            Err(e) => self.hint(Some(&error_text(&e.0)), None),
        }
    }

    /// The files of "Add Attachment…" for the draft the dialog was opened for. Is another one shown by now (the poll
    /// after a replacement, a click on a notification), nothing is attached.
    pub fn files_chosen(&mut self, for_id: &str, paths: Vec<PathBuf>) {
        if self.shown.as_ref().is_some_and(|i| i.item_id == for_id) {
            self.add_files(paths);
        } else {
            self.hint(
                Some(tr(
                    "Nichts angehängt: inzwischen ist ein anderer Entwurf ausgewählt.",
                    "Nothing attached: another draft is selected now.",
                )),
                None,
            );
        }
    }

    /// The file dialog of "Add Attachment…", as a sheet on the window.
    fn choose_files(&mut self) {
        let Some(id) = self
            .shown
            .as_ref()
            .filter(|_| self.editable())
            .map(|i| i.item_id.clone())
        else {
            return;
        };
        let panel = NSOpenPanel::openPanel(self.mtm);
        panel.setAllowsMultipleSelection(true);
        panel.setCanChooseDirectories(false);
        panel.setCanChooseFiles(true);
        let p = panel.clone();
        let done = RcBlock::new(move |response: NSModalResponse| {
            if response != NSModalResponseOK {
                return;
            }
            let paths: Vec<PathBuf> = p
                .URLs()
                .iter()
                .filter_map(|u| u.path())
                .map(|s| PathBuf::from(s.to_string()))
                .collect();
            let id = id.clone();
            with_app(move |a| a.files_chosen(&id, paths));
        });
        panel.beginSheetModalForWindow_completionHandler(&self.window, &done);
    }

    pub fn remove_attachment_at(&mut self, index: usize) {
        if !self.editable() || !self.flush(true) {
            return;
        }
        let (Some(shown), Some(desk)) = (self.shown.clone(), self.desk.clone()) else {
            return;
        };
        let Some(name) = shown.attachments.get(index).map(|a| a.name.clone()) else {
            return;
        };
        let sender = desk.sender();
        match remove_attachment(
            sender.store(),
            &shown.item_id,
            shown.content_revision,
            &name,
        ) {
            Ok(item) => {
                self.shown = Some(item);
                self.sync_editor();
            }
            Err(e) => self.hint(Some(&error_text(&e.0)), None),
        }
    }

    /// Quick Look for the safe types; any other file gets a hint and is never opened.
    pub fn open_attachment(&mut self, index: usize) {
        let (Some(shown), Some(desk)) = (&self.shown, &self.desk) else {
            return;
        };
        let Some(a) = shown.attachments.get(index) else {
            return;
        };
        if !attachments::quicklook_ok(&a.name) {
            let ext = Path::new(&a.name)
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            let text = format!("{} {ext}", tr("Keine Vorschau für", "No preview for"));
            self.hint(Some(text.trim_end()), None);
            return;
        }
        let Ok(dir) = desk.sender().store().item_dir(&shown.item_id) else {
            return;
        };
        let path = dir.join("att").join(&a.name);
        attachments::set_preview_items(vec![NSURL::fileURLWithPath(&NSString::from_str(
            &path.to_string_lossy(),
        ))]);
        // SAFETY: the shared panel on the main thread.
        if let Some(panel) = unsafe { QLPreviewPanel::sharedPreviewPanel(self.mtm) } {
            if panel.isVisible() {
                // SAFETY: plain call while this app controls the panel.
                unsafe { panel.reloadData() };
            } else {
                panel.makeKeyAndOrderFront(None);
            }
        }
    }

    /// Fills the editor with the selected draft as stored now (the poll's copy can be a second old).
    fn load_selected(&mut self) {
        self.dirty = false;
        if let Some(t) = self.autosave.take() {
            t.invalidate();
        }
        self.hint(None, None);
        let read = match (self.selected.as_deref(), &self.desk) {
            (Some(id), Some(desk)) => Some(desk.sender().store().read(id)),
            (Some(id), None) => self.items.iter().find(|r| r.id() == id).cloned(),
            (None, _) => None,
        };
        match read {
            Some(ItemRead::Valid(item)) => {
                let cfg = self
                    .desk
                    .as_ref()
                    .map(|d| d.sender().store().config().clone());
                self.editor.load(&item, &cfg.unwrap_or_default());
                let thread = self
                    .desk
                    .as_ref()
                    .and_then(|d| d.sender().store().thread(&item.item_id));
                self.editor.show_thread(thread.as_deref());
                self.shown = Some(*item);
                self.sync_editor();
            }
            Some(ItemRead::Invalid { .. })
                if self.desk.as_ref().is_some_and(|d| !d.config_ok()) =>
            {
                self.shown = None;
                self.editor.show_invalid();
                self.sync_editor();
                self.hint(
                    Some(tr(
                        "config.json kann nicht gelesen werden. Bis es repariert ist, erscheint jeder Entwurf ungültig; es geht nichts verloren.",
                        "config.json cannot be read. Until it is repaired every draft looks invalid; nothing is lost.",
                    )),
                    None,
                );
            }
            Some(ItemRead::Invalid { item_id, errors }) => {
                self.shown = None;
                self.editor.show_invalid();
                self.sync_editor();
                let text = format!(
                    "{} {}",
                    tr(
                        "Ungültiger Entwurf, kann nicht gesendet werden:",
                        "Invalid draft, it cannot be sent:"
                    ),
                    errors.join("; ")
                );
                let action = (
                    tr("Ins Archiv", "Move to Archive"),
                    HintAction::ArchiveInvalid(item_id),
                );
                self.hint(Some(&text), Some(action));
            }
            None => {
                self.shown = None;
                self.editor.show_nothing();
                // Nothing to act on: the toolbar and the Draft menu say so (they looked active at the start).
                self.sync_editor();
            }
        }
    }

    /// The selected draft after a poll: the same content keeps the fields as they are (the person may be typing);
    /// other content is loaded again when nothing is unsaved.
    fn refresh_selected(&mut self) {
        let fresh = self
            .selected
            .as_deref()
            .and_then(|id| self.items.iter().find(|r| r.id() == id))
            .cloned();
        match (&self.shown, fresh) {
            (Some(shown), Some(ItemRead::Valid(new)))
                if shown.item_id == new.item_id
                    && new.content_revision == shown.content_revision =>
            {
                self.shown = Some(*new);
                self.sync_editor();
            }
            (Some(shown), Some(ItemRead::Valid(new)))
                if shown.item_id == new.item_id && self.dirty =>
            {
                // ponytail: content changed outside while unsaved typing: the outside wins (cannot happen in practice,
                // only the window changes content).
                self.load_selected();
                let _ = new;
                self.hint(
                    Some(tr(
                        "Der Entwurf wurde außerhalb geändert; deine letzte Änderung ist nicht gespeichert.",
                        "The draft changed outside; your last change is not saved.",
                    )),
                    None,
                );
            }
            _ => self.load_selected(),
        }
    }

    /// New queue contents: the sidebar redraws and keeps the selected draft; if it is gone, the row at the same
    /// place (or the last one) is selected instead.
    pub fn set_items(&mut self, items: Vec<ItemRead>) {
        let before = self.selected.as_deref().and_then(sidebar::index_of);
        let now = chrono::Local::now();
        sidebar::set_rows(items.iter().map(|r| sidebar::row(r, now)).collect());
        self.items = items;
        self.table.reloadData();
        let count = sidebar::row_count();
        let index = self
            .selected
            .as_deref()
            .and_then(sidebar::index_of)
            .or_else(|| before.map(|i| i.min(count.saturating_sub(1))))
            .filter(|&i| i < count);
        self.select_index(index);
    }

    /// Selects a draft by id (`None`: no selection).
    pub fn select(&mut self, id: Option<&str>) {
        let index = id.and_then(sidebar::index_of);
        self.select_index(index);
    }

    fn select_index(&mut self, index: Option<usize>) {
        let id = index.and_then(sidebar::row_at).map(|r| r.id);
        let changed = id != self.selected;
        self.selected = id;
        match index {
            Some(i) => self
                .table
                .selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(i), false),
            // SAFETY: plain table call on the main thread; `None` = no sender.
            None => unsafe { self.table.deselectAll(None) },
        }
        if changed {
            self.load_selected();
        } else {
            self.refresh_selected();
        }
    }

    /// The person picked another row. Unsaved changes are saved first; if that fails, the draft stays selected
    /// with the reason and the offer to drop the changes.
    fn selection_changed(&mut self) {
        let row = usize::try_from(self.table.selectedRow()).ok();
        let id = row.and_then(sidebar::row_at).map(|r| r.id);
        if id == self.selected {
            return;
        }
        if !self.flush(true) {
            // Back to the row of the draft with the changes (AppKit calls again; that call finds nothing to do).
            if let Some(i) = self.selected.as_deref().and_then(sidebar::index_of) {
                self.table.selectRowIndexes_byExtendingSelection(
                    &NSIndexSet::indexSetWithIndex(i),
                    false,
                );
            }
            return;
        }
        self.selected = id;
        self.load_selected();
    }

    /// The real start: the standard root, the keychain, the configured transports.
    fn start(&mut self) {
        NSApplication::sharedApplication(self.mtm).disableRelaunchOnLogin();
        let root = match draftdesk_core::config::default_root() {
            Ok(r) => r,
            Err(e) => return self.fatal(&e.0),
        };
        let transports: SharedTransports = Arc::new(default_transport);
        let opened = Desk::open(
            &root,
            draftdesk_core::secrets::system(),
            transports,
            Arc::new(SystemClock),
        );
        self.use_desk(opened);
        self.begin();
    }

    /// Takes the opened desk; a second instance for the same root quits quietly.
    ///
    /// Leaving goes through `process::exit`, not `NSApplication::terminate`: this runs inside `with_app`, where the
    /// delegate's `applicationShouldTerminate` cannot reach the app and would cancel (found in the launch probe
    /// 2026-11-07). Nothing is open yet that needs closing.
    pub fn use_desk(&mut self, opened: Result<Desk, OpenError>) {
        match opened {
            Ok(desk) => self.desk = Some(Arc::new(desk)),
            Err(OpenError::AlreadyRunning) => {
                log("another Draft Desk runs for this root; this one quits");
                std::process::exit(0);
            }
            Err(OpenError::Failed(e)) => self.fatal(&e.0),
        }
    }

    fn fatal(&mut self, detail: &str) {
        log(&format!("start failed: {detail}"));
        let alert = NSAlert::new(self.mtm);
        alert.setMessageText(&NSString::from_str(tr(
            "Draft Desk kann nicht starten",
            "Draft Desk cannot start",
        )));
        alert.setInformativeText(&NSString::from_str(detail));
        alert.runModal();
        // Not `terminate`: see `use_desk`.
        std::process::exit(1);
    }

    /// The first activation after the start (a launch by click) shows the window; `open -g` from the command line
    /// does not activate, so it shows nothing (a new draft brings only a notification).
    fn first_activation(&mut self) {
        if !self.shown_once {
            self.show_window(true);
        }
    }

    /// `activate`: bring the app to the front with the window as key window; otherwise only order the window in,
    /// without taking the keyboard from another app.
    pub fn show_window(&mut self, activate: bool) {
        self.shown_once = true;
        if activate {
            NSApplication::sharedApplication(self.mtm).activate();
            self.window.makeKeyAndOrderFront(None);
        } else {
            self.window.orderFrontRegardless();
        }
    }

    /// Closing hides the window, after saving; a failed save keeps it open with the reason, and
    /// so does a scheduled send (its Undo must stay in reach).
    pub fn window_should_close(&mut self) -> bool {
        if !self.pending.is_empty() {
            self.hint(
                Some(tr(
                    "Während ein Versand geplant ist, bleibt das Fenster offen.",
                    "The window stays open while a send is scheduled.",
                )),
                None,
            );
            return false;
        }
        if self.flush(true) {
            self.window.orderOut(None);
        }
        false
    }

    /// Unsaved changes that cannot be saved ask first; Cancel there leaves everything as it was, countdowns
    /// included (one that ends while the question is open sends). Then quitting cancels every countdown (nothing more
    /// is sent) and waits for a send already under way.
    pub fn should_terminate(&mut self) -> NSApplicationTerminateReply {
        if !self.flush(true) {
            let alert = NSAlert::new(self.mtm);
            alert.setMessageText(&NSString::from_str(tr(
                "Änderungen nicht gespeichert",
                "Changes not saved",
            )));
            alert.setInformativeText(&NSString::from_str(
                &self.editor.banner.text().unwrap_or_default(),
            ));
            let quit = alert.addButtonWithTitle(&NSString::from_str(tr("Beenden", "Quit")));
            quit.setKeyEquivalent(&NSString::from_str(""));
            let cancel = alert.addButtonWithTitle(&NSString::from_str(tr("Abbrechen", "Cancel")));
            cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
            if alert.runModal() != NSAlertFirstButtonReturn {
                return NSApplicationTerminateReply::TerminateCancel;
            }
        }
        if let Some(desk) = self.desk.clone() {
            for id in self.pending_ids() {
                let token = self.pending[&id].token.clone();
                if desk.cancel(&id, &token).unwrap_or(false) {
                    self.end_pending(&id);
                }
            }
            // A countdown left over has fired (perhaps while the question was open): its outcome is on its way,
            // also when its send is no longer under way.
            if !self.pending.is_empty() || desk.sending() {
                self.sync_editor();
                self.tick_undo();
                self.hint(
                    Some(tr(
                        "Ein Versand läuft gerade. Bitte nach seinem Ergebnis noch einmal beenden.",
                        "A send is under way. Please quit again after its outcome.",
                    )),
                    None,
                );
                return NSApplicationTerminateReply::TerminateCancel;
            }
        }
        NSApplicationTerminateReply::TerminateNow
    }

    pub fn on_action(&mut self, action: Action) {
        match action {
            Action::TogglePreview => self.editor.set_preview(!self.editor.preview_on()),
            Action::Check => self.check(),
            Action::Fix => self.fix(),
            Action::Discard => self.discard(),
            Action::Send => self.send(None),
            Action::Settings => self.open_settings(),
        }
    }
}

/// The outcome for the log: its kind only (reasons can hold addresses).
fn fire_code(r: &FireResult) -> &'static str {
    match r {
        FireResult::Sent => "sent",
        FireResult::NotSent(_) => "not_sent",
        FireResult::Failed(_) => "failed",
        FireResult::Rejected(_) => "rejected",
        FireResult::Unknown(_) => "unknown",
    }
}
