//! 参数定义（clap derive）与子命令分发。
//!
//! 语言分层（AGENTS.md）：**帮助文本用英文**（通过显式 `about` / `help` 属性），
//! 本文件中的中文文档注释面向开发者，两者并存时 clap 以属性为准。

use crate::commands;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// 顶层命令行定义。`mak` 与 `myapikey` 共用。
#[derive(Debug, Parser)]
#[command(
    name = "myapikey",
    version,
    about = "Record and sync AI API credentials. Encrypted, synced via your own GitHub private repo",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// 全部子命令（docs/design.md §7）。
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 初始化：设主密码、建本地仓库，可选绑远端。
    #[command(
        about = "Initialize: set the master passphrase, create the local repo, optionally bind a remote"
    )]
    /// 初始化本机（已初始化时自动转为保险合并式重绑，数据不丢）。
    ///
    /// 自动判断三件事：未授权 → 浏览器授权；仓库不存在 → 自动创建
    /// 私有仓库；远端有数据 → 下载并与本地合并。
    #[command(
        about = "Initialize: authorize GitHub if needed, create/reuse the private repo, download or upload the vault"
    )]
    Init {
        /// 仓库（可跳过交互直接指定）：`owner/name`、完整 GitHub URL，
        /// 或裸仓库名（归属当前授权账号）。缺省 = 交互式询问。
        #[arg(
            help = "GitHub repo (owner/name, full URL, or bare name); omit to be asked interactively"
        )]
        repo: Option<String>,
    },

    /// 交互式添加一条记录（缺省的参数会逐项询问）。
    #[command(about = "Add a record interactively. Omitted fields are prompted")]
    Add {
        /// 记录名称。
        #[arg(short, long, help = "Record name")]
        name: Option<String>,
        /// 协议标签。
        #[arg(
            short,
            long,
            help = "Protocol label: openai | anthropic | any custom text"
        )]
        protocol: Option<String>,
        /// base URL。
        #[arg(short, long, help = "Base URL")]
        base_url: Option<String>,
        /// API key。
        #[arg(
            short = 'k',
            long,
            help = "API key. Hidden input if omitted, recommended"
        )]
        api_key: Option<String>,
        /// 模型名（可重复；交互时也支持逗号分隔）。
        #[arg(
            long = "model",
            short = 'm',
            help = "Model name (repeatable, or comma-separated when prompted)"
        )]
        models: Vec<String>,
        /// 备注。
        #[arg(long, help = "Notes, optional")]
        notes: Option<String>,
    },

    /// 联网检查新 Release 并提示升级命令。
    #[command(about = "Check for a newer release and print the upgrade command")]
    Update,

    /// 列出全部记录（key 掩码显示）。
    #[command(about = "List all records. Keys are masked")]
    List,

    /// 模糊搜索记录（名称 / Base URL / 备注）。
    #[command(about = "Search records (fuzzy match on name, URL, notes)")]
    Search {
        /// 查询词：模糊匹配名称、Base URL、备注。
        #[arg(help = "Search query")]
        query: String,
    },

    /// 查看单条记录详情（key 永远掩码）。
    #[command(
        about = "Show one record. Key is always masked; use --copy to put it on the clipboard"
    )]
    Show {
        /// 记录的 id（前缀即可）或名称。
        #[arg(help = "Record id, prefix ok, or name")]
        target: String,
        /// 复制 key。
        #[arg(long, help = "Copy the API key to the clipboard")]
        copy: bool,
    },

    /// 编辑一条记录。
    #[command(about = "Edit a record field by field. Enter keeps the current value")]
    Edit {
        /// 记录的 id（前缀即可）或名称。
        #[arg(help = "Record id, prefix ok, or name")]
        target: String,
    },

    /// 删除一条记录（墓碑）。
    #[command(about = "Delete a record. Keeps a tombstone, purged after the grace period")]
    Delete {
        /// 记录的 id（前缀即可）或名称。
        #[arg(help = "Record id, prefix ok, or name")]
        target: String,
    },

    /// 校验 key 有效性。
    #[command(about = "Check key validity via GET /models. Checks all records if no target given")]
    Check {
        /// 记录的 id（前缀即可）或名称。
        #[arg(help = "Record id, prefix ok, or name")]
        target: Option<String>,
    },

    /// 同步。
    #[command(about = "Sync: pull + merge + push")]
    Sync,

    /// 查看仓库与同步状态。
    #[command(about = "Show repository and sync status")]
    Status,

    /// 管理远端仓库。
    #[command(about = "Manage the remote repository")]
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },

    /// 修改主密码（无参）；或清除本机记住的密码副本（`unset` 子命令）。
    #[command(
        subcommand_required = false,
        about = "Change the master passphrase, or `unset` to clear the remembered copy on this machine"
    )]
    Passwd {
        #[command(subcommand)]
        action: Option<PasswdAction>,
    },

    /// 明文导出。
    #[command(about = "Export all records to a JSON file in PLAINTEXT. Asks twice before writing")]
    Export {
        /// 导出文件路径。
        #[arg(help = "Output file path")]
        path: PathBuf,
    },

    /// 导入。
    #[command(about = "Import records from a JSON file")]
    Import {
        /// 导入文件路径。
        #[arg(help = "Input file path")]
        path: PathBuf,
    },
}

/// `remote` 子命令。
#[derive(Debug, Subcommand)]
pub enum RemoteAction {
    /// 设置或更换远端仓库地址。
    #[command(about = "Set or replace the remote repository URL")]
    Set { url: String },
    /// 查看当前远端配置。
    #[command(about = "Show the current remote")]
    Show,
    /// 撤销绑定：解绑远端并清除本机的 GitHub 授权（回到单机；vault 不动）。
    #[command(
        about = "Unbind the remote and forget the GitHub authorization on this machine (the vault is untouched)"
    )]
    Unset,
}

/// `mak passwd` 的子命令。
#[derive(Debug, clap::Subcommand)]
pub enum PasswdAction {
    /// 修改主密码（`mak passwd` 无参的等价显式形式）。
    #[command(about = "Change the master passphrase")]
    Set,
    /// 清除本机记住的主密码（立即锁定；vault 不变）。
    #[command(
        about = "Clear the remembered master passphrase on this machine (locks immediately; the vault is untouched)"
    )]
    Unset,
}

/// 入口：控制台编码守卫 → 解析 + 分发。
///
/// 守卫必须在任何输出之前建立、存活到 run 结束（Drop 时恢复代码页），
/// 因此放在函数体第一行。
///
/// # Errors
/// 子命令的任何错误原样上抛。
pub fn run() -> myapikey_core::Result<()> {
    let _console_guard = crate::console::enter_utf8();
    let cli = Cli::parse();
    match cli.command {
        Command::Init { repo } => commands::init(repo.as_deref()),
        Command::Add {
            name,
            protocol,
            base_url,
            api_key,
            models,
            notes,
        } => commands::add(name, protocol, base_url, api_key, models, notes),
        Command::Update => commands::update(),
        Command::List => commands::list(None),
        Command::Search { query } => commands::list(Some(&query)),
        Command::Show { target, copy } => commands::show(&target, copy),
        Command::Edit { target } => commands::edit(&target),
        Command::Delete { target } => commands::delete(&target),
        Command::Check { target } => commands::check(target.as_deref()),
        Command::Sync => commands::sync(),
        Command::Status => commands::status(),
        Command::Remote { action } => commands::remote(action),
        Command::Passwd { action } => commands::passwd(action),
        Command::Export { path } => commands::export(&path),
        Command::Import { path } => commands::import(&path),
    }
}
