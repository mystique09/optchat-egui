use optchat::{
    credentials,
    model::Provider,
    runtime::{self, Command, Event, Settings},
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Duration,
};
use tokio::sync::mpsc;

#[tokio::test]
async fn saved_keys_reload_and_failed_replacement_preserves_active_key() {
    // This test binary uses only an in-memory credential store.
    let _ = keyring::Entry::store_status();
    keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
    credentials::save(Provider::Anthropic, "fixture-anthropic".into())
        .await
        .unwrap();
    credentials::save(Provider::DeepSeek, "fixture-old".into())
        .await
        .unwrap();
    assert_eq!(
        credentials::load(Provider::Anthropic).await.unwrap(),
        "fixture-anthropic"
    );
    assert_eq!(
        credentials::load(Provider::DeepSeek).await.unwrap(),
        "fixture-old"
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut bytes = [0; 4096];
        while !request.windows(4).any(|b| b == b"\r\n\r\n") {
            let n = socket.read(&mut bytes).unwrap();
            assert!(n > 0);
            request.extend_from_slice(&bytes[..n]);
        }
        let body = "data: {\"type\":\"message_stop\"}\n\n";
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        String::from_utf8(request).unwrap()
    });
    let directory = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let worker = tokio::spawn(runtime::run(
        Settings {
            provider: Provider::DeepSeek,
            directory: directory.path().into(),
            master: "fixture".into(),
            compactor: "fixture".into(),
            instructions: String::new(),
            key: "fixture-old".into(),
            endpoint,
        },
        rx,
        events_tx,
    ));
    let apply = |key: &str| Command::Models {
        provider: Provider::DeepSeek,
        master: "fixture".into(),
        compactor: "fixture".into(),
        api_key: Some(key.into()),
    };
    tx.send(apply("fixture-new")).unwrap();
    wait(&mut events, true).await;
    assert_eq!(
        credentials::load(Provider::DeepSeek).await.unwrap(),
        "fixture-new"
    );

    let entry = keyring::Entry::new("local.optchat.providers", "DEEPSEEK_API_KEY").unwrap();
    let mock: &keyring_core::mock::Cred = entry.inner.as_any().downcast_ref().unwrap();
    mock.set_error(keyring_core::Error::NoEntry);
    tx.send(apply("fixture-rejected")).unwrap();
    wait(&mut events, false).await;
    assert_eq!(
        credentials::load(Provider::DeepSeek).await.unwrap(),
        "fixture-new"
    );
    assert_eq!(
        credentials::load(Provider::Anthropic).await.unwrap(),
        "fixture-anthropic"
    );
    tx.send(Command::Send("hello".into())).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if matches!(event, Event::Idle) {
                break;
            }
        }
    })
    .await
    .unwrap();
    tx.send(Command::Shutdown).unwrap();
    worker.await.unwrap();
    let request = server.join().unwrap();
    assert!(request.contains("x-api-key: fixture-new\r\n"));
    assert!(!request.contains("fixture-rejected"));
}

async fn wait(events: &mut mpsc::UnboundedReceiver<Event>, success: bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            match event {
                Event::SettingsApplied => {
                    assert!(success);
                    return;
                }
                Event::SettingsRejected => {
                    assert!(!success);
                    return;
                }
                _ => {}
            }
        }
        panic!("worker stopped before applying settings");
    })
    .await
    .unwrap();
}
