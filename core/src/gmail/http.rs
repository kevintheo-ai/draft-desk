//! A minimal HTTP seam: the real client (ureq, system TLS roots) and a fake for tests.

use crate::{Error, Result};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReq {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResp {
    pub status: u16,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// `Err` means no response at all (network, TLS, time limit); any HTTP status is an `Ok`.
pub trait HttpClient {
    fn request(&self, req: HttpReq) -> Result<HttpResp>;
}

pub struct UreqClient(ureq::Agent);

impl UreqClient {
    /// TLS through the operating system with its trust store; each request gets its time limit (`time_limit`).
    pub fn new() -> UreqClient {
        use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
        let tls = TlsConfig::builder()
            .provider(TlsProvider::NativeTls)
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        let cfg = ureq::Agent::config_builder()
            .tls_config(tls)
            .http_status_as_error(false)
            .build();
        UreqClient(ureq::Agent::new_with_config(cfg))
    }
}

impl Default for UreqClient {
    fn default() -> Self {
        UreqClient::new()
    }
}

/// The time limit of a whole request with a body of `len` bytes: 120 s, plus one second per 32 KiB. 32 KiB/s
/// (256 kbit/s) is a poor mobile or hotel uplink; the largest mail (about 34 MB after base64) then gets 19 minutes.
/// Bodies under 32 KiB (token, init, clean-up) add nothing.
fn time_limit(len: usize) -> Duration {
    Duration::from_secs(120 + (len / (32 * 1024)) as u64)
}

impl HttpClient for UreqClient {
    fn request(&self, req: HttpReq) -> Result<HttpResp> {
        let limit = time_limit(req.body.len());
        let mut b = ureq::http::Request::builder()
            .method(req.method)
            .uri(&req.url);
        for (k, v) in &req.headers {
            b = b.header(k, v);
        }
        let request = b
            .body(req.body)
            .map_err(|e| Error(format!("http request: {e}")))?;
        let request = self
            .0
            .configure_request(request)
            .timeout_global(Some(limit))
            .build();
        let mut resp = self
            .0
            .run(request)
            .map_err(|e| Error(format!("http: {e}")))?;
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| Error(format!("http body: {e}")))?;
        Ok(HttpResp {
            status,
            headers,
            body,
        })
    }
}

/// Percent-encoding for form bodies and query strings (RFC 3986 unreserved characters stay).
pub fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Reverses `urlencode` (and `+` as space, as in forms).
pub fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .filter(|h| b[i] == b'%' && h.iter().all(u8::is_ascii_hexdigit))
            .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (hex, b[i]) {
            (Some(v), _) => {
                out.push(v);
                i += 3;
                continue;
            }
            (None, b'+') => out.push(b' '),
            (None, c) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn form(pairs: &[(&str, &str)]) -> Vec<u8> {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Answers requests from a script, records what it was asked.
    pub(crate) struct FakeHttp {
        pub answers: Mutex<Vec<Result<HttpResp>>>,
        pub seen: Mutex<Vec<HttpReq>>,
    }

    impl FakeHttp {
        pub(crate) fn new(answers: Vec<Result<HttpResp>>) -> FakeHttp {
            FakeHttp {
                answers: Mutex::new(answers),
                seen: Mutex::new(vec![]),
            }
        }
        pub(crate) fn seen(&self) -> Vec<HttpReq> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl HttpClient for FakeHttp {
        fn request(&self, req: HttpReq) -> Result<HttpResp> {
            self.seen.lock().unwrap().push(req);
            let mut a = self.answers.lock().unwrap();
            if a.is_empty() {
                Err(Error("fake: no more answers".into()))
            } else {
                a.remove(0)
            }
        }
    }

    pub(crate) fn resp(status: u16, body: &str) -> Result<HttpResp> {
        Ok(HttpResp {
            status,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        })
    }

    #[test]
    fn urldecode_reverses() {
        assert_eq!(urldecode("a%20b%26c%3Dd%2F%C3%A9~+x"), "a b&c=d/é~ x");
        assert_eq!(urldecode("100%"), "100%");
        assert_eq!(urldecode("%+1%zz"), "% 1%zz");
    }

    #[test]
    fn the_time_limit_grows_with_the_body() {
        assert_eq!(time_limit(0), Duration::from_secs(120));
        assert_eq!(
            time_limit(32 * 1024 - 1),
            Duration::from_secs(120),
            "small requests wait no longer"
        );
        // The largest mail: 25 MiB of attachments, about 34 MB after base64.
        assert_eq!(time_limit(34_000_000), Duration::from_secs(120 + 1037));
    }

    #[test]
    fn urlencode_and_form() {
        assert_eq!(urlencode("a b&c=d/é~"), "a%20b%26c%3Dd%2F%C3%A9~");
        assert_eq!(form(&[("k", "v 1"), ("x", "")]), b"k=v%201&x=".to_vec());
    }
}
