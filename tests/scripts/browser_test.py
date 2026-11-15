#!/usr/bin/env python3
"""RISC-V browser fetch/HTML regression with an isolated image and local HTTP.

GUI navigation is covered by apps_regression.py. This verifies page content via
--selftest without fixed UI coordinates or replacing the normal kernel image.
"""
import argparse
import http.server
from pathlib import Path
import tempfile
import threading

from qemu_test import Guest, build_kernel, qemu_command


class Page(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"<html><head><title>RISC-V browser fixture</title></head><body>KunxOS</body></html>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-dir", type=Path)
    parser.add_argument("--kernel", help="reuse an already-built RISC-V image")
    args = parser.parse_args()
    work = (args.build_dir or Path(tempfile.mkdtemp(prefix="kunxos-browser-"))).resolve()
    work.mkdir(parents=True, exist_ok=True)
    kernel = str(Path(args.kernel).resolve()) if args.kernel else build_kernel(work, ("probe", "browser"))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Page)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    guest = None
    try:
        guest = Guest(qemu_command() + ["-kernel", kernel, "-append", "shell verbose",
                      "-serial", "stdio", "-display", "none", "-device", "ramfb", "-no-reboot",
                      "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0"],
                      work / "serial.log")
        assert guest.wait_for("kunxos$", 120), f"Shell did not boot; inspect {work}"
        for url, title in [("file:///etc/test.html", None),
                           (f"http://10.0.2.2:{server.server_port}/index.html", "RISC-V browser fixture")]:
            start = len(guest.log)
            guest.send(f"run /bin/browser --selftest {url}\n".encode())
            assert guest.wait_for("[browser-selftest] PASS", 90, start), f"Fetch failed: {url}; inspect {work}"
            segment = guest.log[start:].decode("utf-8", "replace")
            assert "[browser-selftest] FAIL" not in segment, segment
            if title:
                assert f"标题: {title}" in segment, segment
        print(f"BROWSER RISC-V PASS; artifacts: {work}")
    finally:
        if guest:
            guest.close()
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


if __name__ == "__main__":
    main()
