//! Provider search: ranking, match highlighting and browse grouping.
//!
//! Pure logic (no ratatui, no IO) so it is unit-testable in isolation.

use lmhub_core::Provider;

/// Browse-mode grouping of the provider list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// Local runtimes that need no credentials.
    Local,
    /// Dedicated native adapters (anthropic, openai, azure, bedrock, ...).
    Native,
    /// Everything else routed over a generic wire protocol.
    Routed,
}

impl Group {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Local => "LOCAL",
            Self::Native => "NATIVE",
            Self::Routed => "ROUTED",
        }
    }
}

/// Native adapter type labels (dedicated wire implementations).
const NATIVE_TYPES: &[&str] = &[
    "native-",
    "azure",
    "google-gemini",
    "google-vertex",
    "aws-bedrock",
    "cohere",
    "gitlab-duo",
    "github-copilot",
    "watsonx",
];

pub fn group_of(p: &dyn Provider) -> Group {
    if !p.requires_credentials() {
        Group::Local
    } else if NATIVE_TYPES
        .iter()
        .any(|prefix| p.provider_type().starts_with(prefix))
    {
        Group::Native
    } else {
        Group::Routed
    }
}

/// How well `query` matches `text`. `None` = no match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Score {
    /// 0 = prefix, 1 = contiguous substring, 2 = fuzzy subsequence.
    pub kind: u8,
    /// Character index where the match starts.
    pub start: usize,
}

/// Score a single text field. Case-insensitive.
pub fn rank_field(query: &str, text: &str) -> Option<Score> {
    let q = query.to_ascii_lowercase();
    let t = text.to_ascii_lowercase();
    if q.is_empty() || t.is_empty() {
        return None;
    }
    // Contiguous matches (prefix wins over any later substring).
    if let Some(pos) = t.find(&q) {
        return Some(Score {
            kind: if pos == 0 { 0 } else { 1 },
            start: pos,
        });
    }
    // Fuzzy subsequence: characters in order, not necessarily adjacent.
    let mut qi = q.chars();
    let mut current = qi.next()?;
    let mut start = None;
    for (i, c) in t.char_indices() {
        if c == current {
            start.get_or_insert(i);
            match qi.next() {
                Some(next) => current = next,
                None => {
                    return Some(Score {
                        kind: 2,
                        start: start?,
                    })
                }
            }
        }
    }
    None
}

/// Score a provider across all searchable fields; best field wins.
pub fn rank_provider(query: &str, p: &dyn Provider) -> Option<Score> {
    let mut fields: Vec<String> = vec![
        p.display_name().to_string(),
        p.id().to_string(),
        p.provider_type().to_string(),
    ];
    fields.extend(p.env_keys());
    fields
        .iter()
        .filter_map(|f| rank_field(query, f))
        .min_by_key(|s| (s.kind, s.start))
}

/// Contiguous character ranges to highlight in `text` for `query`.
/// Fuzzy (subsequence) matches highlight from first to last matched char.
pub fn highlight_spans(query: &str, text: &str) -> Vec<(usize, usize)> {
    let q = query.to_ascii_lowercase();
    let t = text.to_ascii_lowercase();
    if q.is_empty() || t.is_empty() {
        return Vec::new();
    }
    if let Some(pos) = t.find(&q) {
        return vec![(pos, pos + q.len())];
    }
    // Subsequence: first..last matched chars (by byte index).
    let mut first: Option<usize> = None;
    let mut last: usize = 0;
    let mut qi = q.chars();
    let mut current = match qi.next() {
        Some(c) => c,
        None => return Vec::new(),
    };
    for (i, c) in t.char_indices() {
        if c == current {
            if first.is_none() {
                first = Some(i);
            }
            last = i + c.len_utf8();
            match qi.next() {
                Some(next) => current = next,
                None => break,
            }
        }
    }
    first.map(|f| vec![(f, last)]).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_beats_substring_beats_subsequence() {
        assert_eq!(
            rank_field("open", "OpenRouter"),
            Some(Score { kind: 0, start: 0 })
        );
        assert_eq!(
            rank_field("router", "OpenRouter"),
            Some(Score { kind: 1, start: 4 })
        );
        // 'gnu' is a subsequence of 'OpenRouter' (o-p-e-n-r-o-u-t-e-r → no 'g').
        assert_eq!(
            rank_field("opnr", "OpenRouter"),
            Some(Score { kind: 2, start: 0 })
        );
        assert_eq!(rank_field("zzz", "OpenRouter"), None);
    }

    #[test]
    fn empty_or_shorter_query_never_matches() {
        assert_eq!(rank_field("", "x"), None);
        assert_eq!(rank_field("abc", "ab"), None);
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(
            rank_field("GROQ", "groq"),
            Some(Score { kind: 0, start: 0 })
        );
        assert_eq!(rank_field("oq", "groq"), Some(Score { kind: 1, start: 2 }));
    }

    #[test]
    fn highlights_substring_and_subsequence() {
        assert_eq!(highlight_spans("oq", "groq"), vec![(2, 4)]);
        assert_eq!(highlight_spans("grq", "groq"), vec![(0, 4)]);
        assert_eq!(highlight_spans("zz", "groq"), Vec::new());
    }

    #[test]
    fn unicode_aware_spans() {
        // "ó" is 2 UTF-8 bytes: byte range 4..6, on char boundaries.
        assert_eq!(highlight_spans("ó", "groqó"), vec![(4, 6)]);
        assert_eq!(rank_field("ó", "groqó"), Some(Score { kind: 1, start: 4 }));
    }
}
