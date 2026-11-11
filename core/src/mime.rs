//! The mail as it is sent: built from exactly the stored item, then parsed back and checked against it.

use crate::address;
use crate::html::{body_to_html, unwrap_body};
use crate::item::{Attachment, Item, MESSAGE_ID_RE, attachment_name_ok};
use crate::{Error, Result, bail, sha256_hex};
use lettre::Message;
use lettre::address::Envelope;
use lettre::message::header::{Bcc, Cc, ContentType, To};
use lettre::message::{Attachment as Part, Mailbox, Mailboxes, MultiPart, SinglePart};
use mailparse::{MailAddr, MailHeaderMap, ParsedMail};
use std::ffi::CString;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};

/// Gmail needs the Bcc header in the message; SMTP and the command transport get it only in the envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BccMode {
    Keep,
    Strip,
}

#[derive(Debug, Clone)]
pub struct Built {
    pub bytes: Vec<u8>,
    /// To, Cc and Bcc addresses: who the transport delivers to.
    pub envelope_rcpts: Vec<String>,
    pub mime_sha256: String,
}

/// Builds the mail: `multipart/alternative` (plain = `unwrap_body`, html = `body_to_html`), wrapped in
/// `multipart/mixed` with the attachments when there are any. Attachments are opened inside `att_dir` only, never
/// through a symlink, and must still match their stored size and hash.
pub fn build(item: &Item, att_dir: &File, message_id: &str, bcc: BccMode) -> Result<Built> {
    if !MESSAGE_ID_RE.is_match(message_id) {
        bail!("message id {message_id:?} is not <local@domain>");
    }
    let from = mailbox(&item.from.name, &item.from.address)?;
    let sender = from.email.clone();
    let mut b = Message::builder().from(from);
    // lettre parses an address header again when it adds a mailbox to it and when it derives the envelope, with its
    // 64-character cap (`address::to_lettre`): each header is set once and the envelope is given. lettre still
    // parses From, which holds the account's own address.
    let mut rcpts = vec![];
    for (list, field) in [(&item.to, 0), (&item.cc, 1), (&item.bcc, 2)] {
        let mut boxes = vec![];
        for entry in list {
            for a in address::parse_list(entry)? {
                boxes.push(mailbox(&a.name, &a.address)?);
            }
        }
        if boxes.is_empty() {
            continue;
        }
        rcpts.extend(boxes.iter().map(|m| m.email.clone()));
        let boxes: Mailboxes = boxes.into_iter().collect();
        b = match field {
            0 => b.header(To::from(boxes)),
            1 => b.header(Cc::from(boxes)),
            _ => b.header(Bcc::from(boxes)),
        };
    }
    b = b.envelope(
        Envelope::new(Some(sender), rcpts)
            .map_err(|e| Error(format!("cannot build the mail: {e}")))?,
    );
    if bcc == BccMode::Keep {
        b = b.keep_bcc();
    }
    b = b
        .subject(item.subject.clone())
        .message_id(Some(message_id.into()))
        .date_now();
    let (in_reply_to, references) = threading(item);
    if let Some(v) = in_reply_to {
        b = b.in_reply_to(v);
    }
    if let Some(v) = references {
        b = b.references(v);
    }
    let plain = unwrap_body(&item.body);
    let html = body_to_html(&plain);
    let alternative = MultiPart::alternative()
        .singlepart(SinglePart::plain(plain))
        .singlepart(SinglePart::html(html));
    let message = if item.attachments.is_empty() {
        b.multipart(alternative)
    } else {
        let mut mixed = MultiPart::mixed().multipart(alternative);
        for a in &item.attachments {
            // A Vec<u8> body is always base64: the bytes go out unchanged, no line-ending conversion.
            mixed = mixed.singlepart(
                Part::new(a.name.clone()).body(read_attachment(att_dir, a)?, content_type(&a.name)),
            );
        }
        b.multipart(mixed)
    }
    .map_err(|e| Error(format!("cannot build the mail: {e}")))?;
    let bytes = message.formatted();
    Ok(Built {
        envelope_rcpts: message
            .envelope()
            .to()
            .iter()
            .map(ToString::to_string)
            .collect(),
        mime_sha256: sha256_hex(&bytes),
        bytes,
    })
}

fn mailbox(name: &str, addr: &str) -> Result<Mailbox> {
    Ok(Mailbox::new(
        (!name.is_empty()).then(|| name.to_string()),
        address::to_lettre(addr)?,
    ))
}

/// In-Reply-To, and References (defaulting to In-Reply-To).
fn threading(item: &Item) -> (Option<String>, Option<String>) {
    let Some(r) = &item.reply else {
        return (None, None);
    };
    let refs = if r.references.is_empty() {
        r.in_reply_to.clone()
    } else {
        Some(r.references.join(" "))
    };
    (r.in_reply_to.clone(), refs)
}

/// Opens `name` inside the attachment folder without following any symlink, then checks size and hash.
fn read_attachment(att_dir: &File, a: &Attachment) -> Result<Vec<u8>> {
    if !attachment_name_ok(&a.name) {
        bail!("attachment {:?}: not a plain file name", a.name);
    }
    let c_name = CString::new(a.name.as_bytes())
        .map_err(|_| Error(format!("attachment {:?}: NUL", a.name)))?;
    // SAFETY: a valid directory descriptor and a NUL-terminated name; the returned descriptor is owned below.
    let fd = unsafe {
        libc::openat(
            att_dir.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        bail!(
            "attachment {:?}: cannot open: {}",
            a.name,
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: `fd` was just opened and is owned by nobody else.
    let f = unsafe { File::from_raw_fd(fd) };
    let meta = f.metadata()?;
    if !meta.file_type().is_file() || meta.len() != a.size {
        bail!(
            "attachment {:?}: not a regular file of the stored size",
            a.name
        );
    }
    let mut bytes = Vec::new();
    f.take(a.size + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != a.size || sha256_hex(&bytes) != a.sha256 {
        bail!("attachment {:?}: changed since it was added", a.name);
    }
    Ok(bytes)
}

// ponytail: a fixed table instead of a MIME database; extend when a common type is missing.
fn content_type(name: &str) -> ContentType {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    let t = match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        // .eml as message/rfc822 would forbid base64 and parsers would descend into it: send it as a file.
        _ => "application/octet-stream",
    };
    ContentType::parse(t).expect("static content types parse")
}

/// Parses `built` and checks it against the item: one From/To/Subject/Message-ID/Date each, a valid date, the same
/// Message-ID, sender, recipients per field in order, subject, Cc/Bcc presence, threading headers, exactly the
/// plain and HTML parts the item produces, and the same attachments (name and sha256).
pub fn verify(built: &[u8], item: &Item, message_id: &str, bcc: BccMode) -> Result<()> {
    let mail = mailparse::parse_mail(built)
        .map_err(|e| Error(format!("cannot parse the built mail: {e}")))?;
    let h = &mail.headers;
    for name in ["From", "To", "Subject", "Message-ID", "Date"] {
        let n = h.get_all_headers(name).len();
        if n != 1 {
            bail!("header {name}: expected exactly one, found {n}");
        }
    }
    let first = |name: &str| h.get_first_value(name).unwrap_or_default();
    if chrono::DateTime::parse_from_rfc2822(first("Date").trim()).is_err() {
        bail!("header Date: not a valid date");
    }
    if first("Message-ID").trim() != message_id {
        bail!(
            "header Message-ID: {:?} != {message_id:?}",
            first("Message-ID")
        );
    }
    let from = addresses(h.get_first_header("From"))?;
    if from != vec![(item.from.name.clone(), item.from.address.to_lowercase())] {
        bail!("header From: {from:?} does not match the item");
    }
    for (name, list, present) in [
        ("To", &item.to, true),
        ("Cc", &item.cc, !item.cc.is_empty()),
        (
            "Bcc",
            &item.bcc,
            bcc == BccMode::Keep && !item.bcc.is_empty(),
        ),
    ] {
        let headers = h.get_all_headers(name);
        if headers.len() != usize::from(present) {
            bail!(
                "header {name}: expected {} header(s), found {}",
                usize::from(present),
                headers.len()
            );
        }
        let mut expected = Vec::new();
        for entry in list.iter().filter(|_| present) {
            expected.extend(
                address::parse_list(entry)?
                    .into_iter()
                    .map(|a| (a.name, a.address.to_lowercase())),
            );
        }
        let actual = match headers.first() {
            Some(header) => addresses(Some(header))?,
            None => vec![],
        };
        if actual != expected {
            bail!("header {name}: recipients do not match the item");
        }
    }
    if first("Subject") != item.subject {
        bail!("header Subject: does not match the item");
    }
    let (in_reply_to, references) = threading(item);
    for (name, expected) in [("In-Reply-To", in_reply_to), ("References", references)] {
        let values = h.get_all_values(name);
        let tokens = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        let ok = match &expected {
            None => values.is_empty(),
            Some(e) => values.len() == 1 && tokens(&values[0]) == tokens(e),
        };
        if !ok {
            bail!("header {name}: threading does not match the item");
        }
    }
    let plain = unwrap_body(&item.body);
    let html = body_to_html(&plain);
    let parts = text_parts(&mail)?;
    if parts
        != vec![
            ("text/html".to_string(), normalize(&html)),
            ("text/plain".to_string(), normalize(&plain)),
        ]
    {
        let kinds: Vec<&str> = parts.iter().map(|(k, _)| k.as_str()).collect();
        bail!("body parts do not match the item (found {kinds:?})");
    }
    let mut expected: Vec<(String, String)> = item
        .attachments
        .iter()
        .map(|a| (a.name.clone(), a.sha256.clone()))
        .collect();
    expected.sort();
    if attachments(&mail)? != expected {
        bail!("attachment parts do not match the item");
    }
    Ok(())
}

/// (display name, lowercase address) of every address in a header; groups are refused.
fn addresses(header: Option<&mailparse::MailHeader>) -> Result<Vec<(String, String)>> {
    let Some(header) = header else {
        return Ok(vec![]);
    };
    let list = mailparse::addrparse_header(header)
        .map_err(|e| Error(format!("header {}: {e}", header.get_key())))?;
    list.iter()
        .map(|a| match a {
            MailAddr::Single(s) => Ok((
                s.display_name.clone().unwrap_or_default(),
                s.addr.to_lowercase(),
            )),
            MailAddr::Group(_) => Err(Error(format!(
                "header {}: groups are not allowed",
                header.get_key()
            ))),
        })
        .collect()
}

fn leaves<'a>(m: &'a ParsedMail<'a>, out: &mut Vec<&'a ParsedMail<'a>>) {
    if m.subparts.is_empty() {
        out.push(m);
    } else {
        m.subparts.iter().for_each(|p| leaves(p, out));
    }
}

fn is_attachment(p: &ParsedMail) -> bool {
    p.get_content_disposition().disposition == mailparse::DispositionType::Attachment
}

/// (content type, normalized text) of every non-attachment leaf, sorted.
fn text_parts(m: &ParsedMail) -> Result<Vec<(String, String)>> {
    let mut all = Vec::new();
    leaves(m, &mut all);
    let mut out = Vec::new();
    for p in all.into_iter().filter(|p| !is_attachment(p)) {
        let body = p
            .get_body()
            .map_err(|e| Error(format!("cannot decode a body part: {e}")))?;
        out.push((p.ctype.mimetype.to_ascii_lowercase(), normalize(&body)));
    }
    out.sort();
    Ok(out)
}

/// (file name, sha256) of every attachment leaf, sorted.
fn attachments(m: &ParsedMail) -> Result<Vec<(String, String)>> {
    let mut all = Vec::new();
    leaves(m, &mut all);
    let mut out = Vec::new();
    for p in all.into_iter().filter(|p| is_attachment(p)) {
        let name = p
            .get_content_disposition()
            .params
            .get("filename")
            .cloned()
            .unwrap_or_default();
        let bytes = p
            .get_body_raw()
            .map_err(|e| Error(format!("cannot decode an attachment: {e}")))?;
        out.push((name, sha256_hex(&bytes)));
    }
    out.sort();
    Ok(out)
}

/// Line endings as LF and no trailing newline: transports may convert line endings and add a final newline.
fn normalize(s: &str) -> String {
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    s.trim_end_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::tests::item;
    use std::path::{Path, PathBuf};

    const MSGID: &str = "<20261106223000.0123456789abcdef@example.org>";

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn att_dir(name: &str, files: &[(&str, &[u8])]) -> (Dir, File) {
        let d = std::env::temp_dir().join(format!(
            "dd-mime-{name}-{}-{}",
            std::process::id(),
            crate::random_hex(4)
        ));
        std::fs::create_dir_all(&d).unwrap();
        for (n, b) in files {
            std::fs::write(d.join(n), b).unwrap();
        }
        let f = File::open(&d).unwrap();
        (Dir(d), f)
    }

    fn with_atts(files: &[(&str, &[u8])]) -> Item {
        let mut it = item();
        it.attachments = files
            .iter()
            .map(|(n, b)| Attachment {
                name: n.to_string(),
                size: b.len() as u64,
                sha256: sha256_hex(b),
            })
            .collect();
        it
    }

    fn header_values(bytes: &[u8], name: &str) -> Vec<String> {
        mailparse::parse_mail(bytes)
            .unwrap()
            .get_headers()
            .get_all_values(name)
    }

    fn built(it: &Item, bcc: BccMode) -> Vec<u8> {
        let (_d, f) = att_dir("b", &[]);
        build(it, &f, MSGID, bcc).unwrap().bytes
    }

    #[test]
    fn from_header_exact() {
        let mut it = item();
        it.from.name = "Jürgen Groß, Sales".into();
        let bytes = built(&it, BccMode::Strip);
        verify(&bytes, &it, MSGID, BccMode::Strip).unwrap();
        let mut other = it.clone();
        other.from.name = "Jürgen Groß".into();
        assert!(
            verify(&bytes, &other, MSGID, BccMode::Strip)
                .unwrap_err()
                .0
                .contains("From")
        );
    }

    #[test]
    fn bcc_kept_for_gmail_stripped_for_smtp() {
        let mut it = item();
        it.bcc = vec!["Secret <secret@example.org>".into()];
        let (_d, f) = att_dir("bcc", &[]);
        let keep = build(&it, &f, MSGID, BccMode::Keep).unwrap();
        let strip = build(&it, &f, MSGID, BccMode::Strip).unwrap();
        assert_eq!(header_values(&keep.bytes, "Bcc").len(), 1);
        assert!(header_values(&strip.bytes, "Bcc").is_empty());
        assert!(!String::from_utf8_lossy(&strip.bytes).contains("secret@"));
        for b in [&keep, &strip] {
            assert_eq!(
                b.envelope_rcpts,
                vec![
                    "ann@example.org".to_string(),
                    "secret@example.org".to_string()
                ]
            );
            assert_eq!(b.mime_sha256, sha256_hex(&b.bytes));
        }
        verify(&keep.bytes, &it, MSGID, BccMode::Keep).unwrap();
        verify(&strip.bytes, &it, MSGID, BccMode::Strip).unwrap();
        assert!(
            verify(&strip.bytes, &it, MSGID, BccMode::Keep)
                .unwrap_err()
                .0
                .contains("Bcc")
        );
        assert!(
            verify(&keep.bytes, &it, MSGID, BccMode::Strip)
                .unwrap_err()
                .0
                .contains("Bcc")
        );
    }

    #[test]
    fn one_valid_date() {
        let bytes = built(&item(), BccMode::Strip);
        let dates = header_values(&bytes, "Date");
        assert_eq!(dates.len(), 1);
        assert!(
            chrono::DateTime::parse_from_rfc2822(&dates[0]).is_ok(),
            "{}",
            dates[0]
        );
    }

    #[test]
    fn no_cc_when_empty() {
        let bytes = built(&item(), BccMode::Strip);
        assert!(header_values(&bytes, "Cc").is_empty());
        let mut it = item();
        it.cc = vec!["Bob <bob@example.org>".into(), "carol@example.org".into()];
        let bytes = built(&it, BccMode::Strip);
        assert_eq!(header_values(&bytes, "Cc").len(), 1);
        verify(&bytes, &it, MSGID, BccMode::Strip).unwrap();
    }

    #[test]
    fn references_default_in_reply_to() {
        let mut it = item();
        it.reply.as_mut().unwrap().references.clear();
        let bytes = built(&it, BccMode::Strip);
        assert_eq!(
            header_values(&bytes, "In-Reply-To"),
            vec!["<a@example.org>".to_string()]
        );
        assert_eq!(
            header_values(&bytes, "References"),
            vec!["<a@example.org>".to_string()]
        );
        verify(&bytes, &it, MSGID, BccMode::Strip).unwrap();
        it.reply = None;
        let bytes = built(&it, BccMode::Strip);
        assert!(
            header_values(&bytes, "In-Reply-To").is_empty()
                && header_values(&bytes, "References").is_empty()
        );
        verify(&bytes, &it, MSGID, BccMode::Strip).unwrap();
    }

    #[test]
    fn many_references_survive_folding() {
        let mut it = item();
        let refs: Vec<String> = (0..30)
            .map(|k| format!("<ref{k}.abcdefghijklmnop@mail.example.org>"))
            .collect();
        it.reply.as_mut().unwrap().references = refs;
        let bytes = built(&it, BccMode::Strip);
        verify(&bytes, &it, MSGID, BccMode::Strip).unwrap();
    }

    #[test]
    fn long_local_part_builds_and_verifies() {
        // Ticket systems reply from local parts beyond the 64 of RFC 5321; this one has 66.
        let rcpt = "ticket-replies-long-routing-x+0f1e2d3c-4b5a-4978-8a9b-0c1d2e3f4a5b@example.org";
        let other = rcpt.replace("en+", "de+");
        let mut it = item();
        // A second address after the long one: lettre would drop the long one from the header when adding it.
        it.to = vec![rcpt.into(), "Bob <bob@example.org>".into()];
        it.cc = vec![format!("Desk <{other}>")];
        let (_d, f) = att_dir("long", &[]);
        let b = build(&it, &f, MSGID, BccMode::Strip).unwrap();
        verify(&b.bytes, &it, MSGID, BccMode::Strip).unwrap();
        assert_eq!(b.envelope_rcpts, [rcpt, "bob@example.org", other.as_str()]);
    }

    #[test]
    fn attachment_changed_sha_fails() {
        let it = with_atts(&[("a.pdf", b"original")]);
        let (_d, f) = att_dir("sha", &[("a.pdf", b"changed!")]);
        assert!(
            build(&it, &f, MSGID, BccMode::Strip)
                .unwrap_err()
                .0
                .contains("a.pdf")
        );
        let (_d, f) = att_dir("size", &[("a.pdf", b"longer than before")]);
        assert!(build(&it, &f, MSGID, BccMode::Strip).is_err());
    }

    #[test]
    fn attachment_symlink_fails() {
        let it = with_atts(&[("a.pdf", b"data")]);
        let (d, f) = att_dir("link", &[("real.pdf", b"data")]);
        std::os::unix::fs::symlink(d.0.join("real.pdf"), d.0.join("a.pdf")).unwrap();
        assert!(build(&it, &f, MSGID, BccMode::Strip).is_err());
        let mut bad = it.clone();
        bad.attachments[0].name = "../a.pdf".into();
        assert!(build(&bad, &f, MSGID, BccMode::Strip).is_err());
    }

    #[test]
    fn attachment_count_mismatch() {
        let two = with_atts(&[("a.pdf", b"1"), ("b.pdf", b"2")]);
        let one = with_atts(&[("a.pdf", b"1")]);
        let (_d, f) = att_dir("count", &[("a.pdf", b"1"), ("b.pdf", b"2")]);
        let bytes = build(&one, &f, MSGID, BccMode::Strip).unwrap().bytes;
        assert!(
            verify(&bytes, &two, MSGID, BccMode::Strip)
                .unwrap_err()
                .0
                .contains("attachment")
        );
        let bytes = build(&two, &f, MSGID, BccMode::Strip).unwrap().bytes;
        verify(&bytes, &two, MSGID, BccMode::Strip).unwrap();
    }

    fn detects(change: fn(&mut Item), msgid: &str, needle: &str) {
        let it = with_atts(&[("a.pdf", b"1")]);
        let (_d, f) = att_dir("detect", &[("a.pdf", b"1")]);
        let bytes = build(&it, &f, MSGID, BccMode::Keep).unwrap().bytes;
        let mut other = it.clone();
        change(&mut other);
        let err = verify(&bytes, &other, msgid, BccMode::Keep).unwrap_err();
        assert!(err.0.contains(needle), "{err}");
    }

    #[test]
    fn verify_detects_recipient() {
        detects(|i| i.to = vec!["Ann <ann@example.net>".into()], MSGID, "To");
        detects(|i| i.to.push("bob@example.org".into()), MSGID, "To");
        detects(
            |i| i.to = vec!["Anna <ann@example.org>".into()],
            MSGID,
            "To",
        );
    }

    #[test]
    fn verify_detects_subject() {
        detects(|i| i.subject = "Re: Offer!".into(), MSGID, "Subject");
    }

    #[test]
    fn verify_detects_message_id() {
        detects(|_| {}, "<other@example.org>", "Message-ID");
    }

    #[test]
    fn verify_detects_from() {
        detects(
            |i| i.from.address = "boss@example.org".into(),
            MSGID,
            "From",
        );
    }

    #[test]
    fn verify_detects_date() {
        let it = item();
        let bytes = built(&it, BccMode::Strip);
        let text = String::from_utf8(bytes).unwrap();
        let date_line = text.lines().find(|l| l.starts_with("Date: ")).unwrap();
        let broken = text.replace(date_line, "Date: not a date at all");
        assert!(
            verify(broken.as_bytes(), &it, MSGID, BccMode::Strip)
                .unwrap_err()
                .0
                .contains("Date")
        );
        let doubled = text.replace(date_line, &format!("{date_line}\r\n{date_line}"));
        assert!(
            verify(doubled.as_bytes(), &it, MSGID, BccMode::Strip)
                .unwrap_err()
                .0
                .contains("Date")
        );
    }

    #[test]
    fn verify_detects_bcc_presence() {
        detects(|i| i.bcc = vec!["x@example.org".into()], MSGID, "Bcc");
    }

    #[test]
    fn verify_detects_attachment_mismatch() {
        detects(
            |i| i.attachments[0].sha256 = "0".repeat(64),
            MSGID,
            "attachment",
        );
        detects(
            |i| i.attachments[0].name = "b.pdf".into(),
            MSGID,
            "attachment",
        );
    }

    #[test]
    fn verify_detects_threading() {
        detects(|i| i.reply = None, MSGID, "In-Reply-To");
        detects(
            |i| {
                i.reply
                    .as_mut()
                    .unwrap()
                    .references
                    .push("<x@example.org>".into())
            },
            MSGID,
            "References",
        );
    }

    #[test]
    fn verify_without_attachments_ok() {
        let it = item();
        let bytes = built(&it, BccMode::Strip);
        verify(&bytes, &it, MSGID, BccMode::Strip).unwrap();
        let ct = header_values(&bytes, "Content-Type");
        assert!(ct[0].starts_with("multipart/alternative"), "{ct:?}");
    }

    #[test]
    fn eml_attachment_as_octet_stream() {
        let it = with_atts(&[
            ("fwd.eml", b"From: x\r\n\r\nhi"),
            ("a.pdf", b"%PDF"),
            ("t.TXT", b"text"),
        ]);
        let (_d, f) = att_dir(
            "eml",
            &[
                ("fwd.eml", b"From: x\r\n\r\nhi"),
                ("a.pdf", b"%PDF"),
                ("t.TXT", b"text"),
            ],
        );
        let bytes = build(&it, &f, MSGID, BccMode::Strip).unwrap().bytes;
        let mail = mailparse::parse_mail(&bytes).unwrap();
        let types: Vec<String> = mail
            .subparts
            .iter()
            .skip(1)
            .map(|p| p.ctype.mimetype.clone())
            .collect();
        assert_eq!(
            types,
            vec!["application/octet-stream", "application/pdf", "text/plain"]
        );
        verify(&bytes, &it, MSGID, BccMode::Strip).unwrap();
    }

    #[test]
    fn tampered_plain_or_html_part_fails() {
        let it = item();
        let bytes = built(&it, BccMode::Strip);
        let mut other = it.clone();
        other.body = "Hello Ann,\n\nthanks!\n".into();
        other.body_sha256 = sha256_hex(other.body.as_bytes());
        assert!(
            verify(&bytes, &other, MSGID, BccMode::Strip)
                .unwrap_err()
                .0
                .contains("body")
        );
    }

    #[test]
    fn non_ascii_filename_roundtrips() {
        let name = "Übersicht Größe.txt";
        let it = with_atts(&[(name, b"inhalt")]);
        let (_d, f) = att_dir("utf", &[(name, b"inhalt")]);
        let bytes = build(&it, &f, MSGID, BccMode::Strip).unwrap().bytes;
        verify(&bytes, &it, MSGID, BccMode::Strip).unwrap();
        let mail = mailparse::parse_mail(&bytes).unwrap();
        let disp = mail.subparts[1].get_content_disposition();
        assert_eq!(disp.params.get("filename").map(String::as_str), Some(name));
    }

    #[test]
    fn unicode_subject_and_long_subject_roundtrip() {
        let mut it = item();
        it.subject = format!("Grüße – {} 🎉", "lang ".repeat(150))
            .trim_end()
            .to_string();
        let bytes = built(&it, BccMode::Strip);
        verify(&bytes, &it, MSGID, BccMode::Strip).unwrap();
    }

    #[test]
    fn crlf_body_parts_match() {
        let mut it = item();
        it.body =
            "Hi Bob,\r\n\r\nline one\r\nline two\r\n- a\r\n- b\r\n\r\nBest Regards\r\nCarol\r\n"
                .into();
        it.body_sha256 = sha256_hex(it.body.as_bytes());
        verify(&built(&it, BccMode::Strip), &it, MSGID, BccMode::Strip).unwrap();
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mime")
            .join(name)
    }

    /// The same draft, built by the Python builder (fixture) and by this module: same header names, same decoded
    /// text and HTML parts, same attachments; and the Python bytes pass `verify` for the item.
    #[test]
    fn python_fixture_semantics() {
        let spec: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(fixture("01.json")).unwrap()).unwrap();
        let python = std::fs::read(fixture("01.eml")).unwrap();
        let att_name = spec["attachment_name"].as_str().unwrap();
        let att_bytes = spec["attachment_content"].as_str().unwrap().as_bytes();
        let mut it = with_atts(&[(att_name, att_bytes)]);
        it.from.name = spec["from_name"].as_str().unwrap().into();
        it.from.address = spec["from_address"].as_str().unwrap().into();
        it.to = vec![spec["to"].as_str().unwrap().into()];
        it.cc = vec![spec["cc"].as_str().unwrap().into()];
        it.subject = spec["subject"].as_str().unwrap().into();
        it.body = spec["body"].as_str().unwrap().into();
        it.body_sha256 = sha256_hex(it.body.as_bytes());
        it.reply.as_mut().unwrap().in_reply_to = Some(spec["in_reply_to"].as_str().unwrap().into());
        it.reply.as_mut().unwrap().references.clear();
        let msgid = spec["message_id"].as_str().unwrap();
        verify(&python, &it, msgid, BccMode::Strip).unwrap();

        let (_d, f) = att_dir("py", &[(att_name, att_bytes)]);
        let ours = build(&it, &f, msgid, BccMode::Strip).unwrap().bytes;
        let (a, b) = (
            mailparse::parse_mail(&python).unwrap(),
            mailparse::parse_mail(&ours).unwrap(),
        );
        let names = |m: &ParsedMail| {
            let mut v: Vec<String> = m
                .headers
                .iter()
                .map(|h| h.get_key().to_ascii_lowercase())
                .collect();
            v.sort();
            v.dedup();
            v
        };
        assert_eq!(names(&a), names(&b));
        let (pa, pb) = (text_parts(&a).unwrap(), text_parts(&b).unwrap());
        assert_eq!(pa, pb);
        assert_eq!(attachments(&a).unwrap(), attachments(&b).unwrap());
    }
}
