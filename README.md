<div align="center">

<img src="assets/logo.png" width="140" alt="Makara logo" />

# Makara

**跨平台的 AI API 凭证记录与同步工具**

`base_url` + `api_key` · 加密存储 · GitHub 同步 · 多设备

![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-informational)
![Status](https://img.shields.io/badge/status-v1.2.1-blue)
![Rust](https://img.shields.io/badge/rust-stable-dea584?logo=rust)
![GUI](https://img.shields.io/badge/GUI-Tauri%202-blue)
![License](https://img.shields.io/badge/license-PolyForm--NC%201.0.0-blue)

[设计文档](docs/design.md) · [CLI 使用指南](docs/cli-guide.md) · [路线图](#️-路线图) · [开发](#️-开发)

</div>

---

> [!IMPORTANT]
> 当前状态：**v1.2.1 已发布**（M0 – M5 完成）。CLI 从源码安装：`cargo install --git https://github.com/Haaaatcher/Makara`；窗口应用（Tauri 2，Windows 与 macOS Apple Silicon）安装包与各平台 CLI 见 [Release](../../releases)。

## ✨ 特性

- 🔐 **加密存储** —— 解锁密码经 Argon2id 派生密钥，AES-256-GCM 加密单文件 vault，仓库中永远只有密文
- 🔄 **GitHub 同步** —— 你自己的 GitHub 私有仓库作后端（Contents API 直连，零外部依赖：不需要安装 git），按 `updated_at` 合并冲突，删除墓碑防止记录复活；不配远端也能纯单机使用
- 🔑 **免输密码** —— 解锁密码输入一次即记住（有效期 = 自动锁定时长，默认 10 分钟，持续使用自动续期），存于系统凭据库（Credential Manager / Keychain / Secret Service），到期或失效后输一次即续期，`makara lock` 随时锁定
- 🐙 **GitHub 一键初始化** —— `makara init` 交互中选同步即可：浏览器授权一次，自动创建/识别私有同步仓库；选否则纯单机使用
- 🩺 **key 校验** —— 按协议调 `GET /models` 探测 key 状态：有效 / 无效 / 无法验证，失败不阻塞保存
- 🏷️ **协议即标签** —— OpenAI（Responses）/ OpenAI Chat / Anthropic（Messages）/ Anthropic Complete / 自定义，由用户手填，兼容任意三方平台与网关，不做自动推断
- 🖥️ **双形态** —— CLI（`myapikey` / `makara`，三平台）+ 窗口应用（Tauri 2，Windows 与 macOS）：宽屏多列卡片列表 / 编辑草稿防丢 / 全字段行内校验 / 设置 / 同步，空闲自动锁，复制到剪贴板的 key 30 秒后自动清空，托盘常驻 + 单实例
- 🔎 **模糊搜索** —— 两端同源评分（fzf 风格）：子串精确优先、URL 按斜线分词、备注可搜，协议类型也可搜（OpenAI / Anthropic / 形态词）；CLI `makara search`，GUI 逐键即时过滤
- ⬆️ **应用内更新** —— GUI 设置页检查更新，下载 + Ed25519 验签 + 静默安装一条龙；CLI `makara update` 零依赖自更新（下载 Release 压缩包替换自身，无需 cargo）；破坏性版本自动拦截引导手动升级

## 🚀 使用（CLI 已可用）

```bash
makara init                # 交互初始化：询问是否绑定 GitHub 私有同步仓库（makara init <owner/repo> 跳过询问）
makara add                 # 记录一条 base_url + api_key
makara list                # 查看列表（key 默认掩码显示）
makara sync                # 多设备同步（pull + 合并 + push）
makara check               # 校验 key 是否有效
makara search openai       # 模糊搜索（名称 / Base URL / 备注 / 协议）
```

<details>
<summary>📖 全部命令</summary>

```text
init     Initialize: authorize GitHub if needed, create/reuse the private repo, download or upload the vault
add      Add a record interactively. Omitted fields are prompted
update   Check for a newer release and print the upgrade command
list     List all records. Keys are masked
search   Search records (fuzzy match on name, URL, notes)
show     Show one record. Key is always masked; use --copy to put it on the clipboard
edit     Edit a record field by field. Enter keeps the current value
delete   Delete a record. Keeps a tombstone, purged after the grace period
check    Check key validity via GET /models. Checks all records if no target given
sync     Sync: pull + merge + push
status   Show repository and sync status
remote   Manage the remote repository
passwd   Change the passphrase
lock     Lock now: forget the remembered passphrase on this machine
export   Export all records to a JSON file in PLAINTEXT. Asks twice before writing
import   Import records from a JSON file
```

中文速览：`makara init [owner/repo]` 初始化（交互选择是否同步 GitHub，默认仓库名 makara-vault；指定地址则跳过询问） · `makara add` 第一步上下键选协议，选 Custom 才填自定义标签 · `makara key <名|#序号|id>` 复制完整 key · `makara show <名|#序号|id> --copy` 查看详情并复制 · `makara delete` 墓碑删除 · `makara lock` 锁定 · `makara remote set/unset` 绑定或解绑。

</details>

## ❓ 常见问题

<details>
<summary><b>macOS：首次打开弹出钥匙串授权？</b></summary>

<br>

主密码记住副本存在 macOS 钥匙串（条目名 `makara`，D57 起的正式条目——创建它的第一个应用自动获得授权）；GitHub 令牌存本地凭据文件（`github-credentials.json`，0600 权限，不弹窗）。每个新的应用二进制访问钥匙串条目都需要单独授权——应用会在解锁页提示这一点，请点「**始终允许**」（仅 1 个弹窗）。

升级自旧版本（条目名 `myapikey`）：首次读取旧副本/旧令牌时自动迁入新条目，可能多弹一次授权窗，迁移完成后不再弹。未购买 Apple 开发者签名，应用更新后新版本可能需要重新授权一次，属已知限制；旧 `myapikey` 条目迁移后不再被读取，可在「钥匙串访问」中手动删除。

</details>

<details>
<summary><b>换设备绑定后数据对不上 / 仓库里有旧数据？</b></summary>

<br>

默认仓库名历史上从 `myapikey-vault` 改为 `makara-vault`（2026-11-10，D38）：**已绑定的设备不受影响、不做迁移**，因此新老设备可能绑了不同的仓库。修正一条命令（数据按 `updated_at` 合并，不会丢）：

```bash
makara remote set <owner/正确的仓库名>
makara sync
```

误建的多余仓库可在 GitHub 上删除。每次同步都在仓库产生提交历史，极端情况下可从历史版本找回数据。

</details>

## 🏗️ 架构

Rust workspace，核心逻辑与界面严格分离，CLI 与 GUI 共用同一个 core：

```
crates/
├── myapikey-core   # 纯逻辑库：数据模型、vault 加解密、GitHub API 同步、key 校验
├── myapikey-cli    # 终端前端（clap + dialoguer 交互组件）
└── myapikey-gui    # 窗口前端（Tauri 2）
```

安全模型：仓库泄露只拿到密文 · 解锁密码明文不写入任何文件 · GitHub 令牌只管读写不参与加密——三者互为独立防线。

## 🛠️ 开发

环境要求：Rust stable（Windows 用 MSVC 工具链）、git；GUI 另需 Node.js 与 WebView2。

```bash
cargo build         # 构建全部 crate
cargo test          # 运行测试
cargo clippy        # lint
cargo fmt           # 格式化
```

## 🗺️ 路线图

- [x] M0 workspace 骨架 + vault 加解密核心（含测试）
- [x] M1 CLI 全功能 + git 同步 —— 日常可用
- [x] 追加：`makara init` 一键初始化（Device Flow + 自动建仓库）
- [x] M2 Windows GUI（Tauri 2：解锁 / 列表 / 编辑 / 搜索 / 设置 / 同步状态、空闲自动锁、剪贴板 30s 清除）
- [x] M3 打磨与发布（导入导出、自动锁、剪贴板清除、三平台 CI 产物 + Windows NSIS 安装包、Linux 无头安装 `--no-default-features`）
- [x] 追加：体验打磨（应用内更新 D16、两端模糊搜索、CLI dialoguer 交互菜单、三批代码审查修复，2026-10-26）
- [x] M4 零外部依赖（D25，2026-10-29）：移除系统 git，同步改 GitHub API 直连（嵌入式 git 因 gitoxide push 未实现不可行）；应用不再要求安装 git
- [x] M5 macOS GUI 适配（D26/D54，v1.2.1 发布，2026-11-13）：窗口应用发布 macOS 版，仅支持 Apple Silicon（M 系列，aarch64，dmg 安装 + 应用内更新）；同批凭据治理——凭据库条目改名 makara 并自动迁移、macOS 令牌改本地凭据文件（钥匙串授权弹窗 4 → 1）、绑定后立即对账与三层同步修复（D55–D57）

决策记录与完整设计见 [docs/design.md](docs/design.md)。

## 📄 许可证

[PolyForm Noncommercial 1.0.0](LICENSE)——源码公开，允许以**非商业目的**使用、修改和分发；商业用途需另行获得授权（双许可模式，欢迎联系）。严格意义上属于 source-available 而非 OSI 开源。
