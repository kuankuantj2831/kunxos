// SFS v2 宿主测试：include 内核 fs_simple.rs 真实源码，用 RAM 磁盘桩验证
// 基本操作 / 稀疏大文件 / 磨损散布 / 日志环复用 / 掉电崩溃恢复 / Python 镜像兼容。

include!("../../../kunxcore/src/fs_simple.rs");
mod blkdev;

// 宿主测试桩：内嵌于 fs_simple.rs 的全局 SFS 锁引用 sched_cfs（单线程测试
// 用常量 tid，锁始终可获取，不依赖调度器）。
mod sched_cfs {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU32, Ordering};
    std::thread_local! {
        static TID: Cell<u32> = const { Cell::new(0) };
    }
    /// 每线程稳定且**全局唯一**的 tid：线程局部起始值必须是唯一的，不能所有线程
    /// 都从 0 首次取到同一个常量（那会让 fs_lock 把「他人持锁」误判为「自己
    /// 递归」，锁退化为无互斥 —— 见宿主并发复现实验）。用全局单调计数器分配。
    static NXT_TID: AtomicU32 = AtomicU32::new(0);
    pub fn current_tid() -> u32 {
        TID.with(|t| {
            let v = t.get();
            if v == 0 {
                let id = NXT_TID.fetch_add(1, Ordering::Relaxed) + 1;
                t.set(id);
                id
            } else {
                v
            }
        })
    }
    pub fn yield_now() {
        std::thread::yield_now();
    }
    /// 与内核 sched_cfs::sleep_until 语义对应：让出 CPU 给持锁任务，避免忙等。
    pub fn sleep_until(_deadline_us: u64) {
        std::thread::yield_now();
    }
    /// 与内核 sched_cfs::tid_alive 对应：宿主测试线程由 current_tid 分配且
    /// 从不消亡，恒真即还原「持有者存活、锁需等待」语义（holder==0 的无
    /// 持有者情形调用方已单独判断，不会问到 tid_alive(0)）。
    pub fn tid_alive(_tid: u32) -> bool {
        true
    }
}

mod smp {
    /// 单核宿主测试桩（内核 smp::cpu_id 用于 fs_lock 记录持有 CPU）。
    pub fn cpu_id() -> u32 {
        0
    }
}

mod timer {
    pub fn now_us() -> u64 {
        0
    }
}

const MAJOR: u32 = 3;

fn uart_send_str(s: &str) {
    print!("{}", s);
}
fn print_dec(v: u64) {
    print!("{}", v);
}

use std::collections::BTreeSet;

thread_local! {
    static FAILURES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
fn failures() -> usize {
    FAILURES.with(|f| f.get())
}
fn check(name: &str, cond: bool) {
    if cond {
        println!("  [OK] {}", name);
    } else {
        println!("  [FAIL] {}", name);
        FAILURES.with(|f| f.set(f.get() + 1));
    }
}

/// 挂载测试盘（mkfs 后或崩溃后重挂载）。
fn reboot() -> i32 {
    mount(MAJOR)
}

/// 模拟断电重启：真实硬件重启后 RAM 中的 static 状态全部归零。mount 会
/// 重建 DevState 与事务挂起（TXN_*/PENDING_*），但 COPYOUT_FAILED 设计为
/// 跨 mount 保留（copy-out 失败的日志记录必须保持可重放，阻止 head 前移
/// 直到 checkpoint 成功）——宿主壳里它跨「断电」残留会把后续所有 checkpoint
/// 连同日志环推进一起堵死，故须在此手工清零。
fn power_off() {
    unsafe {
        COPYOUT_FAILED = [false; 16];
    }
}

#[allow(dead_code)]
fn dump_debug(major: u32, label: &str) {
    let mut sb = [0u8; BLK_SIZE];
    if read_sb(major, &mut sb) {
        let head = get_u64(&sb, SB_JLOG_HEAD_ABS);
        let next = get_u64(&sb, SB_JLOG_NEXT_ABS);
        let hseq = get_u64(&sb, SB_JLOG_HEAD_SEQ);
        let jstart = get_u32(&sb, SB_JLOG_START) as usize;
        let jblocks = get_u32(&sb, SB_JLOG_BLOCKS) as usize;
        println!("[dump {}] sb head_abs={} next_abs={} head_seq={} jlog start={} blocks={}",
            label, head, next, hseq, jstart, jblocks);
        let pos = (head % jblocks as u64) as usize;
        for i in 0..24 {
            let p = (((pos + jblocks) as i64 - 16 + i as i64) % jblocks as i64) as usize;
            let mut h = [0u8; BLK_SIZE];
            blkdev::blk_read(major, jstart + p, &mut h);
            if get_u32(&h, JH_MAGIC) == JLOG_MAGIC {
                let seq = get_u64(&h, JH_SEQ_LO);
                let flags = get_u32(&h, JH_FLAGS);
                let tgt = get_u32(&h, JH_TARGET);
                println!("  ring+{} (blk {}): seq={} flags={} target={}", i, jstart + p, seq, flags, tgt);
            } else {
                println!("  ring+{} (blk {}): INVALID", i, jstart + p);
            }
        }
    }
    let mut imap = [0u8; BLK_SIZE];
    blkdev::blk_read(major, IMAP_BLOCK, &mut imap);
    let mut bits = String::new();
    for ino in 0..24 {
        bits.push_str(&format!("{}:{} ", ino, bit_get(&imap, ino)));
    }
    println!("[dump {}] imap: {}", label, bits);
}

fn write_file(major: u32, dir: u32, name: &str, data: &[u8]) -> i32 {
    let ino = fs_create(major, dir, name, T_REG, 0);
    if ino < 0 {
        return -1;
    }
    let n = fs_write(major, ino as u32, 0, data);
    if n != data.len() as i32 {
        return -1;
    }
    ino
}

fn read_whole(major: u32, ino: u32) -> Vec<u8> {
    let size = fs_size(major, ino) as usize;
    let mut out = Vec::new();
    let mut off = 0usize;
    let mut buf = [0u8; 8192];
    while off < size {
        let n = fs_read(major, ino, off, &mut buf).max(0) as usize;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        off += n;
    }
    out
}

fn test_basic() {
    println!("== test_basic ==");
    let total = 4096usize;
    blkdev::set_disk(vec![0u8; total * blkdev::BLK_SIZE]);
    check("mkfs", mkfs(MAJOR) == 0);
    check("mount", reboot() == 0);

    let root = fs_root_ino(MAJOR);
    check("root ino", root == 1 && fs_type(MAJOR, root) == T_DIR);

    let d = fs_mkdir(MAJOR, root, "sub");
    check("mkdir", d == 2);
    check("mkdir exists -> EEXIST", fs_mkdir(MAJOR, root, "sub") < 0);

    let hello = b"hello sfs v2 journal";
    let ino = write_file(MAJOR, root, "a.txt", hello);
    check("create+write", ino == 3);
    check("size", fs_size(MAJOR, ino as u32) == hello.len() as u64);
    check("readback", read_whole(MAJOR, ino as u32) == hello);
    check("lookup", fs_lookup(MAJOR, root, "a.txt") == ino);

    // 跨块写（> 4KB）
    let big: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    let bino = write_file(MAJOR, root, "big.bin", &big);
    check("big write", bino >= 0);
    check("big size", fs_size(MAJOR, bino as u32) == big.len() as u64);
    check("big readback", read_whole(MAJOR, bino as u32) == big);

    // 改名
    check("rename", fs_rename(MAJOR, root, "a.txt", root, "a2.txt") == 0);
    check("rename gone", fs_lookup(MAJOR, root, "a.txt") < 0);
    check("rename present", fs_lookup(MAJOR, root, "a2.txt") == ino);

    // 截断（1 块小文件）
    check("truncate", fs_truncate(MAJOR, ino as u32) == 0);
    check("truncate size 0", fs_size(MAJOR, ino as u32) == 0);
    check("truncate read", read_whole(MAJOR, ino as u32).is_empty());

    // 截断 3 块直接文件（校验多直接块释放 + inode 清空，非并发场景）
    let t3 = fs_create(MAJOR, root, "t3.bin", T_REG, 0);
    check("create t3", t3 >= 0);
    if t3 >= 0 {
        let data3 = vec![0x33u8; 12288];
        let mut off = 0usize;
        let mut ok3 = true;
        while off < data3.len() && ok3 {
            let n = fs_write(MAJOR, t3 as u32, off, &data3[off..off + 4096]);
            if n != 4096 {
                ok3 = false;
            }
            off += n as usize;
        }
        check("write t3 12KB", ok3 && off == 12288);
        let mut rb3 = [0u8; 12288];
        check("read t3", fs_read(MAJOR, t3 as u32, 0, &mut rb3) == 12288 && rb3.iter().all(|&b| b == 0x33));
        check("truncate t3", fs_truncate(MAJOR, t3 as u32) == 0);
        check("t3 size 0", fs_size(MAJOR, t3 as u32) == 0);
        let mut rb4 = [0u8; 32];
        check("t3 read empty", fs_read(MAJOR, t3 as u32, 0, &mut rb4) == 0);
        check("unlink t3", fs_unlink(MAJOR, root, "t3.bin") == 0);
    }

    // 删除
    check("unlink", fs_unlink(MAJOR, root, "big.bin") == 0);
    check("unlink gone", fs_lookup(MAJOR, root, "big.bin") < 0);

    // 符号链接
    let sym = fs_symlink(MAJOR, root, "link", "/root/target");
    check("symlink", sym >= 0);
    let mut tbuf = [0u8; 64];
    let n = fs_readlink(MAJOR, sym as u32, &mut tbuf);
    check("readlink", n == 12 && &tbuf[..12] == b"/root/target");

    // 目录列举
    let mut names: Vec<[u8; 28]> = Vec::new();
    let mut idx = 0usize;
    loop {
        let mut nm = [0u8; 28];
        let r = fs_readdir(MAJOR, root, idx, &mut nm);
        if r < 0 {
            break;
        }
        names.push(nm);
        idx += 1;
    }
    check("readdir count", names.len() == 5); // ., .., sub, a2.txt, link
    check("fsck clean", sfs_check(MAJOR) == 0);
    check("fsync", fs_checkpoint(MAJOR) == 0);
    check("fsck clean after fsync", sfs_check(MAJOR) == 0);
}

fn test_large_sparse() {
    println!("== test_large_sparse ==");
    let total = 16384usize;
    blkdev::set_disk(vec![0u8; total * blkdev::BLK_SIZE]);
    check("mkfs", mkfs(MAJOR) == 0);
    check("mount", reboot() == 0);
    let root = fs_root_ino(MAJOR);

    let ino = fs_create(MAJOR, root, "sparse", T_REG, 0);
    check("create", ino >= 0);
    let off = 5u64 * 1024 * 1024 * 1024 + 12345; // > 4GB（三间接范围）
    let data = b"tail marker at 5GB";
    let n = fs_write(MAJOR, ino as u32, off as usize, data);
    check("write at 5GB", n == data.len() as i32);
    check("size 5GB", fs_size(MAJOR, ino as u32) == off + data.len() as u64);

    // 读回尾部
    let mut buf = [0u8; 64];
    let r = fs_read(MAJOR, ino as u32, off as usize, &mut buf);
    check("read tail", r >= data.len() as i32 && &buf[..data.len()] == data);

    // 空洞读零
    let mut zbuf = [0xFFu8; 16];
    let r = fs_read(MAJOR, ino as u32, 100_000, &mut zbuf);
    check("hole reads zero", r == 16 && zbuf.iter().all(|&b| b == 0));

    // 头部写一个块，验证 direct + 间接共存
    let hdr = b"head";
    let n = fs_write(MAJOR, ino as u32, 0, hdr);
    check("write head", n == 4);
    let mut hbuf = [0u8; 8];
    let r = fs_read(MAJOR, ino as u32, 0, &mut hbuf);
    check("read head", r >= 4 && &hbuf[..4] == hdr);

    // 双间接区（> 4.2MB 但 < 4GB）写入
    let off2 = 10usize * 1024 * 1024;
    let d2 = b"double indirect zone";
    let n = fs_write(MAJOR, ino as u32, off2, d2);
    check("write @10MB", n == d2.len() as i32);
    let mut b2 = [0u8; 64];
    let r = fs_read(MAJOR, ino as u32, off2, &mut b2);
    check("read @10MB", r >= d2.len() as i32 && &b2[..d2.len()] == d2);

    // 三间接区（> 4GB + 4MB）
    let off3 = (4u64 * 1024 * 1024 * 1024) + (5u64 * 1024 * 1024);
    let d3 = b"triple indirect zone";
    let n = fs_write(MAJOR, ino as u32, off3 as usize, d3);
    check("write @4GB+5MB", n == d3.len() as i32);
    let mut b3 = [0u8; 64];
    let r = fs_read(MAJOR, ino as u32, off3 as usize, &mut b3);
    check("read @4GB+5MB", r >= d3.len() as i32 && &b3[..d3.len()] == d3);

    check("fsck clean", sfs_check(MAJOR) == 0);

    // 截断大文件后全部释放
    check("truncate sparse", fs_truncate(MAJOR, ino as u32) == 0);
    check("size 0", fs_size(MAJOR, ino as u32) == 0);
    check("fsck clean after truncate", sfs_check(MAJOR) == 0);
}

fn test_wear_spread() {
    println!("== test_wear_spread ==");
    let total = 4096usize;
    blkdev::set_disk(vec![0u8; total * blkdev::BLK_SIZE]);
    check("mkfs", mkfs(MAJOR) == 0);
    check("mount", reboot() == 0);
    let root = fs_root_ino(MAJOR);

    // 反复创建/删除不同大小的文件，观察分配是否散布（inode 上限内复用）
    let mut seen_blocks: BTreeSet<u32> = BTreeSet::new();
    for round in 0..200 {
        for i in 0..6 {
            let name = format!("f{}_{}", round, i);
            let data: Vec<u8> = (0..(500 + i * 700) as u32).map(|v| v as u8).collect();
            let ino = write_file(MAJOR, root, &name, &data);
            check(&format!("create {}", name), ino >= 0);
            if ino < 0 {
                return;
            }
            let mut inode = [0u8; INODE_SIZE];
            if read_inode(MAJOR, ino as u32, &mut inode) {
                for i in 0..NDIRECT {
                    let b = inode_direct(&inode, i);
                    if b != 0 {
                        seen_blocks.insert(b);
                    }
                }
            }
        }
        // 全部删除（inode 回收复用）
        for i in 0..6 {
            let name = format!("f{}_{}", round, i);
            fs_unlink(MAJOR, root, &name);
        }
        if round % 40 == 39 {
            check(&format!("fsck round {}", round), sfs_check(MAJOR) == 0);
        }
    }
    check("fsck clean", sfs_check(MAJOR) == 0);
    let blocks: Vec<u32> = seen_blocks.into_iter().collect();
    let ndata = fs_total_blocks(MAJOR);
    let spread = if blocks.len() >= 4 {
        let min = blocks.first().copied().unwrap();
        let max = blocks.last().copied().unwrap();
        (max - min) as f64 / ndata as f64
    } else {
        0.0
    };
    println!("  spread: {} blocks, range fraction {:.2}", blocks.len(), spread);
    check("blocks spread across disk", spread > 0.25);
    let (tw, minw, maxw) = fs_wear_report();
    println!("  wear total={} min={} max={}", tw, minw, maxw);
    // 日志区（环形）与位图区集中写是设计使然；数据区已散布，跳过比率断言
}

fn test_journal_wrap() {
    println!("== test_journal_wrap ==");
    // 小盘：jlog = max(64, 2048/8=256) = 256 → 128 条记录/周期；
    // 单文件反复写入（每次 fs_write = 一个事务）必然触发多次检查点与环复用
    let total = 2048usize;
    blkdev::set_disk(vec![0u8; total * blkdev::BLK_SIZE]);
    check("mkfs", mkfs(MAJOR) == 0);
    check("mount", reboot() == 0);
    let root = fs_root_ino(MAJOR);
    let ino = fs_create(MAJOR, root, "w", T_REG, 0);
    check("create", ino >= 0);

    let mut expected: Vec<u8> = Vec::new();
    for i in 0..400 {
        let chunk = format!("chunk-{:04}-", i).into_bytes();
        let off = i * 5000; // 跨块，每写触发分配/位图/间接元数据
        let n = fs_write(MAJOR, ino as u32, off, &chunk);
        check(&format!("write {}", i), n == chunk.len() as i32);
        if n != chunk.len() as i32 {
            return;
        }
        expected.extend(chunk);
        if i % 100 == 99 {
            check(&format!("fsck at {}", i), sfs_check(MAJOR) == 0);
        }
    }
    check("final fsck", sfs_check(MAJOR) == 0);
    check("fsync", fs_checkpoint(MAJOR) == 0);
    check("fsck after fsync", sfs_check(MAJOR) == 0);

    // 重挂载后数据仍在
    check("remount", reboot() == 0);
    check("fsck after remount", sfs_check(MAJOR) == 0);
    // 按偏移逐块校验（文件有空洞）
    let mut intact = true;
    for i in 0..400 {
        let chunk = format!("chunk-{:04}-", i).into_bytes();
        let off = i * 5000;
        let mut buf = [0u8; 32];
        let r = fs_read(MAJOR, ino as u32, off, &mut buf);
        if r < chunk.len() as i32 || &buf[..chunk.len()] != chunk.as_slice() {
            intact = false;
            println!("  [FAIL] chunk {} mismatch", i);
            break;
        }
    }
    check("content intact", intact);
}

/// 掉电故障注入：在每次写之后把设备「断电」，重挂载验证恢复。
fn test_crash_recovery() {
    println!("== test_crash_recovery ==");
    let total = 8192usize;
    let mut disk = vec![0u8; total * blkdev::BLK_SIZE];
    blkdev::set_disk(disk.clone());
    check("mkfs", mkfs(MAJOR) == 0);
    check("mount", reboot() == 0);
    let root = fs_root_ino(MAJOR);

    let baseline: Vec<(String, Vec<u8>)> = (0..10)
        .map(|i| {
            let data = format!("baseline file {} content 0123456789", i).into_bytes();
            let name = format!("base{}", i);
            let ino = write_file(MAJOR, root, &name, &data);
            check(&format!("baseline {}", i), ino >= 0);
            (name, data)
        })
        .collect();
    check("baseline fsck", sfs_check(MAJOR) == 0);
    check("baseline fsync", fs_checkpoint(MAJOR) == 0);
    disk = blkdev::disk_bytes();

    let mut crash_points = 0usize;
    let mut crash_failures = 0usize;
    let mut dumped = false;
    for k in (1..220).step_by(3) {
        blkdev::set_disk(disk.clone());
        blkdev::set_fault(1, k); // Drop：第 k 次写之后断电
        blkdev::set_trace(true);
        jlog_set_debug(true);
        // 掉电可能落在挂载重放中途：此时挂载返回 -1 是正确防御
        //（不完整重放状态上不写入新记录），不视为 FS 损坏。能挂载才做写入。
        if reboot() == 0 {
            let root = fs_root_ino(MAJOR);
            for i in 0..6 {
                let name = format!("crash{}_{}", k, i);
                let data = format!("crash data {} round {}", i, k).into_bytes();
                let _ = write_file(MAJOR, root, &name, &data);
            }
        }
        // 模拟断电：不做 fsync/checkpoint，直接丢弃运行期状态
        power_off();

        // 重启：清除故障，挂载 → 恢复
        blkdev::clear_fault();
        let rec = reboot();
        check("reboot mount", rec == 0);
        if rec != 0 {
            crash_failures += 1;
            continue;
        }
        if sfs_check(MAJOR) != 0 {
            println!("  [DIAG] first fsck failure at k={}", k);
            if !dumped {
                dump_debug(MAJOR, &format!("k{}", k));
                let n = blkdev::trace_len();
                for i in 0..n {
                    if let Some((blk, ok)) = blkdev::trace_block(i) {
                        println!("  trace+{}: blk={} ok={}", i, blk, ok);
                    }
                }
                dumped = true;
            }
            crash_failures += 1;
        }
        for (name, data) in &baseline {
            let ino = fs_lookup(MAJOR, root, name);
            if ino < 0 || read_whole(MAJOR, ino as u32) != *data {
                println!("  [FAIL] baseline {} lost after crash@{}", name, k);
                crash_failures += 1;
            }
        }
        for i in 0..6 {
            let name = format!("crash{}_{}", k, i);
            let ino = fs_lookup(MAJOR, root, &name);
            if ino >= 0 {
                let data = format!("crash data {} round {}", i, k).into_bytes();
                let size = fs_size(MAJOR, ino as u32) as usize;
                // 允许：不存在 / size 0（create 已提交、write 未提交）/
                // 内容为 data 的前缀（write 部分提交），绝不允许内容撕裂
                let got = read_whole(MAJOR, ino as u32);
                if size > data.len() || got != data[..size] {
                    println!("  [FAIL] torn file {} (size {} vs {}) after crash@{}", name, size, data.len(), k);
                    crash_failures += 1;
                }
            }
        }
        crash_points += 1;
        disk = blkdev::disk_bytes();
    }
    check(
        &format!("crash recovery across {} points", crash_points),
        crash_failures == 0,
    );
    if crash_failures > 0 {
        println!("  crash_failures = {}", crash_failures);
    }
}

/// 撕裂写注入：第 k 次写只写半个块。
fn test_crash_tear() {
    println!("== test_crash_tear ==");
    power_off(); // 前序测试可能遗留 copy-out 失败标志
    let total = 8192usize;
    let mut disk = vec![0u8; total * blkdev::BLK_SIZE];
    blkdev::set_disk(disk.clone());
    check("mkfs", mkfs(MAJOR) == 0);
    check("mount", reboot() == 0);
    let root = fs_root_ino(MAJOR);
    let baseline: Vec<(String, Vec<u8>)> = (0..10)
        .map(|i| {
            let data = format!("tear baseline {} content abcdefgh", i).into_bytes();
            let name = format!("tb{}", i);
            let ino = write_file(MAJOR, root, &name, &data);
            check(&format!("baseline {}", i), ino >= 0);
            (name, data)
        })
        .collect();
    check("baseline fsync", fs_checkpoint(MAJOR) == 0);
    disk = blkdev::disk_bytes();

    let mut failures = 0usize;
    for k in (2..220).step_by(5) {
        blkdev::set_disk(disk.clone());
        blkdev::set_fault(2, k); // Tear
        check("mount", reboot() == 0);
        let root = fs_root_ino(MAJOR);
        for i in 0..5 {
            let name = format!("tear{}_{}", k, i);
            let data = format!("tear data {} round {}", i, k).into_bytes();
            let _ = write_file(MAJOR, root, &name, &data);
        }
        blkdev::clear_fault();
        power_off(); // 撕裂注入期间 copy-out 失败置位的标志随断电清零
        if reboot() != 0 {
            println!("  [FAIL] mount failed after tear@{}", k);
            failures += 1;
            disk = blkdev::disk_bytes();
            continue;
        }
        if sfs_check(MAJOR) != 0 {
            println!("  [FAIL] fsck after tear@{}", k);
            failures += 1;
        }
        for (name, data) in &baseline {
            let ino = fs_lookup(MAJOR, root, name);
            if ino < 0 || read_whole(MAJOR, ino as u32) != *data {
                println!("  [FAIL] baseline {} lost after tear@{}", name, k);
                failures += 1;
            }
        }
        disk = blkdev::disk_bytes();
    }
    check("tear recovery", failures == 0);
    if failures > 0 {
        println!("  tear_failures = {}", failures);
    }
}

/// 锁互斥验证：8 线程各自 fs_lock→临界区忙等 80µs→fs_unlock，记录临界区内
/// 同时驻留的最大线程数。fs_lock 真实互斥时最大并发必须 == 1。
fn test_lock_serialization() {
    println!("== test_lock_serialization ==");
    blkdev::set_disk(vec![0u8; 2048 * blkdev::BLK_SIZE]);
    check("mkfs", mkfs(MAJOR) == 0);

    let inside = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let maxin = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut hs = Vec::new();
    for _ in 0..8 {
        let (ci, co, mx) = (inside.clone(), inside.clone(), maxin.clone());
        hs.push(std::thread::spawn(move || {
            for _ in 0..400 {
                fs_lock();
                let cur = ci.fetch_add(1, Ordering::Relaxed) + 1;
                mx.fetch_max(cur, Ordering::Relaxed);
                let end = std::time::Instant::now() + std::time::Duration::from_micros(80);
                while std::time::Instant::now() < end {}
                co.fetch_sub(1, Ordering::Relaxed);
                fs_unlock();
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let m = maxin.load(Ordering::Relaxed);
    check(&format!("fs_lock mutual exclusion (max_inside={})", m), m == 1);
}

/// 并发回归（宿主多线程，模拟 EL0 多进程在 /var/run 上并发建/写/读/截/删）。
///
/// 每线程在自己的文件上执行 create→write 12KB→read 逐字节校验→truncate→
/// read 必须为空→unlink，重复 ITERS 轮。任何「截断后仍非空 / 内容串线 /
/// 写短 / 删除失败」均计失败。宿主 sched_cfs 桩的 tid 语义决定锁是否真实：
/// - 常量 tid（旧桩）：锁退化为递归无操作 —— 复现「无串行化」原始缺陷；
/// - 每线程独立 tid + sleep_until→yield：锁真实互斥 —— 验证修复。
fn test_concurrent_race() {
    println!("== test_concurrent_race (host threads) ==");
    let total = 8192usize;
    blkdev::set_disk(vec![0u8; total * blkdev::BLK_SIZE]);
    check("mkfs", mkfs(MAJOR) == 0);
    check("mount", reboot() == 0);
    let root = fs_root_ino(MAJOR);

    const NTHREADS: usize = 6;
    const ITERS: usize = 25;
    let nfail = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::new();
    for t in 0..NTHREADS {
        let f = nfail.clone();
        handles.push(std::thread::spawn(move || {
            let marker = (0x50u8 + t as u8);
            let data = vec![marker; 12288];
            let mut rbuf = [0u8; 12288];
            for it in 0..ITERS {
                let name = format!("h{}_{:06}", t, it);
                let ino = fs_create(MAJOR, root, &name, T_REG, 0);
                if ino < 0 {
                    f.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    println!("  [FAIL] create t={} it={}", t, it);
                    return;
                }
                let ino = ino as u32;
                // 分块写（与 sys_write 4096 上限对齐，也放大多事务交错窗口）
                let mut off = 0usize;
                while off < data.len() {
                    let chunk = data.len().min(off + 4096) - off;
                    let n = fs_write(MAJOR, ino, off, &data[off..off + chunk]);
                    if n != chunk as i32 {
                        f.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        println!("  [FAIL] write-short t={} it={} off={} got={}", t, it, off, n);
                        return;
                    }
                    off += n as usize;
                }
                if fs_read(MAJOR, ino, 0, &mut rbuf) != 12288 as i32 {
                    f.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    println!("  [FAIL] read t={} it={}", t, it);
                    return;
                }
                if rbuf.iter().any(|&b| b != marker) {
                    f.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    println!("  [FAIL] cross-talk t={} it={}", t, it);
                    return;
                }
                if fs_truncate(MAJOR, ino) != 0 {
                    f.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    println!("  [FAIL] truncate-rc t={} it={}", t, it);
                    return;
                }
                if fs_size(MAJOR, ino) != 0 {
                    f.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    println!("  [FAIL] truncate-size!=0 t={} it={}", t, it);
                    return;
                }
                let n = fs_read(MAJOR, ino, 0, &mut rbuf);
                if n != 0 {
                    f.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    println!("  [FAIL] truncate-not-empty t={} it={} got={}", t, it, n);
                    return;
                }
                if fs_unlink(MAJOR, root, &name) != 0 {
                    f.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    println!("  [FAIL] unlink t={} it={}", t, it);
                    return;
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let n = nfail.load(std::sync::atomic::Ordering::Relaxed);
    check(&format!("concurrent race {} threads no-fail", NTHREADS), n == 0);
    check("fsck clean after race", sfs_check(MAJOR) == 0);
}

/// Python 生成的镜像（tools/mkfs_sfs.py v2）可被内核挂载。
fn test_python_image(path: &str) {
    println!("== test_python_image ({}) ==", path);
    let data = std::fs::read(path).expect("read image");
    blkdev::set_disk(data);
    check("probe", fs_probe(MAJOR));
    check("mount", reboot() == 0);
    let root = fs_root_ino(MAJOR);
    check("root type", fs_type(MAJOR, root) == T_DIR);
    let mut idx = 0usize;
    let mut entries = 0usize;
    loop {
        let mut nm = [0u8; 28];
        let r = fs_readdir(MAJOR, root, idx, &mut nm);
        if r < 0 {
            break;
        }
        idx += 1;
        entries += 1;
    }
    println!("  root entries: {}", entries);
    check("fsck clean on python image", sfs_check(MAJOR) == 0);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let filter = args.get(1).map(|s| s.as_str());
    let mut ran = 0usize;

    let tests: Vec<(&str, fn())> = vec![
        ("basic", test_basic as fn()),
        ("large", test_large_sparse),
        ("wear", test_wear_spread),
        ("wrap", test_journal_wrap),
        ("crash", test_crash_recovery),
        ("tear", test_crash_tear),
        ("race", test_concurrent_race),
        ("lockser", test_lock_serialization),
    ];
    for (name, f) in tests {
        if filter.map_or(true, |flt| name.contains(flt)) {
            ran += 1;
            f();
        }
    }
    if filter.map_or(true, |flt| "python".contains(flt)) {
        let img = args
            .iter()
            .find(|a| a.ends_with(".img"))
            .cloned()
            .unwrap_or_else(|| "/tmp/sfs_host_rootfs.img".to_string());
        ran += 1;
        test_python_image(&img);
    }

    let fails = failures();
    println!("\n======== total failures: {} ========", fails);
    if fails == 0 && ran > 0 {
        println!("ALL HOST TESTS PASSED");
    } else {
        std::process::exit(1);
    }
}
