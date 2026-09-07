#!/usr/bin/env python3
"""KunxOS 应用 UI 截图探针：逐格点桌面网格，识别每个格子的应用并截其 UI。

用法：python3 tests/scripts/app_ui_probe.py <shots_dir>
对第 0/1 页每格：点击 → 等 6s 内的「[app] 就绪」→ 等 2s → screendump →
点返回键 → 等返回转场。输出每格命中的应用名。
"""
import os
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
SHOTS = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_probe"
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxp_")
PIPE = os.path.join(WORKDIR, "kx")
MON = os.path.join(WORKDIR, "mon")

FRAME_DOWN, FRAME_UP, FRAME_MOVE = 0x01, 0x00, 0x02


def frame(kind, x=0, y=0):
    return bytes([0xAA, kind, x & 0xFF, (x >> 8) & 0xFF, y & 0xFF, (y >> 8) & 0xFF])


os.mkfifo(PIPE + ".in")
os.mkfifo(PIPE + ".out")

cmd = ["qemu-system-aarch64", "-M", "virt", "-cpu", "cortex-a72", "-m", "512M",
       "-kernel", KERNEL, "-serial", "pipe:" + PIPE,
       "-device", "ramfb", "-no-reboot", "-display", "none",
       "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
       "-monitor", "unix:" + MON + ",server,nowait"]

logf = open(os.path.join(SHOTS, "serial.log"), "wb")
p = subprocess.Popen(cmd, cwd=WORKDIR, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
log = bytearray()
stop = False
lock = threading.Lock()


def reader():
    try:
        outfd = os.open(PIPE + ".out", os.O_RDONLY)
    except OSError:
        return
    while not stop:
        try:
            data = os.read(outfd, 4096)
        except OSError:
            break
        if not data:
            time.sleep(0.02)
            continue
        with lock:
            log.extend(data)
            logf.write(data)
            logf.flush()


threading.Thread(target=reader, daemon=True).start()


def send_frame(fr):
    try:
        infd = os.open(PIPE + ".in", os.O_WRONLY)
        os.write(infd, fr)
        os.close(infd)
    except OSError:
        pass


def tap(x, y, hold=0.06):
    send_frame(frame(FRAME_DOWN, x, y))
    time.sleep(hold)
    send_frame(frame(FRAME_UP))
    time.sleep(0.05)


def swipe(x1, y1, x2, y2, steps=10, step_ms=0.012):
    send_frame(frame(FRAME_DOWN, x1, y1))
    time.sleep(0.03)
    for i in range(1, steps + 1):
        t = i / float(steps)
        send_frame(frame(FRAME_MOVE, int(round(x1 + (x2 - x1) * t)), int(round(y1 + (y2 - y1) * t))))
        time.sleep(step_ms)
    time.sleep(0.02)
    send_frame(frame(FRAME_UP))
    time.sleep(0.1)


def snapshot():
    with lock:
        return bytes(log)


def wait_for(text, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if text.encode() in snapshot():
            return True
        if p.poll() is not None:
            return False
        time.sleep(0.2)
    return False


def shot(name):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(MON)
        s.sendall(("screendump %s\n" % os.path.join(SHOTS, name)).encode())
        time.sleep(0.5)
        try:
            s.settimeout(0.5)
            s.recv(4096)
        except socket.timeout:
            pass
        s.close()
    except Exception:
        pass


def oobe_fastforward():
    if wait_for("[launcher] 桌面就绪", 30):
        return
    if not wait_for("[oobe] 欢迎使用 KunxOS", 90):
        print("FAIL: no OOBE/desktop")
        sys.exit(1)
    deadline = time.time() + 600
    idle_until = 0.0
    while time.time() < deadline:
        if "[launcher] 桌面就绪".encode() in snapshot():
            break
        now = time.time()
        if now < idle_until:
            time.sleep(1.0)
            continue
        if "密码太短".encode() in snapshot()[-4000:]:
            idle_until = now + 35
            continue
        tap(465, 925)
        time.sleep(0.8)
    wait_for("[launcher] 桌面就绪", 60)


GRID = [(150, 366), (230, 366), (310, 366), (390, 366),
        (150, 460), (230, 460), (310, 460), (390, 460)]
# CLI 覆盖：app_ui_probe.py <shots> 310,182 390,182 ...（仅第 0 页，含特殊行）
if len(sys.argv) > 2:
    GRID = [tuple(int(v) for v in a.split(",")) for a in sys.argv[2:]]

try:
    oobe_fastforward()
    print("[probe] desktop ready")
    time.sleep(1.5)
    pages = (0,) if len(sys.argv) > 2 else (0, 1)
    for page in pages:
        for (x, y) in GRID:
            base = len(snapshot())
            tap(x, y)
            time.sleep(3.0)
            seg = snapshot()[base:]
            m = re.search(b"\\[launcher\\] " + "启动".encode() + b" (\\S+)", seg)
            name = m.group(1).decode() if m else "(miss)"
            m2 = re.search(b"\\[(\\S+)\\] " + "就绪".encode(), seg)
            ready = m2.group(1).decode() if m2 else "-"
            panic = b"PANIC" in seg
            if name != "(miss)":
                time.sleep(3.0)  # 等字体/首帧
                shot("p%d_%s.png" % (page, name))
            tap(24, 24)
            time.sleep(3.5)
            print("page%d (%d,%d): %s ready=%s panic=%s" % (page, x, y, name, ready, panic))
        if page == 0:
            swipe(400, 500, 120, 500)
            time.sleep(1.2)
    print("[probe] done")
finally:
    stop = True
    p.terminate()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
    logf.close()
