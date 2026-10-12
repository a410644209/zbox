//! Node 模型 -> sing-box outbound JSON。
//!
//! 字段名逐个对照 sing-box **1.14.2** 的 `option/*.go` struct tag 写死（本地留档：
//! `reference/singbox-1142/`，见 `docs/reference/singbox-1.14.2-schema.md`）。
//! 之所以对齐到 tag 而不是文档：1.14.2 在 `options.go:41` 用了
//! `decoder.DisallowUnknownFields()`，**多写一个键 `sing-box check` 就整体报错**，
//! 而 sing-box 一旦在旁路由上起不来，整条局域网就断，所以宁可少写字段。
//!
//! 手写 JSON 而不引第三方 sing-box crate：配置结构是我们自己的领域模型，生成器需要
//! 完全掌控字段取舍与降级行为。
//!
//! 已按 1.14.2 校正、容易踩的三处（都曾是 1.11/1.12 时代的写法）：
//! - `transport.type` 枚举只有 `http,ws,quic,grpc,httpupgrade`，**没有 `tcp`**，
//!   直连 TCP 必须整个省略 `transport`（`option/v2ray_transport.go:11`）
//! - 客户端 reality 只有 `enabled/public_key/short_id`，**没有 `handshake` 子对象**
//!   （那是服务端 `InboundRealityOptions` 的字段，`option/tls.go:252`）
//! - ws 传输没有 `host` 字段，Host 头必须折叠进 `headers`（`option/v2ray_transport.go:94`）

use crate::model::node::{Node, NodeKind, QuicProtocol, Reality, StreamProtocol, Tls, Transport};
use serde_json::{json, Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum OutboundError {
    #[error("节点 {node} 的 alter_id \"{value}\" 不是 sing-box 接受的数字")]
    BadAlterId { node: String, value: String },
    #[error("节点 {0} 缺少服务器地址")]
    MissingServer(String),
}

pub fn node_outbound(node: &Node) -> Result<Value, OutboundError> {
    if node.server.trim().is_empty() {
        return Err(OutboundError::MissingServer(node.name.clone()));
    }
    let mut o = Map::new();
    o.insert("tag".into(), json!(node.name));
    o.insert("server".into(), json!(node.server));
    o.insert("server_port".into(), json!(node.port));

    match &node.kind {
        NodeKind::Stream {
            protocol,
            transport,
            tls,
        } => {
            let ty = match protocol {
                StreamProtocol::Vless { uuid, flow } => {
                    o.insert("uuid".into(), json!(uuid));
                    if let Some(flow) = flow {
                        o.insert("flow".into(), json!(flow));
                    }
                    "vless"
                }
                StreamProtocol::Vmess {
                    uuid,
                    alter_id,
                    security,
                } => {
                    let aid: u32 =
                        alter_id
                            .trim()
                            .parse()
                            .map_err(|_| OutboundError::BadAlterId {
                                node: node.name.clone(),
                                value: alter_id.clone(),
                            })?;
                    o.insert("uuid".into(), json!(uuid));
                    o.insert("alter_id".into(), json!(aid));
                    // vmess 的 security 无 omitempty，恒写
                    o.insert("security".into(), json!(security));
                    "vmess"
                }
                StreamProtocol::Trojan { password } => {
                    o.insert("password".into(), json!(password));
                    "trojan"
                }
                StreamProtocol::Shadowsocks { method, password } => {
                    o.insert("method".into(), json!(method));
                    o.insert("password".into(), json!(password));
                    "shadowsocks"
                }
            };
            o.insert("type".into(), json!(ty));
            if let Some(value) = transport_json(transport) {
                o.insert("transport".into(), value);
            }
            if let Some(tls_value) = tls_json(tls)? {
                o.insert("tls".into(), tls_value);
            }
        }
        NodeKind::Quic { protocol } => match protocol {
            QuicProtocol::Hysteria2 {
                password,
                obfs,
                obfs_password,
                disable_path_mtu_discovery,
                up_mbps,
                down_mbps,
                tls,
            } => {
                o.insert("type".into(), json!("hysteria2"));
                o.insert("password".into(), json!(password));
                if let (Some(kind), Some(pw)) = (obfs, obfs_password) {
                    o.insert("obfs".into(), json!({"type": kind, "password": pw}));
                }
                if *disable_path_mtu_discovery {
                    o.insert("disable_path_mtu_discovery".into(), json!(true));
                }
                if let Some(up) = up_mbps {
                    o.insert("up_mbps".into(), json!(up));
                }
                if let Some(down) = down_mbps {
                    o.insert("down_mbps".into(), json!(down));
                }
                if let Some(tls_value) = tls_json(tls)? {
                    o.insert("tls".into(), tls_value);
                }
            }
            QuicProtocol::Tuic {
                uuid,
                password,
                alpn,
                congestion_control,
                udp_relay_mode,
                udp_over_stream,
                zero_rtt_handshake,
                tls,
            } => {
                o.insert("type".into(), json!("tuic"));
                o.insert("uuid".into(), json!(uuid));
                o.insert("password".into(), json!(password));
                if let Some(cc) = congestion_control {
                    o.insert("congestion_control".into(), json!(cc));
                }
                if let Some(mode) = udp_relay_mode {
                    o.insert("udp_relay_mode".into(), json!(mode));
                }
                if *udp_over_stream {
                    o.insert("udp_over_stream".into(), json!(true));
                }
                if *zero_rtt_handshake {
                    o.insert("zero_rtt_handshake".into(), json!(true));
                }
                let mut tls_value = match tls_json(tls)? {
                    Some(v) => v.as_object().cloned().unwrap_or_default(),
                    None => Map::new(),
                };
                tls_value.insert("enabled".into(), json!(true));
                if !alpn.is_empty() {
                    tls_value.insert("alpn".into(), json!(alpn));
                }
                o.insert("tls".into(), Value::Object(tls_value));
            }
        },
    }
    Ok(Value::Object(o))
}

/// `None` 表示连 `transport` 键都不写（1.14.2 没有 tcp 传输类型）
fn transport_json(t: &Transport) -> Option<Value> {
    match t {
        Transport::Tcp => None,
        Transport::Ws {
            path,
            host,
            headers,
        } => {
            let mut m = json!({"type": "ws", "path": path});
            let mut all: Map<String, Value> = Map::new();
            for (k, v) in headers {
                all.insert(k.clone(), json!(v));
            }
            if let Some(h) = host {
                all.insert("Host".into(), json!(h));
            }
            if !all.is_empty() {
                m.as_object_mut()
                    .unwrap()
                    .insert("headers".into(), Value::Object(all));
            }
            Some(m)
        }
        Transport::Grpc { service_name, .. } => {
            let mut m = json!({"type": "grpc"});
            if let Some(name) = service_name {
                m.as_object_mut()
                    .unwrap()
                    .insert("service_name".into(), json!(name));
            }
            Some(m)
        }
        // h2 传输在 1.8 改名 http，host 是数组（option.V2RayHTTPOptions.Host 是 Listable）
        Transport::H2 { path, host } => {
            let mut m = json!({"type": "http", "path": path});
            if let Some(h) = host {
                m.as_object_mut().unwrap().insert("host".into(), json!([h]));
            }
            Some(m)
        }
        Transport::HttpUpgrade { path, host, .. } => {
            let mut m = json!({"type": "httpupgrade", "path": path});
            if let Some(h) = host {
                // httpupgrade 的 host 是单值 string，不是数组
                m.as_object_mut().unwrap().insert("host".into(), json!(h));
            }
            Some(m)
        }
    }
}

fn tls_json(tls: &Tls) -> Result<Option<Value>, OutboundError> {
    let Tls::Tls(cfg) = tls else {
        return Ok(None);
    };
    let mut m = Map::new();
    m.insert("enabled".into(), json!(true));
    if let Some(sni) = &cfg.sni {
        m.insert("server_name".into(), json!(sni));
    }
    if cfg.insecure {
        m.insert("insecure".into(), json!(true));
    }
    if !cfg.alpn.is_empty() {
        m.insert("alpn".into(), json!(cfg.alpn));
    }
    if let Some(fp) = &cfg.fingerprint {
        m.insert("utls".into(), json!({"enabled": true, "fingerprint": fp}));
    }
    if let Some(ech) = &cfg.ech {
        m.insert("ech".into(), json!({"enabled": true, "config": ech}));
    }
    if let Some(reality) = &cfg.reality {
        m.insert("reality".into(), reality_json(reality));
    }
    Ok(Some(Value::Object(m)))
}

/// `option.OutboundRealityOptions`：只有这三项
fn reality_json(r: &Reality) -> Value {
    json!({
        "enabled": true,
        "public_key": r.public_key,
        "short_id": r.short_id,
    })
}

pub fn selector(tag: &str, default: &str, outbounds: &[String]) -> Value {
    json!({
        "type": "selector",
        "tag": tag,
        "default": default,
        "outbounds": outbounds,
    })
}

/// `option.URLTestOutboundOptions`：`url` 是单个字符串，`interval` 是 duration 字符串
pub fn urltest(
    tag: &str,
    outbounds: &[String],
    url: &str,
    interval: &str,
    tolerance: u16,
) -> Value {
    json!({
        "type": "urltest",
        "tag": tag,
        "outbounds": outbounds,
        "url": url,
        "interval": interval,
        "tolerance": tolerance,
    })
}

/// 策略组里被直连/拒绝等特殊出站占位，不参与节点表生成
pub fn special_direct() -> Value {
    json!({"type": "direct", "tag": "direct"})
}

pub fn special_block() -> Value {
    json!({"type": "block", "tag": "reject"})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::uri::parse_link;
    use base64::Engine as _;

    #[test]
    fn vless_reality_websocket_full_shape() {
        let node = parse_link(
            "vless://11111111-2222-3333-4444-555555555555@example.com:8443?encryption=none&security=reality&sni=www.microsoft.com&pbk=PUB&sid=6ba85179&fp=chrome&flow=xtls-rprx-vision&type=ws&path=%2Fx&host=mask.com#东京-01",
        )
        .unwrap();
        let got = node_outbound(&node).unwrap();
        let want = json!({
            "type": "vless",
            "tag": "东京-01",
            "server": "example.com",
            "server_port": 8443,
            "uuid": "11111111-2222-3333-4444-555555555555",
            "flow": "xtls-rprx-vision",
            "transport": {
                "type": "ws",
                "path": "/x",
                "headers": {"Host": "mask.com"}
            },
            "tls": {
                "enabled": true,
                "server_name": "www.microsoft.com",
                "utls": {"enabled": true, "fingerprint": "chrome"},
                "reality": {
                    "enabled": true,
                    "public_key": "PUB",
                    "short_id": "6ba85179"
                }
            }
        });
        assert_eq!(got, want);
    }

    #[test]
    fn plain_trojan_has_no_tls_block_and_no_transport_key() {
        let node = parse_link("trojan://pw@1.2.3.4:443#A").unwrap();
        let got = node_outbound(&node).unwrap();
        assert_eq!(got["type"], "trojan");
        assert!(got.get("tls").is_none());
        // 1.14.2 的 transport.type 枚举里没有 tcp，写了会被 DisallowUnknownFields 拒掉
        assert!(got.get("transport").is_none());
    }

    #[test]
    fn hysteria2_obfs_and_tls() {
        let node = parse_link(
            "hysteria2://p@ss@c.com:443?sni=c.com&insecure=1&obfs=salamander&obfs-password=pp&disable_mtu_discovery=1&up=50&down=200#HY",
        )
        .unwrap();
        let got = node_outbound(&node).unwrap();
        assert_eq!(got["type"], "hysteria2");
        assert_eq!(got["obfs"], json!({"type": "salamander", "password": "pp"}));
        assert_eq!(got["tls"]["server_name"], "c.com");
        assert_eq!(got["tls"]["insecure"], true);
        assert_eq!(got["disable_path_mtu_discovery"], true);
        assert_eq!(got["up_mbps"], 50);
        assert_eq!(got["down_mbps"], 200);
        assert!(got.get("transport").is_none(), "QUIC 协议不应有 transport");
    }

    #[test]
    fn tuic_alpn_lands_in_tls_block() {
        let node =
            parse_link("tuic://u-1:pw@d.com:6001?cc=bbr&udp_relay_mode=quic&udp_over_stream=1#T")
                .unwrap();
        let got = node_outbound(&node).unwrap();
        assert_eq!(got["tls"]["alpn"], json!(["h3"]));
        assert_eq!(got["tls"]["enabled"], true);
        assert_eq!(got["congestion_control"], "bbr");
        assert_eq!(got["udp_over_stream"], true);
    }

    #[test]
    fn vmess_carries_cipher_and_numeric_alter_id() {
        let json = serde_json::json!({"v":"2","ps":"V","add":"a.com","port":"443","id":"u","aid":"0","net":"tcp","security":"aes-128-gcm","tls":"tls","sni":"a.com"});
        let encoded = base64::engine::general_purpose::STANDARD.encode(json.to_string());
        let node = parse_link(&format!("vmess://{encoded}")).unwrap();
        let got = node_outbound(&node).unwrap();
        assert_eq!(got["type"], "vmess");
        assert_eq!(got["alter_id"], 0);
        assert_eq!(got["security"], "aes-128-gcm");
        assert_eq!(got["tls"]["server_name"], "a.com");
        assert!(got.get("transport").is_none());
    }

    #[test]
    fn vmess_alter_id_must_be_numeric() {
        let json = serde_json::json!({"v":"2","ps":"V","add":"a.com","port":"443","id":"u","aid":"auto","net":"ws","path":"/p"});
        let encoded = base64::engine::general_purpose::STANDARD.encode(json.to_string());
        let node = parse_link(&format!("vmess://{encoded}")).unwrap();
        assert!(matches!(
            node_outbound(&node),
            Err(OutboundError::BadAlterId { .. })
        ));
    }

    #[test]
    fn illegal_enum_values_are_rejected_at_import() {
        // sing-box 对这些键是硬拒的，必须在导入时就报错，不能等到下发
        assert!(parse_link("tuic://u:p@d.com:6001?cc=bbr2#T").is_err());
        assert!(parse_link("tuic://u:p@d.com:6001?udp_relay_mode=stream#T").is_err());
        assert!(
            parse_link("vless://u@d.com:443?flow=xtls-rprx-direct#A").is_err(),
            "已废弃的 flow 不能进模型"
        );
    }

    #[test]
    fn group_shapes() {
        let tags = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            selector("proxy", "auto", &tags),
            json!({"type":"selector","tag":"proxy","default":"auto","outbounds":["a","b"]})
        );
        let u = urltest("auto", &tags, "https://www.google.com", "3m", 50);
        assert_eq!(
            u,
            json!({"type":"urltest","tag":"auto","outbounds":["a","b"],
                   "url":"https://www.google.com","interval":"3m","tolerance":50})
        );
    }
}
