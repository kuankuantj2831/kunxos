#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""KunxOS 浏览器 QEMU 端到端测试。

流程：
1. 用「OOBE 已完成」的临时 rootfs 构建测试内核（避免手工走 OOBE）
2. QEMU 无头启动（ramfb + QMP screendump + 串口触摸注入）
3. 等 launcher 桌面就绪，点 Dock 中的浏览器图标
4. 等浏览器首页渲染完成，截图；再点链接、返回，逐步截图

用法：python3 tests/scripts/browser_test.py [--keep] [--shots-dir DIR]
"""

import argparse
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.abspath(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
KERNEL_IMG = os.path.join(ROOT, "kunxcore", "kernel.img")

FRAME_DOWN = 0x01
FRAME_UP = 0x00

# Dock 中浏览器图标中心（launcher dock_rect 计算，见 apps/launcher/src/main.rs）
BROWSER_TAP = (270, 872)
# 首页「新闻中心」链接大致位置（kunxos://news，首页卡片第一项）
# 内容区 y=136 起；截图后按实际渲染微调


def frame(kind, x, y):
    return bytes([0xAA, kind, x & 0xFF, (x >> 8) & 0xFF, y & 0xFF, (y >> 8) & 0xFF])


def qmp_cmd(sock, cmd):
    sock.sendall((cmd + "\n").encode())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true", help="keep temp files")
    ap.add_argument("--shots-dir", default="/tmp/kx-browser-shots")
    args = ap.parse_args()

    work = tempfile.mkdtemp(prefix="kxb_")
    pipe = os.path.join(work, "kxb")
    qmp_path = os.path.join(work, "kxb-qmp")
    shots = args.shots_dir
    os.makedirs(shots, exist_ok=True)
    logf = open(os.path.join(work, "serial.log"), "wb")

    # ---- 1) 构建测试内核（OOBE 完成状态） ----
    print("[test] building test rootfs (oobe done)...")
    rf = os.path.join(work, "rootfs")
    shutil.copytree(os.path.join(ROOT, "rootfs"), rf)
    oobe_dir = os.path.join(rf, "data", "oobe")
    os.makedirs(oobe_dir, exist_ok=True)
    for f in os.listdir(os.path.join(rf, "etc", "oobe_default")):
        shutil.copy(os.path.join(rf, "etc", "oobe_default", f), os.path.join(oobe_dir, f))
    with open(os.path.join(oobe_dir, ".done"), "w") as f:
        f.write("1\n")
    subprocess.run([sys.executable, os.path.join(ROOT, "tools", "mkfs_sfs.py"),
                    rf, os.path.join(ROOT, "kunxcore", "ramdisk.img"), "1536"],
                   cwd=ROOT, check=True)
    print("[test] building kernel...")
    subprocess.run(["cargo", "build", "--release", "--target", "aarch64-unknown-none",
                    "--manifest-path", os.path.join(ROOT, "kunxcore", "Cargo.toml"),
                    "--features", "qemu"], cwd=ROOT, check=True)
    subprocess.run(["llvm-objcopy", "-O", "binary",
                    os.path.join(ROOT, "kunxcore", "target", "aarch64-unknown-none", "release", "rust-kernel"),
                    KERNEL_IMG], cwd=ROOT, check=True)
    print("[test] 内核就绪")

    # ---- 2) 启动 QEMU ----
    os.mkfifo(pipe + ".in")
    os.mkfifo(pipe + ".out")
    qemu = ["qemu-system-aarch64", "-M", "virt", "-cpu", "cortex-a72", "-m", "512M",
            "-kernel", KERNEL_IMG, "-serial", "pipe:" + pipe,
            "-device", "ramfb", "-no-reboot", "-display", "none",
            "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
            "-device", "virtio-keyboard-device", "-device", "virtio-tablet-device",
            "-device", "virtio-rng-device", "-device", "AC97",
            "-qmp", "unix:%s,server=on,wait=off" % qmp_path]
    proc = subprocess.Popen(qemu, cwd=work, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    log = bytearray()
    stop = False

    def reader():
        try:
            outfd = os.open(pipe + ".out", os.O_RDONLY)
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

    def wait_for(marker, timeout):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if marker.encode() in bytes(log):
                return True
            if proc.poll() is not None:
                return False
            time.sleep(0.1)
        return False

    def tap(x, y, hold=0.03):
        try:
            infd = os.open(pipe + ".in", os.O_WRONLY)
        except OSError:
            return
        os.write(infd, frame(FRAME_DOWN, x, y))
        time.sleep(hold)
        os.write(infd, frame(FRAME_UP, 0, 0))
        os.close(infd)

    def shoot(name):
        time.sleep(0.5)
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(qmp_path)
            s.settimeout(3)
            qmp_cmd(s, '{"execute":"qmp_capabilities"}')
            time.sleep(0.2)
            ppm = os.path.join(shots, name + ".ppm")
            qmp_cmd(s, '{"execute":"screendump","arguments":{"filename":"%s"}}' % ppm)
            time.sleep(0.6)
            s.close()
            if os.path.exists(ppm):
                from PIL import Image
                im = Image.open(ppm).convert("RGB")
                im.save(os.path.join(shots, name + ".png"))
                print("[test] screenshot -> %s.png (%dx%d)" % (name, im.width, im.height))
            else:
                print("[test] WARN: screendump produced no file")
        except Exception as e:
            print("[test] screendump error:", e)

    try:
        if not wait_for("Hello from Rust kernel", 60):
            print("FAIL: kernel did not boot")
            return 1
        print("[test] kernel booted")
        if not wait_for("[launcher] 桌面就绪", 90):
            print("FAIL: launcher not ready; last log:")
            print(bytes(log[-2000:]).decode("utf-8", "replace"))
            return 1
        print("[test] launcher 桌面就绪")
        time.sleep(1.0)
        shoot("00-desktop")
        # 点浏览器 Dock 图标
        tap(*BROWSER_TAP)
        if not wait_for("[browser] 页面就绪", 40):
            print("FAIL: browser did not load page; last log:")
            print(bytes(log[-3000:]).decode("utf-8", "replace"))
            return 1
        print("[test] browser 页面就绪")
        time.sleep(0.8)
        shoot("01-browser-home")
        # 点「新闻中心」链接（需按实际截图位置；先用首页卡片区域尝试）
        # 首页布局：h1(20px) + p + card(h3+ul)。ul 第一项链接约在 y≈300
        tap(60, 305)
        time.sleep(1.5)
        shoot("02-news")
        # 点「← 返回首页」
        tap(60, 880)
        time.sleep(1.0)
        shoot("03-back-home")
        # 前进
        time.sleep(0.3)
        print("[test] done, serial log ->", os.path.join(work, "serial.log"))
    finally:
        stop = True
        try:
            proc.terminate()
        except Exception:
            pass
        time.sleep(1)
        try:
            proc.kill()
        except Exception:
            pass
        time.sleep(0.3)
        logf.close()
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
