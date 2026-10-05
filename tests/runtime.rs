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

fn server(
    replies: Vec<Vec<Value>>,
) -> (String, Arc<Mutex<Vec<Value>>>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/messages", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(vec![]));
    let seen = requests.clone();
    let handle = std::thread::spawn(move || {
        for blocks in replies {
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
            seen.lock()
                .unwrap()
                .push(serde_json::from_slice(&data[head..head + len]).unwrap());
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
