# Plan: streaming + odporność błędów + CI

Decyzje użytkownika: SSE bez Bedrocka; retry **6 prób** (cap 30s); TUI live tail;
CI = ci.yml + release.yml.

---

## 1. Streaming (SSE)

### Zasada projektowa
Nowa metoda `chat_stream()` w traice `Provider` z **domyślną implementacją
delegującą do `chat()`** (jeden event `Completed`) — wszystkie 194 providerów
działają od razu; prawdziwe SSE implementujemy protokołowo, reszta korzysta
z fallbacku bez żadnych zmian w agent/TUI.

### lmhub-core
- `chat.rs`:
  ```rust
  pub enum ChatDelta { Text(String), Thinking(String) }
  pub enum ChatStreamItem {
      Delta(ChatDelta),        // inkrementy (bez szumu tool-call)
      Completed(ChatResponse), // zawsze ostatni: usage/stop_reason/raw blocks
  }
  pub type ChatStream = std::pin::Pin<Box<dyn futures::Stream<
      Item = Result<ChatStreamItem>> + Send>>;
  ```
- `provider.rs`: `async fn chat_stream(&self, req:&ChatRequest) -> Result<ChatStream>`
- `events.rs`: nowy wariant `RunEvent::LlmDelta { ts, turn, text }`
  (**tylko do TUI** — sink NIE pisze delt do `events.jsonl`; schema plików
  runów bez zmian, statystyki nadal z finalnej odpowiedzi)
- `config.rs`: `max_retries: u32 (6)`, `retry_base_ms: u64 (500)`,
  `retry_cap_ms: u64 (30_000)`
- `error.rs`: `CoreError::Transient { code: Option<u16>, retry_after_secs:
  Option<u64>, message: String }` — klasyfikowalne 429/5xx/transport;
  `Provider(String)` zostaje dla pozostałych HTTP (heurystyki
  reasoning_effort/tools retry działają dalej)

### providers — infrastruktura
- **nowy `sse.rs`**: wspólny parser linii SSE nad `bytes_stream()`
  (`event:`/`data:`/wieloliniowe data, terminator `[DONE]`) + builder żądania
  streamingowego; użyty przez wszystkie trzy protokoły
- **`retry` w `http.rs`**: `RetryPolicy` z configu; `send_with_retry()`
  - retry: 429/500/502/503/504, connect/timeout transportowe, `Transient`
  - `Retry-After` (sekundy lub data HTTP) ma pierwszeństwo
  - exponential backoff 0.5s→cap 30s + jitter; warning-log każdej próby (scrub)
  - **retry tylko przed pierwszym bajtem** — mid-stream = błąd końcowy
  - przełączają się na niego `post_json`/`get_json` (czyli też listingi)
- testy: fixture'y SSE jako stringi; fake-clock sleeper wstrzykiwany do policy

### adapterskie implementacje SSE
| Protokół | Detale |
|---|---|
| OpenAI-compat (+Azure, Copilot, GitLab, watsonx, SAP, openrouter…) | `"stream":true` + `stream_options:{include_usage:true}`; flaga `supports_usage_options`, retry-once bez niej przy 400; akumulacja `delta.content`/`reasoning_content`/`tool_calls[index]` (arguments sklejane → JSON na końcu); usage z final chunk |
| Anthropic (+VertexAnthropic) | `message_start`(input/cache) → `content_block_start/delta(text\|thinking\|signature\|input_json)`/`stop` → `message_delta`(stop_reason, output_tokens) → `message_stop`; **rekonstrukcja raw blocks ze signature** → `raw_assistant_message` zachowany (historia thinking działa jak dotychczas); VertexAnthropic endpoint `:rawStreamPredict` |
| Gemini (+VertexGemini) | `:streamGenerateContent?alt=sse`; functionCall przychodzi w całości w jednym chunku; `usageMetadata` w ostatnim |
| Bedrock, Cohere | świadomie NON-streaming (default fallback); Cohere ewentualnie doklejony po głównej pracy |

### agent (`run.rs`)
Jedyna zmiana wywołania (linia ~225): pętla konsumująca stream —
`Delta` → `sink.emit_for_ui(LlmDelta)` + nic do plików; `Completed` → dalszy
przebieg identyczny jak dziś (tool calls, usage, statystyki).

### TUI
- `ActiveRun.live_turn: String` (cap ~2000 znaków, czyszczony przy
  `LlmResponse`/`ToolCall`); Run tab: sekcja „streaming" nad statystykami +
  wskaźnik aktywności (liczba delt); batchowanie redraw (już jest tick 200 ms)
- feed events.jsonl bez zmian (brak spamu deltami)

---

## 2. Odporność (podsumowanie polityki)
6 prób · backoff 0.5s→30s cap · jitter · Retry-After pierwszeństwo ·
retry zero-byte-only dla streamów · wszystko konfigurowalne
(`max_retries/retry_base_ms/retry_cap_ms`) · każdy retry jako Warning event.

---

## 3. CI (GitHub Actions)

**`.github/workflows/ci.yml`** (push/PR):
1. `fmt` — `cargo fmt --check`
2. `clippy` — `cargo clippy --workspace --all-targets -- -D warnings`
3. `test` — `cargo test --workspace --locked` (ubuntu-latest; node obecny dla
   testów sandboxa; testy sieciowe są `#[ignore]`)
4. `build` — `cargo build --release` (smoke)
cache: `Swatinem/rust-cache`.

**`.github/workflows/release.yml`** (tag `v*`):
matrix targets: `x86_64/aarch64-unknown-linux-{gnu,musl}`(gnu+musl opcjonalnie),
`x86_64/aarch64-apple-darwin`; build release → strip → tar.gz
(binarka + `prompts/` + `providers/example.toml.example`) → upload artifact.

---

## Kolejność implementacji (7 kroków, commity osobno)
1. Retry: `CoreError::Transient` + policy + http helper + testy (fake clock)
2. `sse.rs` + OpenAI stream + routed compat + fixture-testy
3. Anthropic stream (+VertexAnthropic)
4. Gemini stream (+VertexGemini)
5. Agent loop na `chat_stream` + `RunEvent::LlmDelta` + TUI live tail
6. Workflows CI/release + badge w README
7. README (streaming/retry/CI) + finalne clippy/testy

## Ryzyka
- `stream_options` bywa odrzucane przez serwery compat → flaga + degradacja
- nazwa endpointu Vertex-Anthropic streaming (`rawStreamPredict`) do
  walidacji w trakcie; fallback = ta jedna ścieżka zostaje non-streaming
- częste delty → throttle renderu (tick 200 ms już istnieje)
