//! Taking a draft from an agent into the queue, and withdrawing an older version it replaces.

use crate::address;
use crate::item::{
    Attachment, CheckSource, DRAFT_SCHEMA, Draft, Edits, From, HistoryEntry, ITEM_FILE, ITEM_ID_RE,
    ITEM_SCHEMA, Item, ItemRead, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS, MAX_ITEM_BYTES,
    MAX_TOTAL_ATTACHMENT_BYTES, MESSAGE_ID_RE, Reply, SendState, State, TransportOutcome,
    attachment_name_ok, content_digest, validate_item,
};
use crate::store::{
    AuditLine, Store, clear_approval, fsync_dir, mkdir_private, push_history, read_regular,
    write_atomic,
};
use crate::text::strip_forbidden;
use crate::{Error, Result, bail, sha256_hex};
use chrono::{DateTime, Local, SecondsFormat};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub const THREAD_FILE: &str = "thread.txt";
pub const MAX_THREAD_CHARS: usize = 200_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddOutcome {
    pub item_id: String,
    pub replaced: Option<ReplaceOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceOutcome {
    /// The old version is discarded and will never be sent.
    Withdrawn(String),
    /// Someone edited the old version in the window; it stays.
    BlockedWindowEdited(String),
    /// The old version is being sent, was sent, or may have been sent.
    TooLate(String),
    /// No such item from the same session.
    NotFound(String),
}

/// Validates the whole draft, then creates `queue/<id>/` in one rename. Nothing is written for an invalid draft.
pub fn add(
    store: &Store,
    draft: Draft,
    replaces: Option<&str>,
    now: DateTime<Local>,
) -> Result<AddOutcome> {
    let id = format!("{}-{}", now.format("%Y%m%d-%H%M%S"), crate::random_hex(4));
    add_with_id(store, draft, replaces, now, &id)
}

pub(crate) fn add_with_id(
    store: &Store,
    draft: Draft,
    replaces: Option<&str>,
    now: DateTime<Local>,
    id: &str,
) -> Result<AddOutcome> {
    let (item, files, thread) = prepare(store, draft, now, id)?;
    let session = item.origin.as_ref().and_then(|o| o.session.as_deref());
    // An agent that retries with the same correlation gets the item it made the first time; a replacement the first
    // run did not finish (it died after writing) still happens.
    let _guard = match item.origin.as_ref().and_then(|o| o.correlation.as_ref()) {
        None => None,
        Some(_) => {
            let guard = store.lock_store("add")?;
            if let Some(existing) = by_correlation(store, &item)? {
                // Naming the very item it made the first time: a plain repeat, nothing to withdraw.
                let replaced = replaces
                    .filter(|old| *old != existing)
                    .map(|old| replace(store, old, session, &existing));
                return Ok(AddOutcome {
                    item_id: existing,
                    replaced,
                });
            }
            Some(guard)
        }
    };
    write_new(store, &item, &files, thread.as_deref())?;
    let replaced = replaces.map(|old| replace(store, old, session, &item.item_id));
    Ok(AddOutcome {
        item_id: item.item_id,
        replaced,
    })
}

type Files = Vec<(String, Vec<u8>)>;

/// Everything up to the first write: normalize, read attachments, build and validate the item.
fn prepare(
    store: &Store,
    draft: Draft,
    now: DateTime<Local>,
    id: &str,
) -> Result<(Item, Files, Option<String>)> {
    if draft.schema != DRAFT_SCHEMA {
        bail!("schema: {:?} != {DRAFT_SCHEMA:?}", draft.schema);
    }
    let cfg = store.config();
    let Some(account) = cfg.account(&draft.account) else {
        bail!("account: unknown account {:?}", draft.account)
    };
    let Some(sender) = account.sender(&draft.from.address) else {
        bail!(
            "from.address: {:?} is not a sender of account {:?}",
            draft.from.address,
            draft.account
        )
    };
    let Some(name) = draft.from.name.clone().or_else(|| sender.name.clone()) else {
        bail!(
            "from.name: missing, and the account has no default name for {:?}",
            draft.from.address
        )
    };
    let (to, cc, bcc) = (
        address::normalize_list("to", &draft.to)?,
        address::normalize_list("cc", &draft.cc)?,
        address::normalize_list("bcc", &draft.bcc)?,
    );
    let reply = draft.reply.map(normalize_reply).transpose()?;
    if draft.attachments.len() > MAX_ATTACHMENTS {
        bail!(
            "attachments: {} > {MAX_ATTACHMENTS}",
            draft.attachments.len()
        );
    }
    let mut files: Files = Vec::new();
    let (mut taken, mut total) = (BTreeSet::new(), 0u64);
    for path in &draft.attachments {
        let bytes = read_attachment(Path::new(path))?;
        total += bytes.len() as u64;
        if total > MAX_TOTAL_ATTACHMENT_BYTES {
            bail!("attachments: total exceeds {MAX_TOTAL_ATTACHMENT_BYTES} bytes");
        }
        let base = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if !attachment_name_ok(base) {
            bail!("attachment {path:?}: unusable file name");
        }
        files.push((unique_name(&mut taken, base), bytes));
    }
    let mut checks = draft.checks;
    if let Some(c) = checks.as_mut() {
        c.source = Some(CheckSource::Agent);
        c.findings = std::mem::take(&mut c.findings)
            .into_iter()
            .map(crate::item::Finding::cleaned)
            .collect();
    }
    let mut item = Item {
        schema: ITEM_SCHEMA.into(),
        item_id: id.into(),
        created_at: now.to_rfc3339_opts(SecondsFormat::Secs, false),
        state: State::Ready,
        state_revision: 1,
        content_revision: 1,
        account: draft.account,
        from: From {
            address: draft.from.address,
            name,
        },
        to,
        cc,
        bcc,
        subject: draft.subject,
        body_sha256: sha256_hex(draft.body.as_bytes()),
        body: draft.body,
        reply,
        mailbox_draft: draft.mailbox_draft,
        origin: draft.origin,
        added_digest: None,
        attachments: files
            .iter()
            .map(|(name, b)| Attachment {
                name: name.clone(),
                size: b.len() as u64,
                sha256: sha256_hex(b),
            })
            .collect(),
        checks,
        edits: Edits::default(),
        send: SendState::default(),
        history: vec![HistoryEntry {
            ts: crate::now_utc(),
            event: "added".into(),
            detail: String::new(),
        }],
    };
    item.added_digest = Some(content_digest(&item));
    // The agent's findings were made for exactly what it hands over (checks are no content: the digest stays).
    if let Some(c) = &mut item.checks {
        c.content_digest.clone_from(&item.added_digest);
    }
    let errors = validate_item(&item, cfg);
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    let thread = draft
        .thread
        .map(|t| strip_forbidden(&t).chars().take(MAX_THREAD_CHARS).collect());
    Ok((item, files, thread))
}

/// `<a@b>` form; a bare `a@b` gets brackets. An invalid In-Reply-To is an error; invalid References are dropped.
fn normalize_reply(r: Reply) -> Result<Reply> {
    let norm = |s: &str| {
        let s = s.trim();
        let s = if s.starts_with('<') {
            s.to_string()
        } else {
            format!("<{s}>")
        };
        MESSAGE_ID_RE.is_match(&s).then_some(s)
    };
    let in_reply_to = match r.in_reply_to.as_deref() {
        None => None,
        Some(v) => Some(
            norm(v).ok_or_else(|| Error(format!("reply.in_reply_to: not a message id: {v:?}")))?,
        ),
    };
    let references = r
        .references
        .iter()
        .flat_map(|v| v.split_whitespace())
        .filter_map(norm)
        .collect();
    Ok(Reply {
        in_reply_to,
        references,
        gmail_thread_id: r.gmail_thread_id,
    })
}

/// Checks and reads through one descriptor: no symlink, no FIFO wait, a regular file, never more than the cap.
fn read_attachment(path: &Path) -> Result<Vec<u8>> {
    crate::store::read_regular(path, MAX_ATTACHMENT_BYTES).map_err(|e| {
        Error(format!(
            "attachment {path:?}: {e} (symlinks and special files are refused)"
        ))
    })
}

/// `a.pdf`, then `a-2.pdf`, `a-3.pdf`, ... `taken` holds lowercase names: the Mac's file system ignores case, so
/// `Offer.pdf` and `offer.pdf` would be one file.
// ponytail: case only; names that differ just in their Unicode normalization (NFC/NFD) are one file on APFS too and
// still fail loudly ("File exists"); fold them as well if that ever happens in practice.
pub(crate) fn unique_name(taken: &mut BTreeSet<String>, name: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut candidate = name.to_string();
    let mut n = 2;
    while taken.contains(&candidate.to_lowercase()) {
        candidate = format!("{stem}-{n}{ext}");
        n += 1;
    }
    taken.insert(candidate.to_lowercase());
    candidate
}

fn write_private(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(name))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Builds the item in `queue/<id>.tmp/` and moves it into place with one rename; any failure removes the temp.
fn write_new(store: &Store, item: &Item, files: &Files, thread: Option<&str>) -> Result<()> {
    let queue = store.dir().join("queue");
    let final_dir = store.item_dir(&item.item_id)?;
    let tmp = queue.join(format!("{}.tmp", item.item_id));
    fs::create_dir(&tmp)?;
    let result = (|| -> Result<()> {
        mkdir_private(&tmp)?;
        let att = tmp.join("att");
        mkdir_private(&att)?;
        for (name, bytes) in files {
            write_private(&att, name, bytes)?;
        }
        fsync_dir(&att)?;
        if let Some(t) = thread {
            write_private(&tmp, THREAD_FILE, t.as_bytes())?;
        }
        write_atomic(&tmp, ITEM_FILE, crate::store::item_json(item)?.as_bytes())?;
        if fs::symlink_metadata(&final_dir).is_ok() {
            bail!("item {} already exists", item.item_id);
        }
        fs::rename(&tmp, &final_dir)?;
        fsync_dir(&queue)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    result
}

/// Withdraws `old` in favour of the item `new_id` (from `session`): only an item of the same session, only while
/// nothing went out, and never one edited in the window.
fn replace(store: &Store, old: &str, session: Option<&str>, new_id: &str) -> ReplaceOutcome {
    // No session on the new draft: it cannot prove it belongs with the old one.
    let Some(session) = session else {
        return ReplaceOutcome::NotFound(old.into());
    };
    match withdraw(store, old, Some(session), "superseded", new_id) {
        Ok(outcome) => outcome,
        // ponytail: a store error while withdrawing reads as "not found"; the new item exists either way.
        Err(_) => ReplaceOutcome::NotFound(old.into()),
    }
}

/// Shared by replacing and by `draftdesk discard`: `ready`/`sending_scheduled` (and a rejected `failed`) become
/// `discarded`, unless edited in the window. `session` = `Some` requires the same `origin.session`.
pub fn withdraw(
    store: &Store,
    id: &str,
    session: Option<&str>,
    event: &str,
    detail: &str,
) -> Result<ReplaceOutcome> {
    let found = |o: fn(String) -> ReplaceOutcome| Ok(o(id.to_string()));
    if !ITEM_ID_RE.is_match(id) {
        return found(ReplaceOutcome::NotFound);
    }
    let _guard = store.lock(id, true)?;
    let item = match store.read(id) {
        ItemRead::Valid(i) => *i,
        ItemRead::Invalid { .. } if archived(store, id) => return found(ReplaceOutcome::TooLate),
        ItemRead::Invalid { .. } => return found(ReplaceOutcome::NotFound),
    };
    if let Some(s) = session
        && item.origin.as_ref().and_then(|o| o.session.as_deref()) != Some(s)
    {
        return found(ReplaceOutcome::NotFound);
    }
    let withdrawable = match item.state {
        State::Ready | State::SendingScheduled => true,
        State::Failed => item.send.transport_outcome == Some(TransportOutcome::Rejected),
        State::Discarded => return found(ReplaceOutcome::Withdrawn),
        State::Sending | State::Sent => false,
    };
    if !withdrawable {
        return found(ReplaceOutcome::TooLate);
    }
    if item.edits.edited {
        return found(ReplaceOutcome::BlockedWindowEdited);
    }
    let updated = store.mutate_locked(id, &[item.state], |i| {
        i.state = State::Discarded;
        clear_approval(i);
        push_history(i, event, detail);
        Ok(())
    })?;
    store.audit(AuditLine::for_item(event, &updated))?;
    found(ReplaceOutcome::Withdrawn)
}

/// The item an earlier `add` made with the same account, `origin.agent` and `origin.correlation`, in the queue or the
/// archive: its id when the content it was added with is the same; an error for other content or more than one.
/// Read loosely (only these fields), so an archived item that no longer validates still counts; an item it cannot
/// read at all stops the add, since it could be the one this retry repeats. The caller holds the store's add lock.
// ponytail: a linear scan over queue and archive (under 600 items today); an index once that gets slow.
fn by_correlation(store: &Store, new: &Item) -> Result<Option<String>> {
    let origin = new.origin.clone().unwrap_or_default();
    // By id: an item the app archives while this runs may show up in both places.
    let mut found = BTreeMap::new();
    let mut scan = |dir: &Path| -> Result<()> {
        for e in fs::read_dir(dir)? {
            let e = e?;
            let Some(id) = e
                .file_name()
                .to_str()
                .filter(|n| ITEM_ID_RE.is_match(n))
                .map(String::from)
            else {
                continue;
            };
            let path = e.path().join(ITEM_FILE);
            // Gone meanwhile: archived (the archive is read after the queue) or deleted by the 90-day housekeeping.
            if fs::symlink_metadata(&path)
                .is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
            {
                continue;
            }
            let v: serde_json::Value = read_regular(&path, MAX_ITEM_BYTES)
                .and_then(|b| Ok(serde_json::from_slice(&b)?))
                .map_err(|err| Error(format!("correlation lookup: item {id}: {err}")))?;
            let text = |v: &serde_json::Value| v.as_str().map(String::from);
            if text(&v["account"]).as_deref() == Some(new.account.as_str())
                && text(&v["origin"]["agent"]) == origin.agent
                && text(&v["origin"]["correlation"]) == origin.correlation
            {
                found.insert(id, text(&v["added_digest"]));
            }
        }
        Ok(())
    };
    scan(&store.dir().join("queue"))?;
    // Listed only now, so a month folder made by an archive during the queue scan is not missed.
    for m in fs::read_dir(store.dir().join("archive"))? {
        let m = m?;
        if m.file_type()?.is_dir() {
            scan(&m.path())?;
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => {
            let (id, digest) = found.pop_first().expect("one entry");
            if digest == new.added_digest {
                Ok(Some(id))
            } else {
                bail!("correlation conflict: different content than item {id}")
            }
        }
        n => bail!(
            "ambiguous correlation: {n} items ({})",
            found.into_keys().collect::<Vec<_>>().join(", ")
        ),
    }
}

fn archived(store: &Store, id: &str) -> bool {
    fs::read_dir(store.dir().join("archive"))
        .map(|months| {
            months
                .filter_map(|m| m.ok())
                .any(|m| m.path().join(id).join(ITEM_FILE).is_file())
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::tests::cfg;
    use crate::item::{Checks, Origin, Verdict, content_digest};
    use crate::store::tests::{TempRoot, temp_root};
    use std::fs::File;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn draft() -> Draft {
        serde_json::from_value(serde_json::json!({
            "schema": "draftdesk/1",
            "account": "work",
            "from": { "address": "me@example.org" },
            "to": ["Ann <ann@example.org>, bob@example.org"],
            "cc": [],
            "subject": "Re: Offer",
            "body": "Hello Ann,\n\nthanks.\n",
            "reply": { "in_reply_to": "a@example.org", "references": ["<r1@example.org> bad <r2@example.org>"] },
            "thread": "earlier\u{202E} mail",
            "origin": { "agent": "test", "session": "s1" }
        }))
        .unwrap()
    }

    fn setup(name: &str) -> (TempRoot, Store, std::path::PathBuf) {
        let root = temp_root(name);
        let store = Store::open(&root.0, cfg()).unwrap();
        let files = root.0.join("files");
        fs::create_dir_all(&files).unwrap();
        (root, store, files)
    }

    fn valid(store: &Store, id: &str) -> Item {
        match store.read(id) {
            ItemRead::Valid(i) => *i,
            ItemRead::Invalid { errors, .. } => panic!("{errors:?}"),
        }
    }

    fn queue_names(store: &Store) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(store.dir().join("queue"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn add_creates_layout_and_hashes() {
        let (_r, store, files) = setup("add1");
        fs::write(files.join("offer.pdf"), b"%PDF-1.4 data").unwrap();
        let mut d = draft();
        d.attachments = vec![files.join("offer.pdf").to_string_lossy().into()];
        let out = add(&store, d, None, Local::now()).unwrap();
        assert!(ITEM_ID_RE.is_match(&out.item_id));
        assert_eq!(out.replaced, None);
        let dir = store.item_dir(&out.item_id).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(dir.join("att/offer.pdf")).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::read_to_string(dir.join(THREAD_FILE)).unwrap(),
            "earlier mail"
        );
        let it = valid(&store, &out.item_id);
        assert_eq!(it.state, State::Ready);
        assert_eq!(it.from.name, "Me", "default name from the configuration");
        assert_eq!(
            it.to,
            vec![
                "Ann <ann@example.org>".to_string(),
                "bob@example.org".to_string()
            ]
        );
        assert_eq!(it.attachments[0].sha256, sha256_hex(b"%PDF-1.4 data"));
        assert_eq!(it.attachments[0].size, 13);
        assert_eq!(it.body_sha256, sha256_hex(b"Hello Ann,\n\nthanks.\n"));
        let reply = it.reply.unwrap();
        assert_eq!(reply.in_reply_to.as_deref(), Some("<a@example.org>"));
        assert_eq!(
            reply.references,
            vec![
                "<r1@example.org>".to_string(),
                "<r2@example.org>".to_string()
            ]
        );
        assert_eq!(it.history[0].event, "added");
        assert_eq!(queue_names(&store), vec![out.item_id]);
    }

    #[test]
    fn add_rejects_before_touching_disk() {
        let (_r, store, _files) = setup("add2");
        let bad: Vec<fn(&mut Draft)> = vec![
            |d| d.schema = "draftdesk/2".into(),
            |d| d.account = "nope".into(),
            |d| d.from.address = "x@example.org".into(),
            |d| d.to = vec![],
            |d| d.to = vec!["not an address".into()],
            |d| d.subject = "a\nb".into(),
            |d| d.body = "  ".into(),
            |d| d.attachments = vec!["/does/not/exist.pdf".into()],
            |d| {
                d.reply = Some(Reply {
                    in_reply_to: Some("<a b>".into()),
                    ..Default::default()
                })
            },
            |d| {
                d.origin = Some(Origin {
                    session: Some("x".repeat(161)),
                    ..Default::default()
                })
            },
        ];
        for (k, f) in bad.iter().enumerate() {
            let mut d = draft();
            f(&mut d);
            assert!(add(&store, d, None, Local::now()).is_err(), "case {k}");
        }
        assert!(queue_names(&store).is_empty());
    }

    #[test]
    fn unknown_field_in_draft_is_rejected() {
        let mut v = serde_json::to_value(draft()).unwrap();
        v["surprise"] = 1.into();
        assert!(serde_json::from_value::<Draft>(v).is_err());
        let mut v = serde_json::to_value(draft()).unwrap();
        v["from"]["x"] = 1.into();
        assert!(serde_json::from_value::<Draft>(v).is_err());
    }

    #[test]
    fn add_cleans_tmp_on_error() {
        let (_r, store, _files) = setup("add3");
        let id = "20261106-223000-0000abcd";
        let taken = store.item_dir(id).unwrap();
        fs::create_dir_all(&taken).unwrap();
        fs::write(taken.join("x"), "occupied").unwrap();
        assert!(add_with_id(&store, draft(), None, Local::now(), id).is_err());
        assert_eq!(
            queue_names(&store),
            vec![id.to_string()],
            "no .tmp left behind"
        );
        assert_eq!(fs::read_to_string(taken.join("x")).unwrap(), "occupied");
    }

    #[test]
    fn attachment_symlink_rejected() {
        let (_r, store, files) = setup("add4");
        fs::write(files.join("real.pdf"), b"x").unwrap();
        std::os::unix::fs::symlink(files.join("real.pdf"), files.join("link.pdf")).unwrap();
        let mut d = draft();
        d.attachments = vec![files.join("link.pdf").to_string_lossy().into()];
        assert!(
            add(&store, d, None, Local::now())
                .unwrap_err()
                .0
                .contains("symlink")
        );
        let mut d = draft();
        d.attachments = vec![files.to_string_lossy().into()];
        assert!(
            add(&store, d, None, Local::now())
                .unwrap_err()
                .0
                .contains("regular file")
        );
        assert!(queue_names(&store).is_empty());
    }

    #[test]
    fn attachment_too_big_rejected() {
        let (_r, store, files) = setup("add5");
        let big = File::create(files.join("big.bin")).unwrap();
        big.set_len(MAX_ATTACHMENT_BYTES + 1).unwrap();
        let mut d = draft();
        d.attachments = vec![files.join("big.bin").to_string_lossy().into()];
        assert!(
            add(&store, d, None, Local::now())
                .unwrap_err()
                .0
                .contains("too large")
        );
        // Two files under the per-file cap that together exceed the total.
        File::create(files.join("a.bin"))
            .unwrap()
            .set_len(MAX_ATTACHMENT_BYTES / 2 + 1)
            .unwrap();
        File::create(files.join("b.bin"))
            .unwrap()
            .set_len(MAX_ATTACHMENT_BYTES / 2 + 1)
            .unwrap();
        let mut d = draft();
        d.attachments = vec![
            files.join("a.bin").to_string_lossy().into(),
            files.join("b.bin").to_string_lossy().into(),
        ];
        assert!(
            add(&store, d, None, Local::now())
                .unwrap_err()
                .0
                .contains("total")
        );
        assert!(queue_names(&store).is_empty());
    }

    #[test]
    fn fifo_attachment_refused_without_waiting() {
        let (_r, store, files) = setup("add10");
        let fifo = files.join("pipe");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: mkfifo on a fresh path in a private temp folder.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let mut d = draft();
        d.attachments = vec![fifo.to_string_lossy().into()];
        let t = std::time::Instant::now();
        assert!(add(&store, d, None, Local::now()).is_err());
        assert!(t.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn invisible_chars_in_attachment_names_refused() {
        let (_r, store, files) = setup("add11");
        let name = files.join("invoice\u{202E}fdp.exe");
        fs::write(&name, "x").unwrap();
        let mut d = draft();
        d.attachments = vec![name.to_string_lossy().into()];
        assert!(
            add(&store, d, None, Local::now())
                .unwrap_err()
                .0
                .contains("file name")
        );
    }

    #[test]
    fn oversized_item_is_refused_before_writing() {
        let (_r, store, _files) = setup("add12");
        let mut d = draft();
        d.from.name = Some("N".repeat(3 * 1024 * 1024));
        assert!(
            add(&store, d, None, Local::now())
                .unwrap_err()
                .0
                .contains("too large")
        );
        assert!(queue_names(&store).is_empty());
    }

    #[test]
    fn name_collision_suffixes() {
        let (_r, store, files) = setup("add6");
        for sub in ["x", "y", "z"] {
            fs::create_dir_all(files.join(sub)).unwrap();
            fs::write(files.join(sub).join("a.pdf"), sub).unwrap();
        }
        fs::write(files.join("README"), "r").unwrap();
        fs::create_dir_all(files.join("w")).unwrap();
        fs::write(files.join("w").join("README"), "w").unwrap();
        let mut d = draft();
        d.attachments = ["x/a.pdf", "y/a.pdf", "z/a.pdf", "README", "w/README"]
            .iter()
            .map(|p| files.join(p).to_string_lossy().into())
            .collect();
        let it = valid(&store, &add(&store, d, None, Local::now()).unwrap().item_id);
        let names: Vec<&str> = it.attachments.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["a.pdf", "a-2.pdf", "a-3.pdf", "README", "README-2"]
        );
        let dir = store.item_dir(&it.item_id).unwrap().join("att");
        assert_eq!(fs::read_to_string(dir.join("a-3.pdf")).unwrap(), "z");
    }

    #[test]
    fn names_differing_only_in_case_get_a_suffix() {
        // The Mac's file system ignores case: "Offer.pdf" and "offer.pdf" would be one file.
        let (_r, store, files) = setup("add6b");
        for sub in ["x", "y"] {
            fs::create_dir_all(files.join(sub)).unwrap();
        }
        fs::write(files.join("x/Offer.pdf"), "x").unwrap();
        fs::write(files.join("y/offer.pdf"), "y").unwrap();
        let mut d = draft();
        d.attachments = ["x/Offer.pdf", "y/offer.pdf"]
            .iter()
            .map(|p| files.join(p).to_string_lossy().into())
            .collect();
        let it = valid(&store, &add(&store, d, None, Local::now()).unwrap().item_id);
        let names: Vec<&str> = it.attachments.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Offer.pdf", "offer-2.pdf"]);
    }

    #[test]
    fn eleven_attachments_rejected() {
        let (_r, store, files) = setup("add7");
        fs::write(files.join("f"), "x").unwrap();
        let mut d = draft();
        d.attachments = (0..11)
            .map(|_| files.join("f").to_string_lossy().into())
            .collect();
        assert!(
            add(&store, d, None, Local::now())
                .unwrap_err()
                .0
                .contains("11 > 10")
        );
    }

    #[test]
    fn checks_from_the_agent_are_marked() {
        let (_r, store, _files) = setup("add8");
        let mut d = draft();
        d.checks = Some(Checks {
            body_sha256: sha256_hex(d.body.as_bytes()),
            verdict: Verdict::Warn,
            findings: vec![],
            ran_at: None,
            source: Some(CheckSource::Command),
            content_digest: None,
        });
        let it = valid(&store, &add(&store, d, None, Local::now()).unwrap().item_id);
        assert_eq!(it.checks.unwrap().source, Some(CheckSource::Agent));
    }

    #[test]
    fn findings_from_the_agent_lose_control_and_bidi_characters() {
        let (_r, store, _files) = setup("add8b");
        let mut d = draft();
        d.checks = Some(Checks {
            body_sha256: sha256_hex(d.body.as_bytes()),
            verdict: Verdict::Block,
            findings: vec![crate::item::Finding {
                level: crate::item::Level::Block,
                title: "Approved\u{202e}deviaw".into(),
                detail: "no\u{200b}thing".into(),
                quote: "\u{1b}[31m".into(),
                confirm: true,
            }],
            ran_at: None,
            source: None,
            content_digest: None,
        });
        let it = valid(&store, &add(&store, d, None, Local::now()).unwrap().item_id);
        let f = &it.checks.unwrap().findings[0];
        assert_eq!(
            (f.title.as_str(), f.detail.as_str(), f.quote.as_str()),
            ("Approveddeviaw", "nothing", "[31m")
        );
    }

    fn first(store: &Store) -> String {
        add(store, draft(), None, Local::now()).unwrap().item_id
    }

    #[test]
    fn replace_withdraws_ready() {
        let (_r, store, _files) = setup("rep1");
        let old = first(&store);
        let out = add(&store, draft(), Some(&old), Local::now()).unwrap();
        assert_eq!(out.replaced, Some(ReplaceOutcome::Withdrawn(old.clone())));
        let it = valid(&store, &old);
        assert_eq!(it.state, State::Discarded);
        assert_eq!(it.history.last().unwrap().event, "superseded");
        assert_eq!(it.history.last().unwrap().detail, out.item_id);
        let audit = fs::read_to_string(store.dir().join("audit.jsonl")).unwrap();
        assert!(audit.contains("\"superseded\""));
    }

    #[test]
    fn replace_withdraws_scheduled() {
        let (_r, store, _files) = setup("rep2");
        let old = first(&store);
        let it = valid(&store, &old);
        store
            .schedule_send(&old, &content_digest(&it), it.content_revision, false)
            .unwrap()
            .unwrap();
        let out = add(&store, draft(), Some(&old), Local::now()).unwrap();
        assert_eq!(out.replaced, Some(ReplaceOutcome::Withdrawn(old.clone())));
        let it = valid(&store, &old);
        assert_eq!(it.state, State::Discarded);
        assert_eq!(it.send.token, None);
        assert_eq!(it.send.approved_digest, None);
    }

    #[test]
    fn replace_blocked_when_window_edited() {
        let (_r, store, _files) = setup("rep3");
        let old = first(&store);
        store
            .mutate(&old, &[], |i| {
                i.edits.edited = true;
                Ok(())
            })
            .unwrap();
        let out = add(&store, draft(), Some(&old), Local::now()).unwrap();
        assert_eq!(
            out.replaced,
            Some(ReplaceOutcome::BlockedWindowEdited(old.clone()))
        );
        assert_eq!(valid(&store, &old).state, State::Ready);
    }

    #[test]
    fn replace_too_late_when_sending() {
        for state in [State::Sending, State::Sent] {
            let (_r, store, _files) = setup("rep4");
            let old = first(&store);
            store
                .mutate(&old, &[], |i| {
                    i.state = state;
                    Ok(())
                })
                .unwrap();
            let out = add(&store, draft(), Some(&old), Local::now()).unwrap();
            assert_eq!(out.replaced, Some(ReplaceOutcome::TooLate(old.clone())));
            assert_eq!(valid(&store, &old).state, state);
        }
        let (_r, store, _files) = setup("rep5");
        let old = first(&store);
        store.archive(&old).unwrap();
        let out = add(&store, draft(), Some(&old), Local::now()).unwrap();
        assert_eq!(out.replaced, Some(ReplaceOutcome::TooLate(old)));
    }

    #[test]
    fn replace_other_session_not_found() {
        let (_r, store, _files) = setup("rep6");
        let old = first(&store);
        let mut d = draft();
        d.origin = Some(Origin {
            session: Some("s2".into()),
            ..Default::default()
        });
        let out = add(&store, d, Some(&old), Local::now()).unwrap();
        assert_eq!(out.replaced, Some(ReplaceOutcome::NotFound(old.clone())));
        assert_eq!(valid(&store, &old).state, State::Ready);
        let mut d = draft();
        d.origin = None;
        assert_eq!(
            add(&store, d, Some(&old), Local::now()).unwrap().replaced,
            Some(ReplaceOutcome::NotFound(old.clone()))
        );
        assert_eq!(
            add(&store, draft(), Some("../etc"), Local::now())
                .unwrap()
                .replaced,
            Some(ReplaceOutcome::NotFound("../etc".into()))
        );
    }

    fn correlated(agent: &str, correlation: &str) -> Draft {
        let mut d = draft();
        d.origin = Some(Origin {
            agent: Some(agent.into()),
            session: Some("s1".into()),
            correlation: Some(correlation.into()),
        });
        d
    }

    fn add_id(store: &Store, d: Draft) -> Result<String> {
        add(store, d, None, Local::now()).map(|o| o.item_id)
    }

    #[test]
    fn same_correlation_same_content_returns_existing() {
        let (_r, store, files) = setup("cor1");
        fs::write(files.join("offer.pdf"), b"%PDF-1.4").unwrap();
        let with_file = || {
            let mut d = correlated("inbox/helper", "c1");
            d.attachments = vec![files.join("offer.pdf").to_string_lossy().into()];
            d
        };
        let first = add_id(&store, with_file()).unwrap();
        let again = add(&store, with_file(), None, Local::now()).unwrap();
        assert_eq!(
            again,
            AddOutcome {
                item_id: first.clone(),
                replaced: None
            }
        );
        assert_eq!(queue_names(&store), vec![first]);
    }

    #[test]
    fn same_correlation_same_content_returns_existing_also_after_archive() {
        let (_r, store, _files) = setup("cor2");
        let first = add_id(&store, correlated("inbox/helper", "c1")).unwrap();
        store.archive(&first).unwrap();
        assert_eq!(
            add_id(&store, correlated("inbox/helper", "c1")).unwrap(),
            first
        );
        assert!(queue_names(&store).is_empty());
    }

    #[test]
    fn different_content_is_conflict() {
        let (_r, store, files) = setup("cor3");
        let first = add_id(&store, correlated("inbox/helper", "c1")).unwrap();
        fs::write(files.join("offer.pdf"), b"%PDF-1.4").unwrap();
        let changes: Vec<fn(&mut Draft, &Path)> = vec![
            |d, _| d.body = "Hello again\n".into(),
            |d, _| d.subject = "Re: Offer 2".into(),
            |d, _| d.to = vec!["carl@example.org".into()],
            |d, f| d.attachments = vec![f.join("offer.pdf").to_string_lossy().into()],
        ];
        for (k, change) in changes.iter().enumerate() {
            let mut d = correlated("inbox/helper", "c1");
            change(&mut d, &files);
            let e = add_id(&store, d).unwrap_err();
            assert!(
                e.0.contains("correlation conflict: different content"),
                "case {k}: {e:?}"
            );
        }
        assert_eq!(queue_names(&store), vec![first]);
    }

    #[test]
    fn window_edits_make_no_conflict() {
        // The automation retries with what it sent first; the person's edit in the window stays.
        let (_r, store, _files) = setup("cor4");
        let first = add_id(&store, correlated("inbox/helper", "c1")).unwrap();
        store
            .mutate(&first, &[], |i| {
                i.body = "Edited in the window\n".into();
                i.body_sha256 = sha256_hex(i.body.as_bytes());
                i.edits.edited = true;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            add_id(&store, correlated("inbox/helper", "c1")).unwrap(),
            first
        );
        assert_eq!(valid(&store, &first).body, "Edited in the window\n");
    }

    #[test]
    fn no_correlation_always_new() {
        let (_r, store, _files) = setup("cor5");
        let a = add_id(&store, draft()).unwrap();
        let b = add_id(&store, draft()).unwrap();
        assert_ne!(a, b);
        assert_eq!(queue_names(&store).len(), 2);
    }

    #[test]
    fn other_account_or_agent_is_new() {
        let root = temp_root("cor6");
        let cfg = crate::config::Config::parse(
            r#"{"accounts":[
              {"name":"work","kind":"command","senders":[{"address":"me@example.org","name":"Me"}],
               "send_command":["/bin/cat"]},
              {"name":"home","kind":"command","senders":[{"address":"me@example.org","name":"Me"}],
               "send_command":["/bin/cat"]}]}"#,
        )
        .unwrap();
        let store = Store::open(&root.0, cfg).unwrap();
        let first = add_id(&store, correlated("inbox/helper", "c1")).unwrap();
        let other_agent = add_id(&store, correlated("inbox/other", "c1")).unwrap();
        let mut d = correlated("inbox/helper", "c1");
        d.account = "home".into();
        let other_account = add_id(&store, d).unwrap();
        let other_correlation = add_id(&store, correlated("inbox/helper", "c2")).unwrap();
        let ids: BTreeSet<String> = [first, other_agent, other_account, other_correlation].into();
        assert_eq!(ids.len(), 4);
        assert_eq!(queue_names(&store).len(), 4);
    }

    #[test]
    fn retry_with_replaces_still_withdraws_the_old_version() {
        // The first run wrote its item and died before withdrawing the old version: the retry finishes the job.
        let (_r, store, _files) = setup("cor9");
        let old = first(&store);
        let new = add_id(&store, correlated("inbox/helper", "c1")).unwrap();
        assert_eq!(valid(&store, &old).state, State::Ready);
        let again = add(
            &store,
            correlated("inbox/helper", "c1"),
            Some(&old),
            Local::now(),
        )
        .unwrap();
        assert_eq!(again.item_id, new);
        assert_eq!(again.replaced, Some(ReplaceOutcome::Withdrawn(old.clone())));
        let it = valid(&store, &old);
        assert_eq!(it.state, State::Discarded);
        assert_eq!(it.history.last().unwrap().detail, new);
    }

    #[test]
    fn a_retry_that_names_its_own_item_as_replaced_keeps_it() {
        let (_r, store, _files) = setup("cor11");
        let first = add_id(&store, correlated("inbox/helper", "c1")).unwrap();
        let again = add(
            &store,
            correlated("inbox/helper", "c1"),
            Some(&first),
            Local::now(),
        )
        .unwrap();
        assert_eq!(
            again,
            AddOutcome {
                item_id: first.clone(),
                replaced: None
            }
        );
        assert_eq!(valid(&store, &first).state, State::Ready);
    }

    #[test]
    fn checks_from_the_agent_go_stale_on_a_subject_change() {
        let (_r, store, _files) = setup("add13");
        let mut d = draft();
        d.checks = Some(Checks {
            body_sha256: sha256_hex(d.body.as_bytes()),
            verdict: Verdict::Pass,
            findings: vec![],
            ran_at: None,
            source: None,
            content_digest: None,
        });
        let id = add(&store, d, None, Local::now()).unwrap().item_id;
        let it = valid(&store, &id);
        assert!(!crate::checks::is_stale(it.checks.as_ref().unwrap(), &it));
        let edit = crate::edit::Edit {
            subject: Some("Re: Another offer".into()),
            ..Default::default()
        };
        let it = crate::edit::save_edits(&store, &id, it.content_revision, &edit).unwrap();
        assert!(crate::checks::is_stale(it.checks.as_ref().unwrap(), &it));
    }

    #[test]
    fn an_item_the_lookup_cannot_read_stops_the_add() {
        // A sent item it cannot check could be the one this retry repeats: refuse rather than queue a second copy.
        for (k, spoil) in [
            |p: &Path| fs::set_permissions(p, fs::Permissions::from_mode(0o000)).unwrap(),
            |p: &Path| fs::write(p, "{ not json").unwrap(),
        ]
        .iter()
        .enumerate()
        {
            let (_r, store, _files) = setup("cor10");
            let first = add_id(&store, correlated("inbox/helper", "c1")).unwrap();
            let archived = store.archive(&first).unwrap();
            spoil(&archived.join(ITEM_FILE));
            let e = add_id(&store, correlated("inbox/helper", "c1")).unwrap_err();
            assert!(e.0.contains("correlation lookup"), "case {k}: {e:?}");
            assert!(queue_names(&store).is_empty(), "case {k}");
        }
    }

    #[test]
    fn two_items_with_one_correlation_are_ambiguous() {
        let (_r, store, _files) = setup("cor7");
        let first = add_id(&store, correlated("inbox/helper", "c1")).unwrap();
        // `add` never makes a second one; place a copy on disk.
        let mut twin = valid(&store, &first);
        twin.item_id = "20261106-223000-0000abcd".into();
        crate::store::tests::put(&store, &twin);
        let e = add_id(&store, correlated("inbox/helper", "c1")).unwrap_err();
        assert!(e.0.contains("ambiguous correlation"), "{e:?}");
        assert_eq!(queue_names(&store).len(), 2);
    }

    #[test]
    fn parallel_adds_of_one_correlation_make_one_item() {
        let (_r, store, _files) = setup("cor8");
        let ids: BTreeSet<String> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| add_id(&store, correlated("inbox/helper", "c1")).unwrap()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(ids.len(), 1);
        assert_eq!(queue_names(&store).len(), 1);
    }

    #[test]
    fn tmp_dirs_are_not_listed() {
        let (_r, store, _files) = setup("add9");
        let tmp = store
            .dir()
            .join("queue")
            .join("20261106-223000-0000abcd.tmp");
        fs::create_dir_all(&tmp).unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(store.list().is_empty());
    }
}
