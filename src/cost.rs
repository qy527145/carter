//! usage × pricing → 成本（美元）。pricing 单位为每百万 token。
//!
//! 用的是底座的 [`Usage`]：它的口径是「`input_tokens` 不含缓存命中部分」，
//! 缓存读写各算各的，所以这里不会把缓存部分重复计费。

use crate::provider::Usage;
use crate::registry::Pricing;

const PER_MILLION: f64 = 1_000_000.0;

/// 计算单次 usage 的成本（USD）。
/// cache_read/write 有单价时单独计价；缺省单价则按 0 处理。
pub fn compute(usage: &Usage, pricing: &Pricing) -> f64 {
    let input = usage.input_tokens as f64 / PER_MILLION * pricing.input;
    let output = usage.output_tokens as f64 / PER_MILLION * pricing.output;
    let cache_read =
        usage.cache.read_tokens as f64 / PER_MILLION * pricing.cache_read.unwrap_or(0.0);
    let cache_write =
        usage.cache.write.total as f64 / PER_MILLION * pricing.cache_write.unwrap_or(0.0);
    input + output + cache_read + cache_write
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::CacheUsage;

    fn usage(input: u32, output: u32, read: u32, write: u32) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache: CacheUsage {
                read_tokens: read,
                write: crate::provider::CacheWriteUsage {
                    total: write,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn pricing() -> Pricing {
        Pricing {
            input: 3.0,
            output: 15.0,
            cache_write: Some(3.75),
            cache_read: Some(0.30),
        }
    }

    #[test]
    fn basic_input_output() {
        // 3.0 + 15.0 = 18.0
        assert!((compute(&usage(1_000_000, 1_000_000, 0, 0), &pricing()) - 18.0).abs() < 1e-9);
    }

    #[test]
    fn includes_cache() {
        // 0.30 + 3.75 = 4.05
        assert!((compute(&usage(0, 0, 1_000_000, 1_000_000), &pricing()) - 4.05).abs() < 1e-9);
    }

    #[test]
    fn missing_cache_price_is_zero() {
        let p = Pricing {
            input: 3.0,
            output: 15.0,
            cache_write: None,
            cache_read: None,
        };
        assert_eq!(compute(&usage(0, 0, 1_000_000, 1_000_000), &p), 0.0);
    }
}
