//! Sending over SMTP with TLS (implicit on 465 or STARTTLS on 587), password from the keychain.
//!
//! The phase of a failure decides the outcome. Connecting, TLS and login happen before any mail command: a failure
//! there means nothing was handed over (`rejected`). During the mail transaction a definite reply from the server
//! (4xx or 5xx, also on a rejected recipient, which ends the transaction before DATA) means the server did not
//! take the mail (`rejected`). Everything else there, such as a dropped connection or a time-out after the
//! content went out, is `unknown`: never resend automatically.

use super::{CleanupReason, CleanupResult, Outcome, Transport};
use crate::address::to_lettre;
use crate::config::{AccountCfg, Security};
use crate::imap::{DraftTarget, ImapMailbox, Mailbox, append_sent, trash_draft};
use crate::item::Item;
use crate::mime::Built;
use crate::secrets::Secrets;
use lettre::Address;
use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{SmtpConnection, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use std::time::Duration;

pub const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    ImplicitTls,
    StartTls,
    /// Only for the local fake server in tests.
    #[cfg(test)]
    Plain,
}

pub struct SmtpTransport<'a> {
    account: AccountCfg,
    secrets: &'a dyn Secrets,
    mode: Mode,
    timeout: Duration,
}

impl<'a> SmtpTransport<'a> {
    /// `None` when the account has no SMTP section.
    pub fn new(account: &AccountCfg, secrets: &'a dyn Secrets) -> Option<SmtpTransport<'a>> {
        let mode = match account.smtp.as_ref()?.security {
            Security::Tls => Mode::ImplicitTls,
            Security::Starttls => Mode::StartTls,
        };
        Some(SmtpTransport {
            account: account.clone(),
            secrets,
            mode,
            timeout: SMTP_TIMEOUT,
        })
    }

    /// Connect, TLS, login. Every failure here happens before a mail command: nothing was handed over.
    fn connect(&self) -> Result<SmtpConnection, Outcome> {
        let smtp = self.account.smtp.as_ref().expect("checked in new");
        let rejected = |code: &str| Outcome::Rejected { code: code.into() };
        let Ok(Some(password)) = self.secrets.get(&format!("{}:smtp", self.account.name)) else {
            return Err(rejected("auth"));
        };
        let tls = TlsParameters::new(smtp.host.clone()).map_err(|_| rejected("connect"))?;
        let hello = ClientId::default();
        let server = (smtp.host.as_str(), smtp.port);
        let mut conn = match self.mode {
            Mode::ImplicitTls => {
                SmtpConnection::connect(server, Some(self.timeout), &hello, Some(&tls), None)
            }
            Mode::StartTls => {
                SmtpConnection::connect(server, Some(self.timeout), &hello, None, None).and_then(
                    |mut c| {
                        // Never fall back to plain text: no STARTTLS offered is an error.
                        c.starttls(&tls, &hello)?;
                        Ok(c)
                    },
                )
            }
            #[cfg(test)]
            Mode::Plain => SmtpConnection::connect(server, Some(self.timeout), &hello, None, None),
        }
        .map_err(|_| rejected("connect"))?;
        let creds = Credentials::new(smtp.username.clone(), password);
        if conn
            .auth(&[Mechanism::Plain, Mechanism::Login], &creds)
            .is_err()
        {
            conn.abort();
            return Err(rejected("auth"));
        }
        Ok(conn)
    }
}

impl Transport for SmtpTransport<'_> {
    fn send(&self, built: &Built, item: &Item, _message_id: &str) -> Outcome {
        let smtp = self.account.smtp.as_ref().expect("checked in new");
        // Envelope sender: the login when it is an address (what most servers expect), else the From address.
        let from: Option<Address> = to_lettre(&smtp.username)
            .ok()
            .or_else(|| to_lettre(&item.from.address).ok());
        let rcpts: Result<Vec<Address>, _> =
            built.envelope_rcpts.iter().map(|r| to_lettre(r)).collect();
        let Ok(envelope) = rcpts
            .map_err(|_| ())
            .and_then(|to| Envelope::new(from, to).map_err(|_| ()))
        else {
            return Outcome::Rejected {
                code: "envelope".into(),
            };
        };
        let mut conn = match self.connect() {
            Ok(c) => c,
            Err(outcome) => return outcome,
        };
        let outcome = match conn.send(&envelope, &built.bytes) {
            Ok(resp) => Outcome::Accepted {
                provider_id: resp.first_line().map(|l| l.trim().to_string()),
            },
            Err(e) if e.is_permanent() => Outcome::Rejected {
                code: "smtp_5xx".into(),
            },
            Err(e) if e.is_transient() => Outcome::Rejected {
                code: "smtp_4xx".into(),
            },
            // Client-side checks (8BITMIME, SMTPUTF8) run before MAIL FROM.
            Err(e) if e.is_client() => Outcome::Rejected {
                code: "smtp_client".into(),
            },
            Err(e) => Outcome::Unknown {
                reason: format!("smtp: {e}"),
            },
        };
        if matches!(outcome, Outcome::Unknown { .. }) {
            conn.abort();
        } else {
            let _ = conn.quit();
        }
        outcome
    }

    fn cleanup_draft(&self, item: &Item, _reason: CleanupReason) -> CleanupResult {
        if item
            .mailbox_draft
            .as_ref()
            .and_then(|m| m.message_id.as_ref())
            .is_none()
        {
            return CleanupResult::Skipped("no mailbox draft".into());
        }
        match self.mailbox() {
            Ok(mut mb) => cleanup_with(&mut mb, &self.account, item),
            Err(e) => CleanupResult::Failed(e),
        }
    }

    fn save_sent_copy(&self, built: &Built, item: &Item) -> CleanupResult {
        if !self.account.save_sent_copy {
            return CleanupResult::Skipped("sent copy not configured".into());
        }
        match self.mailbox() {
            Ok(mut mb) => sent_copy_with(&mut mb, built, item),
            Err(e) => CleanupResult::Failed(e),
        }
    }
}

impl SmtpTransport<'_> {
    fn mailbox(&self) -> Result<ImapMailbox, String> {
        let imap = self.account.imap.as_ref().ok_or("no IMAP configured")?;
        let password = match self.secrets.get(&format!("{}:imap", self.account.name)) {
            Ok(Some(p)) => p,
            _ => return Err("no IMAP password in the keychain".into()),
        };
        ImapMailbox::connect(imap, &password).map_err(|e| e.0)
    }
}

/// The draft as the mailbox should still hold it: fields edited in the window are left out.
pub(crate) fn draft_target(item: &Item) -> DraftTarget {
    let keep = |field: &str| !item.edits.fields_changed.iter().any(|f| f == field);
    DraftTarget {
        to: keep("to").then(|| item.to.clone()),
        cc: keep("cc").then(|| item.cc.clone()),
        bcc: keep("bcc").then(|| item.bcc.clone()),
        subject: keep("subject").then(|| item.subject.clone()),
    }
}

fn cleanup_with(mb: &mut dyn Mailbox, account: &AccountCfg, item: &Item) -> CleanupResult {
    let Some(mid) = item
        .mailbox_draft
        .as_ref()
        .and_then(|m| m.message_id.as_deref())
    else {
        return CleanupResult::Skipped("no mailbox draft".into());
    };
    match trash_draft(mb, account, mid, &draft_target(item)) {
        Ok(r) => {
            let kept = if r.kept.is_empty() {
                String::new()
            } else {
                format!("; kept {}", r.kept.join(", "))
            };
            if r.moved > 0 {
                CleanupResult::Done(format!("moved {} draft(s) to {}{kept}", r.moved, r.trash))
            } else {
                CleanupResult::Skipped(format!("no matching draft{kept}"))
            }
        }
        Err(e) => CleanupResult::Failed(e.0),
    }
}

/// The sent copy carries the Bcc recipients the sent mail had only in its envelope.
// ponytail: addresses without display names, which keeps the added header plain ASCII.
fn sent_copy_with(mb: &mut dyn Mailbox, built: &Built, item: &Item) -> CleanupResult {
    let bcc: Vec<String> = item
        .bcc
        .iter()
        .filter_map(|e| crate::address::parse_list(e).ok())
        .flatten()
        .map(|a| a.address)
        .collect();
    let mut bytes = Vec::new();
    if !bcc.is_empty() {
        bytes.extend_from_slice(format!("Bcc: {}\r\n", bcc.join(", ")).as_bytes());
    }
    bytes.extend_from_slice(&built.bytes);
    match append_sent(mb, &bytes) {
        Ok(folder) => CleanupResult::Done(format!("sent copy filed in {folder}")),
        Err(e) => CleanupResult::Failed(e.0),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Config;
    use crate::item::tests::item;
    use crate::mime::{BccMode, build};
    use crate::secrets::MemorySecrets;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub(crate) struct Fake {
        pub auth_fails: bool,
        pub reject_rcpt: Option<String>,
        pub drop_after_data: bool,
        pub reply_after_data: Option<String>,
    }

    /// A one-connection SMTP server on 127.0.0.1 that records every line it receives.
    pub(crate) fn fake_server(fake: Fake) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let rec = log.clone();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut w = stream.try_clone().unwrap();
            let mut r = BufReader::new(stream);
            let mut say = |s: &str| {
                let _ = w.write_all(s.as_bytes());
            };
            say("220 fake ESMTP\r\n");
            let mut line = String::new();
            while r.read_line(&mut line).unwrap_or(0) > 0 {
                let cmd = line.trim_end().to_string();
                rec.lock().unwrap().push(cmd.clone());
                let upper = cmd.to_ascii_uppercase();
                if upper.starts_with("EHLO") {
                    say("250-fake\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n");
                } else if upper.starts_with("AUTH") {
                    say(if fake.auth_fails {
                        "535 5.7.8 bad credentials\r\n"
                    } else {
                        "235 2.7.0 ok\r\n"
                    });
                } else if upper.starts_with("MAIL FROM") {
                    say("250 ok\r\n");
                } else if upper.starts_with("RCPT TO") {
                    let bad = fake
                        .reject_rcpt
                        .as_ref()
                        .is_some_and(|b| cmd.contains(b.as_str()));
                    say(if bad {
                        "550 5.1.1 no such user\r\n"
                    } else {
                        "250 ok\r\n"
                    });
                } else if upper == "DATA" {
                    say("354 go ahead\r\n");
                    if fake.drop_after_data {
                        return;
                    }
                    let mut data = String::new();
                    loop {
                        data.clear();
                        if r.read_line(&mut data).unwrap_or(0) == 0 {
                            return;
                        }
                        if data == ".\r\n" {
                            break;
                        }
                        rec.lock()
                            .unwrap()
                            .push(format!("DATA| {}", data.trim_end()));
                    }
                    say(fake
                        .reply_after_data
                        .as_deref()
                        .unwrap_or("250 2.0.0 queued as Q1\r\n"));
                } else if upper == "QUIT" {
                    say("221 bye\r\n");
                    return;
                } else {
                    say("500 what\r\n");
                }
                line.clear();
            }
        });
        (port, log)
    }

    pub(crate) fn transport<'a>(port: u16, secrets: &'a MemorySecrets) -> SmtpTransport<'a> {
        let cfg = Config::parse(&format!(
            r#"{{"accounts":[{{"name":"work","kind":"smtp","senders":[{{"address":"me@example.org","name":"Me"}}],
              "smtp":{{"host":"127.0.0.1","port":{port},"security":"tls","username":"me@example.org"}}}}]}}"#
        ))
        .unwrap();
        secrets.set("work:smtp", "pw").unwrap();
        let mut t = SmtpTransport::new(&cfg.accounts[0], secrets).unwrap();
        t.mode = Mode::Plain;
        t.timeout = Duration::from_secs(5);
        t
    }

    fn built(it: &Item) -> Built {
        let dir = std::fs::File::open(std::env::temp_dir()).unwrap();
        build(it, &dir, "<m.1@example.org>", BccMode::Strip).unwrap()
    }

    fn send(fake: Fake, it: &Item) -> (Outcome, Vec<String>) {
        let (port, log) = fake_server(fake);
        let secrets = MemorySecrets::default();
        let o = transport(port, &secrets).send(&built(it), it, "<m.1@example.org>");
        std::thread::sleep(Duration::from_millis(50));
        let lines = log.lock().unwrap().clone();
        (o, lines)
    }

    #[test]
    fn happy_path_envelope_and_data() {
        let (o, log) = send(Fake::default(), &item());
        assert_eq!(
            o,
            Outcome::Accepted {
                provider_id: Some("2.0.0 queued as Q1".into())
            }
        );
        assert!(log.iter().any(|l| l.starts_with("AUTH")));
        assert!(
            log.contains(&"MAIL FROM:<me@example.org>".to_string()),
            "{log:?}"
        );
        assert!(log.contains(&"RCPT TO:<ann@example.org>".to_string()));
        assert!(
            log.iter()
                .any(|l| l == "DATA| Message-ID: <m.1@example.org>")
        );
    }

    #[test]
    fn bcc_only_in_envelope() {
        let mut it = item();
        it.bcc = vec!["hidden@example.org".into()];
        let (o, log) = send(Fake::default(), &it);
        assert!(matches!(o, Outcome::Accepted { .. }));
        assert!(log.contains(&"RCPT TO:<hidden@example.org>".to_string()));
        assert!(
            !log.iter()
                .any(|l| l.starts_with("DATA|") && l.contains("hidden@")),
            "{log:?}"
        );
    }

    #[test]
    fn long_local_part_reaches_the_envelope() {
        let rcpt = "ticket-replies-long-routing-x+0f1e2d3c-4b5a-4978-8a9b-0c1d2e3f4a5b@example.org";
        let mut it = item();
        it.to = vec![rcpt.into()];
        let (o, log) = send(Fake::default(), &it);
        assert!(matches!(o, Outcome::Accepted { .. }), "{o:?}");
        assert!(log.contains(&format!("RCPT TO:<{rcpt}>")), "{log:?}");
    }

    #[test]
    fn rcpt_550_is_rejected_no_data() {
        let mut it = item();
        it.cc = vec!["bad@example.org".into()];
        let (o, log) = send(
            Fake {
                reject_rcpt: Some("bad@".into()),
                ..Default::default()
            },
            &it,
        );
        assert!(
            matches!(o, Outcome::Rejected { ref code } if code == "smtp_5xx"),
            "{o:?}"
        );
        assert!(!log.iter().any(|l| l == "DATA"), "{log:?}");
    }

    #[test]
    fn reply_after_data_decides() {
        let (o, _) = send(
            Fake {
                reply_after_data: Some("554 5.7.1 spam\r\n".into()),
                ..Default::default()
            },
            &item(),
        );
        assert!(
            matches!(o, Outcome::Rejected { ref code } if code == "smtp_5xx"),
            "{o:?}"
        );
        let (o, _) = send(
            Fake {
                reply_after_data: Some("451 4.3.0 try later\r\n".into()),
                ..Default::default()
            },
            &item(),
        );
        assert!(
            matches!(o, Outcome::Rejected { ref code } if code == "smtp_4xx"),
            "{o:?}"
        );
    }

    #[test]
    fn drop_after_data_is_unknown() {
        let (o, _) = send(
            Fake {
                drop_after_data: true,
                ..Default::default()
            },
            &item(),
        );
        assert!(matches!(o, Outcome::Unknown { .. }), "{o:?}");
    }

    #[test]
    fn auth_fail_is_rejected() {
        let (o, log) = send(
            Fake {
                auth_fails: true,
                ..Default::default()
            },
            &item(),
        );
        assert!(
            matches!(o, Outcome::Rejected { ref code } if code == "auth"),
            "{o:?}"
        );
        assert!(!log.iter().any(|l| l.starts_with("MAIL")));
    }

    #[test]
    fn connect_refused_is_rejected() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let secrets = MemorySecrets::default();
        let it = item();
        let o = transport(port, &secrets).send(&built(&it), &it, "<m.1@example.org>");
        assert!(
            matches!(o, Outcome::Rejected { ref code } if code == "connect"),
            "{o:?}"
        );
    }

    #[test]
    fn cleanup_moves_matching_draft_and_skips_edited_fields() {
        use crate::imap::tests::{account, headers, mailbox};
        use crate::item::MailboxDraft;
        let mut it = item();
        it.mailbox_draft = Some(MailboxDraft {
            message_id: Some("<d.1@example.org>".into()),
            ..Default::default()
        });
        let mut mb = mailbox(vec![(7, headers(""))]);
        assert!(matches!(
            cleanup_with(&mut mb, &account(), &it),
            CleanupResult::Done(_)
        ));
        // The subject was changed in the window: the mailbox draft still has the old one and matches.
        it.subject = "Changed in the window".into();
        it.edits.fields_changed = vec!["subject".into()];
        assert_eq!(draft_target(&it).subject, None);
        let mut mb = mailbox(vec![(7, headers(""))]);
        assert!(matches!(
            cleanup_with(&mut mb, &account(), &it),
            CleanupResult::Done(_)
        ));
        // Without the edit note the subject is compared and the draft stays.
        it.edits.fields_changed.clear();
        let mut mb = mailbox(vec![(7, headers(""))]);
        assert!(matches!(
            cleanup_with(&mut mb, &account(), &it),
            CleanupResult::Skipped(_)
        ));
        assert!(mb.moved.is_empty());
        it.mailbox_draft = None;
        assert!(matches!(
            cleanup_with(&mut mb, &account(), &it),
            CleanupResult::Skipped(_)
        ));
    }

    #[test]
    fn sent_copy_carries_bcc() {
        use crate::imap::tests::mailbox;
        let mut it = item();
        it.bcc = vec!["Hidden <hidden@example.org>".into()];
        let b = built(&it);
        let mut mb = mailbox(vec![]);
        assert!(matches!(
            sent_copy_with(&mut mb, &b, &it),
            CleanupResult::Done(_)
        ));
        let copy = mb.appended[0].2.clone();
        assert!(copy.starts_with(b"Bcc: hidden@example.org\r\n"));
        assert!(copy.ends_with(&b.bytes));
    }

    #[test]
    fn missing_password_is_rejected() {
        let (port, _log) = fake_server(Fake::default());
        let secrets = MemorySecrets::default();
        let t = transport(port, &secrets);
        secrets.delete("work:smtp").unwrap();
        let it = item();
        let o = t.send(&built(&it), &it, "<m.1@example.org>");
        assert!(
            matches!(o, Outcome::Rejected { ref code } if code == "auth"),
            "{o:?}"
        );
        // Unblock the fake server thread.
        let _ = TcpStream::connect(("127.0.0.1", port));
    }
}
