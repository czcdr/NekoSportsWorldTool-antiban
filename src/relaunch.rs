//! 桌面端自重启：自更新替换完成后拉起新版本进程。

/// 启动新版本程序，旧进程随后退出。
///
/// `update::apply` 已经把新二进制写到当前 exe 路径（旧文件改名为 `.old`），
/// 所以这里只需按**自身路径**启动一个子进程：不经过 shell、不传任何参数、
/// 不把更新包里的任何内容当命令执行。父进程随后由调用方关闭窗口退出，
/// 残留的 `.old` 由下次启动的 `update::cleanup_residue` 清理。
#[cfg(not(target_os = "android"))]
pub fn relaunch_self() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法定位自身程序：{e}"))?;
    let dir = exe
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    std::process::Command::new(&exe)
        .current_dir(dir)
        .spawn()
        .map_err(|e| format!("启动新版本失败（可手动重开程序）：{e}"))?;
    Ok(())
}

/// Android 上更新由系统安装器完成，应用无法自行重启。
#[cfg(target_os = "android")]
pub fn relaunch_self() -> Result<(), String> {
    Err("更新已安装，请从桌面图标重新打开应用".into())
}
