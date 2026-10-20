//! What a tool is.

use qm_engine::ToolDescriptor;
use qm_types::domain::{EngineKind, Role};
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt;

pub use qm_types::rules::ToolClass;

/// Which engines serve a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EngineScope {
    /// Both engines: every orchestration and crewmate tool.
    Both,
    /// Only engines without their own file tools: the crewmate's file and shell tools. On Claude,
    /// Claude Code's own tools do this.
    ProviderOnly,
}

impl EngineScope {
    pub const fn serves(self, engine: EngineKind) -> bool {
        match self {
            Self::Both => true,
            Self::ProviderOnly => !matches!(engine, EngineKind::Claude),
        }
    }
}

/// One tool. Implemented by a marker type (`ReplyTool`, …) in [`crate::defs`].
pub trait Tool: 'static {
    /// The name the model calls it by.
    const NAME: &'static str;
    /// Unique across the registry: the name, or `name.variant` when one name has a role-specific
    /// variant (a secondmate's `start_crewmate`). Also the description file's name and the schema
    /// snapshot's name.
    const KEY: &'static str = Self::NAME;
    /// The top-level description: everything the model is told about the tool.
    const DESCRIPTION: &'static str;
    const CLASS: ToolClass;
    /// The roles that get it.
    const ROLES: &'static [Role];
    const SCOPE: EngineScope = EngineScope::Both;

    type Args: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
    type Output: Serialize + DeserializeOwned + Send + 'static;

    /// The text the model gets back for a successful call: compact JSON unless a tool says
    /// otherwise (the file tools return plain text, as Claude Code's do).
    fn render(output: &Self::Output) -> String {
        serde_json::to_string(output).expect("tool outputs always serialise")
    }
}

/// A tool definition with its types erased, as the registry holds it.
#[derive(Clone, Copy)]
pub struct ToolDef {
    pub name: &'static str,
    pub key: &'static str,
    pub description: &'static str,
    pub class: ToolClass,
    pub roles: &'static [Role],
    pub scope: EngineScope,
    /// Generates the argument schema.
    pub schema: fn() -> serde_json::Value,
}

impl ToolDef {
    pub const fn of<T: Tool>() -> Self {
        Self {
            name: T::NAME,
            key: T::KEY,
            description: T::DESCRIPTION,
            class: T::CLASS,
            roles: T::ROLES,
            scope: T::SCOPE,
            schema: input_schema::<T>,
        }
    }

    /// The JSON Schema of the arguments, as both engines offer it.
    pub fn input_schema(&self) -> serde_json::Value {
        (self.schema)()
    }

    pub fn serves(&self, role: Role, engine: EngineKind) -> bool {
        self.roles.contains(&role) && self.scope.serves(engine)
    }

    /// What an engine needs to offer this tool.
    pub fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.to_owned(),
            description: self.description.to_owned(),
            input_schema: self.input_schema(),
        }
    }
}

impl fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolDef")
            .field("key", &self.key)
            .field("class", &self.class)
            .field("roles", &self.roles)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// Generates a tool's argument schema: draft-07 vocabulary, every subschema inlined (no `$ref`, so
/// every provider can read it), no `$schema`, and no title or description at the root (the
/// description file is the one place for guidance).
pub fn input_schema<T: Tool>() -> serde_json::Value {
    let mut settings = SchemaSettings::draft07();
    settings.inline_subschemas = true;
    settings.meta_schema = None;
    let schema = settings.into_generator().into_root_schema_for::<T::Args>();
    let mut value = schema.to_value();
    if let Some(root) = value.as_object_mut() {
        root.remove("title");
        root.remove("description");
    }
    value
}
