//! The skills-only plugin folder.
//!
//! A crewmate's skills, gathered by the app into `tasks/C-0042/skills/`, reach Claude Code as a
//! local plugin that holds nothing but them: `<session_dir>/claude-plugin/.claude-plugin/
//! plugin.json` and a `skills` symlink to the gathered folder, passed with
//! `--plugin-dir-no-mcp=`. Nothing else goes in it, so a skill folder's hooks or MCP config are
//! never loaded.

use std::io;
use std::path::{Path, PathBuf};

/// The plugin's manifest.
pub const PLUGIN_MANIFEST: &str = r#"{"name":"quartermaster-skills","version":"0.0.0"}"#;

/// Writes (or refreshes) the plugin folder for `skills_dir` under `session_dir`; returns the
/// folder to pass to `claude`.
pub fn write_plugin(session_dir: &Path, skills_dir: &Path) -> io::Result<PathBuf> {
    let root = session_dir.join("claude-plugin");
    let manifest_dir = root.join(".claude-plugin");
    std::fs::create_dir_all(&manifest_dir)?;
    std::fs::write(manifest_dir.join("plugin.json"), PLUGIN_MANIFEST)?;
    let link = root.join("skills");
    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(&link)? != skills_dir {
                std::fs::remove_file(&link)?;
                std::os::unix::fs::symlink(skills_dir, &link)?;
            }
        }
        Ok(meta) if meta.is_dir() => {
            std::fs::remove_dir_all(&link)?;
            std::os::unix::fs::symlink(skills_dir, &link)?;
        }
        Ok(_) => {
            std::fs::remove_file(&link)?;
            std::os::unix::fs::symlink(skills_dir, &link)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            std::os::unix::fs::symlink(skills_dir, &link)?;
        }
        Err(e) => return Err(e),
    }
    Ok(root)
}
