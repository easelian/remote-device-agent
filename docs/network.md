# Local network configuration

The Rust Agent connects directly unless `RDH_USE_ENV_PROXY=1` is set in its local
launch environment. Pairing, native updates and WebSocket connections use the
same proxy selection and certificate policy. TLS verification remains enabled.

Proxy addresses, passwords and CA files are local operator settings. They do not
belong in device pairing JSON, Hub requests or distributed artifacts. Changing
the environment takes effect after restarting that Agent process.

## Proxy and trust settings

With proxy support enabled, `HTTPS_PROXY` applies to HTTPS/WSS and `HTTP_PROXY` to
HTTP/WS. `NO_PROXY` selects destinations that connect directly. Lowercase names
take precedence over uppercase names; remove conflicting entries. HTTP and HTTPS
proxy endpoints are supported. The Agent does not discover desktop proxy settings.

For a foreground process, replace the proxy URL below with the local endpoint:

```sh
RDH_USE_ENV_PROXY=1 HTTPS_PROXY=http://proxy.example.invalid:3128 \
  NO_PROXY=localhost,127.0.0.1,::1 /absolute/install/rdh --config /absolute/agent.json
```

The Agent uses the platform's trusted root certificates. Set `RDH_CA_BUNDLE` to a
local PEM file to add private trust roots. The file supplements the normal roots;
it does not disable certificate or hostname validation.

Node runtime variables do not configure this executable. The Agent reports
ignored variable names without printing their values. Use the Rust settings
above; certificate and hostname verification cannot be disabled.

## Managed services

Configure the environment of the actual service or logon task. Installing a
service does not copy the installer's shell environment. On Linux, a local
systemd drop-in can reference a private environment file:

```ini
[Service]
EnvironmentFile=/absolute/private/agent-network.env
```

The file may contain:

```ini
RDH_USE_ENV_PROXY=1
HTTPS_PROXY=http://proxy.example.invalid:3128
NO_PROXY=localhost,127.0.0.1,::1
RDH_CA_BUNDLE=/absolute/private/company-ca.pem
```

Use mode `0600`, with ownership suitable for the service manager. Reload the
actual unit and restart it locally; user units use `systemctl --user`. macOS and
Windows require equivalent environment configuration in their local launch
mechanism. Protect any credential-bearing files. Recheck the Hub connection after
changes. Service update inspection rejects definitions it cannot
validate, including unsupported overrides; do not remove an override solely to
make a validation pass without preserving its required network settings.

## Boundaries and evidence

Remote shell processes do not inherit proxy variables or Agent credentials under
the default allowlist. A local policy can explicitly allow named environment
variables. HTTPS CONNECT contains only proxy authentication; the device bearer
token is sent inside the verified destination TLS connection.

Rust loopback fixtures exercise direct connections, proxy opt-in, bypass lists,
private CAs and CONNECT authentication separation. They do not prove that a
particular corporate proxy is reachable or that a deployed service inherited its
intended environment. Confirm both the process environment and a new Hub `ready`
message on the actual device.
