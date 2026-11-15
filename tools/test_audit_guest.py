"""Build an isolated image and exercise VM/syscall fixes on RISC-V QEMU."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import time
from test_display import ROOT, TARGET, Qmp, connect, free_port, run

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default='qemu-system-riscv64')
    parser.add_argument('--build-dir', type=Path, required=True)
    parser.add_argument('--source', type=Path, default=ROOT / 'tests/audit_guest.rs')
    parser.add_argument('--success-marker', default='AUDIT GUEST PASS')
    args = parser.parse_args()
    work = args.build_dir.resolve()
    work.mkdir(parents=True, exist_ok=True)
    elf = work / 'audit_guest'
    run(['rustc', '--edition=2021', '--target', TARGET, '-C', 'panic=abort', '-C', 'opt-level=2',
         '-C', f'link-arg=-T{ROOT / "apps/probe/linker.ld"}', '-C', 'link-arg=-z',
         '-C', 'link-arg=max-page-size=0x1000', args.source.resolve(), '-o', elf])
    # Fresh stage per run. No writes to the user's rootfs or default ramdisk.
    import tempfile
    stage = Path(tempfile.mkdtemp(prefix='rootfs-', dir=work))
    shutil.copytree(ROOT / 'rootfs', stage, dirs_exist_ok=True,
                    ignore=shutil.ignore_patterns('data', 'sfsrace-test', 'audio-test'))
    for directory in ['dev/input','tmp','proc','var/run','var/log']:
        (stage / directory).mkdir(parents=True, exist_ok=True)
    (stage / 'etc/inittab').write_text('# Isolated syscall test; launched by kernel shell.\n')
    shutil.copy2(elf, stage / 'bin/audit_guest')
    ramdisk = work / 'ramdisk.img'
    run([os.sys.executable, ROOT / 'tools/mkfs_sfs.py', stage, ramdisk, '4096'])
    run(['cargo','build','--release','--offline','--target',TARGET,'--target-dir',work / 'target',
         '--manifest-path',ROOT / 'kunxcore/Cargo.toml'], env=dict(os.environ,KUNX_RAMDISK=str(ramdisk)))
    qmp_port, serial_port = free_port(), free_port()
    while serial_port == qmp_port:
        serial_port = free_port()
    output = bytearray()
    with (work / 'qemu-error.log').open('wb') as errors:
        proc = subprocess.Popen([args.qemu,'-M','virt,accel=tcg','-cpu','rv64','-bios','default',
            '-m','512M','-kernel',str(work / 'target' / TARGET / 'release/rust-kernel'),
            '-append','shell verbose','-device','ramfb','-display','none','-no-reboot','-S',
            '-qmp',f'tcp:127.0.0.1:{qmp_port},server=on,wait=off',
            '-serial',f'tcp:127.0.0.1:{serial_port},server=on,wait=off'],
            cwd=ROOT,stdout=subprocess.DEVNULL,stderr=errors)
        try:
            with connect(qmp_port,proc) as qs, connect(serial_port,proc) as serial:
                qs.settimeout(10)
                Qmp(qs).call('cont')
                serial.settimeout(0.2)
                end=time.monotonic()+60
                sent=False
                while time.monotonic()<end:
                    try:
                        data=serial.recv(65536)
                        if not data: break
                        output.extend(data)
                    except TimeoutError:
                        continue
                    if not sent and b'kunxos$' in output:
                        serial.sendall(b'run /bin/audit_guest\n')
                        sent=True
                    failure = output.find(b'AUDIT GUEST FAIL')
                    if (failure >= 0 and b'\n' in output[failure:]) or args.success_marker.encode() in output:
                        break
        finally:
            proc.terminate()
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill(); proc.wait()
            (work / 'serial.log').write_bytes(output)
    assert args.success_marker.encode() in output, f'Guest test incomplete; inspect {work / "serial.log"}'
    print(args.success_marker,work,flush=True)

if __name__=='__main__':
    main()
