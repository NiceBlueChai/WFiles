#!/usr/bin/env python3
"""在一个真 pty 里跑一条命令，把最后一屏文本打出来。

为什么需要它：NSH 0.4.0 的 `nsh test --target tui` 在**带 Rust 原生模块的 app**
上一次都跑不起来（`.nsh/rust/tui-host` 的引擎里没有 app 的 UI set，
`bundle failed: could not load module 'wfiles/native'`），而 `nsh run --target tui`
是好的。于是 TUI 这条路的判据只能落在「真跑起来、看屏幕上有什么」上。

两个必须自己处理的坑：
  · `script` 给的 pty 默认 0×0，TUI 会画个空屏 —— 必须 `TIOCSWINSZ` 显式设尺寸；
  · 输出是光标定位的增量帧，直接按行 `grep` 会读到重叠的旧内容 —— 必须回放成
    最后一屏再判。

解析够用就行：CUP / CHA / VPA / CUU / CUD / CUF / CUB / ED / EL / **ECH / DCH / ICH**、
DECSET 1049（交替屏幕，进的时候会清屏）、OSC 跳过、SGR 跳过。
East Asian 宽字符占两格（不然中文在屏上会错位，判据就写成错的了）。

**ECH（`\x1b[nX`）不能省**：ratatui 只重画脏格子，一行里的文字变短时它用 ECH 擦掉
尾巴上的旧字，不认这条就会把「12 小时前」和先前的「6 天前」叠成「12 小前时 前」——
看起来像界面被挤坏了，其实是回放器在撒谎（本项目踩过：差点据此去改布局）。

用法：
    tty_capture.py COLS ROWS SECONDS [--ready TEXT] [--send 秒:按键 ...] -- CMD [ARGS...]

    --ready alpha.txt      等到屏上出现 `alpha.txt` 才开始计时（首次跑要编 host，
                           按绝对秒数发键会打进编译期）
    --send 1:enter         出现后第 1 秒按回车
    --send 2:down+down     连按（一次写进去，程序按顺序吃）

退出码：命令正常被我们杀掉（超时）算 0；pty 建不出来等真错误非 0。
"""
import fcntl
import os
import pty
import re
import select
import struct
import sys
import termios
import time
import unicodedata


def width_of(ch: str) -> int:
    """终端里这个字符占几格。"""
    if unicodedata.combining(ch):
        return 0
    if unicodedata.east_asian_width(ch) in ("W", "F"):
        return 2
    return 1


def replay(text: str, cols: int, rows: int) -> list[str]:
    screen = [[" "] * cols for _ in range(rows)]
    cont = [set() for _ in range(rows)]  # 宽字符的右半格，打印时跳过
    x = y = 0
    i, n = 0, len(text)

    def put(ch: str) -> None:
        nonlocal x, y
        w = width_of(ch)
        if w == 0:
            return
        if x + w > cols:
            x, y = 0, min(rows - 1, y + 1)
        if 0 <= y < rows and 0 <= x < cols:
            screen[y][x] = ch
            cont[y].discard(x)
            if w == 2 and x + 1 < cols:
                cont[y].add(x + 1)
        x += w
        if x >= cols:
            x, y = 0, min(rows - 1, y + 1)

    while i < n:
        ch = text[i]
        if ch == "\x1b":
            if i + 1 >= n:
                break
            nxt = text[i + 1]
            if nxt == "[":
                m = re.match(r"\x1b\[([0-9;?]*)([A-Za-z@`~])", text[i:])
                if not m:
                    i += 1
                    continue
                raw, final = m.group(1), m.group(2)
                nums = [int(p) for p in raw.replace("?", "").split(";") if p.isdigit()]
                first = nums[0] if nums and nums[0] else 1
                if final in "Hf":
                    y = max(0, (nums[0] if len(nums) > 0 and nums[0] else 1) - 1)
                    x = max(0, (nums[1] if len(nums) > 1 and nums[1] else 1) - 1)
                elif final == "G":
                    x = max(0, first - 1)
                elif final == "d":
                    y = max(0, first - 1)
                elif final == "A":
                    y = max(0, y - first)
                elif final == "B":
                    y = min(rows - 1, y + first)
                elif final == "C":
                    x = min(cols - 1, x + first)
                elif final == "D":
                    x = max(0, x - first)
                elif final == "J":
                    mode = nums[0] if nums else 0
                    if mode == 2 or mode == 3:
                        screen = [[" "] * cols for _ in range(rows)]
                        cont = [set() for _ in range(rows)]
                    elif mode == 0:
                        for cx in range(x, cols):
                            screen[y][cx] = " "
                        for cy in range(y + 1, rows):
                            screen[cy] = [" "] * cols
                            cont[cy] = set()
                elif final == "K":
                    mode = nums[0] if nums else 0
                    if mode == 0:
                        for cx in range(x, cols):
                            screen[y][cx] = " "
                    elif mode == 2:
                        screen[y] = [" "] * cols
                        cont[y] = set()
                elif final == "X":
                    # ECH：擦掉 n 格（ratatui 一行变短时就靠它清尾巴）。
                    for cx in range(x, min(cols, x + first)):
                        screen[y][cx] = " "
                        cont[y].discard(cx)
                elif final == "P":
                    # DCH：删掉 n 格，右边的左移。
                    n = first
                    row = screen[y]
                    del row[x : min(cols, x + n)]
                    row.extend([" "] * (cols - len(row)))
                    screen[y] = row
                    cont[y] = {
                        c - n if c >= x + n else c for c in cont[y] if c < x or c >= x + n
                    }
                elif final == "@":
                    # ICH：插 n 格，右边的右移。
                    n = first
                    row = screen[y]
                    row[x:x] = [" "] * n
                    del row[cols:]
                    screen[y] = row
                    cont[y] = {c + n if c >= x else c for c in cont[y] if c + n < cols}
                elif final in "hl" and raw.startswith("?"):
                    # 私有模式：1049 = 交替屏幕，进的时候整屏清空。
                    if 1049 in nums and final == "h":
                        screen = [[" "] * cols for _ in range(rows)]
                        cont = [set() for _ in range(rows)]
                        x = y = 0
                i += m.end()
                continue
            if nxt == "]":
                m = re.match(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", text[i:])
                i += m.end() if m else 1
                continue
            i += 2
            continue
        if ch == "\r":
            x = 0
        elif ch == "\n":
            y = min(rows - 1, y + 1)
        elif ch == "\b":
            x = max(0, x - 1)
        elif ch in ("\x07", "\x00"):
            pass
        else:
            put(ch)
        i += 1

    out = []
    for row, skip in zip(screen, cont):
        # 宽字符占的那半格直接不输出，不再补一个空格：补空格会把每个汉字
        # 拆成「目 录」，判据写 `子目录` 就永远匹配不上。
        out.append("".join(c for i, c in enumerate(row) if i not in skip).rstrip())
    return out


KEYS = {
    "up": b"\x1b[A",
    "down": b"\x1b[B",
    "right": b"\x1b[C",
    "left": b"\x1b[D",
    "enter": b"\r",
    "tab": b"\t",
    "bs": b"\x7f",      # Backspace（xterm 的 DEL）
    "esc": b"\x1b",
    "space": b" ",
    "ctrl-a": b"\x01",
    "ctrl-z": b"\x1a",
}


def keys_of(spec: str) -> bytes:
    """`down` / `enter` 之类的名字，或 `+` 连起来的一串（`down+down+enter`），
    或原样一个字符。

    TUI 里键盘是**唯一**能带修饰键的入口（`nsh/keymap` 拿到的是 `"shift+down"`
    这样的字符串），所以冒烟要能真的按下去，不能只靠 `Wfiles.xxx()` 从桥接层
    绕过去——那样测的是桥，不是这条路。
    """
    if spec in KEYS:
        return KEYS[spec]
    if "+" in spec:
        return b"".join(KEYS.get(part, part.encode()) for part in spec.split("+"))
    return spec.encode()


def main() -> int:
    argv = sys.argv[1:]
    # `--` 之前是参数：三个位置参数 + 任意顺序的 `--send` / `--ready`。
    # 早期版本只认参数在位置参数**之前**，脚本里写在后面就整趟没跑起来，
    # 只打了 usage —— 判据于是红得不明不白。
    if "--" not in argv:
        print(__doc__, file=sys.stderr)
        return 2
    split = argv.index("--")
    flags, args, cmd = argv[:split], [], argv[split + 1 :]
    sends: list[tuple[float, str]] = []
    ready: str | None = None
    i = 0
    while i < len(flags):
        if flags[i] == "--send":
            when, spec = flags[i + 1].split(":", 1)
            sends.append((float(when), spec))
            i += 2
        elif flags[i] == "--ready":
            ready = flags[i + 1]
            i += 2
        else:
            args.append(flags[i])
            i += 1
    if len(args) != 3 or not cmd:
        print(__doc__, file=sys.stderr)
        return 2
    cols, rows, seconds = int(args[0]), int(args[1]), float(args[2])

    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(cmd[0], cmd)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    buf = b""
    started = time.time()
    deadline = started + seconds
    ready_at: float | None = None if ready else started
    sends.sort()
    while time.time() < deadline:
        if ready_at is None and ready is not None and ready.encode() in buf:
            # `--ready` 等的是**界面真的上屏**那一刻：首次跑要编 host，
            # 按绝对秒数发键在慢机器上会打进编译期。等它出现，再起算。
            ready_at = time.time()
        while sends and ready_at is not None and sends[0][0] <= time.time() - ready_at:
            _, spec = sends.pop(0)
            os.write(fd, keys_of(spec))
        ready_in, _, _ = select.select([fd], [], [], 0.1)
        if fd in ready_in:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            buf += chunk

    try:
        os.kill(pid, 9)
    except ProcessLookupError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass

    for line in replay(buf.decode("utf-8", "replace"), cols, rows):
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
