use serde::{Deserialize, Serialize};

/// 一个可用节点。`kind` 决定了哪些参数有意义：QUIC 类协议不存在 stream transport，
/// 因此用两层枚举把非法组合挡在类型层，而不是靠生成器里的 if。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub server: String,
    pub port: u16,
    pub kind: NodeKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NodeKind {
    /// vless / vmess / trojan / shadowsocks：走 TCP 系传输，可选 TLS
    Stream {
        protocol: StreamProtocol,
        transport: Transport,
        tls: Tls,
    },
    /// hysteria2 / tuic：QUIC 传输，无 stream transport
    Quic { protocol: QuicProtocol },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "protocol")]
pub enum StreamProtocol {
    /// `flow` 只有 vless 有；Reality + Vision 节点几乎都是 `xtls-rprx-vision`，
    /// 丢掉它会让 0-RTT 分流退化，所以必须在模型里留位置。
    Vless {
        uuid: String,
        #[serde(default)]
        flow: Option<String>,
    },
    /// `security` 是消息加密算法（不是 TLS），sing-box 侧无默认值可依赖，恒填。
    Vmess {
        uuid: String,
        alter_id: String,
        #[serde(default = "default_vmess_security")]
        security: String,
    },
    Trojan {
        password: String,
    },
    Shadowsocks {
        method: String,
        password: String,
    },
}

fn default_vmess_security() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Transport {
    /// 直连 TCP。1.14.2 的 `transport.type` 枚举里**不存在** `tcp`
    /// （只有 `http,ws,quic,grpc,httpupgrade`），生成时必须整个省略 `transport` 键。
    #[default]
    Tcp,
    Ws {
        path: String,
        /// 1.14.2 的 ws 传输没有 `host` 字段，生成时必须折叠进 `headers.Host`
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        headers: Vec<(String, String)>,
    },
    Grpc {
        #[serde(default)]
        service_name: Option<String>,
        #[serde(default)]
        idle_timeout: Option<u32>,
    },
    H2 {
        path: String,
        #[serde(default)]
        host: Option<String>,
    },
    HttpUpgrade {
        path: String,
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        headers: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum Tls {
    #[default]
    Disabled,
    Tls(TlsConfig),
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TlsConfig {
    #[serde(default)]
    pub sni: Option<String>,
    #[serde(default)]
    pub alpn: Vec<String>,
    /// 跳过证书校验，仅用于自签测试环境
    #[serde(default)]
    pub insecure: bool,
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub reality: Option<Reality>,
    #[serde(default)]
    pub ech: Option<String>,
}

/// 客户端侧的 Reality 只有公钥与 short id 两项。
///
/// 1.14.2 的 `option.OutboundRealityOptions` 里**没有** `handshake` 子对象（那是服务端
/// `InboundRealityOptions` 才有的字段），握手目标就是出站本身的 `server:server_port`，
/// 伪装域名走 `tls.server_name`。多发一个 `handshake` 键会被 `DisallowUnknownFields` 拒掉。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Reality {
    pub public_key: String,
    #[serde(default)]
    pub short_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "protocol")]
pub enum QuicProtocol {
    Hysteria2 {
        password: String,
        #[serde(default)]
        obfs: Option<String>,
        #[serde(default)]
        obfs_password: Option<String>,
        /// QUIC 侧字段名是 `disable_path_mtu_discovery`，不是 `disable_mtu_discovery`
        #[serde(default)]
        disable_path_mtu_discovery: bool,
        /// 带宽（Mbps），brutal 拥塞控制的前提；为 None 时退回标准 BBR
        #[serde(default)]
        up_mbps: Option<u32>,
        #[serde(default)]
        down_mbps: Option<u32>,
        #[serde(default)]
        tls: Tls,
    },
    Tuic {
        uuid: String,
        password: String,
        #[serde(default)]
        alpn: Vec<String>,
        /// 枚举：`cubic | new_reno | bbr`
        #[serde(default)]
        congestion_control: Option<String>,
        /// 枚举：`native | quic`
        #[serde(default)]
        udp_relay_mode: Option<String>,
        #[serde(default)]
        udp_over_stream: bool,
        #[serde(default)]
        zero_rtt_handshake: bool,
        #[serde(default)]
        tls: Tls,
    },
}

impl Node {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("节点名不能为空".into());
        }
        if self.server.trim().is_empty() {
            return Err(format!("节点 {} 缺少服务器地址", self.name));
        }
        if self.port == 0 {
            return Err(format!("节点 {} 端口非法", self.name));
        }
        match &self.kind {
            NodeKind::Stream { protocol, .. } => match protocol {
                StreamProtocol::Vless { uuid, flow } => {
                    if uuid.trim().is_empty() {
                        return Err(format!("节点 {} 缺少 uuid", self.name));
                    }
                    if let Some(flow) = flow {
                        if !FLOWS.contains(&flow.as_str()) {
                            return Err(format!(
                                "节点 {} 的 flow \"{flow}\" 不是 sing-box 认可的取值（{}）",
                                self.name,
                                FLOWS.join(" / ")
                            ));
                        }
                    }
                }
                StreamProtocol::Vmess { uuid, security, .. } => {
                    if uuid.trim().is_empty() {
                        return Err(format!("节点 {} 缺少 uuid", self.name));
                    }
                    if !VMESS_SECURITY.contains(&security.as_str()) {
                        return Err(format!(
                            "节点 {} 的 vmess 加密方式 \"{security}\" 不被 sing-box 支持（{}）",
                            self.name,
                            VMESS_SECURITY.join(" / ")
                        ));
                    }
                }
                StreamProtocol::Trojan { password } => {
                    if password.is_empty() {
                        return Err(format!("节点 {} 缺少密码", self.name));
                    }
                }
                StreamProtocol::Shadowsocks { method, password } => {
                    if method.is_empty() || password.is_empty() {
                        return Err(format!("节点 {} 缺少加密方式或密码", self.name));
                    }
                }
            },
            NodeKind::Quic { protocol } => match protocol {
                QuicProtocol::Hysteria2 { obfs, .. } => {
                    if let Some(kind) = obfs {
                        if !HY2_OBFS.contains(&kind.as_str()) {
                            return Err(format!(
                                "节点 {} 的 obfs \"{kind}\" 非法（只能是 {}）",
                                self.name,
                                HY2_OBFS.join(" / ")
                            ));
                        }
                    }
                }
                QuicProtocol::Tuic {
                    congestion_control,
                    udp_relay_mode,
                    ..
                } => {
                    if let Some(cc) = congestion_control {
                        if !TUIC_CONGESTION_CONTROL.contains(&cc.as_str()) {
                            return Err(format!(
                                "节点 {} 的 congestion_control \"{cc}\" 非法（只能是 {}）",
                                self.name,
                                TUIC_CONGESTION_CONTROL.join(" / ")
                            ));
                        }
                    }
                    if let Some(mode) = udp_relay_mode {
                        if !TUIC_UDP_RELAY_MODE.contains(&mode.as_str()) {
                            return Err(format!(
                                "节点 {} 的 udp_relay_mode \"{mode}\" 非法（只能是 {}）",
                                self.name,
                                TUIC_UDP_RELAY_MODE.join(" / ")
                            ));
                        }
                    }
                }
            },
        }
        Ok(())
    }
}

/// 以下取值组都来自 1.14.2 `option/` 里的 `enum:` struct tag，sing-box 会硬拒其他值。
const FLOWS: &[&str] = &["xtls-rprx-vision", "xtls-rprx-vision-udp443"];
const VMESS_SECURITY: &[&str] = &[
    "auto",
    "none",
    "zero",
    "aes-128-cfb",
    "aes-128-gcm",
    "chacha20-poly1305",
];
const HY2_OBFS: &[&str] = &["salamander", "gecko"];
const TUIC_CONGESTION_CONTROL: &[&str] = &["cubic", "new_reno", "bbr"];
const TUIC_UDP_RELAY_MODE: &[&str] = &["native", "quic"];
