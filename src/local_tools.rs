use crate::{Error, Result, integrations::Outcome};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

pub fn definitions() -> Vec<Value> {
    vec![
        json!({"name":"shell","description":"Run a shell command on the user's computer using /bin/sh. Specify an absolute working directory. Commands can modify files and access the network. Output is bounded; timeout defaults to 60 seconds (maximum 600).","input_schema":{"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"timeout_seconds":{"type":"integer","minimum":1,"maximum":600}},"required":["command","cwd"],"additionalProperties":false}}),
        json!({"name":"read_file","description":"Read a UTF-8 local file using an absolute path (or ~/). Returns up to 30000 bytes starting at byte offset; use next_offset to continue.","input_schema":{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":0}},"required":["path"],"additionalProperties":false}}),
        json!({"name":"write_file","description":"Write UTF-8 text to a local file at an absolute path (or ~/). Parent directory must exist. Set overwrite=true explicitly to replace an existing file; read it first to preserve unrelated content.","input_schema":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"},"overwrite":{"type":"boolean"}},"required":["path","content"],"additionalProperties":false}}),
        json!({"name":"list_directory","description":"List up to 1000 entries in an absolute local directory (or ~/). Use shell for larger listings or searches.","input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}}),
    ]
}

pub fn contains(name: &str) -> bool {
    matches!(
        name,
        "shell" | "read_file" | "write_file" | "list_directory"
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Shell {
    command: String,
    cwd: PathBuf,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
}
fn default_timeout() -> u64 {
    60
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    path: PathBuf,
    #[serde(default)]
    offset: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Write {
    path: PathBuf,
    content: String,
    #[serde(default)]
    overwrite: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Directory {
    path: PathBuf,
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    let path = crate::skills::expand(&path);
    if !path.is_absolute() {
        return Err(Error::Invalid("An absolute path is required".into()));
    }
    Ok(path)
}

// Kill the entire process group even when the async task is dropped.
struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // SAFETY: the group was created for this child; negative PID targets that group.
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}
async fn drain(mut stream: impl AsyncRead + Unpin) -> std::io::Result<String> {
    let mut saved = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let keep = n.min(crate::CAP.saturating_sub(saved.len()));
        saved.extend_from_slice(&buf[..keep]);
        truncated |= keep < n;
    }
    let mut text = String::from_utf8_lossy(&saved).into_owned();
    if truncated {
        text.push_str("\n[output truncated]");
    }
    Ok(text)
}

pub async fn call(name: &str, input: Value, cancel: &CancellationToken) -> Outcome {
    match execute(name, input, cancel).await {
        Ok(text) => Outcome { text, error: false },
        Err(error) => Outcome::error(error.to_string()),
    }
}
async fn execute(name: &str, input: Value, cancel: &CancellationToken) -> Result<String> {
    if cancel.is_cancelled() {
        return Err(Error::Invalid("Canceled before execution".into()));
    }
    match name {
        "shell" => {
            let args: Shell = serde_json::from_value(input)?;
            if !(1..=600).contains(&args.timeout_seconds) {
                return Err(Error::Invalid("Timeout must be 1–600 seconds".into()));
            }
            let mut command = tokio::process::Command::new("/bin/sh");
            command
                .arg("-c")
                .arg(args.command)
                .current_dir(absolute(args.cwd)?)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .process_group(0);
            let mut child = command.spawn()?;
            let group = ProcessGroup(
                child
                    .id()
                    .ok_or_else(|| Error::Invalid("Process did not start".into()))?,
            );
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            let result = tokio::select! {
                _ = cancel.cancelled() => Err(Error::Invalid("Command canceled".into())),
                result = tokio::time::timeout(Duration::from_secs(args.timeout_seconds), async {
                    tokio::try_join!(child.wait(), drain(stdout), drain(stderr))
                }) => match result {
                    Ok(Ok((status, stdout, stderr))) => {
                        let text = json!({"exit_code":status.code(),"stdout":stdout,"stderr":stderr}).to_string();
                        if status.success() { Ok(text) } else { Err(Error::Invalid(text)) }
                    },
                    Ok(Err(e)) => Err(e.into()),
                    Err(_) => Err(Error::Invalid("Command timed out".into())),
                }
            };
            drop(group);
            let _ = child.wait().await;
            result
        }
        "read_file" => {
            use tokio::io::AsyncSeekExt;
            let args: Read = serde_json::from_value(input)?;
            let mut file = tokio::fs::File::open(absolute(args.path)?).await?;
            file.seek(std::io::SeekFrom::Start(args.offset)).await?;
            let mut bytes = Vec::new();
            file.take((crate::CAP + 4) as u64)
                .read_to_end(&mut bytes)
                .await?;
            let n = match std::str::from_utf8(&bytes) {
                Ok(_) => bytes.len().min(crate::CAP),
                Err(e) if e.error_len().is_none() => e.valid_up_to().min(crate::CAP),
                Err(_) => {
                    return Err(Error::Invalid(
                        "File is not UTF-8, or offset splits a character".into(),
                    ));
                }
            };
            let mut end = n;
            while std::str::from_utf8(&bytes[..end]).is_err() {
                end -= 1;
            }
            Ok(json!({"text":std::str::from_utf8(&bytes[..end]).unwrap(),"next_offset":if end < bytes.len() {Some(args.offset + end as u64)} else {None}}).to_string())
        }
        "write_file" => {
            let args: Write = serde_json::from_value(input)?;
            let path = absolute(args.path)?;
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true);
            if args.overwrite {
                options.create(true).truncate(true);
            } else {
                options.create_new(true);
            }
            let mut file = options.open(path).await?;
            file.write_all(args.content.as_bytes()).await?;
            file.sync_all().await?;
            Ok(format!("Wrote {} bytes", args.content.len()))
        }
        "list_directory" => {
            let args: Directory = serde_json::from_value(input)?;
            let mut dir = tokio::fs::read_dir(absolute(args.path)?).await?;
            let mut entries = Vec::new();
            while let Some(entry) = dir.next_entry().await? {
                if entries.len() == 1000 {
                    return Ok(json!({"entries":entries,"truncated":true}).to_string());
                }
                entries.push(entry.file_name().to_string_lossy().into_owned());
            }
            entries.sort();
            Ok(json!({"entries":entries,"truncated":false}).to_string())
        }
        _ => Err(Error::Invalid("Unknown local tool".into())),
    }
}
