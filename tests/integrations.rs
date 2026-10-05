use optchat::{
    integrations::{Config, Integrations, Server},
    skills,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

fn fixture() -> String {
    format!(
        "{}/tests/fixtures/mcp_server.py",
        env!("CARGO_MANIFEST_DIR")
    )
}
fn stdio(log: &Path, label: &str) -> Server {
    Server::Stdio {
        command: "python3".into(),
        args: vec![
            fixture(),
            "stdio".into(),
            log.display().to_string(),
            label.into(),
        ],
        cwd: None,
        env_from: BTreeMap::new(),
    }
}
fn tool(integrations: &Integrations, server: &str, name: &str) -> String {
    integrations
        .definitions
        .iter()
        .find(|t| {
            t["description"]
                .as_str()
                .unwrap_or("")
                .starts_with(&format!("MCP server {server}, tool {name}."))
        })
        .unwrap()["name"]
        .as_str()
        .unwrap()
        .into()
}
fn calls(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect()
}

#[tokio::test]
async fn stdio_routes_multiple_servers_paginates_and_requires_approval() {
    let dir = tempfile::tempdir().unwrap();
    let left = dir.path().join("left");
    let right = dir.path().join("right");
    let config = Config {
        skill_directories: vec![],
        servers: BTreeMap::from([
            ("left".into(), stdio(&left, "left")),
            ("right".into(), stdio(&right, "right")),
            (
                "broken".into(),
                Server::Stdio {
                    command: "/no/such/command".into(),
                    args: vec![],
                    cwd: None,
                    env_from: BTreeMap::new(),
                },
            ),
        ]),
        timeout_seconds: 1,
    };
    let integrations = Arc::new(Integrations::connect(config).await);
    assert!(integrations.status.iter().any(|s| s.starts_with("broken:")));
    let left_tool = tool(&integrations, "left", "echo");
    let right_tool = tool(&integrations, "right", "echo");
    assert_ne!(left_tool, right_tool);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let i = integrations.clone();
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let task =
        tokio::spawn(async move { i.call(&left_tool, json!({"value":"denied"}), &tx, &c).await });
    rx.recv().await.unwrap().answer.send(false).unwrap();
    assert!(task.await.unwrap().error);
    assert!(calls(&left).iter().all(|m| m["method"] != "tools/call"));
    for (alias, value, expected_error) in [
        (right_tool, "exact", false),
        (tool(&integrations, "left", "fail"), "failed", true),
        (tool(&integrations, "left", "echo"), "stall", true),
    ] {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let i = integrations.clone();
        let task = tokio::spawn(async move {
            i.call(
                &alias,
                json!({"value":value}),
                &tx,
                &CancellationToken::new(),
            )
            .await
        });
        let approval = rx.recv().await.unwrap();
        assert_eq!(approval.arguments["value"], value);
        approval.answer.send(true).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(4), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.error, expected_error);
        if value == "exact" {
            assert!(result.text.contains("right:exact"));
            assert!(result.text.contains("Structured result"));
            assert!(result.text.contains("not rendered"));
        }
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while !calls(&left)
            .iter()
            .any(|m| m["method"] == "notifications/cancelled")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        calls(&right)
            .iter()
            .filter(|m| m["method"] == "tools/call")
            .count(),
        1
    );
    // Canceling while waiting for permission must never send a call.
    let before = calls(&right).len();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let i = integrations.clone();
    let alias = tool(&i, "right", "echo");
    let c = cancel.clone();
    let task = tokio::spawn(async move { i.call(&alias, json!({}), &tx, &c).await });
    let approval = rx.recv().await.unwrap();
    cancel.cancel();
    assert!(task.await.unwrap().error);
    assert!(approval.answer.is_closed());
    assert_eq!(calls(&right).len(), before);
    // Cancel an actual request, after the fixture confirms receipt.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let i = integrations.clone();
    let alias = tool(&i, "right", "echo");
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let task = tokio::spawn(async move { i.call(&alias, json!({"value":"stall"}), &tx, &c).await });
    rx.recv().await.unwrap().answer.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !calls(&right)
            .iter()
            .any(|m| m["params"]["arguments"]["value"] == "stall")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    assert!(task.await.unwrap().error);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !calls(&right)
            .iter()
            .any(|m| m["method"] == "notifications/cancelled")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[test]
#[ignore = "read-only check against locally installed skills"]
fn installed_skill_catalog() {
    let (skills, warnings) = skills::discover(&Config::default().skill_directories);
    eprintln!(
        "Discovered {} installed skills; {} warnings",
        skills.len(),
        warnings.len()
    );
    for warning in warnings {
        eprintln!("{warning}");
    }
    assert!(!skills.is_empty());
    let selected = skills.iter().find(|s| s.name == "linear").unwrap();
    assert!(
        skills::read(&skills, &selected.id, "SKILL.md", 0)
            .unwrap()
            .contains("Linear")
    );
}

#[tokio::test]
async fn streamable_http_accepts_json_and_sse() {
    for mode in ["http", "sse"] {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("requests");
        let mut child = tokio::process::Command::new("python3")
            .args([
                fixture(),
                mode.into(),
                log.display().to_string(),
                mode.into(),
            ])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut port = String::new();
        stdout.read_line(&mut port).await.unwrap();
        let config = Config {
            skill_directories: vec![],
            servers: BTreeMap::from([(
                "remote".into(),
                Server::Http {
                    oauth: None,
                    url: format!("http://127.0.0.1:{}", port.trim()),
                    bearer_token_env: None,
                },
            )]),
            timeout_seconds: 3,
        };
        let integrations = Arc::new(Integrations::connect(config).await);
        assert!(
            integrations
                .status
                .iter()
                .any(|s| s == "remote: connected, 2 tools"),
            "{:?}",
            integrations.status
        );
        let alias = tool(&integrations, "remote", "echo");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            integrations
                .call(
                    &alias,
                    json!({"value":"transport"}),
                    &tx,
                    &CancellationToken::new(),
                )
                .await
        });
        rx.recv().await.unwrap().answer.send(true).unwrap();
        let result = task.await.unwrap();
        assert!(!result.error);
        assert!(result.text.contains(&format!("{mode}:transport")));
        child.kill().await.unwrap();
    }
}

#[test]
fn skill_discovery_and_reference_loading_preserve_text_and_confine_paths() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("nested/first");
    std::fs::create_dir_all(&first).unwrap();
    let text = "---\nname: example\ndescription: |\n  A multiline description.\n  With punctuation: yes.\n---\nFollow these instructions.\n";
    std::fs::write(first.join("SKILL.md"), text).unwrap();
    std::fs::write(first.join("reference.md"), "你".repeat(13_000)).unwrap();
    let broken = dir.path().join("broken");
    std::fs::create_dir(&broken).unwrap();
    std::fs::write(broken.join("SKILL.md"), "bad header").unwrap();
    let duplicate = dir.path().join("second");
    std::fs::create_dir(&duplicate).unwrap();
    std::fs::write(duplicate.join("SKILL.md"), text).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&first, dir.path().join("alias")).unwrap();
    let (skills, warnings) = skills::discover(&[dir.path().into()]);
    assert_eq!(skills.len(), 2);
    assert_eq!(skills[0].name, skills[1].name);
    assert_ne!(skills[0].id, skills[1].id);
    assert_eq!(warnings.len(), 1);
    let loaded: Value =
        serde_json::from_str(&skills::read(&skills, &skills[0].id, "SKILL.md", 0).unwrap())
            .unwrap();
    assert_eq!(loaded["text"], text);
    let first_page: Value =
        serde_json::from_str(&skills::read(&skills, &skills[0].id, "reference.md", 0).unwrap())
            .unwrap();
    assert_eq!(first_page["next_offset"], 12_000);
    let tail: Value = serde_json::from_str(
        &skills::read(&skills, &skills[0].id, "reference.md", 12_000).unwrap(),
    )
    .unwrap();
    assert_eq!(tail["text"].as_str().unwrap().chars().count(), 1000);
    assert!(tail["next_offset"].is_null());
    assert!(skills::read(&skills, &skills[0].id, "../outside", 0).is_err());
    assert!(skills::read(&skills, &skills[0].id, "/etc/passwd", 0).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/passwd", first.join("escape")).unwrap();
        assert!(skills::read(&skills, &skills[0].id, "escape", 0).is_err());
    }
    assert_eq!(
        Config::default().skill_directories,
        vec![std::path::PathBuf::from("~/.agents/skills")]
    );
    assert!(skills::discover(&[dir.path().join("missing")]).0.is_empty());
}

#[tokio::test]
async fn config_round_trip_and_validation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("integrations.json");
    let config = Config::load(&path).await.unwrap();
    config.save(&path).await.unwrap();
    assert_eq!(
        Config::load(&path).await.unwrap().skill_directories,
        config.skill_directories
    );
    assert!(Config::parse(r#"{"timeout_seconds":0}"#).is_err());
    assert!(
        Config::parse(r#"{"servers":{"bad":{"transport":"http","url":"file:///etc/passwd"}}}"#)
            .is_err()
    );
    assert!(Config::parse(r#"{"unexpected":true}"#).is_err());
}
#[tokio::test]
async fn settings_merge_preserves_external_servers_and_rejects_conflicts() {
    use optchat::integrations::Config;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("integrations.json");
    let base = Config::parse(
        r#"{"servers":{"original":{"transport":"http","url":"https://example.com/mcp"}}}"#,
    )
    .unwrap();
    base.save(&path).await.unwrap();
    let mut external = base.clone();
    external.servers.insert(
        "linear".into(),
        optchat::integrations::Server::Http {
            url: "https://mcp.linear.app/mcp".into(),
            bearer_token_env: None,
            oauth: None,
        },
    );
    external.save(&path).await.unwrap();
    let mut draft = base.clone();
    draft.timeout_seconds = 42;
    let saved = draft.save_merged(&path, &base).await.unwrap();
    assert!(saved.servers.contains_key("linear"));
    assert_eq!(saved.timeout_seconds, 42);
    let mut conflicting = base.clone();
    conflicting.timeout_seconds = 55;
    assert!(conflicting.save_merged(&path, &base).await.is_err());
    assert_eq!(Config::load(&path).await.unwrap().timeout_seconds, 42);
    let mut removed = saved.clone();
    removed.servers.remove("original");
    let saved = removed.save_merged(&path, &saved).await.unwrap();
    assert!(!saved.servers.contains_key("original"));
    assert!(saved.servers.contains_key("linear"));
}
