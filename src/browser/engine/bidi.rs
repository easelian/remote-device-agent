//! Native WebDriver BiDi transport, lifecycle and semantic page adapter.
#![cfg_attr(not(test), allow(dead_code))]
mod command;
#[cfg(target_os = "linux")]
mod downloads;
mod event;
pub(in crate::browser) mod page;
#[cfg(target_os = "linux")]
mod session;
mod startup;
mod wire;

use command::Command;
use event::{ContextId, Event};
