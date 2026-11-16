use super::{
    Command,
    event::Contexts,
    wire::{Bidi, Error, Events, Fault},
};
use std::time::Instant;

pub(super) struct Boot {
    pub product: String,
    pub version: String,
    pub contexts: Contexts,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub prompts_ignored: bool,
}

/// Transport readiness is distinct from page initialization and public capability admission.
pub(super) async fn initialize(
    channel: &Bidi,
    events: &mut Events,
    deadline: Instant,
) -> Result<Boot, Error> {
    let bad = || Error::Transport(Fault::Startup);
    let status = channel.call(Command::Status, deadline).await?.value;
    if status["ready"] != true {
        return Err(bad());
    }
    let session = channel.call(Command::NewSession, deadline).await?.value;
    let capabilities = &session["capabilities"];
    if capabilities["browserName"] != "firefox"
        || session["sessionId"]
            .as_str()
            .is_none_or(|id| id.is_empty() || id.len() > 256)
    {
        return Err(bad());
    }
    let version = capabilities["browserVersion"]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 64)
        .ok_or_else(bad)?
        .to_owned();
    let subscribed = channel.call(Command::SubscribeCore, deadline).await?.value;
    if subscribed["subscription"]
        .as_str()
        .is_none_or(|id| id.is_empty() || id.len() > 256)
    {
        return Err(bad());
    }
    let tree = channel.call(Command::Tree, deadline).await?;
    let mut contexts = Contexts::snapshot(channel.scope(), &tree.value).ok_or_else(bad)?;
    // Receive order is stamped by the same reader that correlates the tree
    // response. The queue cannot drop events or overflow silently.
    while events.delivered() < tree.event_boundary {
        let event = tokio::time::timeout_at(deadline.into(), events.recv())
            .await
            .map_err(|_| bad())??;
        if !contexts.apply(&event) {
            return Err(bad());
        }
    }
    if !channel.connected() {
        return Err(bad());
    }
    Ok(Boot {
        product: "firefox".into(),
        version,
        contexts,
        prompts_ignored: capabilities["unhandledPromptBehavior"]["default"] == "ignore",
    })
}
