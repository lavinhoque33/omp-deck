//! Answers to omp's pending dialogs ([`PendingUi`]) in client terms, and the value the live-control
//! extension hands the dialog function ([`deck_core::ctl::CtlCommand::AnswerUi`]). Every client
//! answers through the same translation, validated against the request it answers.

use deck_core::ctl::{PendingUi, PendingUiKind, UiFormField, UiOption};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// An answer to one pending dialog.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UiAnswer {
    /// `select` (single): the option index.
    Choice { index: u32 },
    /// `select` with checkboxes: the option to toggle. omp's checkbox select returns one toggled
    /// label per answer and asks again, so exactly one index is accepted.
    Choices { indices: Vec<u32> },
    /// `input` / `editor`.
    Text { text: String },
    /// `confirm`.
    Confirm { yes: bool },
    /// `ask`: one answer per question.
    Ask { answers: Vec<AskAnswer> },
    /// `approve`.
    Approve { decision: ApproveDecision },
    /// `form`: one answer per listed field, in order.
    Form { fields: Vec<FormAnswer> },
    /// Any kind: dismiss it (omp sees a cancel).
    Cancel,
}

/// The answer to one field of a `form` dialog, of the field's kind.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FormAnswer {
    /// A choice field: the checked option indices and the free-text "Other" (only when the
    /// field offers it).
    Choice {
        checked: Vec<u32>,
        other: Option<String>,
    },
    /// A text field.
    Text { text: String },
}

/// The answer to one question of an `ask` dialog: the selected option indices and an optional
/// own text.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AskAnswer {
    pub id: String,
    pub selected: Vec<u32>,
    pub custom: Option<String>,
}

/// A tool-approval decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApproveDecision {
    Once,
    /// For the rest of the session (only when the dialog offers it).
    Session,
    Deny,
    /// The dialog's grant `index`; `always` = its always-variant.
    Grant {
        index: u32,
        always: bool,
    },
}

/// Why an answer does not fit the dialog it answers.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AnswerError {
    #[error("that answer does not fit a {0} dialog")]
    WrongKind(&'static str),
    #[error("option {0} does not exist")]
    NoSuchOption(u32),
    #[error("choose exactly one option at a time")]
    OneAtATime,
    #[error("question {0:?} does not exist")]
    NoSuchQuestion(String),
    #[error("question {0:?} is answered twice")]
    Duplicate(String),
    #[error("question {0:?} takes one option")]
    SingleChoice(String),
    #[error("this approval cannot be granted for the session")]
    NoSessionGrant,
    #[error("grant {0} does not exist")]
    NoSuchGrant(u32),
    #[error("the form takes {0} answers")]
    FieldCount(usize),
    #[error("answer {0} does not fit its field")]
    FieldKind(usize),
    #[error("option {0} is checked twice")]
    DuplicateOption(u32),
    #[error("option {0} is disabled")]
    DisabledOption(u32),
    #[error("question {0:?} has no own answer")]
    NoOther(String),
}

fn kind_name(kind: PendingUiKind) -> &'static str {
    match kind {
        PendingUiKind::Select => "select",
        PendingUiKind::Confirm => "confirm",
        PendingUiKind::Input => "input",
        PendingUiKind::Editor => "editor",
        PendingUiKind::Ask => "ask",
        PendingUiKind::Approve => "approve",
        PendingUiKind::Form => "form",
    }
}

fn label(options: &[UiOption], index: u32) -> Result<&str, AnswerError> {
    usize::try_from(index)
        .ok()
        .and_then(|i| options.get(i))
        .map(|o| o.label.as_str())
        .ok_or(AnswerError::NoSuchOption(index))
}

/// The dialog function's own result for `answer` to `ui`: a label (`select`), a bool (`confirm`),
/// text (`input`/`editor`), omp's `{kind:"submit",results:[…]}` (`ask`), `"once"` / `"session"` /
/// `"deny"` / `{grant,always}` (`approve`), the extension's `{fields:[…]}` (`form`), or `null`
/// (cancel).
pub fn value_for(ui: &PendingUi, answer: &UiAnswer) -> Result<Value, AnswerError> {
    let wrong = || AnswerError::WrongKind(kind_name(ui.kind));
    match (ui.kind, answer) {
        (_, UiAnswer::Cancel) => Ok(Value::Null),
        (PendingUiKind::Select, UiAnswer::Choice { index }) if !ui.multi => {
            Ok(json!(label(&ui.options, *index)?))
        }
        (PendingUiKind::Select, UiAnswer::Choices { indices }) if ui.multi => match indices[..] {
            [index] => Ok(json!(label(&ui.options, index)?)),
            _ => Err(AnswerError::OneAtATime),
        },
        (PendingUiKind::Confirm, UiAnswer::Confirm { yes }) => Ok(json!(yes)),
        (PendingUiKind::Input | PendingUiKind::Editor, UiAnswer::Text { text }) => Ok(json!(text)),
        (PendingUiKind::Ask, UiAnswer::Ask { answers }) => ask_value(ui, answers),
        (PendingUiKind::Form, UiAnswer::Form { fields }) => {
            form_value(&ui.form.as_ref().ok_or_else(wrong)?.fields, fields)
        }
        (PendingUiKind::Approve, UiAnswer::Approve { decision }) => {
            let approve = ui.approve.as_ref().ok_or_else(wrong)?;
            match decision {
                ApproveDecision::Once => Ok(json!("once")),
                ApproveDecision::Deny => Ok(json!("deny")),
                ApproveDecision::Session if approve.session.is_some() => Ok(json!("session")),
                ApproveDecision::Session => Err(AnswerError::NoSessionGrant),
                ApproveDecision::Grant { index, always } => {
                    let grant = usize::try_from(*index)
                        .ok()
                        .and_then(|i| approve.grants.get(i))
                        .ok_or(AnswerError::NoSuchGrant(*index))?;
                    Ok(json!({"grant": grant.scope, "always": always}))
                }
            }
        }
        _ => Err(wrong()),
    }
}

/// omp's `ask` result: every question in the dialog's order, each with its options, the selected
/// labels and the own text when given.
fn ask_value(ui: &PendingUi, answers: &[AskAnswer]) -> Result<Value, AnswerError> {
    for (i, a) in answers.iter().enumerate() {
        if !ui.questions.iter().any(|q| q.id == a.id) {
            return Err(AnswerError::NoSuchQuestion(a.id.clone()));
        }
        if answers[..i].iter().any(|b| b.id == a.id) {
            return Err(AnswerError::Duplicate(a.id.clone()));
        }
    }
    let mut results = Vec::with_capacity(ui.questions.len());
    for q in &ui.questions {
        let answer = answers.iter().find(|a| a.id == q.id);
        let selected = answer.map_or(&[][..], |a| &a.selected[..]);
        if !q.multi && selected.len() > 1 {
            return Err(AnswerError::SingleChoice(q.id.clone()));
        }
        let selected: Vec<&str> = selected
            .iter()
            .map(|i| label(&q.options, *i))
            .collect::<Result<_, _>>()?;
        let mut r = json!({
            "id": q.id,
            "question": q.question,
            "options": q.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(),
            "multi": q.multi,
            "selectedOptions": selected,
        });
        if let Some(custom) = answer
            .and_then(|a| a.custom.as_deref())
            .filter(|c| !c.trim().is_empty())
        {
            r["customInput"] = json!(custom);
        }
        results.push(r);
    }
    Ok(json!({"kind": "submit", "results": results}))
}

/// The extension's form answer: `{"fields":[…]}`, one `{"checked":[…],"other":…}` or
/// `{"text":…}` per listed field. Only the shape is checked here; omp's own `fieldError`
/// (required, pattern, number bounds) runs in the extension.
fn form_value(fields: &[UiFormField], answers: &[FormAnswer]) -> Result<Value, AnswerError> {
    if answers.len() != fields.len() {
        return Err(AnswerError::FieldCount(fields.len()));
    }
    let mut out = Vec::with_capacity(fields.len());
    for (i, (field, answer)) in fields.iter().zip(answers).enumerate() {
        match (field, answer) {
            (
                UiFormField::Choice {
                    name,
                    multi,
                    other: offers_other,
                    options,
                    ..
                },
                FormAnswer::Choice { checked, other },
            ) => {
                for (j, index) in checked.iter().enumerate() {
                    let option = usize::try_from(*index)
                        .ok()
                        .and_then(|k| options.get(k))
                        .ok_or(AnswerError::NoSuchOption(*index))?;
                    if checked[..j].contains(index) {
                        return Err(AnswerError::DuplicateOption(*index));
                    }
                    if option.disabled {
                        return Err(AnswerError::DisabledOption(*index));
                    }
                }
                if !multi && checked.len() > 1 {
                    return Err(AnswerError::SingleChoice(name.clone()));
                }
                if other.is_some() && !offers_other {
                    return Err(AnswerError::NoOther(name.clone()));
                }
                out.push(json!({"checked": checked, "other": other}));
            }
            (UiFormField::Text { .. }, FormAnswer::Text { text }) => {
                out.push(json!({"text": text}))
            }
            _ => return Err(AnswerError::FieldKind(i)),
        }
    }
    Ok(json!({"fields": out}))
}

#[cfg(test)]
mod tests {
    use deck_core::ctl::{UiApprove, UiForm, UiFormOption, UiGrant, UiQuestion};

    use super::*;

    fn options(labels: &[&str]) -> Vec<UiOption> {
        labels
            .iter()
            .map(|l| UiOption {
                label: (*l).to_owned(),
                description: None,
            })
            .collect()
    }

    fn ui(kind: PendingUiKind) -> PendingUi {
        PendingUi {
            id: 7,
            kind,
            title: "t".to_owned(),
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

    fn select(multi: bool) -> PendingUi {
        PendingUi {
            options: options(&["a", "b"]),
            multi,
            ..ui(PendingUiKind::Select)
        }
    }

    fn question(id: &str, multi: bool) -> UiQuestion {
        UiQuestion {
            id: id.to_owned(),
            question: format!("{id}?"),
            header: None,
            options: options(&["x", "y"]),
            multi,
            recommended: None,
        }
    }

    fn ask() -> PendingUi {
        PendingUi {
            questions: vec![question("q1", false), question("q2", true)],
            ..ui(PendingUiKind::Ask)
        }
    }

    fn approve(session: bool) -> PendingUi {
        PendingUi {
            approve: Some(UiApprove {
                full: "rm -rf build".to_owned(),
                session: session.then(|| "this session".to_owned()),
                grants: vec![UiGrant {
                    scope: "rm:*".to_owned(),
                    label: "rm".to_owned(),
                    always_label: "always rm".to_owned(),
                }],
                ..UiApprove::default()
            }),
            ..ui(PendingUiKind::Approve)
        }
    }

    fn answer(id: &str, selected: &[u32], custom: Option<&str>) -> AskAnswer {
        AskAnswer {
            id: id.to_owned(),
            selected: selected.to_vec(),
            custom: custom.map(str::to_owned),
        }
    }

    fn choice(name: &str, multi: bool, other: bool) -> UiFormField {
        let option = |disabled| UiFormOption {
            label: Vec::new(),
            checked: false,
            disabled,
        };
        UiFormField::Choice {
            name: name.to_owned(),
            question: format!("{name}?"),
            label: Vec::new(),
            multi,
            select: false,
            other,
            required: false,
            options: vec![option(false), option(false), option(true)],
        }
    }

    fn text_field(name: &str) -> UiFormField {
        UiFormField::Text {
            name: name.to_owned(),
            question: format!("{name}?"),
            label: Vec::new(),
            input_type: "number".to_owned(),
            multiline: false,
            value: String::new(),
            placeholder: String::new(),
            required: true,
            min: Some(0.0),
            max: Some(5.0),
            pattern: None,
            min_length: None,
            max_length: None,
            rows: 1,
        }
    }

    /// A radio group with Other, a checkbox group without, a number input.
    fn form() -> PendingUi {
        PendingUi {
            form: Some(UiForm {
                sections: Vec::new(),
                fields: vec![
                    choice("color", false, true),
                    choice("extras", true, false),
                    text_field("retries"),
                ],
            }),
            ..ui(PendingUiKind::Form)
        }
    }

    fn picked(checked: &[u32], other: Option<&str>) -> FormAnswer {
        FormAnswer::Choice {
            checked: checked.to_vec(),
            other: other.map(str::to_owned),
        }
    }

    fn typed(text: &str) -> FormAnswer {
        FormAnswer::Text {
            text: text.to_owned(),
        }
    }

    #[test]
    fn cancel_fits_every_kind() {
        for kind in [
            PendingUiKind::Select,
            PendingUiKind::Confirm,
            PendingUiKind::Input,
            PendingUiKind::Editor,
            PendingUiKind::Ask,
            PendingUiKind::Approve,
            PendingUiKind::Form,
        ] {
            assert_eq!(value_for(&ui(kind), &UiAnswer::Cancel), Ok(Value::Null));
        }
    }

    #[test]
    fn a_select_answers_with_the_option_label() {
        assert_eq!(
            value_for(&select(false), &UiAnswer::Choice { index: 1 }),
            Ok(json!("b"))
        );
        assert_eq!(
            value_for(&select(false), &UiAnswer::Choice { index: 2 }),
            Err(AnswerError::NoSuchOption(2))
        );
        // A checkbox select takes one toggle per answer, not a single choice.
        assert_eq!(
            value_for(&select(true), &UiAnswer::Choice { index: 0 }),
            Err(AnswerError::WrongKind("select"))
        );
        assert_eq!(
            value_for(&select(true), &UiAnswer::Choices { indices: vec![0] }),
            Ok(json!("a"))
        );
        assert_eq!(
            value_for(
                &select(true),
                &UiAnswer::Choices {
                    indices: vec![0, 1]
                }
            ),
            Err(AnswerError::OneAtATime)
        );
        assert_eq!(
            value_for(&select(false), &UiAnswer::Choices { indices: vec![0] }),
            Err(AnswerError::WrongKind("select"))
        );
    }

    #[test]
    fn confirm_and_text_answer_with_their_value() {
        assert_eq!(
            value_for(
                &ui(PendingUiKind::Confirm),
                &UiAnswer::Confirm { yes: true }
            ),
            Ok(json!(true))
        );
        for kind in [PendingUiKind::Input, PendingUiKind::Editor] {
            assert_eq!(
                value_for(&ui(kind), &UiAnswer::Text { text: "hi".into() }),
                Ok(json!("hi"))
            );
        }
        assert_eq!(
            value_for(&ui(PendingUiKind::Input), &UiAnswer::Confirm { yes: false }),
            Err(AnswerError::WrongKind("input"))
        );
    }

    #[test]
    fn ask_submits_every_question_in_dialog_order() {
        let v = value_for(
            &ask(),
            &UiAnswer::Ask {
                answers: vec![
                    answer("q2", &[0, 1], Some("mine")),
                    answer("q1", &[1], Some("  ")),
                ],
            },
        );
        assert_eq!(
            v,
            Ok(json!({
                "kind": "submit",
                "results": [
                    {"id": "q1", "question": "q1?", "options": ["x", "y"], "multi": false,
                     "selectedOptions": ["y"]},
                    {"id": "q2", "question": "q2?", "options": ["x", "y"], "multi": true,
                     "selectedOptions": ["x", "y"], "customInput": "mine"},
                ],
            }))
        );
        // An unanswered question is submitted with nothing selected.
        let v = value_for(
            &ask(),
            &UiAnswer::Ask {
                answers: Vec::new(),
            },
        )
        .expect("value");
        assert_eq!(v["results"][0]["selectedOptions"], json!([]));
    }

    #[test]
    fn ask_refuses_answers_that_do_not_fit() {
        let ask_with = |answers| value_for(&ask(), &UiAnswer::Ask { answers });
        assert_eq!(
            ask_with(vec![answer("q9", &[0], None)]),
            Err(AnswerError::NoSuchQuestion("q9".into()))
        );
        assert_eq!(
            ask_with(vec![answer("q1", &[0], None), answer("q1", &[1], None)]),
            Err(AnswerError::Duplicate("q1".into()))
        );
        assert_eq!(
            ask_with(vec![answer("q1", &[0, 1], None)]),
            Err(AnswerError::SingleChoice("q1".into()))
        );
        assert_eq!(
            ask_with(vec![answer("q2", &[5], None)]),
            Err(AnswerError::NoSuchOption(5))
        );
    }

    #[test]
    fn approve_answers_with_the_decision() {
        let decide = |ui: &PendingUi, decision| value_for(ui, &UiAnswer::Approve { decision });
        assert_eq!(
            decide(&approve(false), ApproveDecision::Once),
            Ok(json!("once"))
        );
        assert_eq!(
            decide(&approve(false), ApproveDecision::Deny),
            Ok(json!("deny"))
        );
        assert_eq!(
            decide(&approve(true), ApproveDecision::Session),
            Ok(json!("session"))
        );
        assert_eq!(
            decide(&approve(false), ApproveDecision::Session),
            Err(AnswerError::NoSessionGrant)
        );
        assert_eq!(
            decide(
                &approve(false),
                ApproveDecision::Grant {
                    index: 0,
                    always: true
                }
            ),
            Ok(json!({"grant": "rm:*", "always": true}))
        );
        assert_eq!(
            decide(
                &approve(false),
                ApproveDecision::Grant {
                    index: 3,
                    always: false
                }
            ),
            Err(AnswerError::NoSuchGrant(3))
        );
        // Without its approval details the dialog cannot be answered as one.
        assert_eq!(
            decide(&ui(PendingUiKind::Approve), ApproveDecision::Once),
            Err(AnswerError::WrongKind("approve"))
        );
    }

    #[test]
    fn a_form_answers_with_one_entry_per_field_in_order() {
        let v = value_for(
            &form(),
            &UiAnswer::Form {
                fields: vec![picked(&[1], None), picked(&[0, 1], None), typed("3")],
            },
        );
        assert_eq!(
            v,
            Ok(json!({"fields": [
                {"checked": [1], "other": null},
                {"checked": [0, 1], "other": null},
                {"text": "3"},
            ]}))
        );
        // Nothing checked plus an own answer; omp's `fieldError` judges `required` and bounds.
        let v = value_for(
            &form(),
            &UiAnswer::Form {
                fields: vec![picked(&[], Some("teal")), picked(&[], None), typed("99")],
            },
        );
        assert_eq!(
            v,
            Ok(json!({"fields": [
                {"checked": [], "other": "teal"},
                {"checked": [], "other": null},
                {"text": "99"},
            ]}))
        );
    }

    #[test]
    fn a_form_refuses_answers_that_do_not_fit() {
        let answer = |fields| value_for(&form(), &UiAnswer::Form { fields });
        let ok = || vec![picked(&[0], None), picked(&[], None), typed("1")];
        let with = |i: usize, a: FormAnswer| {
            let mut fields = ok();
            fields[i] = a;
            fields
        };
        assert!(answer(ok()).is_ok());
        assert_eq!(answer(ok()[..2].to_vec()), Err(AnswerError::FieldCount(3)));
        let mut long = ok();
        long.push(typed("x"));
        assert_eq!(answer(long), Err(AnswerError::FieldCount(3)));
        assert_eq!(
            answer(with(0, typed("red"))),
            Err(AnswerError::FieldKind(0))
        );
        assert_eq!(
            answer(with(2, picked(&[0], None))),
            Err(AnswerError::FieldKind(2))
        );
        assert_eq!(
            answer(with(1, picked(&[3], None))),
            Err(AnswerError::NoSuchOption(3))
        );
        assert_eq!(
            answer(with(1, picked(&[0, 0], None))),
            Err(AnswerError::DuplicateOption(0))
        );
        assert_eq!(
            answer(with(1, picked(&[2], None))),
            Err(AnswerError::DisabledOption(2))
        );
        assert_eq!(
            answer(with(0, picked(&[0, 1], None))),
            Err(AnswerError::SingleChoice("color".into()))
        );
        // Several are fine in a checkbox group.
        assert!(answer(with(1, picked(&[0, 1], None))).is_ok());
        assert_eq!(
            answer(with(1, picked(&[], Some("own")))),
            Err(AnswerError::NoOther("extras".into()))
        );
        // A form answer fits only a form, and a form only a form answer (or cancel).
        assert_eq!(
            value_for(&ask(), &UiAnswer::Form { fields: ok() }),
            Err(AnswerError::WrongKind("ask"))
        );
        assert_eq!(
            value_for(&form(), &UiAnswer::Text { text: "x".into() }),
            Err(AnswerError::WrongKind("form"))
        );
        // Without its parsed form the dialog cannot be answered as one.
        assert_eq!(
            value_for(
                &ui(PendingUiKind::Form),
                &UiAnswer::Form { fields: Vec::new() }
            ),
            Err(AnswerError::WrongKind("form"))
        );
    }
}
