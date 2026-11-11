//! Draft Desk for macOS: the window around `draftdesk-core`. The core decides (editing, sending, the queue); this
//! crate shows it with AppKit and turns clicks into core calls.
//!
//! Threads: the main thread owns the app state and every AppKit object. Slow work (check, fix, countdown, discard,
//! the start-up repair, mailbox cleanup) runs on its own threads; results come back with `on_main`.

#![cfg(target_os = "macos")]

pub mod app;
pub mod attachments;
pub mod editor;
pub mod findings;
pub mod i18n;
pub mod log;
pub mod menu;
pub mod notify;
pub mod preview;
pub mod settings;
pub mod sidebar;
pub mod toolbar;
pub mod ui;

use dispatch2::DispatchQueue;

/// Hands `f` to the main thread.
pub fn on_main(f: impl FnOnce() + Send + 'static) {
    DispatchQueue::main().exec_async(f);
}

/// Runs `f` on a new named thread; a failed start is logged.
pub fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        log::log(&format!("thread {name} did not start: {e}"));
    }
}
