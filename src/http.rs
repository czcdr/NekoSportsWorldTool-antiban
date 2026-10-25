//! HTTP 客户端门面。
//!
//! 对外保持原先 `ureq` 那套调用形状（`Agent` / `Request::set/call/send_string` /
//! `Response::status/header/into_string/into_reader` / `Error::Status|Transport`），
//! 内部换成 `reqwest`，原因只有一个：**要能协商 HTTP/2 与 ALPN**。
//!
//! 官方 Android 客户端是 OkHttp + Conscrypt，TLS 握手里 ALPN 带 `h2`、之后走 h2
//! 帧格式；而 `ureq` 只有 HTTP/1.1，ALPN 里也只能报 `http/1.1`。网关取一次
//! JA3/JA4 + ALPN 就能把这两类客户端分开，代价极低。
//!
//! 换成 reqwest 之后补齐的差异：
//! - ALPN 协商 `h2` / `http/1.1`（启用 `http2` feature）；
//! - 真正说 h2 帧；
//! - `Accept-Encoding: gzip` 并自动解压（OkHttp 默认就发这个头）；
//! - 保留 socks5 / http 代理支持（出口池依赖）。
//!
//! 仍然不同的：密码套件顺序仍是 rustls 而不是 Conscrypt 的 BoringSSL。要完全一致
//! 得换 BoringSSL 绑定，那是另一个量级的工程；ALPN/h2/头集合这几项已经落地。

use std::io::Read;
use std::time::Duration;

/// 单次响应体上限：更新包下载走同一条路径，超过这个量级直接拒绝而不是把内存吃穿。
const MAX_BODY: u64 = 512 * 1024 * 1024;

/// 与原先 `ureq::Error` 等价的两类错误。
pub enum Error {
    /// 服务端回了 >= 400。body 保留给错误信息用。
    Status(u16, Response),
    /// 传输层失败（DNS/TCP/TLS/超时/代理不可用）。Display 里会带 URL，
    /// 调用方必须先用 `textlog::redact_url` 打码再外显。
    Transport(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Status(code, _) => write!(f, "HTTP {code}"),
            Error::Transport(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Status(code, _) => write!(f, "Status({code})"),
            Error::Transport(msg) => write!(f, "Transport({msg})"),
        }
    }
}

/// 已缓冲的响应。body 一次读全：业务响应都是小 JSON，更新包在 15 MB 量级，
/// 并且读之前有 [`MAX_BODY`] 兜底。
pub struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    /// 惰性正文：ureq 的响应体也是延迟读取的，`download_with_limit` 依赖
    /// 「先看 Content-Length 再决定要不要读」这个语义，所以这里不能提前缓冲。
    body: Body,
}

enum Body {
    Buffered(Vec<u8>),
    Live(Box<reqwest::blocking::Response>),
}

impl Response {
    pub fn status(&self) -> u16 {
        self.status
    }

    /// 大小写不敏感的头查询。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 一次性读成字符串（业务 JSON 用）。超过 `MAX_BODY` 直接报错。
    pub fn into_string(self) -> Result<String, std::io::Error> {
        let mut reader = self.into_reader();
        let mut buf = Vec::new();
        let mut limited = reader.by_ref().take(MAX_BODY + 1);
        limited.read_to_end(&mut buf)?;
        if buf.len() as u64 > MAX_BODY {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("响应体超过上限 {MAX_BODY} 字节"),
            ));
        }
        String::from_utf8(buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// 流式读取。调用方（更新包下载、路网下载）自己控制上限。
    pub fn into_reader(self) -> Box<dyn Read> {
        match self.body {
            Body::Buffered(bytes) => Box::new(std::io::Cursor::new(bytes)),
            Body::Live(resp) => Box::new(resp),
        }
    }

    /// 仅供测试构造假响应。
    #[cfg(test)]
    pub fn new(status: u16, _text: &str, body: &str) -> Result<Self, String> {
        Ok(Self {
            status,
            headers: Vec::new(),
            body: Body::Buffered(body.as_bytes().to_vec()),
        })
    }
}

/// 一次请求的构造器（对应原先 `ureq::Request`）。
pub struct Request {
    inner: Option<reqwest::blocking::RequestBuilder>,
}

impl Request {
    pub fn set(mut self, key: &str, value: &str) -> Self {
        if let Some(rb) = self.inner.take() {
            self.inner = Some(rb.header(key, value));
        }
        self
    }

    pub fn call(self) -> Result<Response, Error> {
        let Some(rb) = self.inner else {
            return Err(Error::Transport("请求构造失败".into()));
        };
        finish(rb)
    }

    pub fn send_string(self, body: &str) -> Result<Response, Error> {
        self.send_bytes(body.as_bytes())
    }

    pub fn send_bytes(self, body: &[u8]) -> Result<Response, Error> {
        let Some(rb) = self.inner else {
            return Err(Error::Transport("请求构造失败".into()));
        };
        finish(rb.body(body.to_vec()))
    }

    /// `application/x-www-form-urlencoded` 表单（学校路网的 Overpass 查询用）。
    pub fn send_form(self, pairs: &[(&str, &str)]) -> Result<Response, Error> {
        let Some(rb) = self.inner else {
            return Err(Error::Transport("请求构造失败".into()));
        };
        finish(rb.form(pairs))
    }
}

/// 发送并在内存里组装响应。
fn finish(rb: reqwest::blocking::RequestBuilder) -> Result<Response, Error> {
    let resp = rb.send().map_err(|e| Error::Transport(e.to_string()))?;
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    // 与 ureq 的行为对齐：>= 400 视为错误。错误体本身很小，读出来给错误信息用；
    // 正常响应保持惰性，交给调用方按自己的上限去读（更新包下载依赖这一点）。
    if status >= 400 {
        let mut body = Vec::new();
        let mut limited = resp.take(MAX_BODY + 1);
        let _ = limited.read_to_end(&mut body);
        return Err(Error::Status(
            status,
            Response {
                status,
                headers,
                body: Body::Buffered(body),
            },
        ));
    }
    Ok(Response {
        status,
        headers,
        body: Body::Live(Box::new(resp)),
    })
}

/// 连接池 + TLS 配置的持有者。`reqwest::blocking::Client` 内部是 Arc，克隆很便宜。
#[derive(Clone)]
pub struct Agent {
    inner: reqwest::blocking::Client,
}

impl Agent {
    /// 构造 agent。`proxy_url` 支持 `http://`、`https://`、`socks5://`、`socks5h://`。
    ///
    /// 代理串非法时返回 `Err`，由调用方决定是否降级直连 —— 降级必须在日志里可见，
    /// 否则会出现"以为在走代理、其实直连"。
    pub fn new(timeout: Duration, proxy_url: Option<&str>) -> Result<Self, String> {
        let mut builder = reqwest::blocking::Client::builder()
            .timeout(timeout)
            // OkHttp 默认跟随重定向；保持一致。
            .redirect(reqwest::redirect::Policy::limited(10))
            // OkHttp 默认发 `Accept-Encoding: gzip` 并自动解压。
            .gzip(true)
            // h2 与 http/1.1 由 ALPN 协商（http2 feature 打开后 rustls 自动带上）。
            .http2_adaptive_window(true);
        if let Some(url) = proxy_url {
            let proxy =
                reqwest::Proxy::all(url).map_err(|e| format!("代理 {url} 配置无效：{e}"))?;
            builder = builder.proxy(proxy);
        }
        builder
            .build()
            .map(|inner| Self { inner })
            .map_err(|e| format!("HTTP 客户端构造失败：{e}"))
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self::new(timeout, None).unwrap_or_else(|e| {
            eprintln!("[net] {e}");
            Self {
                inner: reqwest::blocking::Client::new(),
            }
        })
    }

    pub fn request(&self, method: &str, url: &str) -> Request {
        let method =
            reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
        // 不额外塞 Accept：OkHttp 默认不发这个头，少一个头就少一处差异。
        Request {
            inner: Some(self.inner.request(method, url)),
        }
    }

    pub fn get(&self, url: &str) -> Request {
        self.request("GET", url)
    }

    pub fn post(&self, url: &str) -> Request {
        self.request("POST", url)
    }

    pub fn put(&self, url: &str) -> Request {
        self.request("PUT", url)
    }
}
