#!/usr/bin/env python3
"""KunxOS launcher 转场测试：跳过 OOBE（允许其崩溃），直接等 launcher 桌面，
点击第一个应用图标，采集启动/返回转场的 [motion] 帧统计。

用法：python3 tools/launcher_test.py <logfile>
"""
import subprocess, sys, time, threading, os, tempfile

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_launcher.log"
WORKDIR = tempfile.mkdtemp(prefix="kxl_")
PIPE = os.path.join(WORKDIR, "kx")

FRAME_DOWN = 0x01
FRAME_UP = 0x00

def frame(kind, x, y):
    return bytes([0xAA, kind, x & 0xFF, (x >> 8) & 0xFF, y & 0xFF, (y >> 8) & 0xFF])

os.mkfifo(PIPE + ".in")
os.mkfifo(PIPE + ".out")

cmd = ["qemu-system-aarch64", "-M", "virt", "-cpu", "cortex-a72", "-m", "512M",
       "-kernel", KERNEL, "-serial", "pipe:" + PIPE,
       "-device", "ramfb", "-no-reboot", "-display", "none",
       "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0"]

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
            break
        log.extend(c)
        logf.write(c)
        logf.flush()

t = threading.Thread(target=reader, daemon=True)
t.start()

try:
    infd = os.open(PIPE + ".in", os.O_WRONLY)
except OSError:
    infd = -1

def tap(x, y, hold=0.02):
    if infd < 0:
        return
    try:
        os.write(infd, frame(FRAME_DOWN, x, y))
        time.sleep(hold)
        os.write(infd, frame(FRAME_UP, 0, 0))
    except OSError:
        pass

def wait_for(marker, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker.encode() in bytes(log):
            return True
        if p.poll() is not None:
            return False
        time.sleep(0.1)
    return False

try:
    if not wait_for("Hello from Rust kernel", 45):
        print("FAIL: kernel did not boot")
        sys.exit(1)
    print("[test] kernel booted")
    # 等 launcher 桌面就绪（OOBE 崩溃与否无所谓，init 会继续拉起 launcher）
    if not wait_for("[launcher] desktop ready", 60):
        print("FAIL: launcher desktop not ready")
        sys.exit(1)
    print("[test] launcher desktop ready")
    time.sleep(1.5)
    # 点击第一个应用图标（网格第一格）。该应用可能不存在（execve 失败），
    # 仍会完整播放 启动转场 -> spawn_app(失败退出) -> 返回转场。
    tap(130, 100)
    print("[test] tapped first icon (130,100)")
    # 采集启动转场统计 + 应用拉起/失败 + 返回转场统计
    wait_for("[launcher] app '", 20)
    wait_for("[motion] return transition", 20)
    print("[test] transitions captured")
    # 决定性验证：空闲后（应进入 sleep_ms 帧预算睡眠）再点一次空白处。
    # 若内核 nanosleep 唤醒路径损坏，launcher 会冻结，第二次点击无任何输出。
    time.sleep(3)
    tap(400, 500)
    print("[test] second tap sent (400,500)")
    time.sleep(3)
    if "[input] touch down (400,500)".encode() in open(LOGFILE, "rb").read():
        print("[test] SECOND TAP PROCESSED -> sleep works, launcher alive")
    else:
        print("[test] second tap NOT processed -> launcher frozen (sleep hang)")
    time.sleep(1)
finally:
    stop = True
    try:
        p.terminate()
    except Exception:
        pass
    time.sleep(1)
    try:
        p.kill()
    except Exception:
        pass
    if infd >= 0:
        try:
            os.close(infd)
        except OSError:
            pass
    time.sleep(0.3)
    logf.close()
    print("[test] log saved to", LOGFILE)
