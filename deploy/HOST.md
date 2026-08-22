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
- Binary: `/home/board/.local/bin/board-api`
- Source: `/home/board/board-api`
- Config: `/home/board/.config/board-api/config.json` (mode 0600)
- State: `/home/board/state`
- Jobs: `/home/board/state/jobs/<id>.json`
- Workspaces: `/home/board/work/<owner>/<repo>/<job-id>`

Pair code and terminal QR appear in `journalctl -u board-api` once and in the Current pairing section below. QR contains only the one-time code. Exchange it with `POST /v1/pair`. Use authenticated `POST /v1/keys` to mint another phone token; raw tokens are never stored.

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
