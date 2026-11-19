[English](README.md) | [简体中文](README.zh-CN.md)

# Broadcast

An issue board where people and their agents share the work. It runs in the browser; the same HTTP API powers the React UI, the `broadcast` CLI used by the bundled Skill, and the agent runners that work delegated issues with Claude Code or Codex on your own machine.

![Broadcast product screenshot](docs/assets/broadcast.png)

## Requirements

- [pixi](https://pixi.prefix.dev) 0.76 or newer. `pixi.toml` pins Rust 1.100, Node.js 26 and pnpm 12, and `pixi run` installs them on first use. No rustup, nvm or global npm packages are needed.

## Run locally

```bash
pixi run start
```

This builds the web UI and runs the Rust server. Open <http://127.0.0.1:47823>. The SQLite database is stored at `.data/broadcast.sqlite`.

For development with live frontend reload:

```bash
pixi run dev
```

The Vite UI runs at <http://127.0.0.1:5173> and proxies API requests to the Rust server on <http://127.0.0.1:47823>.

## Use the CLI

Build it and run it from the project:

```bash
pixi run build-native

target/debug/broadcast project create \
  --id my-project \
  --name "My project" \
  --workspace-path /absolute/path/to/repository

target/debug/broadcast issue create \
  --project my-project \
  --title "Implement the next slice" \
  --status todo \
  --priority high \
  --labels product,mvp
```

Run `pixi run cargo install --locked --path crates/cli` if you want `broadcast` on your shell path (`~/.cargo/bin`). The CLI talks to `http://127.0.0.1:47823`; set `BROADCAST_URL` to point it at another service.

## Install the Skill

Copy or symlink `skills/manage-broadcast` into your agent's skills directory (`~/.claude/skills` for Claude Code, `~/.agents/skills` for Codex), then start a new conversation:

```bash
ln -s /absolute/path/to/broadcast/skills/manage-broadcast \
  ~/.claude/skills/manage-broadcast
```

The Skill teaches the agent to inspect an issue, move it to `in_progress`, use optimistic versions, verify the work, and then move it to `in_review`; it moves the issue to `done` only after the user explicitly confirms acceptance or asks to mark it complete. The issue detail's “Copy agent instruction” button copies a prompt that starts this workflow on that issue.

## Delegate to agents

Assign an issue to Claude Agent or Codex Agent and set it to `todo`, or mention `@claude` or `@codex` in a comment. A runner on your machine picks it up:

```bash
broadcast runner claude --cwd /absolute/path/to/repository
broadcast runner codex --cwd /absolute/path/to/repository
```

The runner claims the issue, runs `claude -p` or `codex exec` in that directory with the agent CLI's own sign-in, posts the answer as a comment, and moves the issue to `in_review` (or `blocked` when the agent fails). A later mention resumes the same session. Broadcast never receives your agent credentials. The issue detail shows when the runner an issue waits for is offline.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `BROADCAST_HOST` | `0.0.0.0` | HTTP bind address; use `127.0.0.1` to disable LAN access |
| `BROADCAST_PORT` | `47823` | Local HTTP port |
| `BROADCAST_TRUSTED_ORIGINS` | unset | Comma-separated exact HTTPS origins allowed through a loopback reverse tunnel |
| `BROADCAST_DATA_DIR` | `.data` | SQLite data directory |
| `BROADCAST_URL` | `http://127.0.0.1:47823` | CLI API origin |

The server prints both the local URL and the available LAN URLs. Teammates on the same trusted network can open one of those LAN URLs and use the same broadcast service. Task, comment, and attachment changes are broadcast to every open client through server-sent events; reconnecting clients perform a full refresh so changes made while disconnected are not missed. A teammate using `broadcast` can point it at the shared service with `BROADCAST_URL=http://<host-ip>:47823`.

LAN mode has no account authentication: anyone on the trusted local network who can reach the URL can read and write the board. Do not expose it to the public internet without an authenticated boundary in front of it.

For a reverse tunnel that connects to the local listener, set `BROADCAST_TRUSTED_ORIGINS` to the tunnel's public HTTPS origin, for example `https://board.example.test`. Multiple origins are comma-separated. The variable cannot be empty, and duplicate origins (including normalized forms such as a trailing slash or default HTTPS port) are rejected at startup. Entries must otherwise be exact HTTPS origins; paths, queries, fragments, credentials, and wildcards are rejected. A reverse proxy or tunnel can preserve that public `Host`: Broadcast derives its canonical HTTPS origin and requires an exact configured match. If the browser supplies an `Origin`, that header is validated independently; the proxy must preserve it rather than fabricate one. Forwarded headers are not used for either decision. Configured public hosts and trusted origins can use ordinary Broadcast HTTP and realtime endpoints, but device-local capability routes remain unavailable even though the tunnel socket is loopback. Requests using only direct local or private-LAN hosts and origins keep their existing behavior.

## Verify

```bash
pixi run check
```

This runs Clippy, TypeScript checking and Biome, then builds the server, CLI and web UI and runs the Rust tests and the web suites against those builds. `pixi run test-e2e` runs the Playwright suite and `pixi run fmt` formats Rust and JavaScript.

## Task Markdown

Task descriptions and comments support GFM, including tables and task lists. Fenced `mermaid` blocks are rendered as read-only diagrams after the viewer loads; the diagram source remains available when rendering fails. Markdown HTML comments, such as `<!-- trace-analysis:v1 ... -->`, are hidden from the rendered document. Raw HTML is not enabled.

## Acknowledgements

Thanks to [Lingshan21](https://github.com/Lingshan21) for:

- The parent-based project completion proposal and initial implementation in [#371](https://github.com/iinoshirozheng/broadcast/pull/371), which weighted top-level parent issues equally regardless of how many children each had.
- The priority organization proposal and initial implementation in [#372](https://github.com/iinoshirozheng/broadcast/pull/372). The final board sorting controls were adapted to the maintainer's requirements instead of adopting the proposed priority swimlanes.
