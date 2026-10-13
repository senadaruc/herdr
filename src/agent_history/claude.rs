//! Two supported Claude JSONL content variants: string and typed block arrays.
use super::{model::*, HistoryError};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::borrow::Cow;

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn optional_string(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

pub fn normalize(line: &[u8], session: &str) -> Result<Option<ConversationMessage>, HistoryError> {
    Ok(parse_record(line, session)?.message)
}

#[derive(Default)]
pub(crate) struct ParsedRecord {
    pub message: Option<ConversationMessage>,
    pub parent_link: Option<ParentLink>,
}

pub(crate) struct ParentLink {
    pub id: String,
    pub parent: Option<String>,
}

pub(crate) fn parse_record(line: &[u8], session: &str) -> Result<ParsedRecord, HistoryError> {
    let record: Value = serde_json::from_slice(line).map_err(|_| {
        HistoryError::new(
            "unsupported_schema",
            "unrecognized complete transcript record",
        )
    })?;
    if record.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || record.get("agentId").is_some()
    {
        return Ok(ParsedRecord::default());
    }
    let kind = record.get("type").and_then(Value::as_str);
    if kind == Some("attachment") {
        let parent_link = ancestry_link(&record, session)?;
        if !record.get("attachment").is_some_and(Value::is_object) {
            return Err(HistoryError::new(
                "unsupported_schema",
                "attachment ancestry is not a supported main-chain record",
            ));
        }
        return Ok(ParsedRecord {
            message: None,
            parent_link: Some(parent_link),
        });
    }
    if kind == Some("system")
        && matches!(
            record.get("subtype").and_then(Value::as_str),
            Some("stop_hook_summary" | "turn_duration")
        )
        && system_content(&record).is_none()
    {
        if record
            .get("message")
            .is_some_and(|message| !message.is_null() && !message.is_object())
        {
            return Err(HistoryError::new(
                "unsupported_schema",
                "system event has unsupported message encoding",
            ));
        }
        return Ok(ParsedRecord {
            message: None,
            parent_link: Some(ancestry_link(&record, session)?),
        });
    }
    if kind.is_some_and(known_metadata) {
        // Most metadata has no ancestry envelope. Never derive links from payload IDs.
        let has_envelope = record.get("uuid").is_some()
            && record.get("parentUuid").is_some()
            && record.get("sessionId").is_some()
            && record.get("isSidechain").and_then(Value::as_bool) == Some(false);
        return Ok(ParsedRecord {
            message: None,
            parent_link: has_envelope
                .then(|| ancestry_link(&record, session))
                .transpose()?,
        });
    }
    Ok(ParsedRecord {
        message: normalize_record(record, session)?,
        parent_link: None,
    })
}

fn ancestry_link(record: &Value, session: &str) -> Result<ParentLink, HistoryError> {
    if record.get("sessionId").and_then(Value::as_str) != Some(session) {
        return Err(HistoryError::new(
            "session_changed",
            "transcript does not confirm the exact reported session",
        ));
    }
    if record.get("isSidechain").and_then(Value::as_bool) != Some(false) {
        return Err(HistoryError::new(
            "unsupported_schema",
            "nonvisible ancestry is not a supported main-chain record",
        ));
    }
    let stable_id = |value: &str| {
        !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
    };
    let id = optional_string(record, "uuid")
        .filter(|value| stable_id(value))
        .ok_or_else(|| {
            HistoryError::new("unsupported_schema", "ancestry record has no stable ID")
        })?;
    let parent = match record.get("parentUuid") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if stable_id(value) => Some(value.clone()),
        _ => {
            return Err(HistoryError::new(
                "unsupported_schema",
                "ancestry record has no supported parent ID",
            ));
        }
    };
    // Only IDs enter the ancestry index; hidden event bodies are never cached or normalized.
    Ok(ParentLink { id, parent })
}

fn known_metadata(kind: &str) -> bool {
    matches!(
        kind,
        "summary"
            | "file-history-snapshot"
            | "file-history-delta"
            | "progress"
            | "queue-operation"
            | "last-prompt"
            | "custom-title"
            | "ai-title"
            | "agent-name"
            | "agent-color"
            | "session-info"
            | "mode"
            | "permission-mode"
            | "atis-latch"
            | "bridge-session"
            | "pr-link"
            | "cost-state"
            | "frame-link"
            | "artifact-comment-monitor"
            | "artifact-autoreact-ledger"
            | "history-suppression"
    )
}

fn system_content(record: &Value) -> Option<&Value> {
    record
        .get("content")
        .filter(|content| !content.is_null())
        .or_else(|| {
            record
                .get("message")
                .and_then(|message| message.get("content"))
                .filter(|content| !content.is_null())
        })
}

fn normalize_record(
    record: Value,
    session: &str,
) -> Result<Option<ConversationMessage>, HistoryError> {
    let kind = record.get("type").and_then(Value::as_str).ok_or_else(|| {
        HistoryError::new(
            "unsupported_schema",
            "transcript record has no supported type",
        )
    })?;
    if !matches!(kind, "user" | "assistant" | "system") {
        return Err(HistoryError::new(
            "unsupported_schema",
            "unknown transcript record type",
        ));
    }
    if record.get("sessionId").and_then(Value::as_str) != Some(session) {
        return Err(HistoryError::new(
            "session_changed",
            "transcript does not confirm the exact reported session",
        ));
    }
    let id = optional_string(&record, "uuid")
        .filter(|s| !s.is_empty() && s.len() <= 512)
        .ok_or_else(|| {
            HistoryError::new("unsupported_schema", "conversation record has no stable ID")
        })?;
    if kind != "system"
        && record
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            != Some(kind)
    {
        return Err(HistoryError::new(
            "unsupported_schema",
            "transcript message role does not match its record type",
        ));
    }
    let content = if kind == "system" {
        system_content(&record)
    } else {
        record.get("message").and_then(|m| m.get("content"))
    };
    let content = content.ok_or_else(|| {
        HistoryError::new("unsupported_schema", "unsupported conversation content")
    })?;
    let mut blocks = Vec::new();
    let mut human = kind == "user";
    let mut tool = false;
    if let Some(text) = content.as_str() {
        blocks.push(block(&id, 0, "text", Some(text.into()), None, None));
    } else if let Some(items) = content.as_array() {
        for item in items {
            let block_kind = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let index = blocks.len();
            match block_kind {
                "thinking" | "redacted_thinking" => {}
                "text" => {
                    let text = item.get("text").and_then(Value::as_str).ok_or_else(|| {
                        HistoryError::new("unsupported_schema", "text block is missing text")
                    })?;
                    blocks.push(block(&id, index, "text", Some(text.into()), None, None));
                }
                "tool_use" => {
                    human = false;
                    tool = true;
                    let text = item.get("input").map(|input| {
                        serde_json::to_string(&visible_tool_input(input)).unwrap_or_default()
                    });
                    blocks.push(block(
                        &id,
                        index,
                        "tool_call",
                        text,
                        optional_string(item, "id"),
                        optional_string(item, "name"),
                    ));
                }
                "tool_result" => {
                    human = false;
                    tool = true;
                    let text = visible_tool_text(item.get("content"));
                    blocks.push(block(
                        &id,
                        index,
                        "tool_result",
                        Some(text),
                        optional_string(item, "tool_use_id"),
                        None,
                    ));
                }
                "image" | "document" => blocks.push(block(
                    &id,
                    index,
                    "attachment",
                    None,
                    Some(
                        item.get("title")
                            .and_then(Value::as_str)
                            .unwrap_or(if block_kind == "image" {
                                "Image attachment"
                            } else {
                                "Document attachment"
                            })
                            .into(),
                    ),
                    None,
                )),
                _ => {
                    human = false;
                    blocks.push(block(
                        &id,
                        index,
                        "unknown",
                        optional_string(item, "text"),
                        Some("Unsupported content block".into()),
                        None,
                    ));
                }
            }
        }
    } else {
        return Err(HistoryError::new(
            "unsupported_schema",
            "unsupported conversation content encoding",
        ));
    }
    if record.get("isSynthetic").and_then(Value::as_bool) == Some(true)
        || record.get("isMeta").and_then(Value::as_bool) == Some(true)
        || record.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        || record.get("sourceToolAssistantUUID").is_some()
        || record.get("origin").is_some_and(|origin| match origin {
            Value::String(kind) => kind != "human",
            Value::Object(_) => origin.get("kind").and_then(Value::as_str) != Some("human"),
            _ => true,
        })
    {
        human = false;
    }
    // These Claude-injected textual protocol notices are never human rail turns.
    if blocks.iter().filter_map(|b| b.text.as_deref()).any(|text| {
        [
            "<local-command-",
            "<command-name>",
            "<system-reminder>",
            "[Request interrupted by user",
            "This session is being continued from a previous conversation",
        ]
        .iter()
        .any(|prefix| text.starts_with(prefix))
    }) {
        human = false;
    }
    let origin = if kind == "assistant" {
        "model"
    } else if kind == "system"
        || record.get("isMeta").and_then(Value::as_bool) == Some(true)
        || record.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
    {
        "system"
    } else if tool {
        "tool"
    } else if human {
        "human"
    } else {
        "unknown"
    };
    let mut message = ConversationMessage {
        id,
        parent_id: optional_string(&record, "parentUuid").filter(|s| !s.is_empty()),
        turn_id: String::new(),
        role: kind.into(),
        origin: origin.into(),
        timestamp: optional_string(&record, "timestamp"),
        status: if record.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
            || record.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            "compaction_notice"
        } else {
            "recorded"
        }
        .into(),
        message_revision: String::new(),
        order: 0,
        blocks,
    };
    message.message_revision = digest(&serde_json::to_vec(&message).unwrap());
    Ok(Some(message))
}
fn block(
    id: &str,
    index: usize,
    kind: &str,
    text: Option<String>,
    label: Option<String>,
    name: Option<String>,
) -> ContentBlock {
    ContentBlock {
        id: format!("{id}:b{index}"),
        kind: kind.into(),
        text,
        label,
        name,
        truncated: false,
        block_cursor: None,
    }
}
fn visible_tool_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item.get("type").and_then(Value::as_str) {
                Some("text") => item.get("text").and_then(Value::as_str).map(Cow::Borrowed),
                Some("image") => Some(Cow::Borrowed("[Image attachment]")),
                Some("document") => Some(Cow::Borrowed("[Document attachment]")),
                Some("tool_reference") => Some(Cow::Owned(
                    item.get("tool_name")
                        .and_then(Value::as_str)
                        .filter(|name| {
                            !name.is_empty()
                                && name.len() <= 512
                                && !name.chars().any(char::is_control)
                        })
                        .map_or_else(
                            || "Tool reference".into(),
                            |name| format!("Tool reference: {name}"),
                        ),
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Tool arguments may contain typed attachment objects too. Keep ordinary arguments,
/// but never transport encoded image/document bodies through textual tool previews.
fn visible_tool_input(value: &Value) -> Value {
    match value {
        Value::Object(object)
            if object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "image" | "document" | "base64")) =>
        {
            Value::String("[Attachment body omitted]".into())
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), visible_tool_input(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(visible_tool_input).collect()),
        _ => value.clone(),
    }
}
