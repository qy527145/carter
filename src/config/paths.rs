//! 配置/数据根路径解析。全平台统一用 `~/.carter`。
//! 优先 `$CARTER_HOME`；否则 home 目录（Windows `%USERPROFILE%`，其它 `$HOME`）下的 `.carter`。

use std::path::PathBuf;

/// 配置/数据根目录。`$CARTER_HOME` 覆盖；否则 `<home>/.carter`。
pub fn carter_home() -> PathBuf {
    if let Some(explicit) = std::env::var_os("CARTER_HOME") {
        return PathBuf::from(explicit);
    }
    home_dir().join(".carter")
}

/// `<root>/config.toml`。
pub fn config_path() -> PathBuf {
    carter_home().join("config.toml")
}

/// `<root>/models.json`（models.dev 缓存）。
pub fn models_cache_path() -> PathBuf {
    carter_home().join("models.json")
}

/// `<root>/skills`（可发现能力包目录）。
pub fn skills_dir() -> PathBuf {
    carter_home().join("skills")
}

/// `<root>/carter.log`（运行日志；TUI 模式下 tracing 写此处而非终端）。
pub fn log_path() -> PathBuf {
    carter_home().join("carter.log")
}

/// 用户 home 目录：Windows `%USERPROFILE%`，其它 `$HOME`。缺失则回落当前目录。
fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let key = "USERPROFILE";
    #[cfg(not(windows))]
    let key = "HOME";

    std::env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carter_home_respects_env_override() {
        // SAFETY: 测试单线程内设置/清除自有 env。
        unsafe {
            std::env::set_var("CARTER_HOME", "/tmp/custom-carter");
        }
        assert_eq!(carter_home(), PathBuf::from("/tmp/custom-carter"));
        assert_eq!(
            config_path(),
            PathBuf::from("/tmp/custom-carter").join("config.toml")
        );
        assert_eq!(
            models_cache_path(),
            PathBuf::from("/tmp/custom-carter").join("models.json")
        );
        assert_eq!(
            skills_dir(),
            PathBuf::from("/tmp/custom-carter").join("skills")
        );
        unsafe {
            std::env::remove_var("CARTER_HOME");
        }
    }
}
