//! Draft Desk for macOS.

#[cfg(target_os = "macos")]
fn main() {
    draftdesk_macos::app::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("The Draft Desk app runs on macOS only for now; the core crate is platform-neutral.");
}
