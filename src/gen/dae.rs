//! 生成层：期望状态 -> **dae** 的那份 `.dae` 文本。
//!
//! 这一层只渲染，**不落盘、不重启、不校验**——校验是 `dae validate` 的职责（`docs/BLUEPRINT.md`
//! §7 第 3 步那道门），下发是 `apply` 的职责。第一期只把它接进面板的"生成预览"。
//!
//! 两个形状上的规矩都来自上游实测而不是文档：
//!
//! - **输出必须确定**：同一份期望态渲染出逐字节相同的文本，否则面板的差异对比与
//!   "这一格动了哪一行"就无从谈起。所以这里不写时间戳，所有列表按模型里的**声明顺序**展开
//!   （`routing` 自上而下第一个命中即停，顺序本身就是语义）。
//! - **表达不了的组合一律报错，不许悄悄少写一行**。dae 没有查询 API、`geo` 只认自带的那份
//!   geodata；一旦"静默降级"，用户看到的就是面板改了、网络没变。
//!
//! 分工（Blueprint §3、§4）：**域名集合与广告拦截归 oxidns，连接判决与拨出归 dae**。
//! 所以本渲染器不消费 `rule_sets`（`.srs` 是 sing-box / oxidns provider 的形状），
//! 也不生成广告行——`policy.block_ads` 在 oxidns 那一份里落地。

use crate::model::config::{
    DesiredState, DnsStrategy, DomainAction, DomainPattern, DomainRule, FinalOutbound, GroupKind,
};
use crate::model::device::{Device, DeviceMode};
use crate::model::node::{Node, NodeKind, QuicProtocol, StreamProtocol, Tls, TlsConfig, Transport};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use std::fmt::Write as _;

/// dae 的内置出站名，策略组不许占用（重名会让"这一行指向谁"变得含混）。
const RESERVED_OUTBOUND: [&str; 3] = ["direct", "must_direct", "block"];

/// `routing {}` 里那几行**固定文本**（卫生层与全局层）。
///
/// `pub(crate)`：梯级那一屏（[`crate::gen::ladder`]）要说"这一格落在连接层第几行"，
/// 而它的判据必须和这里写出去的是同一个字面量。两边各写一遍，将来就是面板说有、
/// 配置里其实没有——那种错误没有任何一处能自己发现，所以这里只许写一遍。
pub(crate) const LINE_HYGIENE: &str = "dip(224.0.0.0/4, 'ff00::/8') -> direct";
pub(crate) const LINE_PRIVATE: &str = "dip(geoip:private) -> direct";
pub(crate) const LINE_CN_DOMAIN: &str = "domain(geosite:cn) -> direct";
pub(crate) const LINE_CN_IP: &str = "dip(geoip:cn) -> direct";
pub(crate) const LINE_BITTORRENT: &str = "l4proto(tcp) && dport(6881-6889) -> direct";

pub fn render(state: &DesiredState) -> Result<String, String> {
    let mut out = String::new();
    out.push_str(
        "# dae 配置 —— 由 ZBox 从期望状态生成，请勿手改。\n\
         # 面板上每一格对应哪一行，见 docs/BLUEPRINT.md §5 那张映射表。\n\
         # routing 自上而下第一个命中即停，所以顺序是：卫生层 -> 设备层 -> 全局层 -> 域名名单 -> fallback。\n",
    );
    let tags = node_tags(state);
    let aliases = group_aliases(state)?;
    render_global(state, &mut out)?;
    render_nodes(state, &tags, &mut out)?;
    render_groups(state, &tags, &aliases, &mut out)?;
    render_dns(state, &mut out);
    render_routing(state, &aliases, &mut out)?;
    Ok(out)
}

/// dae 的策略组是从节点池里 `filter` 出来的，**不能嵌套**。面板允许成员填别的组 tag，
/// 而订阅导入最常见的形状恰好就是「一个选择组，成员只有一个测速组」——那一条在
/// sing-box 里的语义是"这个组就用那个组的策略"，dae 表达同一个意思的办法是直接指向那个组，
/// 所以这里把它改写成**别名**：不声明新组，所有指向它的行写的就是被指的那一个。
///
/// 只改写这一种形状。多个组或组与节点混着填仍然报错——dae 里把两池拼成一池会换策略，
/// 那是"悄悄改了语义"，按这个文件的规矩不许。
fn group_aliases(state: &DesiredState) -> Result<Vec<(String, String)>, String> {
    let mut direct: Vec<(String, String)> = Vec::new();
    for group in &state.groups {
        let GroupKind::Selector = group.kind else {
            continue;
        };
        if group.members.len() != 1 {
            continue;
        }
        let member = &group.members[0];
        if member == &group.tag {
            return Err(format!("策略组 `{}` 的成员是它自己", group.tag));
        }
        if state.groups.iter().any(|g| &g.tag == member) {
            direct.push((group.tag.clone(), member.clone()));
        }
    }
    // 链要走到头（a -> b -> c 落定成 a -> c），成环要报错而不是无限走
    let resolve = |start: &str| -> Result<String, String> {
        let mut current = start.to_string();
        let mut seen = vec![current.clone()];
        while let Some((_, next)) = direct.iter().find(|(from, _)| from == &current) {
            if seen.contains(next) {
                return Err(format!("策略组别名成环：{} -> {next}", seen.join(" -> ")));
            }
            seen.push(next.clone());
            current = next.clone();
        }
        Ok(current)
    };
    let mut aliases = Vec::new();
    for (from, _) in &direct {
        aliases.push((from.clone(), resolve(from)?));
    }
    Ok(aliases)
}

/// 路由里每一行指向策略组的那个位置：别名先落回真正声明的那一个。
fn resolve_group(aliases: &[(String, String)], tag: &str) -> String {
    aliases
        .iter()
        .find(|(from, _)| from == tag)
        .map(|(_, to)| to.clone())
        .unwrap_or_else(|| tag.to_string())
}

/// 节点的 dae 标签 = 面板里那个名字，**但只在名字的字符集 dae 认的时候**。
///
/// 中文节点名在订阅里是常态，而块语法的标识符字符集在本地材料里没有实证（生死门 G12）。
/// 所以这里给不安全的名字换一个确定的 ASCII 标签 `node<序号>`，把人的名字留在链接的
/// `#fragment` 上——面板上看到的仍是原名，配置里的身份是那个确定的标签。
fn node_tags(state: &DesiredState) -> Vec<(String, String)> {
    state
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let tag = if is_identifier(&node.name) {
                node.name.clone()
            } else {
                format!("node{}", index + 1)
            };
            (tag, node.name.clone())
        })
        .collect()
}

/// dae 的块语法认哪种标识符——2026-10-09 在 v2.1.1 上逐字测过（G12 定案）：
/// `g1` `hk-1_a.b` `hk_1` `_x` 过门，`123` `9hk` `.hk` 与任何中文（裸写、加引号都算）解析就失败。
/// 那条词法要求首字符是字母或下划线，所以"全是合法字符"不够，还得看第一个。
fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn render_global(state: &DesiredState, out: &mut String) -> Result<(), String> {
    out.push_str("\nglobal {\n");
    line(out, "tproxy_port", &state.ports.tproxy.to_string())?;
    line(out, "tproxy_port_protect", "true")?;
    // 显式写 0 = 沿用 dae 内部那个保护性 mark（实测 0x100），且不报"未设置"的 WARN。
    line(out, "so_mark_from_dae", "0")?;
    line(out, "log_level", "info")?;
    line(out, "lan_interface", &state.network.lan_interface)?;
    line(out, "wan_interface", &state.network.wan_interface)?;
    // 两个 auto_config_* 留 false：内核参数由 ZBox 自己按网卡补（那两项的默认语义在全典 §6
    // 里就是"outline 与 example 互相矛盾"的那一类，不能依赖）。
    line(out, "auto_config_kernel_parameter", "false")?;
    line(out, "auto_config_firewall_rule", "false")?;
    // domain 模式是"DNS 经过 dae"这条路线的前提：DNS 不被接管，domain() 整个失效。
    line(out, "dial_mode", "domain")?;
    line(out, "sniffing_timeout", "30ms")?;
    line(out, "tls_implementation", "tls")?;
    line(out, "check_interval", "30s")?;
    line(out, "check_tolerance", "50ms")?;
    out.push_str("}\n");
    Ok(())
}

fn render_nodes(
    state: &DesiredState,
    tags: &[(String, String)],
    out: &mut String,
) -> Result<(), String> {
    if !state.nodes.is_empty() {
        out.push_str("\nnode {\n");
        for ((tag, _), node) in tags.iter().zip(state.nodes.iter()) {
            let link = node_link(node)?;
            line(out, tag, &link)?;
        }
        out.push_str("}\n");
    }
    if !state.rule_sets.is_empty() {
        out.push_str(
            "# 注：rule_sets[] 不在这里展开——dae 的 domain(geosite:…) 只认它自带的那份 geodata，\n\
             # validate 永不下载。自定义域名集合由 oxidns 的 provider 承担（见 gen::oxidns）。\n",
        );
    }
    Ok(())
}

fn render_groups(
    state: &DesiredState,
    tags: &[(String, String)],
    aliases: &[(String, String)],
    out: &mut String,
) -> Result<(), String> {
    if state.groups.is_empty() {
        return Ok(());
    }
    // 成员在模型里记的是面板上的节点名，落到 dae 要换成那个确定的标签
    let resolve = |member: &str| -> Option<String> {
        tags.iter()
            .find(|(_, name)| name == member)
            .map(|(tag, _)| tag.clone())
    };
    if !aliases.is_empty() {
        // 别名要在文本里看得见：面板上那一格填的是 `proxy`，配置里出现的是 `auto`，
        // 没有这句注释就成了"UI 改了但配置里查无此行"。
        out.push_str(
            "# 策略组别名（dae 的组不能嵌套，成员只有一个组的那种形状改写为直接指向它）：\n",
        );
        for (from, to) in aliases {
            writeln!(out, "#   {from} -> {to}").unwrap();
        }
    }
    let declared: Vec<&_> = state
        .groups
        .iter()
        .filter(|g| !aliases.iter().any(|(from, _)| from == &g.tag))
        .collect();
    if declared.is_empty() {
        return Ok(());
    }
    out.push_str("\ngroup {\n");
    for group in declared {
        if RESERVED_OUTBOUND.contains(&group.tag.as_str()) {
            return Err(format!("策略组名 `{}` 与 dae 的内置出站重名", group.tag));
        }
        if !is_identifier(&group.tag) {
            return Err(format!(
                "策略组名 `{}` 不是 dae 认的标识符（G12 已实测：首字符要 ASCII 字母或下划线，\
                 其后只能 ASCII 字母数字与 _ - .；中文与数字开头都会让 dae 解析失败）",
                group.tag
            ));
        }
        if group.members.is_empty() {
            return Err(format!("策略组 `{}` 没有成员", group.tag));
        }
        writeln!(out, "    {} {{", group.tag).unwrap();
        for member in &group.members {
            let tag = resolve(member).ok_or_else(|| {
                format!(
                    "策略组 `{}` 的成员 `{member}` 不是节点名：dae 的 filter 只从节点池里挑，\
                     策略组不能嵌套，内置的 direct/must_direct/block 也不占成员位",
                    group.tag
                )
            })?;
            sub(out, "filter", &format!("name({tag})"));
        }
        match &group.kind {
            // dae 没有"运行期手选节点"的接口，所以选择器就是"钉死第 n 个"；
            // 面板改默认成员 = 改这一行 + reload，这一格因此在预览里看得见。
            GroupKind::Selector => {
                let index =
                    match &group.default {
                        Some(default) => group
                            .members
                            .iter()
                            .position(|m| m == default)
                            .ok_or_else(|| {
                                format!(
                                    "策略组 `{}` 的默认成员 `{default}` 不在成员名单里",
                                    group.tag
                                )
                            })?,
                        None => 0,
                    };
                sub(out, "policy", &format!("fixed({index})"));
            }
            GroupKind::Urltest {
                url,
                interval,
                tolerance,
            } => {
                sub(out, "policy", "min_avg10");
                sub(out, "tcp_check_url", &quote(url));
                sub(out, "check_interval", &maybe_quote(interval));
                sub(out, "check_tolerance", &format!("{tolerance}ms"));
            }
        }
        out.push_str("    }\n");
    }
    out.push_str("}\n");
    Ok(())
}

/// DNS：dae 只把查询转交给本机 oxidns，不做第二个策略脑——**地址族这一档除外**。
///
/// `ipversion_prefer` 的语义来自 `dae export outline`（v2.1.1，guest 实测取回的
/// `.tmp/dae/outline-v2.1.1.json`）："域名同时有 A 与 AAAA 时只答 A，对 AAAA 回空应答"。
/// 这正是 IPv6 防泄漏的原生杠杆——客户端拿不到 AAAA 就不会绕过代理去直连 v6。
fn render_dns(state: &DesiredState, out: &mut String) {
    out.push_str("\ndns {\n");
    let prefer = match state.dns.strategy {
        DnsStrategy::Ipv4Only => Some(4),
        DnsStrategy::Ipv6Only => Some(6),
        DnsStrategy::PreferIpv4 | DnsStrategy::PreferIpv6 => None,
    };
    if let Some(prefer) = prefer {
        let family = if prefer == 4 { "A" } else { "AAAA" };
        writeln!(
            out,
            "    # 地址族：只答 {family}，另一族回空应答（面板 DNS 屏那一档在 dae 的落点）\n    \
             ipversion_prefer: {prefer}\n"
        )
        .unwrap();
    }
    out.push_str("    upstream {\n");
    writeln!(
        out,
        "        oxidns: 'tcp+udp://127.0.0.1:{}'",
        state.ports.dns
    )
    .unwrap();
    out.push_str("    }\n    routing {\n        request {\n            fallback: oxidns\n        }\n    }\n}\n");
    out.push_str(
        "# 注：oxidns 那边若配明文 UDP 53 的上游，那份查询会被 dae 再拦一次回到自己身上（回环）。\n\
         # 生死门 G3 未过之前，oxidns 配置里的上游要写加密协议或 IP 字面量。\n",
    );
    match state.dns.strategy {
        // prefer_* 是"两族都答、按族排序"，dae 没有这一件（outline 的 dns 段只有 ipversion_prefer）。
        // 地址族偏好交给 oxidns 的 prefer_ipv4 / prefer_ipv6 执行器，那份配置里已经有对应的一行。
        DnsStrategy::PreferIpv4 | DnsStrategy::PreferIpv6 => out.push_str(
            "# 注：dns.strategy 的 prefer_* 在 dae 没有对应件，排序由 oxidns 那份做；\n\
             # 两族都会照答，所以这一档不构成 IPv6 防泄漏——要防泄漏得选 *_only。\n",
        ),
        _ => {}
    }
}

fn render_routing(
    state: &DesiredState,
    aliases: &[(String, String)],
    out: &mut String,
) -> Result<(), String> {
    out.push_str("\nrouting {\n");
    out.push_str("    # 卫生层：本该留在局域网的包不能被代理转走，这一层不受任何开关影响\n");
    out.push_str(&format!("    {LINE_HYGIENE}\n"));
    out.push_str("    # 设备层：面板上每台设备那一档\n");
    for device in &state.devices {
        device_rule(device, state, aliases, out)?;
    }
    out.push_str("    # 全局层\n");
    let policy = &state.policy;
    if policy.private_direct {
        out.push_str(&format!("    {LINE_PRIVATE}\n"));
    }
    if policy.cn_direct {
        out.push_str(&format!("    {LINE_CN_DOMAIN}\n"));
        out.push_str(&format!("    {LINE_CN_IP}\n"));
    }
    if policy.bittorrent_direct {
        out.push_str(&format!("    {LINE_BITTORRENT}\n"));
    }
    if !policy.domain_rules.is_empty() {
        out.push_str("    # 域名名单（顺序即优先级）\n");
    }
    for rule in &policy.domain_rules {
        out.push_str(&domain_rule_line(rule, state, aliases)?);
    }
    match &state.policy.final_outbound {
        // 全典 §3.4：fallback 默认就是 direct，"不写"与"显式写直连"在排错时是两回事，
        // 所以这一行永远要出现。
        FinalOutbound::Direct => out.push_str("    fallback: direct\n"),
        FinalOutbound::Proxy(tag) => {
            if !state.groups.iter().any(|g| &g.tag == tag) {
                return Err(format!("兜底出口指向不存在的策略组 `{tag}`"));
            }
            let target = resolve_group(aliases, tag);
            writeln!(out, "    fallback: {target}").unwrap();
        }
    }
    out.push_str("}\n");
    Ok(())
}

/// 一台设备 -> 至多一行。`global` 与 `bypass` 在数据面不留行（前者交给全局层，
/// 后者的流量根本不来这台机器，那一档的动作全在主路由侧）。
fn device_rule(
    device: &Device,
    state: &DesiredState,
    aliases: &[(String, String)],
    out: &mut String,
) -> Result<(), String> {
    let outbound = match device.mode {
        DeviceMode::Global | DeviceMode::Bypass => return Ok(()),
        // 连 53 一起绕开 dae：这台设备的解析不该被归属登记
        DeviceMode::Direct => "must_direct".to_string(),
        DeviceMode::Proxy => match &state.policy.final_outbound {
            FinalOutbound::Proxy(tag) => resolve_group(aliases, tag),
            FinalOutbound::Direct => {
                return Err(format!(
                    "设备 {} 要\"全局科学\"，但兜底出口是直连，没有可去的策略组",
                    device.ip
                ))
            }
        },
    };
    out.push_str(&format!(
        "    sip({device_ip}) -> {outbound} # {label}\n",
        device_ip = device.ip,
        label = device_label(device)
    ));
    Ok(())
}

fn device_label(device: &Device) -> String {
    match &device.hostname {
        Some(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => device.ip.clone(),
    }
}

fn domain_rule_line(
    rule: &DomainRule,
    state: &DesiredState,
    aliases: &[(String, String)],
) -> Result<String, String> {
    let selector = domain_selector(&rule.pattern)?;
    let outbound = match rule.action {
        DomainAction::Direct => "direct".to_string(),
        DomainAction::Block => "block".to_string(),
        DomainAction::Proxy => match &state.policy.final_outbound {
            FinalOutbound::Proxy(tag) => resolve_group(aliases, tag),
            FinalOutbound::Direct => {
                return Err(format!(
                    "域名规则 {} 要走代理，但兜底出口是直连，没有可去的策略组",
                    rule.pattern.raw()
                ))
            }
        },
    };
    Ok(format!("    {selector} -> {outbound}\n"))
}

/// 四种域名写法对应 dae 的四个 key；无 key 就是 suffix（全典 §4）。
///
/// `pub(crate)`：梯级那一屏要拿它当"这一行在连接层的样子"，两种引法各写一遍会漂。
pub(crate) fn domain_selector(pattern: &DomainPattern) -> Result<String, String> {
    Ok(match pattern {
        DomainPattern::Exact(v) => format!("domain(full: {})", maybe_quote(v)),
        DomainPattern::Suffix(v) => format!("domain({})", maybe_quote(v)),
        DomainPattern::Keyword(v) => format!("domain(keyword: {})", maybe_quote(v)),
        DomainPattern::Regex(v) => format!("domain(regex: {})", quote(v)),
    })
}

/// 一个节点 -> dae 认的那条链接。`ingest::uri::parse_link` 是反方向，两者成对测。
pub fn node_link(node: &Node) -> Result<String, String> {
    let name = percent(&node.name);
    let host = maybe_bracket(&node.server);
    let address = format!("{host}:{}", node.port);
    let link = match &node.kind {
        NodeKind::Stream {
            protocol,
            transport,
            tls,
        } => match protocol {
            StreamProtocol::Shadowsocks { method, password } => {
                if transport != &Transport::Tcp || tls != &Tls::Disabled {
                    return Err(format!(
                        "节点 {}：ss 链接只在\"裸 TCP 无 TLS\"这一档有实证写法（G11）",
                        node.name
                    ));
                }
                let credential = STANDARD.encode(format!("{method}:{password}"));
                format!("ss://{credential}@{address}#{name}")
            }
            StreamProtocol::Vmess {
                uuid,
                alter_id,
                security,
            } => {
                let mut json = format!(
                    "{{\"v\":\"2\",\"ps\":{},\"add\":{},\"port\":\"{}\",\"id\":\"{uuid}\",\
                     \"aid\":\"{alter_id}\",\"scy\":\"{security}\",\"net\":\"{}\",\"type\":\"\",\
                     \"host\":\"\",\"path\":\"\"",
                    json_str(&node.name),
                    json_str(&node.server),
                    node.port,
                    transport_name(transport)?
                );
                if let Some(extra) = transport_inline(transport)? {
                    json.push(',');
                    json.push_str(&extra);
                }
                if let Tls::Tls(cfg) = tls {
                    json.push_str(&format!(",\"tls\":\"tls\""));
                    json.push_str(&format!(
                        ",\"sni\":{}",
                        json_str(&cfg.sni.clone().unwrap_or_default())
                    ));
                }
                json.push('}');
                format!("vmess://{}#{name}", STANDARD.encode(json))
            }
            StreamProtocol::Trojan { password } => {
                let mut query = Vec::new();
                push_transport_query(transport, &mut query)?;
                if let Tls::Tls(cfg) = tls {
                    query.extend(tls_query(cfg));
                }
                format!(
                    "trojan://{password}@{address}{}#{name}",
                    query_string(&query)
                )
            }
            StreamProtocol::Vless { uuid, flow } => {
                let mut query = vec![("type".to_string(), transport_name(transport)?)];
                if let Some(flow) = flow {
                    query.push(("flow".to_string(), flow.clone()));
                }
                push_transport_query(transport, &mut query)?;
                if let Tls::Tls(cfg) = tls {
                    query.extend(tls_query(cfg));
                }
                format!("vless://{uuid}@{address}{}#{name}", query_string(&query))
            }
        },
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
                let mut query = Vec::new();
                if let Tls::Tls(cfg) = tls {
                    query.extend(tls_query(cfg));
                }
                if *disable_path_mtu_discovery {
                    query.push(("pmtud".to_string(), "false".to_string()));
                }
                if let Some(up) = up_mbps {
                    query.push(("up".to_string(), format!("{up}Mbps")));
                }
                if let Some(down) = down_mbps {
                    query.push(("down".to_string(), format!("{down}Mbps")));
                }
                match (obfs, obfs_password) {
                    (Some(obfs), Some(password)) => {
                        query.push(("obfs".to_string(), obfs.clone()));
                        query.push(("obfs-password".to_string(), password.clone()));
                    }
                    (None, None) => {}
                    _ => {
                        return Err(format!(
                            "节点 {}：混淆要同时给 obfs 与 obfs-password",
                            node.name
                        ))
                    }
                }
                format!(
                    "hysteria2://{password}@{address}/{}#{name}",
                    query_string(&query)
                )
            }
            // G11：tuic 链接的键名在本地材料里没有实证，宁可报错也不生成一条"看着能连"的
            QuicProtocol::Tuic { .. } => {
                return Err(format!(
                    "节点 {}：tuic 链接的写法还没实测（G11），本渲染器拒绝猜测",
                    node.name
                ))
            }
        },
    };
    if link.contains('\n') || link.contains('\'') {
        return Err(format!("节点 {} 的链接里出现了换行或单引号", node.name));
    }
    Ok(link)
}

fn transport_name(transport: &Transport) -> Result<String, String> {
    Ok(match transport {
        Transport::Tcp => "tcp".into(),
        Transport::Ws { .. } => "ws".into(),
        Transport::Grpc { .. } => "grpc".into(),
        Transport::H2 { .. } => "h2".into(),
        Transport::HttpUpgrade { .. } => "httpupgrade".into(),
    })
}

/// 查询串形状的链接（vless / trojan / hysteria2）用的传输参数。
fn push_transport_query(
    transport: &Transport,
    query: &mut Vec<(String, String)>,
) -> Result<(), String> {
    match transport {
        Transport::Tcp => Ok(()),
        Transport::Ws {
            path,
            host,
            headers,
        }
        | Transport::HttpUpgrade {
            path,
            host,
            headers,
        } => {
            if !headers.is_empty() {
                return Err("自定义握手头在 dae 链接里没有对应键（G11）".into());
            }
            query.push(("path".to_string(), path.clone()));
            if let Some(host) = host {
                query.push(("host".to_string(), host.clone()));
            }
            Ok(())
        }
        Transport::H2 { path, host } => {
            query.push(("path".to_string(), path.clone()));
            if let Some(host) = host {
                query.push(("host".to_string(), host.clone()));
            }
            Ok(())
        }
        Transport::Grpc { service_name, .. } => Err(format!(
            "grpc 传输在 dae 链接里的键名{}还没实测（G11）",
            if service_name.is_some() {
                "、连同 serviceName"
            } else {
                ""
            }
        )),
    }
}

/// vmess 的 JSON 是扁平的，传输参数直接落成键。
fn transport_inline(transport: &Transport) -> Result<Option<String>, String> {
    Ok(match transport {
        Transport::Tcp => None,
        Transport::Ws {
            path,
            host,
            headers,
        }
        | Transport::HttpUpgrade {
            path,
            host,
            headers,
        } => {
            if !headers.is_empty() {
                return Err("vmess 链接装不下自定义握手头（G11）".into());
            }
            Some(format!(
                "\"host\":{},\"path\":{}",
                json_str(&host.clone().unwrap_or_default()),
                json_str(path)
            ))
        }
        Transport::Grpc { .. } | Transport::H2 { .. } => {
            return Err(format!(
                "vmess + {} 的链接形状未实测（G11）",
                transport_name(transport)?
            ))
        }
    })
}

fn tls_query(cfg: &TlsConfig) -> Vec<(String, String)> {
    let mut query = Vec::new();
    match &cfg.reality {
        Some(reality) => {
            query.push(("security".into(), "reality".into()));
            query.push(("pbk".into(), reality.public_key.clone()));
            if !reality.short_id.is_empty() {
                query.push(("sid".into(), reality.short_id.clone()));
            }
            if let Some(fingerprint) = &cfg.fingerprint {
                query.push(("fp".into(), fingerprint.clone()));
            }
            if let Some(sni) = &cfg.sni {
                query.push(("sni".into(), sni.clone()));
            }
        }
        None => {
            query.push(("security".into(), "tls".into()));
            if let Some(sni) = &cfg.sni {
                query.push(("sni".into(), sni.clone()));
            }
            if cfg.insecure {
                query.push(("allowInsecure".into(), "1".into()));
            }
            if !cfg.alpn.is_empty() {
                query.push(("alpn".into(), cfg.alpn.join(",")));
            }
            if let Some(fingerprint) = &cfg.fingerprint {
                query.push(("fp".into(), fingerprint.clone()));
            }
        }
    }
    if cfg.ech.is_some() {
        // 不静默丢：ECH 在 dae 链接里没有实证写法，宁可让预览红一条
        query.push(("__zbox_unsupported_ech".into(), "1".into()));
    }
    query
}

fn query_string(query: &[(String, String)]) -> String {
    if query.is_empty() {
        return String::new();
    }
    format!(
        "?{}",
        query
            .iter()
            .map(|(k, v)| format!("{k}={}", percent(v)))
            .collect::<Vec<_>>()
            .join("&")
    )
}

fn json_str(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn maybe_bracket(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// 值里的特殊字符要用单引号包住；值自身带单引号时无法安全表达，报错比截断好。
fn quote(value: &str) -> String {
    format!("'{}'", value)
}

fn needs_quotes(value: &str) -> bool {
    value
        .chars()
        .any(|c| !c.is_ascii_alphanumeric() && !matches!(c, '.' | '_' | '-' | ':'))
}

fn maybe_quote(value: &str) -> String {
    if needs_quotes(value) {
        quote(value)
    } else {
        value.to_string()
    }
}

fn line(out: &mut String, key: &str, value: &str) -> Result<(), String> {
    for (what, text) in [("键", key), ("值", value)] {
        if text.contains('\'') || text.contains('\n') {
            return Err(format!(
                "`{text}` 这个{what}里有单引号或换行，dae 的块语法没有安全的写法"
            ));
        }
    }
    writeln!(out, "    {}: {}", maybe_quote(key), maybe_quote(value)).unwrap();
    Ok(())
}

fn sub(out: &mut String, key: &str, value: &str) {
    writeln!(out, "        {key}: {value}").unwrap();
}

/// URI 的 `#name` 与查询值要百分号编码；`ingest::uri` 那边解回来的是同一种写法。
fn percent(value: &str) -> String {
    let mut s = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                s.push(*byte as char)
            }
            other => write!(s, "%{other:02X}").unwrap(),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::Group;
    use crate::model::node::Reality;

    fn state() -> DesiredState {
        DesiredState::baseline()
    }

    fn node(name: &str) -> Node {
        Node {
            name: name.into(),
            server: "10.0.0.1".into(),
            port: 8388,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Shadowsocks {
                    method: "aes-256-gcm".into(),
                    password: "x".into(),
                },
                transport: Transport::Tcp,
                tls: Tls::Disabled,
            },
        }
    }

    fn selector_group(tag: &str, members: &[&str]) -> Group {
        Group {
            tag: tag.into(),
            kind: GroupKind::Selector,
            members: members.iter().map(|m| m.to_string()).collect(),
            default: None,
        }
    }

    fn urltest_group(tag: &str, members: &[&str]) -> Group {
        Group {
            tag: tag.into(),
            kind: GroupKind::Urltest {
                url: "http://www.gstatic.com/generate_204".into(),
                interval: "3m".into(),
                tolerance: 50,
            },
            members: members.iter().map(|m| m.to_string()).collect(),
            default: None,
        }
    }

    #[test]
    fn the_fallback_line_is_always_written_out() {
        // 全典 §3.4：不写 = 全直连，但"没写"和"写了 direct"在排错时是两回事
        let text = render(&state()).unwrap();
        assert!(text.contains("    fallback: direct\n"), "{text}");
        assert!(text.contains("    tproxy_port: 9888\n"), "{text}");
        assert!(text.contains("    lan_interface: ens33\n"), "{text}");
        assert!(text.contains("oxidns: 'tcp+udp://127.0.0.1:53'"), "{text}");
    }

    /// 订阅导入的形状恰好是「一个选择组，成员只有一个测速组」。dae 的组不能嵌套，
    /// 这一格因此在文本里落成**一句别名注释 + 指向真组的那一行**：面板上填的是 `proxy`，
    /// 配置里看得见它落到 `auto`，而不是悄悄少一段。
    #[test]
    fn a_selector_whose_only_member_is_a_group_becomes_a_visible_alias() {
        let mut s = state();
        s.nodes.push(node("n1"));
        s.nodes.push(node("n2"));
        s.groups.push(urltest_group("auto", &["n1", "n2"]));
        let mut proxy = selector_group("proxy", &["auto"]);
        proxy.default = Some("auto".into());
        s.groups.push(proxy);
        s.policy.final_outbound = FinalOutbound::Proxy("proxy".into());
        let text = render(&s).unwrap();
        assert!(text.contains("#   proxy -> auto\n"), "{text}");
        assert!(text.contains("    auto {"), "{text}");
        assert!(text.contains("    fallback: auto\n"), "{text}");
        assert!(
            !text.contains("    proxy {"),
            "别名那一格不该再声明一个组:\n{text}"
        );
    }

    /// 只有"成员恰好是一个组"这一种形状能等价改写。两个组混在一个选择器里仍然报错：
    /// dae 里一池只能有一种策略，把它们并成一池是**改语义**，不是渲染。
    #[test]
    fn a_selector_over_two_groups_is_still_refused() {
        let mut s = state();
        s.nodes.push(node("n1"));
        s.groups.push(urltest_group("a", &["n1"]));
        s.groups.push(urltest_group("b", &["n1"]));
        s.groups.push(selector_group("proxy", &["a", "b"]));
        s.policy.final_outbound = FinalOutbound::Proxy("proxy".into());
        let err = render(&s).unwrap_err();
        assert!(err.contains("不是节点名"), "{err}");
    }

    /// 别名要走到头：`proxy -> hand -> auto` 落定成指向 `auto`，中间那一格也不声明。
    /// 成环是坏状态，要报错而不是走不完。
    #[test]
    fn an_alias_chain_resolves_to_the_declared_group_and_a_cycle_is_refused() {
        let mut s = state();
        s.nodes.push(node("n1"));
        s.groups.push(urltest_group("auto", &["n1"]));
        s.groups.push(selector_group("hand", &["auto"]));
        s.groups.push(selector_group("proxy", &["hand"]));
        s.policy.final_outbound = FinalOutbound::Proxy("proxy".into());
        let text = render(&s).unwrap();
        assert!(text.contains("    fallback: auto\n"), "{text}");
        assert!(
            !text.contains("    proxy {") && !text.contains("    hand {"),
            "{text}"
        );

        let mut back = state();
        back.groups.push(selector_group("a", &["b"]));
        back.groups.push(selector_group("b", &["a"]));
        back.policy.final_outbound = FinalOutbound::Proxy("a".into());
        assert!(render(&back).unwrap_err().contains("成环"));
    }

    /// 卫生层那一条钉的是多播的 CIDR 本身：dae 官方 `example.dae` 与 README 都写成
    /// `224.0.0.0/3`，那是它们的笔误——`/3` 圈住的是 32.0.0.0–63.255.255.255（一段公网，
    /// 这部分流量会永远直连），而 mDNS/SSDP 那些 224/4、239/4 的地址一个都没盖到。
    /// 照抄模板会把"卫生层不受任何开关影响"那句注释变成假话，所以连写法一起钉住。
    #[test]
    fn the_hygiene_layer_covers_multicast_not_an_unrelated_public_block() {
        let text = render(&state()).unwrap();
        assert!(
            text.contains("dip(224.0.0.0/4, 'ff00::/8') -> direct\n"),
            "{text}"
        );
        assert!(!text.contains("/3,"), "上游模板那个 /3 是笔误，别抄回来");
    }

    /// `*_only` 两档在 dae 有原生落点（`dns.ipversion_prefer`），这既是"选了要真生效"，
    /// 也是 IPv6 防泄漏的主杠杆：客户端拿不到另一族的地址，就不会绕过代理直连。
    #[test]
    fn an_only_strategy_lands_on_ipversion_prefer() {
        for (strategy, prefer, family) in [
            (DnsStrategy::Ipv4Only, 4, "A"),
            (DnsStrategy::Ipv6Only, 6, "AAAA"),
        ] {
            let mut s = state();
            s.dns.strategy = strategy.clone();
            let text = render(&s).unwrap();
            assert!(
                text.contains(&format!("    ipversion_prefer: {prefer}\n")),
                "{strategy:?} 要写 ipversion_prefer: {prefer}\n{text}"
            );
            assert!(
                text.contains(&format!("只答 {family}")),
                "{strategy:?} 那一行要说明压的是哪一族"
            );
        }
    }

    /// 默认档不许带上 `ipversion_prefer`——它一出现就是"另一族的查询被回空应答"，
    /// 那是行为改变，不是偏好排序。
    #[test]
    fn the_default_strategy_leaves_the_family_key_out() {
        let text = render(&state()).unwrap();
        assert!(!text.contains("ipversion_prefer"), "{text}");
    }

    /// `prefer_*` 在 dae 没有对应件，按项目规矩要留一行看得见的字，
    /// 而且要说清"这一档不防泄漏"，否则用户会以为排序等于接管。
    #[test]
    fn a_prefer_strategy_says_it_does_not_guard_the_family() {
        for strategy in [DnsStrategy::PreferIpv4, DnsStrategy::PreferIpv6] {
            let mut s = state();
            s.dns.strategy = strategy.clone();
            let text = render(&s).unwrap();
            assert!(
                !text.contains("ipversion_prefer"),
                "{strategy:?} 不该出现这一件"
            );
            assert!(
                text.contains("不构成 IPv6 防泄漏"),
                "{strategy:?} 要写明这一档不防泄漏"
            );
        }
    }

    #[test]
    fn the_same_state_renders_the_same_bytes_twice() {
        let a = render(&state()).unwrap();
        let b = render(&state()).unwrap();
        assert_eq!(a, b, "预览的差异对比靠确定性");
        assert!(a.ends_with('\n'));
    }

    #[test]
    fn a_device_row_comes_before_the_global_layer() {
        let mut s = state();
        s.nodes.push(node("n1"));
        s.groups.push(selector_group("auto", &["n1"]));
        s.policy.cn_direct = true;
        s.policy.final_outbound = FinalOutbound::Proxy("auto".into());
        s.devices.push(Device {
            ip: "192.168.1.50".into(),
            mac: None,
            hostname: Some("客厅电视".into()),
            mode: DeviceMode::Direct,
        });
        let text = render(&s).unwrap();
        let device_at = text.find("sip(192.168.1.50) -> must_direct").unwrap();
        let cn_at = text.find("dip(geoip:cn) -> direct").unwrap();
        assert!(device_at < cn_at, "{text}");
        assert!(text.contains("# 客厅电视"), "{text}");
    }

    #[test]
    fn a_proxy_device_needs_a_group_to_go_to() {
        let mut s = state();
        s.devices.push(Device {
            ip: "192.168.1.60".into(),
            mac: None,
            hostname: None,
            mode: DeviceMode::Proxy,
        });
        let err = render(&s).unwrap_err();
        assert!(err.contains("兜底出口是直连"), "{err}");
    }

    #[test]
    fn following_and_bypassing_devices_leave_no_line() {
        let mut s = state();
        for mode in [DeviceMode::Global, DeviceMode::Bypass] {
            s.devices.push(Device {
                ip: "192.168.1.70".into(),
                mac: None,
                hostname: None,
                mode,
            });
        }
        let text = render(&s).unwrap();
        assert!(!text.contains("sip(192.168.1.70)"), "{text}");
    }

    #[test]
    fn the_four_domain_shapes_map_to_the_four_dae_keys() {
        let mut s = state();
        s.nodes.push(node("n1"));
        s.groups.push(selector_group("g", &["n1"]));
        s.policy.final_outbound = FinalOutbound::Proxy("g".into());
        s.policy.domain_rules = vec![
            rule(
                DomainPattern::Suffix("example.com".into()),
                DomainAction::Direct,
            ),
            rule(
                DomainPattern::Exact("a.example.com".into()),
                DomainAction::Block,
            ),
            rule(DomainPattern::Keyword("ads".into()), DomainAction::Direct),
            rule(
                DomainPattern::Regex("^x[0-9]+\\.com$".into()),
                DomainAction::Proxy,
            ),
        ];
        let text = render(&s).unwrap();
        for expected in [
            "domain(example.com) -> direct",
            "domain(full: a.example.com) -> block",
            "domain(keyword: ads) -> direct",
            "domain(regex: '^x[0-9]+\\.com$') -> g",
            "        policy: fixed(0)",
        ] {
            assert!(text.contains(expected), "{expected} 没出现在:\n{text}");
        }
    }

    fn rule(pattern: DomainPattern, action: DomainAction) -> DomainRule {
        DomainRule { pattern, action }
    }

    #[test]
    fn a_group_member_that_is_not_a_node_is_refused() {
        // 策略组套策略组、把内置出站当成员，都是 sing-box 那边的形状
        let mut s = state();
        s.nodes.push(node("n1"));
        s.groups.push(selector_group("outer", &["inner", "direct"]));
        let err = render(&s).unwrap_err();
        assert!(err.contains("不是节点名"), "{err}");
    }

    #[test]
    fn a_group_named_like_a_builtin_outbound_is_refused() {
        let mut s = state();
        s.nodes.push(node("n1"));
        s.groups.push(selector_group("direct", &["n1"]));
        assert!(render(&s).unwrap_err().contains("重名"));
    }

    #[test]
    fn urltest_becomes_a_latency_policy_with_its_own_probe() {
        let mut s = state();
        s.nodes.push(node("n1"));
        s.groups.push(Group {
            tag: "auto".into(),
            kind: GroupKind::Urltest {
                url: "http://www.gstatic.com/generate_204".into(),
                interval: "180s".into(),
                tolerance: 30,
            },
            members: vec!["n1".into()],
            default: None,
        });
        let text = render(&s).unwrap();
        for expected in [
            "        policy: min_avg10\n",
            "        tcp_check_url: 'http://www.gstatic.com/generate_204'\n",
            "        check_interval: 180s\n",
            "        check_tolerance: 30ms\n",
        ] {
            assert!(text.contains(expected), "{expected} 不在:\n{text}");
        }
    }

    #[test]
    fn a_selector_pins_whichever_member_is_default() {
        let mut s = state();
        s.nodes.push(node("n1"));
        s.nodes.push(node("n2"));
        s.groups.push(Group {
            tag: "g".into(),
            kind: GroupKind::Selector,
            members: vec!["n1".into(), "n2".into()],
            default: Some("n2".into()),
        });
        let text = render(&s).unwrap();
        assert!(text.contains("        policy: fixed(1)\n"), "{text}");
    }

    #[test]
    fn a_default_member_outside_the_list_is_refused() {
        let mut s = state();
        s.nodes.push(node("n1"));
        s.groups.push(Group {
            tag: "g".into(),
            kind: GroupKind::Selector,
            members: vec!["n1".into()],
            default: Some("n9".into()),
        });
        assert!(render(&s).unwrap_err().contains("不在成员名单里"));
    }

    #[test]
    fn rule_sets_are_oxidns_business_and_say_so() {
        let mut s = state();
        s.policy.block_ads = true;
        s.rule_sets.push(crate::model::config::RuleSet::Local {
            tag: "cn".into(),
            path: "rule-set/cn.srs".into(),
        });
        let text = render(&s).unwrap();
        assert!(!text.contains("category-ads"), "广告拦截在 oxidns 那一侧");
        assert!(text.contains("gen::oxidns"), "{text}");
    }

    #[test]
    fn the_generated_text_stays_inside_the_six_blocks_v2_1_1_has() {
        // 全典 §2：external/program 在 v2.1.1 不存在，生成器一条都不许产出
        let text = render(&state()).unwrap();
        for forbidden in ["external", "program", "mptcp", "bootstrap_resolver"] {
            assert!(!text.contains(forbidden), "{forbidden} 不该出现在:\n{text}");
        }
    }

    #[test]
    fn a_non_ascii_node_name_gets_a_stable_tag_and_keeps_its_name_in_the_link() {
        // 订阅里的节点名九成是中文，所以身份用确定的 ASCII 标签，人名留在 #fragment
        let mut s = state();
        let mut n = node("老节点");
        n.name = "老节点".into();
        s.nodes.push(n);
        s.groups.push(selector_group("auto", &["老节点"]));
        s.policy.final_outbound = FinalOutbound::Proxy("auto".into());
        let text = render(&s).unwrap();
        assert!(text.contains("    node1: 'ss://"), "{text}");
        assert!(text.contains("        filter: name(node1)\n"), "{text}");
        assert!(text.contains("#%E8%80%81%E8%8A%82%E7%82%B9"), "{text}");
        assert!(
            !text.contains("老节点:"),
            "中文当块键在 v2.1.1 上是硬解析错误（G12 实测），身份必须是 ASCII 标签"
        );
    }

    #[test]
    fn a_non_ascii_group_name_is_refused_not_guessed() {
        let mut s = state();
        s.nodes.push(node("n1"));
        s.groups.push(selector_group("自动选", &["n1"]));
        let err = render(&s).unwrap_err();
        assert!(err.contains("G12"), "{err}");
    }

    #[test]
    fn a_digit_or_dot_leading_tag_is_refused_even_though_the_chars_are_legal() {
        // 这三个形状整串都在字符集内，旧判据会放过，而 dae 停在 `mismatched input '{'`：
        // 首字符那条规则不是风格，是实测出来的（2026-10-09 v2.1.1 validate）
        for tag in ["123", "9hk", ".hk"] {
            let mut s = state();
            s.nodes.push(node("n1"));
            s.groups.push(selector_group(tag, &["n1"]));
            let err = render(&s).unwrap_err();
            assert!(err.contains("策略组名"), "{tag}：{err}");
        }
        // 同一个名字当节点名时不该报错，而是换成稳定标签
        let mut s = state();
        let mut n = node("1HK");
        n.name = "1HK".into();
        s.nodes.push(n);
        s.groups.push(selector_group("hk", &["1HK"]));
        s.policy.final_outbound = FinalOutbound::Proxy("hk".into());
        let text = render(&s).unwrap();
        assert!(text.contains("    node1: 'ss://"), "{text}");
        assert!(text.contains("        filter: name(node1)\n"), "{text}");
    }

    #[test]
    fn the_measured_acceptable_tag_shapes_still_render() {
        // 过门的那四个形状要一直过门：收紧判据时容易顺手把它们也挡了
        for tag in ["g1", "hk-1_a.b", "hk_1", "_x"] {
            let mut s = state();
            s.nodes.push(node("n1"));
            s.groups.push(selector_group(tag, &["n1"]));
            s.policy.final_outbound = FinalOutbound::Proxy(tag.into());
            let text = render(&s).unwrap_or_else(|err| panic!("{tag}：{err}"));
            assert!(text.contains(&format!("    {tag} {{\n")), "{text}");
        }
    }

    #[test]
    fn the_ss_link_carries_an_encoded_credential() {
        let link = node_link(&node("ss1")).unwrap();
        assert_eq!(
            link,
            format!(
                "ss://{}@10.0.0.1:8388#ss1",
                STANDARD.encode("aes-256-gcm:x")
            )
        );
    }

    #[test]
    fn an_ss_link_with_tls_is_refused_not_silently_downgraded() {
        let mut n = node("ss1");
        if let NodeKind::Stream { tls, .. } = &mut n.kind {
            *tls = Tls::Tls(TlsConfig {
                sni: Some("x".into()),
                ..Default::default()
            });
        }
        assert!(node_link(&n).unwrap_err().contains("G11"));
    }

    #[test]
    fn the_vmess_link_is_the_flat_json_dae_parses() {
        let n = Node {
            name: "老节点".into(),
            server: "example.com".into(),
            port: 443,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Vmess {
                    uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    alter_id: "1".into(),
                    security: "auto".into(),
                },
                transport: Transport::Ws {
                    path: "/ws".into(),
                    host: Some("example.com".into()),
                    headers: vec![],
                },
                tls: Tls::Tls(TlsConfig {
                    sni: Some("example.com".into()),
                    ..Default::default()
                }),
            },
        };
        let link = node_link(&n).unwrap();
        let body = link
            .trim_start_matches("vmess://")
            .split('#')
            .next()
            .unwrap();
        let json = String::from_utf8(STANDARD.decode(body).unwrap()).unwrap();
        for expected in [
            "\"add\":\"example.com\"",
            "\"port\":\"443\"",
            "\"net\":\"ws\"",
            "\"path\":\"/ws\"",
            "\"tls\":\"tls\"",
            "\"sni\":\"example.com\"",
            "\"ps\":\"老节点\"",
        ] {
            assert!(json.contains(expected), "{expected} 不在 {json}");
        }
        assert!(link.ends_with("#%E8%80%81%E8%8A%82%E7%82%B9"), "{link}");
    }

    #[test]
    fn the_vless_link_keeps_flow_and_reality_keys() {
        let n = Node {
            name: "v".into(),
            server: "example.com".into(),
            port: 443,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Vless {
                    uuid: "u1".into(),
                    flow: Some("xtls-rprx-vision".into()),
                },
                transport: Transport::Tcp,
                tls: Tls::Tls(TlsConfig {
                    sni: Some("example.com".into()),
                    fingerprint: Some("chrome".into()),
                    reality: Some(Reality {
                        public_key: "PUB".into(),
                        short_id: "SID".into(),
                    }),
                    ..Default::default()
                }),
            },
        };
        let link = node_link(&n).unwrap();
        assert!(link.starts_with("vless://u1@example.com:443?"), "{link}");
        for expected in [
            "type=tcp",
            "flow=xtls-rprx-vision",
            "security=reality",
            "pbk=PUB",
            "sid=SID",
            "fp=chrome",
            "sni=example.com",
            "#v",
        ] {
            assert!(link.contains(expected), "{expected} 不在 {link}");
        }
    }

    #[test]
    fn hysteria2_gets_its_options_percent_encoded() {
        let n = Node {
            name: "h".into(),
            server: "example.com".into(),
            port: 443,
            kind: NodeKind::Quic {
                protocol: QuicProtocol::Hysteria2 {
                    password: "secret".into(),
                    obfs: Some("salamander".into()),
                    obfs_password: Some("obfs pass".into()),
                    disable_path_mtu_discovery: true,
                    up_mbps: Some(50),
                    down_mbps: Some(200),
                    tls: Tls::Tls(TlsConfig {
                        sni: Some("example.com".into()),
                        insecure: true,
                        ..Default::default()
                    }),
                },
            },
        };
        let link = node_link(&n).unwrap();
        assert!(
            link.starts_with("hysteria2://secret@example.com:443/?"),
            "{link}"
        );
        for expected in [
            "obfs-password=obfs%20pass",
            "pmtud=false",
            "up=50Mbps",
            "down=200Mbps",
            "sni=example.com",
            "allowInsecure=1",
        ] {
            assert!(link.contains(expected), "{expected} 不在 {link}");
        }
    }

    #[test]
    fn half_a_obfs_config_is_refused() {
        let n = Node {
            name: "h".into(),
            server: "example.com".into(),
            port: 443,
            kind: NodeKind::Quic {
                protocol: QuicProtocol::Hysteria2 {
                    password: "secret".into(),
                    obfs: Some("salamander".into()),
                    obfs_password: None,
                    disable_path_mtu_discovery: false,
                    up_mbps: None,
                    down_mbps: None,
                    tls: Tls::Disabled,
                },
            },
        };
        assert!(node_link(&n).unwrap_err().contains("同时给"));
    }

    #[test]
    fn an_ipv6_server_gets_brackets_before_the_port() {
        let n = Node {
            name: "v6".into(),
            server: "2001:db8::1".into(),
            port: 443,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Trojan {
                    password: "p".into(),
                },
                transport: Transport::Tcp,
                tls: Tls::Disabled,
            },
        };
        assert_eq!(node_link(&n).unwrap(), "trojan://p@[2001:db8::1]:443#v6");
    }

    #[test]
    fn tuic_and_grpc_are_refused_instead_of_guessed() {
        let tuic = Node {
            name: "t".into(),
            server: "example.com".into(),
            port: 443,
            kind: NodeKind::Quic {
                protocol: QuicProtocol::Tuic {
                    uuid: "u".into(),
                    password: "p".into(),
                    alpn: vec![],
                    congestion_control: None,
                    udp_relay_mode: None,
                    udp_over_stream: false,
                    zero_rtt_handshake: false,
                    tls: Tls::Disabled,
                },
            },
        };
        assert!(node_link(&tuic).unwrap_err().contains("G11"));

        let grpc = Node {
            name: "v".into(),
            server: "example.com".into(),
            port: 443,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Vless {
                    uuid: "u".into(),
                    flow: None,
                },
                transport: Transport::Grpc {
                    service_name: Some("x".into()),
                    idle_timeout: None,
                },
                tls: Tls::Disabled,
            },
        };
        assert!(node_link(&grpc).unwrap_err().contains("G11"));
    }

    #[test]
    fn a_value_with_a_quote_is_refused_not_truncated() {
        let mut s = state();
        s.network.lan_interface = "it's".into();
        let err = render(&s).unwrap_err();
        assert!(err.contains("单引号"), "{err}");
    }
}
