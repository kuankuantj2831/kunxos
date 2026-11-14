"""Regression checks against extracted current kernel source with host stubs.
Run: python docs/audit-2026-11-14-repro.py
This tests logic, not RISC-V traps/TLBs; run the guest and display tests too.
"""
from pathlib import Path
import re
import subprocess
import os
ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'build/audit-2026-11-14/repro'
OUT.mkdir(parents=True, exist_ok=True)
def function(source, name):
    start = source.index('fn ' + name + '(')
    if source[max(0, start-7):start].endswith('unsafe '):
        start -= 7
    body = source.index('{', start)
    depth, end = 1, body + 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]
compat = (ROOT / 'kunxcore/src/linux_compat.rs').read_text(encoding='utf-8')
vm = (ROOT / 'kunxcore/src/vm.rs').read_text(encoding='utf-8')
process = (ROOT / 'kunxcore/src/process.rs').read_text(encoding='utf-8')
code = (ROOT / 'tests/audit_kernel_host.rs').read_text(encoding='utf-8')
names = ['VALID','R','W','X','U','A','D','LEAF_MASK','AF','AP_USER_RW','AP_USER_RO','AP_MASK','FLAG_COW','FLAG_NO_ACCESS','FLAG_USER_CODE','FLAG_USER_DATA','EXEC','LAZY_PAGE','PAGE_SIZE']
flags = '\n'.join(re.search(r'^(?:pub )?const ' + name + r':.*?;', vm, re.M | re.S).group() for name in names)
vm_funcs = '\n'.join('pub ' + function(vm,n) for n in ['pte_to_addr','addr_to_ppn','user_page_pte_flags','user_page_pte_u','user_page_pte_writable','unmap_page_in','user_readable','user_writable','data_permissions','is_cow','page_descriptor','user_range_occupied'])
# Host architecture has no sfence.vma; all other page-walk logic is original.
vm_funcs = re.sub(r'asm!\([^;]+;', '', vm_funcs)
compat_funcs = '\n'.join(function(compat,n) for n in ['timespec_us','sleep_for_us','sys_nanosleep','sys_clock_nanosleep','poll_deadline','poll_wait','fd_poll_events','sys_ppoll','sys_poll_family','sys_mprotect','read_cstr','read_str_array','sys_execveat'])
code = code.replace('__VM__',flags+'\n'+vm_funcs).replace('__COMPAT__',compat_funcs).replace('__WRITABLE__',function(process,'ensure_user_range_writable'))
code = code.replace('__PKG__',(ROOT / 'kunxcore/src/pkg.rs').as_posix())
source = OUT / 'regression.rs'
source.write_text(code, encoding='utf-8')
exe = OUT / ('regression.exe' if os.name == 'nt' else 'regression')
subprocess.run(['rustc','--edition=2021','--test',str(source),'-o',str(exe)],cwd=ROOT,check=True)
subprocess.run([str(exe),'--test-threads=1'],cwd=ROOT,check=True)
