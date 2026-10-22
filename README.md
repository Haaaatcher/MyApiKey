<div align="center">

# 🔑 MyApiKey

**跨平台的 AI API 凭证记录与同步工具**

`base_url` + `api_key` · 加密存储 · git 同步 · 多设备

![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-informational)
![Status](https://img.shields.io/badge/status-design%20phase-orange)
![Rust](https://img.shields.io/badge/rust-stable-dea584?logo=rust)
![GUI](https://img.shields.io/badge/GUI-Tauri%202-blue)
![License](https://img.shields.io/badge/license-PolyForm--NC%201.0.0-blue)

[设计文档](docs/design.md) · [路线图](#️-路线图) · [开发](#️-开发)

</div>

---

> [!IMPORTANT]
> 项目处于**设计阶段**（M0 未开始），代码尚未动工。下述功能与用法均为规划内容。

## ✨ 特性

- 🔐 **加密存储** —— 主密码经 Argon2id 派生密钥，AES-256-GCM 加密单文件 vault，仓库中永远只有密文
- 🔄 **git 同步** —— 任意私有 git 仓库作后端（GitHub / Gitee / 自建均可），按 `updated_at` 合并冲突，删除墓碑防止记录复活；不配远端也能纯单机使用
- 🔑 **免输密码** —— 主密码输入一次即记住，存于系统凭据库（Credential Manager / Keychain / Secret Service），失效自动更新，随时可清除
- 🩺 **key 校验** —— 按协议调 `GET /models` 探测 key 状态：有效 / 无效 / 无法验证，失败不阻塞保存
- 🏷️ **协议即标签** —— OpenAI / Anthropic / 自定义，由用户手填，兼容任意三方平台与网关，不做自动推断
- 🖥️ **双形态** —— CLI（`myapikey` / `mak`，三平台）+ 窗口应用（Tauri 2，首发 Windows）

## 🚀 使用（M1 后可用）

```bash
mak init            # 设置主密码，初始化本地仓库
mak add             # 记录一条 base_url + api_key
mak list            # 查看列表（key 默认掩码显示）
mak sync            # 多设备同步（pull + 合并 + push）
mak check           # 校验 key 是否有效
```

<details>
<summary>📖 全部命令</summary>

```bash
myapikey init               # 初始化：设置主密码、git init、可选配置远端
myapikey add                # 交互式添加记录
myapikey list               # 列表（掩码显示，含校验状态）
myapikey show <id|name>     # 查看单条（--copy 复制 key）
myapikey edit <id|name>     # 编辑
myapikey delete <id|name>   # 删除（墓碑，30 天后清理）
myapikey check [id|name]    # 校验（不带参数 = 全部）
myapikey sync               # pull + 合并 + push
myapikey status             # 同步状态 / 未推送变更
myapikey remote set <url>   # 配置 / 更换远端仓库
myapikey passwd             # 修改主密码
myapikey forget             # 清除记住的密码
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

- [ ] M0 workspace 骨架 + vault 加解密核心（含测试）
- [ ] M1 CLI 全功能 + git 同步 —— 日常可用
- [ ] M2 Windows GUI（Tauri）
- [ ] M3 打磨与发布（导入导出、自动锁、剪贴板清除、三平台 CI 产物）

决策记录与完整设计见 [docs/design.md](docs/design.md)。

## 📄 许可证

[PolyForm Noncommercial 1.0.0](LICENSE)——源码公开，允许以**非商业目的**使用、修改和分发；商业用途需另行获得授权（双许可模式，欢迎联系）。严格意义上属于 source-available 而非 OSI 开源。
