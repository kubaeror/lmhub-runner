# Custom providers — `providers/*.toml`

Every valid `*.toml` here becomes a provider at startup; entries override
built-ins and the bundled catalog by `id` (files are processed in sorted
order). A broken file is reported at startup, never fatal. The directory is
overridable via `LMHUB_PROVIDERS_DIR`. See `example.toml.example` for the
full schema.

The bundled catalog already covers **all 222 models.dev providers** (the same
set opencode uses) — TOML files are only needed to:

- add a provider that is **not** in models.dev,
- override a base URL (proxy/self-hosted endpoint),
- pin a static model list.

## Credential sources

Precedence per provider: `~/.config/lmhub/auth.json` (TUI: select provider →
`Enter`; keys must be ≥ 8 chars) → environment variables listed in the
catalog entry's `key_envs` (the regenerated snapshot splits credential-bearing
names from display-only ones, so e.g. `AZURE_RESOURCE_NAME` is never treated
as a key).

Common key env vars (single source of truth: `key_envs` in
`crates/providers/src/known/known_providers.json`):

| Provider | Key env var(s) |
|---|---|
| openai | `OPENAI_API_KEY` |
| anthropic | `ANTHROPIC_API_KEY` |
| google | `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
| groq | `GROQ_API_KEY` |
| mistral | `MISTRAL_API_KEY` |
| xai | `XAI_API_KEY` |
| openrouter | `OPENROUTER_API_KEY` |
| cohere | `COHERE_API_KEY` |
| azure | `AZURE_API_KEY` (+ `AZURE_RESOURCE_NAME`) |
| azure-cognitive-services | `AZURE_COGNITIVE_SERVICES_API_KEY` (+ resource name) |
| cloudflare-workers-ai | `CLOUDFLARE_API_KEY` (+ `CLOUDFLARE_ACCOUNT_ID`) |
| databricks | `DATABRICKS_TOKEN` (+ `DATABRICKS_HOST`) |
| neon | `NEON_AI_GATEWAY_TOKEN` (+ base URL) |
| snowflake-cortex | `SNOWFLAKE_CORTEX_PAT` (+ `SNOWFLAKE_ACCOUNT`) |
| google-vertex\* | `GOOGLE_APPLICATION_CREDENTIALS` (service account) |
| watsonx | `WATSONX_AI_APIKEY` (+ `WATSONX_AI_PROJECT_ID`) |
| sap-ai-core | `AICORE_SERVICE_KEY` |
| github-copilot | device flow; `GITHUB_TOKEN` may be pre-set |

Local runtimes need no key and listen on localhost:

| Runtime | Base URL |
|---|---|
| ollama | `http://localhost:11434/v1` |
| llama.cpp (`llama-server`) | `http://localhost:8080/v1` |
| vLLM | `http://localhost:8000/v1` |
| LocalAI | `http://localhost:8080/v1` |
| LM Studio | `http://localhost:1234/v1` |

## Protocol notes for exotic providers

| Provider | What resolves automatically |
|---|---|
| azure / azure-cognitive-services | env `AZURE_RESOURCE_NAME` (+ `api-key`) |
| amazon-bedrock | bearer `AWS_BEARER_TOKEN_BEDROCK` or SigV4 from AWS keys, region via `AWS_REGION`/profile (no SSO/IMDS profiles) |
| google | native Gemini API, `GEMINI_API_KEY`/`GOOGLE_API_KEY` |
| google-vertex(-anthropic) | `GOOGLE_VERTEX_PROJECT`, `GOOGLE_VERTEX_LOCATION` (default `global`); token via `VERTEX_ACCESS_TOKEN` or ADC service-account JWT (token_uri must be `https://*.googleapis.com`) |
| github-copilot | device flow — select it and press `Enter` in Setup; `LMHUB_GITHUB_CLIENT_ID` overrides the OAuth client, `LMHUB_COPILOT_EDITOR_VERSION` the editor version header |
| gitlab | `GITLAB_TOKEN` + optional `GITLAB_AI_GATEWAY_URL` |
| watsonx | `WATSONX_AI_APIKEY`, `WATSONX_AI_PROJECT_ID`, optional `WATSONX_AI_URL` |
| sap-ai-core | `AICORE_SERVICE_KEY` JSON (client credentials) |

Everything else speaks the OpenAI-compatible wire at its catalog URL.

