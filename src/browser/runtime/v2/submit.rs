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

pub(super) struct Attempt {
    record: RecordCondition,
    target: sem::evidence::Target,
    endpoint: RequestEndpoint,
    dispatch: Dispatch,
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
        Ok(Some((
            Self {
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
    }

    pub(super) async fn read(
        &self,
        sink: &Sink,
        page: &sem::Page<native::Port>,
        registry: &mut evidence::Registry,
        tab: &str,
        epoch: u64,
        mode: Read,
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
        let (fields, visuals) = if mode == Read::Fields {
            page.read_submit_fields(target, &self.record.anchor_text, deadline)
                .await?
        } else {
            (Vec::new(), Vec::new())
        };
        let explicit_rejection = fields
            .iter()
            .any(|item| item.association != sem::SubmitFieldAssociation::AdjacentText)
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
            if rejected || observed.is_some() || !fields.is_empty() || !visuals.is_empty() {
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
