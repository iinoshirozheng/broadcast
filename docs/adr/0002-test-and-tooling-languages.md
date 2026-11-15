# ADR 0002: Tests and tooling are written in the language of the runtime they drive

- Status: accepted
- Date: 2026-11-15
- Amends: [ADR 0001](0001-rust-runtime-pixi-monorepo.md) decision 5

## Context

After ADR 0001 every runtime is Rust, but the code around it was still mostly Node `.mjs`.
The HTTP, CLI and README suites in `test/*.test.mjs` spawned the Rust binaries through a Node
helper, and Node scripts assembled, signed and checked the release bundles. That split had
three costs:

- A server change meant reading Rust and then writing its test in untyped JavaScript, through
  a helper that rebuilt the server's environment and SQLite rows by hand.
- The Windows server suites needed Node shims (`.cmd` wrappers, symlink workarounds) for
  things a Rust test does natively.
- The release scripts duplicated work that `tauri build` already does.

We considered three options: TypeScript for everything outside the runtime, Rust for everything,
and Python for tooling. Python would add a fourth language to the toolchain with no runtime to
justify it. Rust everywhere does not fit the code that drives a browser, the Codex renderer, or
the cloud worker under miniflare. Those run JavaScript, so their tests do too.

## Decision

1. **Tests of the Rust server and CLI are Rust integration tests in the crate they test.**
   - `crates/server/tests/*.rs` spawn the real `broadcast-server` binary
     (`CARGO_BIN_EXE_broadcast-server`) on an ephemeral loopback port, with the data directory
     and Codex home in a temporary directory.
   - The shared setup is in `crates/server/tests/support`. A `fake-codex` test binary
     (`tests/support/fake_codex.rs`) stands in for the Codex CLI.
   - `crates/cli/tests` run the `broadcast` binary against an in-process `broadcast_server`.
2. **Tests of JavaScript runtimes are TypeScript.** Node runs them directly by type
   stripping, and `tsc` checks them. These cover:
   - the Codex injector userscript (`test/inject*.test.ts`);
   - the cloud worker under miniflare (`test/cloud-*.test.ts`);
   - the web components (`apps/web/test`) and the Playwright e2e suite (`tests/`).
3. **Cross-language contracts and repository text checks also stay in TypeScript.**
   - `test/ai-chat-composer-api.test.ts` checks the web client's composer reference keys
     (`apps/web/src/composerReference.ts`) against the server's rebind API.
   - `test/launcher-release.test.ts` and `test/manage-broadcast-skill.test.ts` check
     repository files.
4. **Build and release tooling is Tauri plus `cargo xtask`.**
   - `tauri build` builds the web and the `broadcast-cli` sidecar, bundles the resources,
     signs and notarizes the App, and signs the DMG and the updater archive.
   - `cargo xtask` covers what Tauri does not: sidecar builds, release naming and ordering,
     the macOS `latest.json`, and checks of the App, DMG, updater, packaged CLI and Linux
     packages.
   - `scripts/migrate-to-cloud.ts` stays TypeScript because it drives wrangler.
5. **No `.mjs` and no Python remain in the repository.**

## Consequences

- `pixi run test` runs `cargo test` and then `pnpm test`. CI runs both on Ubuntu (through
  `pixi run check`) and on Windows (through `pixi run test`).
- The server is tested over its real HTTP boundary in the language it is written in, and a
  test can open the server's SQLite database with `rusqlite` to check rows directly.
- `fake-codex` is a `[[bin]]` of `broadcast-server` with `test = false`, and the package sets
  `default-run = "broadcast-server"`. Release bundles never include it, because:
  - the desktop app embeds the server as a library;
  - the sidecar build compiles only `broadcast-cli`.
- A test that crosses the web and the server goes in TypeScript only when it has to import
  web code. Otherwise it is a Rust integration test.
