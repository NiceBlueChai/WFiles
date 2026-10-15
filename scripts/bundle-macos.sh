#!/usr/bin/env bash
#
# 把 `target/<profile>/wfiles` 打成一个能双击的 `WFiles.app`。
#
# # 为什么需要这一步
#
# 直接从终端跑那个裸二进制，macOS 不把它当普通 App：**它是一个
# "后台 App"**——窗口画得出来，但顶不到最前、切不过去，系统还会弹一句
# 「WFiles 正在后台运行」。原因不在代码里，在 bundle 上：AppKit 靠
# `Info.plist` 里的 `LSUIElement` / `CFBundlePackageType` 判定这个进程
# 是不是"有界面的 App"，一个孤零零的可执行文件没有那些声明。
#
# 顺带还有两个好处：
#   - 有固定的 bundle id 之后，TCC（自动化 Finder、完全磁盘访问权限）
#     记的授权对象是稳定的，不会每次重新问一遍；
#   - 能写进「应用程序」文件夹、能被 LaunchServices 正常启动。
#
# 用法：
#   scripts/bundle-macos.sh              # release 构建
#   scripts/bundle-macos.sh --debug      # debug 构建
#   scripts/bundle-macos.sh --out ~/Applications
#
set -euo pipefail

cd "$(dirname "$0")/.."

profile=release
out_dir="target"
while [ $# -gt 0 ]; do
    case "$1" in
        --debug) profile=debug ;;
        --release) profile=release ;;
        --out)
            shift
            out_dir="${1:?--out 后面要跟一个目录}"
            ;;
        *)
            echo "认不出的参数：$1" >&2
            echo "用法：$0 [--debug|--release] [--out <目录>]" >&2
            exit 2
            ;;
    esac
    shift
done

cargo_args=()
if [ "$profile" = release ]; then
    cargo_args=(--release)
fi
echo "构建（${profile}）……"
# **`${cargo_args[@]}` 要这么写**：macOS 自带的是 bash 3.2，空数组配上
# `set -u` 会报 "unbound variable"（4.x 才修好）。所以先判空再展开。
if [ ${#cargo_args[@]} -gt 0 ]; then
    cargo build "${cargo_args[@]}"
else
    cargo build
fi

binary="target/$profile/wfiles"
[ -x "$binary" ] || { echo "没找到可执行文件：$binary" >&2; exit 1; }

app="$out_dir/WFiles.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/WFiles"

cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <!-- 就是这一句让 macOS 认它是"有界面的 App"而不是后台进程。 -->
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleExecutable</key>
    <string>WFiles</string>
    <key>CFBundleName</key>
    <string>WFiles</string>
    <key>CFBundleDisplayName</key>
    <string>WFiles</string>
    <!-- 这个 id 是 TCC 记授权用的钥匙：一旦定下就别再改，
         改了等于把用户给过的「完全磁盘访问权限」「自动化 Finder」
         全部作废，得重新授一遍。 -->
    <key>CFBundleIdentifier</key>
    <string>com.wfiles.app</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <!-- Rust 2024 + stable 编出来的是 arm64 / x86_64 单架构，
         这一句只是告诉系统"照我这个来"，不参与架构选择。 -->
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.utilities</string>
</dict>
</plist>
PLIST

# 没有原生 xattr 的卷（外置 exFAT / NTFS，比如这个项目所在的 Ventoy）上，
# `cp` 会给每个文件配一个 `._同名` 的资源叉伴生文件，而 codesign 会把它
# 当成"没签名的子组件"直接拒签（`code object is not signed at all`）。
# 先清干净再签；在 APFS 上是空操作。
find "$app" -name '._*' -delete 2>/dev/null || true
xattr -cr "$app" 2>/dev/null || true

# 顺手做一次 ad-hoc 签名。没有它，每次重新构建都会被当成"另一个程序"，
# 系统授予的权限（自动化 Finder / 完全磁盘访问）会一次次重问。
# `-`（ad-hoc）不需要开发者证书，本机自用够了；要分发才需要真证书。
if command -v codesign >/dev/null 2>&1; then
    if sign_error="$(codesign --force --sign - "$app" 2>&1)"; then
        echo "已做 ad-hoc 签名"
    else
        echo "ad-hoc 签名失败（不影响本机运行）：$sign_error" >&2
    fi
fi

echo "打好了：$app"
echo "打开：open \"$app\""
