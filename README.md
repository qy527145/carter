# Carter

Rust 写的终端 agent：一个二进制，既是交互式 TUI，也是可以写进脚本的 CLI。
多 provider（Anthropic / OpenAI 系 / Gemini / Ollama / OpenRouter …）、本地工具
（bash / 文件读写 / 内置 ripgrep）、子 agent 并发、MCP、Skills、多层记忆、
生命周期 Hooks、会话持久化与回滚。

```bash
carter                                  # 进交互式 TUI
carter "把 src 下所有 TODO 列出来"        # 一次性跑完退出
carter --goal --prompt-file task.md     # 无人值守跑长任务，模型停下自动推进
```

## 安装

预编译二进制（macOS / Linux / Windows）：

```bash
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/qy527145/carter/releases/latest/download/carter-installer.sh | sh
# Windows PowerShell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/qy527145/carter/releases/latest/download/carter-installer.ps1 | iex"
```

从 crates.io 编译：

```bash
cargo install carter
```

首次运行 `carter` 没有配置时会进入向导：选协议、填端点和 API key 之后，carter 会向端点
拉取模型列表让你从里面选（可多选，拉不到再手填），每个模型的元数据（上下文窗口、价格、
能力）先按名字自动匹配，匹配不上再从 provider → 模型两级列表里挑。写到
`~/.carter/config.toml`。也可以直接照 [config.example.toml](config.example.toml) 手写。
非交互环境（管道、CI）不会进向导，只打印指引后退出。

最小配置：

```toml
[agent]
model = "my/sonnet"                      # provider/model

[providers.my]
kind = "anthropic"                       # anthropic / openai / openai_responses / gemini / ollama / ...
base_url = "https://api.anthropic.com/v1/"
api_key = "${ANTHROPIC_API_KEY}"         # 支持 ${ENV} 展开

  [providers.my.models.sonnet]
  meta = "anthropic/claude-sonnet-4-5"   # models.dev 里的键，用来取上下文窗口与价格
  api_name = "claude-sonnet-4-5"         # 实际发给 API 的名字（缺省同 meta 的后半段）
```

模型元数据来自 [models.dev](https://models.dev)。二进制里内置了一份发版时的快照，
离线也能跑；`carter update` 会把最新的拉到 `~/.carter/models.json`，有缓存就优先用缓存。

## TUI 用法

不带 prompt 直接运行 `carter` 进入 REPL。

### 快捷键

| 键 | 作用 |
|---|---|
| `Enter` | 发送；一轮进行中按下则作为**插话**，在下一次模型请求里带上 |
| `Shift+Enter` | 换行（需终端支持 Kitty 键盘协议） |
| `↑` / `↓` | 浏览跨会话的输入历史 |
| `/` | 弹出命令补全 |
| `@` | 弹出文件补全；`@图片.png` 自动作为多模态附件 |
| `Esc` | 取消当前流式请求 |
| `Ctrl+C` 两次 / `Ctrl+D` | 退出 |

### 斜杠命令

| 命令 | 说明 |
|---|---|
| `/model` | 弹出模型列表选择：配置里的模型（带上下文窗口 / 能力摘要）+ 各端点实时返回的模型；也可直接 `/model <provider/model>` |
| `/goal <目标>` | 设定长任务目标：模型停下而目标未完成时自动追加「继续」再跑一轮 |
| `/goal` · `/goal pause` · `/goal resume` · `/goal clear` | 查看 / 暂停 / 恢复 / 清除目标 |
| `/btw <问题>` | 侧边问一句：带着主对话的上下文，但不污染主对话 |
| `/compact` | 手动压缩上下文（超过阈值时会自动压） |
| `/context` · `/cost` | 查看上下文占用 / 本会话用量与成本 |
| `/rewind [n]` | 回滚文件改动到第 n 个检查点之前（每次写文件前自动打点） |
| `/new` · `/resume` · `/fork` | 开新会话 / 恢复历史会话 / 从当前会话派生 |
| `/clear` | 清空上下文（磁盘原文保留） |
| `/skills` · `/mcp` | 列出 skills / 查看 MCP server 状态 |
| `/help` · `/quit` | 命令列表 / 退出 |

`~/.carter/commands/<name>.md`（全局）或项目目录下 `.carter/commands/<name>.md` 可以定义自定义斜杠命令。

### 长任务：`/goal`

普通一轮在模型给出一条不带工具调用的回复后就结束。长任务里模型经常做到一半停下来
汇报进度，或者问「要继续吗」。设了目标之后 carter 会接管这个「继续」：

- 目标正文挂到系统提示末尾，模型知道做完要调 `goal_update` 报告；
- 模型停下而目标未完成时自动再推一轮，默认最多 20 次；
- 模型报 completed / blocked / needs_user、连续两轮没调工具（空转）、本轮出错，
  都会停下并告诉你原因；
- 关掉终端时目标处于进行中，下次打开会降级为暂停，`/goal resume` 接着推。

## CLI 用法

给了 prompt（或 `--no-tui`）就走一次性模式：跑完这条 prompt 的完整 agent 循环后退出。
脚本、批处理、评测 harness 都用这条路。完整说明见 [docs/cli.md](docs/cli.md)。

```bash
carter "把 src 下所有 TODO 列出来"
carter --prompt-file task.md                # 长 prompt 放文件
cat task.md | carter -                      # `-` 从 stdin 读
```

### 常用参数

| 参数 | 说明 |
|---|---|
| `--goal` | 目标模式，和 TUI 的 `/goal` 同一套状态机；无人值守跑长任务用它 |
| `--max-continues <N>` | 目标模式自动继续次数上限（默认 20） |
| `--max-turns <N>` | 覆盖配置的模型调用轮数上限（默认 50，自动继续的轮次也计入） |
| `--timeout <SECS>` | 总时长上限，到点取消并以 124 退出 |
| `--output-format text\|json\|stream-json` | 输出形态，见下 |
| `--model <provider/model>` | 覆盖默认模型 |
| `--cwd <DIR>` | 工作目录 |
| `--no-memory` · `--no-thinking` · `--no-color` | 不注入记忆 / 不输出思考 / 不上色 |
| `-c` · `-r <ID>` · `--fork <ID>` · `--session-id <ID>` | 续接最近会话 / 续接指定会话 / 派生 / 指定新会话 id |
| `--ndjson` | 双向 NDJSON 协议，供 IDE 扩展等 host 程序驱动，见 [docs/ndjson.md](docs/ndjson.md) |

### 输出形态

`text`（默认）边跑边打印；stdout 不是终端时自动去掉颜色。

`json` 过程静默，结束时 stdout 只有一行结果：

```json
{"type":"result","outcome":"assistant","session_id":"…","model":"my/sonnet",
 "result":"最后一段 assistant 正文","turns":9,"duration_ms":29414,
 "usage":{"input":26,"output":569,"cache_read":71456,"cache_write":9583,"reasoning":0},
 "cost_usd":0.07369,
 "goal":{"status":"completed","reason":"目标已完成","note":"模型写的说明","auto_continues":3}}
```

`outcome` 与退出码：`assistant` 0、`limit`（触顶 max_turns）0、`cancelled` 130、
`timeout` 124、`error` 1。`goal` 字段只在 `--goal` 时出现。

`stream-json` 每个事件一行 NDJSON，最后一行是上面那个 `result`。

### 一个评测脚本的样子

```bash
carter --goal --max-turns 300 --timeout 3600 \
       --output-format json --cwd ./workspace --prompt-file task.md \
  > result.json; echo "exit=$?"
jq -r '.outcome, .goal.status, .turns, .cost_usd' result.json
```

一次性模式没有人在那头：`ask_user_question` 工具不注册，模型会自己做判断；
会话照常落盘，可以用 `carter sessions` 和 `carter -r <id>` 回看。

### 其它子命令

```bash
carter update              # 拉取 models.dev 模型元数据
carter sessions [--all]    # 列出当前目录（或全部）会话
carter gc [--all]          # 压实会话文件
carter completion zsh      # 输出 shell 补全脚本
```

## 能力一览

**工具**：`bash`（可后台执行、进程组超时清理、Windows 下按 OEM 代码页解码）、
`read_file` / `write_file` / `edit_file`、`glob`、`grep`（内置 ripgrep，遵守 .gitignore）、
`todo_write`、`save_memory`、`task`（并发派子 agent，带进度）、`ask_user_question`、
`goal_update`，以及 MCP server 透明并入的工具。`[agent].disabled_tools` 可按名关掉。

**多模态**：`@图片.png` 或剪贴板粘贴图片，自动 downsample 控制 token 成本。

**上下文管理**：真实 token 计数；超过 `context_window * 0.75` 自动压缩，压缩用
`fast_model`；`/rewind` 基于写前检查点回滚文件。

**多层记忆**：`~/.carter/CARTER.md`（全局）+ 从 cwd 上溯到 git 根每一层的
`CARTER.md` / `AGENTS.md`，作为一段 system 注入。

**Skills**：`~/.carter/skills/<name>/SKILL.md`，目录注入系统提示做发现，正文按需加载。

**MCP**：stdio 与 streamable-http 两种 transport，`[mcp.servers.X]` 配置，
`enabled = false` 可临时停用。

**Hooks**：11 个生命周期事件（session-start、pre-tool-use、post-turn、pre-compact …），
子进程通过 stdin 收 JSON payload，退出码 2 阻断，stdout 输出 JSON 改写参数。
详见 [config.example.toml](config.example.toml)。

**服务端工具**：Anthropic 的 web_search / web_fetch / code_execution，
OpenAI Responses 的 web_search / file_search，按 provider 在 `server_tools` 里开。

## 文件布局

| 路径 | 内容 |
|---|---|
| `~/.carter/config.toml` | 配置 |
| `~/.carter/models.json` | models.dev 元数据缓存 |
| `~/.carter/projects/` | 会话（JSONL，按项目目录分） |
| `~/.carter/goals/` | 长任务目标状态 |
| `~/.carter/skills/` | Skills |
| `~/.carter/CARTER.md` | 全局记忆 |
| `~/.carter/carter.log` | 日志（`RUST_LOG` 控制级别） |
| `~/.carter/debug/llm_log/` | `[debug] log_requests = true` 时的请求抓包 |

设 `CARTER_HOME` 可以整体换目录。

## 作为库使用

`carter` 同时是一个 crate：`carter::agent` / `carter::provider` 这几层不依赖终端，
GUI 或服务端宿主可以直接嵌入，自己实现 `UiSink` 消费事件。`run_cli` 只是其中一个宿主。

## 开发

```bash
cargo build
cargo test
cargo clippy --all-targets
```

发布：改 `Cargo.toml` 版本，打不带 `v` 的 tag（如 `0.2.2`），push 后
[cargo-dist](https://github.com/axodotdev/cargo-dist) 的 Release 工作流出五个平台的产物，
再 `cargo publish`。

## License

Apache-2.0
