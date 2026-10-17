#!/usr/bin/env bash
#
# 把 `assets/wfiles.svg` 渲染成一套 PNG 并打成 `WFiles.icns`。
#
# 用法：
#   scripts/make-icon.sh                            # 产物落 target/icon/WFiles.icns
#   scripts/make-icon.sh --out ~/桌面                # 换个地方
#   scripts/make-icon.sh --preview assets/预览.png   # 附带写一张 512 的 PNG（README 那张）
#   scripts/make-icon.sh --force                    # 无视"比 SVG 新就不用重做"的缓存
#
# # 为什么是浏览器在渲染
#
# macOS 自带的工具里**没有 SVG 光栅器**：`sips` 读不了 SVG；`qlmanage`
# 会把 SVG 当**文本源码**预览（白底、一行行字），拿它渲图是一次教训。
# 无头 Chrome/Chromium 是这台机器上现成的、正经的 SVG 引擎（Blink），
# 于是借它一用——不引新依赖，也不要求装别的东西。
#
# 找不到任何浏览器就直接报错退出：宁可没有图标，不要静默产出半套图。
#
# # 为什么要套一层 HTML
#
# 把 `.svg` 直接交给 Chrome（`--screenshot` 指到文件）时，它按 SVG 的
# **固有尺寸**（1024）渲染再裁剪窗口——256 的窗口里只剩左上角一块透明，
# 而不是"把图标画成 256"。套一层 `<img width=尺寸 height=尺寸>` 才是
# 按目标像素重新光栅化，每个尺寸都从矢量重新画一遍（16 px 也清晰）。
#
# # 尺寸的规矩
#
# 画布 1024、圆角方块 824 × 824、居中留 100 边距、圆角半径 185（≈ 22.4%）
# ——这是 Big Sur 起 macOS 图标的网格，别的应用也在这条线上，摆在一起才一样大。
# 这些都写在 SVG 里，这个脚本只负责渲染和打包。
#
# # 改 SVG 的人留意
#
# **XML 注释里不能出现两个连续的连字符。** 这张 SVG 曾经因为注释里写了
# “两个连字符 + out”那条参数而整份解析失败，Chrome 画出一张粉底黑字的
# 解析错误页，看着像"渲染坏了"，其实是文件坏了。
#
# **`$变量` 后面紧跟全角字符时要写成 `${变量}`。** bash 会把全角括号的字节
# 也算进变量名（`$icns（` 就成了一个没定义过的名字），`set -u` 下当场退出。
# 这一步也踩过一次：图都打好了，最后一行才炸。
#
set -euo pipefail

cd "$(dirname "$0")/.."
svgo="assets/wfiles.svg"
script_self="scripts/make-icon.sh"

out_dir="target/icon"
preview=""
force=0
while [ $# -gt 0 ]; do
    case "$1" in
        --out)
            shift
            out_dir="${1:?--out 后面要跟一个目录}"
            ;;
        --preview)
            shift
            preview="${1:?--preview 后面要跟一个 PNG 路径}"
            ;;
        --force) force=1 ;;
        *)
            echo "认不出的参数：$1" >&2
            echo "用法：$0 [--out <目录>] [--preview <png>] [--force]" >&2
            exit 2
            ;;
    esac
    shift
done

[ -f "$svgo" ] || { echo "找不到图标源稿：$svgo" >&2; exit 1; }

icns="$out_dir/WFiles.icns"
if [ "$force" = 0 ] && [ -f "$icns" ] && [ "$icns" -nt "$svgo" ] && [ "$icns" -nt "$script_self" ]; then
    echo "图标还是新的（比源稿和脚本都新），跳过：$icns"
    exit 0
fi

# 找一个能渲染的浏览器：先看常用安装位置，再看 PATH。
browser=""
for candidate in \
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
    "/Applications/Chromium.app/Contents/MacOS/Chromium" \
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge" \
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser" \
    "$(command -v chromium 2>/dev/null || true)" \
    "$(command -v google-chrome 2>/dev/null || true)"
do
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then
        browser="$candidate"
        break
    fi
done
[ -n "$browser" ] || {
    echo "没找到可用的浏览器（Chrome / Chromium / Edge / Brave）。" >&2
    echo "图标要用它渲染 SVG；装了任意一个再跑。" >&2
    exit 1
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "渲染图标（无头浏览器：$(basename "$browser")）……"
# 每个尺寸都从矢量重画一遍：窗口开成目标尺寸，`<img>` 显式给像素宽高。
# `--force-device-scale-factor=1` 挡住 Retina 机器上可能来的 2 倍图。
for size in 16 32 64 128 256 512 1024; do
    cat > "$work/wrap.html" <<HTML
<html><body style="margin:0;background:transparent"><img src="file://$PWD/$svgo" width="$size" height="$size" style="display:block"></body></html>
HTML
    "$browser" --headless=new --disable-gpu --hide-scrollbars \
        --no-first-run --no-default-browser-check --force-device-scale-factor=1 \
        --user-data-dir="$work/profile" --default-background-color=00000000 \
        --window-size="$size,$size" --screenshot="$work/$size.png" \
        "file://$work/wrap.html" >"$work/chrome.log" 2>&1 || true
    [ -s "$work/$size.png" ] || {
        echo "渲染 $size px 失败。浏览器最后几句话：" >&2
        tail -5 "$work/chrome.log" >&2
        exit 1
    }
done

# iconset 的十个条目，像素尺寸和文件名都是 iconutil 定死的。
iconset="$work/WFiles.iconset"
mkdir -p "$iconset"
cp "$work/16.png"   "$iconset/icon_16x16.png"
cp "$work/32.png"   "$iconset/icon_16x16@2x.png"
cp "$work/32.png"   "$iconset/icon_32x32.png"
cp "$work/64.png"   "$iconset/icon_32x32@2x.png"
cp "$work/128.png"  "$iconset/icon_128x128.png"
cp "$work/256.png"  "$iconset/icon_128x128@2x.png"
cp "$work/256.png"  "$iconset/icon_256x256.png"
cp "$work/512.png"  "$iconset/icon_256x256@2x.png"
cp "$work/512.png"  "$iconset/icon_512x512.png"
cp "$work/1024.png" "$iconset/icon_512x512@2x.png"

command -v iconutil >/dev/null 2>&1 || {
    echo "没有 iconutil——.icns 是 macOS 的产物，这个脚本要在 macOS 上跑。" >&2
    exit 1
}

mkdir -p "$out_dir"
echo "打包 .icns……"
iconutil -c icns -o "$icns" "$iconset"

if [ -n "$preview" ]; then
    mkdir -p "$(dirname "$preview")"
    cp "$work/512.png" "$preview"
    echo "预览图：$preview"
fi

echo "打好了：${icns}（$(du -h "$icns" | cut -f1)）"
