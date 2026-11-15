#!/usr/bin/env python3
"""KunxOS 应用一致性修复最小验证（cloud 长按解锁 / clock+notes 子页返回 /
store 缺签名拒绝安装）。

一台 QEMU 全流程验证（无串口 shell：其行读取会吞注入帧；store 配置预置于测试镜像）：
  1. clock：闹钟编辑子页 → 标题栏返回 / 边缘返回手势 均回列表而非退出。
  2. notes：编辑器 → 边缘返回手势 / ESC 回列表而非退出。
  3. store：本地 mock S3（Packages.json 200、Packages.json.sig 404）→
     索引可浏览但点击安装被明确拒绝，且不发起 .kunx 下载。
  4. cloud：mock 云网关（register/backup/poll/ack）→ poll 下发 lock 命令 →
     cloud 进入锁定覆盖层 → 短按不解锁（负向）→ 按住 ≥2s 解锁。

说明：
  - clock/cloud 通过 launcher 的 folder / folder-app 布局日志定位。
  - 不直接写 /data/oobe/locked：launcher 每秒轮询该文件会触发系统锁屏，
    干扰测试；锁定态改由真实路径（poll 下发 lock 命令）产生。
  - 本脚本需带 inode 余量的内核镜像（SFS 256 inode 上限在默认 rootfs 下
    开机即耗尽，导致 launcher 无法写启动门控文件）：用裁剪 rootfs 构建，
    见任务报告。

前置：make（内核内嵌含全部修复的 ramdisk）。
用法：python3 tests/scripts/app_fixes_verify.py [logfile] [shotsdir]
"""
from qemu_test import Guest, kernel_path, temp_path, qemu_command, monitor_address, monitor_option, launcher_layout, screenshot, monitor_command
import http.server, json, os, sys, tempfile, threading, time

KERNEL = kernel_path()
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else temp_path("kx_appfix.log")
SHOTS = sys.argv[2] if len(sys.argv) > 2 else temp_path("kx_appfix_shots")
MOCK_PORT = int(os.environ.get("MOCK_PORT", "18085"))
CLOUD_GW_PORT = int(os.environ.get("CLOUD_GW_PORT", "8080"))
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxaf_")
MON = monitor_address()

FRAME_DOWN, FRAME_UP, FRAME_MOVE = 0x01, 0x00, 0x02

def frame(kind, x=0, y=0):
    return bytes([0xAA, kind, x & 0xFF, (x >> 8) & 0xFF, y & 0xFF, (y >> 8) & 0xFF])


cmd = qemu_command() + [
       "-kernel", KERNEL, "-serial", "stdio",
       "-device", "ramfb", "-no-reboot", "-display", "none",
       "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
       "-device", "virtio-keyboard-device", "-device", "virtio-tablet-device",
       "-device", "virtio-rng-device",
       "-monitor", monitor_option(MON)]

guest = Guest(cmd, LOGFILE, cwd=WORKDIR)
p = guest.process
log = guest.log
results = []


def send_frame(fr):
    guest.send(fr)

def tap(x, y, hold=0.06):
    send_frame(frame(FRAME_MOVE, x, y))
    time.sleep(0.03)
    send_frame(frame(FRAME_DOWN, x, y))
    time.sleep(hold)
    send_frame(frame(FRAME_UP, x, y))
    time.sleep(0.08)

def hold_press(x, y, secs):
    send_frame(frame(FRAME_MOVE, x, y))
    time.sleep(0.03)
    send_frame(frame(FRAME_DOWN, x, y))
    time.sleep(secs)
    send_frame(frame(FRAME_UP, x, y))
    time.sleep(0.1)

def swipe(x1, y1, x2, y2, steps=10, step_ms=0.012):
    send_frame(frame(FRAME_DOWN, x1, y1))
    time.sleep(0.03)
    for i in range(1, steps + 1):
        tt = i / float(steps)
        x = int(round(x1 + (x2 - x1) * tt))
        y = int(round(y1 + (y2 - y1) * tt))
        send_frame(frame(FRAME_MOVE, x, y))
        time.sleep(step_ms)
    time.sleep(0.02)
    send_frame(frame(FRAME_UP))
    time.sleep(0.15)

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

def count_of(text):
    return log.count(text.encode())

def shell_cmd(line, settle=0.5):
    """串口 shell 执行一行命令（bootargs `shell` 拉起的内置 shell）。
    前置换行：IRQ storm 期间 UART RX 可能丢首字符，先发一个空行唤醒
    行缓冲，再发命令，丢的只是空行。"""
    guest.send(("\n" + line + "\n").encode())
    time.sleep(settle)

def tap_until(taps, marker, tries=10, gap=1.0):
    """依次重发 taps（坐标列表）直到 marker 出现在串口日志。
    storm 窗口注入帧会整帧丢失，固定 sleep 的单次点击不可靠。"""
    for i in range(tries):
        before = len(log)
        for (x, y) in taps:
            tap(x, y)
            time.sleep(0.35)
        deadline = time.time() + 2.5
        while time.time() < deadline:
            if marker.encode() in log[before:]:
                return True
            time.sleep(0.1)
    return marker.encode() in log

def hold_until(x, y, secs, marker, tries=6):
    """重试长按直到 marker 出现（按住时长 ≥ 阈值才可能触发）。"""
    for i in range(tries):
        before = len(log)
        hold_press(x, y, secs)
        deadline = time.time() + 3.0
        while time.time() < deadline:
            if marker.encode() in log[before:]:
                return True
            time.sleep(0.1)
    return marker.encode() in log

def check(cond, msg):
    results.append((msg, bool(cond)))
    print("%s: %s" % ("PASS" if cond else "FAIL", msg))

def home_gesture():
    swipe(270, 935, 270, 620)

# ==================== mock 服务器（S3 + 云网关） =====================

IDX_JSON = json.dumps({
    "revision": 1, "updated": 1755300000,
    "packages": {
        "notepad": {
            "version": "9.9.9", "description": "unsigned index test",
            "author": "t", "category": "tools",
            "filename": "packages/notepad-9.9.9.kunx", "size": 1024,
            "min_api": 1, "permissions": [],
        },
    },
}).encode()

mock_requests = []
cloud_state = {"lock_armed": False}

class MockHandler(http.server.BaseHTTPRequestHandler):
    def _reply(self, code, body=b""):
        self.send_response(code)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        mock_requests.append(self.path)
        if self.path == "/api/register" or self.path == "/api/backup" or self.path == "/api/ack":
            self._reply(200, b"OK\n")
        elif self.path == "/api/poll":
            if cloud_state["lock_armed"]:
                self._reply(200, b"OK\nC|1|lock|verify\n")
            else:
                self._reply(200, b"OK\n")
        else:
            self._reply(404)

    def do_GET(self):
        mock_requests.append(self.path)
        if self.path == "/test/Packages.json":
            self._reply(200, IDX_JSON)
        else:
            # Packages.json.sig 及一切 .kunx 对象 → 404（签名缺失场景）
            self._reply(404)

    def log_message(self, *a):
        pass

mock_srv = http.server.ThreadingHTTPServer(("127.0.0.1", MOCK_PORT), MockHandler)
threading.Thread(target=mock_srv.serve_forever, daemon=True).start()
mock_gw = http.server.ThreadingHTTPServer(("127.0.0.1", CLOUD_GW_PORT), MockHandler)
threading.Thread(target=mock_gw.serve_forever, daemon=True).start()

try:
    # ==================== 启动 / OOBE 快进 ====================
    if not wait_for("[launcher] 桌面就绪", 30):
        if not wait_for("[oobe] 欢迎使用 KunxOS", 150):
            print("FAIL: kernel did not boot to OOBE or desktop")
            sys.exit(1)
        time.sleep(0.5)
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
    if wait_for("[lock] 开机即锁屏", 5):
        tap(180, 589); time.sleep(0.3)
        tap(270, 589); time.sleep(0.3)
        tap(360, 589); time.sleep(0.3)
        tap(180, 661)
        if not wait_for("[lock] 已解锁", 10):
            print("FAIL: could not unlock PIN screen")
            sys.exit(1)
        time.sleep(0.8)
    shot("00_desktop.ppm")

    # 配置说明：无串口 shell（其行读取会吞掉注入的 0xAA 触摸帧）——
    # store 的 /data/store/config 已预置进测试镜像（KERNEL 指向裁剪构建），
    # cloud 默认服务器 10.0.2.2:8080 = mock 云网关（OOBE 默认写入）。

    # 布局解析（与 apps_regression 相同：按 launcher 真实坐标点击）
    wait_for("[launcher] 已加载", 20)
    tiles, docks, folders, folder_apps = launcher_layout(log)
    state = {"page": 0}

    def goto_page(pg):
        for _ in range(4):
            if state["page"] == pg:
                return
            before = len(log)
            if pg > state["page"]:
                swipe(440, 500, 100, 500)
            else:
                swipe(100, 500, 440, 500)
            deadline = time.time() + 3.0
            while time.time() < deadline:
                seg = log[before:].decode(errors="replace")
                if ("[launcher] page=%d" % pg) in seg:
                    state["page"] = pg
                    break
            time.sleep(0.3)

    def dismiss_foreground():
        """回桌面（底部上滑 Home）：无论当前是否有前台应用都安全。
        不在此处发 shell 命令——前台应用的 poll_tty_esc 也在读 UART，
        会与 shell 抢字节导致命令被吞。"""
        home_gesture()
        time.sleep(1.5)

    def open_folder_app(key, marker):
        dismiss_foreground()
        if "tools" not in folders or key not in folder_apps:
            check(False, f"missing folder layout for {key}")
            return False
        pg, fx, fy = folders["tools"]
        goto_page(pg)
        start = len(log)
        tap(fx, fy)
        if not guest.wait_for("[launcher] 已打开文件夹", 10, start):
            check(False, "system tools folder did not open")
            return False
        time.sleep(1)
        start = len(log)
        tap(*folder_apps[key])
        ok = guest.wait_for(marker, 30, start)
        check(ok, f"{key}: started")
        return ok

    def open_tile_app(key, marker):
        # tiles 值为 (page, cx, cy)；docks 值为 (cx, cy)。
        dismiss_foreground()
        if key in tiles:
            pg, x, y = tiles[key]
        elif key in docks:
            x, y = docks[key]
            pg = state["page"]
        else:
            check(False, f"missing app layout for {key}")
            return None
        if key not in docks:
            goto_page(pg)
        tap(x, y)
        if not wait_for(marker, 25):
            check(False, f"{key}: did not start ({marker})")
            return None
        time.sleep(0.8)
        return True

    # ==================== 1) clock：编辑子页返回 ====================
    if open_folder_app("clock", "[clock] 就绪"):
        state["page"] = 1
        # 闹钟页签（200,66）+ 右下「＋ 新建」（480,864），重试到进入编辑子页
        check(tap_until([(200, 66), (480, 864)], "[clock] 进入闹钟编辑（新建）"),
              "clock: entered alarm edit page")
        check(tap_until([(24, 24)], "[clock] 返回 → 闹钟列表"),
              "clock: title-bar back returns to list")
        check(count_of("应用 'Clock' 已退出") == 0, "clock: app NOT exited after subpage back")
        check(tap_until([(480, 864)], "[clock] 进入闹钟编辑（新建）"),
              "clock: re-entered edit page")
        # 左边缘内滑 = 系统返回手势（帧可能丢，重试）
        ok_g = False
        for _ in range(8):
            before = len(log)
            swipe(12, 500, 90, 500)
            if wait_for("[clock] 系统返回 → 闹钟列表", 3):
                ok_g = True
                break
        check(ok_g, "clock: system Back gesture returns to list")
        ok_e = False
        for _ in range(6):
            # ESC 前先回编辑子页（上一手势可能已回列表）
            tap_until([(480, 864)], "[clock] 进入闹钟编辑（新建）", tries=3)
            before = len(log)
            send_esc()
            if wait_for("[clock] ESC → 闹钟列表", 3):
                ok_e = True
                break
        check(ok_e, "clock: ESC returns to list")
        check(tap_until([(24, 24)], "[clock] 返回 → 退出"),
              "clock: back at list level exits app")
        wait_for("应用 'Clock' 已退出", 10)
        time.sleep(2.0)
        shot("01_clock_done.ppm")

    # ==================== 2) notes：编辑器返回 ====================
    if open_tile_app("notes", "[notes] 就绪"):
        check(tap_until([(512, 28)], "[notes] 打开编辑器（新建）"),
              "notes: entered editor page")
        ok_g = False
        for _ in range(8):
            before = len(log)
            swipe(12, 500, 90, 500)
            if wait_for("[notes] 系统返回 -> 列表", 3):
                ok_g = True
                break
        check(ok_g, "notes: system Back gesture returns to list")
        ok_e = False
        for _ in range(6):
            tap_until([(512, 28)], "[notes] 打开编辑器（新建）", tries=3)
            before = len(log)
            send_esc()
            if wait_for("[notes] ESC → 返回列表", 3):
                ok_e = True
                break
        check(ok_e, "notes: ESC returns to list")
        check(tap_until([(512, 28)], "[notes] 打开编辑器（新建）"),
              "notes: re-entered editor")
        check(tap_until([(24, 24)], "[notes] 返回 → 列表"),
              "notes: title-bar back returns to list")
        check(tap_until([(24, 24)], "[notes] 返回 → 退出"),
              "notes: back at list level exits app")
        wait_for("应用 'Notes' 已退出", 10)
        time.sleep(2.0)
        shot("02_notes_done.ppm")

    # ==================== 3) store：缺签名拒绝安装 ====================
    if open_tile_app("store", "[store] 就绪"):
        check(wait_for("[store] 索引未签名（无 Packages.json.sig），仅可浏览，禁止安装", 25),
              "store: unsigned index flagged (no Packages.json.sig)")
        # 首行「安装」按钮（row0: x∈[432,524) y∈[174,214)）
        check(tap_until([(478, 194)], "[store] 拒绝安装：索引未签名", tries=8),
              "store: install explicitly rejected on unsigned index")
        time.sleep(0.5)
        check(count_of("packages/notepad-9.9.9.kunx") == 0,
              "store: no .kunx download attempted")
        shot("03_store_unsigned.ppm")
        tap(24, 24)
        wait_for("应用 'Store' 已退出", 10)
        time.sleep(2.0)

    # ==================== 4) cloud：长按解锁（真实 lock 命令路径） ====================
    if open_folder_app("cloud", "[cloud] 已就绪"):
        state["page"] = 1
        check(tap_until([(150, 503)], "[cloud] 注册成功", tries=6),
              "cloud: registered against mock gateway")
        check(wait_for("[cloud] 备份成功", 30), "cloud: backup uploaded")
        cloud_state["lock_armed"] = True
        check(tap_until([(390, 503)], "[cloud] lock：已锁定", tries=6),
              "cloud: lock executed (overlay shown)")
        check(wait_for("type=lock", 5), "cloud: lock command received")
        time.sleep(0.8)
        shot("04_cloud_locked.ppm")
        base = len(log)
        tap(270, 492)           # 短按解锁按钮（中心）—— 不应解锁
        time.sleep(1.5)
        seg = log[base:].decode(errors="replace")
        check("长按解锁成功" not in seg, "cloud: short tap does NOT unlock")
        check(hold_until(270, 492, 2.4, "[cloud] 长按解锁成功"),
              "cloud: hold >=2s unlocks")
        time.sleep(0.6)
        shot("05_cloud_unlocked.ppm")
        tap(24, 24)
        wait_for("[cloud] back -> exit", 10)
        time.sleep(2.0)

    print("\n=== RESULTS ===")
    fails = 0
    for msg, ok in results:
        print("  %s %s" % ("PASS" if ok else "FAIL", msg))
        if not ok:
            fails += 1
    print("total=%d fail=%d" % (len(results), fails))
    print("=== LOG TAIL ===")
    print(log.decode(errors="replace")[-3000:])
    sys.exit(1 if fails else 0)
finally:
    guest.close()
    mock_srv.shutdown()
    mock_srv.server_close()
    mock_gw.shutdown()
    mock_gw.server_close()
