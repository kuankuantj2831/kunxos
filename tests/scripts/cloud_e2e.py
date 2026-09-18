#!/usr/bin/env python3
"""KunxOS 云服务端到端测试：OOBE → 桌面 → 云服务应用 → 注册/备份 → 服务器验证
→ 下发 lock/wipe → OS 执行并确认。

前置：
  - 本机运行网关（本地模式，端口 18080）：
      python3 cloud/gateway/gateway.py --local --port 18080 --data /tmp/kxcloud_e2e --admin-key admin
  - 已 make kernel（内核内嵌含 /bin/cloud 的 ramdisk）

用法：python3 tests/scripts/cloud_e2e.py [logfile] [shotsdir]
"""
import subprocess, sys, time, threading, os, tempfile, json, urllib.request

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_cloud.log"
SHOTS = sys.argv[2] if len(sys.argv) > 2 else "/tmp/kx_cloud_shots"
GW = "http://127.0.0.1:8080"
GW_KEY = os.environ.get("CLOUD_ADMIN_KEY", "admin")
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxc_")
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
       "-device", "virtio-rng-device",
       # 注意：不加 -device AC97 —— QEMU 在 -display none 下 AC97 DMA 不完成、
       # 内核 play_tone 的 deadline 轮询不退出导致启动挂起（音频与云服务无关）。
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
    send_frame(frame(FRAME_MOVE, x, y))
    time.sleep(0.03)
    send_frame(frame(FRAME_DOWN, x, y))
    time.sleep(hold)
    send_frame(frame(FRAME_UP, x, y))
    time.sleep(0.05)

def swipe(x1, y1, x2, y2, steps=10, step_ms=0.012):
    send_frame(frame(FRAME_DOWN, x1, y1))
    time.sleep(0.03)
    for i in range(1, steps + 1):
        x = int(round(x1 + (x2 - x1) * i / float(steps)))
        y = int(round(y1 + (y2 - y1) * i / float(steps)))
        send_frame(frame(FRAME_MOVE, x, y))
        time.sleep(step_ms)
    time.sleep(0.02)
    send_frame(frame(FRAME_UP))
    time.sleep(0.1)

def shot(name):
    try:
        s = socket_create()
        s.connect(MON)
        s.sendall(b'screendump ' + (os.path.join(SHOTS, name) + '\n').encode())
        time.sleep(0.3)
        s.close()
    except Exception:
        pass

def socket_create():
    import socket
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(3)
    return s

def wait_for(text, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if text.encode() in log:
            return True
        if p.poll() is not None:
            return False
        time.sleep(0.2)
    return False

def gw_get(path):
    with urllib.request.urlopen(GW + path, timeout=5) as r:
        return json.loads(r.read().decode())

def gw_post(path, data):
    req = urllib.request.Request(GW + path, data=json.dumps(data).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=5) as r:
        return json.loads(r.read().decode())

def check(cond, msg):
    results.append((msg, cond))
    print(("%s: %s" % ("PASS" if cond else "FAIL", msg)))
    if not cond:
        sys.exit(1)

try:
    if not wait_for("[oobe] 欢迎使用 KunxOS", 90):
        print("FAIL: kernel did not boot to OOBE")
        sys.exit(1)
    time.sleep(0.5)
    # 快进 OOBE（10 步，前 9 步点下一步）
    for i in range(9):
        tap(465, 925)
        time.sleep(0.35)
    if not wait_for("[launcher] 桌面就绪", 90):
        print("FAIL: no desktop after OOBE")
        sys.exit(1)
    time.sleep(1.5)
    # 锁屏：OOBE 配置了安全方式（默认 PIN=1234），launcher 启动即上锁，
    # 需先解锁才能点击云服务图标（与 apps_regression.py 的处理一致）。
    if wait_for("[lock] 开机即锁屏", 10):
        tap(180, 589)   # 1
        time.sleep(0.3)
        tap(270, 589)   # 2
        time.sleep(0.3)
        tap(360, 589)   # 3
        time.sleep(0.3)
        tap(180, 661)   # 4（PIN 输满 4 位自动校验）
        if not wait_for("[lock] 已解锁", 10):
            print("FAIL: could not unlock PIN screen")
            sys.exit(1)
        time.sleep(0.8)
    shot("00_desktop_page0.ppm")

    # 第 1 页：云服务在「系统工具」文件夹内（launcher 硬编码布局：
    # FOLDER_KEYS = calculator/clock/files/terminal/cloud/mail，3 列网格，
    # 面板 396x500 居中 → cloud 位于第 2 行中列，中心 (270,378)；
    # 文件夹图标在页面网格 (0,0)，中心 ≈ (150,272)）。
    swipe(400, 500, 120, 500)
    time.sleep(1.0)
    shot("01_desktop_page1.ppm")
    tap(150, 178)              # 打开「系统工具」文件夹（第二页网格 (0,0)）
    if not wait_for("[launcher] 已打开文件夹", 10):
        print("FAIL: could not open system tools folder")
        sys.exit(1)
    time.sleep(0.8)
    tap(206, 378)              # 文件夹内 cloud 图标（"calculator" 键不匹配被跳过，
                               #  实际内容 clock/files/terminal/cloud/mail → cloud 在第 2 行左列）
    if not wait_for("[cloud] 已就绪", 30):
        print("FAIL: cloud app did not start")
        sys.exit(1)
    time.sleep(0.8)
    shot("02_cloud_main.ppm")

    # 立即同步（注册 + 备份）→ 按钮中心 (150, 503)
    tap(150, 503)
    ok_reg = wait_for("[cloud] 注册成功", 40)
    ok_bak = wait_for("[cloud] 备份成功", 40)
    check(ok_reg, "device registered (serial)")
    check(ok_bak, "backup uploaded (serial)")
    time.sleep(0.8)
    shot("03_cloud_synced.ppm")

    # 服务器侧验证
    devs = gw_get("/api/admin/devices?key=%s" % GW_KEY)
    check(devs.get("ok") and len(devs.get("devices", [])) >= 1, "admin sees >=1 device")
    dev = devs["devices"][0]
    print("  device:", dev)
    bkp = gw_get("/api/admin/backup?key=%s&device_id=%s" % (GW_KEY, dev["device_id"]))
    files = bkp.get("files", [])
    check(len(files) >= 5, "admin sees >=5 backed-up files (got %d)" % len(files))
    print("  backup files:", [f["path"] for f in files])

    # 下发 lock → 检查命令 → 锁定界面
    r = gw_post("/api/admin/command", {"key": GW_KEY, "device_id": dev["device_id"],
                                       "type": "lock", "payload": "e2e lock test"})
    check(r.get("ok"), "admin sent lock command")
    tap(390, 503)  # 检查命令
    ok_lock = wait_for("type=lock", 40)
    check(ok_lock, "device received lock command")
    time.sleep(0.8)
    shot("04_cloud_locked.ppm")

    # 解锁：真实长按语义 —— 短按不解锁（负向断言），按住解锁按钮 ≥2s 才解锁。
    tap(270, 492)  # 短按解锁按钮
    time.sleep(1.0)
    check("长按解锁成功".encode() not in log, "short tap does NOT unlock")
    send_frame(frame(FRAME_MOVE, 270, 492))
    time.sleep(0.03)
    send_frame(frame(FRAME_DOWN, 270, 492))
    time.sleep(2.4)  # 按住 2.4s（阈值 2s）
    send_frame(frame(FRAME_UP))
    ok_unlock = wait_for("[cloud] 长按解锁成功", 10)
    check(ok_unlock, "long-press (hold >=2s) unlocks")
    time.sleep(0.6)
    shot("05_cloud_unlocked.ppm")

    # 下发 wipe → 检查命令 → 数据擦除
    r = gw_post("/api/admin/command", {"key": GW_KEY, "device_id": dev["device_id"],
                                       "type": "wipe", "payload": ""})
    check(r.get("ok"), "admin sent wipe command")
    tap(390, 503)
    ok_wipe = wait_for("type=wipe", 40)
    check(ok_wipe, "device received wipe command")
    ok_ack = wait_for("[cloud] wipe：已擦除", 30)
    check(ok_ack, "wipe executed on device")
    time.sleep(0.8)
    shot("06_cloud_wiped.ppm")

    print("\n=== E2E CLOUD TEST %s ===" % ("ALL PASS" if all(c for _, c in results) else "FAILED"))
finally:
    stop = True
    logf.close()
    try:
        p.terminate()
    except Exception:
        pass
    sys.exit(0 if all(c for _, c in results) else 1)
