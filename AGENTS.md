# AGENTS.md

给在此仓库工作的 AI 编程代理的规则。**`docs/design.md` 是唯一事实源**：架构或行为变更先更新其中的决策记录（ADR 表），再写代码。

## 命令

```bash
cargo build                    # 构建全部 crate
cargo test                     # 运行测试（提交前必须通过）
cargo clippy -- -D warnings    # lint，要求零警告
cargo fmt                      # 格式化（提交前运行）
```

环境：Windows + MSVC 工具链（Git Bash）；crates.io 依赖走 rsproxy.cn 镜像（`~/.cargo/config.toml` 已配置，勿重复配置或移除）。

## 项目速览

跨平台的 AI API 凭证（`base_url` + `api_key`）记录与同步工具，加密后经用户私有 git 仓库同步多设备。

- 技术栈：Rust stable、clap（CLI）、dialoguer（CLI 交互组件：菜单/确认/隐藏输入）、Tauri 2（仅 GUI crate）
- 结构：`crates/myapikey-core`（纯逻辑库，无 UI 依赖）· `myapikey-cli`（终端前端）· `myapikey-gui`（Tauri，仅发布 Windows）
- 平台策略：GUI 首发 Windows；Linux / macOS 仅 CLI

## 代码风格

- Rust 代码遵循官方最佳实践：rustfmt 默认配置零 diff、clippy 零警告、遵循 std 与 Rust API Guidelines 惯例
- **语言分层**（2026-10-24 决议；2026-10-24 增补 D14）：
  - 面向**用户**的文本：**CLI** 一律**英文**；**GUI** 中/英双语（前端字典 i18n，默认跟随系统语言，设置页可切换——D14）
  - 面向**开发者**的内容（代码注释、docs/ 文档、commit message）保持**中文**；
  - clap 帮助文本用显式 `help = "..."` / `about = "..."` 属性给英文，字段/变体上的文档注释仍写中文（两者并存时 clap 以属性为准）
- **输出排版**：用户可见的提示必须结构化——一行一个要点、短句、`✓`/`⚠` 前缀标状态、辅助说明缩进两空格独立成行、逻辑分组之间空行；禁止长段文字挤在一起让用户找不到重点
- **提示词禁用括号式补充说明**（2026-10-25 决议，CLI 与 GUI 一致执行，**软规则**）：
  - **禁止**：用圆括号或破折号**夹带补充说明**——"它是什么/为什么"的解释要并入短句，或缩进两空格独立成提示行（GUI 用 hover 提示）
  - **允许且推荐**：用括号**呈现选项与伴随信息**——输入提示的 Enter 语义 `(Enter keeps current)`、示例 `(e.g. OpenAI primary)`、伴随状态 `(HTTP 401)`、默认值/约束标注 `(private)`、`(optional)` 等，这类括号让主信息与伴随信息层次分明，起更好的提示作用
  - 界限：括号内是**同一信息的组成部分**（选项、状态、示例、约束）→ 可用；括号内是**岔开的解释**（原因、后果、指引）→ 改短句或独立行
- **注释尽可能详细**（主人的明确要求，优先于"少注释"的社区习惯）：
  - 所有公开项（pub 函数/类型/常量）必须有 `///` 文档注释，写清用途、参数、返回值，并按惯例补 `# Errors`、`# Panics`、`# Safety` 章节
  - 每个模块/文件以 `//!` 开头说明该模块的职责与边界
  - 加密、KDF 参数、同步合并、墓碑这类关键逻辑，行内注释解释"为什么这么做"与安全约束，不止是"做了什么"
  - 详细 ≠ 流水账：仍然 NEVER 写对话史、修改说明或给评审看的解释（见下方规则）

## 规则

- ALWAYS 全程中文交流，结论先行
- ALWAYS 严守请求范围：只做被要求的事，不做"顺便"的调研、检查或重构
- ALWAYS 动手前核对任务的理解与边界，有偏差先问，不猜
- ALWAYS 给新的 core 行为配测试（vault 加解密、同步合并这类逻辑必须有 roundtrip/边界用例）
- ALWAYS 保证日志、错误信息、panic 消息里不出现 api_key 与主密码（含部分明文）
- NEVER 做协议自动推断、key 前缀识别、base_url 归一化——协议只是用户手填的标签
- NEVER 在任何终端输出中显示明文 key（包括 `show` 命令）——完整 key 只进剪贴板（`--copy`）或双重确认的导出文件（2026-10-24 主人明确要求）
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
   - 前端改动先在浏览器 mock 验证（vite dev `localhost:5173` + 浏览器自动化），通过后再 `npm run build`
4. **交付（涉及 GUI）**：
   - 先 `taskkill //IM myapikey-gui.exe //F`——旧进程占用 exe，不杀会导致构建 os error 5
   - 在 `crates/myapikey-gui` 下执行 `node ui/node_modules/@tauri-apps/cli/tauri.js build`（crate 根有 `tauri.conf.json`，npm scripts 在 ui/ 下）
   - 产物以链接交付主人：`target/release/myapikey-gui.exe`（免安装）与 `target/release/bundle/nsis/*-setup.exe`（安装包）；提醒主人**托盘退出后重开**——点 × 只是隐藏到托盘，直接双击 exe 会由单实例唤起旧进程，跑的还是旧代码
5. **交付（涉及 CLI）**：
   - `cargo build --release -p myapikey-cli` 后 `cargo install --path crates/myapikey-cli --force`，即时更新主人的 `mak`
   - 交互类改动用管道喂输入冒烟（顺带覆盖非 TTY 降级路径）
6. **测试数据纪律**：一律用 `MYAPIKEY_HOME` 指向临时目录隔离，**绝不碰主人的真实 `%APPDATA%\myapikey`**；需要清主人本地状态时先整目录备份（`myapikey.bak-<日期>`）再动，删完报告清单
7. **文档同步**：行为/架构变更记入 `docs/design.md`（ADR 表或对应章节），规则变更同步本文件；全程不做 `git commit` / `git push`（等主人批准）

### 发布流程（tag / GitHub Release）

发布属红线动作，除主人明确批准外不得进行；且以里程碑功能完整为前提。

1. **版本号**：workspace `Cargo.toml` 与 `crates/myapikey-gui/tauri.conf.json` 的 `version` 同步 bump
2. **签名构建（GUI）**：注入 `TAURI_SIGNING_PRIVATE_KEY`（私钥路径）与 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（取值见 `%USERPROFILE%\.tauri\myapikey-updater-README.txt`）再构建，产出安装包 + `.sig` 签名
3. **更新清单**：`python scripts/make-latest-json.py <版本> [--breaking]` 生成 `latest.json`；破坏性版本**必须** `--breaking`——应用内守卫靠 notes 首行 `[breaking]` 标记与主版本提升双判定
4. **CI 前置（一次性）**：仓库 Settings → Secrets 配置 `TAURI_SIGNING_PRIVATE_KEY`（私钥文件内容）与 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
5. **打 tag**：`v<版本>` push 后 GitHub Actions 自动构建四平台 CLI 与 Windows GUI（签名）并挂 Release 附件
6. **发布核对**：附件齐全（GUI exe + `.sig` + `latest.json` + CLI 四平台压缩包）；破坏性版本确认 notes 首行标记；`mak update` 与 GUI"检查更新"都能发现新版本
7. **发布后**：在干净环境装新版走一遍冒烟（init → add → sync 链路）

## 需要先征得同意（⚠️）

- **git 提交与推送**：任何 `git commit` / `git push` 前必须获得仓库主人的明确允许
- **发布**：打版本标签 / 创建 GitHub Release 属于发布，除明确允许外，**必须等里程碑功能完整后才能进行**（首个发布 = M3 完成后，2026-10-24 决议）
- 修改 `docs/design.md` 中已定的决策（ADR 表）
- 引入新的第三方依赖
- 更改 vault 加密格式或版本号

## 文档约定

- 面向用户的文档用 GitHub 风格：居中徽章头、alerts、折叠块、复选框路线图
- 不虚构未实现的功能，状态如实标注（如"M1 后可用"）
