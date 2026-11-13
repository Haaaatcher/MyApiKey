# AGENTS.md

给在此仓库工作的 AI 编程代理的规则。**`docs/design.md` 是唯一事实源**：架构或行为变更先更新其中的决策记录（ADR 表），再写代码。

## 命令

```bash
cargo build                    # 构建全部 crate
cargo test                     # 运行测试（提交前必须通过）
cargo clippy -- -D warnings    # lint，要求零警告
cargo fmt                      # 格式化（提交前运行）
```

环境：开发机 macOS（Apple Silicon，`aarch64-apple-darwin`），仓库驻内置盘 APFS（2026-11-12 自 NTFS 外置盘迁入——NTFS 经第三方驱动挂载对 cargo 重活确定性故障：无硬链接、产物写入 ENOENT，**仓库勿放回外置盘**；不得已时设 `CARGO_TARGET_DIR` 指向内置盘）。crates.io 依赖走本地代理 `localhost:7897`（旧 Windows 环境的 rsproxy 镜像不适用，勿搬）。Windows 真机验证与签名构建走 `ssh makara-win`（密钥免密已配；靶机默认 shell 为 PowerShell 5.1，命令用 `;` 不用 `&&`）；发布构建由 CI 完成，本地 Windows 步骤保留于下方开发循环。

## 项目速览

跨平台的 AI API 凭证（`base_url` + `api_key`）记录与同步工具，加密后经用户自己的 GitHub 私有仓库同步多设备（Contents API 直连，**零 git 依赖**，D25）。

- 技术栈：Rust stable、clap（CLI）、dialoguer（CLI 交互组件：菜单/确认/隐藏输入）、Tauri 2（仅 GUI crate）
- 结构：`crates/myapikey-core`（纯逻辑库，无 UI 依赖）· `myapikey-cli`（终端前端）· `myapikey-gui`（Tauri，仅发布 Windows）
- 平台策略：GUI 已发布 Windows；下一阶段适配 macOS GUI（仅 Apple Silicon / M 系列，D26）；Linux 仅 CLI

## 代码风格

- Rust 代码遵循官方最佳实践：rustfmt 默认配置零 diff、clippy 零警告、遵循 std 与 Rust API Guidelines 惯例
- **审查基线**（2026-11-05 采纳）：workspace 级 `[workspace.lints]`（根 Cargo.toml）+ 审查规范
  [BeautifulRust](https://github.com/morganlinton/BeautifulRust) 本地副本见 `docs/review/beautiful-rust/`；
  审查产出按 must fix（严重）/ should fix（建议）/ consider（可选）三级，前两级直接修，第三级交决策
- **语言分层**（2026-10-24 决议；2026-10-24 增补 D14）：
  - 面向**用户**的文本：**CLI** 一律**英文**；**GUI** 中/英双语（前端字典 i18n，默认跟随系统语言，设置页可切换——D14）
  - 面向**开发者**的内容（代码注释、docs/ 文档、commit message）保持**中文**；
  - clap 帮助文本用显式 `help = "..."` / `about = "..."` 属性给英文，字段/变体上的文档注释仍写中文（两者并存时 clap 以属性为准）
- **输出排版**：用户可见的提示必须结构化——一行一个要点、短句、`✔`/`⚠` 前缀标状态（成功行一律走 `prompt::ok`：交互态绿色 `✔` 与 dialoguer 同符，非交互态退化为无色 `✓`，禁止手写对勾字符）、辅助说明缩进两空格独立成行、逻辑分组之间空行；禁止长段文字挤在一起让用户找不到重点。**流分离（2026-11-11，clig.dev）**：数据与结果走 stdout，错误与警告走 stderr（错误出口 `eprintln!`、警告一律 `prompt::warn`）——管道重定向时日志不被警告污染
- **提示词禁用括号式补充说明**（2026-10-25 决议，CLI 与 GUI 一致执行，**软规则**）：
  - **禁止**：用圆括号或破折号**夹带补充说明**——"它是什么/为什么"的解释要并入短句，或缩进两空格独立成提示行（GUI 用 hover 提示）
  - **允许且推荐**：用括号**呈现选项与伴随信息**——输入提示的 Enter 语义 `(Enter keeps current)`、示例 `(e.g. OpenAI primary)`、伴随状态 `(HTTP 401)`、默认值/约束标注 `(private)`、`(optional)` 等，这类括号让主信息与伴随信息层次分明，起更好的提示作用
  - 界限：括号内是**同一信息的组成部分**（选项、状态、示例、约束）→ 可用；括号内是**岔开的解释**（原因、后果、指引）→ 改短句或独立行
- **提示词简约六则**（2026-10-31 定调"简约是设计风格"，CLI 与 GUI 一致执行）：一句一事（现象一句话，出路/细节缩进下一行）；删可推断的（"successfully""可随时重新绑定"这类用户自己能推断的话不写）；动词开头（指令句动词打头，状态句"名词+状态"）；错误 = 现象 + 出路（第一句短现象，紧跟缩进的出路，不铺原因链）；术语统一（bind/unbind、vault、record 全产品一词）；双语各自成文（中文按中文习惯写，不逐字翻译英文）
- **GUI 文案遵循 Apple HIG Writing/Labels**（2026-11-11 采纳）：英文一律 **sentence case**（禁 ALL CAPS 强调，专有名词除外）；按钮与动作标签**动词开头**、1–3 词、无句末标点（完整句说明可用句号）；**toast 提示一律无句末标点**（2026-11-13 拍板：中英文皆然，短句气泡形态加句号视觉突兀；正文段落不受此限）；**避免行话与实现细节**（tombstone/purge 等内部术语不进用户界面）；术语全产品一致（passphrase、record、bind——D32 语义的延伸执行）。中文侧不受英文大小写规则约束，仍按「双语各自成文」。出处：https://developer.apple.com/design/human-interface-guidelines/writing
- **注释尽可能详细**（我的明确要求，优先于"少注释"的社区习惯）：
  - 所有公开项（pub 函数/类型/常量）必须有 `///` 文档注释，写清用途、参数、返回值，并按惯例补 `# Errors`、`# Panics`、`# Safety` 章节
  - 每个模块/文件以 `//!` 开头说明该模块的职责与边界
  - 加密、KDF 参数、同步合并、墓碑这类关键逻辑，行内注释解释"为什么这么做"与安全约束，不止是"做了什么"
  - 详细 ≠ 流水账：仍然 NEVER 写对话史、修改说明或给评审看的解释（见下方规则）

## 规则

- ALWAYS 全程中文交流，结论先行
- ALWAYS 严守请求范围：只做被要求的事，不做"顺便"的调研、检查或重构
- ALWAYS 动手前核对任务的理解与边界，有偏差先问，不猜
- ALWAYS 产品设计/用户可见行为变更必须三端同步：**文档**（design.md 决策记录 + 用户文档）、**CLI**、**GUI** 同批完成，交付前自查清单——除非我显式要求只改某一部分（2026-11-10 决议）
- ALWAYS 产品原则（如"用户不感知 X"）拍板后，**全量清点所有用户可见面**——CLI 命令与文案、GUI 界面与文案、文档、测试——一次清干净；只清当下看到的区域等于原则没有落地（2026-11-11 教训：去掉令牌暴露时漏掉了 CLI 的 secret 命令，用户随后点名再犯）
- ALWAYS 给新的 core 行为配测试（vault 加解密、同步合并这类逻辑必须有 roundtrip/边界用例）
- ALWAYS 保证日志、错误信息、panic 消息里不出现 api_key 与主密码（含部分明文）
- NEVER 做协议自动推断、key 前缀识别、base_url 归一化——协议只是用户手填的标签
- NEVER 在任何终端输出中显示明文 key（包括 `show` 命令）——完整 key 只进剪贴板（`--copy`）或双重确认的导出文件（2026-10-24 我明确要求）
- NEVER 把明文凭证写入任何会被同步或提交的文件；明文导出必须走显式确认流程
- NEVER 让 `myapikey-core` 依赖 CLI / GUI 或任何 UI 相关 crate
- NEVER 绕过删除墓碑机制物理删除记录
- NEVER 在代码注释里写对话史或"本次修改说明"——为没见过对话的读者写
- PREFER core 中用 `thiserror` 传递错误；AVOID `unwrap()` / `panic!`（测试代码除外）
- PREFER 改动前先跑 `cargo test` 确认基线，改后跑受影响的测试而非全量

## 开发与发布流程

按既有工作方式固化为流水线（2026-10-26 整理）。

### 每次改动的开发循环

1. **改前**：核对任务理解与边界，有偏差先问；动 core 前跑 `cargo test` 确认基线
2. **改中**：core 新行为必须配测试（roundtrip/边界用例）；Rust 改完只跑受影响 crate 的测试，不做无谓全量
3. **本地验证**：
   - `cargo test`（受影响 crate）+ `cargo clippy --workspace -- -D warnings` 零警告 + `cargo fmt`
   - 前端改动先在浏览器 mock 验证（vite dev `localhost:5300`——5173/5188 在 Windows 落保留端口段不可用，macOS 无此限制，沿用 5300 保持两端一致 + 浏览器自动化），通过后再 `npm run build`
   - **Windows 靶机验证（2026-11-13 起，双端同步工作流）**：改动涉及 cfg 分叉、Windows 专属路径或发版前，跑 `scripts/sync-windows.sh` 同步源码到靶机（一条命令；保留靶机 `target/` 编译缓存，增量编译），再统一执行：
     `ssh makara-win "cd C:\Users\luohang\Projects\MyApiKey; cargo test --workspace -- --skip lock_clears_remembered --skip set_settings_rewrites; cargo clippy --workspace -- -D warnings"`
     ——凭据库 2 测试（lock_clears / set_settings_rewrites）依赖交互式登录会话，SSH 非交互会话必失败（`ERROR_NO_SUCH_LOGON_SESSION`），属环境限制非缺陷，桌面会话或 CI 可跑；Mac/Windows 的 clippy lint 集有版本差（靶机 1.99.0 更严），**cfg 分叉改动双端都要跑**；删除/重命名源文件需手动清靶机残留；改 `ui/package.json` / `package-lock.json` 后靶机重跑 `npm ci`；Windows 签名构建用靶机 `C:\Users\luohang\build-signed.ps1`（签名口令经 stdin 管道传入，README 口令行解析在 PowerShell GBK 环境不可靠）
4. **交付（涉及 GUI）**：
   - 先 `taskkill //IM myapikey-gui.exe //F`——旧进程占用 exe，不杀会导致构建 os error 5
   - 在 `crates/myapikey-gui` 下执行 `node ui/node_modules/@tauri-apps/cli/tauri.js build`（crate 根有 `tauri.conf.json`，npm scripts 在 ui/ 下）
   - **交付我验证前，再杀一次旧进程**（同上命令）——应用单实例 + 托盘常驻：双击 exe / 点图标只是唤起旧进程窗口（加载的是旧前端），新版改动会「看起来没生效」；杀干净后我打开的才是新构建
   - 产物以链接交付给我：`target/release/myapikey-gui.exe`（免安装）与 `target/release/bundle/nsis/*-setup.exe`（安装包）；提醒我**托盘退出后重开**——点 × 只是隐藏到托盘，直接双击 exe 会由单实例唤起旧进程，跑的还是旧代码
   - **macOS（M5 起）**：
     - 在 `crates/myapikey-gui` 下执行 `ui/node_modules/.bin/tauri build`；本地无 `TAURI_SIGNING_PRIVATE_KEY` 时在 updater 签名步报错退出（dmg / .app / 机制件已产出，仅缺 .sig）——快速验证用 `--bundles dmg` 跳过 updater 链；正式签名构建见发布流程
     - **签名密钥已就位本机 `~/.tauri/`**（2026-11-12 scp 自 Windows 靶机，三件套：`.pem` 私钥 + `.pub` + README）：构建前注入 `TAURI_SIGNING_PRIVATE_KEY=$HOME/.tauri/myapikey-updater.pem` 与 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（口令在 README 的 `口令:` 行，提取后进环境变量、勿回显），本地即可出完整签名构建（2026-11-12 端到端验证：.sig 经 minisign-verify 权威验证通过）
     - dmg 打包的窗口装饰走 osascript 控制 Finder——首次构建会弹 macOS 自动化授权，点「好」放行（授权持久，仅首次）
     - 产物（仓库 `target/` 下）：`release/bundle/dmg/Makara_<版本>_aarch64.dmg`（人装）、`release/bundle/macos/Makara.app`（免安装）、`release/bundle/macos/Makara.app.tar.gz`（updater 机制件，签名后进 latest.json 的 `darwin-aarch64` 键）
     - 交付提醒：无开发者签名/公证，首次打开**右键 → 打开**或 `xattr -d com.apple.quarantine <app>`；托盘常驻语义同 Windows，macOS 上 Cmd+Q / 菜单退出才结束进程
5. **交付（涉及 CLI）**：
   - `cargo build --release -p myapikey-cli` 后 `cargo install --path crates/myapikey-cli --force`，即时更新我的 `makara`
   - 交互类改动用管道喂输入冒烟（顺带覆盖非 TTY 降级路径）
6. **测试数据纪律**：一律用 `MYAPIKEY_HOME` 指向临时目录隔离，**绝不碰我的真实数据目录**（Windows `%APPDATA%\myapikey`、macOS `~/Library/Application Support/myapikey`）；需要清我的本地状态时先整目录备份（`myapikey.bak-<日期>`）再动，删完报告清单
7. **文档同步**：行为/架构变更记入 `docs/design.md`（ADR 表或对应章节），规则变更同步本文件；全程不做 `git commit` / `git push`（等我批准）

### 发布流程（tag / GitHub Release）

发布属红线动作，除我明确批准外不得进行；且以里程碑功能完整为前提。

0. **内嵌 secret 注入（2026-11-06 起）**：GitHub Secrets 配置
   `MYAPIKEY_CLIENT_SECRET`（MakaraApp 的 client secret）；CI 自动写入
   `client-secret.txt` 内嵌进 CLI/GUI。本地发布构建则把该文件放仓库根
   （已 gitignore）。刷新端点实测必须携带 secret（文档的 device flow
   豁免与行为不符），未注入 = 用户自动续期失败
1. **版本号**：workspace `Cargo.toml` 与 `crates/myapikey-gui/tauri.conf.json` 的 `version` 同步 bump
2. **签名构建（GUI）**：注入 `TAURI_SIGNING_PRIVATE_KEY`（私钥路径）与 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（取值见 `%USERPROFILE%\.tauri\myapikey-updater-README.txt`）再构建，产出安装包 + `.sig` 签名
3. **更新清单**：`python scripts/make-latest-json.py <版本> [--breaking]` 生成 `latest.json`；破坏性版本**必须** `--breaking`——应用内守卫靠 notes 首行 `[breaking]` 标记与主版本提升双判定；Release notes **必须提及用户可见行为变化**（如 D50 的 stdout/stderr 分流——既有用户脚本的重定向假设会变）
4. **CI 前置（一次性）**：仓库 Settings → Secrets 配置 `TAURI_SIGNING_PRIVATE_KEY`（私钥文件内容）与 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
5. **打 tag**：`v<版本>` push 后 GitHub Actions 自动构建四平台 CLI 与 Windows GUI（签名）并挂 Release 附件
6. **发布核对**：附件齐全（GUI exe + `.sig` + `latest.json` + CLI 四平台压缩包）；破坏性版本确认 notes 首行标记；`makara update` 与 GUI"检查更新"都能发现新版本
7. **发布后**：在干净环境装新版走一遍冒烟（init → add → sync 链路）

## 需要先征得同意（⚠️）

- **git 提交与推送**：任何 `git commit` / `git push` 前必须获得我的明确允许。**例外（2026-11-13 授权）**：**仅文件改动**——文档（README/docs）、脚本、CI 配置、注释与非行为性文件——可自行提交并推送；**涉及代码行为的改动**（crates 源码逻辑、前端逻辑）仍须先经批准，发布动作（打 tag / Release）同前不变
- **发布**：打版本标签 / 创建 GitHub Release 属于发布，除明确允许外，**必须等里程碑功能完整后才能进行**（首个发布 = M3 完成后，2026-10-24 决议）
- 修改 `docs/design.md` 中已定的决策（ADR 表）
- 引入新的第三方依赖
- 更改 vault 加密格式或版本号

## 文档约定

- 面向用户的文档用 GitHub 风格：居中徽章头、alerts、折叠块、复选框路线图
- 不虚构未实现的功能，状态如实标注（如"M1 后可用"）
- 文档以第一人称书写（2026-10-26 决议）：指代我本人用「我」，不用「主人」等第三人称
