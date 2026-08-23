//! History: scanning previous runs' `statistics.json` and pretty-printing
//! one document. Pure IO + formatting (no ratatui).

use serde_json::Value;
use std::path::{Path, PathBuf};

/// One scanned run, summarised for the history table.
#[derive(Debug, Clone)]
pub struct HistoryRow {
    pub path: PathBuf,
    pub family: String,
    pub model: String,
    pub reasoning: String,
    pub status: String,
    pub duration_ms: Option<u64>,
    pub total_tokens: Option<u64>,
    pub total_usd: Option<f64>,
    pub started_at: Option<String>,
}

const MAX_DEPTH: usize = 4;

/// Recursively collect every `statistics.json` under `output_base`.
/// Newest run first (timestamp-prefixed dirs); path order as fallback.
pub fn scan_history(output_base: &Path) -> Vec<HistoryRow> {
    let mut rows = Vec::new();
    scan_dir_recursive(output_base, 0, &mut rows);
    rows.sort_by(|a, b| {
        b.started_at
            .as_deref()
            .cmp(&a.started_at.as_deref())
            .then_with(|| b.path.cmp(&a.path))
    });
    rows
}

fn scan_dir_recursive(dir: &Path, depth: usize, out: &mut Vec<HistoryRow>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_recursive(&path, depth + 1, out);
        } else if path
            .file_name()
            .map(|n| n == "statistics.json")
            .unwrap_or(false)
        {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if let Some(mut row) = parse_statistics(&raw) {
                    row.path = path.clone();
                    out.push(row);
                }
            }
        }
    }
}

/// Parse the raw `statistics.json` into a summary row.
pub fn parse_statistics(raw: &str) -> Option<HistoryRow> {
    let v: Value = serde_json::from_str(raw).ok()?;
    Some(HistoryRow {
        family: v["family"].as_str().unwrap_or("?").into(),
        model: v["model"].as_str().unwrap_or("?").into(),
        reasoning: v["reasoning"].as_str().unwrap_or("?").into(),
        status: v["status"].as_str().unwrap_or("?").into(),
        duration_ms: v["durationMs"].as_u64(),
        total_tokens: v["tokens"]["total"].as_u64(),
        total_usd: v["pricing"]["totalUsd"].as_f64(),
        started_at: v["startedAt"].as_str().map(String::from),
        path: PathBuf::new(),
    })
}

/// Read + parse the document at `path`.
pub fn read_detail(path: &Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let v: Value =
        serde_json::from_str(&raw).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    Ok(format_detail(&v, path))
}

/// Human-readable rendering of one statistics document — not raw JSON.
fn format_detail(v: &Value, path: &Path) -> String {
    let s = |k: &str| v[k].as_str().unwrap_or("?");
    let n = |k: &str| {
        v[k].as_u64()
            .map(|x| x.to_string())
            .unwrap_or_else(|| "null".into())
    };
    let mut out: Vec<String> = Vec::new();
    out.push(format!("run      : {}", v["runId"].as_str().unwrap_or("?")));
    out.push(format!("status   : {}", s("status")));
    out.push(format!(
        "provider : {} ({})",
        s("provider"),
        s("providerType")
    ));
    out.push(format!("family   : {}", s("family")));
    out.push(format!("model    : {}", s("model")));
    out.push(format!("reasoning: {}", s("reasoning")));
    out.push(format!("started  : {}", s("startedAt")));
    out.push(format!(
        "finished : {}",
        v["finishedAt"].as_str().unwrap_or("?")
    ));
    out.push(format!("duration : {} ms", n("durationMs")));
    out.push(String::new());
    let t = &v["tokens"];
    out.push("— tokens —".to_string());
    out.push(format!(
        "input {}/output {}",
        n2(t, "input"),
        n2(t, "output")
    ));
    out.push(format!(
        "reasoning {} · cache-read {} · cache-write {}",
        n2(t, "reasoning"),
        n2(t, "cacheRead"),
        n2(t, "cacheWrite")
    ));
    out.push(format!(
        "total {} · cache-hit {}",
        n2(t, "total"),
        t["cacheHitRatio"]
            .as_f64()
            .map(|x| format!("{x:.4}"))
            .unwrap_or_else(|| "null".into())
    ));
    let perf = &v["performance"];
    out.push(String::new());
    out.push("— performance —".to_string());
    out.push(format!(
        "turns {} · llm requests {} · tps {} · avg {} ms · max {} ms",
        n2(perf, "turns"),
        n2(perf, "llmRequests"),
        perf["tokensPerSecond"]
            .as_f64()
            .map(|x| format!("{x:.2}"))
            .unwrap_or_else(|| "null".into()),
        n2(perf, "avgLlmRequestMs"),
        n2(perf, "maxLlmRequestMs")
    ));
    let tc = &v["toolCalls"];
    out.push(String::new());
    out.push("— tool calls —".to_string());
    out.push(format!(
        "total {} · ok {} · failed {} (ratio {})",
        n2(tc, "total"),
        n2(tc, "successful"),
        n2(tc, "failed"),
        tc["successRatio"]
            .as_f64()
            .map(|x| format!("{x:.4}"))
            .unwrap_or_else(|| "null".into())
    ));
    let pr = &v["pricing"];
    out.push(String::new());
    out.push("— pricing —".to_string());
    out.push(format!(
        "in {}/1M · out {}/1M · cache-r {}/1M · cache-w {}/1M",
        pr["inputPerMillionTokensUsd"]
            .as_f64()
            .map(|x| format!("{x:.2}"))
            .unwrap_or_else(|| "null".into()),
        pr["outputPerMillionTokensUsd"]
            .as_f64()
            .map(|x| format!("{x:.2}"))
            .unwrap_or_else(|| "null".into()),
        pr["cacheReadPerMillionTokensUsd"]
            .as_f64()
            .map(|x| format!("{x:.2}"))
            .unwrap_or_else(|| "null".into()),
        pr["cacheWritePerMillionTokensUsd"]
            .as_f64()
            .map(|x| format!("{x:.2}"))
            .unwrap_or_else(|| "null".into())
    ));
    out.push(format!(
        "total {} USD (source: {} · snapshot {})",
        pr["totalUsd"]
            .as_f64()
            .map(|x| format!("{x:.6}"))
            .unwrap_or_else(|| "null".into()),
        pr["source"].as_str().unwrap_or("?"),
        pr["snapshotVersion"].as_str().unwrap_or("?")
    ));
    out.push(String::new());
    out.push(format!(
        "errors {} (log: {}) · warnings {}",
        v["errors"]["count"].as_u64().unwrap_or(0),
        v["errors"]["logPath"].as_str().unwrap_or("?"),
        v["warningsCount"].as_u64().unwrap_or(0)
    ));
    out.push(format!("path     : {}", path.display()));
    out.join("\n")
}

fn n2(v: &Value, k: &str) -> String {
    v[k].as_u64()
        .map(|x| x.to_string())
        .unwrap_or_else(|| "null".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "status": "completed",
        "runId": "abc123",
        "provider": "openai",
        "providerType": "native-openai",
        "family": "GPT",
        "model": "gpt-4o",
        "reasoning": "off",
        "startedAt": "2026-08-24T00:00:00Z",
        "finishedAt": "2026-08-24T00:01:00Z",
        "durationMs": 60000,
        "tokens": { "input": 1000, "output": 200, "total": 1200, "cacheHitRatio": 0.25 },
        "performance": { "turns": 3, "llmRequests": 3, "tokensPerSecond": 10.0 },
        "toolCalls": { "total": 2, "successful": 2, "failed": 0 },
        "pricing": { "totalUsd": 0.001, "source": "models.dev" },
        "errors": { "count": 0, "logPath": "errors.log" },
        "warningsCount": 1
    }"#;

    #[test]
    fn parses_summary_row() {
        let row = parse_statistics(SAMPLE).unwrap();
        assert_eq!(row.model, "gpt-4o");
        assert_eq!(row.status, "completed");
        assert_eq!(row.total_tokens, Some(1200));
        assert_eq!(row.total_usd, Some(0.001));
        assert_eq!(row.duration_ms, Some(60000));
        assert_eq!(row.family, "GPT");
    }

    #[test]
    fn parse_never_fabricates() {
        assert!(parse_statistics("not json").is_none());
        assert!(parse_statistics("{}").is_some()); // missing fields → "?"
    }

    #[test]
    fn detail_formatting_contains_key_facts() {
        let v: Value = serde_json::from_str(SAMPLE).unwrap();
        let text = format_detail(&v, Path::new("/tmp/x/statistics.json"));
        assert!(text.contains("completed"));
        assert!(text.contains("gpt-4o"));
        assert!(text.contains("0.001"));
        assert!(text.contains("1200"));
        assert!(text.contains("/tmp/x/statistics.json"));
    }
}
