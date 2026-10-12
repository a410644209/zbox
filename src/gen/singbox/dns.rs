//! 期望状态 -> sing-box `dns` 段（1.14.2 新版 typed server 格式）。
//!
//! 关键约束（见 docs/reference/singbox-1.14.2-schema.md §2、§3）：
//! - 1.14.0 起 legacy `{"address":"..."}` 服务器与顶层 `dns.fakeip{}` 都是硬报错，
//!   fakeip 只能作为一个 `type:"fakeip"` 的 server 存在
//! - 按域名挑上游只能靠 `dns.rules`，server 对象上没有 domain_suffix 之类的键
//! - DNS 规则的动作集合里没有 `direct`/`sniff`/`hijack-dns`

use crate::gen::singbox::route::ordered_domain_matchers;
use crate::model::config::{
    DesiredState, DomainAction, Upstream, Via, RULESET_ADS, RULESET_CN_DOMAIN,
};
use serde_json::{json, Map, Value};

pub const LOCAL_TAG: &str = "local";
pub const FAKEIP_TAG: &str = "fakeip";

/// 保留 TLD：这些名字应该问系统解析器，不该发去任何公共 DNS
const RESERVED_SUFFIXES: &[&str] = &["local", "lan", "internal", "home.arpa", "localdomain"];

pub fn dns_json(state: &DesiredState) -> Value {
    let mut servers = Vec::new();
    if state.dns.use_system_local {
        servers.push(json!({"type": "local", "tag": LOCAL_TAG}));
    }
    for upstream in &state.dns.domestic {
        // 直连的上游**不写 detour**。`detour:"direct"` 在 1.14.2 上是硬错误：
        // `sing-box run` 起不来，报 `detour to an empty direct outbound makes no sense`
        // （`direct` 出站本来就没有实现，只是个占位）。少写这个键就是走默认路由。
        servers.push(upstream_json(
            upstream,
            None,
            bootstrap_tag(state, upstream),
        ));
    }
    for upstream in &state.dns.overseas {
        let detour = match &upstream.via {
            Via::Direct => None,
            Via::Outbound(tag) => Some(tag.as_str()),
        };
        servers.push(upstream_json(
            upstream,
            detour,
            bootstrap_tag(state, upstream),
        ));
    }
    if state.dns.fakeip.enabled {
        servers.push(json!({
            "type": "fakeip",
            "tag": FAKEIP_TAG,
            "inet4_range": state.dns.fakeip.inet4_range,
            "inet6_range": state.dns.fakeip.inet6_range,
        }));
    }

    let final_tag = state
        .dns
        .overseas
        .first()
        .map(|u| u.tag.clone())
        .or_else(|| state.dns.domestic.first().map(|u| u.tag.clone()))
        .unwrap_or_else(|| LOCAL_TAG.to_string());
    // 名单里"代理"那一档在 DNS 层该问谁：开着 fakeip 就是 fakeip（和今天落到末尾那条
    // query_type 规则的结果一致），否则是 dns.final 的那个上游。
    let proxy_server = if state.dns.fakeip.enabled {
        FAKEIP_TAG
    } else {
        &final_tag
    };

    let mut rules = Vec::new();
    if state.dns.use_system_local {
        rules.push(json!({
            "action": "route",
            "server": LOCAL_TAG,
            "domain_suffix": RESERVED_SUFFIXES,
        }));
    }
    if state.policy.block_ads {
        if let Some(tag) = state
            .rule_sets
            .iter()
            .map(|r| r.tag())
            .find(|t| *t == RULESET_ADS)
        {
            rules.push(json!({"action": "reject", "rule_set": [tag]}));
        }
    }
    // 用户域名名单：顺序即优先级，与 route.rs 第 6 步同向（选项 C）。
    // 三种处置在 DNS 层的写法：拦截 -> reject；直连 -> route 到第一个国内上游
    // （一个国内上游都没有时跳过，与 v1 一致）；代理 -> route 到 proxy_server。
    //
    // "代理"这一档在 v1 里是**不出规则**的（那时候它靠"排在地理库之后"来隐含）。合成有序
    // 名单之后必须显式出规则，否则后面的 geosite-cn / `.cn` 会抢先把同一个域名解析成国内，
    // 而路由层已经判它走代理——又回到 #18 那种两层各说各话的状态。
    // 指向 fakeip 时带上 query_type 守卫：非地址查询（HTTPS/SVCB 那类）fakeip 服务器答不了，
    // 让它们继续落到末尾那条兜底规则，与今天的表现逐字一致。
    for (mut m, action) in ordered_domain_matchers(&state.policy.domain_rules) {
        match action {
            DomainAction::Block => {
                m.insert("action".into(), json!("reject"));
            }
            DomainAction::Direct => {
                let Some(primary) = state.dns.domestic.first() else {
                    continue;
                };
                m.insert("action".into(), json!("route"));
                m.insert("server".into(), json!(primary.tag));
            }
            DomainAction::Proxy => {
                m.insert("action".into(), json!("route"));
                m.insert("server".into(), json!(proxy_server));
                if state.dns.fakeip.enabled {
                    m.insert("query_type".into(), json!(["A", "AAAA"]));
                }
            }
        }
        rules.push(Value::Object(m));
    }
    if state.policy.cn_direct {
        if let Some(primary) = state.dns.domestic.first() {
            if state.rule_sets.iter().any(|r| r.tag() == RULESET_CN_DOMAIN) {
                rules.push(json!({
                    "action": "route",
                    "server": primary.tag,
                    "rule_set": [RULESET_CN_DOMAIN],
                }));
            }
            // .cn 顶级域不在任何 geosite 里，只能单列
            rules.push(json!({
                "action": "route",
                "server": primary.tag,
                "domain_suffix": ["cn"],
            }));
        }
    }
    if state.dns.fakeip.enabled {
        rules.push(json!({
            "action": "route",
            "server": FAKEIP_TAG,
            "query_type": ["A", "AAAA"],
        }));
    }

    let mut dns = Map::new();
    dns.insert("servers".into(), Value::Array(servers));
    dns.insert("rules".into(), Value::Array(rules));
    dns.insert("final".into(), json!(final_tag));
    dns.insert("strategy".into(), json!(state.dns.strategy.singbox_value()));
    // 1.14.0 起 independent_cache 已废弃，不再输出
    dns.insert("cache_capacity".into(), json!(8192));
    Value::Object(dns)
}

/// 地址是**域名**的上游需要一条"自己不用解析"的上游先把它解析出来。
///
/// 只能写在 `dns.servers[].domain_resolver` 上：VM 实测 `route.default_domain_resolver`
/// 覆盖不到 DNS 服务器，缺了就是
/// `initialize DNS server[n]: missing domain resolver for domain server address`。
/// 优先同一段里地址为 IP 的上游，其次系统解析器（`type:"local"`），都没有就由 `validate` 拦下。
fn bootstrap_tag<'a>(state: &'a DesiredState, myself: &Upstream) -> Option<&'a str> {
    if myself.address_is_ip() {
        return None;
    }
    state
        .dns
        .domestic
        .iter()
        .chain(&state.dns.overseas)
        .find(|o| o.tag != myself.tag && o.address_is_ip())
        .map(|o| o.tag.as_str())
        .or(if state.dns.use_system_local {
            Some(LOCAL_TAG)
        } else {
            None
        })
}

/// 一个上游 -> `dns.servers[]` 元素。`detour` 决定它自己怎么出去，
/// `domain_resolver` 是"谁来解析我的地址"。
fn upstream_json(u: &Upstream, detour: Option<&str>, domain_resolver: Option<&str>) -> Value {
    let ty = u.transport.singbox_type();
    let mut m = Map::new();
    m.insert("type".into(), json!(ty));
    m.insert("tag".into(), json!(&u.tag));
    m.insert("server".into(), json!(&u.host));
    m.insert(
        "server_port".into(),
        json!(u.port.unwrap_or_else(|| u.transport.default_port())),
    );
    if let Some(detour) = detour {
        m.insert("detour".into(), json!(detour));
    }
    if let Some(resolver) = domain_resolver {
        m.insert("domain_resolver".into(), json!(resolver));
    }
    if matches!(ty, "tls" | "https" | "h3") {
        let mut tls = Map::new();
        tls.insert("enabled".into(), json!(true));
        // DoT/DoH 的 SNI 默认跟随服务器地址
        tls.insert(
            "server_name".into(),
            json!(u.sni.clone().unwrap_or_else(|| u.host.clone())),
        );
        if ty == "h3" {
            // 缺 h3 ALPN 的话 QUIC 握手必失败
            tls.insert("alpn".into(), json!(["h3"]));
        }
        m.insert("tls".into(), Value::Object(tls));
    }
    if matches!(ty, "https" | "h3") {
        m.insert(
            "path".into(),
            json!(u.path.clone().unwrap_or_else(|| "/dns-query".into())),
        );
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::{
        Dns, DomainAction, DomainPattern, DomainRule, FakeIp, Policy, RuleSet, RULESET_CN_DOMAIN,
    };

    const DOMAIN_KEYS: [&str; 4] = ["domain", "domain_suffix", "domain_keyword", "domain_regex"];

    fn state() -> DesiredState {
        DesiredState {
            dns: Dns::default(),
            policy: Policy::default(),
            rule_sets: vec![RuleSet::Local {
                tag: RULESET_CN_DOMAIN.into(),
                path: "rule-set/geosite-cn.srs".into(),
            }],
            ..DesiredState::baseline()
        }
    }

    #[test]
    fn legacy_dns_shapes_are_never_emitted() {
        let mut s = state();
        s.dns.fakeip = FakeIp {
            enabled: true,
            ..FakeIp::default()
        };
        let got = dns_json(&s);
        let text = got.to_string();
        // 这两条是 1.14 硬报错的写法，必须在任何配置里都不出现
        assert!(!text.contains("\"address\""), "{text}");
        assert!(!text.contains("\"independent_cache\""), "{text}");
        // fakeip 只能是 server，不能是顶层块
        assert!(got.get("fakeip").is_none(), "{text}");
        assert!(!text.contains("\"inet4_range\":{"), "{text}");
    }

    #[test]
    fn servers_are_typed_with_server_key() {
        let got = dns_json(&state());
        let servers = got["servers"].as_array().unwrap();
        assert_eq!(servers[0], json!({"type": "local", "tag": "local"}));
        assert_eq!(
            servers[1],
            json!({
                "type": "https",
                "tag": "domestic",
                "server": "223.5.5.5",
                "server_port": 443,
                // 没有 detour：VM 实测 `detour:"direct"` 会让 sing-box 启动失败
                // （"detour to an empty direct outbound makes no sense"）
                "tls": {"enabled": true, "server_name": "223.5.5.5"},
                "path": "/dns-query"
            })
        );
    }

    #[test]
    fn overseas_upstream_detours_through_the_proxy_group() {
        let mut s = state();
        s.dns.overseas = vec![Upstream {
            tag: "overseas".into(),
            host: "8.8.8.8".into(),
            port: None,
            transport: crate::model::config::UpstreamTransport::Udp,
            path: None,
            sni: None,
            via: Via::Outbound("代理".into()),
        }];
        let got = dns_json(&s);
        assert_eq!(got["final"], "overseas");
        let overseas = got["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["tag"] == "overseas")
            .unwrap();
        assert_eq!(
            overseas,
            &json!({"type": "udp", "tag": "overseas", "server": "8.8.8.8",
                    "server_port": 53, "detour": "代理"})
        );
    }

    #[test]
    fn fakeip_is_a_server_and_takes_a_and_aaaa() {
        let mut s = state();
        s.dns.fakeip = FakeIp {
            enabled: true,
            inet4_range: "198.18.0.0/15".into(),
            inet6_range: "fc00::/18".into(),
        };
        let got = dns_json(&s);
        assert!(got["servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["type"] == "fakeip" && x["inet4_range"] == "198.18.0.0/15"));
        assert!(got["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["server"] == "fakeip" && r["query_type"] == json!(["A", "AAAA"])));
    }

    #[test]
    fn fakeip_rule_is_last_because_route_is_terminal() {
        // VM 实测（docs/reference/singbox-1.14.2-schema.md §11.7）：`action:"route"` 是**终止**
        // 语义，命中的那条出结果、后面的规则不再看。于是顺序本身就是正确性：fakeip 那条
        // query_type 兜底规则一旦排到国内/局域网规则之前，国内域名也会拿到 198.18.x.x，
        // 再被 route.final 送进代理组 —— 分流反过来自杀。
        // 名单里"代理"那一档确实会提前指向 fakeip，但那条一定带域名匹配（只圈住用户点名的
        // 域名），所以这里查的是"末尾之前不许出现无域名的 fakeip 规则"。
        let mut s = state();
        s.dns.fakeip = FakeIp {
            enabled: true,
            ..FakeIp::default()
        };
        s.policy.domain_rules = vec![DomainRule::new(
            DomainPattern::Suffix("proxy.me".into()),
            DomainAction::Proxy,
        )];
        let rules = dns_json(&s)["rules"].as_array().unwrap().clone();
        let last = rules.last().expect("dns.rules 不该是空的");
        assert_eq!(last["server"], "fakeip", "{last}");
        assert_eq!(last["query_type"], json!(["A", "AAAA"]));
        assert!(
            DOMAIN_KEYS.iter().all(|k| last.get(*k).is_none()),
            "末尾那条兜底规则不该带域名匹配：{last}"
        );
        for rule in &rules[..rules.len() - 1] {
            if rule["server"] == "fakeip" {
                assert!(
                    DOMAIN_KEYS.iter().any(|k| rule.get(*k).is_some()),
                    "提前指向 fakeip 的规则必须圈住具体域名：{rule}"
                );
            }
        }
    }

    #[test]
    fn a_proxy_entry_resolves_overseas_instead_of_letting_the_cn_rules_win() {
        // 选项 C 在 DNS 层的那一半：代理档要显式出规则，否则后面的 geosite-cn / `.cn`
        // 抢先解析成国内，而路由层已经判它走代理。
        let mut s = state();
        s.dns.fakeip = FakeIp {
            enabled: false,
            ..FakeIp::default()
        };
        s.policy.cn_direct = true;
        s.policy.domain_rules = vec![DomainRule::new(
            DomainPattern::Suffix("cn.test".into()),
            DomainAction::Proxy,
        )];
        let rules = dns_json(&s)["rules"].as_array().unwrap().clone();
        let proxy = rules
            .iter()
            .position(|r| {
                r["action"] == "route"
                    && r["server"] == "overseas"
                    && r["domain_suffix"] == json!(["cn.test"])
            })
            .expect("代理档没生成 DNS 规则");
        let cn = rules
            .iter()
            .position(|r| r["rule_set"] == json!([RULESET_CN_DOMAIN]))
            .expect("geosite-cn 规则没生成");
        assert!(proxy < cn, "代理档要压过地理库：{proxy} vs {cn}");
        // fakeip 关着，所以这条不该带 query_type 守卫，也不该指向 fakeip
        assert_eq!(rules[proxy]["server"], "overseas");
        assert!(rules[proxy].get("query_type").is_none());
    }

    #[test]
    fn a_proxy_entry_keeps_non_address_queries_off_the_fakeip_server() {
        // fakeip 服务器答不了 HTTPS/SVCB 那类查询，所以带守卫，让它们继续落到末尾兜底
        let mut s = state();
        s.dns.fakeip = FakeIp {
            enabled: true,
            ..FakeIp::default()
        };
        s.policy.domain_rules = vec![DomainRule::new(
            DomainPattern::Exact("proxy.me".into()),
            DomainAction::Proxy,
        )];
        let rules = dns_json(&s)["rules"].as_array().unwrap().clone();
        let proxy = rules
            .iter()
            .find(|r| r["domain"] == json!(["proxy.me"]))
            .expect("代理档没生成 DNS 规则");
        assert_eq!(proxy["server"], "fakeip", "{proxy}");
        assert_eq!(proxy["query_type"], json!(["A", "AAAA"]), "{proxy}");
    }

    #[test]
    fn a_direct_entry_without_any_domestic_upstream_emits_no_rule() {
        // 没国内上游时"送国内解析"没有可指的 server，写一个不存在的 tag 会让 check 失败
        let mut s = state();
        s.dns.domestic.clear();
        s.policy.domain_rules = vec![DomainRule::new(
            DomainPattern::Exact("a.test".into()),
            DomainAction::Direct,
        )];
        let rules = dns_json(&s)["rules"].as_array().unwrap().clone();
        assert!(
            rules.iter().all(
                |r| r["domain"] != json!(["a.test"]) && r["domain_suffix"] != json!(["a.test"])
            ),
            "{rules:?}"
        );
    }

    #[test]
    fn baseline_falls_back_to_domestic_final() {
        let mut s = state();
        s.dns.overseas.clear();
        let got = dns_json(&s);
        assert_eq!(got["final"], "domestic");
        assert_eq!(got["strategy"], "prefer_ipv4");
    }

    #[test]
    fn a_domain_addressed_upstream_is_given_a_bootstrap() {
        let mut s = state();
        s.dns.overseas = vec![Upstream {
            tag: "doh-google".into(),
            host: "dns.google".into(),
            port: None,
            transport: crate::model::config::UpstreamTransport::DoH,
            path: Some("/dns-query".into()),
            sni: None,
            via: Via::Direct,
        }];
        let got = dns_json(&s);
        let servers = got["servers"].as_array().unwrap();
        let domain = servers
            .iter()
            .find(|x| x["tag"] == "doh-google")
            .expect("域名地址的上游没生成");
        // 缺这个键 sing-box 直接拒启动：initialize DNS server[n]: missing domain resolver
        assert_eq!(domain["domain_resolver"], "domestic", "{domain}");
        // 地址本来就是 IP 的上游不需要引导，多写反而误导
        assert!(servers
            .iter()
            .find(|x| x["tag"] == "domestic")
            .unwrap()
            .get("domain_resolver")
            .is_none());
    }

    #[test]
    fn the_bootstrap_prefers_an_ip_upstream_over_the_system_resolver() {
        let mut s = state();
        s.dns.domestic = vec![Upstream {
            tag: "doh-cn".into(),
            host: "dns.alidns.com".into(),
            port: None,
            transport: crate::model::config::UpstreamTransport::DoH,
            path: Some("/dns-query".into()),
            sni: None,
            via: Via::Direct,
        }];
        s.dns.overseas = vec![Upstream {
            tag: "plain".into(),
            host: "119.29.29.29".into(),
            port: Some(53),
            transport: crate::model::config::UpstreamTransport::Udp,
            path: None,
            sni: None,
            via: Via::Direct,
        }];
        let got = dns_json(&s);
        let servers = got["servers"].as_array().unwrap();
        let doh = servers.iter().find(|x| x["tag"] == "doh-cn").unwrap();
        // 系统解析器（local）可以兜底，但能用 IP 上游就别用它——旁路由上本机
        // resolv.conf 可能正是指向 sing-box 自己，用它解析会绕回自己
        assert_eq!(doh["domain_resolver"], "plain", "{doh}");
    }
}
