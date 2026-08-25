#!/usr/bin/env python3
"""KunxOS 流畅度测量脚本：QEMU pipe 串口（双向 FIFO）+ 触摸注入。

流程：
  1. 启动 QEMU（-serial pipe:），kx.out 收实时日志，kx.in 发触摸帧
  2. 等 OOBE 欢迎页 -> 每 ~900ms 注入一次下半屏点击，推进 10 步
  3. 等 launcher 桌面就绪 -> 点击第一个应用图标（触发 fork+execve）
  4. 收集串口日志到文件，退出
"""
import subprocess, sys, time, threading, os, tempfile

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kunxos_boot.log"
TAP_ICON = os.environ.get("TAP_ICON", "1") == "1"
WORKDIR = tempfile.mkdtemp(prefix="kxser_")
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

# 打开输入 FIFO（阻塞直到 QEMU 打开读端）
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
    print("[harness] kernel booted")

    if not wait_for("[anim-perf] pipeline perf test end", 45):
        print("WARN: anim-perf marker missing")

    if not wait_for("[oobe] welcome to KunxOS", 45):
        print("FAIL: oobe welcome missing")
        sys.exit(1)
    print("[harness] oobe welcome seen, tapping next")

    for i in range(10):
        time.sleep(0.9)
        tap(400, 900)
        if wait_for("[oobe] exiting", 60):
            print("[harness] oobe exited after %d taps" % (i + 1))
            break

    if not wait_for("[launcher] desktop ready", 45):
        print("WARN: launcher desktop ready missing")
    else:
        print("[harness] launcher ready")
        if TAP_ICON:
            time.sleep(1.5)
            tap(130, 100)
            print("[harness] tapped first app icon (130,100)")
            wait_for("[launcher] app '", 20)

    time.sleep(8)
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
    print("[harness] log saved to", LOGFILE)
