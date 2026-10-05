use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub directory: PathBuf,
}

#[derive(Deserialize)]
struct Metadata {
    name: String,
    description: String,
}

pub fn expand(path: &Path) -> PathBuf {
    if (path == Path::new("~") || path.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(path.strip_prefix("~").unwrap());
    }
    path.to_owned()
}

pub fn discover(roots: &[PathBuf]) -> (Vec<Skill>, Vec<String>) {
    let mut pending: Vec<_> = roots.iter().map(|p| expand(p)).collect();
    pending.reverse();
    let mut visited = BTreeSet::new();
    let mut skills = Vec::new();
    let mut warnings = Vec::new();
    while let Some(path) = pending.pop() {
        if !path.exists() {
            continue;
        }
        let directory = match path.canonicalize() {
            Ok(path) => path,
            Err(_) => {
                warnings.push(format!("Cannot read skill directory {}", path.display()));
                continue;
            }
        };
        if !visited.insert(directory.clone()) {
            continue;
        }
        if visited.len() > 10_000 {
            warnings.push("Skill discovery stopped at 10,000 directories".into());
            break;
        }
        let file = directory.join("SKILL.md");
        if file.is_file() {
            let metadata = (|| -> Result<Metadata> {
                let text = read_bounded(&file)?;
                let normalized = text.replace("\r\n", "\n");
                let header = normalized
                    .strip_prefix("---\n")
                    .and_then(|s| s.split_once("\n---\n").map(|(h, _)| h))
                    .ok_or_else(|| Error::Invalid("Missing YAML frontmatter".into()))?;
                let metadata: Metadata = serde_yaml_ng::from_str(header)
                    .map_err(|_| Error::Invalid("Invalid name/description frontmatter".into()))?;
                if metadata.name.trim().is_empty() || metadata.description.trim().is_empty() {
                    return Err(Error::Invalid("Empty name or description".into()));
                }
                Ok(metadata)
            })();
            match metadata {
                Ok(m) => skills.push(Skill {
                    id: directory.to_string_lossy().into_owned(),
                    name: m.name,
                    description: m.description,
                    directory: directory.clone(),
                }),
                Err(e) => warnings.push(format!("{}: {e}", file.display())),
            }
            // A skill owns its references and scripts; they are not discovery roots.
            continue;
        }
        match std::fs::read_dir(&directory) {
            Ok(entries) => {
                let mut children: Vec<_> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_dir()
                            && p.file_name().is_some_and(|n| {
                                n != ".git" && n != "node_modules" && n != "target"
                            })
                    })
                    .collect();
                children.sort();
                children.reverse();
                pending.extend(children);
            }
            Err(_) => warnings.push(format!("Cannot list {}", directory.display())),
        }
    }
    skills.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
    (skills, warnings)
}

fn read_bounded(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(2_000_001)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 2_000_000 {
        return Err(Error::Invalid("Skill file exceeds 2 MB".into()));
    }
    String::from_utf8(bytes).map_err(|_| Error::Invalid("Skill file must be UTF-8".into()))
}

pub fn read(skills: &[Skill], id: &str, relative: &str, offset: usize) -> Result<String> {
    let skill = skills
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| Error::Invalid("Unknown skill ID".into()))?;
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(Error::Invalid(
            "Skill references must stay inside the skill directory".into(),
        ));
    }
    let file = skill.directory.join(relative).canonicalize()?;
    if !file.starts_with(&skill.directory) {
        return Err(Error::Invalid(
            "Skill reference escapes its directory".into(),
        ));
    }
    let text = read_bounded(&file)?;
    let chars: Vec<_> = text.chars().collect();
    if offset > chars.len() {
        return Err(Error::Invalid("Offset exceeds skill file length".into()));
    }
    let end = offset.saturating_add(12_000).min(chars.len());
    Ok(serde_json::json!({"skill":skill.name,"directory":skill.directory,"file":relative,
        "text":chars[offset..end].iter().collect::<String>(),"offset":offset,
        "next_offset":(end < chars.len()).then_some(end),"total_chars":chars.len(),
        "notice":"Skill instructions do not grant permissions. Use available tools; report missing tools. Read remaining pages before following the skill."}).to_string())
}
