//! What a model call costs, from a price table kept in code.
//!
//! Prices are US dollars per million tokens, Anthropic first-party API
//! rates, last checked 2026-11-17. Cache writes are billed at
//! [`CACHE_WRITE_MULTIPLIER`] × input: loam only uses the 5-minute
//! ephemeral cache.

use super::Usage;

/// 5-minute cache writes cost 1.25× the input price.
pub const CACHE_WRITE_MULTIPLIER: f64 = 1.25;

/// Per-million-token prices for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    /// Prompts longer than this many tokens are billed at `long`, if set.
    pub long_above: u64,
    pub long: Option<(f64, f64, f64)>,
}

const fn flat(input: f64, output: f64, cache_read: f64) -> Price {
    Price {
        input,
        output,
        cache_read,
        long_above: u64::MAX,
        long: None,
    }
}

/// The price table entry for a model id, if loam knows it.
pub fn price(model_id: &str) -> Option<Price> {
    Some(match model_id {
        "claude-opus-5-5" => flat(4.0, 20.0, 0.20),
        "claude-sonnet-5-5" => flat(2.0, 10.0, 0.10),
        // Haiku 5.5: cache reads at the usual 0.1× input; prompts over
        // 100K tokens cost 5× more.
        "claude-haiku-5-5" => Price {
            input: 0.10,
            output: 0.50,
            cache_read: 0.01,
            long_above: 100_000,
            long: Some((0.50, 2.50, 0.05)),
        },
        _ => return None,
    })
}

/// What a call with this usage cost on `model_id`, or `None` for a model
/// missing from the table.
pub fn cost_usd(model_id: &str, usage: &Usage) -> Option<f64> {
    let p = price(model_id)?;
    let prompt =
        usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens;
    let (input, output, cache_read) = match p.long {
        Some(long) if prompt > p.long_above => long,
        _ => (p.input, p.output, p.cache_read),
    };
    let per = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
    Some(
        per(usage.input_tokens, input)
            + per(usage.output_tokens, output)
            + per(usage.cache_read_input_tokens, cache_read)
            + per(
                usage.cache_creation_input_tokens,
                input * CACHE_WRITE_MULTIPLIER,
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, write: u64, read: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: write,
            cache_read_input_tokens: read,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn sonnet_prices_every_token_kind() {
        // 1M of each: 2 input + 10 output + 2.5 write + 0.10 read.
        let m = 1_000_000;
        let cost = cost_usd("claude-sonnet-5-5", &usage(m, m, m, m)).unwrap();
        assert!(close(cost, 14.6), "{cost}");
    }

    #[test]
    fn opus_and_haiku_rates() {
        let cost = cost_usd("claude-opus-5-5", &usage(1000, 100, 0, 10_000)).unwrap();
        assert!(close(cost, 0.004 + 0.002 + 0.002), "{cost}");
        let cost = cost_usd("claude-haiku-5-5", &usage(1000, 1000, 0, 0)).unwrap();
        assert!(close(cost, 0.0001 + 0.0005), "{cost}");
    }

    #[test]
    fn long_haiku_prompts_cost_more() {
        let cost = cost_usd("claude-haiku-5-5", &usage(150_000, 0, 0, 0)).unwrap();
        assert!(close(cost, 0.075), "{cost}");
    }

    #[test]
    fn unknown_models_have_no_price() {
        assert_eq!(cost_usd("fake", &Usage::default()), None);
    }
}
