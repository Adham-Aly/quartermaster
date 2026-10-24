//! A crewmate's instruction files and skills.
//!
//! **Instruction files.** Your `~/.agents/AGENTS.md` first, then the worktree's `AGENTS.md`, or
//! its `CLAUDE.md` only when there is no `AGENTS.md`. Nested `AGENTS.md` files are never loaded up
//! front (the crewmate prompt tells the crewmate to read the one in any folder it works in). Each
//! is read as UTF-8 (lossy) and cut at [`INSTRUCTION_FILE_CAP`] with a pointer to the rest.
//!
//! **Skills.** Your `~/.agents/skills/*/`, and the worktree's `.agents/skills/*/`, or its
//! `.claude/skills/*/` only when `.agents/skills/` doesn't exist. A skill is a folder with a
//! `SKILL.md`; its name is the frontmatter `name:`, else the folder's name; its description the
//! frontmatter `description:`, else the first non-empty line. On a name clash the project's
//! skill wins. Every chosen skill is copied into `tasks/C-0042/skills/<name>/`, so both engines
//! get exactly the same set and a later edit in the worktree doesn't change it mid-task.
//!
//! **Nothing else of the project's is loaded.** No settings, hooks, MCP servers, agents or
//! commands. A skill folder's `hooks/` or `.mcp.json` is copied as plain files and never loaded:
//! Claude gets only the skills-only plugin folder (`qm_engine_claude::plugin`), and the provider
//! engine only lists names and descriptions.

use qm_prompts::brief::{InstructionFile, InstructionSource, SkillEntry};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Component, Path, PathBuf};

/// The most of one instruction file the prompt carries, in bytes.
pub const INSTRUCTION_FILE_CAP: usize = 64 * 1024;

/// A skill's own file.
pub const SKILL_FILE: &str = "SKILL.md";

/// What the app hands a crewmate: its instruction files and its skills.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gathered {
    /// Your file first, then the project's.
    pub files: Vec<InstructionFile>,
    /// The gathered skills, by name; each `path` is its copied `SKILL.md`.
    pub skills: Vec<SkillEntry>,
    /// `tasks/C-0042/skills/`, when at least one skill was gathered.
    pub skills_dir: Option<PathBuf>,
}

/// Gathering failed: a file or folder couldn't be read or written.
#[derive(Debug, thiserror::Error)]
pub enum SkillsError {
    #[error("{action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_err(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> SkillsError + use<> {
    let path = path.to_owned();
    move |source| SkillsError::Io {
        action,
        path,
        source,
    }
}

/// Reads the instruction files and gathers the skills into `task_skills`, replacing whatever an
/// earlier call put there, so calling it again gives the same result. `home` is your home
/// folder, `worktree` the task's worktree.
pub fn gather(home: &Path, worktree: &Path, task_skills: &Path) -> Result<Gathered, SkillsError> {
    let files = instruction_files(home, worktree)?;

    let personal = skills_in(&home.join(".agents").join("skills"))?;
    let project_root = {
        let agents = worktree.join(".agents").join("skills");
        if agents.is_dir() {
            agents
        } else {
            worktree.join(".claude").join("skills")
        }
    };
    let project = skills_in(&project_root)?;

    // The project's skill wins a name clash.
    let mut chosen: BTreeMap<String, FoundSkill> = BTreeMap::new();
    for skill in personal.into_iter().chain(project) {
        chosen.insert(skill.name.clone(), skill);
    }

    replace_dir(task_skills)?;
    let mut skills = Vec::with_capacity(chosen.len());
    for (name, skill) in &chosen {
        let dest = task_skills.join(name);
        copy_skill(&skill.folder, &dest)?;
        skills.push(SkillEntry {
            name: name.clone(),
            description: skill.description.clone(),
            path: dest.join(SKILL_FILE).to_string_lossy().into_owned(),
        });
    }
    let skills_dir = (!skills.is_empty()).then(|| task_skills.to_owned());
    Ok(Gathered {
        files,
        skills,
        skills_dir,
    })
}

/// The app's plan-format skill's name, which wins over any skill of the same name.
pub const PLAN_SKILL: &str = "quartermaster-plan";

/// `gathered` with the app's plan-format skill for `mode` written into `task_skills`, replacing a
/// gathered skill of the same name and any earlier mode's text. A plan task's session gets it.
pub fn with_plan_skill(
    gathered: &Gathered,
    task_skills: &Path,
    mode: qm_types::domain::PlanMode,
) -> Result<Gathered, SkillsError> {
    let dest = task_skills.join(PLAN_SKILL);
    replace_dir(&dest)?;
    let file = dest.join(SKILL_FILE);
    let text = qm_plans::skill(mode);
    fs::write(&file, text).map_err(io_err("can't write", &file))?;
    let description = text
        .lines()
        .find_map(|l| l.strip_prefix("description:"))
        .map(|d| d.trim().to_owned())
        .unwrap_or_default();
    let mut out = gathered.clone();
    out.skills.retain(|s| s.name != PLAN_SKILL);
    out.skills.push(SkillEntry {
        name: PLAN_SKILL.to_owned(),
        description,
        path: file.to_string_lossy().into_owned(),
    });
    out.skills.sort_by(|a, b| a.name.cmp(&b.name));
    out.skills_dir = Some(task_skills.to_owned());
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Instruction files.

fn instruction_files(home: &Path, worktree: &Path) -> Result<Vec<InstructionFile>, SkillsError> {
    let mut files = Vec::new();
    let personal = home.join(".agents").join("AGENTS.md");
    if let Some(text) = read_capped(&personal)? {
        files.push(InstructionFile {
            source: InstructionSource::Personal,
            shown_as: "~/.agents/AGENTS.md".to_owned(),
            text,
        });
    }
    let agents = worktree.join("AGENTS.md");
    let (path, shown_as) = if agents.is_file() {
        (agents, "AGENTS.md")
    } else {
        (worktree.join("CLAUDE.md"), "CLAUDE.md")
    };
    if let Some(text) = read_capped(&path)? {
        files.push(InstructionFile {
            source: InstructionSource::Project,
            shown_as: shown_as.to_owned(),
            text,
        });
    }
    Ok(files)
}

/// A file's text, cut at [`INSTRUCTION_FILE_CAP`]; `None` when it doesn't exist.
fn read_capped(path: &Path) -> Result<Option<String>, SkillsError> {
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("can't read", path)(e)),
    };
    let text = String::from_utf8_lossy(&bytes);
    if text.len() <= INSTRUCTION_FILE_CAP {
        return Ok(Some(text.into_owned()));
    }
    let mut end = INSTRUCTION_FILE_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(Some(format!(
        "{}\n… (cut; read the rest at {})",
        &text[..end],
        path.display()
    )))
}

// ---------------------------------------------------------------------------------------------
// Skills.

/// A skill found in one of the skills folders.
#[derive(Clone, Debug)]
struct FoundSkill {
    name: String,
    description: String,
    folder: PathBuf,
}

/// The skills in `root` (each a folder with a `SKILL.md`), in folder-name order; a name used
/// twice in one folder keeps its first skill. A missing `root` has none.
fn skills_in(root: &Path) -> Result<Vec<FoundSkill>, SkillsError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(Vec::new());
        }
        Err(e) => return Err(io_err("can't list", root)(e)),
    };
    let mut folders: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err("can't list", root))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // The skill folder itself may be a link (a skill installed elsewhere): it is followed.
        if name.starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        folders.push((name, entry.path()));
    }
    folders.sort();

    let mut seen = BTreeMap::new();
    for (folder_name, folder) in folders {
        let skill_file = folder.join(SKILL_FILE);
        if !skill_file_usable(&skill_file) {
            continue;
        }
        let bytes = fs::read(&skill_file).map_err(io_err("can't read", &skill_file))?;
        let text = String::from_utf8_lossy(&bytes);
        let front = Frontmatter::parse(&text);
        let name = front
            .name
            .filter(|n| safe_name(n))
            .unwrap_or_else(|| folder_name.clone());
        let description = front
            .description
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| first_line(front.body));
        seen.entry(name.clone()).or_insert(FoundSkill {
            name,
            description,
            folder,
        });
    }
    Ok(seen.into_values().collect())
}

/// `SKILL.md` exists and is a file, and if it's a link it stays inside the skill.
fn skill_file_usable(skill_file: &Path) -> bool {
    match fs::symlink_metadata(skill_file) {
        Ok(meta) if meta.file_type().is_symlink() => {
            skill_file.is_file()
                && fs::read_link(skill_file)
                    .is_ok_and(|target| link_stays_inside(Path::new(""), &target))
        }
        Ok(meta) => meta.is_file(),
        Err(_) => false,
    }
}

/// A name usable as a folder of its own: not empty, no path separators, not hidden.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && name.trim() == name
}

/// The first non-empty line of the body, without a Markdown heading's `#`s.
fn first_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches('#').trim().to_owned())
        .unwrap_or_default()
}

/// A `SKILL.md`'s frontmatter: its `name:` and `description:`, and the text after it.
#[derive(Debug)]
struct Frontmatter<'a> {
    name: Option<String>,
    description: Option<String>,
    body: &'a str,
}

impl<'a> Frontmatter<'a> {
    /// Reads the block between a first line of `---` and the next `---` line. Values may be
    /// quoted, or folded over indented lines after `>` or `|`.
    fn parse(text: &'a str) -> Self {
        let none = Frontmatter {
            name: None,
            description: None,
            body: text,
        };
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut lines = text.split_inclusive('\n');
        if lines.next().map(str::trim_end) != Some("---") {
            return none;
        }
        let mut block: Vec<&str> = Vec::new();
        let mut consumed = text.find('\n').map_or(text.len(), |i| i + 1);
        let mut closed = false;
        for line in lines {
            consumed += line.len();
            if line.trim_end() == "---" {
                closed = true;
                break;
            }
            block.push(line.trim_end_matches(['\n', '\r']));
        }
        if !closed {
            return none;
        }
        Frontmatter {
            name: value(&block, "name"),
            description: value(&block, "description"),
            body: &text[consumed..],
        }
    }
}

/// The value of top-level `key:` in a frontmatter block.
fn value(block: &[&str], key: &str) -> Option<String> {
    let (i, rest) = block.iter().enumerate().find_map(|(i, line)| {
        line.strip_prefix(key)
            .and_then(|r| r.strip_prefix(':'))
            .map(|r| (i, r.trim()))
    })?;
    if matches!(rest, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
        let folded: Vec<&str> = block[i + 1..]
            .iter()
            .take_while(|l| l.is_empty() || l.starts_with([' ', '\t']))
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        return Some(folded.join(" "));
    }
    let unquoted = rest
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .or_else(|| rest.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
        .unwrap_or(rest);
    Some(unquoted.trim().to_owned())
}

/// Empties `dir` (creating it, mode 0700, if needed), so a gather never keeps an earlier copy.
fn replace_dir(dir: &Path) -> Result<(), SkillsError> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir_all(dir).map_err(io_err("can't clear", dir))?;
        }
        Ok(_) => fs::remove_file(dir).map_err(io_err("can't clear", dir))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("can't inspect", dir)(e)),
    }
    make_dir(dir)
}

fn make_dir(dir: &Path) -> Result<(), SkillsError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(io_err("can't create", dir))
}

/// Copies a skill folder: files and folders, links as links when they stay inside the skill;
/// a link that leads out of it is left out, never followed.
fn copy_skill(from: &Path, to: &Path) -> Result<(), SkillsError> {
    make_dir(to)?;
    copy_tree(from, to, Path::new(""))
}

fn copy_tree(root: &Path, dest_root: &Path, rel: &Path) -> Result<(), SkillsError> {
    let dir = root.join(rel);
    let mut entries: Vec<_> = fs::read_dir(&dir)
        .map_err(io_err("can't list", &dir))?
        .collect::<Result<_, _>>()
        .map_err(io_err("can't list", &dir))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let rel_child = rel.join(entry.file_name());
        let src = root.join(&rel_child);
        let dest = dest_root.join(&rel_child);
        let kind = entry.file_type().map_err(io_err("can't inspect", &src))?;
        if kind.is_symlink() {
            let target = fs::read_link(&src).map_err(io_err("can't read the link", &src))?;
            if link_stays_inside(rel, &target) {
                symlink(&target, &dest).map_err(io_err("can't link", &dest))?;
            } else {
                tracing::debug!(link = %src.display(), "a link out of a skill is left out");
            }
        } else if kind.is_dir() {
            make_dir(&dest)?;
            copy_tree(root, dest_root, &rel_child)?;
        } else if kind.is_file() {
            fs::copy(&src, &dest).map_err(io_err("can't copy", &src))?;
        }
        // Sockets, FIFOs and devices are never part of a skill.
    }
    Ok(())
}

/// Whether a link at `dir/<name>` (with `dir` relative to the skill's folder) pointing at
/// `target` stays inside the skill: a relative target that never climbs above the folder.
fn link_stays_inside(dir: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth: usize = 0;
    for component in dir.components().chain(target.components()) {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}
