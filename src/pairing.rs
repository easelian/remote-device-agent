use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;
use url::Url;

use crate::config::{DeviceConfig, same_hub, sha256_hex};
use crate::error::{Error, Result};
use crate::{network::Network, random, system};

struct Pending {
    verification_url: String,
    user_code: String,
    device_code: String,
    expires: Duration,
    interval: Duration,
}

fn pending(value: &Value, origin: &str) -> Result<Pending> {
    let invalid_url = || Error::message("The Hub returned an invalid pairing URL");
    let uri = value
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .ok_or_else(invalid_url)?;
    let user_code = value
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or_else(invalid_url)?;
    let verification = Url::parse(uri).map_err(|_| invalid_url())?;
    let query: Vec<_> = verification.query_pairs().collect();
    if !same_hub(&verification.origin().ascii_serialization(), origin)
        || !verification.username().is_empty()
        || verification.password().is_some()
        || verification.fragment().is_some()
        || verification.path() != "/auth/pair"
        || query.len() != 1
        || query[0].0 != "code"
        || query[0].1 != user_code
    {
        return Err(invalid_url());
    }
    let invalid_response = || Error::message("The Hub returned an invalid pairing response");
    let device_code = value
        .get("device_code")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
        .ok_or_else(invalid_response)?;
    let expires = value
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(invalid_response)?;
    let interval = value
        .get("interval")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(invalid_response)?;
    Ok(Pending {
        verification_url: verification.to_string(),
        user_code: user_code.to_owned(),
        device_code: device_code.to_owned(),
        expires: Duration::from_secs_f64(expires.min(600.0)),
        interval: Duration::from_secs_f64(interval.clamp(3.0, 10.0)),
    })
}

async fn interrupted(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    server: String,
    token: String,
    roots: BTreeMap<String, String>,
    response: Value,
}

fn read_saved(path: &Path) -> Result<Option<Saved>> {
    crate::binding::read_optional(&crate::binding::sidecar(path, ".pairing.json"))?
        .map(|raw| serde_json::from_slice(&raw).map_err(Error::from))
        .transpose()
}

pub(crate) async fn cancel_saved(network: &Network, path: &Path) -> Result<()> {
    let Some(saved) = read_saved(path)? else {
        return Ok(());
    };
    crate::config::validate_gateway(&saved.server)?;
    let request = pending(&saved.response, &saved.server)?;
    let device_id = saved.response["device_id"]
        .as_str()
        .ok_or_else(|| Error::message("Pending pairing has no device identity"))?;
    let cancelled = network
        .agent_request(
            &saved.server,
            "/api/pair/cancel",
            &saved.token,
            &json!({"device_code":request.device_code}),
        )
        .await?;
    if cancelled["state"] != "cancelled" {
        return Err(Error::message(
            "The Hub did not confirm pairing cancellation",
        ));
    }
    // An approved pairing can outlive its short-lived polling record. Its
    // reserved identity is persisted before the browser is opened.
    let result = network
        .agent_request(
            &saved.server,
            "/api/agent/unbind",
            &saved.token,
            &json!({"device_id":device_id}),
        )
        .await?;
    if result["state"] != "unbound" || result["device_id"] != device_id {
        return Err(Error::message(
            "The Hub did not confirm pending device revocation",
        ));
    }
    Ok(())
}

pub(crate) async fn pair(
    network: &Network,
    server: &str,
    roots: &BTreeMap<String, String>,
    path: &Path,
    running: &crate::binding::Running,
    mut shutdown: watch::Receiver<bool>,
) -> Result<DeviceConfig> {
    let mut browser_opened = false;
    loop {
        if *shutdown.borrow() {
            return Err(Error::message("Operation canceled"));
        }
        let saved = if let Some(saved) = read_saved(path)? {
            if !same_hub(&saved.server, server) || &saved.roots != roots || saved.token.len() < 32 {
                return Err(Error::message(
                    "Pending pairing does not match this Hub or the selected local roots",
                ));
            }
            saved
        } else {
            let token = random::url_token::<32>()?;
            let system = system::info();
            let proposal = json!({"name":system.hostname,"hostname":system.hostname,"os":system.os,
                "machine":system.machine,"roots":roots,"token_hash":sha256_hex(token.as_bytes())});
            let response = tokio::select! {
                result = network.post_json(server, "/api/pair/start", &proposal) => result?,
                _ = interrupted(&mut shutdown) => return Err(Error::message("Operation canceled")),
            };
            if response.status() != reqwest::StatusCode::CREATED {
                return Err(Error::message(format!(
                    "The Hub could not create a pairing request (HTTP {})",
                    response.status().as_u16()
                )));
            }
            let response = Network::response_json(response).await?;
            pending(&response, server)?;
            if !response["device_id"].as_str().is_some_and(|id| {
                !id.is_empty()
                    && id.len() <= 64
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            }) {
                return Err(Error::message(
                    "The Hub did not reserve a valid device identity",
                ));
            }
            let saved = Saved {
                server: server.into(),
                token,
                roots: roots.clone(),
                response,
            };
            crate::storage::write_private_new(
                &crate::binding::sidecar(path, ".pairing.json"),
                &serde_json::to_vec(&saved)?,
            )?;
            saved
        };
        let request = pending(&saved.response, server)?;
        if *shutdown.borrow() || crate::binding::pending_unbind(path)? {
            return Err(Error::message("Operation canceled"));
        }
        running.status(json!({"state":"waiting_for_binding","verification_url":request.verification_url,"user_code":request.user_code}))?;
        println!(
            "\nPairing code: {}\nOpen: {}\n",
            request.user_code, request.verification_url
        );
        println!(
            "Check the account, device and code in your browser, choose permissions, and approve binding."
        );
        if !browser_opened {
            system::open_browser(&request.verification_url);
            browser_opened = true;
        }
        let deadline = Instant::now() + request.expires;
        while Instant::now() < deadline {
            tokio::select! {
                _ = tokio::time::sleep(request.interval) => {},
                _ = interrupted(&mut shutdown) => return Err(Error::message("Operation canceled")),
            }
            let poll_body = json!({"device_code":request.device_code});
            let response = tokio::select! {
                result = network.post_json(server, "/api/pair/poll", &poll_body) => match result { Ok(value) => value, Err(_) => continue },
                _ = interrupted(&mut shutdown) => return Err(Error::message("Operation canceled")),
            };
            match response.status().as_u16() {
                200 => {
                    let body = Network::response_json(response).await?;
                    let mut config = body
                        .get("config")
                        .and_then(Value::as_object)
                        .cloned()
                        .ok_or_else(|| Error::message("Invalid device configuration"))?;
                    config.insert("token".into(), Value::String(saved.token.clone()));
                    #[cfg(windows)]
                    if config.get("shell").and_then(Value::as_str) == Some("/bin/sh") {
                        config.remove("shell");
                    }
                    let model = DeviceConfig::parse(Value::Object(config))?;
                    if !same_hub(&model.gateway_url, server)
                        || &model.roots != roots
                        || model.device_id != saved.response["device_id"]
                    {
                        return Err(Error::message(
                            "The pairing response does not match the reserved device, Hub or file roots",
                        ));
                    }
                    return Ok(model);
                }
                202 | 429 => continue,
                403 | 410 => break,
                status => {
                    return Err(Error::message(format!(
                        "Pairing request failed (HTTP {status})"
                    )));
                }
            }
        }
        cancel_saved(network, path).await?;
        crate::binding::remove_private(&crate::binding::sidecar(path, ".pairing.json"))?;
        println!(
            "The binding request expired or was denied. Creating a new code; the browser will not be opened again automatically."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(uri: &str) -> Value {
        json!({"verification_uri_complete":uri,"user_code":"ABCD","device_code":"id", "interval":3, "expires_in":600})
    }

    #[test]
    fn pairing_link_is_bound_to_the_selected_origin_and_code() {
        let uri = "https://example.com/auth/pair?code=ABCD";
        let accepted = pending(&response(uri), "https://example.com").unwrap();
        assert_eq!(accepted.verification_url, uri);
        for uri in [
            "https://evil.example/auth/pair?code=ABCD",
            "https://example.com/auth/pair?code=OTHER",
            "https://example.com/auth/pair?code=ABCD&x=1",
            "https://example.com/auth/pair?code=ABCD&code=ABCD",
            "https://example.com/auth/pair?code=ABCD#section",
            "https://user@example.com/auth/pair?code=ABCD",
            "https://user:secret@example.com/auth/pair?code=ABCD",
            "https://example.com/auth/pair/?code=ABCD",
            "https://example.com/admin/pair?code=ABCD",
            "https://example.com/auth/login?code=ABCD",
            "https://example.com/other/pair?code=ABCD",
        ] {
            assert!(
                pending(&response(uri), "https://example.com").is_err(),
                "{uri}"
            );
        }
    }
}
