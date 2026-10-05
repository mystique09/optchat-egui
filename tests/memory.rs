use chrono::Local;
use optchat::{
    memory::{Kind, Memory, Message, Node, Part, Store},
    model,
};

fn message(i: usize, text: &str) -> Message {
    Message {
        i,
        kind: Kind::User,
        text: text.into(),
        size: 6 + text.len(),
        date: Local::now(),
    }
}

#[tokio::test]
async fn durable_history_lock_and_reload() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).await.unwrap();
    assert!(Store::open(dir.path()).await.is_err());
    s.message(Kind::User, "Keep my exact words\n你好".into())
        .await
        .unwrap();
    s.node(
        Part { l: 0, i: 0 },
        "user: Keep my exact words\n你好".into(),
    )
    .await
    .unwrap();
    s.node(Part { l: 0, i: 0 }, "replacement".into())
        .await
        .unwrap();
    drop(s);
    let s = Store::open(dir.path()).await.unwrap();
    assert_eq!(s.memory.root[0].text, "Keep my exact words\n你好");
    assert_eq!(s.memory.zoom(0, 1), "0+0|user: Keep my exact words\n你好");
    assert_eq!(
        s.memory.text(Part { l: 0, i: 0 }),
        "user: Keep my exact words\n你好"
    );
    assert!(s.memory.settled());
}

#[tokio::test]
async fn torn_tail_is_reported_separated_and_next_append_survives() {
    use tokio::io::AsyncWriteExt;
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).await.unwrap();
    s.message(Kind::User, "first".into()).await.unwrap();
    drop(s);
    let path = dir
        .path()
        .join("main")
        .join(format!("{}.jsonl", Local::now().format("%Y-%m-%d")));
    let mut f = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .await
        .unwrap();
    f.write_all(b"{\"i\":1,").await.unwrap();
    f.flush().await.unwrap();
    drop(f);
    let mut s = Store::open(dir.path()).await.unwrap();
    assert_eq!(s.warnings.len(), 1);
    s.message(Kind::Talk, "second".into()).await.unwrap();
    drop(s);
    let s = Store::open(dir.path()).await.unwrap();
    assert_eq!(s.memory.root.len(), 2);
    assert_eq!(s.memory.root[1].text, "second");
}

#[tokio::test]
async fn persistence_failure_does_not_publish_a_message_or_node() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).await.unwrap();
    let path = dir
        .path()
        .join("main")
        .join(format!("{}.jsonl", Local::now().format("%Y-%m-%d")));
    tokio::fs::create_dir(&path).await.unwrap();
    assert!(s.message(Kind::User, "lost?".into()).await.is_err());
    assert!(s.memory.root.is_empty());
    tokio::fs::remove_dir(path).await.unwrap();
    s.message(Kind::User, "saved".into()).await.unwrap();
    let path = dir
        .path()
        .join("tree")
        .join(format!("{}.jsonl", Local::now().format("%Y-%m-%d")));
    tokio::fs::create_dir(&path).await.unwrap();
    assert!(
        s.node(Part { l: 0, i: 0 }, "user: saved".into())
            .await
            .is_err()
    );
    assert!(!s.memory.settled());
    tokio::fs::remove_dir(path).await.unwrap();
    s.node(Part { l: 0, i: 0 }, "user: saved".into())
        .await
        .unwrap();
    assert!(s.memory.settled());
}

#[test]
fn ordered_compaction_and_incremental_view() {
    let mut m = Memory {
        budget: 35,
        ..Memory::default()
    };
    for i in 0..8 {
        m.push(message(i, "hello"));
    }
    assert_eq!(m.eligible(), vec![Part { l: 0, i: 0 }]);
    for i in 0..8 {
        let p = Part { l: 0, i };
        assert!(m.eligible().contains(&p));
        assert!(!m.context(p).contains("not summarized"));
        m.tree.insert(
            p,
            Node {
                l: 0,
                i,
                text: format!("user: hello {i}"),
                size: 13,
            },
        );
        m.fit();
    }
    assert!(m.bytes() > m.budget);
    for i in 0..4 {
        m.tree.insert(
            Part { l: 1, i },
            Node {
                l: 1,
                i,
                text: "user: hello pair".into(),
                size: 16,
            },
        );
    }
    m.fit();
    assert_eq!(
        m.view,
        vec![
            Part { l: 1, i: 0 },
            Part { l: 1, i: 1 },
            Part { l: 1, i: 2 },
            Part { l: 1, i: 3 }
        ]
    );
    m.tree.insert(
        Part { l: 2, i: 0 },
        Node {
            l: 2,
            i: 0,
            text: "user: first four".into(),
            size: 16,
        },
    );
    m.fit();
    assert_eq!(m.view[0], Part { l: 2, i: 0 });
    let prefix = m.view[0];
    m.push(message(8, "new"));
    assert_eq!(m.view[0], prefix);
    assert_eq!(m.zoom(1, 2), "No line 1+2.");
    assert_eq!(m.zoom(0, 0), "No line 0+0.");
    assert_eq!(m.zoom(usize::MAX, 2), format!("No line {}+2.", usize::MAX));
    assert!(m.zoom(0, 4).starts_with("0+2|"));
    assert!(m.zoom(0, 1).ends_with("user: hello"));
}

#[test]
fn cache_marks_are_utf8_safe_stable_line_boundaries() {
    let text = format!("<chat>\n{}\n</chat>", "你好 and text\n".repeat(12_000));
    let blocks = model::cache_blocks(&text);
    assert_eq!(
        blocks
            .iter()
            .filter(|b| b.get("cache_control").is_some())
            .count(),
        3
    );
    assert_eq!(
        blocks
            .iter()
            .map(|b| b["text"].as_str().unwrap())
            .collect::<String>(),
        text
    );
    for block in &blocks[..3] {
        assert!(block["text"].as_str().unwrap().ends_with('\n'));
    }
    assert_eq!(model::byte_prefix("你好", 4), "你");
    assert_eq!(model::scale().len(), 512);
    let capped = model::cap(&"你".repeat(40_000));
    assert_eq!(capped.chars().count(), 30_000);
    assert!(capped.contains("characters omitted"));
}

#[test]
fn html_escapes_untrusted_messages() {
    let mut m = Memory::default();
    m.push(message(0, "<script>alert(1)</script>"));
    let html = m.html();
    assert!(!html.contains("<script>"));
    assert!(html.contains("&lt;script&gt;"));
    let view = html.split("<h2>ROOT</h2>").next().unwrap();
    assert!(view.contains("0+1"));
    assert!(view.contains("bytes"));
    assert!(view.contains(&m.date(0)));
}
