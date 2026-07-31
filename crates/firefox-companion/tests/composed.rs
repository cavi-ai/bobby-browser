use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use companion_core::{
    read_native_message, run_native_host, write_native_message, CompanionServer,
    CompanionServerConfig, NativeConnectRequest, NativeHostConfig, PairingInput,
    MAX_NATIVE_MESSAGE_BYTES,
};
use companion_protocol::{
    ActionResult, BrowserTarget, CompanionEvent, CompanionRequest, InteractionPath,
    PageBindingDiscovered, TargetDiscovery, TargetKind, PROTOCOL_VERSION,
};
use firefox_companion::{
    required_extension_capabilities, BidiEvent, BidiTransport, CompanionExtensionObserver,
    FirefoxCompanionWorker,
};
use serde_json::{json, Value};
use tokio::{
    io::{duplex, split},
    sync::{broadcast, mpsc, oneshot, Mutex},
};
use types::{
    ClickCommand, CommandError, Evidence, InspectCommand, PageId, SessionId, TypeTextCommand,
    WorkerId,
};
use worker_pool::{
    BrowserWorker, BrowserWorkerSelector, EnginePreference, FactoryRegistration,
    RequiredCapabilities, SelectedWorkerFactory, WorkerFactory,
};

struct StaticFirefoxFactory {
    worker: Arc<dyn BrowserWorker>,
}

#[async_trait]
impl WorkerFactory for StaticFirefoxFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::clone(&self.worker))
    }
}

struct BindingBidi {
    calls: Mutex<Vec<(String, Value)>>,
    marker: mpsc::UnboundedSender<String>,
    events: broadcast::Sender<BidiEvent>,
}

#[async_trait]
impl BidiTransport for BindingBidi {
    async fn send(&self, method: &str, params: Value) -> Result<Value, types::CommandError> {
        self.calls
            .lock()
            .await
            .push((method.to_owned(), params.clone()));
        match method {
            "session.subscribe" => Ok(json!({})),
            "browsingContext.create" => Ok(json!({"context": "bidi-context-created"})),
            "script.evaluate" if params["expression"] == "document.title" => {
                Ok(json!({"result": {"type": "string", "value": "Composed original title"}}))
            }
            "script.evaluate"
                if params["expression"]
                    .as_str()
                    .is_some_and(|value| value.contains("automation-runtime-binding:")) =>
            {
                self.marker
                    .send(params["expression"].as_str().unwrap().to_owned())
                    .unwrap();
                Ok(json!({"result": {"type": "boolean", "value": true}}))
            }
            "script.evaluate"
                if params["expression"]
                    .as_str()
                    .is_some_and(|value| value.contains("HTMLSelectElement")) =>
            {
                Ok(json!({"result": {"type": "string", "value": "not-select"}}))
            }
            "script.evaluate"
                if params["expression"]
                    .as_str()
                    .is_some_and(|value| value.contains("JSON.stringify({valid:")) =>
            {
                Ok(
                    json!({"result": {"type": "string", "value": "{\"valid\":true,\"message\":\"\"}"}}),
                )
            }
            "script.evaluate"
                if params["expression"]
                    .as_str()
                    .is_some_and(|value| value.starts_with("document.querySelector(")) =>
            {
                Ok(json!({"result": {"type": "node", "sharedId": "native-target"}}))
            }
            "script.callFunction" => Ok(json!({"result": {
                "type": "node",
                "sharedId": params["arguments"][0]["sharedId"]
            }})),
            "script.evaluate" => Ok(json!({"result": {"type": "boolean", "value": true}})),
            _ => Ok(json!({})),
        }
    }

    fn subscribe_events(&self) -> Option<broadcast::Receiver<BidiEvent>> {
        Some(self.events.subscribe())
    }
}

fn binding_nonce(expression: &str) -> String {
    expression
        .split(|character: char| !(character.is_ascii_hexdigit() || character == '-'))
        .find(|part| {
            part.len() == 36
                && part.chars().enumerate().all(|(index, character)| {
                    [8, 13, 18, 23].contains(&index) == (character == '-')
                })
        })
        .expect("binding expression must contain an opaque UUID nonce")
        .to_owned()
}

#[tokio::test]
async fn real_server_binding_and_worker_close_release_the_coordinator_page_id() {
    let server = Arc::new(
        CompanionServer::bind_loopback(CompanionServerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            pairing_code_ttl: Duration::from_secs(60),
            attachment_ttl: Duration::from_secs(300),
        })
        .await
        .unwrap(),
    );
    let registry = server.registry();
    let pairing_code = registry.issue_pairing_code().await;
    let mut pairing = PairingInput::firefox(pairing_code.clone());
    pairing.capabilities.native_input = false;
    let profile_id = pairing.profile_id.clone();
    let native = NativeHostConfig::new(
        format!("ws://{}/v1/companion", server.local_addr())
            .parse()
            .unwrap(),
        pairing_code,
    );
    let connect = json!({
        "kind": "pair",
        "input": NativeConnectRequest {
            protocol_version: PROTOCOL_VERSION,
            companion_id: pairing.companion_id,
            profile_id: profile_id.clone(),
            identity: pairing.identity,
            capabilities: pairing.capabilities,
        }
    });
    let (host_stream, mut extension) = duplex(2 * MAX_NATIVE_MESSAGE_BYTES);
    let (host_reader, host_writer) = split(host_stream);
    let native_host = tokio::spawn(run_native_host(host_reader, host_writer, native));
    write_native_message(&mut extension, &connect)
        .await
        .unwrap();
    assert_eq!(
        read_native_message(&mut extension).await.unwrap().unwrap()["kind"],
        "paired"
    );
    write_native_message(
        &mut extension,
        &serde_json::to_value(CompanionEvent::TargetsDiscovered(TargetDiscovery {
            protocol_version: PROTOCOL_VERSION,
            profile_id: profile_id.clone(),
            targets: vec![BrowserTarget {
                target_id: "existing-target".into(),
                kind: TargetKind::Page,
            }],
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    server
        .wait_for_discovery(&profile_id, Duration::from_secs(1))
        .await
        .unwrap();
    let initial_grant = server.grant_discovered_targets(&profile_id).await.unwrap();
    let initial_grant_wire: CompanionRequest =
        serde_json::from_value(read_native_message(&mut extension).await.unwrap().unwrap())
            .unwrap();
    assert_eq!(
        initial_grant_wire,
        CompanionRequest::Grant(initial_grant.clone())
    );
    let lease = registry
        .resolve_attachment(&initial_grant.attachment_id)
        .await
        .unwrap();
    assert!(!lease.capabilities.native_input);
    assert!(required_extension_capabilities().are_met_by(&lease.capabilities));

    let expected_page = PageId::new();
    let expected_page_for_extension = expected_page.clone();
    let profile_for_assertion = profile_id.clone();
    let (marker_tx, mut marker_rx) = mpsc::unbounded_channel();
    let (events, _) = broadcast::channel(8);
    let (reduced_tx, reduced_rx) = oneshot::channel();
    let bidi = Arc::new(BindingBidi {
        calls: Mutex::new(Vec::new()),
        marker: marker_tx,
        events,
    });
    let bidi_for_assertion = Arc::clone(&bidi);
    let extension_task = tokio::spawn(async move {
        let nonce = binding_nonce(&marker_rx.recv().await.unwrap());
        write_native_message(
            &mut extension,
            &serde_json::to_value(CompanionEvent::TargetsDiscovered(TargetDiscovery {
                protocol_version: PROTOCOL_VERSION,
                profile_id: profile_id.clone(),
                targets: vec![
                    BrowserTarget {
                        target_id: "existing-target".into(),
                        kind: TargetKind::Page,
                    },
                    BrowserTarget {
                        target_id: "new-browser-target".into(),
                        kind: TargetKind::Page,
                    },
                ],
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        write_native_message(
            &mut extension,
            &serde_json::to_value(CompanionEvent::PageBindingDiscovered(
                PageBindingDiscovered {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    target_id: "new-browser-target".into(),
                    binding_nonce: nonce,
                },
            ))
            .unwrap(),
        )
        .await
        .unwrap();

        let updated_grant = match serde_json::from_value(
            read_native_message(&mut extension).await.unwrap().unwrap(),
        )
        .unwrap()
        {
            CompanionRequest::Grant(grant) => grant,
            request => panic!("expected updated grant, got {request:?}"),
        };
        assert!(updated_grant.pages.iter().any(|page| {
            page.target_id == "new-browser-target" && page.page_id == expected_page_for_extension
        }));

        let action = match serde_json::from_value(
            read_native_message(&mut extension).await.unwrap().unwrap(),
        )
        .unwrap()
        {
            CompanionRequest::Action(action) => action,
            request => panic!("expected observation action, got {request:?}"),
        };
        assert_eq!(action.page_id, expected_page_for_extension);
        assert_eq!(action.input["selector"], "#scoped");
        assert_eq!(action.input["includeHtml"], true);
        write_native_message(
            &mut extension,
            &serde_json::to_value(CompanionEvent::ActionCompleted(ActionResult {
                command_id: action.command_id,
                interaction_path: InteractionPath::ExtensionApi,
                output: json!({
                    "url": "https://example.test/page",
                    "title": "Example",
                    "visibleText": "Selector scoped text",
                    "controls": [{
                        "cssPath": "#confirm",
                        "role": "button",
                        "name": "Confirm",
                        "disabled": false
                    }],
                    "html": "<section id=\"scoped\">Selector scoped text</section>"
                }),
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        let reduced_grant = match serde_json::from_value(
            read_native_message(&mut extension).await.unwrap().unwrap(),
        )
        .unwrap()
        {
            CompanionRequest::Grant(grant) => grant,
            request => panic!("expected reduced grant, got {request:?}"),
        };
        assert!(!reduced_grant
            .pages
            .iter()
            .any(|page| page.page_id == expected_page_for_extension));
        reduced_tx.send(()).unwrap();

        let renewed_grant = match serde_json::from_value(
            read_native_message(&mut extension).await.unwrap().unwrap(),
        )
        .unwrap()
        {
            CompanionRequest::Grant(grant) => grant,
            request => panic!("expected renewed grant, got {request:?}"),
        };
        assert!(!renewed_grant
            .pages
            .iter()
            .any(|page| page.page_id == expected_page_for_extension));
    });

    let observer = Arc::new(CompanionExtensionObserver::new(
        Arc::clone(&server),
        Duration::from_secs(2),
    ));
    let worker: Arc<dyn BrowserWorker> = Arc::new(
        FirefoxCompanionWorker::new(
            WorkerId::new(),
            PathBuf::from("/profiles/firefox"),
            lease,
            bidi,
            observer,
        )
        .await
        .unwrap(),
    );
    let session_id = SessionId::new();
    let selector = Arc::new(BrowserWorkerSelector::new(
        vec![FactoryRegistration::negotiated(
            companion_protocol::BrowserEngine::Firefox,
            Some(profile_for_assertion.clone()),
            Arc::new(StaticFirefoxFactory {
                worker: Arc::clone(&worker),
            }),
        )],
        RequiredCapabilities::default(),
    ));
    let selected = SelectedWorkerFactory::new(
        selector,
        EnginePreference::Exact {
            engine: companion_protocol::BrowserEngine::Firefox,
            profile_id: Some(profile_for_assertion.clone()),
        },
    );
    let worker = selected.launch(&session_id).await.unwrap();
    worker.open_page(expected_page.clone()).await.unwrap();
    let evidence = worker
        .inspect(
            &expected_page,
            &InspectCommand {
                selector: Some("#scoped".into()),
                target: None,
                include_html: true,
            },
        )
        .await
        .unwrap();

    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::Inspection { selector, text, html, .. }
            if selector.as_deref() == Some("#scoped")
                && text == "Selector scoped text"
                && html.as_deref() == Some("<section id=\"scoped\">Selector scoped text</section>")
    )));
    let typed = worker
        .type_text(
            &expected_page,
            &TypeTextCommand {
                selector: "#name".into(),
                target: None,
                value: "Bobby".into(),
                clear_first: true,
            },
        )
        .await
        .unwrap();
    let clicked = worker
        .click(
            &expected_page,
            &ClickCommand {
                selector: "#submit".into(),
                target: None,
                boundary: true,
                expected_url: None,
            },
        )
        .await
        .unwrap();
    assert!(typed.iter().chain(&clicked).any(|item| matches!(
        item,
        Evidence::BrowserExecution { interaction_path, .. }
            if interaction_path == "engineNative"
    )));
    worker.close().await.unwrap();
    assert_eq!(
        bidi_for_assertion
            .calls
            .lock()
            .await
            .iter()
            .filter(|(method, _)| method == "browsingContext.close")
            .count(),
        1
    );
    reduced_rx.await.unwrap();
    let active = server.active_grant(&profile_for_assertion).await.unwrap();
    assert!(!active
        .pages
        .iter()
        .any(|page| page.page_id == expected_page));
    let renewed = server
        .renew_grant(&initial_grant.attachment_id)
        .await
        .unwrap();
    assert!(!renewed
        .pages
        .iter()
        .any(|page| page.page_id == expected_page));
    extension_task.await.unwrap();
    native_host.await.unwrap().unwrap();
}
