use super::*;
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;

async fn revoke(network: &crate::network::Network, config: &DeviceConfig) -> Result<()> {
    let value = network
        .agent_request(
            config
                .discovery_url
                .as_deref()
                .unwrap_or(&config.gateway_url),
            "/api/agent/unbind",
            &config.token,
            &json!({"device_id":config.device_id}),
        )
        .await?;
    if value["state"] != "unbound" || value["device_id"] != config.device_id {
        return Err(Error::message(
            "The Hub did not confirm revocation of this device",
        ));
    }
    Ok(())
}

pub async fn unbind() -> Result<()> {
    let options = prepare_options(false)?;
    complete(&options.config, std::slice::from_ref(&options.config)).await?;
    println!(
        "Device unbound. Automatic startup is disabled and the Agent has stopped. Run rdh to bind again."
    );
    Ok(())
}

pub(super) async fn complete(config: &Path, paths: &[PathBuf]) -> Result<()> {
    let command_path = sidecar(config, ".unbind-command");
    let mut command = acquire(std::slice::from_ref(&command_path))?;
    request_unbind(config)?;
    let mut errors = Vec::new();
    let process = match local::instance_process(config) {
        Ok(pid) => pid,
        Err(error) => {
            errors.push(format!("Agent identity: {error}"));
            None
        }
    };
    // Revoke as soon as intent is durable. The running Agent concurrently
    // observes intent, stops admission and drains its supervised operations.
    let network = crate::network::Network::from_env();
    let early = match (&network, DeviceConfig::read(config)) {
        (Ok(network), Ok(current)) => Some((
            current.device_id.clone(),
            current.token.clone(),
            revoke(network, &current).await,
        )),
        (_, Err(Error::Io(error))) if error.kind() == std::io::ErrorKind::NotFound => None,
        (_, Err(error)) => {
            errors.push(format!("local binding: {error}"));
            None
        }
        _ => None,
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    // Let the known Agent release its profile lock before attempting recovery.
    // On Windows, even a failed acquire briefly owns the recovery guard; polling
    // it while the Agent drains can make the Agent's own release fail.
    if let Some((pid, started)) = process {
        while crate::process_identity::running(pid, &started)? && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if crate::process_identity::running(pid, &started)? {
            errors.push("Agent process exit is not confirmed".into());
            if let Err(error) = crate::startup::disable_for_unbind(config) {
                errors.push(format!("automatic startup: {error}"));
            }
            return Err(pending_error(&errors));
        }
    }
    let mut locks = loop {
        match acquire(paths) {
            Ok(locks) => break locks,
            Err(error) if Instant::now() >= deadline => {
                errors.push(format!("Agent/task shutdown is not confirmed ({error})"));
                if let Err(error) = crate::startup::disable_for_unbind(config) {
                    errors.push(format!("automatic startup: {error}"));
                }
                return Err(pending_error(&errors));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };
    if let Err(error) = crate::startup::disable_for_unbind(config) {
        errors.push(format!("automatic startup: {error}"));
    }
    match network {
        Ok(network) => {
            if let Err(error) = crate::pairing::cancel_saved(&network, config).await {
                errors.push(format!("pending pairing: {error}"));
            }
            if let Some(raw) = read_optional(config)? {
                let current = DeviceConfig::parse(serde_json::from_slice(&raw)?)?;
                let outcome = match early {
                    Some((id, token, result))
                        if id == current.device_id && token == current.token =>
                    {
                        result
                    }
                    _ => revoke(&network, &current).await,
                };
                if let Err(error) = outcome {
                    errors.push(format!("Hub revocation: {error}"));
                }
            }
        }
        Err(error) => errors.push(format!("Hub revocation: {error}")),
    }
    if !errors.is_empty() {
        return Err(pending_error(&errors));
    }
    if let Some(raw) = read_optional(config)? {
        let current = DeviceConfig::parse(serde_json::from_slice(&raw)?)?;
        crate::browser::remove_binding(config, &current.token)
            .map_err(|error| pending_error(&[format!("browser data cleanup: {error}")]))?;
    }
    remove_private(config)?;
    remove_private(&sidecar(config, ".pairing.json"))?;
    remove_private(&sidecar(config, ".unbind.json"))?;
    locks.release()?;
    command.release()?;
    Ok(())
}

fn pending_error(errors: &[String]) -> Error {
    Error::message(format!(
        "Unbind is pending: {}. Run rdh unbind again after resolving these errors; the old binding will not restart.",
        errors.join("; ")
    ))
}
