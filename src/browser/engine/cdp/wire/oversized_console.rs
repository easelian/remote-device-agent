//! Chromium's private pipe serializes these notifications as method, params,
//! sessionId. Above the normal message limit we only recognize this envelope.
//! We never decode the oversized parameters or use any fact from them.
const NORMAL_LIMIT: usize = super::MAX_MESSAGE;
const DISCARD_LIMIT: usize = 16 * 1024 * 1024;
const PREFIXES: [&[u8]; 3] = [
    br#"{"method":"Runtime.consoleAPICalled","params":{"#,
    br#"{"method":"Runtime.exceptionThrown","params":{"#,
    br#"{"method":"Runtime.inspectRequested","params":{"#,
];

pub(super) struct Discard {
    remaining: usize,
    closing: [u8; 128],
    depth: usize,
    quoted: bool,
    escaped: bool,
    suffix: Vec<u8>,
    matched: usize,
}

impl Discard {
    pub(super) fn begin(frame: &[u8], child: &str) -> Option<Self> {
        if frame.len() != NORMAL_LIMIT
            || child.is_empty()
            || child.len() > super::sessions::MAX_SESSION_ID_BYTES
            || !child
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return None;
        }
        let prefix = PREFIXES.iter().find(|p| frame.starts_with(p))?;
        let mut state = Self {
            remaining: DISCARD_LIMIT - prefix.len(),
            closing: [0; 128],
            depth: 1,
            quoted: false,
            escaped: false,
            suffix: [b",\"sessionId\":\"".as_slice(), child.as_bytes(), b"\"}"].concat(),
            matched: 0,
        };
        // The prefix has opened the params object. Only its matching closing
        // brace can start the final, exact session field at the top level.
        state.closing[0] = b'}';
        frame[prefix.len()..]
            .iter()
            .all(|&byte| state.push(byte))
            .then_some(state)
    }

    pub(super) fn push(&mut self, byte: u8) -> bool {
        if self.remaining == 0 || byte == 0 {
            return false;
        }
        self.remaining -= 1;
        if self.depth == 0 {
            if self.suffix.get(self.matched) != Some(&byte) {
                return false;
            }
            self.matched += 1;
        } else if self.quoted {
            if byte < 0x20 {
                return false;
            }
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.quoted = false;
            }
        } else {
            match byte {
                b'"' => self.quoted = true,
                b'{' | b'[' => {
                    if self.depth == self.closing.len() {
                        return false;
                    }
                    self.closing[self.depth] = if byte == b'{' { b'}' } else { b']' };
                    self.depth += 1;
                }
                b'}' | b']' => {
                    if self.closing[self.depth - 1] != byte {
                        return false;
                    }
                    self.depth -= 1;
                }
                _ => {}
            }
        }
        true
    }

    pub(super) fn owned(&self) -> bool {
        self.depth == 0 && self.matched == self.suffix.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(session: &str, padding: usize) -> Vec<u8> {
        serde_json::to_vec(&json!({"method":"Runtime.consoleAPICalled",
            "params":{"args":[{"value":"x".repeat(padding)},{"value":",\"sessionId\":\"mine\"}"}],
                      "nested":{"sessionId":"mine"}}, "sessionId":session}))
        .unwrap()
    }

    fn accepted(bytes: &[u8]) -> bool {
        let Some(mut frame) = Discard::begin(&bytes[..NORMAL_LIMIT], "mine") else {
            return false;
        };
        bytes[NORMAL_LIMIT..].iter().all(|&b| frame.push(b)) && frame.owned()
    }

    #[test]
    fn only_the_complete_private_envelope_can_be_discarded() {
        let bytes = message("mine", NORMAL_LIMIT);
        assert!(accepted(&bytes));
        assert!(!accepted(&bytes[..bytes.len() - 1]));
        assert!(!accepted(&message("foreign", NORMAL_LIMIT)));
        let mut extra = bytes.clone();
        extra.push(b'}');
        assert!(!accepted(&extra));
        let reply = serde_json::to_vec(
            &json!({"id":7,"result":{"description":"x".repeat(NORMAL_LIMIT)},"sessionId":"mine"}),
        )
        .unwrap();
        assert!(!accepted(&reply));
        assert!(Discard::begin(&bytes[..NORMAL_LIMIT], "bad\"session").is_none());
    }

    #[test]
    fn suffix_may_cross_the_normal_limit_but_total_discard_is_bounded() {
        let overhead = message("mine", 0).len();
        let bytes = message("mine", NORMAL_LIMIT + 3 - overhead);
        assert_eq!(bytes.len(), NORMAL_LIMIT + 3);
        assert!(accepted(&bytes));
        let big = message("mine", DISCARD_LIMIT - overhead);
        assert!(accepted(&big));
        drop(big);
        assert!(!accepted(&message("mine", DISCARD_LIMIT + 1 - overhead)));
    }
}
