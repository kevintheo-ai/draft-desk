//! IMAP for SMTP accounts: move the provider draft of a sent or discarded item to the trash, and file a copy in
//! the sent folder. Never deletes for good (no STORE, no EXPUNGE). Only compiled with the `send` feature.

use crate::address;
use crate::config::{AccountCfg, ImapCfg};
use crate::item::MESSAGE_ID_RE;
use crate::{Error, Result, bail};
use mailparse::MailHeaderMap;
use std::collections::BTreeSet;
use std::net::{TcpStream, ToSocketAddrs};

/// The operations Draft Desk needs from a mailbox: the real one over TLS, a fake in tests.
pub trait Mailbox {
    /// Asked after login: some servers announce MOVE only then.
    fn has_capability(&mut self, cap: &str) -> Result<bool>;
    /// (flags, name) of every folder.
    fn list(&mut self) -> Result<Vec<(Vec<String>, String)>>;
    fn select(&mut self, folder: &str) -> Result<()>;
    fn uid_search_message_id(&mut self, id: &str) -> Result<Vec<u32>>;
    fn uid_fetch_headers(&mut self, uid: u32) -> Result<Vec<u8>>;
    fn uid_move(&mut self, uid: u32, folder: &str) -> Result<()>;
    fn append(&mut self, folder: &str, flags: &[&str], bytes: &[u8]) -> Result<()>;
    fn logout(&mut self) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Drafts,
    Sent,
    Trash,
}

/// What the provider draft must still look like. A field edited in the window is left out (the mailbox draft
/// still carries the old value), but nothing foreign may be in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftTarget {
    pub to: Option<Vec<String>>,
    pub cc: Option<Vec<String>>,
    pub bcc: Option<Vec<String>>,
    pub subject: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashReport {
    pub moved: usize,
    /// "uid N: <why>" for every candidate left in place.
    pub kept: Vec<String>,
    pub trash: String,
}

/// The folder for a role: the RFC 6154 special-use flag first, then common names (case-insensitive).
pub fn find_folder(list: &[(Vec<String>, String)], role: Role) -> Option<String> {
    let (flag, names): (&str, &[&str]) = match role {
        Role::Drafts => ("\\drafts", &["drafts", "inbox.drafts", "[gmail]/drafts"]),
        Role::Sent => (
            "\\sent",
            &["sent", "inbox.sent", "sent items", "[gmail]/sent mail"],
        ),
        Role::Trash => (
            "\\trash",
            &[
                "trash",
                "inbox.trash",
                "deleted items",
                "deleted messages",
                "[gmail]/trash",
            ],
        ),
    };
    list.iter()
        .find(|(flags, _)| flags.iter().any(|f| f.eq_ignore_ascii_case(flag)))
        .or_else(|| {
            list.iter()
                .find(|(_, name)| names.contains(&name.to_lowercase().as_str()))
        })
        .map(|(_, name)| name.clone())
}

/// Moves the drafts with exactly this Message-ID whose headers still match `target` to the trash. Without MOVE or
/// without a recognizable trash nothing is touched. A failing logout after the work is no error.
pub fn trash_draft(
    mb: &mut dyn Mailbox,
    account: &AccountCfg,
    message_id: &str,
    target: &DraftTarget,
) -> Result<TrashReport> {
    if !MESSAGE_ID_RE.is_match(message_id) {
        bail!("invalid Message-ID {message_id:?}: need <local@domain>");
    }
    let result = (|| {
        if !mb.has_capability("MOVE")? {
            bail!("the IMAP server lacks MOVE: draft kept");
        }
        let list = mb.list()?;
        let drafts = find_folder(&list, Role::Drafts).unwrap_or_else(|| "Drafts".into());
        let Some(trash) = find_folder(&list, Role::Trash) else {
            bail!("no trash folder found: draft kept")
        };
        mb.select(&drafts)?;
        let mut report = TrashReport {
            moved: 0,
            kept: vec![],
            trash: trash.clone(),
        };
        let mut uids = mb.uid_search_message_id(message_id)?;
        uids.sort_unstable();
        for uid in uids {
            let reason = match mb.uid_fetch_headers(uid) {
                Ok(raw) => draft_mismatch(&raw, message_id, account, target),
                Err(_) => Some("headers not fetchable".into()),
            };
            if let Some(why) = reason {
                report.kept.push(format!("uid {uid}: {why}"));
                continue;
            }
            mb.uid_move(uid, &trash).map_err(|e| {
                Error(format!(
                    "UID MOVE {uid} to {trash:?} failed (already moved {}): {e}",
                    report.moved
                ))
            })?;
            report.moved += 1;
        }
        Ok(report)
    })();
    // Logging out is a side matter: the moves are done.
    let _ = mb.logout();
    result
}

/// Why the fetched headers are not the draft we mean, or `None` when they are. The search is only a substring
/// match, so the Message-ID is compared exactly; doubled or unreadable headers make a draft ambiguous.
fn draft_mismatch(
    raw: &[u8],
    message_id: &str,
    account: &AccountCfg,
    target: &DraftTarget,
) -> Option<String> {
    let Ok((headers, _)) = mailparse::parse_headers(raw) else {
        return Some("unparseable".into());
    };
    let ids: Vec<String> = headers
        .iter()
        .filter(|h| h.get_key().eq_ignore_ascii_case("Message-ID"))
        .map(|h| {
            String::from_utf8_lossy(h.get_value_raw())
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    if ids != [message_id] {
        return Some("message-id differs".into());
    }
    for name in ["From", "To", "Cc", "Bcc", "Subject"] {
        if headers.get_all_headers(name).len() > 1 {
            return Some(format!("{} twice", name.to_lowercase()));
        }
    }
    let addrs = |name: &str| -> std::result::Result<BTreeSet<String>, String> {
        let Some(h) = headers.get_first_header(name) else {
            return Ok(BTreeSet::new());
        };
        let list = mailparse::addrparse_header(h)
            .map_err(|_| format!("{} malformed", name.to_lowercase()))?;
        let mut out = BTreeSet::new();
        for a in list.iter() {
            match a {
                mailparse::MailAddr::Single(s) => out.insert(s.addr.to_lowercase()),
                mailparse::MailAddr::Group(_) => {
                    return Err(format!("{} malformed", name.to_lowercase()));
                }
            };
        }
        Ok(out)
    };
    let senders: BTreeSet<String> = account
        .senders
        .iter()
        .map(|s| s.address.to_lowercase())
        .collect();
    match addrs("From") {
        Ok(from) if from.len() == 1 && from.is_subset(&senders) => {}
        Ok(_) => return Some("from differs".into()),
        Err(e) => return Some(e),
    }
    for (name, wanted) in [("To", &target.to), ("Cc", &target.cc), ("Bcc", &target.bcc)] {
        let Some(wanted) = wanted else { continue };
        let mut expected = BTreeSet::new();
        for entry in wanted {
            match address::parse_list(entry) {
                Ok(list) => expected.extend(list.into_iter().map(|a| a.address.to_lowercase())),
                Err(_) => return Some(format!("{} not comparable", name.to_lowercase())),
            }
        }
        match addrs(name) {
            Ok(found) if found == expected => {}
            Ok(_) => return Some(format!("{} differs", name.to_lowercase())),
            Err(e) => return Some(e),
        }
    }
    if let Some(subject) = &target.subject {
        let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        if squash(&headers.get_first_value("Subject").unwrap_or_default()) != squash(subject) {
            return Some("subject differs".into());
        }
    }
    None
}

/// Files a copy in the sent folder, marked as read. Returns the folder.
pub fn append_sent(mb: &mut dyn Mailbox, bytes: &[u8]) -> Result<String> {
    let result = (|| {
        let folder = find_folder(&mb.list()?, Role::Sent).unwrap_or_else(|| "Sent".into());
        mb.append(&folder, &["\\Seen"], bytes)?;
        Ok(folder)
    })();
    let _ = mb.logout();
    result
}

/// Connecting, and every read or write on the connection, gives up after this.
pub const IMAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

type TlsTcp = native_tls::TlsStream<TcpStream>;

/// The real mailbox over implicit TLS (usually port 993), with time limits on connect, read and write.
// ponytail: implicit TLS only (no STARTTLS on 143); add it when a provider needs it.
pub struct ImapMailbox {
    session: imap::Session<TlsTcp>,
}

/// A connection to the first address that answers, each tried in turn (IPv6 first is common, and not every network
/// routes it); the last failure when none does.
fn connect_any(addrs: impl ToSocketAddrs, host: &str) -> Result<TcpStream> {
    let mut last = Error(format!("imap: cannot resolve {host}"));
    for addr in addrs.to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, IMAP_TIMEOUT) {
            Ok(tcp) => return Ok(tcp),
            Err(e) => last = e.into(),
        }
    }
    Err(last)
}

impl ImapMailbox {
    pub fn connect(cfg: &ImapCfg, password: &str) -> Result<ImapMailbox> {
        let tcp = connect_any((cfg.host.as_str(), cfg.port), &cfg.host)?;
        tcp.set_read_timeout(Some(IMAP_TIMEOUT))?;
        tcp.set_write_timeout(Some(IMAP_TIMEOUT))?;
        let connector =
            native_tls::TlsConnector::new().map_err(|e| Error(format!("imap tls: {e}")))?;
        let tls = connector
            .connect(&cfg.host, tcp)
            .map_err(|e| Error(format!("imap tls: {e}")))?;
        let mut client = imap::Client::new(tls);
        client.read_greeting().map_err(imap_err)?;
        let session = client
            .login(&cfg.username, password)
            .map_err(|(e, _)| Error(format!("imap login: {e}")))?;
        Ok(ImapMailbox { session })
    }
}

fn imap_err(e: imap::Error) -> Error {
    Error(format!("imap: {e}"))
}

/// LIST returns quoted names with their escapes still in place.
fn list_unescape(name: &str) -> String {
    let (mut out, mut escaped) = (String::new(), false);
    for c in name.chars() {
        if escaped || c != '\\' {
            out.push(c);
            escaped = false;
        } else {
            escaped = true;
        }
    }
    out
}

impl Mailbox for ImapMailbox {
    fn has_capability(&mut self, cap: &str) -> Result<bool> {
        Ok(self.session.capabilities().map_err(imap_err)?.has_str(cap))
    }
    fn list(&mut self) -> Result<Vec<(Vec<String>, String)>> {
        let names = self.session.list(Some(""), Some("*")).map_err(imap_err)?;
        Ok(names
            .iter()
            .map(|n| {
                // ponytail: attributes as their Debug names ("Drafts", "Trash", ...); the crate is pinned exactly.
                let flags = n.attributes().iter().map(|a| format!("\\{a:?}")).collect();
                (flags, list_unescape(n.name()))
            })
            .collect())
    }
    fn select(&mut self, folder: &str) -> Result<()> {
        self.session.select(folder).map(|_| ()).map_err(imap_err)
    }
    fn uid_search_message_id(&mut self, id: &str) -> Result<Vec<u32>> {
        if !MESSAGE_ID_RE.is_match(id) || id.contains('"') {
            bail!("invalid Message-ID for a search");
        }
        let uids = self
            .session
            .uid_search(format!("HEADER Message-ID \"{id}\""))
            .map_err(imap_err)?;
        Ok(uids.into_iter().collect())
    }
    fn uid_fetch_headers(&mut self, uid: u32) -> Result<Vec<u8>> {
        let fetches = self
            .session
            .uid_fetch(uid.to_string(), "BODY.PEEK[HEADER]")
            .map_err(imap_err)?;
        fetches
            .iter()
            .find_map(|f| f.header().map(<[u8]>::to_vec))
            .ok_or_else(|| Error("no headers".into()))
    }
    fn uid_move(&mut self, uid: u32, folder: &str) -> Result<()> {
        self.session
            .uid_mv(uid.to_string(), folder)
            .map_err(imap_err)
    }
    fn append(&mut self, folder: &str, flags: &[&str], bytes: &[u8]) -> Result<()> {
        // The crate puts the folder name between quotes without escaping.
        if folder.contains(['"', '\\', '\r', '\n']) {
            bail!("folder name {folder:?} cannot be used for APPEND");
        }
        let mut cmd = self.session.append(folder, bytes);
        for f in flags {
            cmd.flag(imap::types::Flag::from(f.to_string()));
        }
        cmd.finish().map(|_| ()).map_err(imap_err)
    }
    fn logout(&mut self) -> Result<()> {
        self.session.logout().map_err(imap_err)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Config;

    /// A mailbox in memory: drafts by uid with their raw headers.
    #[derive(Default)]
    pub(crate) struct FakeMailbox {
        pub caps: Vec<String>,
        pub folders: Vec<(Vec<String>, String)>,
        pub drafts: Vec<(u32, Vec<u8>)>,
        pub selected: Option<String>,
        pub moved: Vec<(u32, String)>,
        pub appended: Vec<(String, Vec<String>, Vec<u8>)>,
        pub refuse_move: bool,
        pub fail_logout: bool,
        pub calls: Vec<String>,
    }

    impl Mailbox for FakeMailbox {
        fn has_capability(&mut self, cap: &str) -> Result<bool> {
            self.calls.push("capabilities".into());
            Ok(self.caps.iter().any(|c| c.eq_ignore_ascii_case(cap)))
        }
        fn list(&mut self) -> Result<Vec<(Vec<String>, String)>> {
            self.calls.push("list".into());
            Ok(self.folders.clone())
        }
        fn select(&mut self, folder: &str) -> Result<()> {
            self.calls.push(format!("select {folder}"));
            self.selected = Some(folder.into());
            Ok(())
        }
        fn uid_search_message_id(&mut self, id: &str) -> Result<Vec<u32>> {
            // Substring search, as servers do.
            Ok(self
                .drafts
                .iter()
                .filter(|(_, h)| String::from_utf8_lossy(h).contains(id))
                .map(|(u, _)| *u)
                .collect())
        }
        fn uid_fetch_headers(&mut self, uid: u32) -> Result<Vec<u8>> {
            self.drafts
                .iter()
                .find(|(u, _)| *u == uid)
                .map(|(_, h)| h.clone())
                .ok_or(Error("no such uid".into()))
        }
        fn uid_move(&mut self, uid: u32, folder: &str) -> Result<()> {
            if self.refuse_move {
                bail!("NO move refused");
            }
            self.moved.push((uid, folder.into()));
            Ok(())
        }
        fn append(&mut self, folder: &str, flags: &[&str], bytes: &[u8]) -> Result<()> {
            self.appended.push((
                folder.into(),
                flags.iter().map(|f| f.to_string()).collect(),
                bytes.to_vec(),
            ));
            Ok(())
        }
        fn logout(&mut self) -> Result<()> {
            self.calls.push("logout".into());
            if self.fail_logout {
                Err(Error("BYE".into()))
            } else {
                Ok(())
            }
        }
    }

    pub(crate) fn account() -> AccountCfg {
        Config::parse(
            r#"{"accounts":[{"name":"work","kind":"smtp",
              "senders":[{"address":"me@example.org"},{"address":"alias@example.org"}],
              "smtp":{"host":"smtp.example.org","port":465,"security":"tls","username":"me@example.org"},
              "imap":{"host":"imap.example.org","port":993,"username":"me@example.org"}}]}"#,
        )
        .unwrap()
        .accounts
        .remove(0)
    }

    const MID: &str = "<d.1@example.org>";

    pub(crate) fn headers(extra: &str) -> Vec<u8> {
        format!("Message-ID: {MID}\r\nFrom: Me <me@example.org>\r\nTo: Ann <ann@example.org>\r\nSubject: Re: Offer\r\n{extra}\r\n")
            .into_bytes()
    }

    pub(crate) fn mailbox(drafts: Vec<(u32, Vec<u8>)>) -> FakeMailbox {
        FakeMailbox {
            caps: vec!["IMAP4rev1".into(), "MOVE".into()],
            folders: vec![
                (vec!["\\HasNoChildren".into()], "INBOX".into()),
                (vec!["\\Drafts".into()], "Entwürfe".into()),
                (vec!["\\Trash".into()], "Deleted Items".into()),
                (vec!["\\Sent".into()], "Sent Items".into()),
            ],
            drafts,
            ..Default::default()
        }
    }

    pub(crate) fn target() -> DraftTarget {
        DraftTarget {
            to: Some(vec!["Ann <ann@example.org>".into()]),
            cc: Some(vec![]),
            bcc: Some(vec![]),
            subject: Some("Re: Offer".into()),
        }
    }

    #[test]
    fn connect_tries_every_address() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let refused = closed.local_addr().unwrap();
        drop(closed);
        let open = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addrs = [refused, open.local_addr().unwrap()];
        assert!(connect_any(&addrs[..], "imap.example.org").is_ok());
        let e = connect_any(&addrs[..1], "imap.example.org").unwrap_err();
        assert!(e.0.contains("refused"), "{e}");
        let none: [std::net::SocketAddr; 0] = [];
        assert!(
            connect_any(&none[..], "imap.example.org")
                .unwrap_err()
                .0
                .contains("cannot resolve imap.example.org")
        );
    }

    #[test]
    fn matching_draft_moves_only() {
        let mut mb = mailbox(vec![
            (7, headers("")),
            (8, headers("Cc: x@example.org\r\n")),
        ]);
        let r = trash_draft(&mut mb, &account(), MID, &target()).unwrap();
        assert_eq!(r.moved, 1);
        assert_eq!(r.trash, "Deleted Items");
        assert_eq!(mb.moved, vec![(7, "Deleted Items".to_string())]);
        assert_eq!(r.kept, vec!["uid 8: cc differs".to_string()]);
        assert_eq!(mb.selected.as_deref(), Some("Entwürfe"));
    }

    #[test]
    fn post_login_capabilities_decide() {
        let mut mb = mailbox(vec![(7, headers(""))]);
        trash_draft(&mut mb, &account(), MID, &target()).unwrap();
        assert_eq!(mb.calls[0], "capabilities");
    }

    #[test]
    fn mismatch_keeps_draft() {
        let cases = [
            (
                "Message-ID: <d.1@example.org>\r\nFrom: Me <me@example.org>\r\nTo: bob@example.org\r\nSubject: Re: Offer\r\n\r\n",
                "to",
            ),
            (
                "Message-ID: <d.1@example.org>\r\nFrom: Me <me@example.org>\r\nTo: Ann <ann@example.org>\r\nSubject: Other\r\n\r\n",
                "subject",
            ),
            (
                "Message-ID: <d.1@example.org>\r\nFrom: Eve <eve@example.org>\r\nTo: Ann <ann@example.org>\r\nSubject: Re: Offer\r\n\r\n",
                "from",
            ),
            (
                "Message-ID: <d.1@example.org> <e.2@example.org>\r\nFrom: Me <me@example.org>\r\nTo: Ann <ann@example.org>\r\nSubject: Re: Offer\r\n\r\n",
                "message-id",
            ),
            (
                "Message-ID: <d.1@example.org>\r\nFrom: Me <me@example.org>\r\nTo: Ann <ann@example.org>\r\nBcc: z@example.org\r\nSubject: Re: Offer\r\n\r\n",
                "bcc",
            ),
            (
                "Message-ID: <d.1@example.org>\r\nFrom: Me <me@example.org>\r\nTo: ann@example.org; z@example.org\r\nSubject: Re: Offer\r\n\r\n",
                "to",
            ),
        ];
        for (raw, why) in cases {
            let mut mb = mailbox(vec![(9, raw.as_bytes().to_vec())]);
            let r = trash_draft(&mut mb, &account(), MID, &target()).unwrap();
            assert_eq!(r.moved, 0, "{why}");
            assert!(mb.moved.is_empty());
            assert!(r.kept[0].contains(why), "{why}: {:?}", r.kept);
        }
    }

    #[test]
    fn two_message_id_lines_kept() {
        let raw = format!(
            "Message-ID: {MID}\r\n{}",
            String::from_utf8(headers("")).unwrap()
        );
        let mut mb = mailbox(vec![(3, raw.into_bytes())]);
        assert_eq!(
            trash_draft(&mut mb, &account(), MID, &target())
                .unwrap()
                .moved,
            0
        );
    }

    #[test]
    fn doubled_header_kept() {
        let mut mb = mailbox(vec![(3, headers("Subject: Re: Offer\r\n"))]);
        let r = trash_draft(&mut mb, &account(), MID, &target()).unwrap();
        assert_eq!(r.moved, 0);
        assert!(r.kept[0].contains("subject twice"), "{:?}", r.kept);
    }

    #[test]
    fn encoded_subject_and_folding_match() {
        let raw = format!(
            "Message-ID: {MID}\r\nFrom: =?utf-8?q?M=C3=BCller?= <me@example.org>\r\nTo: Ann\r\n <ann@example.org>\r\nSubject: =?utf-8?q?Gr=C3=BC=C3=9Fe?=\r\n  zum   Angebot\r\n\r\n"
        );
        let mut mb = mailbox(vec![(3, raw.into_bytes())]);
        let t = DraftTarget {
            subject: Some("Grüße zum Angebot".into()),
            ..target()
        };
        assert_eq!(trash_draft(&mut mb, &account(), MID, &t).unwrap().moved, 1);
    }

    #[test]
    fn fields_outside_target_not_compared() {
        let mut mb = mailbox(vec![(3, headers("Cc: someone@example.org\r\n"))]);
        let t = DraftTarget {
            cc: None,
            subject: None,
            ..target()
        };
        assert_eq!(trash_draft(&mut mb, &account(), MID, &t).unwrap().moved, 1);
    }

    #[test]
    fn missing_draft_moves_nothing() {
        let mut mb = mailbox(vec![]);
        let r = trash_draft(&mut mb, &account(), MID, &target()).unwrap();
        assert_eq!((r.moved, r.kept.len()), (0, 0));
    }

    #[test]
    fn no_move_capability_touches_nothing() {
        let mut mb = mailbox(vec![(7, headers(""))]);
        mb.caps = vec!["IMAP4rev1".into()];
        assert!(
            trash_draft(&mut mb, &account(), MID, &target())
                .unwrap_err()
                .0
                .contains("MOVE")
        );
        assert!(mb.moved.is_empty() && mb.selected.is_none());
    }

    #[test]
    fn no_trash_folder_no_guess() {
        let mut mb = mailbox(vec![(7, headers(""))]);
        mb.folders = vec![(vec![], "INBOX".into()), (vec![], "Papierkorb-alt".into())];
        assert!(
            trash_draft(&mut mb, &account(), MID, &target())
                .unwrap_err()
                .0
                .contains("trash")
        );
        assert!(mb.moved.is_empty());
    }

    #[test]
    fn fallback_names_case_insensitive() {
        let list = vec![
            (vec![], "INBOX.Trash".to_string()),
            (vec![], "Sent Items".to_string()),
        ];
        assert_eq!(
            find_folder(&list, Role::Trash).as_deref(),
            Some("INBOX.Trash")
        );
        assert_eq!(
            find_folder(&list, Role::Sent).as_deref(),
            Some("Sent Items")
        );
        assert_eq!(find_folder(&list, Role::Drafts), None);
    }

    #[test]
    fn refused_move_is_error() {
        let mut mb = mailbox(vec![(7, headers(""))]);
        mb.refuse_move = true;
        assert!(trash_draft(&mut mb, &account(), MID, &target()).is_err());
    }

    #[test]
    fn failing_logout_after_move_is_success() {
        let mut mb = mailbox(vec![(7, headers(""))]);
        mb.fail_logout = true;
        assert_eq!(
            trash_draft(&mut mb, &account(), MID, &target())
                .unwrap()
                .moved,
            1
        );
        assert_eq!(mb.calls.last().map(String::as_str), Some("logout"));
    }

    #[test]
    fn alias_sender_ok_foreign_kept() {
        let alias = String::from_utf8(headers(""))
            .unwrap()
            .replace("me@example.org", "alias@example.org");
        let mut mb = mailbox(vec![(1, alias.into_bytes())]);
        assert_eq!(
            trash_draft(&mut mb, &account(), MID, &target())
                .unwrap()
                .moved,
            1
        );
    }

    #[test]
    fn invalid_message_id_refused() {
        let mut mb = mailbox(vec![(7, headers(""))]);
        assert!(trash_draft(&mut mb, &account(), "d.1@example.org", &target()).is_err());
        assert!(mb.calls.is_empty());
    }

    #[test]
    fn list_escapes_not_doubled() {
        assert_eq!(list_unescape(r#"Say \"hi\""#), r#"Say "hi""#);
        assert_eq!(list_unescape(r"a\\b"), r"a\b");
        assert_eq!(list_unescape("Deleted Items"), "Deleted Items");
    }

    #[test]
    fn append_sent_uses_seen_flag_and_sent_folder() {
        let mut mb = mailbox(vec![]);
        assert_eq!(append_sent(&mut mb, b"mail").unwrap(), "Sent Items");
        assert_eq!(
            mb.appended,
            vec![(
                "Sent Items".to_string(),
                vec!["\\Seen".to_string()],
                b"mail".to_vec()
            )]
        );
        let mut bare = mailbox(vec![]);
        bare.folders = vec![(vec![], "INBOX".into())];
        assert_eq!(append_sent(&mut bare, b"m").unwrap(), "Sent");
    }
}
