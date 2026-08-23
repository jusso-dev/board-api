# board-api

![Board API banner](docs/board-api-banner.png)

`board-api` turns GitHub issues into a small kanban and runs coding-agent jobs on a homelab Linux guest. It is the trusted bridge between the native Board iOS app, GitHub CLI, local repositories, and the `grok`, `codex`, and `cursor-agent` command-line harnesses.

The service is one statically linked Rust binary. It does not use Docker, Node, a database, a cloud SDK, or model HTTP APIs. The phone receives a revocable `board_` token, never a GitHub PAT or vendor credential.

## What it is for

Use Board API when you want to:

- treat open GitHub issues as cards in `backlog`, `ready`, `running`, `review`, and `done`;
- browse repositories owned by the signed-in GitHub account or available through collaboration and organisation membership;
- create and move cards without giving a phone direct GitHub credentials;
- start a signed-in coding CLI against one issue and follow its output over server-sent events;
- keep source checkouts, GitHub authentication, and vendor authentication on your own Ubuntu guest.

The end-to-end path is:

1. The iOS app requests repositories and cards from this API.
2. The API runs authenticated `gh` commands as the Linux user `board`.
3. Starting a job creates an isolated worktree and branch for the issue.
4. The selected local harness runs inside that worktree.
5. The runner records status, requires a pull request for success, and moves the issue to review.

When automatic running is enabled, a narrow background scan also watches for open `board:ready` issues and starts them. This is not a GitHub webhook receiver or a general issue indexer. GitHub remains the source of truth.

## Service contract

- Rust 2021 with `axum`, `tokio`, `serde`, and `tower-http`.
- JSON only, camelCase fields, and ISO-8601 timestamps.
- Open route: `GET /v1/health`.
- One-time linking route: `POST /v1/pair`.
- Bearer authentication on every other route.
- One running job per repository, with `409 Conflict` for a second run.
- Optional automatic pickup of `board:ready` issues from repositories the `board` GitHub identity can push to.
- Job logs and status stream through server-sent events.
- Default bind: `0.0.0.0:8787` on LAN and Tailscale.
- Full machine-readable contract: [`openapi.yaml`](openapi.yaml).

No CORS layer is installed because the supported client is native iOS.

## Requirements

- An Ubuntu Linux guest, x86_64 by default or ARM64 when that is the guest architecture.
- A dedicated user named `board` with home `/home/board`.
- Rust and Cargo to build from source.
- Ubuntu's static C runtime development files, normally supplied by `libc6-dev`.
- `gh` installed and authenticated as `board`.
- At least one allowed harness installed and authenticated as `board`: `grok`, `codex`, or `cursor-agent`.
- Git on `PATH`.
- Tailscale for remote private access, optional for LAN-only use.

The service still starts when GitHub, Tailscale, or a harness is not signed in. The unavailable operation then returns an actionable error.

## Build from source

Run the quality checks first:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

On an x86_64 Ubuntu guest, use a target-scoped Rust flag so target dependencies are static while host-side procedural macros remain loadable:

```bash
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release --locked --target x86_64-unknown-linux-gnu

file target/x86_64-unknown-linux-gnu/release/board-api
ldd target/x86_64-unknown-linux-gnu/release/board-api
```

`ldd` should report `statically linked`. Stop if it lists shared libraries.

For an ARM64 guest:

```bash
rustup target add aarch64-unknown-linux-gnu
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release --locked --target aarch64-unknown-linux-gnu
```

The x86_64 GNU target is installed with Rust on an x86_64 host; add it with `rustup target add x86_64-unknown-linux-gnu` if needed. No separate runtime or shared Rust library is required after the release binary has been installed.

## Install on the `board` guest

Clone the repository into `/home/board/board-api`, then run the following as an administrator. Substitute the ARM64 target directory when that is the guest architecture.

```bash
sudo install -d -o board -g board -m 0700 \
  /home/board/.local/bin \
  /home/board/.config/board-api \
  /home/board/state/jobs \
  /home/board/work

sudo install -o board -g board -m 0755 \
  target/x86_64-unknown-linux-gnu/release/board-api \
  /home/board/.local/bin/board-api

sudo install -o board -g board -m 0600 \
  deploy/config.json \
  /home/board/.config/board-api/config.json

sudo install -o board -g board -m 0600 \
  deploy/HOST.md \
  /home/board/HOST.md

sudo install -m 0644 deploy/board-api.service \
  /etc/systemd/system/board-api.service

sudo systemctl daemon-reload
sudo systemctl enable --now board-api
```

The supplied service runs as `board`, restarts on failure, writes logs to journald, and can write only beneath `/home/board`. The default configuration is:

```json
{
  "listen": "0.0.0.0",
  "port": 8787,
  "stateDir": "/home/board/state",
  "workDir": "/home/board/work",
  "allowedHarnesses": ["grok", "codex", "cursor"],
  "allowedIssueAuthors": ["your-github-login"],
  "autoRun": {
    "enabled": true,
    "pollSeconds": 60,
    "defaultHarness": "codex"
  }
}
```

The configuration must remain mode `0600`. Replace `your-github-login` before installation. `allowedIssueAuthors` is required and must contain at least one GitHub login. It is matched case-insensitively against GitHub's issue author field and applies to automatic pickup and explicit `POST /v1/jobs`. A missing, deleted, or different author fails closed. `autoRun.pollSeconds` accepts 30 to 3600 seconds, and `autoRun.defaultHarness` must also appear in `allowedHarnesses`. Omit `autoRun` to keep automation disabled. `BOARD_API_CONFIG` and `BOARD_API_HOST_DOCUMENT` can override the default paths for development or tests.

## Authenticate the host tools

Authentication belongs to the service account, not root and not the phone:

```bash
sudo -iu board gh auth login
sudo -iu board gh auth status

sudo -iu board sh -lc 'command -v grok'
sudo -iu board sh -lc 'command -v codex'
sudo -iu board sh -lc 'command -v cursor-agent'
```

Run each vendor CLI's own interactive sign-in as `board`, then confirm it can start from a normal `sudo -iu board` shell. Do not put GitHub or vendor tokens in `config.json`, systemd environment variables, the repository, or the mobile app.

## Pair the first client

When no API keys exist, startup creates one eight-character pair code valid for 15 minutes. It writes the code and a terminal QR to journald and mode-`0600` `/home/board/HOST.md`.

```bash
sudo journalctl -u board-api -n 100 --no-pager
sudo less /home/board/HOST.md
```

The QR contains only the one-time code. A phone camera can read it as text, then the code can be entered in Board. It is deliberately not exposed through an unauthenticated HTTP route.

Exchange the code once:

```bash
curl -fsS -X POST http://127.0.0.1:8787/v1/pair \
  -H 'Content-Type: application/json' \
  --data '{"code":"REPLACE_ME"}'
```

The response contains a `board_` token. Only its SHA-256 hash is persisted. Authenticated clients can create or revoke additional keys through `POST /v1/keys` and `DELETE /v1/keys/{id}`.

If an unused code expires, restart the service to generate a replacement:

```bash
sudo systemctl restart board-api
```

Restarting does not create another pair code once at least one key exists. Never delete the key store merely to reveal a code, because that unlinks existing clients.

## Routes

| Purpose | Route |
| --- | --- |
| Health | `GET /v1/health` |
| First-client pairing | `POST /v1/pair` |
| Server URLs, GitHub login, and available harnesses | `GET /v1/server` |
| Mint and revoke API keys | `POST /v1/keys`, `DELETE /v1/keys/{id}` |
| Repositories across all visible owners | `GET /v1/repos` |
| All open board cards across pushable repositories | `GET /v1/overview` |
| List and create cards | `GET /v1/cards`, `POST /v1/cards` |
| Read and move a card | `GET /v1/cards/{number}`, `PATCH /v1/cards/{number}` |
| Read and add card comments | `GET /v1/cards/{number}/comments`, `POST /v1/cards/{number}/comments` |
| List, start, inspect, and cancel jobs | `GET /v1/jobs`, `POST /v1/jobs`, `GET /v1/jobs/{id}`, `POST /v1/jobs/{id}/cancel` |
| Stream job output | `GET /v1/jobs/{id}/events` |

Pagination uses `?page=&perPage=` with a maximum page size of 50. See [`openapi.yaml`](openapi.yaml) for request and response schemas.

## GitHub repositories and cards

`GET /v1/repos` calls GitHub's authenticated-user repositories endpoint with the `owner`, `collaborator`, and `organization_member` affiliations and follows every page. It therefore includes personal repositories plus repositories available from other organisations, subject to the signed-in GitHub account and token scopes.

`GET /v1/overview` is the iOS landing feed. It searches every pushable personal, collaborator, and organisation repository for open issues carrying any supported `board:*` label, attaches the repository name to every card, sorts newest updates first, and paginates at up to 50 cards per response. Search results are held in one shared 60-second snapshot, so every page is consistent and concurrent app or automation reads do not repeat GitHub searches. The phone follows all pages, so it does not need to guess which repository contains work.

A `partial: true` response means at least one GitHub owner could not be refreshed. `unavailableOwners` identifies those owners. When an older snapshot exists, cards belonging to failed owners remain visible from that snapshot while successful owners receive fresh results. A total refresh failure also serves the last snapshot instead of blanking the board. The app shows this as a compact non-blocking warning.

Cards are open GitHub issues. Their column is exactly one of these labels:

- `board:backlog`
- `board:ready`
- `board:running`
- `board:review`
- `board:done`

Moving a card changes only these labels. It does not rewrite the issue body or maintain a second task database.

Card comments are GitHub issue comments, not a second comment store. `GET /v1/cards/{number}/comments` returns them oldest first. `POST /v1/cards/{number}/comments` posts only when the server's GitHub identity is in `allowedIssueAuthors`; otherwise it returns `403 comment_author_not_allowed`. Commenting does not move the card or bypass the trusted issue-author policy.

Before a job starts, Board fetches issue comments and includes only comments whose GitHub author is in `allowedIssueAuthors`. Other users' comments stay visible in GitHub and the app but never enter the agent prompt. Board's own generated job-status comments are also omitted. The original issue author must still be allowed, so a trusted comment cannot make an untrusted issue executable.

`board:done` is only a kanban label. It does not close the GitHub issue, prove that a pull request exists, or prove that a pull request was merged.

### Does it poll GitHub?

`GET /v1/overview` uses a shared GitHub issue-search snapshot with an OR across the five board labels. The snapshot refreshes at most once per 60 seconds and is shared by overview pagination, concurrent clients, and automatic Ready pickup. This prevents the 30-searches-per-minute GitHub Search quota from being consumed once per owner for every page. Each `GET /v1/cards` remains a fresh `gh issue list` for the selected repository. The iOS app calls the overview on initial load and refresh, then calls the repository route only after the user narrows the board to one repository.

When `autoRun.enabled` is true, the server reads Ready cards from the same shared snapshot every `autoRun.pollSeconds`. Snapshot refresh scopes searches to owners returned by the authenticated `/user/repos` call, then filters every result against the exact repository set where `.permissions.push` is true. A public repository the account cannot push to is never run. Before starting work, it also requires the immutable issue author login to appear in `allowedIssueAuthors`. Adding Ready to an issue created by another account does not authorise it. An issue moved to Ready through `POST /v1/cards` or `PATCH /v1/cards/{number}` is considered immediately and updates the snapshot; an issue changed directly in GitHub is normally considered by the next refresh, subject to GitHub search indexing delay.

Harness selection is label-driven:

| GitHub labels | Harness |
| --- | --- |
| `board:ready` only | `autoRun.defaultHarness`, Codex in the supplied config |
| `board:ready` and `agent:grok` | Grok |
| `board:ready` and `agent:codex` | Codex |
| `board:ready` and `agent:cursor` | Cursor |

Use no more than one `agent:*` label. Unknown or multiple `agent:*` labels are skipped and reported in journald instead of choosing an agent arbitrarily. The API creates the three supported agent labels whenever it initialises labels for a repository.

Automatic pickup is durable and loop-safe. An active job for the same issue is not duplicated. After a terminal job, the issue must have a later GitHub `updatedAt` value before automation can select it again. A failed job can therefore return to `board:ready` without immediately retrying forever. Edit, comment on, or deliberately move the issue again to make a revised task eligible, or use `POST /v1/jobs` for an explicit retry. Explicit retries enforce the same author allowlist and return `403 issue_author_not_allowed` when the creator is not trusted.

Unlabelled open issues have no kanban column and are not displayed on the iOS board.

### Let another agent create trusted cards

Use the reusable [agent ticket prompt](docs/agent-ticket-prompt.md). Replace its `<trusted-github-login>` placeholder with the same login configured in `allowedIssueAuthors`. The agent must use an existing `gh` session whose current login matches that value; it must stop instead of requesting a PAT or changing authentication when the login differs.

Verify the identity before creating anything:

```bash
gh api user --jq .login
```

Then create an actionable issue with exactly one board column and at most one harness selector:

```bash
gh issue create \
  --repo owner/name \
  --title "Investigate failure" \
  --body-file /path/to/reviewed-issue-body.md \
  --label board:ready \
  --label agent:codex
```

Use `board:backlog` instead of Ready when the operator has not explicitly authorised immediate execution. Substitute `agent:grok` or `agent:cursor` as needed. Verify the created issue with `gh issue view <url> --json author,labels,url`; the author must match the configured trusted login. Keep credentials outside source control, prompts, issue bodies, and logs. The Board iOS app and its repository never receive that GitHub credential.

## Job lifecycle

`POST /v1/jobs` accepts one primary harness and an optional ordered crew. Crew members run sequentially. The runner:

1. rejects a second active job for the same repository with `409`;
2. creates `/home/board/work/<owner>/<repo>/<job-id>`;
3. creates branch `board/<issue>-<short-job-id>`;
4. runs the selected CLI as `board` and streams log/status events;
5. requires the branch to contain a commit ahead of the default branch and creates a pull request with `gh pr create`;
6. records the pull request URL and comments it on the issue;
7. moves success to `board:review`, or failure back to `board:ready`.

An agent process that exits successfully without changing the repository is not treated as delivered work. The job becomes `failed`, records `agent finished without repository changes`, comments that no pull request was opened, and returns the issue to Ready. This prevents a green `succeeded` state with no verifiable output.

Grok runs in its headless `auto` permission mode so it can inspect, test, and edit the isolated job worktree. It does not use `--always-approve` or `bypassPermissions`. Treat applying `board:ready` as authorisation to run repository code on the `board` guest: restrict label management to trusted maintainers, review issue text before moving it to Ready, and do not have public issue templates apply the label automatically.

At startup, 0.2.0 also reconciles legacy `succeeded` job records whose `prUrl` is null to `failed` with an unverified-delivery error. It preserves the original finish time and does not invent a pull request.

Durable job JSON lives at `/home/board/state/jobs/<id>.json`. A genuinely successful record has `status: "succeeded"` and a non-null `prUrl`. Cancelling asks the child process to stop and records the terminal state. The phone never executes a harness itself.

## LAN and Tailscale access

Direct access to port 8787 is plain HTTP because it stays on the private LAN or encrypted Tailscale network:

```bash
curl -fsS http://127.0.0.1:8787/v1/health
curl -fsS http://LAN_IP:8787/v1/health

sudo tailscale up
tailscale ip -4
tailscale status --json | jq -r '.Self.DNSName'
curl -fsS http://TAILSCALE_IP:8787/v1/health
```

The MagicDNS form is `http://board.<tailnet>.ts.net:8787`. `https://board.<tailnet>.ts.net:8787` is invalid unless a separate HTTPS listener has been configured.

If you already use Tailscale Serve, it can terminate HTTPS in front of the local HTTP service:

```bash
sudo tailscale serve --bg http://127.0.0.1:8787
```

That URL is `https://board.<tailnet>.ts.net` with no `:8787`. Serve is optional and is not the default deployment.

When UFW is enabled, allow TCP 8787 only on `tailscale0` and the actual LAN interface. Do not add a global `ufw allow 8787` rule.

## Operate and verify

```bash
sudo systemctl status board-api --no-pager
sudo journalctl -u board-api -f
sudo systemctl restart board-api

curl -fsS http://127.0.0.1:8787/v1/health
curl -i http://127.0.0.1:8787/v1/cards
```

Health should return JSON with `"ok":true`. The unauthenticated cards request should return `401 Unauthorized`.

Useful checks when data is missing:

- `sudo -iu board gh auth status` for GitHub identity and scopes;
- `sudo -iu board gh repo view owner/name` for repository visibility;
- verify the issue is open and has exactly one `board:*` column label;
- for automatic pickup, verify it has `board:ready` and at most one supported `agent:*` label;
- `sudo -iu board sh -lc 'command -v <harness>'` for service-account `PATH`;
- `sudo journalctl -u board-api -n 200 --no-pager` for job and network errors.

## Security and stored data

- Raw board tokens are returned once and stored only by the client. The server stores SHA-256 hashes.
- Pair codes are short-lived, one-use, local operator data.
- GitHub and vendor credentials remain in their own CLIs under `/home/board`.
- Logs must not contain credentials, raw board tokens, or reusable pair codes after the initial display.
- Configuration contains paths and harness allow-lists, not secrets.
- Worktrees and job records remain on the Ubuntu guest.
- Network exposure should remain limited to trusted LAN and Tailscale interfaces.

## License

UNLICENSED. No permission is granted to copy, modify, or redistribute this code beyond rights provided by applicable law.
