# Rust 代码审查规范(本地副本)

来源:[BeautifulRust](https://github.com/morganlinton/BeautifulRust)(MIT License,见同目录 LICENSE)。
该规范由五份权威来源蒸馏而成:Google Rust Style Guide、Rust API Guidelines、Effective Rust、
Rust Design Patterns、Microsoft Pragmatic Rust Guidelines;冲突处已由上游"选边"并说明理由。

| 文件 | 内容 |
|---|---|
| `rust-review-SKILL.md` | 审查流程:fmt/clippy/test 先行,产出三级报告(must fix / should fix / consider),每条发现引用规则编号与 file:line |
| `rust-best-practices-SKILL.md` | 最佳实践正文:所有权、类型与 API 设计、错误与 panic、惯用法、async、unsafe/FFI、文档、测试、性能 |
| `lints.toml` | Cargo `[lints]` 配置块(可直接并入 workspace Cargo.toml,尚未采用) |
| `clippy.toml` | 配套 clippy 配置 |

使用方式:审查增量代码时按 `rust-review-SKILL.md` 的流程与分级执行;
分级对应本项目惯例——must fix(严重,直接修)、should fix(建议,直接修)、consider(可选,交决策)。

## 2026-11-05 审查记录

- 范围:未提交增量(vs `527b7a3`),Rust 三 crate + GUI 前端 TS
- 第一轮(增量):严重 2 + 建议 3 已修;可选 A-E 交决策
- 第二轮(用户采纳 A-E 全部):A 安装检测轮询上限 10 分钟;B zxcvbn 动态
  import 拆 chunk(主包 505KB→427KB,告警消除);C 安装页 slug 单一来源
  (core `app_install_url`,前端经 `github_install_url` 命令懒取);
  D 自动锁 tick 副本重写节流(剩余 < 半程才写,`keychain::expires_at`
  + 单测);E `[workspace.lints]` 基线启用,存量落差 ~100 条清零
  (doc lints 为 [lib] 级仅 core 启用;`too_many_lines` 等以带理由
  `#[expect]` 放行)
- 终态:77 测试通过、clippy `--workspace --all-targets -D warnings` 零警告、fmt clean
