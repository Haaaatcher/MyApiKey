//! 统一错误类型。
//!
//! 设计约定：
//! - 全库只暴露这一种 [`Error`]，调用方（CLI/GUI）按变体决定提示文案与退出码；
//! - 所有错误信息面向最终用户，用**英文**表述可执行的补救动作
//!   （语言分层规范见 AGENTS.md；本文件内的中文注释面向开发者）；
//! - **任何变体的信息里都不得拼入 `api_key` 或主密码**——例如解密失败只说
//!   密码错误，绝不回显用户输入了什么。

use thiserror::Error;

/// MyApiKey 全库统一的错误类型。
#[derive(Debug, Error)]
pub enum Error {
    /// vault 文件头部不合法：长度不足、magic 不符、被截断等。
    /// 携带人类可读的具体原因（如“文件长度不足最小头部”）。
    #[error("invalid vault file: {0}")]
    Format(String),

    /// 主密码验证失败（AES-GCM 认证标签不匹配）或密文损坏。
    /// 注意：故意不区分“密码错”与“数据坏”，避免给攻击者提供可区分的信号。
    #[error("wrong master passphrase, or the vault data is corrupted")]
    Decrypt,

    /// vault 版本号高于本程序支持的最高版本（例如未来用新版程序加密过）。
    /// 携带读到的版本号。
    #[error("vault version {0} is newer than the supported version {SUPPORTED_VAULT_VERSION}; please upgrade MyApiKey")]
    UnsupportedVersion(u16),

    /// 远端已有更新版本（CAS 冲突：上传时携带的旧 sha 与远端当前
    /// 不一致）。同步流程捕获后走"下载 + LWW 合并 + 重传"，到达用户
    /// 面前时通常已被消化；不消化即提示跑 `mak sync`。
    #[error("the remote vault has changed since the last sync")]
    RemoteConflict,

    /// 底层 IO 错误的直接透传。
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON / TOML 序列化失败。携带上层语义（如“解析 config.toml”）。
    #[error("{0}")]
    Serde(String),

    /// 系统凭据库（Windows Credential Manager / macOS Keychain /
    /// Linux Secret Service）读写失败。
    /// Linux 无头环境下 Secret Service 常不可用，调用方应降级为手动输入而非报错中断。
    #[error("system credential store error: {0}")]
    Keyring(String),

    /// 按 id 或名称找不到记录。
    #[error("record not found: {0}")]
    NotFound(String),

    /// 本机尚未初始化（没有 vault / git 仓库），需要先运行 `init`。
    #[error("not initialized yet, run `mak init` first")]
    NotInitialized,

    /// 调用方使用错误（参数缺失、导入文件内容不对等）。
    #[error("{0}")]
    Usage(String),
}

/// 本程序支持读写的最高 vault 格式版本（见 [`crate::vault`] 的格式说明）。
pub const SUPPORTED_VAULT_VERSION: u16 = 1;

/// 全库统一的 `Result` 别名。
pub type Result<T> = std::result::Result<T, Error>;
