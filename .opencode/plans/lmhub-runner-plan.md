# lmhub-runner — plan implementacji

## Cel
Niezależny runner AI w Rust: modele jako coding agents, publiczny prompt + kontrolowane tools,
sandbox na pliki wygenerowane przez model w `output/{family}/{model}/{reasoning}/output-modelu/`.

## Architektura (Cargo workspace)

```text
lmhub-runner/
├── Cargo.toml                 # workspace + binarka lmhub-runner (src/main.rs)
├── providers/                 # custom providerzy (*.toml), np. example provider
├── prompts/                   # publiczne prompty systemowe (*.md), default.md
├── crates/
│   ├── core/                  # lmhub-core
│   │   ├── błędy (thiserror): CoreError {Provider, Http, MissingApiKey, Parse,
│   │   │   Sandbox, Timeout, Cancelled, Io, Other}
│   │   ├── ModelInfo/Capabilities/ModelCatalog/ModelListSource{ProviderApi|ModelsDev|LocalConfig}
│   │   ├── Usage{input,output,reasoning?,cache_read?,cache_write?} + checked add
│   │   ├── ModelPricing{input/output/cache_read?/cache_write? per 1M USD, source}
│   │   ├── Chat: Role, ToolSpec, ToolCallRequest, ChatMessage(provider_state passthrough),
│   │   │   ChatRequest(model,system,messages,tools,reasoning,max_tokens,cache),
│   │   │   ChatResponse{text,thinking,tool_calls,usage,stop_reason,raw_assistant,warnings,duration_ms}
│   │   ├── trait Provider (async-trait): id/name/type/api_key_env/local_models/
│   │   │   models_dev_hint/caps/list_models_api()/chat()
│   │   └── RunEvent(tagged JSONL) + StatsDocument (dokładnie wg specyfikacji)
│   ├── modelsdev/             # lmhub-modelsdev
│   │   ├── fetch https://models.dev/api.json (reqwest)
│   │   ├── cache ~/.cache/lmhub/models.dev.json + meta{fetched_at,sha256}, TTL z configu,
│   │   │   stale fallback gdy fetch padnie (warning)
│   │   ├── lookup(provider_hint, model_id) — dopasowanie per provider+model, nie rodzina
│   │   └── pricing_from(entry) → None gdy brak input/output (koszt=null + warning)
│   ├── providers/             # lmhub-providers
│   │   ├── openai.rs    — natywny /v1/chat/completions; reasoning_effort; usage details
│   │   │   (cached_tokens, reasoning_tokens); retry-once bez opcjonalnych parametrów przy 400
│   │   ├── anthropic.rs — natywny /v1/messages; thinking(budget wg effort); cache_control
│   │   │   ephemeral na systemie+ostatnim toolu; raw content blocks wracają do historii
│   │   │   (signature preserved); usage: cache_read/cache_creation
│   │   ├── openai_compat.rs / anthropic_compat.rs — adaptery sterowane configiem TOML
│   │   ├── loader.rs    — parsowanie+walidacja providers/*.toml
│   │   └── registry.rs  — resolver listy modeli: Provider API → Models.dev → lokalny TOML;
│   │       wzbogacenie metadanymi/cenami Models.dev; heurystyka family z prefiksu ID
│   ├── sandbox/               # lmhub-sandbox
│   │   ├── jail.rs — PathJail: kanonikalizacja + starts_with, odrzucanie symlink-escape
│   │   ├── proc.rs — run_command: allowlista (node/npm/npx), ręczna resolucja przez PATH,
│   │   │   env wyczyszczony (HOME/TMPDIR=workspace), timeout, kill grupy procesów (libc)
│   │   └── tools.rs — 7 tools: list_directory/read_file/write_file/edit_file/
│   │       create_directory/run_command/read_command_output; ToolOutcome ze statusem,
│   │       duration, bezpiecznymi metadata, violation flagą
│   ├── agent/                 # lmhub-agent
│   │   ├── sink.rs  — EventSink: events.jsonl + errors.log + kanał do TUI; redakcja sekretów
│   │   ├── pricing.rs — normalizacja tokenów (input zawiera cacheRead wg spec),
│   │   │   koszt = (in-cacheRead)*pin + cacheRead*pcr + cacheWrite*pcw + out*pout; null bez ceny
│   │   └── run.rs   — pętla agenta (max_turns, deadline, cancel token, catch_unwind),
│   │       statistics.json pisany ZAWSZE (completed/error/timeout/cancelled/limit_exceeded)
│   └── tui/                   # lmhub-tui (ratatui+crossterm)
│       ├── Setup: provider → lista modeli async (źródło widoczne) → model (detale:
│       │   family/context/capabilities/ceny Models.dev) → reasoning off/low/med/high
│       │   → prompt z prompts/*.md ('d' = ustaw default) → task input → Enter=start
│       ├── Run: live feed eventów + tokeny/koszt/toolcalls/elapsed; 'c' = cancel
│       ├── History: skan output/**/statistics.json, tabela + podgląd JSON
│       └── komunikacja tokio::mpsc, poll co ~150 ms
└── src/main.rs                # tracing(stderr), config ~/.config/lmhub/config.toml,
                               # rejestr providerów (builtin openai/anthropic + TOML), TUI
```

## Kluczowe decyzje

1. **Non-streaming v1** — live view aktualizowany per turą/eventem tool call; SSE później, truity nie zmieniają się.
2. **Token normalization** — wg przykładu ze spec (`1800/4218=0.4267`): `tokens.input` = pełny input **zawierający** cacheRead.
   Koszt: `(input-cacheRead)/1e6*priceIn + cacheRead/1e6*priceRead (+cacheWrite) + output/1e6*priceOut`.
   Reasoning tokens osobno; NIE doliczane do kosztu drugi raz (Anthropic ma je już w output).
3. **Anthropic thinking** — budget wg poziomu: low=2048/medium=8192/high=16384; max_tokens=16384 (clamp do modelu);
   raw bloki (ze signature) przechodzą przez `provider_state`.
4. **Caching** — Anthropic: jawne `cache_control`; OpenAI: automatyczne (cached_tokens z details);
   custom: flaga `supports_prompt_caching`; brak wsparcia ≠ przerwanie runa.
5. **Brak ceny** → wszystkie pola kosztu `null` + ostrzeżenie do errors.log i events.jsonl.
6. **Sekrety** — klucze tylko z env przy wywołaniu HTTP; nigdy w logach/statystykach/eventach.
7. **Allowlista** — domyślnie `["node","npm","npx"]`, nadpisywalna w configu; argv array, bez shella.
8. **Family** — config > Models.dev > heurystyka prefiksu (gpt→GPT, claude→Claude, glm→GLM,
   gemini→Gemini, qwen→Qwen, deepseek→DeepSeek, minimax→MiniMax, …).

## Custom provider TOML

```toml
id = "my-provider"
name = "My Provider"
api_type = "openai-compatible"        # | "anthropic-compatible"
base_url = "https://api.example.com/v1"
api_key_env = "MY_PROVIDER_API_KEY"
models_path = "/models"               # opcjonalne
chat_path = "/chat/completions"       # opcjonalne
supports_tool_calls = true
supports_reasoning = true
supports_prompt_caching = false
models_dev_provider = "hpc-ai"        # opcjonalne mapowanie cen/metadata

[[models]]
id = "model-a"
name = "Model A"
family = "GLM"
reasoning = true
tool_call = true
context_window = 128000
max_output = 8192
```

## Ryzyka / ograniczenia
- Sandbox aplikacyjny (bez Landlock/bwrap) — dokumentowane; silniejsza izolacja jako follow-up.
- Modele bez endpointu `/models` → automatyczny fallback; źródło zawsze pokazane w TUI.
- Różnice API między modelami tego samego providera → retry-once bez opcjonalnych parametrów.

## Weryfikacja
- `cargo check`, `cargo clippy`, testy jednostkowe: ratio math, path-jail escapes, parser Models.dev,
  heurystyka family, builder statistics.
- Smoke-run TUI wymaga klucza API w env — poza CI.
