use crate::{
    JOBS, Result,
    memory::{Kind, Memory, Part, Store},
    model::{self, Client, Provider, Response, StreamEvent, Usage},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};

pub enum Command {
    Send(String),
    Cancel,
    Models {
        provider: Provider,
        master: String,
        compactor: String,
        api_key: Option<String>,
    },
    Export(PathBuf),
    Import(PathBuf),
    Shutdown,
}
pub enum Event {
    Snapshot(Memory),
    Status(String),
    Error(String),
    Text(String),
    Thought(String),
    Usage(Usage),
    ProviderApplied(Provider, bool),
    SettingsApplied,
    SettingsRejected,
    Idle,
}
pub struct Settings {
    pub provider: Provider,
    pub directory: PathBuf,
    pub master: String,
    pub compactor: String,
    pub instructions: String,
    pub key: String,
    pub endpoint: String,
}
struct Turn {
    messages: Vec<Value>,
    task: Option<JoinHandle<()>>,
    pending: Vec<String>,
}
enum Finished {
    Node(Part, Result<String>),
    Step(Result<Response>),
}

pub async fn run(
    settings: Settings,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<Event>,
) {
    if let Err(e) = actor(settings, &mut commands, &events).await {
        let _ = events.send(Event::Error(e.to_string()));
        let _ = events.send(Event::Idle);
        let _ = events.send(Event::Status(
            "Memory worker stopped · restart after resolving the error".into(),
        ));
    }
}
async fn actor(
    settings: Settings,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    let mut store = Store::open(&settings.directory).await?;
    for warning in &store.warnings {
        let _ = events.send(Event::Error(warning.clone()));
    }
    let mut provider = settings.provider;
    let mut client = Client::new(provider, settings.key.clone(), settings.endpoint);
    let _ = events.send(Event::ProviderApplied(
        provider,
        !settings.key.trim().is_empty(),
    ));
    let system = format!(
        "{}\n\n{}",
        include_str!("master.txt"),
        settings.instructions
    );
    let mut master = settings.master;
    let mut compactor = settings.compactor;
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();
    let mut busy: BTreeMap<Part, JoinHandle<()>> = BTreeMap::new();
    let mut retry: BTreeMap<Part, Instant> = BTreeMap::new();
    let mut failed = BTreeSet::new();
    let mut queue = VecDeque::<String>::new();
    let mut turn: Option<Turn> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut changed = true;
    let _ = events.send(Event::Status("Ready".into()));
    loop {
        let mut free_built = false;
        for p in store.memory.eligible() {
            if busy.len() >= JOBS {
                break;
            }
            if busy.contains_key(&p) || retry.get(&p).is_some_and(|at| *at > Instant::now()) {
                continue;
            }
            let source = store.memory.source(p).expect("eligible sources");
            if source.len() <= crate::NODE {
                store.node(p, source).await?;
                changed = true;
                free_built = true;
                continue;
            }
            let context = store.memory.context(p);
            let source = if p.l > 0 {
                p.children()
                    .iter()
                    .map(|child| store.memory.text(*child).replace(['\n', '\r'], " "))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                source
            };
            let client = client.clone();
            let model = compactor.clone();
            let tx = done_tx.clone();
            busy.insert(
                p,
                tokio::spawn(async move {
                    let result = client.compact(&model, context, source, p.l > 0).await;
                    let _ = tx.send(Finished::Node(p, result));
                }),
            );
        }
        if turn.is_none() && !queue.is_empty() && store.memory.settled() {
            let view = store.memory.render();
            let texts: Vec<_> = queue.drain(..).collect();
            for text in &texts {
                store.message(Kind::User, text.clone()).await?;
            }
            let mut blocks = model::cache_blocks(&view);
            blocks.push(json!({"type":"text","text":texts.join("\n\n")}));
            let messages = vec![json!({"role":"user","content":blocks})];
            let task = step(&client, &master, &system, &messages, &done_tx, &stream_tx);
            turn = Some(Turn {
                messages,
                task: Some(task),
                pending: vec![],
            });
            let _ = events.send(Event::Status("Thinking".into()));
            changed = true;
        }
        if changed {
            let _ = events.send(Event::Snapshot(store.memory.clone()));
            changed = false;
        }
        tokio::select! {
            _ = async {}, if free_built => {},
            command = commands.recv() => match command {
                Some(Command::Send(text)) if !text.trim().is_empty() => {
                    if let Some(t) = &mut turn { t.pending.push(text); let _ = events.send(Event::Status("Message queued for the next tool boundary".into())); }
                    else { queue.push_back(text); let _ = events.send(Event::Status("Waiting for memory to finish summarizing".into())); }
                }
                Some(Command::Cancel) => {
                    if let Some(mut t) = turn.take() { if let Some(task) = t.task.take() { task.abort(); let _ = task.await; } for text in t.pending { store.message(Kind::User,text).await?; } }
                    while let Ok(event) = stream_rx.try_recv() { record(event, &mut store, events).await?; }
                    while let Some(text) = queue.pop_front() { store.message(Kind::User,text).await?; }
                    // A response may already be queued when cancellation wins the select.
                    while let Ok(result) = done_rx.try_recv() { if let Finished::Node(p,result) = result { complete_node(p,result,&mut store,&mut busy,&mut retry,&mut failed,events).await?; } }
                    let _ = events.send(Event::Idle); let _ = events.send(Event::Status("Stopped · unsent messages preserved in history".into())); changed = true;
                }
                Some(Command::Models { provider: next, master: m, compactor: c, api_key }) => {
                    if turn.is_some() || !queue.is_empty() {
                        let _ = events.send(Event::Error("Stop or finish the current turn before changing providers/models.".into()));
                        let _ = events.send(Event::SettingsRejected);
                    } else if m.trim().is_empty() || c.trim().is_empty() {
                        let _ = events.send(Event::Error("Both model names are required.".into()));
                        let _ = events.send(Event::SettingsRejected);
                    } else {
                        let key = if let Some(key) = api_key {
                            crate::credentials::save(next, key).await.map(Some)
                        } else if next != provider {
                            crate::credentials::load(next).await.map(Some)
                        } else { Ok(None) };
                        let key = match key {
                            Ok(key) => key,
                            Err(error) => { let _ = events.send(Event::Error(error.to_string())); let _ = events.send(Event::SettingsRejected); continue; }
                        };
                        if let Some(key) = key {
                            let connected = !key.trim().is_empty();
                            if next == provider { client.set_key(key); }
                            else { client = Client::new(next, key, next.endpoint().into()); }
                            provider = next;
                            let _ = events.send(Event::ProviderApplied(provider, connected));
                        }
                        master = m; compactor = c; retry.clear(); failed.clear();
                        let _ = events.send(Event::SettingsApplied);
                        let _ = events.send(Event::Status(format!("{} models applied",provider.label())));
                    }
                }
                Some(Command::Export(path)) => {
                    let result = async { let mut f = tokio::fs::File::create(&path).await?; tokio::io::AsyncWriteExt::write_all(&mut f, store.memory.html().as_bytes()).await?; f.sync_all().await }.await;
                    let _ = events.send(match result { Ok(()) => Event::Status(format!("Exported {}",path.display())), Err(e) => Event::Error(e.to_string()) });
                }
                Some(Command::Import(path)) => {
                    if turn.is_some() || !queue.is_empty() { let _ = events.send(Event::Error("Import when the chat is idle.".into())); }
                    else {
                        match tokio::fs::read_to_string(&path).await {
                            Ok(text) => { for line in text.lines().filter(|l| !l.trim().is_empty()) { store.message(Kind::Note,line.to_owned()).await?; } changed = true; }
                            Err(e) => { let _ = events.send(Event::Error(e.to_string())); }
                        }
                    }
                }
                Some(Command::Shutdown) | None => {
                    if let Some(mut t) = turn.take() { if let Some(task) = t.task.take() { task.abort(); let _ = task.await; } for text in t.pending { store.message(Kind::User,text).await?; } }
                    while let Ok(event) = stream_rx.try_recv() { record(event,&mut store,events).await?; }
                    for text in queue { store.message(Kind::User,text).await?; }
                    for (_,task) in busy { task.abort(); }
                    return Ok(());
                }
                _ => {},
            },
            Some(event) = stream_rx.recv() => { let persisted = matches!(event,StreamEvent::Block(_)); record(event,&mut store,events).await?; changed |= persisted; },
            Some(result) = done_rx.recv() => match result {
                Finished::Node(p,result) => { complete_node(p,result,&mut store,&mut busy,&mut retry,&mut failed,events).await?; changed = true; },
                Finished::Step(result) => {
                    // Both channels are FIFO individually; drain completed blocks before results.
                    while let Ok(event) = stream_rx.try_recv() { record(event,&mut store,events).await?; }
                    if let Some(mut t) = turn.take() {
                        t.task = None;
                        match result {
                            Ok(response) => {
                                let _ = events.send(Event::Usage(response.usage));
                                t.messages.push(json!({"role":"assistant","content":response.content}));
                                let mut results = vec![];
                                for block in &response.content {
                                    if block["type"] != "tool_use" { continue; }
                                    let name = block["name"].as_str().unwrap_or("");
                                    let id = block["input"]["id"].as_u64().and_then(|i| usize::try_from(i).ok());
                                    let text = match (name,id) {
                                        ("zoom",Some(id)) => match block["input"]["n"].as_u64().and_then(|n| usize::try_from(n).ok()) { Some(n) => store.memory.zoom(id,n), None => "Invalid zoom width".into() },
                                        ("date",Some(id)) => store.memory.date(id),
                                        _ => "Unknown tool or invalid arguments".into(),
                                    };
                                    let text = model::cap(&text); store.message(Kind::Echo,text.clone()).await?;
                                    results.push(json!({"type":"tool_result","tool_use_id":block["id"],"content":text}));
                                }
                                if response.stop == "tool_use" && !results.is_empty() {
                                    for text in t.pending.drain(..) { store.message(Kind::User,text.clone()).await?; results.push(json!({"type":"text","text":text})); }
                                    t.messages.push(json!({"role":"user","content":results}));
                                    t.task = Some(step(&client,&master,&system,&t.messages,&done_tx,&stream_tx)); turn = Some(t);
                                } else { queue.extend(t.pending); let _ = events.send(Event::Idle); let _ = events.send(Event::Status(if response.stop == "max_tokens" { "Reply reached the output limit" } else { "Ready" }.into())); }
                            }
                            Err(e) => { for text in t.pending { store.message(Kind::User,text).await?; } let _ = events.send(Event::Error(e.to_string())); let _ = events.send(Event::Idle); let _ = events.send(Event::Status("Reply failed · your message is saved".into())); }
                        }
                    }
                    changed = true;
                }
            },
            _ = tick.tick() => {},
        }
    }
}
fn step(
    client: &Client,
    master: &str,
    system: &str,
    messages: &[Value],
    tx: &mpsc::UnboundedSender<Finished>,
    stream: &mpsc::UnboundedSender<StreamEvent>,
) -> JoinHandle<()> {
    let client = client.clone();
    let master = master.to_owned();
    let system = system.to_owned();
    let messages = messages.to_vec();
    let tx = tx.clone();
    let stream = stream.clone();
    tokio::spawn(async move {
        let result = client
            .ask(&master, &system, &messages, &model::tools(), Some(stream))
            .await;
        let _ = tx.send(Finished::Step(result));
    })
}
async fn record(
    event: StreamEvent,
    store: &mut Store,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    match event {
        StreamEvent::Text(s) => {
            let _ = events.send(Event::Text(s));
        }
        StreamEvent::Thought(s) => {
            let _ = events.send(Event::Thought(s));
        }
        StreamEvent::Block(block) => match block["type"].as_str() {
            Some("text") => {
                store
                    .message(Kind::Talk, block["text"].as_str().unwrap_or("").into())
                    .await?;
            }
            Some("tool_use") => {
                store
                    .message(
                        Kind::Tool,
                        format!(
                            "{} {}",
                            block["name"].as_str().unwrap_or(""),
                            block["input"]
                        ),
                    )
                    .await?;
            }
            _ => {}
        },
    }
    Ok(())
}
async fn complete_node(
    p: Part,
    result: Result<String>,
    store: &mut Store,
    busy: &mut BTreeMap<Part, JoinHandle<()>>,
    retry: &mut BTreeMap<Part, Instant>,
    failed: &mut BTreeSet<Part>,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<()> {
    busy.remove(&p);
    match result {
        Ok(text) => {
            store.node(p, text).await?;
            retry.remove(&p);
            failed.remove(&p);
        }
        Err(e) => {
            retry.insert(p, Instant::now() + Duration::from_secs(10));
            if failed.insert(p) {
                let _ = events.send(Event::Error(format!(
                    "Summary {}: {e}. Retrying in 10 seconds.",
                    p.address()
                )));
            }
        }
    }
    Ok(())
}
