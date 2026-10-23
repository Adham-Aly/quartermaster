//! `find_project_folder`, `clone_repository` and `configure_project`.
//!
//! Each handler checks what it can from the arguments alone, then hands the call to the core's
//! project registry through the [`ToolHost`](crate::handler::ToolHost), which checks the facts
//! (the folder exists and is a repository, the name is free, the project is registered). Autonomy
//! and the verification level have no field in `configure_project`'s arguments, so a call naming
//! either is refused before it gets here (unknown fields are refused).

use crate::defs::{
    Ack, CloneRepositoryArgs, CloneRepositoryTool, ConfigureProjectArgs, ConfigureProjectTool,
    FindProjectFolderArgs, FindProjectFolderTool, FoundFolders,
};
use crate::handler::{Handle, ToolCx, ToolFailure};
use qm_types::BoxFuture;
use qm_types::error::Refusal;
use std::path::{Component, Path};

/// The longest project name.
pub const MAX_PROJECT_NAME_CHARS: usize = 64;

fn refuse(reason: impl Into<String>) -> ToolFailure {
    Refusal::new(reason).into()
}

/// A project name the registry can hold: not blank, one folder name (no `/`), printable, at most
/// [`MAX_PROJECT_NAME_CHARS`].
pub fn check_project_name(name: &str) -> Result<(), Refusal> {
    let n = name.chars().count();
    if name.is_empty() {
        return Err(Refusal::new("a project needs a name"));
    }
    if n > MAX_PROJECT_NAME_CHARS {
        return Err(Refusal::new(format!(
            "a project name is at most {MAX_PROJECT_NAME_CHARS} characters (this one is {n})"
        )));
    }
    if name.starts_with('.') {
        return Err(Refusal::new(
            "a project name can't start with a dot; use the folder's own name",
        ));
    }
    if name.contains(['/', '\\']) || name.chars().any(char::is_control) {
        return Err(Refusal::new(
            "a project name is one folder name, without slashes or control characters",
        ));
    }
    Ok(())
}

/// A file to copy into each worktree: a relative path inside the checkout.
fn check_copy_path(path: &str) -> Result<(), Refusal> {
    let p = Path::new(path);
    let inside = !path.trim().is_empty()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if inside {
        Ok(())
    } else {
        Err(Refusal::new(format!(
            "files_to_copy takes paths relative to the project folder, inside it; {path:?} isn't one"
        )))
    }
}

impl Handle for FindProjectFolderTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: FindProjectFolderArgs,
    ) -> BoxFuture<'a, Result<FoundFolders, ToolFailure>> {
        Box::pin(async move {
            let name = args.name.trim();
            if name.is_empty() {
                return Err(refuse("find_project_folder needs a folder name"));
            }
            if name.contains('/') {
                return Err(refuse(
                    "find_project_folder looks up one folder name, not a path; to work in a \
                     path you already know, name it to start_crewmate",
                ));
            }
            let folders = cx.host.find_project_folder(cx.caller, name).await?;
            Ok(FoundFolders { folders })
        })
    }
}

impl Handle for CloneRepositoryTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: CloneRepositoryArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            let url = args.url.trim();
            if url.is_empty() || url.chars().any(char::is_whitespace) || url.starts_with('-') {
                return Err(refuse(
                    "clone_repository needs one repository URL, or owner/name for GitHub",
                ));
            }
            let location = args
                .location
                .as_deref()
                .map(str::trim)
                .filter(|l| !l.is_empty());
            cx.host.clone_repository(cx.caller, url, location).await
        })
    }
}

impl Handle for ConfigureProjectTool {
    fn handle<'a>(
        cx: &'a ToolCx<'a>,
        args: ConfigureProjectArgs,
    ) -> BoxFuture<'a, Result<Ack, ToolFailure>> {
        Box::pin(async move {
            let mut args = args;
            args.project = args.project.trim().to_owned();
            if args.project.is_empty() {
                return Err(refuse("configure_project needs the project's name"));
            }
            if args.setup_command.is_some() && args.clear_setup_command {
                return Err(refuse(
                    "set setup_command or clear it with clear_setup_command, not both",
                ));
            }
            if let Some(command) = &args.setup_command {
                if command.trim().is_empty() {
                    return Err(refuse(
                        "setup_command can't be blank; use clear_setup_command to remove it",
                    ));
                }
                args.setup_command = Some(command.trim().to_owned());
            }
            if let Some(branch) = &args.default_branch {
                let branch = branch.trim();
                if branch.is_empty() || branch.chars().any(char::is_whitespace) {
                    return Err(refuse("default_branch is one branch name, without spaces"));
                }
                args.default_branch = Some(branch.to_owned());
            }
            if let Some(files) = &args.files_to_copy {
                for file in files {
                    check_copy_path(file)?;
                }
            }
            let nothing = args.delivery.is_none()
                && args.merge_method.is_none()
                && args.default_branch.is_none()
                && args.files_to_copy.is_none()
                && args.setup_command.is_none()
                && !args.clear_setup_command;
            if nothing {
                return Err(refuse(
                    "configure_project needs at least one setting to change",
                ));
            }
            cx.host.configure_project(cx.caller, &args).await
        })
    }
}
