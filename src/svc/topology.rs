//! §6 第 5 步：探测本机的网络形状——上游网关、默认路由网卡、局域网网卡与网段。
//!
//! 这一步以前是"没落地"，代价实测付过一次：彩排用的 state 里网卡名是内置默认的 `ens33`，
//! 而那台机器是 `eth0`，于是持久化 sysctl 文件里绑网卡名的那条键从写下来那天起就没生效过
//! （busybox 的 `sysctl -p` 对未知键只打一行 stderr 然后 rc=0，schema §11.8.6）。
//! 所以这条的要点不是"探得多准"，而是**探不出来的时候不许把默认值写进机器**。
//!
//! 三条硬规则：
//! - state 里 `network` 只要被显式写过（不等于内置基线形状），探测**绝不覆盖**它——
//!   面板和手工 state.json 都是人的判断，机器不跟人抢。
//! - 判不出或有歧义 → 报错停下，把候选列出来给人看。
//! - 唯一允许的"降级继续"是 `ip` 命令压根跑不起来（Windows 开发机、chroot）：
//!   那台机器本来也装不成旁路由，保留现值 + 打警告，不假装有结论。
//!
//! 解析输入的形状来自 guest 逐字抓取：`ip -4 route show default` 是 2026-09-30 原文
//! （schema §11.7），`ip -4 -o addr show` 是 2026-10-05 在单网卡与 netns 双网卡两种现场
//! 各抓一份（schema §11.8.8）。测试 fixture 里除 `wg0`/`docker0`/`secondary` 三条外都是原文。

use crate::model::config::{DesiredState, Network};
use crate::svc::runner::{failure_detail, Runner};
use std::net::Ipv4Addr;

/// 一张网卡上的一个 IPv4 地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addr {
    pub interface: String,
    pub address: Ipv4Addr,
    pub prefix: u8,
}

impl Addr {
    /// 所在网段的 CIDR（把主机位抹掉）。写进快照的应当是网段本身：`ip saddr` 和前缀相交
    /// 判定都只看网络位，留一个 `192.168.219.128/24` 在现场里只会让人以为配错了。
    pub fn network_cidr(&self) -> String {
        let mask = prefix_mask(self.prefix);
        let net = u32::from(self.address) & mask;
        format!("{}/{}", Ipv4Addr::from(net), self.prefix)
    }

    /// 是不是"一个局域网段"的形状。`/32` 是点对点链路（WireGuard、tun 的常见写法），
    /// 它给不出能让局域网设备落在里面的 `lan_cidr`。
    fn is_lan_shaped(&self) -> bool {
        self.prefix < 32
    }
}

/// 现场读到的网络形状。字段分开降级：任何一条命令没跑成都不影响另一条的结论。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Topology {
    pub gateway: Option<Ipv4Addr>,
    /// 默认路由所在网卡
    pub wan_interface: Option<String>,
    pub addrs: Vec<Addr>,
    /// `ip` 没跑起来的原因。与"跑成了但形状判不出"是两回事，后者必须停下问人。
    pub probe_error: Option<String>,
}

impl Topology {
    /// 一次读两条命令：`ip -4 route show default`（WAN + 上游网关）与
    /// `ip -4 -o addr show`（各网卡地址）。解析只看 stdout——实测 stderr 与 stdout
    /// 在 ssh 下都不保证顺序，混着喂给解析器是给自己埋坑。
    pub fn detect(runner: &dyn Runner) -> Self {
        let mut topo = Self::default();
        match runner.exec(&["ip", "-4", "route", "show", "default"]) {
            Ok(exec) if exec.succeeded() => {
                let (iface, gateway) = default_route(&exec.stdout);
                topo.wan_interface = iface;
                topo.gateway = gateway;
            }
            Ok(exec) => {
                topo.probe_error = Some(format!(
                    "`ip -4 route show default` 退出 {}：{}",
                    exec.code,
                    failure_detail(&exec.merged(), exec.code)
                ));
            }
            Err(e) => topo.probe_error = Some(format!("跑不了 `ip -4 route show default`：{e}")),
        }
        match runner.exec(&["ip", "-4", "-o", "addr", "show"]) {
            Ok(exec) if exec.succeeded() => topo.addrs = addrs(&exec.stdout),
            Ok(exec) => {
                if topo.probe_error.is_none() {
                    topo.probe_error = Some(format!(
                        "`ip -4 -o addr show` 退出 {}：{}",
                        exec.code,
                        failure_detail(&exec.merged(), exec.code)
                    ));
                }
            }
            Err(e) => {
                if topo.probe_error.is_none() {
                    topo.probe_error = Some(format!("跑不了 `ip -4 -o addr show`：{e}"));
                }
            }
        }
        topo
    }

    /// 网卡名 → 该卡上网络位最大的那个地址（同一张卡的第二地址、/32 的隧道地址都不算）。
    fn lan_candidates(&self) -> Vec<Addr> {
        let mut kept: Vec<Addr> = Vec::new();
        for addr in self
            .addrs
            .iter()
            .filter(|a| a.interface != "lo" && a.is_lan_shaped())
        {
            match kept.iter().position(|k| k.interface == addr.interface) {
                // 同卡多地址：取前缀最小（网段最大）的那条，这是"这张卡在哪个段"的答案
                Some(i) => {
                    if addr.prefix < kept[i].prefix {
                        kept[i] = addr.clone();
                    }
                }
                None => kept.push(addr.clone()),
            }
        }
        kept
    }

    /// 结论。`Err` 的文案是直接给人看的，带上候选清单，不要求调用方再去查现场。
    pub fn verdict(&self) -> Result<Network, String> {
        if let Some(detail) = &self.probe_error {
            return Err(format!("现场读不到网卡形状：{detail}"));
        }
        let Some(wan) = self.wan_interface.clone() else {
            return Err(
                "机器上没有 IPv4 默认路由，判不出上游网卡。先把本机自己接上网，\
                 或在 state.json 里显式写 network.lan_interface / lan_cidr / wan_interface。"
                    .into(),
            );
        };
        let candidates = self.lan_candidates();
        let others: Vec<&Addr> = candidates.iter().filter(|a| a.interface != wan).collect();
        match others.len() {
            // 只有默认路由那张卡有段：单网卡旁路由，进出同卡
            0 => candidates
                .iter()
                .find(|a| a.interface == wan)
                .map(|a| network(a, wan.clone()))
                .ok_or_else(|| {
                    format!(
                        "默认路由在 {wan} 上，但那张卡没有可用的 IPv4 段（只看到 {:?}）。\
                         写出上游网关的网段再试，或手工指定 network。",
                        self.addrs
                            .iter()
                            .map(|a| format!("{}={}", a.interface, a.network_cidr()))
                            .collect::<Vec<_>>()
                    )
                }),
            // 默认路由之外正好一张有段的卡：那就是局域网
            1 => Ok(network(others[0], wan)),
            n => Err(format!(
                "{n} 张不是上游网卡的网卡都带着 IPv4 段（{}），ZBox 不猜哪张是局域网。\
                 在 state.json 里显式写 network.lan_interface 与 lan_cidr。",
                others
                    .iter()
                    .map(|a| format!("{}={}", a.interface, a.network_cidr()))
                    .collect::<Vec<_>>()
                    .join("、")
            )),
        }
    }
}

fn network(addr: &Addr, wan: String) -> Network {
    Network {
        lan_interface: addr.interface.clone(),
        lan_cidr: addr.network_cidr(),
        // 同一次探测里就有本机地址，oxidns 那份生成物的解析口要绑它（gen::oxidns 的
        // `dns_bind`）。只填 IPv4：v6 一侧的 SLAAC 地址是 `dynamic` 且会到期
        // （2026-10-11 guest 原文：`scope global dynamic mngtmpaddr ... valid_lft 86390sec`），
        // 把一个会自己消失的地址写进配置，等于埋一个定时失效的绑定——那一侧要求人显式给。
        lan_address: addr.address.to_string(),
        wan_interface: wan,
        ..Network::default()
    }
}

/// 填进初始 Model 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filled {
    pub network: Network,
    /// 这份 network 是人写的还是探来的
    pub how: How,
    /// 不拦路但要说出来的话
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    KeptExplicit,
    Detected,
}

/// 决定"用不用探测结果"。唯一的分岔点是 state 里的 `network` 是不是还等于内置基线形状。
pub fn fill(state: &DesiredState, topo: &Topology) -> Result<Filled, String> {
    if state.network != Network::default() {
        let mut warnings = Vec::new();
        // 人写的值不覆盖，但也不能照抄不吭声：彩排那次就是"人写的 ens33"躺在文件里，
        // 机器上是 eth0，一路静默到 sysctl 键名里。
        if topo.probe_error.is_none()
            && !topo
                .addrs
                .iter()
                .any(|a| a.interface == state.network.lan_interface)
        {
            warnings.push(format!(
                "state.json 写的局域网网卡 {} 在本机找不到（本机有：{}）",
                state.network.lan_interface,
                topo.interface_list()
            ));
        }
        if let Some(wan) = &topo.wan_interface {
            if &state.network.wan_interface != wan {
                warnings.push(format!(
                    "state.json 写的上游网卡 {} 与默认路由网卡 {wan} 不一致",
                    state.network.wan_interface
                ));
            }
        }
        // 解析口绑的就是这个地址（gen::oxidns），它不在本机任何网卡上 = oxidns 起不来。
        // 地址会换（DHCP、改名、换网卡），所以这里出声，而不是让人从"面板没反应"倒推。
        if topo.probe_error.is_none()
            && !state.network.lan_address.is_empty()
            && !topo
                .addrs
                .iter()
                .any(|a| a.address.to_string() == state.network.lan_address)
        {
            warnings.push(format!(
                "state.json 写的本机局域网 IPv4 {} 不在本机任何网卡上（本机现在：{}）。\
                 DNS 解析口绑的就是它，对不上就是起不来——doctor 的 inbound_not_listening 会报这一条",
                state.network.lan_address,
                topo.addr_list()
            ));
        }
        return Ok(Filled {
            network: state.network.clone(),
            how: How::KeptExplicit,
            warnings,
        });
    }
    match topo.verdict() {
        Ok(network) => Ok(Filled {
            network,
            how: How::Detected,
            warnings: Vec::new(),
        }),
        // 命令跑不起来（开发机、chroot）：保留现值 + 警告，别把"没 ip"报成"配错了"
        Err(detail) if topo.probe_error.is_some() => Ok(Filled {
            network: state.network.clone(),
            how: How::KeptExplicit,
            warnings: vec![format!("没能探测网卡形状，沿用 state 里的值：{detail}")],
        }),
        Err(detail) => Err(detail),
    }
}

impl Topology {
    fn interface_list(&self) -> String {
        if self.addrs.is_empty() {
            return "一个 IPv4 地址都没读到".into();
        }
        let mut names: Vec<String> = Vec::new();
        for addr in &self.addrs {
            if !names.contains(&addr.interface) {
                names.push(addr.interface.clone());
            }
        }
        names.join("、")
    }

    /// 比 `interface_list` 细一档：网卡名 + 该卡上的地址，供"你写的地址不在本机"这类
    /// 警告把候选直接列出来，不要求人再去跑一次 `ip addr`。
    fn addr_list(&self) -> String {
        if self.addrs.is_empty() {
            return "一个 IPv4 地址都没读到".into();
        }
        self.addrs
            .iter()
            .map(|a| format!("{}={}", a.interface, a.address))
            .collect::<Vec<_>>()
            .join("、")
    }
}

/// `ip -4 route show default` → (dev, via)。实测原文：
/// `default via 192.168.219.2 dev eth0 metric 202 `（行尾有空格）。
/// 取 `dev` 那个 token 而不是下一跳地址。
fn default_route(text: &str) -> (Option<String>, Option<Ipv4Addr>) {
    for line in text.lines() {
        let mut it = line.split_whitespace();
        if it.next() != Some("default") {
            continue;
        }
        let mut iface = None;
        let mut gateway = None;
        while let Some(token) = it.next() {
            match token {
                "dev" => iface = it.next().map(str::to_string),
                "via" => gateway = it.next().and_then(|v| v.parse().ok()),
                _ => {}
            }
        }
        if iface.is_some() {
            return (iface, gateway);
        }
    }
    (None, None)
}

/// `ip -4 -o addr show` → 每张卡的 IPv4 地址。
///
/// 只认带 `inet` 的行；接口名取行首 `N: <name>` 的第二个 token，并把 iproute2 给
/// 虚拟接口加的 `@if3` 后缀去掉——网卡名会原样进 `net.ipv4.conf.<name>.rp_filter`
/// 和 nft 的 `oifname`，带着 `@` 的键名内核不认。
fn addrs(text: &str) -> Vec<Addr> {
    let mut out = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let Some(inet_at) = parts.iter().position(|t| *t == "inet") else {
            continue;
        };
        let Some(slashable) = parts.get(inet_at + 1) else {
            continue;
        };
        let Some((raw_addr, raw_prefix)) = slashable.split_once('/') else {
            continue;
        };
        let Ok(address) = raw_addr.parse::<Ipv4Addr>() else {
            continue;
        };
        let Ok(prefix) = raw_prefix.parse::<u8>() else {
            continue;
        };
        if prefix > 32 {
            continue;
        }
        // 行首两 token 是 `2:` 和网卡名；形状不对就整行跳过，不猜
        let Some(name) = parts
            .first()
            .filter(|t| t.ends_with(':'))
            .and_then(|_| parts.get(1))
        else {
            continue;
        };
        out.push(Addr {
            interface: name.split('@').next().unwrap_or(name).to_string(),
            address,
            prefix,
        });
    }
    out
}

fn prefix_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svc::runner::{Exec, Scripted};

    /// guest 实测原文（schema §11.7 那台 Alpine，VMnet8 NAT）
    const ROUTE: &str = "default via 192.168.219.2 dev eth0 metric 202 \n";

    /// 2026-10-05 guest 逐字原文，单网卡现场（VMnet8 NAT，只有 lo 与 eth0 有 IPv4）。
    /// 注意 `-o` 把续行折成"反斜杠 + 一串空格"，且 `brd` 在 `scope` 之前。
    const ONE_NIC: &str = "\
1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: eth0    inet 192.168.219.128/24 brd 192.168.219.255 scope global eth0\\       valid_lft forever preferred_lft forever
";

    /// 同一台机器、netns 拓扑加载着的时候抓的：局域网侧是桥 `zbox-lan0`，
    /// 桥的地址**没有 `brd` 段**——所以解析不许依赖 `brd` 存在。
    const TWO_NIC: &str = "\
1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: eth0    inet 192.168.219.128/24 brd 192.168.219.255 scope global eth0\\       valid_lft forever preferred_lft forever
8: zbox-lan0    inet 10.99.0.1/24 scope global zbox-lan0\\       valid_lft forever preferred_lft forever
";

    fn exec(code: i32, stdout: &str) -> Result<Exec, String> {
        Ok(Exec {
            code,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    /// 两条命令按 `ip -4 route show default`、`ip -4 -o addr show` 的顺序脚本化
    fn topo(route: Result<Exec, String>, addr: Result<Exec, String>) -> Topology {
        let runner = Scripted::with_results(vec![route, addr]);
        Topology::detect(&runner)
    }

    fn ok_addr(text: &str) -> Result<Exec, String> {
        exec(0, text)
    }

    #[test]
    fn a_single_nic_box_is_the_lan_and_wan_on_the_same_card() {
        let topo = topo(exec(0, ROUTE), ok_addr(ONE_NIC));
        assert_eq!(topo.gateway, Some("192.168.219.2".parse().unwrap()));
        let got = topo.verdict().expect("单网卡形状该给出结论");
        assert_eq!(
            got,
            Network {
                lan_interface: "eth0".into(),
                lan_cidr: "192.168.219.0/24".into(),
                // 同一次探测要把本机地址也填上：oxidns 的解析口只绑它（gen::oxidns::dns_binds）
                lan_address: "192.168.219.128".into(),
                wan_interface: "eth0".into(),
                ..Network::default()
            },
            "主机位要被抹成网段"
        );
    }

    #[test]
    fn the_card_that_is_not_the_default_route_becomes_the_lan() {
        let got = topo(exec(0, ROUTE), ok_addr(TWO_NIC))
            .verdict()
            .expect("双网卡形状该给出结论");
        assert_eq!(got.lan_interface, "zbox-lan0");
        assert_eq!(got.lan_cidr, "10.99.0.0/24");
        assert_eq!(
            got.lan_address, "10.99.0.1",
            "解析口绑的地址要来自局域网那张卡，不是上游卡"
        );
        assert_eq!(got.wan_interface, "eth0");
    }

    #[test]
    fn a_point_to_point_tunnel_is_never_the_lan_segment() {
        // WireGuard 之类只会给出 /32，拿它当 lan_cidr 等于劫持层谁都不覆盖。
        // wg0 这行是按同样形状造的：这台 VM 没有第二个可造段的东西。
        let text = format!(
            "{TWO_NIC}4: wg0    inet 10.8.0.2/32 scope global wg0\\       valid_lft forever preferred_lft forever\n"
        );
        let got = topo(exec(0, ROUTE), ok_addr(&text))
            .verdict()
            .expect("/32 该被跳过，剩下的 zbox-lan0 仍是唯一候选");
        assert_eq!(got.lan_interface, "zbox-lan0");
    }

    #[test]
    fn two_lan_shaped_cards_beyond_the_wan_are_refused_not_guessed() {
        let text = format!(
            "{TWO_NIC}5: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0\\\n"
        );
        let err = topo(exec(0, ROUTE), ok_addr(&text))
            .verdict()
            .expect_err("两张候选卡不能靠排序挑一张");
        assert!(
            err.contains("zbox-lan0") && err.contains("docker0"),
            "{err}"
        );
        assert!(err.contains("不猜"), "错误里要说清为什么不干：{err}");
    }

    #[test]
    fn secondary_addresses_on_one_card_are_one_candidate_not_two() {
        // docker0 与 secondary 两条是按原文形状造的：这台 VM 上没有 Docker，
        // 桥也不能挂第二地址，能造出的真实候选只有 zbox-lan0 一张。
        let text = TWO_NIC.replace(
            "8: zbox-lan0    inet 10.99.0.1/24",
            "8: zbox-lan0    inet 10.99.9.9/32",
        ) + "8: zbox-lan0    inet 10.99.0.1/24 scope global secondary zbox-lan0\\\n";
        let got = topo(exec(0, ROUTE), ok_addr(&text))
            .verdict()
            .expect("同一张卡的多个地址不是歧义");
        assert_eq!(got.lan_cidr, "10.99.0.0/24", "取网络位最大的那条");
    }

    #[test]
    fn a_missing_default_route_stops_and_says_why() {
        let err = topo(exec(0, ""), ok_addr(ONE_NIC))
            .verdict()
            .expect_err("没有默认路由就是没探到上游");
        assert!(err.contains("默认路由"), "{err}");
    }

    #[test]
    fn veth_style_iface_suffix_never_reaches_a_sysctl_key() {
        // `@ifN` 这个形状是在 netns 里看到的（`ip -br addr` 给出 `lan1-eth@if9`），
        // 宿主 ns 的 `ip -4 -o addr show` 实测没打印过带 @ 的名字；这条按造的形状保住键名。
        let text = "7: veth1@if8    inet 10.7.0.1/24 scope global veth1\\\n";
        let parsed = addrs(text);
        assert_eq!(parsed[0].interface, "veth1", "带 @ 的名字进内核键名是废的");
    }

    #[test]
    fn an_unrunnable_probe_degrades_instead_of_inventing_a_fault() {
        // Windows 开发机上两条都跑不了：保留 state 里的值 + 一条警告，不报错
        let topo = topo(
            Err("program not found".into()),
            Err("program not found".into()),
        );
        assert!(topo.probe_error.is_some());
        let filled = fill(&DesiredState::baseline(), &topo).expect("跑不动不拦路");
        assert_eq!(filled.how, How::KeptExplicit);
        assert_eq!(filled.warnings.len(), 1, "{:?}", filled.warnings);
        assert!(filled.warnings[0].contains("没能探测"));
    }

    #[test]
    fn explicit_network_values_are_never_overwritten_but_a_stale_name_is_called_out() {
        let topo = topo(exec(0, ROUTE), ok_addr(ONE_NIC));
        // 只改一张网卡名也算"人写过"：整个 network 都按人说的走，探测不许挑着覆盖
        let mut state = DesiredState::baseline();
        state.network.lan_interface = "br-lan".into();
        let kept = fill(&state, &topo).expect("显式值不覆盖");
        assert_eq!(kept.network, state.network);
        assert_eq!(kept.how, How::KeptExplicit);
        assert!(
            kept.warnings.iter().any(|w| w.contains("br-lan")),
            "本机没有 br-lan 这件事必须说出来：{:?}",
            kept.warnings
        );

        // 全默认形状（没人写过）才是探测的入口，ens33 不会被静默留在机器上
        let filled = fill(&DesiredState::baseline(), &topo).expect("基线形状该用探测值");
        assert_eq!(filled.how, How::Detected);
        assert_eq!(filled.network.lan_interface, "eth0");
        assert!(filled.warnings.is_empty(), "{:?}", filled.warnings);
    }

    /// 解析口只绑这一格（`gen::oxidns::dns_binds`），所以"你填的地址现在不在本机任何网卡上"
    /// 是一件必须在这里说出来的事：它的后果是 DNS 直接不起，不是少一条优化。
    #[test]
    fn a_bind_address_the_machine_no_longer_has_is_called_out() {
        let topo = topo(exec(0, ROUTE), ok_addr(ONE_NIC));
        let mut state = DesiredState::baseline();
        state.network.lan_interface = "eth0".into();
        state.network.lan_cidr = "192.168.219.0/24".into();
        // 机器换了地址（DHCP 重新分配），配置里还留着旧的
        state.network.lan_address = "192.168.219.77".into();
        let kept = fill(&state, &topo).expect("显式值不覆盖");
        assert_eq!(kept.how, How::KeptExplicit);
        let hit = kept
            .warnings
            .iter()
            .find(|w| w.contains("192.168.219.77"))
            .unwrap_or_else(|| panic!("警告里要点名那个地址：{:?}", kept.warnings));
        assert!(hit.contains("inbound_not_listening"), "{hit}");
        assert!(
            hit.contains("192.168.219.128"),
            "要把本机现在有的地址列出来：{hit}"
        );

        // 地址对得上时一声都不出——这条断言防的是"每次都白刷一遍警告"
        let mut good = state.clone();
        good.network.lan_address = "192.168.219.128".into();
        let kept = fill(&good, &topo).expect("同一格填对了就该安静");
        assert!(
            kept.warnings.iter().all(|w| !w.contains("IPv4")),
            "{:?}",
            kept.warnings
        );
    }

    #[test]
    fn an_ambiguous_shape_stops_the_install_not_the_dev_box() {
        let text = format!("{TWO_NIC}5: docker0    inet 172.17.0.1/16 scope global docker0\\\n");
        let topo = topo(exec(0, ROUTE), ok_addr(&text));
        assert!(
            fill(&DesiredState::baseline(), &topo).is_err(),
            "判不出时不许带着默认值往下写"
        );
    }

    #[test]
    fn the_mask_arithmetic_matches_the_cidr_the_kernel_wants() {
        let addr = Addr {
            interface: "eth0".into(),
            address: "198.18.0.1".parse().unwrap(),
            prefix: 15,
        };
        assert_eq!(addr.network_cidr(), "198.18.0.0/15");
        let zero = Addr {
            prefix: 0,
            address: "1.2.3.4".parse().unwrap(),
            ..addr
        };
        assert_eq!(zero.network_cidr(), "0.0.0.0/0");
    }
}
