//! KunxOS 日历 / 联系人同步栈（no_std、无堆、无第三方依赖）。
//!
//! 模块地图：
//! ```text
//! util      —— 字节工具 / base64 / hex / URL / FNV-1a / 时间换算
//! model     —— 数据模型：Event / Contact / Account / Collection / 同步标志
//! ical      —— iCalendar 解析 / 生成 / RRULE 展开 / 时区简化
//! vcard     —— vCard 3.0 / 4.0 解析 / 生成（照片、分组）
//! fs        —— 文件系统抽象（应用接 SFS 裸 syscall；测试接 std）
//! store     —— 本地记录存储：索引 + Blob、查询 API、墓碑压实
//! accounts  —— 账户 / 集合 / KeyStore（OAuth2 预留）
//! http      —— HTTP/1.1 客户端（抽象传输）
//! xml       —— 极简 XML 解析 / DAV 请求体构建
//! dav       —— DAV 核心：请求管道 / 发现 / multistatus 解析
//! caldav    —— CalDAV（RFC 4791）：sync-collection / query / multiget / PUT/DELETE
//! carddav   —— CardDAV（RFC 6352）：地址簿发现 / 联系人 CRUD / 分组
//! sync      —— 同步引擎：变更检测 / 增量同步 / 冲突解决 / 离线恢复
//! ```
//!
//! 使用方式：应用与宿主测试 crate 均经
//! `#[path = "../../../shared/sync/mod.rs"] mod sync;` 引入；
//! 平台差异（FS / 网络）通过 `fs::Fs` 与 `http::Net` trait 注入。
//! 详细设计见 docs/sync.md。

pub mod accounts;
pub mod caldav;
pub mod carddav;
pub mod dav;
pub mod fs;
pub mod http;
pub mod ical;
pub mod model;
pub mod store;
pub mod sync;
pub mod util;
pub mod vcard;
pub mod xml;

#[allow(unused_imports)]
pub use model::*;
