//! TPROXY 依赖的主机侧网络状态：策略路由 + sysctl。
//!
//! 和 `gen::nft` 一样，这部分**不在 sing-box 官方文档里**，属于 ZBox 自证模板。
//! 缺了它，劫持层只成了一半：实测（`docs/reference/ipv6-kernel-measured.md` §8）包照样被
//! `tproxy` 收进透明套接字，但握手的回程出不来，客户端只见超时、上游一条连接日志都不出——
//! 界面看起来"配置下发成功"，行为与"什么都没代理"一模一样。
//!
//! 幂等策略：`ip rule` 没有 `replace` 语义，重复 add 会堆出多条相同规则，
//! 所以一律先 `del`（忽略不存在）再 `add`；路由用 `replace`。
//!
//! IPv6 那一半边只在 `ipv6_mode = dual_stack` 时生成，形状与 v4 逐条对应
//! （v6 是另一套路由数据库，同一个表号 100 与同一个优先级不会与 v4 相撞，
//! `ip -6 rule` / `ip -6 route … local default dev lo table 100` 的实测见
//! `docs/reference/ipv6-kernel-measured.md` §4；同一节还记了 v6 的规则基线只有
//! `0:`/`32766:` 两行，比 v4 少一行）。sysctl 那几条则是实测出来的**下限**，见
//! `docs/reference/ipv6-kernel-measured.md` §1：只写转发不写 `accept_ra=2` 的那一臂里，
//! 本机自己的 v6 默认路由整臂消失、ping6 全 fail、15 分钟无自愈。

use crate::gen::nft;
use crate::gen::singbox::route::DIVERT_MARK;
use crate::model::config::{DesiredState, Ipv6Mode};

/// TPROXY 用的本地路由表号。写死 100：不与发行版的 main/local 冲突，
/// 也留出 101+ 给排查时手工加规则。v4 与 v6 共用这个号，因为它们读的是
/// 两套独立的路由数据库（实测见文件头）。
pub const DIVERT_TABLE: u8 = 100;
/// `ip rule` 的优先级，必须小于 main 表的 32766
pub const DIVERT_PRIORITY: u32 = 100;

pub const APPLY_PATH: &str = "netstate.sh";
pub const TEARDOWN_PATH: &str = "netstate-down.sh";

/// 整机级的内核键，不含任何网卡名。持久化文件只用这一份（见 `gen::init::sysctl_conf`）。
const HOST_KEYS: [(&str, &str); 4] = [
    ("net.ipv4.ip_forward", "1"),
    ("net.ipv4.conf.all.rp_filter", "2"),
    ("net.ipv4.conf.default.rp_filter", "2"),
    ("net.ipv4.conf.all.send_redirects", "0"),
];

/// 接管 v6 连接时要补的整机级键。`all.forwarding` 与 `all.accept_ra` 是一对，
/// 拆开写就是实测里那个"v6 自己断网、v4 看起来一切正常"的形状。
const V6_HOST_KEYS: [(&str, &str); 2] = [
    ("net.ipv6.conf.all.forwarding", "1"),
    ("net.ipv6.conf.all.accept_ra", "2"),
];

/// 修“这一档要中继 v6 却开着 accept_ra 的默认姿态”时成对给的两条键。
/// doctor 的两条 v6 sysctl 判据都指到这里，所以这一份必须与 [`host_sysctl_pairs`]
/// 里追加的那两条逐字相同——漂移过的表现是"诊断让人写的值，生成物里根本不是那个值"。
pub fn v6_host_pairs() -> Vec<(String, String)> {
    V6_HOST_KEYS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// 只给整机级的键。
pub fn host_sysctl_pairs(mode: Ipv6Mode) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = HOST_KEYS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    if mode.takes_connection() {
        pairs.extend(
            V6_HOST_KEYS
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string())),
        );
    }
    pairs
}

/// 修“转发开着但本机不再采纳 RA”这一对时要写的键。
///
/// 两份都写是照实测臂 C 抄的形状（那一臂四个键全写过，所以不做“其实只写 `all`
/// 那份也够”这种没证过的化简）。网卡名为空时只给 `all` 那一条——
/// `net.ipv6.conf..accept_ra` 是个不存在的键，写上去 busybox `sysctl -w` 实测 rc=1。
pub fn accept_ra_pairs(lan_interface: &str) -> Vec<(String, String)> {
    let mut pairs = vec![("net.ipv6.conf.all.accept_ra".to_string(), "2".to_string())];
    if !lan_interface.trim().is_empty() {
        pairs.push((
            format!("net.ipv6.conf.{lan_interface}.accept_ra"),
            "2".to_string(),
        ));
    }
    pairs
}

/// 转发相关内核键。同一份列表既生成下发脚本，也生成 `/etc/sysctl.d/` 持久化文件
/// （`gen::init::sysctl_conf`），因为两者漂移过一次的表现是"当场生效、重启就失效"。
///
/// 持久化文件只取 [`host_sysctl_pairs`]：绑网卡的那条不进那个文件。开机时它是
/// `sysctl -p` 读的，而 Alpine 的 `/sbin/sysctl` 就是 busybox 软链，它的 `-p` 对不认识的键
/// **只打一行 stderr 然后 rc=0**（2026-09-30 实测），网卡名一旦对不上就等于静默少一条。
/// 脚本那侧用 `-w`，同样的键实测 rc=1，是响的。
///
/// `rp_filter` 一律放宽到宽松模式（2），但**这条是保险而不是实测结论**：2026-09-30 的 netns
/// 实测里局域网与 WAN 分属两块网卡，压回严格模式 1 也没丢包（TPROXY 照常命中）。社区通行说法
/// 针对的是单网卡旁路由（进出同网卡、local 路由被判 spoofed），那个形状只能在真机上证。
/// 放宽是无害方向，所以模板继续写 2。详见
/// `docs/reference/singbox-1.14.2-schema.md` §11.7。
///
/// v6 那两条绑网卡的键（`<网卡>.forwarding` / `<网卡>.accept_ra`）是按实测臂 C 原样抄的：
/// 那一臂四个键全写过，所以这里不做"其实只写 all 那份也够"这种没证过的化简。
/// v6 没有 `rp_filter` 的对应键，别照着 v4 那三条去凑。
pub fn sysctl_pairs(state: &DesiredState) -> Vec<(String, String)> {
    let mut pairs = host_sysctl_pairs(state.network.ipv6_mode);
    let lan = &state.network.lan_interface;
    pairs.push((format!("net.ipv4.conf.{lan}.rp_filter"), "2".into()));
    // 这条不能只写 `all` 那份：内核按接口读 `send_redirects`，`all` 不覆盖它。
    // 实测的现场是 `net.ipv4.conf.all.send_redirects=0` 而 `eth0` 仍是 1（guest 2026-10-11），
    // 而 dae 的内核参数门就是按接口读这一条并报 FATAL 退出
    // （`docs/reference/ipv6-kernel-measured.md` §10），所以只写 `all` 等于组件起不来。
    pairs.push((format!("net.ipv4.conf.{lan}.send_redirects"), "0".into()));
    if state.network.ipv6_mode.takes_connection() {
        pairs.push((format!("net.ipv6.conf.{lan}.forwarding"), "1".into()));
        pairs.push((format!("net.ipv6.conf.{lan}.accept_ra"), "2".into()));
    }
    pairs
}

/// 下发前执行：开启转发、放宽反向路径过滤、建立 divert 策略路由
pub fn render_apply(state: &DesiredState) -> String {
    let mode = state.network.ipv6_mode;
    let mut out = script_header();
    out.push_str("\n# 转发与反向路径过滤：键值取自 sysctl_pairs，持久化文件是它的整机级子集。\n");
    for (key, value) in sysctl_pairs(state) {
        // 整个参数一起引号包住：网卡名带空格或引号时也不能被 shell 拆开
        out.push_str(&format!(
            "sysctl -q -w {}\n",
            shell_quote(&format!("{key}={value}"))
        ));
    }
    out.push_str(&format!(
        "\n# tproxy 收下连接之后，带 divert 标记的包要落进本地路由表，回程才出得来。\n\
         # 实测缺这两行的现场：包照样被收进透明套接字，但客户端只看到超时，上游一条连接\n\
         # 日志都不出（docs/reference/ipv6-kernel-measured.md §8）。\n\
         ip -4 rule del fwmark 0x{DIVERT_MARK:x} table {DIVERT_TABLE} priority {DIVERT_PRIORITY} 2>/dev/null || true\n\
         ip -4 rule add fwmark 0x{DIVERT_MARK:x} table {DIVERT_TABLE} priority {DIVERT_PRIORITY}\n\
         ip -4 route replace local default dev lo table {DIVERT_TABLE}\n",
    ));
    if mode.takes_connection() {
        // v6 是另一套路由数据库，同一表号同一优先级，先删后加的理由与 v4 一模一样
        out.push_str(&format!(
            "ip -6 rule del fwmark 0x{DIVERT_MARK:x} table {DIVERT_TABLE} priority {DIVERT_PRIORITY} 2>/dev/null || true\n\
             ip -6 rule add fwmark 0x{DIVERT_MARK:x} table {DIVERT_TABLE} priority {DIVERT_PRIORITY}\n\
             ip -6 route replace local default dev lo table {DIVERT_TABLE}\n"
        ));
    } else if mode == Ipv6Mode::Off {
        // 上一档（只接管 DNS / 双栈）留下的 ip6 表在这份 zbox.nft 里根本不会出现，
        // 留着就是一条没人认领的死链：规则还在内核里跑，但已经没有任何一档说它该在。
        // 只在退回 off 时删——dual_stack 档里 zbox.nft 自己 flush 重建，不需要这里腾位置，
        // 而在这里删会给正常下发开一个 v6 直连的空窗。
        out.push_str(&format!(
            "nft delete table ip6 {} 2>/dev/null || true\n",
            nft::TABLE
        ));
    }
    out
}

/// 关闭劫持 / 卸载时执行。只删 ZBox 自己建的东西，不碰发行版自带的规则。
pub fn render_teardown() -> String {
    let mut out = script_header();
    out.push_str(&format!(
        "\nip -4 rule del fwmark 0x{DIVERT_MARK:x} table {DIVERT_TABLE} priority {DIVERT_PRIORITY} 2>/dev/null || true\n\
         ip -4 route flush table {DIVERT_TABLE} 2>/dev/null || true\n\
         nft delete table ip {} 2>/dev/null || true\n",
        nft::TABLE
    ));
    // v6 那半边无条件删：这一档是不是接管过 v6 由 state 决定，但"退出的时候把话说干净"
    // 不该取决于进来时的姿态。两族各有路由数据库，所以 table 100 要各 flush 一次。
    out.push_str(&format!(
        "ip -6 rule del fwmark 0x{DIVERT_MARK:x} table {DIVERT_TABLE} priority {DIVERT_PRIORITY} 2>/dev/null || true\n\
         ip -6 route flush table {DIVERT_TABLE} 2>/dev/null || true\n\
         nft delete table ip6 {table} 2>/dev/null || true\n\
         echo 'ZBox 劫持层已卸载。局域网设备的网关/DNS 若还指向本机，请改回主路由，否则直接断网。'\n",
        table = nft::TABLE
    ));
    out
}

fn script_header() -> String {
    "#!/bin/sh\n# ZBox 生成，请勿手改：改动会在下一次下发时被覆盖。\nset -eu\n".to_string()
}

/// 网卡名可能带连字符（shell 里没问题）或空格/引号，用单引号包住最稳。
/// `gen::doctor` 的诊断命令也用同一个函数，避免两处转义规则漂移。
pub(crate) fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::nft;

    fn state() -> DesiredState {
        DesiredState {
            ..DesiredState::baseline()
        }
    }

    /// 三档各自的 state。`lan_cidr6` 在这里填上是 validate() 的要求，
    /// 但这一模块的生成不读它（读它的是 nft 那条 `ip6 saddr`）。
    fn state_with(mode: Ipv6Mode) -> DesiredState {
        let mut s = state();
        s.network.ipv6_mode = mode;
        s.network.lan_cidr6 = "fd00:db8:1::/64".into();
        s
    }

    /// doctor 的 fix 用的是 [`v6_host_pairs`]，生成物用的是 [`host_sysctl_pairs`]。
    /// 两处漂开的表现是"诊断让人写的值与生成物里的值不是同一对键"，所以钉住。
    #[test]
    fn the_doctor_fix_and_the_generated_pairs_are_the_same_two_keys() {
        assert_eq!(
            v6_host_pairs(),
            vec![
                (
                    String::from("net.ipv6.conf.all.forwarding"),
                    String::from("1")
                ),
                (
                    String::from("net.ipv6.conf.all.accept_ra"),
                    String::from("2")
                ),
            ],
            "这两条是实测臂 C 的下限，增删都要先过实测"
        );
        let dual = host_sysctl_pairs(Ipv6Mode::DualStack);
        assert_eq!(&dual[dual.len() - 2..], &v6_host_pairs()[..]);
        for pair in v6_host_pairs() {
            assert!(
                !host_sysctl_pairs(Ipv6Mode::Off).contains(&pair),
                "不接管那一档不该写 v6 中继键：{pair:?}"
            );
        }
    }

    #[test]
    fn divert_mark_and_table_are_shared_with_the_nft_layer() {
        let script = render_apply(&state());
        assert!(script.contains(&format!("fwmark 0x{DIVERT_MARK:x} table {DIVERT_TABLE}")));
        assert!(script.contains("route replace local default dev lo table 100"));
        assert!(nft::render(&state()).contains(&format!("meta mark set 0x{DIVERT_MARK:x} accept")));
    }

    #[test]
    fn rp_filter_is_loose_as_a_precaution() {
        let script = render_apply(&state());
        assert!(script.contains("net.ipv4.conf.all.rp_filter=2"));
        assert!(script.contains("net.ipv4.conf.ens33.rp_filter=2"));
        assert!(script.contains("net.ipv4.ip_forward=1"));
        // 旁路由不接管 DHCP，也不做 PMTU 全局禁用那种大刀
        assert!(!script.contains("dhcp"), "{script}");
        assert!(!script.contains("pmtu"), "{script}");
    }

    /// `send_redirects` 要**按接口**写一份：内核读这一条看的是网卡自己的值，`all` 不覆盖它
    /// （guest 2026-10-11：`all=0` 时 `eth0` 仍是 1），而 dae 的内核参数门正是按接口读它，
    /// 读到了就 FATAL 退出（`docs/reference/ipv6-kernel-measured.md` §10）。
    /// 三档都写，因为这条与 IPv6 接管姿态无关——dae 在哪一档都要起来。
    #[test]
    fn the_send_redirects_gate_is_closed_on_the_interface_not_only_on_all() {
        for mode in [Ipv6Mode::Off, Ipv6Mode::DnsOnly, Ipv6Mode::DualStack] {
            let script = render_apply(&state_with(mode));
            assert!(
                script.contains("net.ipv4.conf.ens33.send_redirects=0"),
                "{mode:?}：只写 all 那份等于 dae 起不来：\n{script}"
            );
        }
        // 绑网卡的键一律不进 /etc/sysctl.d 那份持久化文件（开机 `sysctl -p` 认不出网卡名时
        // 只打一行 stderr 然后 rc=0，等于静默少一条）
        assert!(
            !host_sysctl_pairs(Ipv6Mode::DualStack)
                .iter()
                .any(|(key, _)| key.contains("ens33")),
            "整机级那份里出现了网卡名"
        );
    }

    #[test]
    fn every_declared_sysctl_pair_is_actually_applied() {
        let s = state_with(Ipv6Mode::DualStack);
        let script = render_apply(&s);
        assert_eq!(
            script.matches("sysctl -q -w").count(),
            sysctl_pairs(&s).len(),
            "脚本里的 sysctl 行必须和 sysctl_pairs 一一对应，不多写也不漏写"
        );
    }

    #[test]
    fn apply_is_repeatable_and_teardown_removes_only_our_own_state() {
        let script = render_apply(&state());
        assert_eq!(script.matches("ip -4 rule").count(), 2, "必须先删后加");
        let down = render_teardown();
        // set -eu 之下，任何一步都可能因为"本来就没建过"而失败，必须逐条容忍
        assert!(down.contains("nft delete table ip zbox 2>/dev/null || true"));
        // 绝不 flush 整张 main 表或删默认路由，那会把机器自己踢下线
        assert!(!down.contains("ip -4 rule flush"), "{down}");
        assert!(!down.contains("route del default"), "{down}");
    }

    #[test]
    fn scripts_are_posix_and_quote_the_interface_name() {
        for script in [render_apply(&state()), render_teardown()] {
            assert!(script.starts_with("#!/bin/sh\n"), "{script}");
            assert!(script.contains("set -eu"));
        }
        let mut weird = state();
        weird.network.lan_interface = "we'ird".into();
        assert!(render_apply(&weird).contains(r"'net.ipv4.conf.we'\''ird.rp_filter=2'"));
    }

    /// 转发与 accept_ra 是一对：实测里只写转发的臂 A，本机自己的 v6 默认路由整臂消失、
    /// 15 分钟无自愈（docs/reference/ipv6-kernel-measured.md §1）。这一条断言防的就是
    /// "以后有人觉得 accept_ra 多余而删掉一行"。
    #[test]
    fn enabling_v6_forwarding_always_comes_with_accept_ra_two() {
        let script = render_apply(&state_with(Ipv6Mode::DualStack));
        assert!(
            script.contains("net.ipv6.conf.all.forwarding=1"),
            "{script}"
        );
        assert!(script.contains("net.ipv6.conf.all.accept_ra=2"), "{script}");
        assert!(
            script.contains("net.ipv6.conf.ens33.accept_ra=2"),
            "绑网卡那两份也要写：实测的臂 C 就是四条全写，不做没证过的化简"
        );
        assert!(
            script.contains("net.ipv6.conf.ens33.forwarding=1"),
            "{script}"
        );
        // default.accept_ra 不碰：它只管新出现的网卡，写它等于把"以后插上的每一张卡"
        // 都悄悄改成中继姿态
        assert!(
            !script.contains("net.ipv6.conf.default.accept_ra"),
            "{script}"
        );
        assert_eq!(
            script.matches("net.ipv6.conf.all.forwarding").count(),
            1,
            "同一个键写两遍是两份真相"
        );
    }

    #[test]
    fn v6_policy_route_appears_only_when_the_connection_layer_is_taken() {
        // dns_only：收 v6 的 DNS 查询（nft 的 ip6 nat 链），但连接层不接管，
        // 所以既不该有 ip -6 rule，也不该有 v6 转发。
        let dns_only = render_apply(&state_with(Ipv6Mode::DnsOnly));
        assert!(!dns_only.contains("ip -6 rule"), "{dns_only}");
        assert!(!dns_only.contains("net.ipv6.conf"), "{dns_only}");
        assert!(dns_only.contains("ip -4 rule add"), "{dns_only}");

        let off = render_apply(&state());
        assert!(!off.contains("ip -6"), "{off}");
        assert!(!off.contains("net.ipv6"), "{off}");

        let dual = render_apply(&state_with(Ipv6Mode::DualStack));
        assert_eq!(dual.matches("ip -6 rule").count(), 2, "先删后加");
        assert!(
            dual.contains("ip -6 route replace local default dev lo table 100"),
            "{dual}"
        );
        // 两族共用表号与优先级（各自的数据库，实测不冲突）
        assert!(
            dual.contains("ip -6 rule add fwmark 0x1 table 100 priority 100"),
            "{dual}"
        );
    }

    #[test]
    fn downgrading_to_off_removes_the_orphan_ip6_table_but_a_normal_reload_does_not() {
        // 退回 off 时这一份 zbox.nft 里没有 ip6 表，上一档留下的那份必须删掉
        let off = render_apply(&state());
        assert!(
            off.contains("nft delete table ip6 zbox 2>/dev/null || true"),
            "{off}"
        );
        // 而 dual_stack 档里不能删：zbox.nft 自己 flush 重建，在这里删等于给每次下发
        // 开一个"v6 流量不经劫持直连出去"的空窗
        for mode in [Ipv6Mode::DnsOnly, Ipv6Mode::DualStack] {
            let script = render_apply(&state_with(mode));
            assert!(!script.contains("delete table ip6"), "{mode:?}：{script}");
        }
        // 卸载那一份无条件把两族都收干净，不看档位
        let down = render_teardown();
        assert!(
            down.contains("nft delete table ip6 zbox 2>/dev/null || true"),
            "{down}"
        );
        assert_eq!(down.matches("ip -6 rule").count(), 1, "{down}");
        assert_eq!(down.matches("ip -6 route flush").count(), 1, "{down}");
    }
}
