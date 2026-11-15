#!/usr/bin/env python3
"""KunxOS 全应用回归测试：启动 → OOBE → 桌面 → 逐一打开可用应用并返回。

用法：python3 tests/scripts/apps_regression.py [logfile] [shotsdir]
"""
from qemu_test import Guest, kernel_path, temp_path, qemu_command, monitor_address, monitor_option, launcher_layout, screenshot, monitor_command
import sys, time, os, tempfile

import argparse

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("logfile", nargs="?", default=temp_path("kx_apps.log"))
parser.add_argument("shotsdir", nargs="?", default=temp_path("kx_apps_shots"))
parser.add_argument("--only", nargs="+", help="only check these application keys")
args = parser.parse_args()

KERNEL = kernel_path()
LOGFILE = args.logfile
SHOTS = args.shotsdir
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxa_")
MON = monitor_address()

FRAME_DOWN, FRAME_UP, FRAME_MOVE = 0x01, 0x00, 0x02

def frame(kind, x=0, y=0):
    return bytes([0xAA, kind, x & 0xFF, (x >> 8) & 0xFF, y & 0xFF, (y >> 8) & 0xFF])


cmd = qemu_command() + [
       "-kernel", KERNEL, "-serial", "stdio",
       "-device", "ramfb", "-no-reboot", "-display", "none",
       "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
       "-device", "virtio-keyboard-device", "-device", "virtio-tablet-device",
       "-device", "virtio-rng-device", "-audiodev", "none,id=aud0", "-device", "virtio-sound-device,audiodev=aud0",
       "-monitor", monitor_option(MON)]

guest = Guest(cmd, LOGFILE, cwd=WORKDIR)
p = guest.process
log = guest.log
results = []


def send_frame(fr):
    guest.send(fr)

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

def send_esc():
    monitor_command(MON, "sendkey esc")

def shot(name):
    screenshot(MON, SHOTS, name)

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
    # 启动等待加裕量：宿主机负载高（开发机同时跑 IDE/浏览器）时 QEMU 启动
    # 明显变慢，60s 曾误判"未启动到 OOBE"。
    # rootfs 可能预置 /data/oobe/.done（跳过 OOBE 直接进桌面），也可能需要
    # 完整走 13 步 OOBE —— 两者都接受，谁先出现走谁。
    if not wait_for("[launcher] 桌面就绪", 30):
        if not wait_for("[oobe] 欢迎使用 KunxOS", 150):
            print("FAIL: kernel did not boot to OOBE or desktop")
            sys.exit(1)
        time.sleep(0.5)
        # OOBE 13 步：普通步点「下一步」快进；密码/胁迫 PIN 步拒绝空输入
        # （"密码太短"），须停手 35s 让其「30 秒无输入自动下一步」通过。
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
            print("FAIL: no desktop after OOBE")
            sys.exit(1)
    time.sleep(1.5)
    # 锁屏：仅当 OOBE 配置了安全方式时出现（默认 rootfs 无锁屏，短等即可）。
    # 数字键盘中心：1=(180,589) 2=(270,589) 3=(360,589) 4=(180,661)。
    if wait_for("[lock] 开机即锁屏", 5):
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

    # ==================== 布局解析 ====================
    # launcher 启动时打印 [launcher] tile/dock 行（key + 点击中心，见
    # dump_layout）。模块过滤（OOBE 步骤可关模块，如 sms）与网格列数变化
    # 会让固定坐标失效，因此按真实坐标点击；被过滤的应用记 SKIP。
    wait_for("[launcher] 已加载", 20)
    tiles, docks, folders, folder_apps = launcher_layout(log)
    if not tiles and not docks:
        print("FAIL: no [launcher] tile/dock layout lines")
        sys.exit(1)
    print("[test] layout: dock=%s tiles=%d" % (sorted(docks), len(tiles)))

    # 当前页跟踪（launcher 无页面日志，脚本按滑动方向自行推算）
    state = {"page": 0}

    def goto_page(p):
        # 注入帧在内核风暴窗口可能丢字节（滑动 MOVE 帧丢失 → 手势判定为
        # 原地长按 → 翻页不生效）：按 launcher 的页码结算日志确认，未生效
        # 则重试（最多 4 次），不再盲目递增本地页码状态。
        for _ in range(4):
            if state["page"] == p:
                return
            before = len(log)
            if p > state["page"]:
                swipe(440, 500, 100, 500)   # 左滑 → 下一页
            else:
                swipe(100, 500, 440, 500)   # 右滑 → 上一页
            deadline = time.time() + 3.0
            while time.time() < deadline:
                seg = log[before:].decode(errors="replace")
                if ("[launcher] page=%d" % p) in seg:
                    state["page"] = p
                    break
            time.sleep(0.3)

    def app_coords(key):
        """dock 优先，其次网格瓦片；返回 (x, y, page) 或 None（被过滤）。"""
        if key in docks:
            return (docks[key][0], docks[key][1], state["page"])
        if key in tiles:
            pg, cx, cy = tiles[key]
            return (cx, cy, pg)
        if key in folder_apps and "tools" in folders:
            return (*folder_apps[key], folders["tools"][0])
        return None

    def home_gesture():
        # 全面屏手势：底部快速上滑 = 回桌面（默认导航模式；OOBE 快进保持手势）
        swipe(270, 935, 270, 620)

    def open_and_exit(name, disp, marker, terminal=False, shotname=None):
        c = app_coords(name)
        if c is None:
            print("%s: SKIP (filtered by module config)" % name)
            return
        x, y, pg = c
        if name not in docks:
            goto_page(pg)
        if name in folder_apps:
            pg, fx, fy = folders["tools"]
            goto_page(pg)
            before = len(log)
            tap(fx, fy)
            if not guest.wait_for("[launcher] 已打开文件夹", 10, before):
                results.append((name, False))
                return
            time.sleep(1)
        start = len(log)
        tap(x, y)
        ok = guest.wait_for(marker, 30, start)
        ok = ok and guest.wait_for("启动过渡：", 30, start)
        time.sleep(1)  # Do not send Back while launcher is still animating the launch.
        shot(shotname or ("01_%s.ppm" % name))
        # 返回语义：先点标题栏返回 (24,24)（launcher 打印「应用 X 已退出，
        # 退出码=0」，任何应用都生效）；超时（browser 等应用内导航不退）
        # 才回退到底部上滑手势（Home → 应用退出，且桌面回第 0 页）。
        if not terminal:
            tap(24, 24)
            time.sleep(0.5)
        else:
            send_esc()
        back = guest.wait_for("应用 '%s' 已退出" % disp, 10, start)
        if not back and not terminal:
            home_gesture()
            back = guest.wait_for("应用 '%s' 已退出" % disp, 10, start)
            if back:
                state["page"] = 0
        time.sleep(2.5)
        results.append((name, ok and back))
        print("%s: ready=%s back=%s" % (name, ok, back))

    # ==================== 全应用巡检 ====================
    apps = [
        ("phone",    "Phone",    "[phone] 就绪"),
        ("sms",      "SMS",      "[sms] 就绪"),
        ("browser",  "Browser",  "[browser] 就绪"),
        ("camera",   "Camera",   "[camera] 就绪"),
        ("settings", "Settings", "[settings] 就绪"),
        ("contacts", "Contacts", "[contacts] 就绪"),
        ("calendar", "Calendar", "[calendar] 就绪"),
        ("gallery",  "Gallery",  "[gallery] 就绪"),
        ("music",    "Music",    "[music] 就绪"),
        ("weather",  "Weather",  "[weather] 就绪"),
        ("map",      "Maps",     "[map] 就绪"),
        ("store",    "Store",    "[store] 就绪"),
        ("video",    "Video",    "[video] 就绪"),
        ("notes",    "Notes",    "[notes] 就绪"),
        ("files",    "Files",    "[files] 就绪"),
        ("clock",    "Clock",    "[clock] 就绪"),
        ("cloud",    "Cloud",    "[cloud] 已就绪"),
        ("mail",     "Mail",     "[mail] 就绪"),
    ]
    for name, disp, marker in apps:
        if args.only and name not in args.only:
            continue
        open_and_exit(name, disp, marker, terminal=(name == "terminal"))

    if args.only:
        assert results, "No selected application was available"
        sys.exit(0 if all(ok for _, ok in results) else 1)

    # ==================== 交互抽查 ====================
    # phone 拨号 + 呼叫
    c = app_coords("phone")
    if c is not None:
        tap(c[0], c[1])
        wait_for("[phone] 就绪", 20)
        time.sleep(0.6)
        tap(204, 570)  # 数字 5
        time.sleep(0.3)
        tap(204, 570)
        time.sleep(0.3)
        tap(424, 830)  # 呼叫
        time.sleep(0.5)
        shot("04_phone_calling.ppm")
        tap(424, 830)  # 挂断
        time.sleep(0.4)
        tap(24, 24)
        if not wait_for("应用 'Phone' 已退出", 10):
            home_gesture()
            wait_for("应用 'Phone' 已退出", 10)
            state["page"] = 0
        print("phone interaction done")

    # calendar 翻月
    c = app_coords("calendar")
    if c is not None:
        goto_page(c[2])
        tap(c[0], c[1])
        wait_for("[calendar] 就绪", 20)
        time.sleep(0.5)
        tap(452, 82)  # 下月
        time.sleep(0.4)
        shot("05_calendar_next_month.ppm")
        tap(24, 24)
        if not wait_for("应用 'Calendar' 已退出", 10):
            home_gesture()
            wait_for("应用 'Calendar' 已退出", 10)
            state["page"] = 0
        print("calendar interaction done")

    # music 播放/暂停
    c = app_coords("music")
    if c is not None:
        goto_page(c[2])
        tap(c[0], c[1])
        wait_for("[music] 就绪", 20)
        time.sleep(0.5)
        tap(288, 608)  # 播放/暂停
        time.sleep(1.2)
        shot("06_music_playing.ppm")
        tap(288, 608)
        time.sleep(0.3)
        tap(24, 24)
        if not wait_for("应用 'Music' 已退出", 10):
            home_gesture()
            wait_for("应用 'Music' 已退出", 10)
            state["page"] = 0
        print("music interaction done")

    # browser 书签（(24,24) 是应用内返回，退出走手势）
    c = app_coords("browser")
    if c is not None:
        tap(c[0], c[1])
        wait_for("[browser] 就绪", 20)
        time.sleep(0.5)
        tap(60, 148)  # 第一个书签「主页」
        time.sleep(0.6)
        shot("07_browser_page.ppm")
        home_gesture()
        wait_for("应用 'Browser' 已退出", 8)
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
    sys.exit(1 if fails else 0)
finally:
    guest.close()
