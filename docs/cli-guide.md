# MyApiKey 使用指南

> 加密记录你的 AI API 凭证（`base_url` + `api_key`），并通过你自己的私有 git 仓库同步到所有设备。
> 程序界面为英文，本指南为中文说明。

[快速上手](#快速上手) • [多设备](#在另一台设备上使用) • [日常任务](#日常任务) • [数据与安全](#数据与安全) • [命令参考](#命令参考) • [安装](#安装)

## 快速上手

一台新设备，从零到用起来，只要一条命令：

```console
$ mak init --github
Set the master passphrase.
  If you forget it, ALL data is unrecoverable — keep it in a password manager.
New master passphrase: ********
Confirm passphrase: ********
GitHub authorization (one-time only):
  1. Open: https://github.com/login/device
  2. Enter this code: 8163-2A61
Waiting for authorization… this continues automatically.
✓ Authorized — token saved to the credential store (clear with `mak forget --github`)
✓ Sync repository ready (private): https://github.com/<你>/myapikey-vault.git
Remember the passphrase in the system credential store? (no re-entry next time) [Y/n] y

✓ Initialized — 0 record(s)
  Repository: ~/.config/myapikey/repo
```

它会自动在你的 GitHub 账号下创建私有同步仓库（默认名 `myapikey-vault`）。不想用 GitHub 自动创建？[自己建一个空仓库](#安装)后用 `mak init --remote <地址>`。

> [!WARNING]
> 主密码用于加密全部记录，**没有找回功能**——忘记 = 数据丢失，请存入密码管理器。所有设备必须使用同一个主密码。

记录第一条凭证：

```console
$ mak add
Record name (e.g. OpenAI primary): OpenAI 主力号
Protocol [1]openai [2]anthropic [3]custom text (Enter keeps openai): 1
Base URL (stored as-is, no normalization): https://api.openai.com/v1
API Key: sk-proj-********
Notes (Enter to skip):
✓ Pushed to remote
✓ Added "OpenAI 主力号" (id: 01a1683f…)
Key check: invalid (HTTP 401)
```

保存时自动校验一次 key（这里因为是假 key 所以显示 invalid，不影响保存）。

日常三件套：

```console
$ mak list
#    Name               Protocol     BASE URL                           KEY                  Check
--------------------------------------------------------------------------------------------------------
1    OpenAI 主力号      openai       https://api.openai.com/v1          sk-p...0000          invalid
--------------------------------------------------------------------------------------------------------

$ mak show 1
Name       : OpenAI 主力号
Protocol   : openai
Base URL   : https://api.openai.com/v1
API Key    : sk-p...0000
（……其余字段……）

Use `mak show 1 --copy` to copy the full key to the clipboard.

$ mak sync
✓ Up to date (local matches remote)
```

`show` 的目标可以是列表序号（`1`）、名称（`OpenAI 主力号`）或 id 前缀，怎么短怎么来。**key 在任何输出里都是掩码**——完整 key 只通过 `--copy` 进剪贴板，不会出现在终端上（防截图/录屏/终端缓冲泄露）。

## 在另一台设备上使用

新设备上跑同一条命令，浏览器授权后自动找到你的仓库并拉下全部记录——**输入与第一台设备相同的主密码**：

```console
$ mak init --github
(using the saved GitHub authorization)
✓ Sync repository ready (private): https://github.com/<你>/myapikey-vault.git
✓ Joined from remote: https://github.com/<你>/myapikey-vault.git
✓ Initialized — 1 record(s)
```

之后：改完自动推送，开机 `mak sync` 一次拿最新。两台设备离线各改各的也没关系，`sync` 会按最后修改时间自动合并。

## 日常任务

**复制某个 key**（完整 key 只进剪贴板，终端永远掩码；粘贴后建议手动清一次剪贴板）：

```bash
mak show 1 --copy
```

**验证 key 是否还有效**（真发一次 `/models` 请求，不消耗 token）：

```bash
mak check              # 全部
mak check OpenAI主力号  # 单条
```

结论对照：`valid` 有效 / `invalid` 无效 / `no permission` 有效但无权限 / `rate limited` 有效但限额 / `unverifiable` 平台不支持探测。

**修改一条记录**（逐项询问，回车保留原值，备注输 `-` 清空）：

```bash
mak edit 1
```

**删除一条记录**：

```bash
mak delete 1
```

**换绑同步仓库**：

```bash
mak remote set <新仓库地址>
```

**改主密码**（全部记录重新加密并推送，其他设备下次同步用新密码解锁）：

```bash
mak passwd
```

**备份 / 迁移**（导出含明文 key，两次确认，用完即删）：

```bash
mak export backup.json
mak import backup.json
```

**锁定本机（借出电脑前）**：

```bash
mak forget              # 立即清除记住的主密码；下次命令输一次后重新记住
mak forget --github     # 只清 GitHub 授权
```

主密码默认**输入一次记住 7 天**（存在系统凭据库），期间所有命令免输入，每次输入重新续期——至多每周输一次。有效期在 `config.toml` 的 `remember_days` 调整（设 0 = 永不记住）。

## 数据与安全

- **本机数据**：`~/.config/myapikey`（macOS 在 `~/Library/Application Support`，Windows 在 `%APPDATA%`）；
- **云端只有密文**：仓库里的 `vault.mkv` 是 Argon2id + AES-256-GCM 加密文件，仓库泄露也无法读取；
- **主密码与 GitHub 授权**只存各设备的系统凭据库，永远不进仓库；
- 删除采用墓碑机制：已删记录不会从旧设备复活。

## 命令参考

程序自带完整帮助，以下是 `mak --help` 原文；每个子命令还有 `mak <命令> --help`：

```text
Usage: mak <COMMAND>

Commands:
  init    Initialize: set the master passphrase, create the local repo, optionally bind a remote
  add     Add a record interactively (omitted fields are prompted)
  list    List all records (keys are masked)
  show    Show one record (key is always masked; use --copy to put it on the clipboard)
  edit    Edit a record field by field (Enter keeps the current value)
  delete  Delete a record (tombstone; purged after the grace period)
  check   Check key validity via GET /models (all records if no target)
  sync    Sync: pull + merge + push
  status  Show repository and sync status
  remote  Manage the remote repository
  passwd  Change the master passphrase
  forget  Clear the remembered passphrase
  export  Export all records to a JSON file in PLAINTEXT (double confirmation)
  import  Import records from a JSON file

Options:
  -h, --help     Print help
  -V, --version  Print version
```

## 常见问题

**推送时提示 push failed？** 远端有其他设备的新数据，`mak sync` 一次即可（自动合并后再推）。

**GitHub 设备码过期了？** 15 分钟有效，重新运行命令即可。

**主密码忘了？** 无法找回。删除本机配置目录与远端仓库，从头 `mak init`。

**提示 not initialized？** 这台设备还没初始化，先跑 `mak init --github`。

**提示 git is required？** 程序依赖 git 做历史与同步，按报错里给出的命令安装即可（如 `winget install Git.Git`）。

## 安装

正常运行只需要 git（缺失时程序会给出安装命令）。

获取程序：

```bash
# 方式一：源码安装（当前可用，需要 Rust 工具链）
cargo install --git https://github.com/Haaaatcher/MyApiKey

# 方式二：下载预编译可执行文件（发布后可用，零依赖）
# → GitHub Releases: https://github.com/Haaaatcher/MyApiKey/releases
```

安装后得到两个等价命令：`mak`（短）与 `myapikey`（全称）。
