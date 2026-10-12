//! 期望状态 -> sing-box `inbounds` 段。
//!
//! 旁路由需要四类入口（沿用参考实现的端口布局，便于对照排查）：
//! - `dns-in`：`type:"direct"` 绑在 :53，接收局域网设备的 DNS 查询，再由 route 的
//!   `hijack-dns` 动作交给内置 DNS 引擎（1.14 已删除 `type:"dns"` 出站，别写那个）
//! - `tproxy-in`：TPROXY，TCP+UDP 都能劫持
//! - `redirect-in`：REDIRECT，只有 TCP，作为不支持 TPROXY 的内核/容器兜底
//! - `mixed-in` / `socks-in`：给手动设代理的设备用，不参与劫持
//!
//! 入站上**没有** `sniff`/`sniff_override_destination` 这些旧字段（1.13 起拒绝），
//! 全部嗅探由 route 段的 `{"action":"sniff"}` 负责。

use crate::model::config::DesiredState;
use serde_json::{json, Value};

pub const DNS_IN: &str = "dns-in";
pub const TPROXY_IN: &str = "tproxy-in";
pub const REDIRECT_IN: &str = "redirect-in";
pub const MIXED_IN: &str = "mixed-in";
pub const SOCKS_IN: &str = "socks-in";

/// 这一档的两个劫持靶子要不要收 v6 的包。
///
/// 只换 `dns-in` 与 `tproxy-in`——它们是 `gen::nft` 里那两条 ip6 链唯一指向的口，
/// v6 的包进不来就等于劫持层白搭。`redirect-in`（TPROXY 的兜底）、`mixed-in`、
/// `socks-in` 都没有对应的 v6 链，把它们一起放开只是多开监听面。
///
/// 写 `"::"` 而不是"再加一条 v6 入站"：2026-10-09 guest 实测 1.14.2 的 `listen: "::"`
/// 是**一只双栈 socket**（`ss` 报 `*:9888`，v4 与 v6 的 nc 都能连上），见
/// `docs/reference/singbox-1.14.2-schema.md` §5。
fn hijack_listen(state: &DesiredState) -> &'static str {
    if state.network.ipv6_mode.takes_connection() {
        "::"
    } else {
        "0.0.0.0"
    }
}

pub fn inbounds_json(state: &DesiredState) -> Vec<Value> {
    let ports = &state.ports;
    let hijack = hijack_listen(state);
    let mut out = vec![
        json!({
            "type": "direct",
            "tag": DNS_IN,
            "listen": hijack,
            "listen_port": ports.dns,
            "network": ["tcp", "udp"],
        }),
        json!({
            "type": "tproxy",
            "tag": TPROXY_IN,
            "listen": hijack,
            "listen_port": ports.tproxy,
            "network": ["tcp", "udp"],
        }),
        json!({
            "type": "redirect",
            "tag": REDIRECT_IN,
            "listen": "0.0.0.0",
            "listen_port": ports.redirect,
        }),
        json!({
            "type": "mixed",
            "tag": MIXED_IN,
            "listen": "0.0.0.0",
            "listen_port": ports.mixed,
        }),
    ];
    if let Some(port) = ports.socks {
        out.push(json!({
            "type": "socks",
            "tag": SOCKS_IN,
            "listen": "0.0.0.0",
            "listen_port": port,
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::{Ipv6Mode, Network};

    fn state(mode: Ipv6Mode) -> DesiredState {
        DesiredState {
            network: Network {
                lan_cidr6: "fd00:db8:1::/64".into(),
                ipv6_mode: mode,
                ..Network::default()
            },
            ..DesiredState::baseline()
        }
    }

    #[test]
    fn shapes_match_1_14_field_names() {
        let got = inbounds_json(&state(Ipv6Mode::Off));
        assert_eq!(
            got[0],
            json!({"type": "direct", "tag": "dns-in", "listen": "0.0.0.0",
                   "listen_port": 53, "network": ["tcp", "udp"]})
        );
        assert_eq!(
            got[1],
            json!({"type": "tproxy", "tag": "tproxy-in", "listen": "0.0.0.0",
                   "listen_port": 9888, "network": ["tcp", "udp"]})
        );
        // redirect 只有监听字段，没有 network
        assert_eq!(
            got[2],
            json!({"type": "redirect", "tag": "redirect-in", "listen": "0.0.0.0",
                   "listen_port": 9887})
        );
        assert_eq!(got[3]["type"], "mixed");
        assert_eq!(got[4]["type"], "socks");
        assert_eq!(got[4]["listen_port"], 7890);
    }

    /// 双栈档里只有两条劫持靶子换成 `"::"`。这两句断言分别挡住两种反向错误：
    /// 全都不换（v6 的包进不了任何入站，nft 那两条 ip6 链白打），
    /// 以及全都换（监听面平白扩大，而这几条链在 v6 侧根本不存在）。
    #[test]
    fn only_the_two_hijack_targets_go_dual_stack() {
        let got = inbounds_json(&state(Ipv6Mode::DualStack));
        assert_eq!(got[0]["listen"], json!("::"), "dns-in");
        assert_eq!(got[1]["listen"], json!("::"), "tproxy-in");
        for inbound in &got[2..] {
            assert_eq!(
                inbound["listen"],
                json!("0.0.0.0"),
                "没有对应 v6 链的口不该跟着放开：{inbound}"
            );
        }
        // 只收 DNS 那一档不碰连接层，两个口都不该动
        let dns_only = inbounds_json(&state(Ipv6Mode::DnsOnly));
        assert!(dns_only.iter().all(|i| i["listen"] == json!("0.0.0.0")));
    }

    /// 两只 socket 绑同一端口会不会 EADDRINUSE 是实测过的（不会，v4/v6 地址族各一份），
    /// 但这里的形状是**一只**双栈 socket，不存在重复监听。
    #[test]
    fn dual_stack_uses_one_socket_not_two_inbounds_per_port() {
        let got = inbounds_json(&state(Ipv6Mode::DualStack));
        let ports: Vec<u16> = got
            .iter()
            .map(|i| i["listen_port"].as_u64().unwrap() as u16)
            .collect();
        let mut unique = ports.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(ports.len(), unique.len(), "同一端口开了两只：{ports:?}");
    }

    #[test]
    fn optional_socks_inbound_can_be_dropped() {
        let mut s = state(Ipv6Mode::Off);
        s.ports.socks = None;
        let got = inbounds_json(&s);
        assert_eq!(got.len(), 4);
        assert!(got.iter().all(|i| i["tag"] != json!(SOCKS_IN)));
    }

    #[test]
    fn no_legacy_sniff_or_mark_keys() {
        for mode in [Ipv6Mode::Off, Ipv6Mode::DualStack] {
            let text = serde_json::to_string(&inbounds_json(&state(mode))).unwrap();
            for banned in [
                "sniff",
                "so_mark",
                "udp_disable_domain_unmapping",
                "domain_strategy",
                "set_system_mark",
                "v6_only",
            ] {
                assert!(!text.contains(banned), "{banned} 出现在 {text}");
            }
        }
    }

    #[test]
    fn tags_are_unique() {
        let got = inbounds_json(&state(Ipv6Mode::Off));
        let tags: Vec<&str> = got.iter().map(|i| i["tag"].as_str().unwrap()).collect();
        let mut sorted = tags.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(tags.len(), sorted.len(), "入站 tag 重复会让 check 失败");
    }
}
