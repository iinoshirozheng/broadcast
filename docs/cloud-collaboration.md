# Cloud collaboration

Broadcast can run as a small shared Cloudflare deployment for two trusted collaborators:

- one Worker serves the built UI and the JSON API;
- D1 is the authoritative business database;
- a private R2 bucket stores attachments;
- one SQLite-backed Durable Object broadcasts revision changes over hibernating WebSockets;
- UI, API, and attachment routes use HTTPS Basic Authentication; `/health` is public;
- open boards refresh when a revision event arrives; reconnects perform one revision check and never poll periodically.

The production resource names are:

| Resource | Name |
| --- | --- |
| Worker | `board` |
| D1 database | `broadcast-db` |
| R2 bucket | `broadcast-attachments` |
| Durable Object class | `RealtimeHub` |

This is intentionally a shared-password trust model. The Basic username is only the actor name displayed in task and comment attribution, not a verified identity. Anyone who knows the shared password has full read and write access and can choose any actor name. Use it only with the other trusted collaborator.

## What stays local

The cloud stores project, issue, comment, relation, and attachment data. It does not store a device's absolute project or worktree paths.

Each collaborator runs the **local companion**: a **device-local loopback service** (not a chat persona) for Codex, Git/worktree scanning, installed Skill/MCP discovery, and project path mapping. The companion keeps the cloud URL, actor name, shared password, and device-specific project mappings in `.data/cloud-companion.json` with mode `0600`. Ordinary Broadcast HTTP routes (tasks, comments, attachments) are the shared API; they are not a separate “companion API”.

When cloud mode is active, the cloud is the only business-data source. A failed cloud request fails visibly. The companion does not fall back to the local SQLite database and does not write to both databases. `broadcast cloud logout` returns that device to its separate local mode; it does not merge local and cloud data.

## Owner: validate locally

Install the JavaScript dependencies:

```bash
pixi run install-js
```

Create an ignored `.dev.vars` file containing a local-only value for `BROADCAST_SHARED_SECRET`, apply the D1 migration to Wrangler's local state, and start the Worker. Wrangler builds the Rust Worker with `pixi run -e cloud build-cloud`:

```bash
pixi run pnpm cloud:migrate:local
pixi run pnpm dev:cloud
```

Open the printed loopback URL. The browser shows its native Basic Authentication prompt. Enter any local actor name as the username and the value from `.dev.vars` as the password.

Local Wrangler state lives under `.wrangler/` and is not committed.

## Owner: deploy

Authenticate Wrangler first:

```bash
pixi run pnpm exec wrangler login
pixi run pnpm exec wrangler whoami
```

Provision the production D1 database and private R2 bucket using the exact names above.

```bash
pixi run pnpm exec wrangler d1 create broadcast-db
pixi run pnpm exec wrangler r2 bucket create broadcast-attachments
```

`wrangler.jsonc` contains one production configuration and identifies the D1 binding by its resource name and `database_id`. A D1 database ID is public metadata and does not grant access, so it can be committed. Wrangler local development creates persistent local equivalents under `.wrangler/`; those are local simulations, not additional Cloudflare environments.

Apply the remote D1 migration and validate the deployment bundle:

```bash
pixi run pnpm cloud:migrate
pixi run pnpm cloud:deploy:dry-run
```

Set the shared password through Wrangler's private interactive prompt after the database schema is ready. Do not put the value in `wrangler.jsonc`, a shell command, a log, or a committed file. Then deploy the production Worker:

```bash
pixi run pnpm exec wrangler secret put BROADCAST_SHARED_SECRET
pixi run pnpm cloud:deploy
```

These commands create or update Cloudflare resources. This repository contains the production D1 database ID for the binding, but it does not contain the shared password or any API or OAuth token. Keep those credentials out of Git; cloning the repository does not grant access or mean the Worker has already been deployed.

Give the other collaborator the deployed Worker HTTPS origin and shared password through a trusted channel. Never publish the password in the repository, an issue, or logs.

Current Cloudflare references:

- [Workers Static Assets binding](https://developers.cloudflare.com/workers/static-assets/binding/)
- [Durable Objects with WebSocket Hibernation](https://developers.cloudflare.com/durable-objects/best-practices/websockets/)
- [D1 migrations](https://developers.cloudflare.com/d1/reference/migrations/)
- [Create an R2 bucket](https://developers.cloudflare.com/r2/buckets/create-buckets/)
- [Workers secrets](https://developers.cloudflare.com/workers/configuration/secrets/)

## Friend: connect an existing GitHub installation

The owner follows this device setup too, using the owner's own actor name and checkout path. The friend does not need your local database or your filesystem paths. Open the Broadcast desktop App. It runs the loopback companion, embeds the board in Codex, and on macOS links its `broadcast` CLI into `/opt/homebrew/bin`. Without the App, run the companion from an up-to-date clone instead; it serves the board in a browser only:

```bash
git pull --ff-only
pixi run build-native
BROADCAST_HOST=127.0.0.1 pixi run start
```

and use `target/debug/broadcast` in place of `broadcast` below.

Then configure cloud mode. Use the deployed HTTPS Worker origin, choose the actor name that should appear on their actions, and enter the shared password only at the private `Shared key:` prompt:

```bash
broadcast cloud login \
  --url https://YOUR-WORKER-ORIGIN \
  --actor-name "FRIEND-DISPLAY-NAME"

broadcast cloud status
broadcast project list
```

The shared password is not part of the command and is not echoed by the prompt.

For every cloud project used with Codex, map its project ID to that friend's own absolute checkout path:

```bash
broadcast project map PROJECT_ID \
  --workspace-path /absolute/path/on/their/device
```

The owner runs the same mapping command with the owner's own path. Mappings are intentionally different on each device and are never synchronized to D1.

Keep the companion running while using the board. The companion supplies local Codex/Git/Skill/MCP capabilities and sends the shared password to the Worker only in the HTTPS Basic `Authorization` header. It does not write that password to D1 or R2, return it to the browser UI, or print it in logs. Device paths also stay off Cloudflare.

Do not point `BROADCAST_URL` directly at the cloud origin for this workflow. `broadcast` talks to the loopback companion, which applies Basic Authentication and the device's local project mapping. If the companion uses a non-default loopback port, set `BROADCAST_COMPANION_URL` to that loopback origin.

## Browser-only access

Either collaborator can open the deployed HTTPS Worker URL directly. The browser's native Basic Authentication prompt asks for:

- username: the actor display name for that browser;
- password: the shared password.

The browser view supports the shared board and attachments. Device-only Codex, Git/worktree, Skill, and MCP capabilities still require the local companion.

## Rotate or revoke the shared password

The owner rotates the Worker secret using Wrangler's interactive prompt:

```bash
pixi run pnpm exec wrangler secret put BROADCAST_SHARED_SECRET
```

After rotation, both devices rerun `broadcast cloud login` and enter the new password. Browser-only users must authenticate again; closing the authenticated browser session or clearing site authentication may be necessary because browsers cache Basic credentials.

Because both collaborators share one password, rotation affects both at once. There is no individual-user revocation in this two-person trust model.

## Advanced: one-time import of existing local data

The migration tool takes a consistent SQLite snapshot with `VACUUM INTO`, removes structured device-only paths, exports attachment hashes, and writes a private bundle. The default local paths are:

```bash
pixi run pnpm cloud:data export \
  --database .data/broadcast.sqlite \
  --attachments .data/attachments \
  --output cloud-migration-exports/initial
```

The output directory contains issue content and attachment bytes. It is mode-restricted and ignored by Git, but it must still be handled as private data. This export is optional when starting with an empty cloud board.

Before importing, authenticate Wrangler, provision the named D1 and R2 resources, and run `pixi run pnpm cloud:migrate` so the remote D1 schema exists. The target D1 must contain no projects, and none of the bundle's attachment keys may already exist in R2. Import refuses a non-empty target instead of merging or overwriting it.

Run the one-time Wrangler adapter with an explicit remote-operation acknowledgement:

```bash
BROADCAST_MIGRATION_REMOTE=1 pixi run pnpm cloud:data import \
  --bundle cloud-migration-exports/initial \
  --adapter ./scripts/wrangler-cloud-adapter.mjs

BROADCAST_MIGRATION_REMOTE=1 pixi run pnpm cloud:data verify \
  --bundle cloud-migration-exports/initial \
  --adapter ./scripts/wrangler-cloud-adapter.mjs
```

`BROADCAST_MIGRATION_REMOTE=1` is a deliberate safety gate for these two commands. The adapter uses the current Wrangler login and the production resource names from `wrangler.jsonc`; it does not add a migration HTTP endpoint or store Cloudflare credentials. The commands are not run automatically by deployment, so having the repository does not mean data has already been imported.

The adapter has a local-persistence integration test that does not access remote Cloudflare resources:

```bash
node --test test/cloud-migration.test.mjs
```
