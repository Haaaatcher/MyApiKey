//! # myapikey-core
//!
//! MyApiKey 的核心逻辑库，与任何界面（CLI / GUI）完全解耦。
//!
//! 模块职责一览：
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`error`] | 统一错误类型（`thiserror`），全库只用一种 `Error` |
//! | [`model`] | 数据模型：[`Record`](model::Record)（一条凭证记录）与 [`Protocol`](model::Protocol) |
//! | [`vault`] | vault 文件的加解密：Argon2id 派生 + AES-256-GCM，格式带版本与 AAD |
//! | [`merge`] | 多设备记录合并：按 `updated_at` 的 last-write-wins + 删除墓碑 |
//! | [`paths`] | 各操作系统上的目录布局，支持 `MYAPIKEY_HOME` 覆盖（测试/便携用途） |
//! | [`config`] | 非敏感配置（`config.toml`）：墓碑保留期、自动推送开关等 |
//! | [`lock`] | 实例文件锁：防止 CLI 与 GUI 同时写坏 vault |
//! | [`keychain`] | 系统凭据库（记住主密码）的读写封装 |
//! | [`store`] | 本地 git 仓库管理（`git init` / commit / remote），通过 shell 调用系统 git |
//! | [`subprocess`] | 子进程启动工具：Windows GUI 下消除子进程控制台闪窗 |
//! | [`sync`] | 同步引擎：fetch → 解密合并 → commit → push |
//! | [`probe`] | key 有效性探测：按协议调用 `GET /models` |
//! | [`check_cache`] | 校验结果的本地缓存（不参与同步） |
//!
//! 安全约束（全库通用，违反即设计缺陷）：
//! - 主密码与派生密钥使用 [`zeroize::Zeroizing`] 包裹，离开作用域即清零；
//! - 任何日志、错误信息、Display 输出中都不得出现 `api_key` 或主密码（含部分明文）；
//! - `api_key` 明文只允许出现在：内存中的 [`Record`](model::Record)、解密后的 vault 数据、
//!   剪贴板（`show --copy`）与用户双重确认的导出文件中——**任何终端输出一律掩码**。

pub mod check_cache;
pub mod config;
pub mod error;
pub mod github;
pub mod keychain;
pub mod lock;
pub mod merge;
pub mod model;
pub mod paths;
pub mod probe;
pub mod search;
pub mod store;
pub mod subprocess;
pub mod sync;
pub mod vault;

pub use error::{Error, Result};
