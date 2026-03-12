# ZeroClaw Operations Runbook

This runbook is for operators who maintain availability, security posture, and incident response.

Last verified: **February 18, 2026**.

## Scope

Use this document for day-2 operations:

- starting and supervising runtime
- health checks and diagnostics
- safe rollout and rollback
- incident triage and recovery

For first-time installation, start from [one-click-bootstrap.md](../setup-guides/one-click-bootstrap.md).

## Runtime Modes

| Mode | Command | When to use |
|---|---|---|
| Foreground runtime | `zeroclaw daemon` | local debugging, short-lived sessions |
| Foreground gateway only | `zeroclaw gateway` | webhook endpoint testing |
| User service | `zeroclaw service install && zeroclaw service start` | persistent operator-managed runtime |

## Baseline Operator Checklist

1. Validate configuration:

```bash
zeroclaw status
```

2. Verify diagnostics:

```bash
zeroclaw doctor
zeroclaw channel doctor
```

3. Start runtime:

```bash
zeroclaw daemon
```

4. For persistent user session service:

```bash
zeroclaw service install
zeroclaw service start
zeroclaw service status
```

## Health and State Signals

| Signal | Command / File | Expected |
|---|---|---|
| Config validity | `zeroclaw doctor` | no critical errors |
| Channel connectivity | `zeroclaw channel doctor` | configured channels healthy |
| Runtime summary | `zeroclaw status` | expected provider/model/channels |
| Daemon heartbeat/state | `~/.zeroclaw/daemon_state.json` | file updates periodically |

## Logs and Diagnostics

### macOS / Windows (service wrapper logs)

- `~/.zeroclaw/logs/daemon.stdout.log`
- `~/.zeroclaw/logs/daemon.stderr.log`

### Linux (systemd user service)

```bash
journalctl --user -u zeroclaw.service -f
```

## Incident Triage Flow (Fast Path)

1. Snapshot system state:

```bash
zeroclaw status
zeroclaw doctor
zeroclaw channel doctor
```

2. Check service state:

```bash
zeroclaw service status
```

3. If service is unhealthy, restart cleanly:

```bash
zeroclaw service stop
zeroclaw service start
```

4. If channels still fail, verify allowlists and credentials in `~/.zeroclaw/config.toml`.

5. If gateway is involved, verify bind/auth settings (`[gateway]`) and local reachability.

## Safe Change Procedure

Before applying config changes:

1. backup `~/.zeroclaw/config.toml`
2. apply one logical change at a time
3. run `zeroclaw doctor`
4. restart daemon/service
5. verify with `status` + `channel doctor`

## Targeted Tool Rollouts

Use the following feature gates when rolling out the new direct-content and asynchronous tooling:

| Feature | Use when | Avoid when | Fast rollback |
|---|---|---|---|
| `url_prefetch` | users paste gist/raw text URLs and expect “read this first” behavior | generic website browsing, large payloads, untrusted hosts | set `[url_prefetch].enabled = false` and restart |
| `apply_patch` | the model needs one structured multi-file code edit | binary files, fuzzy search/replace, or broad repo surgery | keep it inside CLI only, or restore prior binary if channel exposure is unsafe |
| `process` | long-running shell work needs polling, logs, or stdin | one-shot commands that `shell` can finish immediately | keep `process` in `non_cli_excluded_tools`, restart runtime |
| `child_session` | bounded delegated work should continue asynchronously and hand back a summary | persistent DAG/session orchestration or unconstrained background workers | keep `child_session` in `non_cli_excluded_tools`, restart runtime |

Operator guidance:

- For public messaging channels, leave `apply_patch`, `process`, and `child_session` excluded until you have validated them in CLI or a low-traffic private channel.
- Prefer config-only rollback first: disable `url_prefetch`, and make sure `non_cli_excluded_tools` includes `apply_patch`, `process`, and `child_session`.
- If the feature still misbehaves after config rollback, restore the previous binary and restart the service.

## lt-server-Class Smoke Pass

This is the recommended release-style smoke sequence for servers running a `full` autonomy profile with explicit `allowed_commands`, extra `allowed_roots`, and `url_prefetch` enabled for gist/raw hosts.

1. Confirm target config shape before rollout:
   - `[autonomy].level = "full"`
   - `[autonomy].workspace_only = false`
   - `[autonomy].non_cli_excluded_tools` still protects `apply_patch`, `process`, and `child_session` on public channels
   - `[url_prefetch]` only allows trusted direct-content domains
2. Validate the release binary locally:

```bash
cargo build --release
CARGO_TARGET_DIR=/tmp/zeroclaw-target cargo test --release process_channel_message_prefetch_context_drives_final_answer --lib
CARGO_TARGET_DIR=/tmp/zeroclaw-target cargo test --release process_channel_message_runs_apply_patch_workflow_end_to_end --lib
CARGO_TARGET_DIR=/tmp/zeroclaw-target cargo test --release process_channel_message_runs_background_process_workflow_end_to_end --lib
CARGO_TARGET_DIR=/tmp/zeroclaw-target cargo test --release process_channel_message_runs_child_session_handoff_end_to_end --lib
```

3. On the target host, verify one prompt per feature in a low-risk conversation:
   - gist/raw URL inspection with broad phrasing
   - one realistic `apply_patch` edit in CLI
   - one background `process` start/poll/kill sequence
   - one bounded `child_session` spawn/wait flow
4. Watch service logs during the smoke pass and capture the first failure verbatim before retrying.

## Rollback Procedure

If a rollout regresses behavior:

1. disable `[url_prefetch]` or restore its previous host allowlist
2. restore `[autonomy].non_cli_excluded_tools` so `apply_patch`, `process`, and `child_session` are hidden from non-CLI channels
3. restart runtime (`daemon` or `service`)
4. confirm recovery via `doctor` and channel health checks
5. if needed, restore previous `config.toml` and previously known-good binary
6. document incident root cause and mitigation

## Related Docs

- [one-click-bootstrap.md](../setup-guides/one-click-bootstrap.md)
- [troubleshooting.md](./troubleshooting.md)
- [config-reference.md](../reference/api/config-reference.md)
- [commands-reference.md](../reference/cli/commands-reference.md)
