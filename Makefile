.PHONY: build release clean install

# デバッグビルド
build:
	cargo build

# リリースビルド
release:
	cargo build --release

# インストール (side コマンドとして)
install: release
	rm -f ~/.cargo/bin/side
	cp target/release/simplide ~/.cargo/bin/side

# クリーン
clean:
	cargo clean
