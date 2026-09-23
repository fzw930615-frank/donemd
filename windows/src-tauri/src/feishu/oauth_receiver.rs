//! `FeishuOAuthCallbackReceiver.swift` 的移植 — 回环 HTTP 接收器。
//!
//! 飞书开放平台 2023 起只认 http/https 重定向(拒自定义 scheme),
//! 沿 gh / gcloud / rclone 桌面 OAuth 的成熟模式:注册
//! `http://localhost:18127/oauth/callback` 为重定向 URL,登录期间
//! 在该端口起一个手写 `TcpListener`,收到首个请求即应答并拆除。
//!
//! 零新增依赖(不用 tiny_http):HTTP 解析只认请求行 — 回环重定向
//! 恒为单个 GET,任何更多(keep-alive / chunked / body)对一条
//! 5 秒寿命的监听器都是负担。判定逻辑抽成纯函数
//! [`handle_callback`],socket I/O 只是薄壳。

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 默认回环端口 — 开放平台重定向 URL 列表里注册的就是它。
pub const DEFAULT_PORT: u16 = 18127;
/// 默认回调路径。
pub const DEFAULT_PATH: &str = "/oauth/callback";

/// 接收器错误(Swift `ReceiverError`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverError {
    /// 端口被占 — 常见是另一个 Done.md 实例正在登录。
    PortInUse(u16),
    /// accept / read 阶段的传输层错误。
    ListenerFailed(String),
    /// 请求不是合法的单个 GET 回调。
    MalformedRequest,
    /// `state` 与登录时发的不一致 — 疑似 CSRF。
    StateMismatch,
    /// 等待浏览器授权超时。
    TimedOut,
    /// 外部取消。
    Cancelled,
}

impl std::fmt::Display for ReceiverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReceiverError::PortInUse(port) => write!(
                f,
                "回环端口 {port} 已被占用——请关闭占用它的程序(或另一个正在登录的 Done.md)后重试"
            ),
            ReceiverError::ListenerFailed(detail) => write!(f, "回调监听失败:{detail}"),
            ReceiverError::MalformedRequest => write!(f, "回调请求格式非法"),
            ReceiverError::StateMismatch => write!(f, "state 校验失败(疑似 CSRF),请从 Done.md 重新发起登录"),
            ReceiverError::TimedOut => write!(f, "等待浏览器授权超时"),
            ReceiverError::Cancelled => write!(f, "已取消"),
        }
    }
}

impl std::error::Error for ReceiverError {}

// MARK: - 纯函数(请求解析 / 应答构造 / 判定)

/// 从原始 HTTP/1.1 请求里抠出请求行 URL。回调恒为单个 GET — 其余
/// (HEAD 探测、本机杂服务的 CORS 预检)返回 `None` 走 400。
pub fn parse_request_line_url(request: &str, host: &str, port: u16) -> Option<String> {
    let first_line = request.split("\r\n").next()?;
    let mut parts = first_line.split(' ').filter(|s| !s.is_empty());
    let method = parts.next()?;
    if method != "GET" {
        return None;
    }
    let path_and_query = parts.next()?;
    Some(format!("http://{host}:{port}{path_and_query}"))
}

/// URL 的 path 部分(去 scheme://authority 与 query)。回调 URL 恒为
/// 绝对形式(`http://host:port/path?query`),路径从 authority 后第
/// 一个 `/` 起;authority 后无 `/` 视作根路径 `/`。
pub fn url_path(url: &str) -> &str {
    let no_query = match url.split_once('?') {
        Some((before, _)) => before,
        None => url,
    };
    match no_query.find("://") {
        Some(idx) => {
            let authority_and_path = &no_query[idx + 3..];
            match authority_and_path.find('/') {
                Some(slash) => &authority_and_path[slash..],
                None => "/",
            }
        }
        None => no_query,
    }
}

/// 从 URL query 里取参数值(百分号解码 + `+` 视为空格)。oauth 模块
/// 也用它从回调 URL 里抠 `code`。
pub fn url_query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if percent_decode(k) == name {
            return Some(percent_decode(v));
        }
    }
    None
}

/// 极简百分号解码(`%XX` + `+`→空格)。回调里的 code/state 都是
/// URL 安全字符,这里只需正确,无需完整 RFC 3986。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 单条回调的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackOutcome {
    /// state 匹配 — 携带完整回调 URL 交付调用方。
    Success(String),
    /// 请求行非法 / 路径不对。
    Malformed,
    /// state 不匹配 — 疑似 CSRF。
    StateMismatch,
}

/// 构造极简 HTTP 应答(对齐 Swift `sendResponse`:text/html、
/// Content-Length、Connection: close)。
pub fn build_response(status: u16, body: &str) -> String {
    let status_text = if status == 200 { "OK" } else { "Bad Request" };
    format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// 一条回调的完整判定(纯函数,测试主战场):解析请求行 → 校验
/// 路径与 state → 产出应答字节与结果。`expected_state` 在接收器内
/// 部校验,成功页可以直接向用户点破 CSRF 异常,再把 URL 交给调用方。
pub fn handle_callback(
    raw_request: &str,
    expected_state: &str,
    host: &str,
    port: u16,
    path: &str,
) -> (CallbackOutcome, String) {
    let Some(url) = parse_request_line_url(raw_request, host, port) else {
        return (
            CallbackOutcome::Malformed,
            build_response(400, "<h1>Bad Request</h1>"),
        );
    };
    if url_path(&url) != path {
        return (
            CallbackOutcome::Malformed,
            build_response(400, "<h1>Bad Request</h1>"),
        );
    }
    let returned_state = url_query_param(&url, "state");
    if returned_state.as_deref() != Some(expected_state) {
        return (
            CallbackOutcome::StateMismatch,
            build_response(
                400,
                "<h1>State mismatch</h1><p>Possible CSRF — please retry login from Done.md.</p>",
            ),
        );
    }
    (
        CallbackOutcome::Success(url),
        build_response(200, "<h1>登录成功</h1><p>已授权 Done.md,可关闭此窗口返回应用。</p>"),
    )
}

// MARK: - 回环接收器

/// 登录期间的回环监听器。`await_callback` 绑定
/// `127.0.0.1:port`(只听回环 — 攻击面收窄到「同用户代码」,与
/// 凭据管理器同一条信任边界),等一条 GET,应答后即拆。
pub struct FeishuLoopbackReceiver {
    pub port: u16,
    pub path: String,
}

impl FeishuLoopbackReceiver {
    pub fn new() -> Self {
        Self {
            port: DEFAULT_PORT,
            path: DEFAULT_PATH.to_string(),
        }
    }

    /// 嵌进 authorize URL 的重定向 URI — 必须与开放平台
    /// 「重定向 URL」列表逐字一致。
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}{}", self.port, self.path)
    }

    /// 等浏览器打到重定向 URI。返回完整 URL(调用方抠 `code`),
    /// 超时 / 传输错误 / state 不匹配抛 [`ReceiverError`]。
    pub async fn await_callback(
        &self,
        expected_state: &str,
        timeout: Duration,
    ) -> Result<String, ReceiverError> {
        // 绑不上就是端口被占(另一个登录实例 / 未知占用者),中文
        // 报错直接可读。
        let listener = TcpListener::bind(("127.0.0.1", self.port))
            .await
            .map_err(|_| ReceiverError::PortInUse(self.port))?;

        let accept_and_answer = async {
            let (mut socket, _) = listener
                .accept()
                .await
                .map_err(|e| ReceiverError::ListenerFailed(e.to_string()))?;
            // 读到请求头结束(或 4096 上限,对齐 Swift 的
            // maximumLength: 4096)。GET 无 body,头齐即可应答。
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = socket
                    .read(&mut chunk)
                    .await
                    .map_err(|e| ReceiverError::ListenerFailed(e.to_string()))?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() >= 4096 || buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let raw = String::from_utf8_lossy(&buf);
            let (outcome, response) =
                handle_callback(&raw, expected_state, "localhost", self.port, &self.path);
            // 应答在分离任务里送达 + 优雅关闭:写完 → FIN → 排干对端
            // 到 EOF 再 drop。立即 drop 会让 Windows 对未 ACK 数据回
            // RST,对端可能丢应答(真机测试教训)。判定不等待送达 —
            // 浏览器慢慢收也不拖住超时语义。
            tokio::spawn(async move {
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
                let mut drain = [0u8; 64];
                loop {
                    match socket.read(&mut drain).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
            });
            match outcome {
                CallbackOutcome::Success(url) => Ok(url),
                CallbackOutcome::Malformed => Err(ReceiverError::MalformedRequest),
                CallbackOutcome::StateMismatch => Err(ReceiverError::StateMismatch),
            }
        };

        tokio::time::timeout(timeout, accept_and_answer)
            .await
            .map_err(|_| ReceiverError::TimedOut)?
    }
}

impl Default for FeishuLoopbackReceiver {
    fn default() -> Self {
        Self::new()
    }
}

// MARK: - 测试(纯函数组 + #[ignore] 真端口往返)

#[cfg(test)]
mod tests {
    use super::*;

    const RAW_GET: &str = "GET /oauth/callback?code=auth_code_xyz&state=STATE1 HTTP/1.1\r\n\
        Host: localhost:18127\r\n\
        User-Agent: Mozilla/5.0\r\n\r\n";

    #[test]
    fn parse_request_line_extracts_get_url() {
        assert_eq!(
            parse_request_line_url(RAW_GET, "localhost", 18127),
            Some("http://localhost:18127/oauth/callback?code=auth_code_xyz&state=STATE1".into())
        );
    }

    #[test]
    fn parse_request_line_rejects_non_get() {
        let post = "POST /oauth/callback?code=c HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(parse_request_line_url(post, "localhost", 18127), None);
    }

    #[test]
    fn parse_request_line_rejects_garbage() {
        assert_eq!(parse_request_line_url("", "localhost", 18127), None);
        assert_eq!(parse_request_line_url("garbage no crlf", "localhost", 18127), None);
        assert_eq!(parse_request_line_url("GET\r\n", "localhost", 18127), None);
    }

    #[test]
    fn url_query_param_decodes_values() {
        let url = "http://localhost:18127/cb?code=abc%2B123&state=%E4%B8%AD&empty=";
        assert_eq!(url_query_param(url, "code"), Some("abc+123".into()));
        assert_eq!(url_query_param(url, "state"), Some("中".into()));
        assert_eq!(url_query_param(url, "empty"), Some("".into()));
        assert_eq!(url_query_param(url, "missing"), None);
        assert_eq!(url_query_param("http://x/y", "code"), None);
    }

    #[test]
    fn url_path_strips_query() {
        assert_eq!(url_path("http://h:1/a/b?x=1"), "/a/b");
        assert_eq!(url_path("http://h:1/a/b"), "/a/b");
    }

    #[test]
    fn handle_callback_success_returns_url_and_login_page() {
        let (outcome, response) = handle_callback(RAW_GET, "STATE1", "localhost", 18127, "/oauth/callback");
        assert_eq!(
            outcome,
            CallbackOutcome::Success(
                "http://localhost:18127/oauth/callback?code=auth_code_xyz&state=STATE1".into()
            )
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("登录成功"));
        // Content-Length 与 UTF-8 字节数一致(中文页)。
        let body = response.split("\r\n\r\n").nth(1).unwrap();
        let len_header = response
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
            .unwrap();
        assert_eq!(len_header.trim_end_matches('\r'), format!("Content-Length: {}", body.len()));
    }

    #[test]
    fn handle_callback_state_mismatch_answers_400() {
        let (outcome, response) = handle_callback(RAW_GET, "OTHER", "localhost", 18127, "/oauth/callback");
        assert_eq!(outcome, CallbackOutcome::StateMismatch);
        assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(response.contains("State mismatch"));
    }

    #[test]
    fn handle_callback_missing_state_is_mismatch() {
        // 无 state 参数 ≠ 匹配(空 Some 不等于 Some(expected))。
        let raw = "GET /oauth/callback?code=c HTTP/1.1\r\n\r\n";
        let (outcome, _) = handle_callback(raw, "S", "localhost", 18127, "/oauth/callback");
        assert_eq!(outcome, CallbackOutcome::StateMismatch);
    }

    #[test]
    fn handle_callback_wrong_path_is_malformed() {
        let raw = "GET /favicon.ico HTTP/1.1\r\n\r\n";
        let (outcome, response) = handle_callback(raw, "S", "localhost", 18127, "/oauth/callback");
        assert_eq!(outcome, CallbackOutcome::Malformed);
        assert!(response.starts_with("HTTP/1.1 400"));
    }

    #[test]
    fn handle_callback_non_get_is_malformed() {
        let raw = "POST /oauth/callback?code=c&state=S HTTP/1.1\r\n\r\n";
        let (outcome, _) = handle_callback(raw, "S", "localhost", 18127, "/oauth/callback");
        assert_eq!(outcome, CallbackOutcome::Malformed);
    }

    #[test]
    fn redirect_uri_matches_registered_form() {
        let receiver = FeishuLoopbackReceiver::new();
        assert_eq!(receiver.redirect_uri(), "http://localhost:18127/oauth/callback");
    }

    /// 真端口往返:起接收器 → 本机 TCP 客户端扮演浏览器 → 校验
    /// 应答与交付的 URL。#[ignore] — 生产端口 18127 在并行测试里
    /// 会撞,留作手动验收(须串行:
    /// `cargo test feishu -- --ignored --test-threads=1`)。
    #[tokio::test]
    #[ignore = "真端口绑定,手动跑:cargo test feishu -- --ignored --test-threads=1"]
    async fn real_port_roundtrip_delivers_callback() {
        let receiver = FeishuLoopbackReceiver::new();
        // receiver move 进 async 块(spawn 要 'static,借局部值不行)。
        let awaiting = tokio::spawn(async move {
            receiver.await_callback("STATE9", Duration::from_secs(5)).await
        });

        // 给监听器一点绑定时间。
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut stream = tokio::net::TcpStream::connect("127.0.0.1:18127")
            .await
            .expect("loopback connect");
        stream
            .write_all(
                b"GET /oauth/callback?code=CODE99&state=STATE9 HTTP/1.1\r\n\
                  Host: localhost:18127\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();

        assert!(response.contains("登录成功"), "{response}");
        let url = awaiting.await.unwrap().expect("callback delivered");
        assert_eq!(
            url,
            "http://localhost:18127/oauth/callback?code=CODE99&state=STATE9"
        );
    }

    /// 真端口 state 不匹配:应答 400 + StateMismatch。
    #[tokio::test]
    #[ignore = "真端口绑定,手动跑:cargo test feishu -- --ignored --test-threads=1"]
    async fn real_port_state_mismatch_answers_400() {
        let receiver = FeishuLoopbackReceiver::new();
        let awaiting = tokio::spawn(async move {
            receiver.await_callback("GOOD", Duration::from_secs(5)).await
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut stream = tokio::net::TcpStream::connect("127.0.0.1:18127")
            .await
            .expect("loopback connect");
        stream
            .write_all(b"GET /oauth/callback?code=c&state=EVIL HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(response.contains("State mismatch"));

        let err = awaiting.await.unwrap().unwrap_err();
        assert_eq!(err, ReceiverError::StateMismatch);
    }
}
