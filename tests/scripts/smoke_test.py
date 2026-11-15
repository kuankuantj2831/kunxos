#!/usr/bin/env python3
"""Boot RISC-V and verify real user-mode ecall and syscall probe execution."""
import argparse
from pathlib import Path
import tempfile

from qemu_test import Guest, build_kernel, qemu_command


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-dir", type=Path)
    parser.add_argument("--kernel")
    args = parser.parse_args()
    work = (args.build_dir or Path(tempfile.mkdtemp(prefix="kunxos-smoke-"))).resolve()
    kernel = str(Path(args.kernel).resolve()) if args.kernel else build_kernel(work, ("probe",))
    guest = Guest(qemu_command() + ["-kernel", kernel, "-append", "shell verbose",
                  "-serial", "stdio", "-display", "none", "-no-reboot", "-device", "ramfb"],
                  work / "smoke-serial.log")
    try:
        assert guest.wait_for("kunxos$", 120), f"RISC-V boot failed; inspect {work}"
        output = guest.shell("run /bin/hello", 30)
        assert b"HELLO-FROM-APP" in output and b"BYE" in output, output
        output = guest.shell("run /bin/probe", 90)
        assert b"FAIL" not in output and b"PANIC" not in output, output
        assert "[syscall-test] 通过".encode() in output and "退出码=0".encode() in output, output
        print(f"RISC-V SYSCALL SMOKE PASS; artifacts: {work}")
    finally:
        guest.close()


if __name__ == "__main__":
    main()
