//! 分流梯级：把**期望态**读成一张表——每一格在解析层排第几条、在连接层排第几条，或者根本不落行。
//!
//! 这一模块只说期望态自己的性质，不含任何现场读数（哪一份在跑、下发过没有，由
//! 概览 / 生成预览 / DNS 记录 那三屏各自说）。它存在的理由是：那三屏给的都是文本与服务状态，
//! 而面板上改了十几格之后，用户拿不到一句"所以现在什么流量怎么走"。
//!
//! 两列序号**分别**是 [`crate::gen::dae`] 的 `routing {}` 与 [`crate::gen::oxidns`] 那条链的
//! 真实顺序；跨层没有统一顺序（两台引擎各跑各的），所以一行带**两个**序号，
//! 哪一层不落行就是 `None` + 一句为什么。这份对应关系不靠注释维持：
//! `the_ladder_lines_up_with_both_renderers` 拿两份渲染出来的文本逐行对，
//! 生成器换了顺序而这里没跟着换，测试当场红。

use crate::doctor::covers;
use crate::gen::dae;
use crate::gen::oxidns;
use crate::model::config::{DesiredState, DnsStrategy, DomainAction, FinalOutbound};
use crate::model::device::DeviceMode;
use serde::Serialize;

/// 一行梯级。`tier` 是这一段属于哪一层，面板按它分组；`from` 是这一格在期望态里的落点
/// （`policy.cn_direct` 这类字段路径），用户顺着它能回到编辑那一格。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Row {
    pub tier: &'static str,
    /// 条件，用面板里那种写法（`+example.com`、设备 IP、端口段）
    pub when: String,
    /// 结果。只说这一格要什么，不说它生不生效（生效与否看 `dns` / `connection` 有没有序号）
    pub then: String,
    pub from: String,
    /// 这一行在解析层（oxidns 链）里的序号，1 起；`None` = 解析层没有这一行
    pub dns: Option<usize>,
    /// 这一行在连接层（dae 的 `routing {}`）里的序号，1 起；`None` = 连接层没有这一行
    pub connection: Option<usize>,
    /// 连接层占几行。`policy.cn_direct` 是 2（域名与 IP 各一条），不落行的是 0
    pub connection_lines: usize,
    /// 某一层没落行时的那句为什么。两层都落了时是 `None`
    pub why: Option<String>,
    /// 被表里第几行整个盖住（只有用户自己写的域名名单判得出来，判据是 [`covers`]）
    pub shadowed_by: Option<usize>,
    /// 这一行在数据面里是否已生效。`None` = 无从判断（修订里没存 dae/oxidns 文本，逐行没有可比落点）；
    /// `Some(true)` = 已部署的那份里这一行在；`Some(false)` = 已部署的那份里这一行不在。
    /// 当下永远是 `None`——逐行对照要等 P4 把 dae/oxidns 文本也存进修订。
    pub applied: Option<bool>,
    /// 漂移钉子：这一行在那一份渲染文本里必须出现的样子。只给测试用，不进回执
    #[serde(skip)]
    pub probe_dns: Option<String>,
    #[serde(skip)]
    pub probe_conn: Option<String>,
}

/// 期望态 -> 梯级表。不渲染、不校验、不读盘，所以任何状态都出一张完整的表：
/// 渲染不过的那几格由生成预览那一屏说，这里不重复报错、也不改口径。
pub fn derive(state: &DesiredState) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut conn = 0usize;
    let mut dns = 0usize;

    // 卫生层：连接层永远排在最前的一行，任何开关都影响不到它。
    conn += 1;
    rows.push(Row {
        tier: "fixed",
        when: "组播与保留段".to_string(),
        then: "直连".to_string(),
        from: "生成时固定，不受任何开关影响".to_string(),
        dns: None,
        connection: Some(conn),
        connection_lines: 1,
        why: Some("这一行管的是包往哪走，不是名字往哪查，所以解析层没有它。".to_string()),
        shadowed_by: None,
        applied: None,
        probe_dns: None,
        probe_conn: Some(dae::LINE_HYGIENE.to_string()),
    });

    // 解析层链首那几行：都在任何条件判断之前，所以也归"固定"那一段。
    if state.dns.records_enabled {
        dns += 1;
        rows.push(Row {
            tier: "fixed",
            when: "每一条查询".to_string(),
            then: "先记下来，再往下走".to_string(),
            from: "dns.records_enabled".to_string(),
            dns: Some(dns),
            connection: None,
            connection_lines: 0,
            why: Some("连接层不看查询，它只看已经建起来的连接。".to_string()),
            shadowed_by: None,
            applied: None,
            probe_dns: Some(oxidns::EXEC_RECORD.to_string()),
            probe_conn: None,
        });
    }
    if let Some(qtype) = oxidns::sink_qtype(&state.dns.strategy) {
        dns += 1;
        rows.push(Row {
            tier: "fixed",
            when: format!("有人问 {qtype} 记录时"),
            then: "不给答案（这一族整个压掉）".to_string(),
            from: "dns.strategy".to_string(),
            dns: Some(dns),
            connection: None,
            connection_lines: 0,
            why: Some(
                "连接层管不到地址族：客户端拿不到这一族的地址，就不会照着它直连出去，\
                 所以这一档只在解析层落行。"
                    .to_string(),
            ),
            shadowed_by: None,
            applied: None,
            probe_dns: Some(oxidns::EXEC_SINKHOLE.to_string()),
            probe_conn: None,
        });
    }
    dns += 1;
    rows.push(Row {
        tier: "fixed",
        when: "每一条查询".to_string(),
        then: "先看缓存，命中就直接用缓存里的答案".to_string(),
        from: "生成时固定，不受任何开关影响".to_string(),
        dns: Some(dns),
        connection: None,
        connection_lines: 0,
        why: Some("缓存只在这一侧有意义：连接层没有\"上次问过\"这件事。".to_string()),
        shadowed_by: None,
        applied: None,
        probe_dns: Some(oxidns::EXEC_CACHE.to_string()),
        probe_conn: None,
    });
    if let Some(family) = oxidns::family_exec(&state.dns.strategy) {
        dns += 1;
        rows.push(Row {
            tier: "fixed",
            when: "两条都答得出来时".to_string(),
            then: format!(
                "把{}排在前面",
                if state.dns.strategy == DnsStrategy::PreferIpv6 {
                    " IPv6"
                } else {
                    " IPv4"
                }
            ),
            from: "dns.strategy".to_string(),
            dns: Some(dns),
            connection: None,
            connection_lines: 0,
            why: Some("这一档只改答案的先后，不改连接往哪走。".to_string()),
            shadowed_by: None,
            applied: None,
            probe_dns: Some(family.to_string()),
            probe_conn: None,
        });
    }

    // 设备层：两层都按 devices[] 的原样顺序；Direct/Proxy 落行，Global/Bypass 都不落。
    for (index, device) in state.devices.iter().enumerate() {
        // 一台设备的解析与连接不会分家：落行的条件在两层是同一个，所以 `emits` 只算一次
        let exec = match device.mode {
            DeviceMode::Direct => Some(oxidns::EXEC_DOMESTIC),
            DeviceMode::Proxy => Some(oxidns::EXEC_OVERSEAS),
            _ => None,
        };
        let emits = exec.is_some();
        if emits {
            conn += 1;
            dns += 1;
        }
        let (then, why): (String, Option<String>) = match device.mode {
            DeviceMode::Global => (
                "跟随全局策略".to_string(),
                Some("这一档在两层都不留行：这台设备走的是兜底那一支。".to_string()),
            ),
            DeviceMode::Bypass => (
                "完全不经本机".to_string(),
                Some("这一档的包根本不来这台机器，动作在主路由侧，所以两层都不留行。".to_string()),
            ),
            DeviceMode::Direct => ("强制直连".to_string(), None),
            DeviceMode::Proxy => (
                match &state.policy.final_outbound {
                    FinalOutbound::Proxy(tag) => format!("强制走策略组 {tag}"),
                    FinalOutbound::Direct => {
                        "强制走代理，但兜底出口是直连、没有可去的组（下发会被拒）".to_string()
                    }
                },
                None,
            ),
        };
        rows.push(Row {
            tier: "device",
            when: format!("来自 {} 的流量", device.ip),
            then,
            from: format!("devices[{index}].mode"),
            dns: if emits { Some(dns) } else { None },
            connection: if emits { Some(conn) } else { None },
            connection_lines: if emits { 1 } else { 0 },
            why,
            shadowed_by: None,
            applied: None,
            probe_dns: exec.map(str::to_string),
            probe_conn: if emits {
                Some(format!("sip({})", device.ip))
            } else {
                None
            },
        });
    }

    // 全局层：四格里只有三格在连接层落行，第四格（广告拦截）现在两层都不落。
    let policy = &state.policy;
    if policy.private_direct {
        conn += 1;
        rows.push(Row {
            tier: "global",
            when: "私网 / 回环地址段".to_string(),
            then: "直连".to_string(),
            from: "policy.private_direct".to_string(),
            dns: None,
            connection: Some(conn),
            connection_lines: 1,
            why: Some(
                "解析层不需要这一行：私网名段的解析跟着兜底走，而连接层这一行把包留在局域网里。"
                    .to_string(),
            ),
            shadowed_by: None,
            applied: None,
            probe_dns: None,
            probe_conn: Some(dae::LINE_PRIVATE.to_string()),
        });
    }
    if policy.cn_direct {
        conn += 1;
        rows.push(Row {
            tier: "global",
            when: "中国大陆的域名与 IP".to_string(),
            then: "直连".to_string(),
            from: "policy.cn_direct".to_string(),
            dns: None,
            connection: Some(conn),
            connection_lines: 2,
            why: Some(
                "这一格现在只在连接层落地（域名一条、IP 一条）。解析层落不成：oxidns 的 CN 集合 \
                 provider 读的是本地 .dat 而且从不下载，ZBox 现在产的是 sing-box 的 .srs（G10）。"
                    .to_string(),
            ),
            shadowed_by: None,
            applied: None,
            probe_dns: None,
            probe_conn: Some(dae::LINE_CN_DOMAIN.to_string()),
        });
        // 这一格在连接层占**两**行，序号要跟着走到第二行，否则下一格会顶到它尾巴上
        conn += 1;
    }
    if policy.bittorrent_direct {
        conn += 1;
        rows.push(Row {
            tier: "global",
            when: "TCP 6881-6889（BT 下载端口）".to_string(),
            then: "直连".to_string(),
            from: "policy.bittorrent_direct".to_string(),
            dns: None,
            connection: Some(conn),
            connection_lines: 1,
            why: Some("解析层没有端口这个概念：查询里只有名字。".to_string()),
            shadowed_by: None,
            applied: None,
            probe_dns: None,
            probe_conn: Some(dae::LINE_BITTORRENT.to_string()),
        });
    }
    if policy.block_ads {
        rows.push(Row {
            tier: "global",
            when: "广告域名集合".to_string(),
            then: "拦截".to_string(),
            from: "policy.block_ads".to_string(),
            dns: None,
            connection: None,
            connection_lines: 0,
            why: Some(
                "这一格现在两层都不落行：广告集合的来源要的是逐行文本规则（oxidns 的 \
                 adguard_rule provider），ZBox 现在只有 sing-box 的 .srs（G10）。\
                 自己写的拦截档域名规则不受影响，它们在下面那一段里。"
                    .to_string(),
            ),
            shadowed_by: None,
            applied: None,
            probe_dns: None,
            probe_conn: None,
        });
    }

    // 域名名单：顺序即优先级，两层各按同一个顺序落行。
    let first_domain_row = rows.len();
    for (index, rule) in policy.domain_rules.iter().enumerate() {
        conn += 1;
        dns += 1;
        let exec = match rule.action {
            DomainAction::Direct => oxidns::EXEC_DOMESTIC,
            DomainAction::Proxy => oxidns::EXEC_OVERSEAS,
            DomainAction::Block => oxidns::EXEC_BLOCKED,
        };
        rows.push(Row {
            tier: "domain",
            when: rule.pattern.raw(),
            then: match rule.action {
                DomainAction::Proxy => match &policy.final_outbound {
                    FinalOutbound::Proxy(tag) => format!("走策略组 {tag}"),
                    FinalOutbound::Direct => {
                        "要走代理，但兜底出口是直连、没有可去的组（下发会被拒）".to_string()
                    }
                },
                other => other.label().to_string(),
            },
            from: format!("policy.domain_rules[{index}]"),
            dns: Some(dns),
            connection: Some(conn),
            connection_lines: 1,
            why: None,
            shadowed_by: None,
            applied: None,
            probe_dns: Some(exec.to_string()),
            probe_conn: Some(
                dae::domain_selector(&rule.pattern)
                    .unwrap_or_else(|_| rule.pattern.raw())
                    .trim_start()
                    .to_string(),
            ),
        });
    }
    // 被盖住的判定复用医生那一条（同一个判据，不写两遍）：结果指回表里的行号，
    // 面板那句"这一条永远走不到"才有地方点。
    for offset in 0..policy.domain_rules.len() {
        let later = &policy.domain_rules[offset];
        let Some(earlier) = policy.domain_rules[..offset]
            .iter()
            .position(|candidate| covers(&candidate.pattern, &later.pattern))
        else {
            continue;
        };
        rows[first_domain_row + offset].shadowed_by = Some(first_domain_row + earlier + 1);
    }

    // 兜底：两层各有最后一支，都跟着 `final_outbound`。
    let (then, tail_exec) = match &policy.final_outbound {
        FinalOutbound::Direct => ("直连".to_string(), oxidns::EXEC_DOMESTIC),
        FinalOutbound::Proxy(tag) => (format!("走策略组 {tag}"), oxidns::EXEC_OVERSEAS),
    };
    conn += 1;
    dns += 1;
    let tail_probe = match &policy.final_outbound {
        // 别名会把"选择组 -> 只有一个成员的测速组"改写成指那个成员，所以这里只认 `fallback:` 这个头
        FinalOutbound::Proxy(_) => "fallback:".to_string(),
        FinalOutbound::Direct => "fallback: direct".to_string(),
    };
    rows.push(Row {
        tier: "fallback",
        when: "上面都没落到".to_string(),
        then,
        from: "policy.final_outbound".to_string(),
        dns: Some(dns),
        connection: Some(conn),
        connection_lines: 1,
        why: None,
        shadowed_by: None,
        applied: None,
        probe_dns: Some(tail_exec.to_string()),
        probe_conn: Some(tail_probe),
    });

    // 模型里有、两台引擎都落不成行的那几格。放在表尾，是因为它们不在任何一条判断的顺序里。
    if state.dns.fakeip.enabled {
        rows.push(note_row(
            "fakeip 地址池",
            "dns.fakeip.enabled",
            "这一格现在两层都不落行：dae 的域名归属靠截获 53 端口的真实应答建立，fakeip 把真实地址\
             换成地址池正好打断它；oxidns 侧也没有 fakeip 执行器。它属于 sing-box 那条老路线。",
        ));
    }
    if state.dns.use_system_local {
        rows.push(note_row(
            "本机的 /etc/hosts 那一层",
            "dns.use_system_local",
            "这一格现在是空的：oxidns 的 hosts 插件吃 `<域名> <IP>` 顺序，把它指到 /etc/hosts 会在\
             插件初始化就报错、整个进程起不来（guest 实测），而 oxidns 也没有\"转给系统解析器\"的\
             执行器。这一屏把它显示成没有落点，不显示成已生效。",
        ));
    }
    if !state.rule_sets.is_empty() {
        rows.push(note_row(
            &format!("{} 条规则集声明", state.rule_sets.len()),
            "rule_sets[]",
            "这些 .srs 一条都不在上面这两层里展开：oxidns 的 provider 只吃 .dat、逐行文本与 \
             IP/CIDR 列表，dae 也不消费它（G10）。",
        ));
    }

    rows
}

fn note_row(when: &str, from: &str, why: &str) -> Row {
    Row {
        tier: "note",
        when: when.to_string(),
        then: "没有落点".to_string(),
        from: from.to_string(),
        dns: None,
        connection: None,
        connection_lines: 0,
        why: Some(why.to_string()),
        shadowed_by: None,
        applied: None,
        probe_dns: None,
        probe_conn: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::{
        DomainPattern, DomainRule, Group, GroupKind, Upstream, UpstreamTransport, Via,
    };
    use crate::model::device::Device;
    use crate::model::node::{Node, NodeKind, StreamProtocol};
    use crate::model::node::{Tls, Transport};

    /// 一个能过 `dae::render` 的节点：策略组要有真成员，否则整份渲染不过，
    /// 而漂移那条测试要的是两份文本都渲染得出来。
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
            delay_ms: None,
        }
    }

    fn upstream(tag: &str, host: &str) -> Upstream {
        Upstream {
            tag: tag.into(),
            host: host.into(),
            port: None,
            transport: UpstreamTransport::Udp,
            path: None,
            sni: None,
            via: Via::Direct,
        }
    }

    /// 一份"两层都有内容"的状态：三台设备（强制直连 / 跟随全局 / 完全不经本机）、
    /// 四格全局开关全开、三条域名名单（中间那条被第一条整个盖住）、兜底走组、地址族压掉 AAAA。
    fn a_full_state() -> DesiredState {
        let mut state = DesiredState::baseline();
        // 解析口要显式地址（`oxidns::dns_binds` 那道闸），境外那一组要有上游（链尾要用它）
        state.network.lan_address = "192.0.2.10".into();
        state.dns.overseas = vec![upstream("o1", "198.51.100.53")];
        state.dns.strategy = DnsStrategy::Ipv4Only;
        state.dns.use_system_local = false;
        state.nodes.push(node("n1"));
        state.groups.push(Group {
            tag: "hk".into(),
            kind: GroupKind::Selector,
            members: vec!["n1".into()],
            default: None,
        });
        state.policy.final_outbound = FinalOutbound::Proxy("hk".into());
        state.policy.cn_direct = true;
        state.policy.block_ads = true;
        state.policy.bittorrent_direct = true;
        state.devices.push(Device {
            ip: "192.0.2.30".into(),
            mac: None,
            hostname: Some("电视".into()),
            display_name: None,
            ip_type: crate::model::device::IpType::Dhcp,
            mode: DeviceMode::Direct,
        });
        state.devices.push(Device {
            ip: "192.0.2.31".into(),
            mac: None,
            hostname: None,
            display_name: None,
            ip_type: crate::model::device::IpType::Dhcp,
            mode: DeviceMode::Global,
        });
        state.devices.push(Device {
            ip: "192.0.2.32".into(),
            mac: None,
            hostname: None,
            display_name: None,
            ip_type: crate::model::device::IpType::Dhcp,
            mode: DeviceMode::Bypass,
        });
        state.policy.domain_rules = vec![
            DomainRule::new(DomainPattern::Suffix("cn".into()), DomainAction::Direct),
            DomainRule::new(DomainPattern::Exact("a.cn".into()), DomainAction::Block),
            DomainRule::new(DomainPattern::Suffix("example".into()), DomainAction::Proxy),
        ];
        state
    }

    /// dae 的 `routing {}` 里真落地的规则行（跳过注释与块首尾）。
    ///
    /// 只认**顶格**那一个 `routing {`：`dns {}` 里还嵌着一个同名的 `routing { request { … } }`
    /// （dae 自己的解析路由块），按 trim 之后的样子匹配会把两个块并成一个。
    fn conn_lines(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut inside = false;
        for line in text.lines() {
            if line == "routing {" {
                inside = true;
                continue;
            }
            if !inside {
                continue;
            }
            if line == "}" {
                break;
            }
            let body = line.trim();
            if !body.is_empty() && !body.starts_with('#') {
                out.push(body.to_string());
            }
        }
        out
    }

    /// oxidns 那条链上真落地的执行器。一条规则一个 `exec:`（带 matcher 的那条写在第二行，
    /// 行尾还挂着 `# 注释`），链序就是这些行出现的序。
    fn dns_lines(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|line| {
                let body = line.trim();
                let rest = body
                    .strip_prefix("- exec:")
                    .or_else(|| body.strip_prefix("exec:"))?;
                Some(rest.split('#').next().unwrap_or("").trim().to_string())
            })
            .collect()
    }

    /// 梯级表与两份渲染文本必须逐行对上：序号是真的，`None` 的地方真的没有行。
    /// 这条测试存在的唯一理由就是防漂移——生成器换了顺序而这里没跟着换，它当场红。
    #[test]
    fn the_ladder_lines_up_with_both_renderers() {
        let state = a_full_state();
        let rows = derive(&state);
        let dae_text = crate::gen::dae::render(&state).expect("这份状态要渲染得出来");
        let ox_text = crate::gen::oxidns::render(&state).expect("同上");
        let dae_rules = conn_lines(&dae_text);
        let mut ox_rules = dns_lines(&ox_text);

        // 链尾那条 `accept` 只是把应答定下来，不是一种归属，所以它不在梯级里。
        assert_eq!(
            ox_rules.pop().as_deref(),
            Some("accept"),
            "链尾变了要看这里"
        );

        let counted: usize = rows
            .iter()
            .filter(|row| row.connection.is_some())
            .map(|row| row.connection_lines)
            .sum();
        assert_eq!(
            counted,
            dae_rules.len(),
            "连接层行数与梯级对不上，实际那些行是：{dae_rules:?}"
        );
        let dns_counted = rows.iter().filter(|row| row.dns.is_some()).count();
        assert_eq!(
            dns_counted,
            ox_rules.len(),
            "解析层行数与梯级对不上，实际那些行是：{ox_rules:?}"
        );

        for row in &rows {
            if let Some(order) = row.connection {
                let line = &dae_rules[order - 1];
                let probe = row.probe_conn.as_deref().expect("落了行就该有探针");
                assert!(
                    line.contains(probe),
                    "连接层第 {order} 行不是梯级那一条：{line}"
                );
                // CN 那一格占两行：第二条必须就在紧挨着的下一行
                if row.connection_lines == 2 {
                    assert!(
                        dae_rules[order].contains(dae::LINE_CN_IP),
                        "CN 那两行拆开了：{}",
                        dae_rules[order]
                    );
                }
            }
            if let Some(order) = row.dns {
                let line = &ox_rules[order - 1];
                let probe = row.probe_dns.as_deref().expect("同上");
                assert_eq!(line, probe, "解析层第 {order} 行不是梯级那一条");
            }
        }
    }

    /// 全局那四格：三格落在连接层（其中 CN 占两行），第四格两层都不落——而且这一格
    /// 在两份文本里都只以注释存在，所以梯级不许把它说成"已生效"。
    #[test]
    fn three_of_the_four_switches_land_and_the_fourth_lands_nowhere() {
        let mut state = DesiredState::baseline();
        state.network.lan_address = "192.0.2.10".into();
        state.policy.cn_direct = true;
        state.policy.block_ads = true;
        state.policy.bittorrent_direct = true;
        let rows = derive(&state);
        let dae_text = crate::gen::dae::render(&state).unwrap();
        let ox_text = crate::gen::oxidns::render(&state).unwrap();

        let cn = rows
            .iter()
            .find(|row| row.from == "policy.cn_direct")
            .unwrap();
        assert_eq!(
            cn.connection_lines, 2,
            "CN 那一格在连接层是两条：域名与 IP 各一条"
        );
        assert_eq!(cn.dns, None);
        assert!(dae_text.contains(dae::LINE_CN_DOMAIN) && dae_text.contains(dae::LINE_CN_IP));
        assert!(
            ox_text.contains("# policy.cn_direct"),
            "解析层里它只剩一句注释"
        );
        assert!(!ox_text.contains("geosite:cn"));

        let ads = rows
            .iter()
            .find(|row| row.from == "policy.block_ads")
            .unwrap();
        assert_eq!(ads.connection, None);
        assert_eq!(ads.dns, None);
        assert!(ads.why.as_deref().unwrap().contains("都不落行"));
    }

    /// 被更靠前的名单整个盖住的那一条要指回盖住它的那一行——医生早就报过这件事，
    /// 但那一屏给的是句子，这一屏要给的是位置（面板拿它渲染"第 N 条"）。
    #[test]
    fn a_shadowed_domain_rule_points_at_the_row_that_covers_it() {
        let rows = derive(&a_full_state());
        let position = |field: &str| {
            rows.iter()
                .position(|row| row.from == field)
                .map(|index| index + 1)
        };
        let covered = rows
            .iter()
            .find(|row| row.from == "policy.domain_rules[1]")
            .unwrap();
        assert_eq!(covered.shadowed_by, position("policy.domain_rules[0]"));
        assert_eq!(
            rows.iter()
                .find(|row| row.from == "policy.domain_rules[0]")
                .unwrap()
                .shadowed_by,
            None
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.from == "policy.domain_rules[2]")
                .unwrap()
                .shadowed_by,
            None,
            "后缀 example 盖不住 cn，也不该被 cn 盖住"
        );
    }

    /// 设备那一档只会"两层都有行"或"两层都没有行"：一台设备的解析与连接不会分家。
    /// 而不留行的两档给的必须是**两句不同的为什么**——合成一句就藏掉了"动作在主路由侧"。
    #[test]
    fn devices_that_emit_have_both_layers_and_the_silent_ones_say_why_separately() {
        let rows = derive(&a_full_state());
        for row in rows.iter().filter(|row| row.tier == "device") {
            assert_eq!(
                row.dns.is_some(),
                row.connection.is_some(),
                "{}：设备那一档不会只落一层",
                row.from
            );
        }
        let global = rows
            .iter()
            .find(|row| row.from == "devices[1].mode")
            .unwrap();
        let bypass = rows
            .iter()
            .find(|row| row.from == "devices[2].mode")
            .unwrap();
        assert_eq!(global.dns, None);
        assert_ne!(global.why, bypass.why, "两档都不留行，但不是同一个理由");
        assert!(bypass.why.as_deref().unwrap().contains("主路由"));
    }

    /// 新装的基线也要出一张完整的表：面板不能对着一台还没配过的机器显示"没读到"。
    /// 基线开着"记下每一次查询"、地址族是 prefer_ipv4，所以链首有记录器、有排序、没有抑制。
    #[test]
    fn a_baseline_state_still_gets_a_whole_ladder() {
        let rows = derive(&DesiredState::baseline());
        for tier in ["fixed", "global", "fallback"] {
            assert!(
                rows.iter().any(|row| row.tier == tier),
                "基线梯级少了 {tier} 这一段"
            );
        }
        assert!(!rows.iter().any(|row| row.tier == "device"));
        assert!(!rows.iter().any(|row| row.tier == "domain"));
        let fallback = rows.iter().find(|row| row.tier == "fallback").unwrap();
        assert_eq!(fallback.then, "直连");
        assert!(rows.iter().any(|row| row.from == "dns.records_enabled"));
        assert!(!rows.iter().any(|row| row.then.starts_with("不给答案")));
        assert!(rows
            .iter()
            .any(|row| row.probe_dns.as_deref() == Some("$family")));
        // 基线的 use_system_local 是开的，这一格要出现在表尾那几句"没有落点"里
        assert!(rows
            .iter()
            .any(|row| row.tier == "note" && row.from == "dns.use_system_local"));
    }

    /// 名单里"要走代理"而兜底是直连时，这一格在连接层根本没有可去的组，`dae::render` 会拒绝。
    /// 梯级那一行必须把"下发会被拒"说出来——不重复报错，也不许把这一行显示成已经生效。
    /// 而兜底指向一个不存在的组时说的是**另一句**话：那一格确实有个组名，只是机器上没声明。
    #[test]
    fn a_row_with_nowhere_to_go_says_so_instead_of_looking_fine() {
        let mut state = DesiredState::baseline();
        state.network.lan_address = "192.0.2.10".into();
        state.policy.domain_rules = vec![DomainRule::new(
            DomainPattern::Suffix("t".into()),
            DomainAction::Proxy,
        )];
        let rows = derive(&state);
        let row = rows
            .iter()
            .find(|r| r.from == "policy.domain_rules[0]")
            .unwrap();
        assert!(row.then.contains("下发会被拒"), "{}", row.then);
        assert!(crate::gen::dae::render(&state).is_err(), "前提是渲染不过");

        let mut state = DesiredState::baseline();
        state.network.lan_address = "192.0.2.10".into();
        state.dns.overseas = vec![upstream("o1", "198.51.100.53")];
        state.policy.final_outbound = FinalOutbound::Proxy("gone".into());
        let rows = derive(&state);
        assert_eq!(
            rows.iter().find(|row| row.tier == "fallback").unwrap().then,
            "走策略组 gone"
        );
        assert!(
            crate::gen::dae::render(&state).is_err(),
            "组不存在同样渲染不过"
        );
    }

    /// 回执里十个字段都齐、`None` 落成 `null` 而不是整个键消失，探针不进回执。
    /// 面板按字段名取值，少一个键就是少一列，而"少一列"在界面上读起来像"这一格是空的"。
    #[test]
    fn a_row_arrives_with_the_ten_fields_the_panel_reads() {
        let value = serde_json::to_value(derive(&a_full_state())).unwrap();
        let first = value.as_array().unwrap().first().unwrap().clone();
        let object = first.as_object().unwrap();
        for key in [
            "tier",
            "when",
            "then",
            "from",
            "dns",
            "connection",
            "connection_lines",
            "why",
            "shadowed_by",
            "applied",
        ] {
            assert!(object.contains_key(key), "回执少了 {key}：{object:?}");
        }
        assert!(!object.contains_key("probe_dns"), "探针不该进回执");
        assert!(!object.contains_key("probe_conn"), "同上");
        assert!(first["dns"].is_null(), "None 要落成 null，不能整个键消失");
    }

    /// `applied` 此刻永远是 `null`——逐行对照要等 P4 把 dae/oxidns 文本存进修订。
    /// 这条测试钉的是"全表 null"这个不变量：P4 之后改这一条的断言，别新写一条。
    #[test]
    fn applied_is_always_null_until_p4() {
        let value = serde_json::to_value(derive(&a_full_state())).unwrap();
        for row in value.as_array().unwrap() {
            assert!(
                row["applied"].is_null(),
                "applied 在 P4 之前必须是 null，这一行却是 {}：{row}",
                row["applied"]
            );
        }
    }
}
