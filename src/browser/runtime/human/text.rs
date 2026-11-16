//! Text remains bound to the focused control in the user's image.
use super::*;
use crate::browser::engine::{FocusOutcome, Node};

pub(super) struct Focus {
    frame: page::Frame,
    node: Node,
}

pub(super) async fn capture(
    engine: &Engine,
    frames: &[page::Frame],
    deadline: Instant,
) -> Result<Option<Focus>> {
    for frame in frames {
        if let Some(node) = engine.focused_node(frame, deadline).await? {
            return Ok(Some(Focus {
                frame: frame.clone(),
                node,
            }));
        }
    }
    Ok(None)
}
impl Focus {
    pub(super) async fn insert(
        &self,
        engine: &Engine,
        text: &Value,
        deadline: Instant,
    ) -> Result<()> {
        if !page::frame_tree(engine, &self.frame.session, deadline)
            .await?
            .iter()
            .any(|frame| frame.id == self.frame.id && frame.document == self.frame.document)
        {
            return Err(Failure::new("browser_stale_frame"));
        }
        match engine
            .insert_at_focus(
                &self.frame,
                &self.node,
                text.as_str().unwrap_or(""),
                deadline,
            )
            .await?
        {
            FocusOutcome::Inserted => Ok(()),
            FocusOutcome::Stale => Err(Failure::new("browser_stale_frame")),
            FocusOutcome::Changed => Err(Failure::unknown("browser_not_actionable")),
        }
    }
}
#[cfg(test)]
#[path = "../../engine/cdp/text_tests.rs"]
mod protocol_tests;
