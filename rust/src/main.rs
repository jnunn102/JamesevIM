use std::env;
use std::fs;
use std::io::{self, stdout, Write};
use std::path::{Path, PathBuf};

use crossterm::{
    cursor::{MoveTo, SetCursorStyle, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{
        disable_raw_mode, enable_raw_mode, size, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Insert,
    Command,
    Visual,
}

struct Editor {
    path: Option<PathBuf>,
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    scroll_top: usize,
    visual_anchor: Option<(usize, usize)>,
    mode: Mode,
    command: String,
    pending: Option<char>,
    clipboard: Vec<String>,
    status: String,
    dirty: bool,
    should_quit: bool,
}

impl Editor {
    fn open(path: Option<PathBuf>) -> io::Result<Self> {
        let lines = match path.as_deref() {
            Some(path) if path.exists() => {
                let contents = fs::read_to_string(path)?;
                let mut lines: Vec<String> = contents.lines().map(String::from).collect();
                if contents.ends_with('\n') {
                    lines.push(String::new());
                }
                if lines.is_empty() {
                    lines.push(String::new());
                }
                lines
            }
            _ => vec![String::new()],
        };

        Ok(Self {
            path,
            lines,
            cursor_line: 0,
            cursor_col: 0,
            scroll_top: 0,
            visual_anchor: None,
            mode: Mode::Normal,
            command: String::new(),
            pending: None,
            clipboard: Vec::new(),
            status: String::from("JamesevIM Rust - NORMAL mode"),
            dirty: false,
            should_quit: false,
        })
    }

    fn current_line(&self) -> &str {
        &self.lines[self.cursor_line]
    }

    fn load_path(&mut self, path: PathBuf) -> io::Result<()> {
        let contents = fs::read_to_string(&path)?;
        let mut lines: Vec<String> = contents.lines().map(String::from).collect();
        if contents.ends_with('\n') {
            lines.push(String::new());
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        self.path = Some(path);
        self.lines = lines;
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.scroll_top = 0;
        self.visual_anchor = None;
        self.dirty = false;
        Ok(())
    }

    fn clamp_cursor(&mut self) {
        self.cursor_line = self.cursor_line.min(self.lines.len() - 1);
        self.cursor_col = self.cursor_col.min(self.current_line().len());
        if matches!(self.mode, Mode::Normal | Mode::Visual) && !self.current_line().is_empty() {
            self.cursor_col = self.cursor_col.min(self.current_line().len() - 1);
        }
    }

    fn ensure_visible(&mut self, content_height: usize) {
        if content_height == 0 {
            return;
        }
        if self.cursor_line < self.scroll_top {
            self.scroll_top = self.cursor_line;
        } else if self.cursor_line >= self.scroll_top + content_height {
            self.scroll_top = self.cursor_line + 1 - content_height;
        }
    }

    fn insert_char(&mut self, character: char) {
        self.lines[self.cursor_line].insert(self.cursor_col, character);
        self.cursor_col += character.len_utf8();
        self.dirty = true;
    }

    fn insert_newline(&mut self) {
        let remainder = self.lines[self.cursor_line].split_off(self.cursor_col);
        self.lines.insert(self.cursor_line + 1, remainder);
        self.cursor_line += 1;
        self.cursor_col = 0;
        self.dirty = true;
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let previous = self.current_line()[..self.cursor_col]
                .chars()
                .next_back()
                .expect("cursor column must point after a character");
            let start = self.cursor_col - previous.len_utf8();
            self.lines[self.cursor_line].drain(start..self.cursor_col);
            self.cursor_col = start;
        } else if self.cursor_line > 0 {
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].push_str(&current);
        } else {
            return;
        }
        self.dirty = true;
    }

    fn delete_char(&mut self) {
        if self.current_line().is_empty() {
            return;
        }
        let character = self.current_line()[self.cursor_col..]
            .chars()
            .next()
            .expect("cursor column must be valid");
        let end = self.cursor_col + character.len_utf8();
        self.lines[self.cursor_line].drain(self.cursor_col..end);
        self.dirty = true;
    }

    fn delete_line(&mut self) {
        self.lines.remove(self.cursor_line);
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_line = self.cursor_line.min(self.lines.len() - 1);
        self.cursor_col = 0;
        self.dirty = true;
    }

    fn yank_line(&mut self) {
        self.clipboard = vec![self.current_line().to_string()];
        self.status = "1 line yanked".into();
    }

    fn put_lines(&mut self, before: bool) {
        if self.clipboard.is_empty() {
            self.status = "Nothing to put".into();
            return;
        }
        let index = if before {
            self.cursor_line
        } else {
            self.cursor_line + 1
        };
        let count = self.clipboard.len();
        self.lines.splice(index..index, self.clipboard.clone());
        self.cursor_line = index;
        self.cursor_col = 0;
        self.dirty = true;
        self.status = format!("{count} line{} put", if count == 1 { "" } else { "s" });
    }

    fn join_line(&mut self) {
        if self.cursor_line + 1 >= self.lines.len() {
            self.status = "E294: cannot join last line".into();
            return;
        }
        let next = self.lines.remove(self.cursor_line + 1);
        if !self.lines[self.cursor_line].is_empty() {
            self.lines[self.cursor_line].push(' ');
        }
        self.lines[self.cursor_line].push_str(next.trim_start());
        self.cursor_col = self
            .cursor_col
            .min(self.current_line().len().saturating_sub(1));
        self.dirty = true;
    }

    fn visual_bounds(&self) -> Option<(usize, usize)> {
        let (anchor, current) = (self.visual_anchor?.0, self.cursor_line);
        Some((anchor.min(current), anchor.max(current)))
    }

    fn delete_visual_lines(&mut self) {
        let Some((first, last)) = self.visual_bounds() else {
            return;
        };
        self.clipboard = self.lines[first..=last].to_vec();
        self.lines.drain(first..=last);
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_line = first.min(self.lines.len() - 1);
        self.cursor_col = 0;
        self.visual_anchor = None;
        self.mode = Mode::Normal;
        self.dirty = true;
    }

    fn yank_visual_lines(&mut self) {
        let Some((first, last)) = self.visual_bounds() else {
            return;
        };
        self.clipboard = self.lines[first..=last].to_vec();
        self.status = format!("{} lines yanked", last - first + 1);
        self.visual_anchor = None;
        self.mode = Mode::Normal;
    }

    fn save(&mut self, requested_path: Option<&Path>) -> io::Result<()> {
        if let Some(path) = requested_path {
            self.path = Some(path.to_path_buf());
        }
        let path = self.path.as_deref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no file name; use :w <path>")
        })?;
        let mut contents = self.lines.join("\n");
        if !contents.is_empty() {
            contents.push('\n');
        }
        fs::write(path, contents)?;
        self.dirty = false;
        self.status = format!("\"{}\" written", path.display());
        Ok(())
    }

    fn execute_command(&mut self) -> io::Result<()> {
        let command = std::mem::take(&mut self.command);
        match command.as_str() {
            "q" => {
                if self.dirty {
                    self.status = "No write since last change (use :wq or :q!)".into();
                } else {
                    self.should_quit = true;
                }
            }
            "q!" => self.should_quit = true,
            "w" => self.save(None)?,
            "wq" | "x" | "wqa" => {
                self.save(None)?;
                self.should_quit = true;
            }
            "wq!" | "x!" | "wqa!" => {
                self.save(None)?;
                self.should_quit = true;
            }
            "qa" | "qall" => {
                if self.dirty {
                    self.status = "No write since last change (use :wqa or :qa!)".into();
                } else {
                    self.should_quit = true;
                }
            }
            "qa!" | "qall!" => self.should_quit = true,
            "enew" => {
                if self.dirty {
                    self.status = "No write since last change (use :w or :enew!)".into();
                } else {
                    self.path = None;
                    self.lines = vec![String::new()];
                    self.cursor_line = 0;
                    self.cursor_col = 0;
                    self.scroll_top = 0;
                    self.visual_anchor = None;
                    self.status = "[No Name]".into();
                }
            }
            "pwd" => self.status = env::current_dir()?.display().to_string(),
            "file" => {
                self.status = format!(
                    "\"{}\" {} lines{}",
                    self.path
                        .as_deref()
                        .map_or("[No Name]".into(), |path| path.display().to_string()),
                    self.lines.len(),
                    if self.dirty { " [+]" } else { "" }
                );
            }
            "version" => {
                self.status = "JamesevIM Rust 0.1.0 (Vim-compatible migration)".into();
            }
            "set number" | "set nu" => self.status = "line numbers are enabled".into(),
            "set nonumber" | "set nonu" => {
                self.status = "line numbers cannot be hidden in this build".into()
            }
            "d" | "delete" => {
                self.clipboard = vec![self.current_line().to_string()];
                self.delete_line();
            }
            "y" | "yank" => self.yank_line(),
            "p" | "put" => self.put_lines(false),
            "P" | "Put" => self.put_lines(true),
            "j" | "join" => self.join_line(),
            "help" => self.status = "Core: :e :w :wq :q :qa | Edit: :d :y :p :P :j".into(),
            command if command.starts_with("e ") || command.starts_with("edit ") => {
                let path = command
                    .split_once(' ')
                    .map(|(_, path)| path.trim())
                    .unwrap_or_default();
                if path.is_empty() {
                    self.status = "Usage: :edit <path>".into();
                } else if self.dirty {
                    self.status = "No write since last change (use :w or :edit!)".into();
                } else {
                    self.load_path(PathBuf::from(path))?;
                    self.status = format!("\"{path}\" loaded");
                }
            }
            command if command.starts_with("w ") => {
                let path = command[2..].trim();
                self.save(Some(Path::new(path)))?;
            }
            command if command.starts_with("wq ") => {
                let path = command[3..].trim();
                self.save(Some(Path::new(path)))?;
                self.should_quit = true;
            }
            command if command.starts_with("x ") => {
                let path = command[2..].trim();
                self.save(Some(Path::new(path)))?;
                self.should_quit = true;
            }
            _ => self.status = format!("Not an editor command: {command}"),
        }
        self.mode = Mode::Normal;
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> io::Result<()> {
        match self.mode {
            Mode::Insert => {
                self.handle_insert_key(key);
                Ok(())
            }
            Mode::Command => self.handle_command_key(key),
            Mode::Normal => self.handle_normal_key(key),
            Mode::Visual => self.handle_visual_key(key),
        }
    }

    fn handle_insert_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.clamp_cursor();
                self.status = "NORMAL mode".into();
            }
            KeyCode::Enter => self.insert_newline(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Left => self.cursor_col = self.cursor_col.saturating_sub(1),
            KeyCode::Right => {
                self.cursor_col = (self.cursor_col + 1).min(self.current_line().len())
            }
            KeyCode::Up => self.cursor_line = self.cursor_line.saturating_sub(1),
            KeyCode::Down => self.cursor_line = (self.cursor_line + 1).min(self.lines.len() - 1),
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_char(character)
            }
            _ => {}
        }
        self.clamp_cursor();
    }

    fn handle_command_key(&mut self, key: KeyEvent) -> io::Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.command.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.execute_command()?,
            KeyCode::Backspace => {
                self.command.pop();
                if self.command.is_empty() {
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::Char(character) => self.command.push(character),
            _ => {}
        }
        Ok(())
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> io::Result<()> {
        if let Some(pending) = self.pending.take() {
            match (pending, key.code) {
                ('d', KeyCode::Char('d')) => {
                    self.clipboard = vec![self.current_line().to_string()];
                    self.delete_line();
                }
                ('y', KeyCode::Char('y')) => self.yank_line(),
                _ => self.status = format!("operator {pending} canceled"),
            }
            self.clamp_cursor();
            return Ok(());
        }

        match key.code {
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command.clear();
            }
            KeyCode::Char('v') => {
                self.mode = Mode::Visual;
                self.visual_anchor = Some((self.cursor_line, self.cursor_col));
                self.status = "VISUAL mode".into();
            }
            KeyCode::Char('i') => self.mode = Mode::Insert,
            KeyCode::Char('a') => {
                self.cursor_col = (self.cursor_col + 1).min(self.current_line().len());
                self.mode = Mode::Insert;
            }
            KeyCode::Char('o') => {
                self.cursor_col = self.current_line().len();
                self.insert_newline();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('x') => self.delete_char(),
            KeyCode::Char('d') | KeyCode::Char('y') => {
                self.pending = match key.code {
                    KeyCode::Char(character) => Some(character),
                    _ => None,
                };
            }
            KeyCode::Char('p') => self.put_lines(false),
            KeyCode::Char('P') => self.put_lines(true),
            KeyCode::Char('J') => self.join_line(),
            KeyCode::Char('h') | KeyCode::Left => {
                self.cursor_col = self.cursor_col.saturating_sub(1)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.cursor_col =
                    (self.cursor_col + 1).min(self.current_line().len().saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor_line = self.cursor_line.saturating_sub(1)
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor_line = (self.cursor_line + 1).min(self.lines.len() - 1)
            }
            _ => {}
        }
        self.clamp_cursor();
        Ok(())
    }

    fn handle_visual_key(&mut self, key: KeyEvent) -> io::Result<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('v') => {
                self.mode = Mode::Normal;
                self.visual_anchor = None;
                self.status = "NORMAL mode".into();
            }
            KeyCode::Char('d') => self.delete_visual_lines(),
            KeyCode::Char('y') => self.yank_visual_lines(),
            KeyCode::Char('h') | KeyCode::Left => {
                self.cursor_col = self.cursor_col.saturating_sub(1)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.cursor_col =
                    (self.cursor_col + 1).min(self.current_line().len().saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor_line = self.cursor_line.saturating_sub(1)
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor_line = (self.cursor_line + 1).min(self.lines.len() - 1)
            }
            _ => {}
        }
        self.clamp_cursor();
        Ok(())
    }

    fn render(&mut self) -> io::Result<()> {
        let mut output = stdout();
        let (width, height) = size()?;
        let content_height = height.saturating_sub(2) as usize;
        self.ensure_visible(content_height);

        queue!(output, MoveTo(0, 0), Clear(ClearType::All))?;
        let visual_bounds = self.visual_bounds();
        for (row, (index, line)) in self
            .lines
            .iter()
            .enumerate()
            .skip(self.scroll_top)
            .take(content_height)
            .enumerate()
        {
            let marker = if index == self.cursor_line { ">" } else { " " };
            let available = width.saturating_sub(7) as usize;
            let visible = line.chars().take(available).collect::<String>();
            queue!(
                output,
                MoveTo(0, row as u16),
                if visual_bounds.is_some_and(|(first, last)| (first..=last).contains(&index)) {
                    SetAttribute(Attribute::Reverse)
                } else {
                    SetAttribute(Attribute::Reset)
                },
                Print(format!("{marker}{:>4} {visible}", index + 1)),
                SetAttribute(Attribute::Reset)
            )?;
        }

        let mode = match self.mode {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Command => "COMMAND",
            Mode::Visual => "VISUAL",
        };
        let status = if self.mode == Mode::Command {
            format!(":{}", self.command)
        } else {
            format!(
                "{mode}{} | {}",
                if self.dirty { " [+]" } else { "" },
                self.status
            )
        };
        queue!(
            output,
            MoveTo(0, height.saturating_sub(2)),
            SetAttribute(Attribute::Reverse),
            Print(format!("{:<width$}", status, width = width as usize)),
            SetAttribute(Attribute::Reset),
            MoveTo(0, height.saturating_sub(1)),
            Print(format!(
                "{}:{}  {}",
                self.cursor_line + 1,
                self.cursor_col + 1,
                self.path
                    .as_deref()
                    .map_or("[No Name]".into(), |p| p.display().to_string())
            ))
        )?;

        let (cursor_x, cursor_y) = if self.mode == Mode::Command {
            (
                (self.command.len() + 1).min(width.saturating_sub(1) as usize) as u16,
                height.saturating_sub(2),
            )
        } else {
            (
                (self.cursor_col + 6).min(width.saturating_sub(1) as usize) as u16,
                self.cursor_line.saturating_sub(self.scroll_top) as u16,
            )
        };
        queue!(
            output,
            SetCursorStyle::BlinkingBar,
            Show,
            MoveTo(cursor_x, cursor_y)
        )?;
        output.flush()
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(
            stdout(),
            EnterAlternateScreen,
            Show,
            SetCursorStyle::BlinkingBar
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), Show, LeaveAlternateScreen);
    }
}

fn main() -> io::Result<()> {
    let path = env::args_os().nth(1).map(PathBuf::from);
    let mut editor = Editor::open(path)?;
    let _terminal = TerminalGuard::enter()?;

    while !editor.should_quit {
        editor.render()?;
        if let Event::Key(key) = event::read()? {
            editor.handle_key(key)?;
        }
    }

    Ok(())
}
