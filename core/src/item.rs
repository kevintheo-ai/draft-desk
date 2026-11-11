//! The draft an agent hands over (`draftdesk/1`) and the stored item (`draftdesk-item/1`), with validation and the
//! content digest that binds a send click to exactly what was shown.

use crate::address;
use crate::config::Config;
use crate::sha256_hex;
use crate::text::{has_forbidden, has_linebreak};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;

pub const DRAFT_SCHEMA: &str = "draftdesk/1";
pub const ITEM_SCHEMA: &str = "draftdesk-item/1";

pub const MAX_ATTACHMENTS: usize = 10;
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_ITEM_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_BODY_CHARS: usize = 200_000;
pub const MAX_SUBJECT_CHARS: usize = 998;
pub const MAX_ORIGIN_CHARS: usize = 160;
pub const ITEM_FILE: &str = "item.json";

pub static ITEM_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]{8}-[0-9]{6}-[0-9a-f]{8}$").unwrap());
/// `<local@domain>`, visible ASCII only (no control, invisible, direction or other non-ASCII characters), no `<`,
/// `>` or `@` inside the parts.
pub static MESSAGE_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^<[\x21-\x3B\x3D\x3F\x41-\x7E]+@[\x21-\x3B\x3D\x3F\x41-\x7E]+>$").unwrap()
});
pub static PROVIDER_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.:-]{1,128}$").unwrap());
static SHA256_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{64}$").unwrap());
static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{32}$").unwrap());
static ATTEMPT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{16}$").unwrap());

// ---------------------------------------------------------------------------------------------------------------
// Input from an agent

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub schema: String,
    pub account: String,
    pub from: DraftFrom,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// Paths of files to attach; copied into the queue on `add`.
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default)]
    pub reply: Option<Reply>,
    /// Earlier mails as plain text, shown only.
    #[serde(default)]
    pub thread: Option<String>,
    #[serde(default)]
    pub mailbox_draft: Option<MailboxDraft>,
    #[serde(default)]
    pub origin: Option<Origin>,
    #[serde(default)]
    pub checks: Option<Checks>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftFrom {
    pub address: String,
    #[serde(default)]
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------------------------------------------
// Stored item

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Ready,
    SendingScheduled,
    Sending,
    Sent,
    Failed,
    Discarded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub schema: String,
    pub item_id: String,
    pub created_at: String,
    pub state: State,
    pub state_revision: u64,
    pub content_revision: u64,
    pub account: String,
    pub from: From,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub body_sha256: String,
    pub reply: Option<Reply>,
    pub mailbox_draft: Option<MailboxDraft>,
    pub origin: Option<Origin>,
    /// The content digest when the agent added the draft: a later `add` with the same account, `origin.agent` and
    /// `origin.correlation` must bring exactly this content (edits in the window do not count).
    #[serde(default)]
    pub added_digest: Option<String>,
    pub attachments: Vec<Attachment>,
    pub checks: Option<Checks>,
    pub edits: Edits,
    pub send: SendState,
    pub history: Vec<HistoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct From {
    pub address: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub gmail_thread_id: Option<String>,
}

/// The provider draft this item came from, so it can be cleaned up after sending.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailboxDraft {
    #[serde(default)]
    pub gmail_draft_id: Option<String>,
    #[serde(default)]
    pub gmail_message_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub correlation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    /// The file is `queue/<id>/att/<name>`.
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Warn,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Warn,
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub level: Level,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub quote: String,
    /// Sending needs an explicit confirmation while this finding is open.
    #[serde(default)]
    pub confirm: bool,
}

impl Finding {
    /// The finding with its texts free of control, bidi and invisible characters: they come from the agent or the
    /// check command and the window shows them, also in the question before sending.
    pub fn cleaned(self) -> Finding {
        Finding {
            title: crate::text::strip_forbidden(&self.title),
            detail: crate::text::strip_forbidden(&self.detail),
            quote: crate::text::strip_forbidden(&self.quote),
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckSource {
    /// Handed over with the draft.
    Agent,
    /// Produced by the configured check command.
    Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checks {
    /// The body these findings were made for; a different body makes them stale.
    pub body_sha256: String,
    pub verdict: Verdict,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub ran_at: Option<String>,
    #[serde(default)]
    pub source: Option<CheckSource>,
    /// [`content_digest`] of the item the findings were made for: other recipients, subject or attachments make
    /// them stale too. Set by Draft Desk, not part of the check protocol; `None` in checks stored before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edits {
    pub edited: bool,
    pub fields_changed: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportOutcome {
    Accepted,
    Rejected,
    /// The provider may or may not have the mail: never send again automatically.
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SendState {
    pub token: Option<String>,
    pub scheduled_at: Option<String>,
    pub approved_digest: Option<String>,
    pub approved_revision: Option<u64>,
    pub warnings_confirmed: Option<bool>,
    pub message_id: Option<String>,
    pub attempt_id: Option<String>,
    pub attempted_at: Option<String>,
    pub artifact: Option<String>,
    pub mime_digest: Option<String>,
    pub transport_outcome: Option<TransportOutcome>,
    pub provider_message_id: Option<String>,
    pub error_code: Option<String>,
    pub sent_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    pub ts: String,
    pub event: String,
    pub detail: String,
}

/// An item as read from disk: valid, or the reasons it is not (never sendable).
#[derive(Debug, Clone, PartialEq)]
pub enum ItemRead {
    Valid(Box<Item>),
    Invalid {
        item_id: String,
        errors: Vec<String>,
    },
}

impl ItemRead {
    pub fn id(&self) -> &str {
        match self {
            ItemRead::Valid(i) => &i.item_id,
            ItemRead::Invalid { item_id, .. } => item_id,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------

/// Every rule violation of a stored item, in a stable order.
pub fn validate_item(item: &Item, cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();
    if item.schema != ITEM_SCHEMA {
        errors.push(format!("schema: {:?} != {ITEM_SCHEMA:?}", item.schema));
    }
    if !ITEM_ID_RE.is_match(&item.item_id) {
        errors.push(format!("item_id: invalid format: {:?}", item.item_id));
    }
    if item.created_at.is_empty() {
        errors.push("created_at: empty".into());
    }
    errors.extend(validate_from(cfg, &item.account, &item.from));
    for (label, list) in [("to", &item.to), ("cc", &item.cc), ("bcc", &item.bcc)] {
        for (i, entry) in list.iter().enumerate() {
            match address::parse_list(entry) {
                Ok(addrs) if addrs.len() == 1 => {}
                Ok(_) => errors.push(format!("{label}[{i}]: one address per entry")),
                Err(e) => errors.push(format!("{label}[{i}]: {e}")),
            }
        }
    }
    errors.extend(address::check_recipients(&item.to, &item.cc, &item.bcc));
    errors.extend(validate_subject(&item.subject));
    let body_chars = item.body.chars().count();
    if body_chars > MAX_BODY_CHARS {
        errors.push(format!("body: {body_chars} > {MAX_BODY_CHARS} chars"));
    }
    if item.body.trim().is_empty() {
        errors.push("body: empty".into());
    }
    // Forbidden characters in the body are no validation error: they block sending, so the window can show them.
    if !SHA256_RE.is_match(&item.body_sha256) {
        errors.push("body_sha256: not ^[0-9a-f]{64}$".into());
    } else if sha256_hex(item.body.as_bytes()) != item.body_sha256 {
        errors.push("body_sha256: does not match sha256(body)".into());
    }
    if let Some(r) = &item.reply {
        if let Some(v) = &r.in_reply_to
            && !MESSAGE_ID_RE.is_match(v)
        {
            errors.push(format!(
                "reply.in_reply_to: not a <local@domain> message id: {v:?}"
            ));
        }
        for (i, v) in r.references.iter().enumerate() {
            if !MESSAGE_ID_RE.is_match(v) {
                errors.push(format!(
                    "reply.references[{i}]: not a <local@domain> message id"
                ));
            }
        }
        provider_id(&mut errors, "reply.gmail_thread_id", &r.gmail_thread_id);
    }
    if let Some(m) = &item.mailbox_draft {
        provider_id(
            &mut errors,
            "mailbox_draft.gmail_draft_id",
            &m.gmail_draft_id,
        );
        provider_id(
            &mut errors,
            "mailbox_draft.gmail_message_id",
            &m.gmail_message_id,
        );
        if let Some(v) = &m.message_id
            && !MESSAGE_ID_RE.is_match(v)
        {
            errors.push("mailbox_draft.message_id: not a <local@domain> message id".into());
        }
    }
    if let Some(o) = &item.origin {
        for (label, v) in [
            ("agent", &o.agent),
            ("session", &o.session),
            ("correlation", &o.correlation),
        ] {
            if let Some(v) = v
                && (v.chars().count() > MAX_ORIGIN_CHARS || has_linebreak(v) || has_forbidden(v))
            {
                errors.push(format!(
                    "origin.{label}: longer than {MAX_ORIGIN_CHARS} or has control characters"
                ));
            }
        }
    }
    if let Some(d) = &item.added_digest
        && !SHA256_RE.is_match(d)
    {
        errors.push("added_digest: not ^[0-9a-f]{64}$".into());
    }
    validate_attachments(&mut errors, &item.attachments);
    if let Some(c) = &item.checks
        && !SHA256_RE.is_match(&c.body_sha256)
    {
        errors.push("checks.body_sha256: not ^[0-9a-f]{64}$".into());
    }
    let s = &item.send;
    for (label, v, re) in [
        ("send.token", &s.token, &*TOKEN_RE),
        ("send.approved_digest", &s.approved_digest, &*SHA256_RE),
        ("send.mime_digest", &s.mime_digest, &*SHA256_RE),
        ("send.attempt_id", &s.attempt_id, &*ATTEMPT_RE),
        ("send.message_id", &s.message_id, &*MESSAGE_ID_RE),
    ] {
        if let Some(v) = v
            && !re.is_match(v)
        {
            errors.push(format!("{label}: invalid"));
        }
    }
    if let Some(a) = &s.artifact
        && !(a.len() == 25
            && a.starts_with("send-")
            && a.ends_with(".eml")
            && ATTEMPT_RE.is_match(&a[5..21]))
    {
        errors.push("send.artifact: invalid".into());
    }
    errors
}

fn validate_from(cfg: &Config, account: &str, from: &From) -> Vec<String> {
    let Some(acc) = cfg.account(account) else {
        return vec![format!("account: unknown account {account:?}")];
    };
    let Some(sender) = acc.sender(&from.address) else {
        return vec![format!(
            "from.address: {:?} is not a sender of account {account:?}",
            from.address
        )];
    };
    if from.name.trim().is_empty() || has_linebreak(&from.name) || has_forbidden(&from.name) {
        return vec!["from.name: empty or has control characters".into()];
    }
    match &sender.name {
        Some(fixed) if sender.fixed_name && from.name != *fixed => {
            vec![format!(
                "from.name: must be {fixed:?} for {:?}",
                from.address
            )]
        }
        _ => vec![],
    }
}

fn validate_subject(s: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if s.trim().is_empty() {
        errors.push("subject: empty".into());
    }
    let n = s.chars().count();
    if n > MAX_SUBJECT_CHARS {
        errors.push(format!("subject: {n} > {MAX_SUBJECT_CHARS} chars"));
    }
    if has_linebreak(s) {
        errors.push("subject: contains line separator".into());
    } else if has_forbidden(s) {
        errors.push("subject: contains control/bidi characters".into());
    }
    errors
}

fn provider_id(errors: &mut Vec<String>, label: &str, v: &Option<String>) {
    if let Some(v) = v
        && !PROVIDER_ID_RE.is_match(v)
    {
        errors.push(format!(
            "{label}: not a provider id ([A-Za-z0-9_.:-], max 128)"
        ));
    }
}

/// Name rules shared with `add`: a plain file name, no path, at most 255 bytes, no line breaks and no invisible or
/// direction-changing characters (`invoice\u{202E}fdp.exe` would show as `invoiceexe.pdf`).
pub fn attachment_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.contains(['/', '\0'])
        && name != "."
        && name != ".."
        && !has_linebreak(name)
        && !has_forbidden(name)
}

fn validate_attachments(errors: &mut Vec<String>, list: &[Attachment]) {
    if list.len() > MAX_ATTACHMENTS {
        errors.push(format!("attachments: {} > {MAX_ATTACHMENTS}", list.len()));
        return;
    }
    let total: u64 = list.iter().map(|a| a.size).fold(0, u64::saturating_add);
    if total > MAX_TOTAL_ATTACHMENT_BYTES {
        errors.push(format!(
            "attachments: total {total} > {MAX_TOTAL_ATTACHMENT_BYTES}"
        ));
    }
    let mut names = BTreeSet::new();
    for (i, a) in list.iter().enumerate() {
        if !attachment_name_ok(&a.name) {
            errors.push(format!("attachment[{i}].name: invalid"));
        } else if !names.insert(a.name.as_str()) {
            errors.push(format!("attachment[{i}].name: duplicate"));
        }
        if a.size > MAX_ATTACHMENT_BYTES {
            errors.push(format!(
                "attachment[{i}].size: {} > {MAX_ATTACHMENT_BYTES}",
                a.size
            ));
        }
        if !SHA256_RE.is_match(&a.sha256) {
            errors.push(format!("attachment[{i}].sha256: not ^[0-9a-f]{{64}}$"));
        }
    }
}

/// Reads `queue/<id>/item.json`: size cap, JSON, validation; the item id must equal the folder name.
pub fn load_item(path: &Path, cfg: &Config) -> ItemRead {
    let folder = path
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let invalid = |e: String| ItemRead::Invalid {
        item_id: folder.clone(),
        errors: vec![e],
    };
    // Check and read through one descriptor: no symlink, a regular file, never more than the cap.
    let bytes = match crate::store::read_regular(path, MAX_ITEM_BYTES) {
        Ok(b) => b,
        Err(e) => return invalid(format!("cannot read item: {e}")),
    };
    let item: Item = match serde_json::from_slice(&bytes).map_err(crate::Error::from) {
        Ok(i) => i,
        Err(e) => return invalid(format!("cannot parse item: {e}")),
    };
    let mut errors = validate_item(&item, cfg);
    if item.item_id != folder {
        errors.push(format!(
            "item_id does not match its folder: {:?} != {folder:?}",
            item.item_id
        ));
    }
    if errors.is_empty() {
        ItemRead::Valid(Box::new(item))
    } else {
        ItemRead::Invalid {
            item_id: folder,
            errors,
        }
    }
}

/// sha256 over everything a send click approves. Not included: history, checks, edits, send state, and the Gmail
/// message and thread ids, which change legitimately between click and send or select nothing.
pub fn content_digest(item: &Item) -> String {
    let mut attachments: Vec<(&str, &str, u64)> = item
        .attachments
        .iter()
        .map(|a| (a.name.as_str(), a.sha256.as_str(), a.size))
        .collect();
    attachments.sort();
    let md = item.mailbox_draft.clone().unwrap_or_default();
    let reply = item
        .reply
        .as_ref()
        .map(|r| json!([r.in_reply_to, r.references]));
    let payload = json!({
        "account": item.account,
        "from": [item.from.name, item.from.address],
        "mailbox_draft": [md.gmail_draft_id, md.message_id],
        "to": item.to,
        "cc": item.cc,
        "bcc": item.bcc,
        "subject": item.subject,
        "body_sha256": item.body_sha256,
        "attachments": attachments,
        "reply": reply,
    });
    // serde_json's map is a BTreeMap here (no `preserve_order`), so keys come out sorted: a canonical form.
    sha256_hex(payload.to_string().as_bytes())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn cfg() -> Config {
        Config::parse(
            r#"{"accounts":[{"name":"work","kind":"command",
              "senders":[{"address":"me@example.org","name":"Me"},{"address":"boss@example.org","name":"The Boss","fixed_name":true}],
              "send_command":["/bin/cat"]}]}"#,
        )
        .unwrap()
    }

    pub(crate) fn item() -> Item {
        let body = "Hello Ann,\n\nthanks.\n".to_string();
        Item {
            schema: ITEM_SCHEMA.into(),
            item_id: "20261106-223000-0a1b2c3d".into(),
            created_at: "2026-11-06T22:30:00+01:00".into(),
            state: State::Ready,
            state_revision: 1,
            content_revision: 1,
            account: "work".into(),
            from: From {
                address: "me@example.org".into(),
                name: "Me".into(),
            },
            to: vec!["Ann <ann@example.org>".into()],
            cc: vec![],
            bcc: vec![],
            subject: "Re: Offer".into(),
            body_sha256: sha256_hex(body.as_bytes()),
            body,
            reply: Some(Reply {
                in_reply_to: Some("<a@example.org>".into()),
                references: vec!["<a@example.org>".into()],
                gmail_thread_id: None,
            }),
            mailbox_draft: None,
            origin: Some(Origin {
                agent: Some("test".into()),
                session: Some("s1".into()),
                correlation: None,
            }),
            added_digest: None,
            attachments: vec![],
            checks: None,
            edits: Edits::default(),
            send: SendState::default(),
            history: vec![HistoryEntry {
                ts: "2026-11-06T21:30:00Z".into(),
                event: "added".into(),
                detail: "".into(),
            }],
        }
    }

    fn errs(edit: impl FnOnce(&mut Item)) -> Vec<String> {
        let mut it = item();
        edit(&mut it);
        validate_item(&it, &cfg())
    }

    fn has(errors: &[String], needle: &str) -> bool {
        errors.iter().any(|e| e.contains(needle))
    }

    fn set_body(it: &mut Item, body: &str) {
        it.body = body.into();
        it.body_sha256 = sha256_hex(body.as_bytes());
    }

    #[test]
    fn valid_item_is_clean() {
        assert_eq!(validate_item(&item(), &cfg()), Vec::<String>::new());
    }

    #[test]
    fn unknown_key_top_and_each_sub_object_invalid() {
        let base = serde_json::to_value(item()).unwrap();
        assert!(serde_json::from_value::<Item>(base.clone()).is_ok());
        let mut with_all = base.clone();
        with_all["mailbox_draft"] = json!({"gmail_draft_id": "r1"});
        with_all["attachments"] = json!([{"name": "a.pdf", "size": 1, "sha256": "0".repeat(64)}]);
        with_all["checks"] = json!({"body_sha256": "0".repeat(64), "verdict": "warn",
            "findings": [{"level": "warn", "title": "t"}]});
        assert!(serde_json::from_value::<Item>(with_all.clone()).is_ok());
        let pointers = [
            "",
            "/from",
            "/reply",
            "/mailbox_draft",
            "/origin",
            "/attachments/0",
            "/checks",
            "/checks/findings/0",
            "/edits",
            "/send",
        ];
        for p in pointers {
            let mut v = with_all.clone();
            v.pointer_mut(p)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("surprise".into(), json!(1));
            assert!(serde_json::from_value::<Item>(v).is_err(), "{p}");
        }
    }

    #[test]
    fn history_entry_unknown_key_invalid() {
        let mut v = serde_json::to_value(item()).unwrap();
        v["history"][0]["x"] = json!("y");
        assert!(serde_json::from_value::<Item>(v).is_err());
    }

    #[test]
    fn schema_and_state_strict() {
        assert!(has(
            &errs(|i| i.schema = "draftdesk-item/2".into()),
            "schema"
        ));
        let mut v = serde_json::to_value(item()).unwrap();
        v["state"] = json!("invalid");
        assert!(serde_json::from_value::<Item>(v).is_err());
    }

    #[test]
    fn empty_to() {
        assert_eq!(errs(|i| i.to.clear()), vec!["to: empty".to_string()]);
    }

    #[test]
    fn twenty_ok_twenty_one_rejected() {
        let rcpts = |n: usize| {
            (0..n)
                .map(|k| format!("u{k}@example.org"))
                .collect::<Vec<_>>()
        };
        assert!(errs(|i| i.to = rcpts(20)).is_empty());
        assert!(has(&errs(|i| i.to = rcpts(21)), "recipients: 21 > 20"));
    }

    #[test]
    fn crlf_and_u2028_in_recipient() {
        assert!(has(
            &errs(|i| i.to = vec!["a@example.org\r\nBcc: x@example.org".into()]),
            "to[0]"
        ));
        assert!(has(
            &errs(|i| i.cc = vec!["a@example.org\u{2028}".into()]),
            "cc[0]"
        ));
        assert!(has(
            &errs(|i| i.bcc = vec!["a@example.org, b@example.org".into()]),
            "bcc[0]: one address"
        ));
    }

    #[test]
    fn subject_rules() {
        assert!(has(&errs(|i| i.subject = "  ".into()), "subject: empty"));
        assert!(has(
            &errs(|i| i.subject = "a\r\nBcc: x".into()),
            "subject: contains line separator"
        ));
        assert!(has(
            &errs(|i| i.subject = "x".repeat(999)),
            "subject: 999 > 998"
        ));
        assert!(errs(|i| i.subject = "ü".repeat(998)).is_empty());
        for bad in ["\u{202E}", "\u{200B}", "\u{07}", "\u{FEFF}", "\u{85}"] {
            assert!(
                !errs(|i| i.subject = format!("Hi{bad}there")).is_empty(),
                "{bad:?}"
            );
        }
        assert!(errs(|i| i.subject = "Grüße 日本 🎉".into()).is_empty());
    }

    #[test]
    fn from_rules() {
        assert!(has(
            &errs(|i| i.from.address = "other@example.org".into()),
            "from.address"
        ));
        assert!(has(&errs(|i| i.account = "nope".into()), "account"));
        assert!(has(
            &errs(|i| i.from = From {
                address: "boss@example.org".into(),
                name: "Boss".into()
            }),
            "from.name"
        ));
        assert!(
            errs(|i| i.from = From {
                address: "boss@example.org".into(),
                name: "The Boss".into()
            })
            .is_empty()
        );
        assert!(errs(|i| i.from.name = "Anything Else".into()).is_empty());
        assert!(has(
            &errs(|i| i.from.name = "Me\u{202E}".into()),
            "from.name"
        ));
        assert!(has(&errs(|i| i.from.name = " ".into()), "from.name"));
    }

    #[test]
    fn body_too_long() {
        assert!(has(
            &errs(|i| set_body(i, &"x".repeat(MAX_BODY_CHARS + 1))),
            "body: 200001 > 200000"
        ));
        assert!(errs(|i| set_body(i, &"ä".repeat(MAX_BODY_CHARS))).is_empty());
        assert!(has(&errs(|i| set_body(i, " \n ")), "body: empty"));
    }

    #[test]
    fn bad_body_sha() {
        assert!(has(
            &errs(|i| i.body_sha256 = "XYZ".into()),
            "body_sha256: not"
        ));
    }

    #[test]
    fn body_sha_mismatch_exactly_one_error() {
        assert_eq!(
            errs(|i| i.body.push('!')),
            vec!["body_sha256: does not match sha256(body)".to_string()]
        );
    }

    #[test]
    fn forbidden_chars_in_body_are_not_a_validation_error() {
        assert!(errs(|i| set_body(i, "Hi\u{202E}there")).is_empty());
    }

    #[test]
    fn bad_item_id() {
        for bad in [
            "",
            "2026-11-06",
            "20261106-223000-0A1B2C3D",
            "20261106-223000-0a1b2c3d/..",
        ] {
            assert!(has(&errs(|i| i.item_id = bad.into()), "item_id"), "{bad}");
        }
    }

    fn att(name: &str, size: u64) -> Attachment {
        Attachment {
            name: name.into(),
            size,
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn attachment_name_rules() {
        for bad in ["", ".", "..", "a/b.pdf", "a\0b", &"x".repeat(256)] {
            assert!(
                has(
                    &errs(|i| i.attachments = vec![att(bad, 1)]),
                    "attachment[0].name"
                ),
                "{bad:?}"
            );
        }
        assert!(errs(|i| i.attachments = vec![att(&"x".repeat(255), 1)]).is_empty());
        assert!(errs(|i| i.attachments = vec![att("Übersicht Größe.pdf", 1)]).is_empty());
        assert!(has(
            &errs(|i| i.attachments = vec![att("a.pdf", 1), att("a.pdf", 2)]),
            "duplicate"
        ));
        assert!(has(
            &errs(|i| i.attachments = vec![Attachment {
                sha256: "x".into(),
                ..att("a", 1)
            }]),
            "sha256"
        ));
        assert!(has(
            &errs(|i| i.attachments = vec![att("a", MAX_ATTACHMENT_BYTES + 1)]),
            "size"
        ));
        assert!(has(
            &errs(|i| i.attachments = vec![att("a", MAX_ATTACHMENT_BYTES), att("b", 1)]),
            "total"
        ));
    }

    #[test]
    fn too_many_attachments() {
        let list: Vec<Attachment> = (0..11).map(|k| att(&format!("f{k}"), 1)).collect();
        assert!(has(&errs(|i| i.attachments = list), "attachments: 11 > 10"));
    }

    #[test]
    fn id_fields() {
        assert!(has(
            &errs(|i| i.reply.as_mut().unwrap().in_reply_to = Some("a@b".into())),
            "reply.in_reply_to"
        ));
        assert!(has(
            &errs(|i| i.reply.as_mut().unwrap().references = vec!["<a b@c>".into()]),
            "reply.references"
        ));
        assert!(has(
            &errs(|i| i.reply.as_mut().unwrap().gmail_thread_id = Some("a/b".into())),
            "gmail_thread_id"
        ));
        let md = |f: fn(&mut MailboxDraft)| {
            errs(move |i| {
                let mut m = MailboxDraft::default();
                f(&mut m);
                i.mailbox_draft = Some(m);
            })
        };
        assert!(has(
            &md(|m| m.gmail_draft_id = Some("r 1".into())),
            "mailbox_draft.gmail_draft_id"
        ));
        assert!(has(
            &md(|m| m.gmail_message_id = Some("".into())),
            "mailbox_draft.gmail_message_id"
        ));
        assert!(has(
            &md(|m| m.message_id = Some("d@x".into())),
            "mailbox_draft.message_id"
        ));
        assert!(md(|m| m.message_id = Some("<d@example.org>".into())).is_empty());
    }

    #[test]
    fn message_ids_are_visible_ascii_only() {
        let bad = [
            "<a\u{0}b@example.org>",
            "<a\u{7}@example.org>",
            "<a@example.org\u{7F}>",
            "<a\u{202E}b@example.org>",
            "<a\u{200B}@example.org>",
            "<ä@example.org>",
            "<a@exämple.org>",
        ];
        for v in bad {
            assert!(
                has(
                    &errs(|i| i.reply.as_mut().unwrap().in_reply_to = Some(v.into())),
                    "reply.in_reply_to"
                ),
                "{v:?}"
            );
            assert!(
                has(
                    &errs(|i| i.reply.as_mut().unwrap().references = vec![v.into()]),
                    "reply.references"
                ),
                "{v:?}"
            );
            assert!(
                has(
                    &errs(|i| {
                        i.mailbox_draft = Some(MailboxDraft {
                            message_id: Some(v.into()),
                            ..Default::default()
                        })
                    }),
                    "mailbox_draft.message_id"
                ),
                "{v:?}"
            );
        }
        let good = "<a.b-c_d+e!#$%&'*/=?^`{|}~\"[]@mail-1.example.org>";
        assert!(errs(|i| i.reply.as_mut().unwrap().in_reply_to = Some(good.into())).is_empty());
    }

    #[test]
    fn origin_rules() {
        assert!(has(
            &errs(|i| i.origin.as_mut().unwrap().session = Some("x".repeat(161))),
            "origin.session"
        ));
        assert!(errs(|i| i.origin.as_mut().unwrap().session = Some("x".repeat(160))).is_empty());
        assert!(has(
            &errs(|i| i.origin.as_mut().unwrap().agent = Some("a\nb".into())),
            "origin.agent"
        ));
        assert!(has(
            &errs(|i| i.origin.as_mut().unwrap().correlation = Some("\u{202E}".into())),
            "origin.correlation"
        ));
    }

    #[test]
    fn send_state_shapes() {
        assert!(has(
            &errs(|i| i.send.artifact = Some("../../x.eml".into())),
            "send.artifact"
        ));
        assert!(has(
            &errs(|i| i.send.token = Some("short".into())),
            "send.token"
        ));
        assert!(has(
            &errs(|i| i.send.message_id = Some("nope".into())),
            "send.message_id"
        ));
        assert!(errs(|i| i.send.artifact = Some("send-0123456789abcdef.eml".into())).is_empty());
    }

    fn write_item(dir: &Path, id: &str, json: &str) -> std::path::PathBuf {
        let d = dir.join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(ITEM_FILE), json).unwrap();
        d.join(ITEM_FILE)
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dd-item-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn load_roundtrip() {
        let d = tmp("rt");
        let it = item();
        let p = write_item(&d, &it.item_id, &serde_json::to_string_pretty(&it).unwrap());
        assert_eq!(load_item(&p, &cfg()), ItemRead::Valid(Box::new(it)));
        std::fs::remove_dir_all(&d).unwrap();
    }

    fn invalid(r: ItemRead) -> (String, Vec<String>) {
        match r {
            ItemRead::Invalid { item_id, errors } => (item_id, errors),
            ItemRead::Valid(_) => panic!("expected invalid"),
        }
    }

    #[test]
    fn load_broken_json() {
        let d = tmp("broken");
        let p = write_item(&d, "20261106-223000-0a1b2c3d", "{not json");
        let (id, errors) = invalid(load_item(&p, &cfg()));
        assert_eq!(id, "20261106-223000-0a1b2c3d");
        assert!(has(&errors, "json"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn load_too_large() {
        let d = tmp("large");
        let p = write_item(
            &d,
            "20261106-223000-0a1b2c3d",
            &" ".repeat(MAX_ITEM_BYTES as usize + 1),
        );
        assert!(has(&invalid(load_item(&p, &cfg())).1, "too large"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn load_missing() {
        let d = tmp("missing");
        assert!(has(
            &invalid(load_item(&d.join("x").join(ITEM_FILE), &cfg())).1,
            "cannot read"
        ));
    }

    #[test]
    fn load_id_mismatch_is_invalid() {
        let d = tmp("mismatch");
        let p = write_item(
            &d,
            "20261106-223000-ffffffff",
            &serde_json::to_string(&item()).unwrap(),
        );
        let (id, errors) = invalid(load_item(&p, &cfg()));
        assert_eq!(id, "20261106-223000-ffffffff");
        assert!(has(&errors, "item_id does not match its folder"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn load_invalid_item_reports_errors() {
        let d = tmp("inv");
        let mut it = item();
        it.to.clear();
        let p = write_item(&d, &it.item_id, &serde_json::to_string(&it).unwrap());
        assert_eq!(
            invalid(load_item(&p, &cfg())).1,
            vec!["to: empty".to_string()]
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn digest_binds_draft_selectors() {
        let base = content_digest(&item());
        let with = |f: fn(&mut MailboxDraft)| {
            let mut it = item();
            let mut m = MailboxDraft::default();
            f(&mut m);
            it.mailbox_draft = Some(m);
            content_digest(&it)
        };
        let none = with(|_| {});
        assert_eq!(none, base, "an empty mailbox_draft binds the same as none");
        assert_ne!(with(|m| m.gmail_draft_id = Some("r1".into())), base);
        assert_ne!(
            with(|m| m.message_id = Some("<d@example.org>".into())),
            base
        );
        assert_eq!(with(|m| m.gmail_message_id = Some("18d".into())), base);
        let mut it = item();
        it.reply.as_mut().unwrap().gmail_thread_id = Some("18c".into());
        assert_eq!(content_digest(&it), base);
    }

    #[test]
    fn digest_covers_sendable_fields() {
        let base = content_digest(&item());
        let changes: Vec<fn(&mut Item)> = vec![
            |i| i.account = "other".into(),
            |i| i.from.name = "Other".into(),
            |i| i.from.address = "x@example.org".into(),
            |i| i.to.push("b@example.org".into()),
            |i| i.cc.push("b@example.org".into()),
            |i| i.bcc.push("b@example.org".into()),
            |i| i.subject.push('!'),
            |i| i.body_sha256 = "f".repeat(64),
            |i| i.reply.as_mut().unwrap().in_reply_to = None,
            |i| i.reply.as_mut().unwrap().references.clear(),
            |i| {
                i.attachments.push(Attachment {
                    name: "a".into(),
                    size: 1,
                    sha256: "a".repeat(64),
                })
            },
        ];
        for (k, f) in changes.into_iter().enumerate() {
            let mut it = item();
            f(&mut it);
            assert_ne!(content_digest(&it), base, "change {k}");
        }
        // Attachment order does not matter.
        let a = Attachment {
            name: "a".into(),
            size: 1,
            sha256: "a".repeat(64),
        };
        let b = Attachment {
            name: "b".into(),
            size: 2,
            sha256: "b".repeat(64),
        };
        let (mut x, mut y) = (item(), item());
        x.attachments = vec![a.clone(), b.clone()];
        y.attachments = vec![b, a];
        assert_eq!(content_digest(&x), content_digest(&y));
    }

    #[test]
    fn digest_ignores_history_and_checks() {
        let base = content_digest(&item());
        let mut it = item();
        it.history.push(HistoryEntry {
            ts: "t".into(),
            event: "e".into(),
            detail: "d".into(),
        });
        it.checks = Some(Checks {
            body_sha256: it.body_sha256.clone(),
            verdict: Verdict::Block,
            findings: vec![],
            ran_at: None,
            source: None,
            content_digest: None,
        });
        it.edits.edited = true;
        it.send.token = Some("a".repeat(32));
        it.state = State::SendingScheduled;
        it.state_revision = 9;
        it.content_revision = 9;
        assert_eq!(content_digest(&it), base);
    }
}
