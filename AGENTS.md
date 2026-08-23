# AGENTS.md

Guidance for AI coding agents working in this repository. Read the README for
user-facing behavior; this file is about how the code is built and what must
never break.

## What this is

A Rust workspace (`lmhub-runner` binary) that runs LLMs as **coding agents**:
a model gets a public system prompt + 7 tools and works inside a sandboxed
workspace. A ratatui TUI drives provider/model/reasoning selection, live runs
and history. The bundled catalog covers all 194 providers from models.dev.

## Commands

```bash
cargo run                          # launch the TUI (no CLI flags)
cargo build                        # debug build
cargo test --workspace --locked    # full test suite
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run -p xtask -- gen-providers   # regenerate the bundled catalog snapshot
cargo audit                        # advisory check (CI)
cargo deny check licenses bans sources  # license/policy check (CI)
```

`RUST_LOG=debug` gives more detail in `<cache>/runner.log` (the terminal
belongs to the TUI). `LMHUB_CONFIG_DIR`/`LMHUB_CACHE_DIR`/`LMHUB_OUTPUT_DIR`/
`LMHUB_PROVIDERS_DIR` override path defaults.

## Repository layout

| Path | Contents |
|------|----------|
| `crates/core` | domain types (`ReasoningLevel`, `ChatMessage`, `ChatRequest/Response`), the `Provider` trait, `StatisticsDocument`/`RunEvent` schemas, `AuthStore`, redaction engine, `AppConfig` |
| `crates/modelsdev` | models.dev client, TTL cache, `reasoning_options` → `ReasoningLevel` mapping, pricing lookup |
| `crates/providers` | native adapters (openai, anthropic, azure, gemini, bedrock, vertex, cohere, copilot), `RoutedProvider` for the catalog long tail, registry, SigV4/JWT/device-flow, SSE runners, retry policy, custom-TOML parsing |
| `crates/sandbox` | `PathJail`, `run_allowlisted` process runner (rlimits, seccomp, bwrap), the 7 tools, `detect_runtime` |
| `crates/agent` | `execute(RunSpec, ui_tx)` agent loop, `EventSink`, cost computation |
| `crates/tui` | Elm-style `State`/`Action`/`reduce` core, keymap, views |
| `xtask` | `gen-providers` snapshot regeneration |
| `src/main.rs` | wiring: auth store → redaction init → config → registry → TUI |
| `providers/*.toml` | custom providers (override bundled by `id`), see `example.toml.example` |
| `prompts/*.md` | public system prompts |

## Design invariants (do not break these)

1. **Never guess.** Unknown prices, ratios and cache costs are `null`/`None`
   plus a warning — never fabricated numbers (`core/src/stats.rs` `ratio()`
   returns `None` on zero denominators; pricing returns null when a used
   component lacks a price).
2. **Fail-safe defaults.** `ProviderCaps` defaults all-false so an adapter
   never over-claims; `AppConfig::sanitize` clamps zero/absurd values.
3. **Durability.** `statistics.json` is written for **every** terminal
   state, including panics (the loop runs under `catch_unwind`). Run
   directories are unique per run (`{start}-{runid8}`) — reruns never
   clobber. `auth.json` and the models.dev cache write atomically (`.tmp` +
   rename).
4. **Secrets never leave.** API keys and env secrets are registered with
   `lmhub_core::redact` (`init()` scans marker-named env vars,
   `register_extra` on every stored credential) and scrubbed at every sink.
   Never log raw arguments/headers; use `arguments_keys` (names only) for
   tool-call metadata. Tests assert run artifacts contain no secrets.
5. **Streaming is UI-only.** `LlmDelta` events are `#[serde(skip)]` — never
   persisted. `events.jsonl` stays schema-stable; only `Completed` responses
   drive the agent loop.
6. **Reasoning tokens are informational.** Reported in usage/statistics but
   never billed twice (`billed input = input − cacheRead − cacheWrite`).
7. **Reasoning negotiation.** Requested levels are clamped to the model's
   declared `reasoning_levels` *before* any request; provider rejections
   degrade once (named level → once without reasoning), never hard-fail.
8. **Atomic/deterministic ordering.** Events are written before the terminal
   statistics document so counts are consistent; `finalize()` fails loudly on
   partial persistence (disk full).

## Conventions

- **Schemas**: `statistics.json` is camelCase (`runId`, `cacheHitRatio`);
  `events.jsonl` is tagged snake_case JSON (`run_started`, `tool_call`); all
  timestamps RFC3339 UTC millis (`now_ts`).
- **Errors**: `CoreError` with stable machine-readable `kind()` strings —
  `provider_api`, `http_transport`, `missing_api_key`, `response_parsing`,
  `sandbox_violation`, `timeout`, `cancelled`, `limit_exceeded`, `io`,
  `other`. New error paths must map onto these kinds and land in both
  `errors.log` and `events.jsonl`.
- **Wire fidelity**: adapters keep raw wire state via
  `ChatMessage.provider_state` / `ChatResponse.raw_assistant_message` (e.g.
  Anthropic thinking signatures) — never lose it in round-trips.
- **Providers**: implement `lmhub_core::Provider` (async-trait,
  `Send + Sync`). `chat_stream` defaults to wrapping `chat`, so non-SSE
  protocols are automatic. Never log API keys; document env vars.
- **Tool protocol**: `run_command` takes argv arrays, never a shell string;
  allowlist matching is exact on `argv[0]`; everything the model touches goes
  through `PathJail::resolve` (canonicalization + prefix check, `..`
  rejected).
- **TUI**: all state mutation goes through `reduce(Action) -> Vec<Effect>`;
  async results re-enter as `Action::UiMsg(UiMsg)`. Keybindings live in
  `keymap.rs` (footer hints in `view/shared.rs` must stay in sync).
- **Docs**: public items are rustdoc-commented; doc comments explain *why*
  (invariants, failure modes), matching the existing style.

## Adding a provider

Three ways, in increasing effort:

1. **It's already in the catalog** — just set its env key (or store it via
   the TUI). The bundled snapshot knows all 194.
2. **Custom TOML** — drop a file in `providers/` following
   `example.toml.example` (`api_type` openai-compatible or
   anthropic-compatible, `[[models]]` with optional `reasoning_levels`).
   Overrides bundled providers by `id`; a broken file is reported, never
   fatal.
3. **Native adapter** — implement `Provider` and register it in
   `build_registry` (`crates/providers/src/registry.rs`).

After models.dev changes, regenerate the snapshot with
`cargo run -p xtask -- gen-providers` and commit the diff — CI's
catalog-drift check enforces this.

## Testing notes

- Tests live next to code and in `crates/agent/tests/agent_loop.rs` (the
  `MockProvider` there is the pattern for exercising the full run pipeline:
  happy path, provider failure, unique run dirs, no secrets in artifacts).
- Sandbox tests that need bwrap skip when it's absent; allowlist tests need
  `node` on PATH (preinstalled on CI).
- `every_known_provider_is_routable` in `crates/providers/src/known.rs`
  guarantees the whole catalog resolves — keep it green when touching
  protocol mapping or fallback URLs.
- CI runs `cargo test --workspace --locked`, `clippy -D warnings`,
  `fmt --check`, `cargo audit` and `cargo deny` — mirror these locally.
