//! The contract every way of sending follows. Only compiled with the `send` feature.

pub mod command;
pub mod smtp;

use crate::item::Item;
use crate::mime::Built;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The provider took the mail.
    Accepted { provider_id: Option<String> },
    /// Nothing was handed over; safe to fix and try again by hand.
    Rejected { code: String },
    /// May or may not have gone out: never resend automatically.
    Unknown { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupReason {
    Sent,
    Discarded,
}

impl CleanupReason {
    pub fn as_str(self) -> &'static str {
        match self {
            CleanupReason::Sent => "sent",
            CleanupReason::Discarded => "discarded",
        }
    }
}

/// What happened to the provider draft the item came from. Never an error for the caller: the mail state is
/// already decided, cleanup is best effort with a note in the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupResult {
    Done(String),
    Skipped(String),
    Failed(String),
}

pub trait Transport {
    fn send(&self, built: &Built, item: &Item, message_id: &str) -> Outcome;
    fn cleanup_draft(&self, item: &Item, reason: CleanupReason) -> CleanupResult;
    /// A copy in the sent folder, for providers that do not file one themselves. Never changes the outcome.
    fn save_sent_copy(&self, _built: &Built, _item: &Item) -> CleanupResult {
        CleanupResult::Skipped("the provider files sent mail itself".into())
    }
}
