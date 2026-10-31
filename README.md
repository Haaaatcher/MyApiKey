<div align="center">

<img src="assets/logo.png" width="140" alt="MyApiKey logo" />

# MyApiKey

**跨平台的 AI API 凭证记录与同步工具**

`base_url` + `api_key` · 加密存储 · GitHub 同步 · 多设备

![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-informational)
![Status](https://img.shields.io/badge/status-v1.0.0-blue)
![Rust](https://img.shields.io/badge/rust-stable-dea584?logo=rust)
![GUI](https://img.shields.io/badge/GUI-Tauri%202-blue)
![License](https://img.shields.io/badge/license-PolyForm--NC%201.0.0-blue)

[设计文档](docs/design.md) · [CLI 使用指南](docs/cli-guide.md) · [路线图](#️-路线图) · [开发](#️-开发)

</div>

---

> [!IMPORTANT]
> 当前状态：**v1.0.0 已发布**（M0 – M4 完成）。CLI 从源码安装：`cargo install --git https://github.com/Haaaatcher/MyApiKey`；Windows 窗口应用（Tauri 2）安装包与各平台 CLI 见 [Release](../../releases)。

## ✨ 特性

- 🔐 **加密存储** —— 主密码经 Argon2id 派生密钥，AES-256-GCM 加密单文件 vault，仓库中永远只有密文
- 🔄 **GitHub 同步** —— 你自己的 GitHub 私有仓库作后端（Contents API 直连，零外部依赖：不需要安装 git），按 `updated_at` 合并冲突，删除墓碑防止记录复活；不配远端也能纯单机使用
- 🔑 **免输密码** —— 主密码输入一次即记住（默认 7 天有效，可配置），存于系统凭据库（Credential Manager / Keychain / Secret Service），到期或失效后输一次即续期，`mak passwd unset` 随时锁定
- 🐙 **GitHub 一键初始化** —— `mak init` 交互中选同步即可：浏览器授权一次，自动创建/识别私有同步仓库；选否则纯单机使用
- 🩺 **key 校验** —— 按协议调 `GET /models` 探测 key 状态：有效 / 无效 / 无法验证，失败不阻塞保存
- 🏷️ **协议即标签** —— OpenAI（Responses）/ OpenAI Chat / Anthropic（Messages）/ Anthropic Complete / 自定义，由用户手填，兼容任意三方平台与网关，不做自动推断
- 🖥️ **双形态** —— CLI（`myapikey` / `mak`，三平台）+ 窗口应用（Tauri 2，首发 Windows）：宽屏多列卡片列表 / 编辑草稿防丢 / 全字段行内校验 / 设置 / 同步，空闲自动锁，复制到剪贴板的 key 30 秒后自动清空，托盘常驻 + 单实例
- 🔎 **模糊搜索** —— 两端同源评分（fzf 风格）：子串精确优先、URL 按斜线分词、备注可搜，协议类型也可搜（OpenAI / Anthropic / 形态词）；CLI `mak search`，GUI 逐键即时过滤
- ⬆️ **应用内更新** —— GUI 设置页检查更新，下载 + Ed25519 验签 + 静默安装一条龙；破坏性版本自动拦截引导手动升级；CLI `mak update` 检查并给出升级命令

## 🚀 使用（CLI 已可用）

```bash
mak init                # 交互初始化：询问是否绑定 GitHub 私有同步仓库（mak init <owner/repo> 跳过询问）
mak add                 # 记录一条 base_url + api_key
mak list                # 查看列表（key 默认掩码显示）
mak sync                # 多设备同步（pull + 合并 + push）
mak check               # 校验 key 是否有效
mak search openai       # 模糊搜索（名称 / Base URL / 备注 / 协议）
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
passwd   Change the master passphrase, or `unset` to clear the remembered copy on this machine
export   Export all records to a JSON file in PLAINTEXT. Asks twice before writing
import   Import records from a JSON file
```

中文速览：`mak init [owner/repo]` 初始化（交互选择是否同步 GitHub，默认仓库名 myapikey-vault；指定地址则跳过询问） · `mak add` 第一步上下键选协议，选 Custom 才填自定义标签 · `mak show <名|#序号|id> --copy` 复制完整 key · `mak delete` 墓碑删除 · `mak passwd unset` 锁定 · `mak remote set/unset` 绑定或解绑。

</details>

## 🏗️ 架构

Rust workspace，核心逻辑与界面严格分离，CLI 与 GUI 共用同一个 core：

```
crates/
├── myapikey-core   # 纯逻辑库：数据模型、vault 加解密、GitHub API 同步、key 校验
├── myapikey-cli    # 终端前端（clap + dialoguer 交互组件）
└── myapikey-gui    # 窗口前端（Tauri 2）
```

安全模型：仓库泄露只拿到密文 · 主密码不写入任何文件 · GitHub 令牌只管读写不参与加密——三者互为独立防线。

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
- [x] 追加：`mak init` 一键初始化（Device Flow + 自动建仓库）
- [x] M2 Windows GUI（Tauri 2：解锁 / 列表 / 编辑 / 搜索 / 设置 / 同步状态、空闲自动锁、剪贴板 30s 清除）
- [x] M3 打磨与发布（导入导出、自动锁、剪贴板清除、三平台 CI 产物 + Windows NSIS 安装包、Linux 无头安装 `--no-default-features`）
- [x] 追加：体验打磨（应用内更新 D16、两端模糊搜索、CLI dialoguer 交互菜单、三批代码审查修复，2026-10-26）
- [x] M4 零外部依赖（D25，2026-10-29）：移除系统 git，同步改 GitHub API 直连（嵌入式 git 因 gitoxide push 未实现不可行）；应用不再要求安装 git
- [ ] M5 macOS GUI 适配（D26，2026-10-30）：窗口应用发布 macOS 版，仅支持 Apple Silicon（M 系列，aarch64）

决策记录与完整设计见 [docs/design.md](docs/design.md)。

## 📄 许可证

[PolyForm Noncommercial 1.0.0](LICENSE)——源码公开，允许以**非商业目的**使用、修改和分发；商业用途需另行获得授权（双许可模式，欢迎联系）。严格意义上属于 source-available 而非 OSI 开源。
