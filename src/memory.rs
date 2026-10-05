use crate::{Error, Result, VIEW};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio::io::AsyncWriteExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    User,
    Talk,
    Tool,
    Echo,
    Note,
}
impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Talk => "talk",
            Self::Tool => "tool",
            Self::Echo => "echo",
            Self::Note => "note",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub i: usize,
    pub kind: Kind,
    pub text: String,
    pub size: usize,
    pub date: DateTime<Local>,
}
impl Message {
    pub fn source(&self) -> String {
        format!("{}: {}", self.kind.label(), self.text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Part {
    pub l: u32,
    pub i: usize,
}
impl Part {
    pub fn width(self) -> usize {
        1usize << self.l
    }
    pub fn start(self) -> usize {
        self.i * self.width()
    }
    pub fn end(self) -> usize {
        self.start() + self.width()
    }
    pub fn children(self) -> [Self; 2] {
        [
            Self {
                l: self.l - 1,
                i: self.i * 2,
            },
            Self {
                l: self.l - 1,
                i: self.i * 2 + 1,
            },
        ]
    }
    pub fn address(self) -> String {
        format!("{}+{}", self.start(), self.width())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub l: u32,
    pub i: usize,
    pub text: String,
    pub size: usize,
}
impl Node {
    pub fn part(&self) -> Part {
        Part {
            l: self.l,
            i: self.i,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Memory {
    pub root: Vec<Message>,
    pub tree: BTreeMap<Part, Node>,
    pub view: Vec<Part>,
    pub budget: usize,
}
impl Default for Memory {
    fn default() -> Self {
        Self {
            root: vec![],
            tree: BTreeMap::new(),
            view: vec![],
            budget: VIEW,
        }
    }
}
impl Memory {
    pub fn push(&mut self, message: Message) {
        self.view.push(Part { l: 0, i: message.i });
        self.root.push(message);
        self.fit();
    }
    pub fn built(&self, p: Part) -> bool {
        self.tree.contains_key(&p)
    }
    pub fn text(&self, p: Part) -> &str {
        self.tree
            .get(&p)
            .map_or("(not summarized yet: zoom it)", |n| n.text.as_str())
    }
    pub fn line(&self, p: Part) -> String {
        format!(
            "{}|{}",
            p.address(),
            self.text(p).replace(['\n', '\r'], " ")
        )
    }
    pub fn bytes(&self) -> usize {
        self.view.iter().map(|p| self.text(*p).len()).sum()
    }
    pub fn first(&self) -> usize {
        self.view
            .iter()
            .find(|p| !self.built(**p))
            .map_or(self.root.len(), |p| p.start())
    }
    pub fn settled(&self) -> bool {
        self.first() == self.root.len()
    }
    pub fn render(&self) -> String {
        format!(
            "<chat>\n{}\n</chat>",
            self.view
                .iter()
                .map(|p| self.line(*p))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
    pub fn fit(&mut self) {
        while self.bytes() > self.budget {
            let mut best: Option<(usize, f64)> = None;
            for (j, pair) in self.view.windows(2).enumerate() {
                let [a, b] = [pair[0], pair[1]];
                if a.l != b.l
                    || a.i % 2 != 0
                    || b.i != a.i + 1
                    || !self.built(Part {
                        l: a.l + 1,
                        i: a.i / 2,
                    })
                {
                    continue;
                }
                let due = (self.root.len() - a.start()) as f64 / (4.0 * a.width() as f64);
                if best.is_none_or(|(_, weight)| due > weight) {
                    best = Some((j, due));
                }
            }
            let Some((j, _)) = best else {
                break;
            };
            let p = self.view[j];
            self.view.splice(
                j..j + 2,
                [Part {
                    l: p.l + 1,
                    i: p.i / 2,
                }],
            );
        }
    }
    pub fn source(&self, p: Part) -> Option<String> {
        if p.l == 0 {
            self.root.get(p.i).map(Message::source)
        } else {
            let [a, b] = p.children();
            Some(format!(
                "{}\n{}",
                self.tree.get(&a)?.text,
                self.tree.get(&b)?.text
            ))
        }
    }
    pub fn context(&self, p: Part) -> String {
        let end = if p.l == 0 { p.start() } else { p.end() };
        format!(
            "<chat>\n{}\n</chat>",
            self.view
                .iter()
                .take_while(|q| q.end() <= end)
                .map(|q| self.text(*q).replace(['\n', '\r'], " "))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
    pub fn eligible(&self) -> Vec<Part> {
        let mut result = vec![];
        for l in 0..usize::BITS - 1 {
            let width = 1usize << l;
            if width > self.root.len() {
                break;
            }
            for i in 0..self.root.len() / width {
                let p = Part { l, i };
                let end = if l == 0 { i } else { p.end() };
                if !self.built(p) && end <= self.first() && self.source(p).is_some() {
                    result.push(p);
                }
            }
        }
        result
    }
    pub fn zoom(&self, id: usize, n: usize) -> String {
        let absent = || format!("No line {id}+{n}.");
        if !n.is_power_of_two()
            || !id.is_multiple_of(n)
            || id.checked_add(n).is_none_or(|end| end > self.root.len())
        {
            return absent();
        }
        if n == 1 {
            return format!("{id}+0|{}", self.root[id].source());
        }
        let p = Part {
            l: n.trailing_zeros(),
            i: id / n,
        };
        let [a, b] = p.children();
        if !self.built(a) || !self.built(b) {
            return absent();
        }
        format!("{}\n{}", self.line(a), self.line(b))
    }
    pub fn date(&self, id: usize) -> String {
        self.root
            .get(id)
            .map_or_else(|| format!("No message {id}."), |m| m.date.to_rfc3339())
    }
    pub fn html(&self) -> String {
        fn esc(s: &str) -> String {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        }
        let mut s = String::from(
            "<!doctype html><meta charset=\"utf-8\"><title>OptChat memory</title><style>body{max-width:1000px;margin:40px auto;font:16px system-ui;background:#101821;color:#dbe6ef}pre{white-space:pre-wrap;overflow-wrap:anywhere}details{padding:12px;border-bottom:1px solid #34404d}small{color:#a5b4c5}</style><h1>OptChat memory</h1><h2>Current view</h2><pre>",
        );
        s.push_str(&esc(&self.render()));
        s.push_str("</pre>");
        for p in &self.view {
            s.push_str(&format!(
                "<details><summary>{} · {} bytes · {} — {}</summary><pre>{}</pre></details>",
                p.address(),
                self.text(*p).len(),
                esc(&self.date(p.start())),
                esc(&self.date(p.end() - 1)),
                esc(self.text(*p))
            ));
        }
        s.push_str("<h2>ROOT</h2>");
        for m in &self.root {
            s.push_str(&format!(
                "<details><summary>{}+1 · {} · {} bytes · {}</summary><pre>{}</pre></details>",
                m.i,
                m.kind.label(),
                m.size,
                esc(&m.date.to_rfc3339()),
                esc(&m.text)
            ));
        }
        let mut level = None;
        for (p, n) in &self.tree {
            if level != Some(p.l) {
                s.push_str(&format!("<h2>Level {}</h2>", p.l));
                level = Some(p.l);
            }
            s.push_str(&format!(
                "<details><summary>{} · {} bytes · {} — {}</summary><pre>{}</pre></details>",
                p.address(),
                n.size,
                esc(&self.date(p.start())),
                esc(&self.date(p.end() - 1)),
                esc(&n.text)
            ));
        }
        s
    }
}

pub struct Store {
    pub memory: Memory,
    pub warnings: Vec<String>,
    path: PathBuf,
    _lock: std::fs::File,
}
impl Store {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&path).await?;
        let lock_path = path.join("lock");
        let lock = tokio::task::spawn_blocking(move || {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(lock_path)?;
            f.try_lock().map_err(|e| {
                std::io::Error::other(format!("Chat already open or lock unavailable: {e}"))
            })?;
            Ok::<_, std::io::Error>(f)
        })
        .await
        .map_err(|e| Error::Invalid(e.to_string()))??;
        for name in ["main", "tree"] {
            tokio::fs::create_dir_all(path.join(name)).await?;
        }
        #[cfg(unix)]
        {
            tokio::fs::File::open(&path).await?.sync_all().await?;
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                tokio::fs::File::open(parent).await?.sync_all().await?;
            }
        }
        let mut store = Self {
            memory: Memory::default(),
            warnings: vec![],
            path,
            _lock: lock,
        };
        let messages: Vec<Message> = store.load("main").await?;
        let nodes: Vec<Node> = store.load("tree").await?;
        for node in nodes {
            if node.l >= usize::BITS - 1
                || node
                    .i
                    .checked_add(1)
                    .and_then(|i| i.checked_mul(1usize << node.l))
                    .is_none_or(|end| end > messages.len())
                || node.size != node.text.len()
                || node.text.is_empty()
            {
                return Err(Error::Invalid(
                    "Invalid tree record; refusing to rewrite history".into(),
                ));
            }
            store.memory.tree.entry(node.part()).or_insert(node);
        }
        for m in messages {
            if m.i != store.memory.root.len() || m.size != m.source().len() {
                return Err(Error::Invalid(
                    "Non-contiguous message IDs or invalid size".into(),
                ));
            }
            store.memory.push(m);
        }
        Ok(store)
    }
    async fn load<T: serde::de::DeserializeOwned>(&mut self, stream: &str) -> Result<Vec<T>> {
        let mut dir = tokio::fs::read_dir(self.path.join(stream)).await?;
        let mut paths = vec![];
        while let Some(entry) = dir.next_entry().await? {
            if entry.path().extension().is_some_and(|x| x == "jsonl") {
                paths.push(entry.path());
            }
        }
        paths.sort();
        let mut records = vec![];
        for path in paths {
            let bytes = tokio::fs::read(&path).await?;
            for (line, raw) in bytes.split(|b| *b == b'\n').enumerate() {
                if raw.is_empty() {
                    continue;
                }
                match serde_json::from_slice(raw) {
                    Ok(record) => records.push(record),
                    Err(e) => self.warnings.push(format!(
                        "{}:{} skipped torn/invalid JSON: {e}",
                        path.display(),
                        line + 1
                    )),
                }
            }
            if !bytes.is_empty() && !bytes.ends_with(b"\n") {
                let mut file = tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .await?;
                file.write_all(b"\n").await?;
                file.flush().await?;
                file.sync_all().await?;
            }
        }
        Ok(records)
    }
    async fn append<T: Serialize>(&self, stream: &str, record: &T) -> Result<()> {
        let dir = self.path.join(stream);
        let path = dir.join(format!("{}.jsonl", Local::now().format("%Y-%m-%d")));
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        let written = file.write(&line).await?;
        file.flush().await?;
        file.sync_all().await?;
        if written != line.len() {
            return Err(Error::Invalid(
                "Short append; restart to recover the torn line".into(),
            ));
        }
        #[cfg(unix)]
        tokio::fs::File::open(dir).await?.sync_all().await?;
        Ok(())
    }
    pub async fn message(&mut self, kind: Kind, text: String) -> Result<Message> {
        let m = Message {
            i: self.memory.root.len(),
            kind,
            size: kind.label().len() + 2 + text.len(),
            text,
            date: Local::now(),
        };
        self.append("main", &m).await?;
        self.memory.push(m.clone());
        Ok(m)
    }
    pub async fn node(&mut self, p: Part, text: String) -> Result<()> {
        if self.memory.built(p) {
            return Ok(());
        }
        if text.is_empty() || self.memory.source(p).is_none() {
            return Err(Error::Invalid("Empty node or missing sources".into()));
        }
        let n = Node {
            l: p.l,
            i: p.i,
            size: text.len(),
            text,
        };
        self.append("tree", &n).await?;
        self.memory.tree.insert(p, n);
        self.memory.fit();
        Ok(())
    }
}
