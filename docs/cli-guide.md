# MyApiKey 使用指南

> 加密记录你的 AI API 凭证（`base_url` + `api_key`），并通过你自己的 GitHub 私有仓库同步到所有设备（零外部依赖：不需要安装 git）。
> 程序界面为英文，本指南为中文说明。

[快速上手](#快速上手) • [多设备](#在另一台设备上使用) • [日常任务](#日常任务) • [数据与安全](#数据与安全) • [命令参考](#命令参考) • [安装](#安装)

## 快速上手

一台新设备，从零到用起来，只要一条命令：

```console
$ makara init
Sync to a private GitHub repository? [Y/n] Y
GitHub authorization (one-time only):
  1. Open: https://github.com/login/device
  2. Enter this code: 8163-2A61
Waiting for authorization… this continues automatically.
  Token scopes: repo
✓ Authorized. Token saved to the credential store.
  Forget it: makara remote unset.
✓ Sync repository ready (private): https://github.com/<你>/makara-vault
Set the passphrase.
  If you forget it, ALL data is unrecoverable.
  Keep it in a password manager.
New passphrase: ********
Confirm passphrase: ********
✓ Initialized with an empty vault.
  Baseline uploaded to: https://github.com/<你>/makara-vault
  Vault: %APPDATA%\myapikey\repo\vault.mkv
```

流程分两步：先绑定并就绪同步仓库（默认在你的账号下创建私有仓库 `makara-vault`，浏览器授权是绑定流程的一步），**之后才设置解锁密码**并上传空基线。选否（或 `makara remote unset` 后的机器）则纯单机使用：

```console
$ makara init
Sync to a private GitHub repository? [Y/n] n
Set the passphrase.
  If you forget it, ALL data is unrecoverable.
  Keep it in a password manager.
New passphrase: ********
Confirm passphrase: ********
✓ Initialized (standalone).
  Vault: %APPDATA%\myapikey\repo\vault.mkv
  Bind a remote later: makara remote set <owner/repo>
```

也可以直接 `makara init <owner/repo>` 跳过同步询问。注意解锁密码是在**探测远端之后**才要求的：远端已有数据时要求输入解锁密码以接入，空仓库才设置新密码。

> [!WARNING]
> 解锁密码用于加密全部记录，**没有找回功能**——忘记 = 数据丢失，请存入密码管理器。所有设备必须使用同一个解锁密码。

记录第一条凭证：

```console
$ makara add
Select protocol:
  1) OpenAI Responses (recommended)
  2) OpenAI Chat (widely compatible)
  3) Anthropic Messages (standard)
  4) Anthropic Complete (deprecated)
  5) Custom label
Choice (Enter for 1): Record name (e.g. OpenAI primary): OpenAI 主力号
Base URL (stored as-is, no normalization): https://api.openai.com/v1
API key: sk-proj-********
Models (comma-separated, Enter to skip): Notes (optional, Enter to skip):
✓ Saved locally
  Standalone mode: nothing to sync.
✓ Added "OpenAI 主力号" (id: 01a1683f…)
Key check: invalid (HTTP 401)
```

第一步是上下键选协议（回车取默认 OpenAI Responses；选 Custom label 才追问自定义标签），之后名称与 Base URL 必填、key 隐藏输入。**Base URL 填错不会丢掉已填内容**：格式不合规则原地重问，改对了继续。

```console
Base URL (stored as-is, no normalization): sss
Must start with http:// or https:// followed by a host; try again.
Base URL (stored as-is, no normalization): https://api.openai.com/v1
```

保存分两个阶段分别反馈（`✓ Saved locally` 之后才是 `Syncing to GitHub…`），保存成功后自动校验一次 key（假 key 显示 invalid，不影响保存）。

日常三件套：

```console
$ makara list
#    Name               Protocol     Base URL                           Key                  Models               Check
-----------------------------------------------------------------------------------------------------------------------------
1    OpenAI 主力号      OpenAI       https://api.openai.com/v1          sk-p...0000                               invalid
-----------------------------------------------------------------------------------------------------------------------------
1 record. Use `makara show <name|#index|id>` for full details

$ makara show 1
Name       : OpenAI 主力号
Protocol   : OpenAI
Base URL   : https://api.openai.com/v1
API Key    : sk-p...0000
（……其余字段……）

Use `makara key 1` to copy the full key to the clipboard.

$ makara sync
✓ Up to date
```

`show` 的目标可以是列表序号（`1`）、名称（`OpenAI 主力号`）或 id 前缀，怎么短怎么来。**key 在任何输出里都是掩码**——完整 key 只通过 `--copy` 进剪贴板，不会出现在终端上（防截图/录屏/终端缓冲泄露）。

## 在另一台设备上使用

新设备上跑同一条命令，浏览器授权后自动找到你的仓库并拉下全部记录——**输入与第一台设备相同的解锁密码**：

```console
$ makara init
Sync to a private GitHub repository? [Y/n] Y
Using the saved GitHub authorization.
✓ Sync repository ready (private): https://github.com/<你>/makara-vault
Master passphrase: ********
✓ Initialized with 1 record.
  Vault: %APPDATA%\myapikey\repo\vault.mkv
```

之后：改完自动推送，开机 `makara sync` 一次拿最新。两台设备离线各改各的也没关系，`sync` 会按最后修改时间自动合并。

## 日常任务

**复制某个 key**（完整 key 只进剪贴板，终端永远掩码；粘贴后建议手动清一次剪贴板）：

```bash
makara key 1
```

**验证 key 是否还有效**（真发一次 `/models` 请求，不消耗 token）：

```bash
makara check              # 全部
makara check OpenAI主力号  # 单条
```

结论对照：`valid` 有效 / `invalid` 无效 / `no permission` 有效但无权限 / `rate limited` 有效但限额 / `unverifiable` 平台不支持探测。

**搜索记录**（fzf 风格模糊匹配：名称 / Base URL / 备注可搜，URL 按斜线分词，协议也可搜——OpenAI / Anthropic / 形态词 / 自定义名，权重最低；结果按相关度排序）：

```bash
makara search openai        # 也试试缩写，如 makara search gtw
```

> [!NOTE]
> `search` 结果里的行首序号是显示序号，与 `list` 的顺序不同——
> 后续 `show` / `delete` 定位请用名称或 id，别用 `#序号`。

**检查新版本**（联网比对 GitHub 最新 Release，只提示不自动改文件）：

```bash
makara --version            # 只看当前版本，不联网
makara update               # 检查新版本，有则给出升级命令
```

  **模型列表**：`makara add` / `makara edit` 时可填（逗号分隔，回车跳过）。

**修改一条记录**（逐项询问，回车保留原值；把字段清空后回车即清空该字段）：

```bash
makara edit 1
```

**删除一条记录**：

```bash
makara delete 1
```

**换绑同步仓库**：

```bash
makara remote set <新仓库地址>
```

**改解锁密码**（全部记录重新加密并推送，其他设备下次同步用新密码解锁）：

```bash
makara passwd
```

**备份 / 迁移**（导出含明文 key，两次确认，用完即删）：

```bash
makara export                            # 无参数 = 当前目录 makara-export.json
makara export backup.json
makara export "C:\Users\you\Downloads"   # 目录也行：自动生成 makara-export.json，
                                        # 已存在则 -2/-3 递增，绝不覆盖旧导出
makara import backup.json
```

**锁定本机（借出电脑前）**：

```bash
makara lock                # 立即锁定：清除记住的解锁密码，下次命令重新输入
makara remote unset        # 解绑远端仓库（回到单机）
```

解锁密码默认**输入一次即记住**（存在系统凭据库），有效期与**自动锁定时长**一致（`config.toml` 的 `auto_lock_minutes`，默认 10 分钟），使用期间自动滚动续期；设 0 = 永不锁定（重启也免输）。到期或手动 `makara lock` 后，下次命令要求重新输入。

## 数据与安全

- **本机数据**：`~/.config/myapikey`（macOS 在 `~/Library/Application Support`，Windows 在 `%APPDATA%`）；
- **云端只有密文**：仓库里的 `vault.mkv` 是 Argon2id + AES-256-GCM 加密文件，仓库泄露也无法读取；
- **仓库自解释**：README.md 由 Makara 自动生成（说明仓库用途、防止误删）；已有 README 时不会覆盖，可自行替换；
- **解锁密码与 GitHub 访问凭据**只存各设备的系统凭据库，永远不进仓库；
- 删除采用墓碑机制：已删记录不会从旧设备复活。

## 命令参考

程序自带完整帮助，以下是 `makara --help` 原文；每个子命令还有 `makara <命令> --help`：

```text
Record and sync AI API credentials. Encrypted, synced via your own GitHub private repo

Usage: makara.exe <COMMAND>

Commands:
  init    Initialize: authorize GitHub if needed, create/reuse the private repo, download or upload the vault
  add     Add a record interactively. Omitted fields are prompted
  update  Check for a newer release and self-update in place
  key     Copy a record's API key to the clipboard (never printed)
  list    List all records. Keys are masked
  search  Search records (fuzzy match on name, URL, notes)
  show    Show one record. Key is always masked; use --copy to put it on the clipboard
  edit    Edit a record field by field. Enter keeps the current value
  delete  Delete a record. Keeps a tombstone, purged after the grace period
  check   Check key validity via GET /models. Checks all records if no target given
  sync    Sync: pull + merge + push
  status  Show repository and sync status
  remote  Manage the remote repository
  passwd   Change the passphrase
lock     Lock now: forget the remembered passphrase on this machine
  export  Export all records to a JSON file in PLAINTEXT. Asks twice before writing; no argument = the current directory
  import  Import records from a JSON file

Options:
  -h, --help     Print help
  -V, --version  Print version
```

## 常见问题

**推送时提示 push failed？** 远端有其他设备的新数据，`makara sync` 一次即可（自动合并后再推）。

**GitHub 设备码过期了？** 15 分钟有效，重新运行命令即可。

**解锁密码忘了？** 无法找回。删除本机配置目录与远端仓库，从头 `makara init`。

**提示 not initialized？** 这台设备还没初始化，先跑 `makara init`（按交互提示选择是否绑定 GitHub 同步）。

**之前绑定过，现在显示未绑定（单机模式）？** 本机的 GitHub 访问凭据丢失或被回收——Makara 只呈现「已绑定 / 未绑定」两态，此时按未绑定处理。数据无损：远端仓库与本地记录都还在，重新绑定即可恢复——GUI 点「绑定仓库」，CLI 跑 `makara remote set <owner/repo>`，浏览器授权一次即恢复，同步会自动对账。

## 安装

正常运行零外部依赖（D25 起不需要 git；同步走 GitHub API）。

获取程序：

```bash
# 方式一：源码安装（当前可用，需要 Rust 工具链）
cargo install --git https://github.com/Haaaatcher/MyApiKey

# 无桌面环境的 Linux 服务器（无 D-Bus / libdbus 开发库）：
cargo install --git https://github.com/Haaaatcher/MyApiKey --no-default-features

# 方式二：下载预编译可执行文件（发布后可用，零依赖）
# → GitHub Releases: https://github.com/Haaaatcher/MyApiKey/releases
```

安装后得到两个等价命令：`makara`（短）与 `myapikey`（全称）。

> [!NOTE]
> `--no-default-features` 关闭的是 Linux 的 Secret Service 密码记忆后端：
> 代价是这类机器上不记住解锁密码（每次手动输入），其余功能完全一致。
> 桌面 Linux 不需要也不建议加这个参数。
