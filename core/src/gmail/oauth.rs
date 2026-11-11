//! Google sign-in for a desktop app: loopback redirect on 127.0.0.1, PKCE (S256) and a random state. The user
//! brings their own OAuth client ("Desktop" type); the only scope is `gmail.compose` (send, and read or delete
//! drafts), never read access to the mailbox.

use super::http::{HttpClient, HttpReq, form, urldecode};
use crate::secrets::Secrets;
use crate::{Error, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub const SCOPE: &str = "https://www.googleapis.com/auth/gmail.compose";
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// How long the browser has to come back.
pub const LOGIN_WAIT: Duration = Duration::from_secs(300);
/// How long the redirect's request line may take once the browser connected.
const CALLBACK_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

pub fn secret_key(account: &str) -> String {
    format!("{account}:gmail-secret")
}

pub fn refresh_key(account: &str) -> String {
    format!("{account}:gmail-refresh")
}

/// (verifier, challenge): 32 random bytes base64url as the verifier, base64url(sha256(verifier)) as challenge.
pub fn pkce() -> (String, String) {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("the system random source failed");
    let verifier = URL_SAFE_NO_PAD.encode(buf);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn auth_url(client_id: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    let q = [
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("access_type", "offline"),
        ("prompt", "consent"),
    ];
    format!("{AUTH_URL}?{}", String::from_utf8(form(&q)).expect("ascii"))
}

/// The authorization code from the redirect's request line, after checking the state.
pub fn parse_redirect(request_line: &str, state: &str) -> Result<String> {
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        bail!("sign-in: unexpected request")
    };
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let get = |k: &str| {
        query
            .split('&')
            .find_map(|p| p.strip_prefix(&format!("{k}=")))
            .map(urldecode)
    };
    if get("state").as_deref() != Some(state) {
        bail!("sign-in: state does not match (possible forgery)");
    }
    if let Some(e) = get("error") {
        bail!("sign-in refused: {e}");
    }
    get("code")
        .filter(|c| !c.is_empty())
        .ok_or_else(|| Error("sign-in: no code".into()))
}

/// Why no token came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// The token endpoint gave no answer (network, TLS, time limit): nothing says the sign-in is wrong.
    NoAnswer(Error),
    /// No sign-in stored, or Google refused it: the person signs in again.
    SignIn(Error),
}

impl From<TokenError> for Error {
    fn from(e: TokenError) -> Error {
        match e {
            TokenError::NoAnswer(e) | TokenError::SignIn(e) => e,
        }
    }
}

fn token_request(
    http: &dyn HttpClient,
    pairs: &[(&str, &str)],
) -> std::result::Result<serde_json::Value, TokenError> {
    let resp = http
        .request(HttpReq {
            method: "POST",
            url: TOKEN_URL.into(),
            headers: vec![(
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            )],
            body: form(pairs),
        })
        .map_err(TokenError::NoAnswer)?;
    let json = resp.json().unwrap_or_default();
    // Busy or broken on Google's side: nothing says the sign-in is wrong, so it is tried again like no answer.
    if resp.status == 429 || resp.status >= 500 {
        return Err(TokenError::NoAnswer(Error(format!(
            "token endpoint answered {}",
            resp.status
        ))));
    }
    if resp.status != 200 {
        let err = json["error"].as_str().unwrap_or("unknown_error");
        return Err(TokenError::SignIn(Error(if err == "invalid_grant" {
            "invalid_grant: the Google sign-in expired or was revoked; sign in again: remove the account and add it again (draftdesk account remove, then draftdesk account add-gmail, or in the Settings)"
                .into()
        } else {
            format!("token endpoint answered {}: {err}", resp.status)
        })));
    }
    Ok(json)
}

/// Signs in through the browser and returns the tokens. `open_url` opens the sign-in page.
pub fn authorize(
    http: &dyn HttpClient,
    client_id: &str,
    client_secret: &str,
    open_url: &dyn Fn(&str),
) -> Result<Tokens> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let (verifier, challenge) = pkce();
    let state = crate::random_hex(16);
    open_url(&auth_url(client_id, &redirect_uri, &challenge, &state));
    // ponytail: waits for one connection; a stray request to the port ends the attempt with an error.
    listener.set_nonblocking(false)?;
    let (stream, _) = accept_within(&listener, LOGIN_WAIT)?;
    let line = request_line(&stream, CALLBACK_WAIT)?;
    let code = parse_redirect(&line, &state);
    let page = if code.is_ok() {
        "Signed in. You can close this window."
    } else {
        "Sign-in failed."
    };
    let _ = write!(
        &stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n{page}"
    );
    let code = code?;
    let json = token_request(
        http,
        &[
            ("code", &code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", &redirect_uri),
            ("grant_type", "authorization_code"),
            ("code_verifier", &verifier),
        ],
    )?;
    match (
        json["access_token"].as_str(),
        json["refresh_token"].as_str(),
    ) {
        (Some(a), Some(r)) => Ok(Tokens {
            access_token: a.into(),
            refresh_token: r.into(),
        }),
        _ => bail!("token endpoint gave no refresh token"),
    }
}

/// The redirect's request line: at most 8 KiB, read within `wait` all together. Whatever connects first cannot
/// hold the sign-in or fill the memory with an endless or a trickling request.
fn request_line(stream: &TcpStream, wait: Duration) -> Result<String> {
    /// Every read gets only the time left until the deadline.
    struct Deadline<'a>(&'a TcpStream, Instant);
    impl Read for Deadline<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let left = self.1.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            self.0.set_read_timeout(Some(left))?;
            let mut s = self.0;
            s.read(buf)
        }
    }
    let mut line = String::new();
    BufReader::new(Deadline(stream, Instant::now() + wait))
        .take(8 * 1024)
        .read_line(&mut line)
        .map_err(|e| Error(format!("sign-in: no request from the browser in time: {e}")))?;
    if !line.ends_with('\n') {
        bail!("sign-in: the browser's request line is too long or cut off");
    }
    Ok(line)
}

fn accept_within(
    listener: &TcpListener,
    wait: Duration,
) -> Result<(std::net::TcpStream, std::net::SocketAddr)> {
    listener.set_nonblocking(true)?;
    let start = std::time::Instant::now();
    loop {
        match listener.accept() {
            Ok((s, a)) => {
                s.set_nonblocking(false)?;
                return Ok((s, a));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && start.elapsed() < wait => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                bail!("sign-in: no answer from the browser")
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// A fresh access token from the stored refresh token. `invalid_grant` asks for a new sign-in.
pub fn access_token(
    http: &dyn HttpClient,
    secrets: &dyn Secrets,
    account: &str,
    client_id: &str,
) -> std::result::Result<String, TokenError> {
    let sign_in = |m: String| TokenError::SignIn(Error(m));
    let (Some(secret), Some(refresh)) = (
        secrets
            .get(&secret_key(account))
            .map_err(TokenError::SignIn)?,
        secrets
            .get(&refresh_key(account))
            .map_err(TokenError::SignIn)?,
    ) else {
        return Err(sign_in(format!(
            "no Google sign-in stored for {account}; remove the account and add it again (draftdesk account remove, then draftdesk account add-gmail, or in the Settings)"
        )));
    };
    let json = token_request(
        http,
        &[
            ("client_id", client_id),
            ("client_secret", &secret),
            ("refresh_token", &refresh),
            ("grant_type", "refresh_token"),
        ],
    )?;
    json["access_token"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| sign_in("token endpoint gave no access token".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gmail::http::tests::{FakeHttp, resp};
    use crate::secrets::MemorySecrets;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let (v, c) = pkce();
        assert_eq!(v.len(), 43);
        assert!(
            v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(c, URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes())));
        assert_ne!(pkce().0, v);
    }

    #[test]
    fn requested_scope_is_compose_only() {
        let url = auth_url("cid", "http://127.0.0.1:1234", "ch", "st");
        assert!(
            url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fgmail.compose&"),
            "{url}"
        );
        assert!(!url.contains("readonly") && !url.contains("mail.google.com"));
        assert!(url.contains("code_challenge_method=S256") && url.contains("state=st"));
    }

    #[test]
    fn state_mismatch_rejected() {
        assert_eq!(
            parse_redirect("GET /?state=abc&code=4%2F0x HTTP/1.1", "abc").unwrap(),
            "4/0x"
        );
        assert!(parse_redirect("GET /?state=evil&code=x HTTP/1.1", "abc").is_err());
        assert!(parse_redirect("GET /?code=x HTTP/1.1", "abc").is_err());
        assert!(
            parse_redirect("GET /?state=abc&error=access_denied HTTP/1.1", "abc")
                .unwrap_err()
                .0
                .contains("access_denied")
        );
        assert!(parse_redirect("POST /?state=abc&code=x HTTP/1.1", "abc").is_err());
    }

    #[test]
    fn full_loopback_flow() {
        let http = FakeHttp::new(vec![resp(
            200,
            r#"{"access_token":"at","refresh_token":"rt","expires_in":3599}"#,
        )]);
        let open = |url: &str| {
            // Play the browser: follow the redirect with the state from the URL.
            let get = |k: &str| {
                url.split(['?', '&'])
                    .find_map(|p| p.strip_prefix(&format!("{k}=")))
                    .unwrap()
                    .to_string()
            };
            let redirect = urldecode(&get("redirect_uri"));
            let state = get("state");
            std::thread::spawn(move || {
                let addr = redirect
                    .trim_start_matches("http://")
                    .trim_end_matches('/')
                    .to_string();
                let mut s = std::net::TcpStream::connect(addr).unwrap();
                write!(
                    s,
                    "GET /?state={state}&code=the-code HTTP/1.1\r\nHost: x\r\n\r\n"
                )
                .unwrap();
                let mut answer = String::new();
                let _ = std::io::Read::read_to_string(&mut s, &mut answer);
            });
        };
        let t = authorize(&http, "cid", "secret", &open).unwrap();
        assert_eq!(
            t,
            Tokens {
                access_token: "at".into(),
                refresh_token: "rt".into()
            }
        );
        let req = &http.seen()[0];
        assert_eq!(req.url, TOKEN_URL);
        let body = String::from_utf8(req.body.clone()).unwrap();
        assert!(body.contains("grant_type=authorization_code") && body.contains("code=the-code"));
        assert!(body.contains("code_verifier=") && body.contains("client_secret=secret"));
    }

    #[test]
    fn the_callback_line_is_capped_in_size_and_time() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Whatever connects first: 64 KiB without a line break, then one byte every 50 ms.
        let flood = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            let _ = s.write_all(&[b'a'; 64 * 1024]);
        });
        let (stream, _) = listener.accept().unwrap();
        assert!(request_line(&stream, Duration::from_secs(5)).is_err());
        drop(stream);
        flood.join().unwrap();
        let trickle = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            for _ in 0..40 {
                if s.write_all(b"a").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let (stream, _) = listener.accept().unwrap();
        let t = std::time::Instant::now();
        assert!(request_line(&stream, Duration::from_millis(300)).is_err());
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        drop(stream);
        trickle.join().unwrap();
    }

    #[test]
    fn refresh_flow() {
        let s = MemorySecrets::default();
        s.set(&secret_key("g"), "sec").unwrap();
        s.set(&refresh_key("g"), "rt").unwrap();
        let http = FakeHttp::new(vec![resp(
            200,
            r#"{"access_token":"fresh","expires_in":3599}"#,
        )]);
        assert_eq!(access_token(&http, &s, "g", "cid").unwrap(), "fresh");
        let body = String::from_utf8(http.seen()[0].body.clone()).unwrap();
        assert!(body.contains("grant_type=refresh_token") && body.contains("refresh_token=rt"));
    }

    #[test]
    fn invalid_grant_asks_for_new_login() {
        let s = MemorySecrets::default();
        s.set(&secret_key("g"), "sec").unwrap();
        s.set(&refresh_key("g"), "rt").unwrap();
        let http = FakeHttp::new(vec![resp(
            400,
            r#"{"error":"invalid_grant","error_description":"expired"}"#,
        )]);
        let e = access_token(&http, &s, "g", "cid").unwrap_err();
        assert!(
            matches!(&e, TokenError::SignIn(Error(m)) if m.contains("invalid_grant") && m.contains("add-gmail")),
            "{e:?}"
        );
        let empty = MemorySecrets::default();
        assert!(matches!(
            access_token(&http, &empty, "g", "cid").unwrap_err(),
            TokenError::SignIn(Error(m)) if m.contains("add-gmail")
        ));
        // No answer at all says nothing about the sign-in.
        assert!(matches!(
            access_token(&FakeHttp::new(vec![]), &s, "g", "cid").unwrap_err(),
            TokenError::NoAnswer(_)
        ));
    }
}
