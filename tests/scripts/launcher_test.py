#!/usr/bin/env python3
"""KunxOS launcher 转场测试：跳过 OOBE（允许其崩溃），直接等 launcher 桌面，
点击第一个应用图标，采集启动/返回转场的 [motion] 帧统计。

用法：python3 tests/scripts/launcher_test.py <logfile>
"""
import subprocess, sys, time, threading, os, tempfile

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_launcher.log"
WORKDIR = tempfile.mkdtemp(prefix="kxl_")
PIPE = os.path.join(WORKDIR, "kx")

FRAME_DOWN = 0x01
FRAME_UP = 0x00
FRAME_MOVE = 0x02

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

def tap_swipe_up():
    # 底部上滑回桌面：从 (270,935) 按下 → 上移到 (270,300) → 抬起。
    global infd
    if infd < 0:
        return
    try:
        os.write(infd, frame(FRAME_DOWN, 270, 935))
        time.sleep(0.30)
        for y in range(935, 300, -80):
            os.write(infd, frame(FRAME_MOVE, 270, y))
            time.sleep(0.04)
        os.write(infd, frame(FRAME_UP, 0, 0))
        time.sleep(0.2)
    except OSError:
        pass

def tap(x, y, hold=0.02):
    if infd < 0:
        return
    try:
        os.write(infd, frame(FRAME_DOWN, x, y))
        time.sleep(hold)
        os.write(infd, frame(FRAME_UP, 0, 0))
    except OSError:
        pass

def wait_for(marker, timeout, from_off=None):
    # from_off：只搜索该偏移之后的新增内容（防陈旧标记误匹配）
    deadline = time.time() + timeout
    while time.time() < deadline:
        hay = bytes(log[from_off:]) if from_off is not None else bytes(log)
        if marker.encode() in hay:
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
    # 全新镜像 /data/oobe 为空（Makefile 保证首启进 OOBE）。OOBE 共 13 步，
    # 密码/胁迫 PIN 步会拒绝空输入（"密码太短"），只能靠其 30 秒无输入自动
    # 步进通过 —— 因此持续点「下一步」并给足 ~8 分钟超时。
    if not wait_for("[launcher] 桌面就绪", 30):
        if not wait_for("[oobe] 欢迎使用 KunxOS", 60):
            print("FAIL: neither OOBE nor desktop appeared")
            sys.exit(1)
        print("[test] OOBE running, fast-forwarding (password steps need idle auto-advance)")
        deadline = time.time() + 600
        idle_until = 0.0
        while time.time() < deadline:
            if "[launcher] 桌面就绪".encode() in bytes(log):
                break
            now = time.time()
            # 密码/胁迫 PIN 步拒绝空输入（"密码太短"）：停手 35s，让 OOBE 的
            # 「30 秒无输入自动下一步」通过（持续点击会重置其空闲计时器）。
            if now < idle_until:
                time.sleep(1.0)
                continue
            if "密码太短".encode() in bytes(log)[-4000:]:
                idle_until = now + 35
                continue
            tap(465, 925)
            time.sleep(0.8)
        if not wait_for("[launcher] 桌面就绪", 60):
            print("FAIL: launcher 桌面未就绪")
            sys.exit(1)
    print("[test] launcher 桌面就绪")
    time.sleep(1.5)
    # 点击第一个应用图标（网格第一格图标中心；4 列 grid_x=122，GRID_Y=154，
    # 图标 56x56 → 中心 (150,182)。旧坐标 (130,100) 落在状态栏点不到图标）。
    tap(150, 182)
    print("[test] tapped first icon (150,182)")
    # 采集启动转场统计 + 应用拉起/失败 + 返回转场统计。
    # ★ 2026-09-18：生命周期设计为「应用前台期间 launcher 不读输入（应用
    # 独占 fb0 与输入设备）」——此前的「第二次点击看 launcher [input]」
    # 断言只在应用启动失败（gate 写失败退化）时才会通过。改为真实流程：
    # 等应用就绪 → 点应用内返回键 (24,24) 退出 → 验证 launcher 恢复输入。
    wait_for("[launcher] 启动 ", 20)
    print("[test] transitions captured")
    anchor = len(log)
    app_ready = wait_for("] 就绪", 25, from_off=anchor)
    if not app_ready:
        print("[test] app did NOT become ready -> launch broken")
    else:
        print("[test] app ready; tapping in-app back (24,24) to exit")
        time.sleep(1)
        tap(24, 24)
        anchor2 = len(log)
        exited = wait_for("正在退出", 30, from_off=anchor2)
        if not exited:
            # clock 的返回键在标题栏；不响应则用底部上滑手势回桌面。
            print("[test] back tap no exit; trying bottom-swipe home")
            tap_swipe_up()
            exited = wait_for("正在退出", 20, from_off=anchor2)
        print("[test] app exited" if exited else "[test] app STILL foreground")
    # 决定性验证：回桌面后点空白处，launcher 应打印 [input]（冻结则无输出）。
    time.sleep(4)
    anchor3 = len(log)
    tap(400, 500)
    print("[test] post-exit tap sent (400,500)")
    time.sleep(3)
    if "[input] 按下（400,500)".encode() in bytes(log[anchor3:]):
        print("[test] POST-EXIT TAP PROCESSED -> launcher alive")
    else:
        print("[test] post-exit tap NOT processed -> launcher frozen (hang)")
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
