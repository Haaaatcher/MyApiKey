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

- 技术栈：Rust stable、clap（CLI）、rpassword、Tauri 2（仅 GUI crate）
- 结构：`crates/myapikey-core`（纯逻辑库，无 UI 依赖）· `myapikey-cli`（终端前端）· `myapikey-gui`（Tauri，仅发布 Windows）
- 平台策略：GUI 首发 Windows；Linux / macOS 仅 CLI

## 规则

- ALWAYS 全程中文交流，结论先行
- ALWAYS 严守请求范围：只做被要求的事，不做"顺便"的调研、检查或重构
- ALWAYS 动手前核对任务的理解与边界，有偏差先问，不猜
- ALWAYS 给新的 core 行为配测试（vault 加解密、同步合并这类逻辑必须有 roundtrip/边界用例）
- ALWAYS 保证日志、错误信息、panic 消息里不出现 api_key 与主密码（含部分明文）
- NEVER 做协议自动推断、key 前缀识别、base_url 归一化——协议只是用户手填的标签
- NEVER 把明文凭证写入任何会被同步或提交的文件；明文导出必须走显式确认流程
- NEVER 让 `myapikey-core` 依赖 CLI / GUI 或任何 UI 相关 crate
- NEVER 绕过删除墓碑机制物理删除记录
- NEVER 在代码注释里写对话史或"本次修改说明"——为没见过对话的读者写
- PREFER core 中用 `thiserror` 传递错误；AVOID `unwrap()` / `panic!`（测试代码除外）
- PREFER 改动前先跑 `cargo test` 确认基线，改后跑受影响的测试而非全量

## 需要先征得同意（⚠️）

- 修改 `docs/design.md` 中已定的决策（ADR 表）
- 引入新的第三方依赖
- 更改 vault 加密格式或版本号

## 文档约定

- 面向用户的文档用 GitHub 风格：居中徽章头、alerts、折叠块、复选框路线图
- 不虚构未实现的功能，状态如实标注（如"M1 后可用"）
