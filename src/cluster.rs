//! Wire v1 connection fencing. The trusted discovery service selects targets;
//! the Agent keeps the highest accepted generation for its process lifetime.
use crate::{
    config::validate_gateway,
    error::{Error, Result},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub device_id: String,
    pub node_id: String,
    pub boot_id: String,
    pub connection_id: String,
    pub generation: i64,
    pub instance_id: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub version: u8,
    pub assignment_id: String,
    pub node_id: String,
    pub boot_id: String,
    pub agent_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_ip: Option<std::net::IpAddr>,
    pub expires_at: f64,
    // Admission credentials are transient and must never be serialized or logged.
    #[serde(skip_serializing)]
    pub access_ticket: String,
}

#[derive(Serialize)]
pub struct AssignmentRequest<'a> {
    pub device_id: &'a str,
    pub instance_id: &'a str,
    pub attempt_id: &'a str,
    pub failed_assignment_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_connection: Option<&'a Identity>,
}

pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

impl Assignment {
    pub fn target(&self, discovery: &str) -> Result<String> {
        let invalid = || Error::message("Invalid cluster assignment");
        if self.version != 2
            || self.access_ticket.len() != 43
            || !self
                .access_ticket
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || ![&self.assignment_id, &self.node_id, &self.boot_id]
                .into_iter()
                .all(|s| identifier(s))
            || !self.expires_at.is_finite()
            || self.expires_at <= 0.0
        {
            return Err(invalid());
        }
        let mut url = url::Url::parse(&self.agent_url).map_err(|_| invalid())?;
        if self
            .connect_ip
            .is_some_and(|ip| url.scheme() != "wss" || ip.is_unspecified() || ip.is_multicast())
        {
            return Err(invalid());
        }
        let scheme = match url.scheme() {
            "wss" => "https",
            "ws" if discovery.starts_with("http://") => "http",
            _ => return Err(invalid()),
        };
        url.set_scheme(scheme).map_err(|_| invalid())?;
        validate_gateway(url.as_str()).map_err(|_| invalid())
    }
    pub fn matches(&self, identity: &Identity, device: &str, instance: &str) -> bool {
        identity.device_id == device
            && identity.instance_id == instance
            && identity.node_id == self.node_id
            && identity.boot_id == self.boot_id
            && identity.generation > 0
            && identifier(&identity.connection_id)
    }
}

#[derive(Default)]
pub(crate) struct Fence {
    pub highest: Option<Identity>,
    pub current: Option<(u64, Identity)>,
}
impl Fence {
    pub fn accepts(&self, identity: &Identity) -> bool {
        identity.generation > 0
            && self
                .highest
                .as_ref()
                .is_none_or(|old| identity.generation > old.generation || identity == old)
    }
    pub fn can_start(&self, identity: Option<&Identity>) -> bool {
        match identity {
            Some(identity) => self
                .current
                .as_ref()
                .is_some_and(|(_, current)| current == identity),
            None => self.highest.is_none(),
        }
    }
}

#[cfg(test)]
mod target_tests {
    use super::*;

    #[test]
    fn assignments_require_v2_and_a_bounded_url_safe_ticket() {
        let base = serde_json::json!({"version":2,"access_ticket":"abcdefghijklmnopqrstuvwxyz0123456789_-ABCDE","assignment_id":"a","node_id":"n","boot_id":"b","agent_url":"wss://core.example/","expires_at":1.0});
        let mut missing = base.clone();
        missing.as_object_mut().unwrap().remove("access_ticket");
        assert!(serde_json::from_value::<Assignment>(missing).is_err());
        for ticket in [
            "".to_owned(),
            "x".repeat(42),
            "x".repeat(44),
            format!("{}=", "x".repeat(42)),
            format!("{}\n", "x".repeat(42)),
            "é".repeat(43),
        ] {
            let mut value = base.clone();
            value["access_ticket"] = ticket.into();
            assert!(
                serde_json::from_value::<Assignment>(value)
                    .unwrap()
                    .target("https://gate.example/")
                    .is_err()
            );
        }
        for version in [0, 1, 3] {
            let mut value = base.clone();
            value["version"] = version.into();
            assert!(
                serde_json::from_value::<Assignment>(value)
                    .unwrap()
                    .target("https://gate.example/")
                    .is_err()
            );
        }
    }

    #[test]
    fn direct_ip_cannot_downgrade_or_replace_the_authenticated_hostname() {
        let base = serde_json::json!({"version":2,"access_ticket":"abcdefghijklmnopqrstuvwxyz0123456789_-ABCDE","assignment_id":"a","node_id":"n","boot_id":"b","agent_url":"wss://core.example:8443/","expires_at":1.0});
        let old: Assignment = serde_json::from_value(base.clone()).unwrap();
        assert!(old.connect_ip.is_none());
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("access_ticket")
                .is_none()
        );
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("connect_ip")
                .is_none()
        );
        for ip in ["192.0.2.10", "2001:db8::10"] {
            let mut value = base.clone();
            value["connect_ip"] = ip.into();
            let assignment: Assignment = serde_json::from_value(value).unwrap();
            assert_eq!(
                assignment.target("https://gate.example/").unwrap(),
                "https://core.example:8443"
            );
        }
        for (url, ip) in [
            ("ws://127.0.0.1/", "192.0.2.1"),
            ("wss://core.example/", "0.0.0.0"),
            ("wss://core.example/", "ff02::1"),
        ] {
            let mut value = base.clone();
            value["agent_url"] = url.into();
            value["connect_ip"] = ip.into();
            assert!(
                serde_json::from_value::<Assignment>(value)
                    .unwrap()
                    .target("http://127.0.0.1/")
                    .is_err()
            );
        }
    }
}
