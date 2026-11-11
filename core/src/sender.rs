//! The send flow. Only compiled with the `send` feature, which only the app turns on.
//!
//! The click schedules (`schedule`), the app waits [`UNDO`] and calls `fire`. `fire` sends only if the approval in
//! this process is still fresh on both the wall clock and the monotonic clock, the claim on disk succeeds and the
//! content still equals what was shown. Every step that can fail before the transport ends without sending; once
//! the transport is called, an unclear end is `unknown` and never sent again automatically.

use crate::checks::needs_confirmation;
use crate::config::{AccountCfg, Kind};
use crate::gmail::Gmail;
use crate::gmail::http::UreqClient;
use crate::item::{Item, ItemRead, MailboxDraft, State, TransportOutcome, content_digest};
use crate::mime::{self, BccMode, Built};
use crate::results::{self, ResultRecord, ResultStatus};
use crate::secrets::Secrets;
use crate::store::{
    AuditLine, Store, clear_approval, needs_cli_cleanup, push_history, write_atomic,
};
use crate::text::{body_problems, has_forbidden};
use crate::transport::command::CommandTransport;
use crate::transport::smtp::SmtpTransport;
use crate::transport::{CleanupReason, CleanupResult, Outcome, Transport};
use crate::{Error, Result, bail};
use std::collections::HashMap;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub const UNDO: Duration = Duration::from_secs(15);
/// `fire` is valid in [UNDO, UNDO + FIRE_WINDOW] on both clocks.
pub const FIRE_WINDOW: Duration = Duration::from_secs(1);

pub trait Clock: Send + Sync {
    fn wall(&self) -> SystemTime;
    fn mono(&self) -> Instant;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }
    fn mono(&self) -> Instant {
        Instant::now()
    }
}

/// Builds the transport of an account; tests put fakes here.
pub type TransportFactory = Box<
    dyn for<'s> Fn(&AccountCfg, &'s dyn Secrets) -> Option<Box<dyn Transport + 's>> + Send + Sync,
>;

/// SMTP, Gmail or the configured command, as the account says.
pub fn default_transport<'s>(
    acc: &AccountCfg,
    secrets: &'s dyn Secrets,
) -> Option<Box<dyn Transport + 's>> {
    static HTTP: LazyLock<UreqClient> = LazyLock::new(UreqClient::new);
    match acc.kind {
        Kind::Smtp => {
            SmtpTransport::new(acc, secrets).map(|t| Box::new(t) as Box<dyn Transport + 's>)
        }
        Kind::Gmail => acc.gmail.as_ref().map(|g| {
            Box::new(Gmail {
                http: &*HTTP,
                secrets,
                account: acc.name.clone(),
                client_id: g.client_id.clone(),
                sleep: std::thread::sleep,
            }) as Box<dyn Transport + 's>
        }),
        Kind::Command => {
            CommandTransport::for_account(acc).map(|t| Box::new(t) as Box<dyn Transport + 's>)
        }
    }
}

/// What the window approved, kept in this process: the disk alone is no authority.
struct Approval {
    token: String,
    digest: String,
    wall_start: SystemTime,
    mono_start: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireResult {
    /// Nothing happened (no approval, expired, cancelled, changed content); the item is `ready` or untouched.
    NotSent(String),
    /// Stopped before the transport; nothing was sent, the item is `failed`.
    Failed(String),
    Sent,
    /// The provider refused; nothing was sent.
    Rejected(String),
    /// May have been sent; never resent automatically.
    Unknown(String),
}

pub struct Sender {
    store: Store,
    secrets: Arc<dyn Secrets + Send + Sync>,
    transports: TransportFactory,
    clock: Arc<dyn Clock>,
    approvals: Mutex<HashMap<String, Approval>>,
    #[cfg(test)]
    after_claim: Option<AfterClaim>,
}

/// Test seam: runs between the claim and the build.
#[cfg(test)]
type AfterClaim = Box<dyn Fn(&Store, &str) + Send + Sync>;

impl Sender {
    pub fn new(
        store: Store,
        secrets: Arc<dyn Secrets + Send + Sync>,
        transports: TransportFactory,
        clock: Arc<dyn Clock>,
    ) -> Sender {
        Sender {
            store,
            secrets,
            transports,
            clock,
            approvals: Mutex::new(HashMap::new()),
            #[cfg(test)]
            after_claim: None,
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    fn valid(&self, id: &str) -> Result<Item> {
        match self.store.read(id) {
            ItemRead::Valid(i) => Ok(*i),
            ItemRead::Invalid { errors, .. } => {
                bail!("item {id} is invalid: {}", errors.join("; "))
            }
        }
    }

    fn transport(&self, account: &str) -> Option<Box<dyn Transport + '_>> {
        let acc = self.store.config().account(account)?;
        (self.transports)(acc, self.secrets.as_ref())
    }

    fn bcc_mode(&self, account: &str) -> BccMode {
        match self.store.config().account(account).map(|a| a.kind) {
            Some(Kind::Gmail) => BccMode::Keep,
            _ => BccMode::Strip,
        }
    }

    /// The click. Refuses when the item is not `ready`, an attempt already happened, the text holds invisible or
    /// dangerous characters, or findings need a confirmation that was not given. Returns the undo token.
    pub fn schedule(
        &self,
        id: &str,
        shown_digest: &str,
        shown_revision: u64,
        confirmed: bool,
    ) -> Result<String> {
        let item = self.valid(id)?;
        if item.state != State::Ready {
            bail!("not ready: the item is {:?}", item.state);
        }
        if matches!(
            item.send.transport_outcome,
            Some(TransportOutcome::Accepted | TransportOutcome::Unknown)
        ) {
            bail!("a send was already attempted; never again automatically");
        }
        if !body_problems(&item.body).is_empty() || has_forbidden(&item.subject) {
            bail!("encoding_blocked: remove the invisible or control characters first");
        }
        if needs_confirmation(item.checks.as_ref()) && !confirmed {
            bail!("needs_confirmation: open findings must be confirmed");
        }
        let Some(token) = self
            .store
            .schedule_send(id, shown_digest, shown_revision, confirmed)?
        else {
            bail!("not scheduled: the item changed since it was shown");
        };
        let approval = Approval {
            token: token.clone(),
            digest: shown_digest.to_string(),
            wall_start: self.clock.wall(),
            mono_start: self.clock.mono(),
        };
        self.approvals
            .lock()
            .expect("approvals lock")
            .insert(id.to_string(), approval);
        Ok(token)
    }

    /// Undo during the countdown.
    pub fn cancel(&self, id: &str, token: &str) -> Result<bool> {
        {
            let mut map = self.approvals.lock().expect("approvals lock");
            if map.get(id).is_some_and(|a| a.token == token) {
                map.remove(id);
            }
        }
        self.store.cancel_send(id, token)
    }

    fn fresh(&self, a: &Approval) -> bool {
        let window = UNDO..=UNDO + FIRE_WINDOW;
        let wall = self.clock.wall().duration_since(a.wall_start).ok();
        let mono = self.clock.mono().checked_duration_since(a.mono_start);
        wall.is_some_and(|d| window.contains(&d)) && mono.is_some_and(|d| window.contains(&d))
    }

    /// Back to `ready` after a stale timer; nothing was handed to a transport.
    fn expire(&self, id: &str, token: &str, claimed: bool) -> FireResult {
        let undo = |i: &mut Item| {
            i.state = State::Ready;
            clear_approval(i);
            push_history(
                i,
                "send_timer_expired",
                "the countdown was not on time; a new click is needed",
            );
            Ok(())
        };
        if claimed {
            let _ = self.store.mutate(id, &[State::Sending], |i| {
                if i.send.transport_outcome.is_some() {
                    bail!("already attempted");
                }
                undo(i)
            });
        } else if self.store.cancel_send(id, token).unwrap_or(false) {
            let _ = self.store.note(
                id,
                "send_timer_expired",
                "the countdown was not on time; a new click is needed",
            );
        }
        FireResult::NotSent("the approval expired".into())
    }

    /// Called by the app when the countdown ends.
    pub fn fire(&self, id: &str, token: &str) -> FireResult {
        let approval = {
            let mut map = self.approvals.lock().expect("approvals lock");
            match map.get(id) {
                Some(a) if a.token == token => map.remove(id).expect("present"),
                _ => return FireResult::NotSent("no approval in this process".into()),
            }
        };
        if !self.fresh(&approval) {
            return self.expire(id, token, false);
        }
        match self.store.claim_send(id, token, &approval.digest) {
            Ok(true) => {}
            Ok(false) => {
                return FireResult::NotSent("claim lost: cancelled, replaced or changed".into());
            }
            Err(e) => return FireResult::NotSent(format!("claim failed: {e}")),
        }
        if !self.fresh(&approval) {
            return self.expire(id, token, true);
        }
        #[cfg(test)]
        if let Some(hook) = &self.after_claim {
            hook(&self.store, id);
        }
        // Findings may arrive during the countdown (they are not part of the content): a blocking one that was
        // not confirmed stops the send, back to `ready`.
        if let Ok(item) = self.valid(id)
            && needs_confirmation(item.checks.as_ref())
            && item.send.warnings_confirmed != Some(true)
        {
            let _ = self.store.mutate(id, &[State::Sending], |i| {
                if i.send.transport_outcome.is_some() {
                    bail!("already attempted");
                }
                i.state = State::Ready;
                clear_approval(i);
                push_history(
                    i,
                    "needs_confirmation",
                    "new findings need a confirmation; nothing was sent",
                );
                Ok(())
            });
            return FireResult::NotSent("new findings need a confirmation".into());
        }
        match self.prepare(id, &approval) {
            Ok((item, built, transport, message_id)) => {
                self.transmit(id, &item, &built, transport.as_ref(), &message_id)
            }
            Err(code) => self.fail_before_transport(id, &code),
        }
    }

    fn approved_snapshot(
        &self,
        id: &str,
        approval: &Approval,
    ) -> std::result::Result<Item, String> {
        let item = self.valid(id).map_err(|_| "invalid".to_string())?;
        let digest = content_digest(&item);
        if digest != approval.digest
            || item.send.approved_digest.as_deref() != Some(digest.as_str())
        {
            return Err("content_changed".into());
        }
        Ok(item)
    }

    /// Everything between the claim and the transport. An `Err` is an error code; nothing was sent.
    #[allow(clippy::type_complexity)]
    fn prepare(
        &self,
        id: &str,
        approval: &Approval,
    ) -> std::result::Result<(Item, Built, Box<dyn Transport + '_>, String), String> {
        // Account, sender and transport come only from a snapshot that equals the approval.
        let item = self.approved_snapshot(id, approval)?;
        if !body_problems(&item.body).is_empty() || has_forbidden(&item.subject) {
            return Err("encoding_blocked".into());
        }
        let transport = self.transport(&item.account).ok_or("no_transport")?;
        // A Message-ID is visible ASCII only: an international domain gets the fallback as well.
        let domain = item
            .from
            .address
            .rsplit_once('@')
            .map(|(_, d)| d)
            .filter(|d| d.bytes().all(|b| b.is_ascii_graphic()))
            .unwrap_or("localhost");
        let message_id = format!(
            "<{}.{}@{domain}>",
            chrono::Utc::now().format("%Y%m%d%H%M%S"),
            crate::random_hex(8)
        );
        self.store
            .mutate(id, &[State::Sending], |i| {
                i.send.message_id = Some(message_id.clone());
                Ok(())
            })
            .map_err(|_| "persist_failed".to_string())?;
        // Read again for the build: still exactly what was approved, in this process and on disk. Transport, clean-up
        // and sent copy work on this snapshot only: a later read could hold what a writer without the lock put there.
        let mut item = self.approved_snapshot(id, approval)?;
        let dir = self.store.item_dir(id).map_err(|_| "invalid".to_string())?;
        let bcc = self.bcc_mode(&item.account);
        // The attachment folder itself must not be a symlink either.
        let built = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(dir.join("att"))
            .map_err(Error::from)
            .and_then(|att| mime::build(&item, &att, &message_id, bcc))
            .and_then(|b| mime::verify(&b.bytes, &item, &message_id, bcc).map(|()| b))
            .map_err(|_| "build_failed".to_string())?;
        // The intent goes to disk before the transport: a crash from here on reads as `unknown`, never as unsent.
        let attempt = crate::random_hex(8);
        let artifact = format!("send-{attempt}.eml");
        write_atomic(&dir, &artifact, &built.bytes).map_err(|_| "persist_failed".to_string())?;
        let at = crate::now_utc();
        let intent = |i: &mut Item| {
            i.send.attempt_id = Some(attempt.clone());
            i.send.attempted_at = Some(at.clone());
            i.send.artifact = Some(artifact.clone());
            i.send.mime_digest = Some(built.mime_sha256.clone());
            i.send.transport_outcome = Some(TransportOutcome::Unknown);
        };
        self.store
            .mutate(id, &[State::Sending], |i| {
                intent(i);
                Ok(())
            })
            .map_err(|_| "persist_failed".to_string())?;
        // The snapshot carries the recorded attempt too: a result published from it says `unknown`.
        intent(&mut item);
        Ok((item, built, transport, message_id))
    }

    fn fail_before_transport(&self, id: &str, code: &str) -> FireResult {
        let failed = self.store.mutate(id, &[State::Sending], |i| {
            i.state = State::Failed;
            i.send.transport_outcome = Some(TransportOutcome::Rejected);
            i.send.error_code = Some(code.to_string());
            push_history(i, "send_failed", &format!("{code}: nothing was sent"));
            Ok(())
        });
        if let Ok(item) = failed {
            self.audit(AuditLine {
                outcome: Some("not_sent".into()),
                ..AuditLine::for_item("failed", &item)
            });
            self.publish(&item, ResultStatus::Failed, true);
        }
        FireResult::Failed(code.to_string())
    }

    fn transmit(
        &self,
        id: &str,
        item: &Item,
        built: &Built,
        transport: &dyn Transport,
        message_id: &str,
    ) -> FireResult {
        match transport.send(built, item, message_id) {
            Outcome::Accepted { provider_id } => {
                // Recorded before anything else; if that fails, the outcome stays `unknown` and nothing is cleaned up.
                let accepted = self.store.mutate(id, &[State::Sending], |i| {
                    i.send.transport_outcome = Some(TransportOutcome::Accepted);
                    i.send.provider_message_id = provider_id.clone();
                    Ok(())
                });
                if accepted.is_err() {
                    self.publish(item, ResultStatus::Unknown, false);
                    return FireResult::Unknown(
                        "accepted by the provider, but the store could not record it".into(),
                    );
                }
                // State, audit and result first: a slow or hanging mailbox must not hold back the outcome.
                if let Err(e) = self.finalize_inner(id, false) {
                    self.note(id, "finalize_incomplete", &e.0);
                }
                // The approved snapshot, never the record just re-read: that one may name another mailbox draft.
                let cleanup = transport.cleanup_draft(item, CleanupReason::Sent);
                self.note_cleanup(id, "draft_cleanup", &cleanup);
                match transport.save_sent_copy(built, item) {
                    CleanupResult::Failed(e) => self.note(id, "sent_copy_failed", &e),
                    other => self.note_cleanup(id, "sent_copy", &other),
                }
                if let Err(e) = self.finalize_sent(id) {
                    self.note(id, "finalize_incomplete", &e.0);
                }
                FireResult::Sent
            }
            Outcome::Rejected { code } => {
                let failed = self.store.resolve_attempt(id, |i| {
                    i.state = State::Failed;
                    i.send.transport_outcome = Some(TransportOutcome::Rejected);
                    i.send.error_code = Some(code.clone());
                    push_history(i, "send_rejected", &code);
                    Ok(())
                });
                if let Ok(item) = failed {
                    self.audit(AuditLine {
                        outcome: Some("rejected".into()),
                        ..AuditLine::for_item("failed", &item)
                    });
                    self.publish(&item, ResultStatus::Failed, true);
                }
                FireResult::Rejected(code)
            }
            Outcome::Unknown { reason } => {
                self.mark_unknown(id, &reason);
                FireResult::Unknown(reason)
            }
        }
    }

    fn mark_unknown(&self, id: &str, reason: &str) {
        let failed = self.store.mutate(id, &[State::Sending], |i| {
            i.state = State::Failed;
            i.send.transport_outcome = Some(TransportOutcome::Unknown);
            i.send.error_code = Some("interrupted".into());
            push_history(
                i,
                "transport_unknown",
                &format!("{reason}; check the sent folder, never resent"),
            );
            Ok(())
        });
        if let Ok(item) = failed {
            self.audit(AuditLine {
                outcome: Some("unknown".into()),
                ..AuditLine::for_item("transport_unknown", &item)
            });
            self.publish(&item, ResultStatus::Unknown, false);
        }
    }

    fn note(&self, id: &str, event: &str, detail: &str) {
        let _ = self.store.note(id, event, detail);
    }

    fn note_cleanup(&self, id: &str, event: &str, r: &CleanupResult) {
        let detail = match r {
            CleanupResult::Done(d) => format!("done: {d}"),
            CleanupResult::Skipped(d) => format!("skipped: {d}"),
            CleanupResult::Failed(d) => format!("failed: {d}"),
        };
        self.note(id, event, &detail);
    }

    // ponytail: an audit write error is dropped here; the item history still carries the step.
    fn audit(&self, line: AuditLine) {
        let _ = self.store.audit(line);
    }

    /// `false` when nothing was published (no key or an error); noted in the history.
    fn publish(&self, item: &Item, status: ResultStatus, retry_allowed: bool) -> bool {
        let record = ResultRecord {
            retry_allowed,
            ..ResultRecord::for_item(item, status)
        };
        match results::publish(&self.store, self.secrets.as_ref(), &record) {
            Ok(true) => true,
            Ok(false) => {
                self.note(
                    &item.item_id,
                    "result_skipped",
                    "no signing key in the keychain",
                );
                false
            }
            Err(e) => {
                self.note(&item.item_id, "result_failed", &e.0);
                false
            }
        }
    }

    /// Discards a `ready` or `failed` item; cleans up its provider draft unless another open item of the same account
    /// uses it, then moves it to the archive (as the Python version does), so it leaves the window's list at once.
    /// Without a published result it stays in the queue for the next start to publish one, like a send. Returns how
    /// the cleanup went, for the window: a failed one does not undo the discard.
    pub fn discard(&self, id: &str) -> Result<CleanupResult> {
        let item = self.store.mutate(id, &[State::Ready, State::Failed], |i| {
            i.state = State::Discarded;
            clear_approval(i);
            push_history(i, "discarded", "");
            Ok(())
        })?;
        self.approvals.lock().expect("approvals lock").remove(id);
        let cleanup = self.cleanup_for_discard(&item);
        self.audit(AuditLine::for_item("discarded", &item));
        if self.publish(&item, ResultStatus::Discarded, false)
            && let Err(e) = self.store.archive(id)
        {
            // Discarded all the same; the poll's sweep archives it later.
            self.note(id, "archive_failed", &e.0);
        }
        Ok(cleanup)
    }

    /// True while a countdown runs in this process.
    pub fn has_pending(&self) -> bool {
        !self.approvals.lock().expect("approvals lock").is_empty()
    }

    /// The mailbox cleanup a discard on the command line could not do (the command line cannot reach the mailbox).
    /// Runs once: afterwards the history holds `draft_cleanup`. `true`: cleaned up; `false`: nothing to do (also a
    /// mailbox draft kept for another item); an error: the cleanup failed (noted, not tried again).
    pub fn cleanup_discarded(&self, id: &str) -> Result<bool> {
        let item = self.valid(id)?;
        if !needs_cli_cleanup(&item) {
            return Ok(false);
        }
        match self.cleanup_for_discard(&item) {
            CleanupResult::Done(_) => Ok(true),
            CleanupResult::Skipped(_) => Ok(false),
            CleanupResult::Failed(d) => bail!("mailbox cleanup failed: {d}"),
        }
    }

    /// Cleans up the provider draft of a discarded item, unless another open item of the same account uses it.
    /// Always leaves a `draft_cleanup` entry, also when there is nothing to do.
    fn cleanup_for_discard(&self, item: &Item) -> CleanupResult {
        let id = &item.item_id;
        let shared = item.mailbox_draft.as_ref().is_some_and(|md| {
            self.store.list().iter().any(|r| match r {
                // Another account is another mailbox: the same ids there name another draft.
                ItemRead::Valid(o) => {
                    o.item_id != *id
                        && o.state != State::Discarded
                        && o.account == item.account
                        && same_draft(o, md)
                }
                ItemRead::Invalid { .. } => false,
            })
        });
        if shared {
            self.note(
                id,
                "draft_cleanup",
                "kept: another item uses the same mailbox draft",
            );
            return CleanupResult::Skipped("kept for another item".into());
        }
        let r = match self.transport(&item.account) {
            Some(t) => t.cleanup_draft(item, CleanupReason::Discarded),
            None => CleanupResult::Skipped("the account has no transport".into()),
        };
        self.note_cleanup(id, "draft_cleanup", &r);
        r
    }

    /// After a restart: an item still `sending` becomes `failed` + `unknown` (never sent again); an accepted one is
    /// only finalized; a `failed` or `discarded` one gets its result if the app stopped before publishing it.
    pub fn reconcile(&self, id: &str) -> Result<()> {
        let item = self.valid(id)?;
        let (status, retry) = match (item.state, item.send.transport_outcome) {
            (State::Sending, Some(TransportOutcome::Accepted)) => return self.finalize_sent(id),
            (State::Sending, _) => {
                self.mark_unknown(id, "the app stopped while sending");
                return Ok(());
            }
            (State::Failed, Some(TransportOutcome::Unknown)) => (ResultStatus::Unknown, false),
            (State::Failed, outcome) => (
                ResultStatus::Failed,
                outcome == Some(TransportOutcome::Rejected),
            ),
            (State::Discarded, _) => (ResultStatus::Discarded, false),
            (state, _) => bail!("nothing to reconcile for an item in state {state:?}"),
        };
        let current = results::read_verified(&self.store, self.secrets.as_ref(), id)?;
        if current.is_none_or(|r| r.status != status) {
            self.publish(&item, status, retry);
        }
        Ok(())
    }

    /// Completes an accepted send: state `sent`, audit, result, archive. Never calls a transport. Stops before the
    /// archive while the result cannot be published, so the next start repairs it.
    pub fn finalize_sent(&self, id: &str) -> Result<()> {
        self.finalize_inner(id, true)
    }

    fn finalize_inner(&self, id: &str, archive: bool) -> Result<()> {
        let item = self.valid(id)?;
        let accepted = item.send.transport_outcome == Some(TransportOutcome::Accepted);
        let item = match item.state {
            State::Sent => item,
            State::Sending if accepted => self.store.mutate(id, &[State::Sending], |i| {
                i.state = State::Sent;
                i.send.sent_at = Some(crate::now_utc());
                Ok(())
            })?,
            state => bail!("not an accepted send: state {state:?}"),
        };
        if !item.history.iter().any(|h| h.event == "audited_sent") {
            self.store.audit(AuditLine {
                outcome: Some("accepted".into()),
                ..AuditLine::for_item("sent", &item)
            })?;
            self.note(id, "audited_sent", "");
        }
        let published = results::read_verified(&self.store, self.secrets.as_ref(), id)?
            .is_some_and(|r| r.status == ResultStatus::Sent);
        if !published && !self.publish(&item, ResultStatus::Sent, false) {
            bail!("result not published yet");
        }
        if archive {
            self.store.archive(id)?;
        }
        Ok(())
    }
}

/// The same provider draft: equal Message-ID or equal Gmail draft id.
fn same_draft(item: &Item, md: &MailboxDraft) -> bool {
    let Some(o) = &item.mailbox_draft else {
        return false;
    };
    (md.message_id.is_some() && o.message_id == md.message_id)
        || (md.gmail_draft_id.is_some() && o.gmail_draft_id == md.gmail_draft_id)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::add::add;
    use crate::config::Config;
    use crate::item::Draft;
    use crate::results::ensure_result_key;
    use crate::secrets::MemorySecrets;
    use crate::store::tests::{TempRoot, temp_root};
    use std::os::unix::fs::PermissionsExt;

    pub(crate) struct FakeClock(pub(crate) Mutex<(SystemTime, Instant)>);
    impl FakeClock {
        pub(crate) fn advance(&self, d: Duration) {
            let mut g = self.0.lock().unwrap();
            g.0 += d;
            g.1 += d;
        }
        fn jump_wall(&self, d: Duration) {
            self.0.lock().unwrap().0 += d;
        }
    }
    impl Clock for FakeClock {
        fn wall(&self) -> SystemTime {
            self.0.lock().unwrap().0
        }
        fn mono(&self) -> Instant {
            self.0.lock().unwrap().1
        }
    }

    #[derive(Default)]
    pub(crate) struct Shared {
        pub(crate) outcome: Mutex<Option<Outcome>>,
        pub(crate) cleanup: Mutex<Option<CleanupResult>>,
        sent_copy: Mutex<Option<CleanupResult>>,
        pub(crate) log: Mutex<Vec<String>>,
        on_send: Mutex<Option<Box<dyn Fn() + Send>>>,
        on_cleanup: Mutex<Option<Box<dyn Fn() + Send>>>,
        made: std::sync::atomic::AtomicUsize,
        /// The mailbox draft Message-ID each clean-up and sent copy was handed, in call order.
        drafts: Mutex<Vec<Option<String>>>,
    }

    impl Shared {
        fn saw(&self, item: &Item) {
            let id = item
                .mailbox_draft
                .as_ref()
                .and_then(|m| m.message_id.clone());
            self.drafts.lock().unwrap().push(id);
        }
    }

    pub(crate) struct FakeTransport(pub(crate) Arc<Shared>);
    impl Transport for FakeTransport {
        fn send(&self, built: &Built, _item: &Item, message_id: &str) -> Outcome {
            self.0
                .log
                .lock()
                .unwrap()
                .push(format!("send {message_id} {}", built.bytes.len()));
            if let Some(f) = self.0.on_send.lock().unwrap().as_ref() {
                f();
            }
            self.0
                .outcome
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Outcome::Accepted {
                    provider_id: Some("p-1".into()),
                })
        }
        fn cleanup_draft(&self, item: &Item, reason: CleanupReason) -> CleanupResult {
            self.0.saw(item);
            self.0
                .log
                .lock()
                .unwrap()
                .push(format!("cleanup {}", reason.as_str()));
            if let Some(f) = self.0.on_cleanup.lock().unwrap().as_ref() {
                f();
            }
            self.0
                .cleanup
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(CleanupResult::Done("ok".into()))
        }
        fn save_sent_copy(&self, _built: &Built, item: &Item) -> CleanupResult {
            self.0.saw(item);
            self.0.log.lock().unwrap().push("sent_copy".into());
            self.0
                .sent_copy
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(CleanupResult::Skipped("n/a".into()))
        }
    }

    struct Env {
        _root: TempRoot,
        sender: Sender,
        clock: Arc<FakeClock>,
        secrets: Arc<MemorySecrets>,
        shared: Arc<Shared>,
        id: String,
        files: std::path::PathBuf,
    }

    fn cfg() -> Config {
        Config::parse(
            r#"{"accounts":[{"name":"work","kind":"command","senders":[{"address":"me@example.org","name":"Me"}],
              "send_command":["/bin/cat"]},
              {"name":"home","kind":"command","send_command":["/bin/cat"],
               "senders":[{"address":"me@example.org","name":"Me"},{"address":"me@bücher.example.org","name":"Me"}]}]}"#,
        )
        .unwrap()
    }

    fn draft(files: &std::path::Path) -> Draft {
        std::fs::write(files.join("offer.pdf"), b"%PDF data").unwrap();
        serde_json::from_value(serde_json::json!({
            "schema": "draftdesk/1", "account": "work", "from": {"address": "me@example.org"},
            "to": ["Ann <ann@example.org>"], "bcc": ["boss@example.org"], "subject": "Re: Offer",
            "body": "Hello Ann,\n\nthe offer is attached.\n",
            "attachments": [files.join("offer.pdf").to_string_lossy()],
            "mailbox_draft": {"message_id": "<d.1@example.org>"},
            "origin": {"agent": "test", "session": "s1"}
        }))
        .unwrap()
    }

    fn env(name: &str) -> Env {
        let root = temp_root(name);
        let store = Store::open(&root.0, cfg()).unwrap();
        let files = root.0.join("files");
        std::fs::create_dir_all(&files).unwrap();
        let id = add(&store, draft(&files), None, chrono::Local::now())
            .unwrap()
            .item_id;
        let secrets = Arc::new(MemorySecrets::default());
        ensure_result_key(secrets.as_ref()).unwrap();
        let clock = Arc::new(FakeClock(Mutex::new((SystemTime::now(), Instant::now()))));
        let shared = Arc::new(Shared::default());
        let sender = make_sender(&root, &shared, &secrets, &clock);
        Env {
            _root: root,
            sender,
            clock,
            secrets,
            shared,
            id,
            files,
        }
    }

    fn make_sender(
        root: &TempRoot,
        shared: &Arc<Shared>,
        secrets: &Arc<MemorySecrets>,
        clock: &Arc<FakeClock>,
    ) -> Sender {
        let s = shared.clone();
        let factory: TransportFactory = Box::new(move |_, _| {
            s.made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(Box::new(FakeTransport(s.clone())) as Box<dyn Transport>)
        });
        Sender::new(
            Store::open(&root.0, cfg()).unwrap(),
            secrets.clone(),
            factory,
            clock.clone(),
        )
    }

    impl Env {
        fn item(&self) -> Item {
            self.sender.valid(&self.id).unwrap()
        }
        fn archived(&self) -> Option<Item> {
            let month = chrono::Utc::now().format("%Y-%m").to_string();
            let p = self
                .sender
                .store
                .dir()
                .join("archive")
                .join(month)
                .join(&self.id)
                .join("item.json");
            std::fs::read_to_string(p)
                .ok()
                .map(|s| serde_json::from_str(&s).unwrap())
        }
        fn schedule(&self) -> String {
            let it = self.item();
            self.sender
                .schedule(&self.id, &content_digest(&it), it.content_revision, false)
                .unwrap()
        }
        fn fire_after_undo(&self, token: &str) -> FireResult {
            self.clock.advance(UNDO);
            self.sender.fire(&self.id, token)
        }
        fn log(&self) -> Vec<String> {
            self.shared.log.lock().unwrap().clone()
        }
        fn sends(&self) -> usize {
            self.log().iter().filter(|l| l.starts_with("send ")).count()
        }
        fn result(&self) -> Option<ResultRecord> {
            results::read_verified(&self.sender.store, self.secrets.as_ref(), &self.id).unwrap()
        }
        fn audit(&self) -> String {
            std::fs::read_to_string(self.sender.store.dir().join("audit.jsonl")).unwrap()
        }
        fn cli_discard(&self, id: &str) {
            let out =
                crate::add::withdraw(&self.sender.store, id, None, "discarded_by_cli", "").unwrap();
            assert_eq!(out, crate::add::ReplaceOutcome::Withdrawn(id.into()));
        }
        fn cleanups(&self) -> usize {
            self.log()
                .iter()
                .filter(|l| l.starts_with("cleanup "))
                .count()
        }
    }

    #[test]
    fn cli_discard_gets_mailbox_cleanup_once() {
        let e = env("cd1");
        assert!(
            !e.sender.cleanup_discarded(&e.id).unwrap(),
            "not discarded yet"
        );
        e.cli_discard(&e.id);
        assert!(e.sender.cleanup_discarded(&e.id).unwrap());
        assert_eq!(e.log(), vec!["cleanup discarded"]);
        assert!(!e.sender.cleanup_discarded(&e.id).unwrap());
        assert_eq!(e.cleanups(), 1);
        let last = e.item().history.last().unwrap().clone();
        assert_eq!(
            (last.event.as_str(), last.detail.as_str()),
            ("draft_cleanup", "done: ok")
        );
    }

    #[test]
    fn cli_cleanup_keeps_shared_draft() {
        let e = env("cd2");
        let other = add(&e.sender.store, draft(&e.files), None, chrono::Local::now())
            .unwrap()
            .item_id;
        assert_ne!(other, e.id);
        e.cli_discard(&e.id);
        assert!(
            !e.sender.cleanup_discarded(&e.id).unwrap(),
            "kept: nothing was cleaned up"
        );
        assert_eq!(e.cleanups(), 0, "the other item still uses the draft");
        assert!(e.item().history.last().unwrap().detail.starts_with("kept"));
    }

    #[test]
    fn window_discard_and_superseded_need_no_cli_cleanup() {
        let e = env("cd3");
        e.sender.discard(&e.id).unwrap();
        assert!(e.archived().is_some(), "out of the queue, out of reach");
        assert_eq!(e.cleanups(), 1, "only the window's own cleanup");
        let e2 = env("cd4");
        crate::add::withdraw(&e2.sender.store, &e2.id, None, "superseded", "new").unwrap();
        assert!(!e2.sender.cleanup_discarded(&e2.id).unwrap());
        assert_eq!(e2.cleanups(), 0);
    }

    #[test]
    fn has_pending_tracks_schedule_cancel_and_fire() {
        let e = env("cd5");
        assert!(!e.sender.has_pending());
        let t = e.schedule();
        assert!(e.sender.has_pending());
        assert!(e.sender.cancel(&e.id, &t).unwrap());
        assert!(!e.sender.has_pending());
        let t = e.schedule();
        assert_eq!(e.fire_after_undo(&t), FireResult::Sent);
        assert!(!e.sender.has_pending());
    }

    #[test]
    fn schedule_fire_sends_and_archives() {
        let e = env("s1");
        let token = e.schedule();
        assert_eq!(e.item().state, State::SendingScheduled);
        assert_eq!(e.fire_after_undo(&token), FireResult::Sent);
        assert_eq!(e.sends(), 1);
        let done = e.archived().expect("archived");
        assert_eq!(done.state, State::Sent);
        assert_eq!(
            done.send.transport_outcome,
            Some(TransportOutcome::Accepted)
        );
        assert_eq!(done.send.provider_message_id.as_deref(), Some("p-1"));
        let r = e.result().unwrap();
        assert_eq!(
            (r.status, r.message_id.clone()),
            (ResultStatus::Sent, done.send.message_id.clone())
        );
        assert!(e.audit().contains("\"sent\""));
        assert_eq!(
            e.log(),
            vec![
                e.log()[0].clone(),
                "cleanup sent".to_string(),
                "sent_copy".to_string()
            ]
        );
    }

    #[test]
    fn message_id_persisted_before_transport() {
        let e = env("s2");
        let (store_dir, id) = (e.sender.store.dir().to_path_buf(), e.id.clone());
        let seen = Arc::new(Mutex::new(None));
        let s2 = seen.clone();
        *e.shared.on_send.lock().unwrap() = Some(Box::new(move || {
            let text = std::fs::read_to_string(store_dir.join("queue").join(&id).join("item.json"))
                .unwrap();
            let it: Item = serde_json::from_str(&text).unwrap();
            *s2.lock().unwrap() = Some(it);
        }));
        let token = e.schedule();
        assert_eq!(e.fire_after_undo(&token), FireResult::Sent);
        let during = seen.lock().unwrap().clone().unwrap();
        let mid = during.send.message_id.clone().unwrap();
        assert!(
            e.log()[0].contains(&mid),
            "the transport got the persisted id"
        );
        assert!(mid.ends_with("@example.org>"));
    }

    #[test]
    fn attempt_intent_before_transport_and_accepted_before_cleanup() {
        let e = env("s3");
        let (dir, id) = (e.sender.store.dir().to_path_buf(), e.id.clone());
        let read = move || -> Item {
            serde_json::from_str(
                &std::fs::read_to_string(dir.join("queue").join(&id).join("item.json")).unwrap(),
            )
            .unwrap()
        };
        let at_send = Arc::new(Mutex::new(None));
        let at_cleanup = Arc::new(Mutex::new(None));
        let (r1, a1, r2, a2) = (read.clone(), at_send.clone(), read, at_cleanup.clone());
        *e.shared.on_send.lock().unwrap() =
            Some(Box::new(move || *a1.lock().unwrap() = Some(r1())));
        *e.shared.on_cleanup.lock().unwrap() =
            Some(Box::new(move || *a2.lock().unwrap() = Some(r2())));
        let token = e.schedule();
        e.fire_after_undo(&token);
        let s = at_send.lock().unwrap().clone().unwrap();
        assert_eq!(s.state, State::Sending);
        assert_eq!(s.send.transport_outcome, Some(TransportOutcome::Unknown));
        let artifact = s.send.artifact.clone().unwrap();
        assert!(e.archived().is_some());
        let eml = e
            .sender
            .store
            .dir()
            .join("archive")
            .join(chrono::Utc::now().format("%Y-%m").to_string())
            .join(&e.id)
            .join(&artifact);
        assert_eq!(
            crate::sha256_hex(&std::fs::read(eml).unwrap()),
            s.send.mime_digest.clone().unwrap()
        );
        let c = at_cleanup.lock().unwrap().clone().unwrap();
        assert_eq!(c.send.transport_outcome, Some(TransportOutcome::Accepted));
    }

    #[test]
    fn acceptance_persist_failure_is_unknown_no_cleanup() {
        let e = env("s4");
        let dir = e.sender.store.item_dir(&e.id).unwrap();
        let d2 = dir.clone();
        *e.shared.on_send.lock().unwrap() = Some(Box::new(move || {
            std::fs::set_permissions(&d2, std::fs::Permissions::from_mode(0o500)).unwrap()
        }));
        let token = e.schedule();
        let r = e.fire_after_undo(&token);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(r, FireResult::Unknown(_)), "{r:?}");
        assert!(
            !e.log()
                .iter()
                .any(|l| l.starts_with("cleanup") || l == "sent_copy")
        );
        assert_eq!(e.result().unwrap().status, ResultStatus::Unknown);
        let it = e.item();
        assert_eq!(
            (it.state, it.send.transport_outcome),
            (State::Sending, Some(TransportOutcome::Unknown))
        );
    }

    #[test]
    fn cancel_within_undo_prevents_send() {
        let e = env("s5");
        let token = e.schedule();
        e.clock.advance(Duration::from_secs(5));
        assert!(e.sender.cancel(&e.id, &token).unwrap());
        assert!(matches!(e.fire_after_undo(&token), FireResult::NotSent(_)));
        assert_eq!(e.sends(), 0);
        assert_eq!(e.item().state, State::Ready);
    }

    #[test]
    fn stale_timer_after_reschedule_never_sends_twice() {
        let e = env("s6");
        let t1 = e.schedule();
        assert!(e.sender.cancel(&e.id, &t1).unwrap());
        let t2 = e.schedule();
        e.clock.advance(UNDO);
        assert!(matches!(e.sender.fire(&e.id, &t1), FireResult::NotSent(_)));
        assert_eq!(e.sender.fire(&e.id, &t2), FireResult::Sent);
        assert!(matches!(e.sender.fire(&e.id, &t2), FireResult::NotSent(_)));
        assert_eq!(e.sends(), 1);
    }

    #[test]
    fn fire_outside_window_needs_new_approval() {
        let e = env("s7");
        let token = e.schedule();
        e.clock.advance(Duration::from_secs(10));
        assert!(
            matches!(e.sender.fire(&e.id, &token), FireResult::NotSent(_)),
            "too early"
        );
        assert_eq!(e.item().state, State::Ready);
        let token = e.schedule();
        e.clock
            .advance(UNDO + FIRE_WINDOW + Duration::from_millis(1));
        assert!(
            matches!(e.sender.fire(&e.id, &token), FireResult::NotSent(_)),
            "too late"
        );
        // The wall clock jumped while the monotonic clock did not: not fresh either.
        let token = e.schedule();
        e.clock.advance(UNDO);
        e.clock.jump_wall(Duration::from_secs(3600));
        assert!(
            matches!(e.sender.fire(&e.id, &token), FireResult::NotSent(_)),
            "wall jump"
        );
        assert_eq!(e.sends(), 0);
        let it = e.item();
        assert_eq!(it.state, State::Ready);
        assert_eq!(it.history.last().unwrap().event, "send_timer_expired");
    }

    #[test]
    fn fire_without_ram_entry_does_nothing() {
        let e = env("s8");
        let token = e.schedule();
        let restarted = make_sender(&e._root, &e.shared, &e.secrets, &e.clock);
        e.clock.advance(UNDO);
        assert!(matches!(
            restarted.fire(&e.id, &token),
            FireResult::NotSent(_)
        ));
        assert_eq!(e.sends(), 0);
        assert_eq!(
            e.item().state,
            State::SendingScheduled,
            "untouched; the restart reconcile unschedules it"
        );
    }

    #[test]
    fn forged_disk_digest_never_sends() {
        let e = env("s9");
        let token = e.schedule();
        let mut forged = e.item();
        forged.to = vec!["mallory@example.org".into()];
        forged.send.approved_digest = Some(content_digest(&forged));
        crate::store::tests::put(&e.sender.store, &forged);
        assert!(matches!(e.fire_after_undo(&token), FireResult::NotSent(_)));
        assert_eq!(e.sends(), 0);
        assert_eq!(e.item().state, State::Ready);
    }

    #[test]
    fn digest_change_after_claim_caught_before_build() {
        let mut e = env("s10");
        e.sender.after_claim = Some(Box::new(|store: &Store, id: &str| {
            let mut it = match store.read(id) {
                ItemRead::Valid(i) => *i,
                _ => panic!(),
            };
            it.subject = "Changed after the claim".into();
            crate::store::tests::put(store, &it);
        }));
        let token = e.schedule();
        assert!(
            matches!(e.fire_after_undo(&token), FireResult::Failed(ref c) if c == "content_changed")
        );
        assert_eq!(e.sends(), 0);
    }

    #[test]
    fn sent_item_cannot_be_scheduled_again() {
        let e = env("s11");
        let token = e.schedule();
        e.clock.advance(UNDO);
        // Keep the item in the queue: no signing key, so finalizing stops before the archive.
        e.secrets.delete(crate::results::KEY_NAME).unwrap();
        assert_eq!(e.sender.fire(&e.id, &token), FireResult::Sent);
        let it = e.item();
        assert_eq!(it.state, State::Sent);
        assert!(
            e.sender
                .schedule(&e.id, &content_digest(&it), it.content_revision, true)
                .is_err()
        );
    }

    #[test]
    fn forbidden_char_in_body_blocks_schedule() {
        let e = env("s12");
        e.sender
            .store
            .mutate(&e.id, &[], |i| {
                i.body = "Hello\u{202E} Ann".into();
                i.body_sha256 = crate::sha256_hex(i.body.as_bytes());
                Ok(())
            })
            .unwrap();
        let it = e.item();
        let err = e
            .sender
            .schedule(&e.id, &content_digest(&it), it.content_revision, true)
            .unwrap_err();
        assert!(err.0.contains("encoding_blocked"), "{err}");
        assert_eq!(e.item().state, State::Ready);
    }

    #[test]
    fn build_error_fails_without_transport() {
        let e = env("s13");
        let token = e.schedule();
        std::fs::write(
            e.sender
                .store
                .item_dir(&e.id)
                .unwrap()
                .join("att/offer.pdf"),
            b"swapped!!",
        )
        .unwrap();
        assert!(
            matches!(e.fire_after_undo(&token), FireResult::Failed(ref c) if c == "build_failed")
        );
        assert_eq!(e.sends(), 0);
        let it = e.item();
        assert_eq!(
            (it.state, it.send.transport_outcome),
            (State::Failed, Some(TransportOutcome::Rejected))
        );
        assert_eq!(e.result().unwrap().status, ResultStatus::Failed);
        assert!(e.result().unwrap().retry_allowed);
    }

    #[test]
    fn changed_draft_selector_after_click_never_deleted() {
        let e = env("s14");
        let token = e.schedule();
        let mut it = e.item();
        it.mailbox_draft = Some(MailboxDraft {
            message_id: Some("<someone-else@example.org>".into()),
            ..Default::default()
        });
        crate::store::tests::put(&e.sender.store, &it);
        assert!(matches!(e.fire_after_undo(&token), FireResult::NotSent(_)));
        assert!(e.log().is_empty(), "no send, no cleanup");
    }

    #[test]
    fn rejected_maps_to_failed() {
        let e = env("s15");
        *e.shared.outcome.lock().unwrap() = Some(Outcome::Rejected {
            code: "smtp_5xx".into(),
        });
        let token = e.schedule();
        assert_eq!(
            e.fire_after_undo(&token),
            FireResult::Rejected("smtp_5xx".into())
        );
        let it = e.item();
        assert_eq!(
            (it.state, it.send.transport_outcome),
            (State::Failed, Some(TransportOutcome::Rejected))
        );
        assert_eq!(it.send.error_code.as_deref(), Some("smtp_5xx"));
        assert_eq!(e.result().unwrap().status, ResultStatus::Failed);
        assert!(e.audit().contains("\"failed\""));
        assert!(!e.log().iter().any(|l| l.starts_with("cleanup")));
    }

    #[test]
    fn unknown_maps_to_unknown_never_retryable() {
        let e = env("s16");
        *e.shared.outcome.lock().unwrap() = Some(Outcome::Unknown {
            reason: "dropped".into(),
        });
        let token = e.schedule();
        assert!(matches!(e.fire_after_undo(&token), FireResult::Unknown(_)));
        let it = e.item();
        assert_eq!(
            (it.state, it.send.transport_outcome),
            (State::Failed, Some(TransportOutcome::Unknown))
        );
        assert_eq!(it.send.error_code.as_deref(), Some("interrupted"));
        let r = e.result().unwrap();
        assert_eq!((r.status, r.retry_allowed), (ResultStatus::Unknown, false));
        assert!(e.audit().contains("transport_unknown"));
        assert!(
            e.sender
                .schedule(&e.id, &content_digest(&it), it.content_revision, true)
                .is_err()
        );
        assert!(
            e.sender
                .store
                .mutate(&e.id, &[], |i| {
                    i.state = State::Ready;
                    Ok(())
                })
                .is_err()
        );
    }

    #[test]
    fn reconcile_sending_becomes_unknown_never_resends() {
        let e = env("s17");
        let token = e.schedule();
        let it = e.item();
        assert!(
            e.sender
                .store
                .claim_send(&e.id, &token, &content_digest(&it))
                .unwrap()
        );
        // The app died here. After the restart:
        let restarted = make_sender(&e._root, &e.shared, &e.secrets, &e.clock);
        assert_eq!(
            restarted.store().reconcile_on_start().needs_reconcile,
            vec![e.id.clone()]
        );
        restarted.reconcile(&e.id).unwrap();
        let it = e.item();
        assert_eq!(
            (it.state, it.send.transport_outcome),
            (State::Failed, Some(TransportOutcome::Unknown))
        );
        assert_eq!(e.result().unwrap().status, ResultStatus::Unknown);
        assert_eq!(e.sends(), 0);
    }

    #[test]
    fn reconcile_accepted_only_finalizes() {
        let e = env("s18");
        let token = e.schedule();
        let it = e.item();
        assert!(
            e.sender
                .store
                .claim_send(&e.id, &token, &content_digest(&it))
                .unwrap()
        );
        e.sender
            .store
            .mutate(&e.id, &[], |i| {
                i.send.message_id = Some("<x.1@example.org>".into());
                i.send.transport_outcome = Some(TransportOutcome::Accepted);
                Ok(())
            })
            .unwrap();
        e.sender.reconcile(&e.id).unwrap();
        assert_eq!(e.archived().unwrap().state, State::Sent);
        assert_eq!(e.result().unwrap().status, ResultStatus::Sent);
        assert_eq!(e.sends(), 0);
    }

    #[test]
    fn reconcile_only_from_sending_or_unknown() {
        let e = env("s19");
        assert!(e.sender.reconcile(&e.id).is_err());
        assert_eq!(e.item().state, State::Ready);
    }

    #[test]
    fn finalize_result_failure_then_finalize_sent_repairs() {
        let e = env("s20");
        e.secrets.delete(crate::results::KEY_NAME).unwrap();
        let token = e.schedule();
        assert_eq!(e.fire_after_undo(&token), FireResult::Sent);
        let it = e.item();
        assert_eq!(it.state, State::Sent);
        assert!(it.history.iter().any(|h| h.event == "result_skipped"));
        assert_eq!(
            e.sender.store.reconcile_on_start().needs_finalize,
            vec![e.id.clone()]
        );
        ensure_result_key(e.secrets.as_ref()).unwrap();
        e.sender.finalize_sent(&e.id).unwrap();
        assert!(e.archived().is_some());
        assert_eq!(e.result().unwrap().status, ResultStatus::Sent);
        assert_eq!(
            e.audit().matches("\"event\":\"sent\"").count(),
            1,
            "audited once"
        );
    }

    #[test]
    fn finalize_sent_never_transports() {
        let e = env("s21");
        assert!(
            e.sender.finalize_sent(&e.id).is_err(),
            "a ready item is not finalized"
        );
        assert!(e.log().is_empty());
    }

    #[test]
    fn discard_results_cleans_up_and_archives() {
        let e = env("s22");
        e.sender.discard(&e.id).unwrap();
        // Leaves the queue right away, as in the Python version (a test on the Mac found it staying in the list).
        let it = e.archived().expect("archived");
        assert_eq!(it.state, State::Discarded);
        assert!(e.sender.valid(&e.id).is_err(), "no longer in the queue");
        assert_eq!(e.log(), vec!["cleanup discarded".to_string()]);
        assert_eq!(e.result().unwrap().status, ResultStatus::Discarded);
        assert!(e.audit().contains("\"discarded\""));
        assert!(e.sender.discard(&e.id).is_err());
    }

    #[test]
    fn discard_without_a_result_stays_for_the_next_start() {
        let e = env("s22b");
        e.secrets.delete(results::KEY_NAME).unwrap();
        e.sender.discard(&e.id).unwrap();
        assert_eq!(
            e.item().state,
            State::Discarded,
            "the next start publishes its result"
        );
        assert!(e.archived().is_none());
    }

    #[test]
    fn discard_keeps_draft_referenced_by_other_open_item() {
        let e = env("s23");
        add(&e.sender.store, draft(&e.files), None, chrono::Local::now()).unwrap();
        e.sender.discard(&e.id).unwrap();
        assert!(
            e.log().is_empty(),
            "the other item still uses the mailbox draft"
        );
        assert!(
            e.archived()
                .unwrap()
                .history
                .iter()
                .any(|h| h.event == "draft_cleanup" && h.detail.contains("another item"))
        );
    }

    #[test]
    fn discard_completes_when_cleanup_fails() {
        let e = env("s24");
        *e.shared.cleanup.lock().unwrap() = Some(CleanupResult::Failed("imap down".into()));
        let cleanup = e.sender.discard(&e.id).unwrap();
        assert_eq!(cleanup, CleanupResult::Failed("imap down".into()), "told");
        let it = e.archived().unwrap();
        assert_eq!(it.state, State::Discarded);
        assert!(it.history.iter().any(|h| h.detail.contains("imap down")));
        assert_eq!(e.result().unwrap().status, ResultStatus::Discarded);
    }

    #[test]
    fn replace_after_schedule_prevents_send() {
        let e = env("s25");
        let token = e.schedule();
        let out = add(
            &e.sender.store,
            draft(&e.files),
            Some(&e.id),
            chrono::Local::now(),
        )
        .unwrap();
        assert!(matches!(
            out.replaced,
            Some(crate::add::ReplaceOutcome::Withdrawn(_))
        ));
        assert!(matches!(e.fire_after_undo(&token), FireResult::NotSent(_)));
        assert_eq!(e.sends(), 0);
        assert_eq!(e.item().state, State::Discarded);
    }

    #[test]
    fn needs_confirmation_requires_confirmed_flag() {
        let e = env("s26");
        e.sender
            .store
            .mutate(&e.id, &[], |i| {
                i.checks = Some(crate::item::Checks {
                    body_sha256: i.body_sha256.clone(),
                    verdict: crate::item::Verdict::Warn,
                    findings: vec![crate::item::Finding {
                        level: crate::item::Level::Warn,
                        title: "promise without source".into(),
                        detail: String::new(),
                        quote: String::new(),
                        confirm: true,
                    }],
                    ran_at: None,
                    source: None,
                    content_digest: None,
                });
                Ok(())
            })
            .unwrap();
        let it = e.item();
        assert!(
            e.sender
                .schedule(&e.id, &content_digest(&it), it.content_revision, false)
                .unwrap_err()
                .0
                .contains("confirm")
        );
        let token = e
            .sender
            .schedule(&e.id, &content_digest(&it), it.content_revision, true)
            .unwrap();
        assert_eq!(e.fire_after_undo(&token), FireResult::Sent);
    }

    #[test]
    fn sent_copy_failure_does_not_change_outcome() {
        let e = env("s27");
        *e.shared.sent_copy.lock().unwrap() = Some(CleanupResult::Failed("append refused".into()));
        let token = e.schedule();
        assert_eq!(e.fire_after_undo(&token), FireResult::Sent);
        let done = e.archived().unwrap();
        assert_eq!(done.state, State::Sent);
        assert!(done.history.iter().any(|h| h.event == "sent_copy_failed"));
    }

    #[test]
    fn new_blocking_finding_during_countdown_stops_send() {
        let mut e = env("s29");
        e.sender.after_claim = Some(Box::new(|store: &Store, id: &str| {
            store
                .mutate(id, &[], |i| {
                    i.checks = Some(crate::item::Checks {
                        body_sha256: i.body_sha256.clone(),
                        verdict: crate::item::Verdict::Block,
                        findings: vec![],
                        ran_at: None,
                        source: None,
                        content_digest: None,
                    });
                    Ok(())
                })
                .unwrap();
        }));
        let token = e.schedule();
        assert!(matches!(e.fire_after_undo(&token), FireResult::NotSent(_)));
        assert_eq!(e.sends(), 0);
        let it = e.item();
        assert_eq!(it.state, State::Ready);
        assert_eq!(it.history.last().unwrap().event, "needs_confirmation");
    }

    #[test]
    fn transport_is_chosen_only_after_the_content_check() {
        let mut e = env("s30");
        e.sender.after_claim = Some(Box::new(|store: &Store, id: &str| {
            let mut it = match store.read(id) {
                ItemRead::Valid(i) => *i,
                _ => panic!(),
            };
            it.subject = "Changed after the claim".into();
            crate::store::tests::put(store, &it);
        }));
        let token = e.schedule();
        assert!(
            matches!(e.fire_after_undo(&token), FireResult::Failed(ref c) if c == "content_changed")
        );
        assert_eq!(
            e.shared.made.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no transport was even built"
        );
    }

    #[test]
    fn result_is_published_before_the_mailbox_cleanup() {
        let e = env("s31");
        let results = e
            .sender
            .store
            .dir()
            .join("results")
            .join(format!("{}.json", e.id));
        let seen = Arc::new(Mutex::new(false));
        let s2 = seen.clone();
        *e.shared.on_cleanup.lock().unwrap() =
            Some(Box::new(move || *s2.lock().unwrap() = results.exists()));
        let token = e.schedule();
        assert_eq!(e.fire_after_undo(&token), FireResult::Sent);
        assert!(
            *seen.lock().unwrap(),
            "the sent result existed when the cleanup ran"
        );
    }

    #[test]
    fn symlinked_attachment_folder_fails_without_transport() {
        let e = env("s32");
        let token = e.schedule();
        let dir = e.sender.store.item_dir(&e.id).unwrap();
        let outside = e.files.join("outside");
        std::fs::rename(dir.join("att"), &outside).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("att")).unwrap();
        assert!(
            matches!(e.fire_after_undo(&token), FireResult::Failed(ref c) if c == "build_failed")
        );
        assert_eq!(e.sends(), 0);
    }

    #[test]
    fn reconcile_publishes_a_missing_failed_result() {
        let e = env("s33");
        e.secrets.delete(crate::results::KEY_NAME).unwrap();
        *e.shared.outcome.lock().unwrap() = Some(Outcome::Rejected {
            code: "smtp_5xx".into(),
        });
        let token = e.schedule();
        e.fire_after_undo(&token);
        assert!(e.result().is_none());
        ensure_result_key(e.secrets.as_ref()).unwrap();
        assert_eq!(
            e.sender.store.reconcile_on_start().check_result,
            vec![e.id.clone()]
        );
        e.sender.reconcile(&e.id).unwrap();
        let r = e.result().unwrap();
        assert_eq!((r.status, r.retry_allowed), (ResultStatus::Failed, true));
    }

    #[test]
    fn bcc_kept_only_for_gmail_accounts() {
        let e = env("s28");
        assert_eq!(e.sender.bcc_mode("work"), BccMode::Strip);
    }

    #[test]
    fn cleanup_after_a_send_uses_the_approved_snapshot() {
        // A writer without the lock swaps the mailbox draft while the transport runs: the clean-up and the sent copy
        // still get the draft that was approved and sent.
        let e = env("s34");
        let path = e.sender.store.item_dir(&e.id).unwrap().join("item.json");
        *e.shared.on_send.lock().unwrap() = Some(Box::new(move || {
            let mut it: Item =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            it.mailbox_draft = Some(MailboxDraft {
                message_id: Some("<someone-else@example.org>".into()),
                ..Default::default()
            });
            std::fs::write(&path, serde_json::to_string_pretty(&it).unwrap()).unwrap();
        }));
        let token = e.schedule();
        assert_eq!(e.fire_after_undo(&token), FireResult::Sent);
        assert_eq!(
            *e.shared.drafts.lock().unwrap(),
            vec![Some("<d.1@example.org>".to_string()); 2],
            "clean-up, then sent copy"
        );
    }

    #[test]
    fn discard_cleans_up_although_another_account_has_the_same_draft() {
        let e = env("s35");
        let mut other = draft(&e.files);
        other.account = "home".into();
        add(&e.sender.store, other, None, chrono::Local::now()).unwrap();
        e.sender.discard(&e.id).unwrap();
        assert_eq!(
            e.log(),
            vec!["cleanup discarded".to_string()],
            "the same Message-ID in another account is another mailbox's draft"
        );
    }

    #[test]
    fn message_id_of_an_international_sender_domain_stays_ascii() {
        let e = env("s36");
        let mut d = draft(&e.files);
        d.account = "home".into();
        d.from.address = "me@bücher.example.org".into();
        let id = add(&e.sender.store, d, None, chrono::Local::now())
            .unwrap()
            .item_id;
        let it = e.sender.valid(&id).unwrap();
        let token = e
            .sender
            .schedule(&id, &content_digest(&it), it.content_revision, false)
            .unwrap();
        e.clock.advance(UNDO);
        assert_eq!(e.sender.fire(&id, &token), FireResult::Sent);
        let sent = e.log()[0].clone();
        assert!(sent.contains("@localhost> "), "{sent}");
    }
}
