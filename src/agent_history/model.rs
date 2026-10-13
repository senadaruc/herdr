//! Read-only normalized conversation wire data. No terminal codec uses these types.
use serde::{Deserialize, Serialize};

pub const MAX_BYTES: usize = 1024 * 1024;
fn default_limit() -> usize {
    100
}
fn default_bytes() -> usize {
    MAX_BYTES
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Window {
    Latest,
    Before {
        cursor: String,
    },
    After {
        cursor: String,
    },
    Around {
        message_id: String,
    },
    Search {
        query: String,
        #[serde(default)]
        cursor: Option<String>,
    },
    Turns {
        #[serde(default)]
        cursor: Option<String>,
    },
    Changes {
        cursor: String,
    },
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub limit: usize,
    pub max_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            limit: 100,
            max_bytes: MAX_BYTES,
        }
    }
}
impl Limits {
    pub fn validate(self, turns: bool) -> Result<Self, super::HistoryError> {
        if self.limit == 0
            || self.limit > if turns { 500 } else { 200 }
            || self.max_bytes == 0
            || self.max_bytes > MAX_BYTES
        {
            return Err(super::HistoryError::new(
                "invalid_params",
                "history limits exceed supported bounds",
            ));
        }
        Ok(self)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationReadParams {
    pub target: String,
    pub expected_session_id: String,
    pub window: Window,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_bytes")]
    pub max_bytes: usize,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationBlockReadParams {
    pub target: String,
    pub expected_session_id: String,
    pub block_cursor: String,
    #[serde(default = "default_bytes")]
    pub max_bytes: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConversationBinding {
    pub provider: String,
    pub session_id: String,
    pub generation: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContentBlock {
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_cursor: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConversationMessage {
    pub id: String,
    pub parent_id: Option<String>,
    pub turn_id: String,
    pub role: String,
    pub origin: String,
    pub timestamp: Option<String>,
    pub status: String,
    pub message_revision: String,
    pub order: usize,
    pub blocks: Vec<ContentBlock>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConversationTurn {
    pub id: String,
    pub message_id: String,
    pub ordinal: usize,
    pub preview: String,
    pub timestamp: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConversationPage {
    pub binding: Option<ConversationBinding>,
    pub revision: String,
    pub availability: String,
    pub completeness: String,
    pub messages: Vec<ConversationMessage>,
    pub turns: Vec<ConversationTurn>,
    pub before_cursor: Option<String>,
    pub after_cursor: Option<String>,
    pub turns_cursor: Option<String>,
    pub search_cursor: Option<String>,
    pub changes_cursor: Option<String>,
    pub upserts: Vec<ConversationMessage>,
    pub removed_message_ids: Vec<String>,
    pub match_message_ids: Vec<String>,
}
impl ConversationPage {
    pub fn unavailable(binding: Option<ConversationBinding>, state: &str) -> Self {
        Self {
            binding,
            revision: "0".into(),
            availability: state.into(),
            completeness: "source_unavailable".into(),
            messages: vec![],
            turns: vec![],
            before_cursor: None,
            after_cursor: None,
            turns_cursor: None,
            search_cursor: None,
            changes_cursor: None,
            upserts: vec![],
            removed_message_ids: vec![],
            match_message_ids: vec![],
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlockPage {
    pub binding: ConversationBinding,
    pub revision: String,
    pub message_id: String,
    pub message_revision: String,
    pub block_id: String,
    pub kind: String,
    pub text: String,
    pub slice_index: usize,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConversationJumpParams {
    pub target: String,
    pub expected_session_id: String,
    pub expected_generation: String,
    pub message_id: String,
    pub message_revision: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConversationJumpResult {
    pub binding: ConversationBinding,
    pub message_id: String,
    pub message_revision: String,
    pub content_seq: u64,
    pub source_row: u64,
    pub offset_from_bottom: u64,
}
