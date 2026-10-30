//! Native release index validation and staged rollout decisions.
//! Schema 2 intentionally cannot be confused with the older Node archives.
use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
}
pub struct Channel {
    pub version: String,
    pub min_from: Option<String>,
    pub percent: f64,
    pub platforms: BTreeMap<String, Artifact>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Upgrade(Artifact),
    None(String),
    Blocked(String),
}

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
        _ => a_pre.cmp(&b_pre),
    })
}

pub fn cohort(install_id: &str) -> u16 {
    let digest = Sha256::digest(install_id.as_bytes());
    u16::from_be_bytes([digest[0], digest[1]]) % 100
}

impl Channel {
    pub fn parse(value: &Value) -> Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| Error::message("The native release index must be an object"))?;
        if object.get("schema").and_then(Value::as_f64) != Some(2.0) {
            return Err(Error::message(
                "Native updates require release index schema 2",
            ));
        }
        let version = object
            .get("version")
            .and_then(Value::as_str)
            .filter(|version| valid_version(version))
            .ok_or_else(|| Error::message("The release index has an invalid version"))?
            .to_owned();
        let min_from = match object.get("min_from") {
            None | Some(Value::Null) => None,
            Some(Value::String(version)) if valid_version(version) => Some(version.clone()),
            _ => return Err(Error::message("The release index has an invalid min_from")),
        };
        let percent = match object.get("rollout") {
            None | Some(Value::Null) => 100.0,
            Some(value) => value
                .get("percent")
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
                .ok_or_else(|| Error::message("rollout.percent must be between 0 and 100"))?,
        };
        let platforms = object
            .get("platforms")
            .and_then(Value::as_object)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::message("The release index has no platforms"))?;
        if platforms.len() > 32 {
            return Err(Error::message("Too many native release platforms"));
        }
        let mut result = BTreeMap::new();
        for (platform, value) in platforms {
            if platform.is_empty()
                || platform.len() > 64
                || !platform
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                return Err(Error::message("Invalid native release platform"));
            }
            if value.get("format").and_then(Value::as_str) != Some("native-binary") {
                return Err(Error::message(
                    "Native release artifacts must use native-binary format",
                ));
            }
            let url = value
                .get("url")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 4096)
                .ok_or_else(|| Error::message("Native release artifact has an invalid URL"))?
                .to_owned();
            let sha256 = value
                .get("sha256")
                .and_then(Value::as_str)
                .filter(|value| {
                    value.len() == 64
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
                .ok_or_else(|| Error::message("Native release artifact has an invalid SHA-256"))?
                .to_owned();
            let bytes = value
                .get("bytes")
                .and_then(Value::as_f64)
                .filter(|value| {
                    value.is_finite()
                        && value.fract() == 0.0
                        && (1.0..=MAX_BINARY_BYTES as f64).contains(value)
                })
                .ok_or_else(|| Error::message("Native release artifact has an invalid size"))?
                as u64;
            result.insert(platform.clone(), Artifact { url, sha256, bytes });
        }
        Ok(Self {
            version,
            min_from,
            percent,
            platforms: result,
        })
    }

    pub fn decide(
        &self,
        current: &str,
        platform: &str,
        install_id: &str,
        channel: &str,
        force: bool,
    ) -> Result<Decision> {
        let Some(artifact) = self.platforms.get(platform) else {
            return Ok(Decision::Blocked(format!(
                "This release has no build for {platform}"
            )));
        };
        if compare_versions(&self.version, current)? != std::cmp::Ordering::Greater {
            return Ok(Decision::None(format!(
                "Already running {current}; no update needed"
            )));
        }
        if !force
            && self.min_from.as_ref().is_some_and(|min| {
                compare_versions(current, min).is_ok_and(|order| order == std::cmp::Ordering::Less)
            })
        {
            return Ok(Decision::Blocked(format!(
                "Upgrade to {} first",
                self.min_from.as_ref().unwrap()
            )));
        }
        let bucket = cohort(install_id);
        if channel != "canary" && !force && f64::from(bucket) >= self.percent {
            return Ok(Decision::None(format!(
                "This device is outside the staged rollout ({bucket} >= {}%)",
                self.percent
            )));
        }
        Ok(Decision::Upgrade(artifact.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn channel() -> Channel {
        Channel::parse(&json!({"schema":2,"version":"0.7.3","min_from":"0.7.0",
            "rollout":{"percent":50},"platforms":{"linux-x64-gnu":{"format":"native-binary",
                "url":"rdh","sha256":"a".repeat(64),"bytes":1024}}}))
        .unwrap()
    }
    #[test]
    fn version_rollout_and_force_preserve_the_existing_policy() {
        assert_eq!(
            compare_versions("0.7.2-rust.1", "0.7.2").unwrap(),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_versions("0.8.0", "0.7.99").unwrap(),
            std::cmp::Ordering::Greater
        );
        assert_eq!(cohort("same-install"), cohort("same-install"));
        assert!(matches!(
            channel()
                .decide("0.6.0", "linux-x64-gnu", "id", "stable", false)
                .unwrap(),
            Decision::Blocked(_)
        ));
        assert!(matches!(
            channel()
                .decide("0.6.0", "linux-x64-gnu", "id", "stable", true)
                .unwrap(),
            Decision::Upgrade(_)
        ));
        assert!(matches!(
            channel()
                .decide("0.8.0", "linux-x64-gnu", "id", "stable", true)
                .unwrap(),
            Decision::None(_)
        ));
        assert!(matches!(
            channel()
                .decide("0.7.2", "missing", "id", "canary", false)
                .unwrap(),
            Decision::Blocked(_)
        ));
    }
    #[test]
    fn old_archives_invalid_versions_and_unbounded_artifacts_are_rejected() {
        assert!(
            !valid_version("../1.2.3")
                && !valid_version("1.2.3-..")
                && valid_version("0.7.2-rust.1")
        );
        let old = json!({"schema":1,"version":"1.2.3","platforms":{"linux-x64-gnu":{"url":"node.tar.gz","sha256":"a".repeat(64)}}});
        assert!(Channel::parse(&old).is_err());
        let mut native = json!({"schema":2,"version":"1.2.3","platforms":{"linux-x64-gnu":{"format":"native-binary","url":"rdh","sha256":"a".repeat(64),"bytes":1}}});
        native["platforms"]["linux-x64-gnu"]["bytes"] = json!(MAX_BINARY_BYTES + 1);
        assert!(Channel::parse(&native).is_err());
    }
}
