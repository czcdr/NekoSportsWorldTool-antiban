//! TLS 客户端指纹：把 rustls 的 ClientHello 压成 Conscrypt（Android）的形状。
//!
//! 网关不做任何业务分析就能分类客户端：取一次 JA3/JA4 就够。原来这一层是
//! rustls 的默认顺序 —— 密码套件按 rustls 自己的偏好排、曲线组也是它自己的，
//! 与官方客户端的 OkHttp+Conscrypt 明显不同，于是「官方 App」和「本工具」
//! 在**握手阶段**就是两类。这一层不通，后面的 HTTP 头再齐也没用。
//!
//! 能对齐的部分（本模块负责，全部可在单测里断言）：
//! - 密码套件**集合与顺序**：Conscrypt 默认 9 套，rustls 恰好全部支持（它不支持
//!   CBC，而 Android 10+ 的默认套件里也没有 CBC），所以这里能做到逐项相等；
//! - 曲线组顺序：`X25519` → `secp256r1` → `secp384r1`（BoringSSL 的默认顺序）；
//! - ALPN：`h2` / `http/1.1`（OkHttp 支持的两种，顺序也是 h2 在前）；
//! - TLS 版本：1.3 + 1.2（Android 10+ 关掉了 1.0/1.1）。
//!
//! **仍然不同的（换 crate 才能解决，本轮没有做）**：
//! - 扩展集合与顺序：BoringSSL 会发 `ec_point_formats`、`session_ticket` 等，
//!   rustls 的扩展表不一样 —— 所以 **JA3 的扩展段对不上，完整 JA3 字符串仍然不等**。
//!   等的是 cipher 段（JA3 的第 2 段）与 EC 段（第 4 段）。
//! - `signature_algorithms` 的列表与顺序由 rustls 的 `WebPkiSupportedAlgorithms`
//!   决定，本模块没有改写它（改写需要自建 WebPkiSupportedAlgorithms，风险大于收益）。
//! - 真要逐字节一致只能换 BoringSSL 绑定（`boring`），那是独立一轮的工程：
//!   cmake 构建 + Android 交叉编译 + 与 reqwest 的连接器对接，不是改常量能收口的。
//!
//! 所以本模块的定位要说清：**把 JA3 里最稳的两段（cipher、group）对齐，并把这件事
//! 钉进单测**，而不是宣称"JA3 已经等于官方客户端"。

use std::sync::Arc;

/// Conscrypt（Android 10+）默认启用并**按此顺序**写进 ClientHello 的密码套件。
///
/// 来源是 AOSP Conscrypt 的 `NativeCrypto.DEFAULT_CIPHER_SUITES`：TLS1.3 三套在前，
/// 之后 ECDHE_ECDSA / ECDHE_RSA 交替，每种密钥交换按 GCM-128 → GCM-256 → CHACHA20 排。
/// 顺序不是装饰：JA3 里 cipher 段按原样拼接，顺序变了哈希就变。
pub const CONSCRYPT_CIPHER_ORDER: &[rustls::CipherSuite] = &[
    rustls::CipherSuite::TLS13_AES_128_GCM_SHA256,
    rustls::CipherSuite::TLS13_AES_256_GCM_SHA384,
    rustls::CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
    rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    rustls::CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
    rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    rustls::CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
    rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    rustls::CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
];

/// BoringSSL（Conscrypt）的默认命名曲线顺序，对应 JA3 的 EC 段。
pub const CONSCRYPT_GROUP_ORDER: &[rustls::NamedGroup] = &[
    rustls::NamedGroup::X25519,
    rustls::NamedGroup::secp256r1,
    rustls::NamedGroup::secp384r1,
];

/// OkHttp 的 ALPN 顺序：h2 优先，其次 http/1.1。
pub const ALPN_PROTOCOLS: &[&[u8]] = &[b"h2", b"http/1.1"];

/// 按 Conscrypt 的顺序重排 rustls 的 provider。
///
/// 只重排、不新增：rustls 支持的套件恰好就是这 9 套，所以结果是「与 Conscrypt
/// 逐项相等」，而不是「Conscrypt 的列表 + rustls 的尾巴」。多出来的尾巴会让
/// cipher 段多几项，JA3 反而更不像。
fn provider() -> rustls::crypto::CryptoProvider {
    let base = rustls::crypto::ring::default_provider();
    let cipher_suites: Vec<rustls::SupportedCipherSuite> = CONSCRYPT_CIPHER_ORDER
        .iter()
        .filter_map(|want| base.cipher_suites.iter().copied().find(|s| s.suite() == *want))
        .collect();
    let kx_groups: Vec<&'static dyn rustls::crypto::SupportedKxGroup> = CONSCRYPT_GROUP_ORDER
        .iter()
        .filter_map(|want| base.kx_groups.iter().copied().find(|g| g.name() == *want))
        .collect();
    rustls::crypto::CryptoProvider {
        cipher_suites,
        kx_groups,
        ..base
    }
}

/// 构造给 reqwest 用的 rustls 配置（ALPN 由这里设，reqwest 收到 preconfigured 配置后不再改它）。
pub fn client_config() -> Result<rustls::ClientConfig, String> {
    let provider = provider();
    // 任何一项为空都说明 rustls 的默认表变了（版本升级），此时**必须报错**而不是
    // 静默退回默认顺序：悄悄用回 rustls 自己的顺序，比一开始就不配置更难发现。
    if provider.cipher_suites.len() != CONSCRYPT_CIPHER_ORDER.len() {
        return Err(format!(
            "密码套件对齐失败：期望 {} 套，rustls 只提供 {} 套",
            CONSCRYPT_CIPHER_ORDER.len(),
            provider.cipher_suites.len()
        ));
    }
    if provider.kx_groups.len() != CONSCRYPT_GROUP_ORDER.len() {
        return Err(format!(
            "曲线组对齐失败：期望 {} 组，rustls 只提供 {} 组",
            CONSCRYPT_GROUP_ORDER.len(),
            provider.kx_groups.len()
        ));
    }

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if roots.is_empty() {
        return Err("内置根证书为空，无法建立 TLS".into());
    }

    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| format!("TLS 版本配置失败：{e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = ALPN_PROTOCOLS.iter().map(|p| p.to_vec()).collect();
    // 与 BoringSSL 一致：不做 0-RTT。rustls 默认也是关的，这里显式写出，
    // 免得哪天默认变了又多出一条 OkHttp 不会有的行为。
    config.enable_early_data = false;
    Ok(config)
}

/// 指纹的一行摘要（日志 / 设备页自检用）。只描述本模块**真的设置过**的东西。
pub fn describe() -> String {
    let ciphers = CONSCRYPT_CIPHER_ORDER.len();
    let groups: Vec<String> = CONSCRYPT_GROUP_ORDER
        .iter()
        .map(|g| format!("{g:?}"))
        .collect();
    let alpn: Vec<String> = ALPN_PROTOCOLS
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    format!(
        "{ciphers} 套密码套件（Conscrypt 顺序）/ 曲线 {}/ ALPN {}",
        groups.join(">"),
        alpn.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cipher 段逐项相等 —— 这是 JA3 里最稳、最容易被网关取到的一段。
    #[test]
    fn cipher_suites_match_conscrypt_order() {
        let provider = provider();
        let got: Vec<String> = provider
            .cipher_suites
            .iter()
            .map(|s| format!("{:?}", s.suite()))
            .collect();
        let want: Vec<String> = CONSCRYPT_CIPHER_ORDER
            .iter()
            .map(|s| format!("{s:?}"))
            .collect();
        assert_eq!(got, want, "密码套件顺序必须与 Conscrypt 一致");
        assert_eq!(got.len(), 9, "Conscrypt 默认是 9 套，多一套少一套都是新特征");
    }

    /// EC 段（命名曲线）顺序。
    #[test]
    fn kx_groups_match_conscrypt_order() {
        let provider = provider();
        let got: Vec<String> = provider
            .kx_groups
            .iter()
            .map(|g| format!("{:?}", g.name()))
            .collect();
        assert_eq!(got, vec!["X25519", "secp256r1", "secp384r1"]);
    }

    /// 配置能真的构造出来，且 ALPN 是 h2 优先。
    #[test]
    fn client_config_sets_h2_first_and_both_versions() {
        let config = client_config().expect("TLS 配置必须可构造");
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        assert!(!config.enable_early_data);
    }

    #[test]
    fn describe_mentions_the_three_aligned_facets() {
        let text = describe();
        assert!(text.contains("9 套"), "{text}");
        assert!(text.contains("X25519>secp256r1>secp384r1"), "{text}");
        assert!(text.contains("h2,http/1.1"), "{text}");
    }
}
