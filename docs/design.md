# MyApiKey 设计文档

> 版本：v1.0.0
> 日期：2026-10-31
> 状态：v1.0.0 已发布（M0–M4 完成，M5 macOS GUI 待启动）

---

## 1. 概述

MyApiKey 是一个跨平台（Windows / macOS / Linux）的 AI API 凭证记录与同步工具。用户把成对的 `(base_url, api_key)` 记录进来，工具负责加密存储，并通过用户自己的 GitHub 私有仓库在多台设备间同步。同时提供终端（CLI）和窗口（GUI）两种使用形态。

### 1.1 核心场景

1. 用户在某个 AI 平台（OpenAI / Anthropic 官方，或任何兼容两家协议的三方平台/网关）拿到了 base_url 和 api key；
2. 在任意一台设备的 MyApiKey 中记录这条信息（选择协议只是个用户手填的标签，不做自动推断）；
3. 保存时自动加密，并同步到用户的私有 GitHub 仓库；
4. 其他设备拉取后，输入主密码即可解密使用；
5. 可选：对记录发起"key 是否有效"的校验（调 `/models` 端点）。

### 1.2 非目标（明确不做）

- ❌ 不做 key 前缀识别 / 协议自动推断（三方平台不守约定，无意义）
- ❌ 不做 key 生命周期管理（创建/轮换/撤销等 Admin API 操作）
- ❌ 不做代理 / 网关 / 请求转发
- ❌ 不做团队共享、权限管理
- ❌ 不做 TOTP 之类的动态口令
- ❌ 不做生态集成 / 凭证供给（写配置文件、扫描导入、环境变量——D24，2026-10-29 永久关闭，交给 CC Switch 等专业工具）
- ❌ 不做生物识别解锁（Windows Hello / Touch ID——2026-10-30 讨论后否决：增加攻击面与平台耦合，收益不成比例）

---

## 2. 已定决策记录（ADR 摘要）

| # | 决策 | 理由 |
|---|---|---|
| D1 | 数据模型：`name + protocol(用户手填标签) + base_url + api_key` | 三方平台格式不可靠，不推断 |
| D2 | ~~同步载体：git 私有仓库~~ → **被 D25 取代**（M4 起 GitHub Contents API 直连，仓库中只存密文不变） | 明文 key 不能进仓库历史的底线不变 |
| D3 | 加密：用户主密码 → Argon2id 派生 → AES-256-GCM | 密码不随数据走；三要素（仓库/密码/凭据）互为独立防线 |
| D4 | 主密码不可找回，无重置机制 | 忘记 = 数据丢失，应用需明示 |
| D5 | key 校验：按协议调 `GET /models`，结果作状态标签，不阻断保存 | 三方平台未必实现 /models |
| D6 | 冲突策略：按 `updated_at` last-write-wins | 数据量小，简单够用 |
| D7 | 架构：Rust workspace，core 纯逻辑库 + cli 二进制 + Tauri 2 GUI 二进制 | 一套逻辑两种形态；产物小、无运行时依赖 |
| D8 | ~~git 操作直接 shell 调系统 `git`~~ → **随 D25 整体移除** | 应用不再有任何 git 依赖 |
| D9 | 主密码输入一次即记住（CLI 语义），存 OS 凭据库，**默认 7 天有效（`remember_days` 可配，0=禁用）**；每次输入续期；密码验证失败自动失效；**GUI 锁定（手动/空闲）= 清除记忆（硬锁）**；`mak passwd unset` 随时锁定 | 体验优先（CLI 至多每周输一次）；到期时间存配置目录 `remember.json`（仅时间戳） |
| D10 | vault 格式自研（Argon2id + AES-256-GCM + 版本/AAD 头） | 30 行格式换完全可控的演进与防降级 |
| D11 | 命令同时提供长名 `myapikey` 与短名 `mak` | 同一实现，两个 bin 入口 |
| D12 | GUI 首发只做 Windows；Linux/macOS 暂时 CLI-only | 集中火力；Tauri 代码保持跨平台，后续可补齐 |
| D13 | GitHub 一键初始化：~~`mak init --github [仓库名]`~~ → **收敛为 `mak init [仓库]` 交互式**（无参询问是否同步、仓库名预填；带参跳过询问），OAuth Device Flow（RFC 8628）鉴权后自动创建/识别私有仓库；token 存 OS 凭据库（account=`github`），以 Bearer 头走 REST API；**poll 表单必带 `scope=repo`（2026-10-30 根因修复：缺失则颁发零权限令牌）** | 免去用户手动建仓库+复制 URL；browser 授权一次即可 |
| D14 | GUI 外观设置：主题（明亮/暗黑/跟随系统，CSS 变量双主题 + `prefers-color-scheme` 监听）与界面语言（中文/英文，前端字典 i18n，默认跟随系统语言）。两项均为**本机 UI 偏好**，存 localStorage（不进 config.toml、不参与同步）。CLI 语言策略不变（默认英文） | 主题/语言是设备级个人偏好，随仓库同步会强加给其他设备；localStorage 零后端改动、零敏感面（2026-10-24 决议） |
| D15 | 窗口与进程策略（2026-10-25 需求）：**托盘常驻**（关闭/最小化均收进系统托盘，会话保活；托盘菜单 显示/退出，显式退出才结束）；**单实例**（tauri-plugin-single-instance，二次启动聚焦已有窗口）；**启动状态跟随上次**：上次未锁定退出且记住的密码在有效期内 → 静默恢复解锁；锁定（手动/自动，会清记忆）或过期 → 必须输入主密码 | 托盘保活让"恢复到上次状态"无需把主密码落盘（明文永不写文件是铁律） |
| D16 | 应用内更新（2026-10-25）：GUI 集成官方 `tauri-plugin-updater`，更新源为 GitHub Releases 的 `latest.json`（`scripts/make-latest-json.py` 生成），安装包 Ed25519 签名；设置页手动检查、下载验签、静默安装、重启。**token 失效自愈闭环（2026-10-26）**：设置页「重新授权 GitHub」走设备流刷新 token（不重建 vault）。**破坏性守卫**：Release notes 首行 `[breaking]` 标记或主版本号提升 → 拒绝自动安装，引导手动升级。CLI 不做自替换：`mak update` 检查后提示 `cargo install --git <repo> --tag <v> myapikey-cli` 升级命令 | 普通更新零门槛（管 key 的工具要能及时拿到修复）；签名防中间人是凭证管理器的安全底线 |
| D17–D23 | （已删除：导入/供给/配方引擎相关决议，随 D24 整体关闭） | 见 D24 |
| D24 | **生态集成与凭证供给永久关闭**（2026-10-29 用户拍板）：apply/restore/配方引擎/`[[field]]` 字段模型整体删除，切换凭证交给 CC Switch 等专业工具。**勿再提议** | 做复杂有成本的事（各家私有 schema/漂移修复）；简单替代品（setx/手抄 export）不值得做 |
| D25 | **M4 零外部依赖（2026-10-29）**：移除系统 git，同步改 **GitHub Contents API 直连**（`SyncStore` trait：`fetch_head` / `put_blob`(CAS) / `download_ref` + GitHub 实现 + 测试用内存实现）；单文件 CAS 语义，409 冲突自动重拉重合并（限 3 轮）；本地状态 = `vault.mkv` + config 的 `last_sync_sha`/`push_pending`/`last_sync_at`/`remote_repo`；远端仅支持 GitHub（`owner/name` 归一化，SSH 形态拒绝）；status 的 ahead/behind 提交数 → `pending` 布尔；老仓库无缝迁移（Contents API 直读现有 `vault.mkv`，历史 commit 全保留） | gitoxide push 至 2026 未实现（fetch ✓/push ✗），libgit2 引入 C 编译链；GitHub Contents API 单文件 ≤1MB 全功能且每次 PUT 自动产生 commit（网页历史无损） |
| D26 | **M5 macOS GUI 适配（2026-10-30 立项）**：窗口应用发布 macOS 版，仅 Apple Silicon（`aarch64-apple-darwin`），不做 Intel/Universal | 里程碑编号沿用 M5（原生态集成 M5 已随 D24 关闭） |
| D27 | **同步状态数据层单一化**（2026-10-31）：**config.toml 是同步状态的唯一数据源，显示一律从数据派生**——① `mark_synced` 记账唯一入口（三件套原子写入），解绑走对称的 `unbind_remote`；② `remote_repo` 恒为归一化 `owner/name`；③ GUI「忘记 GitHub 授权」= 清令牌 + 解绑一次写清 | 状态字段散落多处手写必有遗漏（GUI 接入后显示 never 等全是手写漂移） |
| D28 | **协议五值扩充**（2026-10-30，用户提供主流协议分类表）：`OpenAi`=Responses（默认，官方推荐新接口）/ `OpenAiChat`=Chat Completions / `Anthropic`=Messages / `AnthropicComplete`=Text Completions（legacy）/ `Custom`。命名规则：**无后缀 = 最新协议，带后缀 = 旧协议**。旧 vault 值（openai/anthropic）完全兼容；探测与模型拉取按两大家族分派（新旧 API 共享同一套 /models） | 真实平台同时存在新旧两种 API 形态，单值无法表达 |
| D29 | **搜索协议参与**（2026-10-31 用户需求，推翻 2026-10-25 "协议不参与"决议）：`Protocol::search_terms()`（标签 + 完整形态词）以**权重 3（全字段最低）**参与匹配——兼容服务商全员同协议时仍排在名称/URL 命中之后，旧决议担心的噪音用低权重化解 | 用户预期按协议类型找记录（搜 OpenAI 找到走 openai 协议的千问） |
| D30 | **提示词简约六则**（2026-10-31 用户定调"简约是设计风格"）：一句一事；删可推断；动词开头；错误 = 现象 + 出路；术语统一；双语各自成文。细则见 AGENTS.md「代码风格 · 提示词」 | 啰嗦的提示词让用户找不到重点 |

---

## 3. 总体架构

### 3.1 Workspace 结构

```
myapikey/
├── Cargo.toml            # workspace
├── crates/
│   ├── myapikey-core/    # 纯逻辑库：模型、vault、同步（GitHub API）、校验（无 UI 依赖）
│   ├── myapikey-cli/     # CLI（clap + dialoguer；产出 myapikey 与 mak 两个二进制）
│   └── myapikey-gui/     # Tauri 2 应用（前端 TS，逻辑全部走 Tauri command 进 core）
└── docs/
```

### 3.2 分层规则

```
┌─────────────────── myapikey-core（纯逻辑）───────────────────┐
│  model      记录/元数据结构、协议五值、序列化                  │
│  vault      Argon2id + AES-256-GCM 加解密、格式版本化          │
│  remote     SyncStore trait（fetch_head/put_blob CAS/download）│
│             + GitHub Contents API 实现 + 内存测试实现         │
│  sync       save_local(两阶段) → mark_synced 记账 →           │
│             pull → merge(lww) → push（409 重拉重合并）        │
│  probe      按协议探测 /models（并发批量 probe_all），产出校验状态│
│  search     fzf 风格模糊搜索（名称/URL/备注/协议）             │
│  github     Device Flow、scope 可见化、凭据库读写              │
└────────────────────────┬─────────────────────┬───────────────┘
                         │                     │
                  myapikey-cli           myapikey-gui (Tauri)
                  交互、密码采集          原生窗口、密码采集、
                  (dialoguer)           Tauri command 调 core
```

- core 只暴露"给我密码字节，我完成加解密/同步"级别的 API；**密码采集方式完全留给前端**（CLI 用 dialoguer Password，GUI 用原生输入框）。
- GUI 与 core 同进程，通过 Tauri command 调用，不经过任何网络。
- 两个前端不允许同时写 vault：core 内置文件锁（`vault.lock`），第二个实例等待或拒绝。
- **同步状态数据层单一化（D27）**：`mark_synced` 是记账唯一写入口，解绑走 `unbind_remote`；调用方禁止手写状态字段。

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
├── repo/
│   └── vault.mkv      # 加密后的 vault（同步的唯一实质内容，M4 起本地不再是 git 仓库）
├── config.toml        # 非敏感配置：远端地址、同步记账三件套、自动推送、记住密码天数等
├── remember.json      # 记住密码的到期时间戳（D9）
└── vault.lock         # 实例锁
```

---

## 4. 数据模型

### 4.1 记录（Record）

```rust
pub struct Record {
    pub id: Uuid,              // UUID v7（时间有序，便于稳定排序）
    pub name: String,          // 用户命名，如 "OpenAI 主力号"
    pub protocol: Protocol,    // 五值，见下（D28）
    pub base_url: String,      // 原样保存，不做归一化
    pub api_key: String,       // 明文仅存在于内存与 vault 解密后
    pub extra_headers: BTreeMap<String, String>,  // 可选
    pub models: Vec<String>,   // 模型清单（GUI 胶囊展示；探测/拉取按协议直连）
    pub notes: Option<String>, // 可选备注
    pub created_at: Timestamp,
    pub updated_at: Timestamp, // 同步冲突裁决依据
    pub deleted_at: Option<Timestamp>,  // 墓碑，见 §5.4
}
```

### 4.2 协议（Protocol，D28）

```rust
pub enum Protocol {
    OpenAi,             // Responses（默认，无后缀 = 最新）
    OpenAiChat,         // "openai-chat"
    Anthropic,          // Messages
    AnthropicComplete,  // "anthropic-complete"（legacy）
    Custom(String),     // 仅作为展示标签 + probe 提示
}
```

- `protocol` 的作用：① 展示（徽章只显示族名，完整形态 hover）；② probe 时决定怎么发请求；③ 搜索匹配词（`search_terms()`，D29）。**不用于任何"推断"**。
- 旧 vault 值（`openai`/`anthropic`）读入后即新默认形态，完全兼容。

### 4.3 Vault 文件格式

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

- Argon2id：`m = 64 MiB, t = 3, p = 1`；密码与派生密钥 `zeroize` 及时清零。
- 落盘走 **tmp + rename 原子覆盖**（Windows `MoveFileExW(REPLACE_EXISTING)`）——不得先删旧文件（2026-10-31 审查修复的非原子窗口）。

---

## 5. 同步设计（M4：GitHub Contents API 直连）

### 5.1 远端抽象

`SyncStore` trait 三个方法承载全部同步语义：

| 方法 | 语义 |
|---|---|
| `fetch_head()` | 读远端当前 `(sha, bytes)`；无文件返回 `None` |
| `put_blob(message, bytes, expect)` | CAS 上传：`expect` 为声明的远端 sha（`None`=建新）；远端已变 → 409 |
| `download_ref(sha)` | 取任意历史版本（预留给审计/回滚） |

GitHub 实现：`GET/PUT repos/{owner}/{repo}/contents/vault.mkv`；旧版 git 时代的仓库无缝迁移（blob sha 直接复用，网页 commit 历史全保留）。测试用内存实现（`MemStore`，可注入 409 竞态）。

### 5.2 记账（D27）

任何"本地与远端达成一致"的时刻（上传成功 / 下载替换 / 接入完成 / 已是最新）必须经 `mark_synced` 写记账三件套（`last_sync_sha` / `push_pending=false` / `last_sync_at`）并落盘；解绑走对称的 `unbind_remote`（`remote_repo` + 三件套一次归零）。调用方禁止手写字段。

### 5.3 保存与推送（两阶段）

```
save_local(记录变更) → 加密 → 原子写 vault.mkv → 置 push_pending   【阶段 1，毫秒级】
（auto_push 且已绑远端）→ PUT(CAS) → mark_synced                   【阶段 2，网络】
```

- 阶段 1 成功即数据安全；阶段 2 失败（网络/冲突）降级为警告，`push_pending` 保证 `mak sync` 补推。
- CAS 冲突（409）：重拉远端 → `merge_vaults` LWW 合并 → 重推，限 3 轮。

### 5.4 合并与删除

- 合并单位是**记录**（按 `id`），双方都改同一条时 `updated_at` 新者胜。
- **删除走墓碑**（`deleted_at`，保留 30 天），防止"设备 A 删了、设备 B 又复活"。
- 记录数量极小（预期 < 100 条），单文件单 JSON 足够；仓库单文件上限 1MB（GitHub Contents API 限制，当前规模远不可及）。

---

## 6. key 校验（probe）

按记录的协议家族发一次请求，10 秒超时（并发批量：`probe_all`，8 路上限）：

| protocol 家族 | 请求 | 判定 |
|---|---|---|
| OpenAi / OpenAiChat | `GET {base_url}/models`，`Authorization: Bearer`；base 不以 `/v1` 结尾则依次尝试 `{base}/models` 与 `{base}/v1/models` | 200→有效；401→无效；403→有效但无权限；429→有效但限额；404/其他→无法验证 |
| Anthropic / AnthropicComplete | `GET {base_url}/v1/models`，`x-api-key` + `anthropic-version: 2023-06-01` | 同上 |
| Custom | 不探测，状态恒为"未校验" | — |

- 校验结果存本地缓存 `{id: {status, checked_at, http_status}}`，**不参与同步**。
- 触发时机：保存新记录自动一次 + 手动单条 / 一键检测全部（GUI 顶栏；CLI `mak check` 无参）。

---

## 7. CLI 设计（myapikey-cli）

```
mak init [仓库]               # 交互初始化：无参询问是否同步（默认 Y、仓库名预填）；
                              # 带参跳过询问；已初始化一律拒绝并引导（remote set/sync/unset）
mak add                       # 交互添加：协议五项菜单 → 名称/Base URL/key（隐藏输入，原地重问）
                              # → 模型（逗号分隔可跳过）→ 备注；保存两阶段反馈 + 自动校验一次
mak update                    # 联网检查新 Release，提示 cargo 升级命令
mak search <query>            # 模糊搜索（名称 / URL / 备注 / 协议）
mak list                      # 列表（key 掩码；含 Models 列与校验状态）
mak show <名|#序号|id>        # 单条详情（--copy 复制 key 到剪贴板）
mak edit <名|#序号|id>        # 逐项修改（所见即所得：回车保留，清空后回车 = 清空）
mak delete <名|#序号|id>      # 删除（墓碑）
mak check [目标]              # 校验（无参 = 全部并发，8 路上限）
mak sync                      # pull + merge + push（409 自动重拉重合并）
mak status                    # 绑定/待推/上次同步/记录数
mak remote set|show|unset     # 改绑（自动管令牌）/ 查看 / 解绑并清授权
mak passwd                    # 改主密码（验证旧密码 → 全量重加密 → 上传）
mak passwd unset              # 清记住的密码（立即锁定）
mak export <file>             # 明文导出（双重确认 + 0600）
mak import <file>             # 导入（按 updated_at 新者胜）
```

- 二进制同时安装 `myapikey`（长名）与 `mak`（短名），行为完全一致。
- 解锁顺序：`MYAPIKEY_PASSPHRASE` 环境变量（CI/容器逃生通道，**错误即退不重试**）> 系统凭据库记住的密码 > 交互输入。

---

## 8. GUI 设计（myapikey-gui, Tauri 2）

- **解锁页**：锁定后一律手动输入主密码（D9）；**启动状态跟随上次**（D15）。错误提示统一为"密码错误"。
- **初始化（两阶段，2026-10-31）**：init 页无密码框（模式选择：GitHub 自动建仓 / 已有仓库地址 / 单机）→ `init_start` 授权与探测 → 返回 `ProbeDone{has_data}` → **按远端状态**显示对应密码表单（有数据 = 输入主密码接入；空仓库 = 设置新密码）→ `init_finish`。设备码页：授权码进页自动复制、后台自动轮询授权状态（完成后自动继续），手动按钮保留为失败重试入口。
- **搜索**（core::search 单一实现）：fzf 风格模糊匹配，子串优先、URL 剥 scheme 分词、协议参与（权重最低，D29）；1-2 字符短查询只做子串。前端带逐键竞态守卫；mock 的 TS 复刻改规则须与 Rust 同步。
- **主列表**：记录卡片瀑布流（贪心分列，横向纵向间隔固定 10px；列数按窗口宽度 1–3 列自适应）；卡片含名称、协议族徽章（hover 显示完整形态）、彩色模型胶囊（按模型名哈希取 6 色板）、notes 引用块、校验状态点、相对时间。顶栏 chips（上次同步相对时间 / 绑定状态 / 记录数）窄窗口分级隐藏，文字永不竖排。
- **「检测全部」**：核心层并发探测（8 路）；点击后全部 chip 进入检测中脉冲态，后端经 Tauri Channel 逐条推送结果（卡片逐个点亮），按钮显示进度计数，完成弹分类统计。
- **编辑抽屉**：协议五项自绘下拉；每字段小标签；key 掩码输入 + 纯图标眼睛按钮（hover 提示）；模型清单彩色胶囊 + 「获取」拉取后进入**候选复选面板**（过滤/全选/清空/按勾选合并，不整表塞入）；抽屉草稿误关不丢（保存成功或锁定清除）。
- **表单校验**：保存时全量检查所有字段，错误一次性全部行内标出（红框 + 聚焦 + 提示紧贴出错框下方）。
- **自绘 tooltip**：全部 hover 提示走 `data-tip` 属性 + 事件委托的主题化浮层（原生 title 的黑底白字弃用）。
- **设置页**：远端仓库地址（清空 = 解绑）、重新授权 / 解绑 GitHub（单按钮按绑定状态派生）、记住密码天数、自动锁时限、修改后自动推送、主题与语言（本机偏好，即时生效）。保存设置一次性全量落盘。
- **应用内更新（D16）**：设置页检查更新，非破坏性一键升级，破坏性引导手动。
- **自动锁（硬锁）**：空闲 N 分钟（默认 10）销毁解密状态并清除记住的密码；初始化中间态（授权/绑定意图）一并作废。
- **提示词简约**：全部用户可见文案遵循简约六则（D30），GUI 中英双语各自成文。
- 前端只做展示与输入，**一切读写通过 Tauri command 走 core**；前端代码里不出现任何密钥处理逻辑。

---

## 9. 安全清单

| 项 | 措施 |
|---|---|
| 仓库内容 | 永远只有密文（vault.mkv） |
| 主密码 | 不写入任何文件；"记住"仅存于 OS 凭据库（§9.1），`mak passwd unset` / GUI 锁定随时清除；内存中 zeroize |
| 内存 | 派生密钥、密码用 `zeroizing` 包裹；承认 JSON 反序列化产生的明文 String 无法完全避免——文档如实声明此限制 |
| 剪贴板 | CLI 复制后 30s 尽力清除；GUI 同（提示自动清除） |
| 并发 | 文件锁防双实例写坏 vault；写命令持写闸门互斥 |
| 导出 | 明文导出需二次确认，文件 0600，界面警示 |
| 找回 | 无。首次设置密码时界面显著提示"忘记=丢失全部数据" |
| 网络 | 仅两类出站：用户配置的 base_url 校验请求、GitHub API 同步/授权。无遥测 |

### 9.1 主密码记忆机制

**首次输入成功后记住；密码验证失败或用户修改时自动失效。** 存储：OS 凭据库（keyring）——Windows Credential Manager / macOS Keychain / Linux Secret Service（无头环境自动降级为每次手动输入，`--no-default-features` 显式关闭）。

- 条目：service = `myapikey`，account = `default`（主密码本体）与 `github` / `github-refresh`（GitHub 令牌）。存主密码本体而非派生密钥——KDF 参数可随版本演进。
- 解锁流程：凭据库有密码 → 解密 vault → 成功静默进入；失败删除条目回退手动输入。
- 修改主密码：验证旧密码 → 重加密 vault → 更新凭据库条目。
- **威胁模型变化（如实声明）**：记住密码后，本机防线 = 操作系统登录 + 凭据库权限。防"仓库泄露 / 传输窃听 / 密文离线爆破"不变，但不再防"设备失窃且已登录"。

---

## 10. 分发

- CI（GitHub Actions）三平台矩阵：`cargo test` + `clippy` + `fmt`；GUI 构建带 Rust/npm 缓存。
- Release 产物：三平台 CLI 单二进制（各含 `myapikey` + `mak`）；**GUI 安装包仅 Windows**（nsis）+ `.sig` 签名 + `latest.json`。
- 更新器产物链（D16）：CI secrets 注入签名密钥；`scripts/make-latest-json.py` 生成清单（带版本一致性断言）。
- macOS：M5 前暂无 GUI；CLI 触发 Gatekeeper 隔离，文档提示 `xattr -d com.apple.quarantine`。
- 渠道：GitHub Releases（更新通道只认正式 Release）；`cargo install`、scoop/winget/homebrew 后续按需。

---

## 11. 里程碑

| 阶段 | 内容 | 状态 |
|---|---|---|
| M0 | workspace 骨架 + core：模型、vault 加解密（含 roundtrip/篡改测试）、文件锁 | ✅ |
| M1 | CLI 全命令 + 同步 + probe | ✅ 日常可用 |
| M2 | Tauri GUI（解锁/列表/编辑/搜索/设置/同步状态、空闲自动锁、剪贴板清除），仅发布 Windows | ✅ |
| M3 | 导入导出、应用内更新（D16）、两端模糊搜索、dialoguer 交互菜单、三批代码审查 | ✅ v1.0.0 发布（2026-10-26） |
| M4 | 零外部依赖（D25，2026-10-29）：移除系统 git，GitHub Contents API 直连 | ✅ 2026-10-31 完成 |
| M5 | macOS GUI 适配（D26）：仅 Apple Silicon | 🚧 待启动 |

---

## 12. 历史决议存档

- **D17–D23**（导入/供给/配方引擎相关，2026-10-27~28）：已随 D24 于 2026-10-29 整体删除，相关代码（apply/restore/配方引擎/`[[field]]` 字段模型）一并移除。
- **生物识别解锁**（Windows Hello / Touch ID）：2026-10-30 讨论后否决，勿再提议（见 §1.2 非目标）。
- **Codex model_catalog_json**：2026-10-29 决议不做。
