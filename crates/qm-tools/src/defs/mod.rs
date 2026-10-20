//! The argument and result types of every tool.
//!
//! Each tool is a marker type (`ReplyTool`) implementing [`crate::Tool`], with an argument type
//! (`ReplyArgs`) and a result type. Arguments refuse unknown fields, so a typo or a field the
//! model may not set (autonomy) comes back as a refusal naming it. Fields are commented
//! with `//`, never `///` (see the crate docs); each tool's description file explains them to the
//! model.
//!
//! Changing a type here changes a contract: the schema every engine offers the model.

/// Defines a tool's marker type and its `Tool` impl; the description comes from
/// `descriptions/<name>.md`.
macro_rules! tool {
    (
        $(#[$meta:meta])*
        $marker:ident {
            name: $name:literal,
            class: $class:ident,
            roles: [$($role:ident),+ $(,)?],
            args: $args:ty,
            output: $out:ty $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct $marker;

        impl $crate::Tool for $marker {
            const NAME: &'static str = $name;
            const DESCRIPTION: &'static str = include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/descriptions/",
                $name,
                ".md"
            ))
            .trim_ascii();
            const CLASS: $crate::ToolClass = $crate::ToolClass::$class;
            const ROLES: &'static [qm_types::domain::Role] = &[$(qm_types::domain::Role::$role),+];
            type Args = $args;
            type Output = $out;
        }
    };
}

mod common;
mod conversation;
mod crew;
mod crewmate;
pub mod files;
mod github;
mod instructions;
mod projects;
mod secondmates;

pub use common::*;
pub use conversation::*;
pub use crew::*;
pub use crewmate::*;
pub use files::*;
pub use github::*;
pub use instructions::*;
pub use projects::*;
pub use secondmates::*;
