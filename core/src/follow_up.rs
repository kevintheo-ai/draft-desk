//! The optional step after sending (`follow_up`): the agent hands over a label, a line of text and a few choices; the
//! window shows them as a checkbox, a text field and pop-up menus and stores the person's answer with the item. Draft
//! Desk never acts on the answer: the agent reads it with `draftdesk status` once the mail went out.

use crate::text::{LINEBREAKS, has_forbidden, has_linebreak, strip_forbidden};
use crate::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_LABEL_CHARS: usize = 120;
pub const MAX_TEXT_CHARS: usize = 2000;
pub const MAX_CHOICES: usize = 4;
pub const MAX_OPTIONS: usize = 40;
pub const MAX_OPTION_CHARS: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowUp {
    /// The checkbox's title.
    pub label: String,
    /// Only the person checks the box, in the window: an agent cannot hand it over checked.
    #[serde(default)]
    pub checked: bool,
    /// One line; the agent may propose it, the person edits it.
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub choices: Vec<Choice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub name: String,
    pub options: Vec<String>,
    /// One of `options`; an agent that leaves it empty gets the first.
    #[serde(default)]
    pub selected: String,
}

/// The answer as the window holds it: the checkbox, the text, one option per choice in their order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    pub checked: bool,
    pub text: String,
    pub selected: Vec<String>,
}

impl Answer {
    pub fn of(f: &FollowUp) -> Answer {
        Answer {
            checked: f.checked,
            text: f.text.clone(),
            selected: f.choices.iter().map(|c| c.selected.clone()).collect(),
        }
    }
}

/// One line of text: without control, bidi and invisible characters, line breaks turned into spaces.
pub fn clean_line(s: &str) -> String {
    strip_forbidden(s)
        .chars()
        .map(|c| if LINEBREAKS.contains(&c) { ' ' } else { c })
        .collect()
}

/// The follow-up as the agent handed it over, ready to store: refused when checked, the text cleaned, the first
/// option selected where the agent named none.
pub fn from_agent(mut f: FollowUp) -> Result<FollowUp> {
    if f.checked {
        bail!("follow_up.checked: only the person checks the box, in the window");
    }
    f.text = clean_line(&f.text);
    for c in &mut f.choices {
        if c.selected.is_empty()
            && let Some(first) = c.options.first()
        {
            c.selected = first.clone();
        }
    }
    Ok(f)
}

/// Puts the answer into the stored follow-up (the text cleaned); an error when it has another number of choices. An
/// option that is not offered is left to `validate`, which the store runs before writing.
pub fn apply(f: &mut FollowUp, a: &Answer) -> Result<()> {
    if a.selected.len() != f.choices.len() {
        bail!(
            "follow_up: {} answers for {} choices",
            a.selected.len(),
            f.choices.len()
        );
    }
    f.checked = a.checked;
    f.text = clean_line(&a.text);
    for (c, s) in f.choices.iter_mut().zip(&a.selected) {
        c.selected.clone_from(s);
    }
    Ok(())
}

fn line(errors: &mut Vec<String>, what: &str, v: &str, max: usize, required: bool) {
    let n = v.chars().count();
    if required && v.trim().is_empty() {
        errors.push(format!("{what}: empty"));
    } else if n > max {
        errors.push(format!("{what}: {n} > {max} chars"));
    } else if has_linebreak(v) || has_forbidden(v) {
        errors.push(format!("{what}: has line breaks or control characters"));
    }
}

/// Every rule violation of a stored follow-up, for `validate_item`.
pub fn validate(f: &FollowUp) -> Vec<String> {
    let mut errors = Vec::new();
    line(
        &mut errors,
        "follow_up.label",
        &f.label,
        MAX_LABEL_CHARS,
        true,
    );
    line(
        &mut errors,
        "follow_up.text",
        &f.text,
        MAX_TEXT_CHARS,
        false,
    );
    if f.choices.len() > MAX_CHOICES {
        errors.push(format!(
            "follow_up.choices: {} > {MAX_CHOICES}",
            f.choices.len()
        ));
        return errors;
    }
    for (i, c) in f.choices.iter().enumerate() {
        let at = format!("follow_up.choices[{i}]");
        line(
            &mut errors,
            &format!("{at}.name"),
            &c.name,
            MAX_OPTION_CHARS,
            true,
        );
        if c.options.is_empty() || c.options.len() > MAX_OPTIONS {
            errors.push(format!("{at}.options: 1 to {MAX_OPTIONS} needed"));
            continue;
        }
        let mut seen = BTreeSet::new();
        for (k, o) in c.options.iter().enumerate() {
            line(
                &mut errors,
                &format!("{at}.options[{k}]"),
                o,
                MAX_OPTION_CHARS,
                true,
            );
            if !seen.insert(o.as_str()) {
                errors.push(format!("{at}.options[{k}]: duplicate"));
            }
        }
        if !c.options.contains(&c.selected) {
            errors.push(format!("{at}.selected: not one of the options"));
        }
    }
    errors
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> FollowUp {
        serde_json::from_value(serde_json::json!({
            "label": "Wait for a reply, then",
            "text": "compare\nthe offer\u{202E}",
            "choices": [
                {"name": "Who", "options": ["Agent A", "Agent B"]},
                {"name": "Deadline", "options": ["3 working days", "1 week"], "selected": "1 week"}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn from_agent_cleans_text_and_selects_first() {
        let f = from_agent(sample()).unwrap();
        assert_eq!(f.text, "compare the offer");
        assert!(!f.checked);
        assert_eq!(f.choices[0].selected, "Agent A");
        assert_eq!(f.choices[1].selected, "1 week");
        assert!(validate(&f).is_empty(), "{:?}", validate(&f));
    }

    #[test]
    fn agent_cannot_check_the_box() {
        let mut f = sample();
        f.checked = true;
        assert!(from_agent(f).unwrap_err().0.contains("follow_up.checked"));
    }

    #[test]
    fn unknown_keys_refused() {
        let mut v = serde_json::to_value(sample()).unwrap();
        v["surprise"] = serde_json::json!(1);
        assert!(serde_json::from_value::<FollowUp>(v).is_err());
        let mut v = serde_json::to_value(sample()).unwrap();
        v["choices"][0]["surprise"] = serde_json::json!(1);
        assert!(serde_json::from_value::<FollowUp>(v).is_err());
    }

    fn errs(edit: impl FnOnce(&mut FollowUp)) -> Vec<String> {
        let mut f = from_agent(sample()).unwrap();
        edit(&mut f);
        validate(&f)
    }

    fn has(errors: &[String], needle: &str) -> bool {
        errors.iter().any(|e| e.contains(needle))
    }

    #[test]
    fn validation_rules() {
        assert!(has(
            &errs(|f| f.label = " ".into()),
            "follow_up.label: empty"
        ));
        assert!(has(
            &errs(|f| f.label = "x".repeat(121)),
            "follow_up.label: 121"
        ));
        assert!(has(
            &errs(|f| f.label = "a\nb".into()),
            "follow_up.label: has line"
        ));
        assert!(has(
            &errs(|f| f.text = "a\u{200B}".into()),
            "follow_up.text"
        ));
        assert!(has(
            &errs(|f| f.text = "x".repeat(2001)),
            "follow_up.text: 2001"
        ));
        assert!(errs(|f| f.text = String::new()).is_empty());
        assert!(has(
            &errs(|f| f.choices = vec![f.choices[0].clone(); 5]),
            "follow_up.choices: 5 > 4"
        ));
        assert!(has(
            &errs(|f| f.choices[0].options.clear()),
            "options: 1 to 40"
        ));
        assert!(has(
            &errs(|f| f.choices[0].options.push("Agent A".into())),
            "options[2]: duplicate"
        ));
        assert!(has(
            &errs(|f| f.choices[1].options[0] = "a\u{202E}".into()),
            "choices[1].options[0]"
        ));
        assert!(has(
            &errs(|f| f.choices[0].selected = "Agent C".into()),
            "choices[0].selected: not one"
        ));
        assert!(has(
            &errs(|f| f.choices[0].name = String::new()),
            "choices[0].name: empty"
        ));
    }

    #[test]
    fn apply_sets_the_answer() {
        let mut f = from_agent(sample()).unwrap();
        let a = Answer {
            checked: true,
            text: "read it\u{2028}and answer".into(),
            selected: vec!["Agent B".into(), "3 working days".into()],
        };
        apply(&mut f, &a).unwrap();
        assert!(f.checked);
        assert_eq!(f.text, "read it and answer");
        assert_eq!(
            Answer::of(&f).selected,
            vec!["Agent B".to_string(), "3 working days".to_string()]
        );
        let short = Answer {
            selected: vec!["Agent B".into()],
            ..a
        };
        assert!(
            apply(&mut f, &short)
                .unwrap_err()
                .0
                .contains("1 answers for 2")
        );
    }
}
