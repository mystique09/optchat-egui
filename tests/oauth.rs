use optchat::{
    integrations::{Config, Integrations, Server},
    oauth,
};
use std::{collections::BTreeMap, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn oauth_sign_in_refresh_restart_sign_out_and_callback_state() {
    let _ = keyring::Entry::store_status();
    keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("oauth.log");
    let mut child = tokio::process::Command::new("python3")
        .args([
            format!(
                "{}/tests/fixtures/mcp_server.py",
                env!("CARGO_MANIFEST_DIR")
            ),
            "oauth".into(),
            log.display().to_string(),
            "oauth".into(),
        ])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut port = String::new();
    stdout.read_line(&mut port).await.unwrap();
    let url = format!("http://127.0.0.1:{}/mcp", port.trim());
    let oauth_config = oauth::Config::default();
    let mut manager = oauth::manager(&url, &oauth_config).await.unwrap();
    assert!(!manager.initialize_from_store().await.unwrap());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let login_url = url.clone();
    let task = tokio::spawn(async move {
        oauth::sign_in(&login_url, &oauth::Config::default(), |url| {
            tx.send(url).unwrap();
        })
        .await
    });
    let authorization = reqwest::Url::parse(
        &tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let redirect = authorization
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    let http = reqwest::Client::new();
    let invalid = http
        .get(format!("{redirect}?code=bad&state=wrong"))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    assert!(!task.is_finished());
    let browser = http
        .get(authorization)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(browser.contains("Signed in"), "{browser}");
    task.await.unwrap().unwrap();
    // A new manager restores credentials and refreshes an expired token.
    let mut manager = oauth::manager(&url, &oauth_config).await.unwrap();
    assert!(manager.initialize_from_store().await.unwrap());
    assert_eq!(manager.get_access_token().await.unwrap(), "refreshed-token");
    let config = Config {
        skill_directories: vec![],
        timeout_seconds: 5,
        servers: BTreeMap::from([(
            "fixture".into(),
            Server::Http {
                url: url.clone(),
                bearer_token_env: None,
                oauth: Some(oauth_config.clone()),
            },
        )]),
    };
    let integrations = Integrations::connect(config).await;
    assert!(
        integrations
            .status
            .iter()
            .any(|s| s == "fixture: connected, 2 tools"),
        "{:?}",
        integrations.status
    );
    drop(integrations);
    let requests = std::fs::read_to_string(&log).unwrap();
    assert_eq!(requests.matches("\"exchange\"").count(), 1);
    assert_eq!(requests.matches("\"refresh\"").count(), 1);
    // Changing resource URL cannot reuse this grant.
    let mut other = oauth::manager(&format!("{url}/other"), &oauth_config)
        .await
        .unwrap();
    assert!(!other.initialize_from_store().await.unwrap());
    oauth::sign_out(&url, &oauth_config).await.unwrap();
    let mut manager = oauth::manager(&url, &oauth_config).await.unwrap();
    assert!(!manager.initialize_from_store().await.unwrap());

    // Denied consent completes with an error and leaves no saved authorization.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let denied_url = url.clone();
    let denied = tokio::spawn(async move {
        oauth::sign_in(&denied_url, &oauth::Config::default(), |url| {
            tx.send(url).unwrap();
        })
        .await
    });
    let authorization = reqwest::Url::parse(&rx.await.unwrap()).unwrap();
    let params: BTreeMap<_, _> = authorization
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut callback = reqwest::Url::parse(&params["redirect_uri"]).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("state", &params["state"])
        .append_pair("error", "access_denied");
    let browser = http
        .get(callback)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(browser.contains("Sign-in failed"));
    assert!(
        denied
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("declined")
    );
    assert!(
        !oauth::manager(&url, &oauth_config)
            .await
            .unwrap()
            .initialize_from_store()
            .await
            .unwrap()
    );

    // Cancellation closes the callback listener; no token exchange is attempted.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        oauth::sign_in(&url, &oauth::Config::default(), |url| {
            tx.send(url).unwrap();
        })
        .await
    });
    let authorization = reqwest::Url::parse(&rx.await.unwrap()).unwrap();
    let redirect = authorization
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    task.abort();
    let _ = task.await;
    assert!(http.get(redirect).send().await.is_err());
    child.kill().await.unwrap();
}

#[test]
fn oauth_requires_secure_transport_and_exclusive_auth_mode() {
    for url in [
        "https://mcp.example.com/mcp",
        "http://127.0.0.1:8000/mcp",
        "http://[::1]:8000/mcp",
    ] {
        oauth::validate_url(url).unwrap();
    }
    for url in [
        "http://mcp.example.com/mcp",
        "https://user:secret@example.com/mcp",
        "file:///tmp/server",
    ] {
        assert!(oauth::validate_url(url).is_err());
    }
    assert!(Config::parse(r#"{"servers":{"test":{"transport":"http","url":"https://example.com/mcp","bearer_token_env":"TOKEN","oauth":{}}}}"#).is_err());
}
