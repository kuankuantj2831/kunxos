#!/usr/bin/env python3
"""RISC-V camera regression: mailbox replies, JPEG, advancing preview and KXRV.

Usage: python tests/scripts/camera_e2e_test.py [logfile]
Uses the kernel shell's syntax (echo arguments are not POSIX-quoted).
"""
import re
import sys
import time

from qemu_test import Guest, kernel_path, temp_path, qemu_command


def main():
    logfile = sys.argv[1] if len(sys.argv) > 1 else temp_path("kx_cam_e2e.log")
    guest = Guest(qemu_command() + ["-kernel", kernel_path(), "-serial", "stdio",
                  "-device", "ramfb", "-display", "none", "-no-reboot",
                  "-monitor", "none", "-append", "shell verbose"], logfile)
    try:
        assert guest.wait_for("kunxos$", 90), "Guest shell did not start"
        guest.shell("verbose off")
        time.sleep(2)

        def request(number, command, timeout=30):
            guest.shell(f"echo #{number} {command} > /var/run/camerad.ctl")
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                response = guest.shell("cat /var/run/camerad.resp").decode("utf-8", "replace")
                match = re.search(rf"(?m)^#{number} ([^\r\n]+)", response)
                if match:
                    body = match.group(1)
                    assert not body.startswith("ERR"), f"{command}: {body}"
                    return body
                time.sleep(0.2)
            raise TimeoutError(f"No camera response for #{number} {command}; inspect {logfile}")

        def sequence():
            response = guest.shell("cat /tmp/cam/latest")
            numbers = re.findall(rb"(?m)^(\d+)(?=kunxos\$|\r?$)", response)
            assert numbers, f"Missing preview sequence: {response!r}"
            return int(numbers[-1])

        assert int(request(1, "LIST", 90)) >= 1
        assert request(2, "3A_GET 0").startswith("OK ae ")
        print("PASS: camera enumeration and 3A")

        photo = request(3, "PHOTO 0", 60).split()
        assert len(photo) == 3 and int(photo[1]) > 0, photo
        filename = f"/data/photos/photo_{int(photo[0]):06d}.jpg"
        header = guest.shell(f"od {filename} -n 2")
        assert b"ff d8" in header, f"Missing JPEG SOI: {header!r}"
        print("PASS: nonempty JPEG photo and SOI")

        assert request(4, "PREVIEW_START 0") == "OK"
        time.sleep(1)
        before = sequence()
        time.sleep(1)
        after = sequence()
        assert after > before, (before, after)
        assert request(5, "PREVIEW_STOP 0") == "OK"
        latest = sequence()
        header = guest.shell(f"od /tmp/cam/frame_{latest % 6}.yuv -n 4")
        assert b"4b 58 46 52" in header, f"Missing KXFR preview header: {header!r}"
        print("PASS: advancing preview and frame header")

        assert request(6, "REC_START 0") == "OK"
        time.sleep(1.5)
        assert request(7, "REC_STOP 0") == "OK"
        header = guest.shell("od /data/videos/rec_000001.kxv -n 4")
        assert b"4b 58 52 56" in header, f"Missing KXRV recording header: {header!r}"
        print("PASS: recording and KXRV header")
        print("CAMERA RISC-V PASS")
    finally:
        guest.close()


if __name__ == "__main__":
    main()
