//! Validate retained JSON without allocating a Value tree for every record.
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

struct Checked {
    object: bool,
}

impl<'de> Deserialize<'de> for Checked {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // deserialize_ignored_any would also skip UTF-8, surrogate and numeric
        // range checks. Use the ordinary value decoder to preserve validation.
        deserializer.deserialize_any(Self { object: false })
    }
}

impl<'de> Visitor<'de> for Checked {
    type Value = Self;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_str<E>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<Self, A::Error> {
        while values.next_element::<Self>()?.is_some() {}
        Ok(self)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut values: A) -> Result<Self, A::Error> {
        while values.next_entry::<Self, Self>()?.is_some() {}
        Ok(Self { object: true })
    }
}

pub(super) fn valid_object(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Checked>(bytes).is_ok_and(|value| value.object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn validation_matches_value_for_objects_and_malformed_history() {
        let cases: &[&[u8]] = &[
            br#"{"id":"abc","nested":[null,true,false,1,-2,0.25,{},[]]}"#,
            " {\"世界\":\"😀\",\"escape\":\"\\uD83D\\uDE00\"} \n".as_bytes(),
            br#"{"duplicate":1,"duplicate":2,"escaped\u006bey":"x"}"#,
            br#"{"bad":"\uD800"}"#,
            br#"{"bad":"\uDC00"}"#,
            b"{\"bad\":\"\xff\"}",
            b"{\"\xff\":true}",
            b"{\"bad\":\"\x01\"}",
            br#"{"huge":1e9999}"#,
            br#"{"bad":01}"#,
            br#"{"bad":1e}"#,
            br#"{"bad":NaN}"#,
            br#"{"bad":Infinity}"#,
            br#"{"trailing":1,}"#,
            br#"{"missing":}"#,
            br#"{"extra":1} false"#,
            br#"{"unfinished":"#,
            b"[]",
            b"null",
            b"false",
            b"1",
            b"\"string\"",
            b"",
        ];
        for bytes in cases {
            let expected =
                serde_json::from_slice::<Value>(bytes).is_ok_and(|value| value.is_object());
            assert_eq!(valid_object(bytes), expected, "{bytes:?}");
        }
        for depth in [1, 120, 126, 127, 128, 256] {
            let bytes = format!("{{\"nested\":{}0{}}}", "[".repeat(depth), "]".repeat(depth));
            let expected =
                serde_json::from_str::<Value>(&bytes).is_ok_and(|value| value.is_object());
            assert_eq!(valid_object(bytes.as_bytes()), expected, "depth {depth}");
        }
    }
}
