//! 首次运行向导 —— 无 `~/.carter/config.toml` 时，交互式生成一份。
//! 引导用户配置一个服务商 + 一个模型 + 少量常用项，写盘后调用方重载配置继续。
//! 仅在交互式终端触发；非 TTY（管道/自动化）只打印指引、不阻塞。

use std::io::{self, IsTerminal, Write};

use crate::config::paths;

/// 向导收集到的答案（`render_config` 据此生成 TOML）。
struct Answers {
    provider_name: String,
    kind: String,
    base_url: Option<String>,
    api_key: Option<String>,
    model_name: String,
    meta: String,
    proxy: Option<String>,
    show_thinking: bool,
}

/// 预置协议：(kind, 展示名, 默认 provider 名, meta 的 provider 段, 默认模型名)。
const PRESETS: &[(&str, &str, &str, &str, &str)] = &[
    ("anthropic", "Anthropic Messages", "anthropic", "anthropic", "claude-sonnet-4-5"),
    ("openai_compat", "OpenAI 兼容 (Chat Completions：vLLM/LiteLLM/Azure/OpenRouter…)", "openai", "openai", "gpt-4o"),
    ("openai_responses", "OpenAI Responses", "openai", "openai", "gpt-5"),
    ("gemini", "Google Gemini", "gemini", "google", "gemini-2.5-pro"),
    ("deepseek", "DeepSeek", "deepseek", "deepseek", "deepseek-chat"),
];

/// 若 config.toml 缺失且处于交互式终端，引导生成并写盘。
/// Ok(true) = 已生成（调用方应重载配置）；Ok(false) = 跳过（已存在 / 非交互 / 用户中止）。
pub async fn ensure_config() -> crate::Result<bool> {
    let path = paths::config_path();
    if path.exists() {
        return Ok(false);
    }

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("未找到配置文件 {}。", path.display());
        eprintln!("在交互式终端运行 `carter` 可进入配置向导，或手动创建该文件（字段见 docs/config.example.toml）。");
        return Ok(false);
    }

    println!("\n欢迎使用 Carter —— 未检测到配置，进入首次配置向导。");
    println!("（直接回车采用方括号内的默认值；Ctrl+C 取消。）\n");

    let answers = match collect() {
        Some(a) => a,
        None => {
            // 输入提前结束（EOF）→ 不写文件，交回主流程（后续会给出缺配置的友好报错）。
            println!("\n已取消，未写入配置。");
            return Ok(false);
        }
    };

    let toml = render_config(&answers);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &toml)?;
    println!("\n已写入配置：{}", path.display());

    // 代理：写进了 config，但本进程后续（含下面的拉取）也需要立即生效。
    if let Some(proxy) = &answers.proxy {
        set_proxy_env(proxy);
    }

    ensure_models_cache(&answers).await;

    Ok(true)
}

/// 逐项收集答案。任一必填项遇 EOF → None（中止）。
fn collect() -> Option<Answers> {
    let idx = choose("选择模型服务商协议", PRESETS.iter().map(|p| p.1).collect::<Vec<_>>().as_slice(), 0)?;
    let (kind, _label, def_name, meta_provider, def_model) = PRESETS[idx];

    let provider_name = prompt_default("provider 名称（引用与切换时用，如 mycorp）", def_name)?;
    let base_url = prompt_optional("base_url（自定义端点；回车=用官方默认端点）")?;
    let api_key = prompt_optional("api_key（直接粘贴；也可填 ${ENV_NAME} 走环境变量；回车=暂不填）")?;
    let model_name = prompt_default("模型名（端点实际模型名，同时作为引用块名）", def_model)?;
    let default_meta = format!("{meta_provider}/{model_name}");
    let meta = prompt_default(
        "models.dev 元数据 id（provider_id/model_id，用于查上下文窗口/价格）",
        &default_meta,
    )?;
    let proxy = prompt_optional("HTTP 代理（如 http://127.0.0.1:7890；回车=不走代理）")?;
    let show_thinking = yes_no("显示模型思考内容？", true)?;

    Some(Answers {
        provider_name,
        kind: kind.to_string(),
        base_url,
        api_key,
        model_name,
        meta,
        proxy,
        show_thinking,
    })
}

/// 模型元数据缓存：缺失则提议联网拉取；已存在则校验 meta 是否命中（仅告警）。
async fn ensure_models_cache(a: &Answers) {
    let cache_path = paths::models_cache_path();
    if !cache_path.exists() {
        if yes_no("尚无模型元数据缓存，现在从 models.dev 拉取？（需联网，约数 MB）", true).unwrap_or(false) {
            match crate::registry::fetch::fetch_models_dev().await {
                Ok(json) => match crate::registry::fetch::write_cache(&json) {
                    Ok(()) => println!("已缓存模型元数据到 {}", cache_path.display()),
                    Err(e) => eprintln!("写入缓存失败：{e}（稍后可重试 `carter update`）"),
                },
                Err(e) => eprintln!("拉取失败：{e}\n稍后请手动运行 `carter update`。"),
            }
        } else {
            println!("已跳过。首次对话前请运行 `carter update` 拉取模型元数据。");
        }
    }
    // 校验 meta（缓存可读时）：未命中只提醒，不阻断。
    if let Ok(json) = crate::registry::fetch::read_cache() {
        if let Some((p, m)) = a.meta.split_once('/') {
            if crate::registry::models_dev::lookup(&json, p, m).is_none() {
                eprintln!(
                    "提醒：meta `{}` 未在 models.dev 缓存中找到，模型解析可能失败。\n请核对 id，或编辑 {} 后重试。",
                    a.meta,
                    paths::config_path().display()
                );
            }
        }
    }
}

/// 渲染为带注释的 config.toml（纯函数，便于测试）。
fn render_config(a: &Answers) -> String {
    let mut s = String::new();
    s.push_str("# Carter 配置 —— 由首次运行向导生成。可手动编辑；完整字段说明见 docs/config.example.toml。\n\n");

    s.push_str("[agent]\n");
    s.push_str("# 默认模型，引用格式 provider/model。可被 CLI --model 覆盖。\n");
    s.push_str(&format!("model = \"{}/{}\"\n", a.provider_name, a.model_name));
    s.push_str("max_output_tokens = 16000\n");
    s.push_str("# fast_model = \"<provider>/<model>\"   # 压缩/标题用的快模型，省略则用主模型。\n");
    s.push_str("# system_prompt_file = \"~/.carter/system.md\"   # 覆盖内置「特工卡特」人设。\n\n");

    s.push_str("[reasoning]\n");
    s.push_str(&format!("show_thinking = {}\n\n", a.show_thinking));

    if let Some(proxy) = &a.proxy {
        s.push_str("# 启动时设置的进程环境变量（建 HTTP 客户端前生效）。\n");
        s.push_str("[env]\n");
        s.push_str(&format!("https_proxy = \"{}\"\n", toml_escape(proxy)));
        s.push_str(&format!("http_proxy = \"{}\"\n\n", toml_escape(proxy)));
    } else {
        s.push_str("# [env]\n# https_proxy = \"http://127.0.0.1:7890\"   # 需要代理时取消注释。\n\n");
    }

    s.push_str("# [debug]\n# log_requests = true   # 记录每次 LLM 请求到 ~/.carter/debug/llm_log/。\n\n");

    s.push_str("# ---- 服务商：协议 + 端点 + 密钥 ----\n");
    s.push_str(&format!("[providers.{}]\n", a.provider_name));
    s.push_str(&format!("kind = \"{}\"\n", a.kind));
    match &a.base_url {
        Some(b) => s.push_str(&format!("base_url = \"{}\"\n", toml_escape(b))),
        None => s.push_str("# base_url = \"https://your-endpoint/...\"   # 自定义端点；省略用官方默认。\n"),
    }
    match &a.api_key {
        Some(k) => s.push_str(&format!("api_key = \"{}\"\n", toml_escape(k))),
        None => s.push_str("# api_key = \"${YOUR_API_KEY}\"   # 建议用 ${ENV} 插值，避免硬编码。\n"),
    }
    s.push('\n');

    s.push_str("# 块名即引用里的 model；省略 api_name 时默认取块名做端点模型名。\n");
    s.push_str(&format!("  [providers.{}.models.{}]\n", a.provider_name, a.model_name));
    s.push_str("  # meta = models.dev 的 provider_id/model_id，用于查上下文窗口/价格。\n");
    s.push_str(&format!("  meta = \"{}\"\n", toml_escape(&a.meta)));
    s
}

// ---- 终端交互辅助 ----

/// 读一行（去尾换行）。EOF（Ctrl+D / 管道结束）→ None。
fn read_line() -> Option<String> {
    let mut buf = String::new();
    match io::stdin().read_line(&mut buf) {
        Ok(0) => None, // EOF
        Ok(_) => Some(buf.trim_end_matches(['\n', '\r']).to_string()),
        Err(_) => None,
    }
}

/// 带默认值的提问。空输入→默认值。EOF→None。
fn prompt_default(label: &str, default: &str) -> Option<String> {
    print!("{label} [{default}]: ");
    let _ = io::stdout().flush();
    let line = read_line()?;
    Some(if line.is_empty() { default.to_string() } else { line })
}

/// 可选项。空输入→None（表示「不填」）。EOF→None。
/// 注意：返回 None 同时表示 EOF 与「留空」；本向导对这两者处理一致（都视作不填/继续）。
fn prompt_optional(label: &str) -> Option<Option<String>> {
    print!("{label}: ");
    let _ = io::stdout().flush();
    match read_line() {
        Some(s) if s.is_empty() => Some(None),
        Some(s) => Some(Some(s)),
        None => Some(None),
    }
}

/// 单选。打印编号列表，读取序号。空/非法→默认项。EOF→None。
fn choose(label: &str, options: &[&str], default_idx: usize) -> Option<usize> {
    println!("{label}：");
    for (i, opt) in options.iter().enumerate() {
        let mark = if i == default_idx { "*" } else { " " };
        println!("  {mark} {}. {opt}", i + 1);
    }
    print!("输入序号 [{}]: ", default_idx + 1);
    let _ = io::stdout().flush();
    let line = read_line()?;
    if line.is_empty() {
        return Some(default_idx);
    }
    match line.parse::<usize>() {
        Ok(n) if n >= 1 && n <= options.len() => Some(n - 1),
        _ => {
            println!("（无法识别，采用默认）");
            Some(default_idx)
        }
    }
}

/// 是/否。空→默认。EOF→None。
fn yes_no(label: &str, default_yes: bool) -> Option<bool> {
    let hint = if default_yes { "Y/n" } else { "y/N" };
    print!("{label} [{hint}]: ");
    let _ = io::stdout().flush();
    let line = read_line()?;
    let l = line.trim().to_lowercase();
    Some(match l.as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default_yes,
    })
}

/// 即时设置代理环境变量（向导内拉取 models.dev 时需要）。
fn set_proxy_env(proxy: &str) {
    // SAFETY: 启动早期、尚未 spawn 任务 / 建 HTTP 客户端，无并发读 env 者。
    unsafe {
        std::env::set_var("https_proxy", proxy);
        std::env::set_var("http_proxy", proxy);
    }
}

/// TOML 基本字符串里转义反斜杠与双引号。
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(base: Option<&str>, key: Option<&str>, proxy: Option<&str>) -> Answers {
        Answers {
            provider_name: "mycorp".into(),
            kind: "anthropic".into(),
            base_url: base.map(str::to_string),
            api_key: key.map(str::to_string),
            model_name: "claude-sonnet-4-6".into(),
            meta: "anthropic/claude-sonnet-4-5".into(),
            proxy: proxy.map(str::to_string),
            show_thinking: true,
        }
    }

    #[test]
    fn render_is_valid_toml_and_parses_back() {
        let toml = render_config(&sample(
            Some("https://gw.example.com/anthropic/v1/"),
            Some("sk-abc"),
            Some("http://127.0.0.1:7890"),
        ));
        let cfg: crate::config::Config = toml::from_str(&toml).expect("生成的 TOML 应可解析");
        assert_eq!(cfg.agent.model, "mycorp/claude-sonnet-4-6");
        let p = cfg.providers.get("mycorp").expect("provider 应存在");
        assert_eq!(p.kind, "anthropic");
        assert_eq!(p.base_url.as_deref(), Some("https://gw.example.com/anthropic/v1/"));
        assert_eq!(p.api_key.as_deref(), Some("sk-abc"));
        let m = p.models.get("claude-sonnet-4-6").expect("模型块应存在");
        assert_eq!(m.meta, "anthropic/claude-sonnet-4-5");
        assert_eq!(cfg.env.get("https_proxy").map(String::as_str), Some("http://127.0.0.1:7890"));
    }

    #[test]
    fn render_omits_optional_fields_when_absent() {
        let toml = render_config(&sample(None, None, None));
        let cfg: crate::config::Config = toml::from_str(&toml).unwrap();
        let p = cfg.providers.get("mycorp").unwrap();
        assert!(p.base_url.is_none());
        assert!(p.api_key.is_none());
        assert!(cfg.env.is_empty());
    }

    #[test]
    fn toml_escape_handles_quotes_and_backslashes() {
        assert_eq!(toml_escape(r#"a\b"c"#), r#"a\\b\"c"#);
    }

    #[test]
    fn render_escapes_api_key_special_chars() {
        let toml = render_config(&sample(None, Some(r#"k"x\y"#), None));
        let cfg: crate::config::Config = toml::from_str(&toml).unwrap();
        assert_eq!(
            cfg.providers.get("mycorp").unwrap().api_key.as_deref(),
            Some(r#"k"x\y"#)
        );
    }
}
