# Scribe — repo guide for agents

Rust GPU-accelerated terminal emulator: session-owning server + GPUI client
over Unix-socket IPC. Cargo workspace of eight `crates/*` (common, pty,
server, client, cli, hook-helper, image-decode, test) pinned to Rust
1.95.0; GPUI pinned to a Zed git rev; vendored forks in `third_party/`;
packaging assets committed under `dist/`; `just` drives everything.

## Ground rules

- `constitution.md` is binding — notably: no disruptive runtime action
  without authority (a live `--upgrade`/server restart needs explicit
  approval), worktree preservation, risk-based verification.
- Task tracking is Beads: `bd ready`, `bd show <id>`, `bd close <id>`;
  `bd prime` when stale. Specs in `specs/`; learnings in `docs/solutions/`.
- Architecture/protocol/test graph in `lat.md/`; search before coding,
  update after changes, `lat check` before done.
- UI: `PRODUCT.md` + `DESIGN.md` ("Typeset Ink") govern the settings
  window ONLY; terminal chrome is out of scope. Agent CLI contract:
  `docs/agent-api.md`.

## Build, test, gates

From repo root; Rust pinned by `rust-toolchain.toml` (1.95.0 — older
rustc fails the build):

```bash
just build / just build-release / just check
just clippy   # cargo clippy --workspace --all-targets --all-features -- -D warnings (CI)
just test     # cargo test --workspace (CI, Ubuntu + macOS)
just ready    # ratchets + fmt + clippy + test — NOTE: runs mutating cargo fmt
pre-commit run --all-files   # broader than CI: gitleaks, taplo, fmt --check,
                             # cargo deny, cargo machete, staged ratchets
```

- Active hooks: `core.hooksPath=.beads/hooks` → Beads then pre-commit.
- Ratchets: `just lint-suppressions`, `just reachability`,
  `just parity-inventory` (working tree), `just parity-gate` (launch
  threshold, deliberately outside `ready`).
- Codegen: editing `.impeccable/mocks/beads-board-directions.html`
  requires `just beads-board-contract-gen` + `just beads-board-contract`
  (not in ready/CI).
- E2E (Docker): `just docker-func` / `just docker-visual` to build images,
  then `just e2e-func func/smoke.sh` etc. PR CI blocks on func/smoke,
  session-exit-status, cli-smoke, visual/titlebar; full `just e2e` is
  nightly/informational. Bench: `cargo bench -p scribe-server --bench
  agent_api`.

## Dev run

- Debian deps: `clang libfontconfig-dev libssl-dev libvulkan1
  libwayland-dev libx11-xcb-dev libxkbcommon-x11-dev libzstd-dev` + a
  Vulkan ICD (Lavapipe via mesa-vulkan-drivers works).
- `just build`, then `just server &` and `just client`. No watch mode —
  rebuild and `just restart-server` (real `--upgrade` against the live
  socket: get approval first).
- SINGLETON COLLISION: dev binaries use stable names, so they share
  config/state/sockets with an installed Scribe (config
  `~/.config/scribe/config.toml`; sockets `/run/user/$UID/scribe/`).
  Duplicate client/settings launches focus the existing process.
- E2E containers run `--network none`, bind `./test-output`, and restage
  `target/e2e-stage/<profile>`; visual tests use Xvfb + Lavapipe.
- Packaging test regressions: `just test-install`; dpkg installs ship
  hooks under `/usr/share/scribe/` — `dist/` is source, not build output.
