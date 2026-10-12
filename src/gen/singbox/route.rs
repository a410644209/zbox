//! 期望状态 -> sing-box `route` 段：分流阶梯 + 规则集引用。
//!
//! 三条必须记住的语义（都直接决定"装完会不会断网"）：
//! 1. **一条规则里出现的多个匹配字段是"与"关系**。所以同一批域名不能一次性塞进
//!    `domain` + `domain_suffix` + `domain_keyword`，必须按匹配类型拆开——
//!    这就是 [`ordered_domain_matchers`] 只做"相邻同型同处置"合并的原因。
//! 2. **规则顺序即优先级**，第一个命中的生效；`route.final` 是兜底出口。
//!    用户的域名名单因此是一条有序表（`Policy.domain_rules`），不是分档的三份名单。
//! 3. 名单里的"代理"档排在地理库之前：**显式名单压过地理默认**（选项 C 带来的唯一语义变更，
//!    见 docs/ARCHITECTURE.md §4）。

use crate::gen::singbox::dns::LOCAL_TAG;
use crate::gen::singbox::inbound::{DNS_IN, REDIRECT_IN, TPROXY_IN};
use crate::model::config::{
    DesiredState, DomainAction, DomainPattern, DomainRule, FinalOutbound, RuleSet, Via, DIRECT_TAG,
    RULESET_ADS, RULESET_CN_DOMAIN, RULESET_CN_IP, RULESET_NON_CN, RULESET_PRIVATE_IP,
};
use serde_json::{json, Map, Value};

/// sing-box 给自己出站套的防火墙标记，与 nft 层的劫持条件配对使用。
/// 不能和"把局域网流量引进 table 100"的 divert 标记同值，否则本机流量会被自己再劫持一遍。
pub const BYPASS_MARK: u32 = 0x2;
/// nft 给局域网被劫持流量打的标记（`ip rule add fwmark <此值> table 100`）
pub const DIVERT_MARK: u32 = 0x1;
/// 下载远端规则集专用的 HTTP 客户端。
///
/// 必须有：不写 `http_client` 时 sing-box 走"隐式默认客户端 = 默认出站"这条路，
/// 1.14.2 只是 WARN，1.16.0 要删掉；而更现实的问题是默认出站就是代理组，
/// VM 实测下载全被机场出口 IP 挡下（`unexpected HTTP response status: 403`），
/// 规则集初始化失败直接 FATAL，sing-box 根本起不来。
pub const RULESET_CLIENT_TAG: &str = "rule-set-downloader";

#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error(
        "策略需要规则集 {0}，但它不在 rule_sets 里——引用未定义的规则集会让 sing-box check 直接失败"
    )]
    MissingRuleSet(String),
    #[error("设备 {device} 被设为强制代理，但当前没有任何可用的代理策略组")]
    NoProxyGroup { device: String },
    #[error(
        "找不到可用于 default_domain_resolver 的直连上游：真实配置（域名节点 + 远端规则集）缺这个键时\
         sing-box check 直接失败，而节点地址若经由代理解析又是鸡生蛋问题。\
         至少配一条地址是 IP 的国内上游，或开启系统解析器兜底。"
    )]
    NoDomainResolver,
}

/// 有序名单 -> 阶梯里的一段规则，每条带上它自己的处置。
///
/// 相邻且"同匹配型 + 同处置"的条目合并成一条规则（一条规则里的多个域名是**或**关系，
/// 合并不改变命中结果），处置或匹配型一变就断开。这样既守住"顺序即优先级"，
/// 又不必每条域名一条规则地把阶梯撑长。
pub fn ordered_domain_matchers(rules: &[DomainRule]) -> Vec<(Map<String, Value>, DomainAction)> {
    let mut out: Vec<(Map<String, Value>, DomainAction)> = Vec::new();
    for rule in rules {
        let (key, value) = match &rule.pattern {
            DomainPattern::Exact(s) => ("domain", s),
            DomainPattern::Suffix(s) => ("domain_suffix", s),
            DomainPattern::Keyword(s) => ("domain_keyword", s),
            DomainPattern::Regex(s) => ("domain_regex", s),
        };
        match out.last_mut() {
            Some((last, action))
                if *action == rule.action && last.len() == 1 && last.contains_key(key) =>
            {
                last[key].as_array_mut().unwrap().push(json!(value));
            }
            _ => {
                let mut m = Map::new();
                m.insert(key.into(), json!([value]));
                out.push((m, rule.action));
            }
        }
    }
    out
}

pub fn route_json(state: &DesiredState) -> Result<Value, RouteError> {
    let final_tag = match &state.policy.final_outbound {
        FinalOutbound::Proxy(tag) => tag.clone(),
        FinalOutbound::Direct => DIRECT_TAG.to_string(),
    };
    // 强制代理的设备在没有全局代理组时，退到第一个策略组
    let fallback_proxy: Option<String> = match &state.policy.final_outbound {
        FinalOutbound::Proxy(tag) => Some(tag.clone()),
        FinalOutbound::Direct => state.groups.first().map(|g| g.tag.clone()),
    };

    let mut rules: Vec<Value> = Vec::new();

    // 1. DNS 流量交回内置 DNS 引擎（DNAT 进来的和显式指向本机的都覆盖）
    rules.push(json!({"inbound": [DNS_IN], "action": "hijack-dns"}));
    rules.push(json!({
        "inbound": [TPROXY_IN, REDIRECT_IN],
        "protocol": ["dns"],
        "action": "hijack-dns",
    }));

    // 2. 设备级策略，必须先于域名分流，否则"这台机强制直连"会被后面的 geosite-cn 覆盖
    for device in &state.devices {
        let outbound = match device.mode {
            crate::model::device::DeviceMode::Global => continue,
            // Bypass 的语义是"流量根本不该进本机"，由 nft 层排除，sing-box 侧不下规则
            crate::model::device::DeviceMode::Bypass => continue,
            crate::model::device::DeviceMode::Direct => DIRECT_TAG.to_string(),
            crate::model::device::DeviceMode::Proxy => {
                fallback_proxy
                    .clone()
                    .ok_or_else(|| RouteError::NoProxyGroup {
                        device: device.hostname.clone().unwrap_or_else(|| device.ip.clone()),
                    })?
            }
        };
        rules.push(json!({
            "source_ip_cidr": [format!("{}/32", device.ip)],
            "action": "route",
            "outbound": outbound,
        }));
    }

    // 3. 嗅探：透明代理下没有 SNI/Host 之外的域名信息，fakeip 也靠它兜底
    rules.push(json!({"action": "sniff"}));

    // 4. 私网直连
    if state.policy.private_direct {
        rules.push(json!({"ip_is_private": true, "action": "route", "outbound": DIRECT_TAG}));
        rules.push(json!({
            "ip_cidr": [state.network.lan_cidr],
            "action": "route",
            "outbound": DIRECT_TAG,
        }));
        if has_rule_set(state, RULESET_PRIVATE_IP) {
            rules.push(json!({
                "rule_set": [RULESET_PRIVATE_IP],
                "action": "route",
                "outbound": DIRECT_TAG,
            }));
        }
    }

    // 5. 广告规则集拦截。用户自己的名单排在它**之后**，所以"某个域名字母同时在广告库和直连
    // 名单里"时仍是拦截——与 v1 一致，也让 block_ads 这个开关的语义保持"库内一律拦"。
    if state.policy.block_ads {
        require_rule_set(state, RULESET_ADS)?;
        rules.push(json!({"rule_set": [RULESET_ADS], "action": "reject"}));
    }

    // 6. 用户域名名单：**顺序就是优先级**，第一个命中的生效（选项 C）。
    // 三档名单合成这一条之后，"哪档更高"不再是生成器里的默契，而是面板上看得见的行序。
    // 与 dns.rs 的第 3 步同向：同一个域名在两层必须给同一个结论，否则 DNS 侧的处置挡不住
    // 路由侧的放行（客户端有缓存、走 IPv6 或命中静态记录时真会连出去）。
    // 注意设备级 Direct（第 2 步）仍在名单之前：它的语义是"这台机不接管"，本来就不归名单管。
    for (mut m, action) in ordered_domain_matchers(&state.policy.domain_rules) {
        match action {
            DomainAction::Direct | DomainAction::Proxy => {
                let outbound = match action {
                    DomainAction::Direct => DIRECT_TAG.to_string(),
                    _ => final_tag.clone(),
                };
                m.insert("action".into(), json!("route"));
                m.insert("outbound".into(), json!(outbound));
            }
            DomainAction::Block => {
                m.insert("action".into(), json!("reject"));
            }
        }
        rules.push(Value::Object(m));
    }

    // 7. 中国大陆直连
    if state.policy.cn_direct {
        require_rule_set(state, RULESET_CN_DOMAIN)?;
        require_rule_set(state, RULESET_CN_IP)?;
        rules.push(json!({
            "rule_set": [RULESET_CN_DOMAIN],
            "action": "route",
            "outbound": DIRECT_TAG,
        }));
        rules.push(json!({
            "rule_set": [RULESET_CN_IP],
            "action": "route",
            "outbound": DIRECT_TAG,
        }));
    }

    // 8. BT 直连
    if state.policy.bittorrent_direct {
        rules.push(json!({"protocol": ["bittorrent"], "action": "route", "outbound": DIRECT_TAG}));
    }

    // 9. 境外全部走代理：只有最终出口确实是代理时才有意义
    if matches!(state.policy.final_outbound, FinalOutbound::Proxy(_)) {
        require_rule_set(state, RULESET_NON_CN)?;
        rules.push(json!({
            "rule_set": [RULESET_NON_CN],
            "action": "route",
            "outbound": final_tag,
        }));
        if state.dns.fakeip.enabled {
            // fakeip 段里的地址一定是境外域名，直接送代理，省一次 geo 查询
            rules.push(json!({
                "ip_cidr": [state.dns.fakeip.inet4_range.clone(), state.dns.fakeip.inet6_range.clone()],
                "action": "route",
                "outbound": final_tag,
            }));
        }
    }

    let rule_sets: Vec<Value> = state
        .rule_sets
        .iter()
        .map(|rs| match rs {
            RuleSet::Local { tag, path } => json!({
                "type": "local", "tag": tag, "format": "binary", "path": path,
            }),
            RuleSet::Remote {
                tag,
                url,
                initial_path,
                update_interval,
            } => json!({
                "type": "remote", "tag": tag, "format": "binary", "url": url,
                "initial_path": initial_path, "update_interval": update_interval,
                "http_client": RULESET_CLIENT_TAG,
            }),
        })
        .collect();

    let mut route = Map::new();
    route.insert("rules".into(), Value::Array(rules));
    route.insert("rule_set".into(), Value::Array(rule_sets));
    route.insert("final".into(), json!(final_tag));
    route.insert("find_process".into(), json!(false));
    // 旁路由上本机就是出口，不能把出站绑到默认网卡上，否则和 nft 层互相打转
    route.insert("auto_detect_interface".into(), json!(false));
    route.insert("default_mark".into(), json!(BYPASS_MARK));
    route.insert(
        "default_domain_resolver".into(),
        default_domain_resolver(state).ok_or(RouteError::NoDomainResolver)?,
    );
    Ok(Value::Object(route))
}

/// 给出站地址（节点域名）与远端规则集 URL 用的默认解析器。
///
/// VM 实测：ZBox 生成的**每一份**配置（连不含节点的基线也一样）删掉这个键都是
/// `check` rc=1 + `run` FATAL，报
/// `missing 'route.default_domain_resolver' or 'domain_resolver' in dial fields`；
/// 只有手写的最小 IP-only 配置才不触发。逃生阀
/// `ENABLE_DEPRECATED_MISSING_DOMAIN_RESOLVER=true` 只把 FATAL 降成 WARN。
/// 选择顺序体现一条约束——它必须在代理建立**之前**就能工作，所以只认直连类上游，
/// 否则解析节点域名会绕回自己；同为直连时优先地址本来就是 IP 的那一条。
fn default_domain_resolver(state: &DesiredState) -> Option<Value> {
    let direct_ip = state
        .dns
        .domestic
        .iter()
        .chain(&state.dns.overseas)
        .find(|u| u.address_is_ip() && matches!(u.via, Via::Direct));
    let chosen = direct_ip
        .or_else(|| state.dns.domestic.first())
        .or_else(|| {
            state
                .dns
                .overseas
                .iter()
                .find(|u| matches!(u.via, Via::Direct))
        });
    chosen
        .map(|u| json!(u.tag))
        .or_else(|| state.dns.use_system_local.then(|| json!(LOCAL_TAG)))
}

/// 顶层 `http_clients`：只有存在远端规则集时才生成。
///
/// 关键的一条实测结论：**不能给它写 `detour: "direct"`**。sing-box 1.14.2 对
/// "detour 指向 direct 这个空出站"是硬报错（`detour to an empty direct outbound
/// makes no sense`），DNS 服务器和 HTTP 客户端都拒。什么都不写才是直连——
/// 那才是 `ResolveTransport` 里真正建了一个走默认路由的拨号器；写了 `http_client`
/// 也就摆脱了"隐式客户端 = 默认出站（代理组）"那条 1.16.0 要删的老路。
pub fn http_clients_json(state: &DesiredState) -> Option<Value> {
    state
        .rule_sets
        .iter()
        .any(|rs| matches!(rs, RuleSet::Remote { .. }))
        .then(|| json!([{ "tag": RULESET_CLIENT_TAG }]))
}

fn has_rule_set(state: &DesiredState, tag: &str) -> bool {
    state.rule_sets.iter().any(|r| r.tag() == tag)
}

fn require_rule_set(state: &DesiredState, tag: &str) -> Result<(), RouteError> {
    if has_rule_set(state, tag) {
        Ok(())
    } else {
        Err(RouteError::MissingRuleSet(tag.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::{
        builtin_rule_sets, rule_set_product, Dns, Group, GroupKind, Policy,
    };
    use crate::model::device::{Device, DeviceMode};

    fn state() -> DesiredState {
        DesiredState {
            dns: Dns::default(),
            policy: Policy::default(),
            rule_sets: builtin_rule_sets(),
            ..DesiredState::baseline()
        }
    }

    fn rules(s: &DesiredState) -> Vec<Value> {
        route_json(s).unwrap()["rules"].as_array().unwrap().clone()
    }

    #[test]
    fn baseline_ladder_ends_in_direct_and_needs_no_rule_sets() {
        let got = route_json(&DesiredState::baseline()).unwrap();
        assert_eq!(got["final"], DIRECT_TAG);
        let text = got.to_string();
        // 基线不引用任何规则集：第一次下发不该依赖 .srs 是否下得动
        assert!(got["rule_set"].as_array().unwrap().is_empty(), "{text}");
        assert!(!text.contains("geo"), "{text}");
    }

    #[test]
    fn dns_hijack_and_sniff_come_first() {
        let got = rules(&state());
        assert_eq!(
            got[0],
            json!({"inbound": ["dns-in"], "action": "hijack-dns"})
        );
        assert!(got
            .iter()
            .any(|r| r["action"] == "sniff" && r.as_object().unwrap().len() == 1));
    }

    fn rule(raw: &str, action: DomainAction) -> DomainRule {
        DomainRule::new(DomainPattern::parse(raw).unwrap(), action)
    }

    #[test]
    fn domain_fields_are_split_into_separate_rules() {
        let mut s = state();
        s.policy.domain_rules = vec![
            rule("a.com", DomainAction::Direct),
            rule("+b.com", DomainAction::Direct),
            rule("~cd", DomainAction::Direct),
        ];
        let got = route_json(&s).unwrap();
        let hits: Vec<&Value> = got["rules"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["outbound"] == DIRECT_TAG && r["action"] == "route")
            .filter(|r| {
                r.get("domain").is_some()
                    || r.get("domain_suffix").is_some()
                    || r.get("domain_keyword").is_some()
            })
            .collect();
        assert_eq!(hits.len(), 3, "三种匹配必须拆成三条规则");
        // 绝不能出现一条规则同时含 domain 和 domain_suffix（那是"与"，几乎不可能命中）
        for r in &hits {
            let obj = r.as_object().unwrap();
            let kinds = ["domain", "domain_suffix", "domain_keyword", "domain_regex"]
                .iter()
                .filter(|k| obj.contains_key(**k))
                .count();
            assert_eq!(kinds, 1, "{r}");
        }
    }

    #[test]
    fn adjacent_entries_of_the_same_kind_and_action_merge_into_one_rule() {
        let mut s = state();
        s.policy.domain_rules = vec![
            rule("a.test", DomainAction::Direct),
            rule("b.test", DomainAction::Direct),
            rule("c.test", DomainAction::Block),
            rule("d.test", DomainAction::Direct),
        ];
        let got = rules(&s);
        let direct: Vec<&Value> = got
            .iter()
            .filter(|r| r["outbound"] == DIRECT_TAG && r["domain"].is_array())
            .collect();
        assert_eq!(direct.len(), 2, "处置一变就该断开：{direct:?}");
        assert_eq!(direct[0]["domain"], json!(["a.test", "b.test"]));
        assert_eq!(direct[1]["domain"], json!(["d.test"]));
        let reject = at(&got, "第 3 条的拦截", |r| {
            r["action"] == "reject" && r["domain"] == json!(["c.test"])
        });
        let second_batch = at(&got, "d.test 那条直连", |r| {
            r["domain"] == json!(["d.test"])
        });
        assert!(
            reject < second_batch,
            "合并不许把名单里的顺序挪位：{reject} vs {second_batch}"
        );
    }

    #[test]
    fn device_policies_precede_domain_splitting() {
        let mut s = state();
        s.groups = vec![crate::model::config::Group {
            tag: "proxy".into(),
            kind: crate::model::config::GroupKind::Selector,
            members: vec!["direct".into()],
            default: None,
        }];
        s.policy.final_outbound = FinalOutbound::Proxy("proxy".into());
        s.devices = vec![
            Device {
                ip: "192.168.1.50".into(),
                mac: None,
                hostname: Some("电视".into()),
                mode: DeviceMode::Direct,
            },
            Device {
                ip: "192.168.1.60".into(),
                mac: None,
                hostname: None,
                mode: DeviceMode::Proxy,
            },
            Device {
                ip: "192.168.1.70".into(),
                mac: None,
                hostname: None,
                mode: DeviceMode::Bypass,
            },
            Device {
                ip: "192.168.1.80".into(),
                mac: None,
                hostname: None,
                mode: DeviceMode::Global,
            },
        ];
        let got = rules(&s);
        let tv = got
            .iter()
            .position(|r| r["source_ip_cidr"] == json!(["192.168.1.50/32"]))
            .unwrap();
        let cn = got
            .iter()
            .position(|r| r["rule_set"] == json!([RULESET_CN_DOMAIN]))
            .unwrap();
        assert!(tv < cn, "设备规则必须早于地理分流");
        assert_eq!(got[tv]["outbound"], DIRECT_TAG);
        let phone = got
            .iter()
            .position(|r| r["source_ip_cidr"] == json!(["192.168.1.60/32"]))
            .unwrap();
        assert_eq!(got[phone]["outbound"], "proxy");
        // Bypass / Global 不产生 sing-box 规则
        assert_eq!(
            got.iter()
                .filter(|r| r["source_ip_cidr"] == json!(["192.168.1.70/32"]))
                .count(),
            0
        );
        assert_eq!(
            got.iter()
                .filter(|r| r["source_ip_cidr"] == json!(["192.168.1.80/32"]))
                .count(),
            0
        );
    }

    // ---- 映射报告 §3 那张冲突表的护栏 ----
    //
    // sing-box 的 `route` 与 `dns.rules` 里 `action:"route"` 都是**终止**语义（schema §11.7 实测），
    // 一个域名同时命中多条名单时"谁赢"完全由生成顺序决定，配置里不留任何"命中过哪几条"的痕迹。
    // 所以这里断言的是**索引先后**，不是"这条规则存在"：#6 会动阶梯，动之前每条交叉关系都要
    // 被重新确认一次，红了就是有人在无意识地改分流语义。

    /// 阶梯里的位置。找不到就带着整条阶梯 panic——分不清"少生成了一条"和"改了写法"最糟。
    fn at(rules: &[Value], what: &str, find: impl Fn(&Value) -> bool) -> usize {
        rules
            .iter()
            .position(&find)
            .unwrap_or_else(|| panic!("阶梯里没有{what}这条规则：{rules:?}"))
    }

    /// 规则里 `keys` 任一键（`domain`/`domain_suffix`/…）的数组里出现 `values` 任一项。
    fn matches_domain(r: &Value, keys: &[&str], values: &[&str]) -> bool {
        keys.iter().any(|key| {
            r.get(*key).and_then(Value::as_array).is_some_and(|a| {
                a.iter()
                    .any(|v| v.as_str().is_some_and(|s| values.contains(&s)))
            })
        })
    }

    fn sent_to<'a>(outbound: &str, values: &[&'a str]) -> impl Fn(&Value) -> bool + 'a {
        let (outbound, values) = (outbound.to_string(), values.to_vec());
        move |r: &Value| r["outbound"] == outbound && matches_domain(r, &DOMAIN_KEYS, &values)
    }

    fn rejected<'a>(values: &[&'a str]) -> impl Fn(&Value) -> bool + 'a {
        let values = values.to_vec();
        move |r: &Value| r["action"] == "reject" && matches_domain(r, &DOMAIN_KEYS, &values)
    }

    const DOMAIN_KEYS: [&str; 4] = ["domain", "domain_suffix", "domain_keyword", "domain_regex"];

    /// 一个代理组 + 最终出口=它，下面几个测试都要用。
    fn proxying(s: &mut DesiredState) {
        s.groups = vec![Group {
            tag: "proxy".into(),
            kind: GroupKind::Selector,
            members: vec!["direct".into()],
            default: None,
        }];
        s.policy.final_outbound = FinalOutbound::Proxy("proxy".into());
    }

    /// §3 的 2/3 行 vs 9/10 行，在选项 C 之后变成一条更朴素的断言：**名单里谁排在前面谁赢**，
    /// 而且换个写法就得换个结论——档位之间的默认排序已经不存在了，能依赖的只有行序。
    #[test]
    fn the_list_order_decides_which_action_wins() {
        let mut s = state();
        proxying(&mut s);
        s.policy.domain_rules = vec![
            rule("direct-first.test", DomainAction::Direct),
            rule("direct-first.test", DomainAction::Proxy),
            rule("block-first.test", DomainAction::Block),
            rule("block-first.test", DomainAction::Direct),
        ];
        let got = rules(&s);
        let direct = at(
            &got,
            "direct-first.test 送直连",
            sent_to(DIRECT_TAG, &["direct-first.test"]),
        );
        let proxy = at(
            &got,
            "direct-first.test 送代理",
            sent_to("proxy", &["direct-first.test"]),
        );
        assert!(
            direct < proxy,
            "排在前面的直连必须赢过同一域名的代理：{direct} vs {proxy}"
        );
        let reject = at(
            &got,
            "block-first.test 拦截",
            rejected(&["block-first.test"]),
        );
        let also_direct = at(
            &got,
            "block-first.test 送直连",
            sent_to(DIRECT_TAG, &["block-first.test"]),
        );
        assert!(
            reject < also_direct,
            "排在前面的拦截必须赢过同一域名的直连：{reject} vs {also_direct}"
        );
    }

    /// §3 的第 10 行（`cn@!cn`）：同一条域名在国内库和境外库都被声明时判国内。
    #[test]
    fn cn_geo_rules_outrank_the_non_cn_rule() {
        let mut s = state();
        proxying(&mut s);
        s.policy.cn_direct = true;
        let got = rules(&s);
        let cn = at(&got, "geosite-cn 直连", |r| {
            r["rule_set"] == json!([RULESET_CN_DOMAIN]) && r["outbound"] == DIRECT_TAG
        });
        let non_cn = at(&got, "non-cn 送代理", |r| {
            r["rule_set"] == json!([RULESET_NON_CN]) && r["outbound"] == "proxy"
        });
        assert!(cn < non_cn, "国内库要先于境外库落地：{cn} vs {non_cn}");
    }

    /// 选项 C 带来的**唯一语义变更**（要在文档里对外交代的那一条）：整条用户名单排在
    /// 地理库与 BT 默认之前，所以"显式名单压过地理默认"。v1 里 `proxy_domains` 排在
    /// cn_direct 之后，一个既在 geosite-cn 又被用户点名代理的域名当年是直连。
    #[test]
    fn the_user_list_outranks_the_geolocation_defaults() {
        let mut s = state();
        proxying(&mut s);
        s.policy.cn_direct = true;
        s.policy.bittorrent_direct = true;
        s.policy.domain_rules = vec![rule("+cn.test", DomainAction::Proxy)];
        let got = rules(&s);
        let proxy = at(&got, "用户的代理行", sent_to("proxy", &["cn.test"]));
        let cn_domain = at(&got, "geosite-cn 直连", |r| {
            r["rule_set"] == json!([RULESET_CN_DOMAIN]) && r["outbound"] == DIRECT_TAG
        });
        let cn_ip = at(&got, "geoip-cn 直连", |r| {
            r["rule_set"] == json!([RULESET_CN_IP]) && r["outbound"] == DIRECT_TAG
        });
        let bt = at(&got, "BT 直连", |r| {
            r["protocol"] == json!(["bittorrent"]) && r["outbound"] == DIRECT_TAG
        });
        let non_cn = at(&got, "non-cn 送代理", |r| {
            r["rule_set"] == json!([RULESET_NON_CN]) && r["outbound"] == "proxy"
        });
        assert!(
            proxy < cn_domain && proxy < cn_ip && proxy < bt && proxy < non_cn,
            "用户名单必须在地理库与 BT 默认之前：{proxy} vs {cn_domain}/{cn_ip}/{bt}/{non_cn}"
        );
    }

    /// 广告库仍在整条名单之前：`block_ads` 这个开关说的是"库内一律拦"，
    /// 用户把自己的一条域名写成直连不该把广告库打开一个洞（与 v1 一致）。
    #[test]
    fn the_ads_rule_set_still_precedes_the_user_list() {
        let mut s = state();
        proxying(&mut s);
        s.policy.block_ads = true;
        s.policy.domain_rules = vec![rule("tracker.test", DomainAction::Direct)];
        let got = rules(&s);
        let ads = at(&got, "广告库拦截", |r| {
            r["rule_set"] == json!([RULESET_ADS]) && r["action"] == "reject"
        });
        let direct = at(&got, "用户的直连行", sent_to(DIRECT_TAG, &["tracker.test"]));
        assert!(ads < direct, "广告库要压过用户名单：{ads} vs {direct}");
    }

    /// §3 的第 16 行（"列表外域名看上游返回 IP 再判"）在我们这层的等价物：
    /// 私网/局域网段是路由阶梯的第 4 步，一定早于任何按域名送代理的规则。
    #[test]
    fn private_destinations_outrank_the_user_list() {
        let mut s = state();
        proxying(&mut s);
        s.policy.domain_rules = vec![rule("+nas.internal", DomainAction::Proxy)];
        let lan = s.network.lan_cidr.clone();
        let got = rules(&s);
        let private = at(&got, "私网直连", |r| {
            r["ip_is_private"] == json!(true) && r["outbound"] == DIRECT_TAG
        });
        let lan_rule = at(&got, "局域网段直连", |r| {
            r["ip_cidr"] == json!([lan]) && r["outbound"] == DIRECT_TAG
        });
        let proxy = at(&got, "用户的代理行", sent_to("proxy", &["nas.internal"]));
        assert!(
            private < proxy,
            "私网直连压过用户名单：{private} vs {proxy}"
        );
        assert!(
            lan_rule < proxy,
            "局域网段直连压过用户名单：{lan_rule} vs {proxy}"
        );
    }

    /// fakeip 段那条捷径送的是代理组，所以它必须排在拦截与设备级直连**之后**：
    /// 否则"已经拦掉的域名"和"设成直连的设备"都会被假地址夺走。
    #[test]
    fn blocking_and_device_rules_outrank_the_fakeip_shortcut() {
        let mut s = state();
        proxying(&mut s);
        s.dns.fakeip = crate::model::config::FakeIp {
            enabled: true,
            ..Default::default()
        };
        s.policy.domain_rules = vec![rule("ad.test", DomainAction::Block)];
        s.devices = vec![Device {
            ip: "192.168.1.50".into(),
            mac: None,
            hostname: None,
            mode: DeviceMode::Direct,
        }];
        let ranges = json!([
            s.dns.fakeip.inet4_range.clone(),
            s.dns.fakeip.inet6_range.clone()
        ]);
        let got = rules(&s);
        let shortcut = at(&got, "fakeip 段送代理", |r| {
            r["ip_cidr"] == ranges && r["outbound"] == "proxy"
        });
        let reject = at(&got, "用户的拦截行", rejected(&["ad.test"]));
        let device = at(&got, "设备直连", |r| {
            r["source_ip_cidr"] == json!(["192.168.1.50/32"])
        });
        assert!(
            reject < shortcut,
            "拦截不能被 fakeip 短路复活：{reject} vs {shortcut}"
        );
        assert!(
            device < shortcut,
            "设备级直连不能被假地址夺走：{device} vs {shortcut}"
        );
    }

    /// 名单里的处置在两层各自的写法：route 层看 `outbound`，dns 层看 `server`。
    /// 同一个 `(处置, 层)` 只能在这里定义一次，两层断言共用它，免得测试自己先分裂。
    fn matches_action(action: DomainAction, route_layer: bool) -> impl Fn(&Value) -> bool {
        let (key, tag) = match (action, route_layer) {
            (DomainAction::Block, _) => ("action", "reject"),
            (DomainAction::Direct, true) => ("outbound", DIRECT_TAG),
            (DomainAction::Direct, false) => ("server", "domestic"),
            (DomainAction::Proxy, true) => ("outbound", "proxy"),
            (DomainAction::Proxy, false) => ("server", "fakeip"),
        };
        let expected = DomainAction::Block == action;
        move |r: &Value| {
            matches_domain(r, &DOMAIN_KEYS, &["both.test"])
                && (r["action"] == "reject") == expected
                && r[key] == tag
        }
    }

    /// #18 + 选项 C 的定案：**同一份名单在两层按同一个顺序展开**，所以同一个域名不可能
    /// 一层判拦截、另一层判放行。这条同时读 route 和 dns：只改一边就会红。
    /// 断言的是"两层的相对先后一致"，而不是"谁赢"——谁赢由用户写的行序决定。
    #[test]
    fn the_same_list_expands_in_the_same_order_in_both_layers() {
        for (first, second, name) in [
            (DomainAction::Block, DomainAction::Direct, "拦截在前"),
            (DomainAction::Direct, DomainAction::Block, "直连在前"),
            (DomainAction::Proxy, DomainAction::Block, "代理在前"),
        ] {
            let mut s = state();
            proxying(&mut s);
            s.dns.fakeip = crate::model::config::FakeIp {
                enabled: true,
                ..Default::default()
            };
            s.policy.domain_rules = vec![rule("both.test", first), rule("both.test", second)];

            let route = rules(&s);
            let route_first = at(&route, "路由层第一条", matches_action(first, true));
            let route_second = at(&route, "路由层第二条", matches_action(second, true));
            assert!(
                route_first < route_second,
                "{name}时路由层的先后不对：{route_first} vs {route_second}"
            );

            let dns = crate::gen::singbox::dns::dns_json(&s);
            let dns_rules = dns["rules"].as_array().unwrap().clone();
            let dns_first = at(&dns_rules, "DNS 层第一条", matches_action(first, false));
            let dns_second = at(&dns_rules, "DNS 层第二条", matches_action(second, false));
            assert!(
                dns_first < dns_second,
                "{name}时 DNS 层的先后必须与路由层同向：{dns_first} vs {dns_second}"
            );
        }
    }

    #[test]
    fn proxy_mode_without_any_group_is_an_error() {
        let mut s = state();
        s.policy.final_outbound = FinalOutbound::Direct;
        s.groups.clear();
        s.devices = vec![Device {
            ip: "192.168.1.60".into(),
            mac: None,
            hostname: Some("手机".into()),
            mode: DeviceMode::Proxy,
        }];
        match route_json(&s) {
            Err(RouteError::NoProxyGroup { device }) => assert_eq!(device, "手机"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn referencing_an_undefined_rule_set_fails_loudly() {
        let mut s = state();
        s.rule_sets.retain(|r| r.tag() != RULESET_CN_IP);
        match route_json(&s) {
            Err(RouteError::MissingRuleSet(tag)) => assert_eq!(tag, RULESET_CN_IP),
            other => panic!("{other:?}"),
        }
    }

    /// 引用未定义的规则集 tag 会让 sing-box check 整体失败，所以内置集合必须覆盖阶梯用到的每个 tag
    #[test]
    fn builtin_rule_sets_cover_every_tag_the_ladder_references() {
        let mut s = state();
        s.groups = vec![crate::model::config::Group {
            tag: "proxy".into(),
            kind: crate::model::config::GroupKind::Selector,
            members: vec![DIRECT_TAG.into()],
            default: None,
        }];
        s.policy.final_outbound = FinalOutbound::Proxy("proxy".into());
        route_json(&s).unwrap();
        let text = route_json(&s).unwrap().to_string();
        for tag in [
            RULESET_CN_DOMAIN,
            RULESET_CN_IP,
            RULESET_NON_CN,
            RULESET_ADS,
        ] {
            assert!(
                s.rule_sets.iter().any(|r| r.tag() == tag),
                "内置集合缺少规则集 {tag}"
            );
            assert!(text.contains(tag), "阶梯里没有用到 {tag}");
        }
        // 私网段没有官方 .srs（上游 404，实测见 model::config::builtin_rule_sets），
        // 靠 ip_is_private 这条内置判断覆盖，所以它既不在内置集合里、也不该被引用
        assert!(!s.rule_sets.iter().any(|r| r.tag() == RULESET_PRIVATE_IP));
        assert!(text.contains("\"ip_is_private\":true"));
    }

    #[test]
    fn remote_rule_sets_download_in_binary_format_over_a_tagged_client() {
        let got = route_json(&state()).unwrap();
        let sets = got["rule_set"].as_array().unwrap();
        assert!(sets.iter().all(|s| s["format"] == "binary"));
        let remote = sets.iter().find(|s| s["type"] == "remote").unwrap();
        // 必须显式引用命名客户端，否则 sing-box 用"默认出站"下载 = 走代理组
        assert_eq!(remote["http_client"], json!(RULESET_CLIENT_TAG));
        // download_detour 已废弃，必须不出现
        assert!(!got.to_string().contains("download_detour"));
        // 生成的 initial_path 必须逐字等于 `svc::ruleset` 写文件的路径。这一条错了 check 照样
        // 通过，只有冷启动才现形：sing-box 在 initial_path 上看不到文件就去现抓（§11.7）。
        // 内置项里 `non-cn` 的上游文件名是 `geosite-geolocation-!cn`，照抄文件名就会断在这儿。
        for set in sets {
            let tag = set["tag"].as_str().unwrap();
            assert_eq!(set["initial_path"], json!(rule_set_product(tag)), "{tag}");
        }
    }

    #[test]
    fn the_ruleset_client_has_no_detour_because_direct_is_a_fatal_detour() {
        let clients = http_clients_json(&state()).unwrap();
        assert_eq!(clients, json!([{ "tag": RULESET_CLIENT_TAG }]));
        // VM 实测：detour 写成 direct 会让 sing-box run 起不来
        // （`detour to an empty direct outbound makes no sense`），什么都不写才是直连
        assert!(clients[0].get("detour").is_none());
    }

    #[test]
    fn no_http_clients_when_nothing_is_remote() {
        // 基线不含规则集，顶层就不该多出一个 http_clients 键
        assert!(http_clients_json(&DesiredState::baseline()).is_none());
        let local_only = DesiredState {
            rule_sets: vec![RuleSet::Local {
                tag: RULESET_CN_IP.into(),
                path: "rule-set/geoip-cn.srs".into(),
            }],
            ..DesiredState::baseline()
        };
        assert!(http_clients_json(&local_only).is_none());
    }

    #[test]
    fn geolocation_only_cn_and_fakeip_shortcut_when_proxying() {
        let mut s = state();
        s.groups = vec![crate::model::config::Group {
            tag: "proxy".into(),
            kind: crate::model::config::GroupKind::Selector,
            members: vec!["direct".into()],
            default: None,
        }];
        s.policy.final_outbound = FinalOutbound::Proxy("proxy".into());
        s.dns.fakeip = crate::model::config::FakeIp {
            enabled: true,
            ..Default::default()
        };
        let got = rules(&s);
        assert!(got
            .iter()
            .any(|r| r["rule_set"] == json!([RULESET_NON_CN]) && r["outbound"] == "proxy"));
        assert!(got
            .iter()
            .any(|r| r["ip_cidr"] == json!(["198.18.0.0/15", "fc00::/18"])));
    }

    #[test]
    fn outbounds_get_a_bypass_mark_so_they_are_not_re_intercepted() {
        let got = route_json(&state()).unwrap();
        assert_eq!(got["default_mark"], BYPASS_MARK);
        assert_ne!(BYPASS_MARK, DIVERT_MARK);
    }

    #[test]
    fn route_names_a_direct_resolver_for_outbound_addresses() {
        // VM 实测：1.14.2 的 `sing-box check` 缺这个键直接 FATAL
        let got = route_json(&state()).unwrap();
        assert_eq!(got["default_domain_resolver"], "domestic");
    }

    #[test]
    fn a_resolver_that_would_need_the_proxy_is_not_used() {
        // 只剩经由代理的海外上游时，宁可报错也不能生成"解析节点域名要先有节点"的配置
        let mut s = state();
        s.dns = crate::model::config::Dns {
            domestic: Vec::new(),
            use_system_local: false,
            ..Default::default()
        };
        assert!(s
            .dns
            .overseas
            .iter()
            .any(|u| matches!(u.via, Via::Outbound(_))));
        assert!(matches!(route_json(&s), Err(RouteError::NoDomainResolver)));

        // 只有直连海外上游时可以用它
        let mut s = state();
        s.dns = crate::model::config::Dns {
            domestic: Vec::new(),
            overseas: vec![crate::model::config::Upstream {
                tag: "overseas-direct".into(),
                via: Via::Direct,
                ..Default::default()
            }],
            use_system_local: false,
            ..Default::default()
        };
        assert_eq!(
            route_json(&s).unwrap()["default_domain_resolver"],
            "overseas-direct"
        );

        // 什么都不配时退回系统解析器
        let mut s = state();
        s.dns = crate::model::config::Dns {
            domestic: Vec::new(),
            overseas: Vec::new(),
            use_system_local: true,
            ..Default::default()
        };
        assert_eq!(
            route_json(&s).unwrap()["default_domain_resolver"],
            LOCAL_TAG
        );
    }
}
