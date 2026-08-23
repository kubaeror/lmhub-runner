# Plan: wszyscy providerzy z opencode w lmhub-runner

## Zakres (zatwierdzony przez użytkownika)

Pełny zakres: wszystkie 194 providerów z katalogu models.dev (tego samego,
którego używa opencode), **w tym egzotyczne protokoły** (Bedrock SigV4,
Vertex SA-JWT, Copilot device-OAuth, Cohere/WatsonX/SAP). Własny magazyn
kluczy `auth.json` (nie import z opencode). Rejestr jako wbudowany snapshot.

## Analiza katalogu (fakty)

- 194 providerów; rozkład adapterów AI SDK:
  - 155 × `@ai-sdk/openai-compatible` — wszystkie mają publiczny `api` URL
  - 9 × `@ai-sdk/anthropic`, 4 × `@ai-sdk/openai` — pokryte istniejącym kodem
  - ~26 „well-known" bez `api` w models.dev → trzeba zaszyć bazowe URLe/env
  - reszta: własne pakiety SDK, ale niemal wszystkie mówią OpenAI-wire pod
    podanym URL-em; prawdziwe wyjątki: cohere, watsonx, sap-ai-core, bedrock,
    azure ×2, google, google-vertex(±anthropic), gitlab, copilot

## Architektura

### A. Rejestr + poświadczenia

1. **`crates/providers/src/known/`**
   - `known_providers.json` — snapshot `{id,name,npm,api,env}` wygenerowany
     z models.dev, commitowany do repo
   - `xtask gen-providers` — regeneracja snapshotu (`cargo run -p xtask ...`)
   - `known.rs` — parsowanie + mapowanie `npm` → `ProtocolKind`, tabela
     fallbacków URL/env dla 26 well-known providerów
2. **`ProtocolKind`** (nowe wartości): `OpenAiCompat` | `AnthropicCompat` |
   `Azure` | `GeminiNative` | `VertexGemini` | `VertexAnthropic` |
   `Bedrock{Bearer|SigV4}` | `Cohere` | `GitLabDuo` | `Copilot` |
   `OpenAiCompatPreAuth{IbmIam|OauthClientCredentials}`
3. **Własny auth store** `~/.config/lmhub/auth.json` (uprawnienia 0600):
   `{provider_id: {type:"api", key}}`; później `type:"oauth"` blob dla
   Copilota/Vertex. Precedencja: **auth.json > env > brak**.
   Załadowane wartości rejestrowane w `redact::init()` (scrub wszędzie).
4. **`credentials.rs`** — rozwiązywanie klucza per provider (auth.json/env),
   flaga `requires_key=false` dla lokalnych (ollama, lmstudio, llama.cpp,
   atomic-chat, qvac).
5. **Rejestr**: `build_registry()` = builtins + known(194) + user TOML
   (user nadpisuje wszystko po `id`). Dedupe po id.

### B. Adaptery (każdy wg istniejącego wzorca: wire build/parse + testy bez sieci)

| Adapter | Kluczowe detale wire |
|---|---|
| `azure.rs` | URL `https://{AZURE_RESOURCE_NAME}.openai.azure.com/openai/deployments/{model}/chat/completions?api-version=…`, header `api-key`; brak `/models` → źródło Models.dev/local |
| `gemini.rs` | natywne `v1beta/models/{m}:generateContent`, header `x-goog-api-key`; functionCall/functionResponse; `usageMetadata.thoughtsTokenCount` → reasoning tokens; `cachedContentTokenCount` → cache read; listing `GET v1beta/models` |
| `vertex.rs` | URL `…/projects/{p}/locations/{l}/publishers/{pub}/models/{m}:generateContent`; auth: ADC service-account (RS256 JWT → token, cache do expiry) albo `VERTEX_ACCESS_TOKEN`; wariant anthropic = messages-wire na aiplatform URL |
| `bedrock.rs` | Converse API `bedrock-runtime.{region}.amazonaws.com/model/{id}/converse`; toolUse/toolResult blocks; usage inputTokens/cacheRead…; auth: bearer `AWS_BEARER_TOKEN_BEDROCK` LUB SigV4 (hand-rolled signer na hmac+sha2, test na oficjalnym wektorze AWS); region z env/AWS_PROFILE(mini-INI) |
| `cohere.rs` | `/v2/chat`: content jako tablice `{type:"text"}`, tools v2 schema, tool results role:"tool" |
| `preauth.rs` | generyczny hook pre-auth dla openai-compat: IBM IAM (`watsonx`: apikey→token→`ml/v1/text/chat`) i OAuth client-credentials (`sap-ai-core`: AICORE_SERVICE_KEY JSON→token→deployment URL) |
| `gitlab.rs` | PAT/GITLAB_TOKEN + ai-gateway URL (env override), statyczna trójka duo-chat-* + models z konfigu |
| `copilot.rs` | device-flow (device/code → poll → access_token → `copilot_internal/v2/token` exchange, cache w auth.json), nagłówki editor-version/integration-id; `GET /models` działa; client_id wbudowany + override `LMHUB_GITHUB_CLIENT_ID` |

Pozostałe pakiety (xai, groq, perplexity, mistral, together, deepinfra,
cerebras, openrouter, vercel gateway, v0, aihubmix, salad, merge, qvac…) →
`OpenAiCompat` na ich URL-ach z models.dev (audyt per-provider w tabeli
w `known.rs`; odstępstwa udokumentowane).

### C. Integracja

- TUI: lista providerów z **filtrem tekstowym**, badge `[key ok]/[no key]/[local]`,
  ekran „connect" (wpisanie klucza → auth.json), odświeżenie rejestrów
- `statistics.providerType`: nowe etykiety (`azure-openai`, `google-gemini`,
  `aws-bedrock-sigv4`, …) — schemat statystyk bez zmian
- Ceny nadal z models.dev per route (ids zgodne z katalogiem) — zero zmian
- README + przykładowe TOML-e (azure, google, bedrock)

### D. Nowe zależności

`hmac` (SigV4), `jsonwebtoken` (RS256 JWT dla Vertex), `base64`.

## Kolejność implementacji (commity w jednej sesji)

1. xtask + snapshot + `known.rs` + rejestr merge + testy
2. auth store + credentials resolution + TUI connect/filtr
3. azure, gemini (najprostsze nowe wire'y) + testy
4. bedrock (bearer → SigV4 z wektorem testowym) + vertex (token → SA JWT)
5. cohere + preauth (watsonx/sap) + gitlab + copilot
6. audyt 194 wpisów (tabela mapowania), docs, finalne clippy/testy

## Ryzyka

- SigV4/JWT poprawność → oficjalne wektory testowe AWS, testy jednostkowe
- Zmienność egzotycznych API → tolerancyjne parsowanie (serde default) jak
  w modelsdev; błędy trafiają do errors.log zamiast wywracać run
- Copilot wymaga własnego OAuth client_id — stała + override env
- Bedrock profile/credential_process spoza zakresu (jasny błąd)
