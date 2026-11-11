//! AppKit tests of the app. AppKit wants the main thread, so this is its own harness (`harness = false`): `main`
//! points every path (root, secrets, log) into a temporary folder, runs each test inside an autorelease pool with its
//! panic caught, prints one line per test and exits 1 if any failed. Every window stays invisible and lets clicks
//! through (`keep_windows_invisible`): the tests run while someone works at the same Mac. Every test hides what it
//! showed.

fn main() {
    #[cfg(target_os = "macos")]
    harness::run();
}

#[cfg(target_os = "macos")]
mod harness {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;
    use std::sync::Arc;

    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyObject, Sel};
    use objc2::{MainThreadMarker, msg_send, sel};
    use objc2_app_kit::{
        NSAccessibility, NSApplication, NSBackgroundColorAttributeName, NSEvent,
        NSEventModifierFlags, NSEventType, NSMenu, NSTextInputClient,
    };
    use objc2_foundation::{NSDate, NSIndexSet, NSInteger, NSPoint, NSRange, NSRunLoop, NSString};
    use objc2_quick_look_ui::QLPreviewPanel;

    use draftdesk_core::accounts::CommandSlot;
    use draftdesk_core::add::add;
    use draftdesk_core::config::{Config, Kind, Security};
    use draftdesk_core::desk::{Desk, SharedTransports};
    use draftdesk_core::item::{Draft, Item, ItemRead, State};
    use draftdesk_core::secrets::MemorySecrets;
    use draftdesk_core::sender::{SystemClock, default_transport};
    use draftdesk_macos::app::{Action, App, install, uninstall, with_app, with_app_now};
    use draftdesk_macos::{editor, i18n, menu, settings, sidebar, toolbar};
    use objc2::rc::Retained;
    use objc2_app_kit::{
        NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplicationTerminateReply,
        NSModalResponse, NSWindow,
    };
    use objc2_app_kit::{
        NSAppearance, NSAppearanceCustomization, NSBitmapImageFileType, NSButton, NSControl,
        NSImageView, NSScroller, NSTextField, NSTextView, NSView,
    };
    use objc2_foundation::NSDictionary;
    use std::path::Path;

    type Test = (&'static str, fn(MainThreadMarker));

    /// The tests run on a Mac someone is working at: no window of theirs may cover the screen or catch a click. A
    /// window is born transparent, click-through and without its open/close animation (`NSWindow`'s designated
    /// initializer, which every panel, sheet and alert goes through), so even its first frame stays invisible; its
    /// alpha setter only ever sets 0 (fades); each time the main run loop is about to wait or exit, every window is
    /// clamped back (a window loaded from a nib skips the initializer). Views still lay out, draw and answer as before.
    fn keep_windows_invisible() {
        use std::ffi::c_void;
        use std::sync::OnceLock;

        use objc2::runtime::{AnyClass, Bool, Imp};
        use objc2_app_kit::NSWindowAnimationBehavior;
        use objc2_foundation::NSRect;

        // Only what differs: a pass without a new window then costs nothing (a test measures that the main thread idles).
        fn hide(window: &NSWindow) {
            if window.alphaValue() != 0.0 {
                window.setAlphaValue(0.0);
            }
            if !window.ignoresMouseEvents() {
                window.setIgnoresMouseEvents(true);
            }
            if window.animationBehavior() != NSWindowAnimationBehavior::None {
                window.setAnimationBehavior(NSWindowAnimationBehavior::None);
            }
        }

        type Init = unsafe extern "C-unwind" fn(
            *mut AnyObject,
            Sel,
            NSRect,
            usize,
            usize,
            Bool,
        ) -> *mut AnyObject;
        static ORIGINAL: OnceLock<Imp> = OnceLock::new();
        unsafe extern "C-unwind" fn init_hidden(
            this: *mut AnyObject,
            cmd: Sel,
            rect: NSRect,
            style: usize,
            backing: usize,
            defer: Bool,
        ) -> *mut AnyObject {
            // SAFETY: the stored IMP is NSWindow's own initializer with exactly this signature.
            let original: Init =
                unsafe { std::mem::transmute(*ORIGINAL.get().expect("set before the swap")) };
            let window = unsafe { original(this, cmd, rect, style, backing, defer) };
            // SAFETY: a non-null result of NSWindow's initializer is an initialized NSWindow.
            if let Some(window) = unsafe { window.cast::<NSWindow>().as_ref() } {
                hide(window);
            }
            window
        }
        let init = AnyClass::get(c"NSWindow")
            .and_then(|c| c.instance_method(sel!(initWithContentRect:styleMask:backing:defer:)))
            .expect("NSWindow's designated initializer");
        // SAFETY: same signature as the replaced method; swapped once, on the main thread, before any window exists.
        let swapped =
            unsafe { init.set_implementation(std::mem::transmute::<Init, Imp>(init_hidden)) };
        ORIGINAL.set(swapped).expect("swapped once");

        // Quick Look fades its panel in by setting the alpha itself, between two passes of the run loop.
        type SetAlpha = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, f64);
        static ORIGINAL_ALPHA: OnceLock<Imp> = OnceLock::new();
        unsafe extern "C-unwind" fn set_alpha_zero(this: *mut AnyObject, cmd: Sel, _alpha: f64) {
            // SAFETY: the stored IMP is NSWindow's own `setAlphaValue:` with exactly this signature.
            let original: SetAlpha =
                unsafe { std::mem::transmute(*ORIGINAL_ALPHA.get().expect("set before the swap")) };
            unsafe { original(this, cmd, 0.0) }
        }
        let set_alpha = AnyClass::get(c"NSWindow")
            .and_then(|c| c.instance_method(sel!(setAlphaValue:)))
            .expect("NSWindow's alpha setter");
        // SAFETY: as above.
        let swapped = unsafe {
            set_alpha.set_implementation(std::mem::transmute::<SetAlpha, Imp>(set_alpha_zero))
        };
        ORIGINAL_ALPHA.set(swapped).expect("swapped once");

        // Quick Look opens its panel with its own fade, drawn in separate transition windows the two swaps above never
        // see (measured: the 800x500 panel was the one window left visible): it opens like any other window instead.
        let open = sel!(makeKeyAndOrderFront:);
        let (Some(quick_look), Some(plain)) = (
            AnyClass::get(c"QLPreviewPanel").and_then(|c| c.instance_method(open)),
            AnyClass::get(c"NSWindow").and_then(|c| c.instance_method(open)),
        ) else {
            panic!("Quick Look's panel and NSWindow both open with makeKeyAndOrderFront:");
        };
        // SAFETY: the same selector one class up, so the same signature; swapped once, on the main thread.
        unsafe { quick_look.set_implementation(plain.implementation()) };

        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            fn CFRunLoopGetMain() -> *mut c_void;
            fn CFRunLoopObserverCreateWithHandler(
                allocator: *const c_void,
                activities: usize,
                repeats: u8,
                order: isize,
                block: &block2::Block<dyn Fn(*mut c_void, usize)>,
            ) -> *mut c_void;
            fn CFRunLoopAddObserver(
                run_loop: *mut c_void,
                observer: *mut c_void,
                mode: *const c_void,
            );
            static kCFRunLoopCommonModes: *const c_void;
        }
        const BEFORE_WAITING_OR_EXIT: usize = (1 << 5) | (1 << 7);
        let clamp = block2::RcBlock::new(|_: *mut c_void, _: usize| {
            let mtm = MainThreadMarker::new().expect("the main run loop runs on the main thread");
            for window in NSApplication::sharedApplication(mtm).windows().iter() {
                hide(&window);
            }
        });
        // SAFETY: CoreFoundation calls on the main thread; CF copies the block, the observer lives as long as the process.
        unsafe {
            let observer = CFRunLoopObserverCreateWithHandler(
                std::ptr::null(),
                BEFORE_WAITING_OR_EXIT,
                1,
                0,
                &clamp,
            );
            CFRunLoopAddObserver(CFRunLoopGetMain(), observer, kCFRunLoopCommonModes);
        }
    }

    fn windows_stay_invisible(mtm: MainThreadMarker) {
        install(App::new(mtm));
        let window = with_app_now(|a| a.window.clone()).unwrap();
        assert!(
            window.alphaValue() == 0.0 && window.ignoresMouseEvents(),
            "born invisible and click-through, before its first frame"
        );
        with_app_now(|a| a.show_window(false));
        window.setAlphaValue(1.0); // as a fade-in does
        assert!(
            window.alphaValue() == 0.0,
            "the setter keeps it transparent"
        );
        window.setIgnoresMouseEvents(false);
        run_loop(0.05);
        assert!(window.isVisible(), "shown for the app");
        assert!(window.ignoresMouseEvents(), "clamped back on the next pass");
    }

    pub fn run() {
        let tmp = std::env::temp_dir().join(format!("draftdesk-appkit-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // SAFETY: the first statements, no other thread runs yet. Nothing may touch the user's root, keychain or log.
        unsafe {
            std::env::set_var("DRAFTDESK_HOME", tmp.join("home"));
            std::env::set_var("DRAFTDESK_TEST_SECRETS_FILE", tmp.join("secrets.json"));
            std::env::set_var("DRAFTDESK_LOG", tmp.join("app.log"));
        }
        let mtm = MainThreadMarker::new().expect("harness = false runs main on the main thread");
        let _ = NSApplication::sharedApplication(mtm);
        keep_windows_invisible();
        i18n::set_german(true);
        let tests: Vec<Test> = vec![
            ("windows_stay_invisible", windows_stay_invisible),
            (
                "menu_has_edit_and_send_without_shortcut",
                menu_has_edit_and_send_without_shortcut,
            ),
            ("close_hides_window", close_hides_window),
            ("reopen_shows_window", reopen_shows_window),
            (
                "sidebar_lists_drafts_with_labels",
                sidebar_lists_drafts_with_labels,
            ),
            ("selection_survives_reload", selection_survives_reload),
            ("editor_shows_item_fields", editor_shows_item_fields),
            (
                "form_round_trip_is_empty_diff",
                form_round_trip_is_empty_diff,
            ),
            ("read_only_when_not_ready", read_only_when_not_ready),
            ("fixed_name_locks_name_field", fixed_name_locks_name_field),
            (
                "invalid_draft_offers_the_archive",
                invalid_draft_offers_the_archive,
            ),
            (
                "typing_then_flush_saves_and_marks_edited",
                typing_then_flush_saves_and_marks_edited,
            ),
            (
                "silent_autosave_failure_keeps_text_dirty",
                silent_autosave_failure_keeps_text_dirty,
            ),
            (
                "loud_flush_failure_blocks_switch_and_offers_revert",
                loud_flush_failure_blocks_switch_and_offers_revert,
            ),
            (
                "invisible_characters_highlighted_and_removed_with_undo",
                invisible_characters_highlighted_and_removed_with_undo,
            ),
            ("substitutions_are_off", substitutions_are_off),
            (
                "text_view_takes_no_file_drops",
                text_view_takes_no_file_drops,
            ),
            (
                "preview_config_has_no_javascript",
                preview_config_has_no_javascript,
            ),
            (
                "toggle_switches_between_text_and_preview",
                toggle_switches_between_text_and_preview,
            ),
            (
                "dropped_paths_become_attachments",
                dropped_paths_become_attachments,
            ),
            (
                "remove_attachment_updates_row",
                remove_attachment_updates_row,
            ),
            (
                "drop_refused_when_not_editable",
                drop_refused_when_not_editable,
            ),
            (
                "quick_look_takes_the_app_as_controller",
                quick_look_takes_the_app_as_controller,
            ),
            (
                "findings_render_levels_and_stale",
                findings_render_levels_and_stale,
            ),
            (
                "findings_on_one_sentence_share_a_row",
                findings_on_one_sentence_share_a_row,
            ),
            (
                "check_result_appears_and_unlocks",
                check_result_appears_and_unlocks,
            ),
            (
                "fix_replaces_text_once_and_undo_restores",
                fix_replaces_text_once_and_undo_restores,
            ),
            (
                "fix_for_a_draft_no_longer_shown_is_reported",
                fix_for_a_draft_no_longer_shown_is_reported,
            ),
            (
                "fix_discarded_when_text_changed_meanwhile",
                fix_discarded_when_text_changed_meanwhile,
            ),
            (
                "send_waits_for_undo_then_sends",
                send_waits_for_undo_then_sends,
            ),
            (
                "undo_cancels_and_unlocks_editor",
                undo_cancels_and_unlocks_editor,
            ),
            (
                "confirmation_dialog_cancel_is_escape_and_send_has_no_key",
                confirmation_dialog_cancel_is_escape_and_send_has_no_key,
            ),
            (
                "close_refused_during_countdown",
                close_refused_during_countdown,
            ),
            (
                "quit_cancels_countdown_without_sending",
                quit_cancels_countdown_without_sending,
            ),
            (
                "cancel_in_the_quit_question_keeps_the_countdown",
                cancel_in_the_quit_question_keeps_the_countdown,
            ),
            (
                "quit_waits_for_a_countdown_it_could_not_cancel",
                quit_waits_for_a_countdown_it_could_not_cancel,
            ),
            ("discard_after_confirmation", discard_after_confirmation),
            ("invalid_item_goes_to_archive", invalid_item_goes_to_archive),
            (
                "new_item_is_selected_and_window_stays",
                new_item_is_selected_and_window_stays,
            ),
            (
                "fallback_without_notifications_orders_front",
                fallback_without_notifications_orders_front,
            ),
            (
                "poll_merges_checks_without_touching_text",
                poll_merges_checks_without_touching_text,
            ),
            (
                "superseded_selection_moves_to_new_version",
                superseded_selection_moves_to_new_version,
            ),
            (
                "cli_discard_runs_cleanup_command",
                cli_discard_runs_cleanup_command,
            ),
            (
                "notify_is_noop_without_bundle",
                notify_is_noop_without_bundle,
            ),
            ("settings_window_opens_once", settings_window_opens_once),
            (
                "smtp_sheet_saves_account_and_password",
                smtp_sheet_saves_account_and_password,
            ),
            (
                "remove_account_after_confirmation",
                remove_account_after_confirmation,
            ),
            ("program_must_be_executable", program_must_be_executable),
            (
                "command_sheet_saves_the_time_limit",
                command_sheet_saves_the_time_limit,
            ),
            (
                "every_control_has_an_accessibility_label",
                every_control_has_an_accessibility_label,
            ),
            (
                "send_has_no_key_equivalent_anywhere",
                send_has_no_key_equivalent_anywhere,
            ),
            ("snapshots", snapshots),
            (
                "read_only_header_keeps_its_width",
                read_only_header_keeps_its_width,
            ),
            (
                "account_sheets_show_all_their_fields",
                account_sheets_show_all_their_fields,
            ),
            (
                "a_job_before_the_app_is_installed_waits_for_it",
                a_job_before_the_app_is_installed_waits_for_it,
            ),
            (
                "a_job_queued_while_the_app_is_busy_does_not_spin",
                a_job_queued_while_the_app_is_busy_does_not_spin,
            ),
            (
                "a_notification_for_a_gone_draft_keeps_the_selection",
                a_notification_for_a_gone_draft_keeps_the_selection,
            ),
            (
                "typed_recipient_without_separator_counts",
                typed_recipient_without_separator_counts,
            ),
            (
                "strip_keeps_the_caret_on_a_character",
                strip_keeps_the_caret_on_a_character,
            ),
            (
                "confirmation_is_bound_to_the_draft_it_was_asked_for",
                confirmation_is_bound_to_the_draft_it_was_asked_for,
            ),
            (
                "chosen_files_go_to_the_draft_the_dialog_was_opened_for",
                chosen_files_go_to_the_draft_the_dialog_was_opened_for,
            ),
            (
                "replaced_while_typing_says_the_change_is_lost",
                replaced_while_typing_says_the_change_is_lost,
            ),
            (
                "a_replacement_from_before_is_no_news",
                a_replacement_from_before_is_no_news,
            ),
            (
                "broken_config_offers_no_archive",
                broken_config_offers_no_archive,
            ),
            (
                "failed_mailbox_cleanup_after_discard_is_told",
                failed_mailbox_cleanup_after_discard_is_told,
            ),
            (
                "google_sheet_checks_the_name_before_the_sign_in",
                google_sheet_checks_the_name_before_the_sign_in,
            ),
            ("preview_answers_drops_itself", preview_answers_drops_itself),
            (
                "nothing_selected_offers_no_actions",
                nothing_selected_offers_no_actions,
            ),
        ];
        // `cargo test -p draftdesk-macos --test appkit -- <part>`: only the tests whose name holds <part>.
        let only = std::env::args().nth(1);
        let tests: Vec<Test> = tests
            .into_iter()
            .filter(|(name, _)| only.as_deref().is_none_or(|o| name.contains(o)))
            .collect();
        let mut failed = 0;
        let total = tests.len();
        for (name, test) in tests {
            let ok = catch_unwind(AssertUnwindSafe(|| autoreleasepool(|_| test(mtm)))).is_ok();
            println!("test {name} ... {}", if ok { "ok" } else { "FAILED" });
            failed += usize::from(!ok);
            // Hide what the test showed and drop its app (and with it the desk and its lock).
            if let Some(app) = uninstall() {
                app.window.orderOut(None);
                if let Some(s) = &app.settings {
                    s.window.orderOut(None);
                }
            }
        }
        println!("{} passed, {failed} failed", total - failed);
        let _ = std::fs::remove_dir_all(&tmp);
        if failed > 0 {
            std::process::exit(1);
        }
    }

    /// Every item of a menu and its submenus, depth first: (title, action, key equivalent).
    fn items(m: &NSMenu, out: &mut Vec<(String, Option<Sel>, String)>) {
        for i in m.itemArray().iter() {
            out.push((
                i.title().to_string(),
                i.action(),
                i.keyEquivalent().to_string(),
            ));
            if let Some(sub) = i.submenu() {
                items(&sub, out);
            }
        }
    }

    fn menu_has_edit_and_send_without_shortcut(mtm: MainThreadMarker) {
        let app = App::new(mtm);
        let bar = menu::main_menu(mtm, &app.delegate);
        let mut all = Vec::new();
        items(&bar, &mut all);
        let find = |action: Sel| all.iter().find(|(_, a, _)| *a == Some(action)).cloned();
        for (action, key) in [
            (sel!(copy:), "c"),
            (sel!(paste:), "v"),
            (sel!(undo:), "z"),
            (sel!(terminate:), "q"),
        ] {
            assert_eq!(
                find(action).map(|i| i.2),
                Some(key.to_string()),
                "{action:?}"
            );
        }
        let sends: Vec<_> = all
            .iter()
            .filter(|(_, a, _)| *a == Some(sel!(sendDraft:)))
            .collect();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].2, "", "Send has no key equivalent");
        assert_eq!(find(sel!(discardDraft:)).unwrap().2, "");
        assert_eq!(find(sel!(showSettings:)).unwrap().2, ",");
    }

    fn close_hides_window(mtm: MainThreadMarker) {
        install(App::new(mtm));
        let window = with_app_now(|a| {
            a.show_window(false);
            a.window.clone()
        })
        .unwrap();
        assert!(window.isVisible());
        window.performClose(None);
        assert!(!window.isVisible(), "hidden");
        assert!(with_app_now(|a| a.window.isReleasedWhenClosed()) == Some(false));
    }

    fn reopen_shows_window(mtm: MainThreadMarker) {
        install(App::new(mtm));
        let (delegate, window) = with_app_now(|a| (a.delegate.clone(), a.window.clone())).unwrap();
        assert!(!window.isVisible());
        let ns_app = NSApplication::sharedApplication(mtm);
        // SAFETY: the delegate method AppKit calls for a click on the Dock icon.
        let handled: bool = unsafe {
            msg_send![&*delegate, applicationShouldHandleReopen: &*ns_app, hasVisibleWindows: false]
        };
        assert!(handled && window.isVisible());
        window.orderOut(None);
    }

    // ---------- Fixtures: a desk on a temporary root, drafts added the real way ----------

    const CFG: &str = r#"{"accounts":[{"name":"work","kind":"command",
        "senders":[{"address":"me@example.org","name":"Me"},{"address":"boss@example.org","name":"The Boss","fixed_name":true}],
        "send_command":["/bin/cat"]}]}"#;

    /// A desk on a fresh temporary root with `config`; the folder is removed when it drops.
    struct Fixture {
        root: PathBuf,
        desk: Option<Desk>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.desk = None;
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(name: &str, config: &str) -> Fixture {
        let root =
            std::env::temp_dir().join(format!("draftdesk-appkit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("config.json"), config).unwrap();
        let transports: SharedTransports = Arc::new(default_transport);
        let desk = Desk::open(
            &root,
            Arc::new(MemorySecrets::default()),
            transports,
            Arc::new(SystemClock),
        )
        .unwrap_or_else(|_| panic!("desk opens"));
        Fixture {
            root,
            desk: Some(desk),
        }
    }

    /// A draft as an agent hands it over; `extra` adds fields (checks, thread).
    fn draft(subject: &str, body: &str, extra: serde_json::Value) -> Draft {
        let mut d = serde_json::json!({
            "schema": "draftdesk/1", "account": "work", "from": {"address": "me@example.org"},
            "to": ["Ann <ann@example.org>"], "subject": subject, "body": body,
            "origin": {"agent": "test", "session": "s1"}
        });
        if let (Some(map), serde_json::Value::Object(more)) = (d.as_object_mut(), extra) {
            map.extend(more);
        }
        serde_json::from_value(d).unwrap()
    }

    impl Fixture {
        fn add(&self, subject: &str) -> String {
            self.add_body(subject, "Hello Ann,\n\nthanks.\n")
        }
        fn add_body(&self, subject: &str, body: &str) -> String {
            self.add_draft(draft(subject, body, serde_json::json!({})))
        }
        fn add_draft(&self, d: Draft) -> String {
            let desk = self.desk.as_ref().unwrap();
            add(desk.sender().store(), d, None, chrono::Local::now())
                .unwrap()
                .item_id
        }
        /// Moves the desk into an installed app and fills the sidebar from a poll.
        fn install(&mut self, mtm: MainThreadMarker) {
            let mut app = App::new(mtm);
            app.use_desk(Ok(self.desk.take().unwrap()));
            install(app);
            refresh();
        }
    }

    /// One poll into the window, as the app's tick does.
    fn refresh() {
        with_app_now(App::refresh).unwrap();
        run_loop(0.05);
    }

    /// Runs the main run loop: queued app work and timers get their turn.
    fn run_loop(seconds: f64) {
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(seconds));
    }

    fn sidebar_lists_drafts_with_labels(mtm: MainThreadMarker) {
        let mut f = fixture("sb1", CFG);
        f.add("Re: Offer");
        f.add("Invoice");
        f.install(mtm);
        let table = with_app_now(|a| a.table.clone()).unwrap();
        assert_eq!(table.numberOfRows(), 2);
        let titles: Vec<String> = (0..2).map(|i| sidebar::row_at(i).unwrap().title).collect();
        assert!(
            titles.contains(&"Re: Offer".to_string()) && titles.contains(&"Invoice".to_string())
        );
        let view = table.viewAtColumn_row_makeIfNecessary(0, 0, true).unwrap();
        let label = view.accessibilityLabel().unwrap().to_string();
        assert!(label.contains("Bereit"), "{label}");
    }

    fn selection_survives_reload(mtm: MainThreadMarker) {
        let mut f = fixture("sb2", CFG);
        let a = f.add("A");
        let b = f.add("B");
        f.install(mtm);
        with_app_now(|app| app.select(Some(&b))).unwrap();
        run_loop(0.05);
        refresh();
        assert_eq!(
            with_app_now(|app| app.selected.clone()).unwrap(),
            Some(b.clone())
        );
        let table = with_app_now(|app| app.table.clone()).unwrap();
        assert_eq!(
            sidebar::row_at(usize::try_from(table.selectedRow()).unwrap())
                .unwrap()
                .id,
            b
        );
        // B leaves the queue: the selection moves to the row at its place, which is A now.
        with_app_now(|app| {
            let desk = app.desk.as_ref().unwrap();
            desk.sender().store().archive(&b).unwrap();
        })
        .unwrap();
        refresh();
        assert_eq!(with_app_now(|app| app.selected.clone()).unwrap(), Some(a));
    }

    /// A fixture with one draft, installed and selected.
    fn one_draft(mtm: MainThreadMarker, name: &str) -> (Fixture, String) {
        let mut f = fixture(name, CFG);
        let id = f.add("Re: Offer");
        f.install(mtm);
        with_app_now(|a| a.select(Some(&id))).unwrap();
        (f, id)
    }

    fn editor_shows_item_fields(mtm: MainThreadMarker) {
        let (_f, _id) = one_draft(mtm, "ed1");
        with_app_now(|a| {
            let e = &a.editor;
            assert!(!e.root.isHidden() && e.empty.isHidden());
            assert_eq!(editor::tokens(&e.to), vec!["Ann <ann@example.org>"]);
            assert!(editor::tokens(&e.cc).is_empty());
            assert_eq!(e.subject.stringValue().to_string(), "Re: Offer");
            assert_eq!(
                e.from.titleOfSelectedItem().unwrap().to_string(),
                "Me <me@example.org>"
            );
            assert_eq!(e.from_name.stringValue().to_string(), "Me");
            assert_eq!(e.body.string().to_string(), "Hello Ann,\n\nthanks.\n");
            assert!(e.subject.isEditable() && e.to.isEditable() && e.body.isEditable());
        })
        .unwrap();
    }

    fn form_round_trip_is_empty_diff(mtm: MainThreadMarker) {
        let (_f, _id) = one_draft(mtm, "ed2");
        with_app_now(|a| {
            let shown = a.shown.clone().unwrap();
            assert!(draftdesk_core::edit::diff(&shown, &a.editor.form()).is_empty());
            a.editor
                .subject
                .setStringValue(&objc2_foundation::NSString::from_str("Changed"));
            assert_eq!(
                draftdesk_core::edit::diff(&shown, &a.editor.form())
                    .subject
                    .as_deref(),
                Some("Changed")
            );
        })
        .unwrap();
    }

    fn read_only_when_not_ready(mtm: MainThreadMarker) {
        let (_f, id) = one_draft(mtm, "ed3");
        with_app_now(|a| {
            a.desk
                .as_ref()
                .unwrap()
                .sender()
                .store()
                .mutate(&id, &[], |i| {
                    i.state = draftdesk_core::item::State::Failed;
                    Ok(())
                })
                .unwrap();
        })
        .unwrap();
        refresh();
        with_app_now(|a| {
            let e = &a.editor;
            assert!(!e.subject.isEditable() && !e.to.isEditable() && !e.body.isEditable());
            assert!(!e.from.isEnabled());
        })
        .unwrap();
    }

    fn fixed_name_locks_name_field(mtm: MainThreadMarker) {
        let (_f, _id) = one_draft(mtm, "ed4");
        with_app_now(|a| {
            assert!(a.editor.from_name.isEditable());
            a.editor.from.selectItemAtIndex(1);
            a.editor.from_changed();
            assert_eq!(a.editor.from_name.stringValue().to_string(), "The Boss");
            assert!(!a.editor.from_name.isEditable());
            assert_eq!(a.editor.form().from_address, "boss@example.org");
        })
        .unwrap();
    }

    fn invalid_draft_offers_the_archive(mtm: MainThreadMarker) {
        let mut f = fixture("ed5", CFG);
        let bad = "20261107-120000-deadbeef";
        let dir = f.root.join("state/queue").join(bad);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("item.json"), "{broken").unwrap();
        f.install(mtm);
        with_app_now(|a| a.select(Some(bad))).unwrap();
        let text = with_app_now(|a| a.editor.banner.text()).unwrap().unwrap();
        assert!(text.starts_with("Ungültiger Entwurf"), "{text}");
        let target = with_app_now(|a| a.editor.banner.action.clone()).unwrap();
        // The click on "Ins Archiv".
        // SAFETY: a button of the app's own window, main thread.
        unsafe { target.performClick(None) };
        run_loop(0.05);
        assert!(!dir.exists(), "moved to the archive");
        assert_eq!(with_app_now(|a| a.items.len()).unwrap(), 0);
    }

    // ---------- Text and saving (type in place, saved automatically) ----------

    /// The item as stored now, in the queue or the archive.
    fn stored(id: &str) -> Item {
        let read =
            with_app_now(|a| a.desk.as_ref().unwrap().sender().store().read_any(id)).unwrap();
        match read {
            ItemRead::Valid(i) => *i,
            ItemRead::Invalid { errors, .. } => panic!("invalid: {errors:?}"),
        }
    }

    /// The click on the hint's button.
    fn click_hint() {
        let button = with_app_now(|a| a.editor.banner.action.clone()).unwrap();
        // SAFETY: a button of the app's own window, main thread.
        unsafe { button.performClick(None) };
        run_loop(0.05);
    }

    fn typing_then_flush_saves_and_marks_edited(mtm: MainThreadMarker) {
        let (_f, id) = one_draft(mtm, "tx1");
        let body = with_app_now(|a| a.editor.body.clone()).unwrap();
        // Typing at the end, the way the keyboard does it: the text view tells its delegate.
        body.setSelectedRange(NSRange::new(body.string().length(), 0));
        // SAFETY: an NSString is what the input system hands over; `NSNotFound` = at the selection.
        unsafe {
            body.insertText_replacementRange(
                &NSString::from_str("PS: see you"),
                NSRange::new(objc2_foundation::NSNotFound as usize, 0),
            )
        };
        assert!(with_app_now(|a| a.dirty).unwrap(), "typing marks unsaved");
        // Saved by itself 1.5 s after the last change.
        run_loop(1.8);
        let saved = stored(&id);
        assert!(saved.body.ends_with("PS: see you"), "{:?}", saved.body);
        assert!(saved.edits.edited && saved.edits.fields_changed.contains(&"body".to_string()));
        assert!(!with_app_now(|a| a.dirty).unwrap());
        // A checkpoint saves at once and redraws the fields as saved (what would be sent).
        with_app_now(|a| {
            editor::set_tokens(
                &a.editor.to,
                &["Ann <ann@example.org>, bob@example.org".to_string()],
            );
            a.mark_dirty();
            assert!(a.flush(true));
            assert_eq!(
                editor::tokens(&a.editor.to),
                vec!["Ann <ann@example.org>", "bob@example.org"]
            );
            assert!(!a.dirty);
        })
        .unwrap();
        assert_eq!(
            stored(&id).to,
            vec!["Ann <ann@example.org>", "bob@example.org"]
        );
    }

    fn silent_autosave_failure_keeps_text_dirty(mtm: MainThreadMarker) {
        let (_f, id) = one_draft(mtm, "tx2");
        with_app_now(|a| {
            editor::set_tokens(&a.editor.to, &["not an address".to_string()]);
            a.mark_dirty();
        })
        .unwrap();
        run_loop(1.8);
        with_app_now(|a| {
            assert!(a.dirty, "still unsaved");
            assert_eq!(a.editor.banner.text(), None, "an autosave fails silently");
            assert_eq!(editor::tokens(&a.editor.to), vec!["not an address"]);
        })
        .unwrap();
        assert_eq!(stored(&id).to, vec!["Ann <ann@example.org>"]);
    }

    fn loud_flush_failure_blocks_switch_and_offers_revert(mtm: MainThreadMarker) {
        let mut f = fixture("tx3", CFG);
        let a_id = f.add("A");
        let b_id = f.add("B");
        f.install(mtm);
        with_app_now(|a| a.select(Some(&a_id))).unwrap();
        with_app_now(|a| {
            editor::set_tokens(&a.editor.to, &["not an address".to_string()]);
            a.mark_dirty();
        })
        .unwrap();
        // The person clicks B in the sidebar.
        let table = with_app_now(|a| a.table.clone()).unwrap();
        let pick = |id: &str| {
            let row = sidebar::index_of(id).unwrap();
            table.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(row), false);
            run_loop(0.05);
        };
        pick(&b_id);
        with_app_now(|a| {
            assert_eq!(a.selected.as_deref(), Some(a_id.as_str()), "stays on A");
            let row = usize::try_from(a.table.selectedRow()).unwrap();
            assert_eq!(sidebar::row_at(row).unwrap().id, a_id);
            let text = a.editor.banner.text().unwrap();
            assert!(text.starts_with("Eine Empfängeradresse"), "{text}");
            assert_eq!(
                a.editor.banner.action.title().to_string(),
                "Änderungen verwerfen"
            );
        })
        .unwrap();
        click_hint();
        with_app_now(|a| {
            assert!(!a.dirty);
            assert_eq!(editor::tokens(&a.editor.to), vec!["Ann <ann@example.org>"]);
            assert_eq!(a.editor.banner.text(), None);
        })
        .unwrap();
        pick(&b_id);
        assert_eq!(
            with_app_now(|a| a.selected.clone()).unwrap(),
            Some(b_id.clone())
        );
    }

    fn invisible_characters_highlighted_and_removed_with_undo(mtm: MainThreadMarker) {
        let mut f = fixture("tx4", CFG);
        let id = f.add_body("Hi", "Hi\u{200B}there\n");
        f.install(mtm);
        with_app_now(|a| a.select(Some(&id))).unwrap();
        let body = with_app_now(|a| a.editor.body.clone()).unwrap();
        let mark = |index: usize| {
            // SAFETY: the text view's own layout manager; `null` = no effective range wanted.
            unsafe {
                body.layoutManager()
                    .unwrap()
                    .temporaryAttribute_atCharacterIndex_effectiveRange(
                        NSBackgroundColorAttributeName,
                        index,
                        std::ptr::null_mut(),
                    )
            }
            .is_some()
        };
        // The zero-width space at 2 is marked together with the "t" after it, so the mark shows.
        assert!(mark(2) && mark(3) && !mark(0) && !mark(5));
        let text = with_app_now(|a| a.editor.banner.text()).unwrap().unwrap();
        assert!(
            text.starts_with("Unsichtbare Zeichen im Text oder Betreff: 1"),
            "{text}"
        );
        click_hint();
        assert_eq!(body.string().to_string(), "Hithere\n");
        assert!(!mark(2));
        with_app_now(|a| {
            assert!(a.dirty);
            assert_eq!(a.editor.banner.text(), None);
        })
        .unwrap();
        // One undo step brings it back.
        body.undoManager().unwrap().undo();
        run_loop(0.05);
        assert_eq!(body.string().to_string(), "Hi\u{200B}there\n");
    }

    /// Typed like the keyboard does it: through the input method of `view` (a text view or a field editor).
    fn type_text(view: &NSTextView, text: &str) {
        // SAFETY: an NSString is what the input system hands over; `NSNotFound` = at the selection.
        unsafe {
            view.insertText_replacementRange(
                &NSString::from_str(text),
                NSRange::new(objc2_foundation::NSNotFound as usize, 0),
            )
        };
        run_loop(0.05);
    }

    fn typed_recipient_without_separator_counts(mtm: MainThreadMarker) {
        let (_f, id) = one_draft(mtm, "tk1");
        let (window, to) = with_app_now(|a| {
            a.show_window(false);
            (a.window.clone(), a.editor.to.clone())
        })
        .unwrap();
        // The person clicks into "To" and types a second address, without a comma or Return.
        assert!(window.makeFirstResponder(Some(to.as_ref())));
        let field_editor = to
            .currentEditor()
            .expect("a field editor while typing")
            .downcast::<NSTextView>()
            .expect("the field editor is a text view");
        field_editor.setSelectedRange(NSRange::new(field_editor.string().length(), 0));
        type_text(&field_editor, "bob@example.org");
        assert_eq!(
            editor::tokens(&to),
            vec!["Ann <ann@example.org>", "bob@example.org"]
        );
        with_app_now(|a| {
            assert!(a.dirty, "typing in a header field marks unsaved");
            assert!(a.flush(true));
        })
        .unwrap();
        assert_eq!(
            stored(&id).to,
            vec!["Ann <ann@example.org>", "bob@example.org"]
        );
        window.makeFirstResponder(None);
    }

    fn strip_keeps_the_caret_on_a_character(mtm: MainThreadMarker) {
        let mut f = fixture("tx6", CFG);
        let id = f.add_body("Hi", "\u{200B}\u{1F600}\n");
        f.install(mtm);
        with_app_now(|a| a.select(Some(&id))).unwrap();
        let body = with_app_now(|a| a.editor.body.clone()).unwrap();
        // The caret right after the invisible character, in front of the emoji (two UTF-16 units).
        body.setSelectedRange(NSRange::new(1, 0));
        click_hint();
        let units: Vec<u16> = body.string().to_string().encode_utf16().collect();
        assert_eq!(units, "\u{1F600}\n".encode_utf16().collect::<Vec<_>>());
        let at = body.selectedRange().location;
        assert!(at == 0 || at == 2, "the caret sits inside the emoji: {at}");
        type_text(&body, "x");
        let text = body.string().to_string();
        assert!(text == "x\u{1F600}\n" || text == "\u{1F600}x\n", "{text:?}");
    }

    fn substitutions_are_off(mtm: MainThreadMarker) {
        let (_f, _id) = one_draft(mtm, "tx5");
        let b = with_app_now(|a| a.editor.body.clone()).unwrap();
        assert!(!b.isRichText() && !b.importsGraphics());
        assert!(b.allowsUndo() && b.usesFindBar() && b.isContinuousSpellCheckingEnabled());
        assert!(!b.isAutomaticQuoteSubstitutionEnabled());
        assert!(!b.isAutomaticDashSubstitutionEnabled());
        assert!(!b.isAutomaticTextReplacementEnabled());
        assert!(!b.isAutomaticSpellingCorrectionEnabled());
        assert!(!b.isAutomaticLinkDetectionEnabled());
        assert!(!b.isAutomaticDataDetectionEnabled());
    }

    fn text_view_takes_no_file_drops(mtm: MainThreadMarker) {
        let (_f, _id) = one_draft(mtm, "tx6");
        let types = || -> Vec<String> {
            with_app_now(|a| a.editor.body.registeredDraggedTypes())
                .unwrap()
                .iter()
                .map(|t| t.to_string())
                .collect()
        };
        assert_eq!(types(), vec!["public.utf8-plain-text"]);
        // AppKit registers its own types again whenever the editability changes.
        with_app_now(|a| {
            a.editor.set_editable(false);
            a.editor.set_editable(true);
        })
        .unwrap();
        assert_eq!(types(), vec!["public.utf8-plain-text"]);
    }

    // ---------- Preview (a switch above the text) ----------

    fn preview_config_has_no_javascript(mtm: MainThreadMarker) {
        install(App::new(mtm));
        let view = with_app_now(|a| a.editor.preview.clone()).unwrap();
        // SAFETY: plain getters of the app's own web view, main thread.
        unsafe {
            let config = view.configuration();
            assert!(!config.defaultWebpagePreferences().allowsContentJavaScript());
            assert!(!config.preferences().javaScriptCanOpenWindowsAutomatically());
            assert!(!config.websiteDataStore().isPersistent());
            assert!(view.navigationDelegate().is_some());
        }
        let types: Vec<String> = view
            .registeredDraggedTypes()
            .iter()
            .map(|t| t.to_string())
            .collect();
        assert_eq!(types, vec!["public.file-url"], "files only");
    }

    /// Found on the Mac (probe 07.11.): WebKit swallowed a file dropped on the shown preview. The preview answers every
    /// step of a drop itself now (the drop itself is only testable on the Mac: checked there after the fix).
    fn preview_answers_drops_itself(mtm: MainThreadMarker) {
        let (_f, _id) = one_draft(mtm, "pv3");
        let view = with_app_now(|a| {
            a.show_window(false);
            a.editor.set_preview(true);
            a.editor.preview.clone()
        })
        .unwrap();
        run_loop(1.5);
        use objc2::ClassType;
        use objc2_foundation::NSObjectProtocol;
        assert!(view.isKindOfClass(draftdesk_macos::preview::Preview::class()));
        // Inside the web view nothing takes drops in its place.
        let mut views = Vec::new();
        all_views(&view, &mut views);
        assert!(
            views
                .iter()
                .all(|v| v.registeredDraggedTypes().count() == 0),
            "a view inside the preview takes drops"
        );
    }

    /// Found on the Mac (probe 07.11.): with no draft selected, the toolbar looked active.
    fn nothing_selected_offers_no_actions(mtm: MainThreadMarker) {
        let mut f = fixture("ns1", CFG);
        f.add("A");
        f.install(mtm);
        with_app_now(|a| {
            assert!(a.selected.is_none());
            for id in [
                toolbar::CHECK,
                toolbar::FIX,
                toolbar::DISCARD,
                toolbar::SEND,
            ] {
                assert_eq!(toolbar::enabled(&a.window, id), Some(false), "{id}");
            }
        })
        .unwrap();
    }

    fn toggle_switches_between_text_and_preview(mtm: MainThreadMarker) {
        let (_f, _id) = one_draft(mtm, "pv1");
        let (delegate, toggle) =
            with_app_now(|a| (a.delegate.clone(), a.editor.toggle.clone())).unwrap();
        let text_shown = || {
            with_app_now(|a| {
                let e = &a.editor;
                assert_ne!(
                    e.body_scroll.isHidden(),
                    e.preview.isHidden(),
                    "one of both"
                );
                !e.body_scroll.isHidden()
            })
            .unwrap()
        };
        assert!(text_shown());
        // The menu (Shift+Cmd+V).
        with_app_now(|a| a.on_action(Action::TogglePreview)).unwrap();
        assert!(!text_shown());
        assert_eq!(toggle.selectedSegment(), 1);
        // A click on "Text" in the switch.
        toggle.setSelectedSegment(0);
        // SAFETY: the action the switch sends, with the switch as sender.
        let () = unsafe { msg_send![&*delegate, previewToggled: &*toggle] };
        assert!(text_shown());
    }

    // ---------- Attachments ----------

    /// Files on "the person's disk", inside the harness folder (removed at the end).
    fn disk(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("draftdesk-appkit-{}", std::process::id()))
            .join(format!("disk-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        for (n, body) in files {
            std::fs::write(dir.join(n), body).unwrap();
        }
        dir
    }

    fn names(id: &str) -> Vec<String> {
        stored(id)
            .attachments
            .iter()
            .map(|a| a.name.clone())
            .collect()
    }

    fn dropped_paths_become_attachments(mtm: MainThreadMarker) {
        let (_f, id) = one_draft(mtm, "at1");
        let dir = disk(
            "at1",
            &[
                ("offer.pdf", "%PDF-1.4 x"),
                ("notes.txt", "hello"),
                ("run.command", "#!/bin/sh\n"),
            ],
        );
        std::os::unix::fs::symlink(dir.join("notes.txt"), dir.join("link.txt")).unwrap();
        let paths: Vec<PathBuf> = ["offer.pdf", "notes.txt", "link.txt", "run.command"]
            .iter()
            .map(|n| dir.join(n))
            .collect();
        with_app_now(|a| {
            assert!(a.accepts_drop());
            a.add_files(paths);
            let titles = a.editor.strip.titles();
            assert_eq!(titles.len(), 3, "{titles:?}");
            assert!(titles[0].starts_with("offer.pdf ("), "{titles:?}");
            let hint = a.editor.banner.text().unwrap();
            assert!(hint.starts_with("Nicht angehängt: link.txt ("), "{hint}");
        })
        .unwrap();
        assert_eq!(names(&id), vec!["offer.pdf", "notes.txt", "run.command"]);
        assert!(
            stored(&id)
                .edits
                .fields_changed
                .contains(&"attachments".to_string())
        );
        // Never handed to another program: a .command file would run.
        with_app_now(|a| {
            a.open_attachment(2);
            assert_eq!(
                a.editor.banner.text().unwrap(),
                "Keine Vorschau für .command"
            );
        })
        .unwrap();
    }

    fn remove_attachment_updates_row(mtm: MainThreadMarker) {
        let (f, id) = one_draft(mtm, "at2");
        let dir = disk("at2", &[("a.txt", "a"), ("b.txt", "b")]);
        with_app_now(|a| a.add_files(vec![dir.join("a.txt"), dir.join("b.txt")])).unwrap();
        let remove = with_app_now(|a| a.editor.strip.remove_button(0))
            .unwrap()
            .expect("a remove button next to a.txt");
        // SAFETY: a button of the app's own window, main thread.
        unsafe { remove.performClick(None) };
        run_loop(0.05);
        assert_eq!(names(&id), vec!["b.txt"]);
        let titles = with_app_now(|a| a.editor.strip.titles()).unwrap();
        assert!(
            titles.len() == 1 && titles[0].starts_with("b.txt ("),
            "{titles:?}"
        );
        let att = f.root.join("state/queue").join(&id).join("att");
        assert!(!att.join("a.txt").exists() && att.join("b.txt").exists());
    }

    fn drop_refused_when_not_editable(mtm: MainThreadMarker) {
        let (_f, id) = one_draft(mtm, "at3");
        with_app_now(|a| {
            let store = a.desk.as_ref().unwrap().sender();
            store
                .store()
                .mutate(&id, &[], |i| {
                    i.state = draftdesk_core::item::State::Failed;
                    Ok(())
                })
                .unwrap();
        })
        .unwrap();
        refresh();
        let dir = disk("at3", &[("a.txt", "a")]);
        with_app_now(|a| {
            assert!(!a.accepts_drop());
            a.add_files(vec![dir.join("a.txt")]);
            assert!(a.editor.strip.titles().is_empty());
        })
        .unwrap();
        assert!(names(&id).is_empty());
    }

    /// The app's part of Quick Look: the panel opens with the clicked file, the delegate takes control and lists it.
    /// That macOS finds the delegate as controller needs the window as key window of the active app, which a test
    /// does not take from the person; checked on the Mac.
    fn quick_look_takes_the_app_as_controller(mtm: MainThreadMarker) {
        let (_f, id) = one_draft(mtm, "at4");
        let dir = disk("at4", &[("notes.txt", "hello")]);
        with_app_now(|a| {
            a.add_files(vec![dir.join("notes.txt")]);
            a.open_attachment(0);
        })
        .unwrap();
        run_loop(0.2);
        // SAFETY: the shared panel on the main thread.
        let panel = unsafe { QLPreviewPanel::sharedPreviewPanel(mtm) }.unwrap();
        let visible = panel.isVisible();
        let delegate = with_app_now(|a| a.delegate.clone()).unwrap();
        let me: &AnyObject = delegate.as_ref();
        // SAFETY: the calls Quick Look makes on its controller, main thread.
        let (accepts, source, count, after): (bool, bool, NSInteger, bool) = unsafe {
            let accepts: bool = msg_send![me, acceptsPreviewPanelControl: &*panel];
            let () = msg_send![me, beginPreviewPanelControl: &*panel];
            let source = panel
                .dataSource()
                .is_some_and(|s| std::ptr::eq(&*s as *const _ as *const AnyObject, me));
            let count: NSInteger = msg_send![me, numberOfPreviewItemsInPreviewPanel: &*panel];
            let () = msg_send![me, endPreviewPanelControl: &*panel];
            (accepts, source, count, panel.dataSource().is_none())
        };
        panel.orderOut(None);
        run_loop(0.1);
        assert!(visible && accepts && source && after);
        assert_eq!(count, 1);
        let shown = draftdesk_macos::attachments::preview_item(0)
            .and_then(|u| u.path())
            .unwrap()
            .to_string();
        assert!(shown.ends_with(&format!("{id}/att/notes.txt")), "{shown}");
    }

    // ---------- Thread, findings, check and fix ----------

    fn fixture_bin(name: &str) -> String {
        format!(
            "{}/../core/tests/fixtures/bin/{name}",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    /// The test account plus a check command (one finding "subject Re: Offer") and a fix command (a new text for
    /// exactly that finding).
    fn cfg_with_commands() -> String {
        format!(
            r#"{{"accounts":[{{"name":"work","kind":"command",
                "senders":[{{"address":"me@example.org","name":"Me"}}],"send_command":["/bin/cat"]}}],
              "check_command":{{"argv":["{}"],"timeout_s":10}},
              "fix_command":{{"argv":["{}"],"timeout_s":10}}}}"#,
            fixture_bin("check-warn.sh"),
            fixture_bin("fix.sh")
        )
    }

    /// Runs the run loop until `done` holds, at most `seconds`.
    fn wait_for(seconds: f64, done: impl Fn() -> bool) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed().as_secs_f64() < seconds {
            if done() {
                return true;
            }
            run_loop(0.05);
        }
        done()
    }

    fn findings_render_levels_and_stale(mtm: MainThreadMarker) {
        let mut f = fixture("fd1", CFG);
        let body = "Hello Ann,\n\nsee you on Friday.\n";
        let checks = serde_json::json!({
            "body_sha256": draftdesk_core::sha256_hex(body.as_bytes()), "verdict": "block",
            "findings": [
                {"level": "info", "title": "Tone"},
                {"level": "warn", "title": "Date", "detail": "Friday?", "quote": "on Friday", "confirm": true},
                {"level": "block", "title": "Wrong name"}
            ]
        });
        let extra = serde_json::json!({"checks": checks, "thread": "Ann wrote:\n> Hi"});
        let id = f.add_draft(draft("Re: Offer", body, extra));
        f.install(mtm);
        with_app_now(|a| a.select(Some(&id))).unwrap();
        with_app_now(|a| {
            let t = a.editor.findings.texts();
            assert_eq!(t.len(), 4, "{t:?}");
            assert!(
                t[0].starts_with("Prüfung: blockiert") && t[0].ends_with("vom Agenten"),
                "{t:?}"
            );
            assert!(t[1].starts_with("Hinweis: Tone"), "{t:?}");
            assert_eq!(
                t[2],
                "Warnung: Date (vor dem Senden bestätigen) Friday? on Friday"
            );
            assert!(t[3].starts_with("Blockiert: Wrong name"), "{t:?}");
            // The thread: folded until asked for.
            let th = &a.editor.thread;
            assert!(!th.row.isHidden() && th.scroll.isHidden());
            a.editor.toggle_thread();
            assert!(!a.editor.thread.scroll.isHidden());
            assert_eq!(
                a.editor.thread.text.string().to_string(),
                "Ann wrote:\n> Hi"
            );
            // Another text makes the findings stale.
            a.editor
                .body
                .setString(&NSString::from_str("Hello Ann,\n\nsee you on Monday.\n"));
            a.mark_dirty();
            assert!(a.flush(true));
            assert!(a.editor.findings.texts()[0].ends_with("veraltet"));
        })
        .unwrap();
    }

    /// A user test: five findings on one sentence read as five times the same; they share one row now.
    fn findings_on_one_sentence_share_a_row(mtm: MainThreadMarker) {
        let mut f = fixture("fd2", CFG);
        let body =
            "Hallo Ann,\n\nWie besprochen schicke ich dir morgen die Unterlagen. Viele Grüsse\n";
        let checks = serde_json::json!({
            "body_sha256": draftdesk_core::sha256_hex(body.as_bytes()), "verdict": "block",
            "findings": [
                {"level": "warn", "title": "ABSICHT", "detail": "zusage: schicke ich dir morgen die Unterlagen",
                 "quote": "schicke ich dir morgen die Unterlagen", "confirm": true},
                {"level": "block", "title": "WIDERSPRUCH", "detail": "steht gegen die Vorgabe",
                 "quote": "Wie besprochen schicke ich dir morgen die Unterlagen."},
                {"level": "warn", "title": "UNBELEGT", "detail": "Wie besprochen", "quote": "Wie besprochen"},
                {"level": "block", "title": "ANRED-1", "detail": "keine Anrede"}
            ]
        });
        let id = f.add_draft(draft(
            "Re: Offer",
            body,
            serde_json::json!({"checks": checks}),
        ));
        f.install(mtm);
        with_app_now(|a| a.select(Some(&id))).unwrap();
        with_app_now(|a| {
            let t = a.editor.findings.texts();
            assert_eq!(t.len(), 3, "{t:?}");
            assert!(
                t[1].starts_with(
                    "Blockiert: „Wie besprochen schicke ich dir morgen die Unterlagen.“ \
                     ABSICHT (vor dem Senden bestätigen) · WIDERSPRUCH · UNBELEGT ("
                ),
                "{t:?}"
            );
            assert!(
                t[1].contains("WIDERSPRUCH: steht gegen die Vorgabe"),
                "{t:?}"
            );
            assert_eq!(t[2], "Blockiert: ANRED-1 keine Anrede");
            // Findings of an earlier text: one row each, their quotes are not bound to the new text (which still
            // holds them all, so bundling would join them again).
            a.editor.body.setString(&NSString::from_str(
                "Hallo Ann,\n\nWie besprochen schicke ich dir morgen die Unterlagen. Danke!\n",
            ));
            a.mark_dirty();
            assert!(a.flush(true));
            let t = a.editor.findings.texts();
            assert!(t[0].ends_with("veraltet"), "{t:?}");
            assert_eq!(t.len(), 5, "{t:?}");
        })
        .unwrap();
    }

    fn check_result_appears_and_unlocks(mtm: MainThreadMarker) {
        let mut f = fixture("ck1", &cfg_with_commands());
        let id = f.add("Re: Offer");
        f.install(mtm);
        with_app_now(|a| {
            a.select(Some(&id));
            assert!(a.current_actions().check && !a.current_actions().fix);
            a.on_action(Action::Check);
            assert_eq!(a.editor.findings.texts()[0], "Prüfung läuft …");
            assert!(!a.editor.subject.isEditable(), "locked while checking");
            assert!(!a.current_actions().send, "no send during a check");
        })
        .unwrap();
        let done = wait_for(5.0, || {
            with_app_now(|a| a.editor.findings.texts()[0].starts_with("Prüfung: Hinweise"))
                .unwrap_or(false)
        });
        assert!(done, "the check result arrives");
        with_app_now(|a| {
            let t = a.editor.findings.texts();
            assert!(t[0].ends_with("Prüfbefehl"), "{t:?}");
            assert!(t[1].contains("subject Re: Offer"), "{t:?}");
            assert!(a.editor.subject.isEditable(), "unlocked");
            assert!(a.current_actions().send && a.current_actions().fix);
        })
        .unwrap();
        assert_eq!(
            stored(&id).checks.unwrap().source,
            Some(draftdesk_core::item::CheckSource::Command)
        );
    }

    /// A draft with findings of the check command, ready for a fix.
    fn checked_draft(mtm: MainThreadMarker, name: &str) -> (Fixture, String) {
        let mut f = fixture(name, &cfg_with_commands());
        let id = f.add("Re: Offer");
        f.install(mtm);
        with_app_now(|a| {
            a.select(Some(&id));
            a.on_action(Action::Check);
        })
        .unwrap();
        let ready = wait_for(5.0, || {
            with_app_now(|a| a.current_actions().fix).unwrap_or(false)
        });
        assert!(ready, "checked");
        (f, id)
    }

    fn fix_replaces_text_once_and_undo_restores(mtm: MainThreadMarker) {
        let (_f, id) = checked_draft(mtm, "fx1");
        let body = with_app_now(|a| {
            a.on_action(Action::Fix);
            a.editor.body.clone()
        })
        .unwrap();
        let fixed = "Fixed body for: subject Re: Offer";
        assert!(wait_for(5.0, || body.string().to_string() == fixed));
        with_app_now(|a| {
            assert!(a.dirty, "saved like typing");
            let hint = a.editor.banner.text().unwrap();
            assert!(hint.starts_with("Korrektur eingesetzt"), "{hint}");
        })
        .unwrap();
        assert_eq!(
            stored(&id).body,
            "Hello Ann,\n\nthanks.\n",
            "the fix saves nothing itself"
        );
        // One undo step takes it back.
        body.undoManager().unwrap().undo();
        run_loop(0.05);
        assert_eq!(body.string().to_string(), "Hello Ann,\n\nthanks.\n");
    }

    /// A user test: a fix that came back while another draft was shown vanished without a word.
    fn fix_for_a_draft_no_longer_shown_is_reported(mtm: MainThreadMarker) {
        let mut f = fixture("fx3", &cfg_with_commands());
        let id = f.add("Re: Offer");
        let other = f.add("Other");
        f.install(mtm);
        with_app_now(|a| {
            a.select(Some(&id));
            a.on_action(Action::Check);
        })
        .unwrap();
        let checked = wait_for(5.0, || {
            with_app_now(|a| a.current_actions().fix).unwrap_or(false)
        });
        assert!(checked, "checked");
        with_app_now(|a| {
            a.on_action(Action::Fix);
            // Before the result arrives (it waits for the main thread), another draft is shown.
            a.select(Some(&other));
        })
        .unwrap();
        let told = wait_for(5.0, || {
            with_app_now(|a| {
                a.editor
                    .banner
                    .text()
                    .is_some_and(|t| t.starts_with("Korrektur nicht übernommen"))
            })
            .unwrap_or(false)
        });
        assert!(told, "the dropped fix is reported");
        with_app_now(|a| assert_eq!(a.shown.as_ref().unwrap().item_id, other)).unwrap();
    }

    fn fix_discarded_when_text_changed_meanwhile(mtm: MainThreadMarker) {
        let (_f, _id) = checked_draft(mtm, "fx2");
        let body = with_app_now(|a| {
            a.on_action(Action::Fix);
            // Before the result arrives (it waits for the main thread), the text changes.
            a.editor
                .body
                .setString(&NSString::from_str("Changed meanwhile.\n"));
            a.editor.body.clone()
        })
        .unwrap();
        let dropped = wait_for(5.0, || {
            with_app_now(|a| {
                a.editor
                    .banner
                    .text()
                    .is_some_and(|t| t.starts_with("Korrektur verworfen"))
            })
            .unwrap_or(false)
        });
        assert!(dropped, "the fix is dropped with a hint");
        assert_eq!(body.string().to_string(), "Changed meanwhile.\n");
    }

    // ---------- Send with undo, discard, close and quit ----------

    /// The test account sending through `send-record.sh`, which writes what it got to `record`.
    fn cfg_send(record: &std::path::Path) -> String {
        format!(
            r#"{{"accounts":[{{"name":"work","kind":"command",
                "senders":[{{"address":"me@example.org","name":"Me"}}],
                "send_command":["{}","{}"]}}]}}"#,
            fixture_bin("send-record.sh"),
            record.display()
        )
    }

    /// One draft on an account that records its sends; selected, the window shown (sheets need it).
    fn send_draft(
        mtm: MainThreadMarker,
        name: &str,
        extra: serde_json::Value,
    ) -> (Fixture, String, PathBuf) {
        let record = disk(name, &[]).join("sent.eml");
        let mut f = fixture(name, &cfg_send(&record));
        let id = f.add_draft(draft("Re: Offer", "Hello Ann,\n\nthanks.\n", extra));
        f.install(mtm);
        with_app_now(|a| {
            a.select(Some(&id));
            a.show_window(false);
        })
        .unwrap();
        (f, id, record)
    }

    /// The sheet on the window and its buttons as (title, key equivalent).
    fn sheet() -> (Retained<NSWindow>, Retained<NSAlert>, Vec<(String, String)>) {
        with_app_now(|a| {
            let alert = a.alert.clone().expect("a sheet");
            let buttons = alert
                .buttons()
                .iter()
                .map(|b| (b.title().to_string(), b.keyEquivalent().to_string()))
                .collect();
            (a.window.clone(), alert, buttons)
        })
        .unwrap()
    }

    fn answer(window: &NSWindow, alert: &NSAlert, response: NSModalResponse) {
        window.endSheet_returnCode(&alert.window(), response);
        run_loop(0.1);
    }

    fn send_waits_for_undo_then_sends(mtm: MainThreadMarker) {
        let (_f, id, record) = send_draft(mtm, "sd1", serde_json::json!({}));
        with_app_now(|a| {
            a.on_action(Action::Send);
            assert_eq!(a.pending_ids(), vec![id.clone()]);
            assert!(
                !a.editor.subject.isEditable(),
                "locked during the countdown"
            );
            let bar = a.editor.undo.text().unwrap();
            assert!(bar.starts_with("„Re: Offer“ wird in 1"), "{bar}");
            assert!(!a.current_actions().send && !a.current_actions().discard);
        })
        .unwrap();
        run_loop(5.0);
        assert!(!record.exists(), "nothing goes out during the 15 s");
        assert_eq!(stored(&id).state, State::SendingScheduled);
        let sent = wait_for(14.0, || record.exists() && stored(&id).state == State::Sent);
        assert!(sent, "sent after the countdown");
        let told = wait_for(2.0, || {
            with_app_now(|a| {
                a.editor
                    .banner
                    .text()
                    .is_some_and(|t| t == "„Re: Offer“: Gesendet an 1 Empfänger.")
            })
            .unwrap_or(false)
        });
        assert!(told, "the outcome is shown");
        with_app_now(|a| {
            assert!(a.pending_ids().is_empty());
            assert_eq!(a.editor.undo.text(), None);
        })
        .unwrap();
        assert!(
            std::fs::read_to_string(&record)
                .unwrap()
                .contains("ann@example.org")
        );
    }

    fn undo_cancels_and_unlocks_editor(mtm: MainThreadMarker) {
        let (_f, id, record) = send_draft(mtm, "ud1", serde_json::json!({}));
        let button = with_app_now(|a| {
            a.on_action(Action::Send);
            assert!(!a.editor.undo.view.isHidden());
            assert_eq!(a.editor.undo.button.keyEquivalent().to_string(), "\u{1b}");
            a.editor.undo.button.clone()
        })
        .unwrap();
        // SAFETY: a button of the app's own window, main thread.
        unsafe { button.performClick(None) };
        run_loop(0.05);
        with_app_now(|a| {
            assert!(a.pending_ids().is_empty());
            assert!(a.editor.undo.view.isHidden());
            assert!(a.editor.subject.isEditable(), "editable again");
            assert_eq!(
                a.editor.undo.button.keyEquivalent().to_string(),
                "",
                "Escape undoes only during the countdown"
            );
        })
        .unwrap();
        assert_eq!(stored(&id).state, State::Ready);
        assert!(!record.exists());
    }

    fn confirmation_dialog_cancel_is_escape_and_send_has_no_key(mtm: MainThreadMarker) {
        let body = "Hello Ann,\n\nthanks.\n";
        let checks = serde_json::json!({
            "body_sha256": draftdesk_core::sha256_hex(body.as_bytes()), "verdict": "block",
            "findings": [{"level": "block", "title": "Wrong name"}]
        });
        let (_f, id, _record) = send_draft(mtm, "cf1", serde_json::json!({"checks": checks}));
        with_app_now(|a| a.on_action(Action::Send)).unwrap();
        let (window, alert, buttons) = sheet();
        assert_eq!(
            buttons,
            vec![
                ("Abbrechen".to_string(), "\u{1b}".to_string()),
                ("Trotzdem senden".to_string(), String::new())
            ],
            "Escape cancels, Enter does nothing, sending only by a click"
        );
        assert!(alert.informativeText().to_string().contains("Wrong name"));
        answer(&window, &alert, NSAlertFirstButtonReturn);
        with_app_now(|a| assert!(a.pending_ids().is_empty() && a.alert.is_none())).unwrap();
        // Once more, now "Send anyway".
        with_app_now(|a| a.on_action(Action::Send)).unwrap();
        let (window, alert, _) = sheet();
        answer(&window, &alert, NSAlertSecondButtonReturn);
        assert_eq!(with_app_now(|a| a.pending_ids()).unwrap(), vec![id.clone()]);
        with_app_now(|a| a.undo_send()).unwrap();
        assert_eq!(stored(&id).state, State::Ready);
    }

    fn close_refused_during_countdown(mtm: MainThreadMarker) {
        let (_f, _id, _record) = send_draft(mtm, "cl1", serde_json::json!({}));
        let window = with_app_now(|a| {
            a.on_action(Action::Send);
            a.window.clone()
        })
        .unwrap();
        window.performClose(None);
        assert!(window.isVisible(), "stays open while a send is scheduled");
        let hint = with_app_now(|a| a.editor.banner.text()).unwrap().unwrap();
        assert!(hint.starts_with("Während"), "{hint}");
        with_app_now(|a| a.undo_send()).unwrap();
    }

    fn quit_cancels_countdown_without_sending(mtm: MainThreadMarker) {
        let (_f, id, record) = send_draft(mtm, "qt1", serde_json::json!({}));
        let reply = with_app_now(|a| {
            a.on_action(Action::Send);
            a.should_terminate()
        })
        .unwrap();
        assert!(reply == NSApplicationTerminateReply::TerminateNow);
        with_app_now(|a| assert!(a.pending_ids().is_empty())).unwrap();
        assert_eq!(stored(&id).state, State::Ready, "cancelled, nothing sent");
        assert!(!record.exists());
    }

    /// Answers the next app-modal alert (`runModal`) with `response` once it is up, from the main queue (it runs
    /// during the alert). Stopped from there, the alert ends only with the next event: hence an empty one.
    fn answer_modal(response: NSModalResponse) {
        fn attempt(response: NSModalResponse, until: std::time::Instant) {
            draftdesk_macos::on_main(move || {
                let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
                let app = NSApplication::sharedApplication(mtm);
                if app.modalWindow().is_none() {
                    if std::time::Instant::now() < until {
                        attempt(response, until);
                    }
                    return;
                }
                app.stopModalWithCode(response);
                let wake = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                    NSEventType::ApplicationDefined,
                    NSPoint::ZERO,
                    NSEventModifierFlags::empty(),
                    0.0,
                    0,
                    None,
                    0,
                    0,
                    0,
                )
                .expect("an empty event");
                app.postEvent_atStart(&wake, true);
            });
        }
        attempt(
            response,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        );
    }

    /// Cancel in the question about unsaved changes leaves everything as it was, the countdown included; Quit there
    /// cancels it, nothing is sent.
    fn cancel_in_the_quit_question_keeps_the_countdown(mtm: MainThreadMarker) {
        let record = disk("qt2", &[]).join("sent.eml");
        let mut f = fixture("qt2", &cfg_send(&record));
        let scheduled = f.add("Re: Offer");
        let edited = f.add("Invoice");
        f.install(mtm);
        with_app_now(|a| {
            a.select(Some(&scheduled));
            a.show_window(false);
            a.on_action(Action::Send);
            // Another draft, with a change that cannot be saved.
            a.select(Some(&edited));
            editor::set_tokens(&a.editor.to, &["not an address".to_string()]);
            a.mark_dirty();
        })
        .unwrap();
        let quit = |response| {
            answer_modal(response);
            with_app_now(|a| a.should_terminate()).unwrap()
        };
        assert!(quit(NSAlertSecondButtonReturn) == NSApplicationTerminateReply::TerminateCancel);
        assert_eq!(
            with_app_now(|a| a.pending_ids()).unwrap(),
            vec![scheduled.clone()],
            "the countdown still runs"
        );
        assert_eq!(stored(&scheduled).state, State::SendingScheduled);
        assert!(quit(NSAlertFirstButtonReturn) == NSApplicationTerminateReply::TerminateNow);
        with_app_now(|a| assert!(a.pending_ids().is_empty())).unwrap();
        assert_eq!(
            stored(&scheduled).state,
            State::Ready,
            "cancelled, nothing sent"
        );
        assert!(!record.exists());
    }

    /// A countdown that quitting can no longer cancel has fired (here its send already went out, maybe while the
    /// question about unsaved changes was open): quitting waits until its outcome is shown.
    fn quit_waits_for_a_countdown_it_could_not_cancel(mtm: MainThreadMarker) {
        let (_f, id, _record) = send_draft(mtm, "qt3", serde_json::json!({}));
        let reply = with_app_now(|a| {
            a.on_action(Action::Send);
            a.desk
                .as_ref()
                .unwrap()
                .sender()
                .store()
                .mutate(&id, &[], |i| {
                    i.state = State::Sent;
                    Ok(())
                })
                .unwrap();
            a.should_terminate()
        })
        .unwrap();
        assert!(reply == NSApplicationTerminateReply::TerminateCancel);
        let hint = with_app_now(|a| a.editor.banner.text()).unwrap().unwrap();
        assert!(hint.starts_with("Ein Versand läuft gerade"), "{hint}");
        assert_eq!(
            with_app_now(|a| a.pending_ids()).unwrap(),
            vec![id.clone()],
            "its outcome is still due"
        );
    }

    fn discard_after_confirmation(mtm: MainThreadMarker) {
        let (_f, id, _record) = send_draft(mtm, "dc1", serde_json::json!({}));
        // As the app's start does: with the signing key a discard publishes its result and leaves the queue.
        with_app_now(|a| a.desk.as_ref().unwrap().boot()).unwrap();
        with_app_now(|a| a.on_action(Action::Discard)).unwrap();
        let (window, alert, buttons) = sheet();
        assert_eq!(
            buttons,
            vec![
                ("Abbrechen".to_string(), "\u{1b}".to_string()),
                ("Verwerfen".to_string(), String::new())
            ]
        );
        answer(&window, &alert, NSAlertFirstButtonReturn);
        assert_eq!(stored(&id).state, State::Ready);
        // Typing not saved yet when "Discard" is clicked goes with the draft.
        with_app_now(|a| {
            a.editor
                .body
                .setString(&NSString::from_str("Hello Ann, still typing"));
            a.mark_dirty();
            a.on_action(Action::Discard);
        })
        .unwrap();
        let (window, alert, _) = sheet();
        answer(&window, &alert, NSAlertSecondButtonReturn);
        assert!(wait_for(5.0, || stored(&id).state == State::Discarded));
        // Gone from the list right away, without a hint about lost typing.
        assert!(wait_for(5.0, || {
            with_app_now(|a| a.items.iter().all(|r| r.id() != id)).unwrap()
        }));
        with_app_now(|a| assert_eq!(a.editor.banner.text(), None)).unwrap();
    }

    fn invalid_item_goes_to_archive(mtm: MainThreadMarker) {
        let mut f = fixture("iv1", CFG);
        let bad = "20261107-130000-deadbeef";
        let dir = f.root.join("state/queue").join(bad);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("item.json"), "{broken").unwrap();
        f.install(mtm);
        with_app_now(|a| {
            a.select(Some(bad));
            assert!(a.current_actions().archive && !a.current_actions().send);
            a.on_action(Action::Discard);
        })
        .unwrap();
        run_loop(0.05);
        assert!(!dir.exists(), "moved to the archive, nothing sent");
    }

    // ---------- Poll, new drafts, notifications ----------

    /// A draft handed over while the app runs.
    fn add_live(d: Draft, replaces: Option<&str>) -> String {
        with_app_now(|a| {
            let store = a.desk.as_ref().unwrap().sender();
            add(store.store(), d, replaces, chrono::Local::now())
                .unwrap()
                .item_id
        })
        .unwrap()
    }

    fn new_item_is_selected_and_window_stays(mtm: MainThreadMarker) {
        let mut f = fixture("nw1", CFG);
        f.install(mtm);
        with_app_now(|a| {
            a.notifications_work = || true;
            assert!(a.selected.is_none());
        })
        .unwrap();
        let id = add_live(draft("Invoice", "Hello\n", serde_json::json!({})), None);
        refresh();
        with_app_now(|a| {
            assert_eq!(
                a.selected.as_deref(),
                Some(id.as_str()),
                "the new draft is selected"
            );
            assert!(!a.window.isVisible(), "the window stays where it is");
        })
        .unwrap();
        // A second one does not take the selection away.
        let other = add_live(draft("Offer", "Hello\n", serde_json::json!({})), None);
        refresh();
        with_app_now(|a| {
            assert_eq!(a.selected.as_deref(), Some(id.as_str()));
            assert!(sidebar::index_of(&other).is_some());
        })
        .unwrap();
    }

    fn fallback_without_notifications_orders_front(mtm: MainThreadMarker) {
        let mut f = fixture("nw2", CFG);
        f.install(mtm);
        with_app_now(|a| a.notifications_work = || false).unwrap();
        add_live(draft("Invoice", "Hello\n", serde_json::json!({})), None);
        refresh();
        let shown = with_app_now(|a| a.window.isVisible()).unwrap();
        let active = NSApplication::sharedApplication(mtm).isActive();
        assert!(shown, "without notifications the window comes to the front");
        assert!(!active, "without taking the keyboard");
    }

    fn poll_merges_checks_without_touching_text(mtm: MainThreadMarker) {
        let (_f, id) = one_draft(mtm, "pl1");
        with_app_now(|a| {
            a.editor
                .body
                .setString(&NSString::from_str("Hello Ann, still typing"));
            a.mark_dirty();
        })
        .unwrap();
        // Findings arrive from outside (the agent's check command); the content stays the same.
        let checks: draftdesk_core::item::Checks = serde_json::from_value(serde_json::json!({
            "body_sha256": stored(&id).body_sha256, "verdict": "warn",
            "findings": [{"level": "warn", "title": "Date"}]
        }))
        .unwrap();
        with_app_now(|a| {
            let store = a.desk.as_ref().unwrap().sender();
            draftdesk_core::edit::record_checks(store.store(), &id, checks).unwrap();
        })
        .unwrap();
        refresh();
        with_app_now(|a| {
            assert_eq!(
                a.editor.body.string().to_string(),
                "Hello Ann, still typing",
                "the typing stays"
            );
            assert!(a.dirty);
            let t = a.editor.findings.texts();
            assert!(t[0].starts_with("Prüfung: Hinweise"), "{t:?}");
        })
        .unwrap();
    }

    fn superseded_selection_moves_to_new_version(mtm: MainThreadMarker) {
        let mut f = fixture("sp1", CFG);
        let old = f.add("Offer v1");
        f.install(mtm);
        with_app_now(|a| a.select(Some(&old))).unwrap();
        // The agent of the same session hands over a new version.
        let new = add_live(
            draft(
                "Offer v2",
                "Hello Ann,\n\nnew prices.\n",
                serde_json::json!({}),
            ),
            Some(&old),
        );
        refresh();
        with_app_now(|a| {
            assert_eq!(a.selected.as_deref(), Some(new.as_str()));
            assert_eq!(a.editor.subject.stringValue().to_string(), "Offer v2");
            let hint = a.editor.banner.text().unwrap();
            assert!(hint.starts_with("Der Agent hat den Entwurf"), "{hint}");
        })
        .unwrap();
        assert_eq!(stored(&old).state, State::Discarded);
    }

    fn cli_discard_runs_cleanup_command(mtm: MainThreadMarker) {
        let record = disk("cc1", &[]).join("cleanup.json");
        let cfg = format!(
            r#"{{"accounts":[{{"name":"work","kind":"command",
                "senders":[{{"address":"me@example.org","name":"Me"}}],"send_command":["/bin/cat"],
                "discard_command":["{}","{}"]}}]}}"#,
            fixture_bin("send-record.sh"),
            record.display()
        );
        let mut f = fixture("cc1", &cfg);
        let extra = serde_json::json!({"mailbox_draft": {"message_id": "<d@example.org>"}});
        let id = f.add_draft(draft("Re: Offer", "Hello\n", extra));
        f.install(mtm);
        // `draftdesk discard` on the command line: it cannot reach the mailbox, the app's poll does it.
        with_app_now(|a| {
            let store = a.desk.as_ref().unwrap().sender();
            draftdesk_core::add::withdraw(store.store(), &id, None, "discarded_by_cli", "")
                .unwrap();
        })
        .unwrap();
        refresh();
        assert!(wait_for(5.0, || record.exists()), "the discard command ran");
        let cleaned = wait_for(2.0, || {
            stored(&id)
                .history
                .iter()
                .any(|h| h.event == "draft_cleanup")
        });
        assert!(cleaned);
        assert!(
            std::fs::read_to_string(&record)
                .unwrap()
                .contains("<d@example.org>")
        );
        // Once only.
        refresh();
        run_loop(0.3);
        let cleanups = stored(&id)
            .history
            .iter()
            .filter(|h| h.event == "draft_cleanup")
            .count();
        assert_eq!(cleanups, 1);
    }

    fn notify_is_noop_without_bundle(_mtm: MainThreadMarker) {
        assert!(
            !draftdesk_macos::notify::available(),
            "the test binary has no bundle"
        );
        assert!(!draftdesk_macos::notify::works());
        // Neither raises (UserNotifications would without a bundle).
        draftdesk_macos::notify::request();
        draftdesk_macos::notify::post("Draft Desk", "x", "k", Some("20261107-130000-00000001"));
    }

    // ---------- Settings ----------

    fn settings_window_opens_once(mtm: MainThreadMarker) {
        let (_f, _id) = one_draft(mtm, "st1");
        let windows = || NSApplication::sharedApplication(mtm).windows().count();
        let before = windows();
        let first = with_app_now(|a| {
            a.on_action(Action::Settings);
            a.settings.as_ref().unwrap().window.clone()
        })
        .unwrap();
        let again = with_app_now(|a| {
            a.on_action(Action::Settings);
            a.settings.as_ref().unwrap().window.clone()
        })
        .unwrap();
        assert!(std::ptr::eq(&*first, &*again), "one settings window");
        assert_eq!(windows(), before + 1);
        assert!(first.isVisible());
        assert_eq!(settings::row_count(), 1, "the fixture's account");
    }

    /// Every account sheet is tall enough for its fields and buttons (the SMTP one has eleven rows).
    fn account_sheets_show_all_their_fields(mtm: MainThreadMarker) {
        let (_f, _id) = one_draft(mtm, "st9");
        for kind in [Kind::Smtp, Kind::Gmail, Kind::Command] {
            let (shown, needed) = with_app_now(|a| {
                a.on_action(Action::Settings);
                a.settings_add(kind);
                let sheet = a.settings.as_ref().unwrap().sheet.as_ref().unwrap();
                let content = sheet.panel.contentView().unwrap();
                content.layoutSubtreeIfNeeded();
                let shown = content.frame().size.height;
                let needed = content.fittingSize().height;
                a.sheet_cancel();
                (shown, needed)
            })
            .unwrap();
            assert!(
                shown >= needed - 0.5,
                "{kind:?}: {shown} shown, {needed} needed"
            );
        }
    }

    fn smtp_sheet_saves_account_and_password(mtm: MainThreadMarker) {
        let (f, _id) = one_draft(mtm, "st2");
        with_app_now(|a| {
            a.on_action(Action::Settings);
            a.settings_add(Kind::Smtp);
            let sheet = a.settings.as_ref().unwrap().sheet.as_ref().unwrap();
            for (k, v) in [
                ("name", "mail"),
                ("address", "me2@example.org"),
                ("display", "Me Two"),
                ("host", "smtp.example.org"),
                ("port", "587"),
                ("user", "me2"),
                ("password", "s3cret"),
                ("imap_host", "imap.example.org"),
                ("imap_port", "993"),
            ] {
                sheet.set(k, v);
            }
            a.sheet_confirm();
            assert!(
                a.settings.as_ref().unwrap().sheet.is_none(),
                "the sheet closes after saving"
            );
        })
        .unwrap();
        let cfg = Config::load(&f.root).unwrap();
        let acc = cfg.account("mail").expect("saved");
        let smtp = acc.smtp.as_ref().unwrap();
        assert_eq!(
            (smtp.host.as_str(), smtp.port, smtp.security),
            ("smtp.example.org", 587, Security::Starttls)
        );
        assert_eq!(acc.imap.as_ref().unwrap().port, 993);
        assert_eq!(acc.senders[0].name.as_deref(), Some("Me Two"));
        let secrets = draftdesk_core::secrets::system();
        assert_eq!(secrets.get("mail:smtp").unwrap().as_deref(), Some("s3cret"));
        assert_eq!(secrets.get("mail:imap").unwrap().as_deref(), Some("s3cret"));
        assert_eq!(settings::row_count(), 2, "the table shows it");
        secrets.delete("mail:smtp").unwrap();
        secrets.delete("mail:imap").unwrap();
    }

    fn remove_account_after_confirmation(mtm: MainThreadMarker) {
        let (f, _id) = one_draft(mtm, "st3");
        let window = with_app_now(|a| {
            a.on_action(Action::Settings);
            let s = a.settings.as_ref().unwrap();
            assert!(!s.remove.isEnabled(), "nothing selected yet");
            s.table
                .selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(0), false);
            s.window.clone()
        })
        .unwrap();
        run_loop(0.05);
        with_app_now(|a| {
            assert!(a.settings.as_ref().unwrap().remove.isEnabled());
            a.settings_remove();
        })
        .unwrap();
        let alert = with_app_now(|a| a.alert.clone())
            .unwrap()
            .expect("asks first");
        assert!(alert.messageText().to_string().contains("work"));
        answer(&window, &alert, NSAlertFirstButtonReturn);
        assert!(
            Config::load(&f.root).unwrap().account("work").is_some(),
            "cancelled"
        );
        with_app_now(|a| a.settings_remove()).unwrap();
        let alert = with_app_now(|a| a.alert.clone()).unwrap().unwrap();
        answer(&window, &alert, NSAlertSecondButtonReturn);
        assert!(Config::load(&f.root).unwrap().account("work").is_none());
        assert_eq!(settings::row_count(), 0);
    }

    fn program_must_be_executable(mtm: MainThreadMarker) {
        let (f, _id) = one_draft(mtm, "st4");
        let dir = disk("st4", &[("notes.txt", "x")]);
        let plain = dir.join("notes.txt");
        with_app_now(|a| {
            a.on_action(Action::Settings);
            a.settings_add(Kind::Command);
            let sheet = a.settings.as_ref().unwrap().sheet.as_ref().unwrap();
            for (k, v) in [
                ("name", "cmd"),
                ("address", "me3@example.org"),
                ("send", plain.to_str().unwrap()),
            ] {
                sheet.set(k, v);
            }
            a.sheet_confirm();
            let sheet = a
                .settings
                .as_ref()
                .unwrap()
                .sheet
                .as_ref()
                .expect("stays open");
            let error = sheet.error.stringValue().to_string();
            assert!(error.contains("nicht ausführbar"), "{error}");
            a.sheet_cancel();
            // The check command too.
            a.settings_set_command(CommandSlot::Check, &plain);
            let status = a
                .settings
                .as_ref()
                .unwrap()
                .status
                .stringValue()
                .to_string();
            assert!(status.contains("nicht ausführbar"), "{status}");
        })
        .unwrap();
        let cfg = Config::load(&f.root).unwrap();
        assert!(cfg.account("cmd").is_none() && cfg.check_command.is_none());
        let check = fixture_bin("check-warn.sh");
        with_app_now(|a| a.settings_set_command(CommandSlot::Check, Path::new(&check))).unwrap();
        let cmd = Config::load(&f.root).unwrap().check_command.unwrap();
        assert_eq!((cmd.argv, cmd.timeout_s), (vec![check], 300));
    }

    fn command_sheet_saves_the_time_limit(mtm: MainThreadMarker) {
        let (f, _id) = one_draft(mtm, "st5");
        with_app_now(|a| {
            a.on_action(Action::Settings);
            a.settings_add(Kind::Command);
            let sheet = a.settings.as_ref().unwrap().sheet.as_ref().unwrap();
            assert_eq!(
                sheet.get("timeout"),
                "180",
                "the core's default is preselected"
            );
            for (k, v) in [
                ("name", "bridge"),
                ("address", "me4@example.org"),
                ("send", "/bin/cat"),
                ("timeout", "300"),
            ] {
                sheet.set(k, v);
            }
            assert_eq!(sheet.get("timeout"), "300");
            a.sheet_confirm();
            assert!(
                a.settings.as_ref().unwrap().sheet.is_none(),
                "the sheet closes after saving"
            );
        })
        .unwrap();
        let cfg = Config::load(&f.root).unwrap();
        assert_eq!(cfg.account("bridge").expect("saved").timeout_s, Some(300));
    }

    // ---------- Keyboard, VoiceOver, pictures ----------

    /// Every view below `v`, depth first.
    fn all_views(v: &NSView, out: &mut Vec<Retained<NSView>>) {
        for s in v.subviews().iter() {
            out.push(s.clone());
            all_views(&s, out);
        }
    }

    /// A draft with findings, a conversation and an attachment, the way the window shows most of itself.
    fn rich_draft(mtm: MainThreadMarker, name: &str, config: &str) -> (Fixture, String) {
        let mut f = fixture(name, config);
        let body = "Hello Ann,\n\nthe offer for Friday is attached.\n\nKind regards\nMe\n";
        let checks = serde_json::json!({
            "body_sha256": draftdesk_core::sha256_hex(body.as_bytes()), "verdict": "warn",
            "findings": [
                {"level": "warn", "title": "Date", "detail": "Is Friday right?", "quote": "for Friday", "confirm": true},
                {"level": "info", "title": "Tone", "detail": "Friendly and short."}
            ]
        });
        // The agent hands the attachment over with the draft, so its findings were made for it (not stale).
        let dir = disk(name, &[("offer.pdf", "%PDF-1.4 x")]);
        let extra = serde_json::json!({"checks": checks, "thread": "Ann wrote:\n> Could you send the offer?",
            "attachments": [dir.join("offer.pdf")]});
        let id = f.add_draft(draft("Re: Offer", body, extra));
        f.install(mtm);
        with_app_now(|a| a.select(Some(&id))).unwrap();
        (f, id)
    }

    fn every_control_has_an_accessibility_label(mtm: MainThreadMarker) {
        let (_f, _id) = rich_draft(mtm, "ax1", CFG);
        let (window, missing) = with_app_now(|a| {
            a.editor.toggle_thread();
            let mut views = Vec::new();
            all_views(&a.window.contentView().unwrap(), &mut views);
            let missing: Vec<String> = views
                .iter()
                .filter(|v| !v.isHiddenOrHasHiddenAncestor())
                .filter(|v| {
                    let control = v.downcast_ref::<NSControl>().is_some()
                        || v.downcast_ref::<NSTextView>().is_some();
                    let scroller = v.downcast_ref::<NSScroller>().is_some();
                    // A text field that cannot be edited is a label: VoiceOver reads its text.
                    let static_text = v
                        .downcast_ref::<NSTextField>()
                        .is_some_and(|t| !t.isEditable());
                    control && !scroller && !static_text
                })
                .filter(|v| {
                    let label = v
                        .accessibilityLabel()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    let title = v
                        .downcast_ref::<NSButton>()
                        .map(|b| b.title().to_string())
                        .unwrap_or_default();
                    let image = v
                        .downcast_ref::<NSImageView>()
                        .and_then(|i| i.image())
                        .and_then(|i| i.accessibilityDescription())
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    label.trim().is_empty() && title.trim().is_empty() && image.trim().is_empty()
                })
                .map(|v| v.class().name().to_string_lossy().to_string())
                .collect();
            (a.window.clone(), missing)
        })
        .unwrap();
        assert!(missing.is_empty(), "controls without a label: {missing:?}");
        let toolbar = window.toolbar().unwrap();
        for item in toolbar.items().iter() {
            let id = item.itemIdentifier().to_string();
            if id.starts_with("NSToolbar") {
                continue;
            }
            assert!(!item.label().to_string().is_empty(), "toolbar item {id}");
        }
        assert!(window.autorecalculatesKeyViewLoop());
        let first = window.initialFirstResponder().unwrap();
        let body = with_app_now(|a| a.editor.body.clone()).unwrap();
        assert!(
            std::ptr::eq(&*first, &**body as &NSView),
            "the first focus is the text"
        );
    }

    fn send_has_no_key_equivalent_anywhere(mtm: MainThreadMarker) {
        let (_f, _id) = rich_draft(mtm, "kb1", CFG);
        let (delegate, window) = with_app_now(|a| (a.delegate.clone(), a.window.clone())).unwrap();
        let bar = menu::main_menu(mtm, &delegate);
        let mut all = Vec::new();
        items(&bar, &mut all);
        for (title, action, key) in &all {
            if *action == Some(sel!(sendDraft:)) {
                assert_eq!(key, "", "menu item {title}");
            }
        }
        let mut views = Vec::new();
        all_views(&window.contentView().unwrap(), &mut views);
        for v in &views {
            if let Some(b) = v.downcast_ref::<NSButton>() {
                let key = b.keyEquivalent().to_string();
                assert!(
                    b.action() != Some(sel!(sendDraft:)) || key.is_empty(),
                    "a send button with a key"
                );
                assert_ne!(key, "\r", "no button answers Return: {}", b.title());
            }
        }
        let send = toolbar::enabled(&window, toolbar::SEND);
        assert_eq!(send, Some(true), "Send is in the toolbar, by click");
    }

    /// Pictures of the window to look at (no check): with `DRAFTDESK_SNAPSHOTS=<folder>` the harness writes the
    /// window in light and dark for a ready draft with findings and an attachment, a countdown, a failed and an
    /// invalid draft. The failed and the invalid draft join only after the first two pictures, so those show a
    /// plain queue. `DRAFTDESK_SNAPSHOTS_LANG=en` takes them in English, as the README shows them; only this test
    /// switches, every other one checks German texts.
    fn snapshots(mtm: MainThreadMarker) {
        let Some(out) = std::env::var_os("DRAFTDESK_SNAPSHOTS").map(PathBuf::from) else {
            return;
        };
        struct GermanAgain;
        impl Drop for GermanAgain {
            fn drop(&mut self) {
                i18n::set_german(true);
            }
        }
        let _german_again = GermanAgain;
        i18n::set_german(std::env::var_os("DRAFTDESK_SNAPSHOTS_LANG").is_none_or(|l| l != "en"));
        std::fs::create_dir_all(&out).unwrap();
        let record = disk("snap", &[]).join("sent.eml");
        let (_f, id) = rich_draft(mtm, "snap", &cfg_send(&record));
        let failed = add_live(
            draft(
                "Invoice 4711",
                "Hello Bob,\n\nhere it is.\n",
                serde_json::json!({}),
            ),
            None,
        );
        refresh();
        let window = with_app_now(|a| {
            a.show_window(false);
            a.window.clone()
        })
        .unwrap();
        let shoot = |name: &str| {
            for (look, appearance) in [
                ("light", "NSAppearanceNameAqua"),
                ("dark", "NSAppearanceNameDarkAqua"),
            ] {
                let a = NSAppearance::appearanceNamed(&NSString::from_str(appearance)).unwrap();
                window.setAppearance(Some(&a));
                run_loop(0.3);
                // The frame view: the content with title bar and toolbar.
                // SAFETY: plain getter of the app's own window, main thread.
                let frame = unsafe { window.contentView().unwrap().superview() }.unwrap();
                frame.layoutSubtreeIfNeeded();
                let bounds = frame.bounds();
                let rep = frame.bitmapImageRepForCachingDisplayInRect(bounds).unwrap();
                frame.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
                // SAFETY: an empty property dictionary is valid for PNG.
                let png = unsafe {
                    rep.representationUsingType_properties(
                        NSBitmapImageFileType::PNG,
                        &NSDictionary::new(),
                    )
                }
                .unwrap();
                std::fs::write(out.join(format!("{name}-{look}.png")), png.to_vec()).unwrap();
            }
            window.setAppearance(None);
        };
        with_app_now(|a| a.select(Some(&id))).unwrap();
        shoot("1-ready");
        with_app_now(|a| a.on_action(Action::Send)).unwrap();
        // The draft has findings to confirm: "Send anyway".
        let (sheet_window, alert, _) = sheet();
        let alert_window = alert.window();
        answer(&sheet_window, &alert, NSAlertSecondButtonReturn);
        let _ = alert_window;
        shoot("2-countdown");
        with_app_now(|a| a.undo_send()).unwrap();
        let bad = "20261107-090000-deadbeef";
        let bad_dir = with_app_now(|a| {
            a.desk
                .as_ref()
                .unwrap()
                .root()
                .join("state/queue")
                .join(bad)
        })
        .unwrap();
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("item.json"), "{broken").unwrap();
        with_app_now(|a| {
            let store = a.desk.as_ref().unwrap().sender();
            store
                .store()
                .mutate(&failed, &[], |i| {
                    i.state = State::Failed;
                    Ok(())
                })
                .unwrap();
        })
        .unwrap();
        refresh();
        with_app_now(|a| a.select(Some(&failed))).unwrap();
        shoot("3-failed");
        with_app_now(|a| a.select(Some(bad))).unwrap();
        shoot("4-invalid");
        assert!(!record.exists(), "nothing was sent");
    }

    /// A draft that cannot be edited (failed, sending, sent) keeps the header fields as wide as a ready one: the
    /// pictures for the README showed them squeezed to the right, next to a wide empty column of labels.
    fn read_only_header_keeps_its_width(mtm: MainThreadMarker) {
        let mut f = fixture("rowidth", CFG);
        let ready = f.add_draft(draft(
            "Re: Offer",
            "Hello Ann,\n\nhere it is.\n",
            serde_json::json!({}),
        ));
        let failed = f.add_draft(draft(
            "Invoice 4711",
            "Hello Bob,\n\nhere it is.\n",
            serde_json::json!({}),
        ));
        f.install(mtm);
        with_app_now(|a| {
            let store = a.desk.as_ref().unwrap().sender();
            store
                .store()
                .mutate(&failed, &[], |i| {
                    i.state = State::Failed;
                    Ok(())
                })
                .unwrap();
            a.show_window(false);
        })
        .unwrap();
        refresh();
        let subject_width = |id: &str| {
            with_app_now(|a| {
                a.select(Some(id));
                a.window.contentView().unwrap().layoutSubtreeIfNeeded();
                a.editor.subject.frame().size.width
            })
            .unwrap()
        };
        let editable = subject_width(&ready);
        let read_only = subject_width(&failed);
        assert!(
            read_only >= editable - 1.0,
            "the subject field shrank from {editable} to {read_only} when read-only"
        );
    }

    fn cpu_seconds() -> f64 {
        // SAFETY: getrusage fills the struct it is given.
        let mut u: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
        u.ru_utime.tv_sec as f64 + u.ru_utime.tv_usec as f64 / 1e6
    }

    /// While a callback holds the app (the "Changes not saved" question on quit runs a modal loop inside one), a
    /// timer's job waits quietly for the next call that gets through; it used to queue itself again and again.
    fn a_job_queued_while_the_app_is_busy_does_not_spin(mtm: MainThreadMarker) {
        let mut f = fixture("spin", CFG);
        f.install(mtm);
        let ran = std::rc::Rc::new(std::cell::Cell::new(0));
        let r = ran.clone();
        let cpu = with_app_now(move |_a| {
            with_app(move |_| r.set(r.get() + 1));
            let before = cpu_seconds();
            run_loop(0.5);
            cpu_seconds() - before
        })
        .unwrap();
        assert_eq!(ran.get(), 0, "the app was busy");
        assert!(cpu < 0.25, "the main thread spun for {cpu:.2} s of 0.5 s");
        with_app(|_| {});
        run_loop(0.2);
        assert_eq!(
            ran.get(),
            1,
            "the queued job runs once the app is free again"
        );
    }

    /// A callback before the app is in its slot (a notification click that starts the app) waits for it.
    fn a_job_before_the_app_is_installed_waits_for_it(mtm: MainThreadMarker) {
        let mut f = fixture("early", CFG);
        f.install(mtm);
        let app = uninstall().expect("installed");
        let ran = std::rc::Rc::new(std::cell::Cell::new(0));
        let r = ran.clone();
        with_app(move |_| r.set(r.get() + 1));
        run_loop(0.2);
        assert_eq!(ran.get(), 0, "no app yet");
        install(app);
        with_app(|_| {});
        run_loop(0.2);
        assert_eq!(ran.get(), 1, "runs once the app is there");
    }

    /// A notification of a draft that left the queue (sent, discarded, replaced) leaves the selection alone.
    fn a_notification_for_a_gone_draft_keeps_the_selection(mtm: MainThreadMarker) {
        let mut f = fixture("gone", CFG);
        let id = f.add_draft(draft(
            "Re: Offer",
            "Hello Ann,\n\nhere it is.\n",
            serde_json::json!({}),
        ));
        f.install(mtm);
        with_app_now(|a| {
            a.select(Some(&id));
            a.notification_clicked(Some("20261101-120000-00000001".into()));
            assert_eq!(a.selected.as_deref(), Some(id.as_str()));
        })
        .unwrap();
    }

    // ---------- Findings of the review of 2026-11-07 ----------

    fn banner() -> String {
        with_app_now(|a| a.editor.banner.text())
            .unwrap()
            .unwrap_or_default()
    }

    fn confirmation_is_bound_to_the_draft_it_was_asked_for(mtm: MainThreadMarker) {
        let body = "Hello Ann,\n\nthanks.\n";
        let block = |title: &str| {
            serde_json::json!({
                "body_sha256": draftdesk_core::sha256_hex(body.as_bytes()), "verdict": "block",
                "findings": [{"level": "block", "title": title}]
            })
        };
        let (_f, a_id, _record) = send_draft(
            mtm,
            "cf2",
            serde_json::json!({"checks": block("Wrong name")}),
        );
        let b_id = add_live(
            draft("Other", body, serde_json::json!({"checks": block("Other")})),
            None,
        );
        refresh();
        let ask = || {
            with_app_now(|a| a.on_action(Action::Send)).unwrap();
            sheet()
        };
        // While the sheet is open another draft comes into view (the poll after a replacement, a notification).
        let (window, alert, _) = ask();
        with_app_now(|a| a.select(Some(&b_id))).unwrap();
        answer(&window, &alert, NSAlertSecondButtonReturn);
        assert!(
            with_app_now(|a| a.pending_ids()).unwrap().is_empty(),
            "the other draft is not sent"
        );
        assert!(banner().contains("Nichts wurde gesendet"), "{}", banner());
        // New findings arrive while the sheet is open: they were not confirmed.
        with_app_now(|a| a.select(Some(&a_id))).unwrap();
        let (window, alert, _) = ask();
        let late: draftdesk_core::item::Checks =
            serde_json::from_value(block("Late finding")).unwrap();
        with_app_now(|a| {
            let s = a.desk.as_ref().unwrap().sender();
            draftdesk_core::edit::record_checks(s.store(), &a_id, late).unwrap();
        })
        .unwrap();
        refresh();
        answer(&window, &alert, NSAlertSecondButtonReturn);
        assert!(
            with_app_now(|a| a.pending_ids()).unwrap().is_empty(),
            "new findings ask again"
        );
        // Nothing changed: "Send Anyway" sends the draft it was asked for.
        let (window, alert, _) = ask();
        assert!(alert.informativeText().to_string().contains("Late finding"));
        answer(&window, &alert, NSAlertSecondButtonReturn);
        assert_eq!(
            with_app_now(|a| a.pending_ids()).unwrap(),
            vec![a_id.clone()]
        );
        with_app_now(|a| a.undo_send()).unwrap();
        assert_eq!(stored(&a_id).state, State::Ready);
        assert_eq!(stored(&b_id).state, State::Ready);
    }

    fn chosen_files_go_to_the_draft_the_dialog_was_opened_for(mtm: MainThreadMarker) {
        let mut f = fixture("at4", CFG);
        let a_id = f.add("A");
        let b_id = f.add("B");
        f.install(mtm);
        let file = disk("at4", &[("note.txt", "hello")]).join("note.txt");
        // Opened for A; by the click on "Open", B is shown.
        with_app_now(|a| {
            a.select(Some(&b_id));
            a.files_chosen(&a_id, vec![file.clone()]);
        })
        .unwrap();
        assert!(banner().starts_with("Nichts angehängt"), "{}", banner());
        assert!(names(&a_id).is_empty() && names(&b_id).is_empty());
        with_app_now(|a| a.files_chosen(&b_id, vec![file.clone()])).unwrap();
        assert_eq!(names(&b_id), vec!["note.txt"]);
    }

    fn replaced_while_typing_says_the_change_is_lost(mtm: MainThreadMarker) {
        let mut f = fixture("sp2", CFG);
        let old = f.add("Offer v1");
        let other = f.add("Other");
        f.install(mtm);
        let body = with_app_now(|a| {
            a.select(Some(&old));
            a.editor.body.clone()
        })
        .unwrap();
        body.setSelectedRange(NSRange::new(body.string().length(), 0));
        type_text(&body, "PS: see you");
        // Before the quiet save (1.5 s) the agent hands over a new version: nothing holds it back.
        let new = add_live(
            draft(
                "Offer v2",
                "Hello Ann,\n\nnew prices.\n",
                serde_json::json!({}),
            ),
            Some(&old),
        );
        refresh();
        assert_eq!(
            with_app_now(|a| a.selected.clone()).unwrap(),
            Some(new.clone())
        );
        let text = banner();
        assert!(
            text.starts_with("Der Agent hat den Entwurf") && text.contains("nicht gespeichert"),
            "{text}"
        );
        // A draft that leaves the queue while typing says so too.
        let body = with_app_now(|a| {
            a.select(Some(&other));
            a.editor.body.clone()
        })
        .unwrap();
        type_text(&body, "x");
        with_app_now(|a| {
            let s = a.desk.as_ref().unwrap().sender();
            s.store().archive(&other).unwrap();
        })
        .unwrap();
        refresh();
        assert!(banner().contains("nicht gespeichert"), "{}", banner());
    }

    fn a_replacement_from_before_is_no_news(mtm: MainThreadMarker) {
        let mut f = fixture("sp3", CFG);
        // Fixed times, so the queue reads newest first: "Offer v2", "Other", "Offer v1" (replaced before the start).
        let t = chrono::Local::now() - chrono::Duration::seconds(10);
        let store = f.desk.as_ref().unwrap().sender();
        let at = |d: Draft, replaces: Option<&str>, s: i64| {
            add(store.store(), d, replaces, t + chrono::Duration::seconds(s))
                .unwrap()
                .item_id
        };
        let old = at(draft("Offer v1", "Hello\n", serde_json::json!({})), None, 0);
        let other = at(draft("Other", "Hello\n", serde_json::json!({})), None, 1);
        at(
            draft("Offer v2", "Hello\n", serde_json::json!({})),
            Some(&old),
            2,
        );
        drop(store);
        f.install(mtm);
        let body = with_app_now(|a| {
            a.select(Some(&other));
            a.editor.body.clone()
        })
        .unwrap();
        type_text(&body, "x");
        with_app_now(|a| {
            let s = a.desk.as_ref().unwrap().sender();
            s.store().archive(&other).unwrap();
        })
        .unwrap();
        refresh();
        // The selection falls on the row below: "Offer v1".
        assert_eq!(
            with_app_now(|a| a.selected.clone()).unwrap(),
            Some(old.clone())
        );
        let hint = banner();
        assert!(
            hint.starts_with("Der Entwurf ist nicht mehr") && hint.contains("nicht gespeichert"),
            "{hint}"
        );
        // The next poll (the app's tick) keeps that hint: the replacement of "Offer v1" is old news.
        refresh();
        assert_eq!(with_app_now(|a| a.selected.clone()).unwrap(), Some(old));
        assert_eq!(banner(), hint);
    }

    fn broken_config_offers_no_archive(mtm: MainThreadMarker) {
        let mut f = fixture("bc1", CFG);
        let id = f.add("A");
        // The app starts while config.json cannot be read: every draft looks invalid.
        f.desk = None;
        std::fs::write(f.root.join("config.json"), "garbage").unwrap();
        let transports: SharedTransports = Arc::new(default_transport);
        let desk = Desk::open(
            &f.root,
            Arc::new(MemorySecrets::default()),
            transports,
            Arc::new(SystemClock),
        );
        f.desk = Some(desk.unwrap_or_else(|_| panic!("opens with the broken file")));
        f.install(mtm);
        with_app_now(|a| {
            a.select(Some(&id));
            assert!(!a.current_actions().archive);
            assert!(a.editor.banner.action.isHidden(), "no \"Move to Archive\"");
            a.on_action(Action::Discard);
        })
        .unwrap();
        assert!(
            banner().starts_with("config.json kann nicht gelesen"),
            "{}",
            banner()
        );
        run_loop(0.05);
        assert!(
            f.root.join("state/queue").join(&id).is_dir(),
            "still in the queue"
        );
    }

    fn failed_mailbox_cleanup_after_discard_is_told(mtm: MainThreadMarker) {
        let cfg = r#"{"accounts":[{"name":"work","kind":"command",
            "senders":[{"address":"me@example.org","name":"Me"}],"send_command":["/bin/cat"],
            "discard_command":["/usr/bin/false"]}]}"#;
        let mut f = fixture("dc2", cfg);
        let extra = serde_json::json!({"mailbox_draft": {"message_id": "<d@example.org>"}});
        let id = f.add_draft(draft("Re: Offer", "Hello\n", extra));
        f.install(mtm);
        with_app_now(|a| {
            a.select(Some(&id));
            a.show_window(false);
            a.on_action(Action::Discard);
        })
        .unwrap();
        let (window, alert, _) = sheet();
        answer(&window, &alert, NSAlertSecondButtonReturn);
        assert!(wait_for(5.0, || stored(&id).state == State::Discarded));
        let told = wait_for(2.0, || banner().contains("Postfach-Entwurf konnte nicht"));
        assert!(told, "{}", banner());
    }

    fn google_sheet_checks_the_name_before_the_sign_in(mtm: MainThreadMarker) {
        let (_f, _id) = one_draft(mtm, "st5");
        with_app_now(|a| {
            a.on_action(Action::Settings);
            a.settings_add(Kind::Gmail);
            let sheet = a.settings.as_ref().unwrap().sheet.as_ref().unwrap();
            for (k, v) in [
                ("name", "work"),
                ("address", "me@example.org"),
                ("client_id", "abc.apps.googleusercontent.com"),
                ("client_secret", "cs"),
            ] {
                sheet.set(k, v);
            }
            // "work" is the fixture's account: told at once, no browser sign-in starts.
            a.sheet_confirm();
            let s = a.settings.as_ref().unwrap();
            assert_eq!(s.signin, 0, "no sign-in started");
            let error = s
                .sheet
                .as_ref()
                .expect("the sheet stays")
                .error
                .stringValue();
            assert!(error.to_string().contains("exists"), "{error}");
        })
        .unwrap();
    }
}
