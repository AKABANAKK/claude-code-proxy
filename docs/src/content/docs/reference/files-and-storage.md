---
title: Files and storage
description: Canonical claude-code-proxy configuration, credential, device ID, log, error, traffic-capture, and service-log paths on macOS, Linux, and Windows.
---

claude-code-proxy separates configuration and credentials from runtime state.

## Directory roots

| Platform | Configuration root | State root |
| --- | --- | --- |
| macOS | `~/.config/claude-code-proxy` | `${XDG_STATE_HOME:-~/.local/state}/claude-code-proxy` |
| Linux | `${XDG_CONFIG_HOME:-~/.config}/claude-code-proxy` | `${XDG_STATE_HOME:-~/.local/state}/claude-code-proxy` |
| Windows | `%APPDATA%/claude-code-proxy` | `%LOCALAPPDATA%/claude-code-proxy` |

Windows falls back to `%USERPROFILE%/AppData/Roaming` and `%USERPROFILE%/AppData/Local` when the corresponding environment variable is absent.

`CCP_CONFIG_DIR` replaces the configuration root for the current process. It does not change the state root.

## Configuration

`config.json` lives directly under the configuration root. See [Configuration](/reference/configuration/) for its schema and precedence.

## Provider credentials

On macOS, Codex and Cursor use Keychain services:

- `claude-code-proxy.codex`
- `claude-code-proxy.cursor`

Kimi and Grok use `<configuration-root>/<provider>/auth.json` on every platform. Codex and Cursor use the same file layout on Linux and Windows. File-backed credentials are written with restrictive permissions where supported.

When `CCP_CONFIG_DIR` is set, file-backed provider credentials use
`<CCP_CONFIG_DIR>/<provider>/auth.json`, including Codex and Cursor on macOS.
`CCP_CURSOR_AUTH_TOKEN` bypasses Cursor's local credential store for that
process.

OpenCode Go is the exception: it reads its API key from
`CCP_OPENCODE_API_KEY`, `OPENCODE_API_KEY`, or `opencode.apiKey` in
`config.json` and does not create a provider auth store.

Registered Claude accounts use `<configuration-root>/anthropic/accounts.json`
on every platform, with mode `0600` on Unix. The file stores account names,
tokens, and registration timestamps. The proxy loads it at startup and does not
read Claude Code's Keychain credentials. Account selection, temporary blocks,
and invalidation state are kept in memory and reset when the proxy restarts.

The proxy owns these credentials independently of native Codex, Grok, and Cursor Agent stores.

## Claude account usage snapshots

`<state-root>/anthropic/accounts/<account>.json` stores usage separately from
`proxy.log`, one file per loaded account. For example, the default macOS path
for `first` is `~/.local/state/claude-code-proxy/anthropic/accounts/first.json`.
These files contain no tokens and use mode `0600` on Unix.

At server startup, the proxy initializes files for every loaded account and sends
one small `claude-fable-5-1` request per account (`max_tokens: 1`, no streaming).
Fable responses can report all three watched windows, including `7d_oi`.
Up to three probes run concurrently, each with a ten-second timeout. The proxy
finishes these checks before accepting client requests. They consume a small
amount of allowance and can start an unused five-hour window.

Each probe updates its account's JSON and the running pool's eligibility using
the normal 98% threshold, 429 block, and 401 invalidation rules. Checks do not
advance the preferred account or generate account-selection events. A failed
check does not prevent checks of the remaining accounts or server startup.
Missing window headers remain `null`; an HTTP failure is recorded in
`lastResponseStatus`, while a connection failure leaves it `null`.

`anthropic_accounts_refresh_started` and `anthropic_accounts_refresh_completed`
mark the startup checks in `proxy.log`. Each account produces
`anthropic_account_usage_refreshed` or `anthropic_account_usage_refresh_failed`;
the actual window values and reset times are stored in the individual JSON files.
The completion event includes pool counts and `failedAccountCount`.
An HTTP error, a connection error, or a response without any watched window
headers counts as a failed refresh; unavailable windows are never reported as zero.

During normal traffic, the first account selection and each account switch
refresh all loaded accounts' snapshots, and each upstream response updates the
responding account. Previously observed windows are retained within the process.
Commands such as `models` or `kimi auth status` do not query accounts or create
or overwrite snapshots. Files are replaced atomically; a write failure produces
`anthropic_account_usage_write_failed` without failing requests or startup.
Snapshots are not read back to restore pool state after a restart: old data is
replaced with unknown values at startup, then filled from the new probes.

| Field | Meaning |
| --- | --- |
| `account` | Registered name. |
| `asOf` / `asOfUnixSecs` | Snapshot time, as UTC RFC 3339 / Unix seconds. |
| `lastResponseAt` / `lastResponseStatus` | Last upstream response observed for this account, or `null`. |
| `switchThreshold` | Usage threshold used by the running proxy, normally `0.98`. |
| `eligible` | Whether the local pool permits selection at `asOf`; this is not confirmation of remaining upstream allowance. |
| `invalid` | Whether a 401 has invalidated the account in this process. |
| `blockedUntil` / `blockedUntilUnixSecs` | Latest local block expiry, or `null`. Several windows or a 429 can extend this beyond any individual window reset. |
| `windows` | Keys `5h`, `7d`, and `7d_oi`, each containing its latest observation or `null` when unknown. |

Each known window contains `utilization` (a fraction, so `0.98` means 98%),
`observedAt` / `observedAtUnixSecs`, `resetAt` / `resetAtUnixSecs`, and `state`.
Reset times can be `null` if the upstream omitted them. Missing window headers
in a later response preserve the previous observation and its original time.

`state` is `below_threshold`, `threshold_reached`, or `reset_elapsed`, evaluated
at the snapshot's `asOf`. `reset_elapsed` means the advertised reset time has
passed; the previous utilization remains visible until a new response confirms
usage in the next window. Files do not update while the proxy is idle, so compare
reset and block times with the current time when reading an older snapshot.

Lowercase letters, digits, `-`, and `_` are used directly in filenames. Other
bytes, including uppercase letters, are percent-encoded to avoid path traversal
and collisions on case-insensitive filesystems. Very long or reserved names use
a hash filename; the `account` field retains the original name. Files for removed
accounts may remain as old snapshots; use `anthropic accounts list` to identify
current registrations.

Selection logs include `registeredAccountCount`, `loadedAccountCount`, and
`registrationReloadRequired`, distinguishing registrations added on disk from
the running pool. Registration count and reload status are `null` if the file
cannot be read. Selection, block, and invalidation logs also include
`eligibleAccountCount`, `blockedAccountCount`, `invalidAccountCount`, and
`allAccountsUnavailable`. These counts describe the loaded pool; unknown usage
does not by itself make an account unavailable.

## Kimi device ID

Kimi stores a persistent UUID at `<configuration-root>/kimi/device_id` for file-backed setups. It is bound into the Kimi token and must remain paired with that login.

## Structured log

`proxy.log` lives under the state root. It uses JSON Lines and rotates at 20 MiB. Known credential keys, including authorization, access tokens, refresh tokens, ID tokens, and account headers, are redacted before writing.

A Homebrew service also writes `service.log` under the state root.

## Failed responses

`errors/` under the state root contains JSON files for failed proxy responses. A `request_failed` log event includes `errorFile`, which points to the complete redacted payload.

Error payloads are safer to share than raw traffic captures, but inspect their prompt-derived content and paths before publishing them.

## Traffic captures

Set `CCP_TRAFFIC_LOG=1` to create captures under `traffic/` in the state root. Requests are grouped by Claude Code session and request sequence. A request directory can include:

- inbound Anthropic request and metadata
- translated upstream request
- upstream URL and redacted headers
- raw or decoded upstream events
- translated downstream events and response
- transport or reducer error details

Event filenames use monotonic sequence numbers so lexical order preserves emission order.

<div class="security-callout">
<strong>Traffic captures preserve content.</strong> Header and token redaction does not remove prompts, source code, tool definitions, tool inputs, tool results, images, or provider output. Treat the entire directory as sensitive user data.
</div>

Enable capture only for a focused reproduction, keep the directory local, and delete it after the investigation.
