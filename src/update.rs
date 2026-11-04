//! Version and download-size rules for standalone executable updates.
use crate::error::{Error, Result};

pub const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;

pub fn valid_version(value: &str) -> bool {
    if value.is_empty() || value.len() > 128 {
        return false;
    }
    let (core, pre) = value
        .split_once('-')
        .map_or((value, None), |(core, pre)| (core, Some(pre)));
    let numbers: Vec<_> = core.split('.').collect();
    if numbers.len() != 3
        || numbers.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || part.parse::<u64>().is_err()
        })
    {
        return false;
    }
    pre.is_none_or(|pre| {
        !pre.is_empty()
            && pre.split('.').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
    })
}

pub fn compare_versions(left: &str, right: &str) -> Result<std::cmp::Ordering> {
    if !valid_version(left) || !valid_version(right) {
        return Err(Error::message("Invalid native release version"));
    }
    let split = |value: &str| {
        let (core, pre) = value.split_once('-').unwrap_or((value, ""));
        (
            core.split('.')
                .map(|part| part.parse::<u64>().unwrap())
                .collect::<Vec<_>>(),
            pre.to_owned(),
        )
    };
    let (a, a_pre) = split(left);
    let (b, b_pre) = split(right);
    let core = a.cmp(&b);
    if core != std::cmp::Ordering::Equal {
        return Ok(core);
    }
    Ok(match (a_pre.is_empty(), b_pre.is_empty()) {
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => {
            let mut a = a_pre.split('.');
            let mut b = b_pre.split('.');
            loop {
                let order = match (a.next(), b.next()) {
                    (Some(left), Some(right)) => {
                        let numeric = |value: &str| value.bytes().all(|c| c.is_ascii_digit());
                        match (numeric(left), numeric(right)) {
                            (true, true) => {
                                let left = left.trim_start_matches('0');
                                let right = right.trim_start_matches('0');
                                left.len().cmp(&right.len()).then_with(|| left.cmp(right))
                            }
                            (true, false) => std::cmp::Ordering::Less,
                            (false, true) => std::cmp::Ordering::Greater,
                            _ => left.cmp(right),
                        }
                    }
                    (Some(_), None) => std::cmp::Ordering::Greater,
                    (None, Some(_)) => std::cmp::Ordering::Less,
                    (None, None) => break std::cmp::Ordering::Equal,
                };
                if order != std::cmp::Ordering::Equal {
                    break order;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_and_prerelease_order_prevent_downgrades() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        for (left, right, order) in [
            ("0.8.0", "0.7.99", Greater),
            ("0.8.0-rc.10", "0.8.0-rc.9", Greater),
            ("0.8.0-rc.1", "0.8.0", Less),
            ("0.8.0-1", "0.8.0-alpha", Less),
            ("0.8.0-rc", "0.8.0-rc.1", Less),
            ("0.8.0", "0.8.0", Equal),
        ] {
            assert_eq!(compare_versions(left, right).unwrap(), order);
        }
        for invalid in ["", "0.8", "../1.2.3", "1.2.3-", "1.2.3-a..b", "1.2.3+local"] {
            assert!(!valid_version(invalid));
            assert!(compare_versions(invalid, "0.8.0").is_err());
        }
    }
}
