# CLI 一次性模式 — 脚本化 / 评测接口

`carter` 不带参数进交互式 TUI；给了 prompt（或 `--no-tui`）就走**一次性模式**：
跑完这一条 prompt 的完整 agent 循环（模型调工具 → 执行 → 回灌 → 再调……）后退出。
评测 harness、批处理脚本都用这条路。

```bash
carter "把 src 下所有 TODO 列出来"                 # 位置参数
carter --prompt-file task.md                       # 长任务描述放文件
cat task.md | carter -                             # `-` = 从 stdin 读
```

## 常用参数

| 参数 | 说明 |
|---|---|
| `--prompt-file <PATH>` | 从文件读 prompt，与位置参数二选一 |
| `--output-format text\|json\|stream-json` | 输出形态，见下 |
| `--max-turns <N>` | 覆盖配置 `agent.max_turns`（单条 prompt 的模型调用轮数上限，默认 50）。长任务记得调高 |
| `--timeout <SECS>` | 总时长上限；到点取消并以退出码 124 退出 |
| `--cwd <DIR>` | 工作目录（会话、CARTER.md 记忆、工具相对路径都以它为准） |
| `--model <provider/model>` | 覆盖配置里的模型 |
| `--no-memory` | 不注入 CARTER.md / AGENTS.md |
| `--no-thinking` | 不输出思考内容 |
| `--no-color` | 文本输出不带 ANSI 颜色（stdout 非终端或设了 `NO_COLOR` 时自动关） |
| `--session-id <ID>` / `-c` / `-r <ID>` | 指定会话 id / 续接最近会话 / 续接指定会话 |

## 输出形态

**`text`**（默认）：边跑边打印，工具调用、结果、正文、用量都在 stdout。
管道 / 重定向时自动去掉颜色。

**`json`**：过程静默，结束时 stdout 只有一行 JSON 结果对象：

```json
{"type":"result","outcome":"assistant","session_id":"…","model":"anthropic/claude-sonnet-4-5",
 "result":"最后一段 assistant 正文","turns":7,"duration_ms":48213,
 "usage":{"input":1234,"output":567,"cache_read":0,"cache_write":8901,"reasoning":0},
 "cost_usd":0.0412}
```

`outcome` 取值：

| 值 | 含义 | 退出码 |
|---|---|---|
| `assistant` | 模型以普通回复收尾 | 0 |
| `limit` | 触顶 `max_turns`，工具调用被截断 | 0 |
| `cancelled` | Ctrl+C | 130 |
| `timeout` | 超过 `--timeout`，附 `error` 字段 | 124 |
| `error` | 致命错误（provider 连不上等），附 `error` 字段 | 1 |

**`stream-json`**：每个事件一行 NDJSON（事件类型与 [ndjson.md](ndjson.md) 的 Event 一致），
最后一行是上面那个 `result` 对象。适合既要过程又要汇总的评测日志。

## 评测时的注意点

- 一次性模式没有人在那头：模型调 `ask_user_question` 时自动选每题第一个选项；
  没有后台命令表、没有 `/goal` 自动推进。
- 会话照常落盘到 `~/.carter/projects/`，可以用 `carter sessions` / `-r` 回看；
  `--session-id` 能让评测脚本自己指定 id 方便对账。
- 配置文件 `~/.carter/config.toml` 不存在且 stdin 不是终端时不会进向导，直接报错退出。
