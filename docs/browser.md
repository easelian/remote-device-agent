# Browser tasks on a remote device

English | [简体中文](browser-zhCN.md)

Use a connected AI client to read and interact with websites from a bound Linux, macOS or Windows device, sign in through the same remote browser, and retrieve supported files. This preview feature is disabled by default. This guide applies to Agent builds reporting `capabilities.browser.version = 1` and the matching Gate, Core and Business web builds. The [implementation guide](browser-implementation-zhCN.md) maps the implementation to its source modules.

For Server operators, set Core's `http.browser_user_url` to the user website's
origin, allow `browser:control` in Gate's web actions, and apply the matching Auth
application-access contract with the `device-browser` purpose. The Server example
configurations contain these settings. The Agent release does not deploy the
matching services.

Linux x86_64 development builds can explicitly select `semantic_dom_v1` and CDP/BiDi using the [current v2 configuration and behavior](browser-implementation-zhCN.md). Requests without a selection retain the v1 behavior described here.

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
| `private_networks` | `allow` | What happens to private networks and the device itself. With `deny`, only the hosts in `private_allow` open. |
| `private_allow` | empty | Hosts that are exempt from `private_networks`, and from the refusal of metadata and link-local addresses. |

An entry is a host name (`example.com`), every host below a name (`*.example.com`, which does not include `example.com` itself), an address (`192.168.1.10`, `[fd00::1]`) or an address range (`10.0.0.0/8`). A range matches addresses written out in a URL, not names that resolve into it; to open a local site by name, list the name. An entry can end in a port (`localhost:3000`); without one it matches every port. Entries carry no scheme and no path. A list holds at most 256 entries, and the Agent does not start with an entry it cannot read. When the Agent starts, it prints the policy in effect.

The policy covers page navigation: the address the AI gives, links, forms, redirects, frames and new tabs. A refused address is reported to the AI as `browser_site_denied`, and a tab whose own navigation was refused shows `blocked: "site_policy"`. People who take over a session are held to the same policy.

Know its limits before relying on it:

- It does not cover the requests a page makes by itself, such as images, scripts and `fetch`. Chrome 142 and later refuse those from a public page to a private address; earlier versions do not.
- A host name is judged by the addresses the Agent finds for it. The browser looks the name up again, so a name that answers the two differently is not caught. With `private_networks` set to `deny`, the Agent also checks the address the browser really reached and withholds the page; in that mode a page out of the browser's cache takes about 0.3 seconds longer.
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

The AI uses `browser_open`, then `browser_observe`. It acts on returned element references using `browser_act`, waits for the expected page state with `browser_wait`, and observes the result again. Normal text fields, native single-select controls, checkboxes, links, buttons, page scrolling, tabs and supported frames are available. A date, a time, a colour or a slider is filled with a value in the field's own format, such as `2026-03-04`, `09:30`, `#ff8800` or `7`. Passwords and verification codes belong in the human page, not AI tool arguments.

In a v2 session, start with an unfiltered `browser_observe` and use it again after an action. The optional `frame_overview.frames` groups the observed headings, text and current control refs by frame, with iframes first. A control's `record` identifies a heading in its own record when the association is clear. Check `truncated` and `coverage` for omissions. A generic confirmation in an iframe may sit beside several outer records. Verify the requested record's status in the site's own view before reporting success.

Use `within` only with a `frame_id` from the latest observation of the current tab and control epoch. Each successful observation, including a continuation, replaces the IDs. An action clears them even if the iframe stays visible. After navigation, document replacement, handoff, or `browser_stale_reference`, omit `within` to obtain fresh IDs. Keep the original write operation ID for receipt recovery.

An explicit v2 session can use `browser_inspect` when both the device and opened session advertise actionability extension version 1 as available. It checks one current ref: `ready_now` means the sampled prerequisites hold, `needs_scroll` means outside the viewport, `blocked` reports a known restriction, and `indeterminate` means the check could not prove readiness. The check does not scroll, focus, input or renew references. For an intended click, `needs_scroll` with reason `outside_viewport` means you can call `browser_act` directly with `{"kind":"click","ref":"<current ref>"}`, including inside an iframe. The click scrolls as needed and checks that the hit point reaches the target before sending input. Verify the result; observe again after a stale-reference error and never replay an uncertain input based on a diagnostic.

When an explicit v2 session advertises `dom_evidence` version 1, `browser_evidence` can find page nodes and read node facts or local layout. The returned `node_id` is for reads. If a click has an uncertain outcome, check the original operation receipt and the page result. Request human control for sensitive input or a verification the user must complete personally.

In the default v1 observation, each element that the AI can act on has a reference, and the observation names its `kind`:

| Kind | Which elements |
| --- | --- |
| `native` | Controls that HTML defines: links, buttons, inputs of every type including colour and date, text areas, lists, the summary of a `details` element, and editable regions. |
| `aria` | Elements whose `role` attribute declares a control, such as a button, a tab, a menu item, a grid cell or a tree item. |
| `inferred` | Elements that only look like a control: the markup holds a click handler, the pointer turns into a hand over the element and not over what surrounds it, or the element is in the focus order. |

The classification uses the HTML, ARIA and interaction properties visible on the current page. An element that the page does not mark, and that the browser itself treats as a control, still has a reference of kind `native`. An inferred element is named by its own text and takes clicks and keys only. Its reference is a guess: a click can do nothing. No reference is given to an element that only a script makes clickable, that shows a hand only while the pointer is over it, that appears only on hover, or that is drawn on a canvas.

An action that cannot be carried out is refused before any input is sent: an action that does not suit the element, text for a password field, an element that is disabled or hidden, an element in view in the top document that another element covers, a click on an inferred element that a control inside it would take, as the delete button of a card would, or a value that a date, colour or slider field cannot hold. The session keeps running, and the AI observes the page again to resolve the cause. A read-only field accepts clicks and keys but not text. After the browser has scrolled an element into view or received input, a failure has an unknown outcome and pauses the session; check the original receipt with `get_operation_status` and inspect the tab with `browser_tabs`. Once pending input has settled and the session permits it, resume with `browser_control` and observe again. A read that the browser rejects while the page navigates does not pause the session.

An observation can be narrowed to part of a page:

| Parameter | What it returns |
| --- | --- |
| `filter: "interactive"` | The controls and the headings, without the text between them. |
| `filter: "viewport"` | What lies in the part of the page that shows now. |
| `find` | The places that hold a text of up to 128 characters, each with its record: the table row, list item, article or dialog around it, without the records inside it. A row that names a customer comes with the button that edits that customer. |
| `within` | One frame selected by a `frame_id` from the latest observation. On v2 obtain a new ID after each action. |

The parameters combine, references are given as usual, and a continuation keeps what its first call asked for. A search ignores case and the width of spaces. It looks at the text a page shows, at the names and labels of controls and at what fields hold, and never at what a password field holds. It returns at most 20 places, counted across frames, and 30 nodes of one record; `coverage.matches` counts the places, and `coverage.omitted` names `find_match_limit` or `find_record_clipped` when there was more. Text that markup splits is found within a record, and not outside one. `viewport` is judged in each frame by that frame's own window, and takes no account of content that a scrolling element inside the page hides.

Long observations return `next_cursor`. Continue until no cursor remains and inspect `coverage.sequence_complete` and `omitted`; an incomplete page is not evidence that a record does not exist. Every segment replaces earlier action references. Navigation, a change of the page address, an action, control changes, a fresh observation or five minutes without continuation invalidate the cursor.

A page can change while it is read, and the read goes on. When the page changed something that had already been read, `coverage.changed_during_read` is true and the coverage is not complete: the result can hold text that is no longer on the page. That alone is no reason to observe again, because every reference is checked against the page again when it is used. A control in a table row, list item, article or dialog that changed after it was reported gets no reference in that observation and carries `changed_during_read` itself; observe again to act on it. A subframe that navigated or went away during the read is left out, and `omitted` names `frame_changed`. An observation fails only when the tab's own document is replaced. A first read then starts once more on the new document within the same call; a continuation is refused. The reader does not create an atomic snapshot of a changing website.

Each observation reports the document's `ready_state` with the page address. While it is `loading`, the browser has not finished reading the page: the coverage is not complete and names `document_loading`, so wait and observe again before concluding that something is missing. An element reference stays valid while unrelated text near it changes. It stops being valid when the element's own text changes, or when the table row, list item, article or dialog it belongs to shows different text. A navigation that receives no answer is cancelled before the call's deadline and reported as a failed navigation. The session pauses. Check the original operation and tab state, then resume when pending input and cleanup have settled.

For `browser_open`, `browser_act` and `browser_control`, integrations can reserve and save an ID with `prepare_operation` before calling the target tool. This lets the client recover the original operation if the response is lost. Core prepares an ID automatically when one is omitted. Verify the result on the website after an input acknowledgement.

## Sign in and return to the AI

Open the `human_url` returned by the AI, or use **Devices → View browser sessions → Open remote browser**. The page requires your current RDH login and device access. Merely opening it does not take control.

Select **Take / reclaim control**. AI reads and actions pause. Click the remote page's text field before using the protected text input, then complete the supported login steps. The controls act only inside this browser viewport. Native OS dialogs, hardware security keys, biometric prompts and automatic CAPTCHA solving are not supported.

Before selecting **Return to AI**, inspect the pages and remove content that should remain private. Confirm that the page and related file metadata can be shared. Return to the original AI conversation and say “continue”; handing back control does not automatically wake a finished conversation. The AI should query the existing session and obtain fresh references.

If an input result is uncertain, the page stops further input. Use **Check browser status**, then explicitly reclaim control when permitted. Do not repeat uncertain input. **Close remote browser** closes the owned browser; it cannot undo requests the website already received.

## Keep a login or transfer a file

Set `persistent_profiles` to `true` to allow `browser_open.profile_name`. The named profile belongs to this device binding and AI authorization. Normal close asks the browser to flush cookies and site storage; persistence also depends on the browser and OS encryption prerequisites above. Another AI authorization cannot silently use the same profile. Use `browser_profiles` to inspect profiles and `browser_control` with `delete_profile` to remove an unused one. A profile in use cannot be opened twice or deleted.

With `downloads` enabled, supported HTTP document attachments appear in `browser_files` and **Browser files**. Save them from the authenticated page. Each download must have an explicit length, status 200 and no content compression. Blob and streaming downloads without a length are unsupported. File bytes are not returned to the AI.

With `uploads` enabled and `file_authorization` version 1 available, use `files_url` or select **Approve a file** from the session list. Confirm the destination site and the local file for one selection. The AI keeps browser control, gets the `file_grant_id` from `browser_files` and uses `attach_file` with a current main-document file-input reference. The grant expires ten minutes after creation and stays consumed if selection is uncertain. Selecting a file can start an upload immediately. A later form submission is a separate action. The older upload panel is available during a human-controlled session on compatible versions.

## Limits and cleanup

- At most two sessions and eight tabs per session. Sessions close after ten idle minutes; disconnected sessions pause and close after the Agent's five-minute offline grace.
- Temporary browser profiles are removed when their owned processes stop. Named profiles persist until explicitly deleted; at most sixteen are stored per device binding. Profile/cache disk usage and browser memory do not have a hard quota.
- File staging allows 32 MiB per file, 128 MiB in total and sixteen entries shared by uploads and downloads. Completed downloads expire after 24 hours. Consumed upload copies expire no later than 24 hours after creation. Closing a session does not remove completed downloads.
- Confirmed unbinding removes this binding's private browser profiles and artifacts after owned work stops. If cleanup cannot be confirmed, unbinding remains pending and retains the binding record for retry. Other bindings and local downloads are unaffected.
- Agent restart discards unfinished downloads and all uploads and does not resume old sessions or replay website actions. POSIX recovery checks the original process group. On Windows, an independent cleanup child terminates the browser Job after Agent death, confirms that its active process count is zero, and records completion. The next browser request uses that record to clean up the old run and permit reopening the profile. Cleanup in progress returns `browser_cleanup_pending`. If both the Agent and cleanup child are killed, or reliable completion evidence is missing, the data remains quarantined and cannot be reused or deleted by unbinding. An OS restart alone does not supply that proof.
- `.rdh-browser` is private Agent storage, excluded from file reads, writes, listings, searches and configured roots. Matching ignores case; Windows also checks normalized directory paths to reject short-name aliases. Shell permission still grants the Agent OS user's authority; this exclusion is not a shell sandbox or a boundary against that same OS account.

See the [implementation guide](browser-implementation-zhCN.md) for source ownership. Validate image rendering, task continuation, performance and usability with the intended AI host, operating system and browser. The result of `browser_open` names the browser product and version that the session runs on. First-use permission configuration and browser installation are currently local steps; the web page reports readiness and recovers existing sessions.
