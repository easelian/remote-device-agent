use std::collections::BTreeMap;
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
        || verification.path() != "/admin/pair"
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

fn open_browser(uri: &str) {
    if std::env::var_os("SSH_CONNECTION").is_some() {
        return;
    }
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }
    #[cfg(target_os = "linux")]
    let command = ("xdg-open".to_owned(), vec![uri.to_owned()]);
    #[cfg(target_os = "macos")]
    let command = ("open".to_owned(), vec![uri.to_owned()]);
    #[cfg(windows)]
    let command = {
        let executable = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
            .join("System32")
            .join("rundll32.exe");
        (
            executable.to_string_lossy().into_owned(),
            vec!["url.dll,FileProtocolHandler".to_owned(), uri.to_owned()],
        )
    };
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    if let Ok(mut child) = tokio::process::Command::new(command.0)
        .args(command.1)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
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

pub async fn pair(
    network: &Network,
    server: &str,
    roots: &BTreeMap<String, String>,
    display_name: Option<&str>,
    no_browser: bool,
    mut shutdown: watch::Receiver<bool>,
) -> Result<DeviceConfig> {
    let token = random::url_token::<32>()?;
    let system = system::info();
    let proposal = json!({
        "name": display_name.unwrap_or(&system.hostname),
        "hostname": system.hostname,
        "os": system.os,
        "machine": system.machine,
        "roots": roots,
        "token_hash": sha256_hex(token.as_bytes()),
    });
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
    let value = Network::response_json(response).await?;
    let request = pending(&value, server)?;
    println!(
        "\nPairing code: {}\nOpen: {}\n",
        request.user_code, request.verification_url
    );
    println!(
        "Check the device and pairing code in your browser, choose permissions, and approve pairing."
    );
    if !no_browser {
        open_browser(&request.verification_url);
    }
    let deadline = Instant::now() + request.expires;
    while Instant::now() < deadline {
        tokio::select! {
            _ = tokio::time::sleep(request.interval) => {},
            _ = interrupted(&mut shutdown) => return Err(Error::message("Operation canceled")),
        }
        let body = json!({"device_code": request.device_code});
        let response = tokio::select! {
            result = network.post_json(server, "/api/pair/poll", &body) => match result { Ok(response) => response, Err(_) => continue },
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
                config.insert("token".into(), Value::String(token));
                #[cfg(windows)]
                if config.get("shell").and_then(Value::as_str) == Some("/bin/sh") {
                    config.remove("shell");
                }
                let model = DeviceConfig::parse(Value::Object(config))?;
                if !same_hub(&model.gateway_url, server) || &model.roots != roots {
                    return Err(Error::message(
                        "The pairing response does not match the requested Hub or file roots",
                    ));
                }
                return Ok(model);
            }
            202 | 429 => continue,
            403 => {
                return Err(Error::message(
                    "Pairing was denied or revoked. Run the command again.",
                ));
            }
            410 => break,
            status => {
                return Err(Error::message(format!(
                    "Pairing request failed (HTTP {status})"
                )));
            }
        }
    }
    Err(Error::message(
        "Pairing timed out. Run the command again to get a new code.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(uri: &str) -> Value {
        json!({"verification_uri_complete":uri,"user_code":"ABCD","device_code":"id", "interval":3, "expires_in":600})
    }

    #[test]
    fn pairing_link_is_bound_to_the_selected_origin_and_code() {
        assert!(
            pending(
                &response("https://example.com/admin/pair?code=ABCD"),
                "https://example.com"
            )
            .is_ok()
        );
        for uri in [
            "https://evil.example/admin/pair?code=ABCD",
            "https://example.com/admin/pair?code=OTHER",
            "https://example.com/admin/pair?code=ABCD&x=1",
            "https://example.com/admin/pair?code=ABCD#section",
            "https://user@example.com/admin/pair?code=ABCD",
        ] {
            assert!(
                pending(&response(uri), "https://example.com").is_err(),
                "{uri}"
            );
        }
    }
}
