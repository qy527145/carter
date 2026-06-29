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

/// `<root>/system.md`（自定义系统提示词文件的约定位置；存在则覆盖内置人设）。
pub fn system_prompt_path() -> PathBuf {
    carter_home().join("system.md")
}

/// `<root>/CARTER.md`（全局记忆文件；多层记忆注入的最外层）。
pub fn global_memory_path() -> PathBuf {
    carter_home().join("CARTER.md")
}

/// `<root>/facts.md`（全局事实记忆；save_memory kind=facts scope=global 写入）。
pub fn global_facts_path() -> PathBuf {
    carter_home().join("facts.md")
}

/// `<root>/profile.md`（全局用户画像；save_memory kind=profile 写入）。
pub fn global_profile_path() -> PathBuf {
    carter_home().join("profile.md")
}

/// `<root>/skills/<slug>.md`（save_memory kind=skill 写入；与 skills_dir 共目录复用）。
pub fn skill_memory_path(slug: &str) -> PathBuf {
    skills_dir().join(format!("{slug}.md"))
}

/// `<root>/memory_revisions/`（记忆文件每次修改前的快照备份目录）。
pub fn memory_revisions_dir() -> PathBuf {
    carter_home().join("memory_revisions")
}

/// `<root>/carter.log`（运行日志；TUI 模式下 tracing 写此处而非终端）。
pub fn log_path() -> PathBuf {
    carter_home().join("carter.log")
}

/// `<root>/debug/llm_log`（大模型请求日志目录，按天拆分 jsonl）。
pub fn llm_log_dir() -> PathBuf {
    carter_home().join("debug").join("llm_log")
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

/// 测试共享锁：所有动 `CARTER_HOME` 的单元测试都得拿这把锁，避免 cargo test 多线程
/// 下 env 竞态。在 `media` / `memory::writer` 多个模块的测试间共享。
#[cfg(test)]
pub(crate) fn home_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
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
