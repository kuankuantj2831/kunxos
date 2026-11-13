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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qemu", default="qemu-system-riscv64")
    parser.add_argument("--rate", type=int, default=144, choices=range(24, 361), metavar="24..360")
    parser.add_argument("--build-dir", type=Path)
    parser.add_argument("--oobe", action="store_true", help="validate OOBE instead of desktop")
    args = parser.parse_args()
    work = args.build_dir or Path(tempfile.mkdtemp(prefix="kunxos-display-"))
    work = work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    print("Display validation artifacts:", work, flush=True)
    tests = work / ("display-tests.exe" if os.name == "nt" else "display-tests")
    run(["rustc", "--edition=2021", "--test", ROOT / "shared/display.rs", "-o", tests])
    run([tests])
    target_dir = work / "target"
    for app in ("launcher", "oobe"):
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
    (stage / "data/oobe").mkdir(parents=True)
    if not args.oobe:
        (stage / "data/oobe/.done").write_text("1\n")
    for app in ("launcher", "oobe"):
        shutil.copy2(target_dir / TARGET / "release" / app, stage / "bin" / app)
    ramdisk = work / ("ramdisk-oobe.img" if args.oobe else "ramdisk-desktop.img")
    run([os.sys.executable, ROOT / "tools/mkfs_sfs.py", stage, ramdisk, "4096"])
    env = dict(os.environ, KUNX_RAMDISK=str(ramdisk))
    run(["cargo", "build", "--release", "--target", TARGET, "--target-dir", target_dir,
         "--manifest-path", ROOT / "kunxcore/Cargo.toml"], env=env)
    qmp_port, serial_port = free_port(), free_port()
    while serial_port == qmp_port:
        serial_port = free_port()
    qemu_args = [args.qemu, "-M", "virt,accel=tcg", "-cpu", "rv64", "-bios", "default",
                 "-m", "512M", "-kernel", str(target_dir / TARGET / "release/rust-kernel"),
                 "-append", f"shell verbose display-hz={args.rate}", "-device", "ramfb",
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
                        sent = True
                    if ready_at is not None and time.monotonic() - ready_at > 5:
                        break
                qmp.call("screendump", {"filename": str(work / "screen.ppm")})
                text = output.decode("utf-8", errors="replace")
                (work / "serial.log").write_text(text, encoding="utf-8")
                expected = 1_000_000_000 // ((1_000_000_000 + args.rate * 1000 - 1) // (args.rate * 1000))
                assert marker in output, "UI did not reach ready marker; see serial.log"
                assert f"target-mHz={expected}" in text, "display-hz target did not reach kernel/display ABI"
                assert "capabilities=0" in text, "ramfb must not claim native resize or VSYNC"
                assert not re.search(r"PANIC:|panicked at|fatal trap", text), "guest fault; see serial.log"
                report = {"result": "PASS", "ui": "oobe" if args.oobe else "launcher",
                          "requested_hz": args.rate, "target_millihz": expected,
                          "native_resize": False, "hardware_vsync": False}
                (work / "report.json").write_text(json.dumps(report, indent=2))
                print(json.dumps(report), flush=True)
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
