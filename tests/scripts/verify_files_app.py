#!/usr/bin/env python3
"""KunxOS 文件管理器 GUI 端到端验证：驱动 OOBE 完成 → launcher 桌面 →
按 launcher 输出的文件夹布局打开 files→ 验证应用启动并截图。

用法：python3 tests/scripts/verify_files_app.py [logfile] [shots_dir]
"""
from qemu_test import Guest, kernel_path, temp_path, qemu_command, monitor_address, monitor_option, launcher_layout, screenshot, desktop_ready
import sys, time, os, tempfile

KERNEL = kernel_path()
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else temp_path("kx_files.log")
SHOTS = sys.argv[2] if len(sys.argv) > 2 else temp_path("kx_files_shots")
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxf_")
MON = monitor_address()

FRAME_DOWN, FRAME_UP = 0x01, 0x00

def frame(kind, x=0, y=0):
    return bytes([0xAA, kind, x & 0xFF, (x >> 8) & 0xFF, y & 0xFF, (y >> 8) & 0xFF])


cmd = qemu_command() + [
       "-kernel", KERNEL, "-serial", "stdio",
       "-device", "ramfb", "-no-reboot", "-display", "none",
       "-monitor", monitor_option(MON),
       "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
       "-device", "virtio-keyboard-device", "-device", "virtio-tablet-device"]

guest = Guest(cmd, LOGFILE, cwd=WORKDIR)
p = guest.process
log = guest.log


def tap(x, y, hold=0.06):
    try:
        guest.send(frame(FRAME_DOWN, x, y))
        time.sleep(hold)
        guest.send(frame(FRAME_UP, 0, 0))
        time.sleep(0.05)
    except OSError:
        pass

def shot(name):
    screenshot(MON, SHOTS, name)

def text():
    try:
        return log.decode("utf-8", "replace")
    except Exception:
        return ""

def wait_for(marker, timeout=300):
    dl = time.time() + timeout
    while time.time() < dl:
        if marker in text():
            return True
        time.sleep(0.3)
    return False

fail = 0
try:
    desktop_ready(guest, tap)
    time.sleep(1)
    _, _, folders, contents = launcher_layout(log)
    assert "tools" in folders and "files" in contents, "Missing files folder layout"
    page, fx, fy = folders["tools"]
    for target_page in range(1, page + 1):
        page_start = len(log)
        for x in range(440, 99, -34):
            guest.send(frame(0x01 if x == 440 else 0x02, x, 500))
            time.sleep(0.03)
        guest.send(frame(FRAME_UP))
        assert guest.wait_for(f"[launcher] page={target_page}", 15, page_start), "Page turn did not finish"
        time.sleep(0.5)
    start = len(log)
    tap(fx, fy)
    assert guest.wait_for("[launcher] 已打开文件夹", 10, start), "Folder did not open"
    time.sleep(1)
    tap(*contents["files"])
    if not wait_for("[files] 就绪", 120):
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
    if "[files] 正在退出" in text():
        print("OK: 文件管理器正常退出")
    else:
        # 可能已进入子目录，再按一次返回
        tap(20, 20)
        time.sleep(2.0)
        if "[files] 正在退出" in text():
            print("OK: 文件管理器正常退出")
        else:
            print("FAIL: 文件管理器未退出")
            fail = 1
    sys.exit(fail)
finally:
    guest.close()
