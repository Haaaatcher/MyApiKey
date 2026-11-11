//! 构建期注入 GitHub App client secret(2026-11-06 用户拍板"内嵌 secret"路线):
//! 仓库根目录的 `client-secret.txt`(已 gitignore,**永不入库**)在编译时
//! 被读入环境变量 `MYAPIKEY_EMBEDDED_CLIENT_SECRET`,代码侧经
//! `env!()` 常量访问——大众分发形态下 secret 随二进制分发,与
//! VS Code / GitHub Desktop 的业界做法一致。
//!
//! 文件不存在(如 CI 未配置或他人构建)时注入空串——运行时据此回退
//! 到 keychain/env/设置页粘贴路径,编译永远成功。
//!
//! 注意:修改 client-secret.txt 后需要重新编译才生效。

fn main() {
    // 两个候选位置:crate 目录(CI 的 GUI 构建段)与仓库根(本地开发),
    // 任一存在即命中;都不存在(如他人构建)注入空串,编译不失败
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let root_dir = crate_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("client-secret.txt"));
    let secret = std::fs::read_to_string(crate_dir.join("client-secret.txt"))
        .or_else(|_| match root_dir {
            Some(p) => std::fs::read_to_string(p),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no client-secret.txt",
            )),
        })
        .unwrap_or_default();
    println!(
        "cargo:rustc-env=MYAPIKEY_EMBEDDED_CLIENT_SECRET={}",
        secret.trim()
    );
    println!("cargo:rerun-if-changed=client-secret.txt");
}
