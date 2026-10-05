use crate::{
    JOBS, Result,
    integrations::{Approval, Config, Integrations, Outcome},
    memory::{Kind, Memory, Part, Store},
    model::{self, Client, Provider, Response, StreamEvent, Usage},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

pub enum Command {
    Send(String),
    SendFiles {
        text: String,
        paths: Vec<PathBuf>,
    },
    Cancel,
    Permissions(Permissions),
    ReloadIntegrations {
        config: String,
        base: String,
    },
    ReloadFromDisk,
    OAuth {
        config: String,
        base: String,
        server: String,
        sign_out: bool,
    },
    CancelOAuth,
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
    AttachmentRejected {
        text: String,
        paths: Vec<PathBuf>,
        error: String,
        active: bool,
    },
    Snapshot(Memory),
    Status(String),
    Error(String),
    SummaryRecovered(Part),
    Text(String),
    Thought(String),
    Usage(Usage),
    ProviderApplied(Provider, bool),
    SettingsApplied,
    SettingsRejected,
    Permissions(Permissions),
    Integrations {
        config: String,
        status: Vec<String>,
    },
    Approval(Approval),
    OAuthBusy(bool),
    OAuthUrl(String),
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
    pub integrations_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permissions {
    #[default]
    Ask,
    FullAccess,
}
impl Permissions {
    pub async fn load(directory: &std::path::Path) -> Result<Self> {
        match tokio::fs::read(directory.join("permissions.json")).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::Ask),
            Err(error) => Err(error.into()),
        }
    }
    async fn save(self, directory: &std::path::Path) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let temporary = directory.join("permissions.json.tmp");
        let mut file = tokio::fs::File::create(&temporary).await?;
        file.write_all(&serde_json::to_vec(&self)?).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(temporary, directory.join("permissions.json")).await?;
        Ok(())
    }
}

/// Non-secret selections saved separately from credentials and conversation logs.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelSelection {
    pub provider: Provider,
    pub master: String,
    pub compactor: String,
}
impl ModelSelection {
    pub async fn load(directory: &std::path::Path) -> Result<Option<Self>> {
        match tokio::fs::read(directory.join("settings.json")).await {
            Ok(bytes) => {
                let selection: Self = serde_json::from_slice(&bytes)?;
                selection.validate()?;
                Ok(Some(selection))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.master.trim().is_empty() || self.compactor.trim().is_empty() {
            return Err(crate::Error::Invalid(
                "Both model names are required.".into(),
            ));
        }
        Ok(())
    }

    pub async fn save(&self, directory: &std::path::Path) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        self.validate()?;
        let temporary = directory.join("settings.json.tmp");
        let mut file = tokio::fs::File::create(&temporary).await?;
        file.write_all(&serde_json::to_vec_pretty(self)?).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(temporary, directory.join("settings.json")).await?;
        Ok(())
    }
}
struct Turn {
    id: u64,
    cancel: CancellationToken,
    _cancel_on_drop: tokio_util::sync::DropGuard,
    tools_running: bool,
    results: Vec<Value>,
    messages: Vec<Value>,
    task: Option<JoinHandle<()>>,
    pending: Vec<crate::attachments::Message>,
}
enum Finished {
    Node(Part, Result<String>),
    Step(u64, Result<Response>),
    Tool(u64, Value),
    ToolsDone(u64),
    Integrations(Result<(Config, Integrations)>),
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
    let mut permissions = match Permissions::load(&settings.directory).await {
        Ok(mode) => mode,
        Err(error) => {
            let _ = events.send(Event::Error(format!(
                "Could not load permissions; asking for approval: {error}"
            )));
            Permissions::Ask
        }
    };
    let _ = events.send(Event::Permissions(permissions));
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
        "{}\n\n{}\n\nYour integrations configuration file is {:?}. You can read and edit it with local tools. Changes are validated and reloaded after each tool batch; new tools are available on the next model step. Preserve unrelated entries. OAuth sign-in still requires the user to complete authorization in Settings.",
        include_str!("master.txt"),
        settings.instructions,
        settings.integrations_path
    );
    let mut master = settings.master;
    let mut compactor = settings.compactor;
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();
    let (approval_tx, mut approval_rx) = mpsc::unbounded_channel::<Approval>();
    let mut integrations = Arc::new(Integrations::empty());
    let mut loading = settings.integrations_path.is_some();
    let mut reload_task = settings.integrations_path.clone().map(|path| {
        let tx = done_tx.clone();
        tokio::spawn(async move {
            let result = match Config::load(&path).await {
                Ok(config) => {
                    let integration = Integrations::connect(config.clone()).await;
                    Ok((config, integration))
                }
                Err(e) => Err(e),
            };
            let _ = tx.send(Finished::Integrations(result));
        })
    });
    let mut next_turn = 0u64;
    let mut busy: BTreeMap<Part, JoinHandle<()>> = BTreeMap::new();
    let mut retry: BTreeMap<Part, Instant> = BTreeMap::new();
    let mut failed = BTreeSet::new();
    let mut queue = VecDeque::<crate::attachments::Message>::new();
    let mut integration_config = Config::default();
    let mut disk_check = Instant::now();
    let mut turn: Option<Turn> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut changed = true;
    let _ = events.send(Event::Status("Ready".into()));
    loop {
        if !loading && turn.is_none() && disk_check.elapsed() >= Duration::from_secs(1) {
            refresh_integrations(
                &settings.integrations_path,
                &mut integration_config,
                &mut integrations,
                events,
            )
            .await;
            disk_check = Instant::now();
        }
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
        if !loading && turn.is_none() && !queue.is_empty() && store.memory.settled() {
            let view = store.memory.render();
            let texts: Vec<_> = queue.drain(..).collect();
            for text in &texts {
                store.message(Kind::User, text.text.clone()).await?;
            }
            let mut blocks = model::cache_blocks(&view);
            blocks.extend(texts.into_iter().flat_map(|message| message.blocks));
            let messages = vec![json!({"role":"user","content":blocks})];
            next_turn += 1;
            let task = step(
                &client,
                &master,
                &system,
                &messages,
                &integrations.definitions,
                next_turn,
                (&done_tx, &stream_tx),
            );
            let cancel = CancellationToken::new();
            turn = Some(Turn {
                id: next_turn,
                _cancel_on_drop: cancel.clone().drop_guard(),
                cancel,
                tools_running: false,
                results: vec![],
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
                    if let Some(t) = &mut turn { t.pending.push(text.into()); let _ = events.send(Event::Status("Message queued for the next tool boundary".into())); }
                    else { queue.push_back(text.into()); let _ = events.send(Event::Status("Waiting for memory to finish summarizing".into())); }
                }
                Some(Command::SendFiles { text, paths }) => {
                    match crate::attachments::prepare(text.clone(), &paths, &settings.directory).await {
                        Ok(message) => {
                            if let Some(t) = &mut turn { t.pending.push(message); } else { queue.push_back(message); }
                        }
                        Err(error) => { let _ = events.send(Event::AttachmentRejected { text, paths, error: error.to_string(), active: turn.is_some() || !queue.is_empty() }); }
                    }
                }
                Some(Command::Cancel) => {
                    if let Some(mut t) = turn.take() { t.cancel.cancel(); if let Some(task) = t.task.take() && !t.tools_running { task.abort(); let _ = task.await; } for text in t.pending { store.message(Kind::User,text.text).await?; } }
                    while let Ok(event) = stream_rx.try_recv() { record(event, &mut store, events).await?; }
                    while let Some(text) = queue.pop_front() { store.message(Kind::User,text.text).await?; }
                    // Late completions retain their turn ID and cannot resume a later turn.
                    let _ = events.send(Event::Idle); let _ = events.send(Event::Status("Stopped · unsent messages preserved in history".into())); changed = true;
                }
                Some(Command::CancelOAuth) => {
                    if loading {
                        if let Some(task) = reload_task.take() { task.abort(); }
                        loading = false;
                        let _ = events.send(Event::OAuthBusy(false));
                        let _ = events.send(Event::Status("OAuth sign-in canceled".into()));
                    }
                }
                Some(Command::OAuth { config: text, base, server, sign_out }) => {
                    if turn.is_some() || !queue.is_empty() || loading {
                        let _ = events.send(Event::Error("Change OAuth sign-in when the chat and integrations are idle.".into()));
                        let _ = events.send(Event::OAuthBusy(false));
                        continue;
                    }
                    let config = match Config::parse(&text) {
                        Ok(value) => value,
                        Err(error) => { let _ = events.send(Event::Error(error.to_string())); let _ = events.send(Event::OAuthBusy(false)); continue; }
                    };
                    if let Some(path) = settings.integrations_path.clone() {
                        let base = match Config::parse(&base) { Ok(base) => base, Err(e) => { let _ = events.send(Event::Error(e.to_string())); let _ = events.send(Event::OAuthBusy(false)); continue; } };
                        loading = true;
                        let done = done_tx.clone();
                        let events = events.clone();
                        let _ = events.send(Event::OAuthBusy(true));
                        reload_task = Some(tokio::spawn(async move {
                            let result = async {
                                let config = config.save_merged(&path, &base).await?;
                                let (url, oauth) = match config.servers.get(&server) {
                                    Some(crate::integrations::Server::Http { url, oauth: Some(oauth), .. }) => (url, oauth),
                                    _ => return Err(crate::Error::Invalid("Select OAuth for an HTTP server first".into())),
                                };
                                if sign_out { crate::oauth::sign_out(url, oauth).await?; }
                                else {
                                    crate::oauth::sign_in(url, oauth, |url| { let _ = events.send(Event::OAuthUrl(url)); }).await?;
                                }
                                let connected = Integrations::connect(config.clone()).await;
                                Ok((config, connected))
                            }.await;
                            let _ = done.send(Finished::Integrations(result));
                        }));
                    } else { let _ = events.send(Event::OAuthBusy(false)); }
                }
                Some(Command::ReloadFromDisk) => {
                    if turn.is_none() && !loading {
                        // Force a refresh even when the persisted configuration is unchanged.
                        if let Some(path) = &settings.integrations_path {
                            match Config::load(path).await {
                                Ok(config) => {
                                    integrations = Arc::new(Integrations::connect(config.clone()).await);
                                    integration_config = config;
                                    let _ = events.send(Event::Integrations { config: serde_json::to_string(&integration_config)?, status: integrations.status.clone() });
                                }
                                Err(e) => { let _ = events.send(Event::Error(e.to_string())); }
                            }
                        }
                    }
                }
                Some(Command::ReloadIntegrations { config: text, base }) => {
                    if turn.is_some() || !queue.is_empty() || loading {
                        let _ = events.send(Event::Error("Reload integrations when the chat is idle.".into()));
                    } else if let Some(path) = settings.integrations_path.clone() {
                        let base = match Config::parse(&base) { Ok(base) => base, Err(e) => { let _ = events.send(Event::Error(e.to_string())); continue; } };
                        match Config::parse(&text) {
                            Err(e) => { let _ = events.send(Event::Error(e.to_string())); },
                            Ok(config) => {
                                loading = true;
                                let tx = done_tx.clone();
                                reload_task = Some(tokio::spawn(async move {
                                    let result = match config.save_merged(&path, &base).await {
                                        Ok(config) => { let integration = Integrations::connect(config.clone()).await; Ok((config, integration)) },
                                        Err(e) => Err(e),
                                    };
                                    let _ = tx.send(Finished::Integrations(result));
                                }));
                                let _ = events.send(Event::Status("Connecting integrations…".into()));
                            }
                        }
                    }
                }
                Some(Command::Permissions(mode)) => {
                    match mode.save(&settings.directory).await {
                        Ok(()) => { permissions = mode; let _ = events.send(Event::Permissions(mode)); }
                        Err(error) => { let _ = events.send(Event::Error(format!("Could not save permissions: {error}"))); }
                    }
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
                        let selection = ModelSelection { provider: next, master: m.clone(), compactor: c.clone() };
                        if let Err(error) = selection.save(&settings.directory).await {
                            let _ = events.send(Event::Error(format!("Could not save model settings: {error}")));
                            let _ = events.send(Event::SettingsRejected);
                            continue;
                        }
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
                    if let Some(task) = reload_task.take() { task.abort(); }
                    if let Some(mut t) = turn.take() {
                        t.cancel.cancel();
                        if let Some(mut task) = t.task.take() {
                            if !t.tools_running { task.abort(); }
                            if tokio::time::timeout(Duration::from_secs(3), &mut task).await.is_err() { task.abort(); }
                        }
                        for text in t.pending { store.message(Kind::User,text.text).await?; }
                    }
                    while let Ok(event) = stream_rx.try_recv() { record(event,&mut store,events).await?; }
                    while let Ok(result) = done_rx.try_recv() {
                        if let Finished::Tool(_, result) = result { store.message(Kind::Echo,result["content"].as_str().unwrap_or("").to_owned()).await?; }
                    }
                    for text in queue { store.message(Kind::User,text.text).await?; }
                    for (_,task) in busy { task.abort(); }
                    return Ok(());
                }
                _ => {},
            },
            Some(event) = stream_rx.recv() => { let persisted = matches!(event,StreamEvent::Block(_)); record(event,&mut store,events).await?; changed |= persisted; },
            Some(approval) = approval_rx.recv() => {
                if turn.is_some() && !approval.answer.is_closed() {
                    if permissions == Permissions::FullAccess { let _ = approval.answer.send(true); }
                    else { let _ = events.send(Event::Approval(approval)); }
                }
            },
            Some(result) = done_rx.recv() => match result {
                Finished::Integrations(result) => {
                    let _ = events.send(Event::OAuthBusy(false));
                    loading = false; reload_task = None;
                    match result {
                        Ok((config, connected)) => {
                            let _ = events.send(Event::Integrations { config: serde_json::to_string_pretty(&config)?, status: connected.status.clone() });
                            integrations = Arc::new(connected);
                            integration_config = config;
                            let _ = events.send(Event::Status("Integrations ready".into()));
                        }
                        Err(e) => { let _ = events.send(Event::Error(format!("Integrations: {e}"))); }
                    }
                }
                Finished::Tool(id, result) => {
                    store.message(Kind::Echo, result["content"].as_str().unwrap_or("").to_owned()).await?;
                    if let Some(t) = &mut turn && t.id == id { t.results.push(result); }
                    changed = true;
                }
                Finished::ToolsDone(id) => {
                    if let Some(t) = &mut turn && t.id == id {
                        refresh_integrations(&settings.integrations_path, &mut integration_config, &mut integrations, events).await;
                        let mut results = std::mem::take(&mut t.results);
                        for text in t.pending.drain(..) { store.message(Kind::User,text.text).await?; results.extend(text.blocks); }
                        t.messages.push(json!({"role":"user","content":results}));
                        t.tools_running = false;
                        t.task = Some(step(&client,&master,&system,&t.messages,&integrations.definitions,t.id,(&done_tx,&stream_tx)));
                        changed = true;
                    }
                }
                Finished::Node(p,result) => { complete_node(p,result,&mut store,&mut busy,&mut retry,&mut failed,events).await?; changed = true; },
                Finished::Step(id, result) => {
                    if turn.as_ref().is_none_or(|t| t.id != id) { continue; }
                    // Both channels are FIFO individually; drain completed blocks before results.
                    while let Ok(event) = stream_rx.try_recv() { record(event,&mut store,events).await?; }
                    if let Some(mut t) = turn.take() {
                        t.task = None;
                        match result {
                            Ok(response) => {
                                let _ = events.send(Event::Usage(response.usage));
                                t.messages.push(json!({"role":"assistant","content":response.content}));
                                if response.stop == "tool_use" && response.content.iter().any(|b| b["type"] == "tool_use") {
                                    t.tools_running = true;
                                    t.task = Some(tool_batch(response.content, store.memory.clone(), integrations.clone(), t.cancel.clone(), t.id, done_tx.clone(), approval_tx.clone()));
                                    turn = Some(t);
                                } else { queue.extend(t.pending); let _ = events.send(Event::Idle); let _ = events.send(Event::Status(if response.stop == "max_tokens" { "Reply reached the output limit" } else { "Ready" }.into())); }
                            }
                            Err(e) => { for text in t.pending { store.message(Kind::User,text.text).await?; } let _ = events.send(Event::Error(e.to_string())); let _ = events.send(Event::Idle); let _ = events.send(Event::Status("Reply failed · your message is saved".into())); }
                        }
                    }
                    changed = true;
                }
            },
            _ = tick.tick() => {},
        }
    }
}
async fn refresh_integrations(
    path: &Option<PathBuf>,
    config: &mut Config,
    integrations: &mut Arc<Integrations>,
    events: &mpsc::UnboundedSender<Event>,
) {
    let Some(path) = path else {
        return;
    };
    match Config::load(path).await {
        Ok(next) if serde_json::to_value(&next).ok() != serde_json::to_value(&*config).ok() => {
            *integrations = Arc::new(Integrations::connect(next.clone()).await);
            *config = next;
            let _ = events.send(Event::Integrations {
                config: serde_json::to_string(config).expect("config serializes"),
                status: integrations.status.clone(),
            });
        }
        Ok(_) => {}
        Err(e) => {
            let _ = events.send(Event::Error(format!(
                "Integration file was not reloaded: {e}"
            )));
        }
    }
}

fn step(
    client: &Client,
    master: &str,
    system: &str,
    messages: &[Value],
    tools: &[Value],
    id: u64,
    channels: (
        &mpsc::UnboundedSender<Finished>,
        &mpsc::UnboundedSender<StreamEvent>,
    ),
) -> JoinHandle<()> {
    let client = client.clone();
    let master = master.to_owned();
    let system = system.to_owned();
    let messages = messages.to_vec();
    let tx = channels.0.clone();
    let stream = channels.1.clone();
    let tools = tools.to_vec();
    tokio::spawn(async move {
        let result = client
            .ask(&master, &system, &messages, &tools, Some(stream))
            .await;
        let _ = tx.send(Finished::Step(id, result));
    })
}
fn tool_batch(
    blocks: Vec<Value>,
    memory: Memory,
    integrations: Arc<Integrations>,
    cancel: CancellationToken,
    turn_id: u64,
    tx: mpsc::UnboundedSender<Finished>,
    approvals: mpsc::UnboundedSender<Approval>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        for block in blocks.iter().filter(|b| b["type"] == "tool_use") {
            if cancel.is_cancelled() {
                break;
            }
            let name = block["name"].as_str().unwrap_or("");
            let id = block["input"]["id"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok());
            let outcome = match (name, id) {
                ("zoom", Some(id)) => match block["input"]["n"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                {
                    Some(n) => Outcome {
                        text: memory.zoom(id, n),
                        error: false,
                    },
                    None => Outcome::error("Invalid zoom width"),
                },
                ("date", Some(id)) => Outcome {
                    text: memory.date(id),
                    error: false,
                },
                ("zoom" | "date", None) => Outcome::error("Invalid message ID"),
                _ => {
                    integrations
                        .call(name, block["input"].clone(), &approvals, &cancel)
                        .await
                }
            };
            let _ = tx.send(Finished::Tool(turn_id, json!({"type":"tool_result","tool_use_id":block["id"],"content":model::cap(&outcome.text),"is_error":outcome.error})));
        }
        let _ = tx.send(Finished::ToolsDone(turn_id));
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
            if failed.remove(&p) {
                let _ = events.send(Event::SummaryRecovered(p));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn successful_retry_clears_only_the_recovered_summary_error() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path()).await.unwrap();
        store
            .message(Kind::User, "long message ".repeat(100))
            .await
            .unwrap();
        let part = Part { l: 0, i: 0 };
        let other = Part { l: 0, i: 1 };
        let mut busy = BTreeMap::new();
        let mut retry = BTreeMap::new();
        let mut failed = BTreeSet::from([other]);
        let (events, mut received) = mpsc::unbounded_channel();
        complete_node(
            part,
            Err(crate::Error::Invalid("empty summary".into())),
            &mut store,
            &mut busy,
            &mut retry,
            &mut failed,
            &events,
        )
        .await
        .unwrap();
        assert!(matches!(received.recv().await, Some(Event::Error(_))));
        assert!(retry.contains_key(&part));
        complete_node(
            part,
            Ok("user: long message".into()),
            &mut store,
            &mut busy,
            &mut retry,
            &mut failed,
            &events,
        )
        .await
        .unwrap();
        assert!(matches!(received.recv().await, Some(Event::SummaryRecovered(p)) if p == part));
        assert!(!retry.contains_key(&part));
        assert_eq!(failed, BTreeSet::from([other]));
        assert_eq!(store.memory.text(part), "user: long message");
    }
}
