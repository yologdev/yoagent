//! Turning a provider's streamed tool-call argument text into the
//! `arguments` value of a [`Content::ToolCall`](crate::types::Content::ToolCall).
//!
//! Providers that stream arguments as a string (OpenAI-compatible, Responses,
//! Azure) all finish with the same decision, and it has cases that must never
//! be confused:
//!
//! - **Empty** text (or a bare `null`) is a tool with no parameters. There is
//!   no JSON to stream, so `""` resolves to `{}` and the tool runs normally.
//! - **Non-empty text that does not parse** is almost always a stream cut off
//!   mid-`arguments` — in practice the response hit its output token limit.
//!   Resolving it to `{}` would run the tool on its defaults instead of what
//!   the model asked for — `delete_files({"paths":` becomes
//!   `delete_files({})`. Instead the raw text is kept under
//!   [`UNPARSED_ARGUMENTS_KEY`], and the agent loop answers the call with an
//!   error tool result (see [`unparsed_tool_arguments`]) without running it.
//! - **Valid JSON that is not an object** (a string, number, array or bool)
//!   is marked the same way. Tool arguments are always an object; a tool
//!   handed a string reads every field as missing and runs on its defaults.
//!   The motivating case is double-encoded arguments, `"{\"path\":\"src\"}"`,
//!   which parse to a JSON *string* and made `list_files` silently list `.`.
//!
//! The marker lives inside `arguments` rather than in a new field so the
//! `Content::ToolCall` shape and its serde format are unchanged. It is the
//! same key the Anthropic provider uses as its streaming accumulator, so the
//! loop's guard also backs up that provider's own post-stream sweep.

use serde_json::Value;

/// Key under which a tool call carries argument text that did not resolve to
/// a JSON object (did not parse, or parsed to a non-object). A tool call whose `arguments` is exactly `{UNPARSED_ARGUMENTS_KEY:
/// "<raw text>"}` is never executed by the agent loop.
pub const UNPARSED_ARGUMENTS_KEY: &str = "__partial_json";

/// Resolve streamed argument text into a tool call's `arguments`.
///
/// Empty (or whitespace-only) text and a bare `null` resolve to `{}`; a JSON
/// object resolves to itself; anything else — text that does not parse, or
/// valid JSON that is not an object — resolves to the unparsed marker
/// `{"__partial_json": raw}`, which the agent loop turns into an error tool
/// result instead of running the tool.
pub fn parse_tool_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Default::default());
    }
    match serde_json::from_str(raw) {
        Ok(Value::Null) => Value::Object(Default::default()),
        Ok(v @ Value::Object(_)) => v,
        Ok(_) | Err(_) => {
            let mut obj = serde_json::Map::new();
            obj.insert(UNPARSED_ARGUMENTS_KEY.into(), Value::String(raw.into()));
            Value::Object(obj)
        }
    }
}

/// [`parse_tool_arguments`] plus an operator-visible warning when the text is
/// marked unparsed — the providers' shared finalization step.
pub(crate) fn finalize_tool_arguments(tool_name: &str, raw: &str) -> Value {
    let args = parse_tool_arguments(raw);
    if unparsed_tool_arguments(&args).is_some() {
        tracing::warn!(
            tool = %tool_name,
            len = raw.len(),
            "tool-call arguments are not a JSON object (truncated, or not an object); \
             the call will be answered with an error instead of run"
        );
    }
    args
}

/// If `arguments` is the unparsed marker produced by
/// [`parse_tool_arguments`], return the raw argument text it carries.
///
/// Only an object whose sole key is [`UNPARSED_ARGUMENTS_KEY`] with a string
/// value counts, so a tool that happens to take a parameter by that name
/// alongside others is not mistaken for one.
pub fn unparsed_tool_arguments(arguments: &Value) -> Option<&str> {
    let obj = arguments.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    obj.get(UNPARSED_ARGUMENTS_KEY)?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_text_is_a_zero_argument_call() {
        assert_eq!(parse_tool_arguments(""), json!({}));
        assert_eq!(parse_tool_arguments("  \n"), json!({}));
        assert_eq!(unparsed_tool_arguments(&json!({})), None);
    }

    #[test]
    fn valid_json_passes_through() {
        let v = parse_tool_arguments(r#"{"path":"a.txt"}"#);
        assert_eq!(v, json!({"path": "a.txt"}));
        assert_eq!(unparsed_tool_arguments(&v), None);
    }

    #[test]
    fn malformed_text_is_marked_not_defaulted() {
        let v = parse_tool_arguments(r#"{"paths":"#);
        assert_eq!(v, json!({"__partial_json": r#"{"paths":"#}));
        assert_eq!(unparsed_tool_arguments(&v), Some(r#"{"paths":"#));
    }

    #[test]
    fn null_is_a_zero_argument_call() {
        assert_eq!(parse_tool_arguments("null"), json!({}));
        assert_eq!(parse_tool_arguments(" null "), json!({}));
    }

    #[test]
    fn non_object_json_is_marked_not_passed_through() {
        for raw in ["\"src\"", "42", "-1.5", "[1,2]", "[]", "true", "false"] {
            let v = parse_tool_arguments(raw);
            assert_eq!(
                unparsed_tool_arguments(&v),
                Some(raw),
                "{raw} is valid JSON but not an object; it must not reach a tool"
            );
        }
    }

    /// Double-encoded arguments parse to a JSON *string* whose contents are
    /// the object. Passed through, `list_files` read `path` as missing and
    /// silently listed `.` instead of `src`.
    #[test]
    fn double_encoded_arguments_are_marked() {
        let raw = r#""{\"path\":\"src\"}""#;
        assert_eq!(
            serde_json::from_str::<Value>(raw).unwrap(),
            json!(r#"{"path":"src"}"#),
            "precondition: this really is a JSON string holding an object"
        );
        let v = parse_tool_arguments(raw);
        assert_eq!(unparsed_tool_arguments(&v), Some(raw));
    }

    #[test]
    fn marker_key_among_other_keys_is_not_the_marker() {
        let v = json!({"__partial_json": "x", "other": 1});
        assert_eq!(unparsed_tool_arguments(&v), None);
    }
}
