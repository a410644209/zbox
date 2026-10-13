//! ZBox 的期望状态：面板里能改的东西全部落在这里，生成器只认这个结构。
//!
//! 与 `gen::singbox` 的分工：本模块只描述"用户想要什么"，不含任何 sing-box 字段名；
//! 字段名全部集中在 `gen::singbox`，且必须与
//! `docs/reference/singbox-1.14.2-schema.md` 一致。

use crate::model::device::Device;
use crate::model::node::Node;
use serde::{Deserialize, Serialize};

/// ZBox 固定使用的两个特殊出站 tag。定义在模型层，因为面板、生成器和校验器都要引用它。
pub const DIRECT_TAG: &str = "direct";
pub const REJECT_TAG: &str = "reject";

/// 内置规则集的 tag。tag 名在配置里被 `route.rules[].rule_set` 引用，
/// 引用未定义的 tag 会让 `sing-box check` 失败，所以集中定义、不要手打字符串。
pub const RULESET_CN_DOMAIN: &str = "geosite-cn";
pub const RULESET_CN_IP: &str = "geoip-cn";
pub const RULESET_NON_CN: &str = "non-cn";
pub const RULESET_ADS: &str = "geosite-category-ads-all";
pub const RULESET_PRIVATE_IP: &str = "geoip-private";

/// 规则集编译产物的落点目录，相对 sing-box 的 `-D`。
///
/// 引用它的四处必须逐字相同：生成配置里的 `initial_path`、`ingest::ruleset` 的编译目标、
/// `svc::ruleset` 的落地与清单、`Layout::rule_set_dir()`。VM 实测（schema §11.7）里
/// `initial_path` 指向的文件不存在时 sing-box 会在启动的约 15 秒预算内现抓 GitHub。
pub const RULE_SET_DIR: &str = "rule-set";

/// tag 派生的产物相对路径。`initial_path` 只能等于它，否则预置的文件没人去读。
pub fn rule_set_product(tag: &str) -> String {
    format!("{RULE_SET_DIR}/{tag}.srs")
}

/// tag 会被拼成文件路径，这是唯一的防线（对应 "unsafe provider source path"）。
pub fn is_safe_rule_set_tag(tag: &str) -> bool {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    !tag.is_empty() && !tag.starts_with('.') && tag.chars().all(allowed)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DesiredState {
    pub network: Network,
    pub ports: Ports,
    pub dns: Dns,
    pub policy: Policy,
    pub nodes: Vec<Node>,
    pub groups: Vec<Group>,
    pub devices: Vec<Device>,
    pub rule_sets: Vec<RuleSet>,
    /// 改动如何送到数据面：热重载还是重启。默认热重载；G2（`dae reload` 是否真保留连接）
    /// 未过之前，用户可以在面板上退回 restart 并要求"每次改都掐一次连接"这个可见代价。
    #[serde(default)]
    pub apply_policy: ApplyPolicy,
    /// 订阅的元信息：到期时间、流量配额。从机场响应头或 info 条目解析而来，
    /// 面板「订阅与节点」那一屏要显示它。None = 还没导入过订阅。
    #[serde(default)]
    pub subscription_meta: Option<SubscriptionMeta>,
}

/// 订阅的元信息：机场通过响应头或 info 条目传递的到期时间、流量配额。
///
/// 时间戳是 Unix 秒（UTC 00:00:00），流量是字节数。None = 机场没给这一项。
/// 面板上"套餐到期：2027-01-14"和"剩余流量：1016.75 GB"从这两个字段算出来。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SubscriptionMeta {
    /// 套餐到期时间（Unix 时间戳，秒）
    pub expire: Option<i64>,
    /// 总流量配额（字节）
    pub total: Option<i64>,
    /// 已用流量（字节）
    pub used: Option<i64>,
}

/// 下发节奏。两条都有代价，所以做成明写的一格而不是藏在生成器里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyPolicy {
    /// `dae reload` + oxidns `POST /api/reload`：在用连接尽量不断
    #[default]
    Reload,
    /// 整个数据面重启：慢一点、会掐断一次，但不依赖 reload 的语义
    Restart,
}

/// 旁路由的接管方式（`docs/BLUEPRINT.md` §2 的两条路线）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMode {
    /// R2：设备网关仍是主路由，oxidns 把"该走代理的解析结果"作为带 TTL 的动态条目写回主路由。
    /// 默认这一档——旁路由挂了只丢科学流量，不丢网。
    #[default]
    Inject,
    /// R1：设备网关整个交给旁路由，所有流量先到 dae。作用域由设备档位决定。
    Takeover,
}

/// 旁路由自身的网络位置。TPROXY 的 nftables 层要用它来生成劫持规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    /// 本机上承载局域网流量的网卡名
    pub lan_interface: String,
    /// 局域网 CIDR，例如 192.168.1.0/24
    pub lan_cidr: String,
    /// 本机在局域网上的 IPv4。oxidns 的解析口就绑在这个地址上——它没有客户端 ACL，
    /// `listen` 是唯一的闸。空 = 还没给（渲染期硬错误，见 `gen::oxidns`）。
    #[serde(default)]
    pub lan_address: String,
    /// 本机在局域网上的 IPv6（单地址，CIDR 前缀不写）。只有接管 v6 的 DNS 时才用得到。
    /// 空 = 还没给。填之前想一想这地址会不会变：SLAAC 的动态地址会变，
    /// 变了而配置没跟上，解析口就落在一个本机已经没有的地址上（DNS 静默不响应）。
    #[serde(default)]
    pub lan_address6: String,
    /// 局域网的 IPv6 前缀（劫持层要按它限定 `ip6 saddr`）。空 = 还没给。
    #[serde(default)]
    pub lan_cidr6: String,
    /// 默认路由出去的网卡名，用于给出站打标记防回环
    pub wan_interface: String,
    /// 流量路线：注入主路由（默认）还是接管网关
    #[serde(default)]
    pub route_mode: RouteMode,
    /// IPv6 接管姿态。默认不接管——接管要动内核转发，而开转发会连带影响本机自己的
    /// v6 默认路由（见 [`Ipv6Mode`]）。
    #[serde(default)]
    pub ipv6_mode: Ipv6Mode,
}

/// IPv6 的三种姿态。差别不是"开不开 v6"，而是**谁去接 v6 的流量**，以及
/// 不接的时候那条泄漏路径由谁堵。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ipv6Mode {
    /// 完全不碰 v6：v6 流量按主路由的 RA 自己直连出去。这一档必须由 DNS 侧堵泄漏，
    /// 也就是 `dns.strategy` 得是 `*_only`；否则客户端拿到 AAAA 就走 v6 直连。
    #[default]
    Off,
    /// 只把 v6 的 DNS 查询收进来（oxidns 双栈监听 + 劫持层加 v6 的 53 重定向），
    /// 连接层仍不接管 v6。所以这一档**必须**配 `*_only`：解析收口了、路由没收口，
    /// 留着另一族的答案就是留着一条绕过代理的路。
    DnsOnly,
    /// 双栈接管：v6 也进 TPROXY（`table ip6`、`ip -6 rule`、`net.ipv6.conf.*.forwarding=1`）。
    /// 开转发会让内核按 RFC 4861 停止采纳 RA，所以生成物必须成对写 `accept_ra=2`。
    DualStack,
}

impl Ipv6Mode {
    /// 这一档要不要把 v6 的 DNS 查询收进来。
    pub fn takes_dns(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// 这一档要不要在连接层接管 v6（决定 nft 是否加 ip6 链、是否开 v6 转发）。
    pub fn takes_connection(&self) -> bool {
        matches!(self, Self::DualStack)
    }

    /// 面板上那一行的名字。
    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "不接管",
            Self::DnsOnly => "只接管 DNS",
            Self::DualStack => "双栈接管",
        }
    }

    /// serde 名，写给报错和注释看的人（面板那个下拉框用的是同一份字符串）。
    pub fn token(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::DnsOnly => "dns_only",
            Self::DualStack => "dual_stack",
        }
    }
}

impl Default for Network {
    fn default() -> Self {
        Self {
            lan_interface: "ens33".into(),
            lan_cidr: "192.168.1.0/24".into(),
            lan_address: String::new(),
            lan_address6: String::new(),
            lan_cidr6: String::new(),
            wan_interface: "ens33".into(),
            route_mode: RouteMode::default(),
            ipv6_mode: Ipv6Mode::default(),
        }
    }
}

/// 端口布局沿用参考实现的约定，便于对照排查。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ports {
    /// 对局域网设备提供 DNS 的端口（被 nftables DNAT 的目标）
    pub dns: u16,
    /// TPROXY 透明代理端口
    pub tproxy: u16,
    /// REDIRECT 兜底端口（仅 TCP）
    pub redirect: u16,
    /// HTTP/SOCKS 混合入站，给手动设置代理的设备用
    pub mixed: u16,
    /// 可选的纯 SOCKS 入站
    pub socks: Option<u16>,
}

impl Default for Ports {
    fn default() -> Self {
        Self {
            dns: 53,
            tproxy: 9888,
            redirect: 9887,
            mixed: 8888,
            socks: Some(7890),
        }
    }
}

/// 上游传输方式。序列化名显式写出：`DoH` 走 snake_case 会变成 `do_h`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpstreamTransport {
    Udp,
    Tcp,
    /// DNS over TLS
    #[serde(rename = "dot")]
    Dot,
    /// DNS over HTTPS
    #[serde(rename = "doh")]
    DoH,
    /// DNS over HTTP/3
    #[serde(rename = "doh3")]
    DoH3,
}

impl UpstreamTransport {
    pub fn default_port(&self) -> u16 {
        match self {
            Self::Udp | Self::Tcp => 53,
            Self::Dot => 853,
            Self::DoH | Self::DoH3 => 443,
        }
    }

    /// sing-box 的 `dns.servers[].type` 取值
    pub fn singbox_type(&self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
            Self::Dot => "tls",
            Self::DoH => "https",
            Self::DoH3 => "h3",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    #[default]
    Direct,
    /// 经由某个出站/策略组解析（DoH 走代理时用）
    Outbound(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    pub tag: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    pub transport: UpstreamTransport,
    /// DoH/DoH3 的查询路径
    #[serde(default)]
    pub path: Option<String>,
    /// TLS 类上游的 SNI；省略时用 host
    #[serde(default)]
    pub sni: Option<String>,
    #[serde(default)]
    pub via: Via,
}

impl Upstream {
    /// 地址是不是 IP 字面量。域名地址的上游需要一条 IP 地址的上游先解析它（见 `validate`）。
    pub fn address_is_ip(&self) -> bool {
        self.host
            .trim()
            .trim_start_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok()
    }
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            tag: "domestic".into(),
            host: "223.5.5.5".into(),
            port: None,
            transport: UpstreamTransport::DoH,
            path: Some("/dns-query".into()),
            sni: None,
            via: Via::Direct,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeIp {
    pub enabled: bool,
    pub inet4_range: String,
    pub inet6_range: String,
}

impl Default for FakeIp {
    fn default() -> Self {
        Self {
            enabled: true,
            inet4_range: "198.18.0.0/15".into(),
            inet6_range: "fc00::/18".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum DnsStrategy {
    #[default]
    PreferIpv4,
    Ipv4Only,
    Ipv6Only,
    PreferIpv6,
}

impl DnsStrategy {
    pub fn singbox_value(&self) -> &'static str {
        match self {
            Self::PreferIpv4 => "prefer_ipv4",
            Self::PreferIpv6 => "prefer_ipv6",
            Self::Ipv4Only => "ipv4_only",
            Self::Ipv6Only => "ipv6_only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dns {
    /// 国内域名走这些上游（第一个为主，其余做冗余）
    pub domestic: Vec<Upstream>,
    /// 其余域名的上游；开启 fakeip 时它只负责境外解析
    pub overseas: Vec<Upstream>,
    /// 是否用系统解析器兜底解析局域网/主机名
    pub use_system_local: bool,
    /// 让 oxidns 记查询记录（`query_recorder`）——面板「DNS 记录」那一屏的唯一数据源。
    /// 关掉 = 那一屏永远空，所以这件事得在状态里看得见，而不是渲染器里写死。
    /// 默认开：ZBox 的编排前提就是"每台设备的解析走这台机器"，记录是它的观测面。
    #[serde(default = "records_enabled_default")]
    pub records_enabled: bool,
    /// 记录留几天（oxidns 的 `retention_days`）。必须 ≥1：源码在插件构造时就拒
    /// （`query_recorder/mod.rs:246-248`），而 `oxidns check` 不验这一条（2026-10-11 guest 实测
    /// 给 valid + rc=0，`start` 才 rc=1 报 "retention_days must be at least 1"）。
    /// 0 既不是"永久"也不是"不留"，所以保存这一关就退回。
    #[serde(default = "records_retention_default")]
    pub records_retention_days: u64,
    pub fakeip: FakeIp,
    pub strategy: DnsStrategy,
}

/// `records_enabled` 的 serde 默认值：老状态文件里没这一格时按"开着"读。
/// 用 `#[serde(default)]` 会得到 `false`，那等于给每台升级上来的机器静默关掉观测面。
fn records_enabled_default() -> bool {
    true
}

fn records_retention_default() -> u64 {
    7
}

impl Default for Dns {
    fn default() -> Self {
        Self {
            domestic: vec![
                Upstream::default(),
                Upstream {
                    tag: "domestic-2".into(),
                    host: "119.29.29.29".into(),
                    ..Upstream::default()
                },
            ],
            overseas: vec![overseas_upstream("proxy")],
            use_system_local: true,
            records_enabled: true,
            records_retention_days: 7,
            fakeip: FakeIp::default(),
            strategy: DnsStrategy::PreferIpv4,
        }
    }
}

/// 一条域名匹配。生成的 JSON 直接对应 sing-box 的四个同名键。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainPattern {
    Exact(String),
    Suffix(String),
    Keyword(String),
    Regex(String),
}

impl DomainPattern {
    /// 面板输入约定：`+example.com` 后缀、`~example` 关键词、`/re/` 正则，其余精确匹配。
    /// 裸的 `.example.com` 也当后缀处理——这是域名列表里最常见的写法。
    pub fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim();
        if s.is_empty() || s.starts_with('#') {
            return None;
        }
        let mut chars = s.chars();
        match chars.next()? {
            '+' => Some(Self::Suffix(
                chars.as_str().trim_start_matches('.').to_string(),
            )),
            '~' => Some(Self::Keyword(chars.as_str().to_string())),
            '/' => {
                let body = chars.as_str().trim_end_matches('/');
                Some(Self::Regex(body.to_string()))
            }
            '.' => Some(Self::Suffix(s.trim_start_matches('.').to_string())),
            _ => Some(Self::Exact(s.to_string())),
        }
    }

    pub fn is_empty_name(&self) -> bool {
        match self {
            Self::Exact(s) | Self::Suffix(s) | Self::Keyword(s) | Self::Regex(s) => {
                s.trim().is_empty()
            }
        }
    }

    /// 回到面板里的那种写法（`parse` 的逆）。裸 `.example.com` 会归一成 `+example.com`，
    /// 两者语义相同，所以这个往返不承诺逐字符还原输入。
    pub fn raw(&self) -> String {
        match self {
            Self::Exact(s) => s.clone(),
            Self::Suffix(s) => format!("+{s}"),
            Self::Keyword(s) => format!("~{s}"),
            Self::Regex(s) => format!("/{s}/"),
        }
    }
}

/// 一条域名规则的处置方式。与 sing-box 的动作一一对应，
/// 但**由谁先命中决定生效**，不再是"分了档就默认某档更高"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainAction {
    /// 送直连出站
    Direct,
    /// 送最终代理出口
    Proxy,
    /// 拒绝
    Block,
}

impl DomainAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "直连",
            Self::Proxy => "代理",
            Self::Block => "拦截",
        }
    }
}

/// 一条域名规则 = 目标 + 处置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainRule {
    pub pattern: DomainPattern,
    pub action: DomainAction,
}

impl DomainRule {
    pub fn new(pattern: DomainPattern, action: DomainAction) -> Self {
        Self { pattern, action }
    }

    /// 给医生和面板看的一行话：面板写法 + 处置。
    pub fn describe(&self) -> String {
        format!("{}（{}）", self.pattern.raw(), self.action.label())
    }
}

/// 兜底出口。这里必须是外部标签（`{"proxy":"组名"}` / `"direct"`）：
/// serde 的内部标签（`tag = "mode"`）无法序列化装着字符串的新类型变体。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalOutbound {
    /// 未命中任何规则的流量走该策略组
    Proxy(String),
    /// 保守模式：其余全部直连（新装时的默认，装了不断网）
    #[default]
    Direct,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// 私网/回环/组播地址直连
    pub private_direct: bool,
    /// 中国大陆域名与 IP 直连
    pub cn_direct: bool,
    /// 命中广告规则集则直接拒绝
    pub block_ads: bool,
    /// BT 下载直连
    pub bittorrent_direct: bool,
    /// 用户自己的域名名单。**顺序即优先级**：第一条命中的生效，后面的规则不再看
    /// （sing-box 的 `route` 与 `dns.rules` 的 `action:"route"` 都是终止语义，schema §11.7 实测）。
    ///
    /// v1 是三档 `Vec<DomainPattern>`，档位之间有一层看不见默认排序（#18 就是因为两层给的
    /// 顺序相反）。合成一条有序名单之后互斥在模型层就消失了——用户看到的顺序就是引擎的执行顺序。
    pub domain_rules: Vec<DomainRule>,
    pub final_outbound: FinalOutbound,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            private_direct: true,
            cn_direct: true,
            block_ads: true,
            bittorrent_direct: true,
            domain_rules: Vec::new(),
            // 默认保守：先保证不断网，代理由用户显式选组
            final_outbound: FinalOutbound::Direct,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GroupKind {
    Selector,
    Urltest {
        url: String,
        interval: String,
        tolerance: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub tag: String,
    pub kind: GroupKind,
    /// 成员出站 tag；空列表的策略组会被生成器拒绝
    pub members: Vec<String>,
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "source")]
pub enum RuleSet {
    /// 随 ZBox 分发的本地 geodata 编译产物（`.srs`）。ZBox 不解析其内容，只原样交给 sing-box。
    Local { tag: String, path: String },
    Remote {
        tag: String,
        url: String,
        /// 本地缓存路径；sing-box 1.14 用它做离线兜底
        initial_path: String,
        update_interval: String,
    },
}

impl RuleSet {
    pub fn tag(&self) -> &str {
        match self {
            Self::Local { tag, .. } | Self::Remote { tag, .. } => tag,
        }
    }
}

/// 内置规则集，标签与 `gen::singbox::route` 里引用的 `RULESET_*` 常量一致。
///
/// **2026-09-29 实测纠正**：`.srs` 不在 release 附件里 —— `sing-geoip`/`sing-geosite` 的
/// release 只发 `.db`（geodata 老格式），`releases/latest/download/*.srs` 一律 404。
/// 编译好的 `.srs` 发布在 **`rule-set` 分支**上，四个内置项在那儿都是 200：
/// `geosite-cn` / `geosite-category-ads-all` / `geosite-geolocation-!cn` / `geoip-cn`。
/// 这四条不只是 wget 200：2026-09-29 用 54 节点订阅配置（`route.final` = 代理组）跑
/// `sing-box run`，四条全部 `INFO router: updated rule-set <tag>`、零 ERROR 启动成功。
///
/// `geoip-private` 上游根本没有（同一分支 404），所以不在这里给出：私网直连靠
/// `route` 里的 `ip_is_private` 规则，用户想按自己的网段细分可自行添加 `RuleSet::Local`。
///
/// 注意 `initial_path` 按 **tag** 而非上游文件名派生：tag 是 ZBox 侧唯一的身份（清单的键、
/// 编译产物的文件名都从它来），而上游文件名可以随便长（`geosite-geolocation-!cn` 里那个 `!`
/// 就不在允许的文件名字符集里）。照抄文件名会让预置产物落在 sing-box 不会去读的路径上。
pub fn builtin_rule_sets() -> Vec<RuleSet> {
    let remote = |repo: &str, tag: &str, file: &str| RuleSet::Remote {
        tag: tag.into(),
        url: format!("https://raw.githubusercontent.com/SagerNet/{repo}/rule-set/{file}.srs"),
        initial_path: rule_set_product(tag),
        update_interval: "168h".into(),
    };
    vec![
        remote("sing-geosite", RULESET_CN_DOMAIN, "geosite-cn"),
        remote("sing-geosite", RULESET_ADS, "geosite-category-ads-all"),
        remote("sing-geosite", RULESET_NON_CN, "geosite-geolocation-!cn"),
        remote("sing-geoip", RULESET_CN_IP, "geoip-cn"),
    ]
}

/// 智能分流那三条：`cn_direct` 的两份数据加"其余走代理"引用的那一份。
///
/// 从 [`builtin_rule_sets`] 里**筛**而不是另写一份 URL：tag、上游地址、`initial_path`
/// 的派生规则只许有一个定义，多写一处就是"面板设的那一档引用的规则集与 `rule-sets` 那一屏
/// 列出来的不是同一条"。
pub fn smart_split_rule_sets() -> Vec<RuleSet> {
    builtin_rule_sets()
        .into_iter()
        .filter(|rule_set| {
            matches!(
                rule_set.tag(),
                RULESET_CN_DOMAIN | RULESET_CN_IP | RULESET_NON_CN
            )
        })
        .collect()
}

/// 「默认怎么走 = 智能分流」那个兜底策略组的首选名字。
///
/// 这是面板上唯一不用人填的 tag，所以它受两条既有的规矩管：不许和节点重名
/// （`validate()` 里那句"策略组 tag 与节点重名"），也必须是 dae 认的标识符（G12 实测：
/// 首字符 ASCII 字母或下划线）。`auto` 两条都过；撞名时按 [`next_smart_split_group`] 让位。
pub const SMART_SPLIT_GROUP: &str = "auto";

/// urltest 那三条测速参数。`interval` 要过 `validate()` 的时长字符串那一关（`3m` 这种写法）。
pub const SMART_SPLIT_URL: &str = "http://www.gstatic.com/generate_204";
pub const SMART_SPLIT_INTERVAL: &str = "3m";
pub const SMART_SPLIT_TOLERANCE: u16 = 50;

/// 境外 DNS 上游的形状：DoH 到 8.8.8.8，并**经由那个兜底策略组**带出去。
///
/// 抽成函数是因为有两处要的是同一个形状：[`Dns::default()`] 那一格出厂值，和
/// [`DesiredState::enable_smart_split`] 导入时设的那一条。两份写法迟早错开，
/// 表现是"默认档里的境外上游和导入后磁盘上那份不是同一个东西"。
pub fn overseas_upstream(group: &str) -> Upstream {
    Upstream {
        tag: "overseas".into(),
        host: "8.8.8.8".into(),
        port: None,
        transport: UpstreamTransport::DoH,
        path: Some("/dns-query".into()),
        sni: None,
        via: Via::Outbound(group.into()),
    }
}

/// 这个状态里已经被占用的出站身份（节点名 + 组 tag）。
fn outbound_identities(state: &DesiredState) -> Vec<&str> {
    state
        .nodes
        .iter()
        .map(|node| node.name.as_str())
        .chain(state.groups.iter().map(|group| group.tag.as_str()))
        .collect()
}

/// 兜底组实际用的名字：首选 `auto`，被节点或别的组占了就往后让 `auto-2`、`auto-3`……
///
/// "机场里真有个叫 auto 的节点"不是假想敌——订阅里 `auto` 是常见命名。让位之后的名字
/// 会原样出现在回执里（§11.3 D：动的是哪几格要看得见），所以这里给的是**确定的**序列，
/// 不是随机后缀。
pub fn next_smart_split_group(state: &DesiredState) -> String {
    let taken = outbound_identities(state);
    if !taken.contains(&SMART_SPLIT_GROUP) {
        return SMART_SPLIT_GROUP.into();
    }
    (2u32..)
        .map(|i| format!("{SMART_SPLIT_GROUP}-{i}"))
        .find(|candidate| !taken.contains(&candidate.as_str()))
        .unwrap_or_else(|| format!("{SMART_SPLIT_GROUP}-2"))
}

impl DesiredState {
    /// 新装基线：全直连、DNS 只用国内上游、不开 fakeip。
    /// 装完先下发这一份，保证"装了不断网"，之后才由用户显式开启劫持。
    ///
    /// 基线刻意不引用任何规则集（`cn_direct` 在 `final = direct` 下是冗余规则），
    /// 所以第一次下发不需要下载 `.srs`，也不需要能连通 GitHub。
    pub fn baseline() -> Self {
        Self {
            nodes: Vec::new(),
            groups: Vec::new(),
            dns: Dns {
                domestic: Dns::default().domestic,
                overseas: Vec::new(),
                use_system_local: true,
                fakeip: FakeIp {
                    enabled: false,
                    ..FakeIp::default()
                },
                strategy: DnsStrategy::PreferIpv4,
                records_enabled: true,
                records_retention_days: 7,
            },
            policy: Policy {
                private_direct: true,
                cn_direct: false,
                block_ads: false,
                bittorrent_direct: false,
                domain_rules: Vec::new(),
                final_outbound: FinalOutbound::Direct,
            },
            rule_sets: Vec::new(),
            devices: Vec::new(),
            network: Network::default(),
            ports: Ports::default(),
            apply_policy: ApplyPolicy::default(),
            subscription_meta: None,
        }
    }

    /// 把「默认怎么走」设成智能分流那一档，返回真正被用到的兜底组名。
    ///
    /// 一次改**五处**不是图省事，是因为这一档缺任何一处都不成立，而"成立"在两扇门外还要
    /// 各验一次：
    /// - `final_outbound = Proxy(tag)`：`validate()` 拦"引用不存在的出站"，所以 `tag` 必须
    ///   同时进 `groups[]`；
    /// - `dns.overseas`：境外域名要有能问的上游。空着时 `gen::oxidns::check_available`
    ///   **整份渲染失败**，而 `doctor::dns_leak_no_overseas` 在"有代理却没有境外上游"这一组合
    ///   下是 Fatal——那就是"设了智能分流，屏幕告诉你机器坏了"；
    /// - `rule_sets[]`：`cn_direct` 与"其余走代理"引用的那两份数据要有声明，否则
    ///   `gen::singbox::route` 的 `require_rule_set` 直接 generation error；
    /// - `private_direct`：智能分流的前提是"局域网与私网不绕路"，这一格在基线里本来就是开的，
    ///   重新按一遍是为了让这一档的定义**不依赖别处的默认值**。
    ///
    /// 只设 `policy` 那两格会制造出一个自相矛盾的屏幕，正是 §11.3 D 要防的事。
    ///
    /// `block_ads` 与 `bittorrent_direct` **不碰**：这一档说的是"默认怎么走"，
    /// 广告拦截是另一件事，静替人打开它才是暗改。
    ///
    /// `None` = 一个字节都没改，因为这一档的前提是"手上至少有一个出口"——节点列表是空的。
    /// 组名撞车（订阅里真有叫 `auto` 的节点）时不返回失败，按 [`next_smart_split_group`] 让位，
    /// 回执念的是让位之后的那一个名字。
    pub fn enable_smart_split(&mut self) -> Option<String> {
        if self.nodes.is_empty() {
            return None;
        }
        let tag = next_smart_split_group(self);
        self.policy.private_direct = true;
        self.policy.cn_direct = true;
        self.policy.final_outbound = FinalOutbound::Proxy(tag.clone());
        self.groups.push(Group {
            tag: tag.clone(),
            kind: GroupKind::Urltest {
                url: SMART_SPLIT_URL.into(),
                interval: SMART_SPLIT_INTERVAL.into(),
                tolerance: SMART_SPLIT_TOLERANCE,
            },
            members: self.nodes.iter().map(|node| node.name.clone()).collect(),
            // urltest 按测速自动选，1.14.2 没有"默认成员"这个概念（validate 会拒）
            default: None,
        });
        self.dns.overseas = vec![overseas_upstream(&tag)];
        for rule_set in smart_split_rule_sets() {
            if !self
                .rule_sets
                .iter()
                .any(|have| have.tag() == rule_set.tag())
            {
                self.rule_sets.push(rule_set);
            }
        }
        Some(tag)
    }

    /// 智能分流这一档现在**实际**落在哪个组上；不是这一档（兜底直连、或组已经不存在）给 `None`。
    /// 「订阅与节点」那一屏要说"新增的那几个还没进兜底组"，判据从这里取，不在面板那一侧重写一遍。
    pub fn smart_split_group_tag(&self) -> Option<&str> {
        match &self.policy.final_outbound {
            FinalOutbound::Proxy(tag) => self
                .groups
                .iter()
                .find(|group| &group.tag == tag)
                .map(|group| group.tag.as_str()),
            FinalOutbound::Direct => None,
        }
    }

    /// 整份期望状态的门。**一次列全**，不是命中第一条就返回：
    /// 面板上那一条红字要能一次说完"这几格都要改"，而不是改一条按一次保存、
    /// 再撞出下一条——abox 那块红底给的就是这个，用户数得清还剩几处。
    ///
    /// 返回的是**清单**：字面相同的原因只留一条（见结尾那段），顺序 = 检查顺序 = 屏序
    /// （网卡 → 节点 → 组 → 规则集 → DNS → 名单 → 设备），不按字典序重排，
    /// 好让人顺着屏幕从上往下改。
    ///
    /// 措辞与编号的排版不住这里，住 [`describe_problems`]——四条出口共用那一份。
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut problems: Vec<String> = Vec::new();
        // 劫持层会把这两个值原样写进 nft 脚本和 sysctl 键名，必须先形状正确
        if self.network.lan_interface.trim().is_empty() {
            problems.push("缺少局域网网卡名".into());
        }
        if !is_cidr(&self.network.lan_cidr) {
            problems.push(format!(
                "局域网 CIDR \"{}\" 写法非法",
                self.network.lan_cidr
            ));
        }
        if !self.network.lan_cidr6.trim().is_empty() && !is_cidr6(self.network.lan_cidr6.trim()) {
            problems.push(format!(
                "局域网 IPv6 前缀 \"{}\" 写法非法：要的是带斜杠的 v6 段，例如 fd00:db8:1::/64",
                self.network.lan_cidr6
            ));
        }
        // 只要收 v6 的包，就得知道哪些源地址算"局域网"——nft 里那条 `ip6 saddr != 段` 是
        // 作用域的边界，没有它就是把上游那侧的 v6 也劫了。
        if self.network.ipv6_mode != Ipv6Mode::Off && self.network.lan_cidr6.trim().is_empty() {
            problems.push(format!(
                "IPv6 = {}（{}）要先填局域网 IPv6 前缀（network.lan_cidr6），例如 fd00:db8:1::/64",
                self.network.ipv6_mode.token(),
                self.network.ipv6_mode.label()
            ));
        }
        for node in &self.nodes {
            if let Err(detail) = node.validate() {
                problems.push(detail);
            }
        }
        let mut node_tags: Vec<&str> = Vec::new();
        for node in &self.nodes {
            if node_tags.contains(&node.name.as_str()) {
                problems.push(format!("出站 tag 重复：{}", node.name));
            }
            node_tags.push(&node.name);
        }
        let mut group_tags: Vec<&str> = Vec::new();
        for group in &self.groups {
            if group.tag.trim().is_empty() {
                problems.push("策略组缺少 tag".into());
            }
            // 撞了要说清撞的是谁（§11.13 边界 3）：拿同一句"与节点重名"盖住组撞组，
            // 人会去节点那一屏找一个根本不存在的重名。空 tag 也不进这道比较——它谁都没撞，
            // 只是没名字，而"缺少 tag"上一行已经报过；拿空串去比名单，两个匿名组相撞就成了
            // `策略组 tag "" 与节点重名`，既指错对象又说不清在哪儿。
            if !group.tag.trim().is_empty() {
                if node_tags.contains(&group.tag.as_str()) {
                    problems.push(format!("策略组 tag \"{}\" 与节点重名", group.tag));
                } else if group_tags.contains(&group.tag.as_str()) {
                    problems.push(format!("策略组 tag \"{}\" 与另一个策略组重名", group.tag));
                }
            }
            group_tags.push(&group.tag);
            if group.members.is_empty() {
                problems.push(format!("策略组 {} 没有任何成员", group.tag));
            }
            // 引用不存在的出站会让 sing-box check 整体失败，必须在模型层拦住
            for member in &group.members {
                let known = self.nodes.iter().any(|n| &n.name == member)
                    || self.groups.iter().any(|g| &g.tag == member)
                    || member == DIRECT_TAG
                    || member == REJECT_TAG;
                if !known {
                    problems.push(format!("策略组 {} 引用了不存在的成员 {member}", group.tag));
                }
            }
            if let Some(default) = &group.default {
                if matches!(group.kind, GroupKind::Urltest { .. }) {
                    // 不是"多余字段"这么简单：静默丢掉用户选的默认成员，面板上看着设了、
                    // 实际没生效，比直接报错更难查
                    problems.push(format!(
                        "策略组 {} 是 urltest，1.14.2 没有默认成员这个概念（它按测速自动选）",
                        group.tag
                    ));
                }
                if !group.members.iter().any(|m| m == default) {
                    problems.push(format!(
                        "策略组 {} 的默认成员 {default} 不在成员列表里",
                        group.tag
                    ));
                }
            }
            if let GroupKind::Urltest { interval, .. } = &group.kind {
                if !interval.ends_with(['s', 'm', 'h']) {
                    problems.push(format!(
                        "策略组 {} 的 interval \"{interval}\" 不是时长字符串（应为 3m 这类写法）",
                        group.tag
                    ));
                }
            }
        }
        // 规则集的 tag 与落点是绑死的：tag 派生文件名，`initial_path` 必须指向它。
        // 这两条错了不会让 check 失败，只会让冷启动去现抓网络（schema §11.7 的 FATAL），
        // 所以在这里拦住，不留到运行时赌。
        let mut rs_tags: Vec<&str> = Vec::new();
        for rule_set in &self.rule_sets {
            let tag = rule_set.tag();
            if !is_safe_rule_set_tag(tag) {
                problems.push(format!(
                    "规则集 tag \"{tag}\" 非法：它会被当作文件名使用，\
                     只允许字母、数字、'-'、'_'、'.'，且不得以 '.' 开头"
                ));
            }
            if rs_tags.contains(&tag) {
                problems.push(format!("规则集 tag 重复：{tag}"));
            }
            rs_tags.push(tag);
            if let RuleSet::Remote { initial_path, .. } = rule_set {
                let expected = rule_set_product(tag);
                if *initial_path != expected {
                    problems.push(format!(
                        "规则集 {tag} 的 initial_path 应为 {expected}（ZBox 把编译产物落在按 tag \
                         命名的路径上），当前值 {initial_path} 会让 sing-box 找不到预置文件而去现抓网络"
                    ));
                }
            }
        }
        let all_upstreams: Vec<&Upstream> =
            self.dns.domestic.iter().chain(&self.dns.overseas).collect();
        for upstream in all_upstreams.iter() {
            if upstream.host.trim().is_empty() {
                problems.push(format!("DNS 上游 {} 缺少地址", upstream.tag));
            }
            if upstream.address_is_ip() {
                continue;
            }
            // 地址是域名的上游必须自己带 domain_resolver；VM 实测
            // route.default_domain_resolver 覆盖不到它，sing-box 直接
            // `initialize DNS server[n]: missing domain resolver for domain server address`
            let bootstrappable = all_upstreams
                .iter()
                .any(|o| o.tag != upstream.tag && o.address_is_ip())
                || self.dns.use_system_local;
            if !bootstrappable {
                problems.push(format!(
                    "DNS 上游 {} 的地址 \"{}\" 是域名，但没有任何地址为 IP 的上游或系统解析器\
                     能先把它解析出来（sing-box 会拒绝启动）",
                    upstream.tag, upstream.host
                ));
            }
        }
        if self.dns.fakeip.enabled
            && !(is_cidr(&self.dns.fakeip.inet4_range) && is_cidr(&self.dns.fakeip.inet6_range))
        {
            problems.push("fakeip 段必须是 CIDR 写法".into());
        }
        // 记录保留天数：oxidns 的 query_recorder 要求 retention_days ≥ 1，0 是装载期致命项。
        // 这一条在保存这一关就退回，而不是留到生成时报——面板上那就是个数字框，
        // 填 0 的人以为自己在设"永久"（源码里两个意思都不是）。
        if self.dns.records_enabled && self.dns.records_retention_days == 0 {
            problems.push(
                "查询记录的保留天数不能是 0：oxidns 要求 ≥ 1 天。0 既不是\"永久保留\"也不是\"不留\"，\
                 它会让解析器起不来；不留记录请关掉\"记下每一次查询\"那一格"
                    .into(),
            );
        }
        for rule in &self.policy.domain_rules {
            if rule.pattern.is_empty_name() {
                problems.push(format!("域名规则 {:?} 的目标为空", rule.pattern));
            }
        }
        if let Some(tag) = proxy_group_of(&self.policy) {
            if !self.groups.iter().any(|g| g.tag == tag) && tag != "direct" {
                problems.push(format!("策略引用了不存在的出站/策略组 {tag}"));
            }
        }
        // 设备 IP 在这里必须是一个真的地址。理由不在"写法好看"：`gen::nft` 把 Bypass
        // 设备按族分进两个桶（v4 桶用 `parse::<Ipv4Addr>`、v6 桶用 `Ipv6Addr`），
        // 两边都解析失败的地址会**静默落进不了任何一条链**——用户明明把这台设备排除在外，
        // 它却照常被劫持。这种"设了但没生效"的表现比当场报错难查一个数量级。
        for device in &self.devices {
            let ip = device.ip.trim();
            if ip.parse::<std::net::Ipv4Addr>().is_err()
                && ip.parse::<std::net::Ipv6Addr>().is_err()
            {
                // 名字两侧不留空格：没填主机名与 MAC 时这里插的是"（未命名）"，
                // 带着空格打到屏幕上就是 `设备 （未命名） 的地址`，看着像排版坏了。
                problems.push(format!(
                    "设备{}的地址 \"{ip}\" 既不是 IPv4 也不是 IPv6：\
                     这里要填单个地址，不带斜杠和前缀长度（例如 192.168.1.50），\
                     否则劫持层那条豁免规则永远匹配不上它",
                    device
                        .hostname
                        .as_deref()
                        .or(device.mac.as_deref())
                        .unwrap_or("（未命名）")
                ));
            }
        }
        // 一条原因只列一次：说的是**字面相同**的那几条——两个都没填 tag 的策略组、两台同名
        // 同地址的坏设备，各报两遍等于屏幕上多一行废话。节点那类报错带着节点名，本来每条都
        // 不一样，压不住也不该压；几百个节点的订阅靠 `describe_problems` 的上限收成 12 条。
        // 屏序（网卡 -> 节点 -> 组 -> 规则集 -> DNS -> 名单 -> 设备）保持检查顺序，
        // 不按字典序重排，好让人顺着屏幕从上往下改。
        let mut unique: Vec<String> = Vec::new();
        for problem in problems {
            if !unique.contains(&problem) {
                unique.push(problem);
            }
        }
        if unique.is_empty() {
            Ok(())
        } else {
            Err(unique)
        }
    }
}

/// 一份问题清单 -> 给人读的那一段。**措辞与排版只住这一处**：`state::parse`（CLI 读盘与
/// 面板 PUT 同一条路）、`zboxd doctor`、`gen::singbox` 那道门、面板导入订阅那条 400
/// 四条出口都从这里取，各自重写一遍迟早写出四个版本（"改一条撞一条"
/// 就是那份只拿第一条的写法留下的）。
///
/// 这里给的是**清单本身**，不带"某某不成立："那种动作标签：标签属于出口，不属于共享串。
/// 四条出口本来就各自有一句框（面板那句是 `保存被拒（HTTP 400）：`、doctor 那条是
/// `期望状态本身不成立，生成器已拒绝`），共享串再顶一个，屏幕上就是两层标签叠在一句里
/// （2026-10-12 §11.13 边界 1 记的就是这一句：`保存被拒（HTTP 400）：期望状态不成立: 共 3 处：`，
/// 那个 ASCII 冒号还是从 `state::parse` 漏出来的）。所以帽子自己要把主语说出来：
/// "这几处不成立，要改" ——它跟在出口那句之后是一句话，单独出现在 CLI 上（前面只有文件路径）
/// 也是一句话，两种都不需要谁再补一个标签。
///
/// - 只有一条时不加帽子——一条就是平时那一句报错，加前缀反而像出了大事；
/// - 多条时**一条一行**（面板上 `#save-out` 是 `pre-line`，一行一条才数得清还剩几处）；
/// - 超过 [`PROBLEM_CAP`] 条只列前那么多条并明说还有几条：一份坏订阅能报出几百条各带节点名
///   的错，全列出来等于没列，而"还有 137 处"这句本身就是要告诉人的量级。
pub fn describe_problems(problems: &[String]) -> String {
    if problems.is_empty() {
        return String::new();
    }
    if problems.len() == 1 {
        return problems[0].clone();
    }
    const PROBLEM_CAP: usize = 12;
    let shown = problems.len().min(PROBLEM_CAP);
    let mut out = format!("这 {} 处不成立，要改：", problems.len());
    for (index, problem) in problems[..shown].iter().enumerate() {
        out.push_str(&format!("\n{}）{problem}", index + 1));
    }
    if problems.len() > shown {
        out.push_str(&format!(
            "\n…另有 {} 处同类，改完上面这些再看下一批。",
            problems.len() - shown
        ));
    }
    out
}

/// 最终出口用到的策略组 tag
pub fn proxy_group_of(policy: &Policy) -> Option<String> {
    match &policy.final_outbound {
        FinalOutbound::Proxy(tag) => Some(tag.clone()),
        FinalOutbound::Direct => None,
    }
}

/// 只做形状检查，不解析地址：真正的合法性由内核和 sing-box 判定
pub(crate) fn is_cidr(raw: &str) -> bool {
    match raw.split_once('/') {
        Some((addr, prefix)) => {
            !addr.trim().is_empty() && !prefix.is_empty() && prefix.parse::<u8>().is_ok()
        }
        None => false,
    }
}

/// 局域网 IPv6 前缀的写法。这一格会被原样拼进 nft 的 `ip6 saddr != …`，所以要比
/// [`is_cidr`] 严：地址部分真能按 `Ipv6Addr` 解析、前缀不许超过 128、而且必须是 v6
/// （`192.168.1.0/24` 填在这里拦得住，否则它一路拼到 nft 里才炸，那一声响在加载期）。
pub(crate) fn is_cidr6(raw: &str) -> bool {
    let Some((addr, prefix)) = raw.split_once('/') else {
        return false;
    };
    let Ok(len) = prefix.parse::<u8>() else {
        return false;
    };
    len <= 128 && addr.contains(':') && addr.parse::<std::net::Ipv6Addr>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::uri::parse_link;

    /// `validate()` 一次列全之后给的是清单，而这里每一处断言问的仍然是"这一句在不在里面"，
    /// 所以统一把清单并成一个串再查——顺带让 `{err}` 那种插值有东西可打印。
    fn problems(state: &DesiredState) -> String {
        state.validate().unwrap_err().join("\n")
    }

    #[test]
    fn domain_pattern_syntax() {
        assert_eq!(
            DomainPattern::parse("+.example.com"),
            Some(DomainPattern::Suffix("example.com".into()))
        );
        assert_eq!(
            DomainPattern::parse("+example.com"),
            Some(DomainPattern::Suffix("example.com".into()))
        );
        assert_eq!(
            DomainPattern::parse(".example.com"),
            Some(DomainPattern::Suffix("example.com".into()))
        );
        assert_eq!(
            DomainPattern::parse("exact.com"),
            Some(DomainPattern::Exact("exact.com".into()))
        );
        assert_eq!(
            DomainPattern::parse("~porn"),
            Some(DomainPattern::Keyword("porn".into()))
        );
        assert_eq!(
            DomainPattern::parse("/^x[0-9]+\\.com$/"),
            Some(DomainPattern::Regex("^x[0-9]+\\.com$".into()))
        );
        assert_eq!(DomainPattern::parse("# 注释"), None);
        assert_eq!(DomainPattern::parse("  "), None);
    }

    #[test]
    fn raw_is_the_inverse_of_parse_up_to_the_equivalent_dot_form() {
        for raw in ["exact.com", "+example.com", "~porn", "/^x[0-9]+\\.com$/"] {
            assert_eq!(
                DomainPattern::parse(raw).unwrap().raw(),
                raw,
                "{raw} 该原样回到写法"
            );
        }
        // 裸 .example.com 与 +example.com 同义，归一成 + 写法
        assert_eq!(
            DomainPattern::parse(".example.com").unwrap().raw(),
            "+example.com"
        );
    }

    #[test]
    fn baseline_is_all_direct() {
        let state = DesiredState::baseline();
        state.validate().unwrap();
        assert!(state.nodes.is_empty());
        assert!(state.rule_sets.is_empty());
        assert_eq!(state.policy.final_outbound, FinalOutbound::Direct);
        assert!(!state.dns.fakeip.enabled);
        assert!(state.dns.overseas.is_empty());
        // 基线不该依赖任何外部规则集，否则首次下发会在没网时失败
        assert!(!state.policy.cn_direct);
        assert!(!state.policy.block_ads);
    }

    /// 智能分流那一档一次动**五格**。少任何一格，下面某一扇门就过不去：
    /// `validate()`（"策略引用了不存在的出站"）、`gen::oxidns::check_available`
    /// （境外上游空着整份渲染失败）、或 `gen::singbox::route` 的 `require_rule_set`。
    /// 所以这一条钉的不是"改了哪几格"，是"这一档成立所需的最小集合齐不齐"。
    #[test]
    fn smart_split_sets_every_cell_the_default_needs() {
        let mut state = DesiredState::baseline();
        state.nodes = vec![
            parse_link("trojan://pw@1.2.3.4:443#东京").unwrap(),
            parse_link("trojan://pw2@5.6.7.8:443#圣何塞").unwrap(),
        ];
        assert_eq!(
            state.enable_smart_split().as_deref(),
            Some(SMART_SPLIT_GROUP)
        );
        state.validate().expect("设完该过门");
        assert!(state.policy.private_direct);
        assert!(state.policy.cn_direct);
        assert_eq!(
            state.policy.final_outbound,
            FinalOutbound::Proxy(SMART_SPLIT_GROUP.into())
        );
        assert_eq!(state.groups.len(), 1);
        assert_eq!(state.groups[0].tag, SMART_SPLIT_GROUP);
        assert_eq!(state.groups[0].members, vec!["东京".to_string(), "圣何塞".to_string()]);
        assert!(matches!(state.groups[0].kind, GroupKind::Urltest { .. }));
        // urltest 带 default 会被 validate 拒（"1.14.2 没有默认成员这个概念"）
        assert_eq!(state.groups[0].default, None);
        assert_eq!(
            state.dns.overseas,
            vec![overseas_upstream(SMART_SPLIT_GROUP)]
        );
        // 从 builtin 那一份筛出来的三条，顺序就是 `builtin_rule_sets()` 里的顺序
        let tags: Vec<&str> = state.rule_sets.iter().map(|t| t.tag()).collect();
        assert_eq!(tags, vec![RULESET_CN_DOMAIN, RULESET_NON_CN, RULESET_CN_IP]);
        // 广告与 BT 不是"默认怎么走"这一档说的话，一格都不替人动
        assert!(!state.policy.block_ads);
        assert!(!state.policy.bittorrent_direct);
    }

    /// 这一档的前提是"手上至少有一个出口"。没有出口时**一个字节都不改**——
    /// 写一个空成员的组下去，表现是整个面板读不回来（validate 拒空成员）。
    #[test]
    fn smart_split_refuses_when_there_is_no_egress_at_all() {
        let untouched = DesiredState::baseline();
        let mut state = untouched.clone();
        assert_eq!(state.enable_smart_split(), None);
        assert_eq!(state, untouched, "没有出口时这一按该什么都没改");
    }

    /// 订阅里真有机场把节点命名成 `auto`（常见命名），而组名与节点重名是 validate 硬拒的。
    /// 让位不是报错，因为"设默认档"这件事仍然成立——回执念的必须是让位之后的那一个名字。
    #[test]
    fn a_subscribed_node_called_auto_pushes_the_fallback_group_down() {
        let mut state = DesiredState::baseline();
        state.nodes = vec![parse_link("trojan://pw@1.2.3.4:443#auto").unwrap()];
        assert_eq!(state.enable_smart_split().as_deref(), Some("auto-2"));
        state.validate().expect("让位之后该过门");
        assert_eq!(
            state.policy.final_outbound,
            FinalOutbound::Proxy("auto-2".into())
        );
        assert_eq!(state.groups[0].tag, "auto-2");
        assert_eq!(state.dns.overseas, vec![overseas_upstream("auto-2")]);
    }

    /// 境外上游的形状只许有一份：出厂默认值和这一档设的是同一个构造器。
    /// 两处各写一遍，将来错开的表现是"导入后磁盘上那条境外上游与面板默认值不是同一个东西"。
    #[test]
    fn the_default_overseas_upstream_and_the_smart_split_one_are_one_shape() {
        assert_eq!(Dns::default().overseas, vec![overseas_upstream("proxy")]);
        assert_eq!(
            smart_split_rule_sets().len(),
            3,
            "这一档只要 cn 的两份数据加非 cn 那一份，广告那条不属于它"
        );
    }

    #[test]
    fn upstream_transport_maps_to_singbox_server_types() {
        assert_eq!(UpstreamTransport::DoH.singbox_type(), "https");
        assert_eq!(UpstreamTransport::Dot.singbox_type(), "tls");
        assert_eq!(UpstreamTransport::DoH3.singbox_type(), "h3");
        assert_eq!(UpstreamTransport::Udp.default_port(), 53);
        assert_eq!(UpstreamTransport::DoH.default_port(), 443);
    }

    #[test]
    fn duplicate_tags_and_dangling_group_refs_are_rejected() {
        let mut state = DesiredState::baseline();
        let node = parse_link("trojan://pw@1.2.3.4:443#同名").unwrap();
        state.nodes = vec![node.clone(), node.clone()];
        assert!(problems(&state).contains("出站 tag 重复"));

        let mut state = DesiredState::baseline();
        state.nodes = vec![node.clone()];
        state.policy.final_outbound = FinalOutbound::Proxy("不存在".into());
        assert!(problems(&state).contains("不存在的出站"));
    }

    /// 一次列全住在这里：这份夹具在**五扇不同的屏**上各留一处坏，一次读取就要拿到五条。
    /// 老的"命中第一条就 return"写法在这里只会剩一条，用户就得改一条按一次保存、
    /// 再撞出下一条——那是这一格要消灭的表现，不是这一格的功能。
    ///
    /// 顺序也一起钉住：清单的顺序就是屏序（网卡 → 组 → DNS → 名单 → 设备），
    /// 也就是"顺着屏幕从上往下改"的顺序。
    #[test]
    fn one_pass_lists_every_broken_cell_in_screen_order() {
        use crate::model::device::{Device, DeviceMode};

        let mut state = DesiredState::baseline();
        state.network.lan_cidr = "192.168.1.0".into();
        state.groups = vec![Group {
            tag: "auto".into(),
            kind: GroupKind::Selector,
            members: vec![],
            default: None,
        }];
        state.dns.records_retention_days = 0;
        state.policy.domain_rules = vec![DomainRule::new(
            DomainPattern::Suffix("  ".into()),
            DomainAction::Direct,
        )];
        state.devices = vec![Device {
            ip: "tv.lan".into(),
            mac: None,
            hostname: Some("电视".into()),
            mode: DeviceMode::Direct,
            ip_type: crate::model::device::IpType::Dhcp,
            display_name: None,
        }];

        let got = state.validate().unwrap_err();
        assert_eq!(got.len(), 5, "{got:?}");
        for (index, expect) in [
            "CIDR",
            "没有任何成员",
            "保留天数",
            "目标为空",
            "既不是 IPv4",
        ]
        .iter()
        .enumerate()
        {
            assert!(got[index].contains(*expect), "第 {} 条是 {:?}，要的是 {expect}", index + 1, got[index]);
        }
    }

    /// 去重只压**字面相同**的那几条：两台同名同地址的坏设备报的是同一句话，列两遍是废话。
    /// 带名字的报错（每个节点、每台有名字的设备）本来就不是同一条，压不掉也不该压掉——
    /// 那正是"改哪一台"的信息。
    #[test]
    fn the_same_wording_from_two_cells_is_listed_once() {
        use crate::model::device::{Device, DeviceMode};

        let mut state = DesiredState::baseline();
        state.devices = vec![
            Device {
                ip: "tv.lan".into(),
                mac: None,
                hostname: None,
                mode: DeviceMode::Global,
                ip_type: crate::model::device::IpType::Dhcp,
                display_name: None,
            },
            Device {
                ip: "tv.lan".into(),
                mac: None,
                hostname: None,
                mode: DeviceMode::Proxy,
                ip_type: crate::model::device::IpType::Dhcp,
                display_name: None,
            },
        ];
        let got = state.validate().unwrap_err();
        assert_eq!(got.len(), 1, "同一句话列了两遍：{got:?}");
        assert!(got[0].contains("（未命名）"), "{got:?}");
        assert!(
            got[0].starts_with("设备（未命名）的地址"),
            "名字两侧留了空格，屏幕上看着像排版坏了：{}", got[0]
        );

        let mut state = DesiredState::baseline();
        let anonymous = || Group {
            tag: String::new(),
            kind: GroupKind::Selector,
            members: vec![],
            default: None,
        };
        state.groups = vec![anonymous(), anonymous()];
        let got = state.validate().unwrap_err();
        // 两个组各产一条"缺 tag"、一条"空成员"，去重后每样只剩一条。
        let tagged = got.iter().filter(|p| p.contains("策略组缺少 tag")).count();
        assert_eq!(tagged, 1, "同一句话列了两遍：{got:?}");
        let empty = got.iter().filter(|p| p.contains("没有任何成员")).count();
        assert_eq!(empty, 1, "同一句话列了两遍：{got:?}");
        // §11.13 边界 3：两个匿名组**不算撞名**。以前这条会多出一句
        // `策略组 tag "" 与节点重名`——撞的是组不是节点，而那个组压根没有名字，
        // 用户按这句话去节点那一屏找一个不存在的重名，永远找不到。
        assert!(
            !got.iter().any(|p| p.contains("重名")),
            "空 tag 被当成撞名了：{got:?}"
        );
    }

    /// 撞名要说清撞的是谁（§11.13 边界 3 的另一半）：组撞节点与组撞组是两扇不同的屏，
    /// 拿同一句"与节点重名"盖住两种，人就会去错的那一屏找。
    #[test]
    fn a_tag_collision_names_the_side_it_collided_with() {
        let mut state = DesiredState::baseline();
        state.nodes = vec![parse_link("trojan://pw@1.2.3.4:443#proxy").unwrap()];
        state.groups = vec![Group {
            tag: "proxy".into(),
            kind: GroupKind::Selector,
            members: vec!["direct".into()],
            default: None,
        }];
        let got = problems(&state);
        assert!(got.contains("与节点重名"), "{got}");

        let mut state = DesiredState::baseline();
        let named = |tag: &str| Group {
            tag: tag.into(),
            kind: GroupKind::Selector,
            members: vec!["direct".into()],
            default: None,
        };
        state.groups = vec![named("auto"), named("auto")];
        let got = problems(&state);
        assert!(got.contains("与另一个策略组重名"), "{got}");
        assert!(
            !got.contains("与节点重名"),
            "两个组相撞却被说成撞了节点：{got}"
        );
    }

    /// 一条的时候它就是那句普通报错。"这 1 处不成立，要改："那顶帽子会把一次平常的退回说成一件大事，
    /// 而面板上红字本来就已经够响亮了。
    #[test]
    fn a_single_problem_prints_as_a_plain_sentence() {
        assert_eq!(describe_problems(&[]), "");
        assert_eq!(
            describe_problems(&["缺少局域网网卡名".to_string()]),
            "缺少局域网网卡名"
        );
    }

    /// 多条：帽子报总数，下面一行一条、带编号。排版只有这一份，CLI / `doctor` / 面板 400
    /// 三条出口都从这里取——各自 rewrite 一遍迟早写出三个版本。
    ///
    /// 帽子自己带主语（"这 N 处不成立，要改"）是**边界 1 的修法**：共享串里不许有动作标签
    /// （"保存被拒:" / "期望状态不成立:" 那种"标签 + 冒号"），否则它跟在出口那句之后
    /// 就是两层标签叠在一句里。反向钉住这一条：清单里再出现那种标签就说明有人把帽子装回去了。
    #[test]
    fn several_problems_print_one_per_line_with_a_count() {
        let list = ["甲", "乙", "丙"]
            .iter()
            .map(|s| (*s).to_string())
            .collect::<Vec<String>>();
        let got = describe_problems(&list);
        assert_eq!(got, "这 3 处不成立，要改：\n1）甲\n2）乙\n3）丙");
        assert!(
            !got.contains("期望状态不成立") && !got.contains("保存被拒"),
            "清单里又长了动作标签：{got}"
        );
    }

    /// 超过上限只列前 12 条，但**总数照实说**。把 137 条缩成"这 12 处"会让人以为
    /// 改完这 12 条就干净了，而真相是还有一大批。
    #[test]
    fn a_long_list_is_capped_but_still_tells_the_true_total() {
        let list: Vec<String> = (0..13).map(|index| format!("第 {index} 条")).collect();
        let got = describe_problems(&list);
        assert!(got.starts_with("这 13 处不成立，要改："), "{got}");
        assert_eq!(got.lines().count(), 14, "帽子 + 12 条 + 一句剩下的：{got}");
        assert!(got.contains("\n12）第 11 条"), "{got}");
        assert!(!got.contains("第 12 条"), "超出上限的那条不该出现在清单里：{got}");
        assert!(
            got.ends_with("…另有 1 处同类，改完上面这些再看下一批。"),
            "{got}"
        );
    }

    #[test]
    fn state_round_trips_through_json() {
        // 面板与磁盘上的状态文件都走 serde，这里的形状就是对外契约。
        // `model::state` 只在这个形状外面加一个 `schema` 键和版本门，字段本身归这里管。
        let mut state = DesiredState::baseline();
        state.nodes = vec![parse_link(
            "vless://11111111-2222-3333-4444-555555555555@a.com:443?security=reality&sni=a.com&pbk=P&sid=s#A",
        )
        .unwrap()];
        state.groups = vec![Group {
            tag: "proxy".into(),
            kind: GroupKind::Selector,
            members: vec!["A".into()],
            default: Some("A".into()),
        }];
        state.policy.final_outbound = FinalOutbound::Proxy("proxy".into());
        state.policy.domain_rules = vec![
            DomainRule::new(
                DomainPattern::Suffix("example.com".into()),
                DomainAction::Direct,
            ),
            DomainRule::new(DomainPattern::Exact("ad.test".into()), DomainAction::Block),
        ];
        state.rule_sets = builtin_rule_sets();
        let text = serde_json::to_string(&state).unwrap();
        let back: DesiredState = serde_json::from_str(&text).unwrap();
        assert_eq!(back, state);
        assert!(text.contains("\"kind\":\"selector\""), "{text}");
        assert!(text.contains("\"doh\""), "{text}");
        // 有序名单的磁盘形状：目标还是那四种匹配类型之一，处置是三个蛇形名之一
        assert!(
            text.contains(r#""pattern":{"suffix":"example.com"},"action":"direct""#),
            "{text}"
        );
        assert!(
            text.contains(r#""pattern":{"exact":"ad.test"},"action":"block""#),
            "{text}"
        );
    }

    #[test]
    fn an_empty_domain_target_is_rejected_whatever_its_action() {
        for action in [
            DomainAction::Direct,
            DomainAction::Proxy,
            DomainAction::Block,
        ] {
            let mut state = DesiredState::baseline();
            state.policy.domain_rules =
                vec![DomainRule::new(DomainPattern::Suffix("  ".into()), action)];
            let err = problems(&state);
            assert!(err.contains("目标为空"), "{action:?}: {err}");
        }
    }

    #[test]
    fn empty_group_and_bad_interval_are_rejected() {
        let mut state = DesiredState::baseline();
        state.groups = vec![Group {
            tag: "proxy".into(),
            kind: GroupKind::Selector,
            members: vec![],
            default: None,
        }];
        assert!(problems(&state).contains("没有任何成员"));

        let mut state = DesiredState::baseline();
        state.groups = vec![Group {
            tag: "auto".into(),
            kind: GroupKind::Urltest {
                url: "http://www.gstatic.com/generate_204".into(),
                interval: "180".into(),
                tolerance: 50,
            },
            members: vec!["x".into()],
            default: None,
        }];
        state.nodes = vec![parse_link("trojan://pw@1.2.3.4:443#x").unwrap()];
        assert!(problems(&state).contains("时长字符串"));
    }

    #[test]
    fn builtin_rule_sets_point_at_the_rule_set_branch() {
        // release 附件里只有 .db，`releases/latest/download/*.srs` 实测全部 404
        let sets = builtin_rule_sets();
        assert_eq!(sets.len(), 4);
        for rule_set in &sets {
            let RuleSet::Remote { url, .. } = rule_set else {
                panic!("内置规则集应是可远程更新的: {rule_set:?}");
            };
            assert!(
                url.starts_with("https://raw.githubusercontent.com/SagerNet/"),
                "{url}"
            );
            assert!(url.contains("/rule-set/"), "{url}");
            assert!(!url.contains("releases/"), "{url} 是 404 的写法");
        }
        // 落点由 tag 派生，不是上游文件名：`non-cn` 两项就是用来钉住这个区别的
        for rule_set in &sets {
            let RuleSet::Remote { initial_path, .. } = rule_set else {
                panic!("内置规则集应是可远程更新的: {rule_set:?}");
            };
            assert_eq!(
                *initial_path,
                rule_set_product(rule_set.tag()),
                "{} 的 initial_path 与 tag 不一致，预置产物不会被读到",
                rule_set.tag()
            );
        }
        assert!(!sets.iter().any(|s| s.tag() == RULESET_PRIVATE_IP));
    }

    #[test]
    fn rule_set_landing_path_is_locked_to_the_tag() {
        let mut state = DesiredState::baseline();
        state.rule_sets = builtin_rule_sets();
        state.validate().unwrap();

        let remote = |tag: &str, initial_path: &str| RuleSet::Remote {
            tag: tag.into(),
            url: "https://x/y.srs".into(),
            initial_path: initial_path.into(),
            update_interval: "168h".into(),
        };

        // 照抄上游文件名：ZBox 落在 rule-set/<tag>.srs，sing-box 找不到就要在启动预算里现抓
        let mut state = DesiredState::baseline();
        state.rule_sets = vec![remote("non-cn", "rule-set/geosite-geolocation-!cn.srs")];
        assert!(problems(&state).contains("initial_path"));

        let mut state = DesiredState::baseline();
        state.rule_sets = vec![remote("../escape", "rule-set/escape.srs")];
        assert!(problems(&state).contains("非法"));

        let mut state = DesiredState::baseline();
        state.rule_sets = vec![
            remote("cn", "rule-set/cn.srs"),
            remote("cn", "rule-set/cn.srs"),
        ];
        assert!(problems(&state).contains("tag 重复"));
    }

    #[test]
    fn domain_addressed_upstream_needs_an_ip_bootstrap() {
        // VM 实测：route.default_domain_resolver 覆盖不到 dns.servers[]，
        // 地址是域名的上游没有自己的解析器时 sing-box 直接
        // `initialize DNS server[n]: missing domain resolver for domain server address`
        let mut state = DesiredState::baseline();
        state.dns.domestic = vec![Upstream {
            tag: "doh".into(),
            host: "dns.google".into(),
            transport: UpstreamTransport::DoH,
            path: Some("/dns-query".into()),
            ..Upstream::default()
        }];
        state.dns.use_system_local = false;
        let err = problems(&state);
        assert!(err.contains("没有任何地址为 IP 的上游"), "{err}");

        // 兜底一：加一条地址是 IP 的上游
        state.dns.domestic.push(Upstream::default());
        state.validate().unwrap();

        // 兜底二：只有系统解析器也行
        state.dns.domestic.pop();
        state.dns.use_system_local = true;
        state.validate().unwrap();
    }

    /// 记录这一格默认是"开"：老状态文件里没这两格时要按开着读（`#[serde(default)]` 会给出
    /// `false`，等于每台升级上来的机器静默丢掉观测面），而 0 天在保存这一关就退回。
    #[test]
    fn records_default_to_on_and_zero_days_is_refused_at_save() {
        // 老状态文件里没有这两格：把它们从一份完整 DNS 的 JSON 里抹掉再读回来
        let mut legacy = serde_json::to_value(Dns::default()).unwrap();
        let obj = legacy.as_object_mut().unwrap();
        assert!(obj.remove("records_enabled").is_some());
        assert!(obj.remove("records_retention_days").is_some());
        let parsed: Dns = serde_json::from_value(legacy).unwrap();
        assert!(parsed.records_enabled, "缺省要把记录当开着");
        assert_eq!(parsed.records_retention_days, 7);

        let mut state = DesiredState::baseline();
        assert!(state.dns.records_enabled);
        state.validate().unwrap();

        state.dns.records_retention_days = 0;
        let err = problems(&state);
        assert!(err.contains("不能是 0"), "{err}");

        // "不留"是另一格：关掉记录之后 0 天不再拦路（那一份里根本没有 retention_days）
        state.dns.records_enabled = false;
        state.validate().unwrap();
    }

    /// v6 前缀那一格会被原样拼进 nft 的 `ip6 saddr`，所以校验要拦得住"看着像 CIDR
    /// 但其实是 v4"和"前缀写成 129"这两种——它们在 `is_cidr` 那种松写法下都算过。
    #[test]
    fn the_v6_prefix_takes_a_real_v6_cidr() {
        for good in ["fd00:db8:1::/64", "2001:db8::/32", "::/0", "fe80::/10"] {
            assert!(is_cidr6(good), "{good} 是合法的 v6 段");
        }
        for bad in [
            "192.168.1.0/24",     // v4 段填错了栏：进 nft 要等加载期才炸
            "fd00:db8::/129",     // 前缀出界
            "fd00:db8::",         // 没写前缀
            "fd00:db8::/64/64",   // 多写一段
            "fd00:db8::%eth0/64", // 带 zone 的写法解析不了，也不该进脚本
        ] {
            assert!(!is_cidr6(bad), "{bad} 不该算合法");
        }
    }

    /// Bypass 设备的地址按族分桶，两边都解析不了的写法会静默丢掉那条豁免规则——
    /// 用户以为这台设备不经本机，实际上它照常被劫持。所以这种地址在保存时就报错。
    #[test]
    fn device_ips_must_be_a_real_single_address() {
        use crate::model::device::{Device, DeviceMode};

        let mut s = DesiredState::baseline();
        s.devices = vec![Device {
            ip: "192.168.1.50".into(),
            mac: None,
            hostname: Some("电视".into()),
            mode: DeviceMode::Bypass,
            ip_type: crate::model::device::IpType::Dhcp,
            display_name: None,
        }];
        s.validate().unwrap();

        // v6 地址进得了 v6 桶，同样是合法的设备行
        s.devices.push(Device {
            ip: "fd00:db8:1::30".into(),
            mac: None,
            hostname: None,
            mode: DeviceMode::Bypass,
            ip_type: crate::model::device::IpType::Dhcp,
            display_name: None,
        });
        s.validate().unwrap();

        for bad in [
            "",
            "192.168.1.50/32",
            "192.168.1.999",
            "tv.lan",
            "0192.168.1.5",
        ] {
            s.devices.last_mut().unwrap().ip = bad.into();
            let err = problems(&s);
            assert!(
                err.contains("既不是 IPv4 也不是 IPv6"),
                "{bad} 这种写法要当场拦住：{err}"
            );
        }

        // 报错要能认出是哪一台：没有 hostname 时退到 MAC
        s.devices.pop();
        s.devices.push(Device {
            ip: "not-an-ip".into(),
            mac: Some("aa:bb:cc:dd:ee:ff".into()),
            hostname: None,
            mode: DeviceMode::Global,
            ip_type: crate::model::device::IpType::Dhcp,
            display_name: None,
        });
        assert!(problems(&s).contains("aa:bb:cc:dd:ee:ff"));
    }

    #[test]
    fn taking_over_ipv6_requires_the_v6_prefix() {
        // 不接管时留空是正常形状，别拦住第一份状态文件
        DesiredState::baseline().validate().unwrap();

        for mode in [Ipv6Mode::DnsOnly, Ipv6Mode::DualStack] {
            let mut s = DesiredState::baseline();
            s.network.ipv6_mode = mode;
            let err = problems(&s);
            assert!(err.contains("lan_cidr6"), "{mode:?} 留空要报错：{err}");

            s.network.lan_cidr6 = "fd00:db8:1::/64".into();
            s.validate().unwrap();

            s.network.lan_cidr6 = "192.168.1.0/24".into();
            assert!(
                problems(&s).contains("IPv6 前缀"),
                "{mode:?}：v4 段填在 v6 栏要报错"
            );
        }
    }

    #[test]
    fn urltest_does_not_take_a_default_member() {
        // 1.14.2 实测：urltest 的 schema 里没有 default，写上去 sing-box check 就失败。
        // 这里拦下来而不是生成时静默丢掉，免得面板上"设了默认"其实没生效。
        let mut state = DesiredState::baseline();
        state.nodes = vec![parse_link("trojan://pw@1.2.3.4:443#x").unwrap()];
        state.groups = vec![Group {
            tag: "auto".into(),
            kind: GroupKind::Urltest {
                url: "http://www.gstatic.com/generate_204".into(),
                interval: "3m".into(),
                tolerance: 50,
            },
            members: vec!["x".into()],
            default: Some("x".into()),
        }];
        assert!(problems(&state).contains("urltest"));

        let mut state = state;
        state.groups[0].kind = GroupKind::Selector;
        state.validate().expect("selector 有 default 是合法的");
    }

    #[test]
    fn the_two_route_switches_are_optional_on_disk() {
        // v2 的老文件里没有这两个键。加了它们不改 schema 版本号，靠 serde 默认值兜住，
        // 所以"装了新版面板读旧状态"和"旧版面板读新状态"两个方向都不会报错。
        let mut value: serde_json::Value =
            serde_json::from_str(&crate::model::state::to_text(&DesiredState::baseline())).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("apply_policy")
            .unwrap();
        value["network"]
            .as_object_mut()
            .unwrap()
            .remove("route_mode")
            .unwrap();
        let back = crate::model::state::parse(&value.to_string()).unwrap();
        assert_eq!(back.apply_policy, ApplyPolicy::Reload);
        assert_eq!(back.network.route_mode, RouteMode::Inject);
        assert_eq!(back, DesiredState::baseline());
    }

    #[test]
    fn the_two_route_switches_land_in_the_disk_shape() {
        // 面板上这两格要能在磁盘形状里查到，否则"UI 改了 = 配置改了"这条判据断了
        let mut state = DesiredState::baseline();
        state.network.route_mode = RouteMode::Takeover;
        state.apply_policy = ApplyPolicy::Restart;
        let value: serde_json::Value =
            serde_json::from_str(&crate::model::state::to_text(&state)).unwrap();
        assert_eq!(
            value["network"]["route_mode"],
            serde_json::json!("takeover")
        );
        assert_eq!(value["apply_policy"], serde_json::json!("restart"));
    }
}
