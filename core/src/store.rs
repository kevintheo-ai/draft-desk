//! The queue on disk: one folder per item, atomic writes, a lock per item, an audit log without contents, and the
//! archive.
//!
//! Layout under the Draft Desk root: `state/queue/<id>/`, `state/archive/YYYY-MM/<id>/` (UTC month),
//! `state/results/`, `state/locks/item-<id>.lock`, `state/audit.jsonl`. Folders 0700, files 0600.

use crate::config::Config;
use crate::item::{
    HistoryEntry, ITEM_FILE, ITEM_ID_RE, Item, ItemRead, Origin, State, TransportOutcome,
    content_digest, load_item, validate_item,
};
use crate::{Error, Result, bail, sha256_hex};
use serde::Serialize;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const HISTORY_CAP: usize = 200;
/// Discarded items stay visible this long, then move to the archive.
pub const DISCARDED_GRACE: Duration = Duration::from_secs(600);
// ponytail: retention periods are fixed, no configuration.
const ARCHIVE_RETENTION: Duration = Duration::from_secs(90 * 86_400);
const DELIVERED_RETENTION: Duration = Duration::from_secs(7 * 86_400);

pub struct Store {
    dir: PathBuf,
    cfg: Config,
}

/// What the app has to do after a restart.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconcile {
    /// Were scheduled, now `ready` again: a restart never sends.
    pub unscheduled: Vec<String>,
    /// Were sending when the app stopped: the outcome is unknown.
    pub needs_reconcile: Vec<String>,
    /// Sent, but the follow-up steps (result, archive) may be missing.
    pub needs_finalize: Vec<String>,
    /// Failed or discarded: the result may be missing if the app stopped right after the state change.
    pub check_result: Vec<String>,
    pub errors: Vec<(String, String)>,
}

/// One line of `audit.jsonl`: ids, codes, counts and short hashes. Never addresses, subject, attachment names or body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AuditLine {
    pub ts: String,
    pub event: String,
    pub item_id: String,
    pub account: String,
    pub origin: Option<Origin>,
    pub edited: bool,
    pub attachments: Vec<String>,
    pub rcpt_count: usize,
    pub rcpt_hashes: Vec<String>,
    pub subject_hash: String,
    pub message_id: Option<String>,
    pub outcome: Option<String>,
    pub provider_message_id: Option<String>,
    pub error_code: Option<String>,
}

impl AuditLine {
    pub fn for_item(event: &str, item: &Item) -> AuditLine {
        let rcpts: Vec<&String> = item.to.iter().chain(&item.cc).chain(&item.bcc).collect();
        AuditLine {
            ts: crate::now_utc(),
            event: event.into(),
            item_id: item.item_id.clone(),
            account: item.account.clone(),
            origin: item.origin.clone(),
            edited: item.edits.edited,
            // File names say what a mail is about, like the subject.
            attachments: item
                .attachments
                .iter()
                .map(|a| short_hash(&a.name))
                .collect(),
            rcpt_count: rcpts.len(),
            rcpt_hashes: rcpts
                .iter()
                .map(|r| short_hash(&crate::address::bare(r)))
                .collect(),
            subject_hash: short_hash(&item.subject),
            message_id: item.send.message_id.clone(),
            outcome: None,
            provider_message_id: item.send.provider_message_id.clone(),
            error_code: item.send.error_code.clone(),
        }
    }
}

fn short_hash(s: &str) -> String {
    sha256_hex(s.as_bytes())[..16].to_string()
}

/// Creates `d` (0700) or checks the one that is there. A symlink in its place is refused: writes and the chmod would
/// land wherever it points.
pub(crate) fn mkdir_private(d: &Path) -> Result<()> {
    fs::create_dir_all(d)?;
    if !fs::symlink_metadata(d)?.is_dir() {
        bail!("{}: not a folder (a symlink?)", d.display());
    }
    fs::set_permissions(d, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// `audit.jsonl` for appending: no symlink, no wait for a FIFO's reader (O_NONBLOCK), a regular file.
fn open_audit(path: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !f.metadata()?.file_type().is_file() {
        bail!("{}: not a regular file", path.display());
    }
    Ok(f)
}

pub(crate) fn fsync_dir(d: &Path) -> Result<()> {
    File::open(d)?.sync_all()?;
    Ok(())
}

/// Checks and reads through one descriptor: no symlink, no FIFO wait (O_NONBLOCK), a regular file, at most `cap`
/// bytes (also when the file grows while it is read).
pub(crate) fn read_regular(path: &Path, cap: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = f.metadata()?;
    if !meta.file_type().is_file() {
        bail!("not a regular file");
    }
    if meta.len() > cap {
        bail!("too large: {} > {cap} bytes", meta.len());
    }
    let mut bytes = Vec::new();
    f.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        bail!("too large: grew beyond {cap} bytes while reading");
    }
    Ok(bytes)
}

/// The item as written to disk; refuses anything the reader would reject as too large.
pub(crate) fn item_json(item: &Item) -> Result<String> {
    let mut json = serde_json::to_string_pretty(item)?;
    json.push('\n');
    if json.len() as u64 > crate::item::MAX_ITEM_BYTES {
        bail!(
            "item too large: {} > {} bytes",
            json.len(),
            crate::item::MAX_ITEM_BYTES
        );
    }
    Ok(json)
}

/// A folder, opened without following a symlink.
pub(crate) fn open_dir_nofollow(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)?)
}

/// Creates `name` inside `dir` (never over an existing entry, never through a symlink, 0600), writes and syncs it.
pub(crate) fn write_new_at(dir: &File, name: &str, bytes: &[u8]) -> Result<()> {
    let c_name = CString::new(name).map_err(|_| Error(format!("{name:?}: NUL")))?;
    // SAFETY: a valid directory descriptor and a NUL-terminated name; the returned descriptor is owned below.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        bail!(
            "{name:?}: cannot create: {}",
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: `fd` was just opened and is owned by nobody else.
    let mut f = unsafe { File::from_raw_fd(fd) };
    if let Err(e) = f.write_all(bytes).and_then(|()| f.sync_all()) {
        unlink_at(dir, name);
        return Err(e.into());
    }
    Ok(())
}

/// Removes `name` from `dir`; best effort.
pub(crate) fn unlink_at(dir: &File, name: &str) {
    if let Ok(c_name) = CString::new(name) {
        // SAFETY: a valid directory descriptor and a NUL-terminated name.
        unsafe { libc::unlinkat(dir.as_raw_fd(), c_name.as_ptr(), 0) };
    }
}

/// Discarded on the command line, and the mailbox draft is not cleaned up yet: only the app can do that.
pub fn needs_cli_cleanup(item: &Item) -> bool {
    item.state == State::Discarded
        && item
            .history
            .iter()
            .rposition(|h| h.event == "discarded_by_cli")
            .is_some_and(|i| {
                !item.history[i + 1..]
                    .iter()
                    .any(|h| h.event == "draft_cleanup")
            })
}

/// Writes `dir/name` atomically: a private temp file in the same folder, fsync, rename, fsync of the folder.
pub(crate) fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let tmp = dir.join(format!(".{name}.tmp-{}", crate::random_hex(8)));
    let result = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, dir.join(name))?;
        fsync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

impl Store {
    /// Opens the store under the Draft Desk root and creates the layout with private permissions.
    pub fn open(root: &Path, cfg: Config) -> Result<Store> {
        let dir = root.join("state");
        for d in [
            root.to_path_buf(),
            dir.clone(),
            dir.join("queue"),
            dir.join("archive"),
            dir.join("results"),
            dir.join("locks"),
        ] {
            mkdir_private(&d)?;
        }
        open_audit(&dir.join("audit.jsonl"))?.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(Store { dir, cfg })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn item_dir(&self, id: &str) -> Result<PathBuf> {
        if !ITEM_ID_RE.is_match(id) {
            bail!("invalid item id: {id:?}");
        }
        Ok(self.dir.join("queue").join(id))
    }

    /// The item from the queue, or else from the archive (read only: archived items are never sent).
    pub fn read_any(&self, id: &str) -> ItemRead {
        let queued = self.read(id);
        // A refused id (a path, say) never reaches the archive search either: joined onto it, a path would lead out.
        if matches!(queued, ItemRead::Valid(_))
            || self.item_dir(id).is_ok_and(|d| d.exists())
            || self.item_dir(id).is_err()
        {
            return queued;
        }
        let found = fs::read_dir(self.dir.join("archive"))
            .ok()
            .and_then(|months| {
                months
                    .filter_map(|m| m.ok())
                    .map(|m| m.path().join(id).join(ITEM_FILE))
                    .find(|p| p.is_file())
            });
        match found {
            Some(p) => load_item(&p, &self.cfg),
            None => queued,
        }
    }

    /// The earlier mails handed over with the draft (`thread.txt`), for display and the check command.
    pub fn thread(&self, id: &str) -> Option<String> {
        let path = self.item_dir(id).ok()?.join(crate::add::THREAD_FILE);
        let cap = 4 * crate::add::MAX_THREAD_CHARS as u64;
        String::from_utf8(read_regular(&path, cap).ok()?).ok()
    }

    pub fn read(&self, id: &str) -> ItemRead {
        match self.item_dir(id) {
            Ok(d) => load_item(&d.join(ITEM_FILE), &self.cfg),
            Err(e) => ItemRead::Invalid {
                item_id: id.into(),
                errors: vec![e.0],
            },
        }
    }

    /// Every item in the queue (not the archive), sorted by id.
    pub fn list(&self) -> Vec<ItemRead> {
        self.list_ids().iter().map(|id| self.read(id)).collect()
    }

    pub fn list_ids(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.dir.join("queue")) else {
            return vec![];
        };
        let mut ids: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type().is_ok_and(|t| t.is_dir()) && e.path().join(ITEM_FILE).is_file()
            })
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| ITEM_ID_RE.is_match(n))
            .collect();
        ids.sort();
        ids
    }

    /// The single way to change an item: under its lock, from a valid snapshot in one of `allowed` (empty = any),
    /// through every write guard. No outside work happens under the lock.
    pub fn mutate<F>(&self, id: &str, allowed: &[State], f: F) -> Result<Item>
    where
        F: FnOnce(&mut Item) -> Result<()>,
    {
        let _guard = self.lock(id, true)?;
        self.mutate_locked(id, allowed, f)
    }

    pub(crate) fn mutate_locked<F>(&self, id: &str, allowed: &[State], f: F) -> Result<Item>
    where
        F: FnOnce(&mut Item) -> Result<()>,
    {
        self.mutate_inner(id, allowed, false, f)
    }

    /// The sender resolving its own attempt: from `sending` with outcome `unknown`, a definite refusal of the
    /// provider may still become `rejected`. Nothing else gets past the guards this way.
    #[cfg(feature = "send")]
    pub(crate) fn resolve_attempt<F>(&self, id: &str, f: F) -> Result<Item>
    where
        F: FnOnce(&mut Item) -> Result<()>,
    {
        let _guard = self.lock(id, true)?;
        self.mutate_inner(id, &[State::Sending], true, f)
    }

    fn mutate_inner<F>(&self, id: &str, allowed: &[State], resolution: bool, f: F) -> Result<Item>
    where
        F: FnOnce(&mut Item) -> Result<()>,
    {
        let old = match self.read(id) {
            ItemRead::Valid(i) => *i,
            ItemRead::Invalid { errors, .. } => {
                bail!("item {id} is invalid: {}", errors.join("; "))
            }
        };
        if !allowed.is_empty() && !allowed.contains(&old.state) {
            bail!("state conflict: item {id} is {:?}", old.state);
        }
        let mut new = old.clone();
        f(&mut new)?;
        if new.item_id != old.item_id {
            bail!("item_id mismatch in mutation");
        }
        self.write_guarded(&old, &mut new, resolution)?;
        Ok(new)
    }

    /// The write guards: revisions, frozen content, transport outcomes that never become retryable.
    fn write_guarded(&self, old: &Item, new: &mut Item, resolution: bool) -> Result<()> {
        use State::*;
        use TransportOutcome::*;
        let (old_digest, new_digest) = (content_digest(old), content_digest(new));
        if matches!(old.state, SendingScheduled | Sending | Sent | Discarded)
            && old_digest != new_digest
        {
            bail!("content edit conflicts with state {:?}", old.state);
        }
        let outcome = new.send.transport_outcome;
        if outcome == Some(Accepted) && !matches!(new.state, Sending | Sent) {
            bail!("accepted transport outcome needs state sending or sent");
        }
        let own_refusal = resolution
            && old.state == Sending
            && old.send.transport_outcome == Some(Unknown)
            && outcome == Some(Rejected)
            && new.state == Failed;
        if let Some(prev @ (Accepted | Unknown)) = old.send.transport_outcome
            && !own_refusal
            && (matches!(new.state, Ready | SendingScheduled)
                || !(outcome == Some(prev) || outcome == Some(Accepted)))
        {
            bail!("transport outcome {prev:?} cannot become retryable or be downgraded");
        }
        new.state_revision = old.state_revision + 1;
        new.content_revision = old.content_revision + u64::from(old_digest != new_digest);
        let extra = new.history.len().saturating_sub(HISTORY_CAP);
        new.history.drain(..extra);
        let errors = validate_item(new, &self.cfg);
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }
        let json = item_json(new)?;
        write_atomic(&self.item_dir(&new.item_id)?, ITEM_FILE, json.as_bytes())
    }

    /// The send click: `ready` -> `sending_scheduled`, only if the item on disk is exactly what the window showed
    /// (`expected_digest`, `expected_content_revision`). Exactly one caller wins; it gets the undo token.
    pub fn schedule_send(
        &self,
        id: &str,
        expected_digest: &str,
        expected_content_revision: u64,
        warnings_confirmed: bool,
    ) -> Result<Option<String>> {
        let _guard = self.lock(id, true)?;
        let ItemRead::Valid(item) = self.read(id) else {
            return Ok(None);
        };
        if item.state != State::Ready
            || item.content_revision != expected_content_revision
            || matches!(
                item.send.transport_outcome,
                Some(TransportOutcome::Accepted | TransportOutcome::Unknown)
            )
            || !ct_eq(&content_digest(&item), expected_digest)
        {
            return Ok(None);
        }
        let token = crate::random_hex(16);
        self.mutate_locked(id, &[State::Ready], |i| {
            i.state = State::SendingScheduled;
            i.send.token = Some(token.clone());
            i.send.scheduled_at = Some(crate::now_utc());
            i.send.approved_digest = Some(expected_digest.to_string());
            i.send.approved_revision = Some(expected_content_revision);
            i.send.warnings_confirmed = Some(warnings_confirmed);
            push_history(i, "send_scheduled", "");
            Ok(())
        })?;
        Ok(Some(token))
    }

    /// Undo within the countdown: back to `ready`, only with the matching token.
    pub fn cancel_send(&self, id: &str, token: &str) -> Result<bool> {
        let _guard = self.lock(id, true)?;
        let ItemRead::Valid(item) = self.read(id) else {
            return Ok(false);
        };
        if item.state != State::SendingScheduled
            || !item.send.token.as_deref().is_some_and(|t| ct_eq(t, token))
        {
            return Ok(false);
        }
        self.mutate_locked(id, &[State::SendingScheduled], |i| {
            i.state = State::Ready;
            clear_approval(i);
            push_history(i, "send_cancelled", "");
            Ok(())
        })?;
        Ok(true)
    }

    /// Start of the send: `sending_scheduled` -> `sending`, only with the matching token and only if the content
    /// still equals what was approved, both on disk (`approved_digest`) and in the sending process (`ram_digest`;
    /// the disk alone is no authority, a writer could replace content and approval together). On a mismatch the
    /// item falls back to `ready` with history `content_changed_after_click`; nothing is sent.
    pub fn claim_send(&self, id: &str, token: &str, ram_digest: &str) -> Result<bool> {
        let _guard = self.lock(id, true)?;
        let ItemRead::Valid(item) = self.read(id) else {
            return Ok(false);
        };
        if item.state != State::SendingScheduled
            || !item.send.token.as_deref().is_some_and(|t| ct_eq(t, token))
        {
            return Ok(false);
        }
        let current = content_digest(&item);
        let unchanged = item
            .send
            .approved_digest
            .as_deref()
            .is_some_and(|a| ct_eq(a, &current))
            && ct_eq(&current, ram_digest)
            && item.send.approved_revision == Some(item.content_revision);
        if !unchanged {
            self.mutate_locked(id, &[State::SendingScheduled], |i| {
                i.state = State::Ready;
                clear_approval(i);
                push_history(
                    i,
                    "content_changed_after_click",
                    "content changed after the click; nothing was sent",
                );
                Ok(())
            })?;
            return Ok(false);
        }
        self.mutate_locked(id, &[State::SendingScheduled], |i| {
            i.state = State::Sending;
            i.send.token = None;
            Ok(())
        })?;
        Ok(true)
    }

    pub fn note(&self, id: &str, event: &str, detail: &str) -> Result<()> {
        self.mutate(id, &[], |i| {
            i.history.push(HistoryEntry {
                ts: crate::now_utc(),
                event: event.into(),
                detail: detail.into(),
            });
            Ok(())
        })
        .map(|_| ())
    }

    pub fn audit(&self, line: AuditLine) -> Result<()> {
        // A Value prints its keys sorted.
        let mut text = serde_json::to_value(&line)?.to_string();
        text.push('\n');
        let mut f = open_audit(&self.dir.join("audit.jsonl"))?;
        f.lock()?;
        f.write_all(text.as_bytes())?;
        Ok(())
    }

    pub fn archive(&self, id: &str) -> Result<PathBuf> {
        let _guard = self.lock(id, true)?;
        self.archive_unlocked(id)
    }

    fn archive_unlocked(&self, id: &str) -> Result<PathBuf> {
        let src = self.item_dir(id)?;
        if !src.is_dir() {
            bail!("item {id} is not in the queue");
        }
        // Two checked steps: a symlink at `archive` or at the month folder must not lead the move elsewhere.
        let archive = self.dir.join("archive");
        mkdir_private(&archive)?;
        let month_dir = archive.join(chrono::Utc::now().format("%Y-%m").to_string());
        mkdir_private(&month_dir)?;
        // The month folder's own entry must be on disk before the item moves into it; also when another call made
        // the folder and stopped before its fsync.
        fsync_dir(&archive)?;
        let dest = month_dir.join(id);
        if fs::symlink_metadata(&dest).is_ok() {
            bail!("archive already holds {id}");
        }
        fs::rename(&src, &dest)?;
        fsync_dir(&month_dir)?;
        fsync_dir(&self.dir.join("queue"))?;
        Ok(dest)
    }

    /// Moves discarded items untouched for [`DISCARDED_GRACE`] to the archive. Never waits for a busy lock; a
    /// failing item never stops the others. Returns every item that is no longer in the queue afterwards.
    pub fn archive_stale_discarded(&self, now: SystemTime) -> Vec<String> {
        let cutoff = now
            .checked_sub(DISCARDED_GRACE)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let stale = |id: &str| -> Result<bool> {
            let modified = fs::metadata(self.item_dir(id)?.join(ITEM_FILE))?.modified()?;
            // A command-line discard waits for the app's mailbox cleanup.
            Ok(modified <= cutoff
                && matches!(self.read(id), ItemRead::Valid(i)
                    if i.state == State::Discarded && !needs_cli_cleanup(&i)))
        };
        let mut archived = Vec::new();
        for id in self.list_ids() {
            let attempt = || -> Result<bool> {
                if !stale(&id)? {
                    return Ok(false);
                }
                let Some(_guard) = self.lock(&id, false)? else {
                    return Ok(false);
                };
                if !stale(&id)? {
                    return Ok(false);
                }
                self.archive_unlocked(&id).map(|_| true)
            };
            match attempt() {
                Ok(true) => archived.push(id),
                Ok(false) => {}
                // Moved by a parallel sweep, or failed late: report whatever left the queue.
                Err(_) if self.item_dir(&id).is_ok_and(|d| !d.exists()) => archived.push(id),
                Err(_) => {}
            }
        }
        archived
    }

    /// Deletes archived items older than 90 days that never reached a transport, and delivered results older than
    /// 7 days. Never touches the queue.
    pub fn housekeeping(&self, now: SystemTime) -> Result<()> {
        let older_than = |p: &Path, age: Duration| {
            fs::symlink_metadata(p)
                .and_then(|m| m.modified())
                .is_ok_and(|m| m + age < now)
        };
        for month in fs::read_dir(self.dir.join("archive"))?.filter_map(|e| e.ok()) {
            // Only real YYYY-MM folders: a symlink here must never lead the deletion outside the archive.
            let name = month.file_name().to_string_lossy().into_owned();
            let month_like = name.len() == 7
                && name.as_bytes()[4] == b'-'
                && name.replace('-', "").bytes().all(|b| b.is_ascii_digit());
            if !month_like || !month.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(items) = fs::read_dir(month.path()) else {
                continue;
            };
            for entry in items.filter_map(|e| e.ok()) {
                let p = entry.path();
                if !entry.file_type().is_ok_and(|t| t.is_dir())
                    || !older_than(&p, ARCHIVE_RETENTION)
                {
                    continue;
                }
                // Deleted only with positive proof that no transport was ever tried; unreadable stays.
                let never_tried = read_regular(&p.join(ITEM_FILE), crate::item::MAX_ITEM_BYTES)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .is_some_and(|v| {
                        v["send"].is_object() && v["send"]["transport_outcome"].is_null()
                    });
                if never_tried {
                    fs::remove_dir_all(&p)?;
                }
            }
        }
        for entry in fs::read_dir(self.dir.join("results"))?.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.to_string_lossy().ends_with("delivered")
                && entry.file_type().is_ok_and(|t| t.is_file())
                && older_than(&p, DELIVERED_RETENTION)
            {
                fs::remove_file(&p)?;
            }
        }
        Ok(())
    }

    pub fn reconcile_on_start(&self) -> Reconcile {
        let mut out = Reconcile::default();
        for id in self.list_ids() {
            let item = match self.read(&id) {
                ItemRead::Valid(i) => i,
                ItemRead::Invalid { errors, .. } => {
                    out.errors.push((id, errors.join("; ")));
                    continue;
                }
            };
            match item.state {
                State::SendingScheduled => {
                    let undo = self.mutate(&id, &[State::SendingScheduled], |i| {
                        i.state = State::Ready;
                        clear_approval(i);
                        i.history.push(HistoryEntry {
                            ts: crate::now_utc(),
                            event: "reconciled_unscheduled".into(),
                            detail:
                                "app restart: the scheduled send was cancelled, nothing was sent"
                                    .into(),
                        });
                        Ok(())
                    });
                    match undo {
                        Ok(_) => out.unscheduled.push(id),
                        Err(e) => out.errors.push((id, e.0)),
                    }
                }
                State::Sending => out.needs_reconcile.push(id),
                State::Sent => out.needs_finalize.push(id),
                State::Failed | State::Discarded => out.check_result.push(id),
                State::Ready => {}
            }
        }
        out
    }

    /// The item lock (`locks/item-<id>.lock`), held while the returned file is open. Non-blocking: `None` when
    /// someone else holds it.
    // ponytail: lock files stay after archiving (a few bytes each); deleting them safely would need a second lock.
    pub(crate) fn lock(&self, id: &str, blocking: bool) -> Result<Option<File>> {
        self.item_dir(id)?;
        let f = self.lock_file(&format!("item-{id}"))?;
        if blocking {
            f.lock()?;
            return Ok(Some(f));
        }
        match f.try_lock() {
            Ok(()) => Ok(Some(f)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// A lock over the whole store (`locks/<name>.lock`), held while the returned file is open; waits until free.
    pub(crate) fn lock_store(&self, name: &str) -> Result<File> {
        let f = self.lock_file(name)?;
        f.lock()?;
        Ok(f)
    }

    fn lock_file(&self, name: &str) -> Result<File> {
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.dir.join("locks").join(format!("{name}.lock")))?)
    }
}

/// Equality in constant time for equal lengths (digests, tokens).
fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

pub(crate) fn push_history(i: &mut Item, event: &str, detail: &str) {
    i.history.push(HistoryEntry {
        ts: crate::now_utc(),
        event: event.into(),
        detail: detail.into(),
    });
}

/// Clears everything a send click approved.
pub(crate) fn clear_approval(i: &mut Item) {
    i.send.token = None;
    i.send.scheduled_at = None;
    i.send.approved_digest = None;
    i.send.approved_revision = None;
    i.send.warnings_confirmed = None;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::Error;
    use crate::item::tests::{cfg, item};
    use crate::item::{Checks, Verdict};
    use std::os::unix::fs::MetadataExt;

    pub(crate) struct TempRoot(pub PathBuf);
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    pub(crate) fn temp_root(name: &str) -> TempRoot {
        let d = std::env::temp_dir().join(format!(
            "dd-{name}-{}-{}",
            std::process::id(),
            crate::random_hex(4)
        ));
        TempRoot(d)
    }

    /// A store holding one valid `ready` item.
    pub(crate) fn store_with_item(name: &str) -> (TempRoot, Store, String) {
        let root = temp_root(name);
        let store = Store::open(&root.0, cfg()).unwrap();
        let it = item();
        put(&store, &it);
        (root, store, it.item_id)
    }

    /// Places an item directly on disk (tests only; `add` is the real way in).
    pub(crate) fn put(store: &Store, it: &Item) {
        let d = store.item_dir(&it.item_id).unwrap();
        mkdir_private(&d).unwrap();
        let mut json = serde_json::to_string_pretty(it).unwrap();
        json.push('\n');
        write_atomic(&d, ITEM_FILE, json.as_bytes()).unwrap();
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().mode() & 0o777
    }

    fn valid(r: ItemRead) -> Item {
        match r {
            ItemRead::Valid(i) => *i,
            ItemRead::Invalid { errors, .. } => panic!("invalid: {errors:?}"),
        }
    }

    fn set_age(store: &Store, id: &str, age: Duration) {
        let f = File::options()
            .write(true)
            .open(store.item_dir(id).unwrap().join(ITEM_FILE))
            .unwrap();
        f.set_modified(SystemTime::now() - age).unwrap();
    }

    fn discard(store: &Store, id: &str) {
        store
            .mutate(id, &[], |i| {
                i.state = State::Discarded;
                Ok(())
            })
            .unwrap();
    }

    fn with_id(n: u8) -> Item {
        let mut it = item();
        it.item_id = format!("20261106-223000-0000000{n}");
        it
    }

    #[test]
    fn thread_reads_written_text_and_refuses_symlinks() {
        let (_r, store, id) = store_with_item("thread");
        assert_eq!(store.thread(&id), None);
        let dir = store.item_dir(&id).unwrap();
        fs::write(dir.join(crate::add::THREAD_FILE), "earlier mail").unwrap();
        assert_eq!(store.thread(&id).as_deref(), Some("earlier mail"));
        fs::remove_file(dir.join(crate::add::THREAD_FILE)).unwrap();
        let target = dir.join("elsewhere.txt");
        fs::write(&target, "other").unwrap();
        std::os::unix::fs::symlink(&target, dir.join(crate::add::THREAD_FILE)).unwrap();
        assert_eq!(store.thread(&id), None);
        assert_eq!(store.thread("not-an-id"), None);
    }

    fn discarded_with(events: &[&str]) -> Item {
        let mut it = item();
        it.state = State::Discarded;
        it.history = events
            .iter()
            .map(|e| HistoryEntry {
                ts: crate::now_utc(),
                event: (*e).into(),
                detail: String::new(),
            })
            .collect();
        it
    }

    #[test]
    fn needs_cli_cleanup_cases() {
        assert!(needs_cli_cleanup(&discarded_with(&[
            "added",
            "discarded_by_cli"
        ])));
        assert!(!needs_cli_cleanup(&discarded_with(&[
            "added",
            "discarded_by_cli",
            "draft_cleanup"
        ])));
        assert!(needs_cli_cleanup(&discarded_with(&[
            "draft_cleanup",
            "discarded_by_cli"
        ])));
        assert!(!needs_cli_cleanup(&discarded_with(&[
            "added",
            "superseded"
        ])));
        assert!(!needs_cli_cleanup(&discarded_with(&["added", "discarded"])));
        let mut ready = discarded_with(&["discarded_by_cli"]);
        ready.state = State::Ready;
        assert!(!needs_cli_cleanup(&ready));
    }

    #[test]
    fn stale_cli_discard_is_not_archived_before_its_cleanup() {
        let (_r, store, id) = store_with_item("clidisc");
        store
            .mutate(&id, &[], |i| {
                i.state = State::Discarded;
                push_history(i, "discarded_by_cli", "");
                Ok(())
            })
            .unwrap();
        set_age(&store, &id, Duration::from_secs(3600));
        assert!(store.archive_stale_discarded(SystemTime::now()).is_empty());
        store.note(&id, "draft_cleanup", "done: test").unwrap();
        set_age(&store, &id, Duration::from_secs(3600));
        assert_eq!(store.archive_stale_discarded(SystemTime::now()), vec![id]);
    }

    #[test]
    fn write_new_at_refuses_existing_and_symlink() {
        let root = temp_root("wat");
        mkdir_private(&root.0).unwrap();
        let dir = open_dir_nofollow(&root.0).unwrap();
        write_new_at(&dir, "a.txt", b"one").unwrap();
        assert_eq!(fs::read(root.0.join("a.txt")).unwrap(), b"one");
        assert_eq!(mode(&root.0.join("a.txt")), 0o600);
        assert!(write_new_at(&dir, "a.txt", b"two").is_err());
        assert_eq!(fs::read(root.0.join("a.txt")).unwrap(), b"one");
        std::os::unix::fs::symlink(root.0.join("a.txt"), root.0.join("link")).unwrap();
        assert!(write_new_at(&dir, "link", b"x").is_err());
        unlink_at(&dir, "a.txt");
        assert!(!root.0.join("a.txt").exists());
        let link = root.0.join("dirlink");
        std::os::unix::fs::symlink(&root.0, &link).unwrap();
        assert!(open_dir_nofollow(&link).is_err());
    }

    #[test]
    fn layout_permissions() {
        let root = temp_root("layout");
        let store = Store::open(&root.0, cfg()).unwrap();
        for d in [
            "",
            "state",
            "state/queue",
            "state/archive",
            "state/results",
            "state/locks",
        ] {
            assert_eq!(mode(&root.0.join(d)), 0o700, "{d}");
        }
        assert_eq!(mode(&root.0.join("state/audit.jsonl")), 0o600);
        assert_eq!(store.dir(), root.0.join("state"));
        // Opening again keeps everything.
        Store::open(&root.0, cfg()).unwrap();
    }

    #[test]
    fn item_file_0600() {
        let (_r, store, id) = store_with_item("mode");
        store.note(&id, "x", "y").unwrap();
        assert_eq!(mode(&store.item_dir(&id).unwrap().join(ITEM_FILE)), 0o600);
        assert_eq!(
            mode(&store.dir().join(format!("locks/item-{id}.lock"))),
            0o600
        );
    }

    #[test]
    fn atomic_write_order_and_fsync_failure_keeps_old() {
        let (_r, store, id) = store_with_item("atomic");
        let d = store.item_dir(&id).unwrap();
        store.note(&id, "first", "").unwrap();
        // No temp file is left after a write.
        let names: Vec<String> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec![ITEM_FILE.to_string()]);
        // A failing write (folder not writable) keeps the old file intact and leaves no temp file.
        let before = fs::read(d.join(ITEM_FILE)).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(store.note(&id, "second", "").is_err());
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fs::read(d.join(ITEM_FILE)).unwrap(), before);
        assert_eq!(fs::read_dir(&d).unwrap().count(), 1);
        // A write rejected by validation keeps the old file too.
        assert!(
            store
                .mutate(&id, &[], |i| {
                    i.to.clear();
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(fs::read(d.join(ITEM_FILE)).unwrap(), before);
    }

    #[test]
    fn revisions_and_aba_content_revision() {
        let (_r, store, id) = store_with_item("rev");
        let start = valid(store.read(&id));
        let a = store
            .mutate(&id, &[], |i| {
                i.subject = "B".into();
                Ok(())
            })
            .unwrap();
        assert_eq!(
            (a.state_revision, a.content_revision),
            (start.state_revision + 1, start.content_revision + 1)
        );
        let b = store
            .mutate(&id, &[], |i| {
                i.subject = start.subject.clone();
                Ok(())
            })
            .unwrap();
        assert_eq!(
            (b.state_revision, b.content_revision),
            (start.state_revision + 2, start.content_revision + 2)
        );
        store.note(&id, "n", "").unwrap();
        let c = valid(store.read(&id));
        assert_eq!(
            (c.state_revision, c.content_revision),
            (start.state_revision + 3, start.content_revision + 2)
        );
        // A callback cannot forge the revisions.
        let d = store
            .mutate(&id, &[], |i| {
                i.state_revision = 1000;
                i.content_revision = 0;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            (d.state_revision, d.content_revision),
            (c.state_revision + 1, c.content_revision)
        );
    }

    #[test]
    fn content_change_forbidden_when_scheduled() {
        for state in [
            State::SendingScheduled,
            State::Sending,
            State::Sent,
            State::Discarded,
        ] {
            let (_r, store, id) = store_with_item("frozen");
            store
                .mutate(&id, &[], |i| {
                    i.state = state;
                    Ok(())
                })
                .unwrap();
            let err = store
                .mutate(&id, &[], |i| {
                    i.subject.push('!');
                    Ok(())
                })
                .unwrap_err();
            assert!(err.0.contains("content"), "{state:?}: {err}");
            // Non-content changes stay possible.
            store.note(&id, "n", "").unwrap();
        }
    }

    #[test]
    fn allowed_states_and_item_id_are_enforced() {
        let (_r, store, id) = store_with_item("allowed");
        assert!(
            store
                .mutate(&id, &[State::Sent], |_| Ok(()))
                .unwrap_err()
                .0
                .contains("state")
        );
        assert!(
            store
                .mutate(&id, &[], |i| {
                    i.item_id = "20261106-223000-ffffffff".into();
                    Ok(())
                })
                .is_err()
        );
        assert!(
            store
                .mutate(&id, &[], |_| Err(Error("callback".into())))
                .unwrap_err()
                .0
                .contains("callback")
        );
        assert!(
            store
                .mutate("20261106-223000-99999999", &[], |_| Ok(()))
                .is_err()
        );
    }

    #[test]
    fn outcome_guards() {
        let set = |store: &Store, id: &str, state: State, o: Option<TransportOutcome>| {
            store.mutate(id, &[], |i| {
                i.state = state;
                i.send.transport_outcome = o;
                Ok(())
            })
        };
        let (_r, store, id) = store_with_item("outcome");
        set(&store, &id, State::Sending, Some(TransportOutcome::Unknown)).unwrap();
        assert!(set(&store, &id, State::Ready, Some(TransportOutcome::Unknown)).is_err());
        assert!(set(&store, &id, State::Failed, None).is_err());
        assert!(set(&store, &id, State::Failed, Some(TransportOutcome::Rejected)).is_err());
        set(&store, &id, State::Failed, Some(TransportOutcome::Unknown)).unwrap();
        assert!(
            set(
                &store,
                &id,
                State::SendingScheduled,
                Some(TransportOutcome::Unknown)
            )
            .is_err()
        );

        let (_r2, store, id) = store_with_item("outcome2");
        // accepted only together with sending or sent.
        assert!(set(&store, &id, State::Failed, Some(TransportOutcome::Accepted)).is_err());
        set(
            &store,
            &id,
            State::Sending,
            Some(TransportOutcome::Accepted),
        )
        .unwrap();
        assert!(set(&store, &id, State::Failed, Some(TransportOutcome::Unknown)).is_err());
        assert!(set(&store, &id, State::Sending, Some(TransportOutcome::Unknown)).is_err());
        set(&store, &id, State::Sent, Some(TransportOutcome::Accepted)).unwrap();

        // unknown may still resolve to accepted.
        let (_r3, store, id) = store_with_item("outcome3");
        set(&store, &id, State::Sending, Some(TransportOutcome::Unknown)).unwrap();
        set(&store, &id, State::Sent, Some(TransportOutcome::Accepted)).unwrap();
    }

    #[test]
    fn history_cap_200() {
        let (_r, store, id) = store_with_item("cap");
        store
            .mutate(&id, &[], |i| {
                for k in 0..250 {
                    i.history.push(HistoryEntry {
                        ts: "t".into(),
                        event: format!("e{k}"),
                        detail: "".into(),
                    });
                }
                Ok(())
            })
            .unwrap();
        let it = valid(store.read(&id));
        assert_eq!(it.history.len(), HISTORY_CAP);
        assert_eq!(it.history.last().unwrap().event, "e249");
        assert_eq!(it.history[0].event, "e50");
    }

    #[test]
    fn audit_line_has_no_address_or_body() {
        let (_r, store, id) = store_with_item("audit");
        let it = valid(store.read(&id));
        store.audit(AuditLine::for_item("sent", &it)).unwrap();
        store
            .audit(AuditLine {
                outcome: Some("accepted".into()),
                ..AuditLine::for_item("x", &it)
            })
            .unwrap();
        let path = store.dir().join("audit.jsonl");
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(
            !text.contains("example.org") && !text.contains("Offer") && !text.contains("Hello"),
            "{text}"
        );
        let line: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(line["rcpt_count"], 1);
        assert_eq!(line["rcpt_hashes"][0], short_hash("ann@example.org"));
        assert_eq!(line["subject_hash"].as_str().unwrap().len(), 16);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn read_oversized_is_invalid() {
        let (_r, store, id) = store_with_item("big");
        fs::write(
            store.item_dir(&id).unwrap().join(ITEM_FILE),
            vec![b' '; 2 * 1024 * 1024 + 1],
        )
        .unwrap();
        assert!(matches!(store.read(&id), ItemRead::Invalid { .. }));
    }

    #[test]
    fn read_id_mismatch_is_invalid() {
        let (_r, store, _id) = store_with_item("idm");
        let mut other = item();
        other.item_id = "20261106-223000-ffffffff".into();
        let d = store.item_dir("20261106-223000-eeeeeeee").unwrap();
        mkdir_private(&d).unwrap();
        fs::write(d.join(ITEM_FILE), serde_json::to_string(&other).unwrap()).unwrap();
        match store.read("20261106-223000-eeeeeeee") {
            ItemRead::Invalid { item_id, .. } => assert_eq!(item_id, "20261106-223000-eeeeeeee"),
            ItemRead::Valid(_) => panic!("mismatch must be invalid"),
        }
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn reconcile_unscheduled_to_ready_with_history() {
        let (_r, store, id) = store_with_item("rec1");
        store
            .mutate(&id, &[], |i| {
                i.state = State::SendingScheduled;
                i.send.token = Some("a".repeat(32));
                i.send.scheduled_at = Some("t".into());
                i.send.approved_digest = Some(content_digest(i));
                i.send.approved_revision = Some(i.content_revision);
                i.send.warnings_confirmed = Some(true);
                Ok(())
            })
            .unwrap();
        let rec = store.reconcile_on_start();
        assert_eq!(rec.unscheduled, vec![id.clone()]);
        let it = valid(store.read(&id));
        assert_eq!(it.state, State::Ready);
        assert_eq!(it.send, Default::default());
        assert_eq!(it.history.last().unwrap().event, "reconciled_unscheduled");
    }

    #[test]
    fn reconcile_reports_sending_and_sent() {
        let root = temp_root("rec2");
        let store = Store::open(&root.0, cfg()).unwrap();
        let (mut a, mut b) = (with_id(1), with_id(2));
        a.state = State::Sending;
        b.state = State::Sent;
        put(&store, &a);
        put(&store, &b);
        let rec = store.reconcile_on_start();
        assert_eq!(rec.needs_reconcile, vec![a.item_id]);
        assert_eq!(rec.needs_finalize, vec![b.item_id]);
        assert!(rec.unscheduled.is_empty() && rec.errors.is_empty());
    }

    #[test]
    fn reconcile_ready_untouched() {
        let (_r, store, id) = store_with_item("rec3");
        let before = fs::read(store.item_dir(&id).unwrap().join(ITEM_FILE)).unwrap();
        assert_eq!(store.reconcile_on_start(), Reconcile::default());
        assert_eq!(
            fs::read(store.item_dir(&id).unwrap().join(ITEM_FILE)).unwrap(),
            before
        );
    }

    #[test]
    fn reconcile_isolates_item_errors() {
        let root = temp_root("rec4");
        let store = Store::open(&root.0, cfg()).unwrap();
        let bad = store.item_dir("20261106-223000-00000001").unwrap();
        mkdir_private(&bad).unwrap();
        fs::write(bad.join(ITEM_FILE), "{broken").unwrap();
        let mut good = with_id(2);
        good.state = State::Sending;
        put(&store, &good);
        let rec = store.reconcile_on_start();
        assert_eq!(rec.needs_reconcile, vec![good.item_id]);
        assert_eq!(rec.errors.len(), 1);
        assert_eq!(rec.errors[0].0, "20261106-223000-00000001");
    }

    #[test]
    fn read_any_finds_archived_items() {
        let (_r, store, id) = store_with_item("readany");
        store.archive(&id).unwrap();
        assert!(matches!(store.read(&id), ItemRead::Invalid { .. }));
        assert!(matches!(store.read_any(&id), ItemRead::Valid(_)));
        assert!(matches!(
            store.read_any("20261106-223000-99999999"),
            ItemRead::Invalid { .. }
        ));
    }

    #[test]
    fn read_any_takes_no_path_for_an_id() {
        // An item of another store, named by its path: the archive search must not join it onto the archive.
        let (_r, store, id) = store_with_item("readany2");
        let (_o, other, other_id) = store_with_item("readany3");
        store.archive(&id).unwrap();
        let foreign = other.item_dir(&other_id).unwrap();
        assert!(matches!(other.read(&other_id), ItemRead::Valid(_)));
        assert!(matches!(
            store.read_any(&foreign.to_string_lossy()),
            ItemRead::Invalid { .. }
        ));
    }

    #[test]
    fn archive_moves_to_utc_month() {
        let (_r, store, id) = store_with_item("arch");
        let dest = store.archive(&id).unwrap();
        let month = chrono::Utc::now().format("%Y-%m").to_string();
        assert_eq!(dest, store.dir().join("archive").join(&month).join(&id));
        assert!(dest.join(ITEM_FILE).is_file());
        assert!(!store.item_dir(&id).unwrap().exists());
        assert!(store.archive(&id).is_err());
    }

    #[test]
    fn archive_stale_discarded_only_old() {
        let root = temp_root("asd");
        let store = Store::open(&root.0, cfg()).unwrap();
        for n in 1..=3 {
            put(&store, &with_id(n));
        }
        let ids: Vec<String> = (1..=3).map(|n| with_id(n).item_id).collect();
        discard(&store, &ids[0]);
        discard(&store, &ids[1]);
        set_age(&store, &ids[0], Duration::from_secs(601));
        set_age(&store, &ids[2], Duration::from_secs(9999)); // old, but ready
        assert_eq!(
            store.archive_stale_discarded(SystemTime::now()),
            vec![ids[0].clone()]
        );
        assert!(store.item_dir(&ids[1]).unwrap().exists());
        assert!(store.item_dir(&ids[2]).unwrap().exists());
        // Later, the second one is old enough too.
        assert_eq!(
            store.archive_stale_discarded(SystemTime::now() + Duration::from_secs(700)),
            vec![ids[1].clone()]
        );
    }

    #[test]
    fn archive_stale_discarded_rechecks_under_lock() {
        // The pre-check sees an old discarded item; by the time the lock is held it must still hold. Here the
        // item is undone between "now" and the sweep by making it fresh again (as a parallel writer would).
        let (_r, store, id) = store_with_item("asd2");
        discard(&store, &id);
        set_age(&store, &id, Duration::from_secs(601));
        store.note(&id, "touched", "").unwrap(); // a write renews the age
        assert!(store.archive_stale_discarded(SystemTime::now()).is_empty());
    }

    #[test]
    fn archive_stale_discarded_never_waits_for_busy_lock() {
        let (_r, store, id) = store_with_item("asd3");
        discard(&store, &id);
        set_age(&store, &id, Duration::from_secs(601));
        let held = store.lock(&id, true).unwrap().unwrap();
        let t = std::time::Instant::now();
        assert!(store.archive_stale_discarded(SystemTime::now()).is_empty());
        assert!(t.elapsed() < Duration::from_secs(1));
        drop(held);
        assert_eq!(store.archive_stale_discarded(SystemTime::now()), vec![id]);
    }

    #[test]
    fn archive_stale_discarded_skips_failing_item() {
        let root = temp_root("asd4");
        let store = Store::open(&root.0, cfg()).unwrap();
        let bad = store.item_dir("20261106-223000-00000001").unwrap();
        mkdir_private(&bad).unwrap();
        fs::write(bad.join(ITEM_FILE), "{broken").unwrap();
        File::options()
            .write(true)
            .open(bad.join(ITEM_FILE))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(9999))
            .unwrap();
        let good = with_id(2);
        put(&store, &good);
        discard(&store, &good.item_id);
        set_age(&store, &good.item_id, Duration::from_secs(601));
        assert_eq!(
            store.archive_stale_discarded(SystemTime::now()),
            vec![good.item_id]
        );
        assert!(bad.exists());
    }

    #[test]
    fn archive_stale_discarded_tolerates_parallel_archive() {
        let root = temp_root("asd5");
        let store = std::sync::Arc::new(Store::open(&root.0, cfg()).unwrap());
        let ids: Vec<String> = (1..=6).map(|n| with_id(n).item_id).collect();
        for n in 1..=6 {
            put(&store, &with_id(n));
            discard(&store, &ids[n as usize - 1]);
            set_age(&store, &ids[n as usize - 1], Duration::from_secs(601));
        }
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let s = store.clone();
                std::thread::spawn(move || s.archive_stale_discarded(SystemTime::now()))
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(store.list().is_empty());
        let month = chrono::Utc::now().format("%Y-%m").to_string();
        assert_eq!(
            fs::read_dir(store.dir().join("archive").join(month))
                .unwrap()
                .count(),
            6
        );
    }

    #[test]
    fn housekeeping_archive_and_delivered() {
        let root = temp_root("hk");
        let store = Store::open(&root.0, cfg()).unwrap();
        let (plain, mut tried) = (with_id(1), with_id(2));
        tried.state = State::Failed;
        tried.send.transport_outcome = Some(TransportOutcome::Rejected);
        put(&store, &plain);
        put(&store, &tried);
        let a = store.archive(&plain.item_id).unwrap();
        let b = store.archive(&tried.item_id).unwrap();
        let results = store.dir().join("results");
        fs::write(results.join("x.json.delivered"), "{}").unwrap();
        fs::write(results.join("y.json"), "{}").unwrap();
        let old = SystemTime::now() + Duration::from_secs(91 * 86_400);
        store.housekeeping(SystemTime::now()).unwrap();
        assert!(a.exists() && b.exists() && results.join("x.json.delivered").exists());
        store.housekeeping(old).unwrap();
        assert!(!a.exists(), "old archive without transport is deleted");
        assert!(b.exists(), "an item that reached a transport is kept");
        assert!(!results.join("x.json.delivered").exists());
        assert!(results.join("y.json").exists(), "undelivered results stay");
    }

    fn schedule(store: &Store, id: &str) -> Option<String> {
        let it = valid(store.read(id));
        store
            .schedule_send(id, &content_digest(&it), it.content_revision, false)
            .unwrap()
    }

    #[test]
    fn two_threads_exactly_one_winner() {
        let (_r, store, id) = store_with_item("cas1");
        let store = std::sync::Arc::new(store);
        let it = valid(store.read(&id));
        let (digest, rev) = (content_digest(&it), it.content_revision);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (s, id, d) = (store.clone(), id.clone(), digest.clone());
                std::thread::spawn(move || s.schedule_send(&id, &d, rev, false).unwrap())
            })
            .collect();
        let winners: Vec<String> = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .collect();
        assert_eq!(winners.len(), 1);
        let after = valid(store.read(&id));
        assert_eq!(after.state, State::SendingScheduled);
        assert_eq!(after.send.token.as_deref(), Some(winners[0].as_str()));
        assert_eq!(after.send.approved_digest.as_deref(), Some(digest.as_str()));
        assert_eq!(after.send.approved_revision, Some(rev));
        assert_eq!(after.history.last().unwrap().event, "send_scheduled");
    }

    #[test]
    fn schedule_needs_shown_digest_and_revision() {
        let (_r, store, id) = store_with_item("cas2");
        let it = valid(store.read(&id));
        assert_eq!(
            store
                .schedule_send(&id, &"0".repeat(64), it.content_revision, false)
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .schedule_send(&id, &content_digest(&it), it.content_revision + 1, false)
                .unwrap(),
            None
        );
        assert_eq!(valid(store.read(&id)).state, State::Ready);
        let token = store
            .schedule_send(&id, &content_digest(&it), it.content_revision, true)
            .unwrap()
            .unwrap();
        assert_eq!(token.len(), 32);
        assert_eq!(valid(store.read(&id)).send.warnings_confirmed, Some(true));
    }

    #[test]
    fn schedule_only_from_ready() {
        for state in [
            State::SendingScheduled,
            State::Sending,
            State::Sent,
            State::Failed,
            State::Discarded,
        ] {
            let (_r, store, id) = store_with_item("cas3");
            store
                .mutate(&id, &[], |i| {
                    i.state = state;
                    Ok(())
                })
                .unwrap();
            assert_eq!(schedule(&store, &id), None, "{state:?}");
        }
    }

    #[test]
    fn cancel_then_claim_false_and_ready() {
        let (_r, store, id) = store_with_item("cas4");
        let digest = content_digest(&valid(store.read(&id)));
        let token = schedule(&store, &id).unwrap();
        assert!(store.cancel_send(&id, &token).unwrap());
        assert!(!store.claim_send(&id, &token, &digest).unwrap());
        let it = valid(store.read(&id));
        assert_eq!(it.state, State::Ready);
        assert_eq!(it.send, Default::default());
        assert_eq!(it.history.last().unwrap().event, "send_cancelled");
    }

    #[test]
    fn wrong_token_claim_unchanged() {
        let (_r, store, id) = store_with_item("cas5");
        let digest = content_digest(&valid(store.read(&id)));
        schedule(&store, &id).unwrap();
        let before = valid(store.read(&id));
        assert!(!store.claim_send(&id, &"f".repeat(32), &digest).unwrap());
        assert_eq!(valid(store.read(&id)), before);
    }

    #[test]
    fn wrong_token_cancel_false() {
        let (_r, store, id) = store_with_item("cas6");
        schedule(&store, &id).unwrap();
        assert!(!store.cancel_send(&id, &"f".repeat(32)).unwrap());
        assert!(!store.cancel_send(&id, "").unwrap());
        assert_eq!(valid(store.read(&id)).state, State::SendingScheduled);
    }

    #[test]
    fn claim_success_clears_token() {
        let (_r, store, id) = store_with_item("cas7");
        let digest = content_digest(&valid(store.read(&id)));
        let token = schedule(&store, &id).unwrap();
        assert!(store.claim_send(&id, &token, &digest).unwrap());
        let it = valid(store.read(&id));
        assert_eq!(it.state, State::Sending);
        assert_eq!(it.send.token, None);
        assert_eq!(it.send.approved_digest.as_deref(), Some(digest.as_str()));
        // A second claim with the same token loses.
        assert!(!store.claim_send(&id, &token, &digest).unwrap());
    }

    #[test]
    fn forged_disk_digest_loses_to_ram() {
        let (_r, store, id) = store_with_item("cas8");
        let shown = content_digest(&valid(store.read(&id)));
        let token = schedule(&store, &id).unwrap();
        // A writer replaces content and approval together, behind the store's back.
        let mut forged = valid(store.read(&id));
        forged.to = vec!["mallory@example.org".into()];
        forged.send.approved_digest = Some(content_digest(&forged));
        put(&store, &forged);
        assert!(!store.claim_send(&id, &token, &shown).unwrap());
        let it = valid(store.read(&id));
        assert_eq!(it.state, State::Ready);
        assert_eq!(it.send.token, None);
        assert_eq!(
            it.history.last().unwrap().event,
            "content_changed_after_click"
        );
    }

    #[test]
    fn content_changed_on_disk_after_click_loses() {
        let (_r, store, id) = store_with_item("cas9");
        let shown = content_digest(&valid(store.read(&id)));
        let token = schedule(&store, &id).unwrap();
        let mut changed = valid(store.read(&id));
        changed.subject = "Other".into();
        put(&store, &changed);
        assert!(!store.claim_send(&id, &token, &shown).unwrap());
        assert_eq!(valid(store.read(&id)).state, State::Ready);
    }

    #[test]
    fn matching_ram_digest_claims() {
        let (_r, store, id) = store_with_item("cas10");
        let shown = content_digest(&valid(store.read(&id)));
        let token = schedule(&store, &id).unwrap();
        store.note(&id, "unrelated", "").unwrap(); // history does not touch the digest
        assert!(store.claim_send(&id, &token, &shown).unwrap());
    }

    #[test]
    fn housekeeping_never_follows_month_symlinks() {
        let root = temp_root("hk2");
        let store = Store::open(&root.0, cfg()).unwrap();
        let outside = root.0.join("outside");
        fs::create_dir_all(outside.join("victim")).unwrap();
        std::os::unix::fs::symlink(&outside, store.dir().join("archive/2026-01")).unwrap();
        store
            .housekeeping(SystemTime::now() + Duration::from_secs(400 * 86_400))
            .unwrap();
        assert!(outside.join("victim").exists());
    }

    #[test]
    fn housekeeping_keeps_unreadable_archived_items() {
        let (_r, store, id) = store_with_item("hk3");
        let dest = store.archive(&id).unwrap();
        fs::write(dest.join(ITEM_FILE), "{broken").unwrap();
        store
            .housekeeping(SystemTime::now() + Duration::from_secs(91 * 86_400))
            .unwrap();
        assert!(dest.exists(), "no positive proof of 'never tried': keep");
    }

    #[test]
    fn read_regular_refuses_fifo_without_waiting() {
        let root = temp_root("fifo");
        fs::create_dir_all(&root.0).unwrap();
        let fifo = root.0.join("pipe");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: mkfifo on a fresh path in a private temp folder.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let t = std::time::Instant::now();
        assert!(read_regular(&fifo, 10).is_err());
        assert!(t.elapsed() < Duration::from_secs(2));
        fs::write(root.0.join("big"), vec![b'x'; 11]).unwrap();
        assert!(
            read_regular(&root.0.join("big"), 10)
                .unwrap_err()
                .0
                .contains("too large")
        );
        assert_eq!(read_regular(&root.0.join("big"), 11).unwrap().len(), 11);
    }

    #[test]
    fn oversized_write_is_refused() {
        let (_r, store, id) = store_with_item("big2");
        let err = store
            .mutate(&id, &[], |i| {
                i.from.name = "N".repeat(3 * 1024 * 1024);
                Ok(())
            })
            .unwrap_err();
        assert!(err.0.contains("too large"), "{err}");
        assert!(matches!(store.read(&id), ItemRead::Valid(_)));
    }

    #[test]
    fn reconcile_lists_failed_and_discarded_for_a_result_check() {
        let root = temp_root("rec5");
        let store = Store::open(&root.0, cfg()).unwrap();
        let (mut a, mut b) = (with_id(1), with_id(2));
        a.state = State::Failed;
        a.send.transport_outcome = Some(TransportOutcome::Unknown);
        b.state = State::Discarded;
        put(&store, &a);
        put(&store, &b);
        assert_eq!(
            store.reconcile_on_start().check_result,
            vec![a.item_id, b.item_id]
        );
    }

    #[test]
    fn audit_line_hashes_attachment_names() {
        let mut it = item();
        it.attachments = vec![crate::item::Attachment {
            name: "offer-for-ann.pdf".into(),
            size: 3,
            sha256: "a".repeat(64),
        }];
        assert_eq!(
            AuditLine::for_item("sent", &it).attachments,
            vec![short_hash("offer-for-ann.pdf")]
        );
    }

    #[test]
    fn archive_refuses_a_symlinked_month_or_archive_folder() {
        let (r, store, id) = store_with_item("arch-link");
        let outside = r.0.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
        let untouched = |what: &str| {
            assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "{what}: moved");
            assert_eq!(mode(&outside), 0o755, "{what}: chmodded");
            assert!(store.item_dir(&id).unwrap().is_dir(), "{what}: queued");
        };
        let archive = store.dir().join("archive");
        let month = archive.join(chrono::Utc::now().format("%Y-%m").to_string());
        std::os::unix::fs::symlink(&outside, &month).unwrap();
        assert!(store.archive(&id).is_err());
        untouched("month");
        fs::remove_file(&month).unwrap();
        fs::remove_dir(&archive).unwrap();
        std::os::unix::fs::symlink(&outside, &archive).unwrap();
        assert!(store.archive(&id).is_err());
        untouched("archive");
    }

    /// Runs `f` on its own thread; `None` when it still waits after 5 s (then the FIFO gets a reader, so the
    /// thread ends).
    fn within_5s<T: Send + 'static>(
        fifo: &Path,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        let got = rx.recv_timeout(Duration::from_secs(5)).ok();
        if got.is_none() {
            let _reader = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(fifo);
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
        got
    }

    #[test]
    fn audit_and_open_refuse_a_fifo_without_waiting() {
        let (r, store, id) = store_with_item("audit-fifo");
        let it = valid(store.read(&id));
        let path = store.dir().join("audit.jsonl");
        fs::remove_file(&path).unwrap();
        let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: mkfifo on a fresh path in a private temp folder.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let audit = within_5s(&path, move || store.audit(AuditLine::for_item("sent", &it)));
        assert!(audit.expect("no wait for a reader").is_err());
        let root = r.0.clone();
        let opened = within_5s(&path, move || Store::open(&root, cfg()).map(|_| ()));
        assert!(opened.expect("no wait for a reader").is_err());
    }

    #[test]
    fn checks_survive_a_write() {
        let (_r, store, id) = store_with_item("checks");
        let it = store
            .mutate(&id, &[], |i| {
                i.checks = Some(Checks {
                    body_sha256: i.body_sha256.clone(),
                    verdict: Verdict::Warn,
                    findings: vec![],
                    ran_at: None,
                    source: None,
                    content_digest: None,
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(valid(store.read(&id)), it);
    }
}
