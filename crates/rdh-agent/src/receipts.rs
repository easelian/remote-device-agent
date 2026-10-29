//! Bounded in-memory operation metadata; it survives reconnects, not restarts.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde_json::{Map, Value, json};
use sha2::Sha256;

use crate::error::{Error, Result};
use crate::random;

const TRACKED: &[&str] = &[
    "write_file",
    "edit_file",
    "make_directory",
    "move_file",
    "start_process",
    "write_process_stdin",
    "terminate_process",
    "start_search",
    "stop_search",
];

#[derive(Clone, Debug)]
pub struct ReceiptError {
    pub code: &'static str,
    pub message: &'static str,
    pub state: &'static str,
    pub summary: Value,
}

impl ReceiptError {
    fn new(code: &'static str, message: &'static str, state: &'static str) -> Self {
        Self {
            code,
            message,
            state,
            summary: json!({}),
        }
    }
}

struct Receipt {
    fingerprint: [u8; 32],
    state: &'static str,
    summary: Value,
    error_code: Option<String>,
    settled: bool,
    acknowledged: bool,
    expires: Instant,
    sequence: u64,
}

struct State {
    records: HashMap<String, Receipt>,
    sequence: u64,
}

// Contains the token-derived HMAC key; deliberately not Debug.
pub struct Receipts {
    pub instance_id: String,
    pub max_records: usize,
    pub ttl: Duration,
    key: Vec<u8>,
    state: Mutex<State>,
}

pub enum Begin {
    New,
    Existing(Value),
}

fn id(value: &Value) -> std::result::Result<&str, ReceiptError> {
    value
        .as_str()
        .filter(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or_else(|| {
            ReceiptError::new(
                "operation_unavailable",
                "Operation receipt is unavailable",
                "unknown",
            )
        })
}

fn canonical(value: &Value, mac: &mut Hmac<Sha256>) {
    fn length(mac: &mut Hmac<Sha256>, length: usize) {
        mac.update(&(length as u64).to_be_bytes());
    }
    match value {
        Value::Null => mac.update(b"n"),
        Value::Bool(value) => mac.update(if *value { b"t" } else { b"f" }),
        Value::Number(number) => {
            // Node parses JSON numbers as binary64. Normalize 1 and 1.0, -0,
            // and large integer representations before fingerprinting.
            let number = number.as_f64().unwrap_or(0.0);
            let bits = if number == 0.0 { 0 } else { number.to_bits() };
            mac.update(b"d");
            mac.update(&bits.to_be_bytes());
        }
        Value::String(value) => {
            mac.update(b"s");
            length(mac, value.len());
            mac.update(value.as_bytes());
        }
        Value::Array(values) => {
            mac.update(b"a");
            length(mac, values.len());
            for value in values {
                canonical(value, mac);
            }
        }
        Value::Object(values) => {
            mac.update(b"o");
            length(mac, values.len());
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
            for key in keys {
                length(mac, key.len());
                mac.update(key.as_bytes());
                canonical(&values[key], mac);
            }
        }
    }
}

fn fingerprint(key: &[u8], operation: &str, params: &Value) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    canonical(&Value::String(operation.to_owned()), &mut mac);
    canonical(params, &mut mac);
    mac.finalize().into_bytes().into()
}

fn bounded_id(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn integral(value: &Value, min: f64, max: f64) -> Option<f64> {
    value
        .as_f64()
        .filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= min && *n <= max)
}

pub fn result_summary(value: &Value) -> Value {
    let mut out = Map::new();
    let Some(source) = value.as_object() else {
        return Value::Object(out);
    };
    for key in ["job_id", "search_id"] {
        if let Some(value) = source
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| bounded_id(value, 128))
        {
            out.insert(key.into(), json!(value));
        }
    }
    if let Some(value) = source
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        out.insert("sha256".into(), json!(value));
    }
    for key in ["size_bytes", "sent"] {
        if let Some(value) = source
            .get(key)
            .and_then(|value| integral(value, 0.0, 9_007_199_254_740_991.0))
        {
            out.insert(key.into(), json!(value as u64));
        }
    }
    if let Some(value) = source.get("exit_code") {
        if value.is_null() {
            out.insert("exit_code".into(), Value::Null);
        } else if let Some(value) = integral(value, f64::from(i32::MIN), f64::from(i32::MAX)) {
            out.insert("exit_code".into(), json!(value as i32));
        }
    }
    if let Some(value) = source
        .get("status")
        .and_then(Value::as_str)
        .filter(|value| {
            [
                "running",
                "exited",
                "failed",
                "timed_out",
                "terminated",
                "queued",
                "completed",
                "stopped",
            ]
            .contains(value)
        })
    {
        out.insert("status".into(), json!(value));
    }
    if let Some(value) = source.get("stdin_closed").and_then(Value::as_bool) {
        out.insert("stdin_closed".into(), json!(value));
    }
    Value::Object(out)
}

impl Receipts {
    pub fn new(token: &str) -> Result<Self> {
        Self::with_limits(token, 1024, Duration::from_secs(3600))
    }

    pub fn with_limits(token: &str, max_records: usize, ttl: Duration) -> Result<Self> {
        if max_records == 0
            || max_records > 100_000
            || ttl.is_zero()
            || ttl > Duration::from_secs(86_400)
        {
            return Err(Error::message("Invalid operation receipt limits"));
        }
        Ok(Self {
            instance_id: random::hex_token::<16>()?,
            max_records,
            ttl,
            key: token.as_bytes().to_vec(),
            state: Mutex::new(State {
                records: HashMap::new(),
                sequence: 0,
            }),
        })
    }

    fn prune(state: &mut State) {
        let now = Instant::now();
        state
            .records
            .retain(|_, record| !record.settled || record.expires > now);
    }

    fn status_of(&self, operation_id: &str, record: Option<&Receipt>) -> Value {
        let mut result = json!({"operation_id": operation_id, "instance_id": self.instance_id,
            "execution_state": record.map_or("unknown", |record| record.state),
            "result_summary": record.map_or_else(|| json!({}), |record| record.summary.clone()) });
        if let Some(code) = record.and_then(|record| record.error_code.as_ref()) {
            result["error_code"] = json!(code);
        }
        result
    }

    pub fn begin(
        &self,
        operation_id: &Value,
        instance: &Value,
        operation: &str,
        params: &Value,
    ) -> std::result::Result<Begin, ReceiptError> {
        let operation_id = id(operation_id)?;
        if instance.as_str() != Some(self.instance_id.as_str()) || !TRACKED.contains(&operation) {
            return Err(ReceiptError::new(
                "operation_instance_changed",
                "Operation belongs to another Agent instance",
                "not_sent",
            ));
        }
        let digest = fingerprint(&self.key, operation, params);
        let mut state = self.state.lock().map_err(|_| {
            ReceiptError::new(
                "operation_unavailable",
                "Operation receipt is unavailable",
                "unknown",
            )
        })?;
        Self::prune(&mut state);
        if let Some(record) = state.records.get(operation_id) {
            if record.fingerprint != digest {
                return Err(ReceiptError::new(
                    "operation_conflict",
                    "Operation ID was used with different parameters",
                    "not_sent",
                ));
            }
            return Ok(Begin::Existing(self.status_of(operation_id, Some(record))));
        }
        if state.records.len() >= self.max_records {
            return Err(ReceiptError::new(
                "operation_capacity",
                "Operation receipt capacity is busy",
                "not_sent",
            ));
        }
        state.sequence += 1;
        let sequence = state.sequence;
        state.records.insert(
            operation_id.to_owned(),
            Receipt {
                fingerprint: digest,
                state: "running",
                summary: json!({}),
                error_code: None,
                settled: false,
                acknowledged: false,
                expires: Instant::now() + self.ttl,
                sequence,
            },
        );
        Ok(Begin::New)
    }

    pub fn complete(
        &self,
        operation_id: &str,
        outcome: &'static str,
        result: &Value,
        code: Option<&str>,
    ) {
        if let Ok(mut state) = self.state.lock()
            && let Some(record) = state
                .records
                .get_mut(operation_id)
                .filter(|record| !record.settled)
        {
            record.state = outcome;
            record.summary = result_summary(result);
            record.error_code = code.map(str::to_owned);
            record.settled = true;
            record.expires = Instant::now() + self.ttl;
        }
    }

    pub fn status(
        &self,
        operation_id: &Value,
        instance: &Value,
    ) -> std::result::Result<Value, ReceiptError> {
        let operation_id = id(operation_id)?;
        let mut state = self.state.lock().map_err(|_| {
            ReceiptError::new(
                "operation_unavailable",
                "Operation receipt is unavailable",
                "unknown",
            )
        })?;
        Self::prune(&mut state);
        let record = if instance.as_str() == Some(self.instance_id.as_str()) {
            state.records.get(operation_id)
        } else {
            None
        };
        Ok(self.status_of(operation_id, record))
    }

    pub fn pending(&self) -> Vec<String> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        Self::prune(&mut state);
        let mut pending: Vec<_> = state
            .records
            .iter()
            .filter(|(_, record)| record.settled && !record.acknowledged)
            .map(|(id, record)| (record.sequence, id.clone()))
            .collect();
        pending.sort_by_key(|item| item.0);
        pending.into_iter().map(|(_, id)| id).collect()
    }

    /// Stop only after accepted operations have settled and their results have
    /// been acknowledged. Contention or poisoned state is not evidence of idle.
    pub fn quiescent(&self) -> bool {
        self.state.try_lock().is_ok_and(|state| {
            state
                .records
                .values()
                .all(|record| record.settled && record.acknowledged)
        })
    }

    pub fn acknowledge(&self, operation_id: &Value, instance: &Value) {
        if instance.as_str() != Some(self.instance_id.as_str()) {
            return;
        }
        if let (Some(id), Ok(mut state)) = (operation_id.as_str(), self.state.lock())
            && let Some(record) = state.records.get_mut(id).filter(|record| record.settled)
        {
            record.acknowledged = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_conflicts_and_keeps_only_recovery_metadata() {
        let receipts = Receipts::with_limits("secret-token", 2, Duration::from_secs(1)).unwrap();
        let id = json!("a".repeat(32));
        let instance = json!(receipts.instance_id);
        assert!(matches!(
            receipts
                .begin(
                    &id,
                    &instance,
                    "write_file",
                    &json!({"content":"secret","n":1})
                )
                .unwrap(),
            Begin::New
        ));
        assert!(matches!(
            receipts
                .begin(
                    &id,
                    &instance,
                    "write_file",
                    &json!({"n":1.0,"content":"secret"})
                )
                .unwrap(),
            Begin::Existing(_)
        ));
        assert_eq!(
            receipts
                .begin(&id, &instance, "write_file", &json!({"content":"changed"}))
                .err()
                .unwrap()
                .code,
            "operation_conflict"
        );
        receipts.complete(
            id.as_str().unwrap(),
            "succeeded",
            &json!({"sha256":"f".repeat(64),"size_bytes":4,"path":"private","output":"secret"}),
            None,
        );
        receipts.complete(id.as_str().unwrap(), "unknown", &json!({}), None);
        let status = receipts.status(&id, &instance).unwrap();
        assert_eq!(status["execution_state"], "succeeded");
        assert_eq!(
            status["result_summary"],
            json!({"sha256":"f".repeat(64),"size_bytes":4})
        );
        assert!(!status.to_string().contains("private") && !status.to_string().contains("secret"));
        assert_eq!(receipts.pending(), vec![id.as_str().unwrap().to_owned()]);
        receipts.acknowledge(&id, &instance);
        assert!(receipts.pending().is_empty());
        assert!(matches!(
            receipts
                .begin(
                    &id,
                    &instance,
                    "write_file",
                    &json!({"n":1,"content":"secret"})
                )
                .unwrap(),
            Begin::Existing(_)
        ));
    }

    #[test]
    fn running_items_never_expire_and_other_instances_cannot_dispatch() {
        let receipts = Receipts::with_limits("token", 1, Duration::from_millis(1)).unwrap();
        let id = json!("a".repeat(32));
        let instance = json!(receipts.instance_id);
        assert!(matches!(
            receipts
                .begin(&id, &instance, "write_file", &json!({}))
                .unwrap(),
            Begin::New
        ));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            receipts
                .begin(&json!("b".repeat(32)), &instance, "write_file", &json!({}))
                .err()
                .unwrap()
                .code,
            "operation_capacity"
        );
        let restarted = Receipts::new("token").unwrap();
        assert_eq!(
            restarted.status(&id, &instance).unwrap()["execution_state"],
            "unknown"
        );
        assert_eq!(
            restarted
                .begin(&id, &instance, "write_file", &json!({}))
                .err()
                .unwrap()
                .code,
            "operation_instance_changed"
        );
        receipts.complete(id.as_str().unwrap(), "succeeded", &json!({}), None);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            receipts.status(&id, &instance).unwrap()["execution_state"],
            "unknown"
        );
    }

    #[test]
    fn simultaneous_duplicate_admission_has_only_one_new_receipt() {
        let receipts = std::sync::Arc::new(Receipts::new("fixture-token").unwrap());
        let gate = std::sync::Arc::new(std::sync::Barrier::new(8));
        let outcomes = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    let receipts = receipts.clone();
                    let gate = gate.clone();
                    scope.spawn(move || {
                        let id = json!("a".repeat(32));
                        let instance = json!(receipts.instance_id);
                        gate.wait();
                        match receipts.begin(&id, &instance, "write_file", &json!({"path":"x"})) {
                            Ok(Begin::New) => true,
                            Ok(Begin::Existing(status)) => {
                                assert_eq!(status["execution_state"], "running");
                                false
                            }
                            Err(error) => panic!("unexpected receipt error: {error:?}"),
                        }
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(outcomes.iter().filter(|outcome| **outcome).count(), 1);
        assert!(!receipts.quiescent());
        let id = json!("a".repeat(32));
        let instance = json!(receipts.instance_id);
        receipts.complete(id.as_str().unwrap(), "succeeded", &json!({}), None);
        assert_eq!(receipts.pending(), vec!["a".repeat(32)]);
        receipts.acknowledge(&id, &instance);
        assert!(receipts.quiescent());
    }
}
