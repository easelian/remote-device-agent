//! Read-only interpretation of one captured POST submit against its original record.
use super::*;
use sem::task_events::{Method, RequestEndpoint, Sink, SubmitResponse};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Read {
    Outcome,
    Feedback,
    Fields,
}

enum Dispatch {
    NativeInvalid,
    Submitted,
    Unconfirmed,
}

type PixelSlot = sem::program::PixelSlot;
type Crops = Vec<(PixelSlot, pixels::Crop)>;

// Bracket the screenshot with document-bound geometry reads. A moving slot,
// changed source or stale frame invalidates the optional diagnostic.
async fn capture_crops(
    page: &sem::Page<native::Port>,
    target: sem::evidence::Target,
    anchor: &str,
    deadline: Instant,
) -> Option<Crops> {
    let slots = page
        .submit_pixel_slots(target, anchor, deadline)
        .await
        .ok()?;
    if slots.is_empty() {
        return None;
    }
    let image = page.access(false).ok()?.screenshot(deadline).await.ok()?;
    let after = page
        .submit_pixel_slots(target, anchor, deadline)
        .await
        .ok()?;
    if after != slots {
        return None;
    }
    let frame = pixels::Frame::decode(&image)?;
    slots
        .into_iter()
        .map(|slot| {
            if frame.size() != (slot.viewport_width, slot.viewport_height) {
                return None;
            }
            Some((slot, frame.crop(slot.x, slot.y, slot.width, slot.height)?))
        })
        .collect()
}

fn corresponding_slot(before: &PixelSlot, after: &PixelSlot) -> bool {
    before.field_node == after.field_node
        && before.source_node == after.source_node
        && (before.kind == after.kind || before.kind == sem::program::PixelKind::Reserved)
        && after.kind != sem::program::PixelKind::Reserved
        && after.changed
        && before.width == after.width
        && before.height == after.height
        && before.field_x == after.field_x
        && before.field_y == after.field_y
        && before.viewport_width == after.viewport_width
        && before.viewport_height == after.viewport_height
        && before.scroll_x == after.scroll_x
        && before.scroll_y == after.scroll_y
}

pub(super) struct Attempt {
    record: RecordCondition,
    target: sem::evidence::Target,
    endpoint: RequestEndpoint,
    dispatch: Dispatch,
    before: Option<Crops>,
}

impl Attempt {
    pub(super) async fn prepare(
        session: &mut Session,
        page: &sem::Page<native::Port>,
        tab: &str,
        epoch: u64,
        params: &Value,
        deadline: Instant,
    ) -> Result<Option<(Self, sem::SubmitRecordBinding)>> {
        let Some(record) = params["pre_action_diagnostics"].get("rejection_record") else {
            return Ok(None);
        };
        let record = RecordCondition::parse_submit(record)?;
        let target = session.evidence.lookup(&record.node_id, tab, epoch)?;
        let reference = session.reference(
            params["action"]["ref"]
                .as_str()
                .ok_or_else(|| Failure::new("browser_invalid_params"))?,
            tab,
        )?;
        let binding = page
            .prepare_submit_record(
                reference,
                target,
                &record.anchor_text,
                record.expected_text.as_deref(),
                deadline,
            )
            .await?;
        let before = if params["pre_action_diagnostics"]["capture_field_pixels"] == true {
            // Leave time for the authorized input if optional image capture is slow.
            let soft = deadline
                .checked_sub(Duration::from_millis(400))
                .unwrap_or(deadline)
                .min(Instant::now() + Duration::from_millis(650));
            capture_crops(page, target, &record.anchor_text, soft).await
        } else {
            None
        };
        Ok(Some((
            Self {
                before,
                record,
                target,
                endpoint: binding.endpoint.clone(),
                dispatch: Dispatch::Unconfirmed,
            },
            binding,
        )))
    }

    pub(super) async fn acknowledge(
        &mut self,
        page: &sem::Page<native::Port>,
        binding: &sem::SubmitRecordBinding,
        receipt: &Value,
        deadline: Instant,
    ) {
        // The input already ran. A lost read can only remove diagnostic evidence.
        let invalid = receipt.get("native_validation").is_some();
        self.dispatch = match (page.submit_observed(binding, deadline).await, invalid) {
            (Ok(false), true) => Dispatch::NativeInvalid,
            (Ok(true), false) => Dispatch::Submitted,
            _ => Dispatch::Unconfirmed,
        };
    }

    pub(super) fn supersede(&mut self) {
        self.dispatch = Dispatch::Unconfirmed;
        self.before = None;
    }

    pub(super) async fn read(
        &self,
        sink: &Sink,
        page: &sem::Page<native::Port>,
        registry: &mut evidence::Registry,
        tab: &str,
        epoch: u64,
        mode: Read,
        query_include_indicators: bool,
        include_pixels: bool,
        deadline: Instant,
    ) -> Result<Value> {
        let unknown = || json!({"state":"undetermined"});
        let response = || sink.submit_response(&self.endpoint, Method::Post);
        match self.dispatch {
            Dispatch::NativeInvalid => {
                return Ok(match response()? {
                    SubmitResponse::NotObserved => json!({"state":"native_invalid"}),
                    _ => unknown(),
                });
            }
            Dispatch::Unconfirmed => return Ok(unknown()),
            Dispatch::Submitted => (),
        }
        let (request_id, status) = match response()? {
            SubmitResponse::NotObserved | SubmitResponse::Pending => {
                return Ok(json!({"state":"pending"}));
            }
            SubmitResponse::Response {
                request_id,
                status: status @ (200 | 422),
            } => (request_id, status),
            _ => return Ok(unknown()),
        };
        let mut result = if status == 422 {
            json!({"state":"http_422","http_status":status,"request_id":request_id})
        } else {
            unknown()
        };
        let expected = self
            .record
            .expected_text
            .as_deref()
            .filter(|_| status == 200);
        if !sink.active() || (mode == Read::Outcome && expected.is_none()) {
            return Ok(result);
        }

        // A text wait may discard public DOM handles. The capture still owns
        // the original device-side target in this tab, document and epoch.
        let target = if mode != Read::Outcome {
            self.target
        } else {
            registry.lookup(&self.record.node_id, tab, epoch)?
        };
        let observed = if mode == Read::Feedback {
            page.read_submit_feedback(target, &self.record.anchor_text, deadline)
                .await?
        } else {
            None
        };
        let (fields, visuals, indicators) = if mode == Read::Fields {
            page.read_submit_fields(
                target,
                &self.record.anchor_text,
                query_include_indicators,
                deadline,
            )
            .await?
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        let mut pixel_rows = Vec::new();
        if include_pixels
            && !fields.is_empty()
            && let Some(before) = &self.before
            && let Some(after) =
                capture_crops(page, target, &self.record.anchor_text, deadline).await
        {
            // Text may change during the image read. Require the same DOM
            // pair, reason and association on both sides of that read.
            if let Ok((confirmed, _, _)) = page
                .read_submit_fields(target, &self.record.anchor_text, true, deadline)
                .await
            {
                for (slot, crop) in &after {
                    let Some(field) = fields.iter().find(|f| f.field.node == slot.field_node)
                    else {
                        continue;
                    };
                    if (status != 422 && !field.association.is_strong())
                        || !confirmed.iter().any(|f| {
                            f.field == field.field
                                && f.message == field.message
                                && f.association == field.association
                                && f.reason == field.reason
                        })
                    {
                        continue;
                    }
                    if before.iter().any(|(first, old)| {
                        corresponding_slot(first, slot) && old.changed_icon(crop)
                    }) {
                        pixel_rows.push(*slot);
                    }
                }
            }
        }
        let explicit_rejection = fields.iter().any(|item| item.association.is_strong())
            || observed.as_ref().is_some_and(|item| {
            matches!(
                item.kind,
                sem::SubmitFeedbackKind::FieldError | sem::SubmitFeedbackKind::RecordRejection
            )
        });
        if status == 200 {
            let mut rejected = explicit_rejection;
            if !rejected
                && fields.is_empty()
                && visuals.is_empty()
                && indicators.is_empty()
                && let Some(expected) = expected
            {
                let query = sem::evidence::Query::MatchRecord {
                    node_id: self.record.node_id.clone(),
                    anchor_text: self.record.anchor_text.clone(),
                    expected_text: expected.to_owned(),
                };
                let observed = page.evidence(&query, Some(target), deadline).await?;
                let [(_, sem::evidence::Raw::MatchRecord { matched, coverage })] =
                    observed.records.as_slice()
                else {
                    return Err(sem::types::decode());
                };
                if coverage.truncated {
                    return Ok(unknown());
                }
                rejected = *matched;
                if !rejected {
                    result = json!({"state":"pending"});
                }
            }
            if rejected
                || observed.is_some()
                || !fields.is_empty()
                || !visuals.is_empty()
                || !indicators.is_empty()
            {
                result = json!({"state": if rejected {"business_rejected"} else {"page_feedback"},
                    "http_status":200,"request_id":request_id,"record_node_id":self.record.node_id});
            }
        }
        // Network events continue arriving during the DOM read. Only publish
        // field identities while the same unique response still owns the evidence.
        if !matches!(response()?, SubmitResponse::Response { request_id:id, status:code } if id == request_id && code == status)
        {
            return Ok(unknown());
        }
        if !sink.active() {
            return Ok(if status == 422 { result } else { unknown() });
        }
        if result.get("record_node_id").is_some()
            && (mode == Read::Fields
                || (mode == Read::Feedback
                    && registry.lookup(&self.record.node_id, tab, epoch).is_err()))
        {
            result["record_node_id"] = registry.feedback_id(target, tab, epoch)?.into();
        }
        if !fields.is_empty() {
            let mut rows = Vec::with_capacity(fields.len());
            for item in fields {
                rows.push(json!({
                    "field_node_id": registry.feedback_id(item.field, tab, epoch)?,
                    "message_node_id": registry.feedback_id(item.message, tab, epoch)?,
                    "association": item.association,
                    "reason": item.reason
                }));
            }
            result["fields"] = rows.into();
        }
        if query_include_indicators && (!indicators.is_empty() || !pixel_rows.is_empty()) {
            if indicators.len() + pixel_rows.len() > 32 {
                pixel_rows.clear();
            }
            let mut rows = Vec::with_capacity(indicators.len());
            for item in indicators {
                rows.push(
                    json!({"field_node_id":registry.feedback_id(item.field,tab,epoch)?,
                    "source_node_id":registry.feedback_id(item.source,tab,epoch)?,
                    "kind":item.kind}),
                );
            }
            for slot in pixel_rows {
                let field = sem::evidence::Target {
                    node: slot.field_node,
                    ..target
                };
                let source = sem::evidence::Target {
                    node: slot.source_node,
                    ..target
                };
                rows.push(
                    json!({"field_node_id":registry.feedback_id(field,tab,epoch)?,
                    "source_node_id":registry.feedback_id(source,tab,epoch)?, "kind":slot.kind}),
                );
            }
            result["indicators"] = rows.into();
        }
        if !visuals.is_empty() {
            let mut rows = Vec::with_capacity(visuals.len());
            for item in visuals {
                rows.push(
                    json!({"field_node_id":registry.feedback_id(item.field,tab,epoch)?,
                    "changes":item.changes}),
                );
            }
            result["visual_changes"] = rows.into();
        }
        if let Some(item) = observed
            && (result["state"] != "business_rejected" || explicit_rejection)
        {
            let mut info = json!({"kind":item.kind, "reason":item.reason,
                "message_node_id":registry.feedback_id(item.message, tab, epoch)?});
            if let Some(field) = item.field {
                info["field_node_id"] = registry.feedback_id(field, tab, epoch)?.into();
            }
            result["feedback"] = info;
        }
        Ok(result)
    }
}
