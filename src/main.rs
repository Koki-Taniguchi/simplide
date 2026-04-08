use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::panic;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use crossterm::{
    event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
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

/// Base64エンコード（OSC 52用）
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;

        result.push(ALPHABET[b0 >> 2] as char);
        result.push(ALPHABET[((b0 & 0x03) << 4) | (b1 >> 4)] as char);
        if chunk.len() > 1 {
            result.push(ALPHABET[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(ALPHABET[b2 & 0x3f] as char);
        } else {
            result.push('=');
        }
    }
    result
}

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
    Make,
    Hcl,
}

// HCL用のハイライトクエリ（tree-sitter-hclには含まれていないため）
const HCL_HIGHLIGHTS_QUERY: &str = r#"
(comment) @comment
(identifier) @variable
(numeric_lit) @number
(bool_lit) @constant.builtin
(null_lit) @constant.builtin
(string_lit) @string
(heredoc_template) @string

(attribute (identifier) @property)
(block (identifier) @keyword)
(block (string_lit) @string)

(function_call (identifier) @function)

["=" "==" "!=" "<" ">" "<=" ">=" "+" "-" "*" "/" "%" "&&" "||" "!"] @operator
["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," "." ":"] @punctuation.delimiter

["for" "endfor" "in" "if" "else" "endif"] @keyword
"#;

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
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture, DisableBracketedPaste);
}

// === Claude Code MCP連携 ===

/// エディタの共有状態（MCPスレッドからアクセス）
#[derive(Clone)]
struct EditorSharedState {
    file_path: String,
    cursor_line: usize,
    cursor_col: usize,
    selected_text: String,
    visible_start: usize,
    visible_end: usize,
    total_lines: usize,
    buffer_content: String,
    open_tabs: Vec<String>,
}

impl Default for EditorSharedState {
    fn default() -> Self {
        Self {
            file_path: String::new(),
            cursor_line: 0,
            cursor_col: 0,
            selected_text: String::new(),
            visible_start: 0,
            visible_end: 0,
            total_lines: 0,
            buffer_content: String::new(),
            open_tabs: Vec::new(),
        }
    }
}

fn handle_mcp_request(request: &str, state: &Arc<Mutex<EditorSharedState>>) -> Option<String> {
    // 最小限のJSON-RPCパーサー
    let id = extract_json_field(request, "id");
    let method = extract_json_string(request, "method")?;

    match method.as_str() {
        "initialize" => {
            Some(format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"protocolVersion":"2024-11-05","capabilities":{{"tools":{{"listChanged":false}}}},"serverInfo":{{"name":"simplide","version":"0.1.0"}}}}}}"#,
                id.unwrap_or("null".to_string())
            ))
        }
        "notifications/initialized" => None,
        "tools/list" => {
            let tools = r#"[{"name":"getDiagnostics","description":"Returns editor state from simplide: open file, cursor position, selected text, visible code range, and open tabs. Optionally scoped to a file path.","inputSchema":{"type":"object","properties":{"uri":{"type":"string","description":"Optional file path to scope diagnostics to"}}}}]"#;
            Some(format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"tools":{}}}}}"#,
                id.unwrap_or("null".to_string()),
                tools
            ))
        }
        "tools/call" => {
            let tool_name = extract_json_string(request, "name").unwrap_or_default();
            let st = state.lock().unwrap().clone();
            let result_text = match tool_name.as_str() {
                "getDiagnostics" => {
                    let tabs_json: Vec<String> = st.open_tabs.iter()
                        .map(|t| format!(r#""{}""#, escape_json(t))).collect();
                    let visible_code = if !st.file_path.is_empty() {
                        let lines: Vec<&str> = st.buffer_content.lines().collect();
                        let start = st.visible_start.saturating_sub(1);
                        let end = st.visible_end.min(lines.len());
                        let visible: Vec<String> = lines[start..end].iter().enumerate()
                            .map(|(i, l)| format!("{}: {}", start + i + 1, l))
                            .collect();
                        visible.join("\\n")
                    } else {
                        String::new()
                    };
                    format!(
                        r#"{{"editor":"simplide","file":"{}","cursor":{{"line":{},"col":{}}},"selected_text":"{}","visible_range":{{"start":{},"end":{}}},"visible_code":"{}","total_lines":{},"open_tabs":[{}]}}"#,
                        escape_json(&st.file_path), st.cursor_line, st.cursor_col,
                        escape_json(&st.selected_text),
                        st.visible_start, st.visible_end,
                        escape_json(&visible_code),
                        st.total_lines,
                        tabs_json.join(",")
                    )
                }
                _ => format!("Unknown tool: {}", tool_name),
            };
            Some(format!(
                r#"{{"jsonrpc":"2.0","id":{},"result":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
                id.unwrap_or("null".to_string()),
                escape_json(&result_text)
            ))
        }
        _ => {
            Some(format!(
                r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":-32601,"message":"Method not found"}}}}"#,
                id.unwrap_or("null".to_string())
            ))
        }
    }
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "").replace('\t', "\\t")
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let pos = json.find(&pattern)?;
    let after = &json[pos + pattern.len()..];
    let colon = after.find(':')?;
    let after_colon = after[colon + 1..].trim_start();
    if after_colon.starts_with('"') {
        let content = &after_colon[1..];
        let end = content.find('"')?;
        Some(content[..end].to_string())
    } else {
        None
    }
}

fn extract_json_field(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let pos = json.find(&pattern)?;
    let after = &json[pos + pattern.len()..];
    let colon = after.find(':')?;
    let after_colon = after[colon + 1..].trim_start();
    // 数値、文字列、null等を取得
    let end = after_colon.find(|c: char| c == ',' || c == '}' || c == ']')?;
    let value = after_colon[..end].trim();
    Some(value.to_string())
}

/// WebSocket MCPサーバーをバックグラウンド起動し、~/.claude/ide/ にロックファイルを書く
fn start_ide_mcp_server(state: Arc<Mutex<EditorSharedState>>, workspace: &str) -> Option<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    let auth_token = uuid::Uuid::new_v4().to_string();

    // ~/.claude/ide/ にロックファイルを作成
    let ide_dir = dirs::home_dir()?.join(".claude").join("ide");
    let _ = fs::create_dir_all(&ide_dir);
    // ディレクトリのパーミッションを0700に
    let _ = fs::set_permissions(&ide_dir, fs::Permissions::from_mode(0o700));

    let lock_path = ide_dir.join(format!("{}.lock", port));
    let lock_content = format!(
        r#"{{"pid":{},"workspaceFolders":["{}"],"ideName":"simplide","transport":"ws","runningInWindows":false,"authToken":"{}"}}"#,
        std::process::id(),
        escape_json(workspace),
        auth_token
    );
    let _ = fs::write(&lock_path, &lock_content);
    let _ = fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600));

    let expected_token = auth_token.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(stream) = stream {
                let st = state.clone();
                let token = expected_token.clone();
                std::thread::spawn(move || {
                    handle_ws_connection(stream, &st, &token);
                });
            }
        }
    });

    Some(port)
}

fn handle_ws_connection(stream: std::net::TcpStream, state: &Arc<Mutex<EditorSharedState>>, _auth_token: &str) {
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(_) => return,
    };

    loop {
        let msg = match ws.read() {
            Ok(msg) => msg,
            Err(_) => break,
        };

        match msg {
            tungstenite::Message::Text(text) => {
                if let Some(response) = handle_mcp_request(&text, state) {
                    if ws.send(tungstenite::Message::Text(response.into())).is_err() {
                        break;
                    }
                    let _ = ws.flush();
                }
            }
            tungstenite::Message::Close(_) => break,
            _ => {}
        }
    }
}

/// 終了時にロックファイルを削除
fn cleanup_ide_lock(port: u16) {
    if let Some(home) = dirs::home_dir() {
        let lock_path = home.join(".claude").join("ide").join(format!("{}.lock", port));
        let _ = fs::remove_file(&lock_path);
    }
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
    "punctuation.bracket",
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
    "constant.builtin",
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
        Some(&"punctuation.bracket") => Color::White,
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
        Some(&"constant.builtin") => Color::Cyan,
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

        // Makefile
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_make::LANGUAGE.into(),
            "make",
            tree_sitter_make::HIGHLIGHTS_QUERY,
            "",
            "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::Make, config);
        }

        // HCL (Terraform)
        if let Ok(mut config) = HighlightConfiguration::new(
            tree_sitter_hcl::LANGUAGE.into(),
            "hcl",
            HCL_HIGHLIGHTS_QUERY,
            "",
            "",
        ) {
            config.configure(HIGHLIGHT_NAMES);
            configs.insert(Language::Hcl, config);
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
        extension_map.insert("mk".to_string(), Language::Make);
        extension_map.insert("tf".to_string(), Language::Hcl);
        extension_map.insert("tfvars".to_string(), Language::Hcl);
        extension_map.insert("hcl".to_string(), Language::Hcl);

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
            "make" | "makefile" => Some(Language::Make),
            "hcl" | "terraform" | "tf" => Some(Language::Hcl),
            _ => None,
        }
    }

    fn detect_language(&self, path: &PathBuf) -> Option<Language> {
        // まずファイル名で判定（Makefileなど拡張子がないファイル用）
        if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
            match file_name {
                "Makefile" | "makefile" | "GNUmakefile" => return Some(Language::Make),
                _ => {}
            }
        }
        // 拡張子で判定
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

        // injection callback - 言語名から設定を解決（エイリアス対応）
        let injection_callback = |lang_name: &str| -> Option<&HighlightConfiguration> {
            let lang = match lang_name {
                "rust" | "rs" => Some(Language::Rust),
                "javascript" | "js" | "jsx" => Some(Language::JavaScript),
                "typescript" | "ts" => Some(Language::TypeScript),
                "tsx" => Some(Language::Tsx),
                "go" | "golang" => Some(Language::Go),
                "python" | "py" | "python3" => Some(Language::Python),
                "json" | "jsonc" => Some(Language::Json),
                "toml" => Some(Language::Toml),
                "yaml" | "yml" => Some(Language::Yaml),
                "markdown" | "md" => Some(Language::Markdown),
                "markdown_inline" => Some(Language::MarkdownInline),
                "php" => Some(Language::Php),
                "make" | "makefile" | "Makefile" => Some(Language::Make),
                "hcl" | "terraform" | "tf" => Some(Language::Hcl),
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
        let mut color_stack: Vec<Color> = Vec::new();

        for event in highlights {
            match event {
                Ok(HighlightEvent::Source { start, end }) => {
                    for i in start..end.min(colors.len()) {
                        colors[i] = current_color;
                    }
                }
                Ok(HighlightEvent::HighlightStart(h)) => {
                    color_stack.push(current_color);
                    current_color = highlight_color(h);
                }
                Ok(HighlightEvent::HighlightEnd) => {
                    current_color = color_stack.pop().unwrap_or(Color::White);
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
    /// ファイルを開いた時の更新日時（外部変更検知用）
    modified_time: Option<SystemTime>,
    /// 外部で変更されたフラグ
    externally_modified: bool,
}

/// テキスト選択範囲を表す構造体
#[derive(Clone, Copy, Debug)]
struct Selection {
    /// 選択開始位置（行、列）
    start: (usize, usize),
    /// 選択終了位置（行、列）
    end: (usize, usize),
}

impl Selection {
    fn new(line: usize, col: usize) -> Self {
        Self {
            start: (line, col),
            end: (line, col),
        }
    }

    /// 正規化された範囲を取得（startが常にendより前になるように）
    fn normalized(&self) -> ((usize, usize), (usize, usize)) {
        if self.start.0 < self.end.0 || (self.start.0 == self.end.0 && self.start.1 <= self.end.1) {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    /// 指定位置が選択範囲内かどうか
    fn contains(&self, line: usize, col: usize) -> bool {
        let ((start_line, start_col), (end_line, end_col)) = self.normalized();
        if line < start_line || line > end_line {
            return false;
        }
        if line == start_line && line == end_line {
            col >= start_col && col < end_col
        } else if line == start_line {
            col >= start_col
        } else if line == end_line {
            col < end_col
        } else {
            true
        }
    }
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
    sidebar_scroll_x: usize,
    needs_clear: bool,
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
    // ファイルの更新日時（外部変更検知用）
    file_modified_time: Option<SystemTime>,
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
    // 確認ダイアログ
    confirm_dialog: Option<ConfirmAction>,
    // 検索機能
    search_mode: bool,
    search_query: String,
    search_matches: Vec<(usize, usize)>,  // (line, col)
    search_index: usize,
    // サイドバー ファイル名フィルタ
    sidebar_filter_mode: bool,
    sidebar_filter_query: String,
    filtered_entries: Vec<PathBuf>,
    sidebar_filter_selected: usize,
    // フォルダ内grep検索
    grep_mode: bool,
    grep_query: String,
    grep_results: Vec<GrepResult>,
    grep_selected: usize,
    grep_scroll: usize,
    grep_scroll_x: usize,
    grep_tx: Sender<(String, PathBuf, Arc<AtomicBool>)>,
    grep_rx: Receiver<Vec<GrepResult>>,
    grep_cancel: Arc<AtomicBool>,
    grep_searching: bool,
    // サイドバーボタン位置
    sidebar_filter_btn: Option<Rect>,
    sidebar_grep_btn: Option<Rect>,
    // テキスト選択
    selection: Option<Selection>,
    is_selecting: bool,
    // コピーボタン表示位置（画面座標）
    copy_button_area: Option<Rect>,
    // Claude Code MCP連携
    last_state_write: Instant,
    shared_state: Arc<Mutex<EditorSharedState>>,
    mcp_port: Option<u16>,
}

#[derive(Clone)]
struct GrepResult {
    path: PathBuf,
    line_number: usize,
    line_content: String,
    match_col: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum ConfirmAction {
    Quit,
    CloseTab,
}

impl App {
    fn new(initial_path: Option<PathBuf>) -> Self {
        // 初期パスの処理
        let (root_dir, current_dir, initial_file) = if let Some(path) = initial_path {
            let abs_path = if path.is_absolute() {
                path
            } else {
                env::current_dir().unwrap_or_default().join(path)
            };

            if abs_path.is_file() {
                // ファイルの場合：親ディレクトリを開き、ファイルを展開
                let parent = abs_path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                (parent.clone(), parent, Some(abs_path))
            } else if abs_path.is_dir() {
                // ディレクトリの場合：そのディレクトリを開く
                (abs_path.clone(), abs_path, None)
            } else {
                // 存在しない場合：カレントディレクトリ
                let cwd = env::current_dir().unwrap_or_default();
                (cwd.clone(), cwd, None)
            }
        } else {
            let cwd = env::current_dir().unwrap_or_default();
            (cwd.clone(), cwd, None)
        };

        let entries = Self::read_dir(&current_dir);
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

        // Grep検索用のワーカースレッドを起動
        let (grep_tx, grep_rx_worker) = mpsc::channel::<(String, PathBuf, Arc<AtomicBool>)>();
        let (grep_tx_main, grep_rx) = mpsc::channel::<Vec<GrepResult>>();
        std::thread::spawn(move || {
            while let Ok((query, root, cancel)) = grep_rx_worker.recv() {
                let mut results = Vec::new();
                App::grep_recursive(&root, &query, &mut results, 500, &cancel);
                if !cancel.load(Ordering::Relaxed) {
                    let _ = grep_tx_main.send(results);
                }
            }
        });

        let mut app = App {
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
            sidebar_scroll_x: 0,
            needs_clear: false,
            syntax: SyntaxHighlighter::new(&config.extensions),
            source_cache: String::new(),
            highlight_cache: None,
            buffer_dirty: false,
            line_offsets: Vec::new(),
            max_line_width: 0,
            saved_content: String::new(),
            file_modified_time: None,
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
            confirm_dialog: None,
            search_mode: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_index: 0,
            sidebar_filter_mode: false,
            sidebar_filter_query: String::new(),
            filtered_entries: Vec::new(),
            sidebar_filter_selected: 0,
            grep_mode: false,
            grep_query: String::new(),
            grep_results: Vec::new(),
            grep_selected: 0,
            grep_scroll: 0,
            grep_scroll_x: 0,
            grep_tx,
            grep_rx,
            grep_cancel: Arc::new(AtomicBool::new(false)),
            grep_searching: false,
            sidebar_filter_btn: None,
            sidebar_grep_btn: None,
            selection: None,
            is_selecting: false,
            copy_button_area: None,
            last_state_write: Instant::now(),
            shared_state: Arc::new(Mutex::new(EditorSharedState::default())),
            mcp_port: None,
        };

        // IDE MCPサーバー起動 (WebSocket + ~/.claude/ide/ ロックファイル)
        let workspace_str = app.root_dir.to_string_lossy().to_string();
        app.mcp_port = start_ide_mcp_server(app.shared_state.clone(), &workspace_str);

        // 初期ファイルがあれば開く
        if let Some(file_path) = initial_file {
            app.open_file(&file_path);
        }

        app
    }

    /// 未保存のタブがあるかチェック
    fn has_unsaved_tabs(&self) -> bool {
        // 現在のファイルが未保存
        if self.is_unsaved() {
            return true;
        }
        // 他のタブに未保存がある
        !self.unsaved_files.is_empty()
    }

    fn read_dir(path: &PathBuf) -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = fs::read_dir(path)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        entries.sort_by(|a, b| {
            let a_hidden = a.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false);
            let b_hidden = b.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false);
            a_hidden.cmp(&b_hidden)
                .then_with(|| b.is_dir().cmp(&a.is_dir()))
                .then_with(|| a.cmp(b))
        });
        entries
    }

    /// ファイルの更新日時を取得
    fn get_file_modified_time(path: &PathBuf) -> Option<SystemTime> {
        fs::metadata(path).ok().and_then(|m| m.modified().ok())
    }

    fn open_file(&mut self, path: &PathBuf) {
        if path.is_file() {
            // 現在のファイルの状態を保存
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
                            modified_time: self.file_modified_time,
                            externally_modified: false,
                        });
                    } else {
                        // 保存済みならメモリから削除
                        self.unsaved_files.remove(current_path);
                    }
                }
            }

            self.file_path = Some(path.clone());
            self.needs_clear = true;

            // 現在のディスク上のファイルの更新日時を取得
            let current_disk_modified = Self::get_file_modified_time(path);

            if is_image_file(path) {
                // 画像ファイルの場合 - 非同期でデコード
                let _ = self.decode_tx.send((path.clone(), self.picker.clone(), self.image_tx.clone()));
                self.image_state = None;
                self.is_image_mode = true;
                self.image_loading = true;
                // テキストバッファはクリア
                self.buffer = Rope::new();
                self.saved_content.clear();
                self.file_modified_time = current_disk_modified;
                self.current_language = None;
                self.cursor_line = 0;
                self.cursor_col = 0;
                self.scroll_offset = 0;
                self.horizontal_scroll = 0;
            } else if let Some(mut unsaved) = self.unsaved_files.remove(path) {
                // 未保存の状態があれば復元
                // 外部で変更されたか確認
                let was_externally_modified = match (unsaved.modified_time, current_disk_modified) {
                    (Some(saved_time), Some(disk_time)) => disk_time > saved_time,
                    _ => false,
                };

                if was_externally_modified {
                    // 外部変更があった場合、フラグを立てて新しい内容をsaved_contentに
                    let new_content = fs::read_to_string(path).unwrap_or_else(|_| String::new());
                    unsaved.saved_content = new_content;
                    unsaved.externally_modified = true;
                }

                self.buffer = unsaved.buffer;
                self.saved_content = unsaved.saved_content;
                self.cursor_line = unsaved.cursor_line;
                self.cursor_col = unsaved.cursor_col;
                self.scroll_offset = unsaved.scroll_offset;
                self.horizontal_scroll = unsaved.horizontal_scroll;
                self.file_modified_time = current_disk_modified;
                self.current_language = self.syntax.detect_language(path);
                self.image_state = None;
                self.is_image_mode = false;
                self.image_loading = false;
            } else {
                // ディスクから読み込み
                let content = fs::read_to_string(path).unwrap_or_else(|_| String::new());
                self.buffer = Rope::from_str(&content);
                self.saved_content = content;
                self.file_modified_time = current_disk_modified;
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
            // 保存後の更新日時を記録
            self.file_modified_time = Self::get_file_modified_time(path);
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

    /// タブを閉じる（未保存なら確認ダイアログを表示）
    fn close_current_tab(&mut self) {
        if self.is_unsaved() {
            self.confirm_dialog = Some(ConfirmAction::CloseTab);
        } else {
            self.force_close_current_tab();
        }
    }

    /// タブを強制的に閉じる（確認なし）
    fn force_close_current_tab(&mut self) {
        if let Some(current) = &self.file_path.clone() {
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
                    self.source_cache.clear();
                    self.highlight_cache = None;
                    self.line_offsets.clear();
                    self.max_line_width = 0;
                    self.buffer_dirty = true;
                    self.cursor_line = 0;
                    self.cursor_col = 0;
                    self.scroll_offset = 0;
                    self.horizontal_scroll = 0;
                    self.needs_clear = true;
                }
            }
        }
    }

    /// 検索を実行してマッチ位置を更新
    fn search(&mut self) {
        self.search_matches.clear();
        self.search_index = 0;

        if self.search_query.is_empty() {
            return;
        }

        let query_chars: Vec<char> = self.search_query.chars().collect();
        let query_len = query_chars.len();

        for (line_idx, line) in self.buffer.lines().enumerate() {
            let line_chars: Vec<char> = line.chars().collect();
            if line_chars.len() < query_len {
                continue;
            }

            // 文字単位で検索
            for col in 0..=line_chars.len().saturating_sub(query_len) {
                let mut matched = true;
                for (i, &qc) in query_chars.iter().enumerate() {
                    if line_chars.get(col + i) != Some(&qc) {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    self.search_matches.push((line_idx, col));
                }
            }
        }

        // 現在のカーソル位置以降の最初のマッチを選択
        for (i, &(line, col)) in self.search_matches.iter().enumerate() {
            if line > self.cursor_line || (line == self.cursor_line && col >= self.cursor_col) {
                self.search_index = i;
                break;
            }
        }
    }

    /// 次のマッチに移動
    fn next_match(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        self.search_index = (self.search_index + 1) % self.search_matches.len();
        self.jump_to_match();
    }

    /// 前のマッチに移動
    fn prev_match(&mut self) {
        if self.search_matches.is_empty() {
            return;
        }
        if self.search_index == 0 {
            self.search_index = self.search_matches.len() - 1;
        } else {
            self.search_index -= 1;
        }
        self.jump_to_match();
    }

    /// 現在のマッチ位置にジャンプ
    fn jump_to_match(&mut self) {
        if let Some(&(line, col)) = self.search_matches.get(self.search_index) {
            self.cursor_line = line;
            self.cursor_col = col;
            self.follow_cursor = true;
        }
    }

    fn update_sidebar_filter(&mut self) {
        if self.sidebar_filter_query.is_empty() {
            self.filtered_entries = self.entries.clone();
        } else {
            let query = self.sidebar_filter_query.to_lowercase();
            let mut results = Vec::new();
            Self::collect_files_recursive(&self.root_dir, &query, &mut results, 500);
            self.filtered_entries = results;
        }
        self.sidebar_scroll = 0;
        self.sidebar_filter_selected = 0;
    }

    fn collect_files_recursive(dir: &PathBuf, query: &str, results: &mut Vec<PathBuf>, max: usize) {
        if results.len() >= max { return; }
        let Ok(rd) = fs::read_dir(dir) else { return; };
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for entry in entries {
            if results.len() >= max { return; }
            let name = entry.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if name == ".git" || name == "node_modules" || name == "target" {
                continue;
            }
            if name.to_lowercase().contains(query) {
                results.push(entry.clone());
            }
            if entry.is_dir() {
                Self::collect_files_recursive(&entry, query, results, max);
            }
        }
    }

    fn grep_search(&mut self) {
        // 前回の検索をキャンセル
        self.grep_cancel.store(true, Ordering::Relaxed);
        self.grep_results.clear();
        self.grep_selected = 0;
        self.grep_scroll = 0;
        if self.grep_query.is_empty() {
            self.grep_searching = false;
            return;
        }
        let query = self.grep_query.to_lowercase();
        let root = self.root_dir.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.grep_cancel = cancel.clone();
        self.grep_searching = true;
        let _ = self.grep_tx.send((query, root, cancel));
    }

    fn grep_recursive(dir: &PathBuf, query: &str, results: &mut Vec<GrepResult>, max: usize, cancel: &AtomicBool) {
        if results.len() >= max || cancel.load(Ordering::Relaxed) { return; }
        let Ok(rd) = fs::read_dir(dir) else { return; };
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for entry in entries {
            if results.len() >= max || cancel.load(Ordering::Relaxed) { return; }
            let name = entry.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            if entry.is_dir() {
                Self::grep_recursive(&entry, query, results, max, cancel);
            } else if entry.is_file() {
                if let Ok(meta) = fs::metadata(&entry) {
                    if meta.len() > 1_048_576 { continue; }
                }
                if is_image_file(&entry) { continue; }
                if let Ok(content) = fs::read_to_string(&entry) {
                    for (i, line) in content.lines().enumerate() {
                        if results.len() >= max { break; }
                        let lower = line.to_lowercase();
                        if let Some(pos) = lower.find(query) {
                            // byte位置からchar位置に変換
                            let match_col = line[..pos].chars().count();
                            results.push(GrepResult {
                                path: entry.clone(),
                                line_number: i + 1,
                                line_content: line.trim().to_string(),
                                match_col,
                            });
                        }
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

    /// 共有状態を更新（MCPサーバーが参照する）
    fn update_shared_state(&mut self) {
        if self.last_state_write.elapsed().as_millis() < 500 {
            return;
        }
        self.last_state_write = Instant::now();

        let visible_height = self.editor_area.height.saturating_sub(2) as usize;
        let visible_start = self.scroll_offset;
        let visible_end = (self.scroll_offset + visible_height).min(self.buffer.len_lines());

        let new_state = EditorSharedState {
            file_path: self.file_path.as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            cursor_line: self.cursor_line + 1,
            cursor_col: self.cursor_col + 1,
            selected_text: self.get_selected_text().unwrap_or_default(),
            visible_start: visible_start + 1,
            visible_end,
            total_lines: self.buffer.len_lines(),
            buffer_content: self.buffer.to_string(),
            open_tabs: self.tabs.iter().map(|p| p.to_string_lossy().to_string()).collect(),
        };

        if let Ok(mut st) = self.shared_state.lock() {
            *st = new_state;
        }
    }

    /// 終了時に .mcp.json を削除
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

    /// カーソル行が画面中央に来るようスクロール位置を設定
    fn center_cursor(&mut self) {
        let visible_height = self.editor_area.height.saturating_sub(2) as usize;
        if visible_height > 0 {
            self.scroll_offset = self.cursor_line.saturating_sub(visible_height / 2);
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
            let was_filter_mode = self.sidebar_filter_mode;

            // サイドバークリック時にフィルタモード解除
            self.sidebar_filter_mode = false;

            // フィルタモード中はrefreshしない（filtered_entriesを使うため）
            if !was_filter_mode {
                self.refresh_directory();
            }

            let visible_index = (y - self.sidebar_area.y - 1) as usize;
            let index = visible_index + self.sidebar_scroll;
            let show_parent = !was_filter_mode && self.current_dir != self.root_dir;

            if show_parent && index == 0 {
                if let Some(parent) = self.current_dir.parent() {
                    self.current_dir = parent.to_path_buf();
                    self.entries = Self::read_dir(&self.current_dir);
                    self.sidebar_scroll = 0;
                    self.sidebar_scroll_x = 0;
                }
            } else {
                let entry_index = if show_parent { index - 1 } else { index };
                let active_entries = if was_filter_mode {
                    &self.filtered_entries
                } else {
                    &self.entries
                };
                if entry_index < active_entries.len() {
                    let path = active_entries[entry_index].clone();
                    if path.is_dir() {
                        self.current_dir = path;
                        self.entries = Self::read_dir(&self.current_dir);
                        self.sidebar_scroll = 0;
                        self.sidebar_scroll_x = 0;
                    } else {
                        if was_filter_mode {
                            if let Some(parent) = path.parent() {
                                if parent != self.current_dir {
                                    self.current_dir = parent.to_path_buf();
                                    self.entries = Self::read_dir(&self.current_dir);
                                    self.sidebar_scroll = 0;
                                    self.sidebar_scroll_x = 0;
                                }
                            }
                        }
                        self.open_file(&path);
                    }
                }
            }
        }
    }

    /// 現在のディレクトリ内容を再読み込み
    fn refresh_directory(&mut self) {
        let new_entries = Self::read_dir(&self.current_dir);
        if new_entries != self.entries {
            self.entries = new_entries;
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

    fn handle_sidebar_horizontal_scroll(&mut self, x: u16, y: u16, delta: i16) {
        if x >= self.sidebar_area.x
            && x < self.sidebar_area.x + self.sidebar_area.width
            && y >= self.sidebar_area.y
            && y < self.sidebar_area.y + self.sidebar_area.height
        {
            // エントリの最大文字幅を計算
            let show_parent = self.current_dir != self.root_dir;
            let max_entry_width = self.entries.iter()
                .map(|e| {
                    let name = e.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let display = if e.is_dir() {
                        format!("▸ {}/", name)
                    } else {
                        name
                    };
                    display.chars().count()
                })
                .max()
                .unwrap_or(0)
                .max(if show_parent { 2 } else { 0 }); // ".." の幅も考慮

            let visible_width = self.sidebar_area.width.saturating_sub(2) as usize; // ボーダー分を引く
            let max_scroll = max_entry_width.saturating_sub(visible_width);

            if delta < 0 {
                // Scroll left
                self.sidebar_scroll_x = self.sidebar_scroll_x.saturating_sub((-delta) as usize);
            } else {
                // Scroll right
                self.sidebar_scroll_x = (self.sidebar_scroll_x + delta as usize).min(max_scroll);
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

    /// エディタ領域内の座標を行・列に変換
    fn screen_to_editor_pos(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let ln_width = self.line_number_width() as u16;
        if x >= self.editor_area.x + 1 + ln_width
            && x < self.editor_area.x + self.editor_area.width - 1
            && y >= self.editor_area.y + 1
            && y < self.editor_area.y + self.editor_area.height - 1
        {
            let line = (y - self.editor_area.y - 1) as usize + self.scroll_offset;
            if line < self.buffer.len_lines() {
                let clicked_display_col = (x - self.editor_area.x - 1 - ln_width) as usize + self.horizontal_scroll;
                let col = self.display_col_to_char_col(line, clicked_display_col);
                return Some((line, col));
            }
        }
        None
    }

    /// 選択開始
    fn start_selection(&mut self, line: usize, col: usize) {
        self.selection = Some(Selection::new(line, col));
        self.is_selecting = true;
    }

    /// 選択更新
    fn update_selection(&mut self, line: usize, col: usize) {
        if let Some(ref mut sel) = self.selection {
            sel.end = (line, col);
        }
    }

    /// 選択終了
    fn end_selection(&mut self) {
        self.is_selecting = false;
        // 選択範囲が空なら選択解除
        if let Some(sel) = self.selection {
            if sel.start == sel.end {
                self.selection = None;
                self.copy_button_area = None;
            } else {
                // コピーボタンの位置を計算（選択終端の右側）
                self.update_copy_button_position();
            }
        }
    }

    /// コピーボタンの位置を更新
    fn update_copy_button_position(&mut self) {
        if let Some(sel) = self.selection {
            let (_, (end_line, end_col)) = sel.normalized();
            let ln_width = self.line_number_width();

            // 画面上の位置を計算
            if end_line >= self.scroll_offset {
                let screen_line = end_line - self.scroll_offset;
                let screen_y = self.editor_area.y + 1 + screen_line as u16;

                // 列位置を計算（表示幅を考慮）
                let display_col = if end_col >= self.horizontal_scroll {
                    end_col - self.horizontal_scroll
                } else {
                    0
                };
                let screen_x = self.editor_area.x + 1 + ln_width as u16 + display_col as u16;

                // ボタンサイズ: [Copy]
                let button_width = 6u16;
                let button_height = 1u16;

                // 画面内に収まるように調整
                let x = screen_x.min(self.editor_area.x + self.editor_area.width - button_width - 1);
                let y = screen_y.min(self.editor_area.y + self.editor_area.height - button_height - 1);

                self.copy_button_area = Some(Rect::new(x, y, button_width, button_height));
            } else {
                self.copy_button_area = None;
            }
        } else {
            self.copy_button_area = None;
        }
    }

    /// 全選択
    fn select_all(&mut self) {
        let last_line = self.buffer.len_lines().saturating_sub(1);
        let last_col = if last_line < self.buffer.len_lines() {
            let line = self.buffer.line(last_line);
            line.chars().filter(|&c| c != '\n' && c != '\r').count()
        } else {
            0
        };
        self.selection = Some(Selection {
            start: (0, 0),
            end: (last_line, last_col),
        });
        self.is_selecting = false;
    }

    /// 選択解除
    fn clear_selection(&mut self) {
        // コピーボタンが表示されていた場合は画面クリアが必要
        if self.copy_button_area.is_some() {
            self.needs_clear = true;
        }
        self.selection = None;
        self.copy_button_area = None;
        self.is_selecting = false;
    }

    /// 選択範囲を削除（選択範囲がある場合はtrueを返す）
    fn delete_selection(&mut self) -> bool {
        let sel = match self.selection {
            Some(s) => s,
            None => return false,
        };

        let ((start_line, start_col), (end_line, end_col)) = sel.normalized();

        // 開始位置のchar_idx
        let start_idx = if start_line >= self.buffer.len_lines() {
            self.buffer.len_chars()
        } else {
            let line_start = self.buffer.line_to_char(start_line);
            let line_len = self.buffer.line(start_line).len_chars();
            line_start + start_col.min(line_len.saturating_sub(1).max(start_col.min(line_len)))
        };

        // 終了位置のchar_idx
        let end_idx = if end_line >= self.buffer.len_lines() {
            self.buffer.len_chars()
        } else {
            let line_start = self.buffer.line_to_char(end_line);
            let line_len = self.buffer.line(end_line).len_chars();
            line_start + end_col.min(line_len)
        };

        if start_idx < end_idx && end_idx <= self.buffer.len_chars() {
            self.add_to_tabs();
            self.buffer.remove(start_idx..end_idx);
            self.buffer_dirty = true;

            // カーソルを開始位置に移動
            self.cursor_line = start_line;
            self.cursor_col = start_col;
            self.follow_cursor = true;
        }

        self.clear_selection();
        true
    }

    /// 選択範囲のテキストを取得
    fn get_selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let ((start_line, start_col), (end_line, end_col)) = sel.normalized();

        let mut result = String::new();
        for line_idx in start_line..=end_line {
            if line_idx >= self.buffer.len_lines() {
                break;
            }
            let line = self.buffer.line(line_idx);
            // Ropeyのline()は改行を含むので除去
            let line_str: String = line.chars()
                .filter(|&c| c != '\n' && c != '\r')
                .collect();
            let line_len = line_str.chars().count();

            let start = if line_idx == start_line { start_col } else { 0 };
            let end = if line_idx == end_line { end_col.min(line_len) } else { line_len };

            if start <= line_len {
                let chars: Vec<char> = line_str.chars().collect();
                let actual_end = end.min(chars.len());
                if start < actual_end {
                    let slice: String = chars[start..actual_end].iter().collect();
                    result.push_str(&slice);
                }
            }

            // 最終行以外は改行を追加
            if line_idx < end_line {
                result.push('\n');
            }
        }

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    /// OSC 52でクリップボードにコピー
    fn copy_to_clipboard_osc52(&self, text: &str) {
        use std::io::Write;
        let encoded = base64_encode(text.as_bytes());
        // OSC 52: システムクリップボードにコピー
        // \x1b]52;c;<base64>\x07
        let osc52 = format!("\x1b]52;c;{}\x07", encoded);
        let _ = std::io::stdout().write_all(osc52.as_bytes());
        let _ = std::io::stdout().flush();
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

        // 行オフセットキャッシュを構築し、最大行幅を計算（表示幅ベース）
        self.line_offsets.clear();
        self.line_offsets.push(0);
        self.max_line_width = 0;
        let mut current_line_width = 0usize;
        let mut byte_pos = 0usize;
        for ch in self.source_cache.chars() {
            if ch == '\n' {
                self.max_line_width = self.max_line_width.max(current_line_width);
                byte_pos += ch.len_utf8();
                self.line_offsets.push(byte_pos);
                current_line_width = 0;
            } else if ch == '\t' {
                // タブは4スペース相当として計算
                current_line_width += 4;
                byte_pos += 1;
            } else {
                // 表示幅を使用（全角=2, 半角=1）
                current_line_width += ch.width().unwrap_or(1);
                byte_pos += ch.len_utf8();
            }
        }
        // 最終行（改行で終わらない場合）
        self.max_line_width = self.max_line_width.max(current_line_width);

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
                        spans.extend(self.build_spans_from_colors(line_text, line_start, colors, content_width, line_idx));
                        lines.push(Line::from(spans));
                    } else {
                        let mut spans = vec![ln_span];
                        spans.extend(self.build_spans_simple(line_text, content_width, line_idx));
                        lines.push(Line::from(spans));
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

    /// 指定位置が検索マッチ内かどうかチェック
    fn is_in_search_match(&self, line_idx: usize, col: usize) -> bool {
        if !self.search_mode || self.search_query.is_empty() {
            return false;
        }
        let query_len = self.search_query.chars().count();
        for &(match_line, match_col) in &self.search_matches {
            if match_line == line_idx && col >= match_col && col < match_col + query_len {
                return true;
            }
        }
        false
    }

    /// 現在のマッチ位置かどうかチェック
    fn is_current_match(&self, line_idx: usize, col: usize) -> bool {
        if !self.search_mode || self.search_query.is_empty() {
            return false;
        }
        if let Some(&(match_line, match_col)) = self.search_matches.get(self.search_index) {
            let query_len = self.search_query.chars().count();
            return match_line == line_idx && col >= match_col && col < match_col + query_len;
        }
        false
    }

    /// 指定位置が選択範囲内かどうかチェック
    fn is_in_selection(&self, line_idx: usize, col: usize) -> bool {
        if let Some(ref sel) = self.selection {
            sel.contains(line_idx, col)
        } else {
            false
        }
    }

    fn build_spans_from_colors(&self, line_text: &str, line_start: usize, colors: &[Color], visible_width: usize, line_idx: usize) -> Vec<Span<'static>> {
        if line_text.is_empty() {
            return vec![];
        }

        let mut result = Vec::new();
        let mut current_style: Option<Style> = None;
        let mut current_text = String::new();
        let mut byte_offset = 0;
        let mut char_index = 0;
        let mut visible_chars = 0;

        for ch in line_text.chars() {
            let byte_pos = line_start + byte_offset;
            let fg_color = colors.get(byte_pos).copied().unwrap_or(Color::White);

            // タブは4スペースに展開、その他は表示幅を取得
            let (display_ch, char_width) = if ch == '\t' {
                (' ', 4usize)
            } else {
                (ch, ch.width().unwrap_or(1))
            };

            // 横スクロール範囲内の文字のみ処理
            if char_index >= self.horizontal_scroll && visible_chars < visible_width {
                // 表示幅が残り幅を超える場合は終了
                if visible_chars + char_width > visible_width {
                    break;
                }

                // ハイライト優先度: 検索マッチ > 選択範囲 > 通常
                let style = if self.is_current_match(line_idx, char_index) {
                    Style::default().fg(Color::Black).bg(Color::Yellow)
                } else if self.is_in_search_match(line_idx, char_index) {
                    Style::default().fg(fg_color).bg(Color::DarkGray)
                } else if self.is_in_selection(line_idx, char_index) {
                    Style::default().fg(Color::White).bg(Color::Blue)
                } else {
                    Style::default().fg(fg_color)
                };

                if current_style.is_none() {
                    current_style = Some(style);
                }

                if Some(style) != current_style {
                    if !current_text.is_empty() {
                        result.push(Span::styled(current_text.clone(), current_style.unwrap()));
                        current_text.clear();
                    }
                    current_style = Some(style);
                }
                // タブは複数スペースとして追加
                if ch == '\t' {
                    for _ in 0..char_width {
                        current_text.push(' ');
                    }
                } else {
                    current_text.push(display_ch);
                }
                visible_chars += char_width;
            }

            byte_offset += ch.len_utf8();
            char_index += 1;
        }

        if !current_text.is_empty() {
            if let Some(style) = current_style {
                result.push(Span::styled(current_text, style));
            }
        }

        result
    }

    fn build_spans_simple(&self, line_text: &str, visible_width: usize, line_idx: usize) -> Vec<Span<'static>> {
        let mut result = Vec::new();
        let mut current_style: Option<Style> = None;
        let mut current_text = String::new();
        let mut char_index = 0;
        let mut visible_chars = 0;

        for ch in line_text.chars() {
            // タブは4スペースに展開、その他は表示幅を取得
            let char_width = if ch == '\t' { 4usize } else { ch.width().unwrap_or(1) };

            if char_index >= self.horizontal_scroll && visible_chars < visible_width {
                // 表示幅が残り幅を超える場合は終了
                if visible_chars + char_width > visible_width {
                    break;
                }

                // ハイライト優先度: 検索マッチ > 選択範囲 > 通常
                let style = if self.is_current_match(line_idx, char_index) {
                    Style::default().fg(Color::Black).bg(Color::Yellow)
                } else if self.is_in_search_match(line_idx, char_index) {
                    Style::default().bg(Color::DarkGray)
                } else if self.is_in_selection(line_idx, char_index) {
                    Style::default().fg(Color::White).bg(Color::Blue)
                } else {
                    Style::default()
                };

                if current_style.is_none() {
                    current_style = Some(style);
                }

                if Some(style) != current_style {
                    if !current_text.is_empty() {
                        result.push(Span::styled(current_text.clone(), current_style.unwrap()));
                        current_text.clear();
                    }
                    current_style = Some(style);
                }
                // タブは複数スペースとして追加
                if ch == '\t' {
                    for _ in 0..char_width {
                        current_text.push(' ');
                    }
                } else {
                    current_text.push(ch);
                }
                visible_chars += char_width;
            }
            char_index += 1;
        }

        if !current_text.is_empty() {
            if let Some(style) = current_style {
                result.push(Span::styled(current_text, style));
            } else {
                result.push(Span::raw(current_text));
            }
        }

        result
    }
}

fn main() -> io::Result<()> {
    // コマンドライン引数を取得
    let args: Vec<String> = env::args().collect();
    let initial_path = args.get(1).map(PathBuf::from);

    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        reset_terminal();
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(initial_path);

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

        // Grep検索完了イベントを受け取る
        if let Ok(results) = app.grep_rx.try_recv() {
            app.grep_results = results;
            app.grep_selected = 0;
            app.grep_scroll = 0;
            app.grep_searching = false;
        }

        app.update_scroll();
        app.update_shared_state();

        // 画面クリアが必要な場合
        if app.needs_clear {
            let _ = terminal.clear();
            app.needs_clear = false;
        }

        // 描画（エラー時はスキップ）
        if terminal.draw(|frame| {
            // ターミナルが小さすぎる場合はスキップ
            let area = frame.area();
            if area.width < 10 || area.height < 5 {
                let msg = Paragraph::new("Terminal too small");
                frame.render_widget(msg, area);
                return;
            }

            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(20),
                    Constraint::Percentage(80),
                ])
                .split(area);

            // チャンクが不足している場合はスキップ
            if chunks.len() < 2 {
                return;
            }

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
                if editor_chunks.len() < 2 {
                    (None, chunks[1])
                } else {
                    (Some(editor_chunks[0]), editor_chunks[1])
                }
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
            let display_entries = if app.sidebar_filter_mode {
                &app.filtered_entries
            } else {
                &app.entries
            };
            let entry_lines: Vec<(String, Style)> = display_entries.iter().map(|path| {
                let dim_style = Style::default().fg(Color::Rgb(110, 110, 110));
                if app.sidebar_filter_mode && !app.sidebar_filter_query.is_empty() {
                    // フィルタモード: 相対パス表示
                    let rel = path.strip_prefix(&app.root_dir)
                        .unwrap_or(path).to_string_lossy().to_string();
                    let is_hidden = rel.starts_with('.');
                    let display = if path.is_dir() {
                        format!("▸ {}/", rel)
                    } else {
                        rel
                    };
                    let style = if is_hidden { dim_style } else { Style::default() };
                    (display, style)
                } else {
                    let name = path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let is_hidden = name.starts_with('.');
                    if path.is_dir() {
                        let display = format!("▸ {}/", name);
                        let style = if is_hidden { dim_style } else { Style::default() };
                        (display, style)
                    } else {
                        let style = if is_hidden { dim_style } else { Style::default() };
                        (name, style)
                    }
                }
            }).collect();

            let visible_height = chunks[0].height.saturating_sub(2) as usize;
            let show_parent = app.current_dir != app.root_dir;
            let total_items = entry_lines.len() + if show_parent { 1 } else { 0 };

            // 横スクロールを適用するヘルパー
            let apply_h_scroll = |s: &str, scroll_x: usize| -> String {
                let chars: Vec<char> = s.chars().collect();
                if scroll_x >= chars.len() {
                    String::new()
                } else {
                    chars[scroll_x..].iter().collect()
                }
            };

            // フィルタモード中の選択スクロール追従
            if app.sidebar_filter_mode && !app.filtered_entries.is_empty() {
                if app.sidebar_filter_selected < app.sidebar_scroll {
                    app.sidebar_scroll = app.sidebar_filter_selected;
                } else if app.sidebar_filter_selected >= app.sidebar_scroll + visible_height {
                    app.sidebar_scroll = app.sidebar_filter_selected.saturating_sub(visible_height - 1);
                }
            }

            let items: Vec<ListItem> = (0..visible_height)
                .filter_map(|i| {
                    let idx = app.sidebar_scroll + i;
                    if show_parent {
                        if idx == 0 {
                            Some(ListItem::new(Line::from(apply_h_scroll("..", app.sidebar_scroll_x))))
                        } else if idx - 1 < entry_lines.len() {
                            let entry_idx = idx - 1;
                            let (ref text, style) = entry_lines[entry_idx];
                            let style = if app.sidebar_filter_mode && entry_idx == app.sidebar_filter_selected {
                                Style::default().bg(Color::Blue).fg(Color::White)
                            } else {
                                style
                            };
                            Some(ListItem::new(Line::from(Span::styled(apply_h_scroll(text, app.sidebar_scroll_x), style))))
                        } else {
                            None
                        }
                    } else if idx < entry_lines.len() {
                        let (ref text, style) = entry_lines[idx];
                        let style = if app.sidebar_filter_mode && idx == app.sidebar_filter_selected {
                            Style::default().bg(Color::Blue).fg(Color::White)
                        } else {
                            style
                        };
                        Some(ListItem::new(Line::from(Span::styled(apply_h_scroll(text, app.sidebar_scroll_x), style))))
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

            // サイドバータイトルバーにボタン描画
            let btn_y = chunks[0].y;
            let btn_filter_text = " F ";
            let btn_grep_text = " G ";
            let btn_grep_x = chunks[0].x + chunks[0].width.saturating_sub(1 + btn_grep_text.len() as u16);
            let btn_filter_x = btn_grep_x.saturating_sub(btn_filter_text.len() as u16);

            let filter_btn_area = Rect::new(btn_filter_x, btn_y, btn_filter_text.len() as u16, 1);
            let grep_btn_area = Rect::new(btn_grep_x, btn_y, btn_grep_text.len() as u16, 1);
            app.sidebar_filter_btn = Some(filter_btn_area);
            app.sidebar_grep_btn = Some(grep_btn_area);

            let filter_btn_style = if app.sidebar_filter_mode {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default().bg(Color::DarkGray).fg(Color::Cyan)
            };
            let grep_btn_style = if app.grep_mode {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default().bg(Color::DarkGray).fg(Color::Yellow)
            };
            frame.render_widget(
                Paragraph::new(btn_filter_text).style(filter_btn_style),
                filter_btn_area,
            );
            frame.render_widget(
                Paragraph::new(btn_grep_text).style(grep_btn_style),
                grep_btn_area,
            );

            // サイドバーフィルタバー
            if app.sidebar_filter_mode {
                let filter_area = Rect::new(
                    chunks[0].x + 1,
                    chunks[0].y + chunks[0].height.saturating_sub(2),
                    chunks[0].width.saturating_sub(2),
                    1,
                );
                let filter_text = format!("Filter: {}", app.sidebar_filter_query);
                let filter_bar = Paragraph::new(filter_text)
                    .style(Style::default().bg(Color::Blue).fg(Color::White));
                frame.render_widget(filter_bar, filter_area);
            }

            // エディタ
            if app.is_image_mode {
                // 画像モード
                let block = Block::default()
                    .title(format!("{} [Ctrl-C: Quit]", app.file_name()))
                    .borders(Borders::ALL);
                let inner = block.inner(editor_area);
                frame.render_widget(block, editor_area);

                // 画像領域が十分な大きさの場合のみ描画
                if inner.width > 0 && inner.height > 0 {
                    if app.image_loading {
                        // ローディング中
                        let loading = Paragraph::new("Loading...");
                        frame.render_widget(loading, inner);
                    } else if let Some(ref mut image_state) = app.image_state {
                        let image_widget = ThreadImage::default();
                        frame.render_stateful_widget(image_widget, inner, image_state);
                    }
                }
            } else {
                // テキストモード
                let visible_height = editor_area.height.saturating_sub(2) as usize;
                let visible_width = editor_area.width.saturating_sub(2) as usize;
                let lines = app.get_highlighted_lines(visible_height, visible_width);

                let editor_block = Block::default()
                    .title(format!("{}{} [C-s:Save C-w:Close C-]:Tab C-c:Quit]", app.file_name(), if app.is_unsaved() { " *" } else { "" }))
                    .borders(Borders::ALL);
                let editor = Paragraph::new(lines).block(editor_block);
                frame.render_widget(editor, editor_area);

                // カーソル表示（行番号と横スクロール、全角文字幅を考慮）
                let ln_width = app.line_number_width() as u16;
                let display_col = app.cursor_display_col();
                let cursor_x = editor_area.x + 1 + ln_width + display_col.saturating_sub(app.horizontal_scroll) as u16;
                let cursor_y = editor_area.y + 1 + app.cursor_line.saturating_sub(app.scroll_offset) as u16;

                // カーソル位置を画面内に制限
                let max_x = editor_area.x + editor_area.width.saturating_sub(1);
                let max_y = editor_area.y + editor_area.height.saturating_sub(1);
                let cursor_x = cursor_x.min(max_x);
                let cursor_y = cursor_y.min(max_y);

                // 検索バー
                if app.search_mode {
                    if editor_area.height >= 2 {
                        let search_area = Rect::new(
                            editor_area.x,
                            editor_area.y + editor_area.height.saturating_sub(1),
                            editor_area.width,
                            1,
                        );
                        let match_info = if app.search_matches.is_empty() {
                            if app.search_query.is_empty() {
                                String::new()
                            } else {
                                " (no match)".to_string()
                            }
                        } else {
                            format!(" ({}/{})", app.search_index + 1, app.search_matches.len())
                        };
                        let search_text = format!("Search: {}{}", app.search_query, match_info);
                        let search_bar = Paragraph::new(search_text)
                            .style(Style::default().bg(Color::DarkGray).fg(Color::White));
                        frame.render_widget(search_bar, search_area);
                        // 検索バーにカーソルを表示（画面内に制限）
                        let search_cursor_x = (editor_area.x + 8 + app.search_query.len() as u16).min(max_x);
                        let search_cursor_y = editor_area.y + editor_area.height.saturating_sub(1);
                        frame.set_cursor_position((search_cursor_x, search_cursor_y));
                    }
                } else {
                    frame.set_cursor_position((cursor_x, cursor_y));
                }

                // コピーボタン表示
                if let Some(btn_area) = app.copy_button_area {
                    if btn_area.x >= editor_area.x && btn_area.y >= editor_area.y
                        && btn_area.x + btn_area.width <= editor_area.x + editor_area.width
                        && btn_area.y + btn_area.height <= editor_area.y + editor_area.height
                    {
                        frame.render_widget(Clear, btn_area);
                        let copy_btn = Paragraph::new(" Copy ")
                            .style(Style::default().bg(Color::Green).fg(Color::Black));
                        frame.render_widget(copy_btn, btn_area);
                    }
                }
            }

            // 確認ダイアログ
            if let Some(action) = app.confirm_dialog {
                let dialog_width = 40u16;
                let dialog_height = 5u16;
                let area = frame.area();
                let dialog_area = Rect::new(
                    area.x + (area.width.saturating_sub(dialog_width)) / 2,
                    area.y + (area.height.saturating_sub(dialog_height)) / 2,
                    dialog_width.min(area.width),
                    dialog_height.min(area.height),
                );
                let message = match action {
                    ConfirmAction::Quit => "  Quit? Unsaved changes will be lost.",
                    ConfirmAction::CloseTab => "  Close tab? Changes will be lost.",
                };
                let dialog = Paragraph::new(vec![
                    Line::from(""),
                    Line::from(message),
                    Line::from("  (Y/N)"),
                ])
                .block(Block::default().title(" Confirm ").borders(Borders::ALL))
                .style(Style::default().bg(Color::DarkGray));
                frame.render_widget(Clear, dialog_area);
                frame.render_widget(dialog, dialog_area);
            }

            // Grep検索オーバーレイ
            if app.grep_mode {
                let area = frame.area();
                let overlay_width = (area.width * 80 / 100).max(40).min(area.width);
                let overlay_height = (area.height * 70 / 100).max(10).min(area.height);
                let overlay_area = Rect::new(
                    area.x + (area.width.saturating_sub(overlay_width)) / 2,
                    area.y + (area.height.saturating_sub(overlay_height)) / 2,
                    overlay_width,
                    overlay_height,
                );
                frame.render_widget(Clear, overlay_area);

                let block = Block::default().title(" Find in Path ").borders(Borders::ALL)
                    .style(Style::default().bg(Color::DarkGray));
                let inner_area = block.inner(overlay_area);
                frame.render_widget(block, overlay_area);

                // 検索バー (上部1行)
                let search_line = Rect::new(inner_area.x, inner_area.y, inner_area.width, 1);
                let match_info = if app.grep_searching {
                    " (searching...)".to_string()
                } else if app.grep_results.is_empty() {
                    if app.grep_query.is_empty() { String::new() }
                    else { " (no results)".to_string() }
                } else {
                    format!(" ({}/{})", app.grep_selected + 1, app.grep_results.len())
                };
                let search_text = format!("Search: {}{}", app.grep_query, match_info);
                frame.render_widget(
                    Paragraph::new(search_text).style(Style::default().fg(Color::White)),
                    search_line,
                );

                // 結果リスト
                let results_area = Rect::new(
                    inner_area.x, inner_area.y + 1,
                    inner_area.width, inner_area.height.saturating_sub(1),
                );
                let visible = results_area.height as usize;
                // スクロール調整
                if app.grep_selected < app.grep_scroll {
                    app.grep_scroll = app.grep_selected;
                } else if app.grep_selected >= app.grep_scroll + visible {
                    app.grep_scroll = app.grep_selected.saturating_sub(visible - 1);
                }
                let items: Vec<ListItem> = app.grep_results.iter().enumerate()
                    .skip(app.grep_scroll)
                    .take(visible)
                    .map(|(i, r)| {
                        let rel = r.path.strip_prefix(&app.root_dir)
                            .unwrap_or(&r.path).to_string_lossy();
                        let full_text = format!("{}:{}: {}", rel, r.line_number, r.line_content);
                        let chars: Vec<char> = full_text.chars().collect();
                        let text = if app.grep_scroll_x >= chars.len() {
                            String::new()
                        } else {
                            chars[app.grep_scroll_x..].iter().collect()
                        };
                        let style = if i == app.grep_selected {
                            Style::default().bg(Color::Blue).fg(Color::White)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        ListItem::new(Line::from(Span::styled(text, style)))
                    })
                    .collect();
                frame.render_widget(List::new(items), results_area);

                // 検索バーにカーソル表示
                let cursor_x = (search_line.x + 8 + app.grep_query.len() as u16)
                    .min(search_line.x + search_line.width.saturating_sub(1));
                frame.set_cursor_position((cursor_x, search_line.y));
            }

            // サイドバーフィルタのカーソル表示
            if app.sidebar_filter_mode {
                let filter_cursor_x = (chunks[0].x + 1 + 8 + app.sidebar_filter_query.len() as u16)
                    .min(chunks[0].x + chunks[0].width.saturating_sub(2));
                let filter_cursor_y = chunks[0].y + chunks[0].height.saturating_sub(2);
                frame.set_cursor_position((filter_cursor_x, filter_cursor_y));
            }
        }).is_err() {
            // 描画エラー時は画面クリアを試みて続行
            let _ = terminal.clear();
            continue;
        }

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
                    // 確認ダイアログ中の場合
                    if let Some(action) = app.confirm_dialog {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                app.confirm_dialog = None;
                                match action {
                                    ConfirmAction::Quit => true,
                                    ConfirmAction::CloseTab => {
                                        app.force_close_current_tab();
                                        false
                                    }
                                }
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                app.confirm_dialog = None;
                                false
                            }
                            _ => false,
                        }
                    // 検索モード中の場合
                    } else if app.search_mode {
                        // 検索モードでのCtrl+キー処理
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match key.code {
                                KeyCode::Char('h') => {
                                    // Ctrl+H: Backspace
                                    app.search_query.pop();
                                    app.search();
                                    app.jump_to_match();
                                    false
                                }
                                KeyCode::Char('u') => {
                                    // Ctrl+U: クリア
                                    app.search_query.clear();
                                    app.search();
                                    false
                                }
                                KeyCode::Char('n') | KeyCode::Char('g') => {
                                    // Ctrl+N/Ctrl+G: 次のマッチ
                                    app.next_match();
                                    false
                                }
                                KeyCode::Char('p') => {
                                    // Ctrl+P: 前のマッチ
                                    app.prev_match();
                                    false
                                }
                                KeyCode::Char('c') => {
                                    // Ctrl+C: 検索終了
                                    app.search_mode = false;
                                    app.search_matches.clear();
                                    false
                                }
                                _ => false,
                            }
                        } else {
                            match key.code {
                                KeyCode::Esc => {
                                    app.search_mode = false;
                                    app.search_matches.clear();
                                    false
                                }
                                KeyCode::Enter => {
                                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                                        app.prev_match();
                                    } else {
                                        app.next_match();
                                    }
                                    false
                                }
                                KeyCode::Backspace => {
                                    app.search_query.pop();
                                    app.search();
                                    app.jump_to_match();
                                    false
                                }
                                KeyCode::Char(c) => {
                                    app.search_query.push(c);
                                    app.search();
                                    app.jump_to_match();
                                    false
                                }
                                _ => false,
                            }
                        }
                    // サイドバーフィルタモード
                    } else if app.sidebar_filter_mode {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match key.code {
                                KeyCode::Char('c') => { app.sidebar_filter_mode = false; false }
                                KeyCode::Char('h') => {
                                    app.sidebar_filter_query.pop();
                                    app.update_sidebar_filter();
                                    false
                                }
                                KeyCode::Char('u') => {
                                    app.sidebar_filter_query.clear();
                                    app.update_sidebar_filter();
                                    false
                                }
                                KeyCode::Char('n') | KeyCode::Char('g') => {
                                    if !app.filtered_entries.is_empty() {
                                        app.sidebar_filter_selected = (app.sidebar_filter_selected + 1) % app.filtered_entries.len();
                                    }
                                    false
                                }
                                KeyCode::Char('p') => {
                                    if !app.filtered_entries.is_empty() {
                                        app.sidebar_filter_selected = if app.sidebar_filter_selected == 0 {
                                            app.filtered_entries.len() - 1
                                        } else {
                                            app.sidebar_filter_selected - 1
                                        };
                                    }
                                    false
                                }
                                _ => false,
                            }
                        } else {
                            match key.code {
                                KeyCode::Esc => { app.sidebar_filter_mode = false; false }
                                KeyCode::Backspace => {
                                    app.sidebar_filter_query.pop();
                                    app.update_sidebar_filter();
                                    false
                                }
                                KeyCode::Up => {
                                    if app.sidebar_filter_selected > 0 { app.sidebar_filter_selected -= 1; }
                                    false
                                }
                                KeyCode::Down => {
                                    if app.sidebar_filter_selected + 1 < app.filtered_entries.len() {
                                        app.sidebar_filter_selected += 1;
                                    }
                                    false
                                }
                                KeyCode::Enter => {
                                    if let Some(path) = app.filtered_entries.get(app.sidebar_filter_selected).cloned() {
                                        if path.is_dir() {
                                            app.current_dir = path;
                                            app.entries = App::read_dir(&app.current_dir);
                                        } else {
                                            if let Some(parent) = path.parent() {
                                                if parent != app.current_dir {
                                                    app.current_dir = parent.to_path_buf();
                                                    app.entries = App::read_dir(&app.current_dir);
                                                    app.sidebar_scroll = 0;
                                                    app.sidebar_scroll_x = 0;
                                                }
                                            }
                                            app.open_file(&path);
                                        }
                                    }
                                    app.sidebar_filter_mode = false;
                                    false
                                }
                                KeyCode::Char(c) => {
                                    app.sidebar_filter_query.push(c);
                                    app.update_sidebar_filter();
                                    false
                                }
                                _ => false,
                            }
                        }
                    // Grep検索モード
                    } else if app.grep_mode {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match key.code {
                                KeyCode::Char('c') => { app.grep_mode = false; false }
                                KeyCode::Char('n') | KeyCode::Char('g') => {
                                    if !app.grep_results.is_empty() {
                                        app.grep_selected = (app.grep_selected + 1) % app.grep_results.len();
                                    }
                                    false
                                }
                                KeyCode::Char('p') => {
                                    if !app.grep_results.is_empty() {
                                        app.grep_selected = if app.grep_selected == 0 {
                                            app.grep_results.len() - 1
                                        } else {
                                            app.grep_selected - 1
                                        };
                                    }
                                    false
                                }
                                KeyCode::Char('h') => {
                                    app.grep_query.pop();
                                    app.grep_search();
                                    false
                                }
                                KeyCode::Char('u') => {
                                    app.grep_query.clear();
                                    app.grep_search();
                                    false
                                }
                                _ => false,
                            }
                        } else {
                            match key.code {
                                KeyCode::Esc => { app.grep_mode = false; false }
                                KeyCode::Backspace => {
                                    app.grep_query.pop();
                                    app.grep_search();
                                    false
                                }
                                KeyCode::Up => {
                                    if app.grep_selected > 0 { app.grep_selected -= 1; }
                                    false
                                }
                                KeyCode::Down => {
                                    if app.grep_selected + 1 < app.grep_results.len() {
                                        app.grep_selected += 1;
                                    }
                                    false
                                }
                                KeyCode::Left => {
                                    app.grep_scroll_x = app.grep_scroll_x.saturating_sub(4);
                                    false
                                }
                                KeyCode::Right => {
                                    app.grep_scroll_x += 4;
                                    false
                                }
                                KeyCode::Enter => {
                                    if let Some(result) = app.grep_results.get(app.grep_selected).cloned() {
                                        app.open_file(&result.path);
                                        app.cursor_line = result.line_number.saturating_sub(1);
                                        app.cursor_col = result.match_col;
                                        app.center_cursor();
                                        app.follow_cursor = true;
                                        app.grep_mode = false;
                                    }
                                    false
                                }
                                KeyCode::Char(c) => {
                                    app.grep_query.push(c);
                                    app.grep_search();
                                    false
                                }
                                _ => false,
                            }
                        }
                    // Command-S (macOS) または Ctrl-S で保存
                    } else if (key.modifiers.contains(KeyModifiers::SUPER) || key.modifiers.contains(KeyModifiers::CONTROL))
                        && key.code == KeyCode::Char('s')
                    {
                        let _ = app.save_file();
                        false
                    // Command-C (macOS) でコピー
                    } else if key.modifiers.contains(KeyModifiers::SUPER) && key.code == KeyCode::Char('c') {
                        if let Some(text) = app.get_selected_text() {
                            app.copy_to_clipboard_osc52(&text);
                        }
                        false
                    } else if key.modifiers.contains(KeyModifiers::CONTROL) {
                        // Emacs keybindings (Ctrl+key)
                        match key.code {
                            KeyCode::Char('c') => {
                                // 選択範囲がある場合はコピー、ない場合は終了
                                if app.selection.is_some() {
                                    if let Some(text) = app.get_selected_text() {
                                        app.copy_to_clipboard_osc52(&text);
                                    }
                                    false
                                } else if app.has_unsaved_tabs() {
                                    app.confirm_dialog = Some(ConfirmAction::Quit);
                                    false
                                } else {
                                    true
                                }
                            }
                            KeyCode::Char('a') | KeyCode::Char('A') => {
                                if key.modifiers.contains(KeyModifiers::SHIFT) {
                                    // Ctrl+Shift+A: 全選択
                                    app.select_all();
                                } else {
                                    // Ctrl+A: 行頭に移動 (emacs beginning-of-line)
                                    app.clear_selection();
                                    app.move_to_line_start();
                                }
                                false
                            }
                            KeyCode::Char('e') => { app.move_to_line_end(); false }
                            KeyCode::Char('f') | KeyCode::Char('F') => {
                                if key.modifiers.contains(KeyModifiers::SHIFT) {
                                    // Ctrl+Shift+F: 検索モード開始
                                    app.search_mode = true;
                                    app.search_query.clear();
                                    app.search_matches.clear();
                                } else {
                                    // Ctrl+F: カーソルを右に移動 (emacs forward-char)
                                    app.clear_selection();
                                    app.move_right();
                                }
                                false
                            }
                            KeyCode::Char('b') => { app.clear_selection(); app.move_left(); false }
                            KeyCode::Char('p') => { app.clear_selection(); app.move_up(); false }
                            KeyCode::Char('n') => { app.clear_selection(); app.move_down(); false }
                            KeyCode::Char('d') => {
                                if !app.delete_selection() {
                                    app.delete_char_delete();
                                }
                                false
                            }
                            KeyCode::Char('h') => {
                                if !app.delete_selection() {
                                    app.delete_char_backspace();
                                }
                                false
                            }
                            KeyCode::Char('k') => { app.kill_line(); false }
                            KeyCode::Char('w') => { app.close_current_tab(); false }  // タブを閉じる
                            KeyCode::Char(']') => { app.next_tab(); false }  // 次のタブ
                            KeyCode::Char('[') => { app.prev_tab(); false }  // 前のタブ
                            KeyCode::Char('t') => {
                                // Ctrl+T: サイドバーファイル名フィルタ
                                app.grep_mode = false;
                                app.sidebar_filter_mode = true;
                                app.sidebar_filter_query.clear();
                                app.filtered_entries = app.entries.clone();
                                app.sidebar_filter_selected = 0;
                                false
                            }
                            KeyCode::Char('g') => {
                                // Ctrl+G: フォルダ内grep検索
                                app.sidebar_filter_mode = false;
                                app.grep_mode = true;
                                app.grep_query.clear();
                                app.grep_results.clear();
                                app.grep_selected = 0;
                                app.grep_scroll = 0;
                                app.grep_scroll_x = 0;
                                false
                            }
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
                            KeyCode::Esc => {
                                // 選択解除
                                app.clear_selection();
                            }
                            KeyCode::Up => { app.clear_selection(); app.move_up(); }
                            KeyCode::Down => { app.clear_selection(); app.move_down(); }
                            KeyCode::Left => { app.clear_selection(); app.move_left(); }
                            KeyCode::Right => { app.clear_selection(); app.move_right(); }
                            KeyCode::Backspace => {
                                if !app.delete_selection() {
                                    app.delete_char_backspace();
                                }
                            }
                            KeyCode::Delete => {
                                if !app.delete_selection() {
                                    app.delete_char_delete();
                                }
                            }
                            KeyCode::Enter => { app.delete_selection(); app.insert_char('\n'); }
                            KeyCode::Char(c) => { app.delete_selection(); app.insert_char(c); }
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
                    let in_grep_overlay = if app.grep_mode {
                        let sz = terminal.size().unwrap_or_default();
                        let area = Rect::new(0, 0, sz.width, sz.height);
                        let ow = (area.width * 80 / 100).max(40).min(area.width);
                        let oh = (area.height * 70 / 100).max(10).min(area.height);
                        let ox = area.x + (area.width.saturating_sub(ow)) / 2;
                        let oy = area.y + (area.height.saturating_sub(oh)) / 2;
                        x >= ox && x < ox + ow && y >= oy && y < oy + oh
                    } else {
                        false
                    };

                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            // コピーボタンのクリック判定
                            let clicked_copy_button = if let Some(btn_area) = app.copy_button_area {
                                x >= btn_area.x && x < btn_area.x + btn_area.width
                                    && y >= btn_area.y && y < btn_area.y + btn_area.height
                            } else {
                                false
                            };

                            // サイドバーボタンのクリック判定
                            let clicked_filter_btn = if let Some(btn) = app.sidebar_filter_btn {
                                x >= btn.x && x < btn.x + btn.width && y == btn.y
                            } else { false };
                            let clicked_grep_btn = if let Some(btn) = app.sidebar_grep_btn {
                                x >= btn.x && x < btn.x + btn.width && y == btn.y
                            } else { false };

                            // フィルタ/grepモード中、対象外エリアクリックで自動解除
                            if app.sidebar_filter_mode && !clicked_filter_btn {
                                // フィルタバー領域内かチェック
                                let in_filter_bar = {
                                    let fx = app.sidebar_area.x + 1;
                                    let fy = app.sidebar_area.y + app.sidebar_area.height.saturating_sub(2);
                                    let fw = app.sidebar_area.width.saturating_sub(2);
                                    x >= fx && x < fx + fw && y == fy
                                };
                                // サイドバー内のエントリクリックも許可
                                let in_sidebar = x >= app.sidebar_area.x
                                    && x < app.sidebar_area.x + app.sidebar_area.width
                                    && y > app.sidebar_area.y
                                    && y < app.sidebar_area.y + app.sidebar_area.height.saturating_sub(1);
                                if !in_filter_bar && !in_sidebar {
                                    app.sidebar_filter_mode = false;
                                }
                            }
                            if app.grep_mode && !clicked_grep_btn {
                                // grepオーバーレイ領域内かチェック
                                let sz = terminal.size().unwrap_or_default();
                                let area = Rect::new(0, 0, sz.width, sz.height);
                                let ow = (area.width * 80 / 100).max(40).min(area.width);
                                let oh = (area.height * 70 / 100).max(10).min(area.height);
                                let ox = area.x + (area.width.saturating_sub(ow)) / 2;
                                let oy = area.y + (area.height.saturating_sub(oh)) / 2;
                                let in_overlay = x >= ox && x < ox + ow && y >= oy && y < oy + oh;
                                if !in_overlay {
                                    app.grep_mode = false;
                                } else {
                                    // オーバーレイ内クリック: 結果リスト領域かチェック
                                    // inner_area: border(1) + search_bar(1) = oy+2 から結果リスト開始
                                    let results_start_y = oy + 2;
                                    let results_end_y = oy + oh - 1; // border分
                                    if y >= results_start_y && y < results_end_y {
                                        let clicked_idx = app.grep_scroll + (y - results_start_y) as usize;
                                        if clicked_idx < app.grep_results.len() {
                                            app.grep_selected = clicked_idx;
                                            // シングルクリックで遷移
                                            let result = app.grep_results[clicked_idx].clone();
                                            app.open_file(&result.path);
                                            app.cursor_line = result.line_number.saturating_sub(1);
                                            app.cursor_col = result.match_col;
                                            app.center_cursor();
                                            app.follow_cursor = true;
                                            app.grep_mode = false;
                                        }
                                    }
                                }
                            }

                            if clicked_filter_btn {
                                app.grep_mode = false;
                                app.sidebar_filter_mode = true;
                                app.sidebar_filter_query.clear();
                                app.filtered_entries = app.entries.clone();
                                app.sidebar_filter_selected = 0;
                            } else if clicked_grep_btn {
                                app.sidebar_filter_mode = false;
                                app.grep_mode = true;
                                app.grep_query.clear();
                                app.grep_results.clear();
                                app.grep_selected = 0;
                                app.grep_scroll = 0;
                                app.grep_scroll_x = 0;
                            } else if clicked_copy_button {
                                // コピーボタンクリック：OSC 52でコピーして選択解除
                                if let Some(text) = app.get_selected_text() {
                                    app.copy_to_clipboard_osc52(&text);
                                }
                                app.clear_selection();
                            } else if !in_grep_overlay {
                                app.handle_tab_click(x, y);
                                app.handle_sidebar_click(x, y);
                                // エディタ領域でのクリックは選択開始
                                if in_editor {
                                    // 既存の選択を解除
                                    app.clear_selection();
                                    // クリック位置にカーソル移動
                                    app.handle_editor_click(x, y);
                                    // 選択開始
                                    if let Some((line, col)) = app.screen_to_editor_pos(x, y) {
                                        app.start_selection(line, col);
                                    }
                                } else {
                                    app.clear_selection();
                                }
                            }
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            // エディタ領域でのドラッグは選択範囲を更新
                            if in_editor && app.is_selecting {
                                if let Some((line, col)) = app.screen_to_editor_pos(x, y) {
                                    app.update_selection(line, col);
                                    // カーソルも移動
                                    app.cursor_line = line;
                                    app.cursor_col = col;
                                }
                            }
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            // 選択終了
                            app.end_selection();
                        }
                        MouseEventKind::ScrollUp => {
                            if in_grep_overlay {
                                if app.grep_selected > 0 { app.grep_selected -= 1; }
                            } else if in_sidebar {
                                app.handle_sidebar_scroll(x, y, -1);
                            } else if in_editor {
                                app.handle_editor_scroll(-1);
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if in_grep_overlay {
                                if app.grep_selected + 1 < app.grep_results.len() {
                                    app.grep_selected += 1;
                                }
                            } else if in_sidebar {
                                app.handle_sidebar_scroll(x, y, 1);
                            } else if in_editor {
                                app.handle_editor_scroll(1);
                            }
                        }
                        MouseEventKind::ScrollLeft => {
                            if in_grep_overlay {
                                app.grep_scroll_x = app.grep_scroll_x.saturating_sub(4);
                            } else if in_sidebar {
                                app.handle_sidebar_horizontal_scroll(x, y, -2);
                            } else if in_editor {
                                app.handle_editor_horizontal_scroll(-2);
                            }
                        }
                        MouseEventKind::ScrollRight => {
                            if in_grep_overlay {
                                app.grep_scroll_x += 4;
                            } else if in_sidebar {
                                app.handle_sidebar_horizontal_scroll(x, y, 2);
                            } else if in_editor {
                                app.handle_editor_horizontal_scroll(2);
                            }
                        }
                        _ => {}
                    }
                    false
                }
                Event::Paste(text) => {
                    // ペーストされたテキストを挿入
                    app.clear_selection();
                    for c in text.chars() {
                        app.insert_char(c);
                    }
                    false
                }
                Event::Resize(_, _) => {
                    // ターミナルリサイズ時に画面をクリア
                    let _ = terminal.clear();

                    // 画像モードの場合は画像状態をリセット（再レンダリング用）
                    if app.is_image_mode {
                        app.image_state = None;
                        if let Some(path) = app.file_path.clone() {
                            let _ = app.decode_tx.send((path, app.picker.clone(), app.image_tx.clone()));
                            app.image_loading = true;
                        }
                    }

                    // カーソル行の調整
                    let total_lines = app.buffer.len_lines();
                    if app.cursor_line >= total_lines {
                        app.cursor_line = total_lines.saturating_sub(1);
                    }
                    // カーソル列の調整
                    app.clamp_cursor_col();
                    // 垂直スクロールの調整
                    if app.scroll_offset > total_lines.saturating_sub(1) {
                        app.scroll_offset = total_lines.saturating_sub(1);
                    }
                    // 水平スクロールの調整
                    if app.horizontal_scroll > app.max_line_width {
                        app.horizontal_scroll = 0;
                    }
                    // サイドバーの垂直スクロール調整
                    let show_parent = app.current_dir != app.root_dir;
                    let total_items = app.entries.len() + if show_parent { 1 } else { 0 };
                    if app.sidebar_scroll >= total_items {
                        app.sidebar_scroll = total_items.saturating_sub(1);
                    }
                    // サイドバーの水平スクロールをリセット
                    app.sidebar_scroll_x = 0;
                    false
                }
                _ => false,
            };

            if should_break {
                if let Some(port) = app.mcp_port {
                    cleanup_ide_lock(port);
                }
                disable_raw_mode()?;
                execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture, DisableBracketedPaste)?;
                return Ok(());
            }
        }
    }
}
