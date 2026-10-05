use crate::{
    Error, Result, model,
    skills::{self, Skill},
};
use futures_util::{StreamExt, stream};
use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientRequest,
        ServerResult,
    },
    service::{PeerRequestOptions, RunningService},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub skill_directories: Vec<PathBuf>,
    pub servers: BTreeMap<String, Server>,
    pub timeout_seconds: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            skill_directories: vec![PathBuf::from("~/.agents/skills")],
            servers: BTreeMap::new(),
            timeout_seconds: 60,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum Server {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        env_from: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        bearer_token_env: Option<String>,
        #[serde(default)]
        oauth: Option<crate::oauth::Config>,
    },
}
impl Config {
    /// Merge a Settings draft with disk changes made since it was loaded.
    /// Conflicting edits fail without replacing either side.
    pub async fn save_merged(&self, path: &Path, base: &Self) -> Result<Self> {
        let current = Self::load(path).await?;
        let mut merged = serde_json::to_value(&current)?;
        let draft = serde_json::to_value(self)?;
        let base = serde_json::to_value(base)?;
        for field in ["skill_directories", "timeout_seconds"] {
            merge_field(&mut merged, &draft, &base, field)?;
        }
        let names: std::collections::BTreeSet<_> = base["servers"]
            .as_object()
            .unwrap()
            .keys()
            .chain(draft["servers"].as_object().unwrap().keys())
            .cloned()
            .collect();
        for name in names {
            merge_field(
                &mut merged["servers"],
                &draft["servers"],
                &base["servers"],
                &name,
            )?;
        }
        let merged = Self::parse(&merged.to_string())?;
        merged.save(path).await?;
        Ok(merged)
    }
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = serde_json::from_str(text)?;
        if !(1..=600).contains(&config.timeout_seconds) {
            return Err(Error::Invalid("timeout_seconds must be 1–600".into()));
        }
        for (name, server) in &config.servers {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(Error::Invalid(
                    "Server names must contain 1–64 letters, digits, underscores or hyphens".into(),
                ));
            }
            match server {
                Server::Stdio { command, .. } if command.trim().is_empty() => {
                    return Err(Error::Invalid("Server command cannot be empty".into()));
                }
                Server::Http {
                    url,
                    bearer_token_env,
                    oauth,
                } => {
                    if oauth.is_some() && bearer_token_env.is_some() {
                        return Err(Error::Invalid(
                            "Choose OAuth or a bearer token environment variable, not both".into(),
                        ));
                    }
                    if oauth.is_some() {
                        crate::oauth::validate_url(url)?;
                    }
                    let url = reqwest::Url::parse(url)
                        .map_err(|_| Error::Invalid("Invalid MCP URL".into()))?;
                    if !matches!(url.scheme(), "http" | "https")
                        || !url.username().is_empty()
                        || url.password().is_some()
                    {
                        return Err(Error::Invalid(
                            "MCP URL must be HTTP(S), without embedded credentials".into(),
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(config)
    }
    pub async fn load(path: &Path) -> Result<Self> {
        match tokio::fs::read_to_string(path).await {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub async fn save(&self, path: &Path) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let temporary = path.with_extension("json.tmp");
        let mut file = tokio::fs::File::create(&temporary).await?;
        file.write_all(serde_json::to_string_pretty(self)?.as_bytes())
            .await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(temporary, path).await?;
        Ok(())
    }
}

fn merge_field(current: &mut Value, draft: &Value, base: &Value, key: &str) -> Result<()> {
    if draft[key] == base[key] {
        return Ok(());
    }
    if current[key] != base[key] && current[key] != draft[key] {
        return Err(Error::Invalid(format!(
            "Integration setting '{key}' changed on disk. Reload from disk before editing it again."
        )));
    }
    if draft.get(key).is_none() {
        current.as_object_mut().unwrap().remove(key);
    } else {
        current[key] = draft[key].clone();
    }
    Ok(())
}

pub struct Approval {
    pub server: String,
    pub tool: String,
    pub arguments: Value,
    pub answer: oneshot::Sender<bool>,
}
struct RemoteTool {
    server: String,
    name: String,
    service: Arc<RunningService<RoleClient, ()>>,
}
pub struct Integrations {
    pub definitions: Vec<Value>,
    pub status: Vec<String>,
    pub skills: Vec<Skill>,
    remote: BTreeMap<String, RemoteTool>,
    timeout: Duration,
}
#[derive(Debug)]
pub struct Outcome {
    pub text: String,
    pub error: bool,
}
impl Outcome {
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: true,
        }
    }
}

impl Integrations {
    pub fn empty() -> Self {
        Self {
            definitions: model::tools()
                .into_iter()
                .chain(crate::local_tools::definitions())
                .collect(),
            status: vec![],
            skills: vec![],
            remote: BTreeMap::new(),
            timeout: Duration::from_secs(60),
        }
    }
    pub async fn connect(config: Config) -> Self {
        let mut result = Self::empty();
        result.timeout = Duration::from_secs(config.timeout_seconds);
        let roots = config.skill_directories;
        match tokio::task::spawn_blocking(move || skills::discover(&roots)).await {
            Ok((skills, warnings)) => {
                result.skills = skills;
                result.status.extend(warnings);
            }
            Err(_) => result.status.push("Skill discovery failed".into()),
        }
        result
            .status
            .push(format!("{} skills discovered", result.skills.len()));
        let catalog = result
            .skills
            .iter()
            .map(|s| json!({"id":s.id,"name":s.name,"description":s.description}))
            .collect::<Vec<_>>();
        result.definitions.push(json!({"name":"load_skill","description":format!("Load a relevant skill before working. Follow its instructions using available tools; report unavailable requirements. Read additional pages with offset when next_offset is present. Available skills: {}", json!(catalog)),"input_schema":{"type":"object","properties":{"id":{"type":"string"},"offset":{"type":"integer","minimum":0}},"required":["id"],"additionalProperties":false}}));
        result.definitions.push(json!({"name":"read_skill_file","description":"Read a relative UTF-8 reference within a discovered skill directory. Page with offset when next_offset is present. Does not execute scripts.","input_schema":{"type":"object","properties":{"id":{"type":"string"},"path":{"type":"string"},"offset":{"type":"integer","minimum":0}},"required":["id","path"],"additionalProperties":false}}));
        let timeout = result.timeout.min(Duration::from_secs(15));
        let mut connections =
            stream::iter(config.servers.into_iter().map(|(name, server)| async move {
                let connection = tokio::time::timeout(timeout, connect(server)).await;
                (name, connection)
            }))
            .buffer_unordered(4);
        let mut ready = BTreeMap::new();
        while let Some((name, connection)) = connections.next().await {
            ready.insert(name, connection);
        }
        for (name, connection) in ready {
            match connection {
                Ok(Ok((service, tools))) => {
                    result
                        .status
                        .push(format!("{name}: connected, {} tools", tools.len()));
                    let service = Arc::new(service);
                    for tool in tools {
                        let mut hash = std::hash::DefaultHasher::new();
                        (&name, &tool.name).hash(&mut hash);
                        let alias = format!("mcp_{:016x}", hash.finish());
                        if result.remote.contains_key(&alias) {
                            result
                                .status
                                .push(format!("{name}: duplicate tool omitted"));
                            continue;
                        }
                        result.definitions.push(json!({"name":alias,"description":format!("MCP server {}, tool {}. {}",name,tool.name,tool.description.as_deref().unwrap_or("")),"input_schema":tool.input_schema}));
                        result.remote.insert(
                            alias,
                            RemoteTool {
                                server: name.clone(),
                                name: tool.name.into_owned(),
                                service: service.clone(),
                            },
                        );
                    }
                }
                Ok(Err(e)) => result.status.push(format!("{name}: {e}")),
                Err(_) => result
                    .status
                    .push(format!("{name}: connection/discovery timed out")),
            }
        }
        result.definitions[2..].sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        result
    }
    pub async fn call(
        &self,
        name: &str,
        input: Value,
        approvals: &mpsc::UnboundedSender<Approval>,
        cancel: &CancellationToken,
    ) -> Outcome {
        if cancel.is_cancelled() {
            return Outcome::error("Canceled before execution");
        }
        if crate::local_tools::contains(name) {
            let (answer, response) = tokio::sync::oneshot::channel();
            if approvals
                .send(Approval {
                    server: "Local computer".into(),
                    tool: name.into(),
                    arguments: input.clone(),
                    answer,
                })
                .is_err()
            {
                return Outcome::error("Approval unavailable; not executed");
            }
            let allowed = tokio::select! {
                _ = cancel.cancelled() => false,
                result = response => result.unwrap_or(false),
            };
            if !allowed {
                return Outcome::error("Denied or canceled; not executed");
            }
            return crate::local_tools::call(name, input, cancel).await;
        }
        if name == "load_skill" || name == "read_skill_file" {
            let Some(id) = input["id"].as_str() else {
                return Outcome::error("Missing skill ID");
            };
            let path = if name == "load_skill" {
                "SKILL.md"
            } else {
                let Some(path) = input["path"].as_str() else {
                    return Outcome::error("Missing relative path");
                };
                path
            };
            let offset = match input.get("offset") {
                None => 0,
                Some(v) => match v.as_u64().and_then(|v| usize::try_from(v).ok()) {
                    Some(v) => v,
                    None => return Outcome::error("Invalid offset"),
                },
            };
            let skills = self.skills.clone();
            let id = id.to_owned();
            let path = path.to_owned();
            return match tokio::task::spawn_blocking(move || {
                skills::read(&skills, &id, &path, offset)
            })
            .await
            {
                Ok(Ok(text)) => Outcome { text, error: false },
                Ok(Err(e)) => Outcome::error(e.to_string()),
                Err(_) => Outcome::error("Skill read failed"),
            };
        }
        let Some(tool) = self.remote.get(name) else {
            return Outcome::error(
                "Unknown tool; reload integrations between turns if the server changed",
            );
        };
        let Some(arguments) = input.as_object().cloned() else {
            return Outcome::error("Tool arguments must be an object");
        };
        let (answer, reply) = oneshot::channel();
        if approvals
            .send(Approval {
                server: tool.server.clone(),
                tool: tool.name.clone(),
                arguments: input,
                answer,
            })
            .is_err()
        {
            return Outcome::error("Approval UI unavailable");
        }
        let allowed = tokio::select! { biased; _ = cancel.cancelled() => false, result = reply => result.unwrap_or(false) };
        if !allowed || cancel.is_cancelled() {
            return Outcome::error("Tool call denied or canceled; not executed");
        }
        let peer = tool.service.peer();
        let params: CallToolRequestParams =
            match serde_json::from_value(json!({"name":tool.name,"arguments":arguments})) {
                Ok(params) => params,
                Err(_) => return Outcome::error("Invalid MCP arguments"),
            };
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let sent = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            sent = tokio::time::timeout(self.timeout, peer.send_cancellable_request(request, PeerRequestOptions::default())) => sent.ok(),
        };
        let handle = match sent {
            Some(Ok(handle)) => handle,
            _ => {
                return Outcome::error(
                    "MCP request send failed or was canceled; execution status is unknown. Not retried.",
                );
            }
        };
        let request_id = handle.id.clone();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            result = tokio::time::timeout(self.timeout, handle.await_response()) => result.ok(),
        };
        match response {
            Some(Ok(ServerResult::CallToolResult(result))) => {
                let value = serde_json::to_value(&result).unwrap_or_default();
                let mut text = String::new();
                if let Some(contents) = value["content"].as_array() {
                    for content in contents {
                        if let Some(s) = content["text"].as_str() {
                            text.push_str(s);
                            text.push('\n');
                        } else if let Some(s) = content["resource"]["text"].as_str() {
                            text.push_str(s);
                            text.push('\n');
                        } else if content["type"] == "resource_link" {
                            text.push_str(&format!(
                                "Resource link: {} ({})\n",
                                content["name"].as_str().unwrap_or(""),
                                content["uri"].as_str().unwrap_or("")
                            ));
                        } else {
                            text.push_str(&format!(
                                "[MCP {} content is not rendered by this text client]\n",
                                content["type"].as_str().unwrap_or("unknown")
                            ));
                        }
                    }
                }
                if let Some(structured) = value.get("structuredContent") {
                    text.push_str(&format!("\nStructured result:\n{structured}"));
                }
                Outcome {
                    text: model::cap(&text),
                    error: result.is_error.unwrap_or(false),
                }
            }
            Some(Ok(_)) => Outcome::error("Server returned an unsupported MCP extension result"),
            Some(Err(rmcp::service::ServiceError::McpError(error))) => {
                Outcome::error(format!("MCP error: {error}"))
            }
            Some(Err(_)) => {
                Outcome::error("MCP call failed; check the server connection. It was not retried.")
            }
            None => {
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    peer.notify_cancelled(CancelledNotificationParam::new(
                        Some(request_id),
                        Some("OptChat stopped or timed out".into()),
                    )),
                )
                .await;
                Outcome::error(
                    "MCP call canceled or timed out; remote side effects may already have occurred. Not retried.",
                )
            }
        }
    }
}

async fn connect(
    server: Server,
) -> Result<(RunningService<RoleClient, ()>, Vec<rmcp::model::Tool>)> {
    let service = match server {
        Server::Stdio {
            command,
            args,
            cwd,
            env_from,
        } => {
            let mut child = tokio::process::Command::new(command);
            child.args(args).env_clear();
            for name in ["PATH", "HOME", "USER", "TMPDIR", "LANG", "SYSTEMROOT"] {
                if let Some(value) = std::env::var_os(name) {
                    child.env(name, value);
                }
            }
            for (target, source) in env_from {
                let value = std::env::var_os(&source).ok_or_else(|| {
                    Error::Invalid(format!("Missing environment variable {source}"))
                })?;
                child.env(target, value);
            }
            if let Some(cwd) = cwd {
                child.current_dir(skills::expand(&cwd));
            }
            let transport = TokioChildProcess::builder(child)
                .stderr(std::process::Stdio::null())
                .spawn()?
                .0;
            ().serve(transport).await.map_err(|_| {
                Error::Invalid(
                    "MCP initialization failed (stdio); check command and protocol support".into(),
                )
            })?
        }
        Server::Http {
            url,
            bearer_token_env,
            oauth,
        } => {
            let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());
            // Retrying a tools/call after a session failure may repeat a side effect.
            config.reinit_on_expired_session = false;
            if let Some(name) = bearer_token_env {
                config = config.auth_header(std::env::var(&name).map_err(|_| {
                    Error::Invalid(format!("Missing token environment variable {name}"))
                })?);
            }
            let http = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            if let Some(oauth) = oauth {
                let mut manager = crate::oauth::manager(&url, &oauth).await?;
                if !manager.initialize_from_store().await.map_err(|_| {
                    Error::Invalid(
                        "Could not restore OAuth credentials; sign in again in Settings".into(),
                    )
                })? {
                    return Err(Error::Invalid(
                        "OAuth sign-in required; use Sign in in Settings".into(),
                    ));
                }
                let client = rmcp::transport::auth::AuthClient::new(http, manager);
                let transport = StreamableHttpClientTransport::with_client(client, config);
                ().serve(transport).await.map_err(|_| {
                    Error::Invalid(
                        "MCP OAuth connection failed; retry or sign in again in Settings".into(),
                    )
                })?
            } else {
                let transport = StreamableHttpClientTransport::with_client(http, config);
                ().serve(transport).await.map_err(|_| Error::Invalid("MCP initialization failed (HTTP); check URL/authentication or enable OAuth in Settings".into()))?
            }
        }
    };
    let tools = service
        .list_all_tools()
        .await
        .map_err(|_| Error::Invalid("MCP tool discovery failed".into()))?;
    Ok((service, tools))
}
