//! 看板（`docs/BLUEPRINT.md` §11.3 B）的**硬件占用**那一格。
//!
//! 这一格的取向与 [`super::observe`] 相反，而且必须是相反的：`observe` 发命令读现场，
//! 这里**一条命令都不发**，只用 `std::fs` 读 `/proc` 那四份。三条理由：
//!
//! 1. 判据要的口径是"每个数字都说得出取数口"，`/proc/meminfo` 这类文件的名字本身就是取数口，
//!    对用户可核对（`top`、`free -m` 读的是同一处）；
//! 2. 发命令就要选 `df` 还是 `free`、要处理措辞随版本变，而这一格在路由器上会被反复刷新；
//! 3. 只读端点不发命令是可以断言的（`runner.argv()` 恒空），发了就钉不住。
//!
//! 每个数单独降级为 `None`（约定与 `observe` 同一条：**读不到 ≠ 不合规**，面板上就是「未读到」）。
//! 这台机器上没有 `/proc`（Windows 开发机）时四份全空，那是这台机器的形状，不是这一屏坏了。
//!
//! 磁盘**故意不给读数**：`statvfs` 不在 std，读它就得发一条 `df -k`，与上面第 1、3 条冲突。
//! §11.2 那格的许可是"`df -k` 或明写未读取，不猜"——这里选后者，措辞住在
//! [`DISK_NOT_READ`]，面板与将来别的屏共用一份，不各写一遍。
//!
//! 下面每一段 fixture 都是 **2026-10-12 在 Alpine 3.21.3 guest 上逐字抓下来的**
//! （`head -n 5 /proc/meminfo`、`cat /proc/loadavg`、`cat /proc/uptime`、`cat /proc/net/dev`），
//! 不是照 proc(5) 想象的格式。唯一一支想象的输入是「读不到」那一类（空串、缺字段），
//! 因为那正是这台机器与任何一份解析器都可能给出来的形状。

use std::path::Path;

/// 真实机器上的 `/proc`。测试走 [`readings_in`]，不改这个常量。
pub const PROC_ROOT: &str = "/proc";

/// 磁盘那一格的面板措辞。**没有读**，所以这里是一句解释而不是读数。
///
/// 这一句会原样出现在屏幕上，所以它是写给人看的：不带函数名、不带反引号，
/// 而最后那半句是这一格存在的理由——空着的格子读起来像"0"。
pub const DISK_NOT_READ: &str = "这一屏没有读磁盘占用。要读它得在这台机器上跑一条命令，\
                                  而这一屏的约定是一条命令都不发——所以这一格是空的，不是 0。";

/// 「分流引擎 dae」那一格的面板措辞。**读不到**，所以这里是一句解释而不是读数。
///
/// 三条理由都实测过，缺一条这句就成了一句推卸：
/// 1. dae **没有任何可查的口**——它的控制面是进程内对象，那个 listener 只在它自建的 `daens`
///    网络命名空间里绑 tproxy 口，宿主 ns 够不着（`docs/reference/dae-config-dictionary.md` 开头、
///    `dae-d1-guest-measurement.md` §63）；
/// 2. 这一屏的约定是一条命令都不发，所以不能靠 `dae version` 新起一个进程去问；
/// 3. ZBox 现在**也还没把它装成服务**（`gen::init` 那份 unit 里只有 sing-box，二进制落点还没进
///    `Layout`）——所以连"该在哪台机器上的哪个位置"都还没有一个说得出口的权威答案。
///
/// 最后那半句是这一格的界：**读不到不等于没在跑**。
pub const DAE_NOT_READ: &str = "这一格读不到分流引擎（dae）：它没有可以问话的口，而这一屏一条命令都\
                                 不发——要问它在不在，只能在这台机器上新起一个它的进程。ZBox 现在也还\
                                 没把它装成服务。这一格空着说的是「读不到」，不是「没在跑」。";

/// 「主路由」那一格的面板措辞。**没接通**，同样是一句解释。
///
/// 与上面那条不同，这里不是取数口的问题，是**这一版还没做**：与主路由对话要 RouterOS 那个对接面
/// （`docs/BLUEPRINT.md` §11.5 排在后面的分期），所以这一格一直是空的。写死成一句人话是为了
/// 不让人以为"这一格坏了"。
pub const ROUTER_NOT_READ: &str = "这一格还没有接通主路由：ZBox 现在还没跟它说过话（那一条要等主路由\
                                    系统的对接面做好，排在后面）。所以这一格一直是空的，说的是还没接上，\
                                    不是主路由没在跑。";

/// 一块网卡的累计收发字节（`/proc/net/dev` 的一行）。
///
/// **是"这块网卡过了多少字节"，不是"代理了多少"**——§11.2 对这张卡的措辞判定就是这条，
/// 从 #69/G4 起连接层给不出后者，所以这里只搬运计数，不做任何归属判断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetDev {
    pub iface: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// 看板硬件那一格的全部读数。全字段可空 = 全字段可「未读到」。
///
/// 没有 `Eq`：`load` 是 `f64`（loadavg 本来就是小数），浮点等式在这里没有意义，也不许有意义。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct BoardReadings {
    /// `/proc/meminfo` 的 `MemTotal`，单位 kB（内核写的是 KiB，这里原样保留它自己的单位）。
    pub mem_total_kb: Option<u64>,
    /// 同上的 `MemAvailable`。缺这个字段的老内核就是 `None`，不拿 `MemFree` 顶——
    /// 两者差着 buffers/cache，顶了就是一个说不清口径的百分比。
    pub mem_available_kb: Option<u64>,
    /// `/proc/loadavg` 的三个滑动平均。
    pub load: Option<[f64; 3]>,
    /// `/proc/uptime` 的第一个字段（秒，小数部分截掉；第二个字段是全部核心的空闲时间，这里用不到）。
    pub uptime_secs: Option<u64>,
    /// `/proc/net/dev` 逐行（回环与表头之外的那些）。
    pub netdev: Vec<NetDev>,
}

impl BoardReadings {
    /// 已用占总量百分比，向下取整。任一头读不到就是 `None`——面板上「未读到」，
    /// 不画一根长度为 0 的条（那是"没用多少"，与"没读到"是两句话）。
    pub fn used_percent(&self) -> Option<u8> {
        match (self.mem_total_kb, self.mem_available_kb) {
            (Some(total), Some(avail)) if total > 0 => {
                Some(((total.saturating_sub(avail)) * 100 / total) as u8)
            }
            _ => None,
        }
    }

    pub fn used_kb(&self) -> Option<u64> {
        Some(self.mem_total_kb?.saturating_sub(self.mem_available_kb?))
    }
}

/// 真机器那一份。开发机上没有 `/proc` 时它给全 `None`，这是预期行为。
pub fn readings() -> BoardReadings {
    readings_in(Path::new(PROC_ROOT))
}

/// [`readings`] 的根目录可注入版。与 [`super::geo_data_present_in`] 同一个理由：
/// 真实的 `/proc` 在开发机上不存在，两支都要能钉住，判据才只许有一个版本。
///
/// 一个文件只读一次（`MemTotal` 与 `MemAvailable` 共用那一遍），读不动就是那一个字段空着——
/// 目录整个不在（Windows 开发机）时四份全 `None`，这是这台机器的形状，不是这一屏坏了。
pub fn readings_in(root: &Path) -> BoardReadings {
    let text = |name: &str| std::fs::read_to_string(root.join(name)).ok();
    let meminfo = text("meminfo");
    let loadavg = text("loadavg");
    let uptime = text("uptime");
    let netdev = text("net/dev");
    BoardReadings {
        mem_total_kb: meminfo.as_deref().and_then(parse_mem_total),
        mem_available_kb: meminfo.as_deref().and_then(parse_mem_available),
        load: loadavg.as_deref().and_then(parse_loadavg),
        uptime_secs: uptime.as_deref().and_then(parse_uptime_secs),
        netdev: netdev.as_deref().map(parse_netdev).unwrap_or_default(),
    }
}

fn meminfo_field(out: &str, key: &str) -> Option<u64> {
    out.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?.strip_prefix(':')?;
        let first = rest.split_whitespace().next()?;
        first.parse::<u64>().ok()
    })
}

/// `MemTotal:         694832 kB`
pub fn parse_mem_total(out: &str) -> Option<u64> {
    meminfo_field(out, "MemTotal")
}

/// `MemAvailable:     419312 kB`
pub fn parse_mem_available(out: &str) -> Option<u64> {
    meminfo_field(out, "MemAvailable")
}

/// `0.15 0.10 0.03 1/211 78217`：前三段是读数，后面那两段（ runnable 进程数、最后一个 pid）
/// 不是负载，多读一个就把这一格说成别的东西了，所以**只取三个**。
pub fn parse_loadavg(out: &str) -> Option<[f64; 3]> {
    let got: Vec<f64> = out
        .split_whitespace()
        .take(3)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if got.len() == 3 {
        Some([got[0], got[1], got[2]])
    } else {
        None
    }
}

/// `163051.40 324494.79` → 第一个字段取整秒。
pub fn parse_uptime_secs(out: &str) -> Option<u64> {
    let first = out.split_whitespace().next()?;
    // "163051.40" → "163051"：截断而不是四舍五入，运行时长宁少报半秒。
    first.split('.').next()?.parse::<u64>().ok()
}

/// `/proc/net/dev`：两行表头，之后每行 `  eth0: 1721523045 …`。
///
/// 收的 `bytes` 在第 0 列，发的在第 8 列（收那一侧有 bytes/packets/errs/drop/fifo/frame/
/// compressed/multicast 八列，实测 guest 那行的列数与此一致）。
/// 列数不够就跳过这一行——内核或别的实现改了表，宁可少给一块网卡。
pub fn parse_netdev(out: &str) -> Vec<NetDev> {
    out.lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let iface = name.trim();
            // 表头那两行：`Inter-|   Receive …` 的"冒号后"根本不是数字，
            // 而 `lo` 是回环——它过的是本机自己的字节，放进"这块网卡过了多少"里会误导。
            if iface.is_empty() || iface == "lo" {
                return None;
            }
            let cols: Vec<&str> = rest.split_whitespace().collect();
            if cols.len() < 9 {
                return None;
            }
            Some(NetDev {
                iface: iface.to_string(),
                rx_bytes: cols[0].parse().ok()?,
                tx_bytes: cols[8].parse().ok()?,
            })
        })
        .collect()
}

/// 二进制单位的字节数怎么说：小于 1 KB 给整数 B，往上给一位小数（`1.6 GB`，不是 `1 GB`）。
///
/// 单位名用 `KB/MB/GB/TB`，**进制是 1024**（`free`、`/proc/net/dev` 一路的口径），这一条由
/// `the_human_units` 那支测试钉住；将来要换成 `MiB` 只许改这一处。
/// 那位小数是**四舍五入**的，所以升档的门槛用 `1023.95` 而不是 `1024`——正好等于 1024 时
/// 印出来就是 `1024.0 KB` 那种怪话，越过 1023.95 就先升一级单位。
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while unit < UNITS.len() - 1 && value >= 1023.95 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{:.1} {}", value, UNITS[unit])
}

/// 运行时长怎么说：只给到"天/小时/分钟"，秒不留（看板不是秒表）。
pub fn human_duration(secs: u64) -> String {
    let days = secs / 86400;
    let hours = secs % 86400 / 3600;
    let mins = secs % 3600 / 60;
    if days > 0 {
        format!("{} 天 {} 小时", days, hours)
    } else if hours > 0 {
        format!("{} 小时 {} 分钟", hours, mins)
    } else {
        format!("{} 分钟", mins)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-12 guest 逐字（`head -n 5 /proc/meminfo`）。
    const MEMINFO: &str = "MemTotal:         694832 kB
MemFree:           93988 kB
MemAvailable:     419312 kB
Buffers:           22324 kB
Cached:           338128 kB
";

    /// 2026-10-12 guest 逐字。
    const LOADAVG: &str = "0.15 0.10 0.03 1/211 78217\n";
    const UPTIME: &str = "163051.40 324494.79\n";

    /// 2026-10-12 guest 逐字（前两行是表头，`lo` 那一行是回环）。
    const NET_DEV: &str = "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 2803775   27008    0    0    0     0          0         0  2803775   27008    0    0    0     0       0          0
  eth0: 1721523045 1208499    0    0    0     0          0         0 100617238  160593    0   23    0     0       0          0
";

    #[test]
    fn memory_reads_total_and_available_separately() {
        assert_eq!(parse_mem_total(MEMINFO), Some(694832));
        assert_eq!(parse_mem_available(MEMINFO), Some(419312));
        // 老内核没有 MemAvailable：那一头是"没读到"，不拿 MemFree 顶出一个说不清口径的百分比。
        assert_eq!(
            parse_mem_available("MemTotal:  100 kB\nMemFree:   50 kB\n"),
            None
        );
        // 键名前缀撞车（`MemTotal` vs 假想的 `MemTotalXyz`）与冒号后无数字都要落 None。
        assert_eq!(parse_mem_total("MemTotalXyz: 9 kB\n"), None);
        assert_eq!(parse_mem_total(""), None);
    }

    #[test]
    fn load_average_takes_exactly_three_numbers() {
        assert_eq!(parse_loadavg(LOADAVG), Some([0.15, 0.10, 0.03]));
        // 第四段 `1/211` 是 runnable/总进程数，不是负载。
        assert_eq!(parse_loadavg("0.15 0.10 0.03\n"), Some([0.15, 0.10, 0.03]));
        assert_eq!(parse_loadavg("0.15 0.10\n"), None);
        assert_eq!(parse_loadavg(""), None);
    }

    #[test]
    fn uptime_is_the_first_field_truncated_not_rounded() {
        assert_eq!(parse_uptime_secs(UPTIME), Some(163051));
        assert_eq!(parse_uptime_secs("0.50 1.00\n"), Some(0));
        assert_eq!(parse_uptime_secs(""), None);
    }

    #[test]
    fn netdev_skips_the_two_header_lines_and_the_loopback() {
        let got = parse_netdev(NET_DEV);
        assert_eq!(
            got,
            vec![NetDev {
                iface: "eth0".to_string(),
                rx_bytes: 1_721_523_045,
                tx_bytes: 100_617_238,
            }],
            "表头那两行与回环都不该出现；发字节在第 8 列不是第 1 列"
        );
        // 列数不足（别的实现改了表）→ 少给一块网卡，而不是把 packets 当 bytes。
        assert!(parse_netdev("  eth0: 1 2 3\n").is_empty());
        // 名字与冒号之间没有空格的那种写法。
        assert_eq!(parse_netdev("  eth1:7 0 0 0 0 0 0 0 9 0 0 0 0 0 0 0\n").len(), 1);
        assert!(parse_netdev("").is_empty());
    }

    /// 四份文件一起给：这一支钉的是"整格从目录读出来"这条路，而不只是四个解析函数。
    #[test]
    fn readings_come_from_the_injected_proc_root() {
        let root = std::env::temp_dir().join(format!(
            "zbox-proc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for (name, body) in [
            ("meminfo", MEMINFO),
            ("loadavg", LOADAVG),
            ("uptime", UPTIME),
            ("net/dev", NET_DEV),
        ] {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
        }
        let got = readings_in(&root);
        assert_eq!(got.mem_total_kb, Some(694832));
        assert_eq!(got.mem_available_kb, Some(419312));
        assert_eq!(got.used_kb(), Some(275520));
        assert_eq!(got.used_percent(), Some(39));
        assert_eq!(got.load, Some([0.15, 0.10, 0.03]));
        assert_eq!(got.uptime_secs, Some(163051));
        assert_eq!(got.netdev.len(), 1);

        // 目录整个不在（Windows 开发机上的 `/proc`）：全字段空，不是"读取失败"，也不是 0。
        let none = readings_in(&root.join("nope"));
        assert_eq!(none, BoardReadings::default());
        assert_eq!(none.used_percent(), None);
        assert!(none.netdev.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_human_units_are_binary_with_one_decimal() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        // 一位小数会把它进位成 1024.0 KB，所以升档门槛在 1023.95：宁可写 1.0 MB。
        assert_eq!(human_bytes(1_048_575), "1.0 MB");
        assert_eq!(human_bytes(1_048_576), "1.0 MB");
        assert_eq!(human_bytes(1_721_523_045), "1.6 GB");
        assert_eq!(human_bytes(694832 * 1024), "678.5 MB");
        assert_eq!(human_bytes(1024u64.pow(4)), "1.0 TB");
        // 计数器再大也只到 TB，不编一个 PB 出来（这一档之后是坏掉的读数，不是新单位）。
        assert_eq!(human_bytes(1024u64.pow(5)), "1024.0 TB");
        assert_eq!(human_duration(163051), "1 天 21 小时");
        assert_eq!(human_duration(3660), "1 小时 1 分钟");
        assert_eq!(human_duration(59), "0 分钟");
    }

    /// 那三句"为什么这一格是空的"会**原样出现在屏幕上**，而读它的人不懂程序。所以它们不许带代码词：
    /// 反引号、模块路径、文件名、`snake_case`、内部代号（netns / proc / tproxy）都算。
    /// [`DISK_NOT_READ`] 的注释里早就写了这条，但没有测试——U5d 新加的那两句是最容易写成
    /// "它的控制面在 daens 网络命名空间里"那种话的，所以这一条钉三句一起过。
    ///
    /// 后半截钉的是那三句的**落点**：每一句都得把"空着"与"结论"分开（不是 0 / 不是没在跑），
    /// 只解释为什么不读、不留下那半句，用户读到的仍然是一个没有界的空格子。
    #[test]
    fn the_not_read_explanations_stay_in_plain_words() {
        for (cell, text) in [
            ("磁盘", DISK_NOT_READ),
            ("分流引擎", DAE_NOT_READ),
            ("主路由", ROUTER_NOT_READ),
        ] {
            for jargon in ["`", "::", ".rs", "_", "netns", "proc", "tproxy", "**"] {
                assert!(
                    !text.contains(jargon),
                    "{cell} 那句「为什么读不到」里混进了代码词 {jargon:?}：{text}"
                );
            }
            assert!(
                text.contains("不是"),
                "{cell} 那句没把「空着」和「结论」分开：{text}"
            );
        }
    }
}
