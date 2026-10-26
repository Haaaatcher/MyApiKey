#!/usr/bin/env python3
"""发布时生成 updater 用的 latest.json（D16 应用内更新）。

用法（在仓库根目录，签名构建完成后运行）:
    python scripts/make-latest-json.py <version> [--breaking] [--notes "..."]

读取 target/release/bundle/nsis/ 下的安装包与 .sig 签名，输出
target/release/bundle/nsis/latest.json。上传 Release 时把安装包、
.sig 与 latest.json 一起作为附件（latest.json 名字固定，应用按
releases/latest/download/latest.json 拉取）。

notes 首行带 [breaking] 标记时，应用内守卫会拒绝自动安装（破坏性
升级需用户先备份再手动升级）——用 --breaking 自动加标记。
"""

import argparse
import datetime
import json
import pathlib
import sys

REPO = "Haaaatcher/MyApiKey"
BUNDLE_DIR = pathlib.Path("target/release/bundle/nsis")


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

    exe = BUNDLE_DIR / f"MyApiKey_{args.version}_x64-setup.exe"
    sig = pathlib.Path(f"{exe}.sig")
    if not exe.is_file():
        print(f"未找到安装包: {exe}", file=sys.stderr)
        return 1
    if not sig.is_file():
        print(f"未找到签名文件: {sig}（构建时需注入 TAURI_SIGNING_PRIVATE_KEY）", file=sys.stderr)
        return 1

    notes = ("[breaking]\n" if args.breaking else "") + args.notes
    doc = {
        "version": args.version,
        "notes": notes,
        "pub_date": datetime.datetime.now(datetime.timezone.utc)
        .isoformat(timespec="seconds")
        .replace("+00:00", "Z"),
        "platforms": {
            "windows-x86_64": {
                "signature": sig.read_text(encoding="utf-8").strip(),
                "url": f"https://github.com/{REPO}/releases/download/v{args.version}/{exe.name}",
            }
        },
    }
    out = BUNDLE_DIR / "latest.json"
    out.write_text(json.dumps(doc, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"已生成 {out}（v{args.version}{'，BREAKING' if args.breaking else ''}）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
