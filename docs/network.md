# Local network configuration

The Agent follows Node's network policy at process startup. With no proxy opt-in,
connections are direct. The WebSocket channel explicitly reuses Node's global
HTTP(S) agent because `ws` otherwise bypasses it with its own connection factory.
Pairing and upgrade requests use Node's built-in `fetch` and the same startup
environment. There is no custom proxy client or fixed address-family timeout.

Proxy addresses, credentials, bypass lists and network tuning are local operator
settings. They do not belong in device pairing JSON, Hub tool parameters, generated
deployment defaults, release packages or committed service definitions. The Agent
does not discover a desktop proxy or copy the installer's shell environment into
a service. A configuration change takes effect when that Agent process restarts.

## Enable a proxy on one machine

Use Node 22.21+ or 24.5+ for native proxy support. Keep the general Agent minimum at
Node 22: older supported Node versions can still connect directly. Do not enable a
proxy on an older runtime, which may ignore the setting.

Set `NODE_USE_ENV_PROXY=1` before starting Node, together with `HTTPS_PROXY` and/or
`HTTP_PROXY`. Node also accepts its `--use-env-proxy` startup option. `NO_PROXY`
selects destinations that should connect directly. Lowercase proxy variables take
precedence over uppercase variables; remove conflicting entries. The Agent does
not modify these values. TLS certificate verification remains enabled.

For manual startup, supply the environment to the `rdh` process. For a managed
service, configure that service's own environment. On Linux, a local systemd
service drop-in can reference a private environment file:

```ini
[Service]
EnvironmentFile=/absolute/private/path/agent-network.env
```

The file can contain the following settings, with the proxy address replaced by
the operator's local value:

```ini
NODE_USE_ENV_PROXY=1
HTTPS_PROXY=http://proxy.example.invalid:3128
NO_PROXY=localhost,127.0.0.1,::1
```

Restrict the environment file to its owner (mode `0600`) and keep it outside the
installation's version directories. Apply the drop-in to the actual installed
unit, reload systemd and restart that unit. User units require `systemctl --user`.
On macOS or Windows, provide the same environment through the local service or
logon-task configuration and restrict access to any credential-bearing file.
Installer-created startup definitions contain no proxy values; registering startup
does not persist a foreground shell's proxy environment. Package upgrades preserve
external service overrides, but reinstalling or changing startup mode may require
reattaching them. Recheck the service's connection after either operation.

If a particular machine requires a different address-family fallback interval,
set Node's `--network-family-autoselection-attempt-timeout` through that machine's
`NODE_OPTIONS`. The Agent does not force an interval or address family for every
installation. Do not put proxy passwords in command-line arguments.

## Data boundaries and validation

Proxy variables and `NODE_OPTIONS` are not inherited by remote shell processes
under the default environment allowlist. A local hardening policy can explicitly
allow named variables; that is a separate operator decision. Network diagnostics
report bounded error codes and handshake/close status, without proxy URLs or
credentials. HTTPS proxy tunnels do not send the device bearer token in CONNECT
headers; the token is sent inside the verified TLS connection.

The automated tests launch fresh Node processes and exercise default direct
connections, an explicitly enabled HTTP proxy, `NO_PROXY`, local timeout options,
and HTTPS CONNECT credential separation. These tests use local fixtures. They do
not establish connectivity through a particular corporate proxy, or prove a real
macOS/Windows service has inherited its environment.

Node's [native proxy documentation](https://nodejs.org/api/http.html#built-in-proxy-support)
and [startup options](https://nodejs.org/api/cli.html#node_use_env_proxy1) define the
supported proxy syntax and runtime behavior.
