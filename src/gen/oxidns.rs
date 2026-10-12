//! 生成层：期望状态 -> **oxidns** 的那份 YAML（`docs/BLUEPRINT.md` §3：DNS 侧唯一的策略脑）。
//!
//! 与 [`dae`](super::dae) 同一套两条规矩：输出必须逐字节确定（面板的差异对比靠它），
//! 表达不了的组合**要留下一行看得见的字**，不许悄悄少写一条——因为"面板改了、解析没变"
//! 这种表现对用户是不可见的，只有生成预览里那句话能让它现形。
//!
//! 键名与必填性不来自文档而来自源码核实表
//! `docs/reference/oxidns-plugin-args-verified.md`（每处引用都带 `path:line`）。
//! 其中三条决定了这份输出的形状：
//! - 协议由 `addr` 的 URL scheme 决定，**没有** `protocol` 字段，也没有 `server_name`；
//! - `forward`/`cache`/`hosts`/`black_hole` 的 `short_circuit` 才是"拿到应答就收尾"的开关
//!   （源码里 `short_circuit` 为假时执行器返回 `Next`，链会继续往下走），所以上游那两条
//!   必须显式写 `true`——否则国内分支命中后还会被兜底那一支再问一遍境外；
//! - `prefer_ipv4` / `prefer_ipv6` 是**包住后半条链**的执行器（`with_next() == true`），
//!   所以它画在分支之前，不是之后。同一形状的 `query_recorder` 因此画在**整条链第一条**——
//!   记下来的应答与 `execution_path`（"这条查询命中了哪几步"）才是完整的。
//!
//! 第四条同样决定形状：**解析口只有 `listen` 这一道闸**（server 插件没有客户端 ACL），
//! 所以它绑 `network.lan_address`（接管 v6 的 DNS 时另加 `lan_address6`），空值与非法地址
//! 都是渲染期硬错误——每一条的理由写在 `dns_binds` 上。

use crate::model::config::{
    DesiredState, DnsStrategy, DomainAction, DomainPattern, FinalOutbound, Ipv6Mode, RouteMode,
    Upstream, UpstreamTransport, Via,
};
use crate::model::device::{Device, DeviceMode};
use std::fmt::Write as _;

/// 管理 API 的监听地址。**写死且只绑回环**：这一面能改配置、能读全部解析记录，
/// 而这份 YAML 将来会带着主路由的凭据——它不该出现在局域网面上。
///
/// `pub`：读的那一头（[`crate::svc::dns_api`]）必须与写的这一头是同一个地址。两边各写一遍，
/// 将来就是"面板读的那个口解析器没在听"，而这条错误没有任何一处能自己发现。
pub const API_LISTEN: &str = "127.0.0.1:8053";

/// 记录器插件的 tag，也是管理 API 那条路径里的 `<tag>`（`/api/plugins/<tag>/records`）。
/// 同上：写这一行的和读这一路的必须是同一个字面量。
pub const RECORD_TAG: &str = "record";

/// 内存缓存的条数。`cache` 不给落盘项：`dump_file` 是机器级路径，等下发层接手这一份时再给。
const CACHE_SIZE: u32 = 4096;

/// 查询记录的 SQLite 文件与日志文件，都**相对 oxidns 的工作目录**。
/// `-d <DIR>` 实测就是 `set_current_dir(<DIR>)`（`src/app.rs:79-84`），而 ZBox 起 oxidns 时
/// 给的是自己的状态目录，所以这两份落在 `<root>/` 下，不需要把绝对路径写进生成物。
const RECORD_DB: &str = "query-records.db";
/// 名字只在这里写一遍：面板那一屏（`GET /api/logs`）要按这个前缀去 glob 轮转后的
/// `oxidns.log.<日期>`，而 daily 轮转一次之后盘上**没有**叫 `oxidns.log` 的文件（实测）。
/// 那边再拼一次字面量，就是改这里的人会漏改的地方。
pub(crate) const LOG_FILE: &str = "oxidns.log";
/// 日志轮转留几份。状态分区不大，而 `rotation` 的缺省是 `Never`（`types.rs:785-788`），
/// 也就是"没人轮转就一直长到塞满"，所以这一格不是可选项。
const LOG_MAX_FILES: u32 = 3;

/// 链上那几个执行器的 tag。`pub(crate)` 的理由与 `API_LISTEN` 同理：
/// 梯级那一屏（[`crate::gen::ladder`]）要说"这一格落在解析层第几行"，判据必须与这里
/// 写出去的是同一个字面量，两边各写一遍迟早漂。
pub(crate) const EXEC_DOMESTIC: &str = "$domestic";
pub(crate) const EXEC_OVERSEAS: &str = "$overseas";
pub(crate) const EXEC_BLOCKED: &str = "$blocked";
/// 查询记录（面板「DNS 记录」那一屏的数据源）。
pub(crate) const EXEC_RECORD: &str = "$record";
/// `*_only` 那一档压掉另一族用的执行器与 matcher。
pub(crate) const EXEC_SINKHOLE: &str = "$sinkhole";
/// 内存缓存与地址族排序：梯级那一屏也要按链序数它们。
pub(crate) const EXEC_CACHE: &str = "$cache";
pub(crate) const EXEC_FAMILY: &str = "$family";
const MATCHER_SINK: &str = "qtype_sink";

/// 这一档要压掉哪一族。`prefer_*` 返回 `None`——它们是排序，不是抑制。
pub(crate) fn sink_qtype(strategy: &DnsStrategy) -> Option<&'static str> {
    match strategy {
        DnsStrategy::Ipv4Only => Some("AAAA"),
        DnsStrategy::Ipv6Only => Some("A"),
        DnsStrategy::PreferIpv4 | DnsStrategy::PreferIpv6 => None,
    }
}

/// 模型里那一档的名字，写给看配置的人看（与 `strategy` 的 serde 名一致）。
fn strategy_token(strategy: &DnsStrategy) -> &'static str {
    match strategy {
        DnsStrategy::PreferIpv4 => "prefer_ipv4",
        DnsStrategy::PreferIpv6 => "prefer_ipv6",
        DnsStrategy::Ipv4Only => "ipv4_only",
        DnsStrategy::Ipv6Only => "ipv6_only",
    }
}

/// 期望状态 -> 一份 YAML。返回的错误是给面板"生成报错"那一栏看的一句话，不是栈。
pub fn render(state: &DesiredState) -> Result<String, String> {
    let chain = Chain::plan(state)?;

    let mut out = String::new();
    out.push_str(
        "# oxidns 配置 —— 由 ZBox 从期望状态生成，请勿手改。\n\
         # 面板上每一格对应哪一行，见 docs/BLUEPRINT.md §5；键名的源码出处见 \
         docs/reference/oxidns-plugin-args-verified.md。\n\
         # 链自上而下第一个拿到应答的就收尾（每一支都显式 short_circuit），所以顺序是：\n\
         # 查询记录 -> 地址族抑制 -> 缓存 -> 地址族偏好 -> 设备层 -> 域名名单 -> 兜底 -> accept。\n\
         # 这里没有\"本机名\"那一层，理由见下面 use_system_local 那段注释。\n",
    );
    out.push_str("log:\n  level: info\n");
    out.push_str(
        "# 日志再落一份到 oxidns 的工作目录（= ZBox 的状态目录，`-d` 实测就是 set_current_dir）。源码里\n\
         # `file` 是 \"in addition to console\"（`types.rs:779`），所以这一份不顶替监督者吃的那份 stderr。\n\
         # 盘上的真名带日期后缀，而且按 **UTC** 切：2026-10-11 guest 实测，本机 05:59（+08:00）起的服务\n\
         # 写出的是 `oxidns.log.2026-10-01`，并没有一个叫 `oxidns.log` 的文件——所以读日志那一屏要按\n\
         # `oxidns.log.*` 取最新的那份，不能拼死一个名字。rotation 的缺省是 never，不写这一节等于没人\n\
         # 轮转，状态分区不大，那条路会走到塞满。\n",
    );
    let _ = write!(
        out,
        "  file: '{LOG_FILE}'\n  rotation:\n    type: daily\n    max_files: {LOG_MAX_FILES}\n"
    );
    writeln!(out, "api:\n  http: {}", ys(API_LISTEN, "api 监听地址")?).unwrap();
    out.push_str(
        "# 注：api 只绑回环。ZBox 与 oxidns 在同一台机器上，改配置与读解析记录都走 127.0.0.1；\n\
         # 这个口不给局域网，因为这一份里将带主路由凭据。\n",
    );
    render_route_mode(state, &mut out);
    render_unexpressible(state, &mut out);

    out.push_str("plugins:\n");
    render_forwards("domestic", &state.dns.domestic, &mut out)?;
    render_forwards("overseas", &state.dns.overseas, &mut out)?;
    render_infra(state, &chain, &mut out)?;
    render_matchers(state, &mut out)?;
    render_sequence(&chain, &mut out);
    render_servers(state, &mut out)?;
    Ok(out)
}

/// 链上引用了哪些执行器。插件按这份账画：没被引用的就不进链，免得留下"配了但走不到"的东西。
struct Chain {
    rules: Vec<String>,
    uses_domestic: bool,
    uses_overseas: bool,
    uses_blocked: bool,
    /// `*_only` 两档要压掉的那一族（`Some("AAAA")` = 只留 A）。
    /// 这是 IPv6 防泄漏在 DNS 侧的落点：客户端拿不到那一族的地址，就不会绕过代理直连过去。
    sink_qtype: Option<&'static str>,
}

impl Chain {
    fn plan(state: &DesiredState) -> Result<Self, String> {
        let sink = sink_qtype(&state.dns.strategy);
        // 只收口解析、不接管 v6 连接时，另一族的答案就是一条绕过代理的路：
        // 客户端拿到 AAAA 就照着它直连出去，而这台机器的 v6 连接层根本没 hook。
        // 这一条是硬错误而不是注记，因为它不是"没落"，是"落成了漏洞"。
        if state.network.ipv6_mode == Ipv6Mode::DnsOnly && sink.is_none() {
            return Err(
                "IPv6 = 只接管 DNS 时，dns.strategy 必须是 ipv4_only / ipv6_only：\
                 解析收口了但 v6 连接层没接管，留着另一族的答案就是留着一条绕过代理的路。\
                 要么改成双栈接管，要么把地址族策略改成 *_only。"
                    .to_string(),
            );
        }
        let mut chain = Self {
            rules: Vec::new(),
            uses_domestic: false,
            uses_overseas: false,
            uses_blocked: false,
            sink_qtype: sink,
        };
        if state.dns.records_enabled {
            // 记录器画在整条链**最外层**：它是 `with_next() == true` 的执行器
            // （`query_recorder/mod.rs:150-153`），放在第一条等于包住后面整条链——
            // 记下来的应答与命中步骤（`execution_path`，就是"这条查询命中了哪几步"）才是完整的。
            // 排在 `$sinkhole` 之前是有意的：被压掉的那一族也要留下记录，否则那一档看起来"零查询"。
            chain.rules.push(rule(None, EXEC_RECORD, None));
        }
        if let Some(qtype) = sink {
            // 画在除记录器以外的整条链最前面，连 `$cache` 都在它后面：缓存里若还留着上一次那一族的答案，
            // 也必须被压掉——否则"这一档"只在缓存未命中时生效，那是个会自己失效的开关。
            chain.rules.push(rule(
                Some(MATCHER_SINK),
                EXEC_SINKHOLE,
                Some(&format!(
                    "抑制 {qtype}（dns.strategy = {}）",
                    strategy_token(&state.dns.strategy)
                )),
            ));
        }
        chain.rules.push(rule(None, EXEC_CACHE, None));
        if let Some(exec) = family_exec(&state.dns.strategy) {
            chain.rules.push(rule(None, exec, None));
        }
        let mut drawn = 0usize;
        for device in &state.devices {
            let Some(exec) = device_exec(device.mode) else {
                continue;
            };
            chain.mark(exec);
            drawn += 1;
            chain.rules.push(rule(
                Some(&format!("dev{drawn}")),
                exec,
                Some(&device_label(device)),
            ));
        }
        for (index, domain_rule) in state.policy.domain_rules.iter().enumerate() {
            let exec = match domain_rule.action {
                DomainAction::Direct => EXEC_DOMESTIC,
                DomainAction::Proxy => EXEC_OVERSEAS,
                DomainAction::Block => EXEC_BLOCKED,
            };
            chain.mark(exec);
            chain.rules.push(rule(
                Some(&format!("rule{}", index + 1)),
                exec,
                Some(&domain_rule.pattern.raw()),
            ));
        }
        // 兜底那一支由 `policy.final_outbound` 决定——这是映射表里钉死的一格
        // （BLUEPRINT §5 的"兜底出口 -> oxidns：国外分支是否为默认"）。
        let (tail, why) = match &state.policy.final_outbound {
            FinalOutbound::Direct => (EXEC_DOMESTIC.to_string(), "兜底出口是直连".to_string()),
            FinalOutbound::Proxy(tag) => {
                (EXEC_OVERSEAS.to_string(), format!("兜底出口是策略组 {tag}"))
            }
        };
        chain.mark(&tail);
        chain.rules.push(rule(None, &tail, Some(&why)));
        // accept 收尾：链走到这里应答已经设好了，这一句把它定下来
        // （骨架见源码核实表 §6，最后一条是 `- exec: accept`）。
        chain.rules.push(rule(None, "accept", None));
        chain.check_available(state)?;
        Ok(chain)
    }

    fn mark(&mut self, exec: &str) {
        match exec {
            EXEC_DOMESTIC => self.uses_domestic = true,
            EXEC_OVERSEAS => self.uses_overseas = true,
            EXEC_BLOCKED => self.uses_blocked = true,
            _ => {}
        }
    }

    /// 设备档与域名名单可以指向一组空上游：那一条规则会引用链上不存在的执行器，
    /// oxidns 要到装载时才报错，所以在这里拦住。兜底那一支同理。
    fn check_available(&self, state: &DesiredState) -> Result<(), String> {
        for (used, group, name) in [
            (
                self.uses_domestic,
                state.dns.domestic.is_empty(),
                "dns.domestic（国内上游）",
            ),
            (
                self.uses_overseas,
                state.dns.overseas.is_empty(),
                "dns.overseas（境外上游）",
            ),
        ] {
            if used && group {
                return Err(format!(
                    "链上有分支要转 {name}，但那一组一条上游都没填——这些解析没有可去的地方：\
                     {list}",
                    list = chain_reason(state)
                ));
            }
        }
        Ok(())
    }
}

fn chain_reason(state: &DesiredState) -> String {
    match &state.policy.final_outbound {
        FinalOutbound::Direct => "（兜底出口=直连，所以至少要有国内上游）".to_string(),
        FinalOutbound::Proxy(tag) => format!("（兜底出口=策略组 {tag}，所以至少要有境外上游）"),
    }
}

/// 一条 sequence 规则。`matches` 给的是 matcher 的 tag（引用形式是 `$tag`），
/// 不给就是恒真（源码 sequence/mod.rs:109：没有 matches 字段 = 无条件执行）。
/// `note` 是行尾注释：这一行是谁画的，只在看配置的人需要知道时才写。
fn rule(matches: Option<&str>, exec: &str, note: Option<&str>) -> String {
    let mut line = match matches {
        Some(tag) => format!("      - matches: ${tag}\n        exec: {exec}"),
        None => format!("      - exec: {exec}"),
    };
    if let Some(note) = note {
        let _ = write!(line, " # {}", note.replace('\n', " "));
    }
    line
}

/// 一台设备 -> 至多一条分支。`global` 交给兜底，`bypass` 的流量根本不来这台机器（§4 那张表）。
fn device_exec(mode: DeviceMode) -> Option<&'static str> {
    match mode {
        DeviceMode::Global | DeviceMode::Bypass => None,
        DeviceMode::Direct => Some(EXEC_DOMESTIC),
        DeviceMode::Proxy => Some(EXEC_OVERSEAS),
    }
}

/// 地址族偏好执行器只服务 `prefer_*` 两档（排序）。`*_only` 走链首的
/// `qtype $sinkhole` 那条抑制规则（见 [`sink_qtype`]），不经过这里。
///
/// `pub(crate)`：梯级那一屏要按链序数它，理由同上。
pub(crate) fn family_exec(strategy: &DnsStrategy) -> Option<&'static str> {
    match strategy {
        DnsStrategy::PreferIpv4 | DnsStrategy::PreferIpv6 => Some(EXEC_FAMILY),
        DnsStrategy::Ipv4Only | DnsStrategy::Ipv6Only => None,
    }
}

fn family_type(strategy: &DnsStrategy) -> Option<&'static str> {
    match strategy {
        DnsStrategy::PreferIpv4 => Some("prefer_ipv4"),
        DnsStrategy::PreferIpv6 => Some("prefer_ipv6"),
        DnsStrategy::Ipv4Only | DnsStrategy::Ipv6Only => None,
    }
}

fn device_label(device: &Device) -> String {
    match &device.hostname {
        Some(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => device.ip.clone(),
    }
}

/// 路线那一格决定 oxidns 要不要往主路由写动态条目。这一期只说清哪一行是它的落点，
/// 因为 `ros_address_list` / `ros_route` 的凭据不在期望状态里（P2c 才接）。
fn render_route_mode(state: &DesiredState, out: &mut String) {
    out.push_str(match state.network.route_mode {
        RouteMode::Inject =>
            "# 路线 = inject（R2）：该走代理的解析结果要作为带 TTL 的动态条目写回主路由，\n\
             # 落点是两个执行器 ros_address_list / ros_route（键已核实，源码核实表 §2 那两张表）。\n\
             # 它们要 address=<主路由>:8728 / username / password / routing_table|address_list4，\n\
             # 这四条是凭据不是选项，期望状态里还没有格子，所以这一份里**暂时没有这两块**：\n\
             # 于是这份能装载、能解析，但注入不发生——R2 的下半边还没接上（BLUEPRINT §10 P2c）。\n",
        RouteMode::Takeover =>
            "# 路线 = takeover（R1）：设备的网关整个交给旁路由，oxidns 不往主路由写任何条目，\n\
             # 所以这份里没有 ros_address_list / ros_route 这两块，这是设计不是漏。\n",
    });
}

/// 模型里有、这一份里落不成行的那几格，逐条说明为什么。注释就是它们的可见形态。
fn render_unexpressible(state: &DesiredState, out: &mut String) {
    if state.policy.cn_direct {
        out.push_str(
            "# policy.cn_direct：这一档要一份 CN 域名/IP 集合。oxidns 的 geosite/geoip provider\n\
             # 读的是本地 .dat 而且从不下载（源码核实表 §4，这是生死门 G10），ZBox 现在产的是 sing-box\n\
             # 的 .srs——两边对不上。所以这里没有 cn 分支；集合落地后补的是 $cn_domain（qname + geosite）\n\
             # 与 $cn_ip（client_ip + geoip）两条 matcher 和它们在链上的位置。\n",
        );
    }
    if state.policy.block_ads {
        out.push_str(
            "# policy.block_ads：广告拦截本来就该落在这份里（域名集合归 DNS 侧），但它的来源要的是\n\
             # adguard_rule provider 的 rules/files（文本规则），ZBox 现在只有 .srs（G10）。\n\
             # 所以这里没有广告分支；用户自己写的 block 档域名规则不受影响，它们在下面的链里。\n",
        );
    }
    if state.policy.private_direct {
        out.push_str(
            "# policy.private_direct 是连接层的档，落在 dae 的 dip(geoip:private) 那一行；\n\
             # DNS 侧不需要行（私网名段的解析跟着兜底走）。\n",
        );
    }
    if !state.rule_sets.is_empty() {
        out.push_str(
            "# rule_sets[] 一条都不在这里展开：.srs 是 sing-box 的编译产物格式，oxidns 的 provider\n\
             # 只吃 .dat（geosite/geoip）、逐行文本（domain_set/hosts/adguard_rule）与\n\
             # IP/CIDR 列表（ip_set）。这是 G10，不是省略。\n",
        );
    }
    if state.dns.fakeip.enabled {
        out.push_str(
            "# dns.fakeip 不参与生成（BLUEPRINT §3 第 4 条）：dae 的域名归属靠截获 53 端口的真实应答建立，\n\
             # fakeip 把真实地址换成地址池正好打断它；oxidns 侧也没有 fakeip 执行器。这一格属于 sing-box 老路线。\n",
        );
    }
    if state.dns.use_system_local {
        out.push_str(
            "# dns.use_system_local 在这份里没有落点：oxidns 的 hosts 插件吃的是 `<域名> <IP>` 顺序\n\
             # （hosts.rs:307-322），把它指到 /etc/hosts 会在插件初始化就报\n\
             # `invalid hosts IP 'localhost.localdomain'`、整个进程起不来（2026-10-09 guest 实测：\n\
             # Alpine 的 /etc/hosts 首行是 `127.0.0.1<TAB>localhost.localdomain …`）。\n\
             # oxidns 也没有\"转给系统解析器\"的执行器，forward 只吃显式 upstreams。\n\
             # 这一格是空的：面板与预览要按\"没有落点\"显示，不许显示成已生效。\n",
        );
    }
}

/// 一个上游组 -> 一个 `forward` 执行器。协议全在 `addr` 的 scheme 里（源码核实表 §2）。
fn render_forwards(tag: &str, upstreams: &[Upstream], out: &mut String) -> Result<(), String> {
    if upstreams.is_empty() {
        return Ok(());
    }
    writeln!(out, "  - tag: {tag}").unwrap();
    out.push_str("    type: forward\n    args:\n      upstreams:\n");
    for upstream in upstreams {
        // DoH 的查询路径在 `addr` 这个 URL 里，`forward.upstreams[]` 没有单独的 path 键
        // （源码核实表 §2 那张表：键只有 addr/tag/outbound/dial_addr/port/bootstrap/…）。
        writeln!(
            out,
            "        - addr: {}",
            ys(
                &upstream_addr(upstream)?,
                &format!("上游 {} 的地址", upstream.tag)
            )?
        )
        .unwrap();
        for note in upstream_notes(upstream) {
            for line in note.lines() {
                writeln!(out, "        # {line}").unwrap();
            }
        }
    }
    // 命中即收尾：short_circuit 为假时源码里 forward 返回 Next，链会继续走到兜底那一支
    // （国内分支命中后又被境外问一遍），所以这一条必须显式写。
    out.push_str("      short_circuit: true\n");
    Ok(())
}

fn upstream_addr(upstream: &Upstream) -> Result<String, String> {
    let host = upstream.host.trim();
    if host.is_empty() {
        return Err(format!("DNS 上游 {} 缺少地址", upstream.tag));
    }
    let scheme = match upstream.transport {
        UpstreamTransport::Udp => "udp",
        UpstreamTransport::Tcp => "tcp",
        UpstreamTransport::Dot => "tls",
        UpstreamTransport::DoH => "https",
        UpstreamTransport::DoH3 => "h3",
    };
    let address = if host.contains(':') && !host.starts_with('[') {
        // IPv6 字面量在 URL 里必须带方括号，否则冒号会被当成端口分隔符
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let mut addr = format!("{scheme}://{address}");
    if let Some(port) = upstream.port {
        let _ = write!(addr, ":{port}");
    }
    if let Some(path) = &upstream.path {
        let _ = write!(addr, "{path}");
    }
    Ok(addr)
}

/// 模型里有、`forward.upstreams[]` 里没有对应键的那几格，逐条写成行内注释。
/// 它们不是"可选美化"：`sni` 与 `via` 都是用户在面板上填过的字，静丢掉就是"看着设了其实没生效"。
fn upstream_notes(upstream: &Upstream) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(sni) = &upstream.sni {
        if !sni.trim().is_empty() {
            notes.push(format!(
                "sni={sni} 在这份里没有落点：oxidns 的 forward.upstreams 没有 server_name 键，\
                 SNI 由 addr 里的 host 派生（源码 config.rs:523）"
            ));
        }
    }
    if let Via::Outbound(tag) = &upstream.via {
        notes.push(format!(
            "经由 = 出站 {tag}：这一格的落点是 network.outbound.profiles，而那条 profile 的字段\n形状还没核实（G6）。\
             现阶段的\"由 dae 带出去\"靠的是 dae 对 53 端口的拦截\n（BLUEPRINT §3 第 3 条，G3），所以这里不写 outbound 键"
        ));
    }
    if !upstream.address_is_ip() && upstream.transport != UpstreamTransport::Udp {
        notes.push(
            "addr 的 host 是域名：加密类上游要先把它解析出来，oxidns 的 bootstrap 键要的是 \
             IP:port，期望状态里还没有这一格"
                .to_string(),
        );
    }
    notes
}

/// 链上要用到的那几个基础执行器。
fn render_infra(state: &DesiredState, chain: &Chain, out: &mut String) -> Result<(), String> {
    if state.dns.records_enabled {
        let days = state.dns.records_retention_days;
        // 0 既不是"永久"也不是"不留"：oxidns 的校验要求 retention_days ≥ 1，
        // 带着 0 装载是起不来。"不留"在模型里另有那一格（records_enabled）。
        if days == 0 {
            return Err(
                "dns.records_retention_days 是 0，而 oxidns 要求 retention_days ≥ 1。0 不是\"永久保留\
                 \n也不是\"不保留\"，它会让解析器起不来；不留记录请把 dns.records_enabled 关掉。"
                    .to_string(),
            );
        }
        let _ = write!(
            out,
            "  # 查询记录：面板「DNS 记录」那一屏的数据源。SQLite 落在 oxidns 的工作目录（= ZBox 的\n\
             \x20 # 状态目录）里，但 ZBox 不直读那一份——读的是 {API_LISTEN} 上的\n\
             \x20 # /api/plugins/{RECORD_TAG}/records（另有 /records/<id>、/stats/*、/stream）。\n\
             \x20 - tag: {RECORD_TAG}\n    type: query_recorder\n    args:\n\
             \x20     path: '{RECORD_DB}'\n      retention_days: {days}\n"
        );
    } else {
        out.push_str(
            "  # dns.records_enabled = false：这一份里没有 query_recorder，管理 API 照开，\n\
             # 但面板「DNS 记录」那一屏永远是空的——空是这一格造成的，不是采集坏了。\n",
        );
    }
    writeln!(out, "  - tag: cache").unwrap();
    let _ = write!(
        out,
        "    type: cache\n    args:\n      size: {CACHE_SIZE}\n      short_circuit: true\n"
    );
    // 这里没有 `hosts` 执行器：`dns.use_system_local` 在 oxidns 没有落点，
    // 原因和实测证据写在 render_unexpressible 里那条注释上。
    if let Some(kind) = family_type(&state.dns.strategy) {
        let _ = write!(
            out,
            "  - tag: family\n    type: {kind}\n\
             \x20   # family 画在分支之前：prefer_* 是包住后半条链的执行器（with_next() == true），\n\
             \x20   # 它等后面的 forward 拿到应答之后再按地址族挑。\n"
        );
    }
    if chain.uses_blocked {
        out.push_str(
            "  - tag: blocked\n    type: black_hole\n    args:\n      mode: nxdomain\n      short_circuit: true\n",
        );
    }
    if let Some(qtype) = chain.sink_qtype {
        // 与 `$blocked` 分开两份：广告拦截要的是"这域名不存在"（nxdomain），
        // 地址族这一档要的是 NOERROR + 空应答（nodata）——把 AAAA 报成 NXDOMAIN
        // 会让客户端以为站点没有 IPv6 记录之外的任何问题，某些实现的负面缓存与
        // 回退行为因此不一样。两者语义不同，不能共用一个执行器。
        out.push_str(
            "  - tag: sinkhole\n    type: black_hole\n    args:\n      mode: nodata\n      short_circuit: true\n\
             \x20   # nodata = NOERROR 但不带应答，与 dae 的 ipversion_prefer 同语义\n",
        );
        let _ = write!(
            out,
            "  - tag: {MATCHER_SINK}\n    type: qtype\n    args:\n      - {qtype}\n"
        );
    }
    Ok(())
}

/// 域名名单与设备档各要一个 matcher。写法照源码核实表 §3：qname 用 `full:` / `domain:` /
/// `keyword:` / `regexp:` 四个前缀，client_ip 用 IP/CIDR。
fn render_matchers(state: &DesiredState, out: &mut String) -> Result<(), String> {
    let mut drawn = 0usize;
    for device in &state.devices {
        if device_exec(device.mode).is_none() {
            continue;
        }
        drawn += 1;
        let prefix = ys(
            &device_prefix(&device.ip, &device_label(device))?,
            &format!("设备 {}", device_label(device)),
        )?;
        let _ = write!(
            out,
            "  - tag: dev{drawn}\n    type: client_ip\n    args:\n      - {prefix}\n",
        );
    }
    for (index, domain_rule) in state.policy.domain_rules.iter().enumerate() {
        let expression = ys(
            &qname_expression(&domain_rule.pattern),
            &format!("域名规则 {}", domain_rule.pattern.raw()),
        )?;
        let _ = write!(
            out,
            "  - tag: rule{}\n    type: qname\n    args:\n      - {expression}\n",
            index + 1
        );
    }
    Ok(())
}

/// 一个设备的匹配前缀。`ip` 里已经带 `/段` 就照原样用，否则补成主机路由——
/// `client_ip` 接受裸 IP（sources.rs:66），但写出前缀长度更好读，也让人分得出
/// "这是一台设备"和"这是一个段"。
fn device_prefix(raw: &str, label: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.contains('/') {
        return Ok(trimmed.to_string());
    }
    match trimmed.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => Ok(format!("{trimmed}/32")),
        Ok(std::net::IpAddr::V6(_)) => Ok(format!("{trimmed}/128")),
        Err(_) => Err(format!(
            "设备 {label} 的 ip 字段 \"{raw}\" 既不是 IP 也不是 CIDR，oxidns 的 client_ip 匹配器读不了"
        )),
    }
}

/// 面板那四种写法 -> qname 的四个前缀。裸写法在 oxidns 里默认就是后缀匹配，
/// 但这里带上 `domain:` 是为了让读配置的人一眼看见这一条是哪一档。
fn qname_expression(pattern: &DomainPattern) -> String {
    match pattern {
        DomainPattern::Exact(value) => format!("full:{value}"),
        DomainPattern::Suffix(value) => format!("domain:{value}"),
        DomainPattern::Keyword(value) => format!("keyword:{value}"),
        DomainPattern::Regex(value) => format!("regexp:{value}"),
    }
}

fn render_sequence(chain: &Chain, out: &mut String) {
    out.push_str("  - tag: main\n    type: sequence\n    args:\n");
    for rule in &chain.rules {
        out.push_str(rule);
        out.push('\n');
    }
}

/// 监听。`ports.dns` 是局域网设备被指过来的那一个，也是 dae 交回本机的那一个。
///
/// 绑的地址来自 `network.lan_address` / `lan_address6`，不是通配地址：oxidns 的
/// server 插件没有 `allowed_networks` 这一类客户端 ACL（源码核实表 §1 第 5 条），
/// `listen` 是唯一的闸——写成 `0.0.0.0` 就是一个开在上游那一侧的开放解析器，
/// 能被拿去做放大反射攻击。接管 v6 的 DNS 时也不能图省事开 `[::]`：实测 oxidns 1.6.1
/// 不设 `IPV6_V6ONLY`（docs/reference/ipv6-kernel-measured.md §2），那只双栈 socket
/// 连 v4 一起收走，"只绑局域网地址"这道闸当场作废。
/// 这四条（v4 空、v4 非法、v6 空、v6 非法）都是**渲染期硬错误**，不是注记：
/// oxidns 的 `check` 门不验 `listen` 的地址合法性（源码核实表 §"check 门的覆盖面"），
/// 松一份就得到"配置装得上、端口起不来"这种要人到现场才发现的形状。
fn dns_binds(state: &DesiredState) -> Result<(String, Option<String>), String> {
    let port = state.ports.dns;
    let v4 = state.network.lan_address.trim();
    if v4.is_empty() {
        return Err(format!(
            "DNS 解析口要绑本机在局域网上的 IPv4，但 network.lan_address 是空的。\
             绑 0.0.0.0 等于开一个开放解析器（server 插件没有客户端 ACL，唯一的闸就是这个地址）。\
             跑一次 provision 让它从现场探出来，或在面板『网卡与网段』里填上。"
        ));
    }
    let Ok(v4) = v4.parse::<std::net::Ipv4Addr>() else {
        return Err(format!(
            "network.lan_address 不是一个 IPv4 地址：`{v4}`。它直接进 listen，\
             而 oxidns 的 check 门不验这个字段（写错了要到启动绑定那一刻才炸）。"
        ));
    };
    let binds = (format!("{v4}:{port}"), None);
    if !state.network.ipv6_mode.takes_dns() {
        return Ok(binds);
    }
    let v6 = state.network.lan_address6.trim();
    if v6.is_empty() {
        return Err(
            "这一档要把 IPv6 的 DNS 查询收进来，但 network.lan_address6 是空的。\
             开 [::] 不算答复：实测 oxidns 不设 IPV6_V6ONLY，那只双栈 socket 会连 IPv4 \
             一起收走，上面那道『只绑局域网地址』的闸当场作废\
             （docs/reference/ipv6-kernel-measured.md §2）。\
             填本机在局域网上的固定 IPv6——SLAAC 的动态地址会到期消失，别填那一个。"
                .to_string(),
        );
    }
    let Ok(v6) = v6.parse::<std::net::Ipv6Addr>() else {
        return Err(format!(
            "network.lan_address6 不是一个 IPv6 地址：`{v6}`（只写地址本身，不要带前缀长度，\
             也不要带 %网卡名）"
        ));
    };
    Ok((binds.0, Some(format!("[{v6}]:{port}"))))
}

fn render_servers(state: &DesiredState, out: &mut String) -> Result<(), String> {
    let (v4, v6) = dns_binds(state)?;
    let _ = write!(
        out,
        "  - tag: udp_in\n    type: udp_server\n    args:\n      entry: main\n      listen: '{v4}'\n\
         \x20 - tag: tcp_in\n    type: tcp_server\n    args:\n      entry: main\n      listen: '{v4}'\n",
    );
    if let Some(v6) = &v6 {
        let _ = write!(
            out,
            "  - tag: udp_in6\n    type: udp_server\n    args:\n      entry: main\n      listen: '{v6}'\n\
             \x20 - tag: tcp_in6\n    type: tcp_server\n    args:\n      entry: main\n      listen: '{v6}'\n"
        );
        out.push_str(
            "  # v4、v6 各一对，两边都是显式地址。v6 那一侧不写成 [::]：实测 oxidns 1.6.1 不设\n\
             # IPV6_V6ONLY，那只双栈 socket 会连 v4 一起收走，\"解析口只绑局域网地址\"这道闸\n\
             # 当场作废（docs/reference/ipv6-kernel-measured.md §2）。\n",
        );
    }
    out.push_str(
        "# 解析口绑的是 network.lan_address，接管 IPv6 的 DNS 时另绑 lan_address6。\n\
         # 这两个地址换了（DHCP 重新分配、换网卡）而配置没跟上，oxidns 会起不来——不是静默降级：\n\
         # doctor 的 inbound_not_listening 报这一条，处置命令就是那份 worker stderr。\n",
    );
    Ok(())
}

/// YAML 的单引号标量：里面唯一的转义是单引号翻倍。控制字符没有写法，直接报错而不是产出
/// 一份装载不了的 YAML——换行落在单引号标量里会被折成空格，那就是静默改值。
fn ys(value: &str, what: &str) -> Result<String, String> {
    if value.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
        return Err(format!(
            "{what} 里有换行或空字符，YAML 里没有安全写法：`{value}`"
        ));
    }
    Ok(format!("'{}'", value.replace('\'', "''")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::{DomainRule, Group, GroupKind, RuleSet};

    /// 解析口从这一步起要求显式绑定地址（`dns_binds`），所以夹具一律带上"本机在局域网上的
    /// 两个地址"。测试里用的是 RFC 保留段，不指向任何真实机器。
    fn state() -> DesiredState {
        let mut s = DesiredState::baseline();
        s.network.lan_address = "192.0.2.10".into();
        s.network.lan_address6 = "2001:db8::10".into();
        s
    }

    fn overseas(s: &mut DesiredState) {
        s.dns.overseas = crate::model::config::Dns::default().overseas;
    }

    fn upstream(
        host: &str,
        transport: UpstreamTransport,
        port: Option<u16>,
        path: Option<&str>,
        via: Via,
    ) -> Upstream {
        Upstream {
            tag: "u".into(),
            host: host.into(),
            port,
            transport,
            path: path.map(str::to_string),
            sni: None,
            via,
        }
    }

    fn device(ip: &str, hostname: Option<&str>, mode: DeviceMode) -> Device {
        Device {
            ip: ip.into(),
            mac: None,
            hostname: hostname.map(str::to_string),
            mode,
        }
    }

    /// 链上第一个拿到应答的要收尾，否则国内分支命中后还会被兜底那一支再问一遍境外。
    /// 这一条断的是**每个** forward 都带 short_circuit，不是"文案里有这个词"。
    #[test]
    fn every_forward_branch_short_circuits() {
        let text = render(&state()).unwrap();
        let forwards = text.matches("type: forward").count();
        let short_circuits = text
            .split("type: forward")
            .skip(1)
            .filter(|block| block.contains("      short_circuit: true"))
            .count();
        assert_eq!(forwards, short_circuits, "有上游组没被显式收尾：\n{text}");
        assert!(forwards >= 1);
    }

    #[test]
    fn the_chain_ends_with_accept_and_always_has_a_default_branch() {
        let text = render(&state()).unwrap();
        for row in [
            "      - exec: $cache\n",
            "      - exec: $family\n",
            "      - exec: $domestic",
            "      - exec: accept\n",
        ] {
            assert!(text.contains(row), "少了这一行：{row}\n{text}");
        }
        // 基线没有境外上游，所以链上也不许出现 $overseas（引用一个链上没有的执行器是装载期错误）
        assert!(!text.contains("$overseas"), "{text}");
    }

    /// "用系统解析器兜底"这一格在 oxidns 里**只能写成一条说明**。原来写的是
    /// `hosts` 插件 + `files: ['/etc/hosts']`，2026-10-09 在 guest 上实测是启动致命项：
    /// oxidns 的 hosts 规则是 `<域名> <IP>` 顺序（hosts.rs:307-322），而 `/etc/hosts` 反过来，
    /// 于是 Alpine 首行 `127.0.0.1\tlocalhost.localdomain …` 被判成
    /// `invalid hosts IP 'localhost.localdomain'`，插件初始化失败、进程整个不起来。
    #[test]
    fn the_system_resolver_option_is_written_as_a_note_not_as_a_broken_plugin() {
        let mut s = state();
        s.dns.use_system_local = true;
        let text = render(&s).unwrap();
        assert!(
            text.contains("# dns.use_system_local 在这份里没有落点"),
            "{text}"
        );
        // 生成物里一条 hosts 执行器都不许有——链上引用它、或声明它喂 /etc/hosts，都会炸
        assert!(!text.contains("type: hosts"), "{text}");
        assert!(!text.contains("- exec: $hosts"), "{text}");
        assert!(!text.contains("files:\n        - '/etc/hosts'"), "{text}");

        s.dns.use_system_local = false;
        let off = render(&s).unwrap();
        assert!(
            !off.contains("# dns.use_system_local 在这份里没有落点"),
            "关掉那一格还要写这条说明，预览就会永远多一行噪声：{off}"
        );
    }

    #[test]
    fn the_same_state_renders_the_same_bytes_twice() {
        let a = render(&state()).unwrap();
        let b = render(&state()).unwrap();
        assert_eq!(a, b, "预览的差异对比靠确定性");
        assert!(a.ends_with('\n'));
    }

    /// 协议在 `addr` 的 scheme 里，端口不带就是协议默认（DoT 的 853、DoH 的 443 不许写进去，
    /// 那是"把默认值当事实抄了一遍"，以后上游改默认就看不出来了）。
    #[test]
    fn the_protocol_is_in_the_addr_not_in_a_key() {
        let cases = [
            (UpstreamTransport::Udp, "udp://8.8.8.8"),
            (UpstreamTransport::Tcp, "tcp://8.8.8.8"),
            (UpstreamTransport::Dot, "tls://8.8.8.8"),
            (UpstreamTransport::DoH, "https://8.8.8.8"),
            (UpstreamTransport::DoH3, "h3://8.8.8.8"),
        ];
        for (transport, expected) in cases {
            let mut s = state();
            s.dns.domestic = vec![upstream(
                "8.8.8.8",
                transport.clone(),
                None,
                None,
                Via::Direct,
            )];
            let text = render(&s).unwrap();
            assert!(
                text.contains(&format!("- addr: '{expected}'")),
                "{expected} 不对：{text}"
            );
        }
        let mut s = state();
        s.dns.domestic = vec![
            upstream(
                "2001:4860:4860::8888",
                UpstreamTransport::Udp,
                Some(5353),
                None,
                Via::Direct,
            ),
            upstream(
                "dns.google",
                UpstreamTransport::DoH,
                None,
                Some("/dns-query"),
                Via::Direct,
            ),
        ];
        let text = render(&s).unwrap();
        assert!(
            text.contains("- addr: 'udp://[2001:4860:4860::8888]:5353'"),
            "{text}"
        );
        assert!(
            text.contains("- addr: 'https://dns.google/dns-query'"),
            "{text}"
        );
        // 这两类上游的 host 是域名：要先解析它，期望状态里还没有 bootstrap 那一格，所以要说
        assert!(text.contains("# addr 的 host 是域名"), "{text}");
        assert!(!text.contains("protocol:"), "oxidns 没有这个键：{text}");
        assert!(
            !text.contains("\n          path:"),
            "路径在 URL 里，不是单独的键：{text}"
        );
    }

    /// BLUEPRINT §5 的那一行：兜底出口 -> oxidns 的"国外分支是否为默认"。
    #[test]
    fn the_default_branch_follows_the_final_outbound() {
        let direct = render(&state()).unwrap();
        assert!(
            direct.contains("      - exec: $domestic # 兜底出口是直连"),
            "{direct}"
        );

        let mut s = state();
        overseas(&mut s);
        s.groups.push(Group {
            tag: "auto".into(),
            kind: GroupKind::Selector,
            members: vec!["n1".into()],
            default: None,
        });
        s.policy.final_outbound = FinalOutbound::Proxy("auto".into());
        let text = render(&s).unwrap();
        assert!(
            text.contains("      - exec: $overseas # 兜底出口是策略组 auto"),
            "{text}"
        );
    }

    #[test]
    fn a_branch_pointing_at_an_empty_group_is_refused() {
        // 兜底要境外，但境外一条都没有
        let mut s = state();
        s.groups.push(Group {
            tag: "auto".into(),
            kind: GroupKind::Selector,
            members: vec!["n1".into()],
            default: None,
        });
        s.policy.final_outbound = FinalOutbound::Proxy("auto".into());
        let err = render(&s).unwrap_err();
        assert!(err.contains("dns.overseas"), "{err}");
        assert!(err.contains("兜底出口=策略组 auto"), "{err}");

        // 设备档指过去时也一样：链上不能引用没画出来的执行器
        let mut s = state();
        s.devices
            .push(device("192.168.1.60", None, DeviceMode::Proxy));
        assert!(render(&s).unwrap_err().contains("dns.overseas"));

        let mut s = state();
        s.dns.domestic.clear();
        assert!(render(&s).unwrap_err().contains("dns.domestic"));
    }

    #[test]
    fn device_rows_come_before_the_domain_list() {
        let mut s = state();
        overseas(&mut s);
        s.devices = vec![
            device("192.168.1.50", Some("客厅电视"), DeviceMode::Direct),
            device("192.168.1.61", None, DeviceMode::Global),
            device("192.168.1.62", None, DeviceMode::Bypass),
            device("192.168.1.63", None, DeviceMode::Proxy),
        ];
        s.policy.domain_rules = vec![DomainRule::new(
            DomainPattern::parse("a.test").unwrap(),
            DomainAction::Direct,
        )];
        let text = render(&s).unwrap();
        let first_device = text.find("matches: $dev1").unwrap();
        let second_device = text.find("matches: $dev2").unwrap();
        let domain = text.find("matches: $rule1").unwrap();
        let tail = text.find("exec: $domestic # 兜底").unwrap();
        assert!(first_device < second_device, "设备层内部也要按声明顺序");
        assert!(second_device < domain, "设备层排在全局层之前（§4）");
        assert!(domain < tail, "名单排在兜底之前");
        // 两档不生成行：global 交给兜底，bypass 的流量不来这台机器
        assert_eq!(text.matches("type: client_ip").count(), 2, "{text}");
        assert!(text.contains("# 客厅电视"), "{text}");
        assert!(text.contains("192.168.1.63/32"), "没有主机名就用 IP 当名字");
    }

    #[test]
    fn client_ip_takes_a_host_prefix_and_keeps_a_given_cidr() {
        let mut s = state();
        s.devices = vec![
            device("192.168.1.50", None, DeviceMode::Direct),
            device("fd00::7", None, DeviceMode::Direct),
            device("10.30.0.0/16", None, DeviceMode::Direct),
        ];
        let text = render(&s).unwrap();
        assert!(text.contains("      - '192.168.1.50/32'"), "{text}");
        assert!(text.contains("      - 'fd00::7/128'"), "{text}");
        assert!(
            text.contains("      - '10.30.0.0/16'"),
            "给了段就别改它：{text}"
        );

        let mut s = state();
        s.devices.push(device("00:11:22", None, DeviceMode::Direct));
        assert!(render(&s).unwrap_err().contains("既不是 IP 也不是 CIDR"));
    }

    /// 面板的四种写法 -> qname 的四个前缀（源码核实表 §3）。`domain:` 在 oxidns 是默认档，
    /// 这里显式写出来是为了让读配置的人分得出"这是后缀"而不是"有人漏写了 full:"。
    #[test]
    fn the_four_domain_writings_map_to_four_prefixes() {
        let mut s = state();
        s.policy.domain_rules = ["+example.com", "exact.com", "~porn", "/^x[0-9]+\\.com$/"]
            .iter()
            .map(|raw| DomainRule::new(DomainPattern::parse(raw).unwrap(), DomainAction::Block))
            .collect();
        let text = render(&s).unwrap();
        for row in [
            "      - 'domain:example.com'",
            "      - 'full:exact.com'",
            "      - 'keyword:porn'",
            "      - 'regexp:^x[0-9]+\\.com$'",
        ] {
            assert!(text.contains(row), "少了这一行：{row}\n{text}");
        }
        // block 档才画 black_hole，而且要它在链上
        assert!(text.contains("    type: black_hole"), "{text}");
        assert!(text.contains("      mode: nxdomain"), "{text}");
        assert!(text.contains("exec: $blocked"), "{text}");
    }

    /// 模型里有、这份里落不成行的那几格，必须以一行字的形态看得见。
    /// 这一条是这一层的诚实性所在：少了这些行，用户看到的就是"面板改了、解析没变"。
    #[test]
    fn what_cannot_be_said_is_written_as_a_line() {
        let mut s = state();
        s.policy.cn_direct = true;
        s.policy.block_ads = true;
        s.dns.fakeip.enabled = true;
        s.dns.strategy = DnsStrategy::Ipv4Only;
        s.rule_sets.push(RuleSet::Local {
            tag: "mine".into(),
            path: "rule-set/mine.srs".into(),
        });
        s.dns.overseas = vec![Upstream {
            tag: "o".into(),
            host: "dns.example".into(),
            port: None,
            transport: UpstreamTransport::Dot,
            path: None,
            sni: Some("other.example".into()),
            via: Via::Outbound("proxy".into()),
        }];
        s.dns.records_enabled = false;
        let text = render(&s).unwrap();
        for row in [
            "# policy.cn_direct",
            "# policy.block_ads",
            "# policy.private_direct",
            "# rule_sets[] 一条都不在这里展开",
            "# dns.fakeip 不参与生成",
            "# sni=other.example 在这份里没有落点",
            "# 经由 = 出站 proxy：",
            "# dns.records_enabled = false",
        ] {
            assert!(text.contains(row), "少了这一行：{row}\n{text}");
        }
        // 说了不落的就不许偷偷落：这些键名只出现在注释里，不能作为真键出现
        assert!(!text.contains("type: geosite"), "{text}");
        assert!(!text.contains("type: geoip"), "{text}");
        assert!(!text.contains("type: adguard_rule"), "{text}");
        assert!(!text.contains("\n      outbound:"), "{text}");
        assert!(!text.contains("\n        - server_name"), "{text}");
    }

    /// `*_only` 两档在 oxidns 的落点：紧跟在 `$record` 之后的一条 `qtype` -> `black_hole(nodata)`。
    /// 位置是要断言的项而不是备注——抑制若排在 `$cache` 之后，缓存里存着的另一族记录
    /// 就压不掉，这一档会变成"只在缓存未命中时生效"的开关。
    #[test]
    fn an_only_strategy_suppresses_that_family_before_anything_else() {
        for (strategy, suppressed, kept) in [
            (DnsStrategy::Ipv4Only, "AAAA", "A"),
            (DnsStrategy::Ipv6Only, "A", "AAAA"),
        ] {
            let mut s = state();
            s.dns.strategy = strategy.clone();
            let text = render(&s).unwrap();
            assert!(
                text.contains(&format!(
                    "    type: qtype\n    args:\n      - {suppressed}\n"
                )),
                "{strategy:?} 要按 token 压掉 {suppressed}：\n{text}"
            );
            assert!(
                !text.contains(&format!("      - {kept}\n")),
                "{strategy:?} 不许把留着的那一族 {kept} 也压掉：\n{text}"
            );
            assert!(
                text.contains(
                    "  - tag: sinkhole\n    type: black_hole\n    args:\n      mode: nodata\n"
                ),
                "{strategy:?}：\n{text}"
            );

            let sink = text
                .find("      - matches: $qtype_sink\n        exec: $sinkhole")
                .unwrap_or_else(|| panic!("{strategy:?} 链首少了抑制那条：\n{text}"));
            for later in ["      - exec: $cache\n"] {
                let at = text
                    .find(later)
                    .unwrap_or_else(|| panic!("链上少了 {later}\n{text}"));
                assert!(
                    sink < at,
                    "{strategy:?}：抑制必须在 {later} 之前，否则缓存会把它顶掉：\n{text}"
                );
            }

            // 抑制档不是排序档：不许再画 prefer_* 那条包住后半条链的执行器
            assert!(!text.contains("type: prefer_"), "{strategy:?}：\n{text}");
            assert!(
                !text.contains("      - exec: $family\n"),
                "{strategy:?}：\n{text}"
            );
        }
    }

    /// `prefer_*` 是排序，两族都照答。这一条守的是"用户在面板上选了排序、
    /// 却以为自己选了接管"那类事故——它不许生出任何抑制件。
    #[test]
    fn a_prefer_strategy_sorts_and_draws_no_sink() {
        for (strategy, kind) in [
            (DnsStrategy::PreferIpv4, "prefer_ipv4"),
            (DnsStrategy::PreferIpv6, "prefer_ipv6"),
        ] {
            let mut s = state();
            s.dns.strategy = strategy.clone();
            let text = render(&s).unwrap();
            assert!(
                text.contains(&format!("  - tag: family\n    type: {kind}\n")),
                "{strategy:?} 要落在 {kind}：\n{text}"
            );
            assert!(
                text.contains("      - exec: $family\n"),
                "{strategy:?}：\n{text}"
            );
            for absent in ["tag: sinkhole", "type: qtype", "mode: nodata"] {
                assert!(
                    !text.contains(absent),
                    "{strategy:?} 是排序档，不该有抑制件 {absent}：\n{text}"
                );
            }
        }
    }

    /// 抑制不能借用广告拦截那个执行器：广告要的是"这域名不存在"（NXDOMAIN），
    /// 地址族这一档要的是 NOERROR + 空应答。混用会让客户端把"这站点没配 AAAA"
    /// 读成"这域名不存在"，负面缓存与回退行为都不一样。
    #[test]
    fn the_family_sink_and_the_ads_sink_are_two_executors() {
        let mut s = state();
        s.dns.strategy = DnsStrategy::Ipv4Only;
        s.policy.domain_rules.push(DomainRule::new(
            DomainPattern::parse("+ads.example").unwrap(),
            DomainAction::Block,
        ));
        let text = render(&s).unwrap();
        assert!(
            text.contains(
                "  - tag: blocked\n    type: black_hole\n    args:\n      mode: nxdomain\n"
            ),
            "广告那份：\n{text}"
        );
        assert!(
            text.contains(
                "  - tag: sinkhole\n    type: black_hole\n    args:\n      mode: nodata\n"
            ),
            "地址族那份：\n{text}"
        );
        assert_eq!(text.matches("type: black_hole").count(), 2, "{text}");
    }

    /// 路线那一格：inject 要说清"这一份里暂时没有写回主路由的那两块，所以注入不发生"，
    /// takeover 要说清"没有那两块是设计"。两种都不许偷偷生出一个假的 ros_* 插件。
    #[test]
    fn the_route_switch_says_whether_the_router_gets_written_to() {
        let inject = render(&state()).unwrap();
        assert!(inject.contains("# 路线 = inject（R2）"), "{inject}");
        assert!(
            inject.contains("但注入不发生"),
            "R2 的下半边没接上必须明说：{inject}"
        );

        let mut s = state();
        s.network.route_mode = RouteMode::Takeover;
        let takeover = render(&s).unwrap();
        assert!(takeover.contains("# 路线 = takeover（R1）"), "{takeover}");
        assert!(!takeover.contains("但注入不发生"));

        for text in [inject, takeover] {
            assert!(!text.contains("type: ros_route"), "{text}");
            assert!(!text.contains("type: ros_address_list"), "{text}");
        }
    }

    /// 解析口只绑局域网地址。oxidns 的 server 插件没有客户端 ACL（源码核实表 §1 第 5 条），
    /// `listen` 是唯一的闸：`0.0.0.0` 是开在上游那一侧的开放解析器，`[::]` 会借着
    /// "oxidns 不设 IPV6_V6ONLY"那只双栈 socket 把 v4 的口子原样开回来。
    /// 所以两条通配形状都要断言"不出现"，而不只是断言"绑对了"。
    #[test]
    fn the_resolver_listens_on_the_lan_address_and_never_on_a_wildcard() {
        let mut s = state();
        s.ports.dns = 5353;
        let text = render(&s).unwrap();
        assert!(text.contains("      listen: '192.0.2.10:5353'"), "{text}");
        assert_eq!(
            text.matches("listen: '192.0.2.10:5353'").count(),
            2,
            "udp 与 tcp 各一条"
        );
        assert!(
            !text.contains("listen: '0.0.0.0"),
            "通配 IPv4 一份都不许出现：\n{text}"
        );
        assert!(
            !text.contains("listen: '[::]"),
            "通配 IPv6 一份都不许出现：\n{text}"
        );
        assert!(text.contains("entry: main"), "{text}");
        assert!(
            text.contains("# 解析口绑的是 network.lan_address"),
            "闸落在哪一格里要看得见：\n{text}"
        );
        assert!(
            text.contains("inbound_not_listening"),
            "地址过期的后果要说出来，别留成一次静默起不来：\n{text}"
        );
        assert!(text.contains("http: '127.0.0.1:8053'"), "管理 API 只绑回环");
    }

    /// IPv6 姿态那三档在解析口的差别：不接管只有 v4 那一对；收口 DNS 起多开一对显式 v6 口。
    /// 多开两条而不是把 v4 那对换成 `[::]`，也不把 v6 那对写成 `[::]`：实测 oxidns 不设
    /// IPV6_V6ONLY（guest 实测 `docs/reference/ipv6-kernel-measured.md` §2），
    /// 双栈口收不收 v4 取决于 `net.ipv6.bindv6only` 那个运维可改、发行版可不同的键，
    /// 防泄漏与开放解析器这两条都不许押在它上面。
    #[test]
    fn the_v6_listeners_appear_only_once_dns_is_taken_over() {
        let off = render(&state()).unwrap();
        assert_eq!(off.matches("listen: '192.0.2.10:53'").count(), 2, "{off}");
        assert!(
            !off.contains("2001:db8::10"),
            "不接管这一档不该开 v6 口：{off}"
        );

        for mode in [Ipv6Mode::DnsOnly, Ipv6Mode::DualStack] {
            let mut s = state();
            s.dns.strategy = DnsStrategy::Ipv4Only;
            s.network.ipv6_mode = mode;
            let text = render(&s).unwrap();
            assert_eq!(
                text.matches("listen: '[2001:db8::10]:53'").count(),
                2,
                "{mode:?} 要 udp/tcp 各一条 v6 口：\n{text}"
            );
            assert_eq!(
                text.matches("listen: '192.0.2.10:53'").count(),
                2,
                "{mode:?}：v4 那一对不因为多开 v6 而少掉：\n{text}"
            );
            assert!(
                text.contains("IPV6_V6ONLY"),
                "{mode:?}：为什么不写成 [::] 的理由要看得见：\n{text}"
            );
        }
    }

    /// 这四条（v4 空 / v4 非法 / v6 空 / v6 非法）都是渲染期硬错误。拦在这里而不是"少一条监听"
    /// 或一句警告，是因为这两格决定解析口开在谁面前，而 oxidns 的 `check` 门不验 `listen`
    /// 的地址合法性——松一份的现场是"配置装得上、端口起不来"。
    #[test]
    fn a_missing_or_misspelled_bind_address_refuses_the_render() {
        let mut s = state();
        s.network.lan_address = String::new();
        let err = render(&s).unwrap_err();
        assert!(err.contains("lan_address"), "{err}");
        assert!(
            err.contains("开放解析器"),
            "错误要说清拦下来是为了什么：{err}"
        );

        s.network.lan_address = "192.0.2.0/24".into();
        let err = render(&s).unwrap_err();
        assert!(err.contains("不是一个 IPv4 地址"), "{err}");

        for mode in [Ipv6Mode::DnsOnly, Ipv6Mode::DualStack] {
            let mut s = state();
            s.dns.strategy = DnsStrategy::Ipv4Only;
            s.network.ipv6_mode = mode;
            s.network.lan_address6 = String::new();
            let err = render(&s).unwrap_err();
            assert!(err.contains("lan_address6"), "{mode:?}：{err}");
            assert!(
                err.contains("[::]"),
                "{mode:?}：要说清为什么不能开双栈口：{err}"
            );

            s.network.lan_address6 = "2001:db8::/64".into();
            let err = render(&s).unwrap_err();
            assert!(err.contains("不是一个 IPv6 地址"), "{mode:?}：{err}");
        }
    }

    /// `dns_only` + 非 `*_only` 是**漏洞形状**而不是风格差异：解析收了口、v6 连接层没接管，
    /// 客户端拿到另一族的地址就直连出去。所以渲染阶段就报错，让面板那一格红起来。
    #[test]
    fn dns_only_takeover_without_family_suppression_is_refused() {
        for strategy in [DnsStrategy::PreferIpv4, DnsStrategy::PreferIpv6] {
            let mut s = state();
            s.network.ipv6_mode = Ipv6Mode::DnsOnly;
            s.dns.strategy = strategy.clone();
            let err = render(&s).unwrap_err();
            assert!(
                err.contains("ipv4_only / ipv6_only"),
                "{strategy:?} 要拒掉：{err}"
            );
        }

        // 同样的策略在另两档都合法：不接管由用户自己承担泄漏面，双栈接管则连接层也收了
        for mode in [Ipv6Mode::Off, Ipv6Mode::DualStack] {
            let mut s = state();
            s.network.ipv6_mode = mode;
            render(&s).unwrap_or_else(|e| panic!("{mode:?} + prefer_ipv4 不该报错：{e}"));
        }
    }

    #[test]
    fn a_single_quote_doubles_and_a_newline_is_refused() {
        let mut s = state();
        s.dns.domestic = vec![upstream(
            "198.51.100.7",
            UpstreamTransport::Udp,
            None,
            None,
            Via::Direct,
        )];
        let text = render(&s).unwrap();
        assert!(text.contains("addr: 'udp://198.51.100.7'"), "{text}");

        // 单引号在标量里翻倍，不会把后面的内容变成键
        assert_eq!(ys("a'b", "测试").unwrap(), "'a''b'");
        // 换行落在单引号标量里会被折成空格，那是静默改值，所以宁可不生成
        assert!(ys("a\nb", "测试").is_err());
        let mut s = state();
        s.policy.domain_rules = vec![DomainRule::new(
            DomainPattern::Keyword("a\nb".into()),
            DomainAction::Block,
        )];
        assert!(render(&s).unwrap_err().contains("YAML 里没有安全写法"));
    }

    // ---------- 观测面：查询记录 + 日志落盘 ----------

    /// 记录器是 `with_next() == true` 的执行器（`query_recorder/mod.rs:150-153`），
    /// 所以必须画在链首：排在任何分支之后，记下来的 `execution_path` 就少了前面几步；
    /// 排在 `$sinkhole` 之后，被压掉的那一族根本不留痕——面板上那一档看起来"零查询"。
    #[test]
    fn the_recorder_wraps_the_chain_from_the_first_rule() {
        // 基线就是开启的：这一格是面板「DNS 记录」那一屏的唯一数据源，默认关掉等于那一屏永远空着
        let text = render(&state()).unwrap();
        assert!(
            text.contains(
                "  - tag: record\n    type: query_recorder\n    args:\n      path: 'query-records.db'\n      retention_days: 7\n"
            ),
            "记录器插件少了行：\n{text}"
        );
        assert_eq!(
            text.matches("      - exec: $record\n").count(),
            1,
            "链上只该有一条 $record：\n{text}"
        );
        let head = text
            .find("      - exec: $record\n")
            .unwrap_or_else(|| panic!("链首少了记录器：\n{text}"));
        for later in ["      - exec: $cache\n", "      - exec: accept\n"] {
            let at = text
                .find(later)
                .unwrap_or_else(|| panic!("链上少了 {later}\n{text}"));
            assert!(head < at, "记录器要包住 {later}：\n{text}");
        }
    }

    /// `*_only` 那一档压掉的族也要留下记录——这一条断的是"记录器在抑制件之前"这个顺序。
    #[test]
    fn a_suppressed_family_is_still_recorded() {
        let mut s = state();
        s.dns.strategy = DnsStrategy::Ipv6Only;
        let text = render(&s).unwrap();
        let head = text
            .find("      - exec: $record\n")
            .unwrap_or_else(|| panic!("链首少了记录器：\n{text}"));
        let sink = text
            .find("      - matches: $qtype_sink\n        exec: $sinkhole")
            .unwrap_or_else(|| panic!("少了抑制那条：\n{text}"));
        assert!(head < sink, "被压掉的这一族也要先记下来：\n{text}");
    }

    /// 关掉那一格：插件、链上那一条都没了，但**要留一行看得见的字**说明那一屏为什么是空的——
    /// 否则用户只会报告"采集坏了"。管理 API 照开（面板别的屏还要用）。
    #[test]
    fn turning_off_records_removes_the_plugin_and_says_so() {
        let mut s = state();
        s.dns.records_enabled = false;
        let text = render(&s).unwrap();
        assert!(!text.contains("type: query_recorder"), "{text}");
        assert!(!text.contains("- exec: $record"), "{text}");
        assert!(
            text.contains("# dns.records_enabled = false"),
            "关掉也要在预览里说清那一屏为什么空：\n{text}"
        );
        // 链本身完整：记录器没了，兜底与 accept 还在
        assert!(text.contains("      - exec: accept\n"), "{text}");

        // 关掉之后 retention_days = 0 不再是硬错误——那一份根本不生成 retention_days
        s.dns.records_retention_days = 0;
        render(&s).unwrap_or_else(|e| panic!("已关闭记录时 0 天不该拦路：{e}"));
    }

    /// `retention_days` 原样透传，而 0 是渲染期硬错误：oxidns 的校验要求 ≥1
    /// （`docs/reference/oxidns-plugin-args-verified.md` 的 query_recorder 一节），
    /// 带着 0 装载是起不来，而 0 在直觉里既像"永久"又像"不留"。
    #[test]
    fn retention_days_passes_through_and_zero_is_refused() {
        let mut s = state();
        s.dns.records_retention_days = 30;
        let text = render(&s).unwrap();
        assert!(text.contains("      retention_days: 30\n"), "{text}");

        s.dns.records_retention_days = 0;
        let err = render(&s).unwrap_err();
        assert!(err.contains("retention_days ≥ 1"), "{err}");
        assert!(err.contains("records_enabled"), "{err}");
    }

    /// 日志那一格：`file` 是"in addition to console"（`types.rs:779`），不顶替监督者吃的 stderr；
    /// `rotation` 的缺省是 never（`types.rs:785-788`），不写就是"没人轮转、长到塞满"。
    #[test]
    fn the_log_goes_to_a_rotating_file_without_replacing_stderr() {
        let text = render(&state()).unwrap();
        assert!(
            text.contains("log:\n  level: info\n"),
            "log 一节少了 level：\n{text}"
        );
        assert!(text.contains("  file: 'oxidns.log'\n"), "{text}");
        assert!(
            text.contains("  rotation:\n    type: daily\n    max_files: 3\n"),
            "少了轮转，那份日志会一直长：\n{text}"
        );
        // 落在工作目录（= ZBox 的状态目录），不是绝对路径：生成物要能搬到别的机器上读
        assert!(!text.contains("file: '/"), "{text}");
        assert!(
            text.contains("in addition to console"),
            "要说清这不是替换监督者那一份：\n{text}"
        );
    }
}
