use serde::{Deserialize, Serialize};

/// Token accounting for one turn or an accumulated run.
///
/// `input_tokens` is the *total* prompt input including cached tokens,
/// matching the convention of the output schema example
/// (`cache_hit_ratio = cache_read / input`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens spent on reasoning/thinking, when the provider reports them separately.
    /// Never billed twice: cost math uses `output_tokens` only once.
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = sum_opt(self.reasoning_tokens, other.reasoning_tokens);
        self.cache_read_tokens = sum_opt(self.cache_read_tokens, other.cache_read_tokens);
        self.cache_write_tokens = sum_opt(self.cache_write_tokens, other.cache_write_tokens);
    }

    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

fn sum_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.saturating_add(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_optional_fields_without_fabrication() {
        let mut u = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: Some(4),
            ..Default::default()
        };
        u.add(&Usage {
            input_tokens: 1,
            output_tokens: 2,
            reasoning_tokens: Some(7),
            ..Default::default()
        });
        assert_eq!(u.input_tokens, 11);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cache_read_tokens, Some(4));
        assert_eq!(u.reasoning_tokens, Some(7));
    }
}
