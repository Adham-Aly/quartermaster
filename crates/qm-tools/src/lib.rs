//! Every tool, defined once.
//!
//! A tool is a type implementing [`Tool`]: its name, its description (a Markdown file in this
//! crate's `descriptions/` folder, the only guidance the model gets), which roles get it, whether
//! a successful call counts as an effective action ([`ToolClass`]), and its typed
//! arguments and result. The JSON Schema both engines offer the model is generated from the
//! argument type with `schemars` ([`ToolDef::input_schema`]), so every engine sees exactly the
//! same text and schema.
//!
//! - [`defs`]: the argument and result types of every tool.
//! - [`registry`]: [`registry::TOOLS`], the one list of tool definitions, and lookups by role.
//! - [`handler`]: how a call runs. A handler implements [`handler::Handle`] for a tool and is
//!   bound in [`handlers::HANDLERS`]; the core dispatches every call through
//!   [`handler::Dispatcher`], which parses the arguments, runs the handler and reports whether the
//!   call counted.
//!
//! Argument types document their fields with `//` comments, never `///`: a doc comment would
//! become a nested description in the schema, and all guidance belongs in the description. What a
//! field means for the model goes in the tool's description file.

pub mod defs;
pub mod handler;
pub mod handlers;
pub mod registry;
mod spec;

pub use spec::{EngineScope, Tool, ToolClass, ToolDef, input_schema};
