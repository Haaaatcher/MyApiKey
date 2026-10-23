# MyApiKey 设计文档

> 版本：v0.2（草案，供讨论）
> 日期：2026-10-22
> 状态：设计中，未开始编码

---

## 1. 概述

MyApiKey 是一个跨平台（Linux / Windows / macOS）的 AI API 凭证记录与同步工具。用户把成对的 `(base_url, api_key)` 记录进来，工具负责加密存储，并通过 git 在多台设备间同步。同时提供终端（CLI）和窗口（GUI）两种使用形态。

### 1.1 核心场景

1. 用户在某个 AI 平台（OpenAI / Anthropic 官方，或任何兼容两家协议的三方平台/网关）拿到了 base_url 和 api key；
2. 在任意一台设备的 MyApiKey 中记录这条信息（选择协议只是个用户手填的标签，不做自动推断）；
3. 保存时自动加密，并同步到用户的私有 git 仓库；
4. 其他设备拉取后，输入主密码即可解密使用；
5. 可选：对记录发起"key 是否有效"的校验（调 `/models` 端点）。

### 1.2 非目标（明确不做）

- ❌ 不做 key 前缀识别 / 协议自动推断（三方平台不守约定，无意义）
- ❌ 不做 key 生命周期管理（创建/轮换/撤销等 Admin API 操作）
- ❌ 不做代理 / 网关 / 请求转发
- ❌ 不做团队共享、权限管理
- ❌ 不做 TOTP 之类的动态口令

---

## 2. 已定决策记录（ADR 摘要）

| # | 决策 | 理由 |
|---|---|---|
| D1 | 数据模型：`name + protocol(用户手填标签) + base_url + api_key` | 三方平台格式不可靠，不推断 |
| D2 | 同步载体：git 私有仓库，仓库中只存密文 | 免费、有历史、离线可用；明文 key 不能进 git 历史 |
| D3 | 加密：用户主密码 → Argon2id 派生 → AES-256-GCM | 密码不随数据走；三要素（仓库/密码/git凭据）互为独立防线 |
| D4 | 主密码不可找回，无重置机制 | 忘记 = 数据丢失，应用需明示 |
| D5 | key 校验：按协议调 `GET /models`，结果作状态标签，不阻断保存 | 三方平台未必实现 /models |
| D6 | 冲突策略：按 `updated_at` last-write-wins | 数据量小，简单够用 |
| D7 | 架构：Rust workspace，core 纯逻辑库 + cli 二进制 + Tauri 2 GUI 二进制 | 一套逻辑两种形态；产物小、无运行时依赖 |
| D8 | git 操作先直接 shell 调系统 `git` | 目标用户必有 git，行为与预期一致；后续可换 gitoxide |
| D9 | 主密码输入一次即记住（CLI/GUI 一致），存 OS 凭据库，**默认 7 天有效（`remember_days` 可配，0=禁用）**；每次输入续期；密码验证失败自动失效；`forget` = 立即锁定 | 体验优先（至多每周输一次）；到期时间存配置目录 `remember.json`（仅时间戳）；2026-10-24 由永久记住改为 TTL |
| D10 | vault 格式自研（Argon2id + AES-256-GCM + 版本/AAD 头） | 30 行格式换完全可控的演进与防降级 |
| D11 | 命令同时提供长名 `myapikey` 与短名 `mak` | 同一实现，两个 bin 入口 |
| D12 | GUI 首发只做 Windows；Linux/macOS 暂时 CLI-only | 集中火力；Tauri 代码保持跨平台，后续可补齐 |
| D13 | GitHub 一键初始化：`mak init --github [仓库名]`，OAuth Device Flow（RFC 8628）鉴权后自动创建/识别私有仓库；token 存 OS 凭据库（account=github），经环境变量注入 git Authorization 头（不进命令行/不落盘）；`mak forget --github` 可清除 | 免去用户手动建仓库+复制 URL；browser 授权一次即可；scope 仅 `repo`（GitHub 模型限制，无法按单仓库收窄，需文档明示） |

---

## 3. 总体架构

### 3.1 Workspace 结构

```
myapikey/
├── Cargo.toml            # workspace
├── crates/
│   ├── myapikey-core/    # 纯逻辑库：模型、vault、同步、校验（无 UI 依赖）
│   ├── myapikey-cli/     # CLI（clap + rpassword；产出 myapikey 与 mak 两个二进制）
│   └── myapikey-gui/     # Tauri 2 应用（前端 TS，逻辑全部走 Tauri command 进 core）
└── docs/
```

### 3.2 分层规则

```
┌─────────────────── myapikey-core（纯逻辑）───────────────────┐
│  model      记录/元数据结构、序列化                            │
│  vault      Argon2id + AES-256-GCM 加解密、格式版本化          │
│  store      本地 git 仓库管理（init/commit/状态）              │
│  sync       pull → merge(lww) → 加密 → commit → push          │
│  probe      按协议探测 /models，产出校验状态                    │
└────────────────────────┬─────────────────────┬───────────────┘
                         │                     │
                  myapikey-cli           myapikey-gui (Tauri)
                  交互、密码采集          原生窗口、密码采集、
                  (rpassword)           Tauri command 调 core
```

- core 只暴露"给我密码字节，我完成加解密/同步"级别的 API；**密码采集方式完全留给前端**（CLI 用 rpassword，GUI 用原生输入框）。
- GUI 与 core 同进程，通过 Tauri command 调用，不经过任何网络。
- 两个前端不允许同时写 vault：core 内置文件锁（`vault.lock` + fd-lock），第二个实例只读或直接拒绝启动。

### 3.3 本地目录布局

用 `dirs` crate 规范化，core 集中定义：

| OS | 数据目录 |
|---|---|
| Linux | `~/.config/myapikey/` |
| macOS | `~/Library/Application Support/myapikey/` |
| Windows | `%APPDATA%\myapikey\` |

目录内容：

```
<config_dir>/
├── repo/              # 本地 git 仓库（从第一天起就是 git 仓库，见 §5.1）
│   └── vault.mkv      # 加密后的 vault（仓库里唯一的实质内容）
├── config.toml        # 非敏感配置：远端地址、自动同步开关等
└── vault.lock         # 实例锁
```

---

## 4. 数据模型

### 4.1 记录（Record）

```rust
pub struct Record {
    pub id: Uuid,              // UUID v7（时间有序，便于稳定排序）
    pub name: String,          // 用户命名，如 "OpenAI 主力号"
    pub protocol: Protocol,    // 见下
    pub base_url: String,      // 原样保存，不做归一化
    pub api_key: String,       // 明文仅存在于内存与 vault 解密后
    pub extra_headers: BTreeMap<String, String>,  // 可选，如 anthropic-workspace-id / OpenAI-Project
    pub notes: Option<String>, // 可选备注
    pub created_at: Timestamp,
    pub updated_at: Timestamp, // 同步冲突裁决依据
    pub deleted_at: Option<Timestamp>,  // 墓碑，见 §5.3
}

pub enum Protocol {
    OpenAi,
    Anthropic,
    Custom(String),   // 仅作为展示标签 + probe 提示，允许自定义
}
```

- `protocol` 的唯一实际作用：① 展示；② probe 时决定怎么发请求。**不用于任何"推断"**。
- 校验结果（last_check）**只存本地缓存，不进 vault**（避免同步 merge 噪音），见 §6。

### 4.2 Vault 文件格式

单文件、整体加密（文件小，每次保存全量重加密）：

```
magic:   b"MAKV"          4 字节
version: u16              当前 = 1
kdf:     Argon2id 参数编码（m、t、p）
salt:    16 字节随机
nonce:   12 字节随机（每次加密重新生成）
body:    AES-256-GCM(明文 = JSON 序列化的 {records: [...]})
                               AAD = 前面全部头部（防降级/篡改头部）
```

加密参数（初始值，写入文件、可随版本演进）：

- Argon2id：`m = 64 MiB, t = 3, p = 1`
- 密钥：32 字节，由密码 + salt 派生；密码与密钥使用 `zeroize` 及时清零

---

## 5. 同步设计

### 5.1 本地即 git 仓库

首次 `init` 时直接 `git init` 本地 `repo/` 目录——**即使用户还没配置远端**，本地也已经是一个带历史的 git 仓库。之后配置远端（`git remote add`）即可开始跨设备同步。好处：单机使用也有完整本地历史，接入远端是纯增量动作。

仓库布局：`vault.mkv` 放仓库根目录；commit message 用固定机器格式（如 `sync: 2026-10-21T12:00:00Z, 3 records`），不暴露任何业务信息。

### 5.2 同步流程

```
save(记录变更) → 内存合并 → 序列化+加密 → 写 vault.mkv → git commit
                                                        → git push（静默，失败仅提示）
启动/手动 sync → git pull --rebase → 解密远端与本地 → 按记录合并 → 重新加密提交
```

- 远端仓库冲突（push 被拒）：自动 pull --rebase 后重试一次；仍失败则保留本地状态并提示手动处理（开发者用户可自己进仓库救）。
- git 认证完全交给用户已有的 git 配置（SSH key / credential manager），应用不存、不管 git 凭据。

### 5.3 合并与删除

- 合并单位是**记录**（按 `id`），双方都改同一条时 `updated_at` 新者胜。
- **删除走墓碑**（`deleted_at`），防止"设备 A 删了、设备 B 还留着旧数据 → 下次同步又复活"。墓碑保留 30 天后在合并时清理（参数可配）。
- 记录数量极小（预期 < 100 条），无需分文件存储，单 JSON 足够。

---

## 6. key 校验（probe）

按记录的协议标签发一次请求，10 秒超时：

| protocol | 请求 | 判定 |
|---|---|---|
| OpenAi | `GET {base_url}/models`，`Authorization: Bearer`；若 base 不以 `/v1` 结尾则依次尝试 `{base}/models` 和 `{base}/v1/models` | 200→有效；401→无效；403→有效但无权限；429→有效但限额；404/其他→无法验证 |
| Anthropic | `GET {base_url}/v1/models`，`x-api-key` + `anthropic-version: 2023-06-01` | 同上（401 的 Anthropic 错误体是 `authentication_error`） |
| Custom | 不探测，状态恒为"未校验" | — |

- 校验结果存本地缓存 `{id: {status, checked_at, http_status}}`，**不参与同步**。
- 触发时机：保存新记录时自动试一次 + 手动重新校验。校验失败不影响保存。

---

## 7. CLI 设计（myapikey-cli）

```
myapikey init                # 初始化：设主密码（二次确认+强度提示）、git init、可选配远端
myapikey add                 # 交互式添加（name/protocol/base_url/key，key 隐藏输入）
myapikey list                # 列表（默认掩码 key，显示校验状态）
myapikey show <id|name>      # 查看单条（--copy 复制 key 到剪贴板）
myapikey edit <id|name>      # 修改
myapikey delete <id|name>    # 删除（墓碑）
myapikey check [id|name]     # 校验（不带参数=全部）
myapikey sync                # pull + merge + push
myapikey status              # 未推送变更、远端配置、上次同步时间
myapikey remote set <url>    # 配置/更换远端仓库
myapikey export <file>       # 明文导出（双重确认 + 0600 权限）
myapikey import <file>       # 导入
myapikey passwd             # 修改主密码（验证旧密码 → 重加密 vault → 更新凭据库）
myapikey forget             # 清除记住的密码（删除系统凭据库条目，之后每次手动输入）
```

- 二进制同时安装 `myapikey`（长名）与 `mak`（短名），行为完全一致（同一套 clap 定义，两个入口）。
- 解锁顺序：`MYAPIKEY_PASSPHRASE` 环境变量（CI/容器的逃生通道）> 系统凭据库记住的密码 > 交互输入。记住的密码解密失败时自动清除并回退手动输入（§9.1）。

---

## 8. GUI 设计（myapikey-gui, Tauri 2）

- **平台策略（Q12 决议）**：GUI 首发只做 Windows；Linux/macOS 暂时只提供 CLI（Tauri 代码保持跨平台，后续可随时补齐另两端的发布）。
- **解锁页**：若凭据库中存有密码，显示一键解锁（一次点击/回车即用记住的密码解密）；无记忆或记忆失效时手动输入。错误提示统一为"密码错误"（不区分格式错误）。
- **主列表**：记录卡片（name、protocol 徽标、base_url、校验状态点），搜索框；顶栏显示同步状态（上次同步时间/未推送数/失败标记）+ 手动同步按钮。
- **编辑抽屉**：新增/编辑表单；key 输入框默认掩码可切换；protocol 用三个选项（OpenAI / Anthropic / 自定义文本）。
- **复制按钮**：一键复制 key，30 秒后尽力清空剪贴板（best-effort，OS 限制会注明）。
- **设置页**：远端仓库地址、自动同步开关、自动锁时限、墓碑保留期。
- **自动锁（软锁）**：空闲 N 分钟（默认 10）后销毁内存中的解密状态，回到解锁页；因密码已被记住，恢复只需一次确认点击——它防的是离开座位时的顺手查看，不防恶意软件（与 D9 的威胁模型一致，见 §9.1）。
- 前端只做展示与输入，**一切读写通过 Tauri command 走 core**；前端代码里不出现任何密钥处理逻辑。

---

## 9. 安全清单

| 项 | 措施 |
|---|---|
| 仓库内容 | 永远只有密文（vault.mkv） |
| 主密码 | 不写入任何文件；"记住"仅存于 OS 凭据库（§9.1），可随时 forget；内存中 zeroize |
| 内存 | 派生密钥、密码用 `zeroizing` 包裹；承认 JSON 反序列化产生的明文 String 无法完全避免——文档中如实声明此限制 |
| 剪贴板 | 复制后 30s 尽力清除 |
| 并发 | 文件锁防双实例写坏 vault |
| 导出 | 明文导出需二次确认，文件 0600，界面警示 |
| 找回 | 无。首次设置密码时界面显著提示"忘记=丢失全部数据"，并建议记录恢复口令于密码管理器 |
| 网络 | 仅两类出站：用户配置的 base_url 校验请求、git 操作。无遥测 |

### 9.1 主密码记忆机制（Q1/Q2 决议）

两种前端统一策略：**首次输入成功后记住；密码验证失败或用户修改时自动失效**。

**存储位置：操作系统凭据库**（Rust `keyring` crate 统一封装）：

| OS | 后端 | 说明 |
|---|---|---|
| Windows | Credential Manager | GUI 主平台，随 Windows 用户账户保护 |
| macOS | Keychain | CLI-only，同样可用 |
| Linux | Secret Service（GNOME Keyring / KWallet） | 无头服务器、纯 SSH 环境常不可用 → **自动降级为每次手动输入**，不报错 |

- 凭据库条目：service = `myapikey`，account = `default`（为未来多 vault 预留命名空间），内容存**主密码本身**而非派生密钥——这样 KDF 参数可随 vault 版本演进，改参数不用动凭据库。
- **解锁流程（每次启动/命令）**：凭据库有密码 → 直接解密 vault → 成功即静默进入；失败（说明密码已在别处修改或 vault 被换）→ 删除该条目，回退手动输入，成功后重新记住。
- **修改主密码**（`myapikey passwd` / GUI 设置）：验证旧密码 → 用新密码重加密整个 vault → 同步更新凭据库条目。
- **主动忘记**（`myapikey forget` / GUI 设置开关）：删除凭据库条目，之后每次手动输入。
- **威胁模型变化（如实声明）**：记住密码后，本机防线 = 操作系统登录 + 凭据库权限。防"仓库泄露 / 传输窃听 / 密文离线爆破"的能力不变，但不再防"设备失窃且已登录"。GUI 自动锁因此定位为软锁（§8）。
- 后续增强（非 v1）：Windows Hello / Touch ID 作为读取凭据库前的一道本地验证。

---

## 10. 分发

- CI（GitHub Actions）三平台矩阵：`cargo test` + `clippy` + `fmt`。
- Release 产物（按 Q12 决议）：三平台 CLI 单二进制（各含 `myapikey` + `mak`）；**GUI 安装包仅 Windows**（msi/nsis）。
- macOS：暂无 GUI，不做签名/公证；浏览器下载的 CLI 会触发 Gatekeeper 隔离，文档中提示 `xattr -d com.apple.quarantine` 解法，后续再评估开发者账号。
- 渠道：GitHub Releases 起步；`cargo install`、scoop/winget/homebrew 后续按需。

---

## 11. 问题决议状态

**本轮已决议：**

| 原问题 | 决议 |
|---|---|
| Q1/Q2 密码输入体验与记住机制 | D9 + §9.1：输入一次即记住，存 OS 凭据库，失败/修改自动失效 |
| Q9 vault 格式 | D10：自研 AES-GCM 头 |
| Q10 命令名 | D11：`myapikey` 与 `mak` 双名并存 |
| Q12 平台策略 | D12：GUI 主攻 Windows，Linux/macOS 暂时 CLI-only |

**其余小项默认按 v0.1 中的倾向执行**（如有异议随时推翻）：

- 墓碑保留期默认 30 天（可配）
- `extra_headers` v1 保留，GUI 收进"高级"折叠区
- 保存即静默 push，失败仅提示不阻塞
- `Custom(String)` 协议标签保留（不参与 probe）
- v1 不做分组/标签，不做多 vault
- 校验请求遵循系统代理（设置可关）
- CLI/GUI 共享同一份 `config.toml` 与锁语义（在 core 层统一实现）

---

## 12. 里程碑

| 阶段 | 内容 | 产出 |
|---|---|---|
| M0 | workspace 骨架 + core：模型、vault 加解密（含 roundtrip/篡改测试）、文件锁 | 可测试的库 |
| M1 | CLI 全命令 + 本地 git 历史 + 远端同步 + probe | **单机/多机 CLI MVP，日常可用** |
| M2 | Tauri GUI（解锁/列表/编辑/同步状态/设置），**仅发布 Windows** | Windows 用户双形态可用 |
| M3 | 导入导出、自动锁、剪贴板清除、CI 三平台发布 | 1.0 |

MVP 原则：**M1 完成即满足全部核心场景**（记录 + 加密 + git 同步 + 校验），GUI 与打磨都是增强。
