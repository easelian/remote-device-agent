//! Read-only interpretation of one captured POST submit against its original record.
use super::*;
use sem::task_events::{Method, RequestEndpoint, Sink, SubmitResponse};

enum Dispatch {
    NativeInvalid,
    Submitted,
    Unconfirmed,
}

pub(super) struct Attempt {
    record: RecordCondition,
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
        match response()? {
            SubmitResponse::NotObserved | SubmitResponse::Pending => Ok(json!({"state":"pending"})),
            SubmitResponse::Response {
                request_id,
                status: 422,
            } => Ok(json!({"state":"http_422","http_status":422,"request_id":request_id})),
            SubmitResponse::Response {
                request_id,
                status: 200,
            } if sink.active() => {
                let Some(expected) = &self.record.expected_text else {
                    return Ok(unknown());
                };
                let target = registry.lookup(&self.record.node_id, tab, epoch)?;
                let query = sem::evidence::Query::MatchRecord {
                    node_id: self.record.node_id.clone(),
                    anchor_text: self.record.anchor_text.clone(),
                    expected_text: expected.clone(),
                };
                let observed = page.evidence(&query, Some(target), deadline).await?;
                let [(_, sem::evidence::Raw::MatchRecord { matched, coverage })] =
                    observed.records.as_slice()
                else {
                    return Err(sem::types::decode());
                };
                if !sink.active()
                    || !matches!(response()?, SubmitResponse::Response { request_id:id, status:200 } if id == request_id)
                    || coverage.truncated
                {
                    return Ok(unknown());
                }
                Ok(if *matched {
                    json!({"state":"business_rejected","http_status":200,"request_id":request_id,"record_node_id":self.record.node_id})
                } else {
                    json!({"state":"pending"})
                })
            }
            _ => Ok(unknown()),
        }
    }
}
