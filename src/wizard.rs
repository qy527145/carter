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
    /// 选中的模型：(端点模型名, models.dev 的 `provider/model`)。第一个是默认模型。
    models: Vec<(String, String)>,
    proxy: Option<String>,
    show_thinking: bool,
}

impl Answers {
    fn model_name(&self) -> &str {
        self.models.first().map(|(m, _)| m.as_str()).unwrap_or("")
    }
}

/// 预置协议：(kind, 展示名, 默认 provider 名, meta 的 provider 段, 默认模型名)。
const PRESETS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "anthropic",
        "Anthropic Messages",
        "anthropic",
        "anthropic",
        "claude-sonnet-4-5",
    ),
    (
        "openai_compat",
        "OpenAI 兼容 (Chat Completions：vLLM/LiteLLM/Azure/OpenRouter…)",
        "openai",
        "openai",
        "gpt-4o",
    ),
    (
        "openai_responses",
        "OpenAI Responses",
        "openai",
        "openai",
        "gpt-5",
    ),
    (
        "gemini",
        "Google Gemini",
        "gemini",
        "google",
        "gemini-2.5-pro",
    ),
    (
        "deepseek",
        "DeepSeek",
        "deepseek",
        "deepseek",
        "deepseek-chat",
    ),
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
        eprintln!(
            "在交互式终端运行 `carter` 可进入配置向导，或手动创建该文件（字段见 docs/config.example.toml）。"
        );
        return Ok(false);
    }

    println!("\n欢迎使用 Carter —— 未检测到配置，进入首次配置向导。");
    println!("（直接回车采用方括号内的默认值；Ctrl+C 取消。）\n");

    let answers = match collect().await {
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

    Ok(true)
}

/// 逐项收集答案。任一必填项遇 EOF → None（中止）。
///
/// 顺序：协议 → 端点 / key → 代理（要在联网前问）→ 元数据（没缓存就试着拉，拉不到用
/// 内置快照）→ 向端点要模型列表让用户**选**（拉不到再手填）→ 给每个模型配元数据
/// （先自动猜，猜不中让用户从 provider → 模型两级列表里挑）。
async fn collect() -> Option<Answers> {
    let idx = choose(
        "选择模型服务商协议",
        PRESETS.iter().map(|p| p.1).collect::<Vec<_>>().as_slice(),
        0,
    )?;
    let (kind, _label, def_name, meta_provider, def_model) = PRESETS[idx];

    let provider_name = prompt_default("provider 名称（引用与切换时用，如 mycorp）", def_name)?;
    let base_url = prompt_optional("base_url（自定义端点；回车=用官方默认端点）")?;
    let api_key =
        prompt_optional("api_key（直接粘贴；也可填 ${ENV_NAME} 走环境变量；回车=暂不填）")?;
    let proxy = prompt_optional("HTTP 代理（如 http://127.0.0.1:7890；回车=不走代理）")?;
    // 代理：写进 config 之前本进程就要用（拉 models.dev、拉模型列表）。
    if let Some(p) = &proxy {
        set_proxy_env(p);
    }

    // 元数据：有缓存用缓存；没有就试着拉一次，拉不到用内置快照（不阻塞向导）。
    let cache_json = ensure_models_cache().await;

    // 向端点要模型列表。key 是 ${ENV} 形式时先展开，否则带着字面量去请求必然 401。
    let pcfg = crate::config::ProviderConfig {
        kind: kind.to_string(),
        base_url: base_url.clone(),
        api_key: api_key
            .as_deref()
            .map(crate::config::interpolate::interpolate),
        ..Default::default()
    };
    print!("正在向端点拉取模型列表…");
    let _ = io::stdout().flush();
    let remote = match crate::provider::models_api::list_models(&pcfg).await {
        Ok(list) if !list.is_empty() => {
            println!(" 共 {} 个。", list.len());
            list
        }
        Ok(_) => {
            println!(" 列表为空。");
            Vec::new()
        }
        Err(e) => {
            println!(" 失败（{e}）。");
            Vec::new()
        }
    };

    // 选模型（可多选；第一个是默认模型）。
    let picked: Vec<String> = if remote.is_empty() {
        let m = prompt_default("模型名（端点实际模型名，同时作为引用块名）", def_model)?;
        vec![m]
    } else {
        let labels: Vec<String> = remote
            .iter()
            .map(|m| {
                if m.name == m.id {
                    m.id.clone()
                } else {
                    format!("{}  ({})", m.id, m.name)
                }
            })
            .collect();
        let default_idx = remote
            .iter()
            .position(|m| m.id == def_model || m.id.starts_with(def_model))
            .unwrap_or(0);
        let idxs = pick_many(
            "选择要使用的模型（可输入多个序号，逗号分隔；第一个是默认模型；也可直接输入模型名）",
            &labels,
            default_idx,
        )?;
        idxs.into_iter()
            .map(|sel| match sel {
                Pick::Index(i) => remote[i].id.clone(),
                Pick::Custom(name) => name,
            })
            .collect()
    };

    // 给每个模型配元数据：自动猜 → 让用户确认 / 从列表里挑。
    let mut models: Vec<(String, String)> = Vec::with_capacity(picked.len());
    for name in picked {
        let meta = pick_meta(&cache_json, meta_provider, &name)?;
        models.push((name, meta));
    }

    let show_thinking = yes_no("显示模型思考内容？", true)?;

    Some(Answers {
        provider_name,
        kind: kind.to_string(),
        base_url,
        api_key,
        models,
        proxy,
        show_thinking,
    })
}

/// 模型元数据：本地缓存 → 提议拉取 → 内置快照。返回最终可用的 JSON。
async fn ensure_models_cache() -> String {
    use crate::registry::fetch;
    if !fetch::cache_exists()
        && yes_no(
            "尚无模型元数据缓存，现在从 models.dev 拉取？（需联网，约数 MB；拉不到会用内置快照）",
            true,
        )
        .unwrap_or(false)
    {
        match fetch::fetch_models_dev().await {
            Ok(json) => match fetch::write_cache(&json) {
                Ok(()) => println!("已缓存到 {}", paths::models_cache_path().display()),
                Err(e) => eprintln!("写入缓存失败：{e}（先用内置快照，稍后可 `carter update`）"),
            },
            Err(e) => eprintln!("拉取失败：{e}\n先用内置快照，稍后可 `carter update` 刷新。"),
        }
    }
    fetch::read_cache().unwrap_or_else(|_| "{}".to_string())
}

/// 给一个端点模型名定 models.dev 元数据 id。
///
/// 先按协议对应的 provider 猜（`claude-sonnet-4-5-20250929` → `anthropic/claude-sonnet-4-5`），
/// 猜中了让用户回车确认；猜不中、或用户不认，就走 provider → 模型两级列表。
fn pick_meta(cache_json: &str, meta_provider: &str, model_name: &str) -> Option<String> {
    use crate::registry::catalog;
    println!("\n为模型 `{model_name}` 选择元数据（查上下文窗口 / 价格 / 能力）：");
    if let Some((p, m)) = catalog::guess_meta(cache_json, Some(meta_provider), model_name) {
        let guess = format!("{p}/{m}");
        if yes_no(&format!("  匹配到 `{guess}`，用它？"), true)? {
            return Some(guess);
        }
    } else {
        println!("  未能自动匹配。");
    }

    // 一级：provider。协议对应的那个放最前。
    let mut provs = catalog::providers(cache_json);
    if let Some(pos) = provs.iter().position(|p| p.id == meta_provider) {
        let p = provs.remove(pos);
        provs.insert(0, p);
    }
    if provs.is_empty() {
        // 元数据整个不可用（快照损坏）：退回手填。
        return prompt_default(
            "  models.dev 元数据 id（provider_id/model_id）",
            &format!("{meta_provider}/{model_name}"),
        );
    }
    let labels: Vec<String> = provs
        .iter()
        .map(|p| format!("{}  ({}，{} 个模型)", p.id, p.name, p.model_count))
        .collect();
    let pi = match pick_one("  选择元数据 provider（可输入序号或名字过滤）", &labels, 0)?
    {
        Pick::Index(i) => i,
        Pick::Custom(name) => {
            // 用户直接敲了个 id：按它查；没有就当作手填的完整 id。
            match provs.iter().position(|p| p.id == name) {
                Some(i) => i,
                None if name.contains('/') => return Some(name),
                None => {
                    println!("  没有叫 `{name}` 的 provider，按手填处理。");
                    return Some(format!("{meta_provider}/{model_name}"));
                }
            }
        }
    };
    let pid = provs[pi].id.clone();

    // 二级：模型。
    let models = catalog::models(cache_json, &pid);
    let labels: Vec<String> = models
        .iter()
        .map(|m| format!("{}  [{}]", m.id, m.summary()))
        .collect();
    // 默认项：和端点模型名最像的那个。
    let default_idx = catalog::guess_meta(cache_json, Some(&pid), model_name)
        .and_then(|(_, mid)| models.iter().position(|m| m.id == mid))
        .unwrap_or(0);
    match pick_one("  选择模型（可输入序号或名字过滤）", &labels, default_idx)? {
        Pick::Index(i) => Some(format!("{pid}/{}", models[i].id)),
        Pick::Custom(name) => Some(format!("{pid}/{name}")),
    }
}

/// 渲染为带注释的 config.toml（纯函数，便于测试）。
fn render_config(a: &Answers) -> String {
    let mut s = String::new();
    s.push_str("# Carter 配置 —— 由首次运行向导生成。可手动编辑；完整字段说明见 docs/config.example.toml。\n\n");

    s.push_str("[agent]\n");
    s.push_str("# 默认模型，引用格式 provider/model。可被 CLI --model 覆盖。\n");
    s.push_str(&format!(
        "model = \"{}/{}\"\n",
        a.provider_name,
        a.model_name()
    ));
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
        s.push_str(
            "# [env]\n# https_proxy = \"http://127.0.0.1:7890\"   # 需要代理时取消注释。\n\n",
        );
    }

    s.push_str(
        "# [debug]\n# log_requests = true   # 记录每次 LLM 请求到 ~/.carter/debug/llm_log/。\n\n",
    );

    s.push_str("# ---- 服务商：协议 + 端点 + 密钥 ----\n");
    s.push_str(&format!("[providers.{}]\n", a.provider_name));
    s.push_str(&format!("kind = \"{}\"\n", a.kind));
    match &a.base_url {
        Some(b) => s.push_str(&format!("base_url = \"{}\"\n", toml_escape(b))),
        None => s.push_str(
            "# base_url = \"https://your-endpoint/...\"   # 自定义端点；省略用官方默认。\n",
        ),
    }
    match &a.api_key {
        Some(k) => s.push_str(&format!("api_key = \"{}\"\n", toml_escape(k))),
        None => {
            s.push_str("# api_key = \"${YOUR_API_KEY}\"   # 建议用 ${ENV} 插值，避免硬编码。\n")
        }
    }
    s.push('\n');

    s.push_str("# 块名即引用里的 model；省略 api_name 时默认取块名做端点模型名。\n");
    s.push_str("# meta = models.dev 的 provider_id/model_id，用于查上下文窗口/价格。\n");
    for (name, meta) in &a.models {
        s.push_str(&format!(
            "  [providers.{}.models.{}]\n",
            a.provider_name,
            toml_key(name)
        ));
        s.push_str(&format!("  meta = \"{}\"\n\n", toml_escape(meta)));
    }
    s
}

/// TOML 表头里的键：裸键只允许 `A-Za-z0-9_-`，端点模型名里常有 `.` `/` `:`
/// （`gpt-4.1`、`anthropic/claude-sonnet-4-5`、`qwen:7b`），得加引号。
fn toml_key(k: &str) -> String {
    if !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        k.to_string()
    } else {
        format!("\"{}\"", toml_escape(k))
    }
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
    Some(if line.is_empty() {
        default.to_string()
    } else {
        line
    })
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

/// 列表选择的结果：选中某一项，或用户直接敲了个不在列表里的名字。
enum Pick {
    Index(usize),
    Custom(String),
}

/// 一页最多打印多少行；超过就让用户输入关键字过滤。
const PAGE: usize = 30;

/// 打印候选（过多时只打前 `PAGE` 条并提示过滤）。
fn print_candidates(items: &[String], default_idx: usize, filtered_from: Option<usize>) {
    let shown = items.len().min(PAGE);
    for (i, it) in items.iter().take(shown).enumerate() {
        let mark = if i == default_idx { "*" } else { " " };
        println!("  {mark} {:>3}. {it}", i + 1);
    }
    if items.len() > shown {
        println!(
            "    …… 还有 {} 项未显示，输入关键字过滤。",
            items.len() - shown
        );
    }
    if let Some(total) = filtered_from {
        println!(
            "    （已过滤：{} / {total}；输入空行回到全部）",
            items.len()
        );
    }
}

/// 单选，支持输入关键字过滤。回车 = 默认项；输入序号 = 选中；输入文本 =
/// 先当作过滤词（有命中就重新列出），没有任何命中则当作自定义值。EOF → None。
fn pick_one(label: &str, items: &[String], default_idx: usize) -> Option<Pick> {
    pick_many(label, items, default_idx).map(|mut v| v.remove(0))
}

/// 多选版：允许 `1,3,5` 这种序号列表；其余规则同 [`pick_one`]。
/// 返回非空（至少一个）。
fn pick_many(label: &str, items: &[String], default_idx: usize) -> Option<Vec<Pick>> {
    let all: Vec<(usize, String)> = items.iter().cloned().enumerate().collect();
    let mut view: Vec<(usize, String)> = all.clone();
    let mut default_in_view = default_idx.min(items.len().saturating_sub(1));
    println!("{label}：");
    loop {
        let labels: Vec<String> = view.iter().map(|(_, s)| s.clone()).collect();
        print_candidates(
            &labels,
            default_in_view,
            (view.len() != all.len()).then_some(all.len()),
        );
        print!("输入 [{}]: ", default_in_view + 1);
        let _ = io::stdout().flush();
        let line = read_line()?;
        let line = line.trim();

        if line.is_empty() {
            if view.len() != all.len() {
                // 过滤态下回车：回到全部。
                view = all.clone();
                default_in_view = default_idx.min(items.len().saturating_sub(1));
                continue;
            }
            return view
                .get(default_in_view)
                .map(|(orig, _)| vec![Pick::Index(*orig)]);
        }

        // 序号（可多个）。
        let nums: Vec<Option<usize>> = line
            .split([',', ' ', '，'])
            .filter(|t| !t.is_empty())
            .map(|t| t.parse::<usize>().ok())
            .collect();
        if !nums.is_empty() && nums.iter().all(Option::is_some) {
            let mut out: Vec<Pick> = Vec::new();
            for n in nums.into_iter().flatten() {
                match view.get(n.wrapping_sub(1)) {
                    Some((orig, _)) if n >= 1 => out.push(Pick::Index(*orig)),
                    _ => println!("（序号 {n} 超出范围，忽略）"),
                }
            }
            if !out.is_empty() {
                return Some(out);
            }
            continue;
        }

        // 文本：过滤；只命中一个就直接选它；一个都没有就当自定义值。
        let q = line.to_lowercase();
        let hits: Vec<(usize, String)> = all
            .iter()
            .filter(|(_, s)| s.to_lowercase().contains(&q))
            .cloned()
            .collect();
        match hits.len() {
            0 => {
                println!("（列表里没有匹配项，按自定义值 `{line}` 处理）");
                return Some(vec![Pick::Custom(line.to_string())]);
            }
            1 => return Some(vec![Pick::Index(hits[0].0)]),
            _ => {
                // 精确等于某项 id 开头（如用户直接敲了完整模型名）→ 直接选。
                if let Some((orig, _)) = hits.iter().find(|(_, s)| {
                    s.to_lowercase()
                        .split_whitespace()
                        .next()
                        .is_some_and(|head| head == q)
                }) {
                    return Some(vec![Pick::Index(*orig)]);
                }
                view = hits;
                default_in_view = 0;
            }
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
            models: vec![(
                "claude-sonnet-4-6".into(),
                "anthropic/claude-sonnet-4-5".into(),
            )],
            proxy: proxy.map(str::to_string),
            show_thinking: true,
        }
    }

    #[test]
    fn render_multiple_models_and_quotes_odd_keys() {
        let mut a = sample(None, None, None);
        a.models.push(("gpt-4.1".into(), "openai/gpt-4.1".into()));
        a.models.push((
            "anthropic/claude-haiku-4-5".into(),
            "anthropic/claude-haiku-4-5".into(),
        ));
        let toml = render_config(&a);
        let cfg: crate::config::Config = toml::from_str(&toml).expect("带点/斜杠的块名应加引号");
        let p = cfg.providers.get("mycorp").unwrap();
        assert_eq!(p.models.len(), 3);
        assert_eq!(p.models["gpt-4.1"].meta, "openai/gpt-4.1");
        assert_eq!(
            p.models["anthropic/claude-haiku-4-5"].meta,
            "anthropic/claude-haiku-4-5"
        );
        // 第一个是默认模型。
        assert_eq!(cfg.agent.model, "mycorp/claude-sonnet-4-6");
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
        assert_eq!(
            p.base_url.as_deref(),
            Some("https://gw.example.com/anthropic/v1/")
        );
        assert_eq!(p.api_key.as_deref(), Some("sk-abc"));
        let m = p.models.get("claude-sonnet-4-6").expect("模型块应存在");
        assert_eq!(m.meta, "anthropic/claude-sonnet-4-5");
        assert_eq!(
            cfg.env.get("https_proxy").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
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
