# board host notes

Copy this template to `/home/board/HOST.md`, set mode 0600, and replace values marked `<...>`. `board-api` appends and maintains its current pairing block.

## VM

- Hypervisor guest: `board`
- LAN IPv4: `<lan-ip>`
- CPU: 4 vCPU
- Memory: 8192 MiB
- Disk: 100 GB virtio (`/dev/vda`)
- NIC: virtio bridge; guest interface `<lan-interface>`
- Guest agent: active

## API

- LAN URL: `http://<lan-ip>:8787`
- Tailscale URL: pending `sudo tailscale up`
- MagicDNS URL: pending Tailscale authentication
- Health: `curl -fsS http://127.0.0.1:8787/v1/health`
- Version: `/home/board/.local/bin/board-api --version`
- All work: authenticated `GET /v1/overview?page=1&perPage=50`
- Binary: `/home/board/.local/bin/board-api`
- Source: `/home/board/board-api`
- Config: `/home/board/.config/board-api/config.json` (mode 0600)
- State: `/home/board/state`
- Jobs: `/home/board/state/jobs/<id>.json`
- Workspaces: `/home/board/work/<owner>/<repo>/<job-id>`
- Automatic pickup: open `board:ready` issues, every 60 seconds and immediately for API moves
- Agent selectors: `agent:grok`, `agent:codex`, `agent:cursor`; no selector uses Codex
- Effort selectors: `effort:low`, `effort:medium`, `effort:high`, `effort:xhigh`; no selector keeps the harness default
- Allowed issue authors: `<github-login>`; missing or different GitHub authors cannot start jobs

The iOS app opens on All work. The overview contains every open issue with a supported `board:*` label from repositories this GitHub identity can push to, including organisation and collaborator repositories. Overview pages and automatic pickup share one 60-second GitHub snapshot. A `partial: true` response lists failed owners in `unavailableOwners` and retains their older cached cards when possible.

Pair code and terminal QR appear in `journalctl -u board-api` once and in the Current pairing section below. QR contains only the one-time code. Exchange it with `POST /v1/pair`. Use authenticated `POST /v1/keys` to mint another phone token; raw tokens are never stored.

A successful job must contain a non-null `prUrl`. If an agent produces no repository change, the job fails, comments that no pull request was opened, and returns the issue to `board:ready` without an automatic retry loop.

Both automatic pickup and authenticated `POST /v1/jobs` check the immutable GitHub issue author against `allowedIssueAuthors`. Relabelling an issue from another author does not authorise it.

## Harnesses

- Codex: `/home/board/.local/bin/codex` (`<authenticated-or-login-owed>`)
- Grok: `/home/board/.local/bin/grok` (`<authenticated-or-login-owed>`)
- Cursor: `/home/board/.local/bin/cursor-agent` (`<authenticated-or-login-owed>`)
- GitHub CLI: `/usr/bin/gh` (`<authenticated-or-login-owed>`)

## Login debt

Run as `board` unless command contains `sudo`:

```bash
gh auth login
grok login
cursor-agent login
sudo tailscale up
```

After Tailscale authentication:

```bash
tailscale ip -4
tailscale status --json | jq -r '.Self.DNSName'
```

## Credential directories

- Record names and locations only. Never record credential contents.
