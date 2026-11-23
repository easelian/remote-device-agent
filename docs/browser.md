# Browser tasks on a remote device

English | [简体中文](browser-zhCN.md)

Use a connected AI client to read and interact with websites from a bound Linux, macOS or Windows device, sign in through the same remote browser, and retrieve supported files. This preview feature is disabled by default. This guide applies to Agent builds reporting `capabilities.browser.version = 1` and the matching Gate, Core and Business web builds. The [implementation guide](browser-implementation-zhCN.md) maps the implementation to its source modules.

For Server operators, set Core's `http.browser_user_url` to the user website's
origin, allow `browser:control` in Gate's web actions, and apply the matching Auth
application-access contract with the `device-browser` purpose. The Server example
configurations contain these settings. The Agent release does not deploy the
matching services.

Browser sessions currently run only on Linux x86_64, in an Agent built with the `experimental-browser-bidi` feature, and the local policy must set `capability_v2` to `true` besides `enabled`. An Agent on another platform or build reports `available: false` and `reason: "platform_or_build_unsupported"` in `device_info.capabilities.browser` and offers no browser operations. What this guide says about installation, encryption and the sandbox on macOS and Windows describes the browser storage and process supervision code that exists for those platforms; it applies once the session runtime has been ported to them. See the [implementation guide](browser-implementation-zhCN.md).

## Enable browser sessions

The remote device needs a Chromium executable that can run with its sandbox under the Agent's OS account. The Agent uses a private browser profile and a private protocol pipe. It does not attach to your everyday browser. The shared runtime supports Linux and macOS; Windows builds need the native process and private-storage features enabled by the default build. Native validation covers Linux x64, macOS ARM64 and Windows 11 ARM64. Other architectures and browser packages need their own runtime evidence.

1. Run the candidate `rdh` normally and complete device binding if needed. Its terminal output shows `AI browser sessions` and the exact `Local browser policy` path. A bound device keeps its existing identity.
2. Stop the Agent. Open that policy file, or create it if it does not exist. Preserve existing settings and add the `browser` object below. The policy file must belong to the Agent user: mode `0600` on Linux/macOS, or a private ACL with inheritance disabled on Windows. Symbolic links and Windows reparse paths are rejected. See [Hardening](hardening.md#policy-file).
3. Optionally set `executable` to the installed browser's absolute executable path. If omitted, use the platform locations below. Finding a file does not prove that a wrapper, dependencies or the browser sandbox can launch successfully; the first session checks that.
4. Start the same `rdh` again. In the device page, the browser status explains whether it is disabled, unsupported, missing or enabled. Your AI client can also check `device_info.capabilities.browser`. File and shell permissions are separate; browser use does not require enabling shell or general file writes.

This is a complete policy only when no other local settings are needed. The three optional features are disabled here; change each to `true` only when you want it:

```json
{
  "version": 1,
  "browser": {
    "enabled": true,
    "capability_v2": true,
    "persistent_profiles": false,
    "downloads": false,
    "uploads": false
  }
}
```

Automatic detection checks these locations in order; an explicit `executable` must contain an actual absolute path, without environment-variable placeholders.

| Platform | Detection locations |
| --- | --- |
| Linux | `/usr/bin/chromium`, `/usr/bin/chromium-browser`, `/usr/bin/google-chrome`, `/opt/google/chrome/chrome` |
| macOS | Chrome, Edge, Chromium, then Chrome for Testing app executables under `/Applications` |
| Windows | Chrome, Edge, then Chromium under each of `ProgramFiles`, `ProgramFiles(x86)`, then `LOCALAPPDATA` |

On macOS, retained login cookies need the browser's normal access to the user's unlocked Keychain. The persistence fixture passed in the logged-in user's GUI launch session; the SSH-launched fixture retained site storage but lost its cookie and reported unavailable encryption. Run the Agent in the appropriate logged-in user session when retaining logins; the Agent does not unlock the Keychain or bypass browser encryption.

When launched as a macOS user GUI background agent, connecting to a LAN Hub also needs Local Network permission. Allow the corresponding Agent under System Settings → Privacy & Security → Local Network, and check again after changing builds or launch methods. Denial can appear as `No route to host`. See [Apple's local network privacy guidance](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy).

On Windows, deeply nested custom configuration directories can exceed the browser's limits for generated file paths. Edge reported `The filename or extension is too long (0xCE)` and failed the retained-profile check in such a directory; the same candidate passed from a shorter local directory. Use a short local configuration location, then verify the login after closing and reopening the named profile. The Agent's startup check does not detect every browser storage-path limit.

If you use `allowed_operations`, include the browser tools you need. The web pages also require the internal `browser_human` operation. The Agent never installs a browser or enables these permissions automatically. Browser traffic uses the remote device's network; shared file roots do not restrict reachable websites.

## Choose which sites may be opened

By default a browser session opens any public site, sites on your private network and sites on the device itself. Addresses of cloud metadata services and link-local addresses (`169.254.0.0/16`, `fe80::/10`) are refused: a page has no ordinary reason to open them, and some clouds hand out credentials there. To change this, add a `sites` object to `browser` in the policy file:

```json
{
  "version": 1,
  "browser": {
    "enabled": true,
    "sites": {
      "public": "allow",
      "allow": ["*.example.org", "intranet.example.com"],
      "deny": ["*.bank.example"],
      "private_networks": "allow",
      "private_allow": ["localhost:3000", "wiki.corp", "192.168.10.0/24"]
    }
  }
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `public` | `allow` | What happens to a public site that no list names. With `deny`, only the sites in `allow` open. |
| `allow` | empty | The public sites that open when `public` is `deny`. It does not open private addresses. |
| `deny` | empty | Sites that are always refused. It takes precedence over every other field. |
| `private_networks` | `allow` | What happens to private networks and the device itself. `deny` means that only the hosts in `private_allow` open; at present a device set to `deny` offers no browser sessions, for the reason given under the limits below. |
| `private_allow` | empty | Hosts that are exempt from `private_networks`, and from the refusal of metadata and link-local addresses. |

An entry is a host name (`example.com`), every host below a name (`*.example.com`, which does not include `example.com` itself), an address (`192.168.1.10`, `[fd00::1]`) or an address range (`10.0.0.0/8`). A range matches addresses written out in a URL, not names that resolve into it; to open a local site by name, list the name. An entry can end in a port (`localhost:3000`); without one it matches every port. Entries carry no scheme and no path. A list holds at most 256 entries, and the Agent does not start with an entry it cannot read. When the Agent starts, it prints the policy in effect.

The policy covers page navigation: the address the AI gives, links, forms, redirects, frames and new tabs. A refused address is reported to the AI as `browser_site_denied`, and a tab whose own navigation was refused shows `blocked: "site_policy"`. People who take over a session are held to the same policy.

Know its limits before relying on it:

- It does not cover the requests a page makes by itself, such as images, scripts and `fetch`. Chrome 142 and later refuse those from a public page to a private address; earlier versions do not.
- A host name is judged by the addresses the Agent finds for it. The browser looks the name up again, so a name that answers the two differently is not caught. With `private_networks` set to `deny` this gap cannot be accepted, and the session runtime cannot yet compare the address the browser really reached. Under such a policy the Agent therefore offers no browser sessions, and `device_info.capabilities.browser` reports `reason: "browser_policy_unsupported"`.
- With the default settings an AI that a page misleads can still reach your private network. If the device has shell permission, the AI can reach it without the browser.

## Decide whether a shell and browser sessions go together

Shell permission and browser sessions are separate switches. When a device has both, text on a web page can mislead the AI into running a command. A command outside a sandbox runs as the Agent's user: it can read the logins that browser sessions have stored, change this policy file, and reach your network without passing the site policy.

`shell_coexistence` in the `browser` object says what you accept:

| Value | Meaning |
| --- | --- |
| `allow` (default) | A shell and browser sessions are offered together. Nothing changes for existing devices. |
| `sandboxed_only` | Browser sessions are offered beside a shell only while shell commands run in the [sandbox](hardening.md#shell-sandbox). The sandbox hides the browser's storage and the policy file from them. |
| `deny` | Browser sessions are not offered while the device has a shell. |

Where the condition is not met, the shell keeps working and the browser reports itself unavailable, with the reason `shell_not_sandboxed` or `shell_enabled`. A device without a shell is not affected. The sandbox exists on Linux only, so on macOS and Windows `sandboxed_only` has the effect of `deny`.

When the Agent starts with both enabled, it prints what that means. `device_info.capabilities.browser.shell` reports whether a shell is enabled, whether it is sandboxed, and the value in effect.

Know its limits. With `allow`, nothing stops a misled AI from running a command. `sandboxed_only` keeps commands away from the browser's storage and the policy; with `sandbox.network` set to `true`, a command can still reach your network.

## Start and verify a task

Ask your connected AI client to use the specific device and website, for example: “On my test device, open the development site, read the pending records, and report which pages you covered.” Identify the device first if its name is ambiguous.

When the client lists `browser_guidance`, request one topic for the device or an existing session: `open`, `observe`, `recover`, `handoff`, or `files`. It provides examples for explicitly reported capabilities. Replace placeholders from the authenticated results named by `bindings`. Reading guidance does not open a page or authorize an action; each actual call checks current state and permissions.

The AI uses the browser through three operations. `browser_do` carries out a list of steps, such as opening a page, clicking, filling a field, waiting and closing the session. `browser_read` reads the page, the tabs, the sessions or diagnostic information. `browser_see` returns a screenshot of the current viewport. Parameters, reply format and what to do after each kind of failure are in the [browser interface for an AI](browser-ai-interface-zhCN.md).

The earlier operations, such as `browser_open`, `browser_observe`, `browser_act` and `browser_control`, have been removed. The Agent refuses these names as unknown operations. A local policy whose `allowed_operations` still lists one of them does not load; remove the names first.

Normal text fields, native single-select controls, checkboxes, links, buttons, page scrolling, tabs and supported frames are available. A date, a time, a colour and a slider are filled with a value in the form that field holds, such as `2026-03-04`, `09:30`, `#ff8800` or `7`. The AI cannot fill a password field: the step fails with `browser_sensitive_input` and the text is not sent to the page. Enter passwords and verification codes on the human page, not in AI tool arguments.

A step names its element, for example "`Approve` in the row of `Order C1`". When the description fits several elements, the reply lists the candidates and the step is not carried out. The reply to each step says which part of the page changed. After a failed step the following steps are not carried out. When the outcome of a step is unknown, the reply says so and the page may have received the input: check the page with `browser_read` before doing anything else, and do not simply repeat the step.

A read can cover part of a page: `find` returns the places that hold a text, and `filter` limits the read to controls. When a long page was not read to its end, the reply says how to read on; an incomplete read does not prove that a record is absent. A page can change while it is read, and the reader does not freeze a site that keeps changing into one moment.

Controls drawn on a canvas and nodes inside closed Shadow DOM cannot be named, and clicking at a position in a screenshot is not available at present. Hand such controls, and content that requires personal verification, to the user.

`browser_do` is a write and needs an operation receipt. Integrations can reserve and save an ID with `prepare_operation` before the call, so that the same operation can be queried if the response is lost. Core prepares one when the ID is omitted. After the browser acknowledges input, the result on the site still has to be checked.

## Sign in and return to the AI

Open the `human_url` returned by the AI, or use **Devices → View browser sessions → Open remote browser**. The page requires your current RDH login and device access. Merely opening it does not take control.

Select **Take / reclaim control**. AI reads and actions pause. Click the remote page's text field before using the protected text input, then complete the supported login steps. The controls act only inside this browser viewport. Native OS dialogs, hardware security keys, biometric prompts and automatic CAPTCHA solving are not supported.

Before selecting **Return to AI**, inspect the pages and remove content that should remain private. Confirm that the page and related file metadata can be shared. Return to the original AI conversation and say “continue”; handing back control does not automatically wake a finished conversation. The AI should query the existing session and obtain fresh references.

If an input result is uncertain, the page stops further input. Use **Check browser status**, then explicitly reclaim control when permitted. Do not repeat uncertain input. **Close remote browser** closes the owned browser; it cannot undo requests the website already received.

## Keep a login or transfer a file

Named profiles are not available at present: an `open` step cannot name a profile, so every session uses a temporary profile that is removed when its browser process exits, and `persistent_profiles` has no effect for now. Named profiles that an earlier version created through the old operations remain in the private browser storage of the device. The Agent no longer lists or deletes them; they are deleted with the browser data of the binding when the device is unbound.

With `downloads` enabled, supported HTTP document attachments appear in `browser_files` and **Browser files**. Save them from the authenticated page. Each download must have an explicit length, status 200 and no content compression. Blob and streaming downloads without a length are unsupported. File bytes are not returned to the AI.

With `uploads` enabled and `file_authorization` version 1 available, use `files_url` or select **Approve a file** from the session list. Confirm the destination site and the local file for one selection. The AI keeps browser control, gets the `file_grant_id` from `browser_files` and gives the file to a file input of the current main document with the `upload` step of `browser_do`. The grant expires ten minutes after creation and stays consumed if selection is uncertain. Selecting a file can start an upload immediately. A later form submission is a separate action. The older upload panel is available during a human-controlled session on compatible versions.

## Limits and cleanup

- At most two sessions and eight tabs per session. Sessions close after ten idle minutes; disconnected sessions pause and close after the Agent's five-minute offline grace.
- Temporary browser profiles are removed when their owned processes stop. Named profiles persist until explicitly deleted; at most sixteen are stored per device binding. Profile/cache disk usage and browser memory do not have a hard quota.
- File staging allows 32 MiB per file, 128 MiB in total and sixteen entries shared by uploads and downloads. Completed downloads expire after 24 hours. Consumed upload copies expire no later than 24 hours after creation. Closing a session does not remove completed downloads.
- Confirmed unbinding removes this binding's private browser profiles and artifacts after owned work stops. If cleanup cannot be confirmed, unbinding remains pending and retains the binding record for retry. Other bindings and local downloads are unaffected.
- Agent restart discards unfinished downloads and all uploads and does not resume old sessions or replay website actions. POSIX recovery checks the original process group. On Windows, an independent cleanup child terminates the browser Job after Agent death, confirms that its active process count is zero, and records completion. The next browser request uses that record to clean up the old run and permit reopening the profile. Cleanup in progress returns `browser_cleanup_pending`. If both the Agent and cleanup child are killed, or reliable completion evidence is missing, the data remains quarantined and cannot be reused or deleted by unbinding. An OS restart alone does not supply that proof.
- `.rdh-browser` is private Agent storage, excluded from file reads, writes, listings, searches and configured roots. Matching ignores case; Windows also checks normalized directory paths to reject short-name aliases. Shell permission still grants the Agent OS user's authority; this exclusion is not a shell sandbox or a boundary against that same OS account.

See the [implementation guide](browser-implementation-zhCN.md) for source ownership. Validate image rendering, task continuation, performance and usability with the intended AI host, operating system and browser. First-use permission configuration and browser installation are currently local steps; the web page reports readiness and recovers existing sessions.
