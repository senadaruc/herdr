//! Copy source association on the actor; all file work runs on bounded workers.
use super::responses::{encode_error, encode_error_body, encode_success};
use crate::agent_history::{binding::HistoryBinding, model::*, ConversationReader};
use crate::api::schema::{Method, Request, ResponseResult};
use crate::app::App;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex, OnceLock,
};
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static READER: OnceLock<Mutex<ConversationReader>> = OnceLock::new();
struct WorkerSlot;
impl Drop for WorkerSlot {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::Release);
    }
}
impl App {
    pub(crate) fn handle_deferred_history_request(
        &mut self,
        request: Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        if let Method::AgentConversationJump(params) = request.method {
            self.handle_deferred_conversation_jump(request.id, params, respond_to);
            return;
        }
        if request.id.len() > 384 {
            let _ = respond_to.send(encode_error(
                request.id,
                "invalid_params",
                "history request ID exceeds supported bounds",
            ));
            return;
        }
        let (target, expected) = match &request.method {
            Method::AgentConversationRead(params) => (&params.target, &params.expected_session_id),
            Method::AgentConversationBlockRead(params) => {
                (&params.target, &params.expected_session_id)
            }
            _ => return,
        };
        let resolved = match self.resolve_agent_target(target) {
            Ok(resolved) => resolved,
            Err(error) => {
                let _ = respond_to.send(encode_error_body(
                    request.id,
                    self.agent_target_error_body(error),
                ));
                return;
            }
        };
        let terminal = self
            .state
            .terminals
            .values()
            .find(|terminal| terminal.id.to_string() == resolved.terminal_id);
        let Some(terminal) = terminal else {
            let _ = respond_to.send(encode_error(
                request.id,
                "agent_not_found",
                "agent terminal is unavailable",
            ));
            return;
        };
        if terminal
            .persisted_agent_session
            .as_ref()
            .is_some_and(|session| session.session_ref.value != *expected)
        {
            let _ = respond_to.send(encode_error(
                request.id,
                "session_changed",
                "selected agent session changed",
            ));
            return;
        }
        let Some(binding) = terminal.active_history_binding().cloned() else {
            let response = match request.method {
                Method::AgentConversationRead(_) => encode_success(
                    request.id,
                    ResponseResult::AgentConversation {
                        page: ConversationPage::unavailable(None, "no_binding"),
                    },
                ),
                _ => encode_error(
                    request.id,
                    "no_binding",
                    "no exact saved conversation source is associated with this agent",
                ),
            };
            let _ = respond_to.send(response);
            return;
        };
        if binding.session_id != *expected {
            let _ = respond_to.send(encode_error(
                request.id,
                "session_changed",
                "selected agent session changed",
            ));
            return;
        }
        if IN_FLIGHT
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .is_err()
        {
            let _ = respond_to.send(encode_error(
                request.id,
                "busy",
                "conversation reader is busy; retry shortly",
            ));
            return;
        }
        let max_bytes = match &request.method {
            Method::AgentConversationRead(params) => params.max_bytes,
            Method::AgentConversationBlockRead(params) => params.max_bytes,
            _ => unreachable!(),
        };
        let tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let _slot = WorkerSlot;
            let response = match READER
                .get_or_init(|| Mutex::new(ConversationReader::default()))
                .lock()
            {
                Ok(mut reader) => {
                    let result = match request.method {
                        Method::AgentConversationRead(params) => reader
                            .read(
                                &binding,
                                params.window,
                                Limits {
                                    limit: params.limit,
                                    max_bytes: params.max_bytes,
                                },
                            )
                            .map(|page| ResponseResult::AgentConversation { page }),
                        Method::AgentConversationBlockRead(params) => reader
                            .read_block(
                                &binding,
                                &params.block_cursor,
                                Limits {
                                    max_bytes: params.max_bytes,
                                    ..Limits::default()
                                },
                            )
                            .map(|page| ResponseResult::AgentConversationBlock { page }),
                        _ => unreachable!(),
                    };
                    match result {
                        Ok(result) => {
                            let response = encode_success(request.id.clone(), result);
                            if response.len() <= max_bytes {
                                response
                            } else {
                                encode_error(
                                    request.id.clone(),
                                    "response_too_large",
                                    "history response exceeds the serialized response budget",
                                )
                            }
                        }
                        Err(error) => encode_error(request.id.clone(), &error.code, error.message),
                    }
                }
                Err(_) => encode_error(
                    request.id.clone(),
                    "source_unavailable",
                    "conversation reader is unavailable",
                ),
            };
            let event = crate::events::AppEvent::AgentHistoryReadFinished {
                terminal_id: resolved.terminal_id,
                binding,
                request_id: request.id,
                response,
                respond_to,
            };
            let _ = tx.blocking_send(event);
        });
    }
    pub(super) fn finish_history_read(
        &self,
        terminal_id: &str,
        binding: &HistoryBinding,
        request_id: String,
        response: String,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let current = self
            .state
            .terminals
            .values()
            .find(|terminal| terminal.id.to_string() == terminal_id)
            .and_then(|terminal| terminal.active_history_binding());
        let response = if current == Some(binding) {
            response
        } else {
            encode_error(
                request_id,
                "session_changed",
                "selected agent session changed during the read",
            )
        };
        let _ = respond_to.send(response);
    }
}

impl App {
    fn handle_deferred_conversation_jump(
        &mut self,
        id: String,
        params: ConversationJumpParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        use crate::agent_history::HistoryError;
        if id.len() > 384
            || params.message_id.len() > 512
            || params.message_revision.len() > 128
            || params.expected_generation.len() > 256
            || params.expected_session_id.len() > 512
        {
            let _ = respond_to.send(encode_error(
                id,
                "invalid_params",
                "conversation jump identity exceeds supported bounds",
            ));
            return;
        }
        let target = match self.resolve_agent_target(&params.target) {
            Ok(target) => target,
            Err(error) => {
                let _ = respond_to.send(encode_error_body(id, self.agent_target_error_body(error)));
                return;
            }
        };
        let Some(terminal) = self
            .state
            .terminals
            .values()
            .find(|terminal| terminal.id.to_string() == target.terminal_id)
        else {
            let _ = respond_to.send(encode_error(
                id,
                "terminal_changed",
                "agent terminal is unavailable",
            ));
            return;
        };
        let Some(binding) = terminal.active_history_binding().cloned() else {
            let _ = respond_to.send(encode_error(
                id,
                "no_binding",
                "no exact saved conversation source is associated with this agent",
            ));
            return;
        };
        if binding.session_id != params.expected_session_id
            || binding.generation != params.expected_generation
        {
            let _ = respond_to.send(encode_error(
                id,
                "session_changed",
                "conversation association changed; reload history",
            ));
            return;
        }
        let Some(runtime) = self.terminal_runtimes.get(&terminal.id) else {
            let _ = respond_to.send(encode_error(
                id,
                "terminal_changed",
                "agent terminal runtime is unavailable",
            ));
            return;
        };
        let terminal_handle = runtime.conversation_terminal_handle();
        if IN_FLIGHT
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .is_err()
        {
            let _ = respond_to.send(encode_error(
                id,
                "busy",
                "conversation worker is busy; retry shortly",
            ));
            return;
        }
        let tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let _slot = WorkerSlot;
            let result = (|| {
                let prompt = READER
                    .get_or_init(|| Mutex::new(ConversationReader::default()))
                    .lock()
                    .map_err(|_| {
                        HistoryError::new("source_changed", "conversation reader is unavailable")
                    })?
                    .exact_human_prompt(&binding, &params.message_id, &params.message_revision)?;
                let anchor =
                    terminal_handle.resolve_prompt(&prompt.text, &prompt.assistant_line)?;
                // Revalidate the transcript after terminal scanning, still on this worker.
                READER
                    .get()
                    .unwrap()
                    .lock()
                    .map_err(|_| {
                        HistoryError::new("source_changed", "conversation reader is unavailable")
                    })?
                    .validate_jump_source(&prompt)?;
                Ok(Box::new(
                    crate::agent_history::jump::ConversationJumpCandidate {
                        binding: binding.clone(),
                        message_id: prompt.message_id,
                        message_revision: prompt.message_revision,
                        source_guard: prompt.guard,
                        anchor,
                    },
                ))
            })();
            let _ = tx.blocking_send(crate::events::AppEvent::AgentHistoryJumpFinished {
                terminal_id: target.terminal_id,
                binding,
                request_id: id,
                result,
                respond_to,
            });
        });
    }
    pub(super) fn finish_history_jump(
        &mut self,
        terminal_id: &str,
        binding: &crate::agent_history::binding::HistoryBinding,
        request_id: String,
        result: Result<
            Box<crate::agent_history::jump::ConversationJumpCandidate>,
            crate::agent_history::HistoryError,
        >,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        use crate::agent_history::HistoryError;
        let outcome: Result<ConversationJumpResult, HistoryError> = (|| {
            let candidate = result?;
            let terminal = self
                .state
                .terminals
                .values()
                .find(|terminal| terminal.id.to_string() == terminal_id)
                .ok_or_else(|| {
                    HistoryError::new(
                        "terminal_changed",
                        "agent terminal was removed before the jump",
                    )
                })?;
            let runtime = self.terminal_runtimes.get(&terminal.id).ok_or_else(|| {
                HistoryError::new(
                    "terminal_changed",
                    "agent terminal runtime was removed before the jump",
                )
            })?;
            let offset = runtime.commit_conversation_jump(&candidate.anchor, || {
                if terminal.active_history_binding() != Some(binding)
                    || candidate.binding != *binding
                {
                    return Err(HistoryError::new(
                        "session_changed",
                        "conversation association changed before the jump",
                    ));
                }
                candidate.source_guard.validate()
            })?;
            Ok(ConversationJumpResult {
                binding: binding.public(),
                message_id: candidate.message_id,
                message_revision: candidate.message_revision,
                content_seq: candidate.anchor.content_seq,
                source_row: candidate.anchor.source_row as u64,
                offset_from_bottom: offset as u64,
            })
        })();
        let changed = outcome.is_ok();
        let response = match outcome {
            Ok(jump) => encode_success(request_id, ResponseResult::AgentConversationJump { jump }),
            Err(error) => encode_error(request_id, &error.code, error.message),
        };
        let _ = respond_to.send(response);
        if changed {
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn app_fixture() -> (App, String, crate::terminal::TerminalId) {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("synthetic-history")];
        app.state.ensure_test_terminals();
        let pane = app.state.workspaces[0].tabs[0].root_pane;
        let id = app.state.workspaces[0].tabs[0].panes[&pane]
            .attached_terminal_id
            .clone();
        app.state.terminals.get_mut(&id).unwrap().detected_agent =
            Some(crate::detect::Agent::Claude);
        let target = app.public_pane_id(0, pane).unwrap();
        (app, target, id)
    }
    pub(super) fn set_session(app: &mut App, id: &crate::terminal::TerminalId, session: &str) {
        app.state
            .terminals
            .get_mut(id)
            .unwrap()
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "herdr:claude".into(),
                agent: "claude".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id(session).unwrap(),
            });
    }
    fn read_request(target: String, session: &str, window: Window) -> Request {
        Request {
            id: "synthetic-read".into(),
            method: Method::AgentConversationRead(ConversationReadParams {
                target,
                expected_session_id: session.into(),
                window,
                limit: 100,
                max_bytes: MAX_BYTES,
            }),
        }
    }
    #[tokio::test]
    async fn native_worker_reads_fixture_and_rechecks_session_before_response() {
        let (mut app, target, id) = app_fixture();
        set_session(&mut app, &id, "synthetic-session");
        let path = std::env::temp_dir().join(format!(
            "herdr-history-api-fixture-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(
            &path,
            include_str!("../../../tests/fixtures/claude-history/visible.jsonl"),
        )
        .unwrap();
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
        let (tx, rx) = std::sync::mpsc::channel();
        app.handle_deferred_history_request(
            read_request(target.clone(), "synthetic-session", Window::Latest),
            tx,
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), app.event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle_internal_event(event);
        let json: serde_json::Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!(json["result"]["type"], "agent_conversation");
        assert_eq!(json["result"]["availability"], "ready");
        assert_eq!(json["result"]["messages"][0]["id"], "human-1");
        assert_eq!(json["result"]["binding"]["generation"], binding.generation);
        let (tx, rx) = std::sync::mpsc::channel();
        app.handle_deferred_history_request(
            read_request(target, "synthetic-session", Window::Latest),
            tx,
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), app.event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        set_session(&mut app, &id, "replacement-session");
        app.handle_internal_event(event);
        let json: serde_json::Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!(json["error"]["code"], "session_changed");
        std::fs::remove_file(path).unwrap();
    }
    #[tokio::test]
    async fn no_binding_and_expected_session_mismatch_are_distinct() {
        let (mut app, target, id) = app_fixture();
        set_session(&mut app, &id, "synthetic-session");
        let (tx, rx) = std::sync::mpsc::channel();
        app.handle_deferred_history_request(
            read_request(target.clone(), "synthetic-session", Window::Latest),
            tx,
        );
        let json: serde_json::Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!(json["result"]["availability"], "no_binding");
        let (tx, rx) = std::sync::mpsc::channel();
        app.handle_deferred_history_request(
            read_request(target, "another-session", Window::Latest),
            tx,
        );
        let json: serde_json::Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!(json["error"]["code"], "session_changed");
    }
    #[test]
    fn old_capability_json_decodes_without_history() {
        let caps: crate::api::schema::ServerCapabilities =
            serde_json::from_str("{\"live_handoff\":false}").unwrap();
        assert!(!caps.agent_conversation_read);
        assert!(!caps.agent_conversation_block_read);
    }
    async fn worker_response(app: &mut App, request: Request) -> serde_json::Value {
        let (tx, rx) = std::sync::mpsc::channel();
        app.handle_deferred_history_request(request, tx);
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), app.event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle_internal_event(event);
        serde_json::from_str(&rx.recv().unwrap()).unwrap()
    }
    #[tokio::test]
    async fn tiny_window_and_block_rpc_budgets_leave_shared_reader_healthy() {
        let (mut app, target, id) = app_fixture();
        set_session(&mut app, &id, "synthetic-session");
        let path = std::env::temp_dir().join(format!(
            "herdr-history-tiny-budget-fixture-{}.jsonl",
            std::process::id()
        ));
        let record = serde_json::json!({"type":"user","uuid":"root","sessionId":"synthetic-session","message":{"role":"user","content":"needle ".repeat(10000)}});
        let reply = serde_json::json!({"type":"assistant","uuid":"reply","parentUuid":"root","sessionId":"synthetic-session","message":{"role":"assistant","content":"needle reply"}});
        std::fs::write(&path, format!("{record}\n{reply}\n")).unwrap();
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
            .set_history_binding(binding);
        let mut first_request = read_request(target.clone(), "synthetic-session", Window::Latest);
        let Method::AgentConversationRead(ref mut params) = first_request.method else {
            unreachable!()
        };
        params.limit = 1;
        let first = worker_response(&mut app, first_request).await;
        let before = first["result"]["before_cursor"]
            .as_str()
            .unwrap()
            .to_owned();
        let older = worker_response(
            &mut app,
            read_request(
                target.clone(),
                "synthetic-session",
                Window::Before {
                    cursor: before.clone(),
                },
            ),
        )
        .await;
        let after = older["result"]["after_cursor"].as_str().unwrap().to_owned();
        let mut long_request = read_request(
            target.clone(),
            "synthetic-session",
            Window::Around {
                message_id: "root".into(),
            },
        );
        let Method::AgentConversationRead(ref mut params) = long_request.method else {
            unreachable!()
        };
        params.limit = 1;
        params.max_bytes = 4096;
        let long = worker_response(&mut app, long_request).await;
        let block = long["result"]["messages"][0]["blocks"][0]["block_cursor"]
            .as_str()
            .unwrap()
            .to_owned();
        for window in [
            Window::Latest,
            Window::Before { cursor: before },
            Window::After { cursor: after },
            Window::Around {
                message_id: "root".into(),
            },
            Window::Search {
                query: "needle".into(),
                cursor: None,
            },
            Window::Turns { cursor: None },
            Window::Changes {
                cursor: first["result"]["changes_cursor"].as_str().unwrap().into(),
            },
        ] {
            let mut request = read_request(target.clone(), "synthetic-session", window);
            let Method::AgentConversationRead(ref mut params) = request.method else {
                unreachable!()
            };
            params.max_bytes = 1;
            let response = worker_response(&mut app, request).await;
            assert_eq!(response["error"]["code"], "response_too_large");
        }
        let response = worker_response(
            &mut app,
            Request {
                id: "tiny-block".into(),
                method: Method::AgentConversationBlockRead(ConversationBlockReadParams {
                    target: target.clone(),
                    expected_session_id: "synthetic-session".into(),
                    block_cursor: block,
                    max_bytes: 1,
                }),
            },
        )
        .await;
        assert_eq!(response["error"]["code"], "response_too_large");
        let healthy = worker_response(
            &mut app,
            read_request(target, "synthetic-session", Window::Latest),
        )
        .await;
        assert_eq!(healthy["result"]["availability"], "ready");
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
#[path = "agent_history_jump_tests.rs"]
mod jump_tests;
