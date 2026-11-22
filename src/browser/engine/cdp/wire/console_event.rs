//! Typed projection and ownership bounds for Runtime diagnostic events.
use crate::browser::semantic::task_events::{Fact, Level};
use serde_json::Value;
use std::collections::HashSet;

/// Project only the fixed, value-free Runtime facts understood by task capture.
pub(super) fn project(method: &str, params: &Value) -> Option<Fact> {
    let params = params.as_object()?;
    match method {
        "Runtime.consoleAPICalled" => {
            let level = match params.get("type")?.as_str()? {
                "log" => Level::Log,
                "debug" => Level::Debug,
                "info" => Level::Info,
                "warning" => Level::Warning,
                "error" | "assert" => Level::Error,
                _ => return None,
            };
            Some(Fact::Console(level))
        }
        "Runtime.exceptionThrown"
            if params.get("exceptionDetails").is_some_and(Value::is_object) =>
        {
            Some(Fact::Exception)
        }
        _ => None,
    }
}

/// Return a positive finite CDP event timestamp in milliseconds.
pub(super) fn timestamp_ms(params: &Value) -> Option<f64> {
    let value = params.get("timestamp")?.as_f64()?;
    (value.is_finite() && value > 0.0).then_some(value)
}

/// Find bounded native object leases carried by the selected Runtime events.
///
/// A capture of one action ends when a page logs more objects than it should
/// in so short a time. A journal runs for the whole session, where pages that
/// log freely are the ones worth reading, so `wide` allows many more.
pub(super) fn objects<'a>(method: &str, params: &'a Value, wide: bool) -> Result<Vec<&'a str>, ()> {
    let (most_objects, most_parts) = if wide { (64, 64) } else { (8, 16) };
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |value: &'a Value| -> Result<(), ()> {
        let object = value.as_object().ok_or(())?;
        let Some(id) = object.get("objectId") else {
            return Ok(());
        };
        let id = id.as_str().ok_or(())?;
        if id.is_empty() || id.len() > 256 {
            return Err(());
        }
        if seen.insert(id) {
            if result.len() >= most_objects {
                return Err(());
            }
            result.push(id);
        }
        Ok(())
    };
    match method {
        "Runtime.consoleAPICalled" => {
            let Some(args) = params.get("args") else {
                return Ok(result);
            };
            let args = args.as_array().ok_or(())?;
            if args.len() > most_parts {
                return Err(());
            }
            for value in args {
                add(value)?;
            }
        }
        "Runtime.exceptionThrown" => {
            let Some(details) = params.get("exceptionDetails") else {
                return Ok(result);
            };
            let details = details.as_object().ok_or(())?;
            if let Some(exception) = details.get("exception") {
                add(exception)?;
            }
        }
        "Runtime.inspectRequested" => {
            if let Some(object) = params.get("object") {
                add(object)?;
            }
        }
        _ => {}
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn objects<'a>(method: &str, params: &'a Value) -> Result<Vec<&'a str>, ()> {
        super::objects(method, params, false)
    }

    #[test]
    fn a_journal_accepts_what_a_page_that_logs_freely_sends() {
        let many =
            json!({"args":(0..40).map(|i| json!({"objectId":i.to_string()})).collect::<Vec<_>>()});
        assert_eq!(
            super::objects("Runtime.consoleAPICalled", &many, true)
                .unwrap()
                .len(),
            40
        );
        let more =
            json!({"args":(0..65).map(|i| json!({"objectId":i.to_string()})).collect::<Vec<_>>()});
        assert!(super::objects("Runtime.consoleAPICalled", &more, true).is_err());
    }

    #[test]
    fn projects_fixed_facts_without_payload() {
        assert!(matches!(
            project(
                "Runtime.consoleAPICalled",
                &json!({"type":"error","args":[{"value":"secret"}]})
            ),
            Some(Fact::Console(Level::Error))
        ));
        assert!(matches!(
            project(
                "Runtime.exceptionThrown",
                &json!({"exceptionDetails":{"text":"secret"}})
            ),
            Some(Fact::Exception)
        ));
        assert!(project("Runtime.consoleAPICalled", &json!({"type":"unknown"})).is_none());
        assert!(project("Runtime.consoleAPICalled", &json!({})).is_none());
        assert!(project("Runtime.exceptionThrown", &json!({})).is_none());
        let fact = format!(
            "{:?}",
            project(
                "Runtime.consoleAPICalled",
                &json!({"type":"log","args":[{"value":"SECRET"}]})
            )
        );
        assert!(!fact.contains("SECRET"));
    }

    #[test]
    fn timestamps_are_positive_finite() {
        assert_eq!(timestamp_ms(&json!({"timestamp":1.5})), Some(1.5));
        assert!(timestamp_ms(&json!({"timestamp":0})).is_none());
        assert!(timestamp_ms(&json!({"timestamp":-1})).is_none());
        assert!(timestamp_ms(&json!({"timestamp":"1"})).is_none());
    }

    #[test]
    fn object_leases_are_selected_and_deduplicated() {
        let value = json!({"args":[
            {"objectId":"a"}, {"objectId":"a"}, {"value":"safe"}, {"objectId":"b"}
        ]});
        assert_eq!(
            objects("Runtime.consoleAPICalled", &value).unwrap(),
            vec!["a", "b"]
        );
        assert_eq!(
            objects(
                "Runtime.inspectRequested",
                &json!({"object":{"objectId":"x"}})
            )
            .unwrap(),
            vec!["x"]
        );
        assert!(
            objects("Runtime.other", &json!({"objectId":"ignored"}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn object_bounds_and_invalid_values_are_rejected() {
        let many =
            json!({"args":(0..17).map(|i| json!({"objectId":i.to_string()})).collect::<Vec<_>>()});
        assert!(objects("Runtime.consoleAPICalled", &many).is_err());
        let long = json!({"args":[{"objectId":"x".repeat(257)}]});
        assert!(objects("Runtime.consoleAPICalled", &long).is_err());
        let malformed = json!({"args":[{"objectId":42}]});
        assert!(objects("Runtime.consoleAPICalled", &malformed).is_err());
        let unique =
            json!({"args":(0..9).map(|i| json!({"objectId":i.to_string()})).collect::<Vec<_>>()});
        assert!(objects("Runtime.consoleAPICalled", &unique).is_err());
        let too_many_args = json!({"args":(0..17).map(|_| json!({})).collect::<Vec<_>>()});
        assert!(objects("Runtime.consoleAPICalled", &too_many_args).is_err());
    }
}
