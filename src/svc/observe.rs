//! 现场读数：把 [`doctor::Reading`] 填上。
//!
//! 每个字段单独取、单独降级：取不到就是 `None`，诊断器不许把 `None` 判成"不符合"
//! （`doctor.rs` 里那条 `missing_readings_are_never_reported_as_failures` 就是这个约定）。
//! 所以这里没有"整轮失败"这一说——只有哪一条命令没跑成。
//!
//! 所有解析函数的输入 fixture 都是 2026-09-30 在 Alpine 3.21.3 guest 上逐字抓下来的
//! （schema §11.7 那套 netns 拓扑，当时 nft 表与策略路由正加载着），不是照文档想象的格式。

use crate::doctor::Reading;
use crate::gen::init::{InitSystem, ServiceAction};
use crate::gen::netstate;
use crate::gen::nft;
use crate::gen::singbox::route::DIVERT_MARK;
use crate::svc::runner::{argv_refs, Runner};

/// 在听端口（两族）、nft 两张表、两族策略路由、四个 sysctl 键、默认路由——`doctor` 要的十二项现场。
pub struct Observer<'a> {
    runner: &'a dyn Runner,
    init: InitSystem,
}

impl<'a> Observer<'a> {
    pub fn new(runner: &'a dyn Runner, init: InitSystem) -> Self {
        Self { runner, init }
    }

    pub fn read(&self) -> Reading {
        Reading {
            service_active: self.service_active(),
            listening: self.listening(),
            nft_table_loaded: self.nft_table_loaded(),
            divert_rule_loaded: self.divert_rule_loaded(),
            ip_forward: self.sysctl_u8("net.ipv4.ip_forward"),
            rp_filter_all: self.sysctl_u8("net.ipv4.conf.all.rp_filter"),
            default_route_iface: self.default_route_iface(),
            v6_listening: self.v6_listening(),
            v6_table_loaded: self.v6_table_loaded(),
            v6_divert_rule_loaded: self.v6_divert_rule_loaded(),
            v6_forward_all: self.sysctl_u8("net.ipv6.conf.all.forwarding"),
            v6_accept_ra_all: self.sysctl_u8("net.ipv6.conf.all.accept_ra"),
        }
    }

    /// 只看命令退出码：systemd 的 `is-active` 非活跃时 rc≠0，OpenRC 的 `status` 同形。
    /// 不去匹配 "active" 这个字面量，措辞随版本变，rc 不变。
    fn service_active(&self) -> Option<bool> {
        let argv = self.init.control_command(ServiceAction::Status);
        self.runner
            .exec(&argv_refs(&argv))
            .map(|r| r.succeeded())
            .ok()
    }

    /// `ss -lntu` 一条命令同时给 TCP 与 UDP：DNS 入站的 53 在 UDP 侧是 `UNCONN`
    /// 而不是 `LISTEN`，只看 TCP 会把"DNS 起来了"读成"没在听"。
    ///
    /// 公开给 [`crate::svc::Svc::probe`] 用：生效性判据要的是"端口在听"，而**不能靠连端口**
    /// ——实测连一次 TPROXY 端口会自激成环（schema §11.8.4）。
    pub fn listening(&self) -> Option<Vec<u16>> {
        let out = self.ok_stdout(&["ss", "-lntu"])?;
        Some(listening_ports(&out))
    }

    /// 表在不在用 rc 判，不用 stdout：`nft list table ip zbox` 对不存在的表
    /// 报 `Error: No such file or directory` 且 rc=1（实测）。
    fn nft_table_loaded(&self) -> Option<bool> {
        let argv = ["nft", "list", "table", "ip", nft::TABLE];
        self.runner.exec(&argv).map(|r| r.succeeded()).ok()
    }

    fn divert_rule_loaded(&self) -> Option<bool> {
        let out = self.ok_stdout(&["ip", "-4", "rule", "show"])?;
        Some(has_divert_rule(&out))
    }

    /// IPv6 那一侧在听的端口。**只能靠 `-6` 过滤器，不许从 [`Self::listening`] 里推**：
    /// 2026-10-09 在 guest 的空 netns 里跑一只绑 `::` 的入站，`ss -lntu` 打的是
    /// `*:19889`，`ss -lntu -4` **一行都没有**，而 `ss -lntu -6` 有同一行
    /// （fixture 见 `SS6`）。地址字面量判不了族，只有过滤器能。
    ///
    /// 反向不成立，所以 v4 那一半继续用不带过滤器的 `ss -lntu`：双栈 socket 对 v4 客户端
    /// 也是通的（同一只口，`nc -w1 127.0.0.1 19889` 实测 rc=0），拿 `-4` 去查会把
    /// “其实在听”读成“没在听”。
    fn v6_listening(&self) -> Option<Vec<u16>> {
        let out = self.ok_stdout(&["ss", "-lntu", "-6"])?;
        Some(listening_ports(&out))
    }

    /// `ip zbox` 与 `ip6 zbox` 是两张独立的表（同日实测：只加载 ip6 之后
    /// `nft list table ip zbox` 仍 rc=1），所以 v4 那条读数一条也不能代表它。
    fn v6_table_loaded(&self) -> Option<bool> {
        let argv = ["nft", "list", "table", "ip6", nft::TABLE];
        self.runner.exec(&argv).map(|r| r.succeeded()).ok()
    }

    /// v6 的 `ip rule` 行形状与 v4 逐字相同（§4 实测：`100: from all fwmark 0x1 lookup 100`），
    /// 所以解析器共用。**基线不共用**：v4 出厂是三行、v6 只有两行（没有 `32767:`），
    /// 任何按“行数”判的实现都必须分族写。
    fn v6_divert_rule_loaded(&self) -> Option<bool> {
        let out = self.ok_stdout(&["ip", "-6", "rule", "show"])?;
        Some(has_divert_rule(&out))
    }

    fn default_route_iface(&self) -> Option<String> {
        let out = self.ok_stdout(&["ip", "-4", "route", "show", "default"])?;
        default_route_iface(&out)
    }

    fn sysctl_u8(&self, key: &str) -> Option<u8> {
        let out = self.ok_stdout(&["sysctl", "-n", key])?;
        parse_sysctl_value(&out)
    }

    /// rc=0 时取 stdout（sing-box/ss/nft 都往 stdout 正经输出），否则 `None`。
    fn ok_stdout(&self, argv: &[&str]) -> Option<String> {
        self.runner.exec(argv).ok().and_then(|r| {
            if r.succeeded() {
                Some(r.merged())
            } else {
                None
            }
        })
    }
}

/// `ss -lntu` → 在听端口集合（升序去重）。表头行按端口列解析不出来，自然被跳过。
pub fn listening_ports(ss_output: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = Vec::new();
    for line in ss_output.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // Netid State Recv-Q Send-Q Local-Address:Port Peer-Address:Port
        let Some(local) = parts.get(4) else { continue };
        let Some(port) = local.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) else {
            continue;
        };
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports.sort_unstable();
    ports
}

/// 是否存在 `fwmark <DIVERT_MARK> -> table <DIVERT_TABLE>` 这条策略路由。
///
/// 两个条件都要：只匹配 table 会把用户自己加的 `lookup 100` 读成劫持已生效；
/// 只匹配 mark 则 mark 被别的规则用时误报。mark 的十六进制位数随 iproute2 版本
/// 和取值变化（`0x1` / `0x00000001`），所以按数值比，不比字符串。
pub fn has_divert_rule(ip_rule_output: &str) -> bool {
    ip_rule_output.lines().any(|line| {
        let table_hit = line
            .split_whitespace()
            .any(|t| t == netstate::DIVERT_TABLE.to_string());
        let mark_hit = mark_of(line) == Some(DIVERT_MARK);
        table_hit && mark_hit
    })
}

/// 从一条 `ip rule` 里取 fwmark 数值。iproute2 可能写成 `0x1` 或带掩码的 `0x1/0xffffffff`。
fn mark_of(line: &str) -> Option<u32> {
    let mut it = line.split_whitespace();
    while let Some(token) = it.next() {
        if token == "fwmark" {
            let raw = it.next()?.split('/').next()?;
            let digits = raw.strip_prefix("0x").unwrap_or(raw);
            return u32::from_str_radix(digits, if raw.starts_with("0x") { 16 } else { 10 }).ok();
        }
    }
    None
}

fn default_route_iface(route_output: &str) -> Option<String> {
    for line in route_output.lines() {
        let mut it = line.split_whitespace();
        if it.next() != Some("default") {
            continue;
        }
        while let Some(token) = it.next() {
            if token == "dev" {
                return it.next().map(str::to_string);
            }
        }
    }
    None
}

/// `sysctl -n` 的实测输出就是一行数值（`1` / `2`）。
fn parse_sysctl_value(out: &str) -> Option<u8> {
    out.trim().parse::<u8>().ok()
}

/// dae 找 geo 数据的三个共享目录，按文档里的查找序排（`docs/reference/dae-config-dictionary.md`）。
pub const GEO_DATA_DIRS: [&str; 3] = [
    "/root/.local/share/dae",
    "/usr/local/share/dae",
    "/usr/share/dae",
];

/// dae 那份 geo 数据在不在机器上：`geoip.dat` 与 `geosite.dat` 在**同一个目录**里都在。
///
/// 纯文件系统检查，一条命令都不发。它不进 [`Reading`]：这一格决定的是面板上那句话怎么讲
/// （智能分流这一档要的数据机器上还没有），不是要喂给 `doctor` 的现场读数。
///
/// 「同一个目录」是硬条件而不是稳妥——dae 是按目录找这两份的，一半在 `/usr/share`
/// 一半在 `/usr/local/share` 等于没有。
///
/// **查不到"配置文件同目录"那一格**：dae 配置的落点还没进 `Layout`（#56 的前置），
/// 所以数据只放在配置同目录时会报成"不在"。这一条因此是偏保守的方向——代价是面板多说
/// 一句老实话，不是少说一句。
pub fn geo_data_present() -> bool {
    geo_data_present_in(&GEO_DATA_DIRS)
}

/// [`geo_data_present`] 的目录可注入版。给测试用：真实的那三个目录在开发机上不存在，
/// 而在 guest 上存在与否不由测试决定——两支都要能钉住，判据才只许有一个版本。
pub fn geo_data_present_in(dirs: &[&str]) -> bool {
    dirs.iter().any(|dir| {
        let base = std::path::Path::new(dir);
        base.join("geoip.dat").is_file() && base.join("geosite.dat").is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::DesiredState;
    use crate::svc::runner::{Exec, Scripted};

    /// 2026-09-30 guest 实测原文（fakeip 内核在跑、nft 表与策略路由已加载）
    const SS: &str = "Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port
udp   UNCONN 0      0            0.0.0.0:53        0.0.0.0:*   
udp   UNCONN 0      0            0.0.0.0:9888      0.0.0.0:*   
tcp   LISTEN 0      4096         0.0.0.0:7890      0.0.0.0:*   
tcp   LISTEN 0      4096         0.0.0.0:8888      0.0.0.0:*   
tcp   LISTEN 0      4096         0.0.0.0:9888      0.0.0.0:*   
tcp   LISTEN 0      4096         0.0.0.0:9887      0.0.0.0:*   
tcp   LISTEN 0      4096         0.0.0.0:53        0.0.0.0:*   
tcp   LISTEN 0      128          0.0.0.0:22        0.0.0.0:*   
tcp   LISTEN 0      128             [::]:22           [::]:*   
";

    const IP_RULE: &str = "0:	from all lookup local
100:	from all fwmark 0x1 lookup 100
32766:	from all lookup main
32767:	from all lookup default
";

    /// `ss -lntu -6` 的两行，钉的是 **iproute2 怎么打印一个 IPv6 socket**（与被编排的是谁无关）：
    /// `[::]:22` 是宿主机上 sshd 那种 V6ONLY 的口（当前基线实测原文），`*:19889` 是绑 `::`
    /// 的双栈口——2026-10-09 在空 netns 里用一只 `AF_INET6` 的 socket 点火抓到的形态，
    /// 同一次实测里 `ss -lntu -4` 对它**一行都不输出**。两行放一份 fixture 里是为了让
    /// “字面量判不了族、只有 `-6` 过滤器能”这件事在同一份输入上被看见；解析器对两种形态
    /// 都只取末尾冒号后的端口号，所以取到的是 `22` 与 `19889`。
    const SS6: &str = "Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port
tcp   LISTEN 0      4096               *:19889            *:*
tcp   LISTEN 0      128             [::]:22           [::]:*
";

    /// §4 实测：v6 的 `ip rule` 行形状与 v4 逐字相同，出厂基线却只有两行（没有 `32767:`）。
    const IP6_RULE: &str = "0:	from all lookup local
100:	from all fwmark 0x1 lookup 100
32766:	from all lookup main
";

    const DEFAULT_ROUTE: &str = "default via 192.168.219.2 dev eth0 metric 202 \n";

    #[test]
    fn listening_ports_includes_the_udp_dns_inbound() {
        assert_eq!(
            listening_ports(SS),
            vec![22, 53, 7890, 8888, 9887, 9888],
            "UDP 的 UNCONN 必须算在听，否则 :53 会被读成没起来"
        );
        assert!(listening_ports("").is_empty());
    }

    #[test]
    fn the_divert_rule_is_matched_on_mark_and_table_together() {
        assert!(has_divert_rule(IP_RULE));
        // 用户自己往 table 100 加了条无 mark 规则：不算劫持生效
        assert!(!has_divert_rule(
            "0:\tfrom all lookup local\n100:\tfrom all lookup 100\n"
        ));
        // mark 对但表不对（比如它自己的策略路由）
        assert!(!has_divert_rule("100:\tfrom all fwmark 0x1 lookup 200\n"));
        // 同一含义的另一种 iproute2 写法：补零 + 掩码
        assert!(has_divert_rule(
            "100:\tfrom all fwmark 0x00000001/0xffffffff lookup 100\n"
        ));
    }

    #[test]
    fn default_route_reads_the_device_not_the_nexthop() {
        assert_eq!(default_route_iface(DEFAULT_ROUTE).as_deref(), Some("eth0"));
        // 读不出设备就留 None：宁当"没读到"，也不把措辞变化报成故障
        assert_eq!(default_route_iface(""), None);
    }

    /// 「这一档要的那两份数据在不在」只认**同一个目录里两份都在**。
    /// 这条判据是实测来的：`dae validate` 缺任一份就 rc=1
    /// （`geoip.dat: file does not exist in [...]`，2026-10-09 v2.1.1），而它按目录找。
    /// 用临时目录是因为真实的那三个在开发机上根本不存在——两支都要能钉住，
    /// 回执那句话才敢接这个 bool 当输入。
    #[test]
    fn geo_data_counts_only_when_both_files_share_one_directory() {
        let base = std::env::temp_dir().join(format!(
            "zbox-geo-{}-{}",
            std::process::id(),
            "both-in-one-dir"
        ));
        let _ = std::fs::remove_dir_all(&base);
        let first = base.join("first");
        let second = base.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let dirs = [first.display().to_string(), second.display().to_string()];
        let present = || geo_data_present_in(&dirs.iter().map(String::as_str).collect::<Vec<_>>());

        assert!(!present(), "两个空目录不该算在位");
        std::fs::write(first.join("geoip.dat"), b"geo").unwrap();
        assert!(!present(), "只有一半不该算在位");
        // 另一半在**另一个**候选目录里：dae 不会去第二个目录找补，这一半等于没有
        std::fs::write(second.join("geosite.dat"), b"geo").unwrap();
        assert!(!present(), "两份分居两个目录仍然等于缺");
        std::fs::write(first.join("geosite.dat"), b"geo").unwrap();
        assert!(present(), "同一目录里两份都在才算在位");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 目录名撞上同名**文件**时不许算在位（`is_dir` 与 `is_file` 的差别在这台机器上是真实的：
    /// `/usr/share/dae` 存在但是空目录，是开发机上最常见的"看着像在"）。
    #[test]
    fn an_empty_geo_data_directory_is_not_present() {
        let base = std::env::temp_dir().join(format!(
            "zbox-geo-{}-{}",
            std::process::id(),
            "empty-dir"
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert!(
            !geo_data_present_in(&[base.display().to_string().as_str()]),
            "空目录=缺，不许报成在位"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn every_reading_degrades_to_none_independently() {
        // 一条都跑不了（开发机上没有 ss/nft/ip）：整份读数是空，而不是一屏假故障
        let runner = Scripted::with_results(vec![Err("没有这个命令".to_string())]);
        let observer = Observer::new(&runner, InitSystem::OpenRC);
        assert_eq!(observer.read(), Reading::default());
    }

    #[test]
    fn a_full_guest_shape_produces_a_full_reading() {
        let runner = Scripted::with_results(vec![
            ok(""),                   // 服务状态
            ok(SS),                   // ss -lntu
            ok("table ip zbox {\n"),  // nft list table ip zbox
            ok(IP_RULE),              // ip -4 rule show
            ok("1\n"),                // ip_forward
            ok("2\n"),                // rp_filter
            ok(DEFAULT_ROUTE),        // ip -4 route show default
            ok(SS6),                  // ss -lntu -6
            ok("table ip6 zbox {\n"), // nft list table ip6 zbox
            ok(IP6_RULE),             // ip -6 rule show
            ok("1\n"),                // net.ipv6.conf.all.forwarding
            ok("2\n"),                // net.ipv6.conf.all.accept_ra
        ]);
        let observer = Observer::new(&runner, InitSystem::OpenRC);
        let got = observer.read();
        assert_eq!(
            got,
            Reading {
                service_active: Some(true),
                listening: Some(vec![22, 53, 7890, 8888, 9887, 9888]),
                nft_table_loaded: Some(true),
                divert_rule_loaded: Some(true),
                ip_forward: Some(1),
                rp_filter_all: Some(2),
                default_route_iface: Some("eth0".into()),
                // 双栈那只口（`*:19889`）和 V6ONLY 那只（`[::]:22`）都要算进 v6 侧
                v6_listening: Some(vec![22, 19889]),
                v6_table_loaded: Some(true),
                v6_divert_rule_loaded: Some(true),
                v6_forward_all: Some(1),
                v6_accept_ra_all: Some(2),
            }
        );
        // 十二项各跑一条命令，不多问
        assert_eq!(runner.argv().len(), 12, "{:?}", runner.argv());
        // 三条 v6 探针**必须带各自的族参数**：少了 `-6` 读到的就是整份监听表，
        // v6 侧永远“在听”；少了 `ip6` 读的是 v4 那张表，两族独立这件事实测过。
        for probe in [
            vec!["ss", "-lntu", "-6"],
            vec!["nft", "list", "table", "ip6", "zbox"],
            vec!["ip", "-6", "rule", "show"],
        ] {
            assert!(
                runner.argv().iter().any(|argv| argv == &probe.to_vec()),
                "少了这一条探针：{probe:?}｜实际 {:?}",
                runner.argv()
            );
        }
    }

    /// v6 的 hijack 层没加载时的读数：表不在是结论（rc=1），`ip -6 rule` 只有出厂那两行。
    #[test]
    fn the_clean_ipv6_baseline_reads_as_real_negatives() {
        let runner = Scripted::with_results(vec![
            ok(""),
            ok(SS),
            nft_table_missing(),
            ok("0:\tfrom all lookup local\n32766:\tfrom all lookup main\n32767:\tfrom all lookup default\n"),
            ok("1\n"),
            ok("2\n"),
            ok(DEFAULT_ROUTE),
            ok(SS6),
            nft_table_missing(),
            // 出厂基线：两行。别照 v4 的三行判，那会永远报一条不存在的缺失。
            ok("0:\tfrom all lookup local\n32766:\tfrom all lookup main\n"),
            ok("0\n"),
            ok("1\n"),
        ]);
        let got = Observer::new(&runner, InitSystem::OpenRC).read();
        assert_eq!(got.v6_table_loaded, Some(false));
        assert_eq!(got.v6_divert_rule_loaded, Some(false));
        assert_eq!(got.v6_forward_all, Some(0));
        assert_eq!(got.v6_accept_ra_all, Some(1));
        // v6 侧不是“一个口都没有”：sshd 与双栈口都在这份读数里
        assert_eq!(got.v6_listening, Some(vec![22, 19889]));
    }

    #[test]
    fn a_missing_nft_table_is_a_real_negative_not_a_read_failure() {
        // 实测原文：Error: No such file or directory + rc=1
        let runner = Scripted::with_results(vec![
            ok(""),
            ok(SS),
            nft_table_missing(),
            ok("0:\tfrom all lookup local\n32766:\tfrom all lookup main\n"),
            ok("1\n"),
            ok("2\n"),
            ok(DEFAULT_ROUTE),
        ]);
        let got = Observer::new(&runner, InitSystem::OpenRC).read();
        assert_eq!(
            got.nft_table_loaded,
            Some(false),
            "表不在是结论，不是没读到"
        );
        assert_eq!(got.divert_rule_loaded, Some(false));
    }

    #[test]
    fn readings_feed_the_doctor_without_losing_the_severity_rule() {
        use crate::doctor;
        use crate::gen::init::Layout;
        let mut state = DesiredState::baseline();
        state.network.lan_interface = "zbox-lan0".into();
        // 期望全直连（基线），现场也没加载劫持层 -> 只该有提示级
        let runner = Scripted::with_results(vec![
            ok(""),
            ok(SS),
            nft_table_missing(),
            ok("0:\tfrom all lookup local\n32766:\tfrom all lookup main\n"),
            ok("1\n"),
            ok("2\n"),
            ok(DEFAULT_ROUTE),
        ]);
        let reading = Observer::new(&runner, InitSystem::OpenRC).read();
        let findings = doctor::diagnose(&state, &reading, &Layout::default());
        assert!(
            findings.iter().all(|f| !f.is_fatal()),
            "基线下的缺劫持层不该报致命：{findings:?}"
        );
    }

    fn ok(body: &str) -> Result<Exec, String> {
        Ok(Exec {
            code: 0,
            stdout: body.to_string(),
            stderr: String::new(),
        })
    }

    fn err(code: i32, body: &str) -> Result<Exec, String> {
        Ok(Exec {
            code,
            stdout: String::new(),
            stderr: body.to_string(),
        })
    }

    fn nft_table_missing() -> Result<Exec, String> {
        err(
            1,
            "Error: No such file or directory\nlist table ip zbox\n              ^^^^^^^^^^^\n",
        )
    }
}
