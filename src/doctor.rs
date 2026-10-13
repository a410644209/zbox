//! 一键诊断：把**期望状态**和**现场读数**放在一起比对，输出可读的结论 + 可粘贴的命令。
//!
//! 思路取自参考实现的《故障排查》文档：它有效的原因不是"写了多少条 FAQ"，而是把
//! "配了但没生效"拆成了一串**期望值 vs 实际值**的判据。ZBox 把它做成代码，面板上一个按钮
//! 就能跑完。
//!
//! 两条刻意的设计约束：
//! - `Reading` 的字段全是 `Option`。svc 层探针拿不到的一律 `None`，诊断器**不许把
//!   "没读到"当成"读到了不符合"**——那会在 Windows 开发机上刷出一屏假故障。
//! - 判据的严重程度由期望状态决定，不是固定值。例如同一条"nft 表不存在"：基线（全直连、
//!   还没开劫持）下只是提示，而最终出口已指向代理策略组时是致命——后者才是
//!   "配置下发成功、但什么都没代理"这种最难查的形态。

use crate::gen::init::Layout;
use crate::gen::netstate;
use crate::gen::singbox::route::DIVERT_MARK;
use crate::model::config::{
    describe_problems, DesiredState, DnsStrategy, DomainAction, DomainPattern, FinalOutbound,
    Ipv6Mode, Network, Via, DIRECT_TAG, REJECT_TAG,
};
use crate::model::device::DeviceMode;
use serde::Serialize;
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// 不修就一定不工作或会断网
    Fatal,
    /// 能工作，但表现会和预期不一致
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// 稳定机器码：面板按它跳转设置项，不跟着中文措辞变
    pub code: &'static str,
    pub severity: Severity,
    pub summary: String,
    /// 期望是什么
    pub expected: String,
    /// 现场实际是什么
    pub actual: String,
    /// 可直接粘贴执行的命令；纯提示性的问题为空
    pub fix: Vec<String>,
}

impl Finding {
    fn new(
        code: &'static str,
        severity: Severity,
        summary: &str,
        expected: String,
        actual: String,
    ) -> Self {
        Self {
            code,
            severity,
            summary: summary.into(),
            expected,
            actual,
            fix: Vec::new(),
        }
    }

    fn with_fix(mut self, fix: Vec<String>) -> Self {
        self.fix = fix;
        self
    }

    pub fn is_fatal(&self) -> bool {
        self.severity == Severity::Fatal
    }
}

/// svc 层从目标机读到的现场。`None` 表示"没读到"，与 `Some(false)` 含义完全不同。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Reading {
    pub service_active: Option<bool>,
    /// 本机正在监听的端口（TCP 与 UDP、两个地址族合在一份里）
    pub listening: Option<Vec<u16>>,
    pub nft_table_loaded: Option<bool>,
    pub divert_rule_loaded: Option<bool>,
    pub ip_forward: Option<u8>,
    pub rp_filter_all: Option<u8>,
    /// 默认路由所在网卡名
    pub default_route_iface: Option<String>,
    /// **IPv6 那一侧的监听**（`ss -lntu -6` 的端口）。不许拿 [`Self::listening`] 代替：
    /// 2026-10-09 实测一只 socket 绑 `::` 时 `ss` 打的是 `*:端口`，`ss -lntu -4` 里
    /// **一行都没有**，所以族只能由"带不带 `-6`"决定，不能由地址字面量猜。
    pub v6_listening: Option<Vec<u16>>,
    pub v6_table_loaded: Option<bool>,
    pub v6_divert_rule_loaded: Option<bool>,
    /// `net.ipv6.conf.all.forwarding`。整机一个键，不是按网卡的。
    pub v6_forward_all: Option<u8>,
    /// `net.ipv6.conf.all.accept_ra`。与上一条成对判，见 `inspect` 里那条致命项。
    pub v6_accept_ra_all: Option<u8>,
}

/// 完整诊断：静态自检在前，现场比对在后。返回空列表代表没有发现问题。
pub fn diagnose(state: &DesiredState, reading: &Reading, layout: &Layout) -> Vec<Finding> {
    let mut out = self_check(state);
    out.extend(inspect(state, reading, layout));
    out
}

/// 磁盘上那份 `network` **还等于出厂默认**、而本机探出来的是另一张网卡时的那一条。
///
/// 为什么只报这一种：`state.network != Network::default()` 说明有人显式写过，那是人的判断，
/// 机器不跟人抢（`svc::topology` 开头那条硬规则），面板「本机网卡」那一行给出现场读数让人自己比；
/// 而**没人写过**的机器上，出厂默认的 `ens33` / `192.168.1.0/24` 会一路进生成物——
/// dae 的 `lan_interface`、nft 的 `oifname`、sysctl 的键名都带着一个本机根本没有的网卡名，
/// 那声响要到数据面起不来才听得见（彩排那次就是这样静默写出一份永不生效的 sysctl 键）。
///
/// 探不到网卡形状时**不调用这一条**（调用方负责）：那是"没读到"，不是"配错了"，
/// 拿它点亮一块开发机（`ip` 跑不起来）等于造假故障。
pub fn network_shape_finding(state: &DesiredState, probed: &Network) -> Option<Finding> {
    // 有人写过就不报：那是人的判断
    if state.network != Network::default() {
        return None;
    }
    // 探来的正好和默认一致（这台机器确实就长在 192.168.1.0/24 上）也没什么好说的
    if &state.network == probed {
        return None;
    }
    Some(Finding::new(
        "network_still_default",
        Severity::Warn,
        "网卡那几格还是出厂默认，本机探到的是另一张",
        format!(
            "局域网 {}/段 {}/本机地址 {}",
            probed.lan_interface, probed.lan_cidr, probed.lan_address
        ),
        format!(
            "现在用的是：局域网 {}/段 {}{}",
            state.network.lan_interface,
            state.network.lan_cidr,
            if state.network.lan_address.is_empty() {
                "（本机地址没填）"
            } else {
                ""
            }
        ),
    ))
}

/// 只看期望状态就能发现的问题。下发之前也该跑一遍——这些错误写进 nft/sysctl 之后
/// 表现为内核报错，很难回溯到"其实是面板里填错了"。
pub fn self_check(state: &DesiredState) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Err(problems) = state.validate() {
        out.push(Finding::new(
            "invalid_state",
            Severity::Fatal,
            "期望状态本身不成立，生成器已拒绝",
            String::from("可通过校验的期望状态"),
            describe_problems(&problems),
        ));
        // 结构都不成立，后面的判据只会产出连带噪音
        return out;
    }

    let ports = [
        ("dns", state.ports.dns),
        ("tproxy", state.ports.tproxy),
        ("redirect", state.ports.redirect),
        ("mixed", state.ports.mixed),
        ("socks", state.ports.socks.unwrap_or(0)),
    ];
    let mut conflicts: Vec<String> = Vec::new();
    for (name_a, port_a) in ports {
        if port_a == 0 {
            continue; // socks 未启用
        }
        for (name_b, port_b) in ports {
            if name_a < name_b && port_a == port_b {
                conflicts.push(format!("{name_a}/{name_b} 都想用 {port_b}"));
            }
        }
    }
    if !conflicts.is_empty() {
        out.push(Finding::new(
            "port_conflict",
            Severity::Fatal,
            "入站端口冲突，数据面（dae / oxidns）起不来",
            String::from("dns/tproxy/redirect/mixed/socks 各不相同"),
            conflicts.join("；"),
        ));
    }

    if state.dns.fakeip.enabled {
        for (name, range, max) in [
            ("inet4_range", &state.dns.fakeip.inet4_range, 32u8),
            ("inet6_range", &state.dns.fakeip.inet6_range, 128u8),
        ] {
            if !valid_prefix(range, max) {
                out.push(Finding::new(
                    "fakeip_range_invalid",
                    Severity::Fatal,
                    &format!("fakeip 段 {name} 写法非法"),
                    format!("CIDR，前缀 0..{max}"),
                    range.clone(),
                ));
            }
        }
        match overlap_v4(&state.dns.fakeip.inet4_range, &state.network.lan_cidr) {
            Some(true) => out.push(Finding::new(
                "fakeip_overlaps_lan",
                Severity::Fatal,
                "fakeip 段和局域网段重叠，设备会拿到和真实主机冲突的地址",
                format!("fakeip 段与 {} 不相交", state.network.lan_cidr),
                state.dns.fakeip.inet4_range.clone(),
            )),
            // None 表示有一方不是 IPv4 CIDR，上面的形状检查已经报过，这里不重复
            _ => {}
        }
    }

    // 泄漏判据：这是吸收自参考实现的核心语义——"分流表对了但上游指错了"等于没分流
    let proxies_by_default = matches!(state.policy.final_outbound, FinalOutbound::Proxy(_));
    if proxies_by_default && state.dns.overseas.is_empty() {
        out.push(Finding::new(
            "dns_leak_no_overseas",
            Severity::Fatal,
            "最终出口是代理，但没有境外 DNS 上游：未命中国内规则的域名会走国内上游直连解析",
            String::from("dns.overseas 至少一个上游（DoH/DoT，经由代理出站）"),
            String::from("dns.overseas 为空"),
        ));
    }
    for up in &state.dns.overseas {
        if let Via::Outbound(tag) = &up.via {
            let known = state.nodes.iter().any(|n| &n.name == tag)
                || state.groups.iter().any(|g| &g.tag == tag)
                || tag == DIRECT_TAG
                || tag == REJECT_TAG;
            if !known {
                out.push(Finding::new(
                    "dns_upstream_dangling_outbound",
                    Severity::Fatal,
                    &format!("DNS 上游 {} 指定了不存在的出站 {tag}", up.tag),
                    String::from("via 指向已定义的节点或策略组"),
                    tag.clone(),
                ));
            }
        } else if proxies_by_default && up.host.parse::<Ipv4Addr>().is_ok() {
            // Via::Direct 的裸 IP 上游：旁路由上境外 IP 直连解析通常不通且泄漏
            out.push(Finding::new(
                "dns_upstream_bypasses_proxy",
                Severity::Warn,
                &format!("境外 DNS 上游 {} 走直连，可能解析失败或泄漏", up.tag),
                String::from("境外上游的 via 指向代理策略组"),
                format!("{} via=direct", up.host),
            ));
        }
    }

    // 顺序即优先级（选项 C）：一条规则被更靠前的规则**完全盖住**时它永不生效。
    // 危害分两种，值得分开报——被更靠前的"放行"盖住的那条拦截，是"说好拦却照样漏出去"；
    // 其余只是白写一行。广告库那一档查不动：`.srs` 在不解析内容（架构上的刻意选择），
    // 所以这条只覆盖用户自己写的名单。
    let mut leak: Vec<String> = Vec::new();
    let mut dead: Vec<String> = Vec::new();
    for (index, later) in state.policy.domain_rules.iter().enumerate() {
        let cover = state.policy.domain_rules[..index]
            .iter()
            .find(|earlier| covers(&earlier.pattern, &later.pattern));
        let Some(earlier) = cover else { continue };
        let text = format!(
            "{} 被排在它前面的 {} 盖住",
            later.describe(),
            earlier.describe()
        );
        if later.action == DomainAction::Block && earlier.action != DomainAction::Block {
            leak.push(text);
        } else {
            dead.push(text);
        }
    }
    if !leak.is_empty() {
        out.push(Finding::new(
            "block_never_fires_behind_a_pass",
            Severity::Warn,
            "有域名规则要拦截，但更靠前的放行规则已经把它们整个盖住：这些域名照样连得出去",
            String::from("要拦的行排在所有会命中它的放行行之前"),
            leak.join("；"),
        ));
    }
    if !dead.is_empty() {
        out.push(Finding::new(
            "domain_rule_never_fires",
            Severity::Warn,
            "有几条域名规则永远不会生效：前面已有一行把它们的域名整个覆盖",
            String::from("删掉被覆盖的那行，或把它挪到覆盖者之前"),
            dead.join("；"),
        ));
    }

    // “这台机器的 IPv6 连接经不经 ZBox”必须明说，而且要在**没有现场读数**的地方也说得出，
    // 所以这条在 self_check 而不是 inspect：它是期望状态本身的性质，不依赖目标机上有没有 v6。
    // 只数真填了 v6 地址、且不是 bypass 的设备——纯 v4 的局域网里没东西可绕，
    // 而 bypass 那台本来就是“别管我”。
    let v6_devices = state
        .devices
        .iter()
        .filter(|d| d.mode != DeviceMode::Bypass && d.ip.trim().parse::<Ipv6Addr>().is_ok())
        .count();
    if state.network.ipv6_mode == Ipv6Mode::Off && v6_devices > 0 {
        // 同一个“不接管”有两种泄漏面，措辞必须分开：还在答 AAAA 时这些设备是真的在裸奔，
        // 已经掐掉 AAAA 时剩下的只是设备自己指定外部 DNS 那一类。
        let answers_aaaa = matches!(
            state.dns.strategy,
            DnsStrategy::PreferIpv4 | DnsStrategy::PreferIpv6
        );
        out.push(Finding::new(
            "ipv6_not_takeover",
            Severity::Warn,
            &format!(
                "IPv6 选的是不接管，而局域网里有 {v6_devices} 台设备填了 IPv6 地址：\
                 它们的 v6 连接按主路由发的 RA 直接出去，不经 ZBox，也不受任何分流规则约束"
            ),
            if answers_aaaa {
                String::from("要么把 IPv6 改成“只接管 DNS”或“双栈接管”，要么把地址族策略改成 ipv4_only / ipv6_only，让客户端拿不到 AAAA")
            } else {
                String::from("把 IPv6 改成“只接管 DNS”或“双栈接管”，让 v6 连接也有人来管")
            },
            if answers_aaaa {
                format!(
                    "{} 且地址族策略 = {}（还会回答 AAAA）",
                    Ipv6Mode::Off.label(),
                    state.dns.strategy.singbox_value()
                )
            } else {
                format!(
                    "{} 且地址族策略 = {}（不再回答 AAAA，泄漏面已经小很多）",
                    Ipv6Mode::Off.label(),
                    state.dns.strategy.singbox_value()
                )
            },
        ));
    }

    out
}

/// `earlier` 是否**完全盖住** `later`（凡命中 later 的域名必定命中 earlier）。
///
/// `pub(crate)`：梯级那一屏（[`crate::gen::ladder`]）要说"这一条永远走不到"，
/// 判据必须与这里同一个——写两遍就会一边报、一边不报，用户看到的是自相矛盾的两屏。
///
/// 只参与判断的是 2026-10-05 在 guest 上逐字实测过的两个形状（schema 文档 §4 末）：
/// `domain_suffix` 是**标签边界**匹配（`test` 命中 `test` 和 `a.test`，不命中 `mytest`），
/// `domain` 是精确匹配。`Keyword`/`Regex` 作为覆盖方一律不判——判不动就宁可漏报：
/// 误报会让人不再相信这条判据，漏报只是少一句提示（"用 `~ads` 拦、又写了一行直连"
/// 这种重叠不会被报出来，是刻意的已知面）。
pub(crate) fn covers(earlier: &DomainPattern, later: &DomainPattern) -> bool {
    /// `b` 这个后缀是否覆盖名字 `n`：`domain_suffix` 也匹配顶点域本身
    fn suffix_covers(b: &str, n: &str) -> bool {
        n == b || n.ends_with(&format!(".{b}"))
    }
    match (earlier, later) {
        (DomainPattern::Suffix(b), DomainPattern::Suffix(o))
        | (DomainPattern::Suffix(b), DomainPattern::Exact(o)) => suffix_covers(b, o),
        (DomainPattern::Exact(b), DomainPattern::Exact(o)) => b == o,
        _ => false,
    }
}
/// 现场读数比对。只在读数存在时才判定。
fn inspect(state: &DesiredState, reading: &Reading, layout: &Layout) -> Vec<Finding> {
    let mut out = Vec::new();
    let hijack_expected = matches!(state.policy.final_outbound, FinalOutbound::Proxy(_));

    if let Some(active) = reading.service_active {
        if !active {
            out.push(Finding::new(
                "service_not_running",
                Severity::Fatal,
                "ZBox 数据面服务未在运行",
                String::from("zbox 服务 active"),
                String::from("inactive"),
            ));
        }
    }

    if let Some(pairs) = reading.ip_forward {
        if pairs != 1 {
            let (key, value) = netstate::sysctl_pairs(state)
                .into_iter()
                .find(|(k, _)| k == "net.ipv4.ip_forward")
                .expect("ip_forward 必须在 sysctl 键表里");
            out.push(
                Finding::new(
                    "ip_forward_off",
                    Severity::Fatal,
                    "内核没有开启转发，局域网设备的流量出不了本机",
                    format!("{key} = {value}"),
                    format!("{key} = {pairs}"),
                )
                .with_fix(sysctl_fix(&[(key, value)])),
            );
        }
    }
    if let Some(filter) = reading.rp_filter_all {
        if filter != 2 {
            let (key, value) = netstate::sysctl_pairs(state)
                .into_iter()
                .find(|(k, _)| k == "net.ipv4.conf.all.rp_filter")
                .expect("all.rp_filter 必须在 sysctl 键表里");
            out.push(
                Finding::new(
                    "rp_filter_strict",
                    Severity::Fatal,
                    "反向路径过滤过严，TPROXY 收下的包会被内核静默丢弃",
                    format!("{key} = {value}"),
                    format!("{key} = {filter}"),
                )
                .with_fix(sysctl_fix(&[(key, value)])),
            );
        }
    }

    let current = layout.current_dir.trim_end_matches('/');
    if let Some(loaded) = reading.nft_table_loaded {
        if !loaded {
            out.push(
                Finding::new(
                    "nft_table_missing",
                    Severity::if_hijack(hijack_expected),
                    &format!(
                        "nftables 里没有 {} 表，局域网流量根本没被劫持",
                        crate::gen::nft::TABLE
                    ),
                    String::from("zbox 表已加载"),
                    String::from("表不存在"),
                )
                .with_fix(vec![format!("nft -f {current}/zbox.nft")]),
            );
        }
    }
    if let Some(loaded) = reading.divert_rule_loaded {
        if !loaded {
            out.push(Finding::new(
                "divert_rule_missing",
                Severity::if_hijack(hijack_expected),
                &format!(
                    "没有 fwmark 0x{DIVERT_MARK:x} → table {} 的策略路由，nft 打的标记没有意义，包会照常送出 WAN",
                    netstate::DIVERT_TABLE
                ),
                String::from("ip rule 里存在 divert 规则"),
                String::from("规则不存在"),
            )
            .with_fix(vec![format!("sh {current}/netstate.sh")]));
        }
    }

    // —— IPv6 那一半 ——
    // 三档里只有“双栈接管”会在连接层指望 v6，所以下面这几条都以它为前提；
    // 读数缺一项就跳过该项（`missing` 永远不等于“不符合”，见文件头那条约定）。
    let v6_connection_expected = state.network.ipv6_mode.takes_connection();

    if v6_connection_expected {
        if let Some(loaded) = reading.v6_table_loaded {
            if !loaded {
                out.push(
                    Finding::new(
                        "ipv6_nft_table_missing",
                        Severity::Fatal,
                        "IPv6 选的是双栈接管，但 nftables 里没有 ip6 那半边表：v6 的设备在连接层一条规则都没被命中",
                        format!("table ip6 {} 已加载", crate::gen::nft::TABLE),
                        String::from("表不存在"),
                    )
                    // 两份族在同一条命令里：那个文件同时含 ip 与 ip6 两张表
                    .with_fix(vec![format!("nft -f {current}/zbox.nft")]),
                );
            }
        }
        if let Some(loaded) = reading.v6_divert_rule_loaded {
            if !loaded {
                out.push(
                    Finding::new(
                        "ipv6_divert_rule_missing",
                        Severity::Fatal,
                        &format!(
                            "v6 侧没有 fwmark 0x{DIVERT_MARK:x} → table {} 的策略路由：ip6 链打上的标记没人接，包照常送出 WAN",
                            netstate::DIVERT_TABLE
                        ),
                        String::from("ip -6 rule 里存在 divert 规则"),
                        String::from("规则不存在"),
                    )
                    .with_fix(vec![format!("sh {current}/netstate.sh")]),
                );
            }
        }
    }

    // 双栈接管要在这台机器上做 v6 的中继，而 `all.forwarding` 是内核里那条总闸：
    // 它是 0，局域网设备那份 v6 的包根本进不到 divert 链——与 v4 的 `ip_forward_off` 同形。
    // 这一条只在接管连接那一档说得出需求，别的档 0 才是对的。
    // 出路写**成对的两条**（`accept_ra=2` 一起给），因为只补转发就正好落进下面臂 A 那个故障。
    if v6_connection_expected {
        if let Some(forward) = reading.v6_forward_all {
            if forward != 1 {
                out.push(
                    Finding::new(
                        "ipv6_forwarding_off",
                        Severity::Fatal,
                        "IPv6 转发没开：双栈接管要中继局域网设备的 v6 流量，这台机器的内核不转",
                        String::from(
                            "net.ipv6.conf.all.forwarding = 1（与 all.accept_ra = 2 成对写）",
                        ),
                        format!("net.ipv6.conf.all.forwarding = {forward}"),
                    )
                    .with_fix(sysctl_fix(&netstate::v6_host_pairs())),
                );
            }
        }
    }

    // 实测过的一对键（`docs/reference/ipv6-kernel-measured.md` §1 的臂 A）：
    // `all.forwarding=1` 一写下去，内核就停止采纳主路由的 RA，本机那条 `proto ra` 的
    // v6 默认路由当场消失、ping6 整臂失败，而 v4 一个字都看不出异常。
    // 这一条因此是**这台机器自己断网**，不是局域网客户端的事，别的判据盖不住它。
    if let (Some(forward), Some(ra)) = (reading.v6_forward_all, reading.v6_accept_ra_all) {
        if forward == 1 && ra != 2 {
            let (expected, fix) = if v6_connection_expected {
                (
                    String::from("net.ipv6.conf.all.accept_ra = 2（与 all.forwarding=1 成对写）"),
                    sysctl_fix(&netstate::accept_ra_pairs(&state.network.lan_interface)),
                )
            } else {
                (
                    String::from(
                        "net.ipv6.conf.all.forwarding = 0（这一档不在连接层接管 v6，不该开着转发）",
                    ),
                    vec![String::from("sysctl -q -w net.ipv6.conf.all.forwarding=0")],
                )
            };
            out.push(
                Finding::new(
                    "ipv6_forwarding_drops_ra",
                    Severity::if_hijack(v6_connection_expected),
                    "IPv6 转发开着而 accept_ra 不是 2：这台机器自己的 v6 默认路由会被内核删掉，而 v4 一切正常",
                    expected,
                    format!("all.forwarding = {forward}、all.accept_ra = {ra}"),
                )
                .with_fix(fix),
            );
        }
    }

    // DNS 口的 v6 监听：`takes_dns()`（只接管 DNS 与双栈接管两档）时 oxidns 会多生成
    // 一对绑在 `network.lan_address6` 上的监听（`gen/oxidns.rs::render_servers`），
    // 所以 `ss -lntu -6` 里就该看到那个口——这台 guest 的 `bindv6only=0`，
    // 显式 v6 地址的 socket 会被 `-6` 读到。
    // **只查 DNS 那一个口**：tproxy 口实测是绑 v6 的，但它在 dae 自己建的 `daens` netns 里，
    // 宿主 ns 的 `ss -lntu -6` 永远查不到（`docs/reference/ipv6-kernel-measured.md` §10），
    // 所以它不是这条判据能问的东西——写进来就是一条必然误报的故障。
    // 空读数在这里也不是"没读到"，而是"确实一个 v6 口都没在听"。
    if let Some(v6_listening) = &reading.v6_listening {
        if state.network.ipv6_mode.takes_dns() && !v6_listening.contains(&state.ports.dns) {
            out.push(
                Finding::new(
                    "dns_port_not_listening_on_ipv6",
                    Severity::Fatal,
                    "IPv6 要接管 DNS，但 DNS 口没在 IPv6 上监听：v6 客户端的查询根本进不到 ZBox",
                    format!(
                        "oxidns 在 IPv6 的 {port} 上监听（这一档生成物里带 udp_in6/tcp_in6，\
                         绑的是 network.lan_address6 那个地址）",
                        port = state.ports.dns
                    ),
                    format!("IPv6 侧实际在听 {v6_listening:?}"),
                )
                .with_fix(vec![
                    format!("cat {}", layout.worker_stderr_path()),
                    "journalctl -u zbox -n 50".into(),
                ]),
            );
        }
    }

    if let Some(listening) = &reading.listening {
        let mut missing = Vec::new();
        for (name, port) in [
            ("dns", state.ports.dns),
            ("tproxy", state.ports.tproxy),
            ("mixed", state.ports.mixed),
        ] {
            if !listening.contains(&port) {
                missing.push(format!("{name}:{port}"));
            }
        }
        if !missing.is_empty() {
            out.push(
                Finding::new(
                    "inbound_not_listening",
                    Severity::Fatal,
                    "配置里的入站没有监听起来，通常是 dae / oxidns 启动失败或端口被占用",
                    format!("{} 都在监听", missing.join("、")),
                    format!("实际在听 {:?}", listening),
                )
                // 两条读数都要给：OpenRC 那边子进程的 stderr 由 supervise-daemon 接到状态目录里
                // 那个文件（没有它，"起来了但没在听"在现场一个字都查不到，schema §11.8.9）；
                // systemd 那边 stderr 默认进 journal，不存在那个文件。
                // 另记一条实测噪声：刚 restart 完立刻读，端口可能还没绑上（约 1-2 秒），
                // 所以这条在"过两秒再跑一次 doctor 仍报"时才当故障看。
                .with_fix(vec![
                    format!("cat {}", layout.worker_stderr_path()),
                    "journalctl -u zbox -n 50".into(),
                ]),
            );
        }
    }

    if let Some(iface) = &reading.default_route_iface {
        let single_nic = state.network.lan_interface == state.network.wan_interface;
        if single_nic && iface != &state.network.lan_interface {
            out.push(Finding::new(
                "lan_iface_mismatch",
                Severity::Warn,
                "期望单网卡旁路由，但默认路由在另一张网卡上：劫持规则和实际出口网卡不一致",
                format!("默认路由网卡 = {}", state.network.lan_interface),
                iface.clone(),
            ));
        }
    }

    out
}

impl Severity {
    /// 同一个现象，在"还没开劫持"和"已经指望它代理"两种期望下严重程度不同
    fn if_hijack(hijack_expected: bool) -> Self {
        if hijack_expected {
            Severity::Fatal
        } else {
            Severity::Warn
        }
    }
}

fn sysctl_fix(pairs: &[(String, String)]) -> Vec<String> {
    pairs
        .iter()
        .map(|(key, value)| netstate::shell_quote(&format!("{key}={value}")))
        .map(|arg| format!("sysctl -q -w {arg}"))
        .collect()
}

fn valid_prefix(raw: &str, max: u8) -> bool {
    match raw.split_once('/') {
        Some((addr, prefix)) => {
            !addr.trim().is_empty()
                && !prefix.is_empty()
                && prefix.parse::<u8>().is_ok_and(|p| p <= max)
        }
        None => false,
    }
}

fn v4_net(raw: &str) -> Option<(u32, u8)> {
    let (addr, prefix) = raw.trim().split_once('/')?;
    let addr: Ipv4Addr = addr.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Some((u32::from(addr) & mask, prefix))
}

/// 两个 IPv4 网段是否相交。任一侧写法不是 IPv4 CIDR 时返回 `None`（交给形状检查报）。
pub fn overlap_v4(a: &str, b: &str) -> Option<bool> {
    let (a, al) = v4_net(a)?;
    let (b, bl) = v4_net(b)?;
    let keep = al.min(bl);
    let mask = if keep == 0 {
        0
    } else {
        u32::MAX << (32 - keep)
    };
    Some(a & mask == b & mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::uri::parse_link;
    use crate::model::config::{DomainPattern, DomainRule, Group, GroupKind, Network};
    use crate::model::device::Device;

    fn proxied() -> DesiredState {
        let mut state = DesiredState::baseline();
        state.nodes = vec![parse_link("trojan://pw@1.2.3.4:443#节点A").unwrap()];
        state.groups = vec![Group {
            tag: "proxy".into(),
            kind: GroupKind::Selector,
            members: vec!["节点A".into()],
            default: Some("节点A".into()),
        }];
        state.policy.final_outbound = FinalOutbound::Proxy("proxy".into());
        state.dns.overseas = DesiredState::default().dns.overseas;
        state.dns.fakeip = DesiredState::default().dns.fakeip;
        state
    }

    fn codes(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.code).collect()
    }

    /// 一行域名名单。写法用面板那套前缀语法，测试里就不必到处 `DomainPattern::X(..)`。
    fn row(raw: &str, action: DomainAction) -> DomainRule {
        DomainRule::new(DomainPattern::parse(raw).unwrap(), action)
    }

    #[test]
    fn a_clean_proxied_state_reports_nothing() {
        let state = proxied();
        assert_eq!(self_check(&state), Vec::new(), "{:?}", self_check(&state));
    }

    #[test]
    fn duplicate_inbound_ports_are_fatal() {
        let mut state = proxied();
        state.ports.tproxy = state.ports.dns;
        assert!(codes(&self_check(&state)).contains(&"port_conflict"));
    }

    #[test]
    fn fakeip_overlapping_the_lan_is_found_by_mask_arithmetic() {
        let mut state = proxied();
        state.dns.fakeip.inet4_range = "192.168.0.0/16".into();
        assert!(codes(&self_check(&state)).contains(&"fakeip_overlaps_lan"));

        // /15 与 /24 不相交，但和局域网同段时也要报出来（默认 198.18.0.0/15 vs 192.168.1.0/24 不报）
        state.dns.fakeip.inet4_range = "198.18.0.0/15".into();
        assert!(!codes(&self_check(&state)).contains(&"fakeip_overlaps_lan"));
        state.network = Network {
            lan_cidr: "198.18.7.0/24".into(),
            ..Default::default()
        };
        assert!(codes(&self_check(&state)).contains(&"fakeip_overlaps_lan"));
    }

    #[test]
    fn overlap_arithmetic_agrees_with_containment_in_both_directions() {
        assert_eq!(overlap_v4("10.0.0.0/8", "10.5.0.0/16"), Some(true));
        assert_eq!(overlap_v4("10.5.0.0/16", "10.0.0.0/8"), Some(true));
        assert_eq!(overlap_v4("11.0.0.0/8", "10.0.0.0/8"), Some(false));
        assert_eq!(overlap_v4("0.0.0.0/0", "10.0.0.0/8"), Some(true));
        // 写法不是 IPv4 CIDR 时不猜，交给形状检查
        assert_eq!(overlap_v4("fc00::/18", "10.0.0.0/8"), None);
        assert_eq!(overlap_v4("10.0.0.1/33", "10.0.0.0/8"), None);
    }

    /// 覆盖关系照的是 schema 文档 §4 末那批实测：`domain_suffix` 认标签边界，不认裸字符串后缀。
    #[test]
    fn shadowing_follows_the_measured_domain_suffix_shape() {
        let (exact, suffix, keyword, regex) = (
            |s: &str| DomainPattern::Exact(s.into()),
            |s: &str| DomainPattern::Suffix(s.into()),
            |s: &str| DomainPattern::Keyword(s.into()),
            |s: &str| DomainPattern::Regex(s.into()),
        );
        // 盖 +test 盖得住 test（顶点域）、a.test、sub.a.test
        assert!(covers(&suffix("test"), &exact("test")));
        assert!(covers(&suffix("test"), &suffix("a.test")));
        assert!(covers(&suffix("test"), &exact("a.test")));
        assert!(covers(&suffix("test"), &exact("sub.a.test")));
        // 盖不住 mytest（标签边界），也盖不住后缀更宽的那一方
        assert!(!covers(&suffix("test"), &exact("mytest")));
        assert!(!covers(&suffix("a.test"), &suffix("test")));
        assert!(!covers(&suffix("test"), &exact("x.test.evil")));
        // 精确只盖精确；关键词与正则不参与判断
        assert!(covers(&exact("a.test"), &exact("a.test")));
        assert!(!covers(&exact("a.test"), &suffix("a.test")));
        assert!(!covers(&keyword("ads"), &exact("ads.test")));
        assert!(!covers(&regex(".*"), &exact("a.test")));
        assert!(!covers(&suffix("test"), &keyword("test")));
        assert!(!covers(&suffix("test"), &regex("a")));
    }

    #[test]
    fn a_dead_row_and_a_leaked_block_are_reported_separately() {
        let mut state = proxied();
        state.policy.domain_rules = vec![
            // 放行在前、拦截在后：说好拦的域名照样连得出去，这是危险的那一半
            row("+leak.test", DomainAction::Direct),
            row("leak.test", DomainAction::Block),
            // 拦截在前、放行在后：只是白写一行，结论仍然是"拦掉"
            row("+ads.test", DomainAction::Block),
            row("ads.test", DomainAction::Direct),
            row("unrelated.test", DomainAction::Direct),
            row("+a.ads.test", DomainAction::Proxy),
        ];

        let findings = self_check(&state);
        let leak = findings
            .iter()
            .find(|f| f.code == "block_never_fires_behind_a_pass")
            .expect("漏出去的那条该报");
        assert_eq!(leak.severity, Severity::Warn);
        assert_eq!(
            leak.actual, "leak.test（拦截） 被排在它前面的 +leak.test（直连） 盖住",
            "{leak:?}"
        );

        let dead: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.code == "domain_rule_never_fires")
            .collect();
        assert_eq!(dead.len(), 1, "{findings:?}");
        // 只点名被盖住的行，且用面板里那种写法回显（带处置）
        assert_eq!(
            dead[0].actual,
            "ads.test（直连） 被排在它前面的 +ads.test（拦截） 盖住；\
             +a.ads.test（代理） 被排在它前面的 +ads.test（拦截） 盖住",
            "{}",
            dead[0].actual
        );
        assert!(
            !dead[0].actual.contains("unrelated.test"),
            "没被盖住的行不该出现在报告里：{}",
            dead[0].actual
        );
    }

    #[test]
    fn disjoint_rows_report_no_shadow() {
        let mut state = proxied();
        state.policy.domain_rules = vec![
            row("+ads.test", DomainAction::Block),
            row("ads.testx", DomainAction::Direct),
        ];
        let findings = self_check(&state);
        let found = codes(&findings);
        assert!(
            !found.contains(&"domain_rule_never_fires")
                && !found.contains(&"block_never_fires_behind_a_pass"),
            "{found:?}"
        );
    }

    #[test]
    fn a_blocked_row_above_the_passing_one_is_not_a_leak() {
        // #18 定案的那条顺序（拦截在前、直连在后）在新模型里就是"用户把拦的那行写在上面"：
        // 仍然有一行不生效，但不该报成"漏出去"
        let mut state = proxied();
        state.policy.domain_rules = vec![
            row("both.test", DomainAction::Block),
            row("both.test", DomainAction::Direct),
        ];
        let findings = self_check(&state);
        assert_eq!(
            codes(&findings),
            vec!["domain_rule_never_fires"],
            "{findings:?}"
        );
    }

    #[test]
    fn bad_fakeip_shapes_are_reported_instead_of_written_into_the_kernel() {
        // /200 通得过模型层的形状校验（前缀能解析成 u8），但作为 IPv6 段是非法的：
        // 这正是 doctor 要在 validate 之外补的那一层判据
        let mut state = proxied();
        state.dns.fakeip.inet6_range = "fc00::/200".into();
        assert!(codes(&self_check(&state)).contains(&"fakeip_range_invalid"));

        let mut state = proxied();
        state.dns.fakeip.inet4_range = "198.18.0.0/99".into();
        assert!(codes(&self_check(&state)).contains(&"fakeip_range_invalid"));
    }

    #[test]
    fn proxying_without_an_overseas_upstream_is_called_a_leak() {
        let mut state = proxied();
        state.dns.overseas.clear();
        let findings = self_check(&state);
        assert!(codes(&findings).contains(&"dns_leak_no_overseas"));

        // 全直连时同一现象不是泄漏，只是没配境外上游
        let mut state = DesiredState::baseline();
        state.dns.overseas.clear();
        assert!(!codes(&self_check(&state)).contains(&"dns_leak_no_overseas"));
    }

    #[test]
    fn dangling_and_direct_dns_upstreams_are_distinguished() {
        let mut state = proxied();
        state.dns.overseas[0].via = Via::Outbound("不存在的组".into());
        assert!(codes(&self_check(&state)).contains(&"dns_upstream_dangling_outbound"));

        let mut state = proxied();
        state.dns.overseas[0].via = Via::Direct;
        let findings = self_check(&state);
        let leak = findings
            .iter()
            .find(|f| f.code == "dns_upstream_bypasses_proxy")
            .expect("裸 IP 直连的境外上游要提示");
        assert_eq!(leak.severity, Severity::Warn, "这是可能不通，不是必然坏");
    }

    #[test]
    fn an_invalid_state_short_circuits_into_one_finding() {
        let mut state = proxied();
        state.groups[0].members = vec!["幽灵".into()];
        state.ports.dns = state.ports.mixed;
        let findings = self_check(&state);
        assert_eq!(codes(&findings), vec!["invalid_state"], "{findings:?}");

        // 一次列全在 doctor 这条路也成立：两处各坏一点，那条 Fatal 要同时说出两条，
        // 而不是"修完这条再跑一次 doctor 看下一条"。
        let mut state = proxied();
        state.groups[0].members = vec!["幽灵".into()];
        state.groups[0].default = None;
        state.network.lan_cidr = "not-a-cidr".into();
        let actual = &self_check(&state)[0].actual;
        assert!(actual.starts_with("这 2 处不成立，要改："), "{actual}");
        assert!(actual.contains("幽灵") && actual.contains("CIDR"), "{actual}");
    }

    #[test]
    fn missing_readings_are_never_reported_as_failures() {
        // 开发机上 svc 层什么都没读到，这里必须是空列表而不是"一屏假故障"
        assert_eq!(
            inspect(&proxied(), &Reading::default(), &Layout::default()),
            Vec::new()
        );
    }

    #[test]
    fn kernel_findings_carry_the_exact_sysctl_command() {
        let reading = Reading {
            ip_forward: Some(0),
            rp_filter_all: Some(1),
            ..Default::default()
        };
        let findings = inspect(&proxied(), &reading, &Layout::default());
        let fix: Vec<&String> = findings.iter().flat_map(|f| &f.fix).collect();
        assert!(
            fix.contains(&&"sysctl -q -w 'net.ipv4.ip_forward=1'".to_string()),
            "{fix:?}"
        );
        assert!(
            fix.contains(&&"sysctl -q -w 'net.ipv4.conf.all.rp_filter=2'".to_string()),
            "{fix:?}"
        );
        assert!(findings.iter().all(|f| f.severity == Severity::Fatal));
    }

    #[test]
    fn the_same_missing_hijack_layer_changes_severity_with_expectation() {
        let reading = Reading {
            nft_table_loaded: Some(false),
            divert_rule_loaded: Some(false),
            ..Default::default()
        };
        let baseline = inspect(&DesiredState::baseline(), &reading, &Layout::default());
        assert!(
            baseline.iter().all(|f| f.severity == Severity::Warn),
            "{baseline:?}"
        );
        let proxied = inspect(&proxied(), &reading, &Layout::default());
        assert!(
            proxied.iter().all(|f| f.severity == Severity::Fatal),
            "{proxied:?}"
        );
        // 命令里的路径来自 Layout，不写死
        assert!(proxied.iter().any(|f| f
            .fix
            .iter()
            .any(|c| c.contains("nft -f /var/lib/zbox/current/zbox.nft"))));
    }

    #[test]
    fn a_configured_inbound_that_is_not_listening_is_reported_once_with_all_ports() {
        let reading = Reading {
            listening: Some(vec![53, 8888]),
            ..Default::default()
        };
        let findings = inspect(&proxied(), &reading, &Layout::default());
        assert_eq!(
            codes(&findings),
            vec!["inbound_not_listening"],
            "{findings:?}"
        );
        assert!(
            findings[0].expected.contains("tproxy:9888"),
            "{:?}",
            findings[0]
        );
        // 光说"启动失败"不够，得把人指到能查到原因的地方
        assert!(findings[0]
            .fix
            .iter()
            .any(|c| c == "cat /var/lib/zbox/sing-box.stderr.log"));
    }

    #[test]
    fn single_nic_expectation_vs_real_default_route_is_a_hint_not_a_hard_error() {
        let reading = Reading {
            default_route_iface: Some("wg0".into()),
            ..Default::default()
        };
        let findings = inspect(&proxied(), &reading, &Layout::default());
        assert_eq!(codes(&findings), vec!["lan_iface_mismatch"]);
        assert_eq!(findings[0].severity, Severity::Warn);

        // 双网卡形态下默认路由本来就不在局域网网卡上，不该报
        let mut state = proxied();
        state.network.wan_interface = "ens17".into();
        assert!(inspect(&state, &reading, &Layout::default()).is_empty());
    }

    #[test]
    fn diagnose_runs_static_then_live_and_keeps_order() {
        let mut state = proxied();
        // 空目标过不了 `DomainPattern::parse`，只能直接构造——这正是 `validate` 要拦的那种
        state.policy.domain_rules = vec![DomainRule::new(
            DomainPattern::Exact(String::new()),
            DomainAction::Block,
        )];
        let reading = Reading {
            service_active: Some(false),
            ..Default::default()
        };
        let findings = diagnose(&state, &reading, &Layout::default());
        assert_eq!(
            codes(&findings),
            vec!["invalid_state", "service_not_running"],
            "{findings:?}"
        );
    }

    // —— IPv6 可见性 ——

    fn dev(ip: &str, mode: DeviceMode) -> Device {
        Device {
            ip: ip.into(),
            mac: None,
            hostname: Some(ip.into()),
            mode,
            ip_type: crate::model::device::IpType::Dhcp,
            display_name: None,
        }
    }

    fn dual_stack() -> DesiredState {
        let mut state = proxied();
        state.network.ipv6_mode = Ipv6Mode::DualStack;
        state.network.lan_cidr6 = "fd00:db8:1::/64".into();
        state
    }

    fn dns_only() -> DesiredState {
        let mut state = proxied();
        // 只接管 DNS 必须配 *_only，否则 validate 先拒（那一句在 config.rs 里另有测试）
        state.dns.strategy = DnsStrategy::Ipv4Only;
        state.network.ipv6_mode = Ipv6Mode::DnsOnly;
        state.network.lan_cidr6 = "fd00:db8:1::/64".into();
        state
    }

    fn only(findings: &[Finding], code: &str) -> Finding {
        findings
            .iter()
            .find(|f| f.code == code)
            .unwrap_or_else(|| panic!("没有 {code} 这一条：{findings:?}"))
            .clone()
    }

    /// 不接管那一档要把“谁在管 v6”明说，而且要数得准：bypass 那台是自愿豁免，
    /// v4 地址的设备与 v6 无关，都不算“绕过去的那一台”。
    #[test]
    fn an_untaken_ipv6_names_the_devices_that_bypass_it() {
        let mut state = proxied();
        state.devices = vec![
            dev("10.99.0.2", DeviceMode::Global),
            dev("fd00:db8:1::50", DeviceMode::Proxy),
            dev("fd00:db8:1::99", DeviceMode::Bypass),
        ];
        let found = self_check(&state);
        let f = only(&found, "ipv6_not_takeover");
        assert_eq!(f.severity, Severity::Warn);
        assert!(
            f.summary.contains("1 台"),
            "只该数到那一台 v6 的：{}",
            f.summary
        );
        // prefer_ipv4 仍在回答 AAAA，所以“改成 *_only”是这条真出路
        assert!(f.expected.contains("ipv4_only"), "{}", f.expected);
    }

    #[test]
    fn a_refused_aaaa_makes_the_same_situation_a_milder_note() {
        let mut state = proxied();
        state.dns.strategy = DnsStrategy::Ipv4Only;
        state.devices = vec![dev("fd00:db8:1::50", DeviceMode::Global)];
        let f = only(&self_check(&state), "ipv6_not_takeover");
        assert!(f.actual.contains("不再回答 AAAA"), "{}", f.actual);
        assert!(
            !f.expected.contains("ipv4_only"),
            "已经把 AAAA 掐了，别把同一句话再当出路重复一遍：{}",
            f.expected
        );
    }

    #[test]
    fn a_v4_only_lan_says_nothing_about_ipv6() {
        let mut state = proxied();
        state.devices = vec![dev("10.99.0.2", DeviceMode::Global)];
        assert!(
            !codes(&self_check(&state)).contains(&"ipv6_not_takeover"),
            "{:?}",
            self_check(&state)
        );
    }

    #[test]
    fn dual_stack_reports_the_missing_v6_half_by_its_own_name() {
        let reading = Reading {
            v6_table_loaded: Some(false),
            v6_divert_rule_loaded: Some(false),
            // 两个劫持靶子都在 v6 上听着，所以这一轮只该报表和策略路由
            v6_listening: Some(vec![22, 53, 9888]),
            ..Default::default()
        };
        let found = inspect(&dual_stack(), &reading, &Layout::default());
        let got = codes(&found);
        assert!(got.contains(&"ipv6_nft_table_missing"), "{got:?}");
        assert!(got.contains(&"ipv6_divert_rule_missing"), "{got:?}");
        assert!(!got.contains(&"dns_port_not_listening_on_ipv6"), "{got:?}");
        assert!(found.iter().all(|f| f.is_fatal()), "{found:?}");
        // 处置命令指向的是那两个文件（一份 nft 里两张族、netstate.sh 两族都写）
        assert!(only(&found, "ipv6_nft_table_missing")
            .fix
            .iter()
            .any(|c| c.ends_with("zbox.nft")));
        assert!(only(&found, "ipv6_divert_rule_missing")
            .fix
            .iter()
            .any(|c| c.ends_with("netstate.sh")));
    }

    /// 实测里最贵的一对键：转发一开、accept_ra 不是 2，本机自己的 v6 默认路由就被内核删掉，
    /// 而 v4 一个字都看不出异常。期望值随档位分两种——**同一现象，两种出路**，
    /// 给错一条命令就等于让不接管 v6 的机器去当中继。
    #[test]
    fn forwarding_without_accept_ra_two_is_the_silent_v6_outage() {
        let reading = Reading {
            v6_forward_all: Some(1),
            v6_accept_ra_all: Some(1),
            ..Default::default()
        };
        let found = inspect(&dual_stack(), &reading, &Layout::default());
        let f = only(&found, "ipv6_forwarding_drops_ra");
        assert!(f.is_fatal(), "双栈档里这一对是生成物自己写的：{f:?}");
        let fix: Vec<&str> = f.fix.iter().map(String::as_str).collect();
        assert_eq!(
            fix,
            vec![
                "sysctl -q -w 'net.ipv6.conf.all.accept_ra=2'",
                "sysctl -q -w 'net.ipv6.conf.ens33.accept_ra=2'",
            ],
            "all 与局域网网卡两份都要写（实测臂 C 就是这个形状）"
        );

        let off = inspect(&proxied(), &reading, &Layout::default());
        let f = only(&off, "ipv6_forwarding_drops_ra");
        assert_eq!(f.severity, Severity::Warn, "{f:?}");
        assert_eq!(
            f.fix,
            vec![String::from("sysctl -q -w net.ipv6.conf.all.forwarding=0")],
            "这一档不在连接层接管 v6，出路是把转发关掉"
        );
        // 两个键都成对写好了 -> 不报
        assert!(!codes(&inspect(
            &dual_stack(),
            &Reading {
                v6_forward_all: Some(1),
                v6_accept_ra_all: Some(2),
                ..Default::default()
            },
            &Layout::default()
        ))
        .contains(&"ipv6_forwarding_drops_ra"));
    }

    /// 这一条只查 DNS 那一个口，两档都会触发；**双栈档里 tproxy 口没在 v6 上监听时
    /// 也只报这一条**。理由实测过（`ipv6-kernel-measured.md` §10）：dae 的 tproxy 口绑的是
    /// `0.0.0.0` + `[::]` 两只，但都在它自建的 `daens` netns 里，宿主 ns 的探针查不到——
    /// 把它写成故障等于每条双栈配置都误报一次。所以这里反过来断言"只报一条"。
    #[test]
    fn the_dns_port_is_the_one_we_can_claim_on_ipv6() {
        // v4 侧三条口全在（不该重复报 v4 那条），v6 侧只有 sshd
        let reading = Reading {
            listening: Some(vec![22, 53, 8888, 9888]),
            v6_listening: Some(vec![22]),
            ..Default::default()
        };
        for state in [dual_stack(), dns_only()] {
            let found = inspect(&state, &reading, &Layout::default());
            let got = codes(&found);
            assert!(!got.contains(&"inbound_not_listening"), "{got:?}");
            assert_eq!(got, vec!["dns_port_not_listening_on_ipv6"], "{found:?}");
            let f = only(&found, "dns_port_not_listening_on_ipv6");
            assert!(f.is_fatal());
            assert!(f.expected.contains("IPv6 的 53"), "{f:?}");
            assert!(
                f.expected.contains("lan_address6"),
                "expected 要说清这个口绑的是模型里哪一格，人才知道去哪儿改：{f:?}"
            );
            assert!(f.actual.contains("[22]"), "{}", f.actual);
            // 查的是"生成物写下去之后进程为什么没听"，不是"再去加载劫持层"
            assert!(
                f.fix
                    .iter()
                    .any(|c| c.contains("worker") || c.contains("journalctl")),
                "{f:?}"
            );
        }
    }

    /// 空读数在这里是**确定的负例**（一个 v6 口都没有），不是"没读到"；
    /// 而 `None` 才是没读到，那条永远不报——Windows 开发机上 ss 就不存在。
    #[test]
    fn an_empty_ipv6_read_is_a_real_negative_not_a_missing_one() {
        let found = inspect(
            &dns_only(),
            &Reading {
                v6_listening: Some(Vec::new()),
                ..Default::default()
            },
            &Layout::default(),
        );
        assert_eq!(
            codes(&found),
            vec!["dns_port_not_listening_on_ipv6"],
            "{found:?}"
        );
        assert!(
            found[0].actual.contains("[]"),
            "要说得出「一个都没有」：{}",
            found[0].actual
        );

        let unread = inspect(&dns_only(), &Reading::default(), &Layout::default());
        assert!(unread.is_empty(), "{unread:?}");
    }

    /// 双栈档里 v6 转发的两个方向都要报得出：开成 1 而 `accept_ra` 没配对 = 本机自己断网
    /// （上一条测试），停在 0 = 局域网那批 v6 客户端的包进不到中继。两个 fix 都不许是
    /// "只补这一个键"，因为补转发不补 accept_ra 正好落进臂 A。
    #[test]
    fn dual_stack_needs_the_forwarding_switch_on_and_off_is_silent_for_other_modes() {
        let off_reading = Reading {
            v6_forward_all: Some(0),
            v6_accept_ra_all: Some(1),
            ..Default::default()
        };
        let found = inspect(&dual_stack(), &off_reading, &Layout::default());
        let f = only(&found, "ipv6_forwarding_off");
        assert!(f.is_fatal(), "{f:?}");
        let fix: Vec<&str> = f.fix.iter().map(String::as_str).collect();
        assert_eq!(
            fix,
            vec![
                "sysctl -q -w 'net.ipv6.conf.all.forwarding=1'",
                "sysctl -q -w 'net.ipv6.conf.all.accept_ra=2'",
            ],
            "成对给：只补转发就落进实测臂 A 那个静默断网"
        );
        // 这一档不接管连接，转发是 0 才对，所以一个字都不报
        for state in [proxied(), dns_only()] {
            let found = inspect(&state, &off_reading, &Layout::default());
            let got = codes(&found);
            assert!(!got.contains(&"ipv6_forwarding_off"), "{got:?}");
            assert!(!got.contains(&"ipv6_forwarding_drops_ra"), "{got:?}");
        }
        // 读不到就什么也不说
        assert!(!codes(&inspect(
            &dual_stack(),
            &Reading::default(),
            &Layout::default()
        ))
        .contains(&"ipv6_forwarding_off"));
    }

    /// 反向守卫：不接管那一档，v6 侧读到多“缺”都不报。那台机器的 v6 本来就不归 ZBox 管，
    /// 报出来只会把人引去查一条不存在的需求。
    #[test]
    fn off_mode_reports_nothing_from_the_v6_side() {
        let reading = Reading {
            v6_table_loaded: Some(false),
            v6_divert_rule_loaded: Some(false),
            v6_listening: Some(Vec::new()),
            v6_forward_all: Some(0),
            v6_accept_ra_all: Some(1),
            ..Default::default()
        };
        let found = inspect(&proxied(), &reading, &Layout::default());
        assert!(found.is_empty(), "{found:?}");
        // “没读到”与“读到了不符合”永远两回事
        assert!(inspect(&dual_stack(), &Reading::default(), &Layout::default()).is_empty());
    }
}
