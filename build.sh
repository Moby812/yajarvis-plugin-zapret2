#!/bin/sh
# build.sh — собирает Rust-плагин zapret и упаковывает в ZIP для YaJarvis.
#
# Выходные архивы (в корне репозитория):
#   jarvis-plugin-arm64.zip   — для Станций на arm64 (Мини 2/3, Миди, 2/3 и т.п.)
#   jarvis-plugin-armv7.zip   — для Станций на armv7 (часть Мини 1 и др.)
#
# Требуются:
#   - Rust (rustup) с target'ами aarch64-unknown-linux-musl и
#     armv7-unknown-linux-musleabihf (rustup target add ...);
#   - кросс-линкер musl (на macOS удобно через Zig: cargo-zigbuild).
#
# По умолчанию собирается только arm64. Передайте "all", чтобы собрать обе
# архитектуры:
#   ./build.sh all

set -eu

cd "$(dirname "$0")/plugin"

MODE="${1:-arm64}"

# Имя бинарника должно совпадать с полем "main" в plugin.json.
BIN="zapret-plugin"

build_one() {
    target="$1"
    arch="$2"
    zipname="$3"

    echo ">> Сборка $arch ($target) ..."
    if command -v cargo-zigbuild >/dev/null 2>&1; then
        cargo zigbuild --release --target "$target"
    else
        cargo build --release --target "$target"
    fi

    out_dir="dist/$arch"
    rm -rf "$out_dir"
    mkdir -p "$out_dir"
    cp "target/$target/release/$BIN" "$out_dir/$BIN"
    sed "s/\"arch\"[[:space:]]*:[[:space:]]*\"[^\"]*\"/\"arch\": \"$arch\"/" plugin.json > "$out_dir/plugin.json"

    echo ">> Упаковка $zipname ..."
    rm -f "../../$zipname"
    (cd "$out_dir" && zip -q -r "../../$zipname" "$BIN" plugin.json)
    echo ">> Готово: $zipname"
}

case "$MODE" in
    all)
        build_one "aarch64-unknown-linux-musl" "arm64" "jarvis-plugin-arm64.zip"
        build_one "armv7-unknown-linux-musleabihf" "armv7" "jarvis-plugin-armv7.zip"
        ;;
    arm64)
        build_one "aarch64-unknown-linux-musl" "arm64" "jarvis-plugin-arm64.zip"
        ;;
    armv7)
        build_one "armv7-unknown-linux-musleabihf" "armv7" "jarvis-plugin-armv7.zip"
        ;;
    *)
        echo "Неизвестный режим: $MODE (доступно: arm64, armv7, all)" >&2
        exit 1
        ;;
esac