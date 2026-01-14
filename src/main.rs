use std::fs;
use std::io;
use std::panic;
use std::path::PathBuf;

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Terminal,
};
use ropey::Rope;
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

fn reset_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}

// ハイライト名とカラーのマッピング
const HIGHLIGHT_NAMES: &[&str] = &[
    "keyword",
    "function",
    "type",
    "string",
    "number",
    "comment",
    "variable",
    "operator",
    "punctuation",
    "constant",
    "attribute",
    "property",
];

fn highlight_color(highlight: Highlight) -> Color {
    match HIGHLIGHT_NAMES.get(highlight.0) {
        Some(&"keyword") => Color::Magenta,
        Some(&"function") => Color::Blue,
        Some(&"type") => Color::Yellow,
        Some(&"string") => Color::Green,
        Some(&"number") => Color::Cyan,
        Some(&"comment") => Color::DarkGray,
        Some(&"variable") => Color::White,
        Some(&"operator") => Color::Red,
        Some(&"punctuation") => Color::White,
        Some(&"constant") => Color::Cyan,
        Some(&"attribute") => Color::Yellow,
        Some(&"property") => Color::Blue,
        _ => Color::White,
    }
}

struct SyntaxHighlighter {
    highlighter: Highlighter,
    rust_config: Option<HighlightConfiguration>,
}

impl SyntaxHighlighter {
    fn new() -> Self {
        let highlighter = Highlighter::new();

        let mut rust_config = HighlightConfiguration::new(
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            "",  // injections
            "",  // locals
        ).ok();

        if let Some(ref mut config) = rust_config {
            config.configure(HIGHLIGHT_NAMES);
        }

        SyntaxHighlighter {
            highlighter,
            rust_config,
        }
    }

    /// ファイル全体をハイライトして、各バイト位置に対応する色を返す
    fn highlight_all(&mut self, source: &str) -> Vec<Color> {
        let config = match &self.rust_config {
            Some(c) => c,
            None => return vec![Color::White; source.len()],
        };

        let highlights = match self.highlighter.highlight(config, source.as_bytes(), None, |_| None) {
            Ok(h) => h,
            Err(_) => return vec![Color::White; source.len()],
        };

        let mut colors = vec![Color::White; source.len()];
        let mut current_color = Color::White;

        for event in highlights {
            match event {
                Ok(HighlightEvent::Source { start, end }) => {
                    for i in start..end.min(colors.len()) {
                        colors[i] = current_color;
                    }
                }
                Ok(HighlightEvent::HighlightStart(h)) => {
                    current_color = highlight_color(h);
                }
                Ok(HighlightEvent::HighlightEnd) => {
                    current_color = Color::White;
                }
                Err(_) => break,
            }
        }

        colors
    }
}

struct App {
    current_dir: PathBuf,
    entries: Vec<PathBuf>,
    buffer: Rope,
    file_path: Option<PathBuf>,
    cursor_line: usize,
    cursor_col: usize,
    sidebar_area: Rect,
    editor_area: Rect,
    scroll_offset: usize,
    horizontal_scroll: usize,
    sidebar_scroll: usize,
    syntax: SyntaxHighlighter,
    // キャッシュ
    source_cache: String,
    highlight_cache: Option<Vec<Color>>,
    buffer_dirty: bool,
    // 行オフセットキャッシュ（バイト位置）
    line_offsets: Vec<usize>,
    // カーソル追従を有効にするか
    follow_cursor: bool,
}

impl App {
    fn new() -> Self {
        let current_dir = std::env::current_dir().unwrap_or_default();
        let entries = Self::read_dir(&current_dir);
        App {
            current_dir,
            entries,
            buffer: Rope::new(),
            file_path: None,
            cursor_line: 0,
            cursor_col: 0,
            sidebar_area: Rect::default(),
            editor_area: Rect::default(),
            scroll_offset: 0,
            horizontal_scroll: 0,
            sidebar_scroll: 0,
            syntax: SyntaxHighlighter::new(),
            source_cache: String::new(),
            highlight_cache: None,
            buffer_dirty: false,
            line_offsets: Vec::new(),
            follow_cursor: true,
        }
    }

    fn read_dir(path: &PathBuf) -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = fs::read_dir(path)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        entries.sort();
        entries
    }

    fn is_rust_file(&self) -> bool {
        self.file_path
            .as_ref()
            .and_then(|p| p.extension())
            .map(|ext| ext == "rs")
            .unwrap_or(false)
    }

    fn open_file(&mut self, path: &PathBuf) {
        if path.is_file() {
            let content = fs::read_to_string(path).unwrap_or_else(|_| String::new());
            self.buffer = Rope::from_str(&content);
            self.file_path = Some(path.clone());
            self.cursor_line = 0;
            self.cursor_col = 0;
            self.scroll_offset = 0;
            self.horizontal_scroll = 0;
            self.source_cache.clear();
            self.highlight_cache = None;
            self.line_offsets.clear();
            self.buffer_dirty = true;
        }
    }

    fn save_file(&self) -> io::Result<()> {
        if let Some(path) = &self.file_path {
            fs::write(path, self.buffer.to_string())?;
        }
        Ok(())
    }

    fn file_name(&self) -> String {
        self.file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "New File".to_string())
    }

    fn current_line_len(&self) -> usize {
        if self.cursor_line >= self.buffer.len_lines() {
            return 0;
        }
        let line = self.buffer.line(self.cursor_line);
        let len = line.len_chars();
        if len > 0 && line.char(len - 1) == '\n' {
            len - 1
        } else {
            len
        }
    }

    fn clamp_cursor_col(&mut self) {
        let line_len = self.current_line_len();
        if self.cursor_col > line_len {
            self.cursor_col = line_len;
        }
    }

    fn cursor_char_idx(&self) -> usize {
        if self.cursor_line >= self.buffer.len_lines() {
            return self.buffer.len_chars();
        }
        let line_start = self.buffer.line_to_char(self.cursor_line);
        let col = self.cursor_col.min(self.current_line_len());
        line_start + col
    }

    fn move_up(&mut self) {
        self.follow_cursor = true;
        if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.clamp_cursor_col();
        }
    }

    fn move_down(&mut self) {
        self.follow_cursor = true;
        if self.cursor_line + 1 < self.buffer.len_lines() {
            self.cursor_line += 1;
            self.clamp_cursor_col();
        }
    }

    fn move_left(&mut self) {
        self.follow_cursor = true;
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.current_line_len();
        }
    }

    fn move_right(&mut self) {
        self.follow_cursor = true;
        let line_len = self.current_line_len();
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.cursor_line + 1 < self.buffer.len_lines() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    fn insert_char(&mut self, c: char) {
        self.follow_cursor = true;
        let idx = self.cursor_char_idx();
        self.buffer.insert_char(idx, c);
        self.buffer_dirty = true;
        if c == '\n' {
            self.cursor_line += 1;
            self.cursor_col = 0;
        } else {
            self.cursor_col += 1;
        }
    }

    fn delete_char_backspace(&mut self) {
        self.follow_cursor = true;
        let idx = self.cursor_char_idx();
        if idx > 0 {
            let prev_char = self.buffer.char(idx - 1);
            self.buffer.remove(idx - 1..idx);
            self.buffer_dirty = true;
            if prev_char == '\n' {
                self.cursor_line -= 1;
                self.cursor_col = self.current_line_len();
            } else {
                self.cursor_col -= 1;
            }
        }
    }

    fn delete_char_delete(&mut self) {
        self.follow_cursor = true;
        let idx = self.cursor_char_idx();
        if idx < self.buffer.len_chars() {
            self.buffer.remove(idx..idx + 1);
            self.buffer_dirty = true;
        }
    }

    fn update_scroll(&mut self) {
        if !self.follow_cursor {
            return;
        }

        // 縦スクロール
        let visible_height = self.editor_area.height.saturating_sub(2) as usize;
        if visible_height > 0 {
            if self.cursor_line < self.scroll_offset {
                self.scroll_offset = self.cursor_line;
            } else if self.cursor_line >= self.scroll_offset + visible_height {
                self.scroll_offset = self.cursor_line.saturating_sub(visible_height) + 1;
            }
        }

        // 横スクロール
        let visible_width = self.editor_area.width.saturating_sub(2) as usize;
        if visible_width > 0 {
            if self.cursor_col < self.horizontal_scroll {
                self.horizontal_scroll = self.cursor_col;
            } else if self.cursor_col >= self.horizontal_scroll + visible_width {
                self.horizontal_scroll = self.cursor_col.saturating_sub(visible_width) + 1;
            }
        }
    }

    fn handle_editor_scroll(&mut self, delta: i16) {
        self.follow_cursor = false; // マウススクロール中はカーソル追従を無効化
        let total_lines = self.buffer.len_lines();
        let visible_height = self.editor_area.height.saturating_sub(2) as usize;
        let max_scroll = total_lines.saturating_sub(visible_height);

        if delta < 0 {
            self.scroll_offset = self.scroll_offset.saturating_sub((-delta) as usize);
        } else {
            self.scroll_offset = (self.scroll_offset + delta as usize).min(max_scroll);
        }
    }

    fn handle_editor_horizontal_scroll(&mut self, delta: i16) {
        self.follow_cursor = false; // マウススクロール中はカーソル追従を無効化
        if delta < 0 {
            self.horizontal_scroll = self.horizontal_scroll.saturating_sub((-delta) as usize);
        } else {
            self.horizontal_scroll += delta as usize;
        }
    }

    fn handle_sidebar_click(&mut self, x: u16, y: u16) {
        if x >= self.sidebar_area.x
            && x < self.sidebar_area.x + self.sidebar_area.width
            && y >= self.sidebar_area.y
            && y < self.sidebar_area.y + self.sidebar_area.height
        {
            let visible_index = (y - self.sidebar_area.y - 1) as usize;
            let index = visible_index + self.sidebar_scroll;

            if index == 0 {
                if let Some(parent) = self.current_dir.parent() {
                    self.current_dir = parent.to_path_buf();
                    self.entries = Self::read_dir(&self.current_dir);
                    self.sidebar_scroll = 0;
                }
            } else if index - 1 < self.entries.len() {
                let path = self.entries[index - 1].clone();
                if path.is_dir() {
                    self.current_dir = path;
                    self.entries = Self::read_dir(&self.current_dir);
                    self.sidebar_scroll = 0;
                } else {
                    self.open_file(&path);
                }
            }
        }
    }

    fn handle_sidebar_scroll(&mut self, x: u16, y: u16, delta: i16) {
        if x >= self.sidebar_area.x
            && x < self.sidebar_area.x + self.sidebar_area.width
            && y >= self.sidebar_area.y
            && y < self.sidebar_area.y + self.sidebar_area.height
        {
            let total_items = self.entries.len() + 1; // +1 for ".."
            let visible_height = self.sidebar_area.height.saturating_sub(2) as usize;
            let max_scroll = total_items.saturating_sub(visible_height);

            if delta < 0 {
                // Scroll up
                self.sidebar_scroll = self.sidebar_scroll.saturating_sub((-delta) as usize);
            } else {
                // Scroll down
                self.sidebar_scroll = (self.sidebar_scroll + delta as usize).min(max_scroll);
            }
        }
    }

    fn handle_editor_click(&mut self, x: u16, y: u16) {
        let ln_width = self.line_number_width() as u16;
        if x >= self.editor_area.x + 1 + ln_width
            && x < self.editor_area.x + self.editor_area.width - 1
            && y >= self.editor_area.y + 1
            && y < self.editor_area.y + self.editor_area.height - 1
        {
            self.follow_cursor = true;
            let clicked_line = (y - self.editor_area.y - 1) as usize + self.scroll_offset;
            let clicked_col = (x - self.editor_area.x - 1 - ln_width) as usize + self.horizontal_scroll;

            if clicked_line < self.buffer.len_lines() {
                self.cursor_line = clicked_line;
                self.cursor_col = clicked_col.min(self.current_line_len());
            }
        }
    }

    fn update_cache(&mut self) {
        if !self.buffer_dirty {
            return;
        }

        // sourceキャッシュを更新
        self.source_cache.clear();
        for chunk in self.buffer.chunks() {
            self.source_cache.push_str(chunk);
        }

        // 行オフセットキャッシュを構築
        self.line_offsets.clear();
        self.line_offsets.push(0);
        for (i, byte) in self.source_cache.bytes().enumerate() {
            if byte == b'\n' {
                self.line_offsets.push(i + 1);
            }
        }

        // ハイライトキャッシュを更新
        if self.is_rust_file() && !self.source_cache.is_empty() {
            self.highlight_cache = Some(self.syntax.highlight_all(&self.source_cache));
        } else {
            self.highlight_cache = None;
        }

        self.buffer_dirty = false;
    }

    fn get_line_from_cache(&self, line_idx: usize) -> Option<(&str, usize)> {
        if line_idx >= self.line_offsets.len() {
            return None;
        }
        let start = self.line_offsets[line_idx];
        let end = if line_idx + 1 < self.line_offsets.len() {
            self.line_offsets[line_idx + 1]
        } else {
            self.source_cache.len()
        };
        // 改行を除いた範囲
        let text_end = if end > start && self.source_cache.as_bytes().get(end - 1) == Some(&b'\n') {
            end - 1
        } else {
            end
        };
        Some((&self.source_cache[start..text_end], start))
    }

    fn line_number_width(&self) -> usize {
        let total = self.buffer.len_lines().max(1);
        let digits = (total as f64).log10().floor() as usize + 1;
        digits + 1 // +1 for space after number
    }

    fn get_highlighted_lines(&mut self, visible_height: usize, visible_width: usize) -> Vec<Line<'static>> {
        // キャッシュを更新
        self.update_cache();

        let mut lines = Vec::with_capacity(visible_height);
        let total_lines = self.line_offsets.len().max(1);
        let ln_width = self.line_number_width();
        let content_width = visible_width.saturating_sub(ln_width);

        for i in 0..visible_height {
            let line_idx = self.scroll_offset + i;
            let line_num = line_idx + 1;

            if line_idx < total_lines {
                // 行番号
                let ln_str = format!("{:>width$} ", line_num, width = ln_width - 1);
                let ln_span = Span::styled(ln_str, Style::default().fg(Color::DarkGray));

                if let Some((line_text, line_start)) = self.get_line_from_cache(line_idx) {
                    if let Some(ref colors) = &self.highlight_cache {
                        let mut spans = vec![ln_span];
                        spans.extend(self.build_spans_from_colors(line_text, line_start, colors, content_width));
                        lines.push(Line::from(spans));
                    } else {
                        let display_text = self.apply_horizontal_scroll(line_text, content_width);
                        lines.push(Line::from(vec![ln_span, Span::raw(display_text)]));
                    }
                } else {
                    lines.push(Line::from(vec![ln_span]));
                }
            } else {
                let ln_str = format!("{:>width$} ", "~", width = ln_width - 1);
                lines.push(Line::from(Span::styled(ln_str, Style::default().fg(Color::DarkGray))));
            }
        }

        lines
    }

    fn apply_horizontal_scroll(&self, line_text: &str, visible_width: usize) -> String {
        let chars: Vec<char> = line_text.chars().collect();
        if self.horizontal_scroll >= chars.len() {
            return String::new();
        }
        chars[self.horizontal_scroll..]
            .iter()
            .take(visible_width)
            .collect()
    }

    fn build_spans_from_colors(&self, line_text: &str, line_start: usize, colors: &[Color], visible_width: usize) -> Vec<Span<'static>> {
        if line_text.is_empty() {
            return vec![];
        }

        let mut result = Vec::new();
        let mut current_color: Option<Color> = None;
        let mut current_text = String::new();
        let mut byte_offset = 0;
        let mut char_index = 0;
        let mut visible_chars = 0;

        for ch in line_text.chars() {
            let byte_pos = line_start + byte_offset;
            let color = colors.get(byte_pos).copied().unwrap_or(Color::White);

            // 横スクロール範囲内の文字のみ処理
            if char_index >= self.horizontal_scroll && visible_chars < visible_width {
                if current_color.is_none() {
                    current_color = Some(color);
                }

                if Some(color) != current_color {
                    if !current_text.is_empty() {
                        result.push(Span::styled(current_text.clone(), Style::default().fg(current_color.unwrap())));
                        current_text.clear();
                    }
                    current_color = Some(color);
                }
                current_text.push(ch);
                visible_chars += 1;
            }

            byte_offset += ch.len_utf8();
            char_index += 1;
        }

        if !current_text.is_empty() {
            if let Some(color) = current_color {
                result.push(Span::styled(current_text, Style::default().fg(color)));
            }
        }

        result
    }
}

fn main() -> io::Result<()> {
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        reset_terminal();
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    loop {
        app.update_scroll();

        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(20),
                    Constraint::Percentage(80),
                ])
                .split(frame.area());

            app.sidebar_area = chunks[0];
            app.editor_area = chunks[1];

            // サイドバー（スクロール対応）
            let entry_names: Vec<String> = app.entries.iter().map(|path| {
                let name = path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if path.is_dir() {
                    format!("{}/", name)
                } else {
                    name
                }
            }).collect();

            let visible_height = chunks[0].height.saturating_sub(2) as usize;
            let total_items = 1 + entry_names.len(); // ".." + entries

            let items: Vec<ListItem> = (0..visible_height)
                .filter_map(|i| {
                    let idx = app.sidebar_scroll + i;
                    if idx == 0 {
                        Some(ListItem::new(Line::from("..")))
                    } else if idx - 1 < entry_names.len() {
                        Some(ListItem::new(Line::from(entry_names[idx - 1].clone())))
                    } else {
                        None
                    }
                })
                .collect();

            let title = if total_items > visible_height {
                format!("{} [{}/{}]",
                    app.current_dir.to_string_lossy(),
                    app.sidebar_scroll + 1,
                    total_items.saturating_sub(visible_height) + 1)
            } else {
                app.current_dir.to_string_lossy().to_string()
            };

            let sidebar = List::new(items)
                .block(Block::default()
                    .title(title)
                    .borders(Borders::ALL));
            frame.render_widget(sidebar, chunks[0]);

            // エディタ
            let visible_height = chunks[1].height.saturating_sub(2) as usize;
            let visible_width = chunks[1].width.saturating_sub(2) as usize;
            let lines = app.get_highlighted_lines(visible_height, visible_width);

            let editor = Paragraph::new(lines)
                .block(Block::default()
                    .title(format!("{} [Ctrl-S: Save, Ctrl-C: Quit]", app.file_name()))
                    .borders(Borders::ALL));
            frame.render_widget(editor, chunks[1]);

            // カーソル表示（行番号と横スクロールを考慮）
            let ln_width = app.line_number_width() as u16;
            let cursor_x = chunks[1].x + 1 + ln_width + app.cursor_col.saturating_sub(app.horizontal_scroll) as u16;
            let cursor_y = chunks[1].y + 1 + app.cursor_line.saturating_sub(app.scroll_offset) as u16;
            frame.set_cursor_position((cursor_x, cursor_y));
        })?;

        // イベントをバッチ処理（溜まっているイベントを全て処理してから描画）
        use std::time::Duration;

        // 最初のイベントを待つ（ブロッキング）
        if !event::poll(Duration::from_millis(16))? {
            continue; // タイムアウト時は再描画
        }

        loop {
            if !event::poll(Duration::from_millis(0))? {
                break;
            }

            let should_break = match event::read()? {
                Event::Key(key) => {
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        match key.code {
                            KeyCode::Char('c') => true,
                            KeyCode::Char('s') => { let _ = app.save_file(); false }
                            _ => false,
                        }
                    } else if key.modifiers.contains(KeyModifiers::ALT) {
                        match key.code {
                            KeyCode::Left => app.horizontal_scroll = app.horizontal_scroll.saturating_sub(5),
                            KeyCode::Right => app.horizontal_scroll += 5,
                            KeyCode::Up => app.scroll_offset = app.scroll_offset.saturating_sub(5),
                            KeyCode::Down => app.scroll_offset += 5,
                            _ => {}
                        }
                        false
                    } else {
                        match key.code {
                            KeyCode::Up => app.move_up(),
                            KeyCode::Down => app.move_down(),
                            KeyCode::Left => app.move_left(),
                            KeyCode::Right => app.move_right(),
                            KeyCode::Backspace => app.delete_char_backspace(),
                            KeyCode::Delete => app.delete_char_delete(),
                            KeyCode::Enter => app.insert_char('\n'),
                            KeyCode::Char(c) => app.insert_char(c),
                            _ => {}
                        }
                        false
                    }
                }
                Event::Mouse(mouse) => {
                    let x = mouse.column;
                    let y = mouse.row;
                    let in_sidebar = x >= app.sidebar_area.x
                        && x < app.sidebar_area.x + app.sidebar_area.width
                        && y >= app.sidebar_area.y
                        && y < app.sidebar_area.y + app.sidebar_area.height;
                    let in_editor = x >= app.editor_area.x
                        && x < app.editor_area.x + app.editor_area.width
                        && y >= app.editor_area.y
                        && y < app.editor_area.y + app.editor_area.height;

                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            app.handle_sidebar_click(x, y);
                            app.handle_editor_click(x, y);
                        }
                        MouseEventKind::ScrollUp => {
                            if in_sidebar {
                                app.handle_sidebar_scroll(x, y, -1);
                            } else if in_editor {
                                app.handle_editor_scroll(-1);
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if in_sidebar {
                                app.handle_sidebar_scroll(x, y, 1);
                            } else if in_editor {
                                app.handle_editor_scroll(1);
                            }
                        }
                        MouseEventKind::ScrollLeft => {
                            if in_editor {
                                app.handle_editor_horizontal_scroll(-2);
                            }
                        }
                        MouseEventKind::ScrollRight => {
                            if in_editor {
                                app.handle_editor_horizontal_scroll(2);
                            }
                        }
                        _ => {}
                    }
                    false
                }
                _ => false,
            };

            if should_break {
                disable_raw_mode()?;
                execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
                return Ok(());
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}
