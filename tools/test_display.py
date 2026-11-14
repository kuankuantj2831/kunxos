#!/usr/bin/env python3
"""Build and boot isolated display validation images. Never replaces rootfs/kernel.img.

Requires cargo/rustc, riscv64gc-unknown-none-elf and qemu-system-riscv64.
Example: python tools/test_display.py --qemu "C:/Program Files/qemu/qemu-system-riscv64.exe"
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import socket
import struct
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
TARGET = "riscv64gc-unknown-none-elf"


def run(command, **kwargs):
    subprocess.run([str(x) for x in command], cwd=ROOT, check=True, **kwargs)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def connect(port, process):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError("QEMU exited; inspect qemu-error.log")
        try:
            return socket.create_connection(("127.0.0.1", port), timeout=1)
        except OSError:
            time.sleep(0.05)
    raise TimeoutError("QEMU socket did not open")


class Qmp:
    def __init__(self, sock):
        self.sock = sock
        self.file = sock.makefile("rwb", buffering=0)
        self.read()
        self.call("qmp_capabilities")

    def read(self):
        line = self.file.readline()
        if not line:
            raise RuntimeError("QMP connection closed")
        return json.loads(line)

    def call(self, command, arguments=None):
        request = {"execute": command}
        if arguments:
            request["arguments"] = arguments
        self.file.write(json.dumps(request).encode() + b"\n")
        while True:
            response = self.read()
            if "error" in response:
                raise RuntimeError(response["error"])
            if "return" in response:
                return response["return"]

    def screenshot(self, path, width=None, height=None):
        deadline = time.monotonic() + 15
        while True:
            self.call("screendump", {"filename": str(path), "format": "png"})
            actual = struct.unpack(">II", path.read_bytes()[16:24])
            if width is None or actual == (width, height):
                return
            if time.monotonic() >= deadline:
                raise AssertionError(f"scanout stayed {actual}, expected {(width, height)}")
            time.sleep(0.2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qemu", default="qemu-system-riscv64")
    parser.add_argument("--rate", type=int, default=144, choices=range(24, 361), metavar="24..360")
    parser.add_argument("--build-dir", type=Path)
    parser.add_argument("--oobe", action="store_true", help="validate OOBE instead of desktop")
    parser.add_argument("--animate", action="store_true", help="exercise Settings launch/return transitions")
    parser.add_argument("--resize", action="store_true", help="validate negotiated desktop modesets")
    parser.add_argument("--host-cursor", action="store_true", help="validate native host pointer contract")
    parser.add_argument("--all-apps", action="store_true", help="rebuild all adaptive desktop apps")
    parser.add_argument("--no-build", action="store_true", help="reuse the kernel in --build-dir for runtime checks")
    parser.add_argument("--build-only", action="store_true", help="build the isolated image without booting")
    args = parser.parse_args()
    if args.oobe and args.animate:
        parser.error("--animate requires the desktop")
    work = args.build_dir or Path(tempfile.mkdtemp(prefix="kunxos-display-"))
    work = work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    print("Display validation artifacts:", work, flush=True)
    target_dir = work / "target"
    if not args.no_build:
        tests = work / ("display-tests.exe" if os.name == "nt" else "display-tests")
        run(["rustc", "--edition=2021", "--test", ROOT / "shared/display.rs", "-o", tests])
        run([tests])
        page_tests = work / ("page-record-tests.exe" if os.name == "nt" else "page-record-tests")
        run(["rustc", "--edition=2021", "--test", ROOT / "kunxcore/src/page_records.rs", "-o", page_tests])
        run([page_tests])
        run(["cargo", "test", "--release", "--manifest-path", ROOT / "tests/font_startup_host/Cargo.toml"])
        apps = ["launcher", "oobe", "settings"]
        if args.all_apps:
            apps += ["calculator", "terminal", "store"]
            apps += [p.parent.parent.name for p in (ROOT / "apps").glob("*/src/main.rs")
                     if "shared/app_kit.rs" in p.read_text(encoding="utf-8")]
        for app in apps:
            run(["cargo", "build", "--release", "--target", TARGET, "--target-dir", target_dir,
                 "--manifest-path", ROOT / "apps" / app / "Cargo.toml"])
        stage = work / ("rootfs-oobe" if args.oobe else "rootfs-desktop")
        # A fresh stage makes the run independent of the user's data and test flags.
        if stage.exists():
            stage = Path(tempfile.mkdtemp(prefix="rootfs-", dir=work))
            shutil.copytree(ROOT / "rootfs", stage, dirs_exist_ok=True,
                            ignore=shutil.ignore_patterns("data", "sfsrace-test", "audio-test"))
        else:
            shutil.copytree(ROOT / "rootfs", stage,
                            ignore=shutil.ignore_patterns("data", "sfsrace-test", "audio-test"))
        # Git checkouts omit empty directories. Match the normal Makefile skeleton,
        # especially /dev: mknod and the inherited stdio fds depend on it existing.
        for directory in ("dev/input", "etc/init.d", "proc", "tmp", "var/run", "var/log",
                          "data/oobe", "data/photos", "data/videos", "data/app",
                          "var/lib/pkg/cache/index", "var/lib/pkg/installed",
                          "var/lib/pkg/permissions", "var/lib/pkg/reviewed", "var/lib/pkg/backup"):
            (stage / directory).mkdir(parents=True, exist_ok=True)
        if not args.oobe:
            (stage / "data/oobe/.done").write_text("1\n")
        for app in apps:
            shutil.copy2(target_dir / TARGET / "release" / app, stage / "bin" / app)
        ramdisk = work / ("ramdisk-oobe.img" if args.oobe else "ramdisk-desktop.img")
        run([os.sys.executable, ROOT / "tools/mkfs_sfs.py", stage, ramdisk, "4096"])
        env = dict(os.environ, KUNX_RAMDISK=str(ramdisk))
        run(["cargo", "build", "--release", "--target", TARGET, "--target-dir", target_dir,
             "--manifest-path", ROOT / "kunxcore/Cargo.toml"], env=env)
    if args.build_only:
        print("Kernel:", target_dir / TARGET / "release/rust-kernel", flush=True)
        return
    qmp_port, serial_port = free_port(), free_port()
    while serial_port == qmp_port:
        serial_port = free_port()
    qemu_args = [args.qemu, "-M", "virt,accel=tcg", "-cpu", "rv64", "-bios", "default",
                 "-m", "512M", "-kernel", str(target_dir / TARGET / "release/rust-kernel"),
                 "-append", f"shell verbose display-hz={args.rate}" + (" display-resize" if args.resize else "") + (" host-cursor" if args.host_cursor else ""), "-device", "ramfb",
                 "-display", "none", "-no-reboot", "-S",
                 "-qmp", f"tcp:127.0.0.1:{qmp_port},server=on,wait=off",
                 "-serial", f"tcp:127.0.0.1:{serial_port},server=on,wait=off",
                 "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
                 "-device", "virtio-keyboard-device", "-device", "virtio-tablet-device",
                 "-device", "virtio-rng-device", "-audiodev", "none,id=aud0",
                 "-device", "virtio-sound-device,audiodev=aud0"]
    output = bytearray()
    with (work / "qemu-error.log").open("wb") as errors:
        process = subprocess.Popen(qemu_args, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=errors)
        try:
            with connect(qmp_port, process) as qmp_socket, connect(serial_port, process) as serial:
                qmp_socket.settimeout(15)
                qmp = Qmp(qmp_socket)
                serial.settimeout(0.2)
                qmp.call("cont")
                def collect(seconds):
                    until = time.monotonic() + seconds
                    while time.monotonic() < until:
                        try:
                            data = serial.recv(65536)
                            if not data:
                                return
                            output.extend(data)
                        except socket.timeout:
                            pass
                def await_marker(marker, count, seconds=15):
                    until = time.monotonic() + seconds
                    while output.count(marker) < count and time.monotonic() < until:
                        collect(0.2)
                    if output.count(marker) < count:
                        serial.sendall(b"ps\nperf stats\n")
                        collect(1)
                        qmp.screenshot(work / "failure.png")
                        (work / "failure-registers.json").write_text(json.dumps(qmp.call("human-monitor-command", {"command-line": "info registers"})))
                    assert output.count(marker) >= count, f"guest did not reach {marker!r}"
                marker = b"[oobe] font_init done" if args.oobe else b"[launcher] pacing mHz="
                deadline = time.monotonic() + 90
                ready_at = None
                sent = False
                while time.monotonic() < deadline:
                    try:
                        data = serial.recv(65536)
                        if not data:
                            break
                        output.extend(data)
                    except socket.timeout:
                        pass
                    if marker in output and ready_at is None:
                        ready_at = time.monotonic()
                    if ready_at is not None and time.monotonic() - ready_at > 2 and not sent:
                        serial.sendall(b"display\nperf stats\n")
                        collect(0.5)
                        if not args.oobe:
                            serial.sendall(b"run /bin/busybox echo ELF_SHARED_PAGE_OK\n")
                            await_marker(b"ELF_SHARED_PAGE_OK\r\n", 1)
                            await_marker(b"[run]", 1)
                            assert re.search(rb"\[run\][^\n]*" + "退出码=0".encode(), output), "shared ELF page regression"
                        if args.resize:
                            resize_owner = "oobe" if args.oobe else "launcher"
                            for w, h in ((960, 720), (1280, 960), (540, 960)):
                                serial.sendall(b"\xaa\x03" + struct.pack("<HH", w, h))
                                await_marker(f"[{resize_owner}] resize {w}x{h}".encode(), 1, 30)
                                collect(1)
                                qmp.screenshot(work / f"resize-{w}x{h}.png", w, h)
                        # A wide desktop must retain the same host surface during pointer updates.
                        if args.resize and not args.oobe:
                            serial.sendall(b"\xaa\x03" + struct.pack("<HH", 1920, 1001))
                            await_marker(b"[launcher] resize 1920x1001", 1, 30)
                            qmp.screenshot(work / "wide-before.png", 1920, 1001)
                            collect(2)
                            def scanout_stats():
                                start = len(output)
                                serial.sendall(b"display\n")
                                deadline = time.monotonic() + 5
                                while time.monotonic() < deadline:
                                    collect(0.2)
                                    stats = re.search(rb"ramfb: presents=(\d+) reconfigurations=(\d+)", output[start:])
                                    if stats:
                                        return tuple(int(x) for x in stats.groups())
                                raise AssertionError("missing scanout counters")
                            def pointer(x, y):
                                qmp.call("input-send-event", {"events": [
                                    {"type": "abs", "data": {"axis": "x", "value": x}},
                                    {"type": "abs", "data": {"axis": "y", "value": y}}]})
                            def pixels(path):
                                qmp.call("screendump", {"filename": str(path), "format": "ppm"})
                                header, size, maximum, data = path.read_bytes().split(b"\n", 3)
                                assert header == b"P6" and maximum == b"255"
                                return data
                            pointer(3000, 16000)
                            collect(0.2)
                            pointer_before = pixels(work / "pointer-before.ppm")
                            before = scanout_stats()
                            for index in range(60):
                                qmp.call("input-send-event", {"events": [
                                    {"type": "abs", "data": {"axis": "x", "value": 3000 + index * 350}},
                                    {"type": "abs", "data": {"axis": "y", "value": 16000}}]})
                                time.sleep(1 / 60)
                            collect(1)
                            after = scanout_stats()
                            assert args.host_cursor or after[0] > before[0] + 5, f"pointer did not repaint: {before}, {after}"
                            assert after[1] == before[1], f"pointer recreated host surfaces: {before}, {after}"
                            qmp.screenshot(work / "wide-after.png", 1920, 1001)
                            pointer(3000, 16000)
                            collect(0.3)
                            pointer_after = pixels(work / "pointer-after.ppm")
                            # The motion path crosses this otherwise static band. Returning
                            # to the same position must leave exactly the original pixels.
                            assert pointer_before[400*1920*3:800*1920*3] == pointer_after[400*1920*3:800*1920*3], "pointer trails in framebuffer"
                            # Cross Dock hover targets without clicking or launching applications.
                            for index in range(24):
                                qmp.call("input-send-event", {"events": [
                                    {"type": "abs", "data": {"axis": "x", "value": 12000 + index * 350}},
                                    {"type": "abs", "data": {"axis": "y", "value": 30400 if index % 2 else 26000}}]})
                                time.sleep(1 / 60)
                            collect(1)
                            assert scanout_stats()[1] == after[1], "Dock hover recreated host surface"
                            qmp.screenshot(work / "wide-dock.png", 1920, 1001)

                            pointer_presents = after[0] - before[0]
                            serial.sendall(b"\xaa\x03" + struct.pack("<HH", 540, 960))
                            await_marker(b"[launcher] resize 540x960", 2, 30)
                            qmp.screenshot(work / "pointer-restored.png", 540, 960)
                        if args.animate:
                            dock = re.search(rb"dock settings cx=(\d+) cy=(\d+)", output)
                            assert dock, "Settings Dock layout was not reported"
                            dock_x, dock_y = (int(value) for value in dock.groups())
                            settings_ready = "[settings] 就绪".encode()
                            returned = "返回过渡：".encode()
                            for cycle in range(1, 4):
                                serial.sendall(b"\xaa\x01" + struct.pack("<HH", dock_x, dock_y))
                                collect(0.08)
                                serial.sendall(b"\xaa\x00\0\0\0\0")
                                await_marker(settings_ready, cycle)
                                await_marker("启动过渡：".encode(), cycle)
                                collect(0.5)
                                # Before any resize/input can force an app redraw,
                                # ensure the launcher's green Settings placeholder
                                # did not overwrite the app's only initial frame.
                                first_frame = work / f"settings-first-{cycle}.ppm"
                                qmp.call("screendump", {"filename": str(first_frame), "format": "ppm"})
                                frame_data = first_frame.read_bytes().split(b"\n", 3)[3]
                                red, green, blue = frame_data[:3]
                                assert not (green > red + 40 and green > blue + 40), "Settings first frame covered by launch placeholder"
                                if args.host_cursor and cycle == 1:
                                    collect(1)
                                    def settings_pixels(label):
                                        path = work / f"settings-cursor-{label}.ppm"
                                        qmp.call("screendump", {"filename": str(path), "format": "ppm"})
                                        _, size, _, data = path.read_bytes().split(b"\n", 3)
                                        width, height = map(int, size.split())
                                        return data[24 * width * 3:(height - 60) * width * 3]
                                    clean = settings_pixels("before")
                                    for index in range(30):
                                        qmp.call("input-send-event", {"events": [
                                            {"type": "abs", "data": {"axis": "x", "value": 3000 + index * 800}},
                                            {"type": "abs", "data": {"axis": "y", "value": 26000}}]})
                                        collect(0.01)
                                    collect(0.3)
                                    assert settings_pixels("moved") == clean, "host cursor movement changed Settings pixels"
                                    for down in (True, False):
                                        qmp.call("input-send-event", {"events": [
                                            {"type": "btn", "data": {"button": "left", "down": down}}]})
                                        collect(0.08)
                                    collect(1)
                                    assert settings_pixels("clicked") == clean, "blank click left cursor/ripple residue in Settings"
                                if args.resize and cycle == 1:
                                    for w, h in ((960, 720), (540, 960)):
                                        serial.sendall(b"\xaa\x03" + struct.pack("<HH", w, h))
                                        await_marker(f"[settings] resize {w}x{h}".encode(), 1, 30)
                                        collect(1)
                                        qmp.screenshot(work / f"settings-{w}x{h}.png", w, h)
                                qmp.call("send-key", {"keys": [{"type": "qcode", "data": "esc"}], "hold-time": 60})
                                await_marker(returned, cycle)
                                collect(0.2)
                            if args.resize and args.all_apps:
                                phone_dock = re.findall(rb"dock phone cx=(\d+) cy=(\d+)", output)[-1]
                                px, py = (int(value) for value in phone_dock)
                                serial.sendall(b"\xaa\x01" + struct.pack("<HH", px, py))
                                collect(0.08)
                                serial.sendall(b"\xaa\x00\0\0\0\0")
                                await_marker("[phone]".encode(), 1)
                                collect(2)
                                for w, h in ((960, 720), (540, 960)):
                                    serial.sendall(b"\xaa\x03" + struct.pack("<HH", w, h))
                                    await_marker(f"[app-kit] resize {w}x{h}".encode(), 1, 30)
                                    qmp.screenshot(work / f"phone-{w}x{h}.png", w, h)
                                    collect(1)
                                qmp.call("send-key", {"keys": [{"type": "qcode", "data": "esc"}], "hold-time": 60})
                                await_marker(returned, 4)
                        sent = True
                        collect(0.5)
                    if ready_at is not None and time.monotonic() - ready_at > 5:
                        break
                qmp.call("screendump", {"filename": str(work / "screen.png"), "format": "png"})
                text = output.decode("utf-8", errors="replace")
                (work / "serial.log").write_text(text, encoding="utf-8")
                expected = 1_000_000_000 // ((1_000_000_000 + args.rate * 1000 - 1) // (args.rate * 1000))
                assert marker in output, "UI did not reach ready marker; see serial.log"
                if not args.oobe:
                    cursor_mode = b"[cursor] host" if args.host_cursor else b"[cursor] software"
                    assert cursor_mode in output, "launcher cursor mode does not match boot contract"
                assert f"target-mHz={expected}" in text, "display-hz target did not reach kernel/display ABI"
                assert f"capabilities={(2 if args.resize else 0) | (4 if args.host_cursor else 0)}" in text, "incorrect display capabilities"
                assert not re.search(r"PANIC:|panicked at|fatal trap", text), "guest fault; see serial.log"
                if args.animate:
                    assert "返回过渡：" in text, "return animation did not complete; see serial.log"
                report = {"result": "PASS", "ui": "oobe" if args.oobe else "launcher",
                          "requested_hz": args.rate, "target_millihz": expected,
                          "host_resize_bridge": args.resize, "host_cursor": args.host_cursor, "hardware_vsync": False}
                if args.resize and not args.oobe:
                    report["wide_pointer_presents"] = pointer_presents
                    report["pointer_surface_reconfigurations"] = after[1] - before[1]
                if args.animate:
                    launch_frames = [int(x) for x in re.findall(r"启动过渡：\s*(\d+) frames", text)]
                    report["launch_animation_frames"] = launch_frames
                    report["font_init_ms"] = [int(x) for x in re.findall(r"font_init (\d+)ms", text)]
                    assert launch_frames and min(launch_frames) >= 5, f"launch animation skipped frames: {launch_frames}"
                    report["return_animation_frames"] = [int(x) for x in re.findall(r"返回过渡：\s*(\d+) frames", text)]
                (work / "report.json").write_text(json.dumps(report, indent=2))
                print(json.dumps(report), flush=True)
        finally:
            (work / "serial.log").write_text(output.decode("utf-8", errors="replace"), encoding="utf-8")
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
