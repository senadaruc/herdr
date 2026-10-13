use super::*;

fn receive_conversation_endpoint_response(
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
    expected_request_id: &str,
) -> serde_json::Value {
    // Connection state/control packets may precede an asynchronous response.
    for _ in 0..16 {
        let message = read_server_message(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("conversation endpoint response"),
        );
        match message {
            ServerMessage::ClientShellEndpointResponseChunk {
                request_id,
                final_chunk,
                data,
                ..
            } => {
                assert_eq!(request_id, expected_request_id);
                assert!(final_chunk);
                return serde_json::from_slice(&data).unwrap();
            }
            ServerMessage::EndpointControl { .. } | ServerMessage::WindowTitle { .. } => {}
            other => panic!("unexpected conversation endpoint packet: {other:?}"),
        }
    }
    panic!("conversation endpoint response exceeded the control packet bound");
}

#[tokio::test]
async fn conversation_endpoint_worker_preserves_foreground_and_terminal_geometry() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new(
        "synthetic-history-endpoint",
    )];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    let pane = server.app.state.workspaces[0].tabs[0].root_pane;
    let terminal_id = server.app.state.workspaces[0].tabs[0].panes[&pane]
        .attached_terminal_id
        .clone();
    let terminal = server.app.state.terminals.get_mut(&terminal_id).unwrap();
    terminal.detected_agent = Some(crate::detect::Agent::Claude);
    terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
        source: "herdr:claude".into(),
        agent: "claude".into(),
        session_ref: crate::agent_resume::AgentSessionRef::id("synthetic-session").unwrap(),
    });
    let path = std::env::temp_dir().join(format!(
        "herdr-history-endpoint-fixture-{}.jsonl",
        std::process::id()
    ));
    std::fs::write(
        &path,
        include_str!("../../../../tests/fixtures/claude-history/visible.jsonl"),
    )
    .unwrap();
    terminal.set_history_binding(
        crate::agent_history::binding::HistoryBinding::from_report(
            "herdr:claude",
            "claude",
            Some("synthetic-session"),
            path.to_str(),
        )
        .unwrap(),
    );
    let (first_control, _) = connect_test_shell(&mut server, 7, 100, 30);
    let _ = first_control.recv().unwrap();
    let (second_control, _) = connect_test_shell(&mut server, 8, 80, 24);
    let _ = second_control.recv().unwrap();
    let foreground = server.foreground_client_id;
    let target = server.app.public_pane_id(0, pane).unwrap();
    let geometries = server
        .clients
        .iter()
        .map(|(id, c)| {
            (
                *id,
                (
                    *id,
                    c.terminal_size.0,
                    c.terminal_size.1,
                    c.shell_projection_revision,
                ),
            )
        })
        .collect::<HashMap<_, _>>();
    let (runtime, mut input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            40,
            4,
            1024 * 1024,
            "❯ Explain the failing test 🌍\r\n\r\n⏺ Here is the reply.\r\nmore\r\nmore\r\nmore\r\nlatest\r\n"
                .as_bytes(),
            8,
        );
    server
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime);
    let content_seq = server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .unwrap()
        .content_seq();
    let runtimes = server.app.terminal_runtimes.len();
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id: 7,
            boot_id: server.client_shell_boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "history-read".into(),
                method: api::schema::Method::AgentConversationRead(
                    crate::agent_history::model::ConversationReadParams {
                        target: target.clone(),
                        expected_session_id: "synthetic-session".into(),
                        window: crate::agent_history::model::Window::Latest,
                        limit: 100,
                        max_bytes: 1048576
                    }
                ),
            })
        })
    );
    let event = tokio::time::timeout(Duration::from_secs(10), server.app.event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!server.handle_internal_event_with_forwarding(event));
    let ready = tokio::time::timeout(Duration::from_secs(10), server.server_event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!server.handle_server_event(ready));
    let response = receive_conversation_endpoint_response(&first_control, "history-read");
    assert_eq!(response["result"]["availability"], "ready");
    let message_revision = response["result"]["messages"][0]["message_revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let generation = response["result"]["binding"]["generation"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id: 7,
            boot_id: server.client_shell_boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "history-jump".into(),
                method: api::schema::Method::AgentConversationJump(
                    crate::agent_history::model::ConversationJumpParams {
                        target,
                        expected_session_id: "synthetic-session".into(),
                        expected_generation: generation,
                        message_id: "human-1".into(),
                        message_revision,
                    }
                ),
            }),
        })
    );
    let event = tokio::time::timeout(Duration::from_secs(10), server.app.event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(server.handle_internal_event_with_forwarding(event));
    let ready = tokio::time::timeout(Duration::from_secs(10), server.server_event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!server.handle_server_event(ready));
    let response = receive_conversation_endpoint_response(&first_control, "history-jump");
    assert_eq!(response["result"]["type"], "agent_conversation_jump");
    assert!(response["result"]["offset_from_bottom"].as_u64().unwrap() > 0);
    let runtime = server.app.terminal_runtimes.get(&terminal_id).unwrap();
    assert_eq!(runtime.content_seq(), content_seq);
    assert!(runtime
        .visible_text()
        .contains("❯ Explain the failing test 🌍"));
    assert!(input.try_recv().is_err());
    assert_eq!(server.foreground_client_id, foreground);
    assert_eq!(server.app.terminal_runtimes.len(), runtimes);
    for (id, cols, rows, revision) in geometries.values() {
        let client = &server.clients[id];
        assert_eq!(
            (
                client.terminal_size.0,
                client.terminal_size.1,
                client.shell_projection_revision
            ),
            (*cols, *rows, *revision)
        );
    }
    std::fs::remove_file(path).unwrap();
    shutdown_test_runtimes(&mut server);
}
