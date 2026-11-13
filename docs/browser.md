# Browser tasks on a remote device

English | [简体中文](browser-zhCN.md)

Use a connected AI client to read and interact with websites from a bound Linux, macOS or Windows device, sign in through the same remote browser, and retrieve supported files. This preview feature is disabled by default. This guide applies to Agent builds reporting `capabilities.browser.version = 1` and the matching Gate, Core and Business web builds. The [architecture guide](browser-architecture-zhCN.md) maps the implementation to its source modules.

For Server operators, set Core's `http.browser_user_url` to the user website's
origin, allow `browser:control` in Gate's web actions, and apply the matching Auth
application-access contract with the `device-browser` purpose. The Server example
configurations contain these settings. The Agent release does not deploy the
matching services.

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
```

| Field | Default | Meaning |
| --- | --- | --- |
| `public` | `allow` | What happens to a public site that no list names. With `deny`, only the sites in `allow` open. |
| `allow` | empty | The public sites that open when `public` is `deny`. It does not open private addresses. |
| `deny` | empty | Sites that are always refused. It takes precedence over every other field. |
| `private_networks` | `allow` | What happens to private networks and the device itself. With `deny`, only the hosts in `private_allow` open. |
| `private_allow` | empty | Hosts that are exempt from `private_networks`, and from the refusal of metadata and link-local addresses. |

An entry is a host name (`example.com`), every host below a name (`*.example.com`, which does not include `example.com` itself), an address (`192.168.1.10`, `[fd00::1]`) or an address range (`10.0.0.0/8`). A range matches addresses written out in a URL, not names that resolve into it; to open a local site by name, list the name. An entry can end in a port (`localhost:3000`); without one it matches every port. Entries carry no scheme and no path. A list holds at most 256 entries, and the Agent does not start with an entry it cannot read. When the Agent starts, it prints the policy in effect.

The policy covers page navigation: the address the AI gives, links, forms, redirects, frames and new tabs. A refused address is reported to the AI as `browser_site_denied`, and a tab whose own navigation was refused shows `blocked: "site_policy"`. People who take over a session are held to the same policy.

Know its limits before relying on it:

- It does not cover the requests a page makes by itself, such as images, scripts and `fetch`. Chrome 142 and later refuse those from a public page to a private address; earlier versions do not.
- A host name is judged by the addresses the Agent finds for it. The browser looks the name up again, so a name that answers the two differently is not caught. With `private_networks` set to `deny`, the Agent also checks the address the browser really reached and withholds the page; in that mode a page out of the browser's cache takes about 0.3 seconds longer.
- With the default settings an AI that a page misleads can still reach your private network. If the device has shell permission, the AI can reach it without the browser.

## Start and verify a task

Ask your connected AI client to use the specific device and website, for example: “On my test device, open the development site, read the pending records, and report which pages you covered.” Identify the device first if its name is ambiguous.

The AI uses `browser_open`, then `browser_observe`. It acts on returned element references using `browser_act`, waits for the expected page state with `browser_wait`, and observes the result again. Normal text fields, native single-select controls, checkboxes, links, buttons, page scrolling, tabs and supported frames are available. Passwords and verification codes belong in the human page, not AI tool arguments.

An action that cannot be carried out is refused before any input is sent: an action that does not suit the element, text for a password field, an element that is disabled or hidden, or an element in view in the top document that another element covers. The session keeps running, and the AI observes the page again to resolve the cause. A read-only field accepts clicks and keys but not text. After the browser has scrolled an element into view or received input, a failure has an unknown outcome and pauses the session; inspect it with `browser_tabs`, resume with `browser_control`, and observe again. A read that the browser rejects while the page navigates does not pause the session.

Long observations return `next_cursor`. Continue until no cursor remains and inspect `coverage.sequence_complete` and `omitted`; an incomplete page is not evidence that a record does not exist. Every segment replaces earlier action references. Navigation, a change of the page address, an action, control changes, a fresh observation or five minutes without continuation invalidate the cursor.

A page can change while it is read, and the read goes on. When the page changed something that had already been read, `coverage.changed_during_read` is true and the coverage is not complete: the result can hold text that is no longer on the page. That alone is no reason to observe again, because every reference is checked against the page again when it is used. A control in a table row, list item, article or dialog that changed after it was reported gets no reference in that observation and carries `changed_during_read` itself; observe again to act on it. A subframe that navigated or went away during the read is left out, and `omitted` names `frame_changed`. An observation fails only when the tab's own document is replaced. A first read then starts once more on the new document within the same call; a continuation is refused. The reader does not create an atomic snapshot of a changing website.

Each observation reports the document's `ready_state` with the page address. While it is `loading`, the browser has not finished reading the page: the coverage is not complete and names `document_loading`, so wait and observe again before concluding that something is missing. An element reference stays valid while unrelated text near it changes. It stops being valid when the element's own text changes, or when the table row, list item, article or dialog it belongs to shows different text. A navigation that receives no answer is cancelled before the call's deadline and reported as a failed navigation. The session pauses as after any uncertain navigation, keeps the earlier page, and can be resumed at once.

For integrations, `browser_open`, `browser_act` and `browser_control` require an operation ID from `prepare_operation`. Reuse that ID only to recover the same request. An acknowledged input says that browser input completed; verify the website's actual result separately. A timeout can leave an effect unknown, so query operation/session state instead of repeating a submission.

## Sign in and return to the AI

Open the `human_url` returned by the AI, or use **Devices → View browser sessions → Open remote browser**. The page requires your current RDH login and device access. Merely opening it does not take control.

Select **Take / reclaim control**. AI reads and actions pause. Click the remote page's text field before using the protected text input, then complete the supported login steps. The controls act only inside this browser viewport. Native OS dialogs, hardware security keys, biometric prompts and automatic CAPTCHA solving are not supported.

Before selecting **Return to AI**, inspect the pages and remove content that should remain private. Confirm that the page and related file metadata can be shared. Return to the original AI conversation and say “continue”; handing back control does not automatically wake a finished conversation. The AI should query the existing session and obtain fresh references.

If an input result is uncertain, the page stops further input. Use **Check browser status**, then explicitly reclaim control when permitted. Do not repeat uncertain input. **Close remote browser** closes the owned browser; it cannot undo requests the website already received.

## Keep a login or transfer a file

Set `persistent_profiles` to `true` to allow `browser_open.profile_name`. The named profile belongs to this device binding and AI authorization. Normal close asks the browser to flush cookies and site storage; persistence also depends on the browser and OS encryption prerequisites above. Another AI authorization cannot silently use the same profile. Use `browser_profiles` to inspect profiles and `browser_control` with `delete_profile` to remove an unused one. A profile in use cannot be opened twice or deleted.

With `downloads` enabled, supported HTTP document attachments appear in `browser_files` and **Browser files**. Save them from the authenticated page. Each download must have an explicit length, status 200 and no content compression. Blob and streaming downloads without a length are unsupported. File bytes are not returned to the AI.

With `uploads` enabled, take human control, choose a local file and explicitly approve it for the displayed site. The page stages and hashes the file. After you return control, the AI uses the resulting `file_grant_id` with a fresh main-page file-input reference. The grant is used once, expires after ten minutes if unused, and cannot be restored after an uncertain selection. Selecting a file may upload it immediately; later form submission remains a separate action. Arbitrary remote paths and frame file inputs are not accepted.

## Limits and cleanup

- At most two sessions and eight tabs per session. Sessions close after ten idle minutes; disconnected sessions pause and close after the Agent's five-minute offline grace.
- Temporary browser profiles are removed when their owned processes stop. Named profiles persist until explicitly deleted; at most sixteen are stored per device binding. Profile/cache disk usage and browser memory do not have a hard quota.
- File staging allows 32 MiB per file, 128 MiB in total and sixteen entries shared by uploads and downloads. Completed downloads expire after 24 hours. Consumed upload copies expire no later than 24 hours after creation. Closing a session does not remove completed downloads.
- Confirmed unbinding removes this binding's private browser profiles and artifacts after owned work stops. If cleanup cannot be confirmed, unbinding remains pending and retains the binding record for retry. Other bindings and local downloads are unaffected.
- Agent restart discards unfinished downloads and all uploads and does not resume old sessions or replay website actions. POSIX recovery checks the original process group. On Windows, an independent cleanup child terminates the browser Job after Agent death, confirms that its active process count is zero, and records completion. The next browser request uses that record to clean up the old run and permit reopening the profile. Cleanup in progress returns `browser_cleanup_pending`. If both the Agent and cleanup child are killed, or reliable completion evidence is missing, the data remains quarantined and cannot be reused or deleted by unbinding. An OS restart alone does not supply that proof.
- `.rdh-browser` is private Agent storage, excluded from file reads, writes, listings, searches and configured roots. Matching ignores case; Windows also checks normalized directory paths to reject short-name aliases. Shell permission still grants the Agent OS user's authority; this exclusion is not a shell sandbox or a boundary against that same OS account.

See the [architecture guide](browser-architecture-zhCN.md) for source ownership. Validate image rendering, task continuation, performance and usability with the intended AI host, operating system and browser. The result of `browser_open` names the browser product and version that the session runs on. First-use permission configuration and browser installation are currently local steps; the web page reports readiness and recovers existing sessions.
