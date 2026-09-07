#!/usr/bin/env python3
"""KunxOS 打开应用可视化验证：跳过 OOBE，等桌面就绪，逐一打开指定应用并截屏。

用法：python3 tests/scripts/app_open_shots.py <out_shots_dir> [app ...]
默认打开 calculator、settings、store、clock（FBIO_PRESENT 修复的验证对象）。
对每个应用：点击 → 等「[app] 就绪」或「PANIC」→ 等 1.5s → screendump → 返回桌面。
"""
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
SHOTS = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_open_shots"
APPS = sys.argv[2:] or ["calculator", "settings", "store", "clock"]
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxo_")
PIPE = os.path.join(WORKDIR, "kx")
MON = os.path.join(WORKDIR, "mon")

# 应用 → (点击坐标 x, y, 串口就绪标记, 显示名)。坐标为桌面图标/托架中心。
APPS_DB = {
    "clock":      (150, 182, "[clock] 就绪", "Clock"),
    "calculator": (230, 182, "[calculator] 就绪", "Calculator"),
    "settings":   (418, 872, "[settings] 就绪", "Settings"),
    "store":      (230, 366, "[store] 就绪", "Store"),
}

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
        log.extend(data)
        logf.write(data)
        logf.flush()


threading.Thread(target=reader, daemon=True).start()


def tap(x, y, hold=0.06):
    try:
        infd = os.open(PIPE + ".in", os.O_WRONLY)
        os.write(infd, frame(FRAME_DOWN, x, y))
        time.sleep(hold)
        os.write(infd, frame(FRAME_UP))
        os.close(infd)
    except OSError:
        pass
    time.sleep(0.05)


def wait_for(text, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if text.encode() in log:
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
        time.sleep(0.4)
        try:
            s.settimeout(0.5)
            s.recv(4096)
        except socket.timeout:
            pass
        s.close()
    except Exception:
        pass


try:
    if not wait_for("[launcher] 桌面就绪", 30):
        # 全新镜像首启进 OOBE（13 步）：普通步点「下一步」快进；密码/
        # 胁迫 PIN 步拒绝空输入，停手 35s 让「30 秒无输入自动下一步」通过。
        if not wait_for("[oobe] 欢迎使用 KunxOS", 90):
            print("FAIL: neither OOBE nor desktop appeared")
            sys.exit(1)
        print("[test] OOBE running, fast-forwarding")
        deadline = time.time() + 600
        idle_until = 0.0
        while time.time() < deadline:
            if "[launcher] 桌面就绪".encode() in log:
                break
            now = time.time()
            if now < idle_until:
                time.sleep(1.0)
                continue
            if "密码太短".encode() in log[-4000:]:
                idle_until = now + 35
                continue
            tap(465, 925)
            time.sleep(0.8)
        if not wait_for("[launcher] 桌面就绪", 60):
            print("FAIL: 桌面未就绪（OOBE 后）")
            sys.exit(1)
    print("[test] 桌面就绪")
    time.sleep(1.5)
    for name in APPS:
        if name not in APPS_DB:
            print("skip unknown app", name)
            continue
        x, y, marker, disp = APPS_DB[name]
        tap(x, y)
        ready = wait_for(marker, 25)
        panicked = ("PANIC: " + name).encode() in log
        time.sleep(1.5)
        shot("%s.ppm" % name)
        # 返回桌面
        tap(24, 24)
        wait_for("应用 '%s' 已退出" % disp, 20)
        time.sleep(2.0)
        print("%s: ready=%s panic=%s" % (name, ready, panicked))
finally:
    stop = True
    p.terminate()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
    logf.close()
    print("[test] serial log:", os.path.join(SHOTS, "serial.log"))
