# lmhub-runner

Independent AI runner in Rust: models act as **coding agents** — they receive
a public system prompt plus a controlled set of tools and work inside their
own sandboxed workspace under `output/`. Every run is fully recorded
(`statistics.json`, `events.jsonl`, `errors.log`), so tokens, cost, tool
behavior and failures stay auditable after the fact.

## Features

- **194 providers** from the models.dev catalog (the same set opencode ships),
  with native adapters for OpenAI, Anthropic, Google Gemini, Azure OpenAI,
  Amazon Bedrock (SigV4), Vertex AI, Cohere, GitLab Duo, GitHub Copilot
  (device flow) and watsonx/SAP AI Core pre-auth flows.
- **Ratatui TUI**: setup, concurrent live runs, bulk runs across providers, a
  reasoning-level map and a run-history browser — with command palette and
  mouse support.
- **Sandboxed execution**: filesystem jail, allowlisted commands, resource
  limits and timeouts; OS-level isolation via bubblewrap with a seccomp
  legacy fallback.
- **Streaming** (SSE) with automatic retries, `Retry-After` support and
  graceful reasoning-level degradation.
- **Honest accounting**: unknown prices and ratios stay `null` with a warning
  — never guessed. Reasoning tokens are reported separately and never billed
  twice.

## Quick start

```bash
cargo run
```

The interactive TUI starts — no CLI flags. The tabs are:

1. **Setup** — searchable provider list (just type to filter; grouped
   local / native / routed, `F` stars favorites) → models auto-load (source
   shown) → pick a model → reasoning level (`d` pins the current level as the
   model's persistent default, shown with ★) → system prompt (`d` sets the
   default) → task prompt (the user instruction, selected from a list —
   `Ctrl+Enter` runs the run).
2. **Run** — multiple concurrent sessions (`[`/`]` switch) with a structured
   transcript of turns and tool calls, live token/cache/cost counters. Runs
   beyond the concurrency cap (`max_concurrent_runs`, default 2, in
   `~/.config/lmhub/ui.json`) queue as pending and auto-start when a slot
   frees; queued sessions are cancellable.
3. **Bulk runs** — `m` enables multi-select in the models pane, `Space`
   checks models **across providers** (selection survives switching), `x` on
   the task prompt launches them all after a confirmation modal. Each run
   uses its model's pinned default reasoning level when one exists (clamped
   to what the model supports); otherwise the currently selected reasoning
   level is used — that choice **survives navigating between models and
   providers**, so what you set is what bulk runs get. The confirmation
   modal shows the reasoning each run will actually use.
4. **Reasoning** — every model across all 194 providers with its supported
   reasoning levels (type to filter, `D` cycles the ★ default on the
   selected model, `F5` reloads the snapshot).
5. **History** — previous runs; `Enter` opens their `statistics.json`
   pretty-printed (tokens, performance, tool calls, pricing — not raw JSON).

`:` opens the command palette (run, bulk run, cancel all, rescan history,
open output dir, quit); `?` opens the keybinding help overlay. Mouse clicks
focus panes, select list rows and switch tabs; the mouse wheel scrolls
lists and the transcript. Last selections, favorites, per-model reasoning
defaults, the last task prompt and the concurrency cap persist in
`~/.config/lmhub/ui.json`.

### Keybindings

| Screen    | Keys                                                                                  |
|-----------|---------------------------------------------------------------------------------------|
| Global    | `Ctrl-C`/`Ctrl-Q` always quit (first press cancels runs, second forces), `?` help, `:` palette, `Tab` next tab, mouse click + wheel |
| Setup     | `←`/`→` pane, type=search providers (`q` is a filter char here; `Backspace` deletes), `↑`/`↓` select, `Enter` connect/key, `F` favorite, `m` multi-select, `Space` toggle bulk, `C` clear bulk, `x` bulk-run, `F5` force-reload models, `r` reload models, `d` pin reasoning default / default prompt / default task prompt |
| Task      | `↑`/`↓` select task prompt, `Ctrl+Enter` run, `d` set default task prompt, `x` bulk-run |
| Run       | `[`/`]` previous/next session, `↑`/`↓` scroll transcript, `c` cancel session, `C` cancel all, `R` rerun, `v` raw feed, `Enter` run detail |
| History   | `↑`/`↓` select, `Enter` detail (scrollable with `↑`/`↓`/wheel), `F5` rescan  |
| Reasoning | type=filter (`q` is a filter char), `↑`/`↓` select, `D` cycle ★ default, `Esc` clear, `F5` reload snapshot   |

### Credentials & prompts

Providers need a credential: store it in the TUI (`Enter` on a provider →
type the key) or set the env var the provider expects (`OPENAI_API_KEY`,
`ANTHROPIC_API_KEY`, …). Keys live in `~/.config/lmhub/auth.json` (0600,
atomic writes), and credentials resolve **auth.json → environment → none**
per request. Local runtimes (ollama, lmstudio, llama.cpp, …) need none.

Custom providers live in [`providers/`](providers/) as TOML files — see
[`providers/example.toml.example`](providers/example.toml.example). Adding one
requires **no changes to the runner core**, and a custom entry overrides a
bundled provider with the same `id`.

Prompts are markdown files split into two lists, both discovered across
`prompts/` (+ `~/.config/lmhub/prompts`, + cache) and deduped by name:

- **System prompts** — files at the root of each `prompts/` dir (backward
  compatible with the old layout) plus `prompts/system-prompts/`;
  `default.md` is materialized on first run.
- **Task prompts** — the user instruction sent as the first message, in
  `prompts/task-prompts/`; a `default.md` is materialized there on first
  run. The old free-text task editor is gone: pick a task prompt from the
  list (or write your own `*.md`).

## Output layout

Every run writes exactly this structure:

```text
output/
  {family}/            # e.g. Claude, GPT, GLM, DeepSeek…
    {model}/
      {reasoning}/     # off | minimal | low | medium | high | xhigh | max
        {start}-{runid8}/   # unique per run — reruns never clobber
          model-output/    # ONLY files generated by the model
          statistics.json  # written for every terminal state the runner sees
          events.jsonl     # tool calls, LLM turns, errors, warnings
          errors.log       # human-readable error lines
```

The family is decided by: explicit override → models.dev metadata → an
id-prefix heuristic.

- **`statistics.json`** (camelCase): `status` (completed/error/timeout/
  cancelled/limit_exceeded — written for **every** terminal state, including
  panics), `runId` (traceable to the directory suffix), provider/model/
  family/reasoning, `tokens` (input, output, reasoning, cacheRead/cacheWrite,
  total, cacheHitRatio), `performance` (tokensPerSecond, turns, llmRequests,
  avg/max request ms), `toolCalls` (total/successful/failed + ratios),
  `cache`, `pricing` (source, fetchedAt, snapshotVersion, prices and computed
  USD — all `null` when unknown), `errors.count` + `errors.logPath`,
  `warningsCount`.
- **`events.jsonl`** — one JSON object per line (`run_started`,
  `turn_started`, `llm_response`, `tool_call`, `sandbox_violation`,
  `warning`, `error`, `run_finished`). Streaming deltas are UI-only and never
  persisted, so the schema stays stable.
- **`errors.log`** — tab-separated `{timestamp}\t{kind}\t{message}` lines;
  kinds are stable machine-readable strings (`provider_api`,
  `http_transport`, `missing_api_key`, `response_parsing`,
  `sandbox_violation`, `timeout`, `cancelled`, `limit_exceeded`, `io`,
  `other`).

## Tools

The agent gets exactly 7 tools, executed inside the workspace jail:

| Tool                  | Behavior                                                            |
|-----------------------|---------------------------------------------------------------------|
| `list_directory`      | directory listing (dirs as `name/`, files with size); capped at 500 entries |
| `read_file`           | paged reads (`offset_line`, `max_bytes`), server-enforced cap, truncated-marker on overflow |
| `write_file`          | full-content writes (byte cap), parent dirs auto-created            |
| `edit_file`           | exact-substring replacement; must match uniquely unless `replace_all` |
| `create_directory`    | `mkdir -p` inside the workspace                                    |
| `run_command`         | allowlisted command as an **argv array** (no shell), per-command timeout |
| `read_command_output` | captured stdout/stderr tail of the last command                    |

`run_command` is argv-only — no shell, so pipes, redirects, globs and `&&`
do not work. Violations are blocked, logged and counted as failures.

## Sandbox

The model can only affect its `model-output/` directory:

- **Path jail**: all file tools go through path canonicalization + prefix
  checks; `..` traversal is rejected outright, symlink escapes are detected
  (including dangling symlinks on write paths), absolute paths are re-based
  onto the workspace.
- **Commands**: only allowlisted binaries (default `node`, `npm`, `npx`),
  resolved manually via `PATH` and passed as argv arrays — no shell.
- **Process hygiene**: empty environment (`PATH`/`HOME`/`TMPDIR` inside the
  workspace only, `C.UTF-8` locale), per-command timeout, hard resource
  limits (2 GiB address space, CPU, 64 MiB file size, 512 processes,
  1024 fds), process-group termination on expiry (SIGTERM → 2 s grace →
  SIGKILL).
- **Violations** land in `errors.log` and `events.jsonl`; the runner never
  logs API keys or environment secrets (values are scrubbed at every sink).

### OS-level isolation

When **bubblewrap** (`bwrap`) is installed — auto-detected at startup — each
command runs inside user/pid/ipc/uts namespaces with a read-only view of the
system (`/usr`, `/bin`, `/lib`, `/lib64`, `/etc`) and the model workspace as
the only writable directory. `/proc` and `/dev` are fresh sandbox instances,
so the command cannot see host processes or devices. **Network stays
allowed** so `npm install` works. In **legacy mode** (bwrap missing, or user
namespaces blocked — e.g. Ubuntu 24.04's `apparmor_restrict_unprivileged_userns`),
the command runs directly and a **seccomp deny-list** applies instead
(ptrace, cross-process memory access, mounts, kernel module loading, bpf,
userfaultfd, io_uring, `unshare`/`setns`, namespace-flag `clone`); legacy
mode logs a loud warning at startup. Seccomp cannot apply to the bwrap
wrapper itself (bwrap needs those very syscalls), so under bwrap the
namespace split carries that protection. Tune with
`sandbox = "auto" | "bwrap" | "legacy"` in `~/.config/lmhub/config.toml`
(or `LMHUB_BWRAP` to point at a bwrap binary).

Note: this is still application-level sandboxing (no full OS
virtualisation). Run only with providers you trust accordingly.

## Models, pricing & provenance

Models resolve through a fallback chain, always labeled in the TUI:

1. **Provider API** (`/models`) — native OpenAI/Anthropic and custom TOML
   providers with a `models_path`, plus routed OpenAI-compatible providers
   with a base URL.
2. **Models.dev** (<https://models.dev/api.json>, cached in `~/.cache/lmhub`
   with a TTL; a sha256 prefix is recorded as snapshot version; stale cache
   serves offline with a warning).
3. **Local provider config** (`[[models]]` in the custom TOML).

Prices come from Models.dev tied to the concrete provider+model route. If no
price exists, cost fields stay `null` and a warning is logged — never
guessed. Cost conventions: billed plain input = `input − cacheRead −
cacheWrite`; `cacheHitRatio = cacheRead / input`; reasoning tokens are
reported but never billed twice.

## Providers (the full opencode catalog)

The runner ships a bundled snapshot of **models.dev** — all 194 providers
opencode supports are available out of the box, routed through dedicated
adapters per protocol:

- **OpenAI-compatible long tail** (155+): groq, mistral, xai, perplexity,
  together, deepinfra, cerebras, openrouter, fireworks, …
- **Native adapters**: OpenAI, Anthropic, Google Gemini, Azure OpenAI,
  Amazon Bedrock (bearer + SigV4), Vertex AI (Gemini & Claude variants),
  Cohere, GitLab Duo, GitHub Copilot (device flow), watsonx/SAP pre-auth.

The Setup pane shows `[key ok]` / `[no key]` / `[local]` badges per provider
and the source of each model list. Regenerate the catalog snapshot after
models.dev updates:

```bash
cargo run -p xtask -- gen-providers
```

## Streaming & resilience

Model responses stream token-by-token (SSE) for OpenAI-compatible providers,
Anthropic (incl. Vertex-Anthropic), and Gemini (incl. Vertex-Gemini); the
TUI shows a live tail of the current turn plus a delta counter. Protocols
without SSE support (Bedrock, Cohere) transparently fall back to
non-streaming — `statistics.json`, `events.jsonl` and the tool protocol are
identical in both modes (streaming deltas are UI-only and never persisted).

### Reasoning levels

Models declare their supported reasoning levels via models.dev
`reasoning_options` (the same source opencode uses) — a plain on/off toggle
maps to `off/high`, `budget_tokens` to `high/max`, effort lists map 1:1
(`off / minimal / low / medium / high / xhigh / max`). The TUI only offers
the levels a model actually supports, and the agent clamps the requested
level before sending anything, logging a warning when it had to adjust.
Models with no declaration get all levels; custom TOML models can pin theirs
with `reasoning_levels = ["off", "high"]`. Providers that reject a level at
request time degrade once: when the error names a valid level ("please use
low, high, or max") the request is retried with that level, otherwise once
without reasoning — never a hard failure.

### Retries

Transient failures (408/425/429/5xx/connect/timeouts) retry automatically:
**6 attempts**, exponential backoff 0.5 s → 30 s cap with ±14% jitter,
honoring `Retry-After` (hard-capped at 10 minutes). Retries never apply once
stream bytes started flowing. Tune in `~/.config/lmhub/config.toml`:

```toml
max_retries = 6
retry_base_ms = 500
retry_cap_ms = 30000
```

## Configuration

`~/.config/lmhub/config.toml` (all optional; a broken file refuses to start
until fixed, zero/absurd values are clamped with a warning):

```toml
default_prompt = "default"
default_task_prompt = "build"
run_timeout_secs = 900        # wall-clock deadline per run
max_turns = 30                # agent loop cap
command_timeout_secs = 90     # per run_command invocation
allowed_commands = ["node", "npm", "npx"]
modelsdev_ttl_secs = 86400
max_output_tokens = 16384
read_file_max_bytes = 48000
write_file_max_bytes = 1000000
sandbox = "auto"              # auto | bwrap | legacy (command isolation backend)
max_retries = 6
retry_base_ms = 500
retry_cap_ms = 30000
```

UI preferences (last selections, favorites, per-model reasoning defaults,
last task prompt, concurrency cap) live separately in
`~/.config/lmhub/ui.json`:

```json
{
  "last_provider": "anthropic",
  "last_model": "claude-3-7-sonnet",
  "last_reasoning": "high",
  "last_prompt": "default",
  "last_task_prompt": "refactor",
  "favorites": ["openai"],
  "model_defaults": { "gpt-4o": "medium", "claude-3-7-sonnet": "high" },
  "max_concurrent_runs": 2
}
```

Paths default to the project directory and OS config/cache dirs; every path
is overridable via environment: `LMHUB_CONFIG_DIR`, `LMHUB_CACHE_DIR`,
`LMHUB_OUTPUT_DIR`, `LMHUB_PROVIDERS_DIR` (plus `LMHUB_BWRAP` for the bwrap
binary). Structured logs go to `<cache>/runner.log` (10 MiB cap, scrubbed of
secrets; filter via `RUST_LOG`).

## CI & releases

GitHub Actions on every push/PR: `cargo fmt --check`, `clippy -D warnings`,
the full test suite (`--locked`), a release build smoke, `cargo audit`
(advisories), `cargo deny` (licenses/bans/sources) — plus a monthly
models.dev snapshot-drift check that fails if `xtask gen-providers` would
change the bundled catalog. Tagging `v*` builds release binaries for linux
(x86_64, aarch64-musl via cross) and macOS (x86_64, aarch64); tarballs ship
the binary plus `prompts/*.md`, `prompts/system-prompts/`,
`prompts/task-prompts/` and `providers/example.toml.example`.

## Architecture

```text
crates/core        domain types, Provider trait, statistics/event schemas,
                   auth store, redaction engine, config
crates/modelsdev   models.dev client + local cache + pricing lookup
crates/providers   native + routed adapters (194-provider catalog), model
                   resolution chain, SigV4/JWT/device-flow helpers
crates/sandbox     path jail, allowlisted process runner, the 7 tools,
                   bwrap/seccomp runtime detection
crates/agent       agent loop, event sink, cost computation
crates/tui         ratatui interface — Elm-style State/Action/reduce core,
                   command palette, multi-run sessions, bulk start
xtask              `gen-providers` catalog snapshot regeneration
src/main.rs        wiring
```

Design invariants worth knowing before extending:

- **Never guess**: prices, ratios and cache costs are `null`/warning when
  unknown; capabilities default fail-safe (all-false) so an adapter never
  over-claims.
- **Durable by construction**: `statistics.json` is written for every
  terminal state (even panics); `auth.json`, the models.dev cache and run
  artifacts are written atomically; run directories are unique per run, so
  reruns never reuse a dirty workspace.
- **Secrets never leave**: API keys and env secrets are registered with the
  redaction engine and scrubbed at every sink — tests assert the run
  artifacts contain none.

Add a provider = implement `lmhub_core::Provider` (async-trait), drop a TOML
into `providers/`, or just set its env key — the catalog already knows it.
