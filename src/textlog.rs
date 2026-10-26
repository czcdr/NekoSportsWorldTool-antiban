//! 日志文本处理：着色前缀剥离与关键词归类，GUI/CLI 共用。

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LogKind {
    Plain,
    Ok,
    Err,
    Warn,
}

/// 归类并剥离前缀：入站 √/×/⚠ 仅用于着色，不进入展示文本。
pub fn classify(t: &str) -> (LogKind, &str) {
    if let Some(rest) = t.strip_prefix('×') {
        return (LogKind::Err, rest.trim_start());
    }
    if let Some(rest) = t.strip_prefix('⚠') {
        return (LogKind::Warn, rest.trim_start());
    }
    if let Some(rest) = t.strip_prefix('√') {
        return (LogKind::Ok, rest.trim_start());
    }
    if t.contains("失败") || t.contains("错误") {
        return (LogKind::Err, t);
    }
    if t.contains("成功") {
        return (LogKind::Ok, t);
    }
    (LogKind::Plain, t)
}

/// 只取展示文本（CLI 输出用）。
pub fn clean(t: &str) -> &str {
    classify(t).1
}

/// 按字符截断（字节切片会劈开多字节字符导致 panic）。
pub fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 敏感值保留前后各 4 个字符，中间打码；不超过 8 个字符时全部掩码。
pub fn mask(v: &str) -> String {
    let chars: Vec<char> = v.chars().collect();
    if chars.len() <= 8 {
        return "****".into();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

/// 把 URL 里的 userinfo（`user:pass@`）替换为 `***`，其余部分原样保留。
///
/// 出口代理地址常带认证段（`socks5://user:pass@host:port`），任何把它打印到
/// 日志/错误框的路径都等于把代理口令写进终端历史与截图。
pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let rest_start = scheme_end + 3;
    let rest = &url[rest_start..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    let mut out = String::with_capacity(url.len());
    out.push_str(&url[..rest_start]);
    out.push_str("***@");
    out.push_str(&authority[at + 1..]);
    out.push_str(&rest[authority_end..]);
    out
}

/// 在一段任意文本里把内嵌 URL 的凭据打码（网络错误信息常把完整 URL 拼进去）。
pub fn redact_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(pos) = rest.find("://") else {
            out.push_str(rest);
            break;
        };
        // 回溯到 scheme 起点：把前面连续的 scheme 字符（字母数字/+-.）算进来。
        //
        // 必须用 `char_indices`：`rfind` 返回的是**字节**下标，直接 `+1` 会落到
        // 多字节字符内部（"错误http://…" 中 '误' 占 3..6 字节，+1 = 4 不是字符边界），
        // 随后切片 panic —— 错误处理路径自己崩溃，会把可恢复的网络错误变成进程退出。
        let scheme_start = rest[..pos]
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        out.push_str(&rest[..scheme_start]);
        let tail = &rest[scheme_start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '>'))
            .unwrap_or(tail.len());
        let candidate = &tail[..end];
        out.push_str(&redact_url(candidate));
        rest = &tail[end..];
    }
    out
}

/// 把「一整段不确定内容的响应开头」压成可以安全打印的摘要。
///
/// 解密失败的原始响应可能是网关报错页、明文 JSON、甚至一段带手机号/令牌的
/// 调试输出 —— 这类内容一旦拼进错误串就会进终端回滚区、截图和 CI 日志。
/// 三步处理：按字符截断（不劈多字节），能当 JSON 解析就走结构化脱敏，
/// 再统一遮掉内嵌 URL 凭据、11 位手机号与超长令牌串。
pub fn redact_snippet(raw: &str, n: usize) -> String {
    let cut: String = raw.chars().take(n).collect();
    let text = match serde_json::from_str::<serde_json::Value>(&cut) {
        Ok(v) => redact_json(&v).to_string(),
        Err(_) => redact_text(&cut),
    };
    mask_tokens(&text)
}

/// 遮掉手机号（1[3-9]xxxxxxxxx）与长度 ≥40 的字母数字串（token / 会话 id / 摘要）。
fn mask_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            token.push(ch);
        } else {
            flush_token(&mut token, &mut out);
            out.push(ch);
        }
    }
    flush_token(&mut token, &mut out);
    out
}

fn flush_token(token: &mut String, out: &mut String) {
    if token.is_empty() {
        return;
    }
    let t = std::mem::take(token);
    let bytes = t.as_bytes();
    let all_digits = t.chars().all(|c| c.is_ascii_digit());
    if t.len() == 11 && all_digits && bytes[0] == b'1' && (b'3'..=b'9').contains(&bytes[1]) {
        out.push_str(&format!("{}****{}", &t[..3], &t[7..]));
    } else if t.len() >= 40 && t.chars().all(|c| c.is_ascii_alphanumeric()) {
        out.push_str(&mask(&t));
    } else {
        out.push_str(&t);
    }
}

/// 递归掩码敏感键的字符串值，键名忽略大小写；保留原业务数据。
pub fn redact_json(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    // 登录会话、GT4 验证及常见鉴权响应中的凭证字段。
    const SENSITIVE_KEYS: &[&str] = &[
        "token",
        "password",
        "passToken",
        "access_token",
        "refresh_token",
        "authorization",
        "secret",
    ];
    match v {
        Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(key, value)| {
                    let redacted = match value.as_str() {
                        Some(s) if SENSITIVE_KEYS.iter().any(|k| key.eq_ignore_ascii_case(k)) => {
                            Value::String(mask(s))
                        }
                        _ => redact_json(value),
                    };
                    (key.clone(), redacted)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_json).collect()),
        _ => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_mask_long_and_multibyte() {
        assert_eq!(mask("abcdefghijkl"), "abcd****ijkl");
        assert_eq!(mask("123456789"), "1234****6789");
        assert_eq!(mask("中文字符测试多字节"), "中文字符****试多字节");
        assert_eq!(mask("🦀🐱😀🎉abcdef"), "🦀🐱😀🎉****cdef");
    }

    #[test]
    fn test_mask_short_and_empty() {
        for value in ["", "a", "1234567", "12345678", "中文字符测试"] {
            assert_eq!(mask(value), "****");
        }
    }

    #[test]
    fn test_redact_json_sensitive_keys_case_insensitive() {
        for key in [
            "Token",
            "PASSWORD",
            "PassToken",
            "ACCESS_TOKEN",
            "refresh_token",
            "Authorization",
            "secret",
        ] {
            let value = json!({(key): "abcdefghijkl"});
            assert_eq!(redact_json(&value), json!({(key): "abcd****ijkl"}));
            assert_eq!(value[key], "abcdefghijkl");
        }
    }

    #[test]
    fn test_redact_json_nested_objects_and_arrays() {
        let value = json!({"data": {"token": "short", "items": [
            {"password": "abcdefghijkl"}, [{"secret": "12345678"}]],
            "authorization": {"token": "short"}}});
        let expected = json!({"data": {"token": "****", "items": [
            {"password": "abcd****ijkl"}, [{"secret": "****"}]],
            "authorization": {"token": "****"}}});
        assert_eq!(redact_json(&value), expected);
    }

    #[test]
    fn test_redact_json_preserves_other_values() {
        let value = json!({"message": "成功", "uid": 123, "token": 42,
            "password": null, "secret": false, "items": ["plain", 7, null, true]});
        assert_eq!(redact_json(&value), value);
        for value in [json!("plain"), json!(42), json!(null), json!(true)] {
            assert_eq!(redact_json(&value), value);
        }
    }

    /// 多字节字符中间截断不允许 panic。
    #[test]
    fn test_truncate_multibyte() {
        let s = "中文字符串测试";
        for n in 0..=s.len() + 4 {
            let out = truncate(s, n);
            assert!(out.chars().count() <= n.max(0));
        }
        assert_eq!(truncate("中文abc", 4), "中文ab");
    }

    #[test]
    fn test_redact_url_drops_userinfo() {
        assert_eq!(
            redact_url("socks5://alice:s3cret@10.0.0.1:1080"),
            "socks5://***@10.0.0.1:1080"
        );
        assert_eq!(
            redact_url("http://alice:s3cret@proxy.local:3128/path?x=1"),
            "http://***@proxy.local:3128/path?x=1"
        );
        // 无凭据、无 scheme、超长嵌套 @ 的分支
        assert_eq!(redact_url("socks5://10.0.0.1:1080"), "socks5://10.0.0.1:1080");
        assert_eq!(redact_url("10.0.0.1:1080"), "10.0.0.1:1080");
        assert_eq!(redact_url("http://a@b@c:1080"), "http://***@c:1080");
        assert_eq!(redact_url("http://@host"), "http://***@host");
    }

    #[test]
    fn test_redact_text_masks_embedded_credentials() {
        let raw = "网络错误: http://alice:s3cret@proxy.local:3128/ (proxy) 拒绝连接";
        let out = redact_text(raw);
        assert!(!out.contains("s3cret"), "代理口令必须被打码：{out}");
        assert!(out.contains("http://***@proxy.local:3128/"));
        assert!(out.contains("拒绝连接"));
        // 普通文本与无凭据 URL 保持不变
        assert_eq!(redact_text("普通错误信息"), "普通错误信息");
        assert_eq!(
            redact_text("下载 https://example.test/a.apk 失败"),
            "下载 https://example.test/a.apk 失败"
        );
        // 多个 URL 的文本全部处理
        let two = redact_text("a http://u:p@h1/ b http://u:p@h2/");
        assert!(!two.contains("u:p"), "{two}");
    }

    /// 回归：URL 紧跟多字节字符时，scheme 回溯不能切在字符中间（曾 panic）。
    #[test]
    fn test_redact_text_multibyte_prefix_does_not_panic() {
        for prefix in ["错误", "× 网络错误", "⚠连接失败", "🦀", "a中"] {
            let raw = format!("{prefix}http://user:pass@proxy.local:3128");
            let out = redact_text(&raw);
            assert!(!out.contains("pass"), "凭据必须被打码：{out}");
            assert!(out.starts_with(prefix), "前缀必须原样保留：{out}");
            assert!(out.contains("***@proxy.local:3128"), "{out}");
        }
        // 纯多字节前缀 + 无 URL 的文本原样返回（不 panic）
        assert_eq!(redact_text("中文错误信息"), "中文错误信息");
        assert_eq!(redact_text("🦀x"), "🦀x");
    }

    #[test]
    fn test_redact_snippet_masks_phone_token_and_json() {
        // 手机号
        let phone = redact_snippet("登录失败 user=13800001111 密码错误", 200);
        assert!(!phone.contains("13800001111"), "{phone}");
        assert!(phone.contains("138****1111"), "{phone}");
        // 令牌串（≥40 位）
        let token = "abcdefghijklmnopqrstuvwxyz0123456789ABCD";
        let masked = redact_snippet(&format!("token={token}"), 200);
        assert!(!masked.contains(token), "{masked}");
        assert!(masked.contains("****"), "{masked}");
        // JSON 走结构化脱敏
        let json = redact_snippet(r#"{"token":"abcdefghijkl","msg":"ok"}"#, 200);
        assert!(json.contains("abcd****ijkl"), "{json}");
        assert!(json.contains("\"ok\""), "{json}");
        // 截断按字符，不劈多字节
        let long = "中".repeat(50);
        let cut = redact_snippet(&long, 10);
        assert_eq!(cut.chars().count(), 10);
    }
}
