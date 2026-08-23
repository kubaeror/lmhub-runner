# Custom providers — `providers/*.toml`

Every valid `*.toml` here becomes a provider at startup; entries override
built-ins and the bundled catalog by `id`. See `example.toml.example` for the
full schema.

The bundled catalog already covers **all 194 models.dev providers** (the same
set opencode uses) — TOML files are only needed to:

- add a provider that is **not** in models.dev,
- override a base URL (proxy/self-hosted endpoint),
- pin a static model list.

## Credential sources

Precedence per provider: `~/.config/lmhub/auth.json` (TUI: select provider →
`Enter`) → environment variables listed in the catalog entry.

Local runtimes (`ollama`, `lmstudio`, `llama.cpp`, …) need no key.

## Protocol notes for exotic providers

| Provider | What resolves automatically |
|---|---|
| azure / azure-cognitive-services | env `AZURE_RESOURCE_NAME` (+ `api-key`) |
| amazon-bedrock | bearer `AWS_BEARER_TOKEN_BEDROCK` or SigV4 from AWS keys, region via `AWS_REGION`/profile |
| google | native Gemini API, `GEMINI_API_KEY`/`GOOGLE_API_KEY` |
| google-vertex(-anthropic) | `GOOGLE_CLOUD_PROJECT`, `VERTEX_LOCATION`; token via `VERTEX_ACCESS_TOKEN` or ADC service-account JWT |
| github-copilot | device flow — select it and press `Enter` in Setup |
| gitlab | `GITLAB_TOKEN` + optional `GITLAB_AI_GATEWAY_URL` |
| watsonx | `WATSONX_AI_APIKEY`, `WATSONX_AI_PROJECT_ID`, optional `WATSONX_AI_URL` |
| sap-ai-core | `AICORE_SERVICE_KEY` JSON (client credentials) |

Everything else speaks the OpenAI-compatible wire at its catalog URL.
