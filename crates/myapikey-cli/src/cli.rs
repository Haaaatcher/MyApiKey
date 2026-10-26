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
    about = "Record and sync AI API credentials. Encrypted, git-backed, multi-device",
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
    Init {
        /// 远端仓库地址（也可以之后再 `remote set`）。
        #[arg(
            long,
            help = "Remote repository URL. You can also bind it later via `remote set`"
        )]
        remote: Option<String>,
        /// GitHub 一键模式。
        #[arg(
            long,
            help = "One-click GitHub mode: authorize in a browser, then auto-create/reuse a private sync repo. Optional value is the repo name, default myapikey-vault",
            num_args = 0..=1,
            default_missing_value = "myapikey-vault"
        )]
        github: Option<String>,
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
        /// 备注。
        #[arg(long, help = "Notes, optional")]
        notes: Option<String>,
    },

    /// 只显示当前版本（不联网）。
    #[command(about = "Show the current version")]
    Version,

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

    /// 修改主密码。
    #[command(about = "Change the master passphrase")]
    Passwd,

    /// 清除记住的密码。
    #[command(about = "Clear the remembered passphrase")]
    Forget {
        /// 只清 GitHub token。
        #[arg(
            long,
            help = "Clear only the GitHub token. Keeps the remembered passphrase"
        )]
        github: bool,
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
        Command::Init { remote, github } => commands::init(remote.as_deref(), github.as_deref()),
        Command::Add {
            name,
            protocol,
            base_url,
            api_key,
            notes,
        } => commands::add(name, protocol, base_url, api_key, notes),
        Command::Version => commands::version(),
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
        Command::Passwd => commands::passwd(),
        Command::Forget { github } => commands::forget(github),
        Command::Export { path } => commands::export(&path),
        Command::Import { path } => commands::import(&path),
    }
}
