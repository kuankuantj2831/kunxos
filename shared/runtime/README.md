# kunxrt

KunxOS native RV64 userspace support, implemented in this repository with no
third-party crate or libc dependencies. Rust `core`, `alloc` and compiler
builtins are still provided by the Rust toolchain.

- `entry!`: argc/argv entry, psABI stack alignment and exit status propagation.
- `syscall`: raw RV64 syscall0–6, and Linux-style errno decoding. These unsafe
  wrappers do not validate caller pointers or make an unsupported syscall work.
- `memory`: memcpy, memmove, memset, memcmp and bcmp. The `memory-symbols` feature
  exports C names only for bare-metal guests; host tests do not replace libc.
- `allocator!`: a serialized process heap backed by brk. Address-sorted free
  blocks split and coalesce. Allocation headers retain alignment and span;
  dealloc actually reuses memory. Growth is at least 256 KiB and page aligned.
  Rust GlobalAlloc's default realloc and alloc_zeroed use the same heap.
- `process`: complete writes (including EINTR/short writes), exit, allocation-free
  panic reporting, and fork with the heap lock released in both process copies.

Freed heap pages remain mapped for reuse until process exit. This is not a full
C/POSIX library, dynamic loader, pthread implementation or replacement for Rust
language libraries. Signal handlers must not allocate while interrupting a heap
operation. After multithreaded fork, application locks still require ordinary
fork discipline; the runtime only serializes its own heap. Raw clone callers
are responsible for their own synchronization. This heap must exclusively own
the process break; do not interleave independent brk allocators.

Host verification:

```text
cargo test --manifest-path shared/runtime/Cargo.toml
```

The host suite verifies all relative alignments and overlap directions for
memory primitives, errno boundaries, and 30,000 randomized allocations with
payload validation and full coalescing. The public unsafe Heap arena interface
requires exclusive writable nonoverlapping storage and valid live pointers.

Guest verification (requires Rust RV64 target and QEMU; rild also needs a C
cross compiler selected through KUNXOS_CC and KUNXOS_AR):

```text
python tools/test_runtime.py
python tools/test_runtime.py --reuse-binaries --mode desktop
python tools/test_runtime.py --reuse-binaries --mode oobe
```

The script stages freshly built native applications, init, services and package
manager, then rebuilds a separate kernel with that image. rttest verifies
alignment, zeroing/reallocation, 20,000 allocation/free operations, fork, and
100 repeated TrueType parse/render/drop cycles whose mapped heap must plateau.
It also runs the syscall probe and package-manager list command. Desktop and
OOBE modes reuse the existing display validation harness. Artifacts stay under
`build/runtime-validation`; normal rootfs binaries and boot images are not replaced.
