//! Checking what the model returned against what it was asked for (FR-AI-05): tool names and
//! arguments, and structured output such as drafts. A failed answer is retried once with a
//! [`hint`] naming its problems.

use serde_json::Value;

use super::request::{Message, Output, Response, Tool, ToolCall};

const EMPTY: &str = "the answer is empty";
const NO_TOOLS: &str = "no tools are offered here; answer without calling one";

/// An answer that passed validation.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// For [`Output::Text`]: not empty
    Text(String),
    /// For [`Output::Tools`]: each call names an offered tool and has arguments matching its
    /// parameters. There is text, or at least one call.
    Tools { text: String, calls: Vec<ToolCall> },
    /// For [`Output::Json`]: matches the schema
    Json(Value),
}

/// The answer in `response`, or every problem with it.
pub(super) fn check(output: &Output, response: &Response) -> Result<Answer, Vec<String>> {
    let text = response.text.trim();
    match output {
        Output::Text => {
            if !response.tool_calls.is_empty() {
                return Err(vec![NO_TOOLS.to_owned()]);
            }
            if text.is_empty() {
                return Err(vec![EMPTY.to_owned()]);
            }
            Ok(Answer::Text(text.to_owned()))
        }
        Output::Tools(tools) => {
            let problems: Vec<String> = response
                .tool_calls
                .iter()
                .flat_map(|call| check_call(tools, call))
                .collect();
            if !problems.is_empty() {
                return Err(problems);
            }
            if text.is_empty() && response.tool_calls.is_empty() {
                return Err(vec![EMPTY.to_owned()]);
            }
            Ok(Answer::Tools {
                text: text.to_owned(),
                calls: response.tool_calls.clone(),
            })
        }
        Output::Json(schema) => {
            if !response.tool_calls.is_empty() {
                return Err(vec![NO_TOOLS.to_owned()]);
            }
            let value: Value = serde_json::from_str(text)
                .map_err(|_| vec!["the answer is not valid JSON".to_owned()])?;
            let problems = schema.validate(&value);
            if problems.is_empty() {
                Ok(Answer::Json(value))
            } else {
                Err(problems.iter().map(ToString::to_string).collect())
            }
        }
    }
}

fn check_call(tools: &[Tool], call: &ToolCall) -> Vec<String> {
    let Some(tool) = tools.iter().find(|tool| tool.name == call.name) else {
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        return vec![format!(
            "there is no tool `{}`; the tools are: {}",
            call.name,
            names.join(", ")
        )];
    };
    tool.parameters
        .validate(&call.arguments)
        .iter()
        .map(|problem| format!("{}: {problem}", tool.name))
        .collect()
}

/// Asks the model to answer again, naming what was wrong with its answer (FR-AI-05).
pub(super) fn hint(problems: &[String]) -> Message {
    Message::user(format!(
        "[system] Your previous answer was rejected:\n- {}\nAnswer again, following the instructions and schemas exactly.",
        problems.join("\n- ")
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ai::request::Usage;
    use crate::ai::schema::Schema;

    fn tools() -> Output {
        Output::Tools(vec![
            Tool {
                name: "search_products".to_owned(),
                description: "Search".to_owned(),
                parameters: Schema::object()
                    .property("keywords", Schema::string())
                    .optional("maxPrice", Schema::number().minimum(0)),
            },
            Tool {
                name: "get_cart".to_owned(),
                description: "The cart".to_owned(),
                parameters: Schema::object(),
            },
        ])
    }

    fn response(text: &str, calls: &[(&str, Value)]) -> Response {
        Response {
            text: text.to_owned(),
            tool_calls: calls
                .iter()
                .enumerate()
                .map(|(i, (name, arguments))| ToolCall {
                    id: format!("call-{i}"),
                    name: (*name).to_owned(),
                    arguments: arguments.clone(),
                })
                .collect(),
            usage: Usage::default(),
        }
    }

    #[test]
    fn text_must_not_be_empty() {
        assert_eq!(
            check(&Output::Text, &response("  สวัสดี  ", &[])),
            Ok(Answer::Text("สวัสดี".to_owned()))
        );
        assert_eq!(
            check(&Output::Text, &response(" \n", &[])),
            Err(vec![EMPTY.to_owned()])
        );
        assert_eq!(
            check(&Output::Text, &response("x", &[("get_cart", json!({}))])),
            Err(vec![NO_TOOLS.to_owned()])
        );
    }

    #[test]
    fn offered_tools_with_valid_arguments_pass() {
        let calls = [
            (
                "search_products",
                json!({ "keywords": "ไข่ไก่", "maxPrice": 50 }),
            ),
            ("get_cart", json!({})),
        ];
        let Ok(Answer::Tools { text, calls }) = check(&tools(), &response("", &calls)) else {
            panic!("not accepted");
        };
        assert_eq!(text, "");
        assert_eq!(calls.len(), 2);

        // Text alone is an answer too
        assert_eq!(
            check(&tools(), &response("ได้เลย", &[])),
            Ok(Answer::Tools {
                text: "ได้เลย".to_owned(),
                calls: Vec::new()
            })
        );
    }

    #[test]
    fn unknown_tools_and_bad_arguments_are_named() {
        let calls = [
            ("serch_products", json!({ "keywords": "ไข่" })),
            ("search_products", json!({ "maxPrice": "cheap" })),
            ("get_cart", json!("{not json")),
        ];
        assert_eq!(
            check(&tools(), &response("", &calls)),
            Err(vec![
                "there is no tool `serch_products`; the tools are: search_products, get_cart"
                    .to_owned(),
                "search_products: `keywords` is required".to_owned(),
                "search_products: `maxPrice` must be a number".to_owned(),
                "get_cart: must be an object".to_owned(),
            ])
        );
        assert_eq!(
            check(&tools(), &response("", &[])),
            Err(vec![EMPTY.to_owned()])
        );
    }

    #[test]
    fn json_must_parse_and_match_the_schema() {
        let output = Output::Json(
            Schema::object()
                .property("verdict", Schema::string().one_of(["allow", "review"]))
                .property("reason", Schema::string()),
        );
        let valid = r#" {"verdict": "review", "reason": "ราคาแปลก"} "#;
        assert_eq!(
            check(&output, &response(valid, &[])),
            Ok(Answer::Json(
                json!({ "verdict": "review", "reason": "ราคาแปลก" })
            ))
        );
        assert_eq!(
            check(&output, &response("```json\n{}\n```", &[])),
            Err(vec!["the answer is not valid JSON".to_owned()])
        );
        assert_eq!(
            check(&output, &response(r#"{"verdict": "ban"}"#, &[])),
            Err(vec![
                "`verdict` must be one of: allow, review".to_owned(),
                "`reason` is required".to_owned(),
            ])
        );
    }

    #[test]
    fn the_hint_lists_the_problems() {
        let hint = hint(&["a".to_owned(), "b".to_owned()]);
        let text = hint.text();
        assert!(text.contains("- a\n- b\n"), "{text}");
    }
}
