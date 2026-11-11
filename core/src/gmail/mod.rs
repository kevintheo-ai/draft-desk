//! Gmail: sign-in (always available, the command line sets up accounts) and the transport (only with `send`).
//!
//! Sending always uses a resumable upload of the raw message: an init request (may be repeated, it sends nothing)
//! and exactly one upload (never repeated). Drafts are only ever read or deleted, never sent through the drafts
//! endpoint.

pub mod http;
pub mod oauth;

#[cfg(feature = "send")]
pub use transport::Gmail;

#[cfg(feature = "send")]
mod transport {
    use super::http::{HttpClient, HttpReq};
    use super::oauth::{TokenError, access_token};
    use crate::item::{Item, PROVIDER_ID_RE};
    use crate::mime::Built;
    use crate::secrets::Secrets;
    use crate::transport::{CleanupReason, CleanupResult, Outcome, Transport};
    use std::time::Duration;

    const UPLOAD_URL: &str =
        "https://www.googleapis.com/upload/gmail/v1/users/me/messages/send?uploadType=resumable";
    const DRAFTS_URL: &str = "https://gmail.googleapis.com/gmail/v1/users/me/drafts";
    /// Pauses before the second and third init attempt.
    const INIT_PAUSES: [Duration; 2] = [Duration::from_secs(60), Duration::from_secs(120)];

    pub struct Gmail<'a> {
        pub http: &'a dyn HttpClient,
        pub secrets: &'a dyn Secrets,
        pub account: String,
        pub client_id: String,
        pub sleep: fn(Duration),
    }

    impl Gmail<'_> {
        fn bearer(&self) -> Result<(String, String), TokenError> {
            access_token(self.http, self.secrets, &self.account, &self.client_id)
                .map(|t| ("Authorization".to_string(), format!("Bearer {t}")))
        }

        /// The upload session URI and the bearer header that opened it. Repeated on 429, 5xx or no answer (also from
        /// the token endpoint), each time with a fresh token.
        fn init(&self, built: &Built, item: &Item) -> Result<(String, (String, String)), Outcome> {
            let thread = item.reply.as_ref().and_then(|r| r.gmail_thread_id.clone());
            let meta = match thread {
                Some(t) => serde_json::json!({ "threadId": t }),
                None => serde_json::json!({}),
            };
            let mut last = String::new();
            for attempt in 0..=INIT_PAUSES.len() {
                if attempt > 0 {
                    (self.sleep)(INIT_PAUSES[attempt - 1]);
                }
                let auth = match self.bearer() {
                    Ok(a) => a,
                    // No answer from the token endpoint says nothing about the sign-in: tried again like the init.
                    Err(TokenError::NoAnswer(_)) => {
                        last = "gmail_token_network".into();
                        continue;
                    }
                    Err(TokenError::SignIn(_)) => {
                        return Err(Outcome::Rejected {
                            code: "auth".into(),
                        });
                    }
                };
                let req = HttpReq {
                    method: "POST",
                    url: UPLOAD_URL.into(),
                    headers: vec![
                        auth.clone(),
                        (
                            "Content-Type".into(),
                            "application/json; charset=UTF-8".into(),
                        ),
                        ("X-Upload-Content-Type".into(), "message/rfc822".into()),
                        (
                            "X-Upload-Content-Length".into(),
                            built.bytes.len().to_string(),
                        ),
                    ],
                    body: meta.to_string().into_bytes(),
                };
                match self.http.request(req) {
                    Ok(r) if r.status == 200 => {
                        return r.header("location").map(|l| (l.to_string(), auth)).ok_or(
                            Outcome::Rejected {
                                code: "gmail_init_no_session".into(),
                            },
                        );
                    }
                    Ok(r) if r.status == 429 || r.status >= 500 => {
                        last = format!("gmail_init_{}", r.status)
                    }
                    Ok(r) => {
                        return Err(Outcome::Rejected {
                            code: format!("gmail_init_{}", r.status),
                        });
                    }
                    Err(_) => last = "gmail_init_network".into(),
                }
            }
            // The init request never carries the mail: giving up here sent nothing.
            Err(Outcome::Rejected { code: last })
        }
    }

    impl Transport for Gmail<'_> {
        fn send(&self, built: &Built, item: &Item, _message_id: &str) -> Outcome {
            let (session, auth) = match self.init(built, item) {
                Ok(s) => s,
                Err(outcome) => return outcome,
            };
            // Exactly one upload. Whatever goes wrong from here may have sent the mail.
            let req = HttpReq {
                method: "PUT",
                url: session,
                headers: vec![auth, ("Content-Type".into(), "message/rfc822".into())],
                body: built.bytes.clone(),
            };
            match self.http.request(req) {
                Ok(r) if r.status == 200 => {
                    match r.json().and_then(|j| j["id"].as_str().map(String::from)) {
                        Some(id) => Outcome::Accepted {
                            provider_id: Some(id),
                        },
                        None => Outcome::Unknown {
                            reason: "gmail answered without a message id".into(),
                        },
                    }
                }
                Ok(r) => Outcome::Unknown {
                    reason: format!("gmail upload answered {}", r.status),
                },
                Err(e) => Outcome::Unknown {
                    reason: format!("gmail upload: {e}"),
                },
            }
        }

        fn cleanup_draft(&self, item: &Item, _reason: CleanupReason) -> CleanupResult {
            let md = item.mailbox_draft.as_ref();
            let (Some(draft_id), Some(msg_id)) = (
                md.and_then(|m| m.gmail_draft_id.as_deref()),
                md.and_then(|m| m.gmail_message_id.as_deref()),
            ) else {
                return CleanupResult::Skipped("no Gmail draft ids".into());
            };
            if !PROVIDER_ID_RE.is_match(draft_id) || !PROVIDER_ID_RE.is_match(msg_id) {
                return CleanupResult::Skipped("Gmail draft ids are malformed".into());
            }
            let url = format!("{DRAFTS_URL}/{draft_id}");
            let failed = |e: TokenError| CleanupResult::Failed(crate::Error::from(e).0);
            let call = |method: &'static str, url: String| -> Result<u16, CleanupResult> {
                let auth = self.bearer().map_err(failed)?;
                self.http
                    .request(HttpReq {
                        method,
                        url,
                        headers: vec![auth],
                        body: vec![],
                    })
                    .map(|r| r.status)
                    .map_err(|e| CleanupResult::Failed(e.0))
            };
            let auth = match self.bearer() {
                Ok(a) => a,
                Err(e) => return failed(e),
            };
            let got = match self.http.request(HttpReq {
                method: "GET",
                url: format!("{url}?format=minimal"),
                headers: vec![auth],
                body: vec![],
            }) {
                Ok(r) => r,
                Err(e) => return CleanupResult::Failed(e.0),
            };
            match got.status {
                404 => return CleanupResult::Done("the Gmail draft was already gone".into()),
                200 => {}
                s => return CleanupResult::Failed(format!("drafts.get answered {s}: draft kept")),
            }
            // Delete only the draft that still holds the message we know; anything else stays.
            if got
                .json()
                .and_then(|j| j["message"]["id"].as_str().map(String::from))
                .as_deref()
                != Some(msg_id)
            {
                return CleanupResult::Skipped("the Gmail draft changed: kept".into());
            }
            match call("DELETE", url) {
                Ok(200 | 204) => CleanupResult::Done("Gmail draft deleted".into()),
                Ok(404) => CleanupResult::Done("the Gmail draft was already gone".into()),
                Ok(s) => CleanupResult::Failed(format!("drafts.delete answered {s}")),
                Err(r) => r,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::gmail::http::HttpResp;
        use crate::gmail::http::tests::{FakeHttp, resp};
        use crate::gmail::oauth::{refresh_key, secret_key};
        use crate::item::MailboxDraft;
        use crate::item::tests::item;
        use crate::mime::{BccMode, build};
        use crate::secrets::MemorySecrets;
        use crate::{Error, Result};

        fn token() -> Result<HttpResp> {
            resp(200, r#"{"access_token":"tok"}"#)
        }

        fn session() -> Result<HttpResp> {
            Ok(HttpResp {
                status: 200,
                headers: vec![("location".into(), "https://upload.example/s1".into())],
                body: vec![],
            })
        }

        fn secrets() -> MemorySecrets {
            let s = MemorySecrets::default();
            s.set(&secret_key("g"), "sec").unwrap();
            s.set(&refresh_key("g"), "rt").unwrap();
            s
        }

        fn gmail<'a>(http: &'a FakeHttp, secrets: &'a MemorySecrets) -> Gmail<'a> {
            Gmail {
                http,
                secrets,
                account: "g".into(),
                client_id: "cid".into(),
                sleep: |_| {},
            }
        }

        fn send(answers: Vec<Result<HttpResp>>, it: &Item) -> (Outcome, Vec<HttpReq>) {
            let http = FakeHttp::new(answers);
            let s = secrets();
            let dir = std::fs::File::open(std::env::temp_dir()).unwrap();
            let b = build(it, &dir, "<m.1@example.org>", BccMode::Keep).unwrap();
            let o = gmail(&http, &s).send(&b, it, "<m.1@example.org>");
            (o, http.seen())
        }

        fn methods(seen: &[HttpReq]) -> Vec<String> {
            seen.iter()
                .map(|r| format!("{} {}", r.method, r.url.split('?').next().unwrap()))
                .collect()
        }

        #[test]
        fn resumable_post_then_single_put() {
            let mut it = item();
            it.reply.as_mut().unwrap().gmail_thread_id = Some("18c0".into());
            let (o, seen) = send(
                vec![
                    token(),
                    session(),
                    resp(200, r#"{"id":"m-1","threadId":"18c0"}"#),
                ],
                &it,
            );
            assert_eq!(
                o,
                Outcome::Accepted {
                    provider_id: Some("m-1".into())
                }
            );
            assert_eq!(
                methods(&seen),
                vec![
                    "POST https://oauth2.googleapis.com/token",
                    "POST https://www.googleapis.com/upload/gmail/v1/users/me/messages/send",
                    "PUT https://upload.example/s1",
                ]
            );
            let init = &seen[1];
            assert!(init.url.ends_with("uploadType=resumable"));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&init.body).unwrap()["threadId"],
                "18c0"
            );
            let h = |r: &HttpReq, k: &str| {
                r.headers
                    .iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, v)| v.clone())
            };
            assert_eq!(
                h(init, "X-Upload-Content-Type").as_deref(),
                Some("message/rfc822")
            );
            assert_eq!(h(init, "Authorization").as_deref(), Some("Bearer tok"));
            assert_eq!(
                h(init, "X-Upload-Content-Length"),
                Some(seen[2].body.len().to_string())
            );
            // The upload carries the same bearer token as the init (as the proven Python sender does).
            assert_eq!(h(&seen[2], "Authorization").as_deref(), Some("Bearer tok"));
            assert_eq!(
                h(&seen[2], "Content-Type").as_deref(),
                Some("message/rfc822")
            );
        }

        #[test]
        fn init_retries_on_503_with_fresh_token() {
            let (o, seen) = send(
                vec![
                    token(),
                    resp(503, ""),
                    token(),
                    Err(Error("net".into())),
                    token(),
                    session(),
                    resp(200, r#"{"id":"m"}"#),
                ],
                &item(),
            );
            assert!(matches!(o, Outcome::Accepted { .. }), "{o:?}");
            assert_eq!(seen.iter().filter(|r| r.url.contains("oauth2")).count(), 3);
            assert_eq!(seen.iter().filter(|r| r.method == "PUT").count(), 1);
        }

        #[test]
        fn init_gives_up_after_three_attempts() {
            let (o, seen) = send(
                vec![
                    token(),
                    resp(429, ""),
                    token(),
                    resp(500, ""),
                    token(),
                    resp(503, ""),
                ],
                &item(),
            );
            assert!(matches!(o, Outcome::Rejected { .. }), "{o:?}");
            assert!(!seen.iter().any(|r| r.method == "PUT"));
        }

        #[test]
        fn init_4xx_no_retry() {
            let (o, seen) = send(
                vec![token(), resp(400, r#"{"error":{"message":"bad"}}"#)],
                &item(),
            );
            assert!(
                matches!(o, Outcome::Rejected { ref code } if code == "gmail_init_400"),
                "{o:?}"
            );
            assert_eq!(seen.len(), 2);
        }

        #[test]
        fn token_failure_is_rejected() {
            let (o, _) = send(vec![resp(400, r#"{"error":"invalid_grant"}"#)], &item());
            assert!(
                matches!(o, Outcome::Rejected { ref code } if code == "auth"),
                "{o:?}"
            );
        }

        #[test]
        fn token_endpoint_out_of_reach_is_retried_and_no_sign_in_problem() {
            let net = || Err(Error("net".into()));
            let (o, seen) = send(vec![net(), net(), net()], &item());
            assert!(
                matches!(o, Outcome::Rejected { ref code } if code == "gmail_token_network"),
                "{o:?}"
            );
            assert_eq!(seen.len(), 3, "tried three times, never an upload");
            let (o, _) = send(
                vec![net(), token(), session(), resp(200, r#"{"id":"m"}"#)],
                &item(),
            );
            assert!(matches!(o, Outcome::Accepted { .. }), "{o:?}");
        }

        #[test]
        fn token_endpoint_busy_is_retried_and_no_sign_in_problem() {
            // 429 and 5xx from the token endpoint say nothing about the sign-in: tried again like the init.
            let busy = |s| resp(s, r#"{"error":"server_error"}"#);
            let (o, seen) = send(vec![busy(503), busy(429), busy(500)], &item());
            assert!(
                matches!(o, Outcome::Rejected { ref code } if code == "gmail_token_network"),
                "{o:?}"
            );
            assert_eq!(seen.len(), 3, "tried three times, never an upload");
            let (o, _) = send(
                vec![busy(503), token(), session(), resp(200, r#"{"id":"m"}"#)],
                &item(),
            );
            assert!(matches!(o, Outcome::Accepted { .. }), "{o:?}");
        }

        #[test]
        fn put_failure_is_unknown_never_retried() {
            for put in [
                resp(500, ""),
                Err(Error("timeout".into())),
                resp(200, "not json"),
                resp(403, ""),
            ] {
                let (o, seen) = send(vec![token(), session(), put], &item());
                assert!(matches!(o, Outcome::Unknown { .. }), "{o:?}");
                assert_eq!(seen.iter().filter(|r| r.method == "PUT").count(), 1);
            }
        }

        #[test]
        fn missing_id_is_unknown() {
            let (o, _) = send(
                vec![token(), session(), resp(200, r#"{"threadId":"x"}"#)],
                &item(),
            );
            assert!(matches!(o, Outcome::Unknown { .. }));
        }

        #[test]
        fn missing_location_is_rejected() {
            let (o, seen) = send(vec![token(), resp(200, "")], &item());
            assert!(matches!(o, Outcome::Rejected { .. }), "{o:?}");
            assert!(!seen.iter().any(|r| r.method == "PUT"));
        }

        fn with_draft(draft_id: Option<&str>, msg_id: Option<&str>) -> Item {
            let mut it = item();
            it.mailbox_draft = Some(MailboxDraft {
                gmail_draft_id: draft_id.map(String::from),
                gmail_message_id: msg_id.map(String::from),
                message_id: None,
            });
            it
        }

        fn cleanup(answers: Vec<Result<HttpResp>>, it: &Item) -> (CleanupResult, Vec<HttpReq>) {
            let http = FakeHttp::new(answers);
            let s = secrets();
            let r = gmail(&http, &s).cleanup_draft(it, CleanupReason::Sent);
            (r, http.seen())
        }

        #[test]
        fn cleanup_deletes_only_on_matching_message_id() {
            let it = with_draft(Some("r-1"), Some("18d"));
            let (r, seen) = cleanup(
                vec![
                    token(),
                    resp(200, r#"{"id":"r-1","message":{"id":"18d"}}"#),
                    token(),
                    resp(204, ""),
                ],
                &it,
            );
            assert!(matches!(r, CleanupResult::Done(_)), "{r:?}");
            assert_eq!(seen[1].method, "GET");
            assert!(seen[1].url.ends_with("/drafts/r-1?format=minimal"));
            assert_eq!(seen[3].method, "DELETE");
            assert!(seen[3].url.ends_with("/drafts/r-1"));
        }

        #[test]
        fn cleanup_404_is_done() {
            let (r, seen) = cleanup(
                vec![token(), resp(404, "")],
                &with_draft(Some("r-1"), Some("18d")),
            );
            assert!(matches!(r, CleanupResult::Done(_)), "{r:?}");
            assert!(!seen.iter().any(|q| q.method == "DELETE"));
        }

        #[test]
        fn cleanup_mismatch_keeps() {
            let (r, seen) = cleanup(
                vec![
                    token(),
                    resp(200, r#"{"id":"r-1","message":{"id":"OTHER"}}"#),
                ],
                &with_draft(Some("r-1"), Some("18d")),
            );
            assert!(matches!(r, CleanupResult::Skipped(_)), "{r:?}");
            assert!(!seen.iter().any(|q| q.method == "DELETE"));
            let (r, _) = cleanup(
                vec![token(), resp(500, "")],
                &with_draft(Some("r-1"), Some("18d")),
            );
            assert!(matches!(r, CleanupResult::Failed(_)), "{r:?}");
        }

        #[test]
        fn cleanup_without_ids_keeps() {
            for it in [
                with_draft(None, Some("18d")),
                with_draft(Some("r-1"), None),
                item(),
            ] {
                let (r, seen) = cleanup(vec![], &it);
                assert!(matches!(r, CleanupResult::Skipped(_)));
                assert!(seen.is_empty());
            }
        }

        #[test]
        fn never_drafts_send_endpoint() {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/gmail");
            for e in std::fs::read_dir(dir).unwrap() {
                let text = std::fs::read_to_string(e.unwrap().path()).unwrap();
                let needle = ["drafts", "/send"].concat();
                assert!(!text.contains(&needle));
            }
        }

        #[test]
        fn provider_ids_are_checked_before_use() {
            let (r, seen) = cleanup(vec![], &with_draft(Some("../x"), Some("18d")));
            assert!(matches!(r, CleanupResult::Skipped(_)));
            assert!(seen.is_empty());
        }
    }
}
