# board-api

Small Rust HTTP API for a native client that treats GitHub issues as board cards and runs coding jobs through installed vendor CLIs.

`board-api` never calls model HTTP APIs. It executes `gh`, `grok`, `codex`, or `cursor-agent` as its Linux service user. The phone receives a board token, never a GitHub PAT or vendor credential.

## API

- JSON only, camelCase fields and ISO-8601 timestamps.
- Open health route: `GET /v1/health`.
- One-time pairing: `POST /v1/pair`.
- Bearer authentication on every other route.
- GitHub issues are cards; `board:*` labels are columns.
- One running job per repository.
- Job logs and status stream through server-sent events.
- Full contract: [`openapi.yaml`](openapi.yaml).

Default bind is `0.0.0.0:8787`. No CORS layer is installed.

## Requirements

- Linux x86_64, or adjust the Rust target for the guest architecture.
- Rust with Cargo.
- `gh` installed and authenticated as the service user.
- Any enabled harness installed on `PATH`: `grok`, `codex`, or `cursor-agent`.
- Tailscale is optional.

No Docker, Node, Go, cloud SDK, database, or extra runtime is required.

## Build and test

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Build the requested static GNU/Linux release binary:

```bash
rustup target add x86_64-unknown-linux-gnu
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release --locked --target x86_64-unknown-linux-gnu
```

For an ARM64 guest, use its matching Linux GNU target.

## Install

These deployment files assume a dedicated Linux user named `board` with home `/home/board`.

```bash
install -d -o board -g board -m 0700 \
  /home/board/.local/bin /home/board/.config/board-api \
  /home/board/state /home/board/state/jobs /home/board/work

install -o board -g board -m 0755 \
  target/x86_64-unknown-linux-gnu/release/board-api \
  /home/board/.local/bin/board-api

install -o board -g board -m 0600 deploy/config.json \
  /home/board/.config/board-api/config.json

install -o board -g board -m 0600 deploy/HOST.md /home/board/HOST.md
install -m 0644 deploy/board-api.service \
  /etc/systemd/system/board-api.service

systemctl daemon-reload
systemctl enable --now board-api
curl -fsS http://127.0.0.1:8787/v1/health
```

Review placeholders in `/home/board/HOST.md` before starting the service. Root owns the installed systemd unit.

## Pairing and keys

On first boot with no API keys, the service creates an eight-character pairing code valid for 15 minutes. It records the code once in journald and in mode-0600 `/home/board/HOST.md`.

```bash
journalctl -u board-api
```

Exchange the code once through `POST /v1/pair`. The response contains a `board_` token. Only its SHA-256 hash is persisted. Authenticated clients can mint or revoke more keys through `POST /v1/keys` and `DELETE /v1/keys/{id}`.

If the first code expires before use, restart the service to mint another:

```bash
systemctl restart board-api
```

Never commit or log raw board tokens, pair codes, GitHub credentials, or vendor credentials.

## GitHub and jobs

Authenticate GitHub as the service user:

```bash
gh auth login
```

Jobs clone into `/home/board/work/<owner>/<repo>/<job-id>`, use branch `board/<issue>-<short-job-id>`, run the selected harness or sequential crew, create a pull request when needed, and move the issue to review. Job state is stored under `/home/board/state/jobs`.

## Tailscale

When Tailscale is installed but unauthenticated:

```bash
sudo tailscale up
tailscale ip -4
tailscale status --json | jq -r '.Self.DNSName'
```

`GET /v1/server` discovers LAN and Tailscale addresses at request time.

## License

UNLICENSED. No permission is granted to copy, modify, or redistribute this code beyond rights provided by applicable law.
