//! KunxOS 同步栈宿主测试 crate。
//!
//! 两种编译模式：
//!   - `cargo test`（宿主 / x86_64，cfg(test)）：std 可用，运行全部
//!     单元测试与 mock DAV 服务器集成测试；
//!   - `cargo build --target aarch64-unknown-none`（CI 全仓构建循环）：
//!     非 test 模式编译为 no_std rlib，保证同步栈在裸机目标可编译。
//!
//! 平台实现（host）：
//!   - `host::HostFs`：std::fs 整文件语义（匹配 Fs trait）
//!   - `host::HostNet`：std::net::TcpStream 传输
//!   - `host::MemFs`：内存文件系统（测试隔离用）

#![cfg_attr(not(test), no_std)]
#![allow(dead_code)]

#[path = "../../shared/sync/mod.rs"]
pub mod sync;

#[cfg(test)]
mod host;
#[cfg(test)]
mod integration;
