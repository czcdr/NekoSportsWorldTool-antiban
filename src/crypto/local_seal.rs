//! 本地凭据的静态加密：别把口令/令牌以明文写进 `config.json` / `session.json`。
//!
//! # 为什么不是"系统钥匙串"
//! 跨 Windows / Linux / macOS / Android 各写一套钥匙串后端（DPAPI、Keychain、
//! Keystore）成本远大于收益，而且同账户进程照样能调用它们解出明文。
//! 这里的目标很具体：**文件被单独拷走时（网盘同步、备份、截图、误发）不再是明文**。
//!
//! # 密钥从哪来
//! `SHA-256( 用户 + 主机标识 + 数据目录 + 每安装随机 salt )`。salt 首次使用时生成，
//! 落盘权限 0600（`model::write_secret_file`）。
//!
//! # 边界（必须如实说明）
//! salt 与密文在同一台机器的同一存储里，**同账户的其他进程仍可解密**。
//! 这挡的是"文件流失"，不是"本机被入侵"；本机被入侵时任何不依赖硬件密钥的方案
//! 都挡不住。解密失败（换机器、改用户名、删了 salt）时调用方应回退成"让用户重新登录"。
//!
//! # 迁移
//! 旧版本留下的是明文 `password` / `token`：读取路径优先用明文（原样可用），
//! 下次保存时自动改写成 `*_sealed` 并把明文清空。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use cbc::cipher::{block_padding::Pkcs7, BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use sha2::{Digest, Sha256};

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// 密文格式前缀：以后换算法/换密钥派生方式时靠它区分。
const PREFIX: &str = "v1:";
/// salt 文件名（放在数据目录里，与凭据文件同级）。
const SALT_FILE: &str = "local.salt";

/// 读取（或首次生成）每安装随机 salt。
fn salt() -> Option<[u8; 32]> {
    let path = crate::platform::data_dir().join(SALT_FILE);
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return Some(arr);
        }
    }
    let fresh: [u8; 32] = rand::random();
    // 生成失败就放弃加密（返回 None）：宁可退回旧行为，也不能让登录因为
    // 一个只读目录彻底不可用。
    match crate::api::model::write_secret_file(&path, &fresh) {
        Ok(()) => Some(fresh),
        Err(e) => {
            eprintln!("[data] 本地凭据加密不可用（salt 写入失败：{e}），本次按明文保存");
            None
        }
    }
}

/// 密钥派生材料：本机 + 当前用户 + 数据目录。
///
/// 三者任一变化（换机器、改用户名、挪数据目录）都会让旧密文解不开 ——
/// 这是有意的：密文跟着环境走，拷到别处就是废数据。
fn key_material() -> String {
    let dir = crate::platform::data_dir().display().to_string();
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    let host = host_id();
    format!("{user}|{host}|{dir}|neko-local-seal")
}

fn host_id() -> String {
    #[cfg(windows)]
    {
        let name = std::env::var("COMPUTERNAME").unwrap_or_default();
        let arch = std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default();
        format!("{name}/{arch}")
    }
    #[cfg(not(windows))]
    {
        // Linux: /etc/machine-id；Android 上通常读不到，回落到 hostname。
        std::fs::read_to_string("/etc/machine-id")
            .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_default()
    }
}

fn key() -> Option<[u8; 32]> {
    let salt = salt()?;
    let mut hasher = Sha256::new();
    hasher.update(key_material().as_bytes());
    hasher.update(salt);
    Some(hasher.finalize().into())
}

/// 加密。返回 `v1:<base64(iv||ciphertext)>`；空串原样返回空串。
pub fn seal(plain: &str) -> Option<String> {
    if plain.is_empty() {
        return Some(String::new());
    }
    let key = key()?;
    let iv: [u8; 16] = rand::random();
    let ciphertext = Aes256CbcEnc::new(&key.into(), &iv.into())
        .encrypt_padded_vec::<Pkcs7>(plain.as_bytes());
    let mut blob = Vec::with_capacity(16 + ciphertext.len());
    blob.extend_from_slice(&iv);
    blob.extend_from_slice(&ciphertext);
    Some(format!("{PREFIX}{}", B64.encode(blob)))
}

/// 解密。格式不符 / 密钥变了 / 数据被改过都返回 `None`。
pub fn open(sealed: &str) -> Option<String> {
    let body = sealed.strip_prefix(PREFIX)?;
    let blob = B64.decode(body).ok()?;
    if blob.len() < 32 {
        return None;
    }
    let (iv, ciphertext) = blob.split_at(16);
    let key = key()?;
    let iv: [u8; 16] = iv.try_into().ok()?;
    let plain = Aes256CbcDec::new(&key.into(), &iv.into())
        .decrypt_padded_vec::<Pkcs7>(ciphertext)
        .ok()?;
    String::from_utf8(plain).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立数据目录：salt 落在 TEST_DATA_DIR 里。
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "neko-seal-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            crate::platform::TEST_DATA_DIR.with(|cell| *cell.borrow_mut() = Some(dir.clone()));
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            crate::platform::TEST_DATA_DIR.with(|cell| *cell.borrow_mut() = None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn round_trip_and_no_plaintext_in_output() {
        let _dir = TempDir::new("roundtrip");
        let secret = "s3cret-pass-中文";
        let sealed = seal(secret).expect("seal");
        assert!(sealed.starts_with(PREFIX), "{sealed}");
        assert!(!sealed.contains(secret), "密文里不能出现明文");
        assert!(!sealed.contains("s3cret"), "{sealed}");
        assert_eq!(open(&sealed).as_deref(), Some(secret));
        // 空串保持空串（未记住密码的场景）
        assert_eq!(seal("").as_deref(), Some(""));
    }

    #[test]
    fn sealed_values_differ_because_iv_is_random() {
        let _dir = TempDir::new("iv");
        let a = seal("same").unwrap();
        let b = seal("same").unwrap();
        assert_ne!(a, b, "同一明文两次加密不应相同（IV 随机）");
        assert_eq!(open(&a).as_deref(), Some("same"));
        assert_eq!(open(&b).as_deref(), Some("same"));
    }

    #[test]
    fn tampered_or_foreign_ciphertext_is_rejected() {
        let _dir = TempDir::new("tamper");
        let sealed = seal("s3cret").unwrap();
        // 版本前缀不对
        assert!(open(&format!("v2:{}", &sealed[3..])).is_none());
        // 不是 base64
        assert!(open("v1:not-base64!!").is_none());
        // 太短
        assert!(open("v1:AAAA").is_none());
        // 改一个字节 → PKCS7/密钥流不符
        let mut blob = B64.decode(&sealed[3..]).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        assert!(open(&format!("v1:{}", B64.encode(blob))).is_none(), "被改过的密文必须解不开");
    }
}
