//! The choices an agent offers for its session (model, mode, effort) and the user's pick.
//!
//! ACP offers them in three shapes: `configOptions` (current), and `modes` / `models` (older or
//! unstable). They are read into one list. The agent's text is only ever displayed: a pick is
//! sent back only if its value is one the agent itself advertised for that option.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAX_OPTIONS: usize = 8;
const MAX_CHOICES: usize = 64;
const MAX_ID_BYTES: usize = 256;
const MAX_TEXT_CHARS: usize = 200;

/// Which ACP request changes an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    #[default]
    Config,
    Mode,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChoice {
    pub value: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOption {
    pub id: String,
    pub name: String,
    /// `mode`, `model`, `thought_level` or anything else the agent calls it.
    pub category: String,
    pub current: String,
    pub choices: Vec<AgentChoice>,
    #[serde(skip)]
    pub source: Source,
}

fn identifier(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= MAX_ID_BYTES)
        .map(str::to_string)
}
fn label(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(|s| s.chars().take(MAX_TEXT_CHARS).collect::<String>())
        .filter(|s| !s.trim().is_empty())
}

/// Choices of `configOptions`: a flat list, or groups holding lists.
fn config_choices(list: &Value, out: &mut Vec<AgentChoice>, depth: u8) {
    let Some(items) = list.as_array() else {
        return;
    };
    for item in items {
        if out.len() >= MAX_CHOICES {
            return;
        }
        if let Some(value) = identifier(&item["value"]) {
            out.push(AgentChoice {
                name: label(&item["name"]).unwrap_or_else(|| value.clone()),
                description: label(&item["description"]),
                value,
            });
        } else if depth == 0 {
            config_choices(&item["options"], out, 1);
        }
    }
}

fn simple_choices(list: &Value, id_key: &str) -> Vec<AgentChoice> {
    list.as_array()
        .into_iter()
        .flatten()
        .take(MAX_CHOICES)
        .filter_map(|item| {
            let value = identifier(&item[id_key])?;
            Some(AgentChoice {
                name: label(&item["name"]).unwrap_or_else(|| value.clone()),
                description: label(&item["description"]),
                value,
            })
        })
        .collect()
}

/// The options in a `session/new` result or in a `config_option_update`.
pub fn parse(result: &Value) -> Vec<AgentOption> {
    let mut options = Vec::new();
    for item in result["configOptions"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_OPTIONS)
    {
        if item["type"] != "select" {
            continue;
        }
        let (Some(id), Some(current)) =
            (identifier(&item["id"]), identifier(&item["currentValue"]))
        else {
            continue;
        };
        let mut choices = Vec::new();
        config_choices(&item["options"], &mut choices, 0);
        if choices.is_empty() {
            continue;
        }
        options.push(AgentOption {
            name: label(&item["name"]).unwrap_or_else(|| id.clone()),
            category: label(&item["category"]).unwrap_or_else(|| "other".into()),
            id,
            current,
            choices,
            source: Source::Config,
        });
    }
    if options.is_empty() {
        for (key, list, id_key, current_key, id, name, source) in [
            (
                "modes",
                "availableModes",
                "id",
                "currentModeId",
                "mode",
                "Mode",
                Source::Mode,
            ),
            (
                "models",
                "availableModels",
                "modelId",
                "currentModelId",
                "model",
                "Model",
                Source::Model,
            ),
        ] {
            let choices = simple_choices(&result[key][list], id_key);
            let Some(current) = identifier(&result[key][current_key]) else {
                continue;
            };
            if !choices.is_empty() {
                options.push(AgentOption {
                    id: id.into(),
                    name: name.into(),
                    category: id.into(),
                    current,
                    choices,
                    source,
                });
            }
        }
    }
    options
}

/// Makes `value` the current choice of option `id`, if the agent offered it.
pub fn select(options: &mut [AgentOption], id: &str, value: &str) -> bool {
    let Some(option) = options.iter_mut().find(|o| o.id == id) else {
        return false;
    };
    if !option.choices.iter().any(|c| c.value == value) {
        return false;
    }
    option.current = value.to_string();
    true
}

/// The ACP request that changes `option` to `value`.
pub fn change_request(
    option: &AgentOption,
    session_id: &str,
    value: &str,
) -> (&'static str, Value) {
    match option.source {
        Source::Config => (
            "session/set_config_option",
            json!({"sessionId":session_id,"configId":option.id,"value":value}),
        ),
        Source::Mode => (
            "session/set_mode",
            json!({"sessionId":session_id,"modeId":value}),
        ),
        Source::Model => (
            "session/set_model",
            json!({"sessionId":session_id,"modelId":value}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_options_are_read_with_groups_and_bounds() {
        let result = json!({"configOptions":[
            {"id":"model","name":"Model","category":"model","type":"select","currentValue":"fast",
             "options":[{"value":"fast","name":"Fast"},{"value":"deep","name":"Deep","description":"Slow"}]},
            {"id":"effort","name":"Effort","category":"thought_level","type":"select","currentValue":"low",
             "options":[{"group":"g","name":"Group","options":[{"value":"low","name":"Low"},{"value":"high","name":"High"}]}]},
            {"id":"flag","name":"Flag","type":"boolean","currentValue":true},
            {"id":"","name":"No id","type":"select","currentValue":"a","options":[{"value":"a"}]},
            {"id":"empty","name":"Empty","type":"select","currentValue":"a","options":[]}
        ]});
        let options = parse(&result);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].category, "model");
        assert_eq!(options[0].choices[1].description.as_deref(), Some("Slow"));
        assert_eq!(options[1].choices.len(), 2);
        assert_eq!(options[1].source, Source::Config);
    }

    #[test]
    fn modes_and_models_are_the_fallback_and_cannot_flood() {
        let many: Vec<_> = (0..500)
            .map(|i| json!({"id":format!("m{i}"),"name":"x"}))
            .collect();
        let result = json!({
            "modes":{"currentModeId":"default","availableModes":[
                {"id":"default","name":"Default"},{"id":"plan","name":"Plan"}]},
            "models":{"currentModelId":"a","availableModels":[{"modelId":"a","name":"A"},{"modelId":"b","name":"B"}]}
        });
        let options = parse(&result);
        assert_eq!(
            options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            ["mode", "model"]
        );
        assert_eq!(options[0].source, Source::Mode);
        assert_eq!(options[1].source, Source::Model);
        let flood = json!({"modes":{"currentModeId":"m0","availableModes":many}});
        assert_eq!(parse(&flood)[0].choices.len(), MAX_CHOICES);
        assert!(parse(&json!({})).is_empty());
    }

    #[test]
    fn only_an_advertised_value_can_be_picked() {
        let mut options = parse(
            &json!({"modes":{"currentModeId":"default","availableModes":[
            {"id":"default","name":"Default"},{"id":"plan","name":"Plan"}]}}),
        );
        assert!(!select(&mut options, "mode", "bypassEverything"));
        assert!(!select(&mut options, "nothing", "plan"));
        assert_eq!(options[0].current, "default");
        assert!(select(&mut options, "mode", "plan"));
        assert_eq!(options[0].current, "plan");
        let (method, params) = change_request(&options[0], "s1", "plan");
        assert_eq!(method, "session/set_mode");
        assert_eq!(params["modeId"], "plan");
    }
}
