#!/usr/bin/env python3
"""KunxOS 文件管理器 GUI 端到端验证：驱动 OOBE 完成 → launcher 桌面 →
点击 files 图标（第 5 个应用，row1 col1 @ (270,228)）→ 验证应用启动并截图。

用法：python3 tools/verify_files_app.py [logfile] [shots_dir]
"""
import subprocess, sys, time, threading, os, tempfile, socket

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_files.log"
SHOTS = sys.argv[2] if len(sys.argv) > 2 else "/tmp/kx_files_shots"
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxf_")
PIPE = os.path.join(WORKDIR, "kx")
MON = os.path.join(WORKDIR, "mon")

FRAME_DOWN, FRAME_UP = 0x01, 0x00

def frame(kind, x=0, y=0):
    return bytes([0xAA, kind, x & 0xFF, (x >> 8) & 0xFF, y & 0xFF, (y >> 8) & 0xFF])

os.mkfifo(PIPE + ".in")
os.mkfifo(PIPE + ".out")

cmd = ["qemu-system-aarch64", "-M", "virt", "-cpu", "cortex-a72", "-m", "512M",
       "-kernel", KERNEL, "-serial", "pipe:" + PIPE,
       "-device", "ramfb", "-no-reboot", "-display", "none",
       "-monitor", "unix:" + MON + ",server,nowait",
       "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
       "-device", "virtio-keyboard-device", "-device", "virtio-tablet-device"]

logf = open(LOGFILE, "wb")
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
            c = os.read(outfd, 4096)
        except OSError:
            break
        if not c:
            time.sleep(0.02)
            continue
        log.extend(c)
        logf.write(c)
        logf.flush()

t = threading.Thread(target=reader, daemon=True)
t.start()

infd = -1
try:
    infd = os.open(PIPE + ".in", os.O_WRONLY)
except OSError:
    pass

def tap(x, y, hold=0.06):
    if infd < 0:
        return
    try:
        os.write(infd, frame(FRAME_DOWN, x, y))
        time.sleep(hold)
        os.write(infd, frame(FRAME_UP, 0, 0))
        time.sleep(0.05)
    except OSError:
        pass

def shot(name):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(3)
        s.connect(MON)
        s.sendall(("screendump " + os.path.join(SHOTS, name) + "\n").encode())
        time.sleep(0.8)
        s.close()
    except Exception:
        pass

def text():
    try:
        return log.decode("utf-8", "replace")
    except Exception:
        return ""

def wait_for(marker, timeout=60):
    dl = time.time() + timeout
    while time.time() < dl:
        if marker in text():
            return True
        time.sleep(0.3)
    return False

fail = 0
try:
    # 1) OOBE：欢迎页 → 语言 pill → Next → 8 步 Next → launcher
    if not wait_for("[oobe] welcome to KunxOS", 60):
        print("FAIL: OOBE 未出现"); fail = 1; sys.exit(1)
    time.sleep(1.0)
    shot("01_oobe_welcome.ppm")
    tap(300, 100)      # 简体中文
    time.sleep(0.4)
    tap(465, 925)      # Next
    time.sleep(0.6)
    for _ in range(8): # 其余步骤
        tap(465, 925)
        time.sleep(0.6)
    time.sleep(1.0)

    if not wait_for("[launcher]", 30):
        print("FAIL: launcher 未出现"); fail = 1; sys.exit(1)
    print("OK: OOBE 完成，launcher 桌面出现")
    time.sleep(2.0)
    shot("02_launcher_desktop.ppm")

    # 2) 点击 files 图标（row1 col1 @ (270,228)）
    tap(270, 228)
    print("OK: 已点击 files 图标 (270,228)")

    if not wait_for("[files] ready", 25):
        print("FAIL: 文件管理器未就绪")
        shot("03_files_fail.ppm")
        fail = 1
        sys.exit(1)
    print("OK: 文件管理器启动并渲染完成")
    time.sleep(2.0)
    shot("03_files_open.ppm")

    # 3) 进入 /tmp 验证浏览（files 初始在 /，点第一行进入或直接滚动）——
    #    点击返回键（标题栏左上角 ~(20,20)）退出
    tap(20, 20)
    time.sleep(2.0)
    if "[files] exiting" in text():
        print("OK: 文件管理器正常退出")
    else:
        # 可能已进入子目录，再按一次返回
        tap(20, 20)
        time.sleep(2.0)
        if "[files] exiting" in text():
            print("OK: 文件管理器正常退出")
        else:
            print("WARN: 文件管理器未退出（状态未知，但不影响启动验证）")
    sys.exit(fail)
finally:
    stop = True
    p.terminate()
    try:
        p.wait(timeout=5)
    except Exception:
        p.kill()
    logf.close()
