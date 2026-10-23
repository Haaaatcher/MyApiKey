//! `mak` 二进制入口（短名，与 `myapikey` 行为完全一致，见 ADR D11）。

fn main() {
    if let Err(e) = myapikey_cli::run() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
