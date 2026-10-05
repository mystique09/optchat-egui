use crate::{Error, NODE, Result, TRIES};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    DeepSeek,
}
impl Provider {
    pub fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "anthropic" => Ok(Self::Anthropic),
            "deepseek" => Ok(Self::DeepSeek),
            _ => Err(Error::Invalid(format!(
                "Unknown OPTCHAT_PROVIDER {value:?}; use anthropic or deepseek"
            ))),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::DeepSeek => "DeepSeek",
        }
    }
    pub fn key_variable(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::DeepSeek => "DEEPSEEK_API_KEY",
        }
    }
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com/v1/messages",
            Self::DeepSeek => "https://api.deepseek.com/anthropic/v1/messages",
        }
    }
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-sonnet-4-6",
            Self::DeepSeek => "deepseek-flash",
        }
    }
}

#[derive(Debug, Default)]
pub struct Usage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
}
impl Usage {
    pub fn from_api(value: &Value) -> Self {
        Self {
            input: value["input_tokens"]
                .as_u64()
                .or_else(|| value["prompt_tokens"].as_u64()),
            output: value["output_tokens"]
                .as_u64()
                .or_else(|| value["completion_tokens"].as_u64()),
            cache_read: value["cache_read_input_tokens"]
                .as_u64()
                .or_else(|| value["prompt_cache_hit_tokens"].as_u64()),
            cache_write: value["cache_creation_input_tokens"].as_u64(),
        }
    }
    pub fn display(&self) -> String {
        let count = |n: Option<u64>| n.map_or_else(|| "—".into(), |n| n.to_string());
        format!(
            "Input {} · cache read {} · cache write {} · output {}",
            count(self.input),
            count(self.cache_read),
            count(self.cache_write),
            count(self.output)
        )
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    key: String,
    endpoint: String,
    provider: Provider,
}
#[derive(Debug)]
pub enum StreamEvent {
    Text(String),
    Thought(String),
    Block(Value),
}
#[derive(Debug)]
pub struct Response {
    pub content: Vec<Value>,
    pub stop: String,
    pub usage: Usage,
}
impl Client {
    pub(crate) fn set_key(&mut self, key: String) {
        self.key = key;
    }
    pub fn new(provider: Provider, key: String, endpoint: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .expect("HTTP client configuration"),
            key,
            endpoint,
            provider,
        }
    }
    pub async fn ask(
        &self,
        model: &str,
        system: &str,
        messages: &[Value],
        tools: &[Value],
        events: Option<mpsc::UnboundedSender<StreamEvent>>,
    ) -> Result<Response> {
        if self.key.trim().is_empty() {
            return Err(Error::Invalid(format!(
                "Add a key in Settings (or set {}) to connect {}.",
                self.provider.key_variable(),
                self.provider.label()
            )));
        }
        let mut body = json!({"model":model,"max_tokens":8192,"system":system,"messages":messages,"stream":true,"thinking":{"type":"adaptive"},"output_config":{"effort":"medium"},"cache_control":{"type":"ephemeral"}});
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if self.provider == Provider::DeepSeek {
            body.as_object_mut()
                .expect("request object")
                .remove("cache_control");
            body["thinking"] = json!({"type":"enabled"});
            body["output_config"] = json!({"effort":"high"});
            // DeepSeek caches automatically; omit unsupported explicit breakpoints.
            for message in body["messages"].as_array_mut().expect("message array") {
                if let Some(blocks) = message["content"].as_array_mut() {
                    for block in blocks {
                        if let Some(object) = block.as_object_mut() {
                            object.remove("cache_control");
                        }
                    }
                }
            }
        }
        let response = self
            .http
            .post(&self.endpoint)
            .header("x-api-key", &self.key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Invalid(format!(
                "Model API {status}: {}",
                response.text().await?
            )));
        }
        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut content = Vec::<Value>::new();
        let mut inputs = Vec::<String>::new();
        let mut stop = String::new();
        let mut usage = json!({});
        let mut finished = false;
        while let Some(bytes) = stream.next().await {
            pending.extend_from_slice(&bytes?);
            while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = pending.drain(..=end).collect();
                let line = std::str::from_utf8(&line)
                    .map_err(|e| Error::Invalid(e.to_string()))?
                    .trim();
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                let event: Value = serde_json::from_str(data)?;
                match event["type"].as_str().unwrap_or("") {
                    "error" => return Err(Error::Invalid(event["error"].to_string())),
                    "message_start" => usage = event["message"]["usage"].clone(),
                    "content_block_start" => {
                        let index = event["index"]
                            .as_u64()
                            .ok_or_else(|| Error::Invalid("Missing stream index".into()))?
                            as usize;
                        if index != content.len() {
                            return Err(Error::Invalid("Non-sequential stream block".into()));
                        }
                        content.push(event["content_block"].clone());
                        inputs.push(String::new());
                    }
                    "content_block_delta" => {
                        let index = event["index"]
                            .as_u64()
                            .ok_or_else(|| Error::Invalid("Missing stream index".into()))?
                            as usize;
                        let block = content
                            .get_mut(index)
                            .ok_or_else(|| Error::Invalid("Unknown stream block".into()))?;
                        let delta = &event["delta"];
                        let (field, value) = match delta["type"].as_str().unwrap_or("") {
                            "text_delta" => ("text", delta["text"].as_str().unwrap_or("")),
                            "thinking_delta" => {
                                ("thinking", delta["thinking"].as_str().unwrap_or(""))
                            }
                            "signature_delta" => {
                                ("signature", delta["signature"].as_str().unwrap_or(""))
                            }
                            "input_json_delta" => {
                                inputs[index]
                                    .push_str(delta["partial_json"].as_str().unwrap_or(""));
                                continue;
                            }
                            _ => continue,
                        };
                        let mut text = block[field].as_str().unwrap_or("").to_owned();
                        text.push_str(value);
                        block[field] = json!(text);
                        if let Some(tx) = &events {
                            let _ = match field {
                                "text" => tx.send(StreamEvent::Text(value.into())),
                                "thinking" => tx.send(StreamEvent::Thought(value.into())),
                                _ => Ok(()),
                            };
                        }
                    }
                    "content_block_stop" => {
                        let index = event["index"]
                            .as_u64()
                            .ok_or_else(|| Error::Invalid("Missing stream index".into()))?
                            as usize;
                        let block = content
                            .get_mut(index)
                            .ok_or_else(|| Error::Invalid("Unknown stream block".into()))?;
                        if !inputs[index].is_empty() {
                            block["input"] = serde_json::from_str(&inputs[index])?;
                        }
                        if let Some(tx) = &events {
                            let _ = tx.send(StreamEvent::Block(block.clone()));
                        }
                    }
                    "message_delta" => {
                        stop = event["delta"]["stop_reason"].as_str().unwrap_or("").into();
                        if let (Some(dst), Some(src)) =
                            (usage.as_object_mut(), event["usage"].as_object())
                        {
                            dst.extend(src.clone());
                        }
                    }
                    "message_stop" => finished = true,
                    _ => {}
                }
            }
        }
        if !finished {
            return Err(Error::Invalid(
                "Model stream ended before message_stop; completed entries are preserved.".into(),
            ));
        }
        Ok(Response {
            content,
            stop,
            usage: Usage::from_api(&usage),
        })
    }
    pub async fn compact(
        &self,
        model: &str,
        context: String,
        source: String,
        merge: bool,
    ) -> Result<String> {
        if source.len() <= NODE {
            return Ok(source);
        }
        let instruction = if merge {
            "Merge these two lines into one, in at most 512 bytes:"
        } else {
            "Compress this message into one line, in at most 512 bytes:"
        };
        let mut blocks = cache_blocks(&context);
        blocks.push(json!({"type":"text","text":format!("For scale, this line is exactly 512 bytes:\n{}\n\n{instruction}\n{source}", scale())}));
        let mut messages = vec![json!({"role":"user","content":blocks})];
        let mut shortest: Option<String> = None;
        for _ in 0..TRIES {
            let response = self
                .ask(model, include_str!("compact.txt"), &messages, &[], None)
                .await?;
            let line = response
                .content
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
                .trim()
                .to_owned();
            if line.is_empty() {
                return Err(Error::Invalid("Compactor returned an empty summary".into()));
            }
            if shortest.as_ref().is_none_or(|s| line.len() < s.len()) {
                shortest = Some(line.clone());
            }
            if line.len() <= NODE {
                break;
            }
            messages.push(json!({"role":"assistant","content":response.content}));
            messages.push(json!({"role":"user","content":format!("That line is {} bytes; the limit is 512. It must end where it is cut here:\n{}| ← LIMIT", line.len(), byte_prefix(&line, NODE))}));
        }
        shortest.ok_or_else(|| Error::Invalid("No compression attempt".into()))
    }
}

pub fn byte_prefix(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
pub fn cap(text: &str) -> String {
    let chars: Vec<_> = text.chars().collect();
    if chars.len() <= crate::CAP {
        return text.into();
    }
    let mut keep = crate::CAP - 80;
    let mut note = String::new();
    for _ in 0..4 {
        note = format!("\n[... {} characters omitted ...]\n", chars.len() - keep);
        keep = crate::CAP - note.chars().count();
    }
    format!(
        "{}{}{}",
        chars[..keep / 2].iter().collect::<String>(),
        note,
        chars[chars.len() - (keep - keep / 2)..]
            .iter()
            .collect::<String>()
    )
}
pub fn cache_blocks(text: &str) -> Vec<Value> {
    let ends: Vec<_> = text
        .char_indices()
        .enumerate()
        .filter_map(|(c, (b, ch))| (ch == '\n').then_some((c + 1, b + 1)))
        .collect();
    let count = text.chars().count();
    let mut start = 0;
    let mut blocks = vec![];
    for mark in [50_000, 80_000, 100_000] {
        if mark > count {
            continue;
        }
        if let Some((_, end)) = ends.iter().rev().find(|(c, _)| *c <= mark)
            && *end > start
        {
            blocks.push(json!({"type":"text","text":&text[start..*end],"cache_control":{"type":"ephemeral"}}));
            start = *end;
        }
    }
    if start < text.len() {
        blocks.push(json!({"type":"text","text":&text[start..]}));
    }
    blocks
}
pub fn scale() -> String {
    let text = "user: Build OptChat in Rust with egui; keep every message verbatim, sync writes before acknowledging, and use one endless chat. user: Corrections outrank tool noise; zoom before guessing. talk: Chose a binary summary tree and a stable chronological view. echo: Storage tests passed; crash recovery skips torn lines and preserves IDs. tool: Read the memory module and checked its append path. talk: Model credentials stay in the environment; live API validation remains pending. user: Keep the interface clear and usable.";
    let mut s = byte_prefix(text, NODE).to_owned();
    while s.len() < NODE {
        s.push(' ');
    }
    s
}
pub fn tools() -> Vec<Value> {
    vec![
        json!({"name":"zoom","description":"Open the line id+n of the view into the two lines of n/2 under it; n = 1 gives the message whole.","input_schema":{"type":"object","properties":{"id":{"type":"integer","minimum":0},"n":{"type":"integer","minimum":1}},"required":["id","n"],"additionalProperties":false}}),
        json!({"name":"date","description":"The date and time of message id.","input_schema":{"type":"object","properties":{"id":{"type":"integer","minimum":0}},"required":["id"],"additionalProperties":false}}),
    ]
}
