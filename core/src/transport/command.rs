//! Sending through a configured program: the finished MIME (without a Bcc header) on stdin, the envelope in the
//! environment, one JSON line `{"status":"sent","provider_message_id":".."}` on stdout.
//!
//! Exit codes: 0 with that line = accepted; 0 without it = unknown; 2, 3 or 4 = rejected (the program promises
//! nothing was sent); anything else, a signal, no exit status or the time limit = unknown.
//!
//! The optional discard program cleans up the mailbox draft after a send or a discard: on stdin the item as the check
//! command sees it plus `mailbox_draft` and `fields_changed` (fields edited in the window, sorted), in the environment
//! `DRAFTDESK_ACCOUNT` and `DRAFTDESK_REASON` (`sent` or `discarded`); exit 0 = done.

use super::{CleanupReason, CleanupResult, Outcome, Transport};
use crate::checks::item_view;
use crate::config::AccountCfg;
use crate::item::Item;
use crate::mime::Built;
use crate::process;
use serde::Deserialize;
use std::time::Duration;

pub struct CommandTransport {
    pub account: String,
    pub send_argv: Vec<String>,
    pub discard_argv: Option<Vec<String>>,
    pub timeout: Duration,
}

impl CommandTransport {
    /// The account's programs and time limit; `None` without a send program.
    pub fn for_account(acc: &AccountCfg) -> Option<CommandTransport> {
        Some(CommandTransport {
            account: acc.name.clone(),
            send_argv: acc.send_command.clone()?,
            discard_argv: acc.discard_command.clone(),
            timeout: acc.send_timeout(),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirmation {
    status: String,
    #[serde(default)]
    provider_message_id: Option<String>,
}

impl Transport for CommandTransport {
    fn send(&self, built: &Built, _item: &Item, message_id: &str) -> Outcome {
        let env = [
            ("DRAFTDESK_ACCOUNT", self.account.clone()),
            ("DRAFTDESK_MESSAGE_ID", message_id.to_string()),
            ("DRAFTDESK_ENVELOPE_RCPTS", built.envelope_rcpts.join(",")),
        ];
        let out = match process::run(&self.send_argv, &env, built.bytes.clone(), self.timeout) {
            Ok(o) => o,
            // The program never started, so it cannot have sent anything.
            Err(e) => {
                return Outcome::Rejected {
                    code: format!("command_start: {e}"),
                };
            }
        };
        if out.timed_out {
            return Outcome::Unknown {
                reason: "send command hit the time limit".into(),
            };
        }
        match out.code {
            Some(0) => {
                let line = String::from_utf8_lossy(&out.stdout);
                let confirmed = line
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .and_then(|l| serde_json::from_str::<Confirmation>(l).ok())
                    .filter(|c| c.status == "sent");
                match confirmed {
                    Some(c) => Outcome::Accepted {
                        provider_id: c.provider_message_id,
                    },
                    None => Outcome::Unknown {
                        reason: "send command ended without a confirmation line".into(),
                    },
                }
            }
            Some(c @ 2..=4) => Outcome::Rejected {
                code: format!("command_exit_{c}"),
            },
            Some(c) => Outcome::Unknown {
                reason: format!("send command ended with exit {c}"),
            },
            None => Outcome::Unknown {
                reason: "send command ended by a signal or without an exit status".into(),
            },
        }
    }

    fn cleanup_draft(&self, item: &Item, reason: CleanupReason) -> CleanupResult {
        let Some(argv) = &self.discard_argv else {
            return CleanupResult::Skipped("no discard command".into());
        };
        let Some(draft) = &item.mailbox_draft else {
            return CleanupResult::Skipped("no mailbox draft".into());
        };
        let mut view = item_view(item, None);
        view["mailbox_draft"] = serde_json::to_value(draft).unwrap_or_default();
        // Fields edited in the window no longer match the mailbox draft: the program leaves them out of its match.
        let mut changed = item.edits.fields_changed.clone();
        changed.sort();
        view["fields_changed"] = changed.into();
        let env = [
            ("DRAFTDESK_ACCOUNT", self.account.clone()),
            ("DRAFTDESK_REASON", reason.as_str().to_string()),
        ];
        match process::run(argv, &env, view.to_string().into_bytes(), self.timeout) {
            Ok(o) if o.code == Some(0) && !o.timed_out => {
                CleanupResult::Done("discard command succeeded".into())
            }
            Ok(o) => CleanupResult::Failed(format!("discard command failed (exit {:?})", o.code)),
            Err(e) => CleanupResult::Failed(format!("discard command: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::MailboxDraft;
    use crate::item::tests::item;
    use crate::mime::{BccMode, build};
    use std::path::{Path, PathBuf};

    fn bin(name: &str) -> String {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/bin")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    struct Out(PathBuf);
    impl Drop for Out {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn out_dir() -> Out {
        let d = std::env::temp_dir().join(format!(
            "dd-cmd-{}-{}",
            std::process::id(),
            crate::random_hex(4)
        ));
        std::fs::create_dir_all(&d).unwrap();
        Out(d)
    }

    fn transport(argv: Vec<String>, timeout: Duration) -> CommandTransport {
        CommandTransport {
            account: "custom".into(),
            send_argv: argv,
            discard_argv: None,
            timeout,
        }
    }

    fn built(it: &Item) -> Built {
        let dir = std::fs::File::open(std::env::temp_dir()).unwrap();
        build(it, &dir, "<m.1@example.org>", BccMode::Strip).unwrap()
    }

    fn send_with(argv: Vec<String>) -> Outcome {
        let it = item();
        transport(argv, Duration::from_secs(10)).send(&built(&it), &it, "<m.1@example.org>")
    }

    #[test]
    fn accepted_with_provider_id() {
        let o = out_dir();
        let path = o.0.join("mail").to_string_lossy().into_owned();
        assert_eq!(
            send_with(vec![bin("send-record.sh"), path]),
            Outcome::Accepted {
                provider_id: Some("p-1".into())
            }
        );
    }

    #[test]
    fn exit0_without_line_is_unknown() {
        assert!(matches!(
            send_with(vec![bin("empty.sh")]),
            Outcome::Unknown { .. }
        ));
        assert!(matches!(
            send_with(vec![bin("garbage.sh")]),
            Outcome::Unknown { .. }
        ));
        assert!(matches!(
            send_with(vec![bin("send-not-sent.sh")]),
            Outcome::Unknown { .. }
        ));
    }

    #[test]
    fn exit2_3_4_rejected() {
        for code in ["2", "3", "4"] {
            assert_eq!(
                send_with(vec![bin("send-exit.sh"), code.into()]),
                Outcome::Rejected {
                    code: format!("command_exit_{code}")
                }
            );
        }
    }

    #[test]
    fn exit1_is_unknown_never_rejected() {
        for code in ["1", "5", "127"] {
            assert!(
                matches!(
                    send_with(vec![bin("send-exit.sh"), code.into()]),
                    Outcome::Unknown { .. }
                ),
                "{code}"
            );
        }
    }

    #[test]
    fn missing_program_is_rejected() {
        assert!(matches!(
            send_with(vec!["/nonexistent/send".into()]),
            Outcome::Rejected { .. }
        ));
    }

    #[test]
    fn timeout_is_unknown() {
        let it = item();
        let o = transport(vec![bin("sleep.sh")], Duration::from_millis(500)).send(
            &built(&it),
            &it,
            "<m.1@example.org>",
        );
        assert!(matches!(o, Outcome::Unknown { .. }), "{o:?}");
    }

    #[test]
    fn mime_on_stdin_has_no_bcc_header() {
        let o = out_dir();
        let path = o.0.join("mail");
        let mut it = item();
        it.bcc = vec!["hidden@example.org".into()];
        let t = transport(
            vec![bin("send-record.sh"), path.to_string_lossy().into()],
            Duration::from_secs(10),
        );
        let b = built(&it);
        assert!(matches!(
            t.send(&b, &it, "<m.1@example.org>"),
            Outcome::Accepted { .. }
        ));
        let mail = std::fs::read(&path).unwrap();
        assert_eq!(mail, b.bytes);
        assert!(
            !String::from_utf8_lossy(&mail)
                .to_lowercase()
                .contains("bcc")
        );
    }

    #[test]
    fn envelope_env_contains_bcc() {
        let o = out_dir();
        let path = o.0.join("mail");
        let mut it = item();
        it.bcc = vec!["hidden@example.org".into()];
        let t = transport(
            vec![bin("send-record.sh"), path.to_string_lossy().into()],
            Duration::from_secs(10),
        );
        t.send(&built(&it), &it, "<m.1@example.org>");
        let env = std::fs::read_to_string(o.0.join("mail.env")).unwrap();
        assert!(
            env.contains("DRAFTDESK_ENVELOPE_RCPTS=ann@example.org,hidden@example.org"),
            "{env}"
        );
        assert!(env.contains("DRAFTDESK_MESSAGE_ID=<m.1@example.org>"));
        assert!(env.contains("DRAFTDESK_ACCOUNT=custom"));
    }

    #[test]
    fn cleanup_runs_discard_command_with_mailbox_draft() {
        let o = out_dir();
        let path = o.0.join("discard");
        let mut t = transport(vec![bin("empty.sh")], Duration::from_secs(10));
        let mut it = item();
        assert!(matches!(
            t.cleanup_draft(&it, CleanupReason::Sent),
            CleanupResult::Skipped(_)
        ));
        t.discard_argv = Some(vec![bin("send-record.sh"), path.to_string_lossy().into()]);
        assert!(
            matches!(
                t.cleanup_draft(&it, CleanupReason::Sent),
                CleanupResult::Skipped(_)
            ),
            "no mailbox draft"
        );
        it.mailbox_draft = Some(MailboxDraft {
            message_id: Some("<d@example.org>".into()),
            ..Default::default()
        });
        assert!(matches!(
            t.cleanup_draft(&it, CleanupReason::Discarded),
            CleanupResult::Done(_)
        ));
        let stdin = std::fs::read_to_string(&path).unwrap();
        assert!(
            stdin.contains("<d@example.org>") && stdin.contains("\"subject\""),
            "{stdin}"
        );
        assert!(
            std::fs::read_to_string(o.0.join("discard.env"))
                .unwrap()
                .contains("DRAFTDESK_REASON=discarded")
        );
        t.discard_argv = Some(vec![bin("send-exit.sh"), "1".into()]);
        assert!(matches!(
            t.cleanup_draft(&it, CleanupReason::Sent),
            CleanupResult::Failed(_)
        ));
    }

    #[test]
    fn command_timeout_from_config() {
        let cfg = crate::config::Config::parse(
            r#"{"accounts":[
              {"name":"a","kind":"command","senders":[{"address":"me@example.org"}],
               "send_command":["/bin/cat"],"discard_command":["/bin/echo","x"]},
              {"name":"b","kind":"command","senders":[{"address":"me@example.org"}],
               "send_command":["/bin/cat"],"timeout_s":300}]}"#,
        )
        .unwrap();
        let a = CommandTransport::for_account(&cfg.accounts[0]).unwrap();
        assert_eq!(a.account, "a");
        assert_eq!(a.send_argv, vec!["/bin/cat".to_string()]);
        assert_eq!(a.discard_argv, Some(vec!["/bin/echo".into(), "x".into()]));
        assert_eq!(a.timeout, Duration::from_secs(180), "the default");
        let b = CommandTransport::for_account(&cfg.accounts[1]).unwrap();
        assert_eq!(b.timeout, Duration::from_secs(300));
    }

    #[test]
    fn discard_view_lists_changed_fields() {
        let o = out_dir();
        let path = o.0.join("discard");
        let mut t = transport(vec![bin("empty.sh")], Duration::from_secs(10));
        t.discard_argv = Some(vec![bin("send-record.sh"), path.to_string_lossy().into()]);
        let mut it = item();
        it.mailbox_draft = Some(MailboxDraft {
            message_id: Some("<d@example.org>".into()),
            ..Default::default()
        });
        let view = |t: &CommandTransport, it: &Item| -> serde_json::Value {
            assert!(matches!(
                t.cleanup_draft(it, CleanupReason::Discarded),
                CleanupResult::Done(_)
            ));
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
        };
        assert_eq!(view(&t, &it)["fields_changed"], serde_json::json!([]));
        it.edits.edited = true;
        it.edits.fields_changed = vec!["to".into(), "subject".into()];
        assert_eq!(
            view(&t, &it)["fields_changed"],
            serde_json::json!(["subject", "to"])
        );
    }
}
