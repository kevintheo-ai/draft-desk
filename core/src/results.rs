//! What happened to a draft, for the agent that handed it over: `results/<item_id>.json`, signed with a key from
//! the keychain so that a file dropped there by another process is never reported.
//!
//! Honest limit: a process running as the same user that may read the keychain can read the key too. The
//! signature closes the realistic case, a process that drops a file without touching the keychain.

use crate::item::{ITEM_ID_RE, Item, TransportOutcome};
use crate::secrets::Secrets;
use crate::store::{AuditLine, Store, write_atomic};
use crate::{Error, Result, hex};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const RESULT_SCHEMA: &str = "draftdesk-result/1";
/// Keychain entry (service "Draft Desk") holding the signing key.
pub const KEY_NAME: &str = "result-signing-key";
const MAX_RESULT_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultStatus {
    Sent,
    Failed,
    Discarded,
    /// The transport may or may not have the mail: check the sent folder, never resend automatically.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultRecord {
    pub schema: String,
    pub item_id: String,
    pub status: ResultStatus,
    pub account: String,
    pub origin_session: Option<String>,
    pub sent_at: Option<String>,
    pub message_id: Option<String>,
    pub provider_message_id: Option<String>,
    pub rcpt_count: usize,
    pub error_code: Option<String>,
    pub transport_outcome: Option<TransportOutcome>,
    pub retry_allowed: bool,
    #[serde(default)]
    pub sig: String,
}

impl ResultRecord {
    pub fn for_item(item: &Item, status: ResultStatus) -> ResultRecord {
        ResultRecord {
            schema: RESULT_SCHEMA.into(),
            item_id: item.item_id.clone(),
            status,
            account: item.account.clone(),
            origin_session: item.origin.as_ref().and_then(|o| o.session.clone()),
            sent_at: item.send.sent_at.clone(),
            message_id: item.send.message_id.clone(),
            provider_message_id: item.send.provider_message_id.clone(),
            rcpt_count: item.to.len() + item.cc.len() + item.bcc.len(),
            error_code: item.send.error_code.clone(),
            transport_outcome: item.send.transport_outcome,
            retry_allowed: false,
            sig: String::new(),
        }
    }
}

/// Creates the signing key once. Only the app calls this, at start: writing a result never creates a key.
pub fn ensure_result_key(secrets: &dyn Secrets) -> Result<()> {
    if secrets.get(KEY_NAME)?.is_none() {
        secrets.set(KEY_NAME, &crate::random_hex(32))?;
    }
    Ok(())
}

/// HMAC-SHA256 over `draftdesk-result/1\n<item_id>\n<canonical JSON without sig>`. The item id inside the
/// material binds a result to its item, so a copy under another name does not verify.
fn mac(key: &str, r: &ResultRecord) -> Result<Hmac<Sha256>> {
    let mut v = serde_json::to_value(r)?;
    if let Some(o) = v.as_object_mut() {
        o.remove("sig");
    }
    let mut m = Hmac::<Sha256>::new_from_slice(key.as_bytes())
        .map_err(|e| Error(format!("hmac key: {e}")))?;
    m.update(RESULT_SCHEMA.as_bytes());
    m.update(b"\n");
    m.update(r.item_id.as_bytes());
    m.update(b"\n");
    // A Value prints its keys sorted: canonical.
    m.update(v.to_string().as_bytes());
    Ok(m)
}

fn signature_ok(key: &str, r: &ResultRecord) -> bool {
    let Some(given) = unhex(&r.sig) else {
        return false;
    };
    given.len() == 32 && mac(key, r).is_ok_and(|m| m.verify_slice(&given).is_ok())
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A result file: no symlink, a regular file, at most 64 KiB, our schema.
fn read_file(path: &Path) -> Option<ResultRecord> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = f.metadata().ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_RESULT_BYTES {
        return None;
    }
    let mut buf = Vec::new();
    f.take(MAX_RESULT_BYTES + 1).read_to_end(&mut buf).ok()?;
    let r: ResultRecord = serde_json::from_slice(&buf).ok()?;
    (r.schema == RESULT_SCHEMA).then_some(r)
}

fn result_path(store: &Store, id: &str, suffix: &str) -> PathBuf {
    store
        .dir()
        .join("results")
        .join(format!("{id}.json{suffix}"))
}

/// Writes the signed result. `Ok(false)`: no key, nothing published (the caller notes `result_skipped`).
/// Takes the item lock itself: never call while holding it.
pub fn publish(store: &Store, secrets: &dyn Secrets, r: &ResultRecord) -> Result<bool> {
    if !ITEM_ID_RE.is_match(&r.item_id) {
        return Err(Error(format!("invalid item id: {:?}", r.item_id)));
    }
    let Some(key) = secrets.get(KEY_NAME)? else {
        return Ok(false);
    };
    let mut signed = r.clone();
    signed.sig = hex(&mac(&key, &signed)?.finalize().into_bytes());
    let mut json = serde_json::to_string_pretty(&signed)?;
    json.push('\n');
    let _guard = store.lock(&r.item_id, true)?;
    // Delivered already (one marker per status): a second copy would wait behind the marker and go out again once
    // housekeeping removes the marker.
    let delivered = result_path(
        store,
        &r.item_id,
        &format!(".{}-delivered", status_name(r.status)),
    );
    if fs::symlink_metadata(&delivered).is_ok() {
        return Ok(true);
    }
    write_atomic(
        &store.dir().join("results"),
        &format!("{}.json", r.item_id),
        json.as_bytes(),
    )?;
    Ok(true)
}

/// The newest result of an item with a valid signature: unclaimed first, then claimed.
pub fn read_verified(
    store: &Store,
    secrets: &dyn Secrets,
    id: &str,
) -> Result<Option<ResultRecord>> {
    if !ITEM_ID_RE.is_match(id) {
        return Ok(None);
    }
    let Some(key) = secrets.get(KEY_NAME)? else {
        return Ok(None);
    };
    // The unclaimed file and every delivered marker; the newest valid one wins.
    let plain = format!("{id}.json");
    let marker = format!("{id}.json.");
    let mut found: Vec<(std::time::SystemTime, ResultRecord)> =
        fs::read_dir(store.dir().join("results"))?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n == plain || (n.starts_with(&marker) && n.ends_with("-delivered"))
            })
            .filter_map(|e| {
                let r = read_file(&e.path())?;
                let mtime = fs::symlink_metadata(e.path())
                    .and_then(|m| m.modified())
                    .ok()?;
                (r.item_id == id && signature_ok(&key, &r)).then_some((mtime, r))
            })
            .collect();
    found.sort_by_key(|(t, _)| *t);
    Ok(found.pop().map(|(_, r)| r))
}

fn status_name(s: ResultStatus) -> &'static str {
    match s {
        ResultStatus::Sent => "sent",
        ResultStatus::Failed => "failed",
        ResultStatus::Discarded => "discarded",
        ResultStatus::Unknown => "unknown",
    }
}

/// Hands every validly signed result of `session` to the caller exactly once (renamed to `.delivered`), oldest first;
/// with a `limit` at most that many, the rest stays for the next claim (a caller that can show only a few).
pub fn claim_session(
    store: &Store,
    secrets: &dyn Secrets,
    session: &str,
    limit: Option<usize>,
) -> Result<Vec<ResultRecord>> {
    let Some(key) = secrets.get(KEY_NAME)? else {
        return Ok(vec![]);
    };
    let mut ids: Vec<String> = fs::read_dir(store.dir().join("results"))?
        .filter_map(|e| {
            e.ok()?
                .file_name()
                .into_string()
                .ok()?
                .strip_suffix(".json")
                .map(String::from)
        })
        .filter(|id| ITEM_ID_RE.is_match(id))
        .collect();
    ids.sort();
    // From the first rename on, an `Err` would cost the caller the results already marked delivered: a failing item
    // is skipped instead and stays for the next claim.
    let mut out = Vec::new();
    for id in ids {
        if limit.is_some_and(|n| out.len() >= n) {
            break;
        }
        let Ok(_guard) = store.lock(&id, true) else {
            continue;
        };
        let path = result_path(store, &id, "");
        let Some(r) = read_file(&path) else { continue };
        if r.item_id != id || !signature_ok(&key, &r) {
            // ponytail: an audit write error is dropped; the forged file stays and is reported again next time.
            let _ = store.audit(AuditLine {
                ts: crate::now_utc(),
                event: "result_signature_invalid".into(),
                item_id: id,
                ..Default::default()
            });
            continue;
        }
        if r.origin_session.as_deref() != Some(session) {
            continue;
        }
        // One marker per status: a later result of the same item (failed, then discarded) is delivered too.
        let target = result_path(store, &id, &format!(".{}-delivered", status_name(r.status)));
        // Every claimer holds the item lock: a marker is never overwritten.
        if fs::symlink_metadata(&target).is_ok() || fs::rename(&path, &target).is_err() {
            continue;
        }
        out.push(r);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::tests::item;
    use crate::secrets::MemorySecrets;
    use crate::store::tests::{TempRoot, store_with_item};

    fn setup(name: &str) -> (TempRoot, Store, MemorySecrets, ResultRecord) {
        let (root, store, _id) = store_with_item(name);
        let secrets = MemorySecrets::default();
        ensure_result_key(&secrets).unwrap();
        (
            root,
            store,
            secrets,
            ResultRecord::for_item(&item(), ResultStatus::Sent),
        )
    }

    fn file(store: &Store, id: &str) -> PathBuf {
        store.dir().join("results").join(format!("{id}.json"))
    }

    #[test]
    fn ensure_key_creates_once() {
        let s = MemorySecrets::default();
        ensure_result_key(&s).unwrap();
        let k = s.get(KEY_NAME).unwrap().unwrap();
        assert_eq!(k.len(), 64);
        ensure_result_key(&s).unwrap();
        assert_eq!(s.get(KEY_NAME).unwrap().unwrap(), k);
    }

    #[test]
    fn publish_without_key_returns_false() {
        let (_r, store, _s, rec) = setup("res1");
        assert!(!publish(&store, &MemorySecrets::default(), &rec).unwrap());
        assert!(!file(&store, &rec.item_id).exists());
    }

    #[test]
    fn signature_roundtrip() {
        let (_r, store, s, rec) = setup("res2");
        assert!(publish(&store, &s, &rec).unwrap());
        let got = read_verified(&store, &s, &rec.item_id).unwrap().unwrap();
        assert_eq!(got.sig.len(), 64);
        assert_eq!(
            ResultRecord {
                sig: String::new(),
                ..got
            },
            rec
        );
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(file(&store, &rec.item_id)).unwrap().mode() & 0o777,
            0o600
        );
        // A different key does not verify.
        let other = MemorySecrets::default();
        ensure_result_key(&other).unwrap();
        assert_eq!(read_verified(&store, &other, &rec.item_id).unwrap(), None);
    }

    #[test]
    fn tampered_field_fails() {
        let (_r, store, s, rec) = setup("res3");
        publish(&store, &s, &rec).unwrap();
        let p = file(&store, &rec.item_id);
        let text = fs::read_to_string(&p)
            .unwrap()
            .replace("\"sent\"", "\"failed\"");
        fs::write(&p, text).unwrap();
        assert_eq!(read_verified(&store, &s, &rec.item_id).unwrap(), None);
    }

    #[test]
    fn moved_to_other_item_fails() {
        let (_r, store, s, rec) = setup("res4");
        publish(&store, &s, &rec).unwrap();
        let other = "20261106-223000-ffffffff";
        fs::copy(file(&store, &rec.item_id), file(&store, other)).unwrap();
        assert_eq!(read_verified(&store, &s, other).unwrap(), None);
        let text = fs::read_to_string(file(&store, other))
            .unwrap()
            .replace(&rec.item_id, other);
        fs::write(file(&store, other), text).unwrap();
        assert_eq!(read_verified(&store, &s, other).unwrap(), None);
    }

    #[test]
    fn claim_only_own_session() {
        let (_r, store, s, rec) = setup("res5");
        publish(&store, &s, &rec).unwrap();
        assert!(
            claim_session(&store, &s, "someone-else", None)
                .unwrap()
                .is_empty()
        );
        assert!(file(&store, &rec.item_id).exists());
        let got = claim_session(&store, &s, "s1", None).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].item_id, rec.item_id);
    }

    #[test]
    fn claim_renames_once() {
        let (_r, store, s, rec) = setup("res6");
        publish(&store, &s, &rec).unwrap();
        assert_eq!(claim_session(&store, &s, "s1", None).unwrap().len(), 1);
        assert!(claim_session(&store, &s, "s1", None).unwrap().is_empty());
        let delivered = store
            .dir()
            .join("results")
            .join(format!("{}.json.sent-delivered", rec.item_id));
        assert!(delivered.exists() && !file(&store, &rec.item_id).exists());
        // Status still reads the claimed result.
        assert_eq!(
            read_verified(&store, &s, &rec.item_id)
                .unwrap()
                .unwrap()
                .status,
            ResultStatus::Sent
        );
        // The same status once more (a restart publishing what it takes for missing): delivered already, so nothing
        // is written and nothing waits behind the marker.
        publish(&store, &s, &rec).unwrap();
        assert!(claim_session(&store, &s, "s1", None).unwrap().is_empty());
        assert!(!file(&store, &rec.item_id).exists());
    }

    #[test]
    fn failed_then_discarded_delivers_both() {
        let (_r, store, s, rec) = setup("res10");
        publish(
            &store,
            &s,
            &ResultRecord {
                status: ResultStatus::Failed,
                ..rec.clone()
            },
        )
        .unwrap();
        assert_eq!(
            claim_session(&store, &s, "s1", None).unwrap()[0].status,
            ResultStatus::Failed
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        publish(
            &store,
            &s,
            &ResultRecord {
                status: ResultStatus::Discarded,
                ..rec.clone()
            },
        )
        .unwrap();
        assert_eq!(
            claim_session(&store, &s, "s1", None).unwrap()[0].status,
            ResultStatus::Discarded
        );
        assert_eq!(
            read_verified(&store, &s, &rec.item_id)
                .unwrap()
                .unwrap()
                .status,
            ResultStatus::Discarded
        );
    }

    #[test]
    fn unknown_then_sent_delivers_both() {
        let (_r, store, s, rec) = setup("res7");
        publish(
            &store,
            &s,
            &ResultRecord {
                status: ResultStatus::Unknown,
                ..rec.clone()
            },
        )
        .unwrap();
        assert_eq!(
            claim_session(&store, &s, "s1", None).unwrap()[0].status,
            ResultStatus::Unknown
        );
        publish(&store, &s, &rec).unwrap();
        assert_eq!(
            claim_session(&store, &s, "s1", None).unwrap()[0].status,
            ResultStatus::Sent
        );
        assert_eq!(
            read_verified(&store, &s, &rec.item_id)
                .unwrap()
                .unwrap()
                .status,
            ResultStatus::Sent
        );
    }

    #[test]
    fn invalid_signature_is_audited_not_delivered() {
        let (_r, store, s, rec) = setup("res8");
        let mut forged = rec.clone();
        forged.sig = "0".repeat(64);
        fs::write(
            file(&store, &rec.item_id),
            serde_json::to_string(&forged).unwrap(),
        )
        .unwrap();
        assert!(claim_session(&store, &s, "s1", None).unwrap().is_empty());
        assert!(
            fs::read_to_string(store.dir().join("audit.jsonl"))
                .unwrap()
                .contains("result_signature_invalid")
        );
    }

    #[test]
    fn a_result_already_delivered_is_not_written_again() {
        // Otherwise the copy waits behind the marker and goes out a second time once housekeeping removes the marker.
        let (_r, store, s, rec) = setup("res12");
        publish(&store, &s, &rec).unwrap();
        assert_eq!(claim_session(&store, &s, "s1", None).unwrap().len(), 1);
        assert!(publish(&store, &s, &rec).unwrap());
        assert!(!file(&store, &rec.item_id).exists());
    }

    #[test]
    fn a_failing_item_never_costs_the_results_already_delivered() {
        let (_r, store, s, rec) = setup("res11");
        let with_id = |id: &str| ResultRecord {
            item_id: id.into(),
            ..rec.clone()
        };
        let (forged, locked, last) = (
            "20261106-223000-0b000000",
            "20261106-223000-0c000000",
            "20261106-223000-0d000000",
        );
        for r in [rec.clone(), with_id(locked), with_id(last)] {
            publish(&store, &s, &r).unwrap();
        }
        // An invalid signature whose audit line cannot be written, and an item whose lock cannot be opened.
        let bad = ResultRecord {
            sig: "0".repeat(64),
            ..with_id(forged)
        };
        fs::write(file(&store, forged), serde_json::to_string(&bad).unwrap()).unwrap();
        let audit = store.dir().join("audit.jsonl");
        fs::remove_file(&audit).unwrap();
        fs::create_dir(&audit).unwrap();
        let lock = store
            .dir()
            .join("locks")
            .join(format!("item-{locked}.lock"));
        fs::remove_file(&lock).unwrap();
        fs::create_dir(&lock).unwrap();
        let ids = |v: Vec<ResultRecord>| v.into_iter().map(|r| r.item_id).collect::<Vec<_>>();
        assert_eq!(
            ids(claim_session(&store, &s, "s1", None).unwrap()),
            vec![rec.item_id.clone(), last.to_string()]
        );
        // The skipped one stays claimable.
        fs::remove_dir(&lock).unwrap();
        assert_eq!(
            ids(claim_session(&store, &s, "s1", None).unwrap()),
            vec![locked.to_string()]
        );
    }

    #[test]
    fn oversized_or_symlink_ignored() {
        let (_r, store, s, rec) = setup("res9");
        publish(&store, &s, &rec).unwrap();
        let p = file(&store, &rec.item_id);
        let real = store.dir().join("elsewhere.json");
        fs::rename(&p, &real).unwrap();
        std::os::unix::fs::symlink(&real, &p).unwrap();
        assert_eq!(read_verified(&store, &s, &rec.item_id).unwrap(), None);
        assert!(claim_session(&store, &s, "s1", None).unwrap().is_empty());
        fs::remove_file(&p).unwrap();
        let mut big = fs::read_to_string(&real).unwrap();
        big.push_str(&" ".repeat(70_000));
        fs::write(&p, big).unwrap();
        assert_eq!(read_verified(&store, &s, &rec.item_id).unwrap(), None);
        assert!(claim_session(&store, &s, "s1", None).unwrap().is_empty());
    }
}
