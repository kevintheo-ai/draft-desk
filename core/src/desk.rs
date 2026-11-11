//! The app's side of Draft Desk without any window: the single running instance, the repair after a restart, the
//! poll that notices new and changed drafts, and the guards around sending. A window (AppKit now, other platforms
//! later) shows what this returns and calls it. Only compiled with the `send` feature.

use crate::checks::{self, is_stale, needs_confirmation};
use crate::config::{AccountCfg, CONFIG_FILE, Config};
use crate::edit::record_checks;
use crate::item::{
    Finding, Item, ItemRead, Level, State, TransportOutcome, Verdict, content_digest,
};
use crate::results::ensure_result_key;
use crate::secrets::Secrets;
use crate::sender::{Clock, FireResult, Sender, TransportFactory, UNDO};
use crate::store::{Store, needs_cli_cleanup};
use crate::transport::{CleanupResult, Transport};
use crate::{Error, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

/// Builds the transport of an account; shared, because every reload of the configuration needs a new sender.
pub type SharedTransports = Arc<
    dyn for<'s> Fn(&AccountCfg, &'s dyn Secrets) -> Option<Box<dyn Transport + 's>> + Send + Sync,
>;

#[derive(Debug)]
pub enum OpenError {
    /// Another Draft Desk holds `state/app.lock` for this root.
    AlreadyRunning,
    Failed(Error),
}

/// Something the window should tell the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A scheduled send was cancelled by the restart; nothing was sent.
    Unscheduled(String),
    /// The app stopped during a send: the outcome is unknown, it is never sent again.
    Interrupted(String),
    /// It was sent; the steps after the send were completed now.
    Finalized(String),
    /// The mailbox draft of a discard on the command line was cleaned up.
    CleanedUp(String),
    /// `config.json` could not be loaded (the detail); the previous or an empty configuration is used.
    ConfigInvalid(String),
    /// The result signing key is missing (the detail): results do not reach the agents.
    NoSigningKey(String),
    /// Something went wrong with one item (its id, or empty) and why.
    Problem(String, String),
}

/// What one poll found. The window redraws from `items` and uses the lists to notify and react.
#[derive(Debug, Default)]
pub struct Poll {
    /// The queue, newest first.
    pub items: Vec<ItemRead>,
    /// Seen for the first time and `ready` (not on the first poll: that one only takes stock).
    pub new_ready: Vec<String>,
    /// Another state revision than last time, validity flipped, or new and not `ready`.
    pub changed: Vec<String>,
    /// Moved to the archive by this poll.
    pub archived: Vec<String>,
    /// Discarded on the command line: hand each to `cleanup_discarded` on a background thread (once per run).
    pub cleanup_due: Vec<String>,
    pub notes: Vec<Note>,
}

/// `config.json` at the last load: modification and change time (seconds, nanoseconds), inode, size. The change time
/// sees a chmod that makes an unreadable file readable, and an edit of the same size within one modification tick;
/// the inode sees a file put in its place.
type Stamp = Option<(i64, i64, i64, i64, u64, u64)>;

fn config_stamp(root: &Path) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(root.join(CONFIG_FILE))
        .ok()
        .map(|m| {
            (
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
                m.ino(),
                m.len(),
            )
        })
}

fn read_id(r: &ItemRead) -> &str {
    r.id()
}

pub struct Desk {
    root: PathBuf,
    secrets: Arc<dyn Secrets + Send + Sync>,
    transports: SharedTransports,
    clock: Arc<dyn Clock>,
    sender: RwLock<Arc<Sender>>,
    open_notes: Mutex<Vec<Note>>,
    config_stamp: Mutex<Stamp>,
    /// `config.json` could be loaded. Not at a start with a broken one: the empty stand-in makes every draft look
    /// invalid, so none may go to the archive until a valid one is loaded.
    config_ok: AtomicBool,
    /// The version of `config.json` whose store could not be opened, told once; the next poll tries again.
    reload_failed: Mutex<Option<Stamp>>,
    /// The last poll's view: id -> state revision (`None` = invalid). `None` before the first poll.
    seen: Mutex<Option<BTreeMap<String, Option<u64>>>>,
    /// Command-line discards already handed out for their mailbox cleanup.
    handed_out: Mutex<BTreeSet<String>>,
    /// Sends between the end of the countdown and their outcome.
    firing: AtomicUsize,
    /// Items with a check or fix running: no send, no second run.
    busy: Mutex<BTreeSet<String>>,
    /// The countdown's wait (tests make it instant).
    sleep: Arc<dyn Fn(Duration) + Send + Sync>,
    /// Held for the life of the process: one Draft Desk per root.
    _app_lock: File,
}

/// Why the click did not schedule a send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    /// A check or fix runs for this draft.
    Busy,
    /// Open findings need an explicit confirmation first.
    NeedsConfirmation(Confirmation),
    Refused(String),
}

/// What the confirmation dialog lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    /// Findings of level `block` or marked `confirm`.
    pub findings: Vec<Finding>,
    pub verdict_block: bool,
    /// The findings were made for an earlier text.
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scheduled {
    pub token: String,
    pub undo: Duration,
}

/// Which buttons the window enables.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Actions {
    pub check: bool,
    pub fix: bool,
    pub discard: bool,
    pub send: bool,
    /// Only for an invalid item: move it out of the queue.
    pub archive: bool,
}

/// `None`: sending needs no confirmation.
pub fn confirmation(item: &Item) -> Option<Confirmation> {
    let c = item.checks.as_ref()?;
    if !needs_confirmation(Some(c)) {
        return None;
    }
    Some(Confirmation {
        findings: c
            .findings
            .iter()
            .filter(|f| f.level == Level::Block || f.confirm)
            .cloned()
            .collect(),
        verdict_block: c.verdict == Verdict::Block,
        stale: is_stale(c, item),
    })
}

/// `busy`: a check or fix runs for this item; `pending`: its countdown runs.
pub fn actions(read: &ItemRead, cfg: &Config, busy: bool, pending: bool) -> Actions {
    let ItemRead::Valid(i) = read else {
        return Actions {
            archive: true,
            ..Actions::default()
        };
    };
    let free = !busy && !pending;
    let ready = i.state == State::Ready && free;
    Actions {
        check: ready && cfg.check_command.is_some(),
        fix: ready
            && cfg.fix_command.is_some()
            && i.checks.as_ref().is_some_and(|c| !c.findings.is_empty()),
        discard: matches!(i.state, State::Ready | State::Failed) && free,
        send: ready,
        archive: false,
    }
}

/// The busy mark of one item; dropping it frees the item.
struct Busy {
    desk: Arc<Desk>,
    id: String,
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.desk.busy.lock().expect("busy lock").remove(&self.id);
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> crate::Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .map(|_| ())
        .map_err(|e| Error(format!("could not start a thread: {e}")))
}

fn ready_item(sender: &Sender, id: &str) -> crate::Result<Item> {
    let item = crate::edit::valid(sender.store(), id)?;
    if item.state != State::Ready {
        bail!("not ready: the item is {:?}", item.state);
    }
    Ok(item)
}

/// Forces the higher-ranked closure type the sender expects.
fn factory<F>(f: F) -> F
where
    F: for<'s> Fn(&AccountCfg, &'s dyn Secrets) -> Option<Box<dyn Transport + 's>>,
{
    f
}

fn new_sender(
    store: Store,
    secrets: &Arc<dyn Secrets + Send + Sync>,
    transports: &SharedTransports,
    clock: &Arc<dyn Clock>,
) -> Sender {
    let t = transports.clone();
    let make: TransportFactory = Box::new(factory(move |acc, s| t(acc, s)));
    Sender::new(store, secrets.clone(), make, clock.clone())
}

impl Desk {
    /// Takes the single-instance lock and opens the store. A broken `config.json` does not stop the app: it starts
    /// with an empty configuration and reports it (`boot`).
    pub fn open(
        root: &Path,
        secrets: Arc<dyn Secrets + Send + Sync>,
        transports: SharedTransports,
        clock: Arc<dyn Clock>,
    ) -> std::result::Result<Desk, OpenError> {
        let stamp = config_stamp(root);
        let (cfg, notes, config_ok) = match Config::load(root) {
            Ok(c) => (c, vec![], true),
            Err(e) => (Config::default(), vec![Note::ConfigInvalid(e.0)], false),
        };
        let store = Store::open(root, cfg).map_err(OpenError::Failed)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(store.dir().join("app.lock"))
            .map_err(|e| OpenError::Failed(e.into()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(OpenError::AlreadyRunning),
            Err(TryLockError::Error(e)) => return Err(OpenError::Failed(e.into())),
        }
        let sender = new_sender(store, &secrets, &transports, &clock);
        Ok(Desk {
            root: root.to_path_buf(),
            secrets,
            transports,
            clock,
            sender: RwLock::new(Arc::new(sender)),
            open_notes: Mutex::new(notes),
            config_stamp: Mutex::new(stamp),
            config_ok: AtomicBool::new(config_ok),
            reload_failed: Mutex::new(None),
            seen: Mutex::new(None),
            handed_out: Mutex::new(BTreeSet::new()),
            firing: AtomicUsize::new(0),
            busy: Mutex::new(BTreeSet::new()),
            sleep: Arc::new(std::thread::sleep),
            _app_lock: lock,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_sleep(&mut self, f: Arc<dyn Fn(Duration) + Send + Sync>) {
        self.sleep = f;
    }

    pub fn is_busy(&self, id: &str) -> bool {
        self.busy.lock().expect("busy lock").contains(id)
    }

    fn begin_busy(self: &Arc<Self>, id: &str) -> crate::Result<Busy> {
        if !self.busy.lock().expect("busy lock").insert(id.to_string()) {
            bail!("busy: a check or fix already runs for this draft");
        }
        Ok(Busy {
            desk: Arc::clone(self),
            id: id.to_string(),
        })
    }

    /// The click on Send, with the item exactly as the window shows it. On success the countdown runs on its own
    /// thread; after [`UNDO`] it fires and hands the outcome to `done` (on that thread).
    pub fn schedule(
        self: &Arc<Self>,
        shown: &Item,
        confirmed: bool,
        done: impl FnOnce(FireResult) + Send + 'static,
    ) -> std::result::Result<Scheduled, ScheduleError> {
        let id = shown.item_id.clone();
        if self.is_busy(&id) {
            return Err(ScheduleError::Busy);
        }
        if !confirmed && let Some(c) = confirmation(shown) {
            return Err(ScheduleError::NeedsConfirmation(c));
        }
        let sender = self.sender();
        let token = sender
            .schedule(
                &id,
                &content_digest(shown),
                shown.content_revision,
                confirmed,
            )
            .map_err(|e| ScheduleError::Refused(e.0))?;
        let (me, t) = (Arc::clone(self), token.clone());
        let started = spawn("draftdesk-countdown", move || {
            (me.sleep)(UNDO);
            me.firing.fetch_add(1, Ordering::SeqCst);
            let r = sender.fire(&id, &t);
            me.firing.fetch_sub(1, Ordering::SeqCst);
            done(r);
        });
        if let Err(e) = started {
            let _ = self.sender().cancel(&shown.item_id, &token);
            return Err(ScheduleError::Refused(e.0));
        }
        Ok(Scheduled { token, undo: UNDO })
    }

    /// Undo during the countdown; `false`: too late.
    pub fn cancel(&self, id: &str, token: &str) -> crate::Result<bool> {
        self.sender().cancel(id, token)
    }

    /// Discards a `ready` or `failed` draft and cleans up its mailbox draft; on a background thread (network).
    /// Returns how the cleanup went (a failed one does not undo the discard).
    pub fn discard(&self, id: &str) -> crate::Result<CleanupResult> {
        if self.is_busy(id) {
            bail!("busy: a check or fix runs for this draft");
        }
        self.sender().discard(id)
    }

    /// Runs the check command on a background thread and stores the findings; `done` gets the saved item.
    pub fn check(
        self: &Arc<Self>,
        id: &str,
        done: impl FnOnce(crate::Result<Item>) + Send + 'static,
    ) -> crate::Result<()> {
        let sender = self.sender();
        let Some(cmd) = sender.store().config().check_command.clone() else {
            bail!("no_check_command: set up a check command in the settings");
        };
        let item = ready_item(&sender, id)?;
        let busy = self.begin_busy(id)?;
        let thread = sender.store().thread(id);
        let id = id.to_string();
        spawn("draftdesk-check", move || {
            let r = checks::run_check(&cmd, &item, thread.as_deref())
                .and_then(|c| record_checks(sender.store(), &id, c));
            drop(busy);
            done(r);
        })
    }

    /// Runs the fix command on a background thread; `done` gets the new text. Nothing is saved.
    pub fn fix(
        self: &Arc<Self>,
        id: &str,
        done: impl FnOnce(crate::Result<String>) + Send + 'static,
    ) -> crate::Result<()> {
        let sender = self.sender();
        let Some(cmd) = sender.store().config().fix_command.clone() else {
            bail!("no_fix_command: set up a fix command in the settings");
        };
        let item = ready_item(&sender, id)?;
        let findings = item
            .checks
            .as_ref()
            .map(|c| c.findings.clone())
            .unwrap_or_default();
        if findings.is_empty() {
            bail!("no findings to fix");
        }
        let busy = self.begin_busy(id)?;
        spawn("draftdesk-fix", move || {
            let r = checks::run_fix(&cmd, &item, &findings);
            drop(busy);
            done(r);
        })
    }

    /// Moves an invalid item out of the queue (it can never be sent). No result: there is no valid item to sign.
    /// Refused while `config.json` cannot be read: then every draft looks invalid.
    pub fn archive_invalid(&self, id: &str) -> crate::Result<()> {
        if !self.config_ok() {
            bail!(
                "config_invalid: config.json cannot be read; until it is repaired no draft can be judged"
            );
        }
        let sender = self.sender();
        match sender.store().read(id) {
            ItemRead::Valid(_) => bail!("only an invalid item goes to the archive this way"),
            ItemRead::Invalid { .. } => sender.store().archive(id).map(|_| ()),
        }
    }

    /// A countdown runs or a send waits for its outcome: the configuration is not reloaded then, and quitting waits.
    pub fn sending(&self) -> bool {
        self.sender().has_pending() || self.firing.load(Ordering::SeqCst) > 0
    }

    /// Picks up a changed `config.json`: a valid one replaces store and sender when no send is under way (else the
    /// next poll tries again); a broken one is reported once and the old configuration stays.
    fn reload_config(&self, notes: &mut Vec<Note>) {
        let stamp = config_stamp(&self.root);
        let mut last = self.config_stamp.lock().expect("stamp lock");
        if *last == stamp {
            return;
        }
        match Config::load(&self.root) {
            Err(e) => {
                *last = stamp;
                notes.push(Note::ConfigInvalid(e.0));
            }
            Ok(_) if self.sending() => {}
            Ok(cfg) => match Store::open(&self.root, cfg) {
                Ok(store) => {
                    *last = stamp;
                    self.config_ok.store(true, Ordering::SeqCst);
                    let sender = new_sender(store, &self.secrets, &self.transports, &self.clock);
                    *self.sender.write().expect("sender lock") = Arc::new(sender);
                }
                // The next poll tries again; told once for this version of the file.
                Err(e) => {
                    let mut told = self.reload_failed.lock().expect("reload lock");
                    if *told != Some(stamp) {
                        *told = Some(stamp);
                        notes.push(Note::Problem(String::new(), e.0));
                    }
                }
            },
        }
    }

    /// `config.json` was read (at the start or by a later poll).
    pub fn config_ok(&self) -> bool {
        self.config_ok.load(Ordering::SeqCst)
    }

    /// One look at the queue, on the main thread every second: fast, local, no network.
    // ponytail: reads every item each time (the queue is small); cache by mtime if it ever is not.
    pub fn poll(&self, now: SystemTime) -> Poll {
        let mut p = Poll::default();
        self.reload_config(&mut p.notes);
        let sender = self.sender();
        let store = sender.store();
        p.archived = store.archive_stale_discarded(now);
        let mut items = store.list();
        items.reverse();
        let view: BTreeMap<String, Option<u64>> = items
            .iter()
            .map(|r| {
                let rev = match r {
                    ItemRead::Valid(i) => Some(i.state_revision),
                    ItemRead::Invalid { .. } => None,
                };
                (read_id(r).to_string(), rev)
            })
            .collect();
        let mut seen = self.seen.lock().expect("seen lock");
        if let Some(prev) = seen.as_ref() {
            for r in &items {
                let id = read_id(r);
                match prev.get(id) {
                    None if matches!(r, ItemRead::Valid(i) if i.state == State::Ready) => {
                        p.new_ready.push(id.to_string())
                    }
                    None => p.changed.push(id.to_string()),
                    Some(rev) if *rev != view[id] => p.changed.push(id.to_string()),
                    Some(_) => {}
                }
            }
        }
        *seen = Some(view);
        let mut handed = self.handed_out.lock().expect("handed lock");
        for r in &items {
            if let ItemRead::Valid(i) = r
                && needs_cli_cleanup(i)
                && handed.insert(i.item_id.clone())
            {
                p.cleanup_due.push(i.item_id.clone());
            }
        }
        p.items = items;
        p
    }

    /// The mailbox cleanup of a command-line discard; on a background thread (network).
    pub fn cleanup_discarded(&self, id: &str) -> crate::Result<bool> {
        self.sender().cleanup_discarded(id)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The current sender (and through it the store and configuration). A reload replaces it; work already
    /// running keeps the one it started with.
    pub fn sender(&self) -> Arc<Sender> {
        self.sender.read().expect("sender lock").clone()
    }

    /// The start-up repair, on a background thread (keychain, mailbox): creates the signing key, cancels scheduled
    /// sends, turns interrupted sends into `unknown`, completes accepted ones, publishes missing results, cleans up the
    /// mailbox drafts of command-line discards, tidies the archive. Never sends.
    pub fn boot(&self) -> Vec<Note> {
        let mut notes = std::mem::take(&mut *self.open_notes.lock().expect("notes lock"));
        if let Err(e) = ensure_result_key(self.secrets.as_ref()) {
            notes.push(Note::NoSigningKey(e.0));
        }
        let sender = self.sender();
        let store = sender.store();
        let r = store.reconcile_on_start();
        notes.extend(r.unscheduled.into_iter().map(Note::Unscheduled));
        for id in r.needs_reconcile {
            let accepted = matches!(store.read(&id), ItemRead::Valid(i)
                if i.send.transport_outcome == Some(TransportOutcome::Accepted));
            notes.push(match sender.reconcile(&id) {
                Ok(()) if accepted => Note::Finalized(id),
                Ok(()) => Note::Interrupted(id),
                Err(e) => Note::Problem(id, e.0),
            });
        }
        for id in r.needs_finalize {
            notes.push(match sender.finalize_sent(&id) {
                Ok(()) => Note::Finalized(id),
                Err(e) => Note::Problem(id, e.0),
            });
        }
        for id in r.check_result {
            if let Err(e) = sender.reconcile(&id) {
                notes.push(Note::Problem(id.clone(), e.0));
            }
            match sender.cleanup_discarded(&id) {
                Ok(true) => notes.push(Note::CleanedUp(id)),
                Ok(false) => {}
                Err(e) => notes.push(Note::Problem(id, e.0)),
            }
        }
        notes.extend(r.errors.into_iter().map(|(id, e)| Note::Problem(id, e)));
        if let Err(e) = store.housekeeping(self.clock.wall()) {
            notes.push(Note::Problem(String::new(), e.0));
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::add::{ReplaceOutcome, add, withdraw};
    use crate::item::{Checks, Draft};
    use crate::results::{self, KEY_NAME, ResultStatus};
    use crate::secrets::MemorySecrets;
    use crate::sender::tests::{FakeClock, FakeTransport, Shared};
    use crate::store::tests::{TempRoot, temp_root};
    use crate::transport::CleanupResult;
    use std::sync::mpsc;
    use std::time::Instant;

    const CFG: &str = r#"{"accounts":[{"name":"work","kind":"command",
        "senders":[{"address":"me@example.org","name":"Me"}],"send_command":["/bin/cat"]}]}"#;

    struct Env {
        root: TempRoot,
        desk: Arc<Desk>,
        shared: Arc<Shared>,
        secrets: Arc<MemorySecrets>,
        clock: Arc<FakeClock>,
    }

    fn open(
        root: &TempRoot,
        shared: &Arc<Shared>,
        secrets: &Arc<MemorySecrets>,
        clock: &Arc<FakeClock>,
    ) -> std::result::Result<Desk, OpenError> {
        let s = shared.clone();
        let transports: SharedTransports =
            Arc::new(move |_, _| Some(Box::new(FakeTransport(s.clone())) as Box<dyn Transport>));
        Desk::open(&root.0, secrets.clone(), transports, clock.clone())
    }

    /// `gate`: the countdown waits until the test sends on the returned channel; otherwise it ends at once. Both
    /// move the fake clock by the countdown, so the approval is fresh.
    fn env_full(name: &str, config: &str, gate: bool) -> (Env, mpsc::Sender<()>) {
        let root = temp_root(name);
        std::fs::create_dir_all(&root.0).unwrap();
        std::fs::write(root.0.join(CONFIG_FILE), config).unwrap();
        let shared = Arc::new(Shared::default());
        let secrets = Arc::new(MemorySecrets::default());
        let clock = Arc::new(FakeClock(Mutex::new((SystemTime::now(), Instant::now()))));
        let mut desk = open(&root, &shared, &secrets, &clock).unwrap();
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        let c = clock.clone();
        desk.set_sleep(Arc::new(move |d| {
            if gate {
                let _ = rx.lock().unwrap().recv();
            }
            c.advance(d);
        }));
        let env = Env {
            root,
            desk: Arc::new(desk),
            shared,
            secrets,
            clock,
        };
        (env, tx)
    }

    fn env_with(name: &str, config: &str) -> Env {
        env_full(name, config, false).0
    }

    fn env(name: &str) -> Env {
        env_with(name, CFG)
    }

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/bin/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn cfg_with(check: Option<(&str, u64)>, fix: Option<&str>) -> String {
        let mut v: serde_json::Value = serde_json::from_str(CFG).unwrap();
        if let Some((c, t)) = check {
            v["check_command"] = serde_json::json!({"argv": [fixture(c)], "timeout_s": t});
        }
        if let Some(f) = fix {
            v["fix_command"] = serde_json::json!({"argv": [fixture(f)], "timeout_s": 10});
        }
        v.to_string()
    }

    fn wait<T>(rx: &mpsc::Receiver<T>) -> T {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("done in time")
    }

    fn finding(level: Level, confirm: bool, title: &str) -> Finding {
        Finding {
            level,
            title: title.into(),
            detail: String::new(),
            quote: String::new(),
            confirm,
        }
    }

    fn checks_for(it: &Item, verdict: Verdict, findings: Vec<Finding>) -> Checks {
        Checks {
            body_sha256: it.body_sha256.clone(),
            verdict,
            findings,
            ran_at: None,
            source: None,
            content_digest: None,
        }
    }

    #[test]
    fn confirmation_lists_findings_and_stale() {
        let mut it = crate::item::tests::item();
        it.checks = Some(checks_for(
            &it,
            Verdict::Warn,
            vec![
                finding(Level::Info, false, "i"),
                finding(Level::Warn, true, "confirm me"),
                finding(Level::Block, false, "b"),
                finding(Level::Warn, false, "w"),
            ],
        ));
        let c = confirmation(&it).unwrap();
        let titles: Vec<&str> = c.findings.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(titles, vec!["confirm me", "b"]);
        assert!(!c.verdict_block && !c.stale);
        it.body = "Changed.\n".into();
        it.body_sha256 = crate::sha256_hex(it.body.as_bytes());
        assert!(confirmation(&it).unwrap().stale);
        it.checks = Some(checks_for(
            &it,
            Verdict::Warn,
            vec![finding(Level::Warn, false, "w")],
        ));
        assert_eq!(confirmation(&it), None);
        it.checks = Some(checks_for(&it, Verdict::Block, vec![]));
        let c = confirmation(&it).unwrap();
        assert!(c.verdict_block && c.findings.is_empty());
        it.checks = None;
        assert_eq!(confirmation(&it), None);
    }

    #[test]
    fn schedule_needs_confirmation_then_confirmed_sends() {
        let e = env("ac1");
        // As in the app: the start creates the signing key; without it a sent item stays until its result is out.
        assert!(e.desk.boot().is_empty());
        let id = e.add();
        e.mutate(&id, |i| {
            i.checks = Some(checks_for(
                i,
                Verdict::Block,
                vec![finding(Level::Block, false, "b")],
            ));
        });
        let shown = e.item(&id);
        let refused = e.desk.schedule(&shown, false, |_| {});
        assert!(
            matches!(&refused, Err(ScheduleError::NeedsConfirmation(c)) if c.verdict_block),
            "{refused:?}"
        );
        let (tx, rx) = mpsc::channel();
        let s = e
            .desk
            .schedule(&shown, true, move |r| {
                let _ = tx.send(r);
            })
            .unwrap();
        assert_eq!(s.undo, UNDO);
        assert_eq!(wait(&rx), FireResult::Sent);
        assert_eq!(e.sends(), 1);
        assert!(e.archived(&id));
    }

    #[test]
    fn cancel_prevents_send_and_sending_tracks_the_countdown() {
        let (e, gate) = env_full("ac2", CFG, true);
        let id = e.add();
        let shown = e.item(&id);
        let (tx, rx) = mpsc::channel();
        let s = e
            .desk
            .schedule(&shown, false, move |r| {
                let _ = tx.send(r);
            })
            .unwrap();
        assert!(e.desk.sending());
        assert!(e.desk.cancel(&id, &s.token).unwrap());
        gate.send(()).unwrap();
        assert!(matches!(wait(&rx), FireResult::NotSent(_)));
        assert_eq!(e.sends(), 0);
        assert_eq!(e.item(&id).state, State::Ready);
        assert!(!e.desk.sending());
    }

    #[test]
    fn sending_until_the_outcome() {
        let (e, gate) = env_full("ac3", CFG, true);
        let id = e.add();
        let (tx, rx) = mpsc::channel();
        e.desk
            .schedule(&e.item(&id), false, move |r| {
                let _ = tx.send(r);
            })
            .unwrap();
        assert!(e.desk.sending());
        gate.send(()).unwrap();
        assert_eq!(wait(&rx), FireResult::Sent);
        assert!(!e.desk.sending());
    }

    #[test]
    fn schedule_and_discard_refused_while_check_runs() {
        let e = env_with("ac4", &cfg_with(Some(("sleep.sh", 1)), None));
        let id = e.add();
        let (tx, rx) = mpsc::channel();
        e.desk
            .check(&id, move |r| {
                let _ = tx.send(r);
            })
            .unwrap();
        assert!(e.desk.is_busy(&id));
        let shown = e.item(&id);
        assert_eq!(
            e.desk.schedule(&shown, false, |_| {}),
            Err(ScheduleError::Busy)
        );
        assert!(e.desk.discard(&id).is_err());
        let again = e.desk.check(&id, |_| {});
        assert!(again.unwrap_err().0.contains("busy"));
        let err = wait(&rx).unwrap_err();
        assert!(err.0.contains("check_timeout"), "{}", err.0);
        assert!(!e.desk.is_busy(&id));
    }

    #[test]
    fn check_records_findings_and_frees_busy() {
        let e = env_with("ac5", &cfg_with(Some(("check-warn.sh", 10)), None));
        let id = e.add();
        let (tx, rx) = mpsc::channel();
        e.desk
            .check(&id, move |r| {
                let _ = tx.send(r);
            })
            .unwrap();
        let it = wait(&rx).unwrap();
        let c = it.checks.unwrap();
        assert_eq!(
            (c.verdict, c.source),
            (Verdict::Warn, Some(crate::item::CheckSource::Command))
        );
        assert!(!e.desk.is_busy(&id));
        assert!(e.item(&id).checks.is_some(), "stored");

        let e = env_with("ac6", &cfg_with(Some(("check-exit3.sh", 10)), None));
        let id = e.add();
        let (tx, rx) = mpsc::channel();
        e.desk
            .check(&id, move |r| {
                let _ = tx.send(r);
            })
            .unwrap();
        assert!(wait(&rx).unwrap_err().0.contains("check_failed"));
        assert!(!e.desk.is_busy(&id));
        assert!(e.item(&id).checks.is_none());
    }

    #[test]
    fn fix_returns_a_body_and_saves_nothing() {
        let e = env_with("ac7", &cfg_with(None, Some("fix.sh")));
        let id = e.add();
        e.mutate(&id, |i| {
            i.checks = Some(checks_for(
                i,
                Verdict::Warn,
                vec![finding(Level::Warn, false, "subject Re: Offer")],
            ));
        });
        let before = e.item(&id);
        let (tx, rx) = mpsc::channel();
        e.desk
            .fix(&id, move |r| {
                let _ = tx.send(r);
            })
            .unwrap();
        assert_eq!(wait(&rx).unwrap(), "Fixed body for: subject Re: Offer");
        assert_eq!(e.item(&id).body, before.body);
        assert!(!e.desk.is_busy(&id));
    }

    #[test]
    fn check_and_fix_need_their_command_and_findings() {
        let e = env("ac8");
        let id = e.add();
        assert!(
            e.desk
                .check(&id, |_| {})
                .unwrap_err()
                .0
                .starts_with("no_check_command")
        );
        assert!(
            e.desk
                .fix(&id, |_| {})
                .unwrap_err()
                .0
                .starts_with("no_fix_command")
        );
        let e = env_with("ac9", &cfg_with(None, Some("fix.sh")));
        let id = e.add();
        assert!(
            e.desk
                .fix(&id, |_| {})
                .unwrap_err()
                .0
                .contains("no findings")
        );
    }

    #[test]
    fn archive_invalid_only_for_invalid() {
        let e = env("ac10");
        let good = e.add();
        assert!(e.desk.archive_invalid(&good).is_err());
        let bad = "20261107-120000-deadbeef";
        let dir = e.desk.sender().store().item_dir(bad).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("item.json"), "{broken").unwrap();
        e.desk.archive_invalid(bad).unwrap();
        assert!(!dir.exists());
        assert!(e.archived(bad));
    }

    #[test]
    fn actions_matrix() {
        let cfg_plain = Config::parse(CFG).unwrap();
        let cfg_cmds =
            Config::parse(&cfg_with(Some(("check-warn.sh", 10)), Some("fix.sh"))).unwrap();
        let mut it = crate::item::tests::item();
        let valid = |i: &Item| ItemRead::Valid(Box::new(i.clone()));
        let a = actions(&valid(&it), &cfg_plain, false, false);
        assert_eq!(
            a,
            Actions {
                discard: true,
                send: true,
                ..Actions::default()
            }
        );
        let a = actions(&valid(&it), &cfg_cmds, false, false);
        assert!(a.check && !a.fix, "fix needs findings");
        it.checks = Some(checks_for(
            &it,
            Verdict::Warn,
            vec![finding(Level::Warn, false, "w")],
        ));
        assert!(actions(&valid(&it), &cfg_cmds, false, false).fix);
        assert_eq!(
            actions(&valid(&it), &cfg_cmds, true, false),
            Actions::default()
        );
        assert_eq!(
            actions(&valid(&it), &cfg_cmds, false, true),
            Actions::default()
        );
        it.state = State::Failed;
        assert_eq!(
            actions(&valid(&it), &cfg_cmds, false, false),
            Actions {
                discard: true,
                ..Actions::default()
            }
        );
        it.state = State::Sending;
        assert_eq!(
            actions(&valid(&it), &cfg_cmds, false, false),
            Actions::default()
        );
        let invalid = ItemRead::Invalid {
            item_id: "x".into(),
            errors: vec![],
        };
        assert_eq!(
            actions(&invalid, &cfg_cmds, false, false),
            Actions {
                archive: true,
                ..Actions::default()
            }
        );
    }

    fn draft() -> Draft {
        serde_json::from_value(serde_json::json!({
            "schema": "draftdesk/1", "account": "work", "from": {"address": "me@example.org"},
            "to": ["Ann <ann@example.org>"], "subject": "Re: Offer", "body": "Hello Ann,\n\nthanks.\n",
            "mailbox_draft": {"message_id": "<d.1@example.org>"},
            "origin": {"agent": "test", "session": "s1"}
        }))
        .unwrap()
    }

    impl Env {
        fn add(&self) -> String {
            let s = self.desk.sender();
            add(s.store(), draft(), None, chrono::Local::now())
                .unwrap()
                .item_id
        }
        fn item(&self, id: &str) -> Item {
            match self.desk.sender().store().read(id) {
                ItemRead::Valid(i) => *i,
                ItemRead::Invalid { errors, .. } => panic!("{errors:?}"),
            }
        }
        fn mutate(&self, id: &str, f: impl FnOnce(&mut Item)) {
            self.desk
                .sender()
                .store()
                .mutate(id, &[], |i| {
                    f(i);
                    Ok(())
                })
                .unwrap();
        }
        /// `ready` -> `sending_scheduled` on disk only, as a crashed app would leave it.
        fn schedule_on_disk(&self, id: &str) -> String {
            let it = self.item(id);
            self.desk
                .sender()
                .store()
                .schedule_send(id, &content_digest(&it), it.content_revision, false)
                .unwrap()
                .unwrap()
        }
        fn sends(&self) -> usize {
            self.shared
                .log
                .lock()
                .unwrap()
                .iter()
                .filter(|l| l.starts_with("send "))
                .count()
        }
        fn archived(&self, id: &str) -> bool {
            let month = chrono::Utc::now().format("%Y-%m").to_string();
            self.desk
                .sender()
                .store()
                .dir()
                .join("archive")
                .join(month)
                .join(id)
                .is_dir()
        }
    }

    const CFG_TWO: &str = r#"{"accounts":[{"name":"work","kind":"command",
        "senders":[{"address":"me@example.org","name":"Me"}],"send_command":["/bin/cat"]},
        {"name":"two","kind":"command","senders":[{"address":"two@example.org","name":"Two"}],
        "send_command":["/bin/cat"]}]}"#;

    fn has_two(e: &Env) -> bool {
        e.desk.sender().store().config().account("two").is_some()
    }

    #[test]
    fn first_poll_is_baseline() {
        let e = env("pl1");
        let id = e.add();
        let p = e.desk.poll(SystemTime::now());
        assert!(p.new_ready.is_empty() && p.changed.is_empty());
        assert_eq!(p.items.len(), 1);
        assert_eq!(read_id(&p.items[0]), id);
    }

    #[test]
    fn new_ready_reported_once_newest_first() {
        let e = env("pl2");
        let first = e.add();
        e.desk.poll(SystemTime::now());
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = e.add();
        let p = e.desk.poll(SystemTime::now());
        assert_eq!(p.new_ready, vec![second.clone()]);
        assert_eq!(
            p.items.iter().map(read_id).collect::<Vec<_>>(),
            vec![second.as_str(), first.as_str()]
        );
        assert!(e.desk.poll(SystemTime::now()).new_ready.is_empty());
    }

    #[test]
    fn state_change_reported_as_changed() {
        let e = env("pl3");
        let id = e.add();
        e.desk.poll(SystemTime::now());
        e.mutate(&id, |i| i.state = State::Failed);
        let p = e.desk.poll(SystemTime::now());
        assert_eq!((p.changed, p.new_ready), (vec![id], vec![]));
    }

    #[test]
    fn config_change_reloads_when_idle() {
        let e = env("pl4");
        e.desk.poll(SystemTime::now());
        std::fs::write(e.root.0.join(CONFIG_FILE), CFG_TWO).unwrap();
        let p = e.desk.poll(SystemTime::now());
        assert!(p.notes.is_empty(), "{:?}", p.notes);
        assert!(has_two(&e));
    }

    #[test]
    fn config_change_waits_for_countdown() {
        let e = env("pl5");
        let id = e.add();
        let it = e.item(&id);
        let token = e
            .desk
            .sender()
            .schedule(&id, &content_digest(&it), it.content_revision, false)
            .unwrap();
        assert!(e.desk.sending());
        std::fs::write(e.root.0.join(CONFIG_FILE), CFG_TWO).unwrap();
        e.desk.poll(SystemTime::now());
        assert!(!has_two(&e), "no reload during a countdown");
        assert!(e.desk.sender().cancel(&id, &token).unwrap());
        e.desk.poll(SystemTime::now());
        assert!(has_two(&e));
    }

    #[test]
    fn broken_config_noted_once_and_old_kept() {
        let e = env("pl6");
        std::fs::write(e.root.0.join(CONFIG_FILE), "garbage").unwrap();
        let p = e.desk.poll(SystemTime::now());
        assert!(
            matches!(p.notes.as_slice(), [Note::ConfigInvalid(_)]),
            "{:?}",
            p.notes
        );
        assert!(e.desk.poll(SystemTime::now()).notes.is_empty());
        assert!(e.desk.sender().store().config().account("work").is_some());
    }

    #[test]
    fn cli_discard_handed_out_once_and_kept_from_the_archive() {
        let e = env("pl7");
        let id = e.add();
        withdraw(e.desk.sender().store(), &id, None, "discarded_by_cli", "").unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(3600);
        let p = e.desk.poll(later);
        assert_eq!(p.cleanup_due, vec![id.clone()]);
        assert!(p.archived.is_empty(), "waits for its cleanup");
        assert!(e.desk.poll(later).cleanup_due.is_empty(), "handed out once");
        assert!(e.desk.cleanup_discarded(&id).unwrap());
        let p = e
            .desk
            .poll(SystemTime::now() + std::time::Duration::from_secs(7200));
        assert_eq!(p.archived, vec![id]);
    }

    #[test]
    fn old_discarded_archived() {
        let e = env("pl8");
        let id = e.add();
        // A window discard archives at once; a draft its agent replaced waits for the sweep.
        withdraw(e.desk.sender().store(), &id, None, "superseded", "new").unwrap();
        let p = e
            .desk
            .poll(SystemTime::now() + std::time::Duration::from_secs(3600));
        assert_eq!(p.archived, vec![id]);
        assert!(p.items.is_empty());
    }

    #[test]
    fn second_open_is_already_running() {
        let e = env("dk1");
        let again = open(&e.root, &e.shared, &e.secrets, &e.clock);
        assert!(matches!(again, Err(OpenError::AlreadyRunning)));
        drop(e.desk);
        assert!(open(&e.root, &e.shared, &e.secrets, &e.clock).is_ok());
    }

    #[test]
    fn broken_config_opens_with_note() {
        let e = env_with("dk2", "not json");
        let notes = e.desk.boot();
        assert!(
            matches!(notes.as_slice(), [Note::ConfigInvalid(_)]),
            "{notes:?}"
        );
        assert!(e.desk.sender().store().config().accounts.is_empty());
        assert!(e.desk.boot().is_empty(), "reported once");
    }

    #[test]
    fn boot_creates_signing_key() {
        let e = env("dk3");
        assert_eq!(e.secrets.get(KEY_NAME).unwrap(), None);
        assert!(e.desk.boot().is_empty());
        assert!(e.secrets.get(KEY_NAME).unwrap().is_some());
    }

    #[test]
    fn boot_unschedules_scheduled() {
        let e = env("dk4");
        let id = e.add();
        e.schedule_on_disk(&id);
        assert_eq!(e.desk.boot(), vec![Note::Unscheduled(id.clone())]);
        assert_eq!(e.item(&id).state, State::Ready);
        assert_eq!(e.sends(), 0);
    }

    #[test]
    fn boot_interrupted_send_becomes_unknown_and_is_never_resent() {
        let e = env("dk5");
        let id = e.add();
        let token = e.schedule_on_disk(&id);
        let digest = content_digest(&e.item(&id));
        assert!(
            e.desk
                .sender()
                .store()
                .claim_send(&id, &token, &digest)
                .unwrap()
        );
        assert_eq!(e.desk.boot(), vec![Note::Interrupted(id.clone())]);
        let it = e.item(&id);
        assert_eq!(it.state, State::Failed);
        assert_eq!(it.send.transport_outcome, Some(TransportOutcome::Unknown));
        assert_eq!(e.sends(), 0, "never sent again");
        let r = results::read_verified(e.desk.sender().store(), e.secrets.as_ref(), &id).unwrap();
        assert_eq!(r.unwrap().status, ResultStatus::Unknown);
    }

    #[test]
    fn boot_finalizes_sent() {
        let e = env("dk6");
        let accepted = e.add();
        let sent = e.add();
        for id in [&accepted, &sent] {
            e.mutate(id, |i| {
                i.state = State::Sending;
                i.send.transport_outcome = Some(TransportOutcome::Accepted);
            });
        }
        e.mutate(&sent, |i| i.state = State::Sent);
        let notes = e.desk.boot();
        assert!(
            notes.contains(&Note::Finalized(accepted.clone())),
            "{notes:?}"
        );
        assert!(notes.contains(&Note::Finalized(sent.clone())), "{notes:?}");
        assert!(e.archived(&accepted) && e.archived(&sent));
        assert_eq!(e.sends(), 0);
    }

    #[test]
    fn boot_publishes_missing_failed_result() {
        let e = env("dk7");
        let id = e.add();
        e.mutate(&id, |i| {
            i.state = State::Failed;
            i.send.transport_outcome = Some(TransportOutcome::Rejected);
            i.send.error_code = Some("smtp_5xx".into());
        });
        assert!(e.desk.boot().is_empty());
        let r = results::read_verified(e.desk.sender().store(), e.secrets.as_ref(), &id).unwrap();
        let r = r.unwrap();
        assert_eq!((r.status, r.retry_allowed), (ResultStatus::Failed, true));
    }

    #[test]
    fn boot_cleans_up_cli_discard() {
        let e = env("dk8");
        let id = e.add();
        let out = withdraw(e.desk.sender().store(), &id, None, "discarded_by_cli", "").unwrap();
        assert_eq!(out, ReplaceOutcome::Withdrawn(id.clone()));
        assert_eq!(e.desk.boot(), vec![Note::CleanedUp(id.clone())]);
        assert_eq!(*e.shared.log.lock().unwrap(), vec!["cleanup discarded"]);
        assert!(e.desk.boot().is_empty(), "once");
    }

    #[test]
    fn failed_mailbox_cleanup_is_a_problem_not_cleaned_up() {
        let e = env("dk10");
        let (a, b) = (e.add(), e.add());
        for id in [&a, &b] {
            withdraw(e.desk.sender().store(), id, None, "discarded_by_cli", "").unwrap();
        }
        *e.shared.cleanup.lock().unwrap() = Some(CleanupResult::Failed("imap down".into()));
        // The poll's way: the error goes to the window.
        let err = e.desk.cleanup_discarded(&b).unwrap_err();
        assert!(err.0.contains("imap down"), "{}", err.0);
        // The start's way: a problem, not "cleaned up".
        let notes = e.desk.boot();
        assert!(
            matches!(notes.as_slice(), [Note::Problem(id, d)] if *id == a && d.contains("imap down")),
            "{notes:?}"
        );
        // Nothing to clean up is no "cleaned up" either.
        let c = e.add();
        withdraw(e.desk.sender().store(), &c, None, "discarded_by_cli", "").unwrap();
        *e.shared.cleanup.lock().unwrap() = Some(CleanupResult::Skipped("no mailbox draft".into()));
        assert!(!e.desk.cleanup_discarded(&c).unwrap());
    }

    #[test]
    fn archive_refused_while_config_is_broken() {
        let Env {
            root,
            desk,
            shared,
            secrets,
            clock,
        } = env("dk11");
        let id = add(desk.sender().store(), draft(), None, chrono::Local::now())
            .unwrap()
            .item_id;
        drop(desk);
        // A broken config.json at the start: every draft looks invalid (its account is unknown).
        std::fs::write(root.0.join(CONFIG_FILE), "garbage").unwrap();
        let desk = open(&root, &shared, &secrets, &clock).unwrap();
        assert!(matches!(
            desk.sender().store().read(&id),
            ItemRead::Invalid { .. }
        ));
        assert!(!desk.config_ok());
        let err = desk.archive_invalid(&id).unwrap_err();
        assert!(err.0.starts_with("config_invalid"), "{}", err.0);
        assert!(
            desk.sender().store().item_dir(&id).unwrap().is_dir(),
            "still queued"
        );
        // Repaired: the draft is valid again.
        std::fs::write(root.0.join(CONFIG_FILE), CFG).unwrap();
        desk.poll(SystemTime::now());
        assert!(desk.config_ok());
        assert!(matches!(
            desk.sender().store().read(&id),
            ItemRead::Valid(_)
        ));
    }

    #[test]
    fn reload_retried_after_a_failed_store_open_and_noted_once() {
        let e = env("pl9");
        e.desk.poll(SystemTime::now());
        // The store cannot be opened for a moment (here: its audit file is a folder).
        let audit = e.desk.sender().store().dir().join("audit.jsonl");
        std::fs::remove_file(&audit).unwrap();
        std::fs::create_dir(&audit).unwrap();
        std::fs::write(e.root.0.join(CONFIG_FILE), CFG_TWO).unwrap();
        let p = e.desk.poll(SystemTime::now());
        assert!(
            matches!(p.notes.as_slice(), [Note::Problem(..)]),
            "{:?}",
            p.notes
        );
        assert!(e.desk.poll(SystemTime::now()).notes.is_empty(), "told once");
        assert!(!has_two(&e));
        std::fs::remove_dir(&audit).unwrap();
        e.desk.poll(SystemTime::now());
        assert!(has_two(&e), "the new configuration once the store opens");
    }

    #[test]
    fn a_chmod_that_makes_the_config_readable_is_picked_up() {
        use std::os::unix::fs::PermissionsExt;
        let Env {
            root,
            desk,
            shared,
            secrets,
            clock,
        } = env("pl10");
        let path = root.0.join(CONFIG_FILE);
        let mode = |m| {
            // Apart from the change time nothing differs; some file systems only tick every few milliseconds.
            std::thread::sleep(Duration::from_millis(50));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(m)).unwrap()
        };
        // A poll finds the new file unreadable, then the person repairs its permissions.
        std::fs::write(&path, CFG_TWO).unwrap();
        mode(0o000);
        let p = desk.poll(SystemTime::now());
        assert!(
            matches!(p.notes.as_slice(), [Note::ConfigInvalid(_)]),
            "{:?}",
            p.notes
        );
        mode(0o600);
        desk.poll(SystemTime::now());
        assert!(
            desk.sender().store().config().account("two").is_some(),
            "loaded after the chmod"
        );
        // The same when the app started with the unreadable file.
        drop(desk);
        mode(0o000);
        let desk = open(&root, &shared, &secrets, &clock).unwrap();
        assert!(!desk.config_ok());
        mode(0o600);
        desk.poll(SystemTime::now());
        assert!(desk.config_ok());
    }

    #[test]
    fn check_findings_go_stale_on_a_recipient_change() {
        let e = env_with("ac11", &cfg_with(Some(("check-warn.sh", 10)), None));
        let id = e.add();
        let (tx, rx) = mpsc::channel();
        e.desk
            .check(&id, move |r| {
                let _ = tx.send(r);
            })
            .unwrap();
        let checked = wait(&rx).unwrap();
        assert!(!confirmation(&checked).unwrap().stale);
        let edit = crate::edit::Edit {
            to: Some(vec!["bob@example.org".into()]),
            ..Default::default()
        };
        let s = e.desk.sender();
        let it = crate::edit::save_edits(s.store(), &id, checked.content_revision, &edit).unwrap();
        assert!(
            confirmation(&it).unwrap().stale,
            "made for other recipients"
        );
    }

    #[test]
    fn boot_isolates_item_errors() {
        let e = env("dk9");
        let good = e.add();
        e.schedule_on_disk(&good);
        let bad = "20261107-120000-deadbeef";
        let dir = e.desk.sender().store().item_dir(bad).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("item.json"), "{broken").unwrap();
        let notes = e.desk.boot();
        assert!(notes.contains(&Note::Unscheduled(good)), "{notes:?}");
        assert!(
            notes
                .iter()
                .any(|n| matches!(n, Note::Problem(id, _) if id == bad)),
            "{notes:?}"
        );
    }
}
