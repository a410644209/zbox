//! 服务制品：同一份 ZBox 要在两类 init 系统上跑 —— systemd（Ubuntu/Debian）与
//! OpenRC（Alpine，用户定的首选虚拟机系统，占用更小）。
//!
//! OpenRC 部分按 OpenRC 官方惯例手写：参考成品包里**没有** runscript，那三个包提供的
//! `/etc/init.d/*` 全是 OpenWrt 的 `#!/bin/sh /etc/rc.common`（塞进 Alpine 或 Ubuntu 的
//! `/etc/init.d` 都跑不起来）。从它们那里能确认的只有一件事：缺监督、缺 sysctl 持久化。
//! 所以这里补上：
//! - **崩溃拉起**。参考实现只管 start/stop，进程死了没人管；systemd 侧等价物是 `Restart=on-failure`，
//!   OpenRC 侧只有 `supervisor=supervise-daemon` 这一条路（实测：0.55.1 根本不读 `respawn=true`，
//!   见下面 `openrc_runscript` 的注释与 schema §11.8.5）。
//! - **sysctl 持久化**。参考实现完全没设 sysctl，因为它靠"目的地址命中 nft set"分流、不需要转发；
//!   旁路由必须 `ip_forward=1` + `rp_filter=2`，所以这些键由 ZBox 写进 `/etc/sysctl.d/`。
//!
//! 这里产的是**安装制品**（绝对路径，装一次），不是每次下发的快照文件（相对路径）。
//! 两者的区别刻意为之：换配置不该重写服务单元，换服务单元不该动转发规则。
//!
//! 一台机器上因此有**两个服务**：`zbox`（数据面，sing-box）与 `zbox-panel`（面板，zboxd serve）。
//! 分成两个而不是让面板 supervise 数据面，是因为面板必须在数据面坏着的时候也起得来——
//! 那正是需要它的时刻；两份制品之间没有任何依赖关系，这条由单测
//! `the_panel_service_is_never_chained_to_the_data_plane` 钉着。

use crate::api::serve::DEFAULT_PANEL_PORT;
use crate::gen::netstate;
use crate::model::config::{DesiredState, RULE_SET_DIR};
use std::collections::BTreeMap;

pub const SERVICE_NAME: &str = "zbox";
/// 面板那个服务的名字。刻意不叫 `zboxd`：`rc-service zbox` 与 `rc-service zbox-panel`
/// 并排站着的时候，名字里带不带"panel"决定了人要猜几秒才敢按回车。
pub const PANEL_SERVICE_NAME: &str = "zbox-panel";
pub const UNIT_PATH: &str = "/etc/systemd/system/zbox.service";
pub const PANEL_UNIT_PATH: &str = "/etc/systemd/system/zbox-panel.service";
pub const RUNSCRIPT_PATH: &str = "/etc/init.d/zbox";
pub const PANEL_RUNSCRIPT_PATH: &str = "/etc/init.d/zbox-panel";
/// 两类 init 系统都读这个目录（systemd 经 systemd-sysctl，Alpine 经 sysctl 服务）
pub const SYSCTL_PATH: &str = "/etc/sysctl.d/99-zbox.conf";

/// OpenRC 崩溃拉起的三档，实测只有这三个变量会被 supervise-daemon 读到（schema §11.8.5）。
/// 取有限次数的意图和 systemd 的 StartLimit 一样：崩成循环时停在可诊断的状态，不做无限重启风暴。
pub const RESPAWN_DELAY: u32 = 5;
pub const RESPAWN_MAX: u32 = 5;
pub const RESPAWN_PERIOD: u32 = 1800;

/// 面板那一档的重试次数比数据面多，给的理由是**它的启动失败大多是临时的**：
/// 局域网网卡还没拿到 IPv4（DHCP 慢、网卡脚本排在后面）时 `resolve_bind` 会拒起，
/// 等一会儿就好；数据面那 5 次对付的是配置类故障，那种重试不会自己好。
/// 5 秒一次 × 12 次 ≈ 60 秒，够一张卡从"链路起来"走到"有地址"，再多就停在可诊断的状态。
/// systemd 侧用 `StartLimitBurst` + `RestartSec` 对齐同一个预算，两边不许各写一个数。
pub const PANEL_RESPAWN_MAX: u32 = 12;

/// 二进制与目录布局。VM 实测前用这套默认值，装别的机器上再改。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// sing-box 官方二进制绝对路径（ZBox 不自研数据面）
    pub singbox_bin: String,
    /// zboxd 自己的绝对路径：只有面板那份制品用得上。
    ///
    /// 不做成 `current_exe()`：那在 `--prefix` 试跑时会指到临时目录里那份，写进一份要装到
    /// `/etc` 的制品里就是一句假话；而现役 zboxd 由人放到哪（`provision` 只管 sing-box）
    /// 是这台机器的 facts，得说清楚。
    pub zboxd_bin: String,
    /// 指向当前生效快照的符号链接所在目录，`config.json` 就在这个目录里
    pub current_dir: String,
    /// 状态目录：快照、规则集产物、日志
    pub root: String,
    /// 面板端口。它**不进期望状态**：`state.json` 描述的是数据面该长什么样，
    /// 而"这台机器上的管理服务开在哪个端口"不是数据面的一部分——写进去就等于让一次
    /// `rollback <修订号>` 顺手把面板的门牌号也回滚掉。
    pub panel_port: u16,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            singbox_bin: "/usr/local/bin/sing-box".into(),
            zboxd_bin: "/usr/local/bin/zboxd".into(),
            current_dir: "/var/lib/zbox/current".into(),
            root: "/var/lib/zbox".into(),
            panel_port: DEFAULT_PANEL_PORT,
        }
    }
}

impl Layout {
    pub fn config_path(&self) -> String {
        format!("{}/config.json", self.current_dir.trim_end_matches('/'))
    }

    /// `-D` 的落点：**必须可写且不随快照切换而变**。
    ///
    /// VM 实测（schema §11.7）：`rule_set[].path`（local）、`initial_path`、`cache_file.path`
    /// 全部以 `-D` 为基准，`-D` 缺省就是 CWD。快照目录（每次下发换一份、会被 prune）不能当状态目录：
    /// `cache.db` 装着规则集缓存与 fakeip 映射，跟着快照走等于每次回滚都重新联网。
    pub fn state_dir(&self) -> String {
        self.root.trim_end_matches('/').to_string()
    }

    /// 期望状态的规范落点。
    ///
    /// 它跟着 `--root` 走，所以彩排时指到哪份状态就在哪；在此之前 `state.json` 只是
    /// `zboxd` 的一个位置参数，"当前生效的那份状态在哪"这个问题没有答案——面板要读、
    /// CLI 要写的必须是这一个文件。位置参数仍然保留，用来复现和试跑另一份状态。
    pub fn state_path(&self) -> String {
        format!("{}/state.json", self.state_dir())
    }

    /// 面板访问令牌的落点（0600）。
    ///
    /// 放在状态目录里而不是 `current_dir`：后者是指向快照的符号链接那一侧，每次下发都换，
    /// 令牌要活过回滚。
    pub fn panel_token_path(&self) -> String {
        format!("{}/panel.token", self.state_dir())
    }

    /// 规则集产物（`.srs`）的落点，与 `state_dir()` 同源；task #9 写、生成器按相对路径引用。
    /// 目录名必须就是配置里 `initial_path` 的首段（`RULE_SET_DIR`），否则预置的文件不在
    /// sing-box 去看的那个目录下。
    pub fn rule_set_dir(&self) -> String {
        format!("{}/{}", self.state_dir(), RULE_SET_DIR)
    }

    /// §11.6 预跑门专用：它会真下载规则集并写 `cache.db`，所以必须是独立目录，
    /// 不能指到现役状态目录上。
    pub fn preflight_dir(&self) -> String {
        format!("{}/preflight", self.state_dir())
    }

    /// 内核自写的日志。配置里那条 `log.output` 是裸文件名，实测以 `-D` 为基准落盘
    /// （`gen::singbox::LOG_FILE` 的注释记着这两条），所以位置完全由状态目录决定。
    pub fn kernel_log_path(&self) -> String {
        format!("{}/{}", self.state_dir(), crate::gen::singbox::LOG_FILE)
    }

    /// 监督者替子进程接住的 stderr 落点。**它不是给 sing-box 的日志器用的**（那个写在
    /// config 里），而是专门收"配置还没读到就死了"那一类故障——实测这类故障在机器上
    /// 原本无处可查（schema §11.8.9）。跟着状态目录走，所以 `--root` 一挪它也跟着挪。
    pub fn worker_stderr_path(&self) -> String {
        format!("{}/sing-box.stderr.log", self.state_dir())
    }

    /// 面板进程自己的 stderr 落点，与 [`Self::worker_stderr_path`] 同一个理由、同一个跟法。
    ///
    /// 这里收的是"面板起不来"那一类故障：绑定被拒（网卡上没地址、端口被占）时 `serve` 把
    /// 原因写到 stderr 就退出，而 OpenRC 的 syslog 只记退出码。systemd 那侧不需要这个文件
    /// （stderr 天生进 journal），所以只有 OpenRC 的 runscript 引用它。
    pub fn panel_stderr_path(&self) -> String {
        format!("{}/panel.stderr.log", self.state_dir())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitSystem {
    Systemd,
    OpenRC,
}

/// 对服务进程的动词。`reload` 不在这里：**sing-box 不原地重载配置**，svc 层的 reload
/// 语义一律映射成 `Restart`（systemd 单元里刻意没有 ExecReload）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
    Status,
}

impl InitSystem {
    /// 用注入的"路径是否存在"探针判定 init 系统，判定不出来就返回 `None`：
    /// svc 层只能据此生成手工命令清单，不许猜一个往机器上写。
    ///
    /// 探针顺序按官方判据：`/run/systemd/system` 存在即 systemd 正在运行（比查
    /// `/sbin/init` 符号链接可靠），否则看 OpenRC 的运行时标志文件。
    pub fn detect(exists: &dyn Fn(&str) -> bool) -> Option<Self> {
        if exists("/run/systemd/system") {
            return Some(Self::Systemd);
        }
        if exists("/run/openrc/softlevel") || exists("/sbin/openrc-run") {
            return Some(Self::OpenRC);
        }
        None
    }

    /// 服务控制命令（`Svc` 的 reload / probe 用它，两条 init 系统的差异只在这一处收敛）。
    pub fn control_command(&self, action: ServiceAction) -> Vec<String> {
        let name = SERVICE_NAME.to_string();
        match (self, action) {
            (Self::Systemd, ServiceAction::Start) => vec!["systemctl".into(), "start".into(), name],
            (Self::Systemd, ServiceAction::Stop) => vec!["systemctl".into(), "stop".into(), name],
            (Self::Systemd, ServiceAction::Restart) => {
                vec!["systemctl".into(), "restart".into(), name]
            }
            // systemd 的 is-active：活跃返回 rc=0 并打印 active，非活跃 rc 非 0，天生适合当判据
            (Self::Systemd, ServiceAction::Status) => {
                vec!["systemctl".into(), "is-active".into(), name]
            }
            (Self::OpenRC, ServiceAction::Start) => {
                vec!["rc-service".into(), name, "start".into()]
            }
            (Self::OpenRC, ServiceAction::Stop) => {
                vec!["rc-service".into(), name, "stop".into()]
            }
            (Self::OpenRC, ServiceAction::Restart) => {
                vec!["rc-service".into(), name, "restart".into()]
            }
            (Self::OpenRC, ServiceAction::Status) => {
                vec!["rc-service".into(), name, "status".into()]
            }
        }
    }

    /// CLI `--init` 认的那两个词。制品里嵌的就是它，所以这个名字不许飘：生成器写 `openrc`
    /// 而解析器只认别的拼法，面板服务就是一条永远起不来的命令——而它偏偏是修机器用的那一个。
    /// 这一条由 `the_init_words_baked_into_the_artifacts_are_the_ones_the_cli_parses` 对着
    /// `src/bin/zboxd.rs` 的源码钉，不靠注释。
    pub fn cli_word(&self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::OpenRC => "openrc",
        }
    }

    /// 启用服务 + 让 sysctl 持久化生效所需的额外命令。
    ///
    /// `rc-update add sysctl boot` 在 Alpine 3.21.3 上是**空操作**：基础装机就带了这条
    /// （/etc/runlevels/boot/sysctl 的时间戳和同目录其他项一致），实测重复 add 打印
    /// `already installed in runlevel 'boot'; skipping` 且 rc=0。留着是因为
    /// "sysctl 文件重启后没人读"这件事的表现是当场生效、重启静默失效，很难查；
    /// 有人手工删过这条时 install 顺手补回来，代价是一行幂等命令。
    ///
    /// 这里刻意**不含 `chmod`**：runscript 要可执行是"写文件"这件事的收尾，
    /// 由 `svc::Svc::install` 对它真正写入的那个路径执行；写进这里就变成了一份
    /// 硬编码的绝对路径，试跑用的前缀一挪就对不上。
    ///
    /// 顺序是"两个服务都先登记，再都拉起"：登记是幂等的改名工作，拉起才是有副作用的那一步，
    /// 中间不许插队——`rc-update add zbox-panel default` 排在 `rc-service zbox restart` 后面
    /// 就没有任何理由了。
    pub fn enable_commands(&self) -> Vec<Vec<String>> {
        match self {
            Self::Systemd => vec![
                vec!["systemctl".into(), "daemon-reload".into()],
                vec!["systemctl".into(), "enable".into(), SERVICE_NAME.into()],
                vec![
                    "systemctl".into(),
                    "enable".into(),
                    PANEL_SERVICE_NAME.into(),
                ],
                self.start_command(),
                self.panel_start_command(),
            ],
            Self::OpenRC => vec![
                vec![
                    "rc-update".into(),
                    "add".into(),
                    SERVICE_NAME.into(),
                    "default".into(),
                ],
                vec![
                    "rc-update".into(),
                    "add".into(),
                    "sysctl".into(),
                    "boot".into(),
                ],
                vec![
                    "rc-update".into(),
                    "add".into(),
                    PANEL_SERVICE_NAME.into(),
                    "default".into(),
                ],
                self.start_command(),
                self.panel_start_command(),
            ],
        }
    }

    /// 单独把"拉起服务"那条拆出来，是因为 `svc::Svc::install` 要能在首次安装时只摘掉它。
    ///
    /// 那一刻 `<current>/config.json` 还不存在，而这条命令**不会报错**：2026-10-05 guest 实测
    /// `rc-service zbox restart` rc=0、`status` 报 `started`，只有监督者活着，worker 在 5 次
    /// 重试里一直 FATAL，约 25 秒后停在 `stopped`（`status` rc=3）。那时候**那 5 次一个字都不落盘**
    /// （内核自己的日志路径写在 config 里，读不到配置就没有日志器；supervise-daemon 往 syslog 只记
    /// 命令行与退出码，记不到原因）——原因这一半后来用 `error_log` 补上了，见 schema §11.8.9 第七节。
    /// 但"执行成功、`status: started`、数据面为空"这条读数本身仍然要躲开：配置还没写出来时
    /// 拉起它没有任何收益，第 7 步的 apply 会带着真配置重启。见 schema §11.8.9。
    pub fn start_command(&self) -> Vec<String> {
        match self {
            Self::Systemd => vec!["systemctl".into(), "restart".into(), SERVICE_NAME.into()],
            Self::OpenRC => vec!["rc-service".into(), SERVICE_NAME.into(), "restart".into()],
        }
    }

    /// 拉起面板那条。**首次安装也跑它**，这与上一条正好相反，理由是两者面对的前提不同：
    /// 数据面那条在 `<current>/config.json` 还不存在时会"执行成功而数据面为空"（上一条的注释），
    /// 而面板本来就不需要任何配置才能开门——`state.json` 不在就以基线形状呈现
    /// （`load_state_or_baseline`），令牌不在就当场生成一份（`PanelToken::load_or_create`，
    /// 它经 `state::write_private` 顺手把状态目录建出来）。
    ///
    /// 更要紧的是"一键配置"这条路：装完之后人要去开面板写第一份状态，
    /// 所以面板是首次安装里唯一应该立刻在听的那个服务。
    pub fn panel_start_command(&self) -> Vec<String> {
        match self {
            Self::Systemd => vec![
                "systemctl".into(),
                "restart".into(),
                PANEL_SERVICE_NAME.into(),
            ],
            Self::OpenRC => vec![
                "rc-service".into(),
                PANEL_SERVICE_NAME.into(),
                "restart".into(),
            ],
        }
    }
}

/// 安装制品：数据面服务定义 + **面板服务定义** + sysctl 持久化文件。key 为绝对路径。
pub fn artifacts(
    init: InitSystem,
    state: &DesiredState,
    layout: &Layout,
) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    let body = match init {
        InitSystem::Systemd => systemd_unit(state, layout),
        InitSystem::OpenRC => openrc_runscript(state, layout),
    };
    files.insert(
        match init {
            InitSystem::Systemd => UNIT_PATH,
            InitSystem::OpenRC => RUNSCRIPT_PATH,
        }
        .to_string(),
        body,
    );
    let panel = panel_artifact(init, layout);
    files.insert(panel.0.to_string(), panel.1);
    files.insert(SYSCTL_PATH.to_string(), sysctl_conf(state));
    files
}

/// 面板那一份制品，返回（落点绝对路径, 正文）。
///
/// 命令行为什么要单独抽出来：`zboxd serve` 的调用形状只许有一处定义，
/// 否则 systemd 机器和 Alpine 机器上的面板可以是两个不同的东西。
fn panel_artifact(init: InitSystem, layout: &Layout) -> (&'static str, String) {
    match init {
        InitSystem::Systemd => (PANEL_UNIT_PATH, panel_systemd_unit(layout)),
        InitSystem::OpenRC => (PANEL_RUNSCRIPT_PATH, panel_openrc_runscript(layout)),
    }
}

/// 面板服务真正 exec 的那一条命令行，argv[0] 是 zboxd 的路径。
///
/// `--init` 是**生成时就钉死**的，不是让面板自己开机再探一遍：这份制品本来就是按某个
/// init 系统写出来的，判定结果在这里是已知量。把它传下去换掉的是探测失败那一条错误路径
/// （`detect_init` 认不出来就拒绝启动），面板服务不该因为"猜不出自己是谁"而起不来。
/// 不给 `--bind`：绑哪个地址由 `resolve_bind` 按 `state.network.lan_interface` 现探，
/// 地址写死进制品就意味着换条网线要重新 install。
pub fn panel_argv(init: InitSystem, layout: &Layout) -> Vec<String> {
    vec![
        layout.zboxd_bin.clone(),
        "serve".into(),
        "--root".into(),
        layout.state_dir(),
        "--init".into(),
        init.cli_word().to_string(),
        "--port".into(),
        layout.panel_port.to_string(),
    ]
}

/// 开机持久化的内核参数。只落整机级的键（`netstate::host_sysctl_pairs`），
/// 与下发脚本同源，所以两边不可能一边有 rp_filter 一边没有。
///
/// 绑网卡的那条刻意不写在这里：`/etc/init.d/sysctl` 用 `sysctl -p` 读本文件，Alpine 上那是
/// busybox 实现，对不认识的键**静默跳过且 rc=0**（2026-09-30 实测），网卡名一旦与实际不符
/// 就是悄悄少一条。脚本那侧用 `-w`，同一条键实测 rc=1，报错看得见。
///
/// v6 那两条（`all.forwarding=1` + `all.accept_ra=2`）按档位进这份文件，而且**成对进**：
/// 实测里只写转发的那一臂，本机自己的 v6 默认路由整臂消失、15 分钟无自愈
/// （`docs/reference/ipv6-kernel-measured.md` §1）。
pub fn sysctl_conf(state: &DesiredState) -> String {
    let mut out = String::from(
        "# ZBox 生成，请勿手改：下一次 `zbox install` 会覆盖。\n\
         # 旁路由必须转发。rp_filter 放宽到 2（宽松模式）是按社区通行做法上的保险：\n\
         # netns 实测里严格模式 1 也没丢包，单网卡形状上尚未证伪，见 schema 文档 §11.7。\n\
         # 这里只有整机级的键。绑网卡的 net.ipv4.conf.<网卡>.rp_filter 由 netstate.sh 在\n\
         # 开劫持时当场设置：那个脚本用 sysctl -w 写，键名错了会报错；读本文件的 -p 不会。\n",
    );
    if state.network.ipv6_mode.takes_connection() {
        out.push_str(
            "# 这一台按 ipv6_mode = dual_stack 接管 v6 连接，所以开 v6 转发。\n\
             # accept_ra=2 与它是**一对**，不许拆开：内核在 forwarding 打开时停止采纳 RA，\n\
             # 只写转发等于把这台机器自己的 v6 默认路由关掉（guest 三臂实测，见上引文档 §1）。\n\
             # 两条都在这个文件里，所以开机时也不存在「有转发、还没 accept_ra=2」的空窗。\n",
        );
    }
    for (key, value) in netstate::host_sysctl_pairs(state.network.ipv6_mode) {
        out.push_str(&format!("{key} = {value}\n"));
    }
    out
}

fn systemd_unit(state: &DesiredState, layout: &Layout) -> String {
    format!(
        "[Unit]
Description=ZBox 旁路由数据面（sing-box）
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory={root}
ExecStart={bin} run -c {config} -D {state}
Restart=on-failure
RestartSec=5
LimitNOFILE=1048576
# 虚拟网卡（virtio/vmxnet3）上 QUIC 的 GSO 分片会出问题，DoH3/Hysteria2 首当其冲
Environment=QUIC_GO_DISABLE_GSO=true
# 网卡名里带 systemd 的转义规则，只在这个值上做一次替换，其余保持字面量
Environment=ZBOX_LAN_IFACE={lan_iface}

[Install]
WantedBy=multi-user.target
",
        root = layout.root,
        bin = layout.singbox_bin,
        config = layout.config_path(),
        state = layout.state_dir(),
        lan_iface = systemd_escape_interface(&state.network.lan_interface),
    )
}

/// systemd 的 `Environment=` 值里 `%` 和空白有特殊含义；网卡名正常情况不含这些，
/// 真含了就退化成引号包住，让 systemd 按字面量解析。
fn systemd_escape_interface(raw: &str) -> String {
    if raw.contains(['%', '"', '\'']) || raw.split_whitespace().count() != 1 {
        format!("\"{raw}\"")
    } else {
        raw.to_string()
    }
}

fn openrc_runscript(state: &DesiredState, layout: &Layout) -> String {
    format!(
        "#!/sbin/openrc-run
# ZBox 生成，请勿手改：下一次 `zbox install` 会覆盖。

# 崩溃拉起只能走这一条路，是实测定的：OpenRC 0.55.1 的 /sbin/openrc-run 里
# 搜不到 respawn 这个变量（grep -a 全文件 0 命中），旧写法 `respawn=true` 是死的——
# 现场 kill -9 之后等满 12 秒没回来，状态停在 `crashed`。
# 会被读到的是 supervise-daemon：装上它之后同样 kill -9，第 2 秒就拉回新进程、
# `status` 仍是 started、四个入站重新在听（schema §11.8.5）。
supervisor=supervise-daemon

command=\"{bin}\"
command_args=\"run -c {config} -D {state}\"
# 这条 pidfile 记的是**监督者**的 pid，sing-box 是它的子进程；
# `rc-service zbox stop` 实测会把两个一起收掉并删除 pidfile。
pidfile=\"/run/{name}.pid\"

# supervise-daemon 真正读的三个变量（实测它拼出的命令行是
# `--respawn-delay 5 --respawn-max 5 --respawn-period 1800`）。
# respawn_max 有意不是无限：崩成循环时让它放弃并停在可诊断的状态，
# 和 systemd 那边 StartLimit 的语义对齐。
respawn_delay={respawn_delay}
respawn_max={respawn_max}
respawn_period={respawn_period}

# 子进程的 stderr 落到状态目录里这个文件。没有它，\"起来了但一个端口都没在听\"这类故障
# 在现场一个字都查不到：sing-box 自己的日志路径写在 config 里，配置读不到就没有日志器，
# 而 supervise-daemon 只往 syslog 记命令行与退出码（2026-10-05 实测，schema §11.8.9）。
# 只接 stderr：正常启动时它一个字都不写，量级因此由 respawn 上限兜着，不需轮转。
error_log=\"{stderr_log}\"

# 同 systemd 单元那条注释
export QUIC_GO_DISABLE_GSO=true
export ZBOX_LAN_IFACE={iface}

depend() {{
    need net
    after net
}}

# `need net` 只保证网络脚本跑过，不等于已经拿到租约和默认路由。
# 没有默认路由时 sing-box 照样能起来，但出站长连接会一直失败，所以显式等一会儿。
start_pre() {{
    # 先建状态目录：error_log 的父目录就是它，而 supervise-daemon 在 fork worker 之前就要
    # 打开那个文件（面板那份制品的实测与理由写在 panel_openrc_runscript 里）。少这一句，
    # 一个被 prune 掉的 `<root>` 就让数据面变成 status 报 started、worker 一次都没跑、
    # 而原因无处可查。
    mkdir -p {state_quoted}
    i=0
    while [ \"$i\" -lt 30 ]; do
        if ip -4 route show default | grep -q default; then
            return 0
        fi
        i=$((i + 1))
        sleep 1
    done
    ewarn \"等满 30 秒仍无默认路由，sing-box 继续启动，代理可能连不出去\"
    return 0
}}
",
        bin = layout.singbox_bin,
        config = layout.config_path(),
        state = layout.state_dir(),
        name = SERVICE_NAME,
        iface = netstate::shell_quote(&state.network.lan_interface),
        state_quoted = netstate::shell_quote(&layout.state_dir()),
        respawn_delay = RESPAWN_DELAY,
        respawn_max = RESPAWN_MAX,
        respawn_period = RESPAWN_PERIOD,
        stderr_log = layout.worker_stderr_path(),
    )
}

fn panel_openrc_runscript(layout: &Layout) -> String {
    let argv = panel_argv(InitSystem::OpenRC, layout);
    format!(
        "#!/sbin/openrc-run
# ZBox 生成，请勿手改：下一次 `zbox install` 会覆盖。

supervisor=supervise-daemon

command=\"{zboxd}\"
# 命令行只有一处定义：见 `panel_argv`。
command_args=\"{args}\"
# pidfile 与 zbox 那份分开：同名会让两个服务互相能停掉对方。
pidfile=\"/run/{name}.pid\"

# 次数比数据面那一档多（{respawn_max} 次 / {respawn_period} 秒），理由写在 PANEL_RESPAWN_MAX：
# 面板最可能的启动故障是临时的——局域网网卡此刻还没地址，`resolve_bind` 就拒起。
# 这里刻意没有数据面那份那种等默认路由的循环：那种循环会把 `rc-service zbox-panel start` 当场
# 卡住半分钟，把预算交给 supervise-daemon，网卡后来才有地址和人工再起一次才走同一条路。
# （下面的 start_pre 只建目录、不等任何东西。）
respawn_delay={respawn_delay}
respawn_max={respawn_max}
respawn_period={respawn_period}

# serve 起不来时（网卡上没地址、端口被占、令牌文件读不动）原因只写到 stderr，
# 而 supervise-daemon 往 syslog 只记命令行与退出码。数据面那份同理，见 worker_stderr_path。
error_log=\"{stderr_log}\"

# 这一行是把上面那份 error_log 变成可用的前提，实测定的：supervise-daemon 在 fork worker
# **之前**就拿 --stderr 去打开文件，而那个文件的父目录正是状态目录。新机器上 `<root>` 还不存在
# （它是面板启动后写令牌时才顺带建出来的），于是 2026-10-06 guest 上看到的现象是
# `status: started`、监督者进程活着、一个端口都没在听、而 panel.stderr.log 压根没被建出来——
# 连为什么起不来都没地方记。面板被设计成不需要任何配置就能开门，那份制品却
# 依赖一个只有配置写出来才存在的目录，这一句就是把它补上。
# 用 mkdir 不用 checkpath：实测裸 shell 里 `which checkpath` 报 not found，它是 OpenRC 内部的
# shell 函数，不在 runscript 执行时的 PATH 上。
start_pre() {{
    mkdir -p {state_quoted}
}}

depend() {{
    need net
    after net
}}
# 没有 `need zbox`：数据面坏着的时候正是需要面板的时候。把面板挂在它后面，
# 等于把修东西的那扇门焊死在坏掉的那台机器上。
",
        zboxd = layout.zboxd_bin,
        args = argv[1..].join(" "),
        name = PANEL_SERVICE_NAME,
        respawn_delay = RESPAWN_DELAY,
        respawn_max = PANEL_RESPAWN_MAX,
        respawn_period = RESPAWN_PERIOD,
        stderr_log = layout.panel_stderr_path(),
        state_quoted = netstate::shell_quote(&layout.state_dir()),
    )
}

fn panel_systemd_unit(layout: &Layout) -> String {
    let argv = panel_argv(InitSystem::Systemd, layout);
    format!(
        "[Unit]
Description=ZBox 面板（局域网配置面）
After=network-online.target
Wants=network-online.target
# 与 OpenRC 那侧同一个重试预算（{respawn_max} 次 / {respawn_period} 秒窗口）。
# 这两行归 [Unit] 不归 [Service]：systemd 229 起 StartLimit* 搬到 [Unit]，
# 写在 [Service] 里读的是老语义，两处数字就对不上了。
StartLimitIntervalSec={respawn_period}
StartLimitBurst={respawn_max}

[Service]
Type=simple
# 命令行只有一处定义：见 `panel_argv`。
ExecStart={exec}
Restart=on-failure
RestartSec={respawn_delay}
# 没有 ExecReload：面板读的就是磁盘上那一份，不存在原地换配置这回事。
# 没有 QUIC_GO_DISABLE_GSO：那条是给 Go 写的 sing-box 的，zboxd 是 Rust。
# 没有 StandardError=：stderr 默认进 journal，`journalctl -u {name}` 看得到起不来的原因。
# 没有 Requires=/After= zbox.service：见 panel_openrc_runscript 末尾那句关于门不能焊死的说明。

[Install]
WantedBy=multi-user.target
",
        exec = argv.join(" "),
        respawn_delay = RESPAWN_DELAY,
        respawn_max = PANEL_RESPAWN_MAX,
        respawn_period = RESPAWN_PERIOD,
        name = PANEL_SERVICE_NAME,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::Ipv6Mode;

    fn state(lan: &str) -> DesiredState {
        let mut state = DesiredState::baseline();
        state.network.lan_interface = lan.into();
        state
    }

    #[test]
    fn detection_prefers_systemd_then_openrc_then_refuses() {
        let probe = |set: &[&str]| InitSystem::detect(&|p: &str| set.contains(&p));
        assert_eq!(
            probe(&["/run/systemd/system", "/sbin/openrc-run"]),
            Some(InitSystem::Systemd),
            "systemd 机器上 /sbin/openrc-run 也可能存在，判定顺序必须 systemd 在前"
        );
        assert_eq!(probe(&["/sbin/openrc-run"]), Some(InitSystem::OpenRC));
        assert_eq!(probe(&[]), None, "判定不出来就不许猜");
    }

    #[test]
    fn openrc_runscript_carry_the_pieces_the_reference_lacks() {
        let files = artifacts(InitSystem::OpenRC, &state("eth0"), &Layout::default());
        let script = &files[RUNSCRIPT_PATH];
        assert!(script.starts_with("#!/sbin/openrc-run\n"), "{script}");
        // 参考包没有的崩溃拉起。变量名是实测的：`respawn=true` 在 0.55.1 上没人读，
        // 只有 supervise-daemon 这条路真的把进程拉回来（schema §11.8.5）。
        assert!(script.contains("supervisor=supervise-daemon"), "{script}");
        // 被实测判死的旧写法不许再作为**变量**回来（注释里提到它们正是为了记住为什么不用，
        // 所以按行首判，不按整篇文本判）
        for line in script.lines() {
            let head = line.trim_start();
            assert!(
                !head.starts_with("respawn=") && !head.starts_with("command_background="),
                "回到了没人读的旧写法：{head}"
            );
        }
        assert!(
            script.contains(&format!("respawn_delay={RESPAWN_DELAY}")),
            "{script}"
        );
        assert!(
            script.contains(&format!("respawn_max={RESPAWN_MAX}")),
            "{script}"
        );
        assert!(script.contains("respawn_period="), "{script}");
        // pidfile 是监督者的 pid，别再把它当 sing-box 的 pid 用
        assert!(script.contains("pidfile=\"/run/zbox.pid\""), "{script}");
        // 参考包没有的 sysctl 持久化
        assert!(files.contains_key(SYSCTL_PATH));
        assert!(script.contains("need net"));
        assert!(script.contains("start_pre()"));
        assert!(script.contains("run -c /var/lib/zbox/current/config.json"));
        // "起来了但没在听"必须有地方能查到：supervise-daemon 读 `error_log` 变量拼成 `--stderr`
        // （0.55.1 的 /usr/libexec/rc/sh/supervise-daemon.sh:32，真机验过文本落盘）。
        // 变量名不能猜：写 `stderr=` 是没人读的。
        assert!(
            script.contains("error_log=\"/var/lib/zbox/sing-box.stderr.log\""),
            "{script}"
        );
    }

    /// 落点必须跟着状态目录走：`--root` 一挪，试跑就不该往真机的状态目录里写日志。
    #[test]
    fn the_stderr_sink_follows_the_state_dir() {
        let layout = Layout {
            root: "/tmp/zbox-scratch".into(),
            ..Layout::default()
        };
        let files = artifacts(InitSystem::OpenRC, &state("eth0"), &layout);
        assert!(
            files[RUNSCRIPT_PATH].contains("error_log=\"/tmp/zbox-scratch/sing-box.stderr.log\""),
            "{}",
            files[RUNSCRIPT_PATH]
        );
    }

    /// 规范落点必须跟着状态目录走，且尾斜杠不拼出 `//state.json`：
    /// `--root` 的校验只管"必须绝对"，不剥尾巴上的斜杠。
    #[test]
    fn the_canonical_state_file_follows_the_state_dir() {
        let layout = Layout {
            root: "/tmp/zbox-scratch/".into(),
            ..Layout::default()
        };
        assert_eq!(layout.state_path(), "/tmp/zbox-scratch/state.json");
        assert_eq!(Layout::default().state_path(), "/var/lib/zbox/state.json");
    }

    #[test]
    fn systemd_unit_is_reload_free_because_sing_box_has_no_in_process_reload() {
        let files = artifacts(InitSystem::Systemd, &state("ens160"), &Layout::default());
        let unit = &files[UNIT_PATH];
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("ExecStart=/usr/local/bin/sing-box run -c"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        // 没有 ExecReload：sing-box 不原地重载配置，reload 语义由 svc 层映射成 restart
        assert!(!unit.contains("ExecReload"), "{unit}");
        assert!(
            !unit.contains("journal"),
            "日志走 sing-box 自己的 log 配置，不假设 journald"
        );
    }

    #[test]
    fn both_artifacts_pass_the_state_dir_explicitly() {
        // VM 实测（schema §11.7）：`rule_set[].path`（local）、`initial_path`、`cache_file.path`
        // 一律以 `-D` 为基准，而 `-D` 缺省是 CWD。少写 `-D` 的现场就是
        // `FATAL create service: initialize router: parse rule-set[0]:
        //  open <CWD>/rule-set/geosite-cn.srs: no such file or directory`，
        // 而靠 WorkingDirectory 兜住等于把状态目录绑在快照目录上（回滚会连 cache.db 一起丢）。
        for init in [InitSystem::Systemd, InitSystem::OpenRC] {
            let files = artifacts(init, &state("eth0"), &Layout::default());
            let body = files
                .get(match init {
                    InitSystem::Systemd => UNIT_PATH,
                    InitSystem::OpenRC => RUNSCRIPT_PATH,
                })
                .unwrap();
            assert!(
                body.contains("-D /var/lib/zbox"),
                "{init:?} 的制品必须显式给出 -D 状态目录：{body}"
            );
        }
    }

    #[test]
    fn sysctl_file_and_apply_script_never_drift_apart() {
        // 曾经的真实故障：下发脚本设了 rp_filter 但持久化文件里没有，重启后静默失效
        let conf = sysctl_conf(&state("eth0"));
        let file_keys: Vec<&str> = conf
            .lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| line.split(" = ").next())
            .collect();
        assert_eq!(
            file_keys,
            vec![
                "net.ipv4.ip_forward",
                "net.ipv4.conf.all.rp_filter",
                "net.ipv4.conf.default.rp_filter",
                "net.ipv4.conf.all.send_redirects",
            ],
            "不接管 v6 时，持久化文件必须正好是整机级那四条，一条不多一条不落：{conf}"
        );
        for lan in ["eth0", "ens160", "enp0s3"] {
            let script = netstate::render_apply(&state(lan));
            for (key, value) in netstate::host_sysctl_pairs(Ipv6Mode::Off) {
                assert!(conf.contains(&format!("{key} = {value}")), "{conf}");
                assert!(
                    script.contains(&format!("sysctl -q -w '{key}={value}'")),
                    "{script}"
                );
            }
            // 绑网卡的那条只进脚本：文件那侧由 sysctl -p 读，busybox 对不认识的键静默 rc=0，
            // 网卡名一漂移就是悄悄少一条；脚本用 -w，同样写错会 rc=1（schema §11.8.6）
            assert!(
                script.contains(&format!("sysctl -q -w 'net.ipv4.conf.{lan}.rp_filter=2'")),
                "{script}"
            );
            assert!(
                !conf.contains(&format!("net.ipv4.conf.{lan}.rp_filter")),
                "{conf}"
            );
        }
    }

    /// v6 那两条同样受"文件与脚本不许漂移"这条纪律管：它是重启后还在的那一份。
    /// 而且实测里 `accept_ra=2` 必须与 `forwarding=1` 成对出现
    /// （docs/reference/ipv6-kernel-measured.md §1），所以这里成对断言。
    #[test]
    fn the_v6_forwarding_pair_is_persisted_and_never_split() {
        let mut dual = state("eth0");
        dual.network.ipv6_mode = Ipv6Mode::DualStack;
        dual.network.lan_cidr6 = "fd00:db8:1::/64".into();
        let conf = sysctl_conf(&dual);
        let keys: Vec<&str> = conf
            .lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| line.split(" = ").next())
            .collect();
        assert_eq!(
            keys,
            vec![
                "net.ipv4.ip_forward",
                "net.ipv4.conf.all.rp_filter",
                "net.ipv4.conf.default.rp_filter",
                "net.ipv4.conf.all.send_redirects",
                // 顺序上整机级 v4 四条在前，再整机级 v6 两条
                "net.ipv6.conf.all.forwarding",
                "net.ipv6.conf.all.accept_ra",
            ],
            "双栈档的持久化文件：{conf}"
        );
        let script = netstate::render_apply(&dual);
        assert!(
            script.contains("sysctl -q -w 'net.ipv6.conf.eth0.accept_ra=2'"),
            "{script}"
        );
        // default.accept_ra 不进任何一份：它只给"以后新出现的网卡"当模板，
        // 写它等于把以后插上的每张卡都默认改成中继姿态
        for text in [conf.as_str(), script.as_str()] {
            assert!(!text.contains("net.ipv6.conf.default"), "{text}");
        }

        // 另外两档一份 v6 键都不写：dns_only 不收连接层，不需要本机当 v6 路由器
        for mode in [Ipv6Mode::Off, Ipv6Mode::DnsOnly] {
            let mut s = state("eth0");
            s.network.ipv6_mode = mode;
            s.network.lan_cidr6 = "fd00:db8:1::/64".into();
            assert!(
                !sysctl_conf(&s).contains("net.ipv6"),
                "{mode:?} 的持久化文件"
            );
            assert!(
                !netstate::render_apply(&s).contains("net.ipv6.conf.all.forwarding"),
                "{mode:?} 的下发脚本"
            );
        }
    }

    #[test]
    fn enable_commands_match_the_init_system() {
        let systemd = InitSystem::Systemd.enable_commands();
        assert!(systemd
            .iter()
            .any(|c| c.iter().any(|a| a == "daemon-reload")));
        let openrc = InitSystem::OpenRC.enable_commands();
        // 空操作也要发：Alpine 3.21.3 基础装机自带这条，实测重复 add rc=0
        assert!(openrc
            .iter()
            .any(|c| c.join(" ") == "rc-update add sysctl boot"));
        // 面板是第二个服务：登记它，也拉起它
        assert!(openrc
            .iter()
            .any(|c| c.join(" ") == "rc-update add zbox-panel default"));
        assert!(openrc.contains(&InitSystem::OpenRC.panel_start_command()));
        let systemd_names: Vec<&str> = systemd
            .iter()
            .filter(|c| c.first().map(String::as_str) == Some("systemctl"))
            .filter_map(|c| c.last().map(String::as_str))
            .collect();
        assert!(
            systemd_names.contains(&PANEL_SERVICE_NAME),
            "systemctl enable 也要覆盖面板：{systemd:?}"
        );
        // 登记排在拉起前面：反了就是"重启一个还没登记的服务"
        let index_of = |argv: &[String]| {
            openrc
                .iter()
                .position(|c| c.as_slice() == argv)
                .unwrap_or(usize::MAX)
        };
        assert!(
            index_of(&[
                "rc-update".into(),
                "add".into(),
                "zbox-panel".into(),
                "default".into()
            ]) < index_of(&InitSystem::OpenRC.panel_start_command()),
            "{openrc:?}"
        );
        // 启用命令里不该出现任何文件路径：出现就意味着制品被挪走时命令会指向空处
        assert!(
            openrc
                .iter()
                .all(|c| c.iter().all(|a| !a.contains("/etc/"))),
            "{openrc:?}"
        );
        assert!(!openrc
            .iter()
            .any(|c| c.iter().any(|a| a.contains("systemctl"))));
    }

    #[test]
    fn an_interface_name_needs_no_shell_quote_love() {
        let script = artifacts(InitSystem::OpenRC, &state("we'ird"), &Layout::default())
            .remove(RUNSCRIPT_PATH)
            .unwrap();
        assert!(script.contains(r"ZBOX_LAN_IFACE='we'\''ird'"), "{script}");
        let unit = artifacts(InitSystem::Systemd, &state("a%b"), &Layout::default())
            .remove(UNIT_PATH)
            .unwrap();
        assert!(unit.contains("ZBOX_LAN_IFACE=\"a%b\""), "{unit}");
    }

    #[test]
    fn artifacts_use_absolute_paths_and_the_snapshot_uses_relative_ones() {
        // 混用会让 apply 层把服务单元当成每次下发都要替换的快照文件
        for (path, _) in artifacts(InitSystem::Systemd, &state("eth0"), &Layout::default()) {
            assert!(path.starts_with('/'), "{path}");
        }
        let files = crate::gen::snapshot_files(&state("eth0")).unwrap();
        for (path, _) in files {
            assert!(!path.starts_with('/'), "{path}");
        }
    }

    /// 真跑一遍 `sh -n`：busybox ash / dash 才是这些脚本的目标解释器，
    /// 单测里"看起来像 shell"不作数。本机没有可用的 POSIX sh 时跳过（目标机上会真的执行）。
    #[test]
    fn every_generated_shell_script_parses_as_posix_sh() {
        let mut openrc = artifacts(InitSystem::OpenRC, &state("eth0"), &Layout::default());
        let bodies = vec![
            ("zbox-runscript", openrc.remove(RUNSCRIPT_PATH).unwrap()),
            (
                "zbox-panel-runscript",
                openrc.remove(PANEL_RUNSCRIPT_PATH).unwrap(),
            ),
            ("netstate.sh", netstate::render_apply(&state("eth0"))),
            ("netstate-down.sh", netstate::render_teardown()),
        ];
        let dir = std::env::temp_dir().join(format!("zbox-sh-n-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in bodies {
            let path = dir.join(name);
            std::fs::write(&path, &body).unwrap();
            match std::process::Command::new("sh")
                .arg("-n")
                .arg(&path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .status()
            {
                Ok(status) if status.success() => {}
                Ok(status) => panic!("{name} 未通过 sh -n，退出码 {:?}", status.code()),
                Err(_) => {
                    eprintln!("跳过 {name}：本机无可执行的 sh，这条判据留给目标机");
                    return;
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一台机器上装的是**两个**服务。少写一份就等于面板只能手工起——而这一期要收掉的
    /// 正是"`zboxd serve` 靠人敲"这件事。
    #[test]
    fn install_lands_the_panel_service_next_to_the_kernel_service() {
        let openrc_files = artifacts(InitSystem::OpenRC, &state("eth0"), &Layout::default());
        let openrc: Vec<&str> = openrc_files.keys().map(String::as_str).collect();
        assert_eq!(
            openrc,
            vec![RUNSCRIPT_PATH, PANEL_RUNSCRIPT_PATH, SYSCTL_PATH],
            "BTreeMap 有序，所以这三条就是全部落点"
        );
        let systemd_files = artifacts(InitSystem::Systemd, &state("eth0"), &Layout::default());
        let systemd: Vec<&str> = systemd_files.keys().map(String::as_str).collect();
        assert_eq!(systemd, vec![SYSCTL_PATH, PANEL_UNIT_PATH, UNIT_PATH]);
    }

    /// 面板那一条命令行只许有一处定义。两份制品里的参数若是各写各的，
    /// systemd 机器和 Alpine 机器上的面板就是两个不同的东西，而现场只会表现成
    /// "换一台机器就不一样"——最难查的那一类。
    #[test]
    fn the_panel_command_line_is_one_definition_not_two() {
        let layout = Layout::default();
        let argv = panel_argv(InitSystem::Systemd, &layout);
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/zboxd",
                "serve",
                "--root",
                "/var/lib/zbox",
                "--init",
                "systemd",
                "--port",
                "9090",
            ]
        );
        assert!(
            panel_systemd_unit(&layout).contains(&format!("ExecStart={}", argv.join(" "))),
            "{}",
            panel_systemd_unit(&layout)
        );
        // 两份制品的差别只该是 --init 那个词，其余一项一项对着
        let script = panel_openrc_runscript(&layout);
        let openrc_argv = panel_argv(InitSystem::OpenRC, &layout);
        assert!(
            script.contains(&format!("command=\"{}\"", layout.zboxd_bin)),
            "{script}"
        );
        assert!(
            script.contains(&format!("command_args=\"{}\"", openrc_argv[1..].join(" "))),
            "{script}"
        );
    }

    /// `--init` 的值是嵌进制品里的，而它的消费者在**二进制**里（`src/bin/zboxd.rs` 的 match
    /// 分支），单测进不去。所以这里把那个文件当文本读进来对名字：解析器改了拼法而生成器
    /// 没跟上，面板服务就是一条起不来的命令。
    #[test]
    fn the_init_words_baked_into_the_artifacts_are_the_ones_the_cli_parses() {
        let cli = include_str!("../bin/zboxd.rs");
        for init in [InitSystem::Systemd, InitSystem::OpenRC] {
            let word = init.cli_word();
            let arm = format!("\"{word}\" => InitSystem::");
            assert!(
                cli.contains(&arm),
                "CLI 的 --init 分支里没有 {word:?}，制品里的这个词没人认"
            );
            let (path, body) = panel_artifact(init, &Layout::default());
            assert!(body.contains(&format!("--init {word}")), "{path}");
        }
    }

    /// 面板不许挂在数据面后面：数据面坏着的时候正是需要面板的时候。
    #[test]
    fn the_panel_service_is_never_chained_to_the_data_plane() {
        let layout = Layout::default();
        let script = panel_openrc_runscript(&layout);
        let depend: Vec<&str> = script
            .lines()
            .skip_while(|line| !line.starts_with("depend()"))
            .skip(1)
            .take_while(|line| !line.starts_with('}'))
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(depend, vec!["need net", "after net"], "{script}");

        let unit = panel_systemd_unit(&layout);
        for line in unit.lines() {
            for key in ["After=", "Wants=", "Requires=", "BindsTo="] {
                if let Some(value) = line.strip_prefix(key) {
                    assert!(!value.contains("zbox"), "面板单元挂到了数据面上：{line}");
                }
            }
        }
        // 都不许带 reload 这个动词：面板没有"原地换配置"这回事。按行首判，
        // 因为正文里就有解释这一条的注释，整篇搜字符串会搜到注释上去。
        for line in unit.lines() {
            assert!(!line.trim_start().starts_with("ExecReload="), "{line}");
        }
        for line in script.lines() {
            assert!(!line.trim_start().starts_with("reload()"), "{line}");
        }
    }

    /// 重试预算：面板比数据面宽，但两边都是**有限**的。
    #[test]
    fn the_panel_retry_budget_is_bounded_but_wider_than_the_kernels_one() {
        assert!(
            PANEL_RESPAWN_MAX > RESPAWN_MAX,
            "面板的启动失败大多是临时的（网卡还没地址），预算必须比数据面宽"
        );
        let layout = Layout::default();
        let script = panel_openrc_runscript(&layout);
        assert!(
            script.contains(&format!("respawn_delay={RESPAWN_DELAY}")),
            "{script}"
        );
        assert!(
            script.contains(&format!("respawn_max={PANEL_RESPAWN_MAX}")),
            "{script}"
        );
        assert!(
            script.contains(&format!("respawn_period={RESPAWN_PERIOD}")),
            "{script}"
        );
        assert!(script.contains("supervisor=supervise-daemon"), "{script}");
        for line in script.lines() {
            let head = line.trim_start();
            assert!(
                !head.starts_with("respawn=") && !head.starts_with("command_background="),
                "回到了 OpenRC 0.55.1 没人读的旧写法：{head}"
            );
        }
        let unit = panel_systemd_unit(&layout);
        assert!(
            unit.contains(&format!("StartLimitBurst={PANEL_RESPAWN_MAX}")),
            "{unit}"
        );
        assert!(
            unit.contains(&format!("StartLimitIntervalSec={RESPAWN_PERIOD}")),
            "{unit}"
        );
        assert!(
            unit.contains(&format!("RestartSec={RESPAWN_DELAY}")),
            "{unit}"
        );
        assert!(unit.contains("Restart=on-failure"), "{unit}");
        // StartLimitIntervalSec=0 是"不设窗口"，配合无限次数就是重启风暴
        assert!(!unit.contains("StartLimitIntervalSec=0"), "{unit}");
        // 那两个 StartLimit* 归 [Unit]：写在 [Service] 里读的是 systemd 229 之前的老语义。
        // 按节切要认整行等于 "[Service]" 的那一行——正文的注释里就写着 "[Service]" 这个词，
        // 拿 split 找它会切到注释上。
        let service_at = unit
            .lines()
            .position(|line| line.trim() == "[Service]")
            .expect("单元文件要有 [Service] 那一节");
        let after_service: Vec<&str> = unit.lines().skip(service_at + 1).collect();
        assert!(
            !after_service
                .iter()
                .any(|l| l.trim_start().starts_with("StartLimit")),
            "StartLimit* 跑到 [Service] 里了：{after_service:?}"
        );
    }

    /// 两份 OpenRC 制品都必须在 supervise-daemon 打开 `error_log` **之前**把它的父目录建出来。
    ///
    /// 实测的故障形状（2026-10-06 guest，v14 那份面板制品）：`rc-service zbox-panel start`
    /// rc=0、`status` 报 `started`、`pgrep -af` 只见 supervise-daemon 不见 worker、
    /// 9090 空听，而 `<root>` 整个不存在——`--stderr` 是在 fork worker 之前打开的，
    /// 目录不在就打不开，服务于是永远起不来，而且**连"起不来"都没被记下来**。
    /// 面板尤其不能带这个毛病：它是在机器上装完就要开的那扇门，那一刻 `state.json` 还没写。
    ///
    /// 判据取的是"`mkdir` 的参数 == `error_log` 所在目录"，不是"文本里有 mkdir"：
    /// error_log 换了落点而那句没跟着换，正是这条测试要挡的漂移。
    #[test]
    fn the_openrc_sink_directory_is_made_before_the_supervisor_opens_it() {
        let layout = Layout::default();
        let scripts = [
            ("zbox", openrc_runscript(&state("eth0"), &layout)),
            ("zbox-panel", panel_openrc_runscript(&layout)),
        ];
        for (name, script) in scripts {
            let error_log = script
                .lines()
                .find_map(|line| line.strip_prefix("error_log="))
                .unwrap_or_else(|| panic!("{name} 的 runscript 没有 error_log"));
            let error_log = error_log.trim().trim_matches('"');
            let sink_dir = error_log
                .rsplit_once('/')
                .unwrap_or_else(|| panic!("{name}: {error_log} 不是带目录的路径"))
                .0;
            let in_start_pre = script
                .lines()
                .skip_while(|line| !line.trim_start().starts_with("start_pre()"))
                .take_while(|line| !line.trim().ends_with('}'))
                .find_map(|line| line.trim().strip_prefix("mkdir -p "))
                .unwrap_or_else(|| panic!("{name}: start_pre 里没有建目录那一步\n{script}"));
            assert_eq!(
                in_start_pre,
                netstate::shell_quote(sink_dir),
                "{name}: 建的目录不是 error_log 所在的目录"
            );
        }
    }

    /// 端口与落点都要跟着 `Layout` 走：写死 9090 或 `/var/lib/zbox` 的制品，
    /// 在 `--panel-port` / `--root` 之后就是指到别处去了。
    #[test]
    fn the_panel_artifact_follows_the_layout_not_the_hardcoded_defaults() {
        assert_eq!(
            Layout::default().panel_port,
            DEFAULT_PANEL_PORT,
            "制品里的默认端口必须就是监听层那个默认值，两处一个数才不会有人说一套做一套"
        );
        let layout = Layout {
            root: "/tmp/zbox-scratch/".into(),
            zboxd_bin: "/opt/zbox/zboxd".into(),
            panel_port: 9191,
            ..Layout::default()
        };
        let script = panel_openrc_runscript(&layout);
        assert!(script.contains("command=\"/opt/zbox/zboxd\""), "{script}");
        assert!(
            script.contains("serve --root /tmp/zbox-scratch --init openrc --port 9191"),
            "{script}"
        );
        assert!(
            !script.contains("/tmp/zbox-scratch//"),
            "尾斜杠不许拼出双斜杠：{script}"
        );
        assert!(
            script.contains("error_log=\"/tmp/zbox-scratch/panel.stderr.log\""),
            "{script}"
        );
        let unit = panel_systemd_unit(&layout);
        assert!(
            unit.contains("/opt/zbox/zboxd serve --root /tmp/zbox-scratch"),
            "{unit}"
        );
        assert!(unit.contains("--port 9191"), "{unit}");
        // 地址不写进制品：绑哪儿由 resolve_bind 现探，写死了就是换条网线要重新 install
        for body in [script, unit] {
            assert!(
                !body.contains("--bind"),
                "面板制品里不许出现 --bind：{body}"
            );
        }
    }
}
