use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryBinding {
    pub provider: String,
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub generation: String,
}
impl HistoryBinding {
    // Hook paths are private server state, never supplied in a history read request.
    // Canonicalization and source identity validation happen only on the worker.
    pub fn from_report(
        source: &str,
        agent: &str,
        session: Option<&str>,
        path: Option<&str>,
    ) -> Option<Self> {
        if source != "herdr:claude" || agent != "claude" {
            return None;
        }
        let session = session?;
        crate::agent_resume::AgentSessionRef::id(session)?;
        let path = path?;
        if path.len() > 32768
            || path.chars().any(|c| c.is_control())
            || !PathBuf::from(path).is_absolute()
        {
            return None;
        }
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        Some(Self {
            provider: "claude".into(),
            session_id: session.into(),
            transcript_path: path.into(),
            generation: format!(
                "{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                GENERATION.fetch_add(1, Ordering::Relaxed)
            ),
        })
    }
    pub fn public(&self) -> super::model::ConversationBinding {
        super::model::ConversationBinding {
            provider: self.provider.clone(),
            session_id: self.session_id.clone(),
            generation: self.generation.clone(),
        }
    }
}
