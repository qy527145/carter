//! 长任务目标（`/goal`）：状态机、持久化、以及**每轮结束后该怎么走**的决策。
//!
//! ## 为什么状态不放进会话文件
//!
//! `RecordKind` 是**封闭枚举**，会话文件里出现一个它不认识的记录类型，`fold_file`
//! 会解析失败 —— 整条对话都读不出来了（不是「丢掉这一条」，是「这个会话打不开」）。
//! 加一种记录类型还会让老版本 carter 打不开新会话。所以目标状态存在
//! `~/.carter/goals/<session_id>.json`，按会话 id 索引，与 `captures/` 同形。
//!
//! ## 为什么决策是一个纯函数
//!
//! 「模型停下了，要不要再推它一把」是这块最容易出错、也最需要被守住的逻辑：
//! 判错一边是模型自己空转烧钱，判错另一边是长任务干到一半就静静停住了。
//! [`decide_after_turn`] 把这件事收成一个（目标状态 + 本轮结果）→ 决策的纯映射，
//! 每条终止规则都能单独测，runner 那边只剩「照着做」。
//!
//! ## 系统提示里绝不能出现会变的东西
//!
//! [`system_segment`] 拼出来的是系统提示的**最后一段**。断点打在「system 末段 /
//! 工具表 / 最新一条 user 消息」上是既定方向，而**三段前缀里任何一段被改动，整个
//! 会话的缓存就全废**（messages 与 tools 都排在 system 之后，system 变一个字节，
//! 后面的 token 前缀全部错位）。所以这里：目标正文与状态进提示（它们只在用户动
//! `/goal` 时才变），轮数这类每轮都变的东西只进界面。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 连续自动继续的上限。
///
/// 不是 token 预算（那要接 usage 记账，且用户明确说不需要），是一道**死循环的闸**：
/// 模型一直不表态、也不干活时，别让它把钱包烧穿。到了上限就转 `paused` 并告诉用户，
/// 用户点一下「继续」就能再来一轮上限。
pub const MAX_AUTO_CONTINUES: u32 = 20;

/// 连续多少轮「一个工具都没调」就判定为空转。
const IDLE_STREAK_LIMIT: u32 = 2;

/// 自动继续时追加的那条用户消息。
///
/// 写成一条**明确的指令**而不是「继续」两个字：模型看到的最后一条消息如果是
/// 用户自己的话，它可能以为用户换了话题；这里要让它知道这是系统在推着走，
/// 以及推不动时该怎么退出。
pub const CONTINUE_PROMPT: &str = "\
（自动继续）长任务目标还没有完成，接着推进。

如果已经完成：调用 goal_update，status = completed，并说明做了什么。
如果被外部条件卡住（缺权限、缺依赖、上游报错…）：调用 goal_update，status = blocked，说明卡在哪。
如果必须由用户拍板才能往下走（删文件、改协议、选方案…）：调用 goal_update，status = needs_user，把问题问清楚。

不要在没有进展的情况下重复同一件事。";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// 正在推进：模型停下而目标未完成时会自动继续。
    Active,
    /// 暂停：模型不再被自动推着走，但目标仍作为上下文（用户点「继续」恢复）。
    Paused,
    /// 模型报告完成。
    Completed,
    /// 模型报告被卡住。
    Blocked,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Completed => "completed",
            GoalStatus::Blocked => "blocked",
        }
    }

    /// 界面上的说法。状态是给人看的，别让前端再维护一份映射。
    pub fn label(self) -> &'static str {
        match self {
            GoalStatus::Active => "进行中",
            GoalStatus::Paused => "已暂停",
            GoalStatus::Completed => "已完成",
            GoalStatus::Blocked => "被卡住",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    /// 目标正文（用户写的那句话）。
    pub objective: String,
    pub status: GoalStatus,
    /// 已经自动继续了多少轮。**只进界面，不进系统提示**（见模块头）。
    #[serde(default)]
    pub auto_continues: u32,
    /// 含本轮的连续「没调任何工具」轮数。用来识别空转。
    #[serde(default)]
    pub idle_streak: u32,
    /// 模型在 `goal_update` 里写的说明（blocked / needs_user 时界面要显出来）。
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub updated_at: u64,
}

impl Goal {
    pub fn new(objective: impl Into<String>) -> Self {
        Self {
            objective: objective.into(),
            status: GoalStatus::Active,
            auto_continues: 0,
            idle_streak: 0,
            note: None,
            updated_at: crate::session::now_ms(),
        }
    }

    /// 换状态时的公共收尾：清掉/保留 note、刷新时间戳。
    fn with_status(mut self, status: GoalStatus, note: Option<String>) -> Self {
        self.status = status;
        self.note = note;
        self.updated_at = crate::session::now_ms();
        self
    }
}

// ---------------------------------------------------------------------------
// 给模型的完成信号
// ---------------------------------------------------------------------------

/// 模型通过 `goal_update` 报上来的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    Completed { note: Option<String> },
    Blocked { note: String },
    NeedsUser { note: String },
}

/// `goal_update` 工具与 runner 之间那一格共享状态。
///
/// 用共享格子而不是通道：工具是在 `run_turn` 里面被 await 的，而 runner 那会儿正
/// 阻塞在 select 上、没有机会收通道里的东西。一格共享状态最简单，也天然只有
/// 「最后一次调用」—— 模型一轮里调两次时，以后者为准。
///
/// `armed` 是给工具用的：`goal_update` **恒常注册**（工具表每轮都变会改变请求前缀，
/// 见模块头；而且突然多一个工具会让模型重新打量整个工具列表），所以它必须自己知道
/// 「现在这个会话到底有没有在推目标」，否则模型会在没有目标时也调它、还得到一个
/// 看起来成功的回复。
#[derive(Default)]
pub struct GoalSignal {
    armed: std::sync::atomic::AtomicBool,
    slot: std::sync::Mutex<Option<Signal>>,
}

impl GoalSignal {
    /// 目标是否正在推进（= 状态为 `active`）。由 runner 在每轮开始前设置。
    pub fn set_armed(&self, on: bool) {
        self.armed.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn armed(&self) -> bool {
        self.armed.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set(&self, s: Signal) {
        *self.slot.lock().unwrap() = Some(s);
    }

    /// 取走（读一次就清空）。一轮结束后读，避免影响下一轮。
    pub fn take(&self) -> Option<Signal> {
        self.slot.lock().unwrap().take()
    }
}

// ---------------------------------------------------------------------------
// 每轮结束后的决策
// ---------------------------------------------------------------------------

/// 一轮跑完之后，runner 收集到的「这一轮发生了什么」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalOutcome {
    /// 用户在这一轮里点过「停止」。
    pub user_cancelled: bool,
    /// 这一轮以错误收场（上游 5xx、网络断了…）。
    pub failed: bool,
    /// 这一轮调用过工具（含 MCP）。
    pub used_tools: bool,
    /// **含本轮**的连续无工具轮数。
    pub idle_streak: u32,
    /// 模型通过 `goal_update` 报的结局。
    pub signal: Option<Signal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 什么都不做（目标不活跃，或这一轮不需要继续）。
    None,
    /// 追加一条「继续」再跑一轮。
    Continue,
    /// 改状态，并且要告诉用户为什么 —— 静默停下是最坏的：用户以为它还在干。
    Set {
        status: GoalStatus,
        note: Option<String>,
        notice: String,
    },
}

/// 一轮结束后该怎么走。**纯函数**，六条终止规则各自可测。
///
/// 规则顺序即优先级，不能随意调换：
/// 1. 目标不活跃（暂停 / 完成 / 被卡住）→ 不动；
/// 2. 模型报了结局 → 照它说的落状态（这是唯一的「正常收束」路径）；
/// 3. **用户中断过 → 不继续**。用户按停止是有理由的，自动再推一轮会把「停止」
///    这个动作变成假的；状态留在 `active`，他下次发消息或点「继续」就恢复；
/// 4. 本轮出错 → 转 paused。对着一个正在失败的上游自动重试，是把一次故障
///    放大成 20 次；
/// 5. 连续若干轮一个工具都没调 → 转 paused（模型只是在说话，不是在干活）；
/// 6. 到自动继续上限 → 转 paused。
pub fn decide_after_turn(goal: &Goal, outcome: &GoalOutcome) -> Decision {
    if goal.status != GoalStatus::Active {
        return Decision::None;
    }

    if let Some(signal) = &outcome.signal {
        return match signal {
            Signal::Completed { note } => Decision::Set {
                status: GoalStatus::Completed,
                note: note.clone(),
                notice: "目标已完成".to_string(),
            },
            Signal::Blocked { note } => Decision::Set {
                status: GoalStatus::Blocked,
                note: Some(note.clone()),
                notice: format!("模型报告目标被卡住：{note}"),
            },
            Signal::NeedsUser { note } => Decision::Set {
                status: GoalStatus::Paused,
                note: Some(note.clone()),
                notice: format!("模型需要你拍板才能继续：{note}"),
            },
        };
    }

    if outcome.user_cancelled {
        return Decision::None;
    }

    if outcome.failed {
        return Decision::Set {
            status: GoalStatus::Paused,
            note: None,
            notice:
                "本轮出错，已暂停目标模式（避免对着失败反复重试）。点目标条上的「继续」可以再来。"
                    .to_string(),
        };
    }

    if !outcome.used_tools && outcome.idle_streak >= IDLE_STREAK_LIMIT {
        return Decision::Set {
            status: GoalStatus::Paused,
            note: None,
            notice: format!(
                "连续 {IDLE_STREAK_LIMIT} 轮没有调用任何工具，判断为原地打转，已暂停目标模式。"
            ),
        };
    }

    if goal.auto_continues >= MAX_AUTO_CONTINUES {
        return Decision::Set {
            status: GoalStatus::Paused,
            note: None,
            notice: format!(
                "已连续自动继续 {MAX_AUTO_CONTINUES} 轮，先停下来。点「继续」可以接着推。"
            ),
        };
    }

    Decision::Continue
}

// ---------------------------------------------------------------------------
// 系统提示
// ---------------------------------------------------------------------------

/// 目标在系统提示里那一段。`None` = 不该出现（没有目标，或已经收尾）。
///
/// **不能带轮数、时间戳、note 之外的任何变量** —— 见模块头。
pub fn system_segment(goal: &Goal) -> Option<String> {
    match goal.status {
        GoalStatus::Active => Some(format!(
            "## 长任务目标\n{}\n\n\
             这是用户在本次会话里设定的目标，未完成之前要持续推进。\n\
             - 完成之后调用 `goal_update`（status = \"completed\"）说明结果；\n\
             - 被外部条件卡住、或必须由用户拍板才能往下走时，也调用 `goal_update`\n  \
               （status = \"blocked\" / \"needs_user\"）说明卡在哪 —— 不要自己替用户做决定；\n\
             - 你停下而目标还没完成时，本应用会自动再发起一轮「继续」。",
            goal.objective
        )),
        GoalStatus::Paused => Some(format!(
            "## 长任务目标（已暂停）\n{}\n\n\
             用户暂停了这个目标：除非用户明确要求，不要主动推进它。",
            goal.objective
        )),
        // 已完成 / 被卡住：模型刚在上下文里说过，再复述一遍只是烧 token。
        GoalStatus::Completed | GoalStatus::Blocked => None,
    }
}

// ---------------------------------------------------------------------------
// 持久化
// ---------------------------------------------------------------------------
//
// `~/.carter-app/goals/<session_id>.json` —— 一个会话一个文件，与 `captures/` 同形。
// 不共用一个 `goals.json` 是因为主会话与侧边会话可能同时活着，共用就得处理
// 「读改写」的互相覆盖；而会话 id 本身就是索引键，分开存不花任何代价。

/// 会话 id → 文件名。id 是 uuid（侧边会话是 `side-<uuid>`），理论上不会有路径分隔符，
/// 但这里是**拼路径**的地方，一个 `../` 就能写到目录外面去 —— 稳妥起见过一遍白名单。
fn file_for(session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    crate::config::paths::carter_home()
        .join("goals")
        .join(format!("{safe}.json"))
}

/// 读某个会话的目标。没有（或文件坏了）就是没有 —— 坏掉的目标不该让会话起不来。
pub fn load(session_id: &str) -> Option<Goal> {
    load_from(&file_for(session_id))
}

fn load_from(path: &Path) -> Option<Goal> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// 写。**先写临时文件再改名** —— 直接覆盖时中途断电会留下半截 JSON，
/// 那份目标就永远读不回来了。
pub fn save(session_id: &str, goal: &Goal) -> Result<(), String> {
    save_to(&file_for(session_id), goal)
}

fn save_to(path: &Path, goal: &Goal) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败：{e}"))?;
    }
    let json = serde_json::to_string_pretty(goal).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("写 {} 失败：{e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("改名到 {} 失败：{e}", path.display()))
}

/// 删掉某个会话的目标。会话被删时调用，否则这个目录会跟着历史会话一直长。
pub fn remove(session_id: &str) -> bool {
    std::fs::remove_file(file_for(session_id)).is_ok()
}

/// 会话刚起来时该拿什么目标。
///
/// **盘上处于 `active` 的一律降级成 `paused`。** 用户关掉应用时目标还在跑，
/// 下次打开如果原样恢复，模型会在人不在的时候自己开工烧 token。降级 + 一句说明，
/// 让他自己点「继续」。
pub fn on_session_start(mut goal: Goal) -> (Goal, bool) {
    if goal.status != GoalStatus::Active {
        return (goal, false);
    }
    goal.status = GoalStatus::Paused;
    goal.note = None;
    goal.updated_at = crate::session::now_ms();
    (goal, true)
}

/// 宿主用的目标视图（IPC / 状态栏）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GoalView {
    pub objective: String,
    pub status: String,
    pub status_label: String,
    /// 已经自动继续了多少轮。
    pub auto_continues: u32,
    /// 模型在 `goal_update` 里写的说明（被卡住 / 需要拍板时界面要显出来）。
    pub note: Option<String>,
}

/// 界面上的目标动作。拼写由输入框那一侧解析一次，后端只认结构化枚举。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GoalCommand {
    /// 设定 / 改写目标正文。
    Set {
        objective: String,
    },
    Clear,
    Pause,
    Resume,
    /// 标记完成（用户在目标条上点「完成」）。
    Complete,
}

/// 界面用的目标视图（IPC）。
pub fn view(goal: &Goal) -> GoalView {
    GoalView {
        objective: goal.objective.clone(),
        status: goal.status.as_str().to_string(),
        status_label: goal.status.label().to_string(),
        auto_continues: goal.auto_continues,
        note: goal.note.clone(),
    }
}

/// 状态的迁移入口，供 `Command::Goal` 分支调用。返回 `None` = 这个动作在当前
/// 状态下没有意义（例如「继续」一个不存在的目标），调用方据此给提示。
pub fn apply_action(current: Option<Goal>, action: &GoalCommand) -> Option<Option<Goal>> {
    use GoalCommand as A;
    match action {
        A::Set { objective } => {
            let text = objective.trim();
            if text.is_empty() {
                return None;
            }
            // 已有目标时改的是**正文**，状态与计数保留 —— 用户说「改成 X」时
            // 多半是想接着干，而不是想重来。
            //
            // 例外：上一个目标已经收尾（完成 / 被卡住）。那时这就是一件**新事**，
            // 计数得从头来 —— 否则新目标会立刻撞上上一件活攒下的自动继续上限。
            Some(Some(match current {
                Some(mut g) => {
                    if matches!(g.status, GoalStatus::Completed | GoalStatus::Blocked) {
                        g.auto_continues = 0;
                        g.idle_streak = 0;
                    }
                    g.objective = text.to_string();
                    g.status = GoalStatus::Active;
                    g.note = None;
                    g.updated_at = crate::session::now_ms();
                    g
                }
                None => Goal::new(text),
            }))
        }
        A::Clear => current.map(|_| None),
        A::Pause => current.filter(|g| g.status == GoalStatus::Active).map(|g| {
            let note = g.note.clone();
            Some(g.with_status(GoalStatus::Paused, note))
        }),
        A::Resume => current
            .filter(|g| g.status != GoalStatus::Active)
            .map(|mut g| {
                // 恢复时把计数与空转计数归零：用户这一下点击就是一轮新的开始，
                // 否则「继续」会立刻又撞在上限上。
                g.auto_continues = 0;
                g.idle_streak = 0;
                Some(g.with_status(GoalStatus::Active, None))
            }),
        A::Complete => current
            .filter(|g| g.status != GoalStatus::Completed)
            .map(|g| {
                let note = g.note.clone();
                Some(g.with_status(GoalStatus::Completed, note))
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active() -> Goal {
        Goal::new("把 crates/core 的告警清零")
    }

    fn outcome() -> GoalOutcome {
        GoalOutcome {
            used_tools: true,
            ..Default::default()
        }
    }

    /// **这条是这套设计的地基之一**：系统提示的末段正是 cache 断点的位置，
    /// 里面出现任何每轮都变的东西，长任务越跑越贵。
    #[test]
    fn the_system_segment_never_carries_a_counter() {
        let mut g = active();
        g.auto_continues = 7;
        g.idle_streak = 2;
        let seg = system_segment(&g).expect("进行中的目标必须进提示");
        assert!(seg.contains(&g.objective), "{seg}");
        assert!(!seg.contains('7'), "不能出现轮数：{seg}");
        let mut g2 = g.clone();
        g2.auto_continues = 11;
        assert_eq!(system_segment(&g), system_segment(&g2));
    }

    #[test]
    fn finished_goals_leave_the_system_prompt_alone() {
        for st in [GoalStatus::Completed, GoalStatus::Blocked] {
            assert!(system_segment(&active().with_status(st, None)).is_none());
        }
    }

    #[test]
    fn an_unfinished_turn_keeps_going() {
        assert_eq!(decide_after_turn(&active(), &outcome()), Decision::Continue);
    }

    #[test]
    fn the_model_saying_completed_stops_the_loop() {
        let o = GoalOutcome {
            signal: Some(Signal::Completed { note: None }),
            ..outcome()
        };
        assert!(matches!(
            decide_after_turn(&active(), &o),
            Decision::Set {
                status: GoalStatus::Completed,
                ..
            }
        ));
    }

    /// 用户按停止是有理由的，自动再推一轮会把「停止」这个动作变成假的。
    #[test]
    fn a_user_cancel_is_respected() {
        let o = GoalOutcome {
            user_cancelled: true,
            ..outcome()
        };
        assert_eq!(decide_after_turn(&active(), &o), Decision::None);
    }

    #[test]
    fn a_failed_turn_pauses_instead_of_retrying() {
        let o = GoalOutcome {
            failed: true,
            ..outcome()
        };
        assert!(matches!(
            decide_after_turn(&active(), &o),
            Decision::Set {
                status: GoalStatus::Paused,
                ..
            }
        ));
    }

    #[test]
    fn idling_twice_pauses() {
        let o = GoalOutcome {
            used_tools: false,
            idle_streak: 2,
            ..Default::default()
        };
        assert!(matches!(
            decide_after_turn(&active(), &o),
            Decision::Set {
                status: GoalStatus::Paused,
                ..
            }
        ));
    }

    #[test]
    fn the_auto_continue_cap_pauses() {
        let mut g = active();
        g.auto_continues = MAX_AUTO_CONTINUES;
        assert!(matches!(
            decide_after_turn(&g, &outcome()),
            Decision::Set {
                status: GoalStatus::Paused,
                ..
            }
        ));
    }

    /// 盘上 active 的一律降级：用户关掉终端时目标还在跑，下次打开原样恢复
    /// 就意味着人不在的时候模型自己开工烧 token。
    #[test]
    fn a_goal_left_active_by_a_crash_comes_back_paused() {
        let (g, downgraded) = on_session_start(active());
        assert!(downgraded);
        assert_eq!(g.status, GoalStatus::Paused);
    }

    #[test]
    fn resume_resets_the_counters() {
        let mut g = active().with_status(GoalStatus::Paused, None);
        g.auto_continues = 5;
        g.idle_streak = 2;
        let next = apply_action(Some(g), &GoalCommand::Resume)
            .unwrap()
            .unwrap();
        assert_eq!(next.status, GoalStatus::Active);
        assert_eq!(next.auto_continues, 0);
        assert_eq!(next.idle_streak, 0);
    }

    #[test]
    fn a_goal_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "carter-goal-{}-{}",
            std::process::id(),
            crate::session::now_ms()
        ));
        let path = dir.join("g.json");
        let mut g = active();
        g.auto_continues = 3;
        save_to(&path, &g).unwrap();
        assert_eq!(load_from(&path), Some(g));
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
