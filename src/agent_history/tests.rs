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
fn attachment_record(id: &str, parent: Option<&str>) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type":"attachment", "uuid":id, "parentUuid":parent,
            "sessionId":"synthetic-session", "isSidechain":false,
            "attachment":{"type":"system-reminder", "content":"HIDDEN_ATTACHMENT_BODY",
                "rendered":"HIDDEN_RENDERED", "data":"HIDDEN_BASE64"}
        })
    )
}
fn changed_record(line: &str, key: &str, value: serde_json::Value) -> String {
    let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
    record[key] = value;
    format!("{record}\n")
}

fn system_event_record(id: &str, parent: Option<&str>, subtype: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type":"system", "subtype":subtype, "uuid":id, "parentUuid":parent,
            "sessionId":"synthetic-session", "isSidechain":false,
            "hookInfos":[{"command":"HIDDEN_HOOK_COMMAND", "output":"HIDDEN_HOOK_INFO"}],
            "hookAdditionalContext":"HIDDEN_HOOK_CONTEXT", "durationMs":10
        })
    )
}

#[test]
fn content_free_system_events_preserve_oldest_turn_paging_search_and_changes() {
    let fixture = Fixture::new(
        &(record("u1", None, "user", "first question".into())
            + &record("r1", Some("u1"), "assistant", "first reply".into())),
    );
    let mut reader = ConversationReader::default();
    let first = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    fixture.append(
        &(system_event_record("stop", Some("r1"), "stop_hook_summary")
            + &system_event_record("duration", Some("stop"), "turn_duration")
            + &record("u2", Some("duration"), "user", "second question".into())
            + &record("r2", Some("u2"), "assistant", "second reply".into())),
    );
    let limits = Limits {
        limit: 2,
        ..Limits::default()
    };
    let latest = reader
        .read(&fixture.binding, Window::Latest, limits)
        .unwrap();
    assert_eq!(latest.completeness, "source_available");
    assert_eq!(
        latest
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["u2", "r2"]
    );
    assert_eq!(
        latest
            .turns
            .iter()
            .map(|turn| turn.message_id.as_str())
            .collect::<Vec<_>>(),
        ["u1", "u2"]
    );
    let oldest = reader
        .read(
            &fixture.binding,
            Window::Before {
                cursor: latest.before_cursor.unwrap(),
            },
            limits,
        )
        .unwrap();
    assert_eq!(
        oldest
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["u1", "r1"]
    );
    assert!(oldest
        .messages
        .iter()
        .all(|message| message.turn_id == "turn:u1"));
    let around = reader
        .read(
            &fixture.binding,
            Window::Around {
                message_id: "u1".into(),
            },
            limits,
        )
        .unwrap();
    assert_eq!(around.messages[0].id, "u1");
    let search = reader
        .read(
            &fixture.binding,
            Window::Search {
                query: "first question".into(),
                cursor: None,
            },
            limits,
        )
        .unwrap();
    assert_eq!(search.match_message_ids, ["u1"]);
    let changes = reader
        .read(
            &fixture.binding,
            Window::Changes {
                cursor: first.changes_cursor.unwrap(),
            },
            Limits::default(),
        )
        .unwrap();
    assert_eq!(
        changes
            .upserts
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["u2", "r2"]
    );
    assert!(!serde_json::to_string(&changes).unwrap().contains("HIDDEN_"));
    assert!(reader
        .read(
            &fixture.binding,
            Window::Search {
                query: "HIDDEN_".into(),
                cursor: None
            },
            Limits::default()
        )
        .unwrap()
        .match_message_ids
        .is_empty());
    let prompt = reader
        .exact_human_prompt(&fixture.binding, "u1", &oldest.messages[0].message_revision)
        .unwrap();
    assert_eq!(prompt.assistant_line, "first reply");
}

#[test]
fn observed_nonmessage_metadata_shapes_never_expose_payloads_or_fabricate_turns() {
    let records = [
        serde_json::json!({"type":"file-history-delta", "backup":{"path":"HIDDEN_BACKUP"}, "messageId":"u1", "snapshotMessageId":"HIDDEN_SNAPSHOT", "timestamp":"generated", "trackingPath":"HIDDEN_TRACKING_PATH"}),
        serde_json::json!({"type":"ai-title", "aiTitle":"HIDDEN_AI_TITLE", "sessionId":"synthetic-session"}),
        serde_json::json!({"type":"pr-link", "prNumber":1, "prRepository":"HIDDEN_PR_REPOSITORY", "prUrl":"HIDDEN_PR_URL", "sessionId":"synthetic-session", "timestamp":"generated"}),
        serde_json::json!({"type":"cost-state", "modelUsage":{"HIDDEN_MODEL_ACCOUNT":{}}, "sessionId":"synthetic-session", "startTime":0, "totalAPIDuration":1, "totalAPIDurationWithoutRetries":1, "totalCostUSD":0, "totalDuration":1, "totalLinesAdded":1, "totalLinesRemoved":0, "totalToolDuration":1, "hasUnknownModelCost":false}),
        serde_json::json!({"type":"frame-link", "artifactCount":1, "frameUrl":"HIDDEN_FRAME_URL", "path":"HIDDEN_FRAME_PATH", "sessionId":"synthetic-session", "timestamp":"generated", "title":"HIDDEN_FRAME_TITLE"}),
        serde_json::json!({"type":"artifact-comment-monitor", "artifacts":["HIDDEN_COMMENT_BODY"], "sessionId":"synthetic-session", "v":1}),
        serde_json::json!({"type":"artifact-autoreact-ledger", "accountUuid":"HIDDEN_ACCOUNT_UUID", "artifacts":["HIDDEN_ARTIFACT"], "sessionId":"synthetic-session", "v":1}),
        serde_json::json!({"type":"history-suppression", "cause":"HIDDEN_SUPPRESSION_CAUSE", "sessionId":"synthetic-session", "ts":1, "vetoedAgainstAccountUuid":"HIDDEN_VETO_ACCOUNT"}),
    ];
    let fixture = Fixture::new(&record("u1", None, "user", "question".into()));
    let mut reader = ConversationReader::default();
    let first = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    for metadata in records {
        let line = format!("{metadata}\n");
        assert!(claude::normalize(line.as_bytes(), "synthetic-session")
            .unwrap()
            .is_none());
        fixture.append(&line);
    }
    fixture.append(&record("r1", Some("u1"), "assistant", "reply".into()));
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(page.completeness, "source_available");
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.turns.len(), 1);
    assert!(!serde_json::to_string(&page).unwrap().contains("HIDDEN_"));
    let changes = reader
        .read(
            &fixture.binding,
            Window::Changes {
                cursor: first.changes_cursor.unwrap(),
            },
            Limits::default(),
        )
        .unwrap();
    assert_eq!(changes.upserts.len(), 1);
    assert_eq!(changes.upserts[0].id, "r1");
    assert!(!serde_json::to_string(&changes).unwrap().contains("HIDDEN_"));
    assert!(reader
        .read(
            &fixture.binding,
            Window::Search {
                query: "HIDDEN_".into(),
                cursor: None
            },
            Limits::default()
        )
        .unwrap()
        .match_message_ids
        .is_empty());
}

#[test]
fn system_content_null_falls_back_but_invalid_content_is_not_hidden() {
    for subtype in ["stop_hook_summary", "turn_duration"] {
        let event = system_event_record("system", None, subtype);
        assert!(claude::normalize(event.as_bytes(), "synthetic-session")
            .unwrap()
            .is_none());
        let event = changed_record(&event, "content", serde_json::Value::Null);
        let nested = changed_record(
            &event,
            "message",
            serde_json::json!({"role":"system", "content":[{"type":"text", "text":"visible diagnostic"}]}),
        );
        let normalized = claude::normalize(nested.as_bytes(), "synthetic-session")
            .unwrap()
            .unwrap();
        assert_eq!(normalized.origin, "system");
        assert_eq!(
            normalized.blocks[0].text.as_deref(),
            Some("visible diagnostic")
        );
        assert!(!serde_json::to_string(&normalized)
            .unwrap()
            .contains("HIDDEN_"));
        let bearing = changed_record(&event, "content", "visible system text".into());
        assert_eq!(
            claude::normalize(bearing.as_bytes(), "synthetic-session")
                .unwrap()
                .unwrap()
                .blocks[0]
                .text
                .as_deref(),
            Some("visible system text")
        );
        for invalid in [
            true.into(),
            3.into(),
            serde_json::json!({"bad":"HIDDEN_CONTENT"}),
        ] {
            assert_eq!(
                claude::normalize(
                    changed_record(&nested, "content", invalid).as_bytes(),
                    "synthetic-session"
                )
                .unwrap_err()
                .code,
                "unsupported_schema"
            );
        }
        assert_eq!(
            claude::normalize(
                changed_record(&event, "message", "HIDDEN_INVALID_MESSAGE".into()).as_bytes(),
                "synthetic-session"
            )
            .unwrap_err()
            .code,
            "unsupported_schema"
        );
    }
    let unknown = system_event_record("unknown", None, "unknown_system_event");
    assert_eq!(
        claude::normalize(unknown.as_bytes(), "synthetic-session")
            .unwrap_err()
            .code,
        "unsupported_schema"
    );
}

#[test]
fn content_free_system_links_require_exact_valid_mainchain_identity() {
    for subtype in ["stop_hook_summary", "turn_duration"] {
        let event = system_event_record("bridge", Some("u1"), subtype);
        assert_eq!(
            claude::normalize(
                changed_record(&event, "sessionId", "foreign".into()).as_bytes(),
                "synthetic-session"
            )
            .unwrap_err()
            .code,
            "session_changed"
        );
        for (key, invalid) in [
            ("uuid", "x".repeat(513).into()),
            ("uuid", "bad\nuuid".into()),
            ("uuid", serde_json::Value::Null),
            ("parentUuid", "x".repeat(513).into()),
            ("parentUuid", serde_json::json!(["u1"])),
            ("isSidechain", serde_json::Value::Null),
        ] {
            let malformed = changed_record(&event, key, invalid);
            assert_eq!(
                claude::normalize(malformed.as_bytes(), "synthetic-session")
                    .unwrap_err()
                    .code,
                "unsupported_schema"
            );
            let fixture =
                Fixture::new(&(record("u1", None, "user", "question".into()) + &malformed));
            assert_eq!(
                ConversationReader::default()
                    .read(&fixture.binding, Window::Latest, Limits::default())
                    .unwrap()
                    .completeness,
                "partial_unsupported_records"
            );
        }
        for ignored in [
            changed_record(&event, "isSidechain", true.into()),
            changed_record(&event, "agentId", "subagent".into()),
        ] {
            let fixture = Fixture::new(
                &(record("u1", None, "user", "question".into())
                    + &ignored
                    + &record("r1", Some("bridge"), "assistant", "reply".into())
                    + &record("r2", Some("r1"), "assistant", "final".into())),
            );
            let page = ConversationReader::default()
                .read(&fixture.binding, Window::Latest, Limits::default())
                .unwrap();
            assert!(page.turns.is_empty());
            assert!(page.messages.iter().all(|message| message.id != "u1"));
            assert!(!serde_json::to_string(&page).unwrap().contains("HIDDEN_"));
        }
    }
}

#[test]
fn known_metadata_ancestry_requires_complete_envelope_and_replays_turn_reparenting() {
    let link = changed_record(
        &system_event_record("bridge", Some("u1"), "stop_hook_summary"),
        "type",
        "ai-title".into(),
    );
    let fixture = Fixture::new(
        &(record("u1", None, "user", "first question".into())
            + &record("u2", None, "user", "second question".into())
            + &link
            + &record("r1", Some("bridge"), "assistant", "reply".into())),
    );
    let mut reader = ConversationReader::default();
    let first = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(first.turns[0].message_id, "u1");
    let original_prompt = reader
        .exact_human_prompt(&fixture.binding, "u1", &first.messages[0].message_revision)
        .unwrap();
    fixture.append(&changed_record(&link, "parentUuid", "u2".into()));
    let changes = reader
        .read(
            &fixture.binding,
            Window::Changes {
                cursor: first.changes_cursor.unwrap(),
            },
            Limits::default(),
        )
        .unwrap();
    assert_eq!(changes.removed_message_ids, ["u1"]);
    let replayed = changes
        .upserts
        .iter()
        .find(|message| message.id == "r1")
        .unwrap();
    assert_eq!(replayed.turn_id, "turn:u2");
    assert_eq!(
        replayed.message_revision,
        first.messages[1].message_revision
    );
    assert!(!serde_json::to_string(&changes).unwrap().contains("HIDDEN_"));
    assert!(reader.validate_jump_source(&original_prompt).is_err());

    for missing in ["isSidechain", "sessionId"] {
        let mut incomplete: serde_json::Value = serde_json::from_str(&link).unwrap();
        incomplete.as_object_mut().unwrap().remove(missing);
        let fixture = Fixture::new(
            &(record("u1", None, "user", "question".into())
                + &format!("{incomplete}\n")
                + &record("r1", Some("bridge"), "assistant", "reply".into())
                + &record("r2", Some("r1"), "assistant", "final".into())),
        );
        let page = reader
            .read(&fixture.binding, Window::Latest, Limits::default())
            .unwrap();
        assert!(page.turns.is_empty());
        assert_eq!(page.completeness, "source_available");
    }
    assert_eq!(
        claude::normalize(
            changed_record(&link, "sessionId", "foreign".into()).as_bytes(),
            "synthetic-session"
        )
        .unwrap_err()
        .code,
        "session_changed"
    );
    assert_eq!(
        claude::normalize(
            changed_record(&link, "uuid", "bad\nuuid".into()).as_bytes(),
            "synthetic-session"
        )
        .unwrap_err()
        .code,
        "unsupported_schema"
    );
}

#[test]
fn repeated_hidden_system_events_keep_visible_revision_but_invalidate_stale_jump() {
    let fixture = Fixture::new(
        &(record("u1", None, "user", "question".into())
            + &record("r1", Some("u1"), "assistant", "reply".into())),
    );
    let mut reader = ConversationReader::default();
    let first = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    let prompt = reader
        .exact_human_prompt(&fixture.binding, "u1", &first.messages[0].message_revision)
        .unwrap();
    let event = system_event_record("stop", Some("r1"), "stop_hook_summary");
    fixture.append(&(event.clone() + &event));
    let changes = reader
        .read(
            &fixture.binding,
            Window::Changes {
                cursor: first.changes_cursor.unwrap(),
            },
            Limits::default(),
        )
        .unwrap();
    assert!(changes.upserts.is_empty());
    assert!(changes.removed_message_ids.is_empty());
    assert_eq!(changes.revision, first.revision);
    assert!(reader.validate_jump_source(&prompt).is_err());
}

#[test]
fn hidden_system_cycles_and_visible_id_collisions_remain_partial_without_payload_leaks() {
    let root = record("u1", Some("stop"), "user", "question".into());
    let stop = system_event_record("stop", Some("stop"), "stop_hook_summary");
    let fixture =
        Fixture::new(&(root + &stop + &record("r1", Some("u1"), "assistant", "reply".into())));
    let page = ConversationReader::default()
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(page.completeness, "partial_unsupported_records");
    assert!(!serde_json::to_string(&page).unwrap().contains("HIDDEN_"));
    let root = record("u1", None, "user", "question".into());
    let colliding = system_event_record("u1", None, "turn_duration");
    for text in [root.clone() + &colliding, colliding + &root] {
        let fixture = Fixture::new(&text);
        let page = ConversationReader::default()
            .read(&fixture.binding, Window::Latest, Limits::default())
            .unwrap();
        assert_eq!(page.completeness, "partial_unsupported_records");
        assert_eq!(page.messages[0].blocks[0].text.as_deref(), Some("question"));
        assert!(!serde_json::to_string(&page).unwrap().contains("HIDDEN_"));
    }
}

#[test]
fn nested_tool_references_render_only_inert_names_and_preserve_surrounding_text() {
    let fixture = Fixture::new(&record(
        "result",
        None,
        "user",
        serde_json::json!([{"type":"tool_result", "tool_use_id":"generated-tool", "content":[
            {"type":"text", "text":"before reference"},
            {"type":"tool_reference", "tool_name":"inspect_repo", "text":"HIDDEN_REFERENCE_TEXT", "data":"HIDDEN_REFERENCE_DATA"},
            {"type":"tool_reference", "extra":"HIDDEN_REFERENCE_EXTRA"},
            {"type":"unknown_future_block", "text":"HIDDEN_UNKNOWN_PROSE"},
            {"type":"text", "text":"after reference"}
        ]}]),
    ));
    let page = ConversationReader::default()
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(page.messages[0].origin, "tool");
    assert!(page.turns.is_empty());
    assert_eq!(
        page.messages[0].blocks[0].text.as_deref(),
        Some("before reference\nTool reference: inspect_repo\nTool reference\nafter reference")
    );
    assert!(!serde_json::to_string(&page).unwrap().contains("HIDDEN_"));
}

#[test]
fn hidden_attachment_links_preserve_human_turn_search_around_and_jump() {
    let root = changed_record(
        &record("u1", None, "user", "synthetic human question".into()),
        "origin",
        serde_json::json!({"kind":"human"}),
    );
    let mut text = root + &attachment_record("a1", Some("u1"));
    text.push_str(&attachment_record("a2", Some("a1")));
    text.push_str(&record(
        "r1",
        Some("a2"),
        "assistant",
        "supported plain reply".into(),
    ));
    text.push_str(&record("r2", Some("r1"), "assistant", "final reply".into()));
    for kind in ["mode", "permission-mode", "atis-latch", "bridge-session"] {
        text.push_str(&format!(
            "{}\n",
            serde_json::json!({"type":kind,"content":"HIDDEN_METADATA_BODY"})
        ));
    }
    let fixture = Fixture::new(&text);
    let mut reader = ConversationReader::default();
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(page.completeness, "source_available");
    assert_eq!(
        page.messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["u1", "r1", "r2"]
    );
    assert_eq!(page.turns.len(), 1);
    assert_eq!(page.turns[0].message_id, "u1");
    assert!(page
        .messages
        .iter()
        .all(|message| message.turn_id == "turn:u1"));
    let encoded = serde_json::to_string(&page).unwrap();
    assert!(!encoded.contains("HIDDEN_"));
    let search = reader
        .read(
            &fixture.binding,
            Window::Search {
                query: "synthetic human".into(),
                cursor: None,
            },
            Limits::default(),
        )
        .unwrap();
    assert_eq!(search.match_message_ids, ["u1"]);
    let hidden_search = reader
        .read(
            &fixture.binding,
            Window::Search {
                query: "HIDDEN_".into(),
                cursor: None,
            },
            Limits::default(),
        )
        .unwrap();
    assert!(hidden_search.match_message_ids.is_empty());
    let around = reader
        .read(
            &fixture.binding,
            Window::Around {
                message_id: "u1".into(),
            },
            Limits::default(),
        )
        .unwrap();
    assert_eq!(around.messages[0].id, "u1");
    let prompt = reader
        .exact_human_prompt(&fixture.binding, "u1", &page.messages[0].message_revision)
        .unwrap();
    assert_eq!(prompt.text, "synthetic human question");
    assert_eq!(prompt.assistant_line, "supported plain reply");
    assert!(claude::normalize(
        attachment_record("a1", Some("u1")).as_bytes(),
        "synthetic-session"
    )
    .unwrap()
    .is_none());
}

#[test]
fn hidden_attachment_links_follow_appended_branch_replacements() {
    let fixture = Fixture::new(
        &(record("root", None, "user", "question".into())
            + &record("other-root", None, "user", "another question".into())
            + &attachment_record("old-link", Some("root"))
            + &record(
                "abandoned",
                Some("old-link"),
                "assistant",
                "abandoned reply".into(),
            )),
    );
    let mut reader = ConversationReader::default();
    let first = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    fixture.append(
        &(attachment_record("new-link", Some("root"))
            + &attachment_record("new-link-2", Some("new-link"))
            + &record(
                "selected",
                Some("new-link-2"),
                "assistant",
                "selected reply".into(),
            )),
    );
    let changes = reader
        .read(
            &fixture.binding,
            Window::Changes {
                cursor: first.changes_cursor.unwrap(),
            },
            Limits::default(),
        )
        .unwrap();
    assert_eq!(changes.removed_message_ids, ["abandoned"]);
    assert_eq!(changes.upserts.len(), 1);
    assert_eq!(changes.upserts[0].id, "selected");
    assert_eq!(changes.upserts[0].turn_id, "turn:root");
    fixture.append(&record(
        "selected",
        Some("new-link-2"),
        "assistant",
        "replaced reply".into(),
    ));
    let updated = reader
        .read(
            &fixture.binding,
            Window::Changes {
                cursor: changes.changes_cursor.unwrap(),
            },
            Limits::default(),
        )
        .unwrap();
    assert_eq!(updated.upserts.len(), 1);
    assert_eq!(
        updated.upserts[0].blocks[0].text.as_deref(),
        Some("replaced reply")
    );
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    let old_prompt = reader
        .exact_human_prompt(&fixture.binding, "root", &page.messages[0].message_revision)
        .unwrap();
    fixture.append(&attachment_record("new-link-2", Some("other-root")));
    let relinked = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(
        relinked
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["other-root", "selected"]
    );
    assert!(relinked
        .messages
        .iter()
        .all(|message| message.turn_id == "turn:other-root"));
    assert!(reader.validate_jump_source(&old_prompt).is_err());
    fixture.append(&record(
        "reselected",
        Some("abandoned"),
        "assistant",
        "original branch again".into(),
    ));
    let original = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(
        original
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["root", "abandoned", "reselected"]
    );
    assert_eq!(original.turns.len(), 1);
}

#[test]
fn rewrite_clears_hidden_attachment_links_and_invalidates_saved_jump() {
    let fixture = Fixture::new(
        &(record("root", None, "user", "old question".into())
            + &attachment_record("bridge", Some("root"))
            + &record("reply", Some("bridge"), "assistant", "old reply".into())),
    );
    let mut reader = ConversationReader::default();
    let first = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    let prompt = reader
        .exact_human_prompt(
            &fixture.binding,
            "root",
            &first.messages[0].message_revision,
        )
        .unwrap();
    std::fs::write(
        &fixture.path,
        record("root", None, "user", "new question".into())
            + &record("new-reply", Some("bridge"), "assistant", "new reply".into())
            + &record(
                "new-reply-2",
                Some("new-reply"),
                "assistant",
                "final reply".into(),
            ),
    )
    .unwrap();
    let current = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(
        current
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["new-reply", "new-reply-2"]
    );
    assert!(current.turns.is_empty());
    assert!(reader.validate_jump_source(&prompt).is_err());
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
fn attachment_ancestry_cycles_are_bounded_and_disable_terminal_jump() {
    let fixture = Fixture::new(
        &(record("root", Some("a1"), "user", "question".into())
            + &attachment_record("a1", Some("a2"))
            + &attachment_record("a2", Some("a1"))
            + &record("reply", Some("root"), "assistant", "reply".into())),
    );
    let mut reader = ConversationReader::default();
    let page = reader
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(page.completeness, "partial_unsupported_records");
    assert!(!serde_json::to_string(&page).unwrap().contains("HIDDEN_"));
    assert!(reader
        .exact_human_prompt(&fixture.binding, "root", &page.messages[0].message_revision)
        .is_err());
}

#[test]
fn attachment_links_reject_foreign_sessions_and_never_follow_sidechains_or_agents() {
    let foreign = changed_record(
        &attachment_record("bridge", Some("root")),
        "sessionId",
        "foreign-session".into(),
    );
    let fixture = Fixture::new(&(record("root", None, "user", "question".into()) + &foreign));
    let mut reader = ConversationReader::default();
    assert_eq!(
        reader
            .read(&fixture.binding, Window::Latest, Limits::default())
            .unwrap_err()
            .code,
        "session_changed"
    );
    for ignored in [
        changed_record(
            &attachment_record("bridge", Some("root")),
            "isSidechain",
            true.into(),
        ),
        changed_record(
            &attachment_record("bridge", Some("root")),
            "agentId",
            "subagent".into(),
        ),
    ] {
        let fixture = Fixture::new(
            &(record("root", None, "user", "question".into())
                + &ignored
                + &record("reply", Some("bridge"), "assistant", "reply".into())
                + &record("reply-2", Some("reply"), "assistant", "final".into())),
        );
        let page = reader
            .read(&fixture.binding, Window::Latest, Limits::default())
            .unwrap();
        assert!(page.turns.is_empty());
        assert!(page
            .messages
            .iter()
            .all(|message| message.id != "root" && message.id != "bridge"));
        assert!(!serde_json::to_string(&page).unwrap().contains("HIDDEN_"));
    }
}

#[test]
fn attachment_ids_are_bounded_and_unknown_record_types_remain_unsupported() {
    for invalid in [
        changed_record(
            &attachment_record("bridge", Some("root")),
            "uuid",
            "x".repeat(513).into(),
        ),
        changed_record(
            &attachment_record("bridge", Some("root")),
            "parentUuid",
            "x".repeat(513).into(),
        ),
        changed_record(
            &attachment_record("bridge", Some("root")),
            "parentUuid",
            "bad\nparent".into(),
        ),
        changed_record(
            &attachment_record("bridge", Some("root")),
            "attachment",
            "HIDDEN_ATTACHMENT_BODY".into(),
        ),
        changed_record(
            &attachment_record("bridge", Some("root")),
            "type",
            "unknown-new-metadata".into(),
        ),
    ] {
        let fixture = Fixture::new(&(record("root", None, "user", "question".into()) + &invalid));
        let page = ConversationReader::default()
            .read(&fixture.binding, Window::Latest, Limits::default())
            .unwrap();
        assert_eq!(page.completeness, "partial_unsupported_records");
        assert!(!serde_json::to_string(&page).unwrap().contains("HIDDEN_"));
    }
}

#[test]
fn object_origin_classifies_human_and_automation_prompts_conservatively() {
    let mut text = String::new();
    for (id, origin) in [
        ("human-object", serde_json::json!({"kind":"human"})),
        ("human-string", "human".into()),
        (
            "automation-object",
            serde_json::json!({"kind":"automation"}),
        ),
        ("automation-string", "automation".into()),
        ("unknown-object", serde_json::json!({"source":"unknown"})),
        ("malformed-origin", serde_json::json!(["human"])),
    ] {
        text.push_str(&changed_record(
            &record(id, None, "user", "generated request".into()),
            "origin",
            origin,
        ));
    }
    let fixture = Fixture::new(&text);
    let page = ConversationReader::default()
        .read(&fixture.binding, Window::Latest, Limits::default())
        .unwrap();
    assert_eq!(
        page.turns
            .iter()
            .map(|turn| turn.message_id.as_str())
            .collect::<Vec<_>>(),
        ["human-object", "human-string"]
    );
    assert!(page
        .messages
        .iter()
        .skip(2)
        .all(|message| message.origin != "human"));
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
