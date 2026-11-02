//! Exact integer conversion for JSON protocol values.
use serde_json::Value;

pub(crate) fn unsigned(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        let number = value.as_f64()?;
        // The upper bound is exclusive: u64::MAX rounds to 2^64 as f64.
        ((0.0..18_446_744_073_709_551_616.0).contains(&number) && number.fract() == 0.0)
            .then_some(number as u64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integers_retain_precision_and_out_of_range_values_are_rejected() {
        for value in [0, 9_007_199_254_740_993, u64::MAX] {
            assert_eq!(unsigned(&json!(value)), Some(value));
        }
        assert_eq!(unsigned(&json!(1.0)), Some(1));
        assert_eq!(unsigned(&json!(-0.0)), Some(0));
        for value in [
            json!(-1),
            json!(1.5),
            json!(1e100),
            json!(u64::MAX as f64),
            json!(true),
            Value::Null,
        ] {
            assert_eq!(unsigned(&value), None);
        }
    }
}
