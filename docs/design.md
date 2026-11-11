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
4. 其他设备拉取后，输入解锁密码即可解密使用；
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
| D9 | ~~主密码输入一次即记住，默认 7 天有效（`remember_days` 可配，0=禁用）~~ → **被 D32 修订**（记住副本保留为实现细节，时长并入自动锁定） | 见 D32 |
| D10 | vault 格式自研（Argon2id + AES-256-GCM + 版本/AAD 头） | 30 行格式换完全可控的演进与防降级 |
| D11 | 命令同时提供长名 `myapikey` 与短名 `makara` | 同一实现，两个 bin 入口 |
| D12 | GUI 首发只做 Windows；Linux/macOS 暂时 CLI-only | 集中火力；Tauri 代码保持跨平台，后续可补齐 |
| D13 | GitHub 一键初始化：~~`makara init --github [仓库名]`~~ → **收敛为 `makara init [仓库]` 交互式**（无参询问是否同步、仓库名预填；带参跳过询问），OAuth Device Flow（RFC 8628）鉴权后自动创建/识别私有仓库；token 存 OS 凭据库（account=`github`），以 Bearer 头走 REST API；**poll 表单必带 `scope=repo`（2026-10-30 根因修复：缺失则颁发零权限令牌）** | 免去用户手动建仓库+复制 URL；browser 授权一次即可 |
| D14 | GUI 外观设置：主题（明亮/暗黑/跟随系统，CSS 变量双主题 + `prefers-color-scheme` 监听）与界面语言（中文/英文，前端字典 i18n，默认跟随系统语言）。两项均为**本机 UI 偏好**，存 localStorage（不进 config.toml、不参与同步）。CLI 语言策略不变（默认英文） | 主题/语言是设备级个人偏好，随仓库同步会强加给其他设备；localStorage 零后端改动、零敏感面（2026-10-24 决议） |
| D15 | 窗口与进程策略（2026-10-25 需求）：**托盘常驻**（关闭/最小化均收进系统托盘，会话保活；托盘菜单 显示/退出，显式退出才结束）；**单实例**（tauri-plugin-single-instance，二次启动聚焦已有窗口）；**启动状态跟随上次**：上次未锁定退出且记住的密码在有效期内 → 静默恢复解锁；锁定（手动/自动，会清记忆）或过期 → 必须输入主密码 | 托盘保活让"恢复到上次状态"无需把主密码落盘（明文永不写文件是铁律） |
| D16 | 应用内更新（2026-10-25）：GUI 集成官方 `tauri-plugin-updater`，更新源为 GitHub Releases 的 `latest.json`（`scripts/make-latest-json.py` 生成），安装包 Ed25519 签名；设置页手动检查、下载验签、静默安装、重启。**token 失效自愈闭环（2026-10-26）**：设置页「重新授权 GitHub」走设备流刷新 token（不重建 vault）。**破坏性守卫**：Release notes 首行 `[breaking]` 标记或主版本号提升 → 拒绝自动安装，引导手动升级。CLI 不做自替换：`makara update` 检查后提示 `cargo install --git <repo> --tag <v> myapikey-cli` 升级命令。**⚠ 2026-11-11 被 D40 修订：CLI 改为零依赖自更新（Release 附件下载 + rename 替换），cargo 指引废弃——它要求用户装 Rust 工具链，与大众产品定位背离** | 普通更新零门槛（管 key 的工具要能及时拿到修复）；签名防中间人是凭证管理器的安全底线 |
| D17–D23 | （已删除：导入/供给/配方引擎相关决议，随 D24 整体关闭） | 见 D24 |
| D24 | **生态集成与凭证供给永久关闭**（2026-10-29 用户拍板）：apply/restore/配方引擎/`[[field]]` 字段模型整体删除，切换凭证交给 CC Switch 等专业工具。**勿再提议** | 做复杂有成本的事（各家私有 schema/漂移修复）；简单替代品（setx/手抄 export）不值得做 |
| D25 | **M4 零外部依赖（2026-10-29）**：移除系统 git，同步改 **GitHub Contents API 直连**（`SyncStore` trait：`fetch_head` / `put_blob`(CAS) / `download_ref` + GitHub 实现 + 测试用内存实现）；单文件 CAS 语义，409 冲突自动重拉重合并（限 3 轮）；本地状态 = `vault.mkv` + config 的 `last_sync_sha`/`push_pending`/`last_sync_at`/`remote_repo`；远端仅支持 GitHub（`owner/name` 归一化，SSH 形态拒绝）；status 的 ahead/behind 提交数 → `pending` 布尔；老仓库无缝迁移（Contents API 直读现有 `vault.mkv`，历史 commit 全保留） | gitoxide push 至 2026 未实现（fetch ✓/push ✗），libgit2 引入 C 编译链；GitHub Contents API 单文件 ≤1MB 全功能且每次 PUT 自动产生 commit（网页历史无损） |
| D26 | **M5 macOS GUI 适配（2026-10-30 立项）**：窗口应用发布 macOS 版，仅 Apple Silicon（`aarch64-apple-darwin`），不做 Intel/Universal | 里程碑编号沿用 M5（原生态集成 M5 已随 D24 关闭） |
| D27 | **同步状态数据层单一化**（2026-10-31）：**config.toml 是同步状态的唯一数据源，显示一律从数据派生**——① `mark_synced` 记账唯一入口（三件套原子写入），解绑走对称的 `unbind_remote`；② `remote_repo` 恒为归一化 `owner/name`；③ GUI「忘记 GitHub 授权」= 清令牌 + 解绑一次写清 | 状态字段散落多处手写必有遗漏（GUI 接入后显示 never 等全是手写漂移） |
| D28 | **协议五值扩充**（2026-10-30，用户提供主流协议分类表）：`OpenAi`=Responses（默认，官方推荐新接口）/ `OpenAiChat`=Chat Completions / `Anthropic`=Messages / `AnthropicComplete`=Text Completions（legacy）/ `Custom`。命名规则：**无后缀 = 最新协议，带后缀 = 旧协议**。旧 vault 值（openai/anthropic）完全兼容；探测与模型拉取按两大家族分派（新旧 API 共享同一套 /models） | 真实平台同时存在新旧两种 API 形态，单值无法表达 |
| D29 | **搜索协议参与**（2026-10-31 用户需求，推翻 2026-10-25 "协议不参与"决议）：`Protocol::search_terms()`（标签 + 完整形态词）以**权重 3（全字段最低）**参与匹配——兼容服务商全员同协议时仍排在名称/URL 命中之后，旧决议担心的噪音用低权重化解 | 用户预期按协议类型找记录（搜 OpenAI 找到走 openai 协议的千问） |
| D30 | **提示词简约六则**（2026-10-31 用户定调"简约是设计风格"）：一句一事；删可推断；动词开头；错误 = 现象 + 出路；术语统一；双语各自成文。细则见 AGENTS.md「代码风格 · 提示词」 | 啰嗦的提示词让用户找不到重点 |
| D31 | **同步单文件大小护栏**（2026-11-01）：全部上传路径经 `push_one` 唯一收口，put_blob 前检查密文 ≤ `MAX_SYNC_VAULT_BYTES`（1_000_000 字节，十进制取小值，确保先于 GitHub 硬限制触发）；超限报 "vault is too large to sync" + 清理出路；本地保存与下载不受影响 | 1MB 是 GitHub Contents API 的硬约束（D25）；超限时 GitHub 的原始 4xx 语焉不详，护栏把失效模式变成可执行的出路 |
| D32 | **解锁密码语义统一**（2026-11-03 用户定调）：**解锁密码 = 主密码**（英文统一 `passphrase`），用户只感知解锁密码，不感知 vault 加密密钥；「记住密码时长」设置删除，**唯一时间参数 = 自动锁定**（`auto_lock_minutes`，分钟；GUI 以预设档位下拉呈现：5/10/15/30 分钟、1/4/8 小时、永不，存量值就近归档）：解锁成功/持续使用 → 记住副本滚动续期至 `now + N 分钟`（重启时未到期 = 免输恢复解锁，机制保留为实现细节）；空闲到期或手动锁定 = 清会话 + 清副本；**0 = 永不锁定**（副本不过期）；`remember_days` 字段删除（旧 config.toml 字段被 serde 忽略）；`makara passwd unset` 删除，语义并入新命令 `makara lock`；`remember.json` 结构不变；设置保存时若会话解锁中**立即按新时长重写记住副本**（2026-11-04 修复：只改配置不动副本，旧 TTL 过期后重启撞密码要求，「永不」形同虚设）。**长度硬性限制 8–128 字符**（2026-11-04 用户要求，`vault::validate_passphrase` 三层校验） | 「记住密码时长」与「自动锁定」是同一心智的两个参数，并存让用户困惑；主密码不再以可逆副本形式长期驻留（TTL 随使用滚动）；vault 加密机制（Argon2id 派生、多设备同密码、忘记不可恢复）完全不变 |
| D35 | **授权模型定稿：OAuth App + 令牌永不过期**（2026-11-04，GitHub App 迁移路线与「开启过期 + 自动续期」路线均告失败/放弃后用户拍板）。根因链：用户的 OAuth App「MakaraApp」开启令牌过期后，刷新请求被 GitHub 拒绝（`incorrect_client_credentials`，实测探测证实）——**GitHub 对 OAuth App 的刷新要求携带 `client_secret`**（GitHub App 才免 secret），桌面应用保管/传递 secret 不安全也不现实。最终形态：OAuth App **关闭** Expire user authorization tokens → device flow 一次性授权 → 令牌永不过期，无刷新环节；撤销授权/用户手动撤销才需重新授权。`incorrect_client_credentials` 的出现含义即"刷新请求对当前 App 配置不合法"（探测手法：假令牌直打刷新端点）；GitHub App 迁移路线（建 App+配权限+安装+换 client_id）因配置转嫁最终用户而放弃（D34 交付版即可用）。**⚠ 2026-11-10 被 D37 修订：过期实际持续生效，「永不过期」前提不成立，当前形态见 D37** | 一次性授权 + 永久令牌是桌面工具在 OAuth App 模式下的正确形态；授权成本从「建 App+配权限+安装」降回「输一次码」；刷新代码保留闲置（无 secret 场景永不触发，无害） |
| D34 | **密码强度引擎改用 zxcvbn**（2026-11-04 用户要求"用大家都认可的"，推翻 2026-11-03 手写启发式决议）：`@zxcvbn-ts/core@4.2.0` + `language-common`（npm 运行时依赖，bundle +450KB，接受）；评分 0–4 映射四档（0/1→弱、2→中、3→强、4→极强）；**纯 zxcvbn 标准，不注入本地语料**（用户定调）——拼音组合等英文词库覆盖不到的模式按其原判输出（如 woaini1314→强、23571113→中，属标准的客观判断）。`passStrength` 仅做映射，手写规则（黑名单/序列/字符类）全删 | 自己制定规则不如用公认标准：手写启发式实测漏评质数拼接（23571113 评「中」）；长度硬性限制（D32）已保证进入评价的密码长度合法 |
| D33 | **设置页即时保存**（2026-11-04 用户要求）：「保存设置」按钮删除，配置改完即生效——锁定档位/自动推送**变更即写**、远端地址**失焦/回车提交**（输入过程不触发）；清空地址 = 本机停止同步（保留 GitHub 授权），确认框防误触、取消回填原值；锁定/推送保存时远端用「已保存基准」值，避免地址框编辑中的半输入被一并写入；主题/语言维持本机即时生效（D14）；非法地址标错不落库 | 两步操作（改 + 点保存）与单参数改动不成比例；显示从数据派生的原则不变（保存后 toast 反馈、失败 toast 错误） |
| D36 | **绑定/授权原子化：失效折叠为未绑定**（2026-11-10 用户定调「绑定远程仓库和有令牌应该是一个原子，用户只感知绑定远程仓库，不感知令牌」「用户只看到未绑定，不看到授权失效」）：全产品用户可见状态收敛为**「已绑定 / 未绑定」两态**，令牌失效或缺失在一切界面（CLI 文案、GUI 引导条 / 顶栏 chip / toast / 设置页）呈现为未绑定，不出现"授权 / 令牌 / 失效"字样。折叠点在数据层——「绑定」= `config.remote_repo` 有值 **∧** 凭据库令牌条目存在：CLI `remote_of` 无令牌按单机处理（sync 给单机引导；写操作跳过推送、本地照存，重绑后 sync 合并带上，数据无损）；`status` / `remote show` 同折叠；GUI `remote_of` / `status_view` / `diagnose` 同折叠（`settings_view` 刻意不折叠——折叠会让保存动作把 `None` 写回 config 静默解绑，设置页由前端 `bindingBroken()` 折叠渲染）；重新绑定走既有授权流程（`makara remote set <owner/repo>` / GUI「绑定仓库」），错误识别串 `is_auth_error` 收敛为 `GitHub rejected the access` 与 HTTP 状态模式 | 绑定与令牌是同一心智的原子，两态并存呈现（「已绑定」chip +「绑定失效」toast）互相打架且出路互斥；折叠后失效态自然获得未绑定的全部出路，用户零概念负担；诊断层曾谎报（令牌缺失时 `remote_bound` 写死）导致 UI 三处不一致，折叠统一从数据派生 |
| D37 | **授权模型现状修订：过期令牌 + 内嵌 secret 自动续期**（2026-11-10 以当前状态为准修订 D35——「关闭过期」的前提实测不成立，过期持续生效，自动续期链路修复后闭环）。当前形态：OAuth App「MakaraApp」开启 Expire user authorization tokens（access token 8h 过期）→ 请求 401 → `refresh_access_token` 静默续期 → 重试。刷新端点实测**四要件**：① **client_secret 必须携带**（OAuth App 无 GitHub App 的豁免；取值三级：env `MYAPIKEY_CLIENT_SECRET` > keychain > 编译期内嵌）② `redirect_uri=http://localhost/`（须与 App 注册的 Redirect URI 一致）③ `grant_type=refresh_token`（简单值，非 urn 长形态）④ **刷新后的新 access token 必须写回凭据库**（GitHub 每次刷新轮换令牌，漏写 = 「刷新成功仍 401」）。**secret 内嵌路线**（2026-11-06 用户拍板）：仓库根 `client-secret.txt`（gitignore，永不入库）经 build.rs 注入编译期环境变量，代码侧 `env!()` 常量访问；未注入（他人构建/CI 未配置）编译仍成功，运行时回退 keychain/env。CI 经 GitHub Secrets `MYAPIKEY_CLIENT_SECRET` 注入（AGENTS 发布流程步骤 0）。失效兜底：刷新失败 → 失效条目顺手清除 → 按 D36 折叠为未绑定，重新绑定即恢复（数据无损） | 桌面应用携带 secret 的风险由「内嵌 + gitignore + CI Secrets」控制（业界同法：VS Code / GitHub Desktop），换来无需用户干预的续期闭环；四层根因（缺 secret / redirect_uri / grant_type / 新令牌落库）逐层修复的教训——刷新失败只有「刷新成功仍 401」一类表象，必须逐层实测定位 |
| D38 | **默认同步仓库名 makara-vault + 配置化**（2026-11-10 用户要求，品牌更名收尾）：`DEFAULT_REPO_NAME` 常量 myapikey-vault → **makara-vault**，并从硬编码配置化——config.toml 新增 `default_repo_name` 字段（serde 缺省 = makara-vault，旧 config 无此字段自动补齐，兼容性有测试）；init 无参绑定 / GUI 授权刷新与接管建仓 / 设备授权探测全部改读 config。**已绑定的仓库不受影响**——远端仓库名是用户的 GitHub 资产，不做迁移 | 品牌更名（2026-11-09）遗漏了默认仓名；配置化为后续按需调整留口（用户"暂时放到配置文件中，后面可能有用"） |
| D39 | **同步仓库自动 README 防误删**（2026-11-11 用户提出：仓库只有一个数据文件，用户容易诧异误删）：`api_ensure_repo` 建仓与复用两分支统一经 `ensure_repo_readme` 确保 README.md 存在——已存在（含用户自己写的）尊重不动，缺失则写入双语说明（这是什么软件 / `vault.mkv` 是加密凭证库请勿手动编辑 / 请勿删除仓库，误删后重新绑定即可恢复，数据无损）；任何失败（无权 / 网络）只落日志不阻塞绑定主流程。老用户仓库在下一次授权刷新 / 重新绑定时自动补上 | 仓库孤零零一个 `vault.mkv` 看不出用途，是误删的高危形态；README 让仓库自解释，且不参与 SyncStore 单文件 CAS 对账（README 是仓库级说明，与 vault 同步语义无关） |
| D40 | **CLI 零依赖自更新**（2026-11-11 用户指出 cargo 升级指引违背零依赖出发点；Release 四平台压缩包自 CI 建立起就已存在）：`makara update` 检查到新版**直接自更新，不询问**（2026-11-11 用户拍板的流程）→ 下载 Release 平台附件（`makara-<tag>-<target>.zip/.tar.gz`，行内百分比进度，core `download_asset` 逐块回调）→ 系统 tar 解压（Windows 10 1803+ 内置 bsdtar 支持 zip，POSIX 原生 tar.gz，零新依赖）→ **rename 策略替换自身**——运行中的 exe 不能覆盖但**可重命名**：自身改名 `.update-old` 备份、新二进制从同目录暂存名落位原路径，下次启动生效，落位失败回滚；非交互环境同样直接更新（进度静默、只留状态行）；无预编译包的平台给手动下载出路。配套 **release.yml 打包修复**——旧步骤还在复制已删除的 `mak`/`myapikey` 二进制（发版必失败），改单 bin `makara` 平铺打包 + `makara-` 前缀附件名（update 的 URL 构造与之强耦合，测试锁定形态） | D16「不做自替换」源于 Windows 运行中 exe 自锁，rename 策略正面解决；cargo 指引要求用户装 Rust 工具链与大众产品定位背离；完整性依赖 HTTPS + GitHub 官方源（TLS 传输 + 资产不可变），与 Homebrew 同级保障 |
| D41 | **CLI 模型选择交互定稿：统一勾选式**（2026-11-11 用户定稿「空格选中和取消选中，最后提交」，推翻 D28 时代的双模式分派）：`pick_models` 不再按清单规模分派（小清单 MultiSelect / 大清单 FuzzySelect 循环）——模糊循环选中即加入、无法反悔，用户实测不便；统一 `multiselect_from` 勾选式：↑↓ 移动、空格选中/取消（可反复切换）、回车提交，长列表滚动浏览、勾错一个空格撤回；全不选回车 = 手动输入兜底（清单外私有型号），非交互态保留前 12 项宁多勿缺；拉取失败的降级手填不变（D28） | 可撤销是选择器的底线体验；两套交互模式并存徒增认知负担；列表滚动浏览在 dialoguer 渲染下可用，勾错即撤回的价值大于翻页成本 |
| D42 | **新命令 `makara key <target>`**（2026-11-11 用户提议）：复制完整 key 到剪贴板的直达命令——`show <t> --copy` 的独立形态（复制 key 是高频动作，命令名即意图）。安全红线不变：key 只进剪贴板、永不打印到终端；`show --copy` 保留兼容，show 无参提示改指 `makara key`；测试脚本子命令清单与日常任务文档同步 | `show` 的语义是「查看详情」（掩码），复制只是附带动作——直达入口让高频操作少一层绕行 |
| D43 | **删除 `makara secret` 命令与 GUI secret 粘贴通道**（2026-11-11 用户拍板——「用户不感知令牌/secret」原则的再犯清理：当时只清了 GUI，CLI 命令面残留 `makara secret`）：CLI `makara secret` 删除、GUI `save_client_secret`/`has_client_secret` tauri 命令删除（前端调用早已随 UI 删除）、core `keychain_set_client_secret` 写入函数删除；**读取级保留**——`client_secret()` 仍按 env > 凭据库 > 内嵌取值，历史用户凭据库里调试期存过的 secret 条目继续兼容；`keychain_delete_client_secret` 保留（remote unset 清理四条目路径）；开发者调试场景由 env `MYAPIKEY_CLIENT_SECRET` 覆盖（优先级高于凭据库） | secret 只属于 App 所有者，大众用户根本没有 secret 可填（用户自己也问过「Client secret 是什么」）——写入入口在内嵌路线后无存在意义；「用户不感知内部凭据」的原则必须全量清点所有用户可见面，局部清理等于没清（AGENTS 已固化为 ALWAYS 规则） |
| D44 | **模型胶囊单击复制**（2026-11-11 用户提出：右键菜单只有复制一项，不值得绕一层）：主页卡片模型胶囊单击即复制模型名（copy_text，非敏感不做 30 秒清除），右键同效并压掉 WebView 默认菜单；自绘右键菜单机制（openContextMenu/单例浮层/全局关闭监听/CSS）整体删除——唯一调用点消失后即为死代码；cursor 由 context-menu 改 pointer | 单项菜单是交互绕行；机制随唯一用途一起清掉，不留死代码（AGENTS 全量清点原则的 GUI 侧执行） |
| D45 | **export 无参数默认导出**（2026-11-11 用户要求：不指定路径就导出到当前目录）：`makara export` 无参 = 当前目录，复用目录分支的不冲突命名（`makara-export.json`，已存在 -2/-3 递增绝不覆盖）；顺带修正 D38 漏网的品牌残留——不冲突命名的默认名 `myapikey-export.json` → `makara-export.json`；带路径行为不变 | 高频备份动作不应强迫用户想文件名；目录分支语义天然覆盖无参场景，零新逻辑 |
| D46 | **import 幂等零变化不落盘不推送**（2026-11-11 用户指出：导入刚导出的文件 0 added 0 updated 仍走全套保存+推送）：逐条合并后 added+updated 均为 0 → 早退，只输出 `Import done: 0 added, 0 updated.`——数据未变时重加密落盘会产生不同密文、推送会产生远端空提交，均为无意义噪音；有实际变化（added/updated 任一非零）时行为不变 | 空提交污染 GitHub 历史且让用户误以为有东西变了；「零变化 = 零动作」是幂等操作的应有语义 |
| D47 | **GUI 导入/导出（备份与迁移）**（2026-11-11 用户要求，设置页新增「备份与迁移」区块）：导出 = 两道确认（明文泄露警示，对齐 CLI 双确认）→ 会话存活记录序列化裸数组（与 CLI 同格式可互导）→ 写 Downloads/`makara-export-<时间戳>.json`（固定目录零依赖——官方 dialog 插件属新依赖被排除；时间戳命名防覆盖）+ unix 0600 → toast 呈现完整路径；导入 = WebView 原生文件选择器（`<input type=file>`，零依赖）→ 确认 → 后端按 id LWW 合并。合并逻辑上收 core `merge::import_records`（CLI import 同步 refactor 共用——严格大于覆盖/相等跳过/新 id 时间戳重置为落地时刻），幂等零变化不落盘不推送（D46 语义），GUI 有变化走 delete_record 同编排（实例锁 + sync::save + 认证失败静默续期重试）并回写会话。Mock 浏览器验证：区块渲染/导出确认框/导出 toast 全通过；导入的文件选择器 IAB 不支持自动化，留用户实测 | 备份迁移是 CLI 已有的核心能力，GUI 缺位；合并逻辑上收 core 消除双实现漂移；Downloads 固定目录 + toast 路径在不引插件的前提下给出完整闭环 |
| D48 | **passwd 改密保存走两阶段展示**（2026-11-11 用户指出改密后看不到同步过程）：CLI `passwd` 的保存从静默 `sync::save` 改走 `save_and_report` 共享编排（Saved locally → Syncing/Synced to GitHub，或单机提示）；推送失败语义同步增强——不再硬报错，本地新密文已落盘、pending 补推（与 add/edit/delete 一致）。GUI 改密抽屉本有按钮 busy 与 toast 反馈，无涉 | 改密是关键操作，静默完成让用户无从确认远端是否已更新；共享编排消除 passwd 与其他写命令的输出差异 |
| D49 | **GUI 导出接官方 dialog 插件（用户自选路径）**（2026-11-11 用户要求「导出要支持用户自己选择文件」，AskUserQuestion 批准官方插件方案——零依赖替代 [PowerShell SaveFileDialog spawn] 有 1~2s 启动延迟且非标准仅 Windows）：Rust 加 `tauri-plugin-dialog`（workspace deps，与既有插件同法）+ builder 注册 + capabilities `dialog:default`；前端走 `window.__TAURI__.dialog.save` 全局命名空间——**零 npm 包**（沿用 withGlobalTauri 零运行时依赖架构，bridge.ts 封装 saveDialog helper，mock/浏览器模式返回 defaultPath 让导出流程可继续验证）；流程改为 双重确认 → 另存为对话框（默认名 makara-export-<时间戳>.json，取消=放弃）→ 后端 `export_records(sh, path)` 写入选定路径；后端 Downloads 固定目录方案（D47 的临时形态）删除 | 「用户自己选文件」在 WebView 内无零依赖实现（file input 只读不写）；官方插件是标准做法且 macOS 适配（D26）直接复用；用户经 AskUserQuestion 明确批准依赖 |
| D50 | **错误与警告走 stderr**（2026-11-11 用户拍板采纳 clig.dev 流分离）：数据与结果走 stdout，错误（主出口 `eprintln!` + exit 1，此前已实现）与警告（`prompt::warn` 改 `eprintln!`，check 缓存失败/剪贴板失败两个手写点归一 `warn()`）走 stderr——管道重定向时日志不被警告污染；AGENTS 输出排版规则同步固化 | clig.dev 流分离条目；终端合流显示下用户无感知差异，仅脚本重定向场景受益 |
| D51 | **GUI 文案规范采纳 Apple HIG Writing/Labels**（2026-11-11 用户拍板，首次引入外部文案规范）：英文词条一律 sentence case（禁 ALL CAPS 强调，专有名词除外）、按钮与动作标签动词开头 1–3 词、片段无句末标点、避免行话（tombstone/purge 等内部术语不进用户界面）、术语全产品一致（passphrase/record/bind，D32 语义的延伸执行）；中文侧不受英文大小写规则约束，仍按「双语各自成文」。首轮全量审查（约 200 词条）已修 4 处：exportConfirm PLAINTEXT→plaintext、master passphrase×3 统一为 passphrase、repoUnbound 同义重复删除 | Apple HIG 是桌面应用文案的成熟业界标准，与「简约六则」高度互补（错误=现象+出路、术语一致逐字对应）；sentence case 与术语一致直接消除用户困惑源（「Client secret 是什么」类困惑的预防性治理）；规则已固化进 AGENTS.md 代码风格区 |
| D52 | **中低风险全修**（2026-11-11 用户令「中低风险全修」，覆盖审查报告 6 项）：①`makara update` 非交互（管道/脚本）不再自动替换二进制——降级打印直链（交互终端仍直接更新不询问，拍板语义不变）；②AGENTS 发布流程步骤 3 固化「Release notes 必须提及用户可见行为变化」（stderr 分流等）；③⑤导入恢复快照——core `import_records` 改返回 `ImportReport { added, updated, overwritten }`（overwritten=被覆盖的本地旧版本）+ HashMap 索引替代 O(n²) find；CLI 覆盖发生时写当前目录 `makara-import-backup-<ts>.json`（0600），GUI 写 Downloads 并经 `ImportedView.backup_path` toast 提示；快照是数据保底而非一键回滚（其 updated_at 早于现数据，再导入会被 LWW 跳过）；④自更新临时目录成败都清理（do_upgrade 拆 inner）；⑥Release 附件配 .sha256（unix sha256sum / Windows Get-FileHash）+ `makara update` 下载后用系统工具（certutil/sha256sum）比对——D40 的 checksum 缺口落地；连带修复 release.yml latest.json 的旧品牌 glob `MyApiKey_\<v\>_*.sig`（找签名必落空，发版必崩的第二个定时炸弹）；sha256 解析按行精确提取（certutil 输出的 SHA256 标题含数字，字符级过滤会混入） | 修复方式尊重既有拍板语义（①不加询问只降级非交互；③不做一键回滚做数据保底快照）；零新依赖（哈希用系统工具，与 tar 同哲学） |

---

## 3. 总体架构

### 3.1 Workspace 结构

```
myapikey/
├── Cargo.toml            # workspace
├── crates/
│   ├── myapikey-core/    # 纯逻辑库：模型、vault、同步（GitHub API）、校验（无 UI 依赖）
│   ├── myapikey-cli/     # CLI（clap + dialoguer；产出 myapikey 与 makara 两个二进制）
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
├── config.toml        # 非敏感配置：远端地址、同步记账三件套、自动推送、自动锁定分钟数等
├── remember.json      # 记住解锁密码副本的到期时间戳（D32）
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

- 阶段 1 成功即数据安全；阶段 2 失败（网络/冲突）降级为警告，`push_pending` 保证 `makara sync` 补推。
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
- 触发时机：保存新记录自动一次 + 手动单条 / 一键检测全部（GUI 顶栏；CLI `makara check` 无参）。

---

## 7. CLI 设计（myapikey-cli）

```
makara init [仓库]               # 交互初始化：无参询问是否同步（默认 Y、仓库名预填）；
                              # 带参跳过询问；已初始化一律拒绝并引导（remote set/sync/unset）
makara add                       # 交互添加：协议五项菜单 → 名称/Base URL/key（隐藏输入，原地重问）
                              # → 模型（逗号分隔可跳过）→ 备注；保存两阶段反馈 + 自动校验一次
makara update                    # 联网检查新 Release，提示 cargo 升级命令
makara search <query>            # 模糊搜索（名称 / URL / 备注 / 协议）
makara list                      # 列表（key 掩码；含 Models 列与校验状态）
makara show <名|#序号|id>        # 单条详情（--copy 复制 key 到剪贴板）
makara edit <名|#序号|id>        # 逐项修改（所见即所得：回车保留，清空后回车 = 清空）
makara delete <名|#序号|id>      # 删除（墓碑）
makara check [目标]              # 校验（无参 = 全部并发，8 路上限）
makara sync                      # pull + merge + push（409 自动重拉重合并）
makara status                    # 绑定/待推/上次同步/记录数
makara remote set|show|unset     # 改绑（自动管令牌）/ 查看 / 解绑并清授权
makara passwd                    # 改主密码（验证旧密码 → 全量重加密 → 上传）
makara lock                       # 立即锁定：清记住的解锁密码副本
makara export <file>             # 明文导出（双重确认 + 0600）
makara import <file>             # 导入（按 updated_at 新者胜）
```

- 二进制同时安装 `myapikey`（长名）与 `makara`（短名），行为完全一致。
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
- **设置页**：远端仓库（绑定态只读展示完整 GitHub URL + 「解绑」；未绑定或失效折叠态显示「未绑定」+ 「绑定仓库」按钮，D36）、自动锁定档位（D32）、修改后自动推送、日志级别、主题与语言（本机偏好，即时生效）。锁定/推送变更即写（D33）。
- **应用内更新（D16）**：设置页检查更新，非破坏性一键升级，破坏性引导手动。
- **自动锁（硬锁）**：空闲 N 分钟（默认 10）销毁解密状态并清除记住的密码；初始化中间态（授权/绑定意图）一并作废。
- **提示词简约**：全部用户可见文案遵循简约六则（D30），GUI 中英双语各自成文。
- 前端只做展示与输入，**一切读写通过 Tauri command 走 core**；前端代码里不出现任何密钥处理逻辑。

---

## 9. 安全清单

| 项 | 措施 |
|---|---|
| 仓库内容 | 永远只有密文（vault.mkv） |
| 解锁密码 | 明文不写入任何文件；"记住"仅存于 OS 凭据库（§9.1），`makara lock` / GUI 锁定随时清除；内存中 zeroize |
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
- Release 产物：三平台 CLI 单二进制（各含 `myapikey` + `makara`）；**GUI 安装包仅 Windows**（nsis）+ `.sig` 签名 + `latest.json`。
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
