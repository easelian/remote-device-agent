//! Internal BiDi development channel. It deliberately does not implement the
//! public page ports or change Engine's CDP/v1 backend selection.
#![cfg_attr(not(test), allow(dead_code))]
mod command;
mod event;
pub(in crate::browser) mod page;
#[cfg(target_os = "linux")]
mod session;
mod startup;
mod wire;

use command::Command;
use event::{ContextId, Event};
