//! Projects: finding and configuring them, and cloning a repository.

use super::Ack;
use qm_types::domain::{DeliveryMode, MergeMethod};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

tool! {
    /// Looks a folder up by name in the search roots from settings. Read only.
    FindProjectFolderTool {
        name: "find_project_folder",
        class: Read,
        roles: [Quartermaster],
        args: FindProjectFolderArgs,
        output: FoundFolders,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindProjectFolderArgs {
    pub name: String,
}

/// Folders that matched, best first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundFolders {
    pub folders: Vec<String>,
}

tool! {
    /// Clones a repository onto this Mac, with no card. Nothing is registered: a task started in
    /// the folder it returns makes it a project.
    CloneRepositoryTool {
        name: "clone_repository",
        class: Act,
        roles: [Quartermaster, Secondmate],
        args: CloneRepositoryArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloneRepositoryArgs {
    // The repository: a URL, or owner/name for GitHub.
    pub url: String,
    // The parent folder; the first search root from settings, else your home folder, when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

tool! {
    /// Changes a project's settings. There is no field for autonomy or the verification level: only
    /// you change those, from settings.
    ConfigureProjectTool {
        name: "configure_project",
        class: Act,
        roles: [Quartermaster],
        args: ConfigureProjectArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigureProjectArgs {
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_method: Option<MergeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_to_copy: Option<Vec<String>>,
    // Set or clear the setup command (not both).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_command: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_setup_command: bool,
}
