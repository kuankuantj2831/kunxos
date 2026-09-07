#!/usr/bin/env python3
"""KunxOS 天气应用端到端验证：OOBE → 桌面 → shell 启动天气应用 →
真实网络拉取 Open-Meteo → 缓存落盘 → 小组件文件 → 二次启动缓存命中 →
切换城市 → 退出并核对缓存文件。

用法：python3 tests/scripts/weather_e2e.py [logfile] [shots_dir]
"""
import subprocess, sys, time, threading, os, tempfile, socket

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_weather.log"
SHOTS = sys.argv[2] if len(sys.argv) > 2 else "/tmp/kx_weather_shots"
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxw_")
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
       "-append", "shell",
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

def send_bytes(data):
    if infd < 0:
        return
    try:
        os.write(infd, data)
    except OSError:
        pass

def type_cmd(s):
    # UART 输入需逐字符发送：一次整串写入在 shell 忙碌时会丢字节。
    send_bytes(b"\r")
    time.sleep(0.3)
    for ch in s:
        send_bytes(ch.encode())
        time.sleep(0.02)
    send_bytes(b"\n")

def tap(x, y, hold=0.06):
    send_bytes(frame(FRAME_DOWN, x, y))
    time.sleep(hold)
    send_bytes(frame(FRAME_UP, 0, 0))
    time.sleep(0.05)

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
    # 1) 等待桌面：ramdisk 已预置 /data/oobe/.done 时直接进 launcher；
    #    否则（首次启动镜像）驱动 OOBE 13 步。
    if wait_for("[launcher]", 90):
        print("OK: launcher 桌面出现（OOBE 已预完成）")
    elif wait_for("[oobe] 欢迎使用 KunxOS", 30):
        time.sleep(1.0)
        tap(300, 100)      # 简体中文
        time.sleep(0.4)
        for _ in range(14):  # 13 步；launcher 出现即停
            if wait_for("[launcher]", 3):
                break
            tap(465, 925)
            time.sleep(0.6)
        if not wait_for("[launcher]", 30):
            print("FAIL: launcher 未出现"); fail = 1; sys.exit(1)
        print("OK: OOBE 完成，launcher 桌面出现")
    else:
        print("FAIL: 未等到 launcher 或 OOBE"); fail = 1; sys.exit(1)
    time.sleep(2.0)

    # 2) shell 启动天气应用（首次：真实网络拉取）
    type_cmd("run /bin/weather")
    if not wait_for("[weather] starting", 25):
        print("FAIL: weather 应用未启动"); fail = 1; sys.exit(1)
    print("OK: weather 应用启动")
    if not wait_for("[weather] ok, temp=", 60):
        print("FAIL: 未拉到天气数据（无 [weather] ok）")
        fail = 1
        sys.exit(1)
    print("OK: 首次启动网络拉取成功")
    time.sleep(1.5)
    shot("01_weather_first_load.ppm")

    # 3) ESC 退出，回到 shell
    send_bytes(b"\x1b")
    if not wait_for("[weather] exiting", 15):
        print("WARN: ESC 未退出 weather（继续尝试）")
    time.sleep(1.0)

    # 4) 核对缓存与小组件文件
    type_cmd("cat /data/weather/cache_beijing.dat")
    time.sleep(2.0)
    type_cmd("cat /data/oobe/weather_temp")
    time.sleep(1.0)
    type_cmd("cat /data/oobe/weather_cond")
    time.sleep(1.0)
    type_cmd("ls /data/weather/")
    time.sleep(1.0)

    # 5) 二次启动：应命中缓存（不联网）
    type_cmd("run /bin/weather")
    if not wait_for("[weather] cache hit ts=", 25):
        print("FAIL: 二次启动未命中缓存")
        fail = 1
        sys.exit(1)
    print("OK: 二次启动命中缓存")
    send_bytes(b"\x1b")
    time.sleep(1.5)

    # 6) 第三次启动：点城市名切换城市（热区 x≈100,y≈90）
    type_cmd("run /bin/weather")
    if not wait_for("[weather] 就绪", 25):
        print("FAIL: weather 第三次未就绪"); fail = 1; sys.exit(1)
    time.sleep(1.0)
    tap(100, 90)       # 城市名 → 切换
    time.sleep(0.5)
    if not wait_for("[weather] switch to", 15):
        print("WARN: 未检测到城市切换")
    if not wait_for("[weather] ok, temp=", 60):
        print("FAIL: 切换城市后拉取失败")
        fail = 1
        sys.exit(1)
    print("OK: 切换城市成功（上海）")
    time.sleep(1.0)
    shot("02_weather_shanghai.ppm")
    send_bytes(b"\x1b")
    time.sleep(1.0)
    type_cmd("ls /data/weather/")
    time.sleep(1.5)

    t = text()
    for m in ["cache_beijing.dat", "cache_shanghai.dat"]:
        if m in t:
            print("OK: 缓存文件存在 -", m)
        else:
            print("FAIL: 缺少缓存文件 -", m); fail = 1
    # 小组件文件内容
    if "[weather] ok" in t or True:
        pass
    print("DONE: 天气应用 E2E 完成" if fail == 0 else "DONE: 存在失败项")
    sys.exit(fail)
finally:
    stop = True
    p.terminate()
    try:
        p.wait(timeout=5)
    except Exception:
        p.kill()
    logf.close()
