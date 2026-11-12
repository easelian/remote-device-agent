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
  NO_PROXY=localhost,127.0.0.1,::1 rdh
```

The Agent uses the platform's trusted root certificates. Set `RDH_CA_BUNDLE` to a
local PEM file to add private trust roots. The file supplements the normal roots;
it does not disable certificate or hostname validation.

## Pairing

Pairing starts at Gate's `/api/pair/start` endpoint on the configured RDH product
origin. The browser link must use `/auth/pair` on that same origin and contain
only the returned pairing code. Business serves the product pairing page and
uses Auth for customer sign-in; Gate owns the pairing decision and device credential. The Agent rejects pairing links outside `/auth/pair`. After browser approval,
`/api/pair/poll` supplies the configuration, including the Gate's `discovery_url`;
the Agent saves it with the locally generated device credential.

Gate does not accept WebSocket connections at `/agent/{device_id}`. Saved
profiles must include `discovery_url` to connect through Gate. If Gate also
rejects the saved credential, back up that profile and pair again; adding a
discovery address alone does not renew the credential.

## Cluster discovery

A paired `discovery_url` identifies the Gate. The saved device credential authenticates `/agent/assign`, `/api/agent/status`
and `/api/agent/unbind` at this Gate. The request declares `direct-ip-v2` and binds
the Agent process instance; the version 2 reply supplies a short-lived admission
ticket, assignment identity and Core address. Reconnects request a new ticket.
Invalid or missing tickets fail discovery; the Agent does not fall back to sending
the device credential to the Core. Direct bearer authentication is available only for configurations without
discovery and is not accepted by a v2 Core.

If the assignment includes `connect_ip`, only the socket destination changes.
The Core URL hostname still selects proxy bypass rules, WebSocket Host and TLS
SNI/certificate verification. WSS is required, and a failed hinted connection
returns to discovery instead of retrying that connection through DNS.

After detecting a lost active connection, the Agent disables the old control
generation and requests a fresh ticket from Gate with its complete previous
connection identity. Gate can release that exact predecessor after checking the
device credential and process instance, without waiting for its lease to expire.
An available previous Core is tried first; a failed connection returns to Gate
for a different Core. Gate and Core must support `direct-ip-v2` admission and
predecessor connection release.

Fast recovery shares a 10-second budget from disconnect detection through the
new Core's `active` confirmation. Discovery gets at most 2 seconds; the first
Core connection and handshake get 2 seconds, and subsequent attempts get at most
4 seconds, all capped by the remaining budget. Initial fast retries add 0–199 ms
of jitter. If recovery still fails, retries use exponential backoff from 1 to 30
seconds with less than 1 second of additional jitter. Ownership conflicts do not
mark a healthy Core unreachable. Recovery requires Gate, its database and a Core
to be available; silent packet loss still has to reach the heartbeat deadline
before this recovery clock starts.

`Connection lifecycle` log records expose `disconnected` and `ready` events with
process-local `monotonic_ms` values. Their difference measures detected-disconnect
to active control recovery without depending on remote wall-clock synchronization.

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
changes. `rdh update` preserves the executable path and startup registration.

## Boundaries and evidence

Remote shell processes do not inherit proxy variables or Agent credentials under
the default allowlist. A local policy can explicitly allow named environment
variables. HTTPS CONNECT contains only proxy authentication; the Gate credential
or Core admission ticket is sent only inside its verified destination TLS connection.

Rust loopback fixtures exercise direct connections, proxy opt-in, bypass lists,
private CAs and CONNECT authentication separation. They do not prove that a
particular corporate proxy is reachable or that a deployed service inherited its
intended environment. Confirm both the process environment and a new Hub `ready`
message on the actual device.
