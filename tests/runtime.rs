use optchat::{
    memory::Kind,
    model::{Client, Provider, StreamEvent, Usage},
    runtime::{self, Command, Event, Settings},
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

#[tokio::test]
async fn attachments_reach_provider_as_images_and_text() {
    for provider in [Provider::Anthropic, Provider::DeepSeek] {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.png");
        image::RgbImage::new(2, 2).save(&image).unwrap();
        let log = dir.path().join("input.log");
        std::fs::write(&log, "ERROR: connection refused").unwrap();
        let (endpoint, requests, server) =
            server(vec![vec![json!({"type":"text","text":"Analyzed."})]]);
        let (tx, rx) = mpsc::unbounded_channel();
        let (ev, mut events) = mpsc::unbounded_channel();
        let task = tokio::spawn(runtime::run(
            Settings {
                integrations_path: None,
                provider,
                directory: dir.path().into(),
                master: "fixture".into(),
                compactor: "fixture-compactor".into(),
                instructions: String::new(),
                key: "fixture".into(),
                endpoint,
            },
            rx,
            ev,
        ));
        tx.send(Command::SendFiles {
            text: "Analyze these".into(),
            paths: vec![image, log],
        })
        .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = events.recv().await {
                match event {
                    Event::Idle => break,
                    Event::Error(e) => panic!("{e}"),
                    Event::AttachmentRejected { error, .. } => panic!("{error}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        tx.send(Command::Shutdown).unwrap();
        task.await.unwrap();
        server.join().unwrap();
        let requests = requests.lock().unwrap();
        let request = requests.iter().find(|r| r["model"] == "fixture").unwrap();
        let blocks = request["messages"][0]["content"].as_array().unwrap();
        assert!(
            blocks
                .iter()
                .any(|b| b["type"] == "image" && b["source"]["media_type"] == "image/png")
        );
        assert!(
            blocks
                .iter()
                .any(|b| b["text"] == "ERROR: connection refused")
        );
    }
}

#[tokio::test]
async fn local_tool_config_edits_reload_before_next_model_step() {
    use optchat::integrations::{Config, Server};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("integrations.json");
    let base = Config {
        skill_directories: vec![],
        ..Config::default()
    };
    base.save(&path).await.unwrap();
    let mut edited = base.clone();
    edited.servers.insert(
        "added".into(),
        Server::Stdio {
            command: "python3".into(),
            args: vec![
                format!(
                    "{}/tests/fixtures/mcp_server.py",
                    env!("CARGO_MANIFEST_DIR")
                ),
                "stdio".into(),
                dir.path().join("mcp.log").display().to_string(),
                "added".into(),
            ],
            cwd: None,
            env_from: Default::default(),
        },
    );
    let (endpoint, requests, server) = server(vec![
        vec![
            json!({"type":"tool_use","id":"edit","name":"write_file","input":{"path":path,"content":serde_json::to_string(&edited).unwrap(),"overwrite":true}}),
        ],
        vec![json!({"type":"text","text":"Configuration loaded."})],
    ]);
    let (tx, rx) = mpsc::unbounded_channel();
    let (ev, mut events) = mpsc::unbounded_channel();
    let task = tokio::spawn(runtime::run(
        Settings {
            integrations_path: Some(path.clone()),
            provider: Provider::Anthropic,
            directory: dir.path().into(),
            master: "fixture".into(),
            compactor: "fixture-compactor".into(),
            instructions: String::new(),
            key: "fixture".into(),
            endpoint,
        },
        rx,
        ev,
    ));
    tx.send(Command::Send("Add a server".into())).unwrap();
    let mut loaded = false;
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = events.recv().await {
            match event {
                Event::Approval(approval) => {
                    approval.answer.send(true).unwrap();
                }
                Event::Integrations { config, .. } => {
                    loaded |= Config::parse(&config)
                        .unwrap()
                        .servers
                        .contains_key("added");
                }
                Event::Idle => break,
                Event::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    tx.send(Command::Shutdown).unwrap();
    task.await.unwrap();
    server.join().unwrap();
    assert!(loaded);
    assert!(
        Config::load(&path)
            .await
            .unwrap()
            .servers
            .contains_key("added")
    );
    let requests = requests.lock().unwrap();
    let master: Vec<_> = requests
        .iter()
        .filter(|r| r["model"] == "fixture")
        .collect();
    assert_eq!(master.len(), 2);
    assert!(master[1]["tools"].as_array().unwrap().iter().any(|t| {
        t["description"]
            .as_str()
            .unwrap_or("")
            .starts_with("MCP server added,")
    }));
}

#[tokio::test]
async fn skills_and_mcp_complete_a_model_turn_with_durable_results() {
    check_mcp_permissions(false).await;
}

#[tokio::test]
async fn full_access_executes_without_prompt_and_can_return_to_ask() {
    check_mcp_permissions(true).await;
}

async fn check_mcp_permissions(full_access: bool) {
    use optchat::integrations::{Config, Integrations, Server};
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: fixture\ndescription: Test skill\n---\nUse the echo tool.",
    )
    .unwrap();
    let log = dir.path().join("mcp.log");
    let config = Config {
        skill_directories: vec![skill_dir.clone()],
        servers: std::collections::BTreeMap::from([(
            "fixture".into(),
            Server::Stdio {
                command: "python3".into(),
                args: vec![
                    format!(
                        "{}/tests/fixtures/mcp_server.py",
                        env!("CARGO_MANIFEST_DIR")
                    ),
                    "stdio".into(),
                    log.display().to_string(),
                    "runtime".into(),
                ],
                cwd: None,
                env_from: Default::default(),
            },
        )]),
        timeout_seconds: 3,
    };
    let integration = Integrations::connect(config.clone()).await;
    let alias = integration
        .definitions
        .iter()
        .find(|d| {
            d["description"]
                .as_str()
                .unwrap_or("")
                .starts_with("MCP server fixture, tool echo.")
        })
        .unwrap()["name"]
        .as_str()
        .unwrap()
        .to_owned();
    let skill_id = integration.skills[0].id.clone();
    drop(integration);
    let config_path = dir.path().join("integrations.json");
    config.save(&config_path).await.unwrap();
    let (endpoint, requests, server) = server(vec![
        vec![json!({"type":"tool_use","id":"skill","name":"load_skill","input":{"id":skill_id}})],
        vec![json!({"type":"tool_use","id":"mcp","name":alias,"input":{"value":"worked"}})],
        vec![json!({"type":"text","text":"Completed the skill with the MCP tool."})],
        vec![
            json!({"type":"tool_use","id":"cancel-me","name":alias,"input":{"value":"never-execute"}}),
        ],
        vec![json!({"type":"text","text":"A fresh turn after cancellation."})],
    ]);
    let (tx, rx) = mpsc::unbounded_channel();
    let (ev, mut events) = mpsc::unbounded_channel();
    let task = tokio::spawn(runtime::run(
        Settings {
            provider: Provider::DeepSeek,
            directory: dir.path().into(),
            master: "fixture".into(),
            compactor: "fixture-compactor".into(),
            instructions: String::new(),
            key: "fixture".into(),
            endpoint,
            integrations_path: Some(config_path.clone()),
        },
        rx,
        ev,
    ));
    if full_access {
        tx.send(Command::Permissions(runtime::Permissions::FullAccess))
            .unwrap();
    }
    tx.send(Command::Send("Use the fixture skill".into()))
        .unwrap();
    let mut approvals = 0;
    let mut pending_approval = None;
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = events.recv().await {
            match event {
                Event::Approval(approval) => {
                    assert!(!full_access, "Full access must not emit approval prompts");
                    assert_eq!(approval.server, "fixture");
                    assert_eq!(approval.arguments["value"], "worked");
                    approvals += 1;
                    tx.send(Command::Send("Also keep this mid-run instruction".into()))
                        .unwrap();
                    pending_approval = Some(approval);
                }
                Event::Status(status) if status.contains("queued for the next tool boundary") => {
                    pending_approval.take().unwrap().answer.send(true).unwrap();
                }
                Event::Idle => break,
                Event::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(approvals, usize::from(!full_access));
    assert_eq!(
        runtime::Permissions::load(dir.path()).await.unwrap(),
        if full_access {
            runtime::Permissions::FullAccess
        } else {
            runtime::Permissions::Ask
        }
    );
    tx.send(Command::Permissions(runtime::Permissions::Ask))
        .unwrap();
    tx.send(Command::ReloadIntegrations {
        config: "invalid JSON".into(),
        base: "{}".into(),
    })
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(event) = events.recv().await {
            if matches!(event, Event::Error(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        Config::load(&config_path)
            .await
            .unwrap()
            .servers
            .contains_key("fixture")
    );
    tx.send(Command::Send("Try another call".into())).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            match event {
                Event::Approval(approval) => {
                    assert_eq!(approval.arguments["value"], "never-execute");
                    tx.send(Command::Cancel).unwrap();
                    pending_approval = Some(approval);
                }
                Event::Idle => break,
                Event::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    tx.send(Command::Send("Start fresh".into())).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            if matches!(event, Event::Idle) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(pending_approval.take().unwrap().answer.is_closed());
    tx.send(Command::Shutdown).unwrap();
    task.await.unwrap();
    server.join().unwrap();
    let store = optchat::memory::Store::open(dir.path()).await.unwrap();
    assert!(
        store
            .memory
            .root
            .iter()
            .any(|m| m.kind == Kind::Echo && m.text.contains("Use the echo tool."))
    );
    assert!(
        store
            .memory
            .root
            .iter()
            .any(|m| m.kind == Kind::Echo && m.text.contains("runtime:worked"))
    );
    let requests = requests.lock().unwrap();
    let master: Vec<_> = requests
        .iter()
        .filter(|r| r["model"] == "fixture")
        .collect();
    assert_eq!(master.len(), 5);
    assert_eq!(master[0]["tools"], master[2]["tools"]);
    assert!(master[2]["messages"].to_string().contains("runtime:worked"));
    assert_eq!(
        master[2]["messages"]
            .to_string()
            .contains("Also keep this mid-run instruction"),
        !full_access
    );
    assert_eq!(master[4]["messages"].as_array().unwrap().len(), 1);
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(!calls.contains("never-execute"));
}

fn server(
    replies: Vec<Vec<Value>>,
) -> (String, Arc<Mutex<Vec<Value>>>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/messages", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(vec![]));
    let seen = requests.clone();
    let handle = std::thread::spawn(move || {
        let mut replies = std::collections::VecDeque::from(replies);
        while !replies.is_empty() {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut data = vec![];
            let mut buf = [0u8; 4096];
            let (head, len) = loop {
                let n = socket.read(&mut buf).unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
                if let Some(pos) = data.windows(4).position(|b| b == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..pos]);
                    let len = headers
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|s| s.parse::<usize>().ok())
                        })
                        .unwrap();
                    break (pos + 4, len);
                }
            };
            while data.len() < head + len {
                let n = socket.read(&mut buf).unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
            }
            let request: Value = serde_json::from_slice(&data[head..head + len]).unwrap();
            let blocks = if request["model"] == "fixture-compactor" {
                vec![
                    json!({"type":"text","text":"user: integration test; echo: tool returned a result."}),
                ]
            } else {
                replies.pop_front().unwrap()
            };
            seen.lock().unwrap().push(request);
            let mut events = vec![
                json!({"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":50}}}),
            ];
            for (i, block) in blocks.iter().enumerate() {
                let mut start = block.clone();
                let mut deltas = vec![];
                match block["type"].as_str().unwrap() {
                    "text" => {
                        start["text"] = json!("");
                        deltas.push(json!({"type":"text_delta","text":block["text"]}));
                    }
                    "thinking" => {
                        start["thinking"] = json!("");
                        start["signature"] = json!("");
                        deltas.push(json!({"type":"thinking_delta","thinking":block["thinking"]}));
                        deltas
                            .push(json!({"type":"signature_delta","signature":block["signature"]}));
                    }
                    "tool_use" => {
                        start["input"] = json!({});
                        let input = block["input"].to_string();
                        let middle = input.len() / 2;
                        deltas.push(
                            json!({"type":"input_json_delta","partial_json":&input[..middle]}),
                        );
                        deltas.push(
                            json!({"type":"input_json_delta","partial_json":&input[middle..]}),
                        );
                    }
                    _ => {}
                }
                events.push(json!({"type":"content_block_start","index":i,"content_block":start}));
                for delta in deltas {
                    events.push(json!({"type":"content_block_delta","index":i,"delta":delta}));
                }
                events.push(json!({"type":"content_block_stop","index":i}));
            }
            events.push(json!({"type":"message_delta","delta":{"stop_reason":if blocks.iter().any(|b|b["type"]=="tool_use") { "tool_use" } else { "end_turn" }},"usage":{"output_tokens":20}}));
            events.push(json!({"type":"message_stop"}));
            let body = events
                .iter()
                .map(|e| format!("data: {e}\n\n"))
                .collect::<String>();
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
        }
    });
    (endpoint, requests, handle)
}

#[tokio::test]
async fn fresh_turn_tool_loop_preserves_reasoning_but_never_logs_it() {
    check_turn_loop(Provider::Anthropic).await;
}

#[tokio::test]
async fn deepseek_fresh_turn_tool_loop_preserves_reasoning_but_never_logs_it() {
    check_turn_loop(Provider::DeepSeek).await;
}

async fn check_turn_loop(provider: Provider) {
    let (endpoint, requests, server) = server(vec![
        vec![
            json!({"type":"thinking","thinking":"private reasoning","signature":"signed-value"}),
            json!({"type":"tool_use","id":"tool_1","name":"zoom","input":{"id":0,"n":1}}),
        ],
        vec![json!({"type":"text","text":"I found your exact message."})],
        vec![json!({"type":"text","text":"A fresh turn."})],
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let (ev, mut events) = mpsc::unbounded_channel();
    let task = tokio::spawn(runtime::run(
        Settings {
            integrations_path: None,
            provider,
            directory: dir.path().into(),
            master: "fixture".into(),
            compactor: "fixture".into(),
            instructions: "Keep exact words".into(),
            key: "fixture".into(),
            endpoint,
        },
        rx,
        ev,
    ));
    tx.send(Command::Send("Remember blue".into())).unwrap();
    let mut memory = None;
    for turn in 0..2 {
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(e) = events.recv().await {
                match e {
                    Event::Snapshot(m) => memory = Some(m),
                    Event::Error(e) => panic!("{e}"),
                    Event::Idle => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        if turn == 0 {
            tx.send(Command::Send("What did I say?".into())).unwrap();
        }
    }
    tx.send(Command::Shutdown).unwrap();
    task.await.unwrap();
    server.join().unwrap();
    let store = optchat::memory::Store::open(dir.path()).await.unwrap();
    assert_eq!(
        store
            .memory
            .root
            .iter()
            .filter(|m| m.kind == Kind::User)
            .count(),
        2
    );
    assert!(
        store
            .memory
            .root
            .iter()
            .any(|m| m.kind == Kind::Echo && m.text == "0+0|user: Remember blue")
    );
    assert!(
        store
            .memory
            .root
            .iter()
            .all(|m| !m.text.contains("private reasoning"))
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["system"], requests[2]["system"]);
    assert_eq!(requests[0]["tools"], requests[2]["tools"]);
    assert_eq!(requests[0]["messages"].as_array().unwrap().len(), 1);
    assert_eq!(requests[2]["messages"].as_array().unwrap().len(), 1);
    assert!(
        !requests[0]["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Remember blue")
    );
    assert_eq!(
        requests[1]["messages"][1]["content"][0]["signature"],
        "signed-value"
    );
    assert!(
        requests[2]["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Remember blue")
    );
}

#[tokio::test]
async fn compactor_retries_in_same_conversation_and_keeps_shortest() {
    check_compactor(Provider::Anthropic).await;
}

#[tokio::test]
async fn deepseek_compactor_retries_in_same_conversation_and_keeps_shortest() {
    check_compactor(Provider::DeepSeek).await;
}

async fn check_compactor(provider: Provider) {
    let replies = (0..5)
        .map(|i| vec![json!({"type":"text","text":"x".repeat(530-i)})])
        .collect();
    let (endpoint, requests, server) = server(replies);
    let client = Client::new(provider, "fixture".into(), endpoint);
    let result = client
        .compact(
            "fixture",
            "<chat>\nuser: context\n</chat>".into(),
            "message".repeat(100),
            false,
        )
        .await
        .unwrap();
    assert_eq!(result.len(), 526);
    server.join().unwrap();
    let requests = requests.lock().unwrap();
    for request in requests.iter() {
        if provider == Provider::DeepSeek {
            assert_eq!(request["max_tokens"], 384_000);
            assert_eq!(request["thinking"]["type"], "enabled");
            assert_eq!(request["output_config"]["effort"], "high");
        } else {
            assert_eq!(request["max_tokens"], 32_768);
            assert_eq!(request["thinking"]["type"], "adaptive");
            assert_eq!(request["output_config"]["effort"], "medium");
        }
    }
    assert_eq!(requests[4]["messages"].as_array().unwrap().len(), 9);
    assert!(
        requests[1]["messages"][2]["content"]
            .as_str()
            .unwrap()
            .contains("530 bytes")
    );
    assert!(
        requests[0]["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("user: context")
    );
    assert!(requests[0].get("tools").is_none());
}

#[tokio::test]
async fn cancel_while_waiting_keeps_user_input() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut s = optchat::memory::Store::open(dir.path()).await.unwrap();
        s.message(Kind::Note, "long ".repeat(200)).await.unwrap();
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let (ev, mut events) = mpsc::unbounded_channel();
    let task = tokio::spawn(runtime::run(
        Settings {
            integrations_path: None,
            provider: Provider::Anthropic,
            directory: dir.path().into(),
            master: "fixture".into(),
            compactor: "fixture".into(),
            instructions: String::new(),
            key: String::new(),
            endpoint: "http://127.0.0.1:1".into(),
        },
        rx,
        ev,
    ));
    tx.send(Command::Send("Keep this even if I stop".into()))
        .unwrap();
    tx.send(Command::Cancel).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(e) = events.recv().await {
            if matches!(e, Event::Idle) {
                break;
            }
        }
    })
    .await
    .unwrap();
    tx.send(Command::Shutdown).unwrap();
    task.await.unwrap();
    let s = optchat::memory::Store::open(dir.path()).await.unwrap();
    assert_eq!(s.memory.root[1].text, "Keep this even if I stop");
}

#[tokio::test]
async fn completed_blocks_are_forwarded_for_durable_logging() {
    let (endpoint, _, server) = server(vec![vec![json!({"type":"text","text":"hello"})]]);
    let (tx, mut rx) = mpsc::unbounded_channel();
    Client::new(Provider::Anthropic, "fixture".into(), endpoint)
        .ask(
            "fixture",
            "system",
            &[json!({"role":"user","content":"hi"})],
            &[],
            Some(tx),
        )
        .await
        .unwrap();
    assert!(matches!(rx.recv().await,Some(StreamEvent::Text(s)) if s=="hello"));
    assert!(matches!(rx.recv().await,Some(StreamEvent::Block(b)) if b["text"]=="hello"));
    server.join().unwrap();
}

#[tokio::test]
async fn provider_requests_use_distinct_thinking_and_cache_settings() {
    for provider in [Provider::Anthropic, Provider::DeepSeek] {
        let (endpoint, requests, server) =
            server(vec![vec![json!({"type":"text","text":"hello"})]]);
        let view = "summary line\n".repeat(10_000);
        let blocks = optchat::model::cache_blocks(&view);
        let messages = vec![json!({"role":"user","content":blocks})];
        let response = Client::new(provider, "fixture".into(), endpoint)
            .ask(
                provider.default_model(),
                "system",
                &messages,
                &optchat::model::tools(),
                None,
            )
            .await
            .unwrap();
        server.join().unwrap();
        let requests = requests.lock().unwrap();
        let request = &requests[0];
        assert_eq!(response.usage.cache_read, Some(50));
        assert_eq!(response.usage.output, Some(20));
        assert_eq!(request["model"], provider.default_model());
        assert_eq!(request["tools"], json!(optchat::model::tools()));
        assert!(messages[0]["content"][0].get("cache_control").is_some());
        let sent = request["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            sent.iter()
                .map(|b| b["text"].as_str().unwrap())
                .collect::<String>(),
            view
        );
        match provider {
            Provider::Anthropic => {
                assert_eq!(request["thinking"]["type"], "adaptive");
                assert!(request.get("cache_control").is_some());
                assert!(sent[0].get("cache_control").is_some());
            }
            Provider::DeepSeek => {
                assert_eq!(request["thinking"]["type"], "enabled");
                assert_eq!(request["output_config"]["effort"], "high");
                assert!(request.get("cache_control").is_none());
                assert!(sent.iter().all(|b| b.get("cache_control").is_none()));
            }
        }
    }
}

#[tokio::test]
async fn missing_key_names_the_selected_provider() {
    for provider in [Provider::Anthropic, Provider::DeepSeek] {
        let error = Client::new(provider, String::new(), "http://127.0.0.1:1".into())
            .ask("fixture", "system", &[], &[], None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(provider.key_variable()));
    }
    assert!(Provider::parse("typo").is_err());
    assert_eq!(Provider::parse("deepseek").unwrap(), Provider::DeepSeek);
}

#[test]
fn usage_preserves_unavailable_counters_and_deepseek_cache_hits() {
    let usage = Usage::from_api(
        &json!({"prompt_tokens":150,"completion_tokens":20,"prompt_cache_hit_tokens":100,"prompt_cache_miss_tokens":50}),
    );
    assert_eq!(usage.input, Some(150));
    assert_eq!(usage.cache_read, Some(100));
    assert_eq!(usage.cache_write, None);
    assert!(usage.display().contains("cache write —"));
    assert_eq!(Usage::from_api(&json!({})).input, None);
}

#[tokio::test]
async fn provider_cannot_change_while_a_turn_waits_for_memory() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store = optchat::memory::Store::open(dir.path()).await.unwrap();
        store
            .message(Kind::Note, "pending summary ".repeat(100))
            .await
            .unwrap();
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let (ev, mut events) = mpsc::unbounded_channel();
    let task = tokio::spawn(runtime::run(
        Settings {
            integrations_path: None,
            provider: Provider::Anthropic,
            directory: dir.path().into(),
            master: "fixture".into(),
            compactor: "fixture".into(),
            instructions: String::new(),
            key: String::new(),
            endpoint: "http://127.0.0.1:1".into(),
        },
        rx,
        ev,
    ));
    tx.send(Command::Send("Waiting message".into())).unwrap();
    tx.send(Command::Models {
        api_key: None,
        provider: Provider::DeepSeek,
        master: "deepseek-flash".into(),
        compactor: "deepseek-flash".into(),
    })
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            match event {
                Event::ProviderApplied(provider, _) => assert_eq!(provider, Provider::Anthropic),
                Event::Error(error) if error.contains("before changing providers") => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    tx.send(Command::Shutdown).unwrap();
    task.await.unwrap();
    let store = optchat::memory::Store::open(dir.path()).await.unwrap();
    assert_eq!(store.memory.root[1].text, "Waiting message");
}
