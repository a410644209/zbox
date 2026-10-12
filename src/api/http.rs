//! HTTP/1.1 的**线上格式**：把 socket 上的字节读成 [`Request`]，把 [`Response`] 写回字节。
//!
//! 手写而不是引框架，有两个原因：依赖面（`docs/ARCHITECTURE.md` §2.1 的 `--offline` 约定，
//! 本地 registry 里没有任何 HTTP 框架），以及这一层需要支持的只是"一个局域网里的浏览器
//! 对着 `ROUTES` 那张表做 fetch"——表里每一条都不带路径参数，查询串在这一层就剥掉，
//! keep-alive、分块传输、pipelining 一个都不必支持，
//! 每个响应都带 `Connection: close`，反而少了参考实现里那种连接复用带来的边界情况。
//!
//! 几条不是随手写下的解析约定：
//! - **行尾必须是 CRLF**。只用 LF 分隔的请求行在拒收之列：浏览器不会这么发，
//!   而宽容在这里换不来任何兼容，换来的是请求走私的空间。
//! - **不认 `*`、不认绝对形式**（`GET http://host/api/state`）。路径只和 `const ROUTES`
//!   逐字比对，没有任何客户端能给这一层喂路径——§7.2 第一条硬约束在协议层的落点就是这里。
//! - **先看 `Content-Length` 再决定读不读体**。超过 `MAX_BODY` 的直接 413 并断开，
//!   不去收那个大 body；对面因此打不穿内存，也只能占一个连接的量。
//! - **`Expect: 100-continue` 要回一句**，否则对面（curl 一类工具在 body 超过 1 KiB 时会自动加）
//!   会一直等一个永不到来的中间响应。
//! - **每个连接设读超时**。服务是单线程顺序接连接的，一个慢客户端能占住整块面板。

use super::{Method, Request, Response, MAX_BODY};
use std::io::{Read, Write};

/// 请求头上限。五个头加起来远用不到 16 KiB；给这么宽是为了让"头太大"成为一个
/// 明确的拒收，而不是缓冲区截断之后的一个莫名 400。
pub const MAX_HEADER: usize = 16 * 1024;

/// 协议层能给出的错误。它与"应用层错误"分开，是因为 `Response::fail` 的措辞是给人看的，
/// 而这些是给协议守不住的连接看的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpError {
    pub status: u16,
    pub detail: String,
}

impl HttpError {
    fn new(status: u16, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }

    /// 错误也要走同一个响应形状（`{"error": ...}`），面板不需要为协议错误写第二条分支。
    pub fn to_response(&self) -> Response {
        Response::fail(self.status, self.detail.clone())
    }
}

/// 解析出来的请求，字段是 owned 的，所以它能活着走出 socket 这一层，
/// 而 [`Request`] 保持它是一个借用的视图（应用层的测试直接构造后者）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owned {
    pub method: Method,
    pub path: String,
    pub body: Vec<u8>,
    pub token: Option<String>,
    pub if_match: Option<String>,
}

impl Owned {
    /// 借成应用层要的形状。路径、头值与体都只被引用，不复制第二次。
    pub fn request(&self) -> Request<'_> {
        Request {
            method: self.method,
            path: self.path.as_str(),
            body: &self.body,
            token: self.token.as_deref(),
            if_match: self.if_match.as_deref(),
        }
    }
}

/// 请求行的方法 + 路径 + 几个我们认的头 + `Content-Length`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: Method,
    pub path: String,
    pub token: Option<String>,
    pub if_match: Option<String>,
    pub content_length: usize,
    pub expect_continue: bool,
}

/// 只解析头部块（不含体）。拆出来是为了这些规则能脱离 socket 测：CRLF、绝对形式、
/// 重复的 `Content-Length`、方法白名单，每一条都是一个具体的连接形状。
pub fn parse_head(raw: &[u8]) -> Result<Head, HttpError> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| HttpError::new(400, "请求头不是合法的 UTF-8，读不了"))?;
    let Some((request_line, rest)) = text.split_once("\r\n") else {
        return Err(HttpError::new(
            400,
            "请求行没按 CRLF 结束：这一层只收 CRLF 分隔的 HTTP/1.1 请求",
        ));
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(path), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::new(
            400,
            format!("请求行应是 `方法 路径 HTTP/1.x`，收到 {request_line:?}"),
        ));
    };
    if parts.next().is_some() {
        return Err(HttpError::new(
            400,
            format!("请求行多出一段：{request_line:?}"),
        ));
    }
    if !version.starts_with("HTTP/1.") {
        return Err(HttpError::new(
            505,
            format!("只支持 HTTP/1.x，收到 {version:?}"),
        ));
    }
    let method = match method {
        "GET" => Method::Get,
        "PUT" => Method::Put,
        "POST" => Method::Post,
        other => {
            return Err(HttpError::new(
                405,
                format!("方法只认 GET / PUT / POST，收到 {other:?}"),
            ))
        }
    };
    if !path.starts_with('/') {
        // 绝对形式（代理请求）与 `*`（OPTIONS 的常见写法）都从这里掉出去：
        // 这一层不接受任何由客户端给定的路径，一个字节都不接受。
        return Err(HttpError::new(
            400,
            format!("请求 URI 必须以 / 开头（这一层不收绝对形式与 `*`），收到 {path:?}"),
        ));
    }
    if path.contains("://") || path.contains('\\') {
        return Err(HttpError::new(
            400,
            format!("请求 URI 里不许带 schemes 或反斜杠：{path:?}"),
        ));
    }

    // 上面三条校验的都是**原始 request target**，剥查询串放在它们之后：`?` 后面出现
    // schemes、反斜杠或不以 / 开头的东西，照样在进路由之前 400 掉。
    // 查询串本身是不认识的参数而不是坏路径——这一层没有任何端点读它，丢掉才是实话，
    // 而把它留着当路径的一截会让 `/?from=share` 这种地址整个面板打不开
    // （2026-10-11 真浏览器一轮里撞到的：那时页面显示的是 401 的 JSON，不是面板）。
    let path = match path.find('?') {
        Some(at) => &path[..at],
        None => path,
    };

    let mut head = Head {
        method,
        path: path.to_string(),
        token: None,
        if_match: None,
        content_length: 0,
        expect_continue: false,
    };
    // 头名不区分大小写（RFC 9110 §10.1），值里的前后空白不算值的一部分。
    for line in rest.lines() {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::new(
                400,
                format!("请求头这一行没有冒号：{line:?}"),
            ));
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "x-zbox-token" => head.token = Some(value.to_string()),
            "if-match" => head.if_match = Some(value.to_string()),
            "expect" => head.expect_continue = value.eq_ignore_ascii_case("100-continue"),
            "transfer-encoding" => {
                return Err(HttpError::new(
                    501,
                    format!("不支持分块传输（收到 {value:?}）：请带 Content-Length 发完整 body"),
                ))
            }
            "content-length" => {
                let Ok(len) = value.parse::<usize>() else {
                    return Err(HttpError::new(
                        400,
                        format!("Content-Length 不是一个非负整数：{value:?}"),
                    ));
                };
                if head.content_length != 0 && head.content_length != len {
                    // 两个不一致的长度是走私的温床，不是"取大的那个"或"取小的那个"
                    return Err(HttpError::new(
                        400,
                        format!(
                            "出现多个 Content-Length（{} 与 {len}）",
                            head.content_length
                        ),
                    ));
                }
                head.content_length = len;
            }
            // 其余的头（Host / User-Agent / Accept / Cookie / Origin…）一律忽略：
            // 这一层不需要虚拟主机，不需要内容协商，也没有 cookie 会话。
            _ => {}
        }
    }
    Ok(head)
}

/// 读一个完整请求：头部块 -> （按需）100 Continue -> 定长体。
///
/// 需要 `Read + Write` 只为 `Expect: 100-continue` 那一句中间响应；
/// 不这么处理的话，一个超过 1 KiB 的 body 会让 curl 那类客户端干等。
pub fn read_request<S: Read + Write>(stream: &mut S) -> Result<Owned, HttpError> {
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|e| HttpError::new(400, format!("读请求头失败: {e}")))?;
        if read == 0 {
            return Err(HttpError::new(
                400,
                format!(
                    "对面在头部块结束之前就断了（已读 {} 字节，没读到 CRLF CRLF）",
                    buffer.len()
                ),
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_HEADER {
            return Err(HttpError::new(
                413,
                format!("请求头超过 {MAX_HEADER} 字节上限，已断开"),
            ));
        }
        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    // 切掉的是**空行那一个** CRLF，留一个给 parse_head 当请求行的结尾：
    // 一个没有任何头的请求（`GET /api/state HTTP/1.1\r\n\r\n`）也是合法的。
    let head = parse_head(&buffer[..head_end - 2])?;
    if head.content_length > MAX_BODY {
        // 先拒再断：不去收那个大 body，也不给应用层机会解析它。
        return Err(HttpError::new(
            413,
            super::oversize_detail(head.content_length),
        ));
    }
    if head.expect_continue {
        stream
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .map_err(|e| HttpError::new(400, format!("回 100 Continue 失败: {e}")))?;
    }
    let mut body = buffer[head_end..].to_vec();
    if body.len() > head.content_length {
        // 声明比实收少：多出来的字节属于"下一个请求"，而这一层不做 keep-alive。
        // 长度不符不是拿它猜的理由，报错断开是唯一诚实的处置。
        return Err(HttpError::new(
            400,
            format!(
                "声明的 body 是 {} 字节，头部块之后已经收到 {}",
                head.content_length,
                body.len()
            ),
        ));
    }
    body.extend_from_slice(&read_exact_up_to(stream, head.content_length - body.len())?);
    Ok(Owned {
        method: head.method,
        path: head.path,
        body,
        token: head.token,
        if_match: head.if_match,
    })
}

/// 读满 `want` 字节。对面提前断开就是一个错误，不能拿短了的 body 去解析。
fn read_exact_up_to<S: Read>(stream: &mut S, want: usize) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::with_capacity(want.min(MAX_BODY));
    let mut left = want;
    let mut chunk = [0u8; 4096];
    while left > 0 {
        let want_now = left.min(chunk.len());
        let read = stream
            .read(&mut chunk[..want_now])
            .map_err(|e| HttpError::new(400, format!("读请求体失败: {e}")))?;
        if read == 0 {
            return Err(HttpError::new(
                400,
                format!("对面在送满 {want} 字节的 body 之前就断了"),
            ));
        }
        out.extend_from_slice(&chunk[..read]);
        left -= read;
    }
    Ok(out)
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

/// 写一个响应。`Connection: close` 是刻意的：不做 keep-alive 就没有"这个连接还能不能用"
/// 这个状态，也就没有它带来的那一类边界情况。
///
/// 没有 `Set-Cookie`、没有 `Access-Control-Allow-*`：面板与 API 同源，令牌走自定义头，
/// 于是跨源授权和 cookie 会话这两类攻击面在这台机器上不存在。
///
/// `Content-Length` 取的是 `String::len()`，那本来就是**字节**数——这里的措辞值得留一行，
/// 因为回执与静态页都是满屏中文，按字符数写长度会把浏览器读到截断。
pub fn write_response<S: Write>(stream: &mut S, res: &Response) -> std::io::Result<()> {
    let reason = status_text(res.status);
    let text = format!(
        "HTTP/1.1 {} {reason}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        res.status,
        res.content_type,
        res.body.len(),
        res.body
    );
    stream.write_all(text.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn head(text: &str) -> Head {
        parse_head(text.as_bytes()).expect("这条应当解析得过")
    }

    #[test]
    fn a_plain_get_makes_a_request_with_no_token_and_no_body() {
        let got = head("GET /api/state HTTP/1.1\r\nHost: 192.168.219.128:9090\r\n\r\n");
        assert_eq!(got.method, Method::Get);
        assert_eq!(got.path, "/api/state");
        assert_eq!(got.token, None, "没带头就是没鉴权，不给默认值");
        assert_eq!(got.content_length, 0);
        assert!(!got.expect_continue);
    }

    #[test]
    fn header_names_are_case_insensitive_and_values_are_trimmed() {
        let got = head(
            "POST /api/apply HTTP/1.1\r\n\
             X-ZBox-Token: \taa00\r\n\
             IF-MATCH: absent\r\n\
             Content-Length: 0\r\n\r\n",
        );
        assert_eq!(got.token.as_deref(), Some("aa00"), "前后空白不算值");
        assert_eq!(got.if_match.as_deref(), Some("absent"));
    }

    #[test]
    fn a_query_string_is_dropped_but_still_checked() {
        // 面板的静态路由是字面量表，留着 `?...` 当路径的一截就等于整个面板打不开
        for (target, want) in [
            ("GET / HTTP/1.1", "/"),
            ("GET /?v=1 HTTP/1.1", "/"),
            ("GET /? HTTP/1.1", "/"),
            ("GET /?a=1&b=2 HTTP/1.1", "/"),
            ("GET /panel.js?v=1 HTTP/1.1", "/panel.js"),
            ("GET /api/state?etag=absent HTTP/1.1", "/api/state"),
            // 只剥**第一个** `?` 之后的：后面的 `?` 属于查询串
            ("GET /x?a?b HTTP/1.1", "/x"),
        ] {
            assert_eq!(
                head(&format!("{target}\r\n\r\n")).path,
                want,
                "{target:?} 的落点不对"
            );
        }
        // 校验跑在剥之前：查询串里塞 schemes / 反斜杠 / 不以 / 开头，一律还是 400
        for bad in [
            "GET /?u=http://x/api/state HTTP/1.1\r\n\r\n",
            "GET /?p=\\api\\state HTTP/1.1\r\n\r\n",
        ] {
            assert!(
                parse_head(bad.as_bytes()).is_err(),
                "{bad:?} 竟过了：查询串不该是绕过校验的口子"
            );
        }
    }

    #[test]
    fn anything_not_a_plain_origin_path_is_refused() {
        // 绝对形式、`*`、带 scheme 的、带反斜杠的——一律 400，绝不进路由
        for bad in [
            "GET http://x/api/state HTTP/1.1\r\n\r\n",
            "OPTIONS * HTTP/1.1\r\n\r\n",
            "GET api/state HTTP/1.1\r\n\r\n",
            "GET /\\api\\state HTTP/1.1\r\n\r\n",
        ] {
            let err = parse_head(bad.as_bytes()).unwrap_err();
            assert!((400..=505).contains(&err.status), "{bad:?} 竟过了：{err:?}");
        }
    }

    #[test]
    fn lf_only_framing_and_unknown_methods_are_refused() {
        let lf = "GET /api/state HTTP/1.1\n\n";
        assert!(parse_head(lf.as_bytes()).is_err(), "只用 LF 分隔的连接不收");
        let del = parse_head(b"DELETE /api/state HTTP/1.1\r\n\r\n");
        assert_eq!(del.unwrap_err().status, 405);
        assert_eq!(
            parse_head(b"GET /api/state FTP/1.0\r\n\r\n")
                .unwrap_err()
                .status,
            505
        );
    }

    #[test]
    fn conflicting_content_lengths_and_chunking_are_refused() {
        let twice = "PUT /api/state HTTP/1.1\r\nContent-Length: 10\r\ncontent-length: 11\r\n\r\n";
        assert!(parse_head(twice.as_bytes())
            .unwrap_err()
            .detail
            .contains("Content-Length"));
        let chunked = "PUT /api/state HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(
            parse_head(chunked.as_bytes()).unwrap_err().status,
            501,
            "分块传输意味着长度要一边读一边猜，这一层只认定长"
        );
    }

    #[test]
    fn a_request_is_read_end_to_end_from_a_byte_source() {
        let text = r#"{"schema":2}"#;
        let raw = format!(
            "PUT /api/state HTTP/1.1\r\nX-ZBox-Token: dead0000\r\nIf-Match: absent\r\n\
             Content-Length: {}\r\n\r\n{text}",
            text.len()
        );
        let mut cursor = Cursor::new(raw.into_bytes());
        let got = read_request(&mut cursor).expect("这条应当读得完");
        assert_eq!(got.method, Method::Put);
        assert_eq!(got.token.as_deref(), Some("dead0000"));
        assert_eq!(got.if_match.as_deref(), Some("absent"));
        assert_eq!(got.body, text.as_bytes());

        // 借出来的视图与 owned 的那份是同一批字节
        let view = got.request();
        assert_eq!(view.path, "/api/state");
        assert_eq!(view.body, got.body.as_slice());
        assert_eq!(view.token, Some("dead0000"));
    }

    #[test]
    fn a_body_over_the_limit_is_refused_without_being_read() {
        let raw = format!(
            "PUT /api/state HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
            MAX_BODY + 1,
            "x".repeat(64) // 真 body 只给 64 字节：这条要验的是"没去收剩下的"
        );
        let mut cursor = Cursor::new(raw.into_bytes());
        let err = read_request(&mut cursor).unwrap_err();
        assert_eq!(err.status, 413, "{err:?}");
        assert!(err.detail.contains("不是分片"), "{}", err.detail);
    }

    #[test]
    fn a_client_that_disconnects_mid_body_is_an_error_not_a_short_read() {
        let raw = "PUT /api/state HTTP/1.1\r\nContent-Length: 500\r\n\r\nonly a bit";
        let mut cursor = Cursor::new(raw.as_bytes().to_vec());
        let err = read_request(&mut cursor).unwrap_err();
        assert_eq!(err.status, 400, "{err:?}");
        assert!(err.detail.contains("断"), "{}", err.detail);
    }

    #[test]
    fn responses_carry_a_length_and_close_the_connection() {
        let mut out: Vec<u8> = Vec::new();
        write_response(&mut out, &Response::ok(serde_json::json!({"ok": true})))
            .expect("写响应不该失败");
        let text = String::from_utf8(out).expect("响应是 UTF-8");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("Content-Length: "), "{text}");
        assert!(text.contains("Connection: close"), "{text}");
        assert!(
            text.contains("Cache-Control: no-store"),
            "回执里有凭据，不许被浏览器缓存"
        );
        assert!(!text.contains("Set-Cookie"), "{text}");
        // 体只写一次：Content-Length 必须与它逐字节对齐
        let body_at = text.find("\r\n\r\n").unwrap() + 4;
        let declared: usize = text
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(declared, text[body_at..].len(), "{text}");
    }
}
