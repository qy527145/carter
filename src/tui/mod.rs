//! TUI 层 —— 交互式 REPL：可滚动历史 + 输入框 + 状态栏 + 流式增量渲染 + 流式中断。
//! 隔离纪律：本模块是**唯一**允许 import `ratatui`/`crossterm` 的地方；
//! 不得 import `genai::*`。与 agent 层经 `UiEvent` 通道 + `CancelToken` 通信。

use std::io;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as CtEvent, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use unicode_width::UnicodeWidthStr;

use crate::agent::ui::{UiEvent, UiSink};
use crate::agent::CancelToken;
use crate::provider::Usage;
use crate::tools::{TodoItem, TodoStatus};

/// 输入框最大行数（超出则框内滚动）；viewport 总高据此固定。
const MAX_INPUT_ROWS: u16 = 8;
/// inline viewport 总高 = 预览 1 + 输入框(最大 + 上下边框 2) + 状态栏 1。
const VIEWPORT_HEIGHT: u16 = MAX_INPUT_ROWS + 4;

/// 终端 RAII guard —— inline 模式：仅进 raw mode + bracketed paste，drop 时恢复。
/// 不进 alternate screen、不捕获鼠标，使终端原生滚动/选择/复制可用，退出后历史留存。
/// 另推送 keyboard enhancement（DISAMBIGUATE_ESCAPE_CODES），让支持 Kitty 协议的终端
/// 能区分 Shift+Enter 与 Enter（用于多行输入换行）；不支持的终端自动忽略。
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(out, EnableBracketedPaste)?;
        // 尝试开启增强键盘上报；失败（终端不支持）不致命，忽略。
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = execute!(out, PopKeyboardEnhancementFlags);
        let _ = execute!(out, DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
}

/// 把 `UiEvent` 投递到 TUI 的 sink（agent 任务持有）。
pub struct ChannelSink {
    tx: UnboundedSender<UiEvent>,
}

impl ChannelSink {
    pub fn new(tx: UnboundedSender<UiEvent>) -> Self {
        Self { tx }
    }
}

impl UiSink for ChannelSink {
    fn emit(&mut self, ev: UiEvent) {
        // 接收端已关闭（TUI 退出）时忽略——agent 任务即将随之结束。
        let _ = self.tx.send(ev);
    }
}

/// 历史区可渲染块。
#[derive(Debug, Clone)]
enum Block_ {
    User(String),
    /// 累积中的 assistant 正文（流式追加）。
    Assistant(String),
    Thinking(String),
    Tool { ok: Option<bool>, line: String },
    Todos(Vec<TodoItem>),
    Notice(String),
    /// 带标签的分隔线（上下文边界可视化）。
    Divider(String),
}

/// 流式块种类（区分 assistant 正文与 thinking）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind_ {
    Assistant,
    Thinking,
}

/// 斜杠命令补全候选（供菜单展示）。由 main 用内置 + 自定义命令组装后传入。
#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub name: String,
    pub description: String,
    pub hint: Option<String>,
}

/// 补全模式：`/` 命令、`@` 文件、或命令的参数（如 `/model <provider/model>`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionKind {
    Slash,
    File,
    Arg,
}

/// 补全菜单的一条候选（已算好的展示/插入数据）。
#[derive(Debug, Clone)]
struct CompletionEntry {
    /// 插入值（不含触发符 `/`、`@`），如 `clear` 或 `src/main.rs`。
    value: String,
    description: String,
    hint: Option<String>,
}

/// 补全弹窗状态。
#[derive(Debug, Clone)]
struct CompletionState {
    kind: CompletionKind,
    /// 触发符（`/` 或 `@`）在 `input` 中的起始字节偏移。
    start: usize,
    entries: Vec<CompletionEntry>,
    selected: usize,
}

/// 状态栏累计数据。
#[derive(Debug, Default, Clone)]
struct StatusLine {
    model: String,
    total_in: u64,
    total_out: u64,
    total_cost: f64,
    /// 会话标题（fast 模型生成，首条 prompt 后一次）。
    title: Option<String>,
}

/// TUI 应用状态。纯逻辑（apply 可单测），不持有终端句柄。
/// inline 模式：定稿块进 `outbox` 待 flush 进 scrollback；流式块累积在 `pending`。
pub struct App {
    /// 已定稿、待 flush 进终端 scrollback 的块（FIFO）。
    outbox: Vec<Block_>,
    /// 当前正在流式累积的块（assistant/thinking），含未满一行的残行。
    pending: Option<Block_>,
    input: String,
    /// 输入光标的字节偏移（始终落在 char 边界）。
    cursor: usize,
    status: StatusLine,
    streaming: bool,
    should_quit: bool,
    /// 上一帧输入框内容区（用于放置光标）。
    input_inner: Rect,
    /// 上一帧输入框的垂直滚动行数（内容超高时滚动以保持光标可见）。
    input_scroll: u16,
    /// 是否已收到一次 Ctrl+C（用于双击强退判定）。
    armed_quit: bool,
    /// 已提交过的 prompt 历史（供 Up/Down 调阅，最新在末尾）。
    sent: Vec<String>,
    /// 当前在 sent 中的浏览位置：None = 未浏览（停在实时输入）；Some(i) = sent[i]。
    sent_pos: Option<usize>,
    /// 进入历史浏览前的实时输入草稿（浏览结束后恢复）。
    draft: String,
    /// 斜杠命令补全候选全集（内置 + 自定义）。
    commands: Vec<CompletionItem>,
    /// 当前补全弹窗状态（None = 未激活）。
    completion: Option<CompletionState>,
    /// 工作目录（用于 `@` 文件补全的相对路径与扫描根）。
    cwd: std::path::PathBuf,
    /// `@` 文件补全的惰性扫描缓存（首次 `@` 时填充）。
    file_cache: Option<Vec<String>>,
    /// 命令参数补全候选（静态）：命令名 → 候选项（如 model→各模型引用）。
    arg_completions: Vec<(String, Vec<CompletionItem>)>,
    /// 会话候选动态来源（resume/fork 用）：每次打开弹窗重新拉取，保证新建会话即时可见。
    session_candidates: Option<Box<dyn Fn() -> Vec<CompletionItem> + Send>>,
    /// 当前参数补全的候选缓存（命令名 → 候选），打开弹窗时取一次，过滤期间复用。
    arg_cache: Option<(String, Vec<CompletionItem>)>,
}

impl App {
    fn new(model: String) -> Self {
        Self {
            outbox: Vec::new(),
            pending: None,
            input: String::new(),
            cursor: 0,
            status: StatusLine {
                model,
                ..Default::default()
            },
            streaming: false,
            should_quit: false,
            input_inner: Rect::new(0, 0, 0, 0),
            input_scroll: 0,
            armed_quit: false,
            sent: Vec::new(),
            sent_pos: None,
            draft: String::new(),
            commands: Vec::new(),
            completion: None,
            cwd: std::path::PathBuf::from("."),
            file_cache: None,
            arg_completions: Vec::new(),
            session_candidates: None,
            arg_cache: None,
        }
    }

    /// 注入补全候选（命令菜单）。
    fn with_commands(mut self, commands: Vec<CompletionItem>) -> Self {
        self.commands = commands;
        self
    }

    /// 注入命令参数补全候选（静态，如 /model）。
    fn with_arg_completions(mut self, arg: Vec<(String, Vec<CompletionItem>)>) -> Self {
        self.arg_completions = arg;
        self
    }

    /// 注入会话候选动态来源（/resume·/fork）。
    fn with_session_candidates(
        mut self,
        f: Box<dyn Fn() -> Vec<CompletionItem> + Send>,
    ) -> Self {
        self.session_candidates = Some(f);
        self
    }

    /// 预载跨会话输入历史（上下方向键召回；最新在末尾）。
    fn with_history(mut self, history: Vec<String>) -> Self {
        self.sent = history;
        self
    }

    /// 设置工作目录（`@` 文件补全用）。
    fn with_cwd(mut self, cwd: std::path::PathBuf) -> Self {
        self.cwd = cwd;
        self
    }

    /// 消费一个来自 agent 的 UiEvent，更新状态。纯逻辑，便于测试。
    /// 定稿块写入 `outbox`（待 flush 进 scrollback）；流式 delta 累积进 `pending`，
    /// 整行（`\n` 之前）切出定稿，残行留 pending 供 viewport 预览。
    fn apply(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::AssistantTextDelta(t) => self.stream_delta(t, Kind_::Assistant),
            UiEvent::ThinkingDelta(t) => self.stream_delta(t, Kind_::Thinking),
            UiEvent::AssistantTextEnd => self.flush_pending(),
            UiEvent::ToolCallStarted { name, args_preview } => {
                self.flush_pending();
                self.outbox.push(Block_::Tool {
                    ok: None,
                    line: format!("⚙ {name}({args_preview})"),
                });
            }
            UiEvent::ToolResult { ok, summary } => {
                self.flush_pending();
                let mark = if ok { "✓" } else { "✗" };
                self.outbox.push(Block_::Tool {
                    ok: Some(ok),
                    line: format!("{mark} {summary}"),
                });
            }
            UiEvent::TodoUpdated(todos) => {
                self.flush_pending();
                self.outbox.push(Block_::Todos(todos));
            }
            UiEvent::Notice(msg) => {
                self.flush_pending();
                self.outbox.push(Block_::Notice(msg));
            }
            UiEvent::Title(t) => {
                // 标题只进状态栏，不落入对话历史（避免与上下文混在一起）。
                self.status.title = Some(t);
            }
            UiEvent::ModelChanged(m) => {
                // 状态栏权威模型来源：启动 / 切换时立即更新。
                self.status.model = m;
            }
            UiEvent::Divider(label) => {
                self.flush_pending();
                self.outbox.push(Block_::Divider(label));
            }
            UiEvent::ReplayHistory(items) => {
                self.flush_pending();
                for it in items {
                    let block = match it.role {
                        crate::agent::ReplayRole::User => Block_::User(it.text),
                        crate::agent::ReplayRole::Assistant => Block_::Assistant(it.text),
                        crate::agent::ReplayRole::ToolCall => Block_::Tool {
                            ok: None,
                            line: it.text,
                        },
                        crate::agent::ReplayRole::ToolResult => Block_::Tool {
                            ok: Some(true),
                            line: it.text,
                        },
                    };
                    self.outbox.push(block);
                }
            }
            UiEvent::Idle => {
                // 一轮 / 斜杠命令处理结束：无论是否产生 usage，都恢复可交互。
                self.flush_pending();
                self.streaming = false;
            }
            UiEvent::TurnUsage { usage, cost, model: _ } => {
                self.flush_pending();
                // 模型名由 ModelChanged 权威维护，这里不覆盖（避免「请求后才同步」）。
                self.accumulate(&usage, cost);
                self.streaming = false;
            }
        }
    }

    /// 累积一段流式文本到 `pending`（类型不符时先定稿旧 pending）。
    /// 累积后把所有完整行切出定稿进 outbox，残行留在 pending。
    fn stream_delta(&mut self, t: String, kind: Kind_) {
        // 类型切换：先把旧 pending 定稿。
        let same_kind = matches!(
            (&self.pending, kind),
            (Some(Block_::Assistant(_)), Kind_::Assistant)
                | (Some(Block_::Thinking(_)), Kind_::Thinking)
        );
        if !same_kind {
            self.flush_pending();
        }
        // 取出（或新建）当前累积缓冲。
        let buf = match self.pending.take() {
            Some(Block_::Assistant(s)) | Some(Block_::Thinking(s)) => s,
            _ => String::new(),
        };
        let mut buf = buf;
        buf.push_str(&t);

        // 切出完整行（保留末尾残行）。
        while let Some(nl) = buf.find('\n') {
            let line: String = buf.drain(..=nl).collect();
            let line = line.trim_end_matches('\n').to_string();
            self.outbox.push(match kind {
                Kind_::Assistant => Block_::Assistant(line),
                Kind_::Thinking => Block_::Thinking(line),
            });
        }
        // 残行回填 pending（空残行也保留块类型，便于后续累积）。
        self.pending = Some(match kind {
            Kind_::Assistant => Block_::Assistant(buf),
            Kind_::Thinking => Block_::Thinking(buf),
        });
    }

    /// 把 pending 残行定稿进 outbox（非空才写），清空 pending。
    fn flush_pending(&mut self) {
        if let Some(b) = self.pending.take() {
            let empty = matches!(&b, Block_::Assistant(s) | Block_::Thinking(s) if s.is_empty());
            if !empty {
                self.outbox.push(b);
            }
        }
    }

    fn accumulate(&mut self, usage: &Usage, cost: f64) {
        self.status.total_in += usage.input;
        self.status.total_out += usage.output;
        self.status.total_cost += cost;
    }

    fn push_user(&mut self, text: String) {
        self.flush_pending();
        self.outbox.push(Block_::User(text));
    }

    // ---- 输入行编辑（cursor 为 char 边界的字节偏移）----

    /// 在光标处插入字符。
    fn input_insert(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// 删除光标前一个字符（Backspace）。
    fn input_backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.input.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    /// 删除光标后一个字符（Delete）。
    fn input_delete(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        let next = self.input[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.cursor);
        self.input.replace_range(self.cursor..next, "");
    }

    /// 光标左移一个字符。
    fn input_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    /// 光标右移一个字符。
    fn input_right(&mut self) {
        if let Some(c) = self.input[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    fn input_home(&mut self) {
        self.cursor = 0;
    }

    fn input_end(&mut self) {
        self.cursor = self.input.len();
    }

    /// 清空输入（提交/取消后）。
    fn input_clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    /// 记录一条已提交的 prompt，并重置历史浏览状态。
    fn push_sent(&mut self, text: String) {
        // 去掉与上一条完全相同的连续重复。
        if self.sent.last().map(|s| s.as_str()) != Some(text.as_str()) {
            self.sent.push(text);
        }
        self.sent_pos = None;
        self.draft.clear();
    }

    /// 切换输入为某段历史/草稿文本，光标置末尾。
    fn set_input(&mut self, text: String) {
        self.input = text;
        self.cursor = self.input.len();
    }

    /// Up：向更早的历史移动。
    fn history_prev(&mut self) {
        if self.sent.is_empty() {
            return;
        }
        let next = match self.sent_pos {
            None => {
                // 首次进入浏览：保存当前实时输入为草稿。
                self.draft = self.input.clone();
                self.sent.len() - 1
            }
            Some(0) => 0, // 已在最早一条，保持。
            Some(i) => i - 1,
        };
        self.sent_pos = Some(next);
        let text = self.sent[next].clone();
        self.set_input(text);
    }

    /// Down：向更晚的历史移动；越过最新一条则回到草稿。
    fn history_next(&mut self) {
        let Some(i) = self.sent_pos else {
            return; // 未在浏览，Down 无效。
        };
        if i + 1 < self.sent.len() {
            self.sent_pos = Some(i + 1);
            let text = self.sent[i + 1].clone();
            self.set_input(text);
        } else {
            // 越过最新 → 恢复草稿，退出浏览。
            self.sent_pos = None;
            let draft = std::mem::take(&mut self.draft);
            self.set_input(draft);
        }
    }

    // ---- 斜杠命令 / @文件 补全 ----

    /// 依当前输入与光标重算补全候选。
    /// 优先 `/` 命令（行首未结束）→ `/cmd <arg>` 参数补全 → `@` 文件补全。
    fn update_completion(&mut self) {
        let before = &self.input[..self.cursor];

        if let Some(after) = before.strip_prefix('/') {
            match after.split_once(char::is_whitespace) {
                // 命令名 token 未结束 → 命令补全。
                None => {
                    self.build_slash_completion(after.to_string());
                    return;
                }
                // 命令已确定 → 若该命令有参数候选，做参数补全。
                Some((cmd, _)) => {
                    // 先取完 before 借用相关的值（cmd/tok_start/query 都 owned），再做可变借用。
                    let cmd = cmd.to_string();
                    let tok_start = before.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
                    let query = self.input[tok_start..self.cursor].to_string();
                    if let Some(cands) = self.arg_candidates_cached(&cmd) {
                        self.build_arg_completion(tok_start, query, cands);
                    } else {
                        self.arg_cache = None;
                        self.completion = None;
                    }
                    return;
                }
            }
        }

        // `@` 文件：光标前最近的、位于词边界、且其后到光标无空白的 `@`。
        if let Some(at) = before.rfind('@') {
            let boundary = at == 0 || before[..at].ends_with(char::is_whitespace);
            let token = &before[at + 1..];
            if boundary && !token.contains(char::is_whitespace) {
                let q = token.to_string();
                self.build_file_completion(at, q);
                return;
            }
        }

        self.arg_cache = None;
        self.completion = None;
    }

    /// 取某命令的参数候选，带缓存：同命令过滤期间复用；首次打开时拉取
    /// （resume/fork 走动态来源以即时反映新建会话；其余取静态表）。
    fn arg_candidates_cached(&mut self, cmd: &str) -> Option<Vec<CompletionItem>> {
        if let Some((c, v)) = &self.arg_cache {
            if c == cmd {
                return Some(v.clone());
            }
        }
        let fresh = if (cmd == "resume" || cmd == "fork") && self.session_candidates.is_some() {
            Some((self.session_candidates.as_ref().unwrap())())
        } else {
            self.arg_completions
                .iter()
                .find(|(c, _)| c == cmd)
                .map(|(_, v)| v.clone())
        }?;
        self.arg_cache = Some((cmd.to_string(), fresh.clone()));
        Some(fresh)
    }

    fn build_slash_completion(&mut self, query: String) {
        self.arg_cache = None;
        let mut scored: Vec<(i32, usize)> = self
            .commands
            .iter()
            .enumerate()
            .filter_map(|(i, c)| fuzzy_score(&query, &c.name).map(|s| (s, i)))
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| self.commands[a.1].name.len().cmp(&self.commands[b.1].name.len()))
        });
        let entries: Vec<CompletionEntry> = scored
            .into_iter()
            .map(|(_, i)| {
                let c = &self.commands[i];
                CompletionEntry {
                    value: c.name.clone(),
                    description: c.description.clone(),
                    hint: c.hint.clone(),
                }
            })
            .collect();
        self.completion = if entries.is_empty() {
            None
        } else {
            Some(CompletionState { kind: CompletionKind::Slash, start: 0, entries, selected: 0 })
        };
    }

    fn build_file_completion(&mut self, start: usize, query: String) {
        self.arg_cache = None;
        // 惰性扫描一次工作目录。
        if self.file_cache.is_none() {
            self.file_cache = Some(scan_files(&self.cwd));
        }
        let files = self.file_cache.as_deref().unwrap_or(&[]);
        let mut scored: Vec<(i32, &String)> = files
            .iter()
            .filter_map(|p| fuzzy_score(&query, p).map(|s| (s, p)))
            .collect();
        // 分数降序；同分短路径优先。
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.len().cmp(&b.1.len())));
        scored.truncate(50);
        let entries: Vec<CompletionEntry> = scored
            .into_iter()
            .map(|(_, p)| CompletionEntry {
                value: p.clone(),
                description: String::new(),
                hint: None,
            })
            .collect();
        self.completion = if entries.is_empty() {
            None
        } else {
            Some(CompletionState { kind: CompletionKind::File, start, entries, selected: 0 })
        };
    }

    fn build_arg_completion(&mut self, start: usize, query: String, cands: Vec<CompletionItem>) {
        let mut scored: Vec<(i32, usize)> = cands
            .iter()
            .enumerate()
            .filter_map(|(i, c)| fuzzy_score(&query, &c.name).map(|s| (s, i)))
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| cands[a.1].name.len().cmp(&cands[b.1].name.len()))
        });
        let entries: Vec<CompletionEntry> = scored
            .into_iter()
            .map(|(_, i)| {
                let c = &cands[i];
                CompletionEntry {
                    value: c.name.clone(),
                    description: c.description.clone(),
                    hint: c.hint.clone(),
                }
            })
            .collect();
        self.completion = if entries.is_empty() {
            None
        } else {
            Some(CompletionState { kind: CompletionKind::Arg, start, entries, selected: 0 })
        };
    }

    /// 弹窗选择上下移（delta=+1 下，-1 上），环绕。
    fn completion_move(&mut self, delta: i32) {
        if let Some(c) = &mut self.completion {
            let n = c.entries.len() as i32;
            if n == 0 {
                return;
            }
            c.selected = (((c.selected as i32 + delta) % n + n) % n) as usize;
        }
    }

    /// 接受当前选中项：把当前 token 替换为补全值（命令加 `/`、文件加 `@`、参数无前缀），尾随空格。
    /// 返回 true = 改动了输入（应停留）；false = 输入已等于该命令（应直接提交，仅 `/` 命令）。
    fn completion_accept(&mut self) -> bool {
        let Some(c) = self.completion.take() else {
            return false;
        };
        let Some(entry) = c.entries.get(c.selected) else {
            return false;
        };
        // token 结束 = 从 start 起第一个空白，或行尾。
        let token_end = self.input[c.start..]
            .find(char::is_whitespace)
            .map(|i| c.start + i)
            .unwrap_or(self.input.len());

        // `/` 命令且已精确匹配 → 不改动，交给 Enter 提交。
        if c.kind == CompletionKind::Slash
            && self.input[c.start..token_end] == format!("/{}", entry.value)
        {
            return false;
        }

        let replacement = match c.kind {
            CompletionKind::Slash => format!("/{} ", entry.value),
            CompletionKind::File => format!("@{} ", entry.value),
            CompletionKind::Arg => format!("{} ", entry.value),
        };
        self.input.replace_range(c.start..token_end, &replacement);
        self.cursor = c.start + replacement.len();
        true
    }
}

/// 扫描工作目录收集相对文件路径（供 `@` 补全）。跳过重目录/隐藏项，限深限量。
fn scan_files(root: &std::path::Path) -> Vec<String> {
    const SKIP: &[&str] = &[
        ".git", "target", "node_modules", ".carter", "dist", "build", ".idea", ".vscode",
    ];
    const MAX_FILES: usize = 5000;
    const MAX_DEPTH: usize = 10;

    let mut out: Vec<String> = Vec::new();
    // (dir, depth)
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            let path = ent.path();
            let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
                if out.len() >= MAX_FILES {
                    return out;
                }
            }
        }
    }
    out
}

/// 模糊打分：子序列匹配（大小写不敏感），前缀 / 连续命中加权。无法匹配返回 None。
/// 空 query 视为匹配全部（打开 `/` 即列全部命令）。
fn fuzzy_score(query: &str, cand: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let c: Vec<char> = cand.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score = 0;
    let mut last: Option<usize> = None;
    for (i, ch) in c.iter().enumerate() {
        if qi < q.len() && *ch == q[qi] {
            score += 10;
            if i == 0 {
                score += 15; // 前缀命中
            }
            if let Some(li) = last {
                if i == li + 1 {
                    score += 5; // 连续命中
                }
            }
            last = Some(i);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score)
}

/// 用户输入处理结果。
enum Action {
    None,
    /// 提交 prompt 给 agent。
    Submit(String),
    /// 取消当前流式请求。
    Cancel,
    /// 优雅退出 REPL（等 agent 任务收尾）。
    Quit,
    /// 强制立即退出进程（卡死逃生口）。
    ForceQuit,
}

fn handle_key(key: KeyEvent, app: &mut App) -> Action {
    // 只处理按下（Windows 会重复发 Release/Repeat）。
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // 任何非 Ctrl+C 的按键都解除「已预备强退」状态。
    let is_ctrl_c = ctrl && matches!(key.code, KeyCode::Char('c'));
    if !is_ctrl_c {
        app.armed_quit = false;
    }

    // 补全弹窗激活时，Tab/↑/↓/Esc 优先用于导航/关闭（不落到历史浏览/中断）。
    if app.completion.is_some() {
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                app.completion_move(1);
                return Action::None;
            }
            KeyCode::Up => {
                app.completion_move(-1);
                return Action::None;
            }
            KeyCode::Esc => {
                app.completion = None;
                return Action::None;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Char('c') if ctrl => {
            // 双击 Ctrl+C 强退：第二次（已 armed）无条件强制退出进程。
            if app.armed_quit {
                return Action::ForceQuit;
            }
            app.armed_quit = true;
            if app.streaming {
                Action::Cancel
            } else {
                // 空闲时首次 Ctrl+C：提示再按一次退出。
                app.outbox.push(Block_::Notice(
                    "再按一次 Ctrl+C 退出（或 /quit）".into(),
                ));
                Action::None
            }
        }
        KeyCode::Char('d') if ctrl => {
            if app.input.is_empty() {
                Action::Quit
            } else {
                Action::None
            }
        }
        KeyCode::Esc => {
            if app.streaming {
                Action::Cancel
            } else {
                Action::None
            }
        }
        KeyCode::Enter => {
            // 带 Shift/Alt/Ctrl 任一修饰 → 插入换行（多行输入）；裸 Enter → 发送。
            let newline_mod = key.modifiers.intersects(
                KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
            );
            if newline_mod {
                app.input_insert('\n');
                return Action::None;
            }
            if app.streaming {
                return Action::None;
            }
            // 补全激活：Enter 先接受选中项（除非输入已精确等于该命令则直接提交）。
            if app.completion.is_some() && app.completion_accept() {
                return Action::None;
            }
            let text = app.input.trim().to_string();
            app.input_clear();
            app.completion = None;
            if text.is_empty() {
                Action::None
            } else if text == "/quit" || text == "/exit" {
                Action::Quit
            } else {
                app.push_sent(text.clone());
                Action::Submit(text)
            }
        }
        KeyCode::Backspace => {
            app.input_backspace();
            app.update_completion();
            Action::None
        }
        KeyCode::Delete => {
            app.input_delete();
            app.update_completion();
            Action::None
        }
        KeyCode::Left => {
            app.input_left();
            app.update_completion();
            Action::None
        }
        KeyCode::Right => {
            app.input_right();
            app.update_completion();
            Action::None
        }
        // 上/下：调阅已提交的 prompt 历史（草稿在越过最新一条后恢复）。
        KeyCode::Up => {
            app.history_prev();
            app.update_completion();
            Action::None
        }
        KeyCode::Down => {
            app.history_next();
            app.update_completion();
            Action::None
        }
        // Home/End：始终作用于输入光标（历史滚动交给终端原生）。
        KeyCode::Home => {
            app.input_home();
            app.update_completion();
            Action::None
        }
        KeyCode::End => {
            app.input_end();
            app.update_completion();
            Action::None
        }
        KeyCode::Char(c) => {
            app.input_insert(c);
            app.update_completion();
            Action::None
        }
        _ => Action::None,
    }
}

/// 启动 banner：彩色 ASCII art「CARTER」+ 特工卡特谐音梗标语。
fn banner_lines() -> Vec<Line<'static>> {
    let art = [
        r"  ____    _    ____ _____ _____ ____  ",
        r" / ___|  / \  |  _ \_   _| ____|  _ \ ",
        r"| |     / _ \ | |_) || | |  _| | |_) |",
        r"| |___ / ___ \|  _ < | | | |___|  _ < ",
        r" \____/_/   \_\_| \_\|_| |_____|_| \_\",
    ];
    let cyan = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> =
        art.iter().map(|l| Line::from(Span::styled(l.to_string(), cyan))).collect();
    lines.push(Line::from(Span::styled(
        "特工卡特已就位 · Agent Carter reporting for duty".to_string(),
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::raw(String::new())));
    lines
}

/// 输入文本在给定内容宽度下折行后的总视觉行数（含软折行；不 clamp）。纯函数，便于单测。
fn total_visual_rows(input: &str, inner_width: u16) -> u16 {
    let w = inner_width.max(1) as usize;
    let mut rows: usize = 0;
    // split('\n') 对空串产出一个空段 → 至少 1 行。
    for seg in input.split('\n') {
        let cells = UnicodeWidthStr::width(seg).max(1);
        rows += cells.div_ceil(w);
    }
    rows.max(1) as u16
}

/// 估算输入框所需高度 = 总视觉行数 clamp `[1, MAX_INPUT_ROWS]`。纯函数，便于单测。
fn input_height(input: &str, inner_width: u16) -> u16 {
    total_visual_rows(input, inner_width).min(MAX_INPUT_ROWS)
}

/// 计算光标在多行输入中的 (列显示宽度, 视觉行号)，与 `Paragraph` 的折行一致。
/// 列宽用 `UnicodeWidthStr` 计（含 CJK 全宽）；视觉行号 = 光标前所有逻辑行折行后的累计行数
/// + 当前逻辑行内已折行数。纯函数，便于单测。
fn cursor_rowcol(input: &str, cursor_byte: usize, inner_width: u16) -> (u16, u16) {
    let w = inner_width.max(1) as usize;
    let before = &input[..cursor_byte.min(input.len())];
    // 光标前已结束的逻辑行（最后一个 \n 之前）各自折行累计。
    let mut row: usize = 0;
    let last_nl = before.rfind('\n');
    if let Some(idx) = last_nl {
        for seg in before[..idx].split('\n') {
            let cells = UnicodeWidthStr::width(seg).max(1);
            row += cells.div_ceil(w);
        }
    }
    // 当前逻辑行（最后一个 \n 之后到光标）。
    let cur_line = match last_nl {
        Some(idx) => &before[idx + 1..],
        None => before,
    };
    let cur_cells = UnicodeWidthStr::width(cur_line);
    // 当前行内已折过的行数 + 行内列偏移。
    row += cur_cells / w;
    let col = (cur_cells % w) as u16;
    (col, row as u16)
}

/// 把一个定稿块渲染成 styled 行（纯函数，可单测）。
fn block_to_lines(block: &Block_) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    match block {
        Block_::User(s) => {
            // 多行用户输入：逐逻辑行渲染，保留换行（首行加前缀，续行对齐缩进）。
            let style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            for (i, l) in s.split('\n').enumerate() {
                let text = if i == 0 {
                    format!("› {l}")
                } else {
                    format!("  {l}")
                };
                lines.push(Line::from(Span::styled(text, style)));
            }
        }
        Block_::Assistant(s) => {
            for l in s.split('\n') {
                lines.push(Line::from(Span::raw(l.to_string())));
            }
        }
        Block_::Thinking(s) => {
            for l in s.split('\n') {
                lines.push(Line::from(Span::styled(
                    format!("[thinking] {l}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        Block_::Tool { ok, line } => {
            let color = match ok {
                None => Color::Magenta,
                Some(true) => Color::Green,
                Some(false) => Color::Red,
            };
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(color),
            )));
        }
        Block_::Todos(todos) => {
            for t in todos {
                let (mark, color, text) = match t.status {
                    TodoStatus::Completed => ("[x]", Color::Green, &t.content),
                    TodoStatus::InProgress => ("[~]", Color::Yellow, &t.active_form),
                    TodoStatus::Pending => ("[ ]", Color::DarkGray, &t.content),
                };
                lines.push(Line::from(Span::styled(
                    format!("  {mark} {text}"),
                    Style::default().fg(color),
                )));
            }
        }
        Block_::Notice(msg) => {
            // 系统/元信息（压缩、恢复、中断提示）：暗灰 + 斜体 + `⋯` 前缀，
            // 与对话正文明显区分，避免被误读为对话历史。多行各自加前缀。
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            for l in msg.split('\n') {
                lines.push(Line::from(Span::styled(format!("⋯ {l}"), style)));
            }
        }
        Block_::Divider(label) => {
            // 上下文边界分隔线：青色标签居中夹在 `─` 之间。
            let dash = "─".repeat(12);
            let text = if label.is_empty() {
                "─".repeat(28)
            } else {
                format!("{dash} {label} {dash}")
            };
            lines.push(Line::from(Span::styled(
                text,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::DIM),
            )));
        }
    }
    lines
}

/// 把 buffer 里宽字符（CJK）的占位续单元 symbol 清空，避免 `insert_before` 的
/// append_lines 回落路径逐 cell `Print(" ")` 在每个宽字符后多输出一个空格。
/// 只清宽字符紧随其后的那一个续单元，不动正常空格，故词间空格仍保留。
fn blank_wide_placeholders(buf: &mut ratatui::buffer::Buffer) {
    let area = buf.area;
    for y in area.top()..area.bottom() {
        let mut x = area.left();
        while x < area.right() {
            let w = UnicodeWidthStr::width(buf[(x, y)].symbol());
            if w >= 2 && x + 1 < area.right() {
                buf[(x + 1, y)].set_symbol("");
                x += 2;
            } else {
                x += 1;
            }
        }
    }
}

/// 把 outbox 里所有定稿块逐个 flush 进 viewport 上方的终端 scrollback。
/// 每块按其**折行后**的真实行数 `insert_before`，内容永久落入终端缓冲区
/// （可原生选择/复制/退出留存）。按渲染宽度估算折行行数，否则宽行软折会溢出
/// 进 viewport、覆盖历史。
///
/// 关掉 `scrolling-regions` 后 `insert_before` 走 append_lines 回落路径（scrollback 正常），
/// 但该路径逐 cell `Print(symbol())`，宽字符（CJK）的占位续单元 `symbol()` 返回 `" "`，
/// 导致每个中文字后多一个空格。故在 buffer draw 后把宽字符的续单元 symbol 清空
/// （`Print("")` 不输出），既保留 scrollback 又消除多余空格。
fn flush_outbox(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    let width = terminal.get_frame().area().width.max(1);
    for block in app.outbox.drain(..) {
        let lines = block_to_lines(&block);
        // 每条逻辑行按 width 软折，累加真实占用行数（与 Wrap{trim:false} 一致）。
        let h: u16 = lines
            .iter()
            .map(|l| {
                let cells = UnicodeWidthStr::width(l.to_string().as_str()).max(1);
                cells.div_ceil(width as usize) as u16
            })
            .sum::<u16>()
            .max(1);
        terminal.insert_before(h, |buf| {
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .render(buf.area, buf);
            blank_wide_placeholders(buf);
        })?;
    }
    Ok(())
}

/// 渲染底部固定 viewport：预览区（pending 残行）+ 输入框（随行数自适应高度）+ 状态栏。
fn render_viewport(f: &mut Frame, app: &mut App) {
    // 输入框内容宽度（去左右边框）→ 估算所需行数 → 输入框 widget 高度（+上下边框 2）。
    let inner_w = f.area().width.saturating_sub(2);
    let input_box_h = input_height(&app.input, inner_w) + 2;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(input_box_h),
            Constraint::Length(1),
        ])
        .split(f.area());

    render_preview(f, app, chunks[0]);
    render_completion(f, app, chunks[0]);
    render_input(f, app, chunks[1]);
    render_status(f, app, chunks[2]);

    // 光标聚焦输入框：按 (视觉行号, 行内显示宽度) 定位，减去输入框滚动偏移后 clamp 到内容区。
    let inner = app.input_inner;
    if inner.width > 0 && inner.height > 0 {
        let (col, row) = cursor_rowcol(&app.input, app.cursor, inner.width);
        let visible_row = row.saturating_sub(app.input_scroll);
        let cx = inner.x + col.min(inner.width.saturating_sub(1));
        let cy = inner.y + visible_row.min(inner.height.saturating_sub(1));
        f.set_cursor_position(Position::new(cx, cy));
    }
}

/// 流式残行预览：画当前 pending 块未满一行的内容（无则空）。
fn render_preview(f: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = match &app.pending {
        Some(b @ (Block_::Assistant(s) | Block_::Thinking(s))) if !s.is_empty() => block_to_lines(b),
        _ => Vec::new(),
    };
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

/// 斜杠命令 / `@` 文件补全弹窗：底部对齐渲染于预览区，高亮选中项。
fn render_completion(f: &mut Frame, app: &App, area: Rect) {
    let Some(c) = &app.completion else {
        return;
    };
    if area.height == 0 || c.entries.is_empty() {
        return;
    }
    let trigger = match c.kind {
        CompletionKind::Slash => "/",
        CompletionKind::File => "@",
        CompletionKind::Arg => "",
    };
    let max_rows = (area.height as usize).min(8);
    // 选中项落在可见窗口内（必要时上滚）。
    let start = if c.selected >= max_rows {
        c.selected + 1 - max_rows
    } else {
        0
    };
    let end = (start + max_rows).min(c.entries.len());

    let mut lines: Vec<Line> = Vec::with_capacity(end - start);
    for vis in start..end {
        let item = &c.entries[vis];
        let hint = item
            .hint
            .as_deref()
            .map(|h| format!(" {h}"))
            .unwrap_or_default();
        let mut spans = vec![
            Span::styled(
                format!(" {trigger}{}", item.value),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(hint, Style::default().fg(Color::DarkGray)),
        ];
        if !item.description.is_empty() {
            spans.push(Span::styled(
                format!("  {}", item.description),
                Style::default().fg(Color::Gray),
            ));
        }
        let mut line = Line::from(spans);
        if vis == c.selected {
            line = line.style(Style::default().bg(Color::Blue));
        }
        lines.push(line);
    }

    let h = lines.len() as u16;
    let rect = Rect {
        x: area.x,
        y: area.y + area.height - h,
        width: area.width,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_widget(Paragraph::new(lines), rect);
}

fn render_input(f: &mut Frame, app: &mut App, area: Rect) {
    let title = if app.streaming {
        " streaming…  (Esc/Ctrl+C 中断) "
    } else {
        " 输入 (Enter 发送 · Shift+Enter 换行 · / 命令 · @ 文件 · /quit 退出) "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    // 记录内容区（去边框）供光标定位。
    let inner = block.inner(area);
    app.input_inner = inner;

    // 内容超过可见行数时垂直滚动，使光标所在视觉行始终落在可见窗口内。
    let (_, cur_row) = cursor_rowcol(&app.input, app.cursor, inner.width);
    let visible = inner.height.max(1);
    let scroll = if cur_row >= visible {
        cur_row - visible + 1
    } else {
        0
    };
    app.input_scroll = scroll;

    let para = Paragraph::new(app.input.as_str())
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(para, area);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    // 暗色底栏 + 语义着色的分段。各段以暗灰 `│` 分隔。
    let bar = Style::default().bg(Color::Indexed(236));
    let sep = || Span::styled(" │ ", Style::default().fg(Color::DarkGray).bg(Color::Indexed(236)));
    let seg = |s: String, c: Color| {
        Span::styled(s, Style::default().fg(c).bg(Color::Indexed(236)))
    };

    let mut spans: Vec<Span> = Vec::new();
    // 状态指示：streaming 黄、idle 绿。
    if app.streaming {
        spans.push(Span::styled(
            " ⟳ 思考中… ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(
            " ● 就绪 ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // 标题（若有）。
    if let Some(t) = &app.status.title {
        spans.push(seg(format!(" {t}"), Color::Cyan));
    }
    spans.push(sep());
    // 模型。
    spans.push(seg(app.status.model.clone(), Color::White));
    spans.push(sep());
    // token 用量（k 格式）。
    spans.push(seg(
        format!("↑{} ↓{}", fmt_tokens(app.status.total_in), fmt_tokens(app.status.total_out)),
        Color::Gray,
    ));
    spans.push(sep());
    // 成本。
    spans.push(seg(format!("${:.4}", app.status.total_cost), Color::LightGreen));

    let para = Paragraph::new(Line::from(spans)).style(bar);
    f.render_widget(para, area);
}

/// token 数 → 紧凑文本（≥1000 用 k）。
fn fmt_tokens(n: u64) -> String {
    if n >= 10_000 {
        format!("{:.0}k", n as f64 / 1000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// 跑交互式 REPL。`ui_rx` 收 agent 输出事件；`input_tx` 发用户 prompt 给 agent 任务；
/// `cancel` 在中断时 set。返回时终端已恢复（RAII guard）。
pub async fn run(
    model: String,
    commands: Vec<CompletionItem>,
    arg_completions: Vec<(String, Vec<CompletionItem>)>,
    session_candidates: Box<dyn Fn() -> Vec<CompletionItem> + Send>,
    history: Vec<String>,
    cwd: std::path::PathBuf,
    mut ui_rx: UnboundedReceiver<UiEvent>,
    input_tx: UnboundedSender<String>,
    cancel: CancelToken,
) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    // Inline viewport：底部固定 VIEWPORT_HEIGHT 行（预览 + 输入框最大高 + 状态栏）；
    // 不切 alt-screen，定稿块经 insert_before 落入上方终端 scrollback。
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )?;

    let mut app = App::new(model)
        .with_commands(commands)
        .with_cwd(cwd)
        .with_arg_completions(arg_completions)
        .with_session_candidates(session_candidates)
        .with_history(history);
    let mut term_events = EventStream::new();

    // 先 draw 一次：触发 autoresize 填充终端真实尺寸（last_known_area），
    // 否则首个 insert_before 会按 stale 宽度切分 cells，导致字符间出现额外空格。
    terminal.draw(|f| render_viewport(f, &mut app))?;

    // 首帧后打 banner 进 viewport 上方 scrollback（last_known_area 已就绪）。
    {
        let lines = banner_lines();
        let h = (lines.len() as u16).max(1);
        terminal.insert_before(h, |buf| {
            Paragraph::new(lines).render(buf.area, buf);
            blank_wide_placeholders(buf);
        })?;
    }

    loop {
        flush_outbox(&mut terminal, &mut app)?;
        terminal.draw(|f| render_viewport(f, &mut app))?;
        if app.should_quit {
            break;
        }

        tokio::select! {
            maybe_ev = ui_rx.recv() => {
                match maybe_ev {
                    Some(ev) => app.apply(ev),
                    None => break, // agent 任务结束、通道关闭。
                }
            }
            maybe_term = term_events.next() => {
                match maybe_term {
                    Some(Ok(CtEvent::Key(key))) => {
                        match handle_key(key, &mut app) {
                            Action::None => {}
                            Action::Submit(text) => {
                                app.push_user(text.clone());
                                app.streaming = true;
                                if input_tx.send(text).is_err() {
                                    app.should_quit = true;
                                }
                            }
                            Action::Cancel => {
                                cancel.set();
                                app.apply(UiEvent::Notice("[中断] 已请求取消当前请求".into()));
                            }
                            Action::Quit => app.should_quit = true,
                            Action::ForceQuit => {
                                // 卡死逃生口：agent 任务可能 wedged，process::exit 不会跑
                                // guard 的 Drop，故先清 viewport 残留 + 手动恢复终端再立即退出。
                                let _ = execute!(io::stdout(), Clear(ClearType::FromCursorDown));
                                drop(_guard);
                                std::process::exit(130);
                            }
                        }
                    }
                    Some(Ok(_)) => {} // resize/paste 等：下一帧自然重绘。
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    // 退出清理：清掉 inline viewport 占据的底部区域，避免内容残留在 shell 提示符后。
    // inline 模式 clear() 把光标移到 viewport 原点并 Clear(AfterCursor)，正合所需。
    terminal.clear()?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_wide_placeholders_clears_only_cjk_continuation() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        // "中a 文" → 中(2) a(1) 空格(1) 文(2) → 占位续单元在 中 后、文 后。
        buf.set_string(0, 0, "中a 文", Style::default());
        blank_wide_placeholders(&mut buf);
        // 中 的续单元（x=1）被清空；'a'(x=2)、空格(x=3) 保留；文 的续单元（x=5）清空。
        assert_eq!(buf[(0, 0)].symbol(), "中");
        assert_eq!(buf[(1, 0)].symbol(), "");
        assert_eq!(buf[(2, 0)].symbol(), "a");
        assert_eq!(buf[(3, 0)].symbol(), " ");
        assert_eq!(buf[(4, 0)].symbol(), "文");
        assert_eq!(buf[(5, 0)].symbol(), "");
    }

    #[test]
    fn assistant_delta_accumulates_into_pending_no_newline() {
        let mut app = App::new("m".into());
        app.apply(UiEvent::AssistantTextDelta("Hel".into()));
        app.apply(UiEvent::AssistantTextDelta("lo".into()));
        // 无换行：全部留在 pending，outbox 为空。
        assert!(app.outbox.is_empty());
        match &app.pending {
            Some(Block_::Assistant(s)) => assert_eq!(s, "Hello"),
            _ => panic!("expected pending assistant block"),
        }
    }

    #[test]
    fn assistant_delta_flushes_complete_lines_keeps_residual() {
        let mut app = App::new("m".into());
        app.apply(UiEvent::AssistantTextDelta("line1\nline2\npart".into()));
        // 两条完整行入 outbox，残行 "part" 留 pending。
        assert_eq!(app.outbox.len(), 2);
        match (&app.outbox[0], &app.outbox[1]) {
            (Block_::Assistant(a), Block_::Assistant(b)) => {
                assert_eq!(a, "line1");
                assert_eq!(b, "line2");
            }
            _ => panic!("expected two assistant blocks"),
        }
        match &app.pending {
            Some(Block_::Assistant(s)) => assert_eq!(s, "part"),
            _ => panic!("expected residual pending"),
        }
    }

    #[test]
    fn text_end_flushes_pending_residual_to_outbox() {
        let mut app = App::new("m".into());
        app.apply(UiEvent::AssistantTextDelta("a".into()));
        app.apply(UiEvent::AssistantTextEnd);
        app.apply(UiEvent::AssistantTextDelta("b".into()));
        // "a" 定稿进 outbox，"b" 留 pending。
        assert_eq!(app.outbox.len(), 1);
        match &app.outbox[0] {
            Block_::Assistant(s) => assert_eq!(s, "a"),
            _ => panic!("expected assistant block"),
        }
        assert!(matches!(&app.pending, Some(Block_::Assistant(s)) if s == "b"));
    }

    #[test]
    fn switching_stream_kind_finalizes_previous_pending() {
        let mut app = App::new("m".into());
        app.apply(UiEvent::AssistantTextDelta("hi".into()));
        app.apply(UiEvent::ThinkingDelta("think".into()));
        // assistant 残行先定稿，thinking 残行留 pending。
        assert_eq!(app.outbox.len(), 1);
        assert!(matches!(&app.outbox[0], Block_::Assistant(s) if s == "hi"));
        assert!(matches!(&app.pending, Some(Block_::Thinking(s)) if s == "think"));
    }

    #[test]
    fn tool_events_flush_pending_then_append() {
        let mut app = App::new("m".into());
        app.apply(UiEvent::AssistantTextDelta("draft".into()));
        app.apply(UiEvent::ToolCallStarted {
            name: "read".into(),
            args_preview: "path=x".into(),
        });
        app.apply(UiEvent::ToolResult {
            ok: true,
            summary: "ok".into(),
        });
        // pending "draft" 定稿 + 两个 tool 块 = 3。
        assert_eq!(app.outbox.len(), 3);
        assert!(app.pending.is_none());
    }

    #[test]
    fn turn_usage_accumulates_and_clears_streaming() {
        let mut app = App::new("m".into());
        app.streaming = true;
        let usage = Usage {
            input: 10,
            output: 5,
            ..Default::default()
        };
        app.apply(UiEvent::TurnUsage {
            usage,
            cost: 0.25,
            model: "claude".into(),
        });
        assert_eq!(app.status.total_in, 10);
        assert_eq!(app.status.total_out, 5);
        assert!((app.status.total_cost - 0.25).abs() < 1e-9);
        // 模型名不再被 TurnUsage 覆盖（由 ModelChanged 权威维护）。
        assert_eq!(app.status.model, "m");
        assert!(!app.streaming);
    }

    #[test]
    fn model_changed_updates_status_authoritatively() {
        let mut app = App::new("ws/old".into());
        app.apply(UiEvent::ModelChanged("ws/claude-sonnet-4-6".into()));
        assert_eq!(app.status.model, "ws/claude-sonnet-4-6");
    }

    #[test]
    fn divider_and_replay_render_into_outbox() {
        use crate::agent::{ReplayMsg, ReplayRole};
        let mut app = App::new("m".into());
        app.apply(UiEvent::Divider("恢复会话 标题 (abcd1234)".into()));
        app.apply(UiEvent::ReplayHistory(vec![
            ReplayMsg { role: ReplayRole::User, text: "问题".into() },
            ReplayMsg { role: ReplayRole::Assistant, text: "回答".into() },
            ReplayMsg { role: ReplayRole::ToolCall, text: "⚙ read(x)".into() },
            ReplayMsg { role: ReplayRole::ToolResult, text: "ok".into() },
        ]));
        // 1 分隔线 + 4 回放块。
        assert_eq!(app.outbox.len(), 5);
        assert!(matches!(&app.outbox[0], Block_::Divider(s) if s.contains("恢复会话")));
        assert!(matches!(&app.outbox[1], Block_::User(s) if s == "问题"));
        assert!(matches!(&app.outbox[2], Block_::Assistant(s) if s == "回答"));
        assert!(matches!(&app.outbox[3], Block_::Tool { ok: None, .. }));
        assert!(matches!(&app.outbox[4], Block_::Tool { ok: Some(true), .. }));
    }

    #[test]
    fn block_to_lines_renders_each_block_kind() {
        assert_eq!(block_to_lines(&Block_::User("hi".into())).len(), 1);
        // 多行用户输入按 \n 拆成多行，保留换行显示。
        assert_eq!(block_to_lines(&Block_::User("a\nb\nc".into())).len(), 3);
        // 多行 assistant 块按 \n 拆成多行。
        assert_eq!(
            block_to_lines(&Block_::Assistant("a\nb\nc".into())).len(),
            3
        );
        assert_eq!(
            block_to_lines(&Block_::Tool {
                ok: Some(true),
                line: "x".into()
            })
            .len(),
            1
        );
        assert_eq!(block_to_lines(&Block_::Notice("n".into())).len(), 1);
    }

    #[test]
    fn title_event_sets_status_only_not_history() {
        let mut app = App::new("m".into());
        app.apply(UiEvent::AssistantTextDelta("resid".into()));
        app.apply(UiEvent::Title("修复字间隔".into()));
        assert_eq!(app.status.title.as_deref(), Some("修复字间隔"));
        // 标题不落入对话历史，也不打断流式残行。
        assert!(app.outbox.is_empty());
        assert!(matches!(&app.pending, Some(Block_::Assistant(s)) if s == "resid"));
    }

    #[test]
    fn idle_clears_streaming_and_flushes_pending() {
        let mut app = App::new("m".into());
        app.streaming = true;
        app.apply(UiEvent::AssistantTextDelta("resid".into()));
        app.apply(UiEvent::Idle);
        assert!(!app.streaming);
        // 残行被定稿进 outbox。
        assert_eq!(app.outbox.len(), 1);
        assert!(app.pending.is_none());
    }

    #[test]
    fn banner_lines_nonempty() {
        let lines = banner_lines();
        assert!(lines.len() >= 5);
    }

    fn app_with_cmds() -> App {
        App::new("m".into()).with_commands(vec![
            CompletionItem { name: "clear".into(), description: "清空".into(), hint: None },
            CompletionItem { name: "compact".into(), description: "压缩".into(), hint: None },
            CompletionItem { name: "cost".into(), description: "成本".into(), hint: None },
            CompletionItem {
                name: "git:commit".into(),
                description: "提交".into(),
                hint: Some("[scope]".into()),
            },
        ])
    }

    fn press(app: &mut App, code: KeyCode) -> Action {
        handle_key(KeyEvent::new(code, KeyModifiers::NONE), app)
    }

    #[test]
    fn fuzzy_score_matches_subsequence_and_ranks_prefix() {
        assert!(fuzzy_score("", "clear").is_some()); // 空 query 命中全部
        assert!(fuzzy_score("clr", "clear").is_some()); // 子序列
        assert!(fuzzy_score("xyz", "clear").is_none()); // 不匹配
        // 前缀命中分更高。
        assert!(fuzzy_score("co", "compact").unwrap() > fuzzy_score("ct", "compact").unwrap());
    }

    #[test]
    fn typing_slash_opens_and_filters_popup() {
        let mut app = app_with_cmds();
        press(&mut app, KeyCode::Char('/'));
        // 只有 "/" → 列全部 4 条。
        assert_eq!(app.completion.as_ref().unwrap().entries.len(), 4);
        press(&mut app, KeyCode::Char('c'));
        // "/c" → clear/compact/cost/commit 仍可子序列匹配（git:commit 含 c）。
        assert!(app.completion.is_some());
        press(&mut app, KeyCode::Char('o'));
        // "/co" → compact, cost, git:commit。
        let m = app.completion.as_ref().unwrap();
        assert!(m.entries.len() >= 2);
    }

    #[test]
    fn at_triggers_file_completion_and_accepts_path() {
        // 临时工作目录放一个文件。
        let dir = std::env::temp_dir().join(format!("carter-tui-{}", crate::session::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hello.rs"), "x").unwrap();
        let mut app = App::new("m".into()).with_cwd(dir.clone());

        for c in "看 @hel".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let c = app.completion.as_ref().expect("file popup active");
        assert_eq!(c.kind, CompletionKind::File);
        assert!(c.entries.iter().any(|e| e.value == "hello.rs"));
        // Enter 接受：把 @hel 替换为 @hello.rs（带尾空格）。
        let a = press(&mut app, KeyCode::Enter);
        assert!(matches!(a, Action::None));
        assert!(app.input.contains("@hello.rs "));
        assert!(app.completion.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn at_not_at_boundary_does_not_trigger() {
        let mut app = App::new("m".into()).with_cwd(std::env::temp_dir());
        for c in "a@b".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        // 邮箱式 a@b：@ 非词边界 → 不触发文件补全。
        assert!(app.completion.is_none());
    }

    #[test]
    fn arg_completion_after_command_space_filters_and_accepts() {
        let mut app = App::new("m".into()).with_arg_completions(vec![(
            "model".into(),
            vec![
                CompletionItem { name: "ws/sonnet".into(), description: "[anthropic]".into(), hint: None },
                CompletionItem { name: "ws/gpt".into(), description: "[openai]".into(), hint: None },
            ],
        )]);
        // "/model " → 参数补全列出全部 2 项。
        for ch in "/model ".chars() {
            press(&mut app, KeyCode::Char(ch));
        }
        let comp = app.completion.as_ref().expect("arg popup active");
        assert_eq!(comp.kind, CompletionKind::Arg);
        assert_eq!(comp.entries.len(), 2);
        // 输入 g → 仅 ws/gpt 匹配。
        press(&mut app, KeyCode::Char('g'));
        let comp = app.completion.as_ref().unwrap();
        assert_eq!(comp.entries.len(), 1);
        assert_eq!(comp.entries[0].value, "ws/gpt");
        // Enter 接受：参数无前缀，原位替换 + 尾空格。
        let a = press(&mut app, KeyCode::Enter);
        assert!(matches!(a, Action::None));
        assert_eq!(app.input, "/model ws/gpt ");
        assert!(app.completion.is_none());
    }

    #[test]
    fn arg_completion_only_for_known_commands() {
        let mut app = App::new("m".into()).with_arg_completions(vec![(
            "model".into(),
            vec![CompletionItem { name: "ws/x".into(), description: String::new(), hint: None }],
        )]);
        // /clear 无参数候选 → 输入空格后不弹窗。
        for ch in "/clear ".chars() {
            press(&mut app, KeyCode::Char(ch));
        }
        assert!(app.completion.is_none());
    }

    #[test]
    fn resume_arg_uses_dynamic_session_candidates() {
        let mut app = App::new("m".into()).with_session_candidates(Box::new(|| {
            vec![CompletionItem {
                name: "abc12345".into(),
                description: "标题A".into(),
                hint: None,
            }]
        }));
        for ch in "/resume ".chars() {
            press(&mut app, KeyCode::Char(ch));
        }
        let comp = app.completion.as_ref().expect("arg popup active");
        assert_eq!(comp.kind, CompletionKind::Arg);
        assert_eq!(comp.entries.len(), 1);
        assert_eq!(comp.entries[0].value, "abc12345");
    }

    #[test]
    fn popup_closes_when_arg_token_starts() {
        let mut app = app_with_cmds();
        for c in "/clear".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(app.completion.is_some());
        press(&mut app, KeyCode::Char(' '));
        // 命令名 token 结束（出现空格）→ 关闭弹窗。
        assert!(app.completion.is_none());
    }

    #[test]
    fn enter_accepts_selection_then_submits() {
        let mut app = app_with_cmds();
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('c'));
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Char('s')); // 偏向 cost
        // 首次 Enter：接受选中项，补成 "/<name> "，不提交。
        let a = press(&mut app, KeyCode::Enter);
        assert!(matches!(a, Action::None));
        assert!(app.input.starts_with('/') && app.input.ends_with(' '));
        assert!(app.completion.is_none());
        // 再 Enter：提交。
        let a = press(&mut app, KeyCode::Enter);
        assert!(matches!(a, Action::Submit(_)));
    }

    #[test]
    fn tab_and_arrows_navigate_popup_not_history() {
        let mut app = app_with_cmds();
        press(&mut app, KeyCode::Char('/'));
        let first = app.completion.as_ref().unwrap().selected;
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.completion.as_ref().unwrap().selected, first + 1);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.completion.as_ref().unwrap().selected, first);
    }

    #[test]
    fn esc_dismisses_popup() {
        let mut app = app_with_cmds();
        press(&mut app, KeyCode::Char('/'));
        assert!(app.completion.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.completion.is_none());
        // 输入保留。
        assert_eq!(app.input, "/");
    }

    #[test]
    fn input_height_counts_lines() {
        // 单行短文本 = 1。
        assert_eq!(input_height("hi", 80), 1);
        // 三段（两个 \n）= 3。
        assert_eq!(input_height("a\nb\nc", 80), 3);
        // 空串 = 1。
        assert_eq!(input_height("", 80), 1);
        // 超宽折行：宽 4，10 个字符 → 3 行。
        assert_eq!(input_height("aaaaaaaaaa", 4), 3);
        // clamp 到 MAX_INPUT_ROWS。
        let many = "x\n".repeat(20);
        assert_eq!(input_height(&many, 80), MAX_INPUT_ROWS);
    }

    #[test]
    fn cursor_rowcol_locates_position() {
        // 空串 → (0,0)。
        assert_eq!(cursor_rowcol("", 0, 80), (0, 0));
        // 单行末尾：col = 宽度，row = 0。
        assert_eq!(cursor_rowcol("abc", 3, 80), (3, 0));
        // 第二行：row = 1，col = 行内宽度。
        let s = "ab\ncd";
        assert_eq!(cursor_rowcol(s, s.len(), 80), (2, 1));
        // CJK 宽度按 2 计。
        let z = "中文";
        assert_eq!(cursor_rowcol(z, z.len(), 80), (4, 0));
        // 软折行：宽 4，第 5 个字符落到第 2 视觉行起始（col=1, row=1）。
        assert_eq!(cursor_rowcol("aaaaa", 5, 4), (1, 1));
        // 逻辑换行 + 软折行叠加：首逻辑行 6 字符在宽 4 占 2 视觉行，光标在第二逻辑行首。
        let m = "aaaaaa\nb";
        // before='aaaaaa\nb'：前一逻辑行 6/4→2 行，当前行 'b' → row=2, col=1。
        assert_eq!(cursor_rowcol(m, m.len(), 4), (1, 2));
    }

    #[test]
    fn shift_enter_inserts_newline_bare_enter_submits() {
        for m in [KeyModifiers::SHIFT, KeyModifiers::ALT, KeyModifiers::CONTROL] {
            let mut app = App::new("m".into());
            app.input = "hi".into();
            app.cursor = app.input.len();
            let a = handle_key(KeyEvent::new(KeyCode::Enter, m), &mut app);
            assert!(matches!(a, Action::None));
            assert!(app.input.contains('\n'), "modifier {m:?} should insert newline");
        }
        // 裸 Enter → 提交。
        let mut app = App::new("m".into());
        app.input = "hello".into();
        let a = handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut app);
        assert!(matches!(a, Action::Submit(t) if t == "hello"));
    }

    #[test]
    fn push_user_finalizes_pending_then_appends_user() {
        let mut app = App::new("m".into());
        app.apply(UiEvent::AssistantTextDelta("resid".into()));
        app.push_user("question".into());
        assert_eq!(app.outbox.len(), 2);
        assert!(matches!(&app.outbox[0], Block_::Assistant(s) if s == "resid"));
        assert!(matches!(&app.outbox[1], Block_::User(s) if s == "question"));
        assert!(app.pending.is_none());
    }


    #[test]
    fn enter_submits_nonempty_input() {
        let mut app = App::new("m".into());
        app.input = "hello".into();
        let action = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
        );
        assert!(matches!(action, Action::Submit(t) if t == "hello"));
        assert!(app.input.is_empty());
    }

    #[test]
    fn slash_quit_quits() {
        let mut app = App::new("m".into());
        app.input = "/quit".into();
        let action = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
        );
        assert!(matches!(action, Action::Quit));
    }

    #[test]
    fn enter_ignored_while_streaming() {
        let mut app = App::new("m".into());
        app.input = "hi".into();
        app.streaming = true;
        let a = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
        );
        assert!(matches!(a, Action::None));
    }

    #[test]
    fn double_ctrl_c_force_quits() {
        let mut app = App::new("m".into());
        // 空闲首次 Ctrl+C：仅 arm + 提示，不退出。
        let a = handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut app,
        );
        assert!(matches!(a, Action::None));
        assert!(app.armed_quit);
        // 第二次 Ctrl+C：强退。
        let b = handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut app,
        );
        assert!(matches!(b, Action::ForceQuit));
    }

    #[test]
    fn streaming_ctrl_c_cancels_then_second_force_quits() {
        let mut app = App::new("m".into());
        app.streaming = true;
        let a = handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut app,
        );
        assert!(matches!(a, Action::Cancel));
        assert!(app.armed_quit);
        let b = handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut app,
        );
        assert!(matches!(b, Action::ForceQuit));
    }

    #[test]
    fn other_key_disarms_quit() {
        let mut app = App::new("m".into());
        handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut app,
        );
        assert!(app.armed_quit);
        // 敲普通字符 → 解除预备状态。
        handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &mut app);
        assert!(!app.armed_quit);
    }

    #[test]
    fn cursor_insert_and_move() {
        let mut app = App::new("m".into());
        app.input_insert('a');
        app.input_insert('中'); // 3 字节
        app.input_insert('b');
        assert_eq!(app.input, "a中b");
        assert_eq!(app.cursor, app.input.len());
        // 左移跨过 CJK 字符（按 char 边界）。
        app.input_left();
        assert_eq!(app.cursor, "a中".len());
        app.input_left();
        assert_eq!(app.cursor, "a".len());
        // 在中间插入。
        app.input_insert('X');
        assert_eq!(app.input, "aX中b");
        // backspace 删除光标前一个字符。
        app.input_backspace();
        assert_eq!(app.input, "a中b");
        // home/end 跳到端点。
        app.input_home();
        assert_eq!(app.cursor, 0);
        app.input_end();
        assert_eq!(app.cursor, app.input.len());
        // delete 删除光标后字符（光标在末尾 = 空操作）。
        app.input_delete();
        assert_eq!(app.input, "a中b");
        app.input_home();
        app.input_delete();
        assert_eq!(app.input, "中b");
    }

    #[test]
    fn history_up_down_navigates_sent_prompts() {
        let mut app = App::new("m".into());
        app.push_sent("first".into());
        app.push_sent("second".into());
        // 正在输入的草稿。
        app.set_input("draft".into());
        // Up → 最新一条。
        app.history_prev();
        assert_eq!(app.input, "second");
        // Up → 更早一条。
        app.history_prev();
        assert_eq!(app.input, "first");
        // 已在最早，再 Up 不变。
        app.history_prev();
        assert_eq!(app.input, "first");
        // Down → 回到较新一条。
        app.history_next();
        assert_eq!(app.input, "second");
        // Down 越过最新 → 恢复草稿。
        app.history_next();
        assert_eq!(app.input, "draft");
        // 未在浏览时 Down 无效。
        app.history_next();
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn history_dedups_consecutive_and_resets_browse() {
        let mut app = App::new("m".into());
        app.push_sent("a".into());
        app.push_sent("a".into()); // 连续重复不入栈。
        app.push_sent("b".into());
        assert_eq!(app.sent, vec!["a".to_string(), "b".to_string()]);
        // 浏览后提交新 prompt 应重置浏览状态。
        app.history_prev();
        assert!(app.sent_pos.is_some());
        app.push_sent("c".into());
        assert_eq!(app.sent_pos, None);
    }
}
