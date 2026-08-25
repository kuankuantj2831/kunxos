#!/usr/bin/env python3
"""KunxOS 全应用回归测试：启动 → OOBE → 桌面 → 逐一打开全部 17 个应用并返回。

用法：python3 tools/apps_regression.py [logfile] [shotsdir]
"""
import subprocess, sys, time, threading, os, tempfile, socket

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_apps.log"
SHOTS = sys.argv[2] if len(sys.argv) > 2 else "/tmp/kx_apps_shots"
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxa_")
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
       "-device", "virtio-keyboard-device", "-device", "virtio-tablet-device",
       "-device", "virtio-rng-device", "-device", "AC97",
       "-monitor", "unix:" + MON + ",server,nowait"]

logf = open(LOGFILE, "wb")
p = subprocess.Popen(cmd, cwd=WORKDIR, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
log = bytearray()
stop = False
results = []

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
        log.extend(data); logf.write(data); logf.flush()

t = threading.Thread(target=reader, daemon=True)
t.start()

def send_frame(fr):
    infd = os.open(PIPE + ".in", os.O_WRONLY)
    os.write(infd, fr)
    os.close(infd)

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
        x = int(round(x1 + (x2 - x1) * t))
        y = int(round(y1 + (y2 - y1) * t))
        send_frame(frame(FRAME_MOVE, x, y))
        time.sleep(step_ms)
    time.sleep(0.02)
    send_frame(frame(FRAME_UP))
    time.sleep(0.1)

def shot(name):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(MON)
        s.sendall(("screendump %s\n" % os.path.join(SHOTS, name)).encode())
        time.sleep(0.4)
        try:
            s.settimeout(0.5); s.recv(4096)
        except socket.timeout:
            pass
        s.close()
    except Exception:
        pass

def wait_for(text, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if text.encode() in log:
            return True
        if p.poll() is not None:
            return False
        time.sleep(0.2)
    return False

def since(text):
    """text 出现后是否有后续日志（用于确认进程退出后的桌面恢复）。"""
    return text.encode() in log

try:
    if not wait_for("[oobe] welcome to KunxOS", 60):
        print("FAIL: kernel did not boot to OOBE")
        sys.exit(1)
    time.sleep(0.5)
    # 快进 OOBE（10 步，前 9 步点下一步）
    for i in range(9):
        tap(465, 925)
        time.sleep(0.35)
    if not wait_for("[launcher] desktop ready", 60):
        print("FAIL: no desktop after OOBE")
        sys.exit(1)
    time.sleep(1.5)
    shot("00_desktop_page0.ppm")

    # 第 0 页 9 个应用（icon 中心）
    page0 = [
        ("phone",    130, 100, "PHONE"),
        ("sms",      270, 100, "SMS"),
        ("contacts", 410, 100, "Contacts"),
        ("settings", 130, 228, "Settings"),
        ("files",    270, 228, "Files"),
        ("browser",  410, 228, "Browser"),
        ("calc",     130, 356, "Calculator"),
        ("calendar", 270, 356, "Calendar"),
        ("camera",   410, 356, "Camera"),
    ]
    for name, x, y, disp in page0:
        tap(x, y)
        ok = wait_for("[%s] ready" % name, 20)
        time.sleep(0.8)
        shot("01_%s.ppm" % name)
        tap(24, 24)  # 返回键
        back = wait_for("app '%s' exited" % disp, 20)
        time.sleep(0.6)
        results.append((name, ok and back))
        print("%s: ready=%s back=%s" % (name, ok, back))

    # 切到第 1 页
    swipe(400, 500, 120, 500)
    time.sleep(1.0)
    shot("02_desktop_page1.ppm")

    page1 = [
        ("clock",    130, 100, "Clock"),
        ("gallery",  270, 100, "Gallery"),
        ("music",    410, 100, "Music"),
        ("mail",     130, 228, "Mail"),
        ("map",      270, 228, "Maps"),
        ("weather",  410, 228, "Weather"),
        ("store",    130, 356, "Store"),
        ("terminal", 270, 356, "Terminal"),
    ]
    for name, x, y, disp in page1:
        tap(x, y)
        ok = wait_for("[%s] ready" % name, 20)
        time.sleep(0.8)
        shot("03_%s.ppm" % name)
        tap(24, 24)
        back = wait_for("app '%s' exited" % disp, 20)
        time.sleep(0.6)
        results.append((name, ok and back))
        print("%s: ready=%s back=%s" % (name, ok, back))

    # 交互抽查：phone 拨号 + 呼叫
    swipe(400, 500, 120, 500)
    time.sleep(0.8)
    tap(130, 100)  # phone
    wait_for("[phone] ready", 20)
    time.sleep(0.6)
    tap(204, 570)  # 数字 5（键盘 row1 col1 中心）
    time.sleep(0.3)
    tap(204, 570)  # 再按 5
    time.sleep(0.3)
    tap(424, 830)  # 呼叫按钮（右下）
    time.sleep(0.5)
    shot("04_phone_calling.ppm")
    tap(424, 830)  # 挂断
    time.sleep(0.4)
    tap(24, 24)
    wait_for("app 'Phone' exited", 20)
    print("phone interaction done")

    # 交互抽查：calendar 翻月
    tap(270, 356)  # calendar (page0 slot7)
    wait_for("[calendar] ready", 20)
    time.sleep(0.5)
    tap(452, 82)  # 下月按钮
    time.sleep(0.4)
    shot("05_calendar_next_month.ppm")
    tap(24, 24)
    wait_for("app 'Calendar' exited", 20)
    print("calendar interaction done")

    # 交互抽查：music 播放/暂停
    swipe(400, 500, 120, 500)
    time.sleep(0.8)
    tap(410, 100)  # music (page1 slot2)
    wait_for("[music] ready", 20)
    time.sleep(0.5)
    tap(288, 608)  # 播放/暂停
    time.sleep(1.2)
    shot("06_music_playing.ppm")
    tap(288, 608)  # 暂停
    time.sleep(0.3)
    tap(24, 24)
    wait_for("app 'Music' exited", 20)
    print("music interaction done")

    # 交互抽查：browser 书签
    swipe(400, 500, 120, 500)
    time.sleep(0.8)
    tap(410, 228)  # browser (page0 slot5)
    wait_for("[browser] ready", 20)
    time.sleep(0.5)
    tap(60, 148)  # 第一个书签「主页」
    time.sleep(0.6)
    shot("07_browser_page.ppm")
    tap(24, 24)
    wait_for("app 'Browser' exited", 20)
    print("browser interaction done")

    time.sleep(1.0)
    print("=== RESULTS ===")
    fails = 0
    for name, ok in results:
        print("  %-10s %s" % (name, "PASS" if ok else "FAIL"))
        if not ok:
            fails += 1
    print("total=%d fail=%d" % (len(results), fails))
    print("=== LOG TAIL ===")
    print(log.decode(errors="replace")[-4000:])
finally:
    stop = True
    p.terminate()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
    logf.close()
