//! `${ENV}` 与内置变量插值。密钥永不入文件，统一用环境变量。

use std::env;

/// 把字符串里的 `${VAR}` 替换为环境变量值。
/// 未定义的变量原样保留（不报错，便于配置可提交）。UTF-8 安全。
pub fn interpolate(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let var = &after[..end];
                match env::var(var) {
                    Ok(val) => out.push_str(&val),
                    Err(_) => {
                        // 原样保留 `${VAR}`。
                        out.push_str("${");
                        out.push_str(var);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                // 无闭合 `}`，原样输出剩余部分。
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_defined_var() {
        unsafe { env::set_var("CARTER_TEST_KEY", "secret123") };
        assert_eq!(interpolate("${CARTER_TEST_KEY}"), "secret123");
        assert_eq!(
            interpolate("prefix-${CARTER_TEST_KEY}-suffix"),
            "prefix-secret123-suffix"
        );
    }

    #[test]
    fn keeps_undefined_var() {
        assert_eq!(
            interpolate("${CARTER_UNDEFINED_XYZ}"),
            "${CARTER_UNDEFINED_XYZ}"
        );
    }

    #[test]
    fn no_placeholder_passthrough() {
        assert_eq!(interpolate("plain 文本 text"), "plain 文本 text");
    }
}
