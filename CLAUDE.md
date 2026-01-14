# Rust製TUI Editor

## 要件
- 軽量
- TUIで起動
- shortcutなどなし (terminal上のkeybindでそのまま動く)
- Tree-sitter対応
- ディレクトリ表示対応
- マウス操作対応 (ディレクトリ操作にも対応)
- 画像表示対応 (PNG, JPEG, GIF, WebP)

## 使用ライブラリ

| 用途 | ライブラリ | 説明 |
|------|------------|------|
| TUI描画 | Ratatui | 事実上の標準。レイアウト（サイドバー分割）が簡単 |
| 端末操作 | Crossterm | キー入力、マウスイベント取得、画面クリアなどを担当 |
| テキスト管理 | Ropey | 文字列を「Rope構造」で管理。巨大ファイルも爆速 |
| ハイライト | tree-sitter | 構文解析用。Rustバインディングが優秀 |
| 画像表示 | ratatui-image | ターミナル上での画像表示 |
| 設定 | toml, serde | 設定ファイルの読み込み |
