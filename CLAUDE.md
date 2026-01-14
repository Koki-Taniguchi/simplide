# Rust製TUI Editor

## 要件
- 軽量
- TUIで起動
- shortcutなどなし (terminal上のkeybindでそのまま動く)
- Tree-sitter対応
- ディレクトリ表示対応
- マウス操作対応 (ディレクトリ操作にも対応)
- LSP対応

## 使用想定ライブラリ

TUI描画	Ratatui	事実上の標準。レイアウト（サイドバー分割）が簡単。
端末操作	Crossterm	キー入力、マウスイベント取得、画面クリアなどを担当。
テキスト管理	Ropey	文字列を「Rope構造」で管理。巨大ファイルも爆速で開けます。
ハイライト	tree-sitter	解析用。Rustバインディングが優秀。
非同期処理	Tokio	LSPサーバーとの通信（JSON-RPC）に必須。
LSP型定義	lsp-types	LSPの仕様書を読む時間を節約できます。

