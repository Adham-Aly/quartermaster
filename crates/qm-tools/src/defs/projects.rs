//! Projects: finding, proposing, configuring and removing them.

use super::{Ack, Held};
use qm_types::domain::{DeliveryMode, MergeMethod, ProjectAction};
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
    /// Proposes linking, cloning or creating a project; always a held action, with the location for
    /// you to confirm or change.
    ProposeProjectTool {
        name: "propose_project",
        class: Act,
        roles: [Quartermaster],
        args: ProposeProjectArgs,
        output: Held,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposeProjectArgs {
    pub action: ProjectAction,
    pub name: String,
    // link: the folder to link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    // clone: the repository URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    // clone and create: the parent folder; the app suggests one when absent.
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

tool! {
    /// Unregisters a project; never deletes files.
    RemoveProjectTool {
        name: "remove_project",
        class: Act,
        roles: [Quartermaster],
        args: RemoveProjectArgs,
        output: Ack,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveProjectArgs {
    pub project: String,
}
