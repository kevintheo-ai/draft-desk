//! Address entries: `Name <a@example.com>` or `a@example.com`, kept raw (never RFC 2047) for display and editing.

use crate::text::{has_forbidden, has_linebreak};
use crate::{Error, Result};
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::LazyLock;

pub const MAX_RECIPIENTS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addr {
    pub name: String,
    pub address: String,
}

static ADDR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^<>]*<([^@>\s]+@[^@>\s]+)>$|^([^@\s<>]+@[^@\s<>]+)$").unwrap());

/// RFC 5322 dot-atom: what an unquoted local part may hold.
static DOT_ATOM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[A-Za-z0-9!#$%&'*+/=?^_`{|}~-]+)*$").unwrap()
});

/// The address as lettre needs it to build and send. lettre refuses local parts beyond the 64 characters of
/// RFC 5321, but ticket systems reply from longer ones and mail servers take them. Up to 64 lettre checks
/// everything as before; beyond that the local part must be a plain dot-atom and the domain must pass lettre.
// ponytail: beyond 64 only ASCII dot-atoms; quoted or UTF-8 local parts that long stay refused until one shows up.
pub fn to_lettre(addr: &str) -> Result<lettre::Address> {
    addr.parse::<lettre::Address>().or_else(|e| {
        addr.rsplit_once('@')
            .filter(|(user, domain)| {
                user.len() > 64
                    && DOT_ATOM_RE.is_match(user)
                    && lettre::Address::new("x", domain).is_ok()
            })
            .map(|(user, domain)| lettre::Address::new_dangerous(user, domain))
            .ok_or_else(|| Error(format!("address {addr:?}: {e}")))
    })
}

/// Parses one entry, which may hold several comma-separated addresses.
pub fn parse_list(entry: &str) -> Result<Vec<Addr>> {
    if has_linebreak(entry) {
        return Err(Error(format!("address contains line separator: {entry:?}")));
    }
    if has_forbidden(entry) {
        return Err(Error(format!(
            "address contains control/bidi characters: {entry:?}"
        )));
    }
    let parts: Vec<&str> = split_outside_quotes(entry)
        .into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(Error(format!("empty address: {entry:?}")));
    }
    parts
        .into_iter()
        .map(|raw| {
            let caps = ADDR_RE
                .captures(raw)
                .ok_or_else(|| Error(format!("invalid address: {raw:?}")))?;
            Ok(match (caps.get(1), caps.get(2)) {
                (Some(addr), _) => Addr {
                    name: unquote(&raw[..raw.rfind('<').unwrap_or(0)]),
                    address: addr.as_str().into(),
                },
                (_, Some(addr)) => Addr {
                    name: String::new(),
                    address: addr.as_str().into(),
                },
                _ => unreachable!("one of the two alternatives matched"),
            })
        })
        .collect()
}

/// Splits at commas outside `"..."`; a backslash escapes the next character only inside quotes.
fn split_outside_quotes(s: &str) -> Vec<&str> {
    let (mut parts, mut start, mut in_quotes, mut escaped) = (Vec::new(), 0, false, false);
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' && in_quotes {
            escaped = true;
        } else if c == '"' {
            in_quotes = !in_quotes;
        } else if c == ',' && !in_quotes {
            parts.push(&s[start..i]);
            start = i + 1;
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Display name as a reader sees it: quotes dropped, escapes inside quotes resolved, trimmed.
fn unquote(name: &str) -> String {
    let (mut out, mut in_quotes, mut escaped) = (String::new(), false, false);
    for c in name.chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' && in_quotes {
            escaped = true;
        } else if c == '"' {
            in_quotes = !in_quotes;
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// Display form, raw; names with RFC 5322 specials are quoted with `\` and `"` escaped.
pub fn format(a: &Addr) -> String {
    let name = a.name.trim();
    if name.is_empty() {
        a.address.clone()
    } else if name.contains(['(', ')', '<', '>', '@', ',', ';', ':', '\\', '"', '[', ']']) {
        format!(
            "\"{}\" <{}>",
            name.replace('\\', "\\\\").replace('"', "\\\""),
            a.address
        )
    } else {
        format!("{name} <{}>", a.address)
    }
}

/// Every entry of a recipient field, each split into its addresses and written in display form. An error names the
/// field and the entry (`to[2]: ...`).
pub fn normalize_list(label: &str, entries: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let addrs = parse_list(entry).map_err(|e| Error(format!("{label}[{i}]: {e}")))?;
        out.extend(addrs.iter().map(format));
    }
    Ok(out)
}

/// The first address of an entry, lowercased (for duplicate detection).
pub fn bare(entry: &str) -> String {
    // ponytail: to_lowercase instead of Python's casefold; they differ only for a few non-ASCII letters (ß).
    match parse_list(entry) {
        Ok(list) => list[0].address.to_lowercase(),
        Err(_) => entry.trim().to_lowercase(),
    }
}

/// `to` not empty, at most [`MAX_RECIPIENTS`] entries in total, no address twice across the three fields.
pub fn check_recipients(to: &[String], cc: &[String], bcc: &[String]) -> Vec<String> {
    let mut errors = Vec::new();
    if to.is_empty() {
        errors.push("to: empty".to_string());
    }
    let total = to.len() + cc.len() + bcc.len();
    if total > MAX_RECIPIENTS {
        errors.push(format!("recipients: {total} > {MAX_RECIPIENTS}"));
        return errors;
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for e in to.iter().chain(cc).chain(bcc) {
        *counts.entry(bare(e)).or_default() += 1;
    }
    let dups: Vec<String> = counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(b, _)| b)
        .collect();
    if !dups.is_empty() {
        errors.push(format!("recipients: duplicates: {}", dups.join(", ")));
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_list_reports_label_and_index() {
        let ok = normalize_list("to", &["Ann <ann@example.org>, bob@example.org".into()]).unwrap();
        assert_eq!(ok, vec!["Ann <ann@example.org>", "bob@example.org"]);
        let err = normalize_list("cc", &["ok@example.org".into(), "nope".into()]).unwrap_err();
        assert!(err.0.starts_with("cc[1]: "), "{}", err.0);
        assert!(normalize_list("bcc", &[]).unwrap().is_empty());
    }

    fn a(name: &str, address: &str) -> Addr {
        Addr {
            name: name.into(),
            address: address.into(),
        }
    }

    #[test]
    fn parse_valid_forms() {
        assert_eq!(
            parse_list("ann@example.org").unwrap(),
            vec![a("", "ann@example.org")]
        );
        assert_eq!(
            parse_list("Ann Lee <ann@example.org>").unwrap(),
            vec![a("Ann Lee", "ann@example.org")]
        );
        assert_eq!(
            parse_list("  <ann@example.org>  ").unwrap(),
            vec![a("", "ann@example.org")]
        );
        assert_eq!(
            parse_list(r#""Müller, Max" <max@example.org>"#).unwrap(),
            vec![a("Müller, Max", "max@example.org")]
        );
        assert_eq!(
            parse_list(r#""Say \"hi\"" <hi@example.org>"#).unwrap(),
            vec![a(r#"Say "hi""#, "hi@example.org")]
        );
        assert_eq!(
            parse_list("Jürgen Groß <j@example.org>").unwrap(),
            vec![a("Jürgen Groß", "j@example.org")]
        );
    }

    #[test]
    fn parse_two_entries() {
        assert_eq!(
            parse_list(r#"a@example.org, "B, C" <b@example.org>"#).unwrap(),
            vec![a("", "a@example.org"), a("B, C", "b@example.org")]
        );
        // Empty parts between commas are skipped.
        assert_eq!(
            parse_list("a@example.org,,").unwrap(),
            vec![a("", "a@example.org")]
        );
    }

    #[test]
    fn parse_rejects() {
        for bad in [
            "",
            "   ",
            ",",
            "a@b.de x",
            "a@b.de\r\nBcc: x@y.de",
            "a@b.de\u{2028}",
            "<>",
            "a@b@ c",
            "nur-text",
            "Name <a@b.de",
            "Name <a b@c.de>",
            "a@b.de <c@d.de>x",
        ] {
            assert!(parse_list(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parse_rejects_control_and_bidi() {
        for bad in [
            "Ann\u{202E} <a@b.de>",
            "a\u{200B}@b.de",
            "Ann\u{7f} <a@b.de>",
            "Ann\u{E0041} <a@b.de>",
        ] {
            assert!(parse_list(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn to_lettre_lifts_only_the_length_cap() {
        let long = "ticket-replies-long-routing-x+0f1e2d3c-4b5a-4978-8a9b-0c1d2e3f4a5b@example.org";
        assert_eq!(to_lettre(long).unwrap().to_string(), long);
        assert_eq!(
            to_lettre("ann@example.org").unwrap().to_string(),
            "ann@example.org"
        );
        let pad = "a".repeat(65);
        for bad in [
            format!("{pad}\"q@example.org"),
            format!("{pad}(c)@example.org"),
            format!("{pad}..b@example.org"),
            format!(".{pad}@example.org"),
            format!("{pad}ü@example.org"),
            format!("{pad}@exa mple.org"),
            format!("{pad}@"),
            "a(b)@example.org".to_string(),
            "nur-text".to_string(),
        ] {
            assert!(to_lettre(&bad).is_err(), "{bad:?}");
        }
    }

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn duplicates_across_fields() {
        let errs = check_recipients(
            &v(&["Ann <ANN@example.org>"]),
            &v(&["b@example.org"]),
            &v(&["ann@example.org"]),
        );
        assert_eq!(
            errs,
            vec!["recipients: duplicates: ann@example.org".to_string()]
        );
        assert_eq!(
            check_recipients(&[], &v(&["b@example.org"]), &[]),
            vec!["to: empty".to_string()]
        );
    }

    #[test]
    fn exactly_20_ok() {
        let to: Vec<String> = (0..20).map(|i| format!("u{i}@example.org")).collect();
        assert!(check_recipients(&to, &[], &[]).is_empty());
    }

    #[test]
    fn twenty_one_rejected() {
        let to: Vec<String> = (0..15).map(|i| format!("u{i}@example.org")).collect();
        let bcc: Vec<String> = (15..21).map(|i| format!("u{i}@example.org")).collect();
        assert_eq!(
            check_recipients(&to, &[], &bcc),
            vec!["recipients: 21 > 20".to_string()]
        );
    }

    #[test]
    fn format_quotes_specials() {
        assert_eq!(
            format(&a("Müller, Max", "m@example.org")),
            r#""Müller, Max" <m@example.org>"#
        );
        assert_eq!(
            format(&a(r#"Say "hi""#, "h@example.org")),
            r#""Say \"hi\"" <h@example.org>"#
        );
        assert_eq!(
            format(&a(r"back\slash", "b@example.org")),
            r#""back\\slash" <b@example.org>"#
        );
        assert_eq!(format(&a("  ", "b@example.org")), "b@example.org");
    }

    #[test]
    fn format_keeps_non_ascii_raw() {
        assert_eq!(
            format(&a("Jürgen Groß", "j@example.org")),
            "Jürgen Groß <j@example.org>"
        );
    }

    #[test]
    fn roundtrip() {
        for x in [
            a("Müller, Max", "m@example.org"),
            a(r#"Say "hi""#, "h@example.org"),
            a(r"back\slash (x)", "b@example.org"),
            a("Plain Name", "p@example.org"),
            a("", "n@example.org"),
        ] {
            assert_eq!(parse_list(&format(&x)).unwrap(), vec![x.clone()]);
        }
    }

    #[test]
    fn bare_takes_first_lowercased() {
        assert_eq!(
            bare("Ann <Ann@Example.ORG>, b@example.org"),
            "ann@example.org"
        );
        assert_eq!(bare("not an address"), "not an address");
    }
}
