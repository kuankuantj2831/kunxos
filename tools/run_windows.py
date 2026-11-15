#!/usr/bin/env python3
"""Run native Windows QEMU with a host-to-guest window-size bridge.

The bridge observes only the QEMU process it launches. GTK's menu is disabled
so the client rectangle is the drawing area.
Guest builds must include the display-resize protocol. No external packages.
"""
import argparse
import hashlib
import json
import ctypes
from ctypes import wintypes as wt
from pathlib import Path
import os
import socket
import struct
import subprocess
import sys
import threading
import time

ROOT = Path(__file__).resolve().parents[1]


def select_gpu(requested, kernel):
    if requested != "auto":
        return requested
    # Only opt a known build into the new backend. Old kernels and externally
    # replaced images remain bootable through ramfb; the hash rejects stale metadata.
    try:
        metadata = json.loads(kernel.with_suffix(".display.json").read_text(encoding="utf-8"))
        if ("virtio-gpu" in metadata.get("backends", [])
                and metadata.get("sha256") == hashlib.sha256(kernel.read_bytes()).hexdigest()):
            return "virtio-gpu"
    except (OSError, ValueError, TypeError, AttributeError):
        pass
    return "ramfb"


def main():
    if os.name != "nt":
        raise SystemExit("This launcher requires Windows; use make run-gui on Linux.")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qemu", type=Path, default=Path(r"C:\Program Files\qemu\qemu-system-riscv64.exe"))
    parser.add_argument("--kernel", type=Path, help="explicit ELF or kernel image")
    parser.add_argument("--build", action="store_true", help="build the adaptive Windows image before launch")
    parser.add_argument("--gpu", choices=("auto", "ramfb", "virtio-gpu"), default="auto",
                        help="auto selects virtio-gpu for verified new builds, ramfb for old images (2D only)")
    parser.add_argument("--display", choices=("gtk", "sdl"), default="gtk",
                        help="native display backend (GTK is the tested Windows default)")
    parser.add_argument("--rate", type=int, help="override detected host refresh target (24..360 Hz)")
    parser.add_argument("--log", type=Path, default=ROOT / "qemu-windows-serial.log")
    parser.add_argument("--qmp-port", type=int, help="optional localhost QMP port for diagnostics")
    args = parser.parse_args()
    built_kernel = ROOT / "build/windows/target" / "riscv64gc-unknown-none-elf/release/rust-kernel"
    if args.build:
        subprocess.run([sys.executable, str(ROOT / "tools/test_display.py"),
                        "--build-only", "--all-apps", "--build-dir", str(ROOT / "build/windows")], check=True)
        args.kernel = built_kernel
    if args.kernel is None:
        args.kernel = built_kernel if built_kernel.is_file() else ROOT / "kunxcore/kernel.img"
    if not args.qemu.is_file() or not args.kernel.is_file():
        parser.error("QEMU or kernel does not exist; supply --qemu and --kernel.")
    args.gpu = select_gpu(args.gpu, args.kernel)
    if args.rate is not None and not 24 <= args.rate <= 360:
        parser.error("--rate must be between 24 and 360")
    user = ctypes.WinDLL("user32", use_last_error=True)
    gdi = ctypes.WinDLL("gdi32", use_last_error=True)
    user.SetProcessDPIAware()
    user.GetDC.restype = wt.HDC
    gdi.GetDeviceCaps.argtypes = [wt.HDC, ctypes.c_int]
    user.ReleaseDC.argtypes = [wt.HWND, wt.HDC]
    dc = user.GetDC(None)
    try:
        detected_rate = gdi.GetDeviceCaps(dc, 116)
        rate = args.rate or (detected_rate if 24 <= detected_rate <= 360 else 60)
    finally:
        user.ReleaseDC(None, dc)
    callback_type = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
    user.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
    user.GetClientRect.argtypes = [wt.HWND, ctypes.POINTER(wt.RECT)]
    user.IsWindowVisible.argtypes = [wt.HWND]
    user.IsIconic.argtypes = [wt.HWND]
    user.EnumWindows.argtypes = [callback_type, wt.LPARAM]
    user.GetWindowDpiAwarenessContext.argtypes = [wt.HWND]
    user.GetWindowDpiAwarenessContext.restype = wt.HANDLE
    user.SetThreadDpiAwarenessContext.argtypes = [wt.HANDLE]
    user.SetThreadDpiAwarenessContext.restype = wt.HANDLE
    user.GetAwarenessFromDpiAwarenessContext.argtypes = [wt.HANDLE]
    user.GetAwarenessFromDpiAwarenessContext.restype = ctypes.c_int
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(0.5)
        port = listener.getsockname()[1]
        command = [str(args.qemu), "-name", "KunxOS", "-M", "virt,accel=tcg",
                   "-cpu", "rv64", "-bios", "default", "-m", "512M",
                   "-kernel", str(args.kernel.resolve()),
                   "-append", f"shell verbose display-resize host-cursor display-hz={rate}",
                   "-display", ("gtk,gl=off,show-cursor=on,show-menubar=off" if args.display == "gtk"
                                else "sdl,gl=off,show-cursor=on"), "-device",
                   ("virtio-gpu-device,xres=540,yres=960" if args.gpu == "virtio-gpu" else "ramfb"), "-no-reboot",
                   "-serial", f"tcp:127.0.0.1:{port}",
                   "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
                   "-device", "virtio-keyboard-device", "-device", "virtio-tablet-device",
                   "-device", "virtio-rng-device", "-audiodev", "none,id=aud0",
                   "-device", "virtio-sound-device,audiodev=aud0"]
        error_path = args.log.with_suffix(".error.log")
        if args.qmp_port:
            command += ["-qmp", f"tcp:127.0.0.1:{args.qmp_port},server=on,wait=off"]
        with error_path.open("wb") as errors, args.log.open("wb", buffering=0) as log:
            process = subprocess.Popen(command, cwd=ROOT, stderr=errors,
                                       stdout=subprocess.DEVNULL, creationflags=subprocess.CREATE_NO_WINDOW,
                                       env=dict(os.environ, SDL_WINDOWS_DPI_AWARENESS="permonitorv2"))
            print(f"QEMU PID={process.pid}; gpu={args.gpu}; target={rate} Hz; log={args.log}", flush=True)
            try:
                connect_until = time.monotonic() + 20
                while True:
                    if process.poll() is not None:
                        raise RuntimeError(f"QEMU exited; inspect {error_path}")
                    try:
                        connection, _ = listener.accept()
                        break
                    except socket.timeout:
                        if time.monotonic() >= connect_until:
                            raise TimeoutError("QEMU did not connect its serial channel")
                with connection:
                    ready = threading.Event()
                    def read_guest():
                        tail = b""
                        try:
                            while data := connection.recv(65536):
                                log.write(data)
                                tail = (tail + data)[-4096:]
                                if b"[launcher] pacing" in tail or "[launcher] 桌面就绪".encode() in tail or b"[oobe] font_init done" in tail:
                                    ready.set()
                        except OSError:
                            pass
                    reader = threading.Thread(target=read_guest, daemon=True)
                    reader.start()
                    previous = None
                    candidate = None
                    changed_at = 0
                    dpi_logged = False
                    while process.poll() is None:
                        windows = []
                        @callback_type
                        def visit(hwnd, _):
                            nonlocal dpi_logged
                            pid = wt.DWORD()
                            user.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
                            if pid.value == process.pid and user.IsWindowVisible(hwnd) and not user.IsIconic(hwnd):
                                rect = wt.RECT()
                                # Measure in QEMU's coordinate space. Measuring an
                                # unaware SDL window from an aware helper otherwise
                                # feeds Windows' 125% scaling back into every modeset.
                                context = user.GetWindowDpiAwarenessContext(hwnd)
                                if not dpi_logged:
                                    awareness = user.GetAwarenessFromDpiAwarenessContext(context)
                                    log.write(f"\n[host-window] dpi-awareness={awareness}\n".encode())
                                    dpi_logged = True
                                previous_context = user.SetThreadDpiAwarenessContext(context)
                                try:
                                    if user.GetClientRect(hwnd, ctypes.byref(rect)):
                                        windows.append((rect.right - rect.left, rect.bottom - rect.top))
                                finally:
                                    if previous_context:
                                        user.SetThreadDpiAwarenessContext(previous_context)
                            return True
                        user.EnumWindows(visit, 0)
                        if ready.is_set() and windows:
                            w, h = max(windows, key=lambda size: size[0] * size[1])
                            size = (max(360, min(1920, w)), max(480, min(1200, h)))
                            now = time.monotonic()
                            if size != candidate:
                                candidate, changed_at = size, now
                            # Coalesce resize storms, retaining a live response during dragging.
                            if size != previous and now - changed_at >= 0.06:
                                connection.sendall(b"\xaa\x03" + struct.pack("<HH", *size))
                                previous = size
                                log.write(f"\n[host-window] request {size[0]}x{size[1]}\n".encode())
                        time.sleep(0.03)
                    try:
                        connection.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                    reader.join(timeout=2)
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=10)


if __name__ == "__main__":
    main()
