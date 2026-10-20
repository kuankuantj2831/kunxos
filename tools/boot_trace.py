#!/usr/bin/env python3
"""KunxOS 启动全链路时间戳记录器（优化轮基建，round-012 起用）。

启动 QEMU GUI 实例，串口逐块打时间戳落盘，按标记切分启动阶段并输出
耗时分解；OOBE 用「下一步」注入快进（PIN/密码步停手等 30s 自动推进），
到达桌面后采集 [perf] 直绘渲染窗口并经 QEMU monitor 截屏。

用法：python3 tools/boot_trace.py [日志前缀] [--perf-secs 20]
退出码 0 = 完整跑到桌面。
"""
import os
import re
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

QEMU = os.environ.get(
    "QEMU", os.path.expanduser("~/qemu-riscv-pkgs/root/usr/bin/qemu-system-riscv64"))
BIOS = os.environ.get(
    "BIOS", os.path.expanduser(
        "~/qemu-riscv-pkgs/root/usr/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin"))
KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))

PREFIX = sys.argv[1] if len(sys.argv) > 1 and not sys.argv[1].startswith("--") \
    else "/tmp/kunxos-boot"
PERF_SECS = 20
for a in sys.argv[1:]:
    if a.startswith("--perf-secs="):
        PERF_SECS = int(a.split("=", 1)[1])

LOG = PREFIX + ".log"
SHOT = PREFIX + "-desktop.ppm"

WORKDIR = tempfile.mkdtemp(prefix="kxboot_")
PIPE = os.path.join(WORKDIR, "kx")
MON = os.path.join(WORKDIR, "mon")
os.mkfifo(PIPE + ".in")
os.mkfifo(PIPE + ".out")

cmd = [
    QEMU, "-M", "virt", "-accel", "tcg", "-cpu", "rv64", "-bios", BIOS, "-m", "512M",
    "-kernel", KERNEL,
    "-serial", "pipe:" + PIPE,
    "-monitor", "unix:" + MON + ",server,nowait",
    "-device", "ramfb", "-no-reboot", "-vnc", ":0",
    "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
    "-device", "virtio-keyboard-device", "-device", "virtio-tablet-device",
    "-device", "virtio-rng-device",
    "-audiodev", "none,id=aud0", "-device", "virtio-sound-device,audiodev=aud0",
]

# 阶段标记：日志子串 → 阶段名（首次出现时间被记录）
MARKERS = [
    ("OpenSBI v", "opensbi"),
    ("[lifd]", "首个服务"),           # lifd 任意输出 = init 已拉起服务
    ("初始内存压力=", "lifd 就绪"),
    ("[oobe] 欢迎使用", "OOBE 首个 UI 输出"),
    ("[perf] 帧", "首帧呈现"),
    ("[launcher] 桌面就绪", "桌面就绪"),
]

t_start = time.time()
qemu_err = open(PREFIX + "-qemu.err", "wb")
p = subprocess.Popen(cmd, cwd=WORKDIR, stdout=subprocess.DEVNULL,
                     stderr=qemu_err)
logf = open(LOG, "wb")
tsf = open(PREFIX + ".ts", "w")  # 行级到达时间戳（与 .log 行号对应）
hits = {}
lines = []  # (t, line) 仅粗存含标记/统计的行
stop = False
buf = bytearray()


def note(marker):
    if marker not in hits:
        hits[marker] = time.time() - t_start


def reader():
    # 永不退出：读线程死亡会让串口 fifo 失去读者，QEMU 写侧 EPIPE 直接
    # 退出（首跑 8 秒即死的疑似根因）。任何异常都吞掉继续。
    outfd = os.open(PIPE + ".out", os.O_RDONLY)
    pending = bytearray()
    while not stop:
        try:
            data = os.read(outfd, 4096)
        except OSError:
            time.sleep(0.05)
            continue
        if not data:
            time.sleep(0.02)
            continue
        t = time.time() - t_start
        logf.write(data)  # 原始日志不加前缀（保内容纯净）
        logf.flush()
        pending.extend(data)
        while b"\n" in pending:
            line_b, _, rest = pending.partition(b"\n")
            del pending[:]
            pending.extend(rest)
            tsf.write("%.3f\t%s\n" % (t, line_b.decode(errors="replace")))
            tsf.flush()
            s = line_b.decode(errors="replace")
            for m, name in MARKERS:
                if m in s:
                    note(name)
            if "[perf] 帧" in s or "[launcher]" in s or "转场" in s or "t2f" in s:
                lines.append((t, s))


threading.Thread(target=reader, daemon=True).start()


def frame(kind, x=0, y=0):
    return bytes((0xAA, kind, x & 0xFF, x >> 8, y & 0xFF, y >> 8))


def swipe_up():
    """底部上滑回桌面（Home）：按下→分步上移→抬起。"""
    infd = os.open(PIPE + ".in", os.O_WRONLY)
    os.write(infd, frame(0x01, 270, 900))
    for y in range(860, 240, -60):
        os.write(infd, frame(0x02, 270, y))
        time.sleep(0.03)
    os.write(infd, frame(0x00))
    os.close(infd)
    time.sleep(0.1)


def tap(x, y, hold=0.06):
    infd = os.open(PIPE + ".in", os.O_WRONLY)
    os.write(infd, frame(0x01, x, y))
    time.sleep(hold)
    os.write(infd, frame(0x00))
    os.close(infd)
    time.sleep(0.05)


def wait_for(text, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if text in hits:
            return True
        if any(text in s for _, s in lines):
            return True
        try:
            with open(LOG, "rb") as f:
                if text.encode() in f.read():
                    return True
        except OSError:
            pass
        if p.poll() is not None:
            return False
        time.sleep(0.2)
    return False


def screendump(path):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(MON)
        s.sendall(("screendump %s\n" % path).encode())
        time.sleep(0.5)
        s.close()
    except Exception as e:
        print("[boot-trace] screendump 失败:", e, flush=True)


def main():
    try:
        # 到桌面：可能直接就绪（旧镜像已走完 OOBE），否则 OOBE 快进——
        # 普通步点「下一步」(465,925)；密码/胁迫 PIN 步拒绝空输入，停手
        # 35s 等「30 秒无输入自动下一步」（与 app_open_shots.py 同款）。
        if not wait_for("[launcher] 桌面就绪", 30):
            # 连续风暴快进（r053，r052 定证变现）：r47 窗口化点按的
            # 「52 发 8 达」是时序伪影——连续风暴实测 40/40 全达且卡片页
            # 照样推进。改为持续点按，仅在步号刚变时退避 2.5s（转场+入场
            # 动画落定，动画期 pump 吞点击为已知设计）；步号 >30s 未变则
            # 停手让「30 秒无输入」兜底（密码/胁迫 PIN 页拒绝空输入）。
            deadline = time.time() + 700
            last_step = -1
            step_t = time.time()
            since_tap = 0.0
            while time.time() < deadline:
                if "桌面就绪" in hits:
                    break
                cur = -1
                try:
                    with open(LOG, "rb") as f:
                        f.seek(-8192, os.SEEK_END)
                        tail = f.read().decode(errors="replace")
                    steps = re.findall(r"第 (\d+)/13 步", tail)
                    if steps:
                        cur = int(steps[-1])
                except OSError:
                    pass
                if cur != last_step:
                    last_step = cur
                    step_t = time.time()
                    since_tap = 0.0
                    print(f"[boot-trace] OOBE 步进 {cur}/13 (+{time.time()-t_start:.0f}s)", flush=True)
                    time.sleep(2.5)  # 转场/入场动画落定
                    continue
                if time.time() - step_t < 30:
                    tap(465, 925)
                    since_tap = 0.0
                    time.sleep(1.8)
                else:
                    # 卡步 >30s：停手让空闲兜底（快速连点会重置其计时器）
                    if since_tap == 0.0:
                        print(f"[boot-trace] 步 {cur} 卡 >30s，停手等空闲兜底", flush=True)
                        since_tap = time.time()
                    time.sleep(1.0)
        # 桌面交互 t2f 采集（r44）：点应用图标 → 等就绪标记（超时 25s 放弃
        # 继续下一个），共产出多段 [perf] t2f 样本（内核 perf.rs 已按帧记录
        # 触控→上屏延迟）。坐标取 app_open_shots.py APPS_DB。
        # 坐标以实测为准（r54 定证）：APPS_DB 的 calculator(230,182) 在
        # 当前 21 应用布局实际命中 Clock——旧表过时。三连点改为
        # clock×2+settings（两处独立坐标均实证可启动）。
        taps = [("clock", 150, 182, "[clock]"),
                ("clock2", 230, 182, "[clock]"),
                ("settings", 418, 872, "[settings]")]
        import re as _re
        print("[boot-trace] 桌面就绪，开始应用点按采集", flush=True)
        for name, x, y, marker in taps:
            if name != taps[0][0]:
                swipe_up()  # 回桌面（前一应用仍在前台，否则点击打在其界面上）
                t0 = time.time()
                while time.time() - t0 < 15:
                    try:
                        with open(LOG, "rb") as f:
                            if "已退出".encode() in f.read()[-6000:]:
                                time.sleep(5.0)  # 返回/取消转场落定（转场中点击=「启动已取消」；r47 实测取消路径回弹更久）
                                break
                    except OSError:
                        pass
                    time.sleep(0.5)
            tap(x, y)
            t0 = time.time()
            hit = None
            while time.time() - t0 < 40:
                try:
                    with open(LOG, "rb") as f:
                        txt = f.read().decode(errors="replace")
                    for ln in txt.splitlines():
                        if ln.startswith(marker):
                            hit = ln[:70]
                    if hit:
                        print(f"[boot-trace] {name}: {hit}", flush=True)
                        break
                except OSError:
                    pass
                if p.poll() is not None:
                    break
                time.sleep(0.5)
            if not hit:
                print(f"[boot-trace] {name}: 40s 无 [{name}] 行", flush=True)
        # 桌面 [perf] 采集窗口
        print(f"[boot-trace] 采集桌面 [perf] 窗口 {PERF_SECS}s…", flush=True)
        t_perf0 = time.time()
        while time.time() - t_perf0 < PERF_SECS:
            if p.poll() is not None:
                break
            time.sleep(1.0)
        screendump(SHOT)
    finally:
        stop = True
        p.terminate()
        try:
            p.wait(timeout=5)
        except Exception:
            p.kill()
        logf.close()
        qemu_err.close()

    prev = 0.0
    print(f"[boot-trace] 阶段分解（QEMU 启动 = 0.000s）", flush=True)
    for m, name in MARKERS:
        if name in hits:
            t = hits[name]
            print("  %8.3fs (+%6.3fs) %s" % (t, t - prev, name), flush=True)
            prev = t
    perf = [(t, s) for t, s in lines if "[perf] 帧" in s]
    if perf:
        print(f"[boot-trace] 桌面 [perf] 直绘渲染窗口（{len(perf)} 条）", flush=True)
        for t, s in perf[-4:]:
            print("  %.1fs %s" % (t, s.strip()), flush=True)
    print("[boot-trace] 日志: %s 截图: %s" % (LOG, SHOT), flush=True)


if __name__ == "__main__":
    main()
