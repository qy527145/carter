//! usage × pricing → 成本（美元）。pricing 单位为每百万 token。

use crate::provider::Usage;
use crate::registry::Pricing;

const PER_MILLION: f64 = 1_000_000.0;

/// 计算单次 usage 的成本（USD）。
/// cache_read/write 有单价时单独计价；缺省单价则按 0 处理。
pub fn compute(usage: &Usage, pricing: &Pricing) -> f64 {
    let input = usage.input as f64 / PER_MILLION * pricing.input;
    let output = usage.output as f64 / PER_MILLION * pricing.output;
    let cache_read =
        usage.cache_read as f64 / PER_MILLION * pricing.cache_read.unwrap_or(0.0);
    let cache_write =
        usage.cache_write as f64 / PER_MILLION * pricing.cache_write.unwrap_or(0.0);
    input + output + cache_read + cache_write
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_input_output() {
        let usage = Usage {
            input: 1_000_000,
            output: 1_000_000,
            ..Default::default()
        };
        let pricing = Pricing {
            input: 3.0,
            output: 15.0,
            cache_write: Some(3.75),
            cache_read: Some(0.30),
        };
        // 3.0 + 15.0 = 18.0
        assert!((compute(&usage, &pricing) - 18.0).abs() < 1e-9);
    }

    #[test]
    fn includes_cache() {
        let usage = Usage {
            input: 0,
            output: 0,
            cache_read: 1_000_000,
            cache_write: 1_000_000,
            reasoning: 0,
        };
        let pricing = Pricing {
            input: 3.0,
            output: 15.0,
            cache_write: Some(3.75),
            cache_read: Some(0.30),
        };
        // 0.30 + 3.75 = 4.05
        assert!((compute(&usage, &pricing) - 4.05).abs() < 1e-9);
    }

    #[test]
    fn missing_cache_price_is_zero() {
        let usage = Usage {
            cache_read: 1_000_000,
            ..Default::default()
        };
        let pricing = Pricing {
            input: 3.0,
            output: 15.0,
            cache_write: None,
            cache_read: None,
        };
        assert_eq!(compute(&usage, &pricing), 0.0);
    }
}
