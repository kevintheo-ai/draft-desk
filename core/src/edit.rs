//! Changes made in the window: header fields and text, attachments, check results. Only a `ready` item changes, only
//! against the content revision the window showed, and nothing is written when nothing changed (looking at a draft
//! must not mark it as edited).

use crate::add::unique_name;
use crate::address;
use crate::item::{
    Attachment, CheckSource, Checks, Item, ItemRead, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS,
    MAX_TOTAL_ATTACHMENT_BYTES, State, attachment_name_ok,
};
use crate::store::{
    Store, mkdir_private, open_dir_nofollow, push_history, read_regular, unlink_at, write_new_at,
};
use crate::{Result, bail, sha256_hex};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// At most this many files from one drop or one file dialog.
pub const MAX_DROPPED_PATHS: usize = 25;

/// What adding attachments did: the saved item, the names added and every file left out with the reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Attached {
    pub item: Item,
    pub added: Vec<String>,
    /// (path, reason)
    pub skipped: Vec<(String, String)>,
}

// Names in `edits.fields_changed`. The IMAP cleanup reads `to`, `cc`, `bcc` and `subject` (transport/smtp.rs).
pub const FIELD_FROM_ADDRESS: &str = "from_address";
pub const FIELD_FROM_NAME: &str = "from_name";
pub const FIELD_TO: &str = "to";
pub const FIELD_CC: &str = "cc";
pub const FIELD_BCC: &str = "bcc";
pub const FIELD_SUBJECT: &str = "subject";
pub const FIELD_BODY: &str = "body";
pub const FIELD_ATTACHMENTS: &str = "attachments";

/// What the window's fields hold; recipient fields as their entries (tokens).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub from_address: String,
    pub from_name: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
}

impl Form {
    pub fn of(item: &Item) -> Form {
        Form {
            from_address: item.from.address.clone(),
            from_name: item.from.name.clone(),
            to: item.to.clone(),
            cc: item.cc.clone(),
            bcc: item.bcc.clone(),
            subject: item.subject.clone(),
            body: item.body.clone(),
        }
    }
}

/// The fields to change; `None` leaves a field as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    pub from_address: Option<String>,
    pub from_name: Option<String>,
    pub to: Option<Vec<String>>,
    pub cc: Option<Vec<String>>,
    pub bcc: Option<Vec<String>>,
    pub subject: Option<String>,
    pub body: Option<String>,
}

impl Edit {
    pub fn is_empty(&self) -> bool {
        *self == Edit::default()
    }
}

/// Recipient entries without blanks, trimmed.
fn tokens(list: &[String]) -> Vec<String> {
    list.iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Only what the form changed compared with the item.
pub fn diff(item: &Item, form: &Form) -> Edit {
    let text = |new: &str, old: &str| (new != old).then(|| new.to_string());
    let list = |new: &[String], old: &[String]| {
        let t = tokens(new);
        (t != old).then_some(t)
    };
    Edit {
        from_address: text(&form.from_address, &item.from.address),
        from_name: text(form.from_name.trim(), &item.from.name),
        to: list(&form.to, &item.to),
        cc: list(&form.cc, &item.cc),
        bcc: list(&form.bcc, &item.bcc),
        subject: text(&form.subject, &item.subject),
        body: text(&form.body, &item.body),
    }
}

pub(crate) fn valid(store: &Store, id: &str) -> Result<Item> {
    match store.read(id) {
        ItemRead::Valid(i) => Ok(*i),
        ItemRead::Invalid { errors, .. } => bail!("item {id} is invalid: {}", errors.join("; ")),
    }
}

/// Marks the item as edited in the window: `edits.fields_changed` collects every field ever changed.
pub(crate) fn mark_edited(i: &mut Item, fields: &[&str]) {
    i.edits.edited = true;
    let mut all: BTreeSet<String> = i.edits.fields_changed.drain(..).collect();
    all.extend(fields.iter().map(|f| f.to_string()));
    i.edits.fields_changed = all.into_iter().collect();
}

/// Saves the window's changes. Recipient entries are split and normalized; sender, subject and limits are checked
/// by the store's write guard. Forbidden characters in the text are saved (sending refuses them, the window shows and
/// removes them).
pub fn save_edits(
    store: &Store,
    id: &str,
    expected_content_revision: u64,
    edit: &Edit,
) -> Result<Item> {
    let current = valid(store, id)?;
    let norm = |label: &str, v: &Option<Vec<String>>| {
        v.as_ref()
            .map(|l| address::normalize_list(label, l))
            .transpose()
    };
    let (to, cc, bcc) = (
        norm(FIELD_TO, &edit.to)?,
        norm(FIELD_CC, &edit.cc)?,
        norm(FIELD_BCC, &edit.bcc)?,
    );
    let apply = |i: &mut Item| -> Vec<&'static str> {
        let mut changed = Vec::new();
        if let Some(v) = &edit.from_address
            && *v != i.from.address
        {
            i.from.address = v.clone();
            changed.push(FIELD_FROM_ADDRESS);
        }
        if let Some(v) = &edit.from_name
            && v.trim() != i.from.name
        {
            i.from.name = v.trim().to_string();
            changed.push(FIELD_FROM_NAME);
        }
        if let Some(v) = &to
            && *v != i.to
        {
            i.to = v.clone();
            changed.push(FIELD_TO);
        }
        if let Some(v) = &cc
            && *v != i.cc
        {
            i.cc = v.clone();
            changed.push(FIELD_CC);
        }
        if let Some(v) = &bcc
            && *v != i.bcc
        {
            i.bcc = v.clone();
            changed.push(FIELD_BCC);
        }
        if let Some(v) = &edit.subject
            && *v != i.subject
        {
            i.subject = v.clone();
            changed.push(FIELD_SUBJECT);
        }
        if let Some(v) = &edit.body
            && *v != i.body
        {
            i.body = v.clone();
            i.body_sha256 = sha256_hex(v.as_bytes());
            changed.push(FIELD_BODY);
        }
        changed
    };
    if apply(&mut current.clone()).is_empty() {
        return Ok(current);
    }
    store.mutate(id, &[State::Ready], |i| {
        if i.content_revision != expected_content_revision {
            bail!("content_changed: the draft changed since it was shown");
        }
        let changed = apply(i);
        mark_edited(i, &changed);
        push_history(i, "edited", &changed.join(", "));
        Ok(())
    })
}

fn changed_since_shown() -> crate::Error {
    crate::Error("content_changed: the draft changed since it was shown".into())
}

/// Copies files from the person's disk into the item (dropped or chosen in a dialog). Each file is read once through
/// one descriptor (no symlink, no FIFO wait, a regular file, at most 25 MiB); a file that breaks a rule is left out
/// with its reason, the others are added. Names get `-2`, `-3` on collisions.
pub fn add_attachments(
    store: &Store,
    id: &str,
    expected_content_revision: u64,
    paths: &[PathBuf],
) -> Result<Attached> {
    let current = valid(store, id)?;
    if current.state != State::Ready {
        bail!("not ready: the item is {:?}", current.state);
    }
    if current.content_revision != expected_content_revision {
        return Err(changed_since_shown());
    }
    let mut skipped = Vec::new();
    // Lowercase, as `unique_name` compares them. A file an earlier attach left behind (it died before recording it)
    // keeps its name too: creating never overwrites.
    let mut taken: BTreeSet<String> = current
        .attachments
        .iter()
        .map(|a| a.name.to_lowercase())
        .collect();
    if let Ok(entries) = std::fs::read_dir(store.item_dir(id)?.join("att")) {
        taken.extend(
            entries.filter_map(|e| Some(e.ok()?.file_name().into_string().ok()?.to_lowercase())),
        );
    }
    let mut count = current.attachments.len();
    let mut total: u64 = current.attachments.iter().map(|a| a.size).sum();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for (n, path) in paths.iter().enumerate() {
        let shown = path.display().to_string();
        let mut skip = |why: String| skipped.push((shown.clone(), why));
        if n >= MAX_DROPPED_PATHS {
            skip(format!("more than {MAX_DROPPED_PATHS} files at once"));
            continue;
        }
        let base = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !attachment_name_ok(base) {
            skip("unusable file name".into());
            continue;
        }
        if count >= MAX_ATTACHMENTS {
            skip(format!("at most {MAX_ATTACHMENTS} attachments"));
            continue;
        }
        let bytes = match read_regular(path, MAX_ATTACHMENT_BYTES) {
            Ok(b) => b,
            Err(e) => {
                skip(format!("{e} (symlinks and special files are refused)"));
                continue;
            }
        };
        if total + bytes.len() as u64 > MAX_TOTAL_ATTACHMENT_BYTES {
            skip(format!(
                "attachments together exceed {MAX_TOTAL_ATTACHMENT_BYTES} bytes"
            ));
            continue;
        }
        total += bytes.len() as u64;
        count += 1;
        files.push((unique_name(&mut taken, base), bytes));
    }
    if files.is_empty() {
        return Ok(Attached {
            item: current,
            added: vec![],
            skipped,
        });
    }
    let names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
    let _guard = store.lock(id, true)?;
    let att_path = store.item_dir(id)?.join("att");
    if std::fs::symlink_metadata(&att_path).is_err() {
        mkdir_private(&att_path)?;
    }
    let att = open_dir_nofollow(&att_path)?;
    let mut written = Vec::new();
    let result = (|| -> Result<Item> {
        for (name, bytes) in &files {
            write_new_at(&att, name, bytes)?;
            written.push(name.clone());
        }
        att.sync_all()?;
        store.mutate_locked(id, &[State::Ready], |i| {
            if i.content_revision != expected_content_revision {
                return Err(changed_since_shown());
            }
            for (name, bytes) in &files {
                i.attachments.push(Attachment {
                    name: name.clone(),
                    size: bytes.len() as u64,
                    sha256: sha256_hex(bytes),
                });
            }
            mark_edited(i, &[FIELD_ATTACHMENTS]);
            push_history(i, "attachment_added", &names.join(", "));
            Ok(())
        })
    })();
    match result {
        Ok(item) => Ok(Attached {
            item,
            added: names,
            skipped,
        }),
        Err(e) => {
            for name in &written {
                unlink_at(&att, name);
            }
            Err(e)
        }
    }
}

/// Stores what the check command found. Findings for content that changed meanwhile are stored too and count as
/// stale (`checks::is_stale`): they keep the digest of the snapshot the command ran on, never one of the item now.
/// Checks are no content: the content revision stays.
pub fn record_checks(store: &Store, id: &str, checks: Checks) -> Result<Item> {
    store.mutate(id, &[State::Ready], |i| {
        let verdict = serde_json::to_value(checks.verdict)?;
        push_history(i, "checked", verdict.as_str().unwrap_or(""));
        i.checks = Some(Checks {
            source: Some(CheckSource::Command),
            ..checks
        });
        Ok(())
    })
}

/// Removes one attachment from the item, then its file (no longer referenced).
pub fn remove_attachment(
    store: &Store,
    id: &str,
    expected_content_revision: u64,
    name: &str,
) -> Result<Item> {
    if !attachment_name_ok(name) {
        bail!("unknown attachment {name:?}");
    }
    let _guard = store.lock(id, true)?;
    let item = store.mutate_locked(id, &[State::Ready], |i| {
        if i.content_revision != expected_content_revision {
            return Err(changed_since_shown());
        }
        let before = i.attachments.len();
        i.attachments.retain(|a| a.name != name);
        if i.attachments.len() == before {
            bail!("unknown attachment {name:?}");
        }
        mark_edited(i, &[FIELD_ATTACHMENTS]);
        push_history(i, "attachment_removed", name);
        Ok(())
    })?;
    if let Ok(att) = open_dir_nofollow(&store.item_dir(id)?.join("att")) {
        unlink_at(&att, name);
    }
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::add::{ReplaceOutcome, withdraw};
    use crate::checks::is_stale;
    use crate::item::tests::item;
    use crate::item::{Checks, Verdict};
    use crate::store::tests::store_with_item;
    use crate::text::body_problems;

    fn get(store: &Store, id: &str) -> Item {
        valid(store, id).unwrap()
    }

    fn subject(s: &str) -> Edit {
        Edit {
            subject: Some(s.into()),
            ..Default::default()
        }
    }

    #[test]
    fn form_of_item_diffs_empty() {
        let it = item();
        assert!(diff(&it, &Form::of(&it)).is_empty());
    }

    #[test]
    fn diff_detects_each_field() {
        let it = item();
        let mut f = Form::of(&it);
        f.subject = "Other".into();
        f.to = vec!["bob@example.org".into()];
        f.from_name = "Me Too".into();
        f.body = "Changed".into();
        let e = diff(&it, &f);
        assert_eq!(e.subject.as_deref(), Some("Other"));
        assert_eq!(e.to, Some(vec!["bob@example.org".to_string()]));
        assert_eq!(e.from_name.as_deref(), Some("Me Too"));
        assert_eq!(e.body.as_deref(), Some("Changed"));
        assert_eq!((e.cc, e.bcc, e.from_address), (None, None, None));
    }

    #[test]
    fn diff_ignores_blank_tokens_and_surrounding_spaces() {
        let it = item();
        let mut f = Form::of(&it);
        f.cc = vec!["  ".into()];
        f.to = vec![" Ann <ann@example.org> ".into(), String::new()];
        f.from_name = " Me ".into();
        assert!(diff(&it, &f).is_empty());
    }

    #[test]
    fn unchanged_save_writes_nothing() {
        let (_r, store, id) = store_with_item("ed1");
        let before = get(&store, &id);
        let after = save_edits(&store, &id, 1, &subject(&before.subject)).unwrap();
        assert_eq!(after.state_revision, before.state_revision);
        assert!(!after.edits.edited);
        assert!(save_edits(&store, &id, 1, &Edit::default()).is_ok());
        assert_eq!(get(&store, &id).state_revision, before.state_revision);
    }

    #[test]
    fn save_marks_edited_and_unions_fields() {
        let (_r, store, id) = store_with_item("ed2");
        let a = save_edits(&store, &id, 1, &subject("Re: Offer (v2)")).unwrap();
        assert_eq!(a.content_revision, 2);
        assert!(a.edits.edited);
        assert_eq!(a.edits.fields_changed, vec!["subject"]);
        let body = Edit {
            body: Some("Hello again.\n".into()),
            ..Default::default()
        };
        let b = save_edits(&store, &id, 2, &body).unwrap();
        assert_eq!(b.edits.fields_changed, vec!["body", "subject"]);
        assert_eq!(b.body_sha256, sha256_hex(b"Hello again.\n"));
        let edited: Vec<&str> = b
            .history
            .iter()
            .filter(|h| h.event == "edited")
            .map(|h| h.detail.as_str())
            .collect();
        assert_eq!(edited, vec!["subject", "body"]);
    }

    #[test]
    fn save_splits_and_normalizes_recipients() {
        let (_r, store, id) = store_with_item("ed3");
        let e = Edit {
            to: Some(vec!["Ann <ann@example.org>, bob@example.org".into()]),
            cc: Some(vec!["Carl   <carl@example.org>".into()]),
            ..Default::default()
        };
        let it = save_edits(&store, &id, 1, &e).unwrap();
        assert_eq!(it.to, vec!["Ann <ann@example.org>", "bob@example.org"]);
        assert_eq!(it.cc, vec!["Carl <carl@example.org>"]);
        assert_eq!(it.edits.fields_changed, vec!["cc", "to"]);
    }

    #[test]
    fn invalid_recipient_leaves_item_untouched() {
        let (_r, store, id) = store_with_item("ed4");
        let e = Edit {
            to: Some(vec!["not an address".into()]),
            subject: Some("Changed".into()),
            ..Default::default()
        };
        let err = save_edits(&store, &id, 1, &e).unwrap_err();
        assert!(err.0.starts_with("to[0]: "), "{}", err.0);
        let it = get(&store, &id);
        assert_eq!((it.state_revision, it.subject.as_str()), (1, "Re: Offer"));
    }

    #[test]
    fn foreign_sender_refused_and_fixed_name_enforced() {
        let (_r, store, id) = store_with_item("ed5");
        let from = |a: &str, n: &str| Edit {
            from_address: Some(a.into()),
            from_name: Some(n.into()),
            ..Default::default()
        };
        assert!(save_edits(&store, &id, 1, &from("x@example.org", "X")).is_err());
        assert!(save_edits(&store, &id, 1, &from("boss@example.org", "Someone")).is_err());
        let it = save_edits(&store, &id, 1, &from("boss@example.org", "The Boss")).unwrap();
        assert_eq!(it.edits.fields_changed, vec!["from_address", "from_name"]);
    }

    #[test]
    fn content_revision_conflict_refused() {
        let (_r, store, id) = store_with_item("ed6");
        let err = save_edits(&store, &id, 7, &subject("Changed")).unwrap_err();
        assert!(err.0.contains("content_changed"), "{}", err.0);
        assert_eq!(get(&store, &id).subject, "Re: Offer");
    }

    #[test]
    fn only_ready() {
        let (_r, store, id) = store_with_item("ed7");
        store
            .mutate(&id, &[], |i| {
                i.state = State::Failed;
                Ok(())
            })
            .unwrap();
        assert!(save_edits(&store, &id, 1, &subject("Changed")).is_err());
    }

    #[test]
    fn body_change_makes_checks_stale() {
        let (_r, store, id) = store_with_item("ed8");
        store
            .mutate(&id, &[], |i| {
                i.checks = Some(Checks {
                    body_sha256: i.body_sha256.clone(),
                    verdict: Verdict::Pass,
                    findings: vec![],
                    ran_at: None,
                    source: None,
                    content_digest: None,
                });
                Ok(())
            })
            .unwrap();
        let e = Edit {
            body: Some("New text.\n".into()),
            ..Default::default()
        };
        let it = save_edits(&store, &id, 1, &e).unwrap();
        assert!(is_stale(it.checks.as_ref().unwrap(), &it));
    }

    #[test]
    fn forbidden_chars_are_saved() {
        let (_r, store, id) = store_with_item("ed9");
        let e = Edit {
            body: Some("Hi \u{202E}there\n".into()),
            ..Default::default()
        };
        let it = save_edits(&store, &id, 1, &e).unwrap();
        assert_eq!(body_problems(&it.body).len(), 1);
    }

    #[test]
    fn edited_item_blocks_replace() {
        let (_r, store, id) = store_with_item("ed10");
        save_edits(&store, &id, 1, &subject("Changed")).unwrap();
        let out = withdraw(&store, &id, Some("s1"), "superseded", "new").unwrap();
        assert_eq!(out, ReplaceOutcome::BlockedWindowEdited(id));
    }

    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// A folder with source files next to the store.
    fn src(root: &Path, files: &[(&str, &[u8])]) -> PathBuf {
        let d = root.join("src");
        fs::create_dir_all(&d).unwrap();
        for (name, bytes) in files {
            fs::write(d.join(name), bytes).unwrap();
        }
        d
    }

    fn att_names(store: &Store, id: &str) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(store.item_dir(id).unwrap().join("att"))
            .map(|rd| {
                rd.map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn adds_files_hashes_and_marks_edited() {
        let (r, store, id) = store_with_item("at1");
        let d = src(&r.0, &[("offer.pdf", b"%PDF-1.4")]);
        let out = add_attachments(&store, &id, 1, &[d.join("offer.pdf")]).unwrap();
        assert_eq!(out.added, vec!["offer.pdf"]);
        assert!(out.skipped.is_empty());
        let a = &out.item.attachments[0];
        assert_eq!(
            (a.size, a.sha256.as_str()),
            (8, sha256_hex(b"%PDF-1.4").as_str())
        );
        assert_eq!(out.item.edits.fields_changed, vec!["attachments"]);
        assert_eq!(out.item.content_revision, 2);
        let file = store.item_dir(&id).unwrap().join("att/offer.pdf");
        assert_eq!(fs::read(&file).unwrap(), b"%PDF-1.4");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(get(&store, &id), out.item);
    }

    #[test]
    fn symlink_fifo_and_directory_skipped_with_reason() {
        let (r, store, id) = store_with_item("at2");
        let d = src(&r.0, &[("real.txt", b"x")]);
        std::os::unix::fs::symlink(d.join("real.txt"), d.join("link.txt")).unwrap();
        let fifo = std::ffi::CString::new(d.join("pipe").to_str().unwrap()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        fs::create_dir(d.join("folder")).unwrap();
        let paths = [d.join("link.txt"), d.join("pipe"), d.join("folder")];
        let out = add_attachments(&store, &id, 1, &paths).unwrap();
        assert!(out.added.is_empty());
        assert_eq!(out.skipped.len(), 3, "{:?}", out.skipped);
        assert_eq!(get(&store, &id).state_revision, 1, "nothing written");
    }

    #[test]
    fn eleventh_and_oversize_skipped() {
        let (r, store, id) = store_with_item("at3");
        let names: Vec<String> = (0..11).map(|n| format!("f{n}.txt")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
        let d = src(&r.0, &files);
        let paths: Vec<PathBuf> = names.iter().map(|n| d.join(n)).collect();
        let out = add_attachments(&store, &id, 1, &paths).unwrap();
        assert_eq!(out.added.len(), 10);
        assert!(out.skipped[0].1.contains("at most 10"), "{:?}", out.skipped);

        let (r2, store2, id2) = store_with_item("at3b");
        let d2 = src(&r2.0, &[]);
        for n in ["a.bin", "b.bin"] {
            fs::File::create(d2.join(n))
                .unwrap()
                .set_len(13 * 1024 * 1024)
                .unwrap();
        }
        fs::File::create(d2.join("huge.bin"))
            .unwrap()
            .set_len(MAX_ATTACHMENT_BYTES + 1)
            .unwrap();
        let paths = [d2.join("a.bin"), d2.join("b.bin"), d2.join("huge.bin")];
        let out = add_attachments(&store2, &id2, 1, &paths).unwrap();
        assert_eq!(out.added, vec!["a.bin"]);
        assert!(out.skipped[0].1.contains("together"), "{:?}", out.skipped);
        assert!(out.skipped[1].1.contains("too large"), "{:?}", out.skipped);
    }

    #[test]
    fn name_collision_gets_suffix() {
        let (r, store, id) = store_with_item("at4");
        let d = src(&r.0, &[("a.txt", b"one")]);
        let first = add_attachments(&store, &id, 1, &[d.join("a.txt")]).unwrap();
        let second = add_attachments(&store, &id, 2, &[d.join("a.txt")]).unwrap();
        assert_eq!(first.added, vec!["a.txt"]);
        assert_eq!(second.added, vec!["a-2.txt"]);
        assert_eq!(att_names(&store, &id), vec!["a-2.txt", "a.txt"]);
    }

    #[test]
    fn file_left_by_a_crash_does_not_block_its_name() {
        let (r, store, id) = store_with_item("at4b");
        let d = src(&r.0, &[("a.txt", b"one")]);
        // An earlier attach died between creating the file and recording it.
        let att = store.item_dir(&id).unwrap().join("att");
        mkdir_private(&att).unwrap();
        std::fs::write(att.join("a.txt"), b"").unwrap();
        let out = add_attachments(&store, &id, 1, &[d.join("a.txt")]).unwrap();
        assert_eq!(out.added, vec!["a-2.txt"]);
        assert_eq!(get(&store, &id).attachments[0].name, "a-2.txt");
    }

    #[test]
    fn name_differing_only_in_case_gets_suffix() {
        let (r, store, id) = store_with_item("at4c");
        let d = src(&r.0, &[("Offer.pdf", b"one")]);
        add_attachments(&store, &id, 1, &[d.join("Offer.pdf")]).unwrap();
        let d2 = src(&r.0.join("other"), &[("offer.pdf", b"two")]);
        let out = add_attachments(&store, &id, 2, &[d2.join("offer.pdf")]).unwrap();
        assert_eq!(out.added, vec!["offer-2.pdf"]);
    }

    #[test]
    fn revision_conflict_adds_nothing_and_leaves_no_file() {
        let (r, store, id) = store_with_item("at5");
        let d = src(&r.0, &[("a.txt", b"one")]);
        let err = add_attachments(&store, &id, 5, &[d.join("a.txt")]).unwrap_err();
        assert!(err.0.contains("content_changed"), "{}", err.0);
        assert!(att_names(&store, &id).is_empty());
        assert!(get(&store, &id).attachments.is_empty());
    }

    #[test]
    fn attachments_not_ready_refused() {
        let (r, store, id) = store_with_item("at6");
        let d = src(&r.0, &[("a.txt", b"one")]);
        store
            .mutate(&id, &[], |i| {
                i.state = State::Failed;
                Ok(())
            })
            .unwrap();
        assert!(add_attachments(&store, &id, 1, &[d.join("a.txt")]).is_err());
        assert!(remove_attachment(&store, &id, 1, "a.txt").is_err());
    }

    #[test]
    fn more_than_25_paths_capped() {
        let (r, store, id) = store_with_item("at7");
        let d = src(&r.0, &[("a.txt", b"one")]);
        let mut paths: Vec<PathBuf> = (0..MAX_DROPPED_PATHS)
            .map(|n| d.join(format!("missing-{n}")))
            .collect();
        paths.push(d.join("a.txt"));
        let out = add_attachments(&store, &id, 1, &paths).unwrap();
        assert!(out.added.is_empty(), "the 26th path is never read");
        assert_eq!(out.skipped.len(), 26);
        assert!(
            out.skipped[25].1.contains("at once"),
            "{:?}",
            out.skipped[25]
        );
    }

    fn warn_for(body_sha256: &str) -> Checks {
        Checks {
            body_sha256: body_sha256.into(),
            verdict: Verdict::Warn,
            findings: vec![crate::item::Finding {
                level: crate::item::Level::Warn,
                title: "Tone".into(),
                detail: String::new(),
                quote: String::new(),
                confirm: false,
            }],
            ran_at: Some(crate::now_utc()),
            source: None,
            content_digest: None,
        }
    }

    #[test]
    fn records_command_checks_with_history() {
        let (_r, store, id) = store_with_item("rc1");
        let sha = get(&store, &id).body_sha256;
        let it = record_checks(&store, &id, warn_for(&sha)).unwrap();
        let c = it.checks.as_ref().unwrap();
        assert_eq!(
            (c.source, c.verdict, c.findings.len()),
            (Some(CheckSource::Command), Verdict::Warn, 1)
        );
        let last = it.history.last().unwrap();
        assert_eq!(
            (last.event.as_str(), last.detail.as_str()),
            ("checked", "warn")
        );
        assert_eq!(it.content_revision, 1, "checks are no content");
        assert!(!is_stale(c, &it));
    }

    #[test]
    fn checks_for_an_old_body_are_stale() {
        let (_r, store, id) = store_with_item("rc2");
        let old = get(&store, &id).body_sha256;
        let e = Edit {
            body: Some("Edited while the check ran.\n".into()),
            ..Default::default()
        };
        save_edits(&store, &id, 1, &e).unwrap();
        let it = record_checks(&store, &id, warn_for(&old)).unwrap();
        assert!(is_stale(it.checks.as_ref().unwrap(), &it));
    }

    #[test]
    fn checks_stay_bound_to_the_snapshot_they_ran_on() {
        // The recipients change while the check runs: the findings were made for the old ones.
        let (_r, store, id) = store_with_item("rc4");
        let cmd = crate::config::CommandCfg {
            argv: vec![format!(
                "{}/tests/fixtures/bin/check-warn.sh",
                env!("CARGO_MANIFEST_DIR")
            )],
            timeout_s: 10,
        };
        let shown = get(&store, &id);
        let checks = crate::checks::run_check(&cmd, &shown, None).unwrap();
        let e = Edit {
            to: Some(vec!["bob@example.org".into()]),
            ..Default::default()
        };
        save_edits(&store, &id, shown.content_revision, &e).unwrap();
        let it = record_checks(&store, &id, checks).unwrap();
        assert!(is_stale(it.checks.as_ref().unwrap(), &it));
    }

    #[test]
    fn record_checks_only_when_ready() {
        let (_r, store, id) = store_with_item("rc3");
        let sha = get(&store, &id).body_sha256;
        store
            .mutate(&id, &[], |i| {
                i.state = State::Discarded;
                Ok(())
            })
            .unwrap();
        assert!(record_checks(&store, &id, warn_for(&sha)).is_err());
    }

    // The second write fails because APFS ignores Unicode normalization (the stray NFC name blocks the NFD one),
    // so this runs on the Mac only.
    #[cfg(target_os = "macos")]
    #[test]
    fn failed_write_removes_the_files_written_before() {
        let (r, store, id) = store_with_item("at9");
        let (nfc, nfd) = ("\u{e9}.txt", "e\u{301}.txt");
        let d = src(&r.0, &[("x.txt", b"x"), (nfd, b"y")]);
        let att = store.item_dir(&id).unwrap().join("att");
        mkdir_private(&att).unwrap();
        fs::write(att.join(nfc), b"stray").unwrap();
        assert!(add_attachments(&store, &id, 1, &[d.join("x.txt"), d.join(nfd)]).is_err());
        assert_eq!(att_names(&store, &id), vec![nfc]);
        assert!(get(&store, &id).attachments.is_empty());
    }

    #[test]
    fn remove_drops_entry_and_file() {
        let (r, store, id) = store_with_item("at8");
        let d = src(&r.0, &[("a.txt", b"one"), ("b.txt", b"two")]);
        add_attachments(&store, &id, 1, &[d.join("a.txt"), d.join("b.txt")]).unwrap();
        let it = remove_attachment(&store, &id, 2, "a.txt").unwrap();
        assert_eq!(it.attachments.len(), 1);
        assert_eq!(it.attachments[0].name, "b.txt");
        assert_eq!(it.content_revision, 3);
        assert_eq!(att_names(&store, &id), vec!["b.txt"]);
        assert!(remove_attachment(&store, &id, 3, "nope.txt").is_err());
        assert!(remove_attachment(&store, &id, 3, "../b.txt").is_err());
        assert_eq!(att_names(&store, &id), vec!["b.txt"]);
    }
}
