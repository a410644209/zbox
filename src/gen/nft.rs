//! 局域网流量的劫持层（nftables）。
//!
//! **这段不属于 sing-box 官方文档覆盖的范围**：1.14.2 的 tproxy 文档只讲入站配置，
//! 完全不提 nftables / ip rule（见 docs/reference/singbox-1.14.2-schema.md §8）。
//! 所以本模块是 ZBox 自证的模板，形状取自社区通行的 TPROXY 做法，必须在 VM 上实测。
//!
//! 三条不能错的顺序：
//! 1. **只处理局域网转发流量**：`iifname` + `ip saddr` 双重限定，本机自己发出的、
//!    主路由进来的、以及 sing-box 出口（带标记）的一概不碰。
//! 2. **DNS 用 nat redirect，TPROXY 用 filter mark**，两者分属不同链；DNS 必须先于
//!    TPROXY 落地，否则 53 端口的包会被当成普通 UDP 送进 tproxy 入站。
//! 3. **v6 那一半边按档位生成**（`network.ipv6_mode`）：不接管就一份 ip6 表都不写；
//!    只收 DNS 就只写 nat 那条 `dns` 链；双栈才写 `divert` 链与 tproxy。理由与实测见
//!    `docs/reference/ipv6-kernel-measured.md` —— §5 记的是这份 `table ip6` 真在这台
//!    guest 上加载过一次（两族是两张独立的表、幂等照旧），§2 记的是"v6 监听口是不是
//!    双栈"属于内核 sysctl（`bindv6only`）而不是插件保证，所以 v6 的靶子必须显式开；
//!    §9 记的是真 v6 客户端流量进来时豁免链确实按 `ip6 saddr` 命中。
//! 4. **`nft list` 的输出不许拿来和本文件比字符串**：十六进制会被补零重写，单元素集合的
//!    花括号会被去掉（§5 末尾那张表）。要比就数规则条数或按语义比。
//! 5. **divert 链本身不构成防泄漏保证**：`tproxy` 未命中时，同条规则后面的
//!    `meta mark set 0x1 accept` 不执行，但链继续走下一条 —— 包不置 mark、按普通路由转发出去，
//!    是**静默泄漏**不是黑洞（§8 三臂计数器实测）。所以"上游起没起"必须是探测与诊断的问题，
//!    不许当成这条链自带的能力。

use crate::gen::singbox::route::DIVERT_MARK;
use crate::model::config::{DesiredState, Ipv6Mode, Ports};
use crate::model::device::DeviceMode;
use std::net::{Ipv4Addr, Ipv6Addr};

pub const TABLE: &str = "zbox";
pub const DNS_CHAIN: &str = "dns";
pub const DIVERT_CHAIN: &str = "divert";
pub const NFT_PATH: &str = "zbox.nft";
/// 客户端查询的端口固定是 53；`ports.dns` 只是本机监听的端口，两者不能混用
const DNS_QUERY_PORT: u16 = 53;

/// 生成 `nft -f` 可直接加载的脚本。
///
/// 开头先 `{}` 建表再 `flush`，是为了让重复下发不会产生重复规则——`nft -f` 对已存在的
/// 链是**追加**语义，不 flush 就会每下发一次多一份规则。
pub fn render(state: &DesiredState) -> String {
    let mut out = render_family(state, false);
    if state.network.ipv6_mode != Ipv6Mode::Off {
        out.push_str(&render_family(state, true));
    }
    out
}

fn render_family(state: &DesiredState, v6: bool) -> String {
    let family = if v6 { "ip6" } else { "ip" };
    let mut out = String::new();
    out.push_str(&format!(
        "table {family} {TABLE} {{ }}\nflush table {family} {TABLE}\n\ntable {family} {TABLE} {{\n"
    ));

    out.push_str(&format!("  chain {DNS_CHAIN} {{\n"));
    out.push_str("    type nat hook prerouting priority dstnat; policy accept;\n");
    push_scope_guards(&mut out, state, v6);
    // DNS 劫持要覆盖"客户端指了主路由/公共 DNS"两种情况，所以放在私网目的地豁免之前
    out.push_str(&format!(
        "    udp dport {DNS_QUERY_PORT} redirect to :{dns}\n    tcp dport {DNS_QUERY_PORT} redirect to :{dns}\n",
        dns = state.ports.dns
    ));
    out.push_str("  }\n");

    // v6 的连接层是单独一档：dns_only 只收查询，不该出现 tproxy 链
    if !v6 || state.network.ipv6_mode.takes_connection() {
        out.push_str("\n");
        out.push_str(&format!("  chain {DIVERT_CHAIN} {{\n"));
        out.push_str("    type filter hook prerouting priority mangle; policy accept;\n");
        push_scope_guards(&mut out, state, v6);
        // 目标是本机（面板、显式代理端口、以及上面刚 DNAT 过的 DNS）不该再被 TPROXY
        out.push_str("    fib daddr type local return\n");
        // 组播/广播留给局域网自己处理（mDNS、SSDP、DHCP）
        if v6 {
            // v6 没有广播地址这一说，组播是 ff00::/8；NDP/MLD 全在这一段里
            out.push_str("    ip6 daddr { ff00::/8 } return\n");
            // DHCPv6 是 546/547，与 v4 的 67/68 不是同一对端口
            out.push_str("    meta l4proto udp udp dport { 546, 547 } return\n");
        } else {
            out.push_str("    ip daddr { 224.0.0.0/4, 255.255.255.255 } return\n");
            out.push_str("    meta l4proto udp udp dport { 67, 68 } return\n");
        }
        // 53 已由 nat 链接管；两条链同优先级，先后顺序不由我们决定，所以两侧都设防线
        out.push_str(&format!(
            "    udp dport {DNS_QUERY_PORT} return\n    tcp dport {DNS_QUERY_PORT} return\n"
        ));
        push_tproxy(&mut out, &state.ports);
        out.push_str("  }\n");
    } else {
        // 这一档故意没有 divert 链：写一条空链进去只会让人以为"v6 连接也被管了"
        out.push_str(&format!(
            "  # 这一档（ipv6_mode = dns_only）不生成 {DIVERT_CHAIN} 链：只把 v6 的 DNS 查询收进来，\n\
             # v6 的连接层仍然由主路由负责。要连接层也接管，档位改成 dual_stack。\n\
             # 于是这一份里有一条**明写的泄漏路径**：客户端拿到 AAAA 记录就能 v6 直连出去，\n\
             # 所以模型层要求这一档必须配 *_only 的 dns.strategy（gen::oxidns 那条渲染期硬错误）。\n"
        ));
    }
    out.push_str("}\n");
    out
}

/// 作用域限定：只有"从局域网网卡进来、源地址属于局域网、且未被标记"的包才继续处理
fn push_scope_guards(out: &mut String, state: &DesiredState, v6: bool) {
    let lan = &state.network.lan_interface;
    let (saddr_key, cidr) = if v6 {
        ("ip6 saddr", state.network.lan_cidr6.trim())
    } else {
        ("ip saddr", state.network.lan_cidr.trim())
    };
    // 已经被 sing-box 出口打标（或上一轮已被处理）的包直接放行，防止自我劫持
    out.push_str("    meta mark != 0x0 return\n");
    out.push_str(&format!("    iifname != \"{lan}\" return\n"));
    out.push_str(&format!("    {saddr_key} != {cidr} return\n"));
    let bypassed = bypass_devices(state, v6);
    if !bypassed.is_empty() {
        out.push_str(&format!(
            "    {saddr_key} {{ {} }} return\n",
            bypassed.join(", ")
        ));
    }
}

/// Bypass 设备的地址，按族挑出来。
///
/// 不能整份塞进一条链：v6 地址写进 v4 链会让 `nft -f` 在加载时就报错，而加载失败是
/// **整份劫持层**下不去，不是"这一条没用"。`validate()` 那一头已经拦住"两个族都不像"
/// 的写法，所以这里两个桶加起来就是全部。
fn bypass_devices(state: &DesiredState, v6: bool) -> Vec<&str> {
    state
        .devices
        .iter()
        .filter(|device| device.mode == DeviceMode::Bypass)
        .map(|device| device.ip.trim())
        .filter(|ip| {
            if v6 {
                ip.parse::<Ipv6Addr>().is_ok()
            } else {
                ip.parse::<Ipv4Addr>().is_ok()
            }
        })
        .collect()
}

fn push_tproxy(out: &mut String, ports: &Ports) {
    // tproxy 语句保留原始目的地，mark 交给 ip rule 把包送进 local 路由表
    out.push_str(&format!(
        "    meta l4proto tcp tproxy to :{} meta mark set 0x{:x} accept\n",
        ports.tproxy, DIVERT_MARK
    ));
    out.push_str(&format!(
        "    meta l4proto udp tproxy to :{} meta mark set 0x{:x} accept\n",
        ports.tproxy, DIVERT_MARK
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::singbox::route::{route_json, BYPASS_MARK};
    use crate::model::config::{DesiredState, Ipv6Mode, Network};
    use crate::model::device::Device;

    fn state() -> DesiredState {
        DesiredState {
            network: Network {
                lan_interface: "ens33".into(),
                lan_cidr: "192.168.1.0/24".into(),
                wan_interface: "ens33".into(),
                ..Network::default()
            },
            ..DesiredState::baseline()
        }
    }

    #[test]
    fn scope_is_limited_to_forwarded_lan_traffic() {
        let text = render(&state());
        assert!(text.contains("meta mark != 0x0 return"), "{text}");
        assert!(text.contains("iifname != \"ens33\" return"));
        assert!(text.contains("ip saddr != 192.168.1.0/24 return"));
        // 绝不做 SNAT：旁路由后面的主路由负责出网地址
        assert!(!text.contains("masquerade"), "{text}");
        assert!(!text.contains("snat"), "{text}");
        // 默认档位（不接管 v6）连一张 ip6 表都不该有——写了就是一条没人认领的链
        assert!(!text.contains("ip6"), "{text}");
    }

    /// 每条链都自带全套作用域守卫。prerouting 的几条链是同优先级的独立链，一条链
    /// `return` 之后包还会进另一条链，所以守卫必须逐链重复——只在第一条链里写一次，
    /// 等于给后面的链留了一条"不看网卡、不看源地址"的入口。
    #[test]
    fn every_chain_repeats_the_scope_guards() {
        let off = render(&state());
        assert_eq!(
            off.matches("meta mark != 0x0 return").count(),
            2,
            "不接管 v6：dns + divert 两条链，一条一份守卫：{off}"
        );

        let mut dual = state();
        dual.network.ipv6_mode = Ipv6Mode::DualStack;
        dual.network.lan_cidr6 = "fd00:db8:1::/64".into();
        let both = render(&dual);
        assert_eq!(
            both.matches("meta mark != 0x0 return").count(),
            4,
            "双栈是两族各两条链：{both}"
        );
        assert_eq!(both.matches("iifname != \"ens33\" return").count(), 4);

        // dns_only 少一条 v6 的 divert 链，守卫跟着少一份
        let mut dns_only = state();
        dns_only.network.ipv6_mode = Ipv6Mode::DnsOnly;
        dns_only.network.lan_cidr6 = "fd00:db8:1::/64".into();
        assert_eq!(
            render(&dns_only).matches("meta mark != 0x0 return").count(),
            3
        );
    }

    #[test]
    fn dns_redirect_precedes_tproxy_and_both_chains_exist() {
        let text = render(&state());
        let dns = text.find("udp dport 53 redirect to :53").unwrap();
        let tp = text.find("meta l4proto tcp tproxy to :9888").unwrap();
        assert!(dns < tp, "DNS 必须在 TPROXY 之前落地");
        assert!(text.contains("type nat hook prerouting priority dstnat"));
        assert!(text.contains("type filter hook prerouting priority mangle"));
        // 53 的包已被 nat 链接管，不能再进 tproxy
        assert!(text.contains("udp dport 53 return"));
    }

    #[test]
    fn divert_mark_matches_the_policy_route_and_differs_from_singbox_own_mark() {
        let text = render(&state());
        assert!(text.contains(&format!("meta mark set 0x{DIVERT_MARK:x} accept")));
        assert_ne!(DIVERT_MARK, BYPASS_MARK);
        // route 段给自己出站打的正是 BYPASS_MARK，nft 层的 "meta mark != 0x0 return" 才拦得住
        let route = route_json(&state()).unwrap();
        assert_eq!(route["default_mark"], BYPASS_MARK);
    }

    #[test]
    fn reload_is_idempotent_and_bypass_devices_are_exempted() {
        let mut s = state();
        s.devices = vec![
            Device {
                ip: "192.168.1.50".into(),
                mac: None,
                hostname: Some("电视".into()),
                mode: DeviceMode::Bypass,
            },
            Device {
                ip: "192.168.1.60".into(),
                mac: None,
                hostname: None,
                mode: DeviceMode::Proxy,
            },
        ];
        let text = render(&s);
        // 建表后先 flush，重复下发不会累加规则
        assert!(text.starts_with("table ip zbox { }\nflush table ip zbox\n"));
        assert!(text.contains("ip saddr { 192.168.1.50 } return"));
        // 只有 Bypass 才落在 nft 层；Proxy/Direct 是 sing-box 的事
        assert!(!text.contains("192.168.1.60"));
    }

    #[test]
    fn local_and_multicast_destinations_are_left_alone() {
        let text = render(&state());
        assert!(text.contains("fib daddr type local return"));
        assert!(text.contains("224.0.0.0/4"));
        assert!(text.contains("udp dport { 67, 68 } return"));
        // 面板和显式代理端口都是"目的地为本机"，靠 fib local 豁免，不需要逐个列端口
        assert!(!text.contains("dport 8888"), "{text}");
    }

    #[test]
    fn hijack_matches_port_53_but_redirects_to_the_configured_listener() {
        // systemd-resolved 占着 :53 时可以把监听口挪走，但客户端查的仍是 53
        let mut s = state();
        s.ports.dns = 5353;
        let text = render(&s);
        assert!(text.contains("udp dport 53 redirect to :5353"), "{text}");
        assert!(text.contains("tcp dport 53 redirect to :5353"), "{text}");
        assert!(text.contains("tcp dport 53 return"), "{text}");
        assert!(!text.contains("dport 5353"), "{text}");
    }

    #[test]
    fn v6_off_renders_no_ip6_table_at_all() {
        // 默认档位（不接管 v6）里一个字节的 ip6 都不许出现：留一条空链会让人以为
        // "v6 也被管了"，而这一档的真实状态是 v6 完全由主路由负责
        let text = render(&state());
        assert!(!text.contains("ip6"), "{text}");
        assert!(!text.contains("table ip6"), "{text}");
    }

    #[test]
    fn v6_scope_is_limited_to_the_configured_v6_prefix() {
        let mut s = state();
        s.network.ipv6_mode = Ipv6Mode::DnsOnly;
        s.network.lan_cidr6 = "fd00:db8:1::/64".into();
        let text = render(&s);
        assert!(text.starts_with("table ip zbox { }"), "v4 那份永远在最前面");
        let v6 = text
            .split_once("table ip6 zbox { }")
            .map(|(_, rest)| rest)
            .expect("接管 v6 时必须有 ip6 表：\n{text}");
        assert!(v6.contains("flush table ip6 zbox"), "重复下发不累加：{v6}");
        assert!(v6.contains("iifname != \"ens33\" return"), "{v6}");
        assert!(v6.contains("ip6 saddr != fd00:db8:1::/64 return"), "{v6}");
        // v6 链里绝不出现 v4 的段
        assert!(!v6.contains("ip saddr !="), "v4 的段拼进 ip6 链：{v6}");
    }

    #[test]
    fn dns_only_takes_v6_queries_but_not_v6_connections() {
        let mut s = state();
        s.network.ipv6_mode = Ipv6Mode::DnsOnly;
        s.network.lan_cidr6 = "fd00:db8:1::/64".into();
        let text = render(&s);
        let v6 = text.split_once("table ip6 zbox").unwrap().1;
        // 查询层：v6 的 53 收到本机监听口
        assert!(v6.contains("udp dport 53 redirect to :53"), "{v6}");
        assert!(v6.contains("tcp dport 53 redirect to :53"), "{v6}");
        // 连接层：这一档不碰。写成"链不存在"而不是"链里没规则"，
        // 因为空链会让人以为它只是还没配规则
        assert!(!v6.contains("tproxy"), "{v6}");
        assert!(!v6.contains("chain divert"), "{v6}");
        assert!(
            v6.contains("不生成 divert 链"),
            "这一档的泄漏路径要明写：{v6}"
        );
        // v4 那两条链不受影响，一份文件里两族各管各的
        assert!(text.contains("meta l4proto tcp tproxy to :9888"));
    }

    #[test]
    fn dual_stack_hijacks_v6_connections_with_v6_own_exemptions() {
        let mut s = state();
        s.network.ipv6_mode = Ipv6Mode::DualStack;
        s.network.lan_cidr6 = "fd00:db8:1::/64".into();
        let text = render(&s);
        let v6 = text.split_once("table ip6 zbox").unwrap().1;
        assert!(
            v6.contains("type filter hook prerouting priority mangle"),
            "{v6}"
        );
        assert!(
            v6.contains(&format!(
                "meta l4proto tcp tproxy to :9888 meta mark set 0x{DIVERT_MARK:x} accept"
            )),
            "{v6}"
        );
        assert!(v6.contains("meta l4proto udp tproxy to :9888"), "{v6}");
        // 豁免按 v6 的写法：组播是 ff00::/8，DHCPv6 是 546/547 —— v4 那份的
        // 224.0.0.0/4 与 67/68 在 v6 里一个都不存在
        assert!(v6.contains("ip6 daddr { ff00::/8 } return"), "{v6}");
        assert!(v6.contains("udp dport { 546, 547 } return"), "{v6}");
        assert!(!v6.contains("224.0.0.0/4"), "{v6}");
        assert!(!v6.contains("67, 68"), "{v6}");
        assert!(v6.contains("fib daddr type local return"), "{v6}");
        assert!(v6.contains("udp dport 53 return"), "{v6}");
    }

    #[test]
    fn bypass_devices_split_by_address_family() {
        let mut s = state();
        s.network.ipv6_mode = Ipv6Mode::DualStack;
        s.network.lan_cidr6 = "fd00:db8:1::/64".into();
        s.devices = vec![
            Device {
                ip: "192.168.1.50".into(),
                mac: None,
                hostname: None,
                mode: DeviceMode::Bypass,
            },
            Device {
                ip: "fd00:db8:1::30".into(),
                mac: None,
                hostname: None,
                mode: DeviceMode::Bypass,
            },
        ];
        let text = render(&s);
        let (v4, v6) = text.split_once("table ip6 zbox").unwrap();
        // v6 地址进了 v4 链，`nft -f` 会在加载时报错、整份劫持层下不去
        assert!(v4.contains("ip saddr { 192.168.1.50 } return"), "{v4}");
        assert!(!v4.contains("fd00:db8:1::30"), "v6 地址漏进 v4 链：{v4}");
        assert!(v6.contains("ip6 saddr { fd00:db8:1::30 } return"), "{v6}");
        assert!(!v6.contains("192.168.1.50"), "v4 地址漏进 v6 链：{v6}");
    }

    #[test]
    fn the_ipv6_mode_drives_every_v6_decision() {
        // 三档之间换一遍，v4 那一份不许变；ip6 那份从"没有"到"只有 DNS"到"两条链"
        let off = render(&state());
        let mut dns_only = state();
        dns_only.network.ipv6_mode = Ipv6Mode::DnsOnly;
        dns_only.network.lan_cidr6 = "fd00:db8:1::/64".into();
        let mut dual = state();
        dual.network.ipv6_mode = Ipv6Mode::DualStack;
        dual.network.lan_cidr6 = "fd00:db8:1::/64".into();

        let dns = render(&dns_only);
        let both = render(&dual);
        for text in [&dns, &both] {
            assert_eq!(
                text.split_once("table ip6 zbox").unwrap().0,
                off,
                "v4 那半边跟着档位变了，而档位只管 v6"
            );
        }
        assert_eq!(dns.matches("type nat hook").count(), 2, "{dns}");
        assert_eq!(dns.matches("type filter hook").count(), 1, "{dns}");
        assert_eq!(both.matches("type nat hook").count(), 2, "{both}");
        assert_eq!(both.matches("type filter hook").count(), 2, "{both}");
    }
}
