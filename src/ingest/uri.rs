//! 代理节点链接解析：vless / vmess / trojan / ss / hysteria2 / tuic。
//!
//! 这里只负责"链接文本 -> 领域模型"，不揣测 sing-box 的字段名；字段对齐发生在
//! `gen::singbox`，且必须由 VM 上的 `sing-box check` 实测确认。

use crate::model::node::{
    Node, NodeKind, QuicProtocol, Reality, StreamProtocol, Tls, TlsConfig, Transport,
};
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("链接缺少 :// 协议前缀")]
    MissingScheme,
    #[error("不支持的协议: {0}")]
    UnsupportedScheme(String),
    #[error("无法解析服务器地址: {0}")]
    BadAddress(String),
    #[error("缺少必需字段: {0}")]
    MissingField(&'static str),
    #[error("base64 内容无法解码")]
    BadBase64,
    #[error("传输层类型不支持: {0}")]
    UnsupportedTransport(String),
    #[error("配置不合法: {0}")]
    Invalid(String),
}

/// 解析单条节点链接。
pub fn parse_link(link: &str) -> Result<Node, LinkError> {
    let link = link.trim();
    let (scheme, rest) = link.split_once("://").ok_or(LinkError::MissingScheme)?;
    let node = match scheme.to_ascii_lowercase().as_str() {
        "vless" | "trojan" | "ss" | "shadowsocks" => parse_stream(rest, scheme)?,
        "hysteria2" | "hy2" => parse_hysteria2(rest)?,
        "tuic" => parse_tuic(rest)?,
        "vmess" => parse_vmess(rest)?,
        other => return Err(LinkError::UnsupportedScheme(other.to_string())),
    };
    node.validate().map_err(LinkError::Invalid)?;
    Ok(node)
}

struct Parts {
    userinfo: String,
    server: String,
    port: u16,
    query: Query,
    name: String,
}

fn split_parts(rest: &str) -> Result<Parts, LinkError> {
    let (main, frag) = match rest.split_once('#') {
        Some((m, f)) => (m, percent_decode(f)),
        None => (rest, String::new()),
    };
    let (main, query_str) = match main.split_once('?') {
        Some((a, q)) => (a, q),
        None => (main, ""),
    };
    let (userinfo, hostport) = main
        .rsplit_once('@')
        .ok_or(LinkError::MissingField("服务器地址"))?;
    let (server, port) = split_hostport(hostport)?;
    let name = if frag.is_empty() {
        format!("{server}:{port}")
    } else {
        frag
    };
    Ok(Parts {
        userinfo: userinfo.to_string(),
        server,
        port,
        query: Query::new(query_str),
        name,
    })
}

fn parse_stream(rest: &str, scheme: &str) -> Result<Node, LinkError> {
    let p = split_parts(rest)?;
    let transport = build_transport(&p.query)?;
    let tls = build_tls(&p.query)?;
    let protocol = match scheme.to_ascii_lowercase().as_str() {
        "vless" => StreamProtocol::Vless {
            uuid: p.userinfo.clone(),
            flow: p.query.get(&["flow"]).map(|s| s.to_ascii_lowercase()),
        },
        "trojan" => StreamProtocol::Trojan {
            password: p.userinfo.clone(),
        },
        "ss" | "shadowsocks" => {
            let (method, password) = decode_ss_userinfo(&p.userinfo)?;
            StreamProtocol::Shadowsocks { method, password }
        }
        other => return Err(LinkError::UnsupportedScheme(other.to_string())),
    };
    Ok(Node {
        name: p.name,
        server: p.server,
        port: p.port,
        kind: NodeKind::Stream {
            protocol,
            transport,
            tls,
        },
    })
}

fn parse_hysteria2(rest: &str) -> Result<Node, LinkError> {
    let p = split_parts(rest)?;
    let mbps = |keys: &[&str]| p.query.get(keys).and_then(|s| s.parse::<u32>().ok());
    Ok(Node {
        name: p.name,
        server: p.server,
        port: p.port,
        kind: NodeKind::Quic {
            protocol: QuicProtocol::Hysteria2 {
                password: p.userinfo.clone(),
                obfs: p.query.get(&["obfs"]).map(String::from),
                obfs_password: p
                    .query
                    .get(&["obfs-password", "obfs_password"])
                    .map(String::from),
                disable_path_mtu_discovery: p.query.flag(&[
                    "disable_mtu_discovery",
                    "disable_path_mtu_discovery",
                    "mci",
                ]),
                up_mbps: mbps(&["up_mbps", "up"]),
                down_mbps: mbps(&["down_mbps", "down"]),
                tls: build_tls(&p.query)?,
            },
        },
    })
}

fn parse_tuic(rest: &str) -> Result<Node, LinkError> {
    let p = split_parts(rest)?;
    let (uuid, password) = p
        .userinfo
        .split_once(':')
        .ok_or(LinkError::MissingField("tuic 的 uuid:password"))?;
    let alpn = p
        .query
        .get(&["alpn"])
        .map(split_alpn)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["h3".into()]);
    Ok(Node {
        name: p.name,
        server: p.server,
        port: p.port,
        kind: NodeKind::Quic {
            protocol: QuicProtocol::Tuic {
                uuid: uuid.to_string(),
                password: password.to_string(),
                alpn,
                congestion_control: p.query.get(&["cc", "congestion_control"]).map(String::from),
                udp_relay_mode: p.query.get(&["udp_relay_mode"]).map(String::from),
                udp_over_stream: p.query.flag(&["udp_over_stream"]),
                zero_rtt_handshake: p.query.flag(&["0rtt", "zero_rtt_handshake"]),
                tls: build_tls(&p.query)?,
            },
        },
    })
}

fn parse_vmess(rest: &str) -> Result<Node, LinkError> {
    let decoded = decode_b64(rest).ok_or(LinkError::BadBase64)?;
    let text = String::from_utf8(decoded).map_err(|_| LinkError::BadBase64)?;
    let text = if text.trim_start().starts_with('{') {
        text
    } else {
        // 双重 base64 编码的 vmess 在旧订阅里仍能见到
        let again = decode_b64(text.trim()).ok_or(LinkError::BadBase64)?;
        String::from_utf8(again).map_err(|_| LinkError::BadBase64)?
    };
    let v: Value = serde_json::from_str(&text).map_err(|_| LinkError::BadBase64)?;

    let server = json_str(&v, "add").ok_or(LinkError::MissingField("add"))?;
    let port: u16 = json_str(&v, "port")
        .ok_or(LinkError::MissingField("port"))?
        .parse()
        .map_err(|_| LinkError::BadAddress(format!("{server} 端口非法")))?;
    let uuid = json_str(&v, "id").ok_or(LinkError::MissingField("id"))?;
    let alter_id = json_str(&v, "aid").unwrap_or_else(|| "0".into());
    let name = json_str(&v, "ps")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{server}:{port}"));

    // vmess 的 `security` 是消息加密算法，`tls` 才是是否套 TLS——两个语义不能混用同一个键
    let cipher = json_str(&v, "security")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "auto".into());

    let mut fields: Vec<(String, String)> = Vec::new();
    for (k, keys) in [
        ("security", &["tls"][..]),
        ("sni", &["sni"][..]),
        ("alpn", &["alpn"][..]),
        ("type", &["net", "type"][..]),
        ("path", &["path"][..]),
        ("host", &["host"][..]),
        ("fp", &["fp", "fingerprint"][..]),
    ] {
        for key in keys {
            if let Some(val) = json_str(&v, key) {
                if val.is_empty() {
                    continue;
                }
                // vmess 里 tls=none / security=none 很常见，不能当成"启用 TLS"的信号
                if k == "security" && matches!(val.as_str(), "none" | "auto" | "plain" | "0") {
                    break;
                }
                fields.push((k.to_string(), val));
                break;
            }
        }
    }
    let query = Query(fields);
    let transport = build_transport(&query)?;
    let tls = build_tls(&query)?;

    Ok(Node {
        name,
        server,
        port,
        kind: NodeKind::Stream {
            protocol: StreamProtocol::Vmess {
                uuid,
                alter_id,
                security: cipher,
            },
            transport,
            tls,
        },
    })
}

fn decode_ss_userinfo(raw: &str) -> Result<(String, String), LinkError> {
    // ss://base64(method:password)@host:port —— 也容忍明文 method:password
    let text = match decode_b64(raw) {
        Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        None => raw.to_string(),
    };
    let (method, password) = text
        .split_once(':')
        .ok_or(LinkError::MissingField("shadowsocks 加密方式"))?;
    Ok((method.to_string(), password.to_string()))
}

fn build_transport(q: &Query) -> Result<Transport, LinkError> {
    let kind = q
        .get(&["type", "network"])
        .unwrap_or("tcp")
        .to_ascii_lowercase();
    let path = q.get(&["path"]).unwrap_or("/");
    let host = q.get(&["host"]).map(String::from);
    Ok(match kind.as_str() {
        "" | "tcp" | "none" | "raw" => Transport::Tcp,
        "ws" | "websocket" => Transport::Ws {
            path: path.to_string(),
            host,
            headers: Vec::new(),
        },
        "grpc" => Transport::Grpc {
            service_name: q.get(&["servicename", "service_name"]).map(String::from),
            idle_timeout: None,
        },
        "h2" => Transport::H2 {
            path: path.to_string(),
            host,
        },
        "httpupgrade" => Transport::HttpUpgrade {
            path: path.to_string(),
            host,
            headers: Vec::new(),
        },
        other => return Err(LinkError::UnsupportedTransport(other.to_string())),
    })
}

fn build_tls(q: &Query) -> Result<Tls, LinkError> {
    let security = q.get(&["security"]).unwrap_or("").to_ascii_lowercase();
    let sni = q.get(&["sni", "peer"]).map(String::from);
    let insecure = q.flag(&["allowinsecure", "insecure", "skip_cert_verify"]);
    let alpn = q.get(&["alpn"]).map(split_alpn).unwrap_or_default();
    // reality 链接里 uTLS 指纹的参数名几乎都是 fp，不是 fingerprint
    let fingerprint = q
        .get(&["fp", "fingerprint", "utlsfingerprint"])
        .map(String::from);
    let ech = q.get(&["ech", "echconfig"]).map(String::from);

    let enabled = matches!(security.as_str(), "tls" | "reality")
        || sni.is_some()
        || insecure
        || !alpn.is_empty();
    if !enabled {
        return Ok(Tls::Disabled);
    }

    let reality = if security == "reality" {
        Some(Reality {
            public_key: q
                .get(&["pbk", "publickey", "reality_pubkey"])
                .ok_or(LinkError::MissingField("reality 公钥 pbk"))?
                .to_string(),
            short_id: q
                .get(&["sid", "shortid", "short_id"])
                .unwrap_or("")
                .to_string(),
        })
    } else {
        None
    };

    Ok(Tls::Tls(TlsConfig {
        sni,
        alpn,
        insecure,
        fingerprint,
        reality,
        ech,
    }))
}

fn split_hostport(hp: &str) -> Result<(String, u16), LinkError> {
    // `hysteria2://pw@host:443/?insecure=false#名字` 这种端口后带斜杠的写法在真实订阅里很常见，
    // 不剥掉就是 `"443/".parse::<u16>()` 失败，整条节点被静默丢掉。
    let hp = hp.trim_end_matches('/');
    if let Some(inner) = hp.strip_prefix('[') {
        let (host, rest) = inner
            .split_once(']')
            .ok_or_else(|| LinkError::BadAddress(hp.to_string()))?;
        let port = rest
            .strip_prefix(':')
            .ok_or_else(|| LinkError::BadAddress(hp.to_string()))?;
        return Ok((host.to_string(), parse_port(port, hp)?));
    }
    let (host, port) = hp
        .rsplit_once(':')
        .ok_or_else(|| LinkError::BadAddress(hp.to_string()))?;
    Ok((host.to_string(), parse_port(port, hp)?))
}

fn parse_port(raw: &str, whole: &str) -> Result<u16, LinkError> {
    raw.parse()
        .map_err(|_| LinkError::BadAddress(whole.to_string()))
}

fn split_alpn(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn json_str(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub(crate) fn decode_b64(s: &str) -> Option<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    for engine in [&URL_SAFE_NO_PAD, &URL_SAFE, &STANDARD_NO_PAD, &STANDARD] {
        if let Ok(bytes) = engine.decode(&cleaned) {
            return Some(bytes);
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone)]
struct Query(Vec<(String, String)>);

impl Query {
    fn new(raw: &str) -> Self {
        Query(
            raw.split('&')
                .filter(|p| !p.is_empty())
                .map(|p| match p.split_once('=') {
                    Some((k, v)) => (percent_decode(k).to_ascii_lowercase(), percent_decode(v)),
                    None => (percent_decode(p).to_ascii_lowercase(), String::new()),
                })
                .collect(),
        )
    }

    fn get(&self, keys: &[&str]) -> Option<&str> {
        keys.iter().find_map(|k| {
            self.0
                .iter()
                .find(|(ak, _)| ak == k)
                .map(|(_, v)| v.as_str())
                .filter(|v| !v.is_empty())
        })
    }

    fn flag(&self, keys: &[&str]) -> bool {
        matches!(self.get(keys), Some("1") | Some("true") | Some("TRUE"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hysteria2_link_with_trailing_slash_before_query() {
        // 真实订阅（2026-09-29）里 hy2 一律写成 `host:443/?...`，端口带尾斜杠
        let n = parse_link(
            "hysteria2://p@ss@aws-hy1.example.xyz:443/?insecure=false&sni=dl.example.com&pinSHA256=5343bfcb1c5db62d#香港专线01",
        )
        .unwrap();
        assert_eq!(n.server, "aws-hy1.example.xyz");
        assert_eq!(n.port, 443);
        assert_eq!(n.name, "香港专线01");
        let NodeKind::Quic {
            protocol: QuicProtocol::Hysteria2 { password, tls, .. },
        } = &n.kind
        else {
            panic!("不是 hysteria2: {:?}", n.kind);
        };
        assert_eq!(password, "p@ss", "密码里的 @ 不能把地址切错");
        let Tls::Tls(tls) = tls else {
            panic!("有 sni 就该启用 TLS");
        };
        assert_eq!(tls.sni.as_deref(), Some("dl.example.com"));
        assert!(!tls.insecure, "insecure=false 不能被当成真");
    }

    #[test]
    fn vless_reality() {
        let n = parse_link(
            "vless://11111111-2222-3333-4444-555555555555@example.com:8443?encryption=none&security=reality&sni=www.microsoft.com&pbk=ABC&sid=6ba85179&fp=chrome&flow=xtls-rprx-vision&type=tcp#东京-01",
        )
        .unwrap();
        assert_eq!(n.name, "东京-01");
        assert_eq!(n.server, "example.com");
        assert_eq!(n.port, 8443);
        match &n.kind {
            NodeKind::Stream { protocol, tls, .. } => {
                assert!(
                    matches!(protocol, StreamProtocol::Vless { uuid, flow: Some(flow) }
                        if uuid.starts_with("11111111") && flow == "xtls-rprx-vision")
                );
                match tls {
                    Tls::Tls(c) => {
                        assert_eq!(c.sni.as_deref(), Some("www.microsoft.com"));
                        assert_eq!(c.fingerprint.as_deref(), Some("chrome"));
                        let r = c.reality.as_ref().unwrap();
                        assert_eq!(r.public_key, "ABC");
                        assert_eq!(r.short_id, "6ba85179");
                    }
                    other => panic!("期望 tls，实际 {other:?}"),
                }
            }
            other => panic!("期望 stream，实际 {other:?}"),
        }
    }

    #[test]
    fn trojan_websocket_with_percent_encoded_path() {
        let n = parse_link(
            "trojan://pass123@1.2.3.4:443?allowInsecure=1&sni=a.com&type=ws&path=%2Fws%3Fk%3D1#HK",
        )
        .unwrap();
        assert_eq!(n.port, 443);
        match &n.kind {
            NodeKind::Stream { transport, tls, .. } => {
                assert!(matches!(transport, Transport::Ws { path, .. } if path == "/ws?k=1"));
                assert!(matches!(tls, Tls::Tls(c) if c.insecure));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ss_sip002_and_plain_forms_agree() {
        let packed = {
            let mut s = String::from("ss://");
            s.push_str(&URL_SAFE_NO_PAD.encode("chacha20-ietf-poly1305:secret@pass"));
            s.push_str("@1.2.3.4:8388#A");
            s
        };
        let n = parse_link(&packed).unwrap();
        match &n.kind {
            NodeKind::Stream { protocol, .. } => assert!(
                matches!(protocol, StreamProtocol::Shadowsocks { method, password }
                    if method == "chacha20-ietf-poly1305" && password == "secret@pass")
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn vmess_base64_json() {
        let json = serde_json::json!({
            "v":"2","ps":"US-2","add":"a.example.com","port":"443",
            "id":"99999999-8888-7777-6666-555555555555","aid":"1","net":"ws",
            "path":"/x","host":"a.example.com","tls":"tls","sni":"a.example.com"
        });
        let link = format!("vmess://{}", STANDARD.encode(json.to_string()));
        let n = parse_link(&link).unwrap();
        assert_eq!(n.name, "US-2");
        match &n.kind {
            NodeKind::Stream {
                protocol,
                transport,
                tls,
            } => {
                assert!(
                    matches!(protocol, StreamProtocol::Vmess { alter_id, .. } if alter_id == "1")
                );
                assert!(matches!(transport, Transport::Ws { path, .. } if path == "/x"));
                assert!(matches!(tls, Tls::Tls(c) if c.sni.as_deref() == Some("a.example.com")));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hysteria2_and_tuic_are_quic_and_ignore_transport() {
        let h = parse_link("hysteria2://p@ss@c.example.com:443?sni=c.example.com&insecure=1&obfs=salamander&obfs-password=pp#HY2").unwrap();
        match &h.kind {
            NodeKind::Quic { protocol } => match protocol {
                QuicProtocol::Hysteria2 {
                    obfs,
                    obfs_password,
                    tls,
                    ..
                } => {
                    assert_eq!(obfs.as_deref(), Some("salamander"));
                    assert_eq!(obfs_password.as_deref(), Some("pp"));
                    assert!(matches!(tls, Tls::Tls(_)));
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        let t = parse_link("tuic://uuid-1:pw@d.example.com:6001?congestion_control=bbr&udp_relay_mode=native&alpn=h3&sni=d.example.com#TUIC").unwrap();
        assert!(matches!(
            &t.kind,
            NodeKind::Quic { protocol: QuicProtocol::Tuic { congestion_control, .. } }
                if congestion_control.as_deref() == Some("bbr")
        ));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(
            parse_link("no scheme here"),
            Err(LinkError::MissingScheme)
        ));
        assert!(matches!(
            parse_link("snell://x@1.2.3.4:443"),
            Err(LinkError::UnsupportedScheme(_))
        ));
        assert!(matches!(
            parse_link("vless://uuid@1.2.3.4:notaport"),
            Err(LinkError::BadAddress(_))
        ));
        assert!(matches!(
            parse_link("vless://uuid@1.2.3.4:443?security=reality&type=ws"),
            Err(LinkError::MissingField("reality 公钥 pbk"))
        ));
        assert!(matches!(
            parse_link("vless://uuid@1.2.3.4:443?type=hysteria"),
            Err(LinkError::UnsupportedTransport(_))
        ));
    }

    #[test]
    fn ipv6_literal_host() {
        let n = parse_link("trojan://pw@[2001:db8::1]:443?sni=a.com#V6").unwrap();
        assert_eq!(n.server, "2001:db8::1");
        assert_eq!(n.port, 443);
    }
}
