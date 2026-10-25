//! 自动更新：GitHub Release 检查 / 下载 / 解包 / 自替换。
//!
//! 只查本项目自己的仓库（`REPO_OWN`）：这个 fork 的改造是运行安全的全部依据，
//! 自动更新回上游原版等于把改造覆盖掉；把第三方账号（历史上的
//! `hb-degithub` 镜像）当更新源，也等于让另一个账号拥有替换二进制的权力。
//! `NSWT_UPDATE_REPO` 环境变量可强制指定仓库（测试/自建发布用）。
//! 版本号 / 重启 / 安装的平台差异见 platform.rs。

use crate::api::client::{make_agent, http_err};

/// 本项目的发布仓库（唯一默认更新源）。
pub const REPO_OWN: &str = "czcdr/NekoSportsWorldTool-antiban";

const UA: &str = concat!("NekoSportsWorldTool/", env!("CARGO_PKG_VERSION"));

/// Android 更新包落盘文件名（私有目录，ApkProvider 只暴露这一个文件）。
pub const APK_NAME: &str = "update.apk";

/// 单次下载上限：正常发布包都在 20 MB 量级，512 MB 已足够宽松，
/// 又不至于让一个被篡改的 Content-Length 把内存吃光。
pub const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
/// 解包时单个文件的读取上限（桌面端解包用；Android 整包交给系统安装器）。
#[cfg(not(target_os = "android"))]
const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;
/// 解包时所有文件累计的读取上限。
#[cfg(not(target_os = "android"))]
const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

/// 检查更新要查询的仓库（按序）。
fn repos() -> Vec<String> {
    if let Ok(r) = std::env::var("NSWT_UPDATE_REPO") {
        if !r.trim().is_empty() {
            return vec![r.trim().to_string()];
        }
    }
    vec![REPO_OWN.to_string()]
}

/// 当前平台对应的 Release 资产名；无匹配产物的平台返回 None。
pub fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("NekoSportsWorldTool-win-x64.zip"),
        ("macos", "aarch64") => Some("NekoSportsWorldTool-macos-arm64.tar.gz"),
        ("linux", "x86_64") => Some("NekoSportsWorldTool-linux-x64.tar.gz"),
        ("linux", "aarch64") => Some("NekoSportsWorldTool-linux-aarch64.tar.gz"),
        ("android", "aarch64") => Some("NekoSportsWorldTool-android-arm64.apk"),
        _ => None,
    }
}

/// 当前程序版本（platform.rs：桌面读 Cargo，Android 读 manifest versionName）。
pub fn current_version() -> String {
    crate::platform::version_name()
}

/// Release 摘要（GUI / CLI 共用，经 channel JSON 传递）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReleaseInfo {
    pub repo: String,
    pub tag: String,
    pub title: String,
    pub notes: String,
    pub asset_name: String,
    pub asset_url: String,
    pub asset_size: u64,
    /// 资产摘要，形如 `sha256:<hex>`（GitHub Release API 的 `assets[].digest`）。
    /// 老接口/老资产可能没有，此时桌面端拒绝自动替换。
    #[serde(default)]
    pub asset_digest: Option<String>,
}

/// 检查最新 Release；比当前新返回 Some，已是最新返回 None。
/// 多仓库（Android）时取带资产且版本最新的候选。
pub fn check_latest(log: &mut dyn FnMut(&str)) -> Result<Option<ReleaseInfo>, String> {
    let want = asset_name().ok_or_else(|| "当前平台没有对应的发布产物，无法自动更新".to_string())?;
    let current = current_version();
    let mut best: Option<ReleaseInfo> = None;
    let mut last_err: Option<String> = None;
    for repo in repos() {
        match fetch_release(&repo, want, log) {
            Ok(Some(rel)) => {
                if best.as_ref().map(|b| is_newer(&b.tag, &rel.tag)).unwrap_or(true) {
                    best = Some(rel);
                }
            }
            Ok(None) => {}
            Err(e) => {
                log(&format!("[update] {repo}: {e}"));
                last_err = Some(e);
            }
        }
    }
    match best {
        Some(rel) => {
            if is_newer(&current, &rel.tag) {
                log(&format!(
                    "√ [update] {} 最新 {}（当前 v{}，{:.1} MB）",
                    rel.repo,
                    rel.tag,
                    current,
                    rel.asset_size as f64 / 1024.0 / 1024.0
                ));
                Ok(Some(rel))
            } else {
                log(&format!("√ [update] 已是最新版本（v{current}）"));
                Ok(None)
            }
        }
        None => Err(last_err.unwrap_or_else(|| "更新源没有可用的发布资产".into())),
    }
}

/// 拉取单仓库最新 Release 并匹配资产；Release 存在但无匹配资产 → Ok(None)。
fn fetch_release(repo: &str, want: &str, log: &mut dyn FnMut(&str)) -> Result<Option<ReleaseInfo>, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = make_agent()
        .get(&url)
        .set("User-Agent", UA)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(http_err)?;
    let text = resp.into_string().map_err(|e| format!("读取 Release 失败: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("解析 Release 失败: {e}"))?;
    let tag = v.get("tag_name").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if tag.is_empty() {
        return Err("Release 缺少 tag".into());
    }
    let Some(asset) = pick_asset(&v, want) else {
        return Ok(None);
    };
    log(&format!("[update] {repo} 最新 {tag}，匹配资产 {}", asset.name));
    Ok(Some(ReleaseInfo {
        repo: repo.to_string(),
        tag,
        title: v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        notes: v.get("body").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        asset_name: asset.name,
        asset_url: asset.url,
        asset_size: asset.size,
        asset_digest: asset.digest,
    }))
}

struct AssetPick {
    name: String,
    url: String,
    size: u64,
    digest: Option<String>,
}

/// 资产匹配：桌面产物要求名字完全一致；APK 先精确后模糊（兼容历史命名）。
fn pick_asset(v: &serde_json::Value, want: &str) -> Option<AssetPick> {
    let assets = v.get("assets")?.as_array()?;
    let find = |pred: &dyn Fn(&str) -> bool| -> Option<AssetPick> {
        assets.iter().find_map(|a| {
            let name = a.get("name")?.as_str()?;
            if !pred(name) {
                return None;
            }
            Some(AssetPick {
                name: name.to_string(),
                url: a.get("browser_download_url")?.as_str()?.to_string(),
                size: a.get("size")?.as_u64().unwrap_or(0),
                digest: a
                    .get("digest")
                    .and_then(|d| d.as_str())
                    .map(|d| d.trim().to_string())
                    .filter(|d| !d.is_empty()),
            })
        })
    };
    if want.ends_with(".apk") {
        find(&|n| n == want)
            .or_else(|| find(&|n| n.ends_with(".apk") && n.contains("arm64")))
            .or_else(|| find(&|n| n.ends_with(".apk")))
    } else {
        find(&|n| n == want)
    }
}

/// 版本比较：剥 v 前缀按点分段数字比较；主版本相同时 -android.N 后缀大的更新。
pub fn is_newer(current: &str, latest: &str) -> bool {
    let (c_base, c_rev) = split_rev(current);
    let (l_base, l_rev) = split_rev(latest);
    if c_base != l_base {
        return l_base > c_base;
    }
    l_rev > c_rev
}

/// "v0.2.5-android.3" → ([0,2,5], Some(3))。
fn split_rev(v: &str) -> (Vec<u64>, Option<u64>) {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let (base, rev) = match v.split_once('-') {
        Some((b, r)) => (b, r.rsplit('.').next().and_then(|n| n.parse().ok())),
        None => (v, None),
    };
    let segs = base.split('.').map(|s| s.parse::<u64>().unwrap_or(0)).collect();
    (segs, rev)
}

/// 流式下载（默认 agent 30s 总超时不够下几 MB 的包，专用长超时）。
/// 全程受 `MAX_DOWNLOAD_BYTES` 约束：先看 Content-Length，再按实际读取量兜底，
/// 避免被一个伪造的长度或无限流把内存吃光。
pub fn download(url: &str, progress: impl FnMut(u64, Option<u64>)) -> Result<Vec<u8>, String> {
    download_with_limit(url, MAX_DOWNLOAD_BYTES, progress)
}

/// 下载实现。`max` 单独开出来是为了让测试用很小的上限覆盖「实际读取量超限」分支
/// —— 那条分支在真实上限下要下载 512 MB 才能触发。
fn download_with_limit(
    url: &str,
    max: u64,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<Vec<u8>, String> {
    let agent = crate::http::Agent::with_timeout(std::time::Duration::from_secs(300));
    let resp = agent.get(url).set("User-Agent", UA).call().map_err(http_err)?;
    let total = resp.header("Content-Length").and_then(|h| h.parse::<u64>().ok());
    if let Some(declared) = total {
        if declared > max {
            return Err(format!(
                "更新包声明 {} 字节，超过下载上限 {} 字节，已放弃",
                declared, max
            ));
        }
    }
    let mut buf = Vec::new();
    let mut reader = resp.into_reader();
    let mut chunk = vec![0u8; 65536];
    loop {
        let n = std::io::Read::read(&mut reader, &mut chunk)
            .map_err(|e| format!("下载中断: {e}"))?;
        if n == 0 {
            break;
        }
        if buf.len() as u64 + n as u64 > max {
            return Err(format!("更新包超过下载上限 {max} 字节，已中止"));
        }
        buf.extend_from_slice(&chunk[..n]);
        progress(buf.len() as u64, total);
    }
    Ok(buf)
}

/// 下载后校验：声明的字节数 + Release 元数据里的 SHA-256。
///
/// 摘要来自 GitHub Release API（同一 HTTPS 通道），能挡住"资产被替换/下载被
/// 篡改/连错仓库"这类问题；它防不了发布账号本身被控制 —— 那需要发布侧签名，
/// 本项目没有签署流程，这点必须如实说明。
pub fn verify(rel: &ReleaseInfo, bytes: &[u8]) -> Result<(), String> {
    if rel.asset_size > 0 && bytes.len() as u64 != rel.asset_size {
        return Err(format!(
            "更新包大小不符：Release 声明 {} 字节，实际下载 {} 字节",
            rel.asset_size,
            bytes.len()
        ));
    }
    match rel.asset_digest.as_deref() {
        Some(digest) => verify_sha256(bytes, digest),
        None if rel.asset_name.ends_with(".apk") => {
            // APK 由系统安装器校验签名：签名不符会被系统拒绝，风险相对可控。
            eprintln!("[update] 该资产没有摘要，依赖 APK 签名校验兜底");
            Ok(())
        }
        None => Err(
            "更新包没有 SHA-256 摘要，无法确认完整性；请到 Release 页面手动下载核对".into(),
        ),
    }
}

/// 校验 `sha256:<hex>` 摘要。
pub fn verify_sha256(bytes: &[u8], digest: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let want = digest
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(digest.trim())
        .trim()
        .to_ascii_lowercase();
    if want.len() != 64 || !want.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("更新包摘要格式异常：{digest}"));
    }
    let got = format!("{:x}", Sha256::digest(bytes));
    if got == want {
        Ok(())
    } else {
        Err(format!(
            "更新包 SHA-256 不匹配（期望 {want}，实际 {got}），已丢弃"
        ))
    }
}

/// 从发布包解出新程序二进制。桌面：zip 内找 *.exe（7z 打包带 target/
/// 目录前缀，取最大的一条）；tar.gz 内找根级 nekosportsworldtool。
#[cfg(not(target_os = "android"))]
pub fn extract(asset_name: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    if asset_name.ends_with(".zip") {
        extract_zip(bytes)
    } else if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
        extract_tar_gz(bytes)
    } else if asset_name.ends_with(".apk") {
        Ok(bytes.to_vec())
    } else {
        Err(format!("不支持的发布包格式：{asset_name}"))
    }
}

#[cfg(not(target_os = "android"))]
fn extract_zip(bytes: &[u8]) -> Result<Vec<u8>, String> {
    extract_zip_with_limits(bytes, MAX_ENTRY_BYTES, MAX_TOTAL_BYTES)
}

/// 解包 zip 并取出可执行文件。
///
/// 三个上限都必要：`ZipFile::size()` 是**压缩包自己声明的**值，恶意包可以声明
/// 几十 GB，`Vec::with_capacity(声明值)` 会直接把进程打崩；`read_to_end` 也没有
/// 天然上限。所以预分配只按小块给，实际读取量另行累加校验。
#[cfg(not(target_os = "android"))]
fn extract_zip_with_limits(
    bytes: &[u8],
    max_entry: u64,
    max_total: u64,
) -> Result<Vec<u8>, String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("zip 打开失败: {e}"))?;
    let mut best: Option<Vec<u8>> = None;
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| format!("zip 读取失败: {e}"))?;
        if f.is_dir() {
            continue;
        }
        let is_exe = std::path::Path::new(f.name())
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("exe"))
            .unwrap_or(false);
        if !is_exe {
            continue;
        }
        let declared = f.size();
        if declared > max_entry {
            return Err(format!(
                "zip 条目 {} 声明 {} 字节，超过单文件上限 {} 字节",
                f.name(),
                declared,
                max_entry
            ));
        }
        // 只按当前长度预分配，不信任声明值。
        let mut out = Vec::new();
        let mut limited = std::io::Read::take(&mut f, max_entry + 1);
        std::io::Read::read_to_end(&mut limited, &mut out)
            .map_err(|e| format!("zip 解压失败: {e}"))?;
        if out.len() as u64 > max_entry {
            return Err(format!("zip 条目 {} 实际解压超过上限 {} 字节", f.name(), max_entry));
        }
        total = total.saturating_add(out.len() as u64);
        if total > max_total {
            return Err(format!("zip 累计解压超过上限 {} 字节", max_total));
        }
        if best.as_ref().map(|b| out.len() > b.len()).unwrap_or(true) {
            best = Some(out);
        }
    }
    best.ok_or_else(|| "zip 内未找到程序文件".into())
}

#[cfg(not(target_os = "android"))]
fn extract_tar_gz(bytes: &[u8]) -> Result<Vec<u8>, String> {
    extract_tar_gz_with_limits(bytes, MAX_ENTRY_BYTES, MAX_TOTAL_BYTES)
}

#[cfg(not(target_os = "android"))]
fn extract_tar_gz_with_limits(
    bytes: &[u8],
    max_entry: u64,
    max_total: u64,
) -> Result<Vec<u8>, String> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut tar = tar::Archive::new(gz);
    let mut best: Option<Vec<u8>> = None;
    let mut total: u64 = 0;
    let mut entries = tar.entries().map_err(|e| format!("tar 打开失败: {e}"))?;
    while let Some(entry) = entries.next() {
        let mut e = entry.map_err(|e| format!("tar 读取失败: {e}"))?;
        if !e.header().entry_type().is_file() {
            continue;
        }
        let path = e.path().map_err(|e| format!("tar 路径异常: {e}"))?;
        if path.file_name().and_then(|n| n.to_str()) != Some("nekosportsworldtool") {
            continue;
        }
        // tar 头里的 size 同样来自压缩包自身，先按它拦一道，再按实际读取量兜底。
        if e.size() > max_entry {
            return Err(format!("tar 条目声明 {} 字节，超过单文件上限 {max_entry} 字节", e.size()));
        }
        let mut out = Vec::new();
        let mut limited = std::io::Read::take(&mut e, max_entry + 1);
        std::io::Read::read_to_end(&mut limited, &mut out)
            .map_err(|e| format!("tar 解压失败: {e}"))?;
        if out.len() as u64 > max_entry {
            return Err(format!("tar 条目实际解压超过上限 {max_entry} 字节"));
        }
        total = total.saturating_add(out.len() as u64);
        if total > max_total {
            return Err(format!("tar 累计解压超过上限 {max_total} 字节"));
        }
        if best.as_ref().map(|b| out.len() > b.len()).unwrap_or(true) {
            best = Some(out);
        }
    }
    best.ok_or_else(|| "tar.gz 内未找到程序文件".into())
}

/// 自替换：新二进制落 .new → 当前程序改名 .old（Windows 允许重命名运行中的
/// exe）→ .new 转正；失败回滚。调用方随后退出，残留 .old 由下次启动清理。
#[cfg(not(target_os = "android"))]
pub fn apply(binary: &[u8]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法定位自身路径: {e}"))?;
    let dir = exe.parent().ok_or("无法定位 exe 目录")?;
    let file = exe.file_name().and_then(|n| n.to_str()).ok_or("exe 文件名异常")?;
    let new = dir.join(format!("{file}.new"));
    let old = dir.join(format!("{file}.old"));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&new)
            .map_err(|e| format!("写入更新文件失败（目录可能只读）: {e}"))?;
        f.write_all(binary).map_err(|e| format!("写入更新内容失败: {e}"))?;
        f.sync_all().map_err(|e| format!("落盘失败: {e}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("设置执行权限失败: {e}"))?;
    }
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| {
        let _ = std::fs::remove_file(&new);
        format!("无法移动当前程序（可能被占用）: {e}")
    })?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("替换程序失败: {e}"));
    }
    Ok(())
}

/// 清理上次更新残留的 .old/.new（旧进程未退出时会删不掉，静默跳过）。
pub fn cleanup_residue() {
    let Ok(exe) = std::env::current_exe() else { return };
    let Some(dir) = exe.parent() else { return };
    let Some(file) = exe.file_name().and_then(|n| n.to_str()) else { return };
    for suffix in [".old", ".new"] {
        let _ = std::fs::remove_file(dir.join(format!("{file}{suffix}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.2.5", "v0.2.6"));
        assert!(is_newer("0.2.9", "v0.3.0"));
        assert!(is_newer("0.2.9", "v0.10.0"));
        assert!(!is_newer("0.2.5", "v0.2.5"));
        assert!(!is_newer("0.2.6", "v0.2.5"));
        // android 后缀序号
        assert!(is_newer("0.2.5-android.3", "v0.2.5-android.4"));
        assert!(!is_newer("0.2.5-android.4", "v0.2.5-android.4"));
        assert!(is_newer("0.2.5", "v0.2.6-android.1"));
        assert!(is_newer("0.2.5-android.99", "v0.2.6-android.1"));
        // 位数不同 / 前后空格
        assert!(is_newer("0.2", "v0.2.1"));
        assert!(!is_newer(" 0.2.5 ", "v0.2.5"));
    }

    #[test]
    fn asset_pick_exact_and_fuzzy() {
        let mk = |names: &[&str]| {
            serde_json::json!({
                "assets": names.iter().map(|n| serde_json::json!({
                    "name": n,
                    "size": 100,
                    "browser_download_url": format!("https://x/{n}"),
                })).collect::<Vec<_>>(),
            })
        };
        let exact = pick_asset(&mk(&["NekoSportsWorldTool-win-x64.zip", "other.txt"]), "NekoSportsWorldTool-win-x64.zip");
        assert_eq!(exact.unwrap().name, "NekoSportsWorldTool-win-x64.zip");
        // 桌面产物必须精确匹配
        assert!(pick_asset(&mk(&["wrong.zip"]), "NekoSportsWorldTool-win-x64.zip").is_none());
        // APK 模糊匹配：先精确、再 arm64、再任意 apk
        let apk = pick_asset(&mk(&["NekoSportsWorldTool-0.2.5-android.2-arm64.apk"]), "NekoSportsWorldTool-android-arm64.apk");
        assert_eq!(apk.unwrap().name, "NekoSportsWorldTool-0.2.5-android.2-arm64.apk");
        let any = pick_asset(&mk(&["app-arm64.apk", "plain.apk"]), "NekoSportsWorldTool-android-arm64.apk");
        assert_eq!(any.unwrap().name, "app-arm64.apk");
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn extract_zip_nested_exe() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opt = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("target/x86_64-pc-windows-msvc/release/nekosportsworldtool.exe", opt).unwrap();
            std::io::Write::write_all(&mut w, b"BIN_CONTENT").unwrap();
            w.start_file("readme.txt", opt).unwrap();
            std::io::Write::write_all(&mut w, b"ignore").unwrap();
            w.finish().unwrap();
        }
        let bytes = buf.into_inner();
        assert_eq!(extract_zip(&bytes).unwrap(), b"BIN_CONTENT");
        assert_eq!(extract("NekoSportsWorldTool-win-x64.zip", &bytes).unwrap(), b"BIN_CONTENT");
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn extract_tar_gz_root_binary() {
        let mut bytes = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
            let mut tar = tar::Builder::new(gz);
            let mut hdr = tar::Header::new_gnu();
            let content: &[u8] = b"BIN_CONTENT";
            hdr.set_size(content.len() as u64);
            hdr.set_mode(0o755);
            hdr.set_cksum();
            tar.append_data(&mut hdr, "nekosportsworldtool", content).unwrap();
            let gz = tar.into_inner().unwrap();
            gz.finish().unwrap();
        }
        assert_eq!(extract_tar_gz(&bytes).unwrap(), b"BIN_CONTENT");
        assert_eq!(extract("NekoSportsWorldTool-linux-x64.tar.gz", &bytes).unwrap(), b"BIN_CONTENT");
    }

    fn release_fixture(name: &str, size: u64, digest: Option<&str>) -> ReleaseInfo {
        ReleaseInfo {
            repo: REPO_OWN.into(),
            tag: "v9.9.9".into(),
            title: String::new(),
            notes: String::new(),
            asset_name: name.into(),
            asset_url: "https://example.invalid/x".into(),
            asset_size: size,
            asset_digest: digest.map(|d| d.to_string()),
        }
    }

    #[test]
    fn sha256_verification_accepts_only_the_declared_digest() {
        let payload = b"BIN_CONTENT";
        let good = format!("sha256:{:x}", sha2::Sha256::digest(payload));
        assert!(verify_sha256(payload, &good).is_ok());
        // 大小写与不带前缀都接受
        assert!(verify_sha256(payload, &good.to_uppercase().replace("SHA256:", "")).is_ok());
        let replacement = if good.as_bytes()[7] == b'0' { '1' } else { '0' };
        let bad = format!("sha256:{}{}", replacement, &good[8..]);
        assert!(verify_sha256(payload, &bad).is_err());
        // 格式异常不能被当成"通过"
        for weird in ["", "sha256:", "sha256:zz", "md5:abc"] {
            assert!(verify_sha256(payload, weird).is_err(), "{weird} 不应通过");
        }
    }

    #[test]
    fn verify_rejects_size_mismatch_and_unverified_desktop_payloads() {
        let payload = b"BIN_CONTENT";
        let good = format!("sha256:{:x}", sha2::Sha256::digest(payload));
        assert!(verify(&release_fixture("a.zip", payload.len() as u64, Some(&good)), payload).is_ok());
        // 声明大小不符 → 拒绝
        assert!(verify(&release_fixture("a.zip", payload.len() as u64 + 1, Some(&good)), payload).is_err());
        // 桌面包没有摘要 → 拒绝自动替换
        let err = verify(&release_fixture("a.zip", payload.len() as u64, None), payload).unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        // APK 没有摘要 → 交给系统签名校验兜底
        assert!(verify(&release_fixture("a.apk", payload.len() as u64, None), payload).is_ok());
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn zip_and_tar_extraction_are_bounded() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opt = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("a.exe", opt).unwrap();
            std::io::Write::write_all(&mut w, &vec![7u8; 4096]).unwrap();
            w.finish().unwrap();
        }
        let zip_bytes = buf.into_inner();
        assert!(extract_zip_with_limits(&zip_bytes, 1024, u64::MAX).is_err(), "单文件上限");
        assert!(extract_zip_with_limits(&zip_bytes, 8192, 1024).is_err(), "累计上限");
        assert_eq!(
            extract_zip_with_limits(&zip_bytes, 8192, u64::MAX).unwrap().len(),
            4096
        );

        let mut tar_bytes = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut tar_bytes, flate2::Compression::default());
            let mut tar = tar::Builder::new(gz);
            let mut hdr = tar::Header::new_gnu();
            let content = vec![7u8; 4096];
            hdr.set_size(content.len() as u64);
            hdr.set_mode(0o755);
            hdr.set_cksum();
            tar.append_data(&mut hdr, "nekosportsworldtool", content.as_slice()).unwrap();
            let gz = tar.into_inner().unwrap();
            gz.finish().unwrap();
        }
        assert!(extract_tar_gz_with_limits(&tar_bytes, 1024, u64::MAX).is_err(), "单文件上限");
        assert!(extract_tar_gz_with_limits(&tar_bytes, 8192, 1024).is_err(), "累计上限");
        assert_eq!(
            extract_tar_gz_with_limits(&tar_bytes, 8192, u64::MAX).unwrap().len(),
            4096
        );
    }

    /// 起一个只服务一次请求的本地 HTTP 服务器（返回固定响应头 + 正文）。
    fn one_shot_server(head: &'static str, body: &'static [u8]) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        format!("http://{addr}/asset")
    }

    #[test]
    fn download_rejects_oversized_content_length_before_reading() {
        let url = one_shot_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n",
            b"",
        );
        let err = download_with_limit(&url, 1024, |_, _| {}).unwrap_err();
        assert!(err.contains("声明"), "{err}");
    }

    #[test]
    fn download_aborts_when_streamed_body_exceeds_limit() {
        static BODY: [u8; 8192] = [b'x'; 8192];
        // 不声明 Content-Length：走「按实际读取量兜底」的分支
        let url = one_shot_server("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n", &BODY);
        let err = download_with_limit(&url, 1024, |_, _| {}).unwrap_err();
        assert!(err.contains("上限"), "{err}");
        // 上限足够时同一份正文应当完整下载
        let url = one_shot_server("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n", &BODY);
        let bytes = download_with_limit(&url, 8192, |_, _| {}).unwrap();
        assert_eq!(bytes.len(), 8192);
    }
}
