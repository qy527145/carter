# NDJSON 协议 — host driver 嵌入接口

`carter --ndjson` 启动后通过 stdin/stdout 双向 JSON 流协议与 host 程序通信。
适合嵌入到 **VSCode 扩展 / 服务端 / 自动化脚本** 等需要程序化驱动 agent 的场景。

每行 = 一个完整 JSON 对象（`\n` 分隔，UTF-8）。

---

## 启动

```bash
carter --ndjson [--model provider/name] [--resume <id>] [--continue]
```

启动后 carter 立刻发一条 `Ready` 事件，host 收到后即可送 `user_prompt`。

---

## host → carter (stdin Commands)

| Command | 字段 | 说明 |
|---|---|---|
| `user_prompt` | `text: string` | 提交一条 user prompt 给 agent loop（== TUI 回车） |
| `cancel` | — | 取消当前流式请求（== TUI Esc） |
| `set_model` | `model: string` | **暂不支持热切换**，会发 Notice 提示用 `--model` 重启 |
| `ask_response` | `id: u64`, `answer: string` | 回答之前 `ask_user` 事件，id 必须对应 |
| `stop` | — | 优雅退出 |

示例：
```json
{"type":"user_prompt","text":"列出 src 下所有 Rust 文件"}
{"type":"cancel"}
{"type":"ask_response","id":1,"answer":"yes"}
{"type":"stop"}
```

---

## carter → host (stdout Events)

| Event | 字段 | 说明 |
|---|---|---|
| `ready` | `session_id`, `model`, `cwd`, `resumed: bool` | 启动完成，host 可开始送 prompt |
| `text_delta` | `text` | assistant 正文流式增量 |
| `thinking_delta` | `text` | 思考增量（启用 show_thinking） |
| `text_end` | — | 一个 assistant 文本块结束 |
| `tool_call_started` | `name`, `args_preview` | 工具调用前 |
| `tool_result` | `ok: bool`, `summary` | 工具执行后单行预览 |
| `todo_updated` | `todos: [{status, content, active_form}]` | todo 列表刷新 |
| `notice` | `message` | 系统通知（压缩日志等） |
| `title` | `title` | 会话标题（fast 模型生成，一次） |
| `model_changed` | `model` | 当前模型变更 |
| `divider` | `label` | 上下文边界（"已恢复会话"等） |
| `idle` | — | 一轮处理结束 — host 据此显示"可输入" |
| `ask_user` | `id`, `question`, `options: string[]` | 反向 RPC，host **必须**回 `ask_response` |
| `turn_usage` | `usage: {input,output,cache_read,cache_write,reasoning}`, `cost`, `model` | 用量 + 成本 |
| `error` | `message` | 致命错误（之后 carter 退出） |

---

## 反向 RPC：AskUser

模型主动调用 `ask_user_question` 工具时，carter 发 `ask_user` 事件给 host：

```json
{"type":"ask_user","id":1,"question":"覆盖 README.md 吗？","options":["yes","no","只看 diff"]}
```

host **必须**回复，否则工具会一直 await：

```json
{"type":"ask_response","id":1,"answer":"yes"}
```

`answer` 可以是 options 中的一项，也可以是任意自由文本（如用户在你的 UI 里手输的）。

---

## 完整会话示例（Node.js host）

```js
import { spawn } from 'node:child_process';
import readline from 'node:readline';

const carter = spawn('carter', ['--ndjson', '--model', 'ws/sonnet']);
const reader = readline.createInterface({ input: carter.stdout });

reader.on('line', line => {
  const ev = JSON.parse(line);
  switch (ev.type) {
    case 'ready':
      console.log(`[ready] session=${ev.session_id} model=${ev.model}`);
      carter.stdin.write(JSON.stringify({
        type: 'user_prompt',
        text: '帮我列一下 src/ 下所有 .rs 文件'
      }) + '\n');
      break;
    case 'text_delta':
      process.stdout.write(ev.text);
      break;
    case 'tool_call_started':
      console.log(`\n[tool] ${ev.name}(${ev.args_preview})`);
      break;
    case 'ask_user':
      // 在你的 UI 里弹窗，拿到 answer 后：
      carter.stdin.write(JSON.stringify({
        type: 'ask_response', id: ev.id, answer: 'yes'
      }) + '\n');
      break;
    case 'idle':
      console.log('\n[ready for next prompt]');
      break;
  }
});

// 用户决定退出时：
// carter.stdin.write(JSON.stringify({type:'stop'}) + '\n');
```

---

## 行为对齐

NDJSON 模式与 TUI 共用同一套 agent loop，因此以下行为完全一致：

- 工具并发执行（safe 工具同轮并发派发）
- 自动上下文压缩（L0 ImageStrip + L1 elide + L2/L3 summary）
- Hook 11 事件触发（session-start/end、pre/post-turn、pre/post-tool-use 等）
- 子代理 (`task` 工具) + MCP 工具透传
- 真实 token 计数（tiktoken-rs）
- 多模态图片（`@路径.png` 或 `read_file` 都会自动入库 + downsample）
- 文件 checkpoint 持久化（resume 后 `/rewind` 仍可用）
- Prompt caching（Anthropic 2 个 cache breakpoint）

---

## 调试

- carter 内部日志写到 `~/.carter/carter.log`（设 `RUST_LOG=debug`）
- LLM 请求日志 `~/.carter/debug/llm_log/<date>.jsonl`（设 `[debug] log_requests = true`）
- host 端协议错误：carter 会发 `notice` 提示但不退出，便于排错
