//! Pure logic of the Needs-you inbox: the drafts the popover edits for each pending omp dialog
//! ([`PendingUi`]) and the [`UiAnswer`] each control sends. Mirrors the phone's editors
//! (`pwa/src/app/screens/conversation/pending.ts`) so both clients answer omp the same way; the
//! gateway validates every answer again ([`deck_service::ui_answer::value_for`]).

use deck_core::ctl::{PendingUi, PendingUiKind, UiApprove, UiFormField, UiQuestion};
use deck_service::ui_answer::{ApproveDecision, AskAnswer, FormAnswer, UiAnswer};

/// One question's draft of an `ask` dialog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AskDraft {
    /// Sorted option indices.
    pub selected: Vec<u32>,
    pub custom: String,
}

/// One field's draft of a `form` dialog. `other_picked`: a radio group's Other row is the pick
/// (no option checked); a checkbox group's Other counts whenever it has text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormDraft {
    Choice {
        checked: Vec<u32>,
        other: String,
        other_picked: bool,
    },
    Text(String),
}

/// What the popover edits before an answer is sent. `select`, `confirm` and `approve` answer
/// with one click and keep no draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Draft {
    None,
    Text(String),
    Ask(Vec<AskDraft>),
    Form(Vec<FormDraft>),
}

/// The starting draft of a dialog: prefilled text, recommended ask options, checked form
/// options.
pub fn initial(ui: &PendingUi) -> Draft {
    match ui.kind {
        PendingUiKind::Input => Draft::Text(String::new()),
        PendingUiKind::Editor => Draft::Text(ui.prefill.clone().unwrap_or_default()),
        PendingUiKind::Ask => Draft::Ask(ui.questions.iter().map(initial_ask).collect()),
        PendingUiKind::Form => Draft::Form(
            ui.form
                .as_ref()
                .map(|f| f.fields.iter().map(initial_field).collect())
                .unwrap_or_default(),
        ),
        PendingUiKind::Select | PendingUiKind::Confirm | PendingUiKind::Approve => Draft::None,
    }
}

fn initial_ask(q: &UiQuestion) -> AskDraft {
    let selected = q
        .recommended
        .filter(|&r| (r as usize) < q.options.len())
        .map(|r| vec![r])
        .unwrap_or_default();
    AskDraft {
        selected,
        custom: String::new(),
    }
}

fn initial_field(field: &UiFormField) -> FormDraft {
    match field {
        UiFormField::Text { value, .. } => FormDraft::Text(value.clone()),
        UiFormField::Choice { multi, options, .. } => {
            let mut checked: Vec<u32> = options
                .iter()
                .enumerate()
                .filter(|(_, o)| o.checked && !o.disabled)
                .map(|(i, _)| i as u32)
                .collect();
            // Of several checked radios the last wins, as in a browser.
            if !multi && checked.len() > 1 {
                checked.drain(..checked.len() - 1);
            }
            FormDraft::Choice {
                checked,
                other: String::new(),
                other_picked: false,
            }
        }
    }
}

/// The answer to a `select`: a single select answers its option; omp's checkbox select toggles
/// exactly one option per answer and asks again.
pub fn select_answer(ui: &PendingUi, index: u32) -> Option<UiAnswer> {
    if index as usize >= ui.options.len() {
        return None;
    }
    Some(if ui.multi {
        UiAnswer::Choices {
            indices: vec![index],
        }
    } else {
        UiAnswer::Choice { index }
    })
}

/// The text answer of an `input` (single line: line breaks become spaces) or an `editor`.
pub fn text_answer(kind: PendingUiKind, text: &str) -> UiAnswer {
    let text = if kind == PendingUiKind::Input {
        text.replace("\r\n", " ").replace('\n', " ")
    } else {
        text.to_owned()
    };
    UiAnswer::Text { text }
}

/// Tone of an approval button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Primary,
    Plain,
    Danger,
}

/// The decisions an approval offers, in display order: Allow once, the session grant when
/// offered, each grant and its "always" variant, Deny.
pub fn approve_choices(a: &UiApprove) -> Vec<(&str, ApproveDecision, Tone)> {
    let mut out = vec![("Allow once", ApproveDecision::Once, Tone::Primary)];
    if let Some(label) = &a.session {
        out.push((label.as_str(), ApproveDecision::Session, Tone::Plain));
    }
    for (i, g) in a.grants.iter().enumerate() {
        let index = i as u32;
        out.push((
            g.label.as_str(),
            ApproveDecision::Grant {
                index,
                always: false,
            },
            Tone::Plain,
        ));
        out.push((
            g.always_label.as_str(),
            ApproveDecision::Grant {
                index,
                always: true,
            },
            Tone::Plain,
        ));
    }
    out.push(("Deny", ApproveDecision::Deny, Tone::Danger));
    out
}

/// Toggles option `index` of a question: single choice replaces (or clears), multi choice adds or
/// removes.
pub fn toggle_ask(q: &UiQuestion, draft: &mut AskDraft, index: u32) {
    if index as usize >= q.options.len() {
        return;
    }
    if !q.multi {
        draft.selected = if draft.selected == [index] {
            Vec::new()
        } else {
            vec![index]
        };
        return;
    }
    match draft.selected.binary_search(&index) {
        Ok(at) => {
            draft.selected.remove(at);
        }
        Err(at) => draft.selected.insert(at, index),
    }
}

/// A question is answered by at least one option or a non-blank own text.
pub fn ask_answered(draft: &AskDraft) -> bool {
    !draft.selected.is_empty() || !draft.custom.trim().is_empty()
}

/// The `ask` answer for every question; `None` while one is still unanswered.
pub fn ask_answer(questions: &[UiQuestion], drafts: &[AskDraft]) -> Option<UiAnswer> {
    if drafts.len() != questions.len() {
        return None;
    }
    let mut answers = Vec::with_capacity(questions.len());
    for (q, d) in questions.iter().zip(drafts) {
        if !ask_answered(d) {
            return None;
        }
        let selected: Vec<u32> = d
            .selected
            .iter()
            .copied()
            .filter(|&i| (i as usize) < q.options.len())
            .collect();
        if !q.multi && selected.len() > 1 {
            return None;
        }
        let custom = d.custom.trim();
        answers.push(AskAnswer {
            id: q.id.clone(),
            selected,
            custom: (!custom.is_empty()).then(|| custom.to_owned()),
        });
    }
    Some(UiAnswer::Ask { answers })
}

/// Clicks option `index` of a choice field: a radio picks it (Other set aside), a checkbox
/// toggles it. Disabled options do nothing.
pub fn toggle_form_option(field: &UiFormField, draft: &mut FormDraft, index: u32) {
    let (
        UiFormField::Choice { multi, options, .. },
        FormDraft::Choice {
            checked,
            other_picked,
            ..
        },
    ) = (field, draft)
    else {
        return;
    };
    if options.get(index as usize).is_none_or(|o| o.disabled) {
        return;
    }
    if !multi {
        *checked = vec![index];
        *other_picked = false;
        return;
    }
    match checked.binary_search(&index) {
        Ok(at) => {
            checked.remove(at);
        }
        Err(at) => checked.insert(at, index),
    }
}

/// Picks a radio group's Other row: it becomes the pick, the checked option is cleared.
pub fn pick_form_other(field: &UiFormField, draft: &mut FormDraft) {
    if let (
        UiFormField::Choice {
            multi: false,
            other: true,
            ..
        },
        FormDraft::Choice {
            checked,
            other_picked,
            ..
        },
    ) = (field, draft)
    {
        checked.clear();
        *other_picked = true;
    }
}

/// Whether a choice draft's Other answer counts (omp's `#otherChosen`; blank text is none).
fn other_chosen(field: &UiFormField, draft: &FormDraft) -> bool {
    match (field, draft) {
        (
            UiFormField::Choice {
                other: true, multi, ..
            },
            FormDraft::Choice { checked, other, .. },
        ) => !other.trim().is_empty() && (*multi || checked.is_empty()),
        _ => false,
    }
}

/// A text field's text as omp sees it (`textValue` then `String()`: numbers normalized).
fn text_of(input_type: &str, text: &str) -> String {
    if matches!(input_type, "number" | "range")
        && let Ok(n) = text.trim().parse::<f64>()
        && n.is_finite()
    {
        return format_number(n);
    }
    text.to_owned()
}

/// JavaScript's `String(number)` for the values a form field takes.
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Why a field's draft is not acceptable, or `None` (omp's `fieldError`, same messages, so the
/// card flags what omp would refuse). Unnamed fields submit nothing and are never checked.
/// `pattern` is left to omp: it is a JavaScript regular expression.
pub fn form_field_error(field: &UiFormField, draft: Option<&FormDraft>) -> Option<String> {
    match field {
        UiFormField::Choice {
            name,
            required,
            multi,
            options,
            ..
        } => {
            if name.is_empty() || !required {
                return None;
            }
            let picked = draft.is_some_and(|d| {
                let checked = match d {
                    FormDraft::Choice { checked, .. } => checked
                        .iter()
                        .any(|&i| options.get(i as usize).is_some_and(|o| !o.disabled)),
                    FormDraft::Text(_) => false,
                };
                checked || other_chosen(field, d)
            });
            if picked {
                None
            } else if *multi && options.len() > 1 {
                Some("Select at least one".to_owned())
            } else {
                Some("Required".to_owned())
            }
        }
        UiFormField::Text {
            name,
            input_type,
            required,
            min,
            max,
            min_length,
            max_length,
            ..
        } => {
            if name.is_empty() {
                return None;
            }
            let raw = match draft {
                Some(FormDraft::Text(t)) => t.as_str(),
                _ => "",
            };
            let text = text_of(input_type, raw);
            if text.trim().is_empty() {
                return required.then(|| "Required".to_owned());
            }
            if matches!(input_type.as_str(), "number" | "range") {
                let Some(n) = text.trim().parse::<f64>().ok().filter(|n| n.is_finite()) else {
                    return Some("Enter a number".to_owned());
                };
                if let Some(min) = min.filter(|&m| n < m) {
                    return Some(format!("Must be at least {}", format_number(min)));
                }
                if let Some(max) = max.filter(|&m| n > m) {
                    return Some(format!("Must be at most {}", format_number(max)));
                }
            }
            let len = text.encode_utf16().count() as u32;
            if let Some(min) = min_length.filter(|&m| len < m) {
                return Some(format!("Needs at least {min} characters"));
            }
            if let Some(max) = max_length.filter(|&m| len > m) {
                return Some(format!("At most {max} characters"));
            }
            if input_type == "email" && !looks_like_email(&text) {
                return Some("Enter an email address".to_owned());
            }
            if input_type == "url" && !looks_like_url(&text) {
                return Some("Enter a full URL".to_owned());
            }
            None
        }
    }
}

/// `^[^\s@]+@[^\s@]+$`.
fn looks_like_email(text: &str) -> bool {
    let ok = |s: &str| !s.is_empty() && !s.contains(char::is_whitespace);
    text.split_once('@')
        .is_some_and(|(a, b)| ok(a) && ok(b) && !b.contains('@'))
}

/// A WHATWG-parsable absolute URL, approximately: `scheme:` followed by something.
fn looks_like_url(text: &str) -> bool {
    let Some((scheme, rest)) = text.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && !rest.trim().is_empty()
        && !text.contains(char::is_whitespace)
}

/// The `form` answer, one entry per field in order; `None` while a field fails
/// [`form_field_error`].
pub fn form_answer(fields: &[UiFormField], drafts: &[FormDraft]) -> Option<UiAnswer> {
    let mut out = Vec::with_capacity(fields.len());
    for (i, field) in fields.iter().enumerate() {
        let draft = drafts.get(i);
        if form_field_error(field, draft).is_some() {
            return None;
        }
        match field {
            UiFormField::Text { value, .. } => out.push(FormAnswer::Text {
                text: match draft {
                    Some(FormDraft::Text(t)) => t.clone(),
                    _ => value.clone(),
                },
            }),
            UiFormField::Choice { multi, options, .. } => {
                let (checked, other) = match draft {
                    Some(d @ FormDraft::Choice { checked, other, .. }) => (
                        checked
                            .iter()
                            .copied()
                            .filter(|&o| options.get(o as usize).is_some_and(|o| !o.disabled))
                            .collect::<Vec<_>>(),
                        other_chosen(field, d).then(|| other.clone()),
                    ),
                    _ => (Vec::new(), None),
                };
                if !multi && checked.len() > 1 {
                    return None;
                }
                out.push(FormAnswer::Choice { checked, other });
            }
        }
    }
    Some(UiAnswer::Form { fields: out })
}

/// The kind's short name on the card's badge.
pub fn kind_label(kind: PendingUiKind) -> &'static str {
    match kind {
        PendingUiKind::Select => "Choice",
        PendingUiKind::Confirm => "Confirm",
        PendingUiKind::Input => "Input",
        PendingUiKind::Editor => "Editor",
        PendingUiKind::Ask => "Question",
        PendingUiKind::Approve => "Approval",
        PendingUiKind::Form => "Form",
    }
}

/// The headline of a dialog: its title, else a form's first question, else the kind.
pub fn headline(ui: &PendingUi) -> &str {
    let title = ui.title.trim();
    if !title.is_empty() && !(ui.kind == PendingUiKind::Form && title == "Ask") {
        return title;
    }
    let question = ui.form.as_ref().and_then(|f| {
        f.fields.iter().find_map(|field| {
            let q = match field {
                UiFormField::Choice { question, .. } | UiFormField::Text { question, .. } => {
                    question.trim()
                }
            };
            (!q.is_empty()).then_some(q)
        })
    });
    question
        .or_else(|| ui.questions.first().map(|q| q.question.trim()))
        .filter(|q| !q.is_empty())
        .unwrap_or(if title.is_empty() {
            kind_label(ui.kind)
        } else {
            title
        })
}

/// The Needs-you button's badge: `1`–`9`, then `9+`.
pub fn badge_text(n: usize) -> Option<String> {
    match n {
        0 => None,
        1..=9 => Some(n.to_string()),
        _ => Some("9+".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::ctl::{UiForm, UiFormOption, UiGrant, UiOption};

    fn ui(kind: PendingUiKind) -> PendingUi {
        PendingUi {
            id: 1,
            kind,
            title: String::new(),
            message: None,
            options: Vec::new(),
            multi: false,
            initial: None,
            prefill: None,
            questions: Vec::new(),
            approve: None,
            form: None,
            timeout_ms: None,
        }
    }

    fn opt(label: &str) -> UiOption {
        UiOption {
            label: label.to_owned(),
            description: None,
        }
    }

    fn question(id: &str, multi: bool, recommended: Option<u32>) -> UiQuestion {
        UiQuestion {
            id: id.to_owned(),
            question: format!("{id}?"),
            header: None,
            options: vec![opt("a"), opt("b"), opt("c")],
            multi,
            recommended,
        }
    }

    fn choice(name: &str, multi: bool, other: bool, required: bool, n: usize) -> UiFormField {
        UiFormField::Choice {
            name: name.to_owned(),
            question: String::new(),
            label: Vec::new(),
            multi,
            select: false,
            other,
            required,
            options: (0..n)
                .map(|i| UiFormOption {
                    label: Vec::new(),
                    checked: i == 0,
                    disabled: i == n - 1,
                })
                .collect(),
        }
    }

    fn text_field(name: &str, input_type: &str, required: bool) -> UiFormField {
        UiFormField::Text {
            name: name.to_owned(),
            question: String::new(),
            label: Vec::new(),
            input_type: input_type.to_owned(),
            multiline: false,
            value: String::new(),
            placeholder: String::new(),
            required,
            min: Some(1.0),
            max: Some(10.0),
            pattern: None,
            min_length: None,
            max_length: None,
            rows: 1,
        }
    }

    #[test]
    fn select_answers_one_index_and_a_checkbox_select_toggles_one_at_a_time() {
        let mut s = ui(PendingUiKind::Select);
        s.options = vec![opt("x"), opt("y")];
        assert_eq!(select_answer(&s, 1), Some(UiAnswer::Choice { index: 1 }));
        assert_eq!(select_answer(&s, 2), None);
        s.multi = true;
        assert_eq!(
            select_answer(&s, 0),
            Some(UiAnswer::Choices { indices: vec![0] })
        );
    }

    #[test]
    fn input_text_is_one_line_and_editor_text_is_kept() {
        assert_eq!(
            text_answer(PendingUiKind::Input, "a\r\nb\nc"),
            UiAnswer::Text {
                text: "a b c".into()
            }
        );
        assert_eq!(
            text_answer(PendingUiKind::Editor, "a\nb"),
            UiAnswer::Text {
                text: "a\nb".into()
            }
        );
    }

    #[test]
    fn ask_starts_on_the_recommended_option_and_needs_every_question() {
        let qs = vec![question("q1", false, Some(1)), question("q2", true, None)];
        let mut d = match initial(&PendingUi {
            questions: qs.clone(),
            ..ui(PendingUiKind::Ask)
        }) {
            Draft::Ask(d) => d,
            other => panic!("{other:?}"),
        };
        assert_eq!(d[0].selected, vec![1]);
        assert_eq!(ask_answer(&qs, &d), None, "q2 is unanswered");
        toggle_ask(&qs[1], &mut d[1], 2);
        toggle_ask(&qs[1], &mut d[1], 0);
        toggle_ask(&qs[0], &mut d[0], 0);
        d[1].custom = "  mine ".into();
        assert_eq!(
            ask_answer(&qs, &d),
            Some(UiAnswer::Ask {
                answers: vec![
                    AskAnswer {
                        id: "q1".into(),
                        selected: vec![0],
                        custom: None
                    },
                    AskAnswer {
                        id: "q2".into(),
                        selected: vec![0, 2],
                        custom: Some("mine".into())
                    },
                ]
            })
        );
        // A single question's second click on its option clears it.
        toggle_ask(&qs[0], &mut d[0], 0);
        assert!(d[0].selected.is_empty());
        d[0].custom = "own".into();
        assert!(ask_answer(&qs, &d).is_some(), "own text alone answers");
    }

    #[test]
    fn approve_offers_session_and_both_grant_variants_between_once_and_deny() {
        let a = UiApprove {
            full: "rm -rf x".into(),
            compact: String::new(),
            session: Some("Allow for session".into()),
            grants: vec![UiGrant {
                scope: "folder".into(),
                label: "Allow in folder".into(),
                always_label: "Always allow in folder".into(),
            }],
        };
        let c = approve_choices(&a);
        let decisions: Vec<ApproveDecision> = c.iter().map(|(_, d, _)| *d).collect();
        assert_eq!(
            decisions,
            vec![
                ApproveDecision::Once,
                ApproveDecision::Session,
                ApproveDecision::Grant {
                    index: 0,
                    always: false
                },
                ApproveDecision::Grant {
                    index: 0,
                    always: true
                },
                ApproveDecision::Deny,
            ]
        );
        assert_eq!(c.last().map(|(_, _, t)| *t), Some(Tone::Danger));
    }

    #[test]
    fn form_enforces_required_fields_and_answers_every_field_in_order() {
        let fields = vec![
            choice("pick", false, true, true, 3),
            choice("many", true, false, true, 3),
            text_field("n", "number", true),
            text_field("", "text", true),
        ];
        let form = UiForm {
            sections: Vec::new(),
            fields: fields.clone(),
        };
        let mut d = match initial(&PendingUi {
            form: Some(form),
            ..ui(PendingUiKind::Form)
        }) {
            Draft::Form(d) => d,
            other => panic!("{other:?}"),
        };
        // Option 0 starts checked in both choices; the number is still empty.
        assert_eq!(
            form_field_error(&fields[2], d.get(2)),
            Some("Required".into())
        );
        assert_eq!(form_answer(&fields, &d), None);
        d[2] = FormDraft::Text("12".into());
        assert_eq!(
            form_field_error(&fields[2], d.get(2)),
            Some("Must be at most 10".into())
        );
        d[2] = FormDraft::Text("4.0".into());
        // Unchecking the only option of a required checkbox group fails it.
        toggle_form_option(&fields[1], &mut d[1], 0);
        assert_eq!(
            form_field_error(&fields[1], d.get(1)),
            Some("Select at least one".into())
        );
        toggle_form_option(&fields[1], &mut d[1], 1);
        // Disabled options never toggle.
        toggle_form_option(&fields[1], &mut d[1], 2);
        // The radio's Other takes over once picked and typed.
        pick_form_other(&fields[0], &mut d[0]);
        if let FormDraft::Choice { other, .. } = &mut d[0] {
            *other = "custom".into();
        }
        assert_eq!(
            form_answer(&fields, &d),
            Some(UiAnswer::Form {
                fields: vec![
                    FormAnswer::Choice {
                        checked: vec![],
                        other: Some("custom".into())
                    },
                    FormAnswer::Choice {
                        checked: vec![1],
                        other: None
                    },
                    FormAnswer::Text { text: "4.0".into() },
                    FormAnswer::Text {
                        text: String::new()
                    },
                ]
            })
        );
        // Picking a radio option sets Other aside again.
        toggle_form_option(&fields[0], &mut d[0], 1);
        assert!(matches!(
            &d[0],
            FormDraft::Choice { checked, other_picked: false, .. } if checked == &[1]
        ));
    }

    #[test]
    fn email_and_url_fields_check_their_shape() {
        let email = text_field("e", "email", false);
        let url = text_field("u", "url", false);
        let t = |s: &str| FormDraft::Text(s.into());
        assert!(form_field_error(&email, Some(&t("a@b"))).is_none());
        assert!(form_field_error(&email, Some(&t("a b@c"))).is_some());
        assert!(form_field_error(&email, Some(&t(""))).is_none(), "optional");
        assert!(form_field_error(&url, Some(&t("https://x.y/z"))).is_none());
        assert!(form_field_error(&url, Some(&t("x.y/z"))).is_some());
    }

    #[test]
    fn badge_caps_at_nine_plus() {
        assert_eq!(badge_text(0), None);
        assert_eq!(badge_text(3).as_deref(), Some("3"));
        assert_eq!(badge_text(12).as_deref(), Some("9+"));
    }
}
