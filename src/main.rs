use std::collections::HashMap;
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
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Terminal,
};
use ropey::Rope;
use serde::Deserialize;
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};
use ratatui_image::{
    picker::Picker,
    protocol::StatefulProtocol,
    thread::{ThreadImage, ThreadProtocol},
    Resize,
};
use std::sync::mpsc::{self, Receiver, Sender};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Language {
    Rust,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Python,
    Json,
    Toml,
    Yaml,
    Markdown,
    MarkdownInline,
    Php,
}

#[derive(Debug, Deserialize, Default)]
struct Config {
    #[serde(default)]
    extensions: HashMap<String, String>,
}

impl Config {
    fn load() -> Self {
        let config_path = dirs::config_dir()
            .map(|p| p.join("simplide").join("config.toml"));

        if let Some(path) = config_path {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(config) = toml::from_str(&content) {
                    return config;
                }
            }
        }
        Config::default()
    }
}

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
    // Markdown用
    "text.title",
    "text.literal",
    "text.uri",
    "text.reference",
    "text.emphasis",
    "text.strong",
    "punctuation.special",
    "punctuation.delimiter",
    "string.escape",
    "markup.heading",
    "markup.link",
    "markup.list",
    "markup.raw",
    // 追加の一般的なハイライト名
    "tag",
    "label",
    "namespace",
    "module",
    "parameter",
    "field",
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
        // Markdown用
        Some(&"text.title") => Color::Yellow,
        Some(&"text.literal") => Color::Green,
        Some(&"text.uri") => Color::Cyan,
        Some(&"text.reference") => Color::Blue,
        Some(&"text.emphasis") => Color::LightYellow,
        Some(&"text.strong") => Color::LightRed,
        Some(&"punctuation.special") => Color::Magenta,
        Some(&"punctuation.delimiter") => Color::DarkGray,
        Some(&"string.escape") => Color::Red,
        Some(&"markup.heading") => Color::Yellow,
        Some(&"markup.link") => Color::Cyan,
        Some(&"markup.list") => Color::Magenta,
        Some(&"markup.raw") => Color::Green,
        // 追加
        Some(&"tag") => Color::Red,
        Some(&"label") => Color::Yellow,
        Some(&"namespace") => Color::Yellow,
        Some(&"module") => Color::Yellow,
        Some(&"parameter") => Color::White,
        Some(&"field") => Color::Blue,
        _ => Color::White,
    }
}

struct SyntaxHighlighter {
    highlighter: Highlighter,
    configs: HashMap<Language, HighlightConfiguration>,
    extension_map: HashMap<String, Language>,
}

impl SyntaxHighlighter {
    fn new(custom_extensions: &HashMap<String, String>) -> Self {
        let highlighter = Highlighter::new();
        let mut configs = HashMap::new();

        // Rust
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            "", "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::Rust, config);
        }

        // JavaScript
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::INJECTIONS_QUERY,
            tree_sitter_javascript::LOCALS_QUERY,
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::JavaScript, config);
        }

        // TypeScript
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "", "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::TypeScript, config);
        }

        // TSX
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "tsx",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "", "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::Tsx, config);
        }

        // Go
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_go::LANGUAGE.into(),
            "go",
            tree_sitter_go::HIGHLIGHTS_QUERY,
            "", "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::Go, config);
        }

        // Python
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_python::LANGUAGE.into(),
            "python",
            tree_sitter_python::HIGHLIGHTS_QUERY,
            "", "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::Python, config);
        }

        // JSON
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_json::LANGUAGE.into(),
            "json",
            tree_sitter_json::HIGHLIGHTS_QUERY,
            "", "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::Json, config);
        }

        // TOML
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_toml_ng::language().into(),
            "toml",
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            "", "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::Toml, config);
        }

        // YAML
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_yaml::LANGUAGE.into(),
            "yaml",
            tree_sitter_yaml::HIGHLIGHTS_QUERY,
            "", "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::Yaml, config);
        }

        // Markdown block parser
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_md::LANGUAGE.into(),
            "markdown",
            tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            tree_sitter_md::INJECTION_QUERY_BLOCK,
            "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::Markdown, config);
        }

        // Markdown inline parser (for injection callback)
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_md::INLINE_LANGUAGE.into(),
            "markdown_inline",
            tree_sitter_md::HIGHLIGHT_QUERY_INLINE,
            "",
            "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::MarkdownInline, config);
        }

        // PHP
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_php::LANGUAGE_PHP.into(),
            "php",
            tree_sitter_php::HIGHLIGHTS_QUERY,
            tree_sitter_php::INJECTIONS_QUERY,
            "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::Php, config);
        }

        // デフォルトの拡張子マッピング
        let mut extension_map = HashMap::new();
        extension_map.insert("rs".to_string(), Language::Rust);
        extension_map.insert("js".to_string(), Language::JavaScript);
        extension_map.insert("mjs".to_string(), Language::JavaScript);
        extension_map.insert("cjs".to_string(), Language::JavaScript);
        extension_map.insert("jsx".to_string(), Language::JavaScript);
        extension_map.insert("ts".to_string(), Language::TypeScript);
        extension_map.insert("mts".to_string(), Language::TypeScript);
        extension_map.insert("cts".to_string(), Language::TypeScript);
        extension_map.insert("tsx".to_string(), Language::Tsx);
        extension_map.insert("go".to_string(), Language::Go);
        extension_map.insert("py".to_string(), Language::Python);
        extension_map.insert("pyw".to_string(), Language::Python);
        extension_map.insert("json".to_string(), Language::Json);
        extension_map.insert("toml".to_string(), Language::Toml);
        extension_map.insert("yaml".to_string(), Language::Yaml);
        extension_map.insert("yml".to_string(), Language::Yaml);
        extension_map.insert("md".to_string(), Language::Markdown);
        extension_map.insert("markdown".to_string(), Language::Markdown);
        extension_map.insert("php".to_string(), Language::Php);

        // カスタム拡張子マッピングを適用
        for (ext, lang_str) in custom_extensions {
            if let Some(lang) = Self::parse_language(lang_str) {
                extension_map.insert(ext.clone(), lang);
            }
        }

        SyntaxHighlighter {
            highlighter,
            configs,
            extension_map,
        }
    }

    fn parse_language(s: &str) -> Option<Language> {
        match s.to_lowercase().as_str() {
            "rust" | "rs" => Some(Language::Rust),
            "javascript" | "js" => Some(Language::JavaScript),
            "typescript" | "ts" => Some(Language::TypeScript),
            "tsx" => Some(Language::Tsx),
            "go" | "golang" => Some(Language::Go),
            "python" | "py" => Some(Language::Python),
            "json" => Some(Language::Json),
            "toml" => Some(Language::Toml),
            "yaml" | "yml" => Some(Language::Yaml),
            "markdown" | "md" => Some(Language::Markdown),
            "php" => Some(Language::Php),
            _ => None,
        }
    }

    fn detect_language(&self, path: &PathBuf) -> Option<Language> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(|ext| self.extension_map.get(ext).copied())
    }

    /// ファイル全体をハイライトして、各バイト位置に対応する色を返す
    fn highlight_all(&mut self, source: &str, language: Language) -> Vec<Color> {
        let config = match self.configs.get(&language) {
            Some(c) => c,
            None => return vec![Color::White; source.len()],
        };

        // configsへの参照を取得（borrow checkerのためにここで分離）
        let configs = &self.configs;

        // injection callback - 言語名から設定を解決
        let injection_callback = |lang_name: &str| -> Option<&HighlightConfiguration> {
            let lang = match lang_name {
                "rust" => Some(Language::Rust),
                "javascript" | "js" => Some(Language::JavaScript),
                "typescript" | "ts" => Some(Language::TypeScript),
                "tsx" => Some(Language::Tsx),
                "go" => Some(Language::Go),
                "python" => Some(Language::Python),
                "json" => Some(Language::Json),
                "toml" => Some(Language::Toml),
                "yaml" | "yml" => Some(Language::Yaml),
                "markdown" => Some(Language::Markdown),
                "markdown_inline" => Some(Language::MarkdownInline),
                "php" => Some(Language::Php),
                _ => None,
            };
            lang.and_then(|l| configs.get(&l))
        };

        let highlights = match self.highlighter.highlight(config, source.as_bytes(), None, injection_callback) {
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

fn is_image_file(path: &PathBuf) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

fn decode_image(path: &PathBuf) -> Option<image::DynamicImage> {
    image::ImageReader::open(path)
        .ok()?
        .decode()
        .ok()
}

/// 未保存のファイル状態を保持する構造体
struct UnsavedFile {
    buffer: Rope,
    saved_content: String,
    cursor_line: usize,
    cursor_col: usize,
    scroll_offset: usize,
    horizontal_scroll: usize,
}

struct App {
    root_dir: PathBuf,
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
    // 最大行幅キャッシュ（文字数）
    max_line_width: usize,
    // 保存済みの内容（比較用）
    saved_content: String,
    // カーソル追従を有効にするか
    follow_cursor: bool,
    // 現在のファイルの言語
    current_language: Option<Language>,
    // 画像表示用
    picker: Picker,
    image_state: Option<ThreadProtocol>,
    is_image_mode: bool,
    image_loading: bool,
    // 画像リサイズ用スレッド通信
    image_tx: Sender<(StatefulProtocol, Resize, Rect)>,
    image_rx: Receiver<StatefulProtocol>,
    // 画像デコード用スレッド通信
    decode_tx: Sender<(PathBuf, Picker, Sender<(StatefulProtocol, Resize, Rect)>)>,
    decode_rx: Receiver<ThreadProtocol>,
    // 未保存ファイルの保持（タブ機能）
    unsaved_files: HashMap<PathBuf, UnsavedFile>,
    // タブ管理
    tabs: Vec<PathBuf>,
    tab_area: Rect,
    // Git branch
    git_branch: Option<String>,
}

fn get_git_branch(dir: &PathBuf) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

impl App {
    fn new() -> Self {
        let current_dir = std::env::current_dir().unwrap_or_default();
        let root_dir = current_dir.clone();
        let entries = Self::read_dir(&current_dir);
        let git_branch = get_git_branch(&root_dir);
        let config = Config::load();
        let picker = Picker::from_query_stdio()
            .unwrap_or_else(|_| Picker::from_fontsize((8, 12)));

        // 画像リサイズ用のワーカースレッドを起動
        let (tx_worker, rx_worker) = mpsc::channel::<(StatefulProtocol, Resize, Rect)>();
        let (tx_main, rx_main) = mpsc::channel::<StatefulProtocol>();
        std::thread::spawn(move || {
            while let Ok((mut protocol, resize, area)) = rx_worker.recv() {
                protocol.resize_encode(&resize, protocol.background_color(), area);
                let _ = tx_main.send(protocol);
            }
        });

        // 画像デコード用のワーカースレッドを起動
        let (decode_tx, decode_rx_worker) = mpsc::channel::<(PathBuf, Picker, Sender<(StatefulProtocol, Resize, Rect)>)>();
        let (decode_tx_main, decode_rx) = mpsc::channel::<ThreadProtocol>();
        std::thread::spawn(move || {
            while let Ok((path, picker, resize_tx)) = decode_rx_worker.recv() {
                let dyn_img = decode_image(&path);
                if let Some(dyn_img) = dyn_img {
                    // 大きすぎる画像は事前に縮小
                    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                    let max_width = (cols as u32) * 10;
                    let max_height = (rows as u32) * 20;
                    let img = if dyn_img.width() > max_width || dyn_img.height() > max_height {
                        dyn_img.resize(max_width, max_height, image::imageops::FilterType::Nearest)
                    } else {
                        dyn_img
                    };
                    let protocol = picker.new_resize_protocol(img);
                    let thread_protocol = ThreadProtocol::new(resize_tx, protocol);
                    let _ = decode_tx_main.send(thread_protocol);
                }
            }
        });

        App {
            root_dir,
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
            syntax: SyntaxHighlighter::new(&config.extensions),
            source_cache: String::new(),
            highlight_cache: None,
            buffer_dirty: false,
            line_offsets: Vec::new(),
            max_line_width: 0,
            saved_content: String::new(),
            follow_cursor: true,
            current_language: None,
            picker,
            image_state: None,
            is_image_mode: false,
            image_loading: false,
            image_tx: tx_worker,
            image_rx: rx_main,
            decode_tx,
            decode_rx,
            unsaved_files: HashMap::new(),
            tabs: Vec::new(),
            tab_area: Rect::default(),
            git_branch,
        }
    }

    fn read_dir(path: &PathBuf) -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = fs::read_dir(path)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        entries.sort();
        entries
    }

    fn open_file(&mut self, path: &PathBuf) {
        if path.is_file() {
            // 現在のファイルの状態を保存（未保存の場合のみ）
            if let Some(current_path) = &self.file_path.clone() {
                if !self.is_image_mode {
                    if self.is_unsaved() {
                        // 未保存なら保持
                        self.unsaved_files.insert(current_path.clone(), UnsavedFile {
                            buffer: self.buffer.clone(),
                            saved_content: self.saved_content.clone(),
                            cursor_line: self.cursor_line,
                            cursor_col: self.cursor_col,
                            scroll_offset: self.scroll_offset,
                            horizontal_scroll: self.horizontal_scroll,
                        });
                    } else {
                        // 保存済みならメモリから削除
                        self.unsaved_files.remove(current_path);
                    }
                }
            }

            self.file_path = Some(path.clone());

            if is_image_file(path) {
                // 画像ファイルの場合 - 非同期でデコード
                let _ = self.decode_tx.send((path.clone(), self.picker.clone(), self.image_tx.clone()));
                self.image_state = None;
                self.is_image_mode = true;
                self.image_loading = true;
                // テキストバッファはクリア
                self.buffer = Rope::new();
                self.saved_content.clear();
                self.current_language = None;
                self.cursor_line = 0;
                self.cursor_col = 0;
                self.scroll_offset = 0;
                self.horizontal_scroll = 0;
            } else if let Some(unsaved) = self.unsaved_files.remove(path) {
                // 未保存の状態があれば復元
                self.buffer = unsaved.buffer;
                self.saved_content = unsaved.saved_content;
                self.cursor_line = unsaved.cursor_line;
                self.cursor_col = unsaved.cursor_col;
                self.scroll_offset = unsaved.scroll_offset;
                self.horizontal_scroll = unsaved.horizontal_scroll;
                self.current_language = self.syntax.detect_language(path);
                self.image_state = None;
                self.is_image_mode = false;
                self.image_loading = false;
            } else {
                // ディスクから読み込み
                let content = fs::read_to_string(path).unwrap_or_else(|_| String::new());
                self.buffer = Rope::from_str(&content);
                self.saved_content = content;
                self.current_language = self.syntax.detect_language(path);
                self.image_state = None;
                self.is_image_mode = false;
                self.image_loading = false;
                self.cursor_line = 0;
                self.cursor_col = 0;
                self.scroll_offset = 0;
                self.horizontal_scroll = 0;
            }

            self.source_cache.clear();
            self.highlight_cache = None;
            self.line_offsets.clear();
            self.max_line_width = 0;
            self.buffer_dirty = true;
        }
    }

    fn save_file(&mut self) -> io::Result<()> {
        if let Some(path) = &self.file_path {
            let content = self.buffer.to_string();
            fs::write(path, &content)?;
            self.saved_content = content;
        }
        Ok(())
    }

    fn is_unsaved(&self) -> bool {
        self.buffer.to_string() != self.saved_content
    }

    /// 現在のファイルをタブに追加（まだなければ）
    fn add_to_tabs(&mut self) {
        if let Some(path) = &self.file_path {
            if !self.tabs.contains(path) {
                self.tabs.push(path.clone());
            }
        }
    }

    /// 次のタブに切り替え
    fn next_tab(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        if let Some(current) = &self.file_path {
            if let Some(idx) = self.tabs.iter().position(|p| p == current) {
                let next_idx = (idx + 1) % self.tabs.len();
                let next_path = self.tabs[next_idx].clone();
                self.open_file(&next_path);
            }
        }
    }

    /// 前のタブに切り替え
    fn prev_tab(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        if let Some(current) = &self.file_path {
            if let Some(idx) = self.tabs.iter().position(|p| p == current) {
                let prev_idx = if idx == 0 { self.tabs.len() - 1 } else { idx - 1 };
                let prev_path = self.tabs[prev_idx].clone();
                self.open_file(&prev_path);
            }
        }
    }

    /// タブバーのクリック処理
    fn handle_tab_click(&mut self, x: u16, y: u16) {
        if !self.tabs.is_empty()
            && y == self.tab_area.y
            && x >= self.tab_area.x
            && x < self.tab_area.x + self.tab_area.width
        {
            // クリック位置からタブを特定
            let mut current_x = self.tab_area.x;
            for path in &self.tabs.clone() {
                let file_name = path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "New".to_string());

                let is_unsaved = if Some(path) == self.file_path.as_ref() {
                    self.is_unsaved()
                } else {
                    self.unsaved_files.contains_key(path)
                };

                let unsaved_mark = if is_unsaved { "*" } else { "" };
                let tab_text = format!(" {}{} ", file_name, unsaved_mark);
                let tab_len = tab_text.len() as u16;

                if x >= current_x && x < current_x + tab_len {
                    // このタブがクリックされた
                    self.open_file(path);
                    return;
                }

                current_x += tab_len + 1; // +1 for space between tabs
            }
        }
    }

    /// タブを閉じる
    fn close_current_tab(&mut self) {
        if let Some(current) = &self.file_path.clone() {
            // 未保存でなければタブから削除
            if !self.is_unsaved() {
                if let Some(idx) = self.tabs.iter().position(|p| p == current) {
                    self.tabs.remove(idx);
                    self.unsaved_files.remove(current);
                    // 別のタブがあれば切り替え
                    if !self.tabs.is_empty() {
                        let new_idx = idx.min(self.tabs.len() - 1);
                        let new_path = self.tabs[new_idx].clone();
                        self.open_file(&new_path);
                    } else {
                        // タブがなくなったらクリア
                        self.file_path = None;
                        self.buffer = Rope::new();
                        self.saved_content.clear();
                    }
                }
            }
        }
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

    /// カーソル位置までの表示幅を計算（全角文字を考慮）
    fn cursor_display_col(&self) -> usize {
        if self.cursor_line >= self.buffer.len_lines() {
            return 0;
        }
        let line = self.buffer.line(self.cursor_line);
        line.chars()
            .take(self.cursor_col)
            .map(|c| c.width().unwrap_or(1))
            .sum()
    }

    /// 表示幅から文字インデックスを計算（クリック位置→カーソル位置）
    fn display_col_to_char_col(&self, line_idx: usize, display_col: usize) -> usize {
        if line_idx >= self.buffer.len_lines() {
            return 0;
        }
        let line = self.buffer.line(line_idx);
        let mut current_width = 0;
        let mut char_col = 0;
        for ch in line.chars() {
            if ch == '\n' {
                break;
            }
            let ch_width = ch.width().unwrap_or(1);
            if current_width + ch_width > display_col {
                break;
            }
            current_width += ch_width;
            char_col += 1;
        }
        char_col
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
        self.add_to_tabs();
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
        self.add_to_tabs();
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
        self.add_to_tabs();
        self.follow_cursor = true;
        let idx = self.cursor_char_idx();
        if idx < self.buffer.len_chars() {
            self.buffer.remove(idx..idx + 1);
            self.buffer_dirty = true;
        }
    }

    fn move_to_line_start(&mut self) {
        self.follow_cursor = true;
        self.cursor_col = 0;
    }

    fn move_to_line_end(&mut self) {
        self.follow_cursor = true;
        self.cursor_col = self.current_line_len();
    }

    fn kill_line(&mut self) {
        self.add_to_tabs();
        self.follow_cursor = true;
        let line_len = self.current_line_len();
        if self.cursor_col >= line_len {
            // カーソルが行末にある場合、改行を削除（次の行と結合）
            let idx = self.cursor_char_idx();
            if idx < self.buffer.len_chars() {
                self.buffer.remove(idx..idx + 1);
                self.buffer_dirty = true;
            }
        } else {
            // カーソルから行末まで削除
            let start_idx = self.cursor_char_idx();
            let line_start = self.buffer.line_to_char(self.cursor_line);
            let end_idx = line_start + line_len;
            if start_idx < end_idx {
                self.buffer.remove(start_idx..end_idx);
                self.buffer_dirty = true;
            }
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
        let visible_width = self.editor_area.width.saturating_sub(2) as usize;
        let ln_width = self.line_number_width();
        let content_width = visible_width.saturating_sub(ln_width);
        let max_scroll = self.max_line_width.saturating_sub(content_width);

        if delta < 0 {
            self.horizontal_scroll = self.horizontal_scroll.saturating_sub((-delta) as usize);
        } else {
            self.horizontal_scroll = (self.horizontal_scroll + delta as usize).min(max_scroll);
        }
    }

    fn handle_sidebar_click(&mut self, x: u16, y: u16) {
        if x >= self.sidebar_area.x
            && x < self.sidebar_area.x + self.sidebar_area.width
            && y > self.sidebar_area.y
            && y < self.sidebar_area.y + self.sidebar_area.height.saturating_sub(1)
        {
            let visible_index = (y - self.sidebar_area.y - 1) as usize;
            let index = visible_index + self.sidebar_scroll;
            let show_parent = self.current_dir != self.root_dir;

            if show_parent && index == 0 {
                if let Some(parent) = self.current_dir.parent() {
                    self.current_dir = parent.to_path_buf();
                    self.entries = Self::read_dir(&self.current_dir);
                    self.sidebar_scroll = 0;
                }
            } else {
                let entry_index = if show_parent { index - 1 } else { index };
                if entry_index < self.entries.len() {
                    let path = self.entries[entry_index].clone();
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
    }

    fn handle_sidebar_scroll(&mut self, x: u16, y: u16, delta: i16) {
        if x >= self.sidebar_area.x
            && x < self.sidebar_area.x + self.sidebar_area.width
            && y >= self.sidebar_area.y
            && y < self.sidebar_area.y + self.sidebar_area.height
        {
            let show_parent = self.current_dir != self.root_dir;
            let total_items = self.entries.len() + if show_parent { 1 } else { 0 };
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
        // エディタ領域内（ボーダー除く）かつ有効な行をクリックした場合
        if x >= self.editor_area.x + 1
            && x < self.editor_area.x + self.editor_area.width - 1
            && y >= self.editor_area.y + 1
            && y < self.editor_area.y + self.editor_area.height - 1
        {
            self.follow_cursor = true;
            let clicked_line = (y - self.editor_area.y - 1) as usize + self.scroll_offset;

            if clicked_line < self.buffer.len_lines() {
                self.cursor_line = clicked_line;
                // 行番号領域をクリックした場合は行頭に移動
                if x < self.editor_area.x + 1 + ln_width {
                    self.cursor_col = 0;
                } else {
                    // クリック位置（表示幅）から文字インデックスに変換
                    let clicked_display_col = (x - self.editor_area.x - 1 - ln_width) as usize + self.horizontal_scroll;
                    self.cursor_col = self.display_col_to_char_col(clicked_line, clicked_display_col);
                }
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

        // 行オフセットキャッシュを構築し、最大行幅を計算
        self.line_offsets.clear();
        self.line_offsets.push(0);
        self.max_line_width = 0;
        let mut current_line_chars = 0usize;
        for (i, byte) in self.source_cache.bytes().enumerate() {
            if byte == b'\n' {
                self.max_line_width = self.max_line_width.max(current_line_chars);
                self.line_offsets.push(i + 1);
                current_line_chars = 0;
            } else if (byte & 0b11000000) != 0b10000000 {
                // UTF-8の先頭バイトのみカウント（継続バイトは除外）
                current_line_chars += 1;
            }
        }
        // 最終行（改行で終わらない場合）
        self.max_line_width = self.max_line_width.max(current_line_chars);

        // ハイライトキャッシュを更新
        if let Some(lang) = self.current_language {
            if !self.source_cache.is_empty() {
                self.highlight_cache = Some(self.syntax.highlight_all(&self.source_cache, lang));
            } else {
                self.highlight_cache = None;
            }
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
        // 画像デコード完了イベントを受け取る
        if let Ok(thread_protocol) = app.decode_rx.try_recv() {
            app.image_state = Some(thread_protocol);
            app.image_loading = false;
        }

        // 画像リサイズ完了イベントを受け取る
        if let Ok(protocol) = app.image_rx.try_recv() {
            if let Some(ref mut state) = app.image_state {
                state.set_protocol(protocol);
            }
        }

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

            // タブがある場合はエディタ領域を分割
            let (tab_area, editor_area) = if !app.tabs.is_empty() {
                let editor_chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),  // タブバー
                        Constraint::Min(0),      // エディタ
                    ])
                    .split(chunks[1]);
                (Some(editor_chunks[0]), editor_chunks[1])
            } else {
                (None, chunks[1])
            };
            app.tab_area = tab_area.unwrap_or(Rect::default());
            app.editor_area = editor_area;

            // タブバーの描画
            if let Some(tab_rect) = tab_area {
                let mut tab_spans: Vec<Span> = Vec::new();

                for path in app.tabs.iter() {
                    let file_name = path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "New".to_string());

                    // このタブが未保存かチェック
                    let is_unsaved = if Some(path) == app.file_path.as_ref() {
                        app.is_unsaved()
                    } else {
                        app.unsaved_files.contains_key(path)
                    };

                    let is_active = Some(path) == app.file_path.as_ref();
                    let unsaved_mark = if is_unsaved { "*" } else { "" };
                    let tab_text = format!(" {}{} ", file_name, unsaved_mark);

                    let style = if is_active {
                        Style::default().bg(Color::DarkGray).fg(Color::White)
                    } else {
                        Style::default().fg(Color::Gray)
                    };

                    tab_spans.push(Span::styled(tab_text, style));
                    tab_spans.push(Span::raw(" ")); // タブ間のスペース
                }

                let tab_line = Line::from(tab_spans);
                let tab_bar = Paragraph::new(vec![tab_line]);
                frame.render_widget(tab_bar, tab_rect);
            }

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
            let show_parent = app.current_dir != app.root_dir;
            let total_items = entry_names.len() + if show_parent { 1 } else { 0 };

            let items: Vec<ListItem> = (0..visible_height)
                .filter_map(|i| {
                    let idx = app.sidebar_scroll + i;
                    if show_parent {
                        if idx == 0 {
                            Some(ListItem::new(Line::from("..")))
                        } else if idx - 1 < entry_names.len() {
                            Some(ListItem::new(Line::from(entry_names[idx - 1].clone())))
                        } else {
                            None
                        }
                    } else if idx < entry_names.len() {
                        Some(ListItem::new(Line::from(entry_names[idx].clone())))
                    } else {
                        None
                    }
                })
                .collect();

            let dir_name = if app.current_dir == app.root_dir {
                app.root_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "/".to_string())
            } else {
                let root_name = app.root_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let rel_path = app.current_dir
                    .strip_prefix(&app.root_dir)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| app.current_dir.to_string_lossy().to_string());
                format!("{}/{}", root_name, rel_path)
            };
            let title = if total_items > visible_height {
                format!("{} [{}/{}]",
                    dir_name,
                    app.sidebar_scroll + 1,
                    total_items.saturating_sub(visible_height) + 1)
            } else {
                dir_name
            };

            let sidebar = List::new(items)
                .block(Block::default()
                    .title(title)
                    .borders(Borders::ALL));
            frame.render_widget(sidebar, chunks[0]);

            // エディタ
            if app.is_image_mode {
                // 画像モード
                let block = Block::default()
                    .title(format!("{} [Ctrl-C: Quit]", app.file_name()))
                    .borders(Borders::ALL);
                let inner = block.inner(editor_area);
                frame.render_widget(block, editor_area);

                if app.image_loading {
                    // ローディング中
                    let loading = Paragraph::new("Loading...");
                    frame.render_widget(loading, inner);
                } else if let Some(ref mut image_state) = app.image_state {
                    let image_widget = ThreadImage::default();
                    frame.render_stateful_widget(image_widget, inner, image_state);
                }
            } else {
                // テキストモード
                let visible_height = editor_area.height.saturating_sub(2) as usize;
                let visible_width = editor_area.width.saturating_sub(2) as usize;
                let lines = app.get_highlighted_lines(visible_height, visible_width);

                let mut editor_block = Block::default()
                    .title(format!("{}{} [C-S:Save C-W:Close C-]/:Tab C-C:Quit]", app.file_name(), if app.is_unsaved() { " *" } else { "" }))
                    .borders(Borders::ALL);
                if let Some(ref branch) = app.git_branch {
                    editor_block = editor_block.title_top(Line::from(format!(" {} ", branch)).alignment(Alignment::Right));
                }
                let editor = Paragraph::new(lines).block(editor_block);
                frame.render_widget(editor, editor_area);

                // カーソル表示（行番号と横スクロール、全角文字幅を考慮）
                let ln_width = app.line_number_width() as u16;
                let display_col = app.cursor_display_col();
                let cursor_x = editor_area.x + 1 + ln_width + display_col.saturating_sub(app.horizontal_scroll) as u16;
                let cursor_y = editor_area.y + 1 + app.cursor_line.saturating_sub(app.scroll_offset) as u16;
                frame.set_cursor_position((cursor_x, cursor_y));
            }
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
                    // Command-S (macOS) または Ctrl-S で保存
                    if (key.modifiers.contains(KeyModifiers::SUPER) || key.modifiers.contains(KeyModifiers::CONTROL))
                        && key.code == KeyCode::Char('s')
                    {
                        let _ = app.save_file();
                        false
                    } else if key.modifiers.contains(KeyModifiers::CONTROL) {
                        // Emacs keybindings (Ctrl+key)
                        match key.code {
                            KeyCode::Char('c') => true,
                            KeyCode::Char('a') => { app.move_to_line_start(); false }
                            KeyCode::Char('e') => { app.move_to_line_end(); false }
                            KeyCode::Char('f') => { app.move_right(); false }
                            KeyCode::Char('b') => { app.move_left(); false }
                            KeyCode::Char('p') => { app.move_up(); false }
                            KeyCode::Char('n') => { app.move_down(); false }
                            KeyCode::Char('d') => { app.delete_char_delete(); false }
                            KeyCode::Char('h') => { app.delete_char_backspace(); false }
                            KeyCode::Char('k') => { app.kill_line(); false }
                            KeyCode::Char('w') => { app.close_current_tab(); false }  // タブを閉じる
                            KeyCode::Char(']') => { app.next_tab(); false }  // 次のタブ
                            KeyCode::Char('[') => { app.prev_tab(); false }  // 前のタブ
                            _ => false,
                        }
                    } else if key.modifiers.contains(KeyModifiers::ALT) {
                        match key.code {
                            KeyCode::Left => app.horizontal_scroll = app.horizontal_scroll.saturating_sub(5),
                            KeyCode::Right => {
                                let visible_width = app.editor_area.width.saturating_sub(2) as usize;
                                let ln_width = app.line_number_width();
                                let content_width = visible_width.saturating_sub(ln_width);
                                let max_scroll = app.max_line_width.saturating_sub(content_width);
                                app.horizontal_scroll = (app.horizontal_scroll + 5).min(max_scroll);
                            }
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
                            app.handle_tab_click(x, y);
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
}
