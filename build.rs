fn main() {
    // 本包同时产出 `cdylib`（给 Android 的 NativeActivity 用）。换到 reqwest 之后
    // 依赖带来的符号量超过了 MinGW 的导出序号上限：
    //     ld: error: export ordinal too large: 69390
    // 桌面这份 cdylib 谁也不用，只是 `cargo build` 会顺带链接它。
    // 用 `--exclude-all-symbols` 只保留 rustc 通过 .def 显式列出的导出，
    // 顺便消掉"整个 Rust 符号表都被导出"这个无谓信息面。
    //
    // 只对 windows + gnu 生效：ELF（Android）用 lld，不认识这个参数。
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "gnu" {
        println!("cargo:rustc-link-arg=-Wl,--exclude-all-symbols");
    }
}
