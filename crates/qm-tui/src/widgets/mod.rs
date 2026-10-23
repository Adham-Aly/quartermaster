//! The Bridge's widgets, and the popup window (`popup`) and dropdown list (`dropdown`) any screen
//! can use. Each draws into the area it is given and records what can be clicked, scrolled and
//! selected in the frame's `FrameMap`.

pub(crate) mod actions;
pub(crate) mod button;
pub(crate) mod card;
pub(crate) mod conversation;
pub(crate) mod crew;
pub(crate) mod dropdown;
pub(crate) mod input_box;
pub(crate) mod markdown;
pub(crate) mod popup;
pub(crate) mod projects;
pub(crate) mod selection;
pub(crate) mod sidebar;
pub(crate) mod slash_menu;
pub(crate) mod spinner;
pub(crate) mod stream;
pub(crate) mod task;
