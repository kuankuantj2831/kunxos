# 无用代码、依赖与历史遗留检查

## 清理结果（2026-11-14）

用户授权后已执行以下清理；下文为清理前的检查记录。

- 删除 text_host 的六个未使用依赖，重新生成 Cargo.lock；`cargo run --offline` 编译并执行通过，输出 `ALL L10N OK`。
- 删除 stub 应用、默认构建/clean 引用及其专用字库。
- 删除未接入的 kunxui 控件库、shared/text_engine.rs 和专用 Emoji 字体；字体生成器同步移除 Emoji 生成逻辑。当前使用的 fontdue、Phosphor 图标、普通与粗体字体均保留。
- 删除 tools/backup_cjk、全量旧点阵字库、browser/settings 两份无引用字库，以及 19 份旧调试文本；其他仍被应用引用的点阵字库保留。
- 删除未使用的 GICv2 文件和旧 ARM64 kunxboot；移除失配的 boot/boot-pi4 与顶层 pi4 启动构建入口。保留现有 Pi 4 HAL 开发代码、OpenSBI 启动、磁盘签名打包、恢复应用和 OTA 路径。
- 内核目录 Makefile 改为转发到顶层 RISC-V 构建；更新 README、lint 默认目标、宿主 crate 分类及 CI 文件枚举，避免扫描本地第三方工具链和缓存。
- 通过 Cargo clean 清理扫描到的 62 个 target 目录，清理前合计约 3,478.5 MiB（3.40 GiB）；验证测试的产物放在系统临时目录。
- Python 语法、Bash 语法、Cargo 清单和 Make 数据库解析检查通过；Make 返回查询模式的“目标需更新”状态，并报告已有镜像的时间戳超前。交叉编译与 QEMU 启动仍受本机工具链限制，未验证。
- 最终检查：59 个跟踪文件已删除；活动 `#[path]` 引用未发现缺失文件，仓库内没有剩余 target 目录。CJK 子集生成器实际生成 461 个字形成功；修改文件的 diff 空白检查通过。

没有改写历史 CHANGELOG 和旧审查文档中的模块名；它们保留历史记录用途。

检查日期：2026-11-14。检查对象为当前工作区；检查期间存在其他工作区修改，本报告是扫描时的快照。

## 结论与范围

确实存在未接入的库、无效构建依赖、未使用的依赖声明、旧平台文件和调试产物。已对 Git 跟踪文件进行清单扫描（扫描时 395 个 Rust 文件，约 206,361 行），交叉检查 Cargo 清单、Rust 模块声明、共享源码引入、Makefile、CI 和启动配置。未删除源码、依赖、镜像或用户文件。

这是静态检查加少量编译验证，不是全系统运行覆盖证明。库的公开接口、汇编导出符号、测试程序、可选平台后端不能仅凭“名字出现少”判定可删。以下“未引用”结论均限定于当前仓库与构建路径。

## 可以优先处理的项目

### 1. text_host 的六个直接依赖没有被现有程序使用（高置信度）

- 位置：`tests/text_host/Cargo.toml:6`。
- 依赖：`rustybuzz`、`unicode-bidi`、`unicode-linebreak`、`unicode-script`、`fontdue`、`miniz_oxide`。
- `tests/text_host/src/main.rs:1` 只引入 `shared/l10n.rs`；该模块只使用标准库/内建 alloc，不引入文本引擎或以上依赖。
- 验证：直接执行 `rustc --edition 2021 --crate-type bin --emit metadata tests/text_host/src/main.rs` 成功，没有传入任何外部 crate。该检查验证了类型检查阶段不需要这六个依赖，未进行链接或运行测试。
- 建议：删除六个依赖声明并更新该 crate 的锁文件。减少解析、下载与构建依赖的成本；不能据此声称最终 ELF 一定缩小。

### 2. stub 已无镜像用途，却仍被默认构建（高置信度）

- 位置：`Makefile:268` 定义编译规则，`Makefile:328` 的 launcher 目标和 `Makefile:392` 的 ramdisk 目标仍依赖 `$(STUB_ELF)`。
- ramdisk 规则复制了 phone、sms、contacts 等独立实现，没有复制 STUB_ELF；没有找到 `/bin/stub` 的运行调用。
- `README.md` 也说明各应用已有独立实现，不再共用 stub。
- `apps/stub` 有约 924 行 Rust 源码，目录内跟踪文件约 33.5 KiB。
- 建议：先去掉默认目标中的 STUB_ELF 依赖；如需模板，保留为显式手动构建目标，否则连同 `shared/cjk_font_stub.rs` 一并评估删除。

### 3. 旧 CJK 备份未接入当前构建（高置信度）

- `tools/backup_cjk/` 有三份旧 Rust 源码，共约 4,388 行、145.5 KiB。
- 没有找到当前构建或模块声明对该备份目录的引用。
- `shared/cjk_font_full.rs` 约 755.1 KiB，源码引入只出现在旧备份 `tools/backup_cjk/ui_framework_cjk.rs` 中；`tools/gen_cjk_font.py` 还保留生成该文件的用法示例。
- 当前内核 `ui_framework.rs` 使用 `shared/font_render.rs`。
- 建议：如果不再保留旧点阵实现，可先删除备份目录，再清理仅供该备份使用的全量字库。生成工具仍可能用于其他应用的字库，不能整份一起删。

### 4. 两份应用点阵字库已无源码引入（高置信度）

- `shared/cjk_font_browser.rs`：335 行、约 38.8 KiB。
- `shared/cjk_font_settings.rs`：386 行、约 46.5 KiB。
- 未找到其他被跟踪源码、脚本或构建文件对这两个文件名的引用；当前应用走共享字体渲染路径。
- 建议：作为独立清理项，删除后编译 browser/settings 验证。其他应用的 CJK 文件仍有引入，不能批量删除整个系列。

## 未接入，但删除前应决定是否保留功能

### 5. kunxui 是当前系统未使用的独立控件库

- 位置：`kunxui/Cargo.toml:1`、`kunxui/src/lib.rs:1`。
- 全仓 Cargo 清单没有对 kunxui 的依赖，应用也没有通过 path 引入它的源码或控件；相关类名仅在该库自身及示例/测试中出现。
- 约 3,100 行 Rust；跟踪文件约 104.5 KiB。
- `docs/NIGHT_PLAN.md:132` 已明确记录：保留为后续设置应用控件库候选。因此这属于有意保留、尚未接入的代码。
- `cargo check --offline --manifest-path kunxui/Cargo.toml` 在 Windows 宿主检查通过。能编译不代表系统在使用。
- CI 会遍历 Cargo.toml，因此该库虽然没有运行调用，仍会增加 CI 构建工作。
- 建议：确定还要不要这套控件；需要则安排接入，不需要则归档/删除。

### 6. 完整文本引擎未接入应用，现有 text_host 也未验证它

- `shared/text_engine.rs` 有约 1,140 行、36.3 KiB，包含 shaping、BiDi、换行、Emoji 等逻辑。
- 未找到其他源码对该文件的模块引入；现有 `tests/text_host` 只引入 l10n。
- `shared/l10n.rs` 当前只有宿主测试引用，未见应用接入；它至少有测试用途，不能等同于完全无用。
- `fonts/emoji-full.ttf` 约 10.18 MiB、`tools/fonts/emoji-subset.ttf` 约 4.53 MiB：生成器仍使用它们，但当前 Makefile 未将 Emoji 字体复制到 rootfs，也未见活动文本引擎加载路径。
- 建议：如果要保留完整文本能力，补实际接入和针对文本引擎的测试；如果已经放弃，可一起整理源码、测试依赖和相关字体。不能只删字体而留下默认仍执行 `build_emoji()` 的生成脚本。

## 平台迁移与检查机制遗留

### 7. ARM64 → RISC-V 迁移没有收尾

- 顶层 `Makefile:82` 使用 `riscv64gc-unknown-none-elf`，内核入口已使用 PLIC/SBI。
- `kunxcore/src/gic.rs` 是旧 GICv2 实现，149 行，当前 main.rs 没有声明该模块，也未找到活动源码引入。对当前 RISC-V 内核属于未编译文件；若保留 ARM 恢复计划，应归档到对应平台目录。
- `kunxcore/Makefile:3` 仍默认 aarch64，QEMU 仍为 qemu-system-aarch64；README 的主架构和构建命令也仍是 ARM64。
- `tools/lint.sh:19` 默认 aarch64，且没有按宿主/裸机分类；CI 传入的 TARGET 能覆盖此默认，但直接运行该脚本会用错目标，遍历也会纳入宿主 crate 与本地第三方目录。
- `kunxboot/Makefile:3` 仍编译 ARM64，而顶层 `Makefile:736` 的 boot 目标把它交给 RISC-V QEMU 加载；CI 已明确排除 kunxboot。这是遗留的失配启动路径，不能当作已经支持的 RISC-V 引导链，也不应未核对恢复/签名功能就整目录删除。

### 8. 内核全局屏蔽死代码告警

- 位置：`kunxcore/src/main.rs:5-8` 的 `#![allow(dead_code)]`。
- 该设置让废弃接口和未调用函数长期不产生编译告警；kunxui 和 stub 也有类似设置。
- 建议：先在专用检查中用 `--force-warn dead_code` 收集清单，再逐项判断；只对寄存器常量、预留接口、特定构建配置等明确情况局部 allow。不要直接批量删除告警项。

## 磁盘与仓库杂物

- 各 `target/` 目录合计约 **3,296.5 MiB（3.22 GiB）**，其中 `kos/target` 约 1,612.5 MiB。属于可重新生成的编译缓存，可按需清理；重新编译会消耗时间，不属于源码冗余。
- `tools/gdb_*.txt`、`tools/rsp_capture.txt` 共 19 个被 Git 跟踪的调试文本，约 27.3 KiB，未发现构建输入引用，可评估从仓库移除并添加对应忽略规则。
- rootfs 字体与 tools/fonts 子集，以及 rootfs/share/app_icons.rgba 与 shared/app_icons.rgba 有相同副本；这是 Makefile 明确执行复制得到的部署文件，不应仅按哈希重复就删除源文件或运行时副本。
- vendor、交叉工具链、源字体、测试应用都存在实际构建/运行或重建用途，不列为可直接删除项。
- `fontdue` 通过 `shared/font_render.rs:21`、`shared/text_engine.rs` 被使用，应用通过共享源码获得依赖。扫描单个应用目录找不到 fontdue 字样，是误判风险。
- `kunxcore/src/kos_trust_pk.rs` 经 `kos_vfs.rs:24` 的 include! 引入，不是孤立文件；AC97、内核 UI 框架也有实际调用。

## 验证边界与建议顺序

已完成：文件/模块/依赖/构建路径静态核对，kunxui 的宿主 cargo check，以及 text_host 无第三方 crate 的 rustc 元数据编译。

未完成：全内核与全部应用交叉编译、完整 dead_code 编译器清单和 QEMU 回归。本机只安装 Windows 宿主 Rust target；尝试离线内核 cargo check 时，在解析阶段即因缺少 fontdue 缓存失败，尚未进入 RISC-V 编译。

推荐清理顺序：先处理 text_host 依赖、stub 默认构建、旧 CJK 文件和调试文本；再决定 kunxui/文本引擎的去留；最后统一平台入口与 lint 目标，并在交叉编译环境启用死代码检查。每批清理后只运行受影响 crate 与启动路径的验证。
