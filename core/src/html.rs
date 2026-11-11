//! Plain text to the two body parts of a mail: `unwrap_body` (text/plain) and `body_to_html` (text/html).
//!
//! A port of a proven Python builder; `matches_python_golden` holds both byte for byte to its output.

use regex::Regex;
use std::sync::LazyLock;

/// A line starting with one of these opens the signature (kept line by line).
const SIG_MARKERS: [&str; 7] = [
    "Freundliche Gr",
    "Best Regards",
    "Mit freundlichen",
    "Kind Regards",
    "Viele Gr",
    "Beste Gr",
    "Regards,",
];
/// Closings inside a sentence ("Many thanks and best regards"), compared in lower case.
const SIG_FRAGMENTS: [&str; 3] = ["freundliche gr", "best regards", "mit freundlichen"];

static NUMBERED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\d+[.)]\s").unwrap());
static NUMBERED_PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\d+[.)]\s*").unwrap());
static DASH_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*-\s*").unwrap());

const PARA_STYLE: &str = "line-height:1.5";

/// Joins hard-wrapped paragraph lines into one line each. Blank lines, bullets (`- `, `* `, `+ `), numbered items,
/// quotes (`> `), indented lines and everything from the signature on stay as they are.
pub fn unwrap_body(body: &str) -> String {
    if strip(body).is_empty() {
        return body.to_string();
    }
    let lines: Vec<&str> = body.split('\n').collect();
    let sig_start = lines
        .iter()
        .position(|l| {
            let s = strip(l);
            let low = s.to_lowercase();
            SIG_MARKERS.iter().any(|m| s.starts_with(m))
                || SIG_FRAGMENTS.iter().any(|f| low.contains(f))
        })
        .unwrap_or(lines.len());
    let starts_block = |l: &str| {
        is_bullet(l)
            || NUMBERED_RE.is_match(l)
            || l.trim_start_matches(is_space).starts_with("> ")
            || l.starts_with("  ")
    };
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if i > sig_start || starts_block(line) {
            out.push(line.to_string());
        } else if strip(line).is_empty() {
            out.push(String::new());
        } else {
            let mut paragraph = line.trim_end_matches(is_space).to_string();
            while i < lines.len()
                && i < sig_start
                && !strip(lines[i]).is_empty()
                && !starts_block(lines[i])
            {
                paragraph.push(' ');
                paragraph.push_str(strip(lines[i]));
                i += 1;
            }
            out.push(paragraph);
        }
    }
    out.join("\n")
}

/// The full HTML document: a fixed head, Arial 14px, one `<div>` per paragraph with an empty `<div><br></div>`
/// between them, lists of two or more `- ` or numbered lines as `<ul>`/`<ol>`, the signature with `<br>`.
pub fn body_to_html(body: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <meta name=\"supported-color-schemes\" content=\"light dark\"></head><body>{}</body></html>",
        convert(body)
    )
}

fn convert(text: &str) -> String {
    if strip(text).is_empty() {
        return "<div></div>".into();
    }
    // The signature starts at the first marker on a line of its own (markers in list order), else at a line with a
    // closing inside a sentence, else the whole text is one when it starts with a marker.
    let mut split: Option<usize> = SIG_MARKERS
        .iter()
        .find_map(|m| text.find(&format!("\n{m}")));
    if split.is_none() {
        split = SIG_FRAGMENTS.iter().find_map(|f| {
            text.split('\n')
                .enumerate()
                .filter(|(i, line)| *i > 0 && line.to_lowercase().contains(f))
                .find_map(|(_, line)| text.find(&format!("\n{}", strip(line))))
        });
    }
    let (main, signature) = match split {
        Some(idx) => (&text[..idx], Some(&text[idx + 1..])),
        None if SIG_MARKERS.iter().any(|m| text.starts_with(m)) => ("", Some(text)),
        None => (text, None),
    };
    let mut parts: Vec<String> = Vec::new();
    for para in main.split("\n\n") {
        let para = strip(para);
        if para.is_empty() {
            continue;
        }
        let lines: Vec<&str> = para.split('\n').collect();
        if lines.len() >= 2
            && lines
                .iter()
                .all(|l| l.trim_start_matches(is_space).starts_with("- "))
        {
            parts.push(render_list(&lines, false));
        } else if lines.len() >= 2 && lines.iter().all(|l| NUMBERED_RE.is_match(l)) {
            parts.push(render_list(&lines, true));
        } else {
            // ponytail: escape only; tags and entities in the text stay visible as text (the Python builder
            // unescaped first, which needs the full HTML5 entity table and surprised more often than it helped).
            parts.push(format!(
                "<div style=\"{PARA_STYLE}\">{}</div>",
                escape(para).replace('\n', "<br>")
            ));
        }
    }
    if let Some(sig) = signature {
        let sig = sig.trim_end_matches(is_space);
        parts.push(format!(
            "<div style=\"{PARA_STYLE}\">{}</div>",
            escape(sig).replace('\n', "<br>")
        ));
    }
    let spacer = format!("<div style=\"{PARA_STYLE}\"><br></div>");
    format!(
        "<div style=\"font-family:Arial,Helvetica,sans-serif;font-size:14px\">{}</div>",
        parts.join(&spacer)
    )
}

fn render_list(lines: &[&str], ordered: bool) -> String {
    let (tag, prefix) = if ordered {
        ("ol", &*NUMBERED_PREFIX_RE)
    } else {
        ("ul", &*DASH_PREFIX_RE)
    };
    let items: String = lines
        .iter()
        .map(|l| format!("<li>{}</li>", escape(&prefix.replace(l, ""))))
        .collect();
    format!("<{tag} style=\"margin:0;line-height:1.5;padding-left:1.5em\">{items}</{tag}>")
}

/// `& < > " '` as entities.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Python's whitespace (`str.isspace`), which also counts U+001C-U+001F.
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1C}'..='\u{1F}').contains(&c)
}

fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

fn is_bullet(line: &str) -> bool {
    let l = line.trim_start_matches(is_space);
    l.starts_with("- ") || l.starts_with("* ") || l.starts_with("+ ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn matches_python_golden() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/html");
        let mut checked = 0;
        for n in 1..=99 {
            let base = dir.join(format!("{n:02}"));
            let Ok(text) = std::fs::read_to_string(base.with_extension("txt")) else {
                break;
            };
            let plain = std::fs::read_to_string(base.with_extension("plain")).unwrap();
            let html = std::fs::read_to_string(base.with_extension("html")).unwrap();
            let got = unwrap_body(&text);
            assert_eq!(got, plain, "fixture {n:02}: unwrap_body");
            assert_eq!(body_to_html(&got), html, "fixture {n:02}: body_to_html");
            checked += 1;
        }
        assert_eq!(checked, 12);
    }

    #[test]
    fn never_contains_anchors_or_script() {
        let html = body_to_html(&unwrap_body(
            "see https://example.org and <script>alert(1)</script>\n",
        ));
        assert!(!html.contains("<a ") && !html.contains("<script"), "{html}");
        assert!(html.contains("https://example.org"));
    }

    #[test]
    fn escapes_markup_as_text() {
        // Deviation from the Python builder: tags and entities are shown as text, never interpreted.
        let html = body_to_html("a<br>b &amp; &para=3 <p>x</p>");
        assert!(
            html.contains("a&lt;br&gt;b &amp;amp; &amp;para=3 &lt;p&gt;x&lt;/p&gt;"),
            "{html}"
        );
    }

    #[test]
    fn empty_body() {
        assert_eq!(unwrap_body(""), "");
        assert_eq!(unwrap_body(" \n "), " \n ");
        assert!(body_to_html("").contains("<body><div></div></body>"));
    }
}
