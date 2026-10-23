<div align="center">

<img src="assets/logo.png" width="140" alt="MyApiKey logo" />

# MyApiKey

**跨平台的 AI API 凭证记录与同步工具**

`base_url` + `api_key` · 加密存储 · git 同步 · 多设备

![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-informational)
![Status](https://img.shields.io/badge/status-alpha%20(M1)-orange)
![Rust](https://img.shields.io/badge/rust-stable-dea584?logo=rust)
![GUI](https://img.shields.io/badge/GUI-Tauri%202-blue)
![License](https://img.shields.io/badge/license-PolyForm--NC%201.0.0-blue)

[设计文档](docs/design.md) · [CLI 使用指南](docs/cli-guide.md) · [路线图](#️-路线图) · [开发](#️-开发)

</div>

---

> [!IMPORTANT]
> 当前状态：**M0 + M1 完成**——CLI（加密记录 / git 多设备同步 / key 校验 / GitHub 一键初始化）已可用，从源码安装：`cargo install --git https://github.com/Haaaatcher/MyApiKey`。GUI（M2）尚未开始。

## ✨ 特性

- 🔐 **加密存储** —— 主密码经 Argon2id 派生密钥，AES-256-GCM 加密单文件 vault，仓库中永远只有密文
- 🔄 **git 同步** —— 任意私有 git 仓库作后端（GitHub / Gitee / 自建均可），按 `updated_at` 合并冲突，删除墓碑防止记录复活；不配远端也能纯单机使用
- 🔑 **免输密码** —— 主密码输入一次即记住（默认 7 天有效，可配置），存于系统凭据库（Credential Manager / Keychain / Secret Service），到期或失效后输一次即续期，`mak forget` 随时锁定
- 🐙 **GitHub 一键初始化** —— `mak init --github`：浏览器授权一次，自动创建/识别私有同步仓库，无需手动建仓库复制地址
- 🩺 **key 校验** —— 按协议调 `GET /models` 探测 key 状态：有效 / 无效 / 无法验证，失败不阻塞保存
- 🏷️ **协议即标签** —— OpenAI / Anthropic / 自定义，由用户手填，兼容任意三方平台与网关，不做自动推断
- 🖥️ **双形态** —— CLI（`myapikey` / `mak`，三平台）+ 窗口应用（Tauri 2，首发 Windows）

## 🚀 使用（CLI 已可用）

```bash
mak init --github       # 一键初始化：浏览器授权 GitHub → 自动创建/识别私有同步仓库
mak add                 # 记录一条 base_url + api_key
mak list                # 查看列表（key 默认掩码显示）
mak sync                # 多设备同步（pull + 合并 + push）
mak check               # 校验 key 是否有效
```

<details>
<summary>📖 全部命令</summary>

```bash
myapikey init [--github [仓库名]]  # 初始化；--github 走浏览器授权自动建仓库（默认名 myapikey-vault）
myapikey init --remote <url>       # 初始化并绑定已有仓库（第二台设备会自动克隆接入）
myapikey add                # 交互式添加记录
myapikey list               # 列表（掩码显示，含校验状态）
myapikey show <id|name>     # 查看单条（key 掩码；--copy 复制完整 key）
myapikey edit <id|name>     # 编辑
myapikey delete <id|name>   # 删除（墓碑，30 天后清理）
myapikey check [id|name]    # 校验（不带参数 = 全部）
myapikey sync               # pull + 合并 + push
myapikey status             # 同步状态 / 未推送变更
myapikey remote set <url>   # 配置 / 更换远端仓库
myapikey passwd             # 修改主密码
myapikey forget [--github]  # 清除记住的密码 / GitHub 授权
myapikey export <file>      # 明文导出（双重确认）
myapikey import <file>      # 导入
```

</details>

## 🏗️ 架构

Rust workspace，核心逻辑与界面严格分离，CLI 与 GUI 共用同一个 core：

```
crates/
├── myapikey-core   # 纯逻辑库：数据模型、vault 加解密、git 同步、key 校验
├── myapikey-cli    # 终端前端（clap + rpassword）
└── myapikey-gui    # 窗口前端（Tauri 2）
```

安全模型：仓库泄露只拿到密文 · 主密码不写入任何文件 · git 凭据只管读写不参与加密——三者互为独立防线。

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
- [x] 追加：`mak init --github` 一键初始化（Device Flow + 自动建仓库）
- [ ] M2 Windows GUI（Tauri）
- [ ] M3 打磨与发布（导入导出、自动锁、剪贴板清除、三平台 CI 产物）

决策记录与完整设计见 [docs/design.md](docs/design.md)。

## 📄 许可证

[PolyForm Noncommercial 1.0.0](LICENSE)——源码公开，允许以**非商业目的**使用、修改和分发；商业用途需另行获得授权（双许可模式，欢迎联系）。严格意义上属于 source-available 而非 OSI 开源。
