# Before starting work

- Run `lat search` to find sections relevant to your task. Read them to understand the design intent before writing code.
- Run `lat expand` on user prompts to expand any `[[refs]]` — this resolves section names to file locations and provides context.
- Worktree directory preference: `~/.config/superpowers/worktrees/scribe/`
- NEVER restart the Scribe server without first asking the user and receiving explicit approval. This includes `just restart-server`, `just restart-server-release`, and direct `scribe-server --upgrade` invocations; restarting is extremely disruptive to active work.

## Validation and testing: Docker sandbox ONLY (HARD RULE)

- ALL validation, testing, debugging, and experimentation against
  `scribe-server`, `scribe-client`, or `scribe-test` MUST run inside the
  Docker E2E harness (`docker/Dockerfile.func`, `docker/Dockerfile.visual`)
  — NEVER against this machine's live install. The developer works inside
  Scribe all day; touching the host server disrupts active work.
- Enter ONLY through the just recipes: `just docker-func` /
  `just docker-visual` build the images, then `just e2e-func <script>`,
  `just e2e-visual <script>`, `just e2e`, or the purpose-built `e2e-*`
  recipes run them. Test scripts live under `tests/e2e/`; logs and
  screenshots land in `./test-output`.
- The image recipes run the release build themselves before staging, so
  `just build-release` beforehand is optional and a no-op when nothing
  changed. A compile failure aborts staging, so an image can never be
  built from binaries that do not match the working tree.
- NEVER run `scribe-server`, `scribe-client`, `scribe`, `scribe-dev`, or
  any `scribe-test` subcommand directly on the host. The socket path
  (`crates/scribe-common/src/socket.rs`) has no environment override, so a
  host invocation targets the developer's LIVE server socket and PID file
  — `scribe-test server stop` would SIGTERM the real server. The container
  boundary is the only isolation guarantee.
- If the harness lacks a capability you need (debug builds, extra shells,
  CLI coverage, version pairing, ...), STOP and ask the user. Do not fall
  back to driving the host as a workaround.

### Harness containers must not touch the host network (HARD RULE)

- Every `docker run` in the `justfile` that starts a `scribe-test-*` image
  passes `--network none`. No suite needs the bridge: the server and client
  talk over a Unix socket, and the SSH and LAN suites dial `127.0.0.1`,
  which exists under the null driver. The hardened profile has always run
  this way (`hardened_e2e_flags`); the default profile now matches it.
- This is not a hardening nicety. A bridge attach/detach rewrites the
  Docker NAT and iptables rules for the WHOLE HOST, so every container
  start and stop briefly disrupts new TCP connections to every other
  project's published ports on this machine. A repeated run loop on the
  bridge has broken unrelated dev stacks mid-session.
- Never remove `--network none` to "fix" a suite. If a suite genuinely
  needs egress, STOP and ask the user rather than putting the harness back
  on the bridge.

### Never loop the harness unbounded (HARD RULE)

- `just e2e-func` / `just e2e-visual` create and destroy a container per
  invocation. Repeating one in an unbounded loop is forbidden.
- Measuring a flaky test's failure rate is the usual temptation. State an
  explicit hard cap up front (for example "run it exactly 5 times"), never
  an open-ended floor like "at least 5" — that reads as a licence to run
  45. If the cap is not enough to characterise the flake, report the
  inconclusive result and ask; do not keep going.
- Prefer one container reused via `docker exec` over N containers when a
  suite genuinely needs many iterations.

### Native macOS Metal validation exception

- Native Scribe runtime validation is authorized only through
  `.github/workflows/native-macos-metal.yml` on GitHub's hosted
  `macos-14` ARM64 runner. This narrow exception does not authorize
  invoking Scribe on a developer workstation or another macOS host.
- A repository maintainer with GitHub write access owns dispatch, failure
  triage, evidence review, and the release decision. Dispatch only after the
  workflow is present on the default branch and the target ref contains the
  executable `tests/native-macos/terminal-images-metal.sh` corpus driver.
- Invoke with
  `gh workflow run native-macos-metal.yml --ref <release-candidate-ref>`.
  No repository or environment secrets are required. The workflow uploads
  `test-output/terminal-images/macos/` as
  `native-macos-metal-<run-id>` for 14 days.
- Any build, corpus, timeout, missing-driver, or artifact-upload failure leaves
  the native gate red and blocks platform-dependent GPUI work and default-on
  release. Do not retry product failures without a fixing commit. A maintainer
  may retry an Actions infrastructure failure, retaining both run URLs in the
  release evidence.

## Build environment

- Use Rust 1.95.0 or newer.
- On Debian/Ubuntu, install `clang`, `libfontconfig-dev`, `libssl-dev`,
  `libvulkan1`, `libwayland-dev`, `libx11-xcb-dev`,
  `libxkbcommon-x11-dev`, and `libzstd-dev`. A Vulkan ICD is also required
  to run the client; `mesa-vulkan-drivers` provides Lavapipe when needed.
- `just build`, `just build-release`, `just check`, and `just ready` build,
  release-build, type-check, and run the local quality gate. GPUI is compiled
  at `opt-level = 3` in debug builds; Scribe's own debug code remains
  unoptimized.

# Post-task checklist (REQUIRED — do not skip)

After EVERY task, before responding to the user:

- [ ] Update `lat.md/` if you added or changed any functionality, architecture, tests, or behavior
- [ ] Run `lat check` — all wiki links and code refs must pass
- [ ] Do not skip these steps. Do not consider your task done until both are complete.

---

# What is lat.md?

This project uses [lat.md](https://www.npmjs.com/package/lat.md) to maintain a structured knowledge graph of its architecture, design decisions, and test specs in the `lat.md/` directory. It is a set of cross-linked markdown files that describe **what** this project does and **why** — the domain concepts, key design decisions, business logic, and test specifications. Use it to ground your work in the actual architecture rather than guessing.

# Commands

```bash
lat locate "Section Name"      # find a section by name (exact, fuzzy)
lat refs "file#Section"        # find what references a section
lat search "natural language"  # semantic search across all sections
lat expand "user prompt text"  # expand [[refs]] to resolved locations
lat check                      # validate all links and code refs
```

Run `lat --help` when in doubt about available commands or options.

If `lat search` fails because no API key is configured, explain to the user that semantic search requires a key provided via `LAT_LLM_KEY` (direct value), `LAT_LLM_KEY_FILE` (path to key file), or `LAT_LLM_KEY_HELPER` (command that prints the key). Supported key prefixes: `sk-...` (OpenAI) or `vck_...` (Vercel). If the user doesn't want to set it up, use `lat locate` for direct lookups instead.

# Syntax primer

- **Section ids**: `lat.md/path/to/file#Heading#SubHeading` — full form uses project-root-relative path (e.g. `lat.md/tests/search#RAG Replay Tests`). Short form uses bare file name when unique (e.g. `search#RAG Replay Tests`, `cli#search#Indexing`).
- **Wiki links**: `[[target]]` or `[[target|alias]]` — cross-references between sections. Can also reference source code: `[[src/foo.ts#myFunction]]`.
- **Source code links**: Wiki links in `lat.md/` files can reference functions, classes, constants, and methods in TypeScript/JavaScript/Python/Rust/Go/C files. Use the full path: `[[src/config.ts#getConfigDir]]`, `[[src/server.ts#App#listen]]` (class method), `[[lib/utils.py#parse_args]]`, `[[src/lib.rs#Greeter#greet]]` (Rust impl method), `[[src/app.go#Greeter#Greet]]` (Go method), `[[src/app.h#Greeter]]` (C struct). `lat check` validates these exist.
- **Code refs**: `// @lat: [[section-id]]` (JS/TS/Rust/Go/C) or `# @lat: [[section-id]]` (Python) — ties source code to concepts

# Test specs

Key tests can be described as sections in `lat.md/` files (e.g. `tests.md`). Add frontmatter to require that every leaf section is referenced by a `// @lat:` or `# @lat:` comment in test code:

```markdown
---
lat:
  require-code-mention: true
---
# Tests

Authentication and authorization test specifications.

## User login

Verify credential validation and error handling for the login endpoint.

### Rejects expired tokens
Tokens past their expiry timestamp are rejected with 401, even if otherwise valid.

### Handles missing password
Login request without a password field returns 400 with a descriptive error.
```

Every section MUST have a description — at least one sentence explaining what the test verifies and why. Empty sections with just a heading are not acceptable. (This is a specific case of the general leading paragraph rule below.)

Each test in code should reference its spec with exactly one comment placed next to the relevant test — not at the top of the file:

```python
# @lat: [[tests#User login#Rejects expired tokens]]
def test_rejects_expired_tokens():
    ...

# @lat: [[tests#User login#Handles missing password]]
def test_handles_missing_password():
    ...
```

Do not duplicate refs. One `@lat:` comment per spec section, placed at the test that covers it. `lat check` will flag any spec section not covered by a code reference, and any code reference pointing to a nonexistent section.

# Section structure

Every section in `lat.md/` **must** have a leading paragraph — at least one sentence immediately after the heading, before any child headings or other block content. The first paragraph must be ≤250 characters (excluding `[[wiki link]]` content). This paragraph serves as the section's overview and is used in search results, command output, and RAG context — keeping it concise guarantees the section's essence is always captured.

```markdown
# Good Section

Brief overview of what this section documents and why it matters.

More detail can go in subsequent paragraphs, code blocks, or lists.

## Child heading

Details about this child topic.
```

```markdown
# Bad Section

## Child heading

Details about this child topic.
```

The second example is invalid because `Bad Section` has no leading paragraph. `lat check` validates this rule and reports errors for missing or overly long leading paragraphs.

<!-- SPECKIT START -->
For GPUI client rebuild context, read
`specs/016-gpui-client-rebuild/plan.md` and its adjacent specification and
parity inventory.
<!-- SPECKIT END -->


<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:970c3bf2 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   bd dolt push
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->
