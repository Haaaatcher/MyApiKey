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
| D9 | 主密码输入一次即记住（CLI 语义），存 OS 凭据库，**默认 7 天有效（`remember_days` 可配，0=禁用）**；每次输入续期；密码验证失败自动失效；`forget` = 立即锁定；**GUI 不消费记忆：解锁页（启动/锁定后）一律手动输入；GUI 锁定（手动/空闲）= `forget` 语义清除记忆（2026-10-24 决议）** | 体验优先（CLI 至多每周输一次）；到期时间存配置目录 `remember.json`（仅时间戳）；2026-10-24 由永久记住改为 TTL，同日锁定语义改硬锁、GUI 去除一键解锁 |
| D10 | vault 格式自研（Argon2id + AES-256-GCM + 版本/AAD 头） | 30 行格式换完全可控的演进与防降级 |
| D11 | 命令同时提供长名 `myapikey` 与短名 `mak` | 同一实现，两个 bin 入口 |
| D12 | GUI 首发只做 Windows；Linux/macOS 暂时 CLI-only | 集中火力；Tauri 代码保持跨平台，后续可补齐 |
| D13 | GitHub 一键初始化：`mak init --github [仓库名]`，OAuth Device Flow（RFC 8628）鉴权后自动创建/识别私有仓库；token 存 OS 凭据库（account=github），经环境变量注入 git Authorization 头（不进命令行/不落盘）；`mak forget --github` 可清除 | 免去用户手动建仓库+复制 URL；browser 授权一次即可；scope 仅 `repo`（GitHub 模型限制，无法按单仓库收窄，需文档明示） |
| D14 | GUI 外观设置：主题（明亮/暗黑/跟随系统，CSS 变量双主题 + `prefers-color-scheme` 监听）与界面语言（中文/英文，前端字典 i18n，默认跟随系统语言）。两项均为**本机 UI 偏好**，存 localStorage（不进 config.toml、不参与同步）。CLI 语言策略不变（默认英文） | 主题/语言是设备级个人偏好，随仓库同步会强加给其他设备；localStorage 零后端改动、零敏感面（2026-10-24 决议） |
| D15 | 窗口与进程策略（2026-10-25 需求）：**托盘常驻**（关闭/最小化均收进系统托盘，会话保活；托盘菜单 显示/退出，显式退出才结束）；**单实例**（tauri-plugin-single-instance，二次启动聚焦已有窗口）；**启动状态跟随上次**：上次未锁定退出且记住的密码在有效期内 → 静默恢复解锁；锁定（手动/自动，会清记忆）或过期 → 必须输入主密码 | 托盘保活让"恢复到上次状态"无需把主密码落盘（明文永不写文件是铁律）；静默恢复与"锁定=必须重输"不冲突：锁过必然清了记忆 |
| D16 | 应用内更新（2026-10-25）：GUI 集成官方 `tauri-plugin-updater`，更新源为 GitHub Releases 的 `latest.json`（`scripts/make-latest-json.py` 生成），安装包 Ed25519 签名（私钥 `%USERPROFILE%\.tauri\myapikey-updater.pem`，由我保管）；设置页手动检查、下载验签、静默安装、重启。**token 失效自愈闭环（2026-10-26）**：设置页「重新授权 GitHub」走设备流刷新 token（不重建 vault），写命令入口每次重读凭据库保鲜认证——另一端刷新授权后本端立即可用。**破坏性守卫**：Release notes 首行 `[breaking]` 标记或主版本号提升 → 拒绝自动安装，引导先备份再手动升级。`releases/latest/download` 只认正式 Release，prerelease 不进更新通道。CLI 不做自替换：`mak version` 检查后提示 `cargo install --git <repo> --tag <v> myapikey-cli` 升级命令（现阶段唯一实际通道是 cargo；Release 二进制分发启用后可并行提示） | 普通更新零门槛（管 key 的工具要能及时拿到修复）；vault 格式变更属破坏性（红线需我批准），更新通道与红线在同一关卡汇合；签名防中间人是凭证管理器的安全底线 |
| D17 | **导入并入添加流，且为次级路径**（2026-10-26 我决议：导入与添加同质，都是创建记录；同日修订：导入是低频次级功能，不与手动输入并列）：CLI `mak add` 缺省走纯手动向导（交互与既有版本完全一致），`mak add --import` 附加参数直达扫描导入（多选勾选、批量入库、同名同 URL 幂等跳过；非 TTY 降级为序号行输入，可管道驱动）；GUI 添加抽屉默认手动表单，actions 行左侧一个 ghost 次级链接切换到导入面板（导入态翻转为「返回手动填写」，不与手动并列 tab）。独立 `mak scan` 命令与设置页扫描区移除（单一入口 + 主次分明）；GUI 后端 `scan_apps`/`import_records` 命令不变 | 凭证进 vault 只有一个入口（add），扫描只是 add 的"从现实预填"；主路径（手动）不被次级功能打断——来源菜单/并列 tab 会把每次手动添加都拖进一次额外选择；`--import` 让扫描路径反而可被脚本直接驱动 |
| D18 | **生态集成：尽可能对等 + 领域分表 + 只认官方语义变量**（2026-10-26 我定调并澄清）：①对等是方向不是硬约束——导入与供给力争对称，但"只写有把握的变量/格式"优先，无把握则只做导入并如实标注；②**环境变量与应用程序分开两张表**描述（进程级变量约定与各应用配置文件是两回事，行为不同）；③**custom 协议不映射环境变量**——记录名造的 `<NAME>_API_KEY` 无从保证任何应用读取，规则删除（实现同步：`env_pairs_for` 仅映射 openai/anthropic，custom 返回空；CLI 提示不可写、GUI 对 custom 记录隐藏写入按钮）。已知对等缺口列 ecosystem.md 文末，随修复批次处理；第二批 GUI 应用（Cherry Studio/Chatbox）供给需"写配置文件"形态，随第二批设计 | 混在一张表会掩盖两类事物的行为差异；造变量名的规则无法验证正确性，宁可不做；硬凑对等会逼出"写没把握的东西" |
| D19 | **移除声明式源清单，逐软件专属接入**（2026-10-26 决议；文档当日重写，代码 2026-10-27 迁移完成）：不再维护 `sources.toml` 通用清单与五种解析器，每个受支持的智能体软件写**专属解析模块**逐个攻破（`core/src/discovery/`，各带夹具单测）；**导入范围收缩为智能体软件的配置文件**——环境变量族（11 家 `*_API_KEY`）与通用 `~/.env` 不再扫描，环境变量只作为供给载体（在各软件章节内说明）；`docs/ecosystem.md` 按软件逐章组织并标注软件类型（CLI/TUI · GUI 桌面 · IDE 插件），落地明细见其文末 | 各家配置的私有语义重（字段引用、嵌套结构、变体位置），清单表达力不够会引入静默错误（如 protocol 裸值静默回退）；"加配置即支持"的通用抽象在格式各自私有的现实下不成立，专属代码每个都确定正确 |
| D20 | **移除导入，M5 收敛为纯供给**（2026-10-27 决议，先文档后代码；导入侧的 D17/D19 随之失效）：删除扫描导入全链路（`core::discovery` 模块、`mak add --import` 旗标、GUI 导入面板与 `scan_apps`/`import_records` 命令、mock 与文案）；保留供给侧（`mak env` / GUI 写入，映射函数 `env_pairs_for` 迁至 `core::supply` 模块）。动因：装机实测三类智能体均为账号登录态或 key 不落盘（Qwen 的 `envKey` 引用指向无值变量 `ZAI_API_KEY`），扫描在真实机器上收编不到凭证 | 供给是日常高频路径（配一次到处用），导入是一次性收编且实际收编不到东西；功能的维护面以真实收益衡量；D18"只写有官方语义的变量"原则继续有效 |
| D21 | ⚠️ **已随 D24 移除**（原决议：供给 = 写配置文件）——（2026-10-27 决议，先文档后代码，取代 D20 的环境变量供给）：`mak apply <记录> --to claude-code\|codex\|qwen` 把记录写进目标应用配置文件（各应用写入矩阵见 [ecosystem.md](ecosystem.md)）；GUI 卡片「配置到应用」。**环境变量供给（`mak env` / GUI 写入按钮）整体移除**——不做用不上的能力（写环境变量太简单，无产品价值）。三条铁律：①mak 只动自己拥有的键，读-改-写保留其余字段；②写入前滚动备份 3 份 + `mak restore <app>` 还原；③目标应用内 key 明文与否是应用自己的存储设计，与 mak 无关——mak 只保证 vault 内加密。写入器按 D19 的漂移对策实现（能力探测/保字段/写后验证），TOML 保格式用 `toml_edit` | 手写各家私有 schema 易错且要扛版本漂移，一键化才是产品价值；参考火山 Ark Helper（向导式写 settings.json）与 CC-Switch（多供应商切换，Issue #902 印证写入必须以磁盘现状为准） |
| D22 | ⚠️ **已随 D24 移除**（原决议：声明式应用配方）——（2026-10-28 定稿；当日两轮演进：厂商×应用成对脚本 → 声明式 per-app toml，取代 D21 的 Rust 写入器；2026-10-29 交互细化）。触发点=BigModel 官方文档实证 Claude Code 需要模型槽位映射（`ANTHROPIC_DEFAULT_OPUS/SONNET/HAIKU_MODEL`）+ 厂商增强项，这些知识无法通用化。**配方按 Agent 应用组织**（claude-code / codex / qwen 各一份声明式 toml，内置编译 + `config_dir/recipes/` 同 id 覆盖，坏文件警告跳过回退内置），mak 内置引擎解释执行（操作集封闭：set / set_models / 模型槽位 / 询问项；JSON/TOML 双后端）。**模型槽位为配方声明的任意槽位名**（2026-10-29：claude-code 四槽 opus/sonnet/haiku/fable、codex 单槽 main；引擎从槽位映射表按名取值，取代有序 primary/secondary）。**apply 流程定序：选目标应用（主体）→ 选记录（数据源）→ 逐项填数据**——记录里有的询问"是否用这个值"（key 掩码展示）；槽位与询问项的提示直接展示**目标配置键名 + 字段含义**，**置空回车 = 跳过该字段，配方不携带建议默认值**（2026-10-29 决议：建议值属厂商文档，mak 不代持）。引擎职责：滚动备份 + 失败自动回滚 + 原子写 0600 + 保字段读改写。曾评估方案：脚本（交互弱/双份实现/解释器依赖）、厂商成对配置（太重）、WASM（过重）均否，声明式 toml 胜出（一份通吃三 OS/零依赖/纯数据安全/漂移修复改文件即生效） | 模型映射与增强项是厂商/应用私有知识，写死在 Rust 里每家都要发版；配方即数据，漂移修复=改一个 toml；apply 主体是 Agent 应用，记录只是数据源；提示键名+含义让用户知道写了什么，建议值回填会替用户做决定 |
| D23 | ⚠️ **已随 D24 移除**（原决议：行为层配方 = 统一字段模型）——（2026-10-29 决议，D22 的结构化升级）。配方体系三层显式化：**交互层**（CLI/GUI）读**行为层**（recipes toml）渲染交互、从**数据层**（vault 记录）取值预填——行为层文件自足，交互层只读文件即可完成配置。五种操作（set/value/model/ask/set_models）收敛为统一 `[[field]]` 字段模型，四要素：`key`（写入点路径）、`source`（数据来源声明：`{record="api_key"\|base_url}` / `{slot="opus"}` / `{models="all"}`）、`value`（常量插值模板，不经用户）、`desc`（含义说明），辅以 `secret`（值不回显）、`required`、`remove_if_empty`、`entry`（条目组 base_url_field/env_key）。**交互形态由声明推导**（交互层唯一渲染逻辑）：secret+有预填 → 确认采用；非 secret+有预填 → 预填输入（WYSIWYG）；slot → 模型单选（含 skip）；models → 多选暴露；无 source → 空表单项。引擎值袋统一为 `answers`（写入路径 → 最终值），`slot_models` 删除；slot 字段的映射结果也进 answers。三家配方的 key/url 统一为 source 字段（codex 由插值改为交互确认——apply 即切记录，确认有防呆价值）。导出指纹机制（同日）负责覆盖目录的格式迁移：未改动的旧导出在内置更新时自动刷新。**协议亲和**（同日增补）：配方顶层声明 `protocol = anthropic|openai|any`（claude-code/codex/qwen 各异），apply 与记录协议二值化比对、错配在 base_url 字段处显式警告不阻断——anthropic 端点配 Codex responses 方言这类坏配置必须在填表时被看见。**范围边界**（同日）：Codex `model_catalog_json`（千问 Responses 模式要求的每模型元数据目录）经评估**不做**——属切档体验而非凭证供给，元数据漂移维护面大，交由 CC Switch 等切档工具承担；枚举值域字段配 `options` 走选择菜单（wire_api/effort/开关类），自由输入仅留给连续值。GUI 前端的通用字段表单为后续增量（本轮后端对齐） | key/url 预填、槽位收集、GUI 对话框此前各写一套且行为变更要改代码；字段模型把"行为"全部搬进文件，交互层只剩一个通用表单渲染器，CLI/GUI 天然一致；source 命名空间（record/slot/models，未来可扩 env/file）让新数据源不改交互层 |
| D24 | **供给整体移除，apply/restore/配方引擎下线**（2026-10-29 决议，D21/D22/D23 随之失效）。触发点：千问官方文档实证 Codex 接入的真实复杂度在 model_catalog 等切档细节，且社区已有成熟专门的切档工具 CC Switch（千问官方文档直接推荐）。mak 退出供给赛道，定位收敛为**凭证底座**（加密 vault + git 多设备同步 + 记录管理），切换/目录维护交给专门工具。代码全删：`core::supply`（配方引擎/备份滚动/restore）、三份内置配方、CLI `apply`/`restore`、GUI「配置到应用」全链路。用户已写入的目标应用配置与 `.mak-bak-*` 备份不回收（文件留在原地，mak 不再管理） | 供给与凭证底座是两个赛道：凭证底座的壁垒是加密+同步+记录模型，切换器的壁垒是快节奏跟进各家 schema——千问 model_catalog 的维护面实证了后者会持续吞噬开发预算；承认专业分工，把工程预算还给底座 |

---

## 3. 总体架构

### 3.1 Workspace 结构

```
myapikey/
├── Cargo.toml            # workspace
├── crates/
│   ├── myapikey-core/    # 纯逻辑库：模型、vault、同步、校验（无 UI 依赖）
│   ├── myapikey-cli/     # CLI（clap + dialoguer；产出 myapikey 与 mak 两个二进制）
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
                  (dialoguer)           Tauri command 调 core
```

- core 只暴露"给我密码字节，我完成加解密/同步"级别的 API；**密码采集方式完全留给前端**（CLI 用 dialoguer Password，GUI 用原生输入框）。
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
    pub models: Vec<String>,   // 模型名列表（2026-10-27 增补，D21 供给用：一份 key 对多模型；只存名字不存参数）
                               // 不变量：至少一项——空读取/保存时归一化为占位 "unknown"（写入器跳过纯占位，端点不会收到该名字）
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
- **模型列表自动获取**（2026-10-27 增补）：探测的 200 响应体（`data[].id`）
  即模型列表——`mak add` 填完 key 后自动拉取（Enter 收下 / 手输覆盖 /
  失败回落手输），GUI 抽屉提供「获取模型列表」按钮；Custom 协议不拉取。
  与探测共用网络边界：只出站到记录的 base_url，失败不阻塞（D5）。

---

## 7. CLI 设计（myapikey-cli）

```
myapikey init                # 初始化：设主密码（二次确认+强度提示）、git init、可选配远端
myapikey add                 # 交互式添加：上下键选协议（OpenAI/Anthropic/Custom，选 Custom 才追问标签），之后名称/Base URL 必填、key 隐藏输入；命令行参数始终优先
myapikey version            # 只显示当前版本，不联网
myapikey update             # 联网检查新 Release，提示 cargo 升级命令
myapikey search <query>      # 模糊搜索记录（名称 / Base URL / 备注）
myapikey list                # 列表（默认掩码 key，显示校验状态）
myapikey show <id|name>      # 查看单条（--copy 复制 key 到剪贴板）
myapikey edit <id|name>      # 修改
myapikey delete <id|name>    # 删除（墓碑）
myapikey check [id|name]     # 校验（不带参数=全部）
myapikey sync                # pull + merge + push
myapikey apply <t> --to <app> # 把一条记录写进目标应用配置文件（claude-code/codex/qwen，D21）
myapikey restore <app>        # 还原目标应用最近一次写入前的配置备份
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
- **解锁页**：锁定后一律手动输入主密码（2026-10-24 决议）；**启动状态跟随上次**（D15，2026-10-25）：上次未锁定退出且记住的密码有效 → 静默恢复解锁直接进主界面，否则手动输入。错误提示统一为"密码错误"（不区分格式错误）。
- **密码可见性切换**（2026-10-25）：初始化页/解锁页的主密码框带行内眼睛按钮切换明文/掩码；WebView2 对 `type=password` 的原生"眼睛"（`::-ms-reveal`）已在 CSS 全局隐藏（只在悬停出现、点击一次即消失，行为不受控）。语言切换重渲染时已填字段按 `data-field` 快照回填，不再清空。
- **搜索（单一实现 core::search，2026-10-26 下沉核心层）**：fzf 风格模糊匹配，子串精确命中优先于子序列模糊命中，按评分排序；名称/备注整字段参与，URL 剥协议前缀后按 / 分词逐段参与（域名段权重高、路径段低）；协议不参与（兼容性标签）；短查询按**字符数**判定（1-2 字符只做子串，双汉字不误入子序列）。CLI 用 `mak search <query>`（list 只做全量）；GUI 前端经 `search_records` 命令调 core，带逐键竞态守卫；浏览器 mock 里的 TS 复刻仅服务开发预览，改规则以 Rust 为准。
- **git 依赖预检**：启动即探测 git，缺失进安装引导页（打开 git-scm 下载页 / 复制 winget 命令 / 装好重试），主流程不开始；不做自动安装（用户知情权）。
- **主列表**：记录卡片（name、protocol 徽标、base_url、校验状态点），搜索框；顶栏显示同步状态（上次同步时间/未推送数/失败标记）+ 手动同步按钮。宽屏自适应多列网格（内容上限 1400px，卡片按宽度自动 1/2/3 列，2026-10-26 我选定）。搜索为 fzf 风格模糊匹配（2026-10-25）：子串精确命中优先于子序列模糊命中，按评分排序；名称/备注整字段参与，**URL 剥掉协议前缀后按 / 分词、逐段参与**（域名段权重高、路径段权重低——域名是服务商身份，路径是端点细节；分段天然阻断跨段子序列伪命中）；**协议不参与**（兼容性标签，DeepSeek 等都用 openai 协议，按它搜会命中所有兼容服务商）；**1-2 字符短查询只做子串精确匹配**；零依赖自实现，百条规模每键全量重算。
- **编辑抽屉**：新增/编辑表单；key 输入框默认掩码可切换；protocol 用三个选项（OpenAI / Anthropic / 自定义文本）；添加抽屉内的导入入口已随 D20 移除（导入功能整体下线）。点击遮罩关闭带 mousedown 守卫：WebView2 的密码自动填充建议被选择时会向页面合成 click，落点可能在遮罩上，无"按下"过程的合成点击被过滤，避免抽屉被误关（2026-10-25）。
- **抽屉草稿（2026-10-26）**：误触关闭（遮罩/Escape/取消）不丢已填内容，重开恢复（含 key 明文/掩码状态，静默恢复不提示）；保存成功或应用锁定（手动+自动）时清除。
- **表单校验（2026-10-26）**：保存时全量检查所有字段（名称/Base URL/API key[新增必填]/自定义标签），错误一次性全部行内标出——红框 + 聚焦 + 提示紧贴出错框下方，修改即消；Base URL 校验 http(s)+主机，远端地址另用宽松规则（任意 scheme/本地路径放行，只拒裸域名）。
- **复制按钮**：一键复制 key，30 秒后尽力清空剪贴板（best-effort，OS 限制会注明）。
- **设置页**：远端仓库地址、自动同步开关、自动锁时限、外观（主题明亮/暗黑/跟随系统 + 界面语言中文/英文，D14：本机偏好存 localStorage，即时生效）。墓碑保留期已从设置页移除（2026-10-25 决议：同步引擎的内部正确性参数，默认 30 天，不面向用户；进阶用户可改 config.toml 的 `tombstone_days`）。
- **应用内更新（D16，2026-10-25）**：设置页"更新"区显示当前版本 + 检查更新；非破坏性更新一键"升级并重启"（下载 → Ed25519 验签 → NSIS 静默安装 → 重启）；破坏性更新（notes 首行 `[breaking]` 或主版本提升）不提供一键升级，只显示备份与手动升级指引。冷启动说明：自更新只能更新"已装了 updater 的版本"，首个带此功能的版本需手动安装一次。
- **自动锁（硬锁，2026-10-24 决议）**：空闲 N 分钟（默认 10）后销毁内存中的解密状态，**并清除记住的密码（= `forget` 语义，手动 Lock 按钮相同）**——锁定意味着必须重新输入主密码。**GUI 解锁页一律手动输入**（启动亦然）；"记住的密码"（TTL）只服务 CLI。它防的是离开座位时的顺手查看，不防恶意软件（与 D9 的威胁模型一致，见 §9.1）。
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

- CI（GitHub Actions）三平台矩阵：`cargo test` + `clippy` + `fmt`；GUI 构建带 Rust/npm 缓存。
- Release 产物（按 Q12 决议）：三平台 CLI 单二进制（各含 `myapikey` + `mak`）；**GUI 安装包仅 Windows**（nsis）。
- **更新器产物链（D16）**：GUI 构建注入签名密钥（CI secrets `TAURI_SIGNING_PRIVATE_KEY[_PASSWORD]`）产出安装包 + `.sig`；release job 自动生成 `latest.json` 一并挂附件。本地等价流程见 AGENTS.md 发布节与 `scripts/make-latest-json.py`（带与 tauri.conf.json 的版本一致性断言）。
- macOS：暂无 GUI，不做签名/公证；浏览器下载的 CLI 会触发 Gatekeeper 隔离，文档中提示 `xattr -d com.apple.quarantine` 解法，后续再评估开发者账号。
- 渠道：GitHub Releases 起步（更新通道只认正式 Release，prerelease 不进 latest/download）；`cargo install`、scoop/winget/homebrew 后续按需。

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
| M3+ 打磨（2026-10-26） | 应用内更新（D16）、两端同源模糊搜索（core::search）、CLI dialoguer 交互菜单、GUI 体验细节（密码可见切换/自绘确认弹窗/托盘/单实例/中英主题）、三批代码审查修复 | 待发布的首个 1.0 |
| M4（计划中，2026-10-26 我提出；技术调研 2026-10-26 完成） | **消除对系统 git 的依赖**：以嵌入式库替换子进程 git.exe，运行时零外部依赖。**用量盘点**：本应用只消费 git 的窄子集——对象库读写（单文件 vault）、线性 commit、引用管理、smart HTTP fetch/push（自定义 Authorization header）、ff-only 前进；**数据层合并在应用内完成，不依赖 git 合并引擎**。**选型结论**：首选 `gix`（gitoxide，纯 Rust，库级生产可用，cargo 自用）——但 2026-08 有 HTTP Basic 凭据误发旁路主机的 CVE（CISA SB26-243），push 阶段必须 pin 修复版并加认证注入专项测试；备选 `git2-rs`（libgit2 静态链接，API 完整成熟，同样达成运行时零依赖，代价是 C 工具链参与编译）。**分阶段**：P1 本地对象/引用/commit 内置化（护栏：与 git CLI 产物逐字节互通校验）→ P2 smart HTTP fetch/push + header 认证 → P3 灰度双后端（环境变量 `MYAPIKEY_GIT=lib|cli`，同步测试矩阵双跑）→ P4 删除子进程路径与安装引导页 | 无 git 依赖的 1.x |
| M5（**供给 = 写配置文件**，D21 2026-10-27 定调） | **生态集成 = 凭证供给**：`mak apply <记录> --to <app>`（CLI）/ GUI 卡片「配置到应用」，把记录写进 Claude Code / Codex / Qwen Code 的配置文件（切换语义、只动 mak 命名空间的键、滚动备份 + `mak restore` 还原、写后验证）；各应用写入矩阵与 schema 漂移对策见 [docs/ecosystem.md](ecosystem.md)（唯一详细记录）。导入（D17/D19/D20）与环境变量供给（`mak env`）均已移除——不做用不上的能力 | 从"管理凭证"到"供给凭证" |

MVP 原则：**M1 完成即满足全部核心场景**（记录 + 加密 + git 同步 + 校验），GUI 与打磨都是增强。
