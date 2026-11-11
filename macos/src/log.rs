//! The app's log: `$DRAFTDESK_LOG`, else `~/Library/Logs/Draft Desk.log`, 0600. Item ids, events and error codes only;
//! never addresses, subjects or text. From 1 MiB on the file moves to `<name>.old`, keeping one old file.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;

const ROTATE_BYTES: u64 = 1024 * 1024;

pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("DRAFTDESK_LOG").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Logs/Draft Desk.log")
}

/// One line with a UTC time stamp, from any thread. A log that cannot be written is skipped.
pub fn log(message: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = path();
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
        let mut old = path.clone().into_os_string();
        old.push(".old");
        let _ = std::fs::rename(&path, old);
    }
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path);
    if let Ok(mut f) = file {
        let _ = writeln!(f, "{} {message}", draftdesk_core::now_utc());
    }
}
