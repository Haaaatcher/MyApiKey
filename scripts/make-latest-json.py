#!/usr/bin/env python3
"""发布时生成 updater 用的 latest.json（D16 应用内更新）。

用法（在仓库根目录，签名构建完成后运行）:
    python scripts/make-latest-json.py <version> [--breaking] [--notes "..."]

扫描 target/release/bundle/ 下已构建平台的安装包/机制件与 .sig 签名：
    windows-x86_64: bundle/nsis/Makara_<version>_x64-setup.exe
    darwin-aarch64: bundle/macos/Makara*.app.tar.gz（updater 机制件，D54）
输出 target/release/bundle/latest.json（多平台清单放 bundle 根，不再
限定 nsis 目录）。上传 Release 时把安装包、.sig 与 latest.json 一起
作为附件（latest.json 名字固定，应用按 releases/latest/download/latest.json
拉取）。

notes 首行带 [breaking] 标记时，应用内守卫会拒绝自动安装（破坏性
升级需用户先备份再手动升级）——用 --breaking 自动加标记。
"""

import argparse
import datetime
import json
import pathlib
import sys

REPO = "Haaaatcher/Makara"
BUNDLE_DIR = pathlib.Path("target/release/bundle")


def collect_platforms(version: str) -> dict[str, dict[str, str]]:
    """扫描已构建平台，返回 latest.json 的 platforms 映射。

    每个平台键要求安装包/机制件与其 .sig 同时存在——缺签名视为致命
    （updater 验签失败等于更新链路断裂），直接报错退出。
    """
    platforms: dict[str, dict[str, str]] = {}

    # Windows：NSIS 安装包本体进清单（updater 下载后静默换装）
    nsis = BUNDLE_DIR / "nsis" / f"Makara_{version}_x64-setup.exe"
    if nsis.is_file():
        sig = pathlib.Path(f"{nsis}.sig")
        if not sig.is_file():
            print(f"未找到签名文件: {sig}（构建时需注入 TAURI_SIGNING_PRIVATE_KEY）", file=sys.stderr)
            raise SystemExit(1)
        platforms["windows-x86_64"] = {
            "signature": sig.read_text(encoding="utf-8").strip(),
            "url": f"https://github.com/{REPO}/releases/download/v{version}/{nsis.name}",
        }

    # macOS（D54）：updater 机制件 Makara*.app.tar.gz 进清单，人装的
    # dmg 不进（应用内更新走 .app.tar.gz 增量换装，机制要求）
    macos_dir = BUNDLE_DIR / "macos"
    artifacts = sorted(macos_dir.glob("*.app.tar.gz")) if macos_dir.is_dir() else []
    if artifacts:
        artifact = artifacts[0]
        sig = pathlib.Path(f"{artifact}.sig")
        if not sig.is_file():
            print(f"未找到签名文件: {sig}（构建时需注入 TAURI_SIGNING_PRIVATE_KEY）", file=sys.stderr)
            raise SystemExit(1)
        platforms["darwin-aarch64"] = {
            "signature": sig.read_text(encoding="utf-8").strip(),
            "url": f"https://github.com/{REPO}/releases/download/v{version}/{artifact.name}",
        }

    return platforms


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("version", help="版本号，如 0.2.0")
    ap.add_argument("--notes", default="", help="发布说明（多行文本）")
    ap.add_argument("--breaking", action="store_true", help="标注破坏性更新")
    args = ap.parse_args()

    # 版本号与 tauri.conf.json 断言一致：传错版本会生成指向不存在
    # 资产的 latest.json（2026-10-26 采纳审查建议）
    conf = json.loads(pathlib.Path("crates/myapikey-gui/tauri.conf.json").read_text(encoding="utf-8"))
    if conf.get("version") != args.version:
        print(
            f"版本不一致: tauri.conf.json 是 {conf.get('version')}，参数是 {args.version}",
            file=sys.stderr,
        )
        return 1

    platforms = collect_platforms(args.version)
    if not platforms:
        print(
            "未找到任何平台的安装包/机制件: 先完成签名构建再生成清单",
            file=sys.stderr,
        )
        return 1
    # D54 起 latest.json 必须双平台齐全：缺键会让对应平台的应用内更新
    # 静默失效（updater 找不到自己的键就什么都不发生），发版入口强制
    # 校验而非事后发现（2026-11-13 审查建议）。本地只测单平台生成时
    # 可临时注释，切勿带着缺键清单发版
    required = ("darwin-aarch64", "windows-x86_64")
    missing = [k for k in required if k not in platforms]
    if missing:
        print(
            f"缺平台键: {'、'.join(missing)}——对应平台的安装包/机制件未构建或未找到。"
            "缺键平台的应用内更新会静默失效，禁止以缺键清单发版",
            file=sys.stderr,
        )
        return 1

    notes = ("[breaking]\n" if args.breaking else "") + args.notes
    doc = {
        "version": args.version,
        "notes": notes,
        "pub_date": datetime.datetime.now(datetime.timezone.utc)
        .isoformat(timespec="seconds")
        .replace("+00:00", "Z"),
        "platforms": platforms,
    }
    out = BUNDLE_DIR / "latest.json"
    out.write_text(json.dumps(doc, ensure_ascii=False, indent=2), encoding="utf-8")
    found = "、".join(sorted(platforms))
    print(f"已生成 {out}（v{args.version}{'，BREAKING' if args.breaking else ''}；平台：{found}）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
