//! Agent-independent input plans, bounded by visible controls and permission scope.
use crate::question::{Answer, Question};
use anyhow::{bail, Context, Result};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputPlan {
    Prompt(String),
    Text(String),
    Keys(Vec<String>),
}

pub(crate) fn valid_text(text: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > 16_000 || text.chars().any(char::is_control) {
        bail!("Input must be nonempty text without terminal control characters or line breaks")
    }
    Ok(())
}
pub(crate) fn valid_prompt(text: &str) -> Result<()> {
    if text.trim().is_empty()
        || text.len() > 16_000
        || text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        bail!("Prompt must be nonempty text without terminal escape or control characters")
    }
    Ok(())
}

fn has_enter(instructions: &str) -> bool {
    let s = instructions.to_lowercase();
    let words: Vec<_> = s.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    words.contains(&"enter")
        && ![
            "ctrl+enter",
            "ctrl-enter",
            "ctrl enter",
            "control+enter",
            "control-enter",
            "alt+enter",
            "alt-enter",
            "shift+enter",
            "shift-enter",
            "shift enter",
        ]
        .iter()
        .any(|keys| s.contains(keys))
        && [
            "press enter",
            "enter to confirm",
            "enter to submit",
            "enter to select",
            "enter confirms",
            "enter submits",
        ]
        .iter()
        .any(|phrase| s.contains(phrase))
}
fn arrows(instructions: &str) -> bool {
    let s = instructions.to_lowercase();
    let words: Vec<_> = s.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    (s.contains('↑') && s.contains('↓')) || (words.contains(&"up") && words.contains(&"down"))
}
fn focused(screen: &str, label: &str) -> bool {
    screen.lines().any(|line| {
        let s = line.trim_start();
        let Some(s) = s
            .strip_prefix('>')
            .or_else(|| s.strip_prefix('›'))
            .or_else(|| s.strip_prefix('❯'))
        else {
            return false;
        };
        let s = s.trim_start();
        let digits = s.trim_start_matches(|c: char| c.is_ascii_digit());
        let s = digits
            .strip_prefix('.')
            .or_else(|| digits.strip_prefix(')'))
            .unwrap_or(s)
            .trim_start();
        let s = s
            .strip_prefix("[ ]")
            .or_else(|| s.strip_prefix("[x]"))
            .or_else(|| s.strip_prefix("[X]"))
            .or_else(|| s.strip_prefix('☑'))
            .or_else(|| s.strip_prefix('☐'))
            .unwrap_or(s)
            .trim_start();
        s.strip_prefix(label).is_some_and(|rest| {
            rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('(')
        })
    })
}
fn persistent(label: &str) -> bool {
    let s = label.to_lowercase().replace('’', "'");
    [
        "always",
        "don't ask again",
        "do not ask again",
        "remember",
        "persistent",
        "for this session",
        "for all",
        "forever",
        "trust",
        "never ask",
    ]
    .iter()
    .any(|phrase| s.contains(phrase))
}
fn shortcut_key(key: &str) -> Result<String> {
    match key.to_lowercase().as_str() {
        "esc" | "escape" => Ok("esc".into()),
        "enter" | "return" => Ok("enter".into()),
        _ if key.len() == 1 && key.bytes().all(|b| b.is_ascii_alphanumeric()) => Ok(key.to_owned()),
        _ => bail!("The displayed shortcut is not a supported terminal key"),
    }
}
fn shortcut_grounded(screen: &str, label: &str, key: &str, instructions: &str) -> bool {
    // Bind shortcuts to the option, rather than accepting a letter anywhere in
    // the transcript. Numbered labels alone never establish numeric shortcuts.
    screen.contains(&format!("{label} ({key})"))
        || screen.contains(&format!("{label} [{key}]"))
        || label.ends_with(&format!("({key})"))
        || (instructions.to_lowercase().contains("type")
            && instructions
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word.eq_ignore_ascii_case(key)))
}

pub(crate) fn plan(
    question: &Question,
    extracted: &Value,
    screen: &str,
    conversation: bool,
    answer: &Answer,
) -> Result<InputPlan> {
    question.validate(answer)?;
    let options = extracted["options"]
        .as_array()
        .context("Missing extracted choices")?;
    if options.len() != question.options.len() || options.len() > 30 {
        bail!("Unsupported or inconsistent choices")
    }
    if matches!(answer, Answer::Clarify) {
        bail!("No specific answer was selected")
    }
    let body = question.body.to_lowercase();
    if !conversation
        && body.starts_with("press ")
        && body.contains("enter")
        && (body.contains("esc") || body.contains("cancel"))
    {
        bail!("Only keyboard instructions were extracted; the substantive question must be read before answering")
    }
    let instructions = extracted["interaction"]["instructions"]
        .as_str()
        .unwrap_or("");
    let method = extracted["interaction"]["method"]
        .as_str()
        .unwrap_or("unknown");
    match answer {
        Answer::Custom { text } => {
            if conversation {
                valid_prompt(text)?;
                return Ok(InputPlan::Prompt(text.clone()));
            }
            valid_text(text)?;
            if method == "text_entry" && has_enter(instructions) {
                return Ok(InputPlan::Text(text.clone()));
            }
            bail!("This dialog does not show how to submit a custom answer; use its terminal")
        }
        Answer::Choices { option_ids } => {
            let indices: Vec<usize> = option_ids
                .iter()
                .map(|id| {
                    question
                        .options
                        .iter()
                        .position(|o| &o.id == id)
                        .context("Unknown question option")
                })
                .collect::<Result<_>>()?;
            if conversation {
                if question.question_kind == "permission" {
                    bail!("Permissions require a dedicated dialog")
                }
                let text = indices
                    .iter()
                    .map(|i| question.options[*i].label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                valid_prompt(&text)?;
                return Ok(InputPlan::Prompt(text));
            }
            for i in &indices {
                if question.question_kind == "permission"
                    && options[*i]["effect"] == "allow_once"
                    && persistent(&question.options[*i].label)
                {
                    bail!("The option grants persistent access and cannot be treated as allow once")
                }
            }
            if question.multiple {
                // A checkbox menu needs both focus and checked-state evidence.
                // Never toggle an option whose current state is unknown.
                if !arrows(instructions)
                    || !has_enter(instructions)
                    || !instructions.to_lowercase().contains("space")
                {
                    bail!("Checkbox navigation and submission controls are not visible; use the terminal")
                }
                let mut at = extracted["interaction"]["selected_index"]
                    .as_u64()
                    .context("Checkbox focus is unknown")? as usize;
                if at >= options.len() || !focused(screen, &question.options[at].label) {
                    bail!("Checkbox focus is not grounded")
                }
                let mut keys = Vec::new();
                for (index, option) in options.iter().enumerate() {
                    let selected = option["selected"]
                        .as_bool()
                        .context("Checkbox checked state is unknown; use the terminal")?;
                    let markers: &[&str] = if selected {
                        &["[x]", "[X]", "☑"]
                    } else {
                        &["[ ]", "☐"]
                    };
                    let label = &question.options[index].label;
                    if !screen.lines().any(|line| {
                        markers.iter().any(|marker| {
                            line.split_once(marker)
                                .is_some_and(|(_, after)| after.trim_start().starts_with(label))
                        })
                    }) {
                        bail!("Checkbox state is not grounded in visible markers")
                    }
                    let desired = indices.contains(&index);
                    if selected != desired {
                        move_to(&mut keys, &mut at, index);
                        keys.push("space".into());
                    }
                }
                keys.push("enter".into());
                return Ok(InputPlan::Keys(keys));
            }
            let index = indices[0];
            let label = &question.options[index].label;
            let focus = extracted["interaction"]["selected_index"]
                .as_u64()
                .map(|x| x as usize);
            if has_enter(instructions)
                && focus.is_some_and(|at| {
                    at < options.len() && focused(screen, &question.options[at].label)
                })
            {
                let mut at = focus.unwrap();
                if at == index || arrows(instructions) {
                    let mut keys = Vec::new();
                    move_to(&mut keys, &mut at, index);
                    keys.push("enter".into());
                    return Ok(InputPlan::Keys(keys));
                }
            }
            if let Some(key) = options[index]["shortcut"].as_str() {
                if shortcut_grounded(screen, label, key, instructions) {
                    let key = shortcut_key(key)?;
                    if key == "enter" {
                        bail!("Enter alone requires grounded focus on the selected option")
                    }
                    let mut keys = vec![key.clone()];
                    // Only append Enter when the UI explicitly asks for it.
                    if key != "esc"
                        && key != "enter"
                        && (instructions.to_lowercase().contains("then")
                            || instructions.to_lowercase().contains("type"))
                        && has_enter(instructions)
                    {
                        keys.push("enter".into());
                    }
                    return Ok(InputPlan::Keys(keys));
                }
            }
            bail!("The selected option has no grounded input controls; use the agent terminal")
        }
        Answer::Clarify => unreachable!(),
    }
}
fn move_to(keys: &mut Vec<String>, at: &mut usize, target: usize) {
    keys.extend(std::iter::repeat_n(
        if target > *at {
            "down".into()
        } else {
            "up".into()
        },
        target.abs_diff(*at),
    ));
    *at = target;
}

pub(crate) fn permission(question: &Question, extracted: &Value, approve: bool) -> Result<Answer> {
    if question.question_kind != "permission" {
        bail!("This is not a permission request")
    }
    let wanted = if approve { "allow_once" } else { "deny" };
    let indices: Vec<_> = extracted["options"]
        .as_array()
        .context("Missing permission choices")?
        .iter()
        .enumerate()
        .filter(|(_, o)| o["effect"] == wanted)
        .map(|(index, _)| index)
        .collect();
    if indices.len() != 1 {
        bail!("No unambiguous {wanted} option; name a specific option or use the terminal")
    }
    let option = question
        .options
        .get(indices[0])
        .context("Permission choices changed")?;
    if approve && persistent(&option.label) {
        bail!("Approval cannot grant persistent access implicitly")
    }
    Ok(Answer::Choices {
        option_ids: vec![option.id.clone()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn question(permission: bool) -> (Question, Value, String) {
        let labels = if permission {
            vec!["Yes, proceed", "Always allow", "No"]
        } else {
            vec!["Local", "Cloud", "Other"]
        };
        let effects = if permission {
            vec!["allow_once", "allow_persistent", "deny"]
        } else {
            vec!["other"; 3]
        };
        let screen = format!("Choose destination:\n› 1. {} (y)\n  2. {} (p)\n  3. {} (esc)\nPress enter to confirm or esc to cancel",labels[0],labels[1],labels[2]);
        let value = json!({"options":labels.iter().zip(effects).zip(["y","p","esc"]).map(|((label,effect),key)|
            json!({"label":label,"effect":effect,"shortcut":key,"selected":null})).collect::<Vec<_>>(),
            "interaction":{"method":"typed_shortcut","selected_index":0,"instructions":"Press enter to confirm or esc to cancel"}});
        let q = Question {
            question_id: "q1".into(),
            question_kind: if permission { "permission" } else { "question" }.into(),
            title: "Test".into(),
            body: "Choose destination:".into(),
            options: labels
                .iter()
                .enumerate()
                .map(|(i, label)| crate::question::OptionItem {
                    id: i.to_string(),
                    label: (*label).into(),
                    description: String::new(),
                })
                .collect(),
            multiple: false,
            allow_custom: false,
        };
        (q, value, screen)
    }
    fn choices(index: usize) -> Answer {
        Answer::Choices {
            option_ids: vec![index.to_string()],
        }
    }
    #[test]
    fn approvals_do_not_promote_once_to_persistent_and_explicit_choices_preserve_scope() {
        let (q, v, screen) = question(true);
        let answer = permission(&q, &v, true).unwrap();
        assert!(matches!(&answer,Answer::Choices{option_ids} if option_ids==&vec!["0"]));
        assert_eq!(
            plan(&q, &v, &screen, false, &answer).unwrap(),
            InputPlan::Keys(vec!["enter".into()])
        );
        assert_eq!(
            plan(&q, &v, &screen, false, &permission(&q, &v, false).unwrap()).unwrap(),
            InputPlan::Keys(vec!["esc".into()])
        );
        assert_eq!(
            plan(&q, &v, &screen, false, &choices(1)).unwrap(),
            InputPlan::Keys(vec!["p".into()])
        );
        let mut bad = v.clone();
        bad["options"][0]["effect"] = json!("unknown");
        assert!(permission(&q, &bad, true).is_err());
        let mut badq = q.clone();
        badq.options[0].label = "Yes, don't ask again".into();
        assert!(permission(&badq, &v, true).is_err());
        assert!(plan(&badq, &v, &screen, false, &choices(0)).is_err());
    }
    #[test]
    fn numbered_choices_do_not_imply_digit_shortcuts_or_navigation() {
        let (q, mut v, screen) = question(false);
        v["options"][1]["shortcut"] = json!("2");
        assert!(plan(&q, &v, &screen, false, &choices(1)).is_err());
        v["interaction"]["selected_index"] = Value::Null;
        v["options"][1]["shortcut"] = json!("enter");
        assert!(plan(&q, &v, &screen, false, &choices(1)).is_err());
        let mut q = q;
        q.body = "Press enter to confirm or esc to cancel".into();
        assert!(plan(&q, &v, &screen, false, &choices(0)).is_err());
    }
    #[test]
    fn menu_navigation_is_ordered_and_requires_focus_and_displayed_keys() {
        let (q, mut v, mut screen) = question(false);
        let instructions = "Use up/down arrow keys. Press Enter to select.";
        v["interaction"]["instructions"] = json!(instructions);
        screen.push_str(instructions);
        assert_eq!(
            plan(&q, &v, &screen, false, &choices(2)).unwrap(),
            InputPlan::Keys(vec!["down".into(), "down".into(), "enter".into()])
        );
        let screen = screen.replace('›', " ");
        for option in v["options"].as_array_mut().unwrap() {
            option["shortcut"] = Value::Null;
        }
        assert!(plan(&q, &v, &screen, false, &choices(0)).is_err());
    }
    #[test]
    fn chat_replies_and_forms_keep_text_separate_from_key_names() {
        let (mut q, mut v, screen) = question(false);
        q.allow_custom = true;
        assert_eq!(
            plan(
                &q,
                &v,
                &screen,
                true,
                &Answer::Custom {
                    text: "ctrl+c".into()
                }
            )
            .unwrap(),
            InputPlan::Prompt("ctrl+c".into())
        );
        assert_eq!(
            plan(&q, &v, &screen, true, &choices(1)).unwrap(),
            InputPlan::Prompt("Cloud".into())
        );
        v["interaction"] = json!({"method":"text_entry","selected_index":null,"instructions":"Press Enter to submit"});
        assert_eq!(
            plan(
                &q,
                &v,
                &screen,
                false,
                &Answer::Custom {
                    text: "ctrl+c".into()
                }
            )
            .unwrap(),
            InputPlan::Text("ctrl+c".into())
        );
        assert!(plan(
            &q,
            &v,
            &screen,
            false,
            &Answer::Custom {
                text: "bad\u{1b}text".into()
            }
        )
        .is_err());
        assert!(plan(
            &q,
            &v,
            &screen,
            false,
            &Answer::Custom {
                text: "line\nline".into()
            }
        )
        .is_err());
        assert!(plan(&q, &v, &screen, false, &Answer::Clarify).is_err());
    }
    #[test]
    fn multiselect_toggles_only_different_checked_states() {
        let (mut q, mut v, _) = question(false);
        q.multiple = true;
        let instructions = "Use up/down arrows, Space to toggle, Enter to submit";
        v["interaction"] =
            json!({"method":"keyboard_menu","selected_index":0,"instructions":instructions});
        v["options"][0]["selected"] = json!(true);
        v["options"][1]["selected"] = json!(false);
        v["options"][2]["selected"] = json!(false);
        let screen =
            format!("Choose destination:\n› [x] Local\n  [ ] Cloud\n  [ ] Other\n{instructions}");
        assert_eq!(
            plan(&q, &v, &screen, false, &choices(1)).unwrap(),
            InputPlan::Keys(vec![
                "space".into(),
                "down".into(),
                "space".into(),
                "enter".into()
            ])
        );
        v["options"][1]["selected"] = Value::Null;
        assert!(plan(&q, &v, &screen, false, &choices(1)).is_err());
    }
}
