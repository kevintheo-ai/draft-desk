//! Draft Desk core: the draft format, the queue and everything needed to send a reviewed draft.

pub mod accounts;
pub mod add;
pub mod address;
pub mod checks;
pub mod config;
#[cfg(feature = "send")]
pub mod desk;
pub mod edit;
pub mod gmail;
pub mod html;
#[cfg(feature = "send")]
pub mod imap;
pub mod item;
pub mod mime;
pub mod process;
pub mod results;
pub mod secrets;
#[cfg(feature = "send")]
pub mod sender;
pub mod store;
pub mod text;
#[cfg(feature = "send")]
pub mod transport;

use sha2::{Digest, Sha256};
use std::fmt;

/// The app's bundle identifier: the command line wakes the app with it, the build signs the bundle with it.
pub const APP_BUNDLE_ID: &str = "io.github.kevintheo-ai.draft-desk";

/// Lowercase hex sha256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// `n` random bytes as lowercase hex.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("the system random source failed");
    hex(&buf)
}

/// Now, UTC, RFC 3339 with seconds.
pub fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One error type for the whole core: a short message meant for logs and the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(format!("io: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error(format!("json: {e}"))
    }
}

/// `Err(Error(..))` from a format string.
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => { return Err($crate::Error(format!($($arg)*))) };
}
