//! 自定义 Backend：基于 crossterm 自己实现 ratatui `Backend` trait，
//! **拦截 `get_cursor_position`** 返回追踪值，永不调用 `crossterm::cursor::position()`。
//!
//! ## 为什么需要它
//!
//! ratatui inline viewport 启动 + autoresize 时调用 `Backend::get_cursor_position`
//! 决定锚点行号；ratatui 自带的 `CrosstermBackend` 把它转发到 `crossterm::cursor::position()`，
//! 后者在 unix 上发 `ESC[6n`(DSR 6) 给终端，再从 stdin 读 `ESC[<row>;<col>R` 应答。
//!
//! crossterm 自己的注释（cursor/sys/unix.rs）写得很清楚：
//! > On unix systems, this function will block and possibly time out while
//! > `crossterm::event::read` or `crossterm::event::poll` are being called.
//!
//! 也就是 `EventStream` 后台 reader 与 `cursor::position` 抢同一把内部锁。
//! macOS Terminal.app 上 BSD pty 的 read 会一直阻塞（等键盘输入），锁拿不到，
//! 2 秒后 timeout 报 *"The cursor position could not be read within a normal duration"*。
//! 流式输出每次 autoresize 都会触发 → 表现为整个 TUI 一卡一卡。
//!
//! 同样问题 Codex / Claude Code / Warp 都遇到过；解决方案一致 —— **不依赖 ESC[6n**，
//! 在客户端追踪光标行号，只在程序最早期（EventStream 起来前）做一次真实查询拿到锚点。
//!
//! ## 工作原理
//!
//! - 启动时调一次 `crossterm::cursor::position()`（早于 `EventStream::new()`，无 race）
//!   拿到初始 Y，作为追踪起点
//! - 之后所有 ratatui 调用 `get_cursor_position` 都返回追踪值
//! - `set_cursor_position` 更新追踪值并把 MoveTo 写盘
//! - `append_lines(n)` 把 Y 增加 n（夹紧到终端高度 - 1，模拟 scroll-on-bottom 行为）
//!
//! 实现策略：参照 ratatui-crossterm 0.1.2 的 `CrosstermBackend`，只改 `get_cursor_position`
//! 的语义，其余照搬。

use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::{
    Attribute as CAttribute, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{self as cterm, Clear};
use crossterm::{queue, QueueableCommand};
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};

/// 自实现 ratatui Backend，**永不**调用 `crossterm::cursor::position()`。
pub struct TrackingBackend<W: Write> {
    writer: W,
    /// 追踪光标位置（行号是 ratatui inline 锚点的关键字段；列号仅供完整性）。
    cursor: Position,
}

impl<W: Write> TrackingBackend<W> {
    /// 用初始光标位置构造（必须在 EventStream::new() 之前用真实 `cursor::position()` 取一次）。
    pub fn with_initial_cursor(writer: W, initial: Position) -> Self {
        Self { writer, cursor: initial }
    }
}

impl<W: Write> Write for TrackingBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Write> Backend for TrackingBackend<W> {
    type Error = io::Error;

    /// 把 cells 增量写入终端（与 ratatui-crossterm 默认实现等价：
    /// 维护当前 fg/bg/modifier 状态，遇到变化才发 SetXxx；遇到不连续坐标才发 MoveTo）。
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut modifier = Modifier::empty();
        let mut last_pos: Option<(u16, u16)> = None;
        for (x, y, cell) in content {
            // 不连续坐标 → MoveTo。
            if !matches!(last_pos, Some(p) if x == p.0 + 1 && y == p.1) {
                queue!(self.writer, MoveTo(x, y))?;
            }
            last_pos = Some((x, y));
            // 文本属性变化 → SetAttributes。
            if cell.modifier != modifier {
                let diff = ModifierDiff { from: modifier, to: cell.modifier };
                diff.queue(&mut self.writer)?;
                modifier = cell.modifier;
            }
            if cell.fg != fg {
                queue!(self.writer, SetForegroundColor(to_crossterm_color(cell.fg)))?;
                fg = cell.fg;
            }
            if cell.bg != bg {
                queue!(self.writer, SetBackgroundColor(to_crossterm_color(cell.bg)))?;
                bg = cell.bg;
            }
            queue!(self.writer, Print(cell.symbol()))?;
        }
        // 重置颜色 / 属性，避免影响后续 stdout。
        queue!(
            self.writer,
            SetForegroundColor(crossterm::style::Color::Reset),
            SetBackgroundColor(crossterm::style::Color::Reset),
            SetAttribute(CAttribute::Reset),
        )?;
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.writer.queue(Hide)?;
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.writer.queue(Show)?;
        Ok(())
    }

    /// **不发 ESC[6n** —— 直接返回追踪值。
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let p = position.into();
        self.cursor = p;
        self.writer.queue(MoveTo(p.x, p.y))?;
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.clear_region(ClearType::All)
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.writer.queue(Clear(match clear_type {
            ClearType::All => cterm::ClearType::All,
            ClearType::AfterCursor => cterm::ClearType::FromCursorDown,
            ClearType::BeforeCursor => cterm::ClearType::FromCursorUp,
            ClearType::CurrentLine => cterm::ClearType::CurrentLine,
            ClearType::UntilNewLine => cterm::ClearType::UntilNewLine,
        }))?;
        Ok(())
    }

    /// 写 n 个换行，并把追踪 Y 推进 n 行（夹紧到终端底）。
    /// 终端在到底时会自动 scroll，光标停在最末行——我们的夹紧规则与之一致。
    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        for _ in 0..n {
            self.writer.queue(Print("\n"))?;
        }
        // 更新追踪 Y。raw 模式下 `\n` 一般只移动行不重置列，但
        // ratatui 紧接着会用 set_cursor_position 校正，故只追踪 Y 即可。
        let max_y = match self.size() {
            Ok(s) => s.height.saturating_sub(1),
            Err(_) => self.cursor.y,
        };
        self.cursor.y = self.cursor.y.saturating_add(n).min(max_y);
        Ok(())
    }

    fn size(&self) -> io::Result<Size> {
        let (w, h) = cterm::size()?;
        Ok(Size::new(w, h))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let crossterm::terminal::WindowSize {
            rows,
            columns,
            width,
            height,
        } = cterm::window_size()?;
        Ok(WindowSize {
            columns_rows: Size::new(columns, rows),
            pixels: Size::new(width, height),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// 把 ratatui Color 转成 crossterm Color（与 ratatui-crossterm 实现等价）。
fn to_crossterm_color(c: Color) -> crossterm::style::Color {
    use crossterm::style::Color as CC;
    match c {
        Color::Reset => CC::Reset,
        Color::Black => CC::Black,
        Color::Red => CC::DarkRed,
        Color::Green => CC::DarkGreen,
        Color::Yellow => CC::DarkYellow,
        Color::Blue => CC::DarkBlue,
        Color::Magenta => CC::DarkMagenta,
        Color::Cyan => CC::DarkCyan,
        Color::Gray => CC::Grey,
        Color::DarkGray => CC::DarkGrey,
        Color::LightRed => CC::Red,
        Color::LightGreen => CC::Green,
        Color::LightYellow => CC::Yellow,
        Color::LightBlue => CC::Blue,
        Color::LightMagenta => CC::Magenta,
        Color::LightCyan => CC::Cyan,
        Color::White => CC::White,
        Color::Rgb(r, g, b) => CC::Rgb { r, g, b },
        Color::Indexed(i) => CC::AnsiValue(i),
    }
}

/// 计算 modifier 差异并写出对应的 SetAttribute（与 ratatui-crossterm 实现等价）。
struct ModifierDiff {
    from: Modifier,
    to: Modifier,
}

impl ModifierDiff {
    fn queue<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            w.queue(SetAttribute(CAttribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            w.queue(SetAttribute(CAttribute::NormalIntensity))?;
            // 部分终端 NormalIntensity 也清掉 dim；如果原本要 dim 就重新加。
            if self.to.contains(Modifier::DIM) {
                w.queue(SetAttribute(CAttribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            w.queue(SetAttribute(CAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            w.queue(SetAttribute(CAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            w.queue(SetAttribute(CAttribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            w.queue(SetAttribute(CAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            w.queue(SetAttribute(CAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            w.queue(SetAttribute(CAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) {
            w.queue(SetAttribute(CAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            w.queue(SetAttribute(CAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            w.queue(SetAttribute(CAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) {
            w.queue(SetAttribute(CAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            w.queue(SetAttribute(CAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            w.queue(SetAttribute(CAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            w.queue(SetAttribute(CAttribute::RapidBlink))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn returns_initial_cursor_unchanged() {
        let mut backend = TrackingBackend::with_initial_cursor(Cursor::new(Vec::new()), Position::new(3, 7));
        assert_eq!(backend.get_cursor_position().unwrap(), Position::new(3, 7));
    }

    #[test]
    fn set_cursor_updates_tracked_value() {
        let mut backend = TrackingBackend::with_initial_cursor(
            Cursor::new(Vec::new()),
            Position::new(0, 0),
        );
        backend.set_cursor_position(Position::new(10, 5)).unwrap();
        assert_eq!(backend.get_cursor_position().unwrap(), Position::new(10, 5));
    }
}
