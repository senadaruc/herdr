use super::{binding::HistoryBinding, model::*, *};
use std::io::Write;
struct Fixture {
    path: std::path::PathBuf,
    binding: HistoryBinding,
}
impl Fixture {
    fn new(text: &str) -> Self {
        static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "herdr-synthetic-history-{}-{}.jsonl",
            std::process::id(),
            ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, text).unwrap();
        let binding = HistoryBinding::from_report(
            "herdr:claude",
            "claude",
            Some("synthetic-session"),
            path.to_str(),
        )
        .unwrap();
        Self { path, binding }
    }
    fn append(&self, text: &str) {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
fn record(id: &str, parent: Option<&str>, role: &str, content: serde_json::Value) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":role,"uuid":id,"parentUuid":parent,"sessionId":"synthetic-session","isSidechain":false,"message":{"role":role,"content":content}})
    )
}
#[test]
fn exact_binding_keeps_custom_long_paths_and_rejects_other_owners() {
    let path = format!(
        "/custom-config/{}/synthetic-session.jsonl",
        "long-project-".repeat(20)
    );
    let binding = HistoryBinding::from_report(
        "herdr:claude",
        "claude",
        Some("synthetic-session"),
        Some(&path),
    )
    .unwrap();
    assert_eq!(binding.transcript_path.to_str().unwrap(), path);
    assert!(HistoryBinding::from_report("plugin", "claude", Some("s"), Some(&path)).is_none());
    assert!(
        HistoryBinding::from_report("herdr:claude", "claude", Some("s"), Some("relative"))
            .is_none()
    );
}
#[test]
fn normalizes_human_reply_tools_and_excludes_reasoning_and_attachments() {
    let fixture = Fixture::new(include_str!(
        "../../tests/fixtures/claude-history/visible.jsonl"
    ));
    let mut reader = ConversationReader::default();
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(page.availability, "ready");
    assert_eq!(
        page.turns
            .iter()
            .map(|t| t.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["human-1"]
    );
    assert_eq!(
        page.messages
            .iter()
            .filter(|m| m.id == "assistant-1")
            .count(),
        1
    );
    let json = serde_json::to_string(&page).unwrap();
    assert!(!json.contains("SECRET_BASE64"));
    assert!(!json.contains("INTERNAL_THINKING"));
    assert!(page
        .messages
        .iter()
        .any(|m| m.id == "tool-result" && m.origin == "tool"));
}
#[test]
fn whole_source_search_around_and_chronological_paging() {
    let text: String = (0..5000)
        .map(|i| {
            record(
                &format!("m{i}"),
                if i == 0 { None } else { Some("") },
                "user",
                format!("prompt {i}").into(),
            )
        })
        .collect();
    let fixture = Fixture::new(&text);
    let mut reader = ConversationReader::default();
    let latest = reader
        .read(
            &fixture.binding,
            Window::Latest,
            Limits {
                limit: 2,
                ..Limits::default()
            },
        )
        .unwrap();
    assert_eq!(latest.messages[0].id, "m4998");
    let older = reader
        .read(
            &fixture.binding,
            Window::Before {
                cursor: latest.before_cursor.clone().unwrap(),
            },
            Limits {
                limit: 2,
                ..Limits::default()
            },
        )
        .unwrap();
    assert_eq!(older.messages[0].id, "m4996");
    let search = reader
        .read(
            &fixture.binding,
            Window::Search {
                query: "prompt 3".into(),
                cursor: None,
            },
            Limits::default(),
        )
        .unwrap();
    assert!(search.match_message_ids.contains(&"m3".into()));
    let around = reader
        .read(
            &fixture.binding,
            Window::Around {
                message_id: "m3".into(),
            },
            Limits {
                limit: 3,
                ..Limits::default()
            },
        )
        .unwrap();
    assert!(around.messages.iter().any(|m| m.id == "m3"));
    assert!(serde_json::to_vec(&latest).unwrap().len() < MAX_BYTES);
}
#[test]
fn append_revision_upserts_and_partial_tail_then_rewrite_expiry() {
    let fixture = Fixture::new(&record("human-1", None, "user", "hello".into()));
    let mut reader = ConversationReader::default();
    let first = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    fixture.append(&record(
        "assistant-1",
        Some("human-1"),
        "assistant",
        "first".into(),
    ));
    let second = reader
        .read(
            &fixture.binding,
            Window::Changes {
                cursor: first.changes_cursor.unwrap(),
            },
            Limits::default(),
        )
        .unwrap();
    assert_eq!(second.upserts[0].id, "assistant-1");
    fixture.append(&record(
        "assistant-1",
        Some("human-1"),
        "assistant",
        "revised".into(),
    ));
    fixture.append("{\"partial\"");
    let third = reader
        .read(
            &fixture.binding,
            Window::Changes {
                cursor: second.changes_cursor.unwrap(),
            },
            Limits::default(),
        )
        .unwrap();
    assert_eq!(third.upserts.len(), 1);
    assert_eq!(third.upserts[0].blocks[0].text.as_deref(), Some("revised"));
    let cursor = third.changes_cursor.unwrap();
    std::fs::write(
        &fixture.path,
        record("new", None, "user", "replacement".into()),
    )
    .unwrap();
    assert_eq!(
        reader
            .read(
                &fixture.binding,
                Window::Changes { cursor },
                Limits::default()
            )
            .unwrap_err()
            .code,
        "cursor_expired"
    );
}
#[test]
fn oversized_unicode_blocks_reconstruct_exactly_inside_whole_response_budget() {
    let text = "🌍 \n\t\"\\".repeat(150000);
    let fixture = Fixture::new(&record("large", None, "user", text.clone().into()));
    let mut reader = ConversationReader::default();
    let limits = Limits {
        limit: 1,
        max_bytes: 8192,
    };
    let page = reader
        .read(&fixture.binding, Window::Latest, limits)
        .unwrap();
    assert!(serde_json::to_vec(&page).unwrap().len() <= limits.max_bytes);
    let block = &page.messages[0].blocks[0];
    assert!(block.truncated);
    let mut actual = block.text.clone().unwrap();
    let mut cursor = block.block_cursor.clone();
    let mut slice = 1;
    while let Some(c) = cursor {
        let next = reader.read_block(&fixture.binding, &c, limits).unwrap();
        assert_eq!(next.slice_index, slice);
        assert!(serde_json::to_vec(&next).unwrap().len() <= limits.max_bytes);
        actual.push_str(&next.text);
        cursor = next.next_cursor;
        slice += 1;
    }
    assert_eq!(actual, text);
}
#[test]
fn mismatched_sessions_subagents_and_malformed_records_are_explicit() {
    let mismatch=Fixture::new("{\"type\":\"user\",\"sessionId\":\"another\",\"uuid\":\"x\",\"message\":{\"role\":\"user\",\"content\":\"foreign\"}}\n");
    let mut reader = ConversationReader::default();
    assert_eq!(
        reader
            .read(&mismatch.binding, Window::Latest, Limits::default())
            .unwrap_err()
            .code,
        "session_changed"
    );
    let malformed = Fixture::new("not-json\n");
    assert_eq!(
        reader
            .read(&malformed.binding, Window::Latest, Limits::default())
            .unwrap()
            .availability,
        "unsupported_schema"
    );
    std::fs::remove_file(&mismatch.path).unwrap();
    assert_eq!(
        reader
            .read(&mismatch.binding, Window::Latest, Limits::default())
            .unwrap()
            .availability,
        "source_missing"
    );
}

#[test]
fn current_branch_removes_abandoned_reply_and_expired_block_rejects_revision_change() {
    let fixture = Fixture::new(
        &(record("root", None, "user", "question".into())
            + &record("abandoned", Some("root"), "assistant", "old reply".into())),
    );
    let mut reader = ConversationReader::default();
    let first = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    fixture.append(&record(
        "branch",
        Some("root"),
        "assistant",
        "new reply".into(),
    ));
    let changes = reader
        .read(
            &fixture.binding,
            Window::Changes {
                cursor: first.changes_cursor.unwrap(),
            },
            Limits::default(),
        )
        .unwrap();
    assert!(changes.removed_message_ids.contains(&"abandoned".into()));
    assert_eq!(changes.upserts[0].id, "branch");
    let big = "unicode 🌍 ".repeat(20000);
    fixture.append(&record("branch", Some("root"), "assistant", big.into()));
    let page = reader
        .read(
            &fixture.binding,
            Window::Around {
                message_id: "branch".into(),
            },
            Limits {
                limit: 1,
                max_bytes: 8192,
            },
        )
        .unwrap();
    let cursor = page.messages[0].blocks[0].block_cursor.clone().unwrap();
    fixture.append(&record(
        "branch",
        Some("root"),
        "assistant",
        "replacement".into(),
    ));
    assert_eq!(
        reader
            .read_block(&fixture.binding, &cursor, Limits::default())
            .unwrap_err()
            .code,
        "cursor_expired"
    );
}
#[test]
fn session_generation_and_turn_index_cursor_are_isolated() {
    let fixture = Fixture::new(
        &(0..600)
            .map(|i| record(&format!("m{i}"), None, "user", "turn".into()))
            .collect::<String>(),
    );
    let mut reader = ConversationReader::default();
    let first = reader
        .read(
            &fixture.binding,
            Window::Turns { cursor: None },
            Limits {
                limit: 500,
                ..Limits::default()
            },
        )
        .unwrap();
    assert_eq!(first.turns.len(), 500);
    let second = reader
        .read(
            &fixture.binding,
            Window::Turns {
                cursor: first.turns_cursor.clone(),
            },
            Limits {
                limit: 500,
                ..Limits::default()
            },
        )
        .unwrap();
    assert_eq!(second.turns.len(), 100);
    let replacement = HistoryBinding::from_report(
        "herdr:claude",
        "claude",
        Some("synthetic-session"),
        fixture.path.to_str(),
    )
    .unwrap();
    assert_eq!(
        reader
            .read(
                &replacement,
                Window::Turns {
                    cursor: first.turns_cursor
                },
                Limits::default()
            )
            .unwrap_err()
            .code,
        "cursor_expired"
    );
}
#[test]
fn history_binding_generation_survives_repeated_reports_and_session_transition_invalidates_it() {
    let path = "/custom/long/root/synthetic-session.jsonl";
    let mut terminal = crate::terminal::TerminalState::new(
        crate::terminal::TerminalId::alloc(),
        "/synthetic".into(),
    );
    terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
        source: "herdr:claude".into(),
        agent: "claude".into(),
        session_ref: crate::agent_resume::AgentSessionRef::id("synthetic-session").unwrap(),
    });
    let binding = HistoryBinding::from_report(
        "herdr:claude",
        "claude",
        Some("synthetic-session"),
        Some(path),
    )
    .unwrap();
    terminal.set_history_binding(binding.clone());
    terminal.set_history_binding(
        HistoryBinding::from_report(
            "herdr:claude",
            "claude",
            Some("synthetic-session"),
            Some(path),
        )
        .unwrap(),
    );
    assert_eq!(
        terminal.active_history_binding().unwrap().generation,
        binding.generation
    );
    terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
        source: "herdr:claude".into(),
        agent: "claude".into(),
        session_ref: crate::agent_resume::AgentSessionRef::id("clear-session").unwrap(),
    });
    assert!(terminal.active_history_binding().is_none());
}

#[test]
fn tight_pages_keep_latest_and_around_target_and_preserve_navigation_progress() {
    let text: String = (0..50)
        .map(|i| {
            record(
                &format!("m{i}"),
                None,
                "user",
                ("quoted \"\\🌍".repeat(2000)).into(),
            )
        })
        .collect();
    let fixture = Fixture::new(&text);
    let mut reader = ConversationReader::default();
    let limits = Limits {
        limit: 50,
        max_bytes: 8192,
    };
    let latest = reader
        .read(&fixture.binding, Window::Latest, limits)
        .unwrap();
    assert_eq!(latest.messages.last().unwrap().id, "m49");
    assert!(serde_json::to_vec(&serde_json::json!({"id":"synthetic-request","result":{"type":"agent_conversation","binding":latest.binding,"revision":latest.revision,"availability":latest.availability,"completeness":latest.completeness,"messages":latest.messages,"turns":latest.turns,"before_cursor":latest.before_cursor,"after_cursor":latest.after_cursor,"turns_cursor":latest.turns_cursor,"search_cursor":latest.search_cursor,"changes_cursor":latest.changes_cursor,"upserts":latest.upserts,"removed_message_ids":latest.removed_message_ids,"match_message_ids":latest.match_message_ids}})).unwrap().len()<=limits.max_bytes);
    let around = reader
        .read(
            &fixture.binding,
            Window::Around {
                message_id: "m20".into(),
            },
            limits,
        )
        .unwrap();
    assert!(around.messages.iter().any(|m| m.id == "m20"));
    let before = around.before_cursor.clone().unwrap();
    let after = around.after_cursor.clone().unwrap();
    let previous = reader
        .read(
            &fixture.binding,
            Window::Before { cursor: before },
            Limits { limit: 1, ..limits },
        )
        .unwrap();
    assert_eq!(
        previous.messages[0].id,
        format!("m{}", around.messages.first().unwrap().order - 2)
    );
    let next = reader
        .read(
            &fixture.binding,
            Window::After { cursor: after },
            Limits { limit: 1, ..limits },
        )
        .unwrap();
    assert_eq!(
        next.messages[0].id,
        format!("m{}", around.messages.last().unwrap().order)
    );
}
#[test]
fn oversized_lines_and_unknown_provenance_do_not_hide_later_supported_messages() {
    let mut text = "x".repeat(16 * 1024 * 1024 + 1);
    text.push('\n');
    text.push_str(&record(
        "human",
        None,
        "user",
        "supported later message".into(),
    ));
    text.push_str("{\"type\":\"user\",\"uuid\":\"synthetic\",\"sessionId\":\"synthetic-session\",\"isSynthetic\":true,\"message\":{\"role\":\"user\",\"content\":\"automatic notice\"}}\n");
    let fixture = Fixture::new(&text);
    let mut reader = ConversationReader::default();
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(page.completeness, "partial_unsupported_records");
    assert_eq!(page.turns.len(), 1);
    assert_eq!(page.turns[0].message_id, "human");
}
#[test]
fn change_journal_expiry_is_explicit() {
    let fixture = Fixture::new(&record("root", None, "user", "first".into()));
    let mut reader = ConversationReader::default();
    let first = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    fixture.append(
        &(0..1001)
            .map(|i| record(&format!("new-{i}"), None, "assistant", "reply".into()))
            .collect::<String>(),
    );
    assert_eq!(
        reader
            .read(
                &fixture.binding,
                Window::Changes {
                    cursor: first.changes_cursor.unwrap()
                },
                Limits::default()
            )
            .unwrap_err()
            .code,
        "cursor_expired"
    );
}

#[test]
fn compaction_retains_available_earlier_turns_and_branch_ancestors_can_be_reselected() {
    let fixture = Fixture::new(
        &(record("root", None, "user", "question".into())
            + &record("old", Some("root"), "assistant", "old reply".into())),
    );
    let mut reader = ConversationReader::default();
    reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    fixture.append(&record(
        "branch",
        Some("root"),
        "assistant",
        "alternate reply".into(),
    ));
    reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    fixture.append(&record(
        "continued-old",
        Some("old"),
        "user",
        "follow original branch".into(),
    ));
    let reselected = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(
        reselected
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["root", "old", "continued-old"]
    );
    fixture.append("{\"type\":\"user\",\"uuid\":\"compact\",\"sessionId\":\"synthetic-session\",\"parentUuid\":null,\"isCompactSummary\":true,\"message\":{\"role\":\"user\",\"content\":\"Saved compaction notice\"}}\n");
    fixture.append(&record(
        "after-compact",
        Some("compact"),
        "assistant",
        "continued reply".into(),
    ));
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(
        page.turns
            .iter()
            .map(|t| t.message_id.as_str())
            .collect::<Vec<_>>(),
        vec!["root", "continued-old"]
    );
    assert!(page
        .messages
        .iter()
        .any(|m| m.id == "compact" && m.origin == "system"));
}
#[test]
fn replaced_file_and_nonregular_source_expire_without_blocking() {
    let fixture = Fixture::new(&record("root", None, "user", "question".into()));
    let mut reader = ConversationReader::default();
    let first = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    let replacement = fixture.path.with_extension("replacement");
    std::fs::write(
        &replacement,
        record("root", None, "user", "question".into()),
    )
    .unwrap();
    std::fs::rename(&replacement, &fixture.path).unwrap();
    assert_eq!(
        reader
            .read(
                &fixture.binding,
                Window::Changes {
                    cursor: first.changes_cursor.unwrap()
                },
                Limits::default()
            )
            .unwrap_err()
            .code,
        "cursor_expired"
    );
    let mut directory = fixture.binding.clone();
    directory.transcript_path = std::env::temp_dir();
    assert_eq!(
        reader
            .read(&directory, Window::Latest, Limits::default())
            .unwrap()
            .availability,
        "unsupported_schema"
    );
}

#[test]
fn tiny_budgets_are_typed_errors_for_every_window_and_block_without_poisoning_reads() {
    let fixture = Fixture::new(
        &(record("root", None, "user", "needle ".repeat(10000).into())
            + &record("reply", Some("root"), "assistant", "reply needle".into())),
    );
    let mut reader = ConversationReader::default();
    let first = reader
        .read(
            &fixture.binding,
            Window::Latest,
            Limits {
                limit: 1,
                ..Limits::default()
            },
        )
        .unwrap();
    let older = reader
        .read(
            &fixture.binding,
            Window::Before {
                cursor: first.before_cursor.clone().unwrap(),
            },
            Limits {
                limit: 1,
                ..Limits::default()
            },
        )
        .unwrap();
    let long = reader
        .read(
            &fixture.binding,
            Window::Around {
                message_id: "root".into(),
            },
            Limits {
                limit: 1,
                max_bytes: 4096,
            },
        )
        .unwrap();
    let block = long.messages[0].blocks[0].block_cursor.clone().unwrap();
    fixture.append(&record(
        "next",
        Some("reply"),
        "assistant",
        "next needle".into(),
    ));
    for window in [
        Window::Latest,
        Window::Before {
            cursor: first.before_cursor.unwrap(),
        },
        Window::After {
            cursor: older.after_cursor.unwrap(),
        },
        Window::Around {
            message_id: "root".into(),
        },
        Window::Search {
            query: "needle".into(),
            cursor: None,
        },
        Window::Turns { cursor: None },
        Window::Changes {
            cursor: first.changes_cursor.unwrap(),
        },
    ] {
        let error = reader
            .read(
                &fixture.binding,
                window,
                Limits {
                    limit: 1,
                    max_bytes: 1,
                },
            )
            .unwrap_err();
        assert_eq!(error.code, "response_too_large");
        assert_eq!(
            reader
                .read(&fixture.binding, Window::Latest, Limits::default())
                .unwrap()
                .availability,
            "ready"
        );
    }
    assert_eq!(
        reader
            .read_block(
                &fixture.binding,
                &block,
                Limits {
                    limit: 1,
                    max_bytes: 1
                }
            )
            .unwrap_err()
            .code,
        "response_too_large"
    );
    assert!(!reader
        .read_block(&fixture.binding, &block, Limits::default())
        .unwrap()
        .text
        .is_empty());
}

#[test]
fn nested_tool_attachments_omit_encoded_bodies_and_keep_visible_arguments() {
    let fixture = Fixture::new(&record(
        "tool-call",
        None,
        "assistant",
        serde_json::json!([
            {"type":"tool_use","id":"call","name":"SyntheticTool","input":{"path":"synthetic.png","attachment":{"type":"image","source":{"type":"base64","data":"SECRET_NESTED_BASE64"}}}},
            {"type":"tool_result","content":[{"type":"document","source":{"type":"base64","data":"SECRET_DOCUMENT_BASE64"}},{"type":"text","text":"visible result"}]}
        ]),
    ));
    let mut reader = ConversationReader::default();
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    let json = serde_json::to_string(&page).unwrap();
    assert!(!json.contains("SECRET_NESTED_BASE64"));
    assert!(!json.contains("SECRET_DOCUMENT_BASE64"));
    assert!(json.contains("synthetic.png"));
    assert!(json.contains("visible result"));
    assert!(json.contains("Document attachment"));
}

#[test]
fn turn_index_budget_cannot_return_an_empty_nonadvancing_cursor() {
    let fixture = Fixture::new(&record(
        &"long-id-".repeat(60),
        None,
        "user",
        "\u{1}".repeat(1000).into(),
    ));
    let mut reader = ConversationReader::default();
    let full = reader
        .read(
            &fixture.binding,
            Window::Turns { cursor: None },
            Limits::default(),
        )
        .unwrap();
    let mut empty = full.clone();
    empty.turns.clear();
    empty.turns_cursor = None;
    let budget = serde_json::to_vec(&empty).unwrap().len() + 512;
    assert!(serde_json::to_vec(&full).unwrap().len() > budget);
    let result = reader.read(
        &fixture.binding,
        Window::Turns { cursor: None },
        Limits {
            limit: 1,
            max_bytes: budget,
        },
    );
    assert_eq!(result.unwrap_err().code, "response_too_large");
    assert_eq!(
        reader
            .read(
                &fixture.binding,
                Window::Turns { cursor: None },
                Limits::default()
            )
            .unwrap()
            .turns
            .len(),
        1
    );
}

#[test]
fn conversation_jump_source_rejects_revisions_duplicates_attachments_and_evicted_guards() {
    let fixture = Fixture::new(&record(
        "root",
        None,
        "user",
        "Exact unique 🌍 prompt".into(),
    ));
    fixture.append(&record(
        "context",
        Some("root"),
        "assistant",
        "plain reply".into(),
    ));
    let mut reader = ConversationReader::default();
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    let revision = page.messages[0].message_revision.clone();
    let prompt = reader
        .exact_human_prompt(&fixture.binding, "root", &revision)
        .unwrap();
    assert_eq!(prompt.text, "Exact unique 🌍 prompt");
    prompt.guard.validate().unwrap();
    assert_eq!(
        reader
            .exact_human_prompt(&fixture.binding, "root", "stale")
            .err()
            .unwrap()
            .code,
        "message_changed"
    );
    fixture.append(&record(
        "again",
        Some("root"),
        "user",
        "Exact unique 🌍 prompt".into(),
    ));
    assert_eq!(
        reader
            .exact_human_prompt(&fixture.binding, "root", &revision)
            .err()
            .unwrap()
            .code,
        "jump_ambiguous"
    );
    assert_eq!(prompt.guard.validate().unwrap_err().code, "source_changed");
    let attachment = Fixture::new(&record(
        "image",
        None,
        "user",
        serde_json::json!([{"type":"image","source":{"type":"base64","data":"SYNTHETIC_BODY"}}]),
    ));
    let page = reader
        .read(&attachment.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(
        reader
            .exact_human_prompt(
                &attachment.binding,
                "image",
                &page.messages[0].message_revision
            )
            .err()
            .unwrap()
            .code,
        "jump_unavailable"
    );
    let unique = Fixture::new(&record("unique", None, "user", "distinct prompt".into()));
    unique.append(&record(
        "context",
        Some("unique"),
        "assistant",
        "plain reply".into(),
    ));
    let page = reader
        .read(&unique.binding, Window::Latest, Limits::default())
        .unwrap();
    let guarded = reader
        .exact_human_prompt(
            &unique.binding,
            "unique",
            &page.messages[0].message_revision,
        )
        .unwrap();
    let mut additional = Vec::new();
    for i in 0..4 {
        let new = Fixture::new(&record(
            &format!("other-{i}"),
            None,
            "user",
            format!("other prompt {i}").into(),
        ));
        reader
            .read(&new.binding, Window::Latest, Limits::default())
            .unwrap();
        additional.push(new);
    }
    assert_eq!(guarded.guard.validate().unwrap_err().code, "source_changed");
}
#[test]
fn conversation_jump_source_rewrite_after_read_cannot_pass_worker_validation() {
    let fixture = Fixture::new(&record("root", None, "user", "original".into()));
    fixture.append(&record(
        "context",
        Some("root"),
        "assistant",
        "plain reply".into(),
    ));
    let mut reader = ConversationReader::default();
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    let prompt = reader
        .exact_human_prompt(&fixture.binding, "root", &page.messages[0].message_revision)
        .unwrap();
    std::fs::write(
        &fixture.path,
        record("root", None, "user", "replacement".into()),
    )
    .unwrap();
    assert_eq!(
        reader.validate_jump_source(&prompt).unwrap_err().code,
        "source_changed"
    );
    assert_eq!(prompt.guard.validate().unwrap_err().code, "source_changed");
}

#[test]
fn conversation_jump_refuses_pending_or_unsupported_records_and_invalidates_completeness_changes() {
    let fixture = Fixture::new(&record("root", None, "user", "original".into()));
    fixture.append(&record(
        "context",
        Some("root"),
        "assistant",
        "plain reply".into(),
    ));
    let mut reader = ConversationReader::default();
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    let prompt = reader
        .exact_human_prompt(&fixture.binding, "root", &page.messages[0].message_revision)
        .unwrap();
    fixture.append("{\"pending\"");
    assert_eq!(
        reader.validate_jump_source(&prompt).unwrap_err().code,
        "source_changed"
    );
    assert_eq!(
        reader
            .exact_human_prompt(&fixture.binding, "root", &page.messages[0].message_revision)
            .err()
            .unwrap()
            .code,
        "jump_unavailable"
    );
    std::fs::write(
        &fixture.path,
        record("root", None, "user", "original".into()) + "unsupported-complete-record\n",
    )
    .unwrap();
    assert_eq!(
        reader
            .exact_human_prompt(&fixture.binding, "root", &page.messages[0].message_revision)
            .err()
            .unwrap()
            .code,
        "jump_unavailable"
    );
}

#[test]
fn conversation_jump_requires_supported_exact_assistant_context_for_selected_turn() {
    for reply in [
        None,
        Some(serde_json::json!("**Markdown reply**")),
        Some(serde_json::json!([{"type":"tool_use","id":"tool","name":"Read","input":{}}])),
    ] {
        let fixture = Fixture::new(&record("root", None, "user", "plain prompt".into()));
        if let Some(reply) = reply {
            fixture.append(&record("reply", Some("root"), "assistant", reply));
        }
        let mut reader = ConversationReader::default();
        let page = reader
            .read(&fixture.binding, Window::Latest, Limits::default())
            .unwrap();
        assert_eq!(
            reader
                .exact_human_prompt(&fixture.binding, "root", &page.messages[0].message_revision)
                .err()
                .unwrap()
                .code,
            "jump_unavailable"
        );
    }
    let fixture = Fixture::new(
        &(record("root", None, "user", "plain prompt".into())
            + &record(
                "reply",
                Some("root"),
                "assistant",
                "Exact visible Unicode 界 reply\nnext line".into(),
            )),
    );
    let mut reader = ConversationReader::default();
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(
        reader
            .exact_human_prompt(&fixture.binding, "root", &page.messages[0].message_revision)
            .unwrap()
            .assistant_line,
        "Exact visible Unicode 界 reply"
    );
}
