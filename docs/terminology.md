# Remote Device Hub terminology

Remote Device Hub (RDH) is the Easelian product. Remote Device Server is the server implementation; Remote Device Agent (Agent) is the native program `rdh` running on a device. A client plugin or extension connects an AI host to RDH and supplies host-specific integration and operating guidance.

| Term | Meaning |
| --- | --- |
| Auth | Customer identity, sign-in, OIDC and MCP OAuth. |
| Gate | Device pairing, long-lived device credentials, management authorization, assignment and MCP routing. |
| Core | Agent WSS connections, execution admission, actual consumption and operation receipts. |
| Business | Product website, website sessions, commerce, support and versioned entitlement exports. Web means this website or service when used in older documentation. |
| Admin | Staff identity and management, separate from customer identity. |
| Pairing / binding | Pairing is the approval flow; a binding is the resulting saved device identity and configuration. Unbinding revokes device access. |
| Authorization / local policy | Account and device access is checked by the services; Agent policy limits operations on the device. Client approval prompts are an additional host control. |
| File root / shared directory | A named file-tool boundary. Shell commands run with the Agent OS account's permissions; file roots do not sandbox commands. |
| Entitlement / allowance / consumption | Entitlements declare available rights and allowance windows; allowance is a call budget; consumption is actual usage maintained by Core. Resource limits on the Agent are separate. |
| Operation / receipt / managed job | An operation has a stable operation ID and a recoverable outcome. Its receipt records execution facts. A process or search job can outlive the request that starts it and has its own job ID. A timeout can leave effects unknown. |

Clients authorize through Auth and send MCP calls to Gate, which routes authorized execution to Core. The Agent presents its long-lived device credential to Gate for discovery and uses the returned short-lived, single-use admission ticket to open an outbound WSS connection to Core. Device credentials, admission tickets, MCP OAuth tokens and website sessions have separate purposes.

Browser sessions are an opt-in preview. A browser profile stores browser login state; it is separate from an Agent binding or an RDH account. Human takeover is an explicit handoff within a browser session.

The server owns the [canonical terminology and Chinese equivalents](https://github.com/easelian/remote-device-server/blob/main/docs/terminology.md) and the [five-service architecture](https://github.com/easelian/remote-device-server/blob/main/docs/product-architecture.md). Keep user-facing descriptions at product level; use service names when describing responsibility, credentials or network paths. Preserve API names, configuration keys and historical migration names verbatim.
