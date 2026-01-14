.PHONY: build release clean

# デバッグビルド
build:
	cargo build

# リリースビルド
release:
	cargo build --release

# クリーン
clean:
	cargo clean
