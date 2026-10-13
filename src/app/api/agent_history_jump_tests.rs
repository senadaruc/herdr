use super::*;
use crate::terminal::TerminalId;
use bytes::Bytes;
use tokio::sync::mpsc;
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
struct Fixture {
    app: App,
    target: String,
    id: TerminalId,
    binding: HistoryBinding,
    path: std::path::PathBuf,
    revision: String,
    input: mpsc::Receiver<Bytes>,
}
impl Fixture {
    fn new() -> Self {
        let (mut app, target, id) = super::tests::app_fixture();
        super::tests::set_session(&mut app, &id, "synthetic-session");
        let(runtime,input)=crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(40,4,1024*1024,"❯ Exact Unicode 🌍 prompt\r\n\r\n⏺ reply\r\nline 3\r\nline 4\r\nline 5\r\nline 6\r\nlatest\r\n".as_bytes(),8);
        app.terminal_runtimes.insert(id.clone(), runtime);
        let path = std::env::temp_dir().join(format!(
            "herdr-jump-fixture-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path,format!("{}\n",serde_json::json!({"type":"user","uuid":"root","sessionId":"synthetic-session","message":{"role":"user","content":"Exact Unicode 🌍 prompt"}}))).unwrap();
        let mut saved = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        writeln!(saved, "{}", serde_json::json!({"type":"assistant","uuid":"reply","parentUuid":"root","sessionId":"synthetic-session","message":{"role":"assistant","content":"reply"}})).unwrap();
        let binding = HistoryBinding::from_report(
            "herdr:claude",
            "claude",
            Some("synthetic-session"),
            path.to_str(),
        )
        .unwrap();
        app.state
            .terminals
            .get_mut(&id)
            .unwrap()
            .set_history_binding(binding.clone());
        let mut reader = ConversationReader::default();
        let page = reader
            .read(&binding, Window::Latest, Limits::default())
            .unwrap();
        let revision = page.messages[0].message_revision.clone();
        Self {
            app,
            target,
            id,
            binding,
            path,
            revision,
            input,
        }
    }
    fn request(&self) -> Request {
        Request {
            id: "synthetic-jump".into(),
            method: Method::AgentConversationJump(ConversationJumpParams {
                target: self.target.clone(),
                expected_session_id: self.binding.session_id.clone(),
                expected_generation: self.binding.generation.clone(),
                message_id: "root".into(),
                message_revision: self.revision.clone(),
            }),
        }
    }
    async fn prepare(&mut self) -> (crate::events::AppEvent, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.app.handle_deferred_history_request(self.request(), tx);
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(10), self.app.event_rx.recv())
                .await
                .unwrap()
                .unwrap();
        (event, rx)
    }
    fn runtime(&self) -> &crate::terminal::TerminalRuntime {
        self.app.terminal_runtimes.get(&self.id).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
#[tokio::test]
async fn conversation_jump_worker_moves_exact_source_viewport_without_input_focus_resize_or_pty_replacement(
) {
    let _lock = LOCK.lock().await;
    let mut fixture = Fixture::new();
    let before = fixture.runtime().content_seq();
    let geometry = fixture.runtime().current_size();
    let count = fixture.app.terminal_runtimes.len();
    let focus = (fixture.app.state.active, fixture.app.state.selected);
    let (event, response) = fixture.prepare().await;
    assert!(fixture.app.handle_internal_event_with_render_impact(event));
    let json: serde_json::Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(json["result"]["type"], "agent_conversation_jump");
    assert_eq!(json["result"]["message_id"], "root");
    assert_eq!(json["result"]["content_seq"], before);
    assert_eq!(json["result"]["source_row"], 0);
    assert!(json["result"]["offset_from_bottom"].as_u64().unwrap() > 0);
    assert!(fixture
        .runtime()
        .visible_text()
        .contains("❯ Exact Unicode 🌍 prompt"));
    assert_eq!(fixture.runtime().content_seq(), before);
    assert_eq!(fixture.runtime().current_size(), geometry);
    assert_eq!(fixture.app.terminal_runtimes.len(), count);
    assert_eq!(
        (fixture.app.state.active, fixture.app.state.selected),
        focus
    );
    assert!(fixture.input.try_recv().is_err());
}
#[tokio::test]
async fn conversation_jump_completion_refuses_output_resize_session_and_runtime_changes_without_scrolling(
) {
    let _lock = LOCK.lock().await;
    for change in ["output", "resize", "session", "runtime", "alternate"] {
        let mut fixture = Fixture::new();
        let (event, response) = fixture.prepare().await;
        match change {
            "output" => fixture.runtime().test_process_pty_bytes(b"changed output"),
            "resize" => fixture.runtime().resize(5, 30, 0, 0),
            "session" => {
                super::tests::set_session(&mut fixture.app, &fixture.id, "another-session")
            }
            "runtime" => {
                fixture.app.terminal_runtimes.insert(
                    fixture.id.clone(),
                    crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 4, b"replacement"),
                );
            }
            "alternate" => fixture.runtime().test_process_pty_bytes(b"\x1b[?1049h"),
            _ => unreachable!(),
        }
        let before = fixture.runtime().scroll_metrics();
        assert!(!fixture.app.handle_internal_event_with_render_impact(event));
        let json: serde_json::Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
        assert_eq!(
            json["error"]["code"],
            match change {
                "session" => "session_changed",
                "runtime" => "terminal_changed",
                _ => "content_changed",
            }
        );
        assert_eq!(fixture.runtime().scroll_metrics(), before);
        assert!(fixture.input.try_recv().is_err());
    }
}
#[tokio::test]
async fn conversation_jump_completion_refuses_evicted_source_guard_and_initial_stale_identity() {
    let _lock = LOCK.lock().await;
    let mut fixture = Fixture::new();
    let (event, response) = fixture.prepare().await;
    let mut paths = Vec::new();
    for i in 0..4 {
        let path = fixture.path.with_extension(format!("other-{i}.jsonl"));
        std::fs::write(&path,format!("{}\n",serde_json::json!({"type":"user","uuid":format!("other-{i}"),"sessionId":"synthetic-session","message":{"role":"user","content":format!("other {i}")}}))).unwrap();
        let binding = HistoryBinding::from_report(
            "herdr:claude",
            "claude",
            Some("synthetic-session"),
            path.to_str(),
        )
        .unwrap();
        READER
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .read(&binding, Window::Latest, Limits::default())
            .unwrap();
        paths.push(path);
    }
    let before = fixture.runtime().scroll_metrics();
    assert!(!fixture.app.handle_internal_event_with_render_impact(event));
    let json: serde_json::Value = serde_json::from_str(&response.recv().unwrap()).unwrap();
    assert_eq!(json["error"]["code"], "source_changed");
    assert_eq!(fixture.runtime().scroll_metrics(), before);
    let mut request = fixture.request();
    let Method::AgentConversationJump(ref mut params) = request.method else {
        unreachable!()
    };
    params.expected_generation = "stale-generation".into();
    let (tx, rx) = std::sync::mpsc::channel();
    fixture.app.handle_deferred_history_request(request, tx);
    let json: serde_json::Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
    assert_eq!(json["error"]["code"], "session_changed");
    assert_eq!(fixture.runtime().scroll_metrics(), before);
    assert!(fixture.input.try_recv().is_err());
    for path in paths {
        std::fs::remove_file(path).unwrap();
    }
}
