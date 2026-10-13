//! Exact transcript identity for a source-terminal viewport action.
use super::{binding::HistoryBinding, model::ConversationMessage, HistoryError};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
#[derive(Debug, Clone)]
pub(crate) struct JumpSourceGuard {
    pub revision: Arc<AtomicU64>,
    pub expected: u64,
}
impl JumpSourceGuard {
    pub fn validate(&self) -> Result<(), HistoryError> {
        if self.revision.load(Ordering::Acquire) != self.expected {
            return Err(HistoryError::new(
                "source_changed",
                "saved conversation changed or was evicted; reload history",
            ));
        }
        Ok(())
    }
}
pub(crate) struct ExactHumanPrompt {
    pub binding: HistoryBinding,
    pub message_id: String,
    pub message_revision: String,
    pub text: String,
    pub assistant_line: String,
    pub guard: JumpSourceGuard,
}
pub(super) fn exact_prompt(message: &ConversationMessage) -> Option<&str> {
    if message.role != "user" || message.origin != "human" || message.blocks.len() != 1 {
        return None;
    }
    let block = &message.blocks[0];
    if block.kind != "text" || block.truncated {
        return None;
    }
    block.text.as_deref()
}
pub(super) fn validate_prompt(text: &str) -> Result<(), HistoryError> {
    if text.len() > MAX_PROMPT_BYTES {
        return Err(HistoryError::new(
            "scan_limit",
            "saved prompt exceeds the terminal jump scan limit",
        ));
    }
    if text.is_empty()
        || text.chars().any(|c| c.is_control() && c != '\n')
        || text
            .split('\n')
            .any(|line| line.is_empty() || line.trim_end() != line)
    {
        return Err(HistoryError::new(
            "jump_unavailable",
            "full visible prompt whitespace cannot be verified in retained terminal rows",
        ));
    }
    Ok(())
}

pub(super) fn assistant_context_line(
    message: &ConversationMessage,
) -> Result<String, HistoryError> {
    let unavailable = || {
        HistoryError::new("jump_unavailable", "a complete plain Claude assistant response is required to verify the rendered exchange")
    };
    let block = message.blocks.first().ok_or_else(unavailable)?;
    if message.role != "assistant" || block.kind != "text" || block.truncated {
        return Err(unavailable());
    }
    let line = block
        .text
        .as_deref()
        .and_then(|text| text.split('\n').next())
        .ok_or_else(unavailable)?;
    // Markdown rendering can remove syntax or insert rows. This version supports
    // an exact plain first logical line only, with a bounded context scan.
    if line.is_empty()
        || line.len() > 4096
        || line.trim() != line
        || !line.chars().next().is_some_and(char::is_alphanumeric)
        || line
            .chars()
            .any(|c| c.is_control() || "`*~[]<>".contains(c))
    {
        return Err(unavailable());
    }
    Ok(line.to_owned())
}

#[derive(Debug)]
pub(crate) struct ConversationJumpCandidate {
    pub binding: HistoryBinding,
    pub message_id: String,
    pub message_revision: String,
    pub source_guard: JumpSourceGuard,
    pub anchor: crate::pane::ConversationTerminalJumpAnchor,
}
