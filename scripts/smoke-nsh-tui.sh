#!/usr/bin/env bash
# WFiles × NSH 的**终端**冒烟：真跑 `nsh run --target tui`，在一个 120×40 的
# 真 pty 里，把最后一屏回放成文本，再逐项判。
#
# 为什么不是 `nsh test --target tui`：NSH 0.4.0 那条路上，**带 Rust 原生模块的
# app 一次都跑不起来**——`.nsh/rust/tui-host` 起来了、bundle 也编好了，但引擎里
# 没有 app 自己的 UI set：
#     error bundle failed: Error: could not load module 'wfiles/native'
# 同一个二进制 `nsh run --target tui` 却是好的，所以缺口在测试通道，不在宿主。
# 在它修好之前，TUI 的判据只能落在这里：真终端 + 真 app + 屏上真有的字。
#
# 判据挑的是「接上了那条路」：
#   · alpha.txt / beta.txt 是 core 遍历**磁盘**来的（不是界面里写死的）
#   · `0 B` 是 core 的 format_size 出的
#   · `重名：保留两者` 是 core 的 ConflictPolicy 文案
#   · `撤销` 是 core 的 undo_label 转发（栈空时就是这两个字）
#   · `3 / 3` 是 core 的「显示数 / 总数」
# 把桥接层任何一条摘掉，屏上就只剩壳——见 docs/nsh-client.md §六 的负验证。
#
# 环境：`WFILES_HOME` 指到夹具目录（app 的落点是 `Wfiles::home()`）。
# 不能用 `HOME` 顶替——rustup / cargo 的 toolchain 就在 `$HOME` 底下，
# 一改 nsh 连 cargo 都起不来（实测：`rustup could not choose a version of cargo`）。
# NSH 默认取 ~/.nsh-source/bin/nsh（源码构建的 0.4.0，带 tui target）；
# 发行版 nsh 没有 tui，这个脚本会直接报错而不是假装绿。
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
NSH="${NSH:-$HOME/.nsh-source/bin/nsh}"
SECONDS_OPEN="${SMOKE_SECONDS:-90}"

cd "$HERE/nsh-poc"

if [[ ! -x "$NSH" ]]; then
  printf '找不到 %s：TUI 需要带 tui target 的 0.4.0（源码构建），见 docs/nsh-client.md\n' "$NSH" >&2
  exit 2
fi

export DEVELOPER_DIR=/Library/Developer/CommandLineTools
export CARGO_INCREMENTAL=0

ROOT="$(mktemp -d)"
OUT="$(mktemp)"
DEEP="$(mktemp)"
trap 'rm -rf "$ROOT" "$OUT" "$DEEP"' EXIT

# 夹具：两个文件 + 一个子目录（子目录里再放一个只有它有的文件）。
# ASCII 名字，免得判据跟终端宽度纠缠。
mkdir -p "$ROOT/子目录"
: > "$ROOT/alpha.txt"
printf 'beta needle\n' > "$ROOT/beta.txt"
: > "$ROOT/子目录/inside.txt"

WFILES_HOME="$ROOT" python3 "$HERE/scripts/tty_capture.py" 120 40 "$SECONDS_OPEN" -- \
  "$NSH" run --target tui > "$OUT" 2>&1 || true

# 第二趟：真的按键盘。`--ready` 等界面把 `alpha.txt` 画上来再起算，
# 否则首次跑编 host 的那 20 多秒里发出去的键全丢。
#
# **必须先 `↓`**：焦点初始是空的（没有选中行），直接回车 `openFocused()`
# 什么也不做——第一版就是这么红的，屏上一动不动，看不出是键没到还是没焦点。
# `↓` 之后焦点落在第一行（目录优先 → `子目录`），再回车才进得去。
WFILES_HOME="$ROOT" python3 "$HERE/scripts/tty_capture.py" 120 40 "$SECONDS_OPEN" \
  --ready 'alpha.txt' --send 1:down --send 2:enter -- \
  "$NSH" run --target tui > "$DEEP" 2>&1 || true

fail=0
check() {
  if grep -qF -- "$1" "$OUT"; then
    printf '  ok   %s\n' "$2"
  else
    printf '  FAIL %s\n' "$2"
    fail=1
  fi
}
# 同上，但看的是「按了回车之后」那一屏。
check_deep() {
  if grep -qF -- "$1" "$DEEP"; then
    printf '  ok   %s\n' "$2"
  else
    printf '  FAIL %s\n' "$2"
    fail=1
  fi
}
check_deep_absent() {
  if grep -qF -- "$1" "$DEEP"; then
    printf '  FAIL %s（屏上还有「%s」，说明没进去）\n' "$2" "$1"
    fail=1
  else
    printf '  ok   %s\n' "$2"
  fi
}

check 'alpha.txt' '列出的是磁盘上的真文件（core 遍历 → 桥接 → 终端）'
check 'beta.txt' '第二个文件也在，不是只画了第一条'
check '子目录'   '目录也在，排序里目录优先'
check '0 B'      '空文件的大小是 core::format_size 出的文案'
check '12 B'     '非空文件的大小也来自 core（读了磁盘元数据，不是猜的）'
check '重名：保留两者' '冲突策略的文案来自 core，不是界面另存的'
check '3 / 3'    '底栏的「显示数 / 总数」来自 core 的计数'

# 键盘这条才是 TUI 的真正入口：headless 那 43 条全走 `app.press`，终端里能不能
# 收到键、keymap 认不认、Enter 走不走 `Command::Enter`，只有真按下去才知道。
# 判据落在「进没进去」：子目录里独有的文件在不在，上一层那个文件是不是已经没了。
check_deep 'inside.txt' '回车真的进了子目录（子目录独有的文件上屏）'
check_deep_absent 'alpha.txt' '进了子目录就不该还画着上一层的文件'
check_deep '1 / 1' '底栏跟着变成子目录的计数'

# 界面骨架得先画出来，否则上面几条「找不到」会当成功能坏了。
for want in 类型 名称 撤销; do
  if ! grep -qF -- "$want" "$OUT"; then
    printf '  FAIL 骨架缺了「%s」（界面没渲染）\n' "$want"
    fail=1
  fi
done

if (( fail )); then
  printf '\n--- 开屏那一屏 ---\n'
  cat "$OUT"
  printf '\n--- 按回车之后 ---\n'
  cat "$DEEP"
  exit 1
fi
printf 'NSH 终端冒烟：10/10 通过（7 条静态 + 3 条键盘）\n'
