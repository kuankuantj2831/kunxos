#!/usr/bin/env python3
"""Boot a built image and verify virtio-gpu transport and device-loss fallback.

Uses QEMU's qtest MMIO API to reset only the GPU after desktop startup. The
guest must time out once, preserve the kernel shell and present through ramfb.
The tested kernel/rootfs are never modified. No visible QEMU window is opened.
"""
import argparse
import json
from pathlib import Path
import re
import socket
import struct
import subprocess
import time

from test_display import ROOT, Qmp, connect, free_port


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", required=True, type=Path)
    parser.add_argument("--qemu", default="qemu-system-riscv64")
    parser.add_argument("--build-dir", required=True, type=Path)
    parser.add_argument("--modern", action="store_true")
    parser.add_argument("--force-ramfb", action="store_true")
    args = parser.parse_args()
    work = args.build_dir.resolve()
    work.mkdir(parents=True, exist_ok=True)
    ports = set()
    while len(ports) < 3:
        ports.add(free_port())
    qmp_port, serial_port, test_port = ports
    command = [args.qemu, "-M", "virt,accel=tcg", "-cpu", "rv64", "-bios", "default",
               "-m", "512M", "-kernel", str(args.kernel.resolve()),
               "-append", "shell verbose display-resize display-hz=60"
               + (" display-backend=ramfb" if args.force_ramfb else ""),
               "-device", "ramfb,id=ramfb0", "-device", "virtio-gpu-device,id=gpu0,xres=540,yres=960",
               "-display", "none", "-no-reboot", "-S",
               "-qmp", f"tcp:127.0.0.1:{qmp_port},server=on,wait=off",
               "-serial", f"tcp:127.0.0.1:{serial_port},server=on,wait=off",
               "-qtest", f"tcp:127.0.0.1:{test_port},server=on,wait=off", "-qtest-log", str(work / "qtest.log")]
    if args.modern:
        command += ["-global", "virtio-mmio.force-legacy=false"]
    output = bytearray()
    with (work / "qemu-error.log").open("wb") as errors:
        process = subprocess.Popen(command, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=errors)
        try:
            with connect(qmp_port, process) as qmps, connect(serial_port, process) as serial, connect(test_port, process) as qts:
                qmps.settimeout(15)
                qmp = Qmp(qmps)
                serial.settimeout(0.2)
                qts.settimeout(5)
                test = qts.makefile("rwb", buffering=0)

                def qt(request):
                    test.write(request.encode() + b"\n")
                    while True:
                        line = test.readline().decode().strip()
                        if not line:
                            raise RuntimeError("qtest closed")
                        if line.startswith("OK"):
                            return int(line.split()[1], 0) if len(line.split()) > 1 else None
                        assert line.startswith("IRQ"), line

                base = next(address for address in range(0x10001000, 0x10009000, 0x1000)
                            if qt(f"readl {address + 8:#x}") == 16)
                version = qt(f"readl {base + 4:#x}")
                assert version == (2 if args.modern else 1), f"unexpected transport {version}"

                def wait_for(marker, seconds=60, start=0):
                    deadline = time.monotonic() + seconds
                    while marker not in output[start:] and time.monotonic() < deadline:
                        try:
                            data = serial.recv(65536)
                            if not data:
                                raise RuntimeError("serial closed")
                            output.extend(data)
                        except socket.timeout:
                            pass
                    assert marker in output[start:], f"missing {marker!r}"

                def capture(device, name, w, h):
                    path = work / name
                    deadline = time.monotonic() + 15
                    while True:
                        qmp.call("screendump", {"filename": str(path), "device": device, "head": 0, "format": "png"})
                        actual = struct.unpack(">II", path.read_bytes()[16:24])
                        if actual == (w, h):
                            break
                        assert time.monotonic() < deadline, f"scanout stayed {actual}, expected {(w, h)}"
                        time.sleep(0.2)
                    # PPM also verifies that a surface with actual pixels was shown.
                    ppm = path.with_suffix(".ppm")
                    qmp.call("screendump", {"filename": str(ppm), "device": device, "head": 0, "format": "ppm"})
                    data = ppm.read_bytes().split(b"\n", 3)[3]
                    assert len(set(data)) > 32, "display remained blank"

                qmp.call("cont")
                wait_for(b"[launcher] pacing mHz=")
                backend = "ramfb" if args.force_ramfb else "virtio-gpu"
                assert f"[gpu] {backend} 540x960".encode() in output
                # Request a frame and wait for the cooperating app to repaint.
                serial.sendall(b"\xaa\x03" + struct.pack("<HH", 960, 720))
                wait_for(b"[launcher] resize 960x720")
                time.sleep(1)
                capture("ramfb0" if args.force_ramfb else "gpu0", "before.png", 960, 720)
                if not args.force_ramfb:
                    # Clear device status and its queues: the next request cannot
                    # complete. Its permanent DMA pages must not be freed/reused.
                    qt(f"writel {base + 0x70:#x} 0")
                    serial.sendall(b"\xaa\x03" + struct.pack("<HH", 540, 960))
                    wait_for(b"[gpu] display fallback: ramfb", 15)
                    wait_for(b"[launcher] resize 540x960", 15)
                    time.sleep(1)
                    capture("ramfb0", "fallback.png", 540, 960)
                start = len(output)
                serial.sendall(b"display\n")
                wait_for(b"ramfb: presents=", 10, start)
                if not args.force_ramfb:
                    wait_for(b"failures=1", 10, start)
                    assert output.count(b"virtio-gpu failed:") == 1, "failed queue was reused"
                text = output.decode("utf-8", errors="replace")
                assert not re.search(r"PANIC:|panicked at|fatal trap", text)
                report = {"result": "PASS", "transport": version, "forced_ramfb": args.force_ramfb,
                          "device_loss_fallback": not args.force_ramfb, "shell_responsive": True}
                (work / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
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
