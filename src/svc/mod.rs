//! `svc`：真正动机器的那一层。
//!
//! `apply::engine` 只管状态机（快照、指纹、提交、回滚），所有 syscall 级的动作从这里注入：
//!
//! - [`validate`] 把 schema §11.1 那两条"check 放行、run 才炸"的写法挡在生效之前
//!   （§11.6 的预跑门，不占端口、现役内核不用停）；
//! - [`ruleset`] 把远端名单抓回来编成 `.srs` 并验收（§11.7 实测规则集初始化只有约 15 秒
//!   预算，所以这一步是 `apply` 的前置条件，不是可选优化）；
//! - [`provision`] 按"架构 + libc"把钉死版本的官方 sing-box 落地（§6 第 1 步，验收方式是
//!   真跑一次 `sing-box version`，release 里没有 `.sha256` 可抄）；
//! - [`hijack`] 装卸 §11.7 实测过的 nft + 策略路由；
//! - [`observe`] 取 `doctor::Reading` 的七项现场读数；
//! - [`board`] 看板那一格的硬件占用——只读 `/proc`，**一条命令都不发**（与 `observe` 相反，理由写在那里）；
//! - [`dns_api`] 读 oxidns 管理 API（只绑回环那一份）——观测面「DNS 记录」唯一的取数路；
//! - [`topology`] 探本机网卡形状（§6 第 5 步），探不出来就停下问人，不拿默认值往机器上写；
//! - `reload` / `probe` 走 init 系统的服务动词，systemd 与 OpenRC 的差异只在
//!   `gen::init::InitSystem::control_command` 一处收敛。
//! - `sync_rule_sets` / `enable_hijack` / `disable_hijack` / `rollback` 这四个是**动作**，
//!   不是命令：CLI 与面板共用这一份定义，两边只把自己的回执打印或序列化出去。搬下来的理由
//!   写在每个方法的注释里——一句话有两个版本，将来就是"面板说的和 CLI 做的不一样"。
//!
//! 目录布局也是实测结论（§11.7）：快照目录不可变，`-D` 必须是可写状态目录
//! （`Layout::state_dir()`，默认就是 `Layout::root`），本地 `.srs` 与 `cache.db` 都落在那里，
//! 配置一律用绝对路径传给 `-c`。

pub mod board;
pub mod dns_api;
pub mod geo;
pub mod hijack;
pub mod observe;
pub mod provision;
pub mod ruleset;
pub mod runner;
pub mod subscribe;
pub mod topology;
pub mod validate;

use crate::apply::engine::{self, Actions, Target};
use crate::apply::store::{Revision, Store};
use crate::gen::init::{InitSystem, Layout, ServiceAction, PANEL_RUNSCRIPT_PATH, RUNSCRIPT_PATH};
use crate::model::config::Ports;
use crate::svc::hijack::Hijack;
use crate::svc::observe::Observer;
use crate::svc::provision::Provision;
use crate::svc::ruleset::Sync;
use crate::svc::runner::{argv_refs, failure_detail, Runner};
use crate::svc::subscribe::Subscribe;
use crate::svc::validate::Validator;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// `probe` 的总预算。绑定入站是亚秒级动作，慢的是虚拟网卡上的首次拉起；
/// 这一档决定的是"还要不要再试"，"等多久算起不来"由 [`Svc::probe_within`] 的现场时限给。
pub const PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// 预算内的轮询间隔。绑定动作本身是亚秒级的，间隔只要别把 CPU 转起来就行。
const PROBE_POLL: Duration = Duration::from_millis(200);

/// 内核日志的容量上限。这条是被实测的形状逼出来的：1.14.2 的 `log` 对象只有
/// `disabled/level/output/timestamp` 四个键（`sing-box schema` 里 `$defs/LogOptions`，
/// `additionalProperties: false`），**没有任何轮转配置**，而 `log.output` 是**追加**打开的
/// ——旧内容原样留着，两次启动后 4 行变 7 行（schema §11.8.3）。不兜底就是一个在路由器上
/// 只增不减的文件。
pub const LOG_CAP: u64 = 512 * 1024;

pub struct Svc<'a> {
    layout: &'a Layout,
    init: InitSystem,
    runner: &'a dyn Runner,
    ports: &'a Ports,
}

impl<'a> Svc<'a> {
    pub fn new(
        layout: &'a Layout,
        init: InitSystem,
        runner: &'a dyn Runner,
        ports: &'a Ports,
    ) -> Self {
        Self {
            layout,
            init,
            runner,
            ports,
        }
    }

    /// 交给 `apply::engine::apply` 的三个动作。
    pub fn actions(&self) -> Actions<'_> {
        Actions::new(
            |snapshot| self.validate(snapshot),
            || self.reload(),
            || self.probe(),
        )
    }

    pub fn layout(&self) -> &'a Layout {
        self.layout
    }

    /// 这一台机器上动机器的那一层用的**同一份** runner。
    ///
    /// 给出去只为一件事：`svc::topology` 那一次网卡探测要发 `ip` 两条命令，而探网卡的那台机器
    /// 必须和下发的那台是同一台——各处自己造一个 runner，将来就是"面板探到的网卡与它下发的不是
    /// 一台机器"这种没人能复现的形状。
    pub fn runner(&self) -> &'a dyn Runner {
        self.runner
    }

    pub fn init(&self) -> InitSystem {
        self.init
    }

    /// 修订仓库：`<root>/revisions`。
    ///
    /// 从 `zboxd` 命令行里挪到这里，是为了让 CLI 和面板（`src::api`）用的是**同一个定义**——
    /// 两边各自拼一次路径，将来错开就是"面板列出的修订 `apply` 找不到"这种查起来很痛的形状。
    pub fn store(&self) -> Store {
        Store::new(std::path::Path::new(&self.layout.root).join("revisions"))
    }

    /// 快照切换器。生产环境用符号链接（`rename` 覆盖同名链接在 POSIX 上是原子的），
    /// 开发机跑不到那条分支，用整目录复制代替。
    pub fn target(&self) -> Box<dyn Target> {
        #[cfg(unix)]
        let t: Box<dyn Target> = Box::new(engine::SymlinkTarget::new(&self.layout.current_dir));
        #[cfg(not(unix))]
        let t: Box<dyn Target> = Box::new(engine::CopyTarget::new(&self.layout.current_dir));
        t
    }

    pub fn validate(&self, snapshot: &std::path::Path) -> Result<(), String> {
        Validator::new(self.layout, self.runner).gate(snapshot)
    }

    /// sing-box 不原地重载配置，所以 reload = restart。
    /// 这条映射在 `gen::init` 里也留了痕迹（systemd 单元刻意没有 ExecReload）。
    pub fn reload(&self) -> Result<(), String> {
        // 换内核之前先把日志兜住容量：实测 `log.output` 是追加打开、而 1.14.2 没有任何
        // 轮转配置，重启越频繁攒得越快（见 [`LOG_CAP`]）。
        self.rotate_kernel_log()?;
        let argv = self.init.control_command(ServiceAction::Restart);
        let result = self.runner.exec(&argv_refs(&argv))?;
        if !result.succeeded() {
            return Err(format!(
                "重启数据面失败：{}",
                failure_detail(&result.merged(), result.code)
            ));
        }
        Ok(())
    }

    /// 容量兜底：日志超过 [`LOG_CAP`] 就整份挪成 `<name>.1`（再超一次把它覆盖），内核随后
    /// 重开一份新的。放在重启**之前**：旧进程握着原 inode，rename 之后它顶多往 `.1` 里补几行，
    /// 不会写进新文件——和 §11.8.2 换二进制是同一个道理。
    ///
    /// 只留一档是刻意的：这是路由器上的状态目录，不是日志系统；上限因此是 2 x `LOG_CAP`。
    pub fn rotate_kernel_log(&self) -> Result<(), String> {
        let log_path = self.layout.kernel_log_path();
        let path = std::path::Path::new(&log_path);
        let size = match std::fs::metadata(path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("读 {} 失败: {e}", path.display())),
        };
        if size <= LOG_CAP {
            return Ok(());
        }
        let name = path
            .file_name()
            .ok_or_else(|| format!("{} 没有文件名，无法轮转", path.display()))?
            .to_string_lossy()
            .into_owned();
        let prev = path.with_file_name(format!("{name}.1"));
        std::fs::rename(path, &prev).map_err(|e| {
            format!(
                "内核日志已经 {size} 字节，挪去 {} 却失败：{e}（状态目录不可写，数据面大概也起不来）",
                prev.display()
            )
        })
    }

    /// 生效性探测：**服务在活 + 入站真在听**。
    ///
    /// 刻意不去"境外域名该打不开"：§11.7 实测过，出网本身不通时长得和分流失效一模一样，
    /// 那种判据只会把网络故障报成配置故障。真正的分流判据在 `doctor` 里按
    /// "DNS 答案变了 + 日志判决轨迹"来给。
    pub fn probe(&self) -> Result<(), String> {
        self.probe_within(PROBE_DEADLINE)
    }

    /// `probe` 的可注入时限版本。
    ///
    /// **只看 `ss -lntu`，一个端口都不连。**这条是实测逼出来的硬约束：连一次 TPROXY 端口
    /// 就自激成环——`inbound/tproxy[tproxy-in]: inbound connection to 127.0.0.1:9888`
    /// 后面接 `outbound/direct[direct]: outbound connection to 127.0.0.1:9888`，再进同一个
    /// 入站，半秒写出 350MB 日志、每秒 6MB 地涨（schema §11.8.4）。
    ///
    /// 时限同样是实测逼出来的：`rc-service zbox restart` 返回 0 的那一刻 sing-box 还没把
    /// 入站绑上，首次安装的现场是 `服务报告正常，但这些入站没在听：DNS(:53)、TPROXY(:9888)、
    /// REDIRECT(:9887)、混合入站(:8888)`，而几秒后 `ss -lntu` 里这四个端口全在。
    /// 一次性的读数会把"还没起来"读成"起不来"，进而触发根本没有良好快照可回滚的失败。
    pub fn probe_within(&self, budget: Duration) -> Result<(), String> {
        let argv = self.init.control_command(ServiceAction::Status);
        let result = self.runner.exec(&argv_refs(&argv))?;
        if !result.succeeded() {
            return Err(format!(
                "服务未处于活动状态：{}",
                failure_detail(&result.merged(), result.code)
            ));
        }
        let want: [(&str, u16); 4] = [
            ("DNS", self.ports.dns),
            ("TPROXY", self.ports.tproxy),
            ("REDIRECT", self.ports.redirect),
            ("混合入站", self.ports.mixed),
        ];
        let observer = Observer::new(self.runner, self.init);
        let deadline = Instant::now() + budget;
        // 读不到在听端口时宁可等到预算用尽、响亮地判"读不到"，也不给别的结论
        // ——见 observe.rs 开头那条 None 约定。
        let (silent, readable) = loop {
            let outcome = match observer.listening() {
                Some(ports) => {
                    let silent: Vec<String> = want
                        .iter()
                        .filter(|(_, port)| !ports.contains(port))
                        .map(|(what, port)| format!("{what}(:{port})"))
                        .collect();
                    (silent, true)
                }
                None => {
                    let all: Vec<String> = want.iter().map(|(w, p)| format!("{w}(:{p})")).collect();
                    (all, false)
                }
            };
            if outcome.0.is_empty() || Instant::now() >= deadline {
                break outcome;
            }
            std::thread::sleep(PROBE_POLL);
        };
        if !readable {
            return Err(
                "判不了生效性：`ss -lntu` 没有跑成（iproute2 不在？），探针拒绝把\"读不到\"当\"没起来\""
                    .to_string(),
            );
        }
        if !silent.is_empty() {
            return Err(format!(
                "服务报告正常，但等了 {}s 这些入站仍没在听：{}（多半是端口被占或入站没生成）",
                budget.as_secs(),
                silent.join("、")
            ));
        }
        Ok(())
    }

    /// 劫持层。开启前必须先过 `probe`——顺序反了就是 §11.7 里那个 `Connection refused`。
    pub fn hijack(&self) -> Hijack<'a> {
        Hijack::new(self.runner, &self.layout.singbox_bin)
    }

    /// 现场读数，喂给 `doctor::diagnose`。
    pub fn observe(&self) -> Observer<'a> {
        Observer::new(self.runner, self.init)
    }

    /// 现场确实没有 ZBox 的劫持层：nft 表与策略路由**两**路探针都明确回答"没加载"。
    ///
    /// `disable-hijack` 用它把"这台机器本来就没开过劫持"变成空操作。否则在快照被裁掉、
    /// 或 `--root` 指错的机器上，实测只会看到 `zboxd: netstate-down.sh 执行失败：
    /// sh: can't open '.../current/netstate-down.sh'`——一条对谁都不可读的错误。
    /// 只要有一路答"未知"就不许断定没事，仍然走真卸载或响亮地失败。
    pub fn hijack_absent(&self) -> bool {
        let reading = self.observe().read();
        reading.nft_table_loaded == Some(false) && reading.divert_rule_loaded == Some(false)
    }

    /// 规则集落地。`apply` 之前跑一次，冷启动才不必赌那 15 秒（§11.7）。
    pub fn ruleset(&self) -> Sync<'a> {
        Sync::new(self.layout, self.runner)
    }

    /// 订阅抓取（只到"读成节点"为止）。落盘的那一步不在这里——面板那一份磁盘指纹
    /// 只有 `src/api` 拿着，合并与写盘在那里做。
    pub fn subscription(&self) -> Subscribe<'a> {
        Subscribe::new(self.layout, self.runner)
    }

    /// 数据面就位（§6 第 1 步）：按架构 + libc 选官方资产，验收过了才顶掉现役二进制。
    pub fn provision(&self) -> Provision<'a> {
        Provision::new(self.layout, self.runner)
    }

    /// dae 的 geo 数据就位（#56）：把 geoip.dat 与 geosite.dat 放到 dae 找得到的目录里。
    ///
    /// 与 `provision` 分开的原因：dae 二进制本身还没有自动 provision（#43/#69 的前置），
    /// 但 geo 数据可以先手动放进去。等 dae provision 落地后这两步会合到一起。
    pub fn geo(&self) -> geo::GeoData {
        geo::GeoData::default_target()
    }

    /// 抓取 -> 编译 -> 验收 -> 记清单，**并把"声明了但磁盘上没有"升成硬失败**。
    ///
    /// 原来这一段长在 `bin/zboxd.rs::sync_rule_sets` 里。面板要用同一个动作就得把那句
    /// "把 .srs 放进 <目录>，或去掉这条声明"再抄一遍，而两个定义迟早错开——所以现在它住在这里，
    /// CLI 与 API 都只负责把自己的回执形状打印/序列化出去。
    ///
    /// 硬缺口与硬失败是分开的两类（[`SyncError`]）：面板要把它们显示成不同的码、
    /// 不同的话，因为它们要求人做的两件事完全不同——一类去放文件或删声明，另一类去查网络。
    /// 而"哪一类"这个判定只许有一个版本：CLI 与 API 各自看一遍报告再猜，迟早猜得不一样。
    pub fn sync_rule_sets(
        &self,
        state: &crate::model::config::DesiredState,
        now: u64,
        force: bool,
    ) -> Result<ruleset::Report, SyncError> {
        let report = self
            .ruleset()
            .sync(state, now, force)
            .map_err(SyncError::Hard)?;
        if report.is_clean() {
            return Ok(report);
        }
        Err(SyncError::Gap {
            detail: format!(
                "这些规则集声明了但文件不在：{}。把 .srs 放进 {}（相对 sing-box 的 -D），\
                 或在面板里去掉这条声明——ZBox 不去猜别人的本地文件从哪来。",
                report.local_missing.join("、"),
                self.layout.rule_set_dir()
            ),
            report,
        })
    }

    /// 装载劫持层：先确认内核真在听，再 `nft -c` 自证、再下 `netstate.sh` + `zbox.nft`。
    ///
    /// **这一条不下发配置**（与 `zboxd enable-hijack` 的差别只在于那条会先 apply 一次）：
    /// 顺序是硬的——内核没在听就下 nft，局域网侧的表现是 `Connection refused`（§11.7），
    /// 而"要不要重启数据面"和"要不要开劫持"是两个人该分开按的按钮。
    pub fn enable_hijack(&self) -> Result<(), HijackGate> {
        self.probe().map_err(HijackGate::NotListening)?;
        let current = std::path::Path::new(&self.layout.current_dir);
        let hijack = self.hijack();
        hijack
            .check(current)
            .map_err(|detail| HijackGate::Load(detail.into()))?;
        hijack
            .enable(current)
            .map_err(|detail| HijackGate::Load(detail.into()))
    }

    /// 撤掉劫持层。返回 `false` 表示**现场本来就没有**：只跑了读探针，一条卸载命令都没发。
    ///
    /// 判据是 [`Svc::hijack_absent`]：两路探针都明确回答"没加载"才敢断定没事，
    /// 只要有一路是"读不到"就仍然走真卸载（然后大概率响亮地失败），不静默成功。
    pub fn disable_hijack(&self) -> Result<bool, String> {
        if self.hijack_absent() {
            return Ok(false);
        }
        let current = std::path::Path::new(&self.layout.current_dir);
        self.hijack().disable(current)?;
        Ok(true)
    }

    /// 回到某个修订：`None` = 已知良好那一份。提交 -> 重启 -> 探测，与 CLI 同一条定义。
    ///
    /// 挪到这一层的原因和 `sync_rule_sets` 一样：面板也要能回滚，而"给的修订号不存在时
    /// 按新到旧列出全部现有修订"这句话只许有一个版本。
    pub fn rollback(&self, revision: Option<&str>) -> Result<Rollback, RollbackError> {
        let store = self.store();
        // 走 list() 而不是 get(id)：list 只认 `<毫秒>-<十六进制>` 形状的目录，
        // 传进来的字符串再怎么写也拼不出别的路径。
        let known = store.list();
        let good = store.last_good();
        let (rev, how) = match revision {
            Some(id) => (
                known.iter().find(|r| r.id == id).ok_or_else(|| {
                    RollbackError::NotFound(format!(
                        "没有修订 {id}；现有的（新到旧）：{}",
                        revision_list(&known)
                    ))
                })?,
                RollbackHow::Specified,
            ),
            None => (
                good.as_ref().ok_or_else(|| {
                    RollbackError::NotFound(
                        "没有已知良好快照，回不了滚；检查一遍现场后重新 apply".into(),
                    )
                })?,
                RollbackHow::LastGood,
            ),
        };
        let id = rev.id.clone();
        self.target()
            .commit(&rev.path)
            .map_err(RollbackError::Failed)?;
        self.reload().map_err(RollbackError::Failed)?;
        self.probe().map_err(RollbackError::Failed)?;
        Ok(Rollback { id, how })
    }

    /// 安装：写服务制品 + 执行启用命令。制品是绝对路径的一次性写入，不进每次下发的快照。
    ///
    /// `start_service=false` 时只把服务**登记**到运行级别，跳过最后那条拉起/重启命令。
    /// 首次安装走这一条：此刻 `<current>/config.json` 还不存在，而 restart 面对不存在的路径
    /// **照样 rc=0**（实测见 `InitSystem::start_command` 的注释与 schema §11.8.9）。
    /// 紧随其后的下发配置会带着真配置重启，所以那条命令除了制造一个"成功但数据面为空"的
    /// 读数之外没有任何作用。
    ///
    /// 跳掉的只有**数据面**那一条：面板的拉起照跑，因为面板不需要任何配置就能开门
    /// （`state.json` 不在就以基线形状呈现，令牌不在就当场生成一份），而"装完就连得上面板"
    /// 正是一键配置的另一半。理由全文见 `InitSystem::panel_start_command`。
    pub fn install(
        &self,
        artifacts: &BTreeMap<String, String>,
        start_service: bool,
    ) -> Result<Vec<Vec<String>>, String> {
        let mut commands = self.write_artifacts(artifacts)?;
        if !start_service {
            let start = self.init.start_command();
            commands.retain(|argv| *argv != start);
        }
        let mut ran = Vec::new();
        for argv in commands {
            let result = self.runner.exec(&argv_refs(&argv))?;
            if !result.succeeded() {
                return Err(format!(
                    "`{}` 失败：{}",
                    argv.join(" "),
                    failure_detail(&result.merged(), result.code)
                ));
            }
            ran.push(argv);
        }
        Ok(ran)
    }

    /// 只写制品，把**本来要执行**的启用命令原样交回，一条都不跑。
    ///
    /// `--prefix` 试跑走这条：启用命令是按服务名寻址的（`rc-update add zbox default`、
    /// `systemctl enable zbox`），制品没落在 `/etc/init.d`、`/etc/systemd/system` 里时
    /// 它们面对的就是一个不存在的服务，报错只会让人以为逻辑坏了。
    pub fn stage(&self, artifacts: &BTreeMap<String, String>) -> Result<Vec<Vec<String>>, String> {
        self.write_artifacts(artifacts)
    }

    fn write_artifacts(
        &self,
        artifacts: &BTreeMap<String, String>,
    ) -> Result<Vec<Vec<String>>, String> {
        for (path, body) in artifacts {
            // 目录本身也补上：Alpine 最小安装里没有 /etc/sysctl.d 是常见情况
            if let Some(parent) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("建 {} 失败: {e}", parent.display()))?;
            }
            std::fs::write(path, body).map_err(|e| format!("写 {path} 失败: {e}"))?;
        }
        let mut commands = Vec::new();
        // runscript 不可执行 rc-service 会直接拒；chmod 跟着真正写入的那个路径走，
        // 制品被挪去别处（试跑前缀）时也不会去 chmod 一个空位置。
        // **两份**都要：漏掉面板那份的表现是 install 一路打印"已写入"，最后
        // `rc-service zbox-panel restart` 报服务不存在——而那条是这一步最后一条命令。
        for runscript in artifacts.keys().filter(|path| is_runscript(path)) {
            commands.push(vec!["chmod".into(), "+x".into(), runscript.clone()]);
        }
        commands.extend(self.init.enable_commands());
        Ok(commands)
    }
}

/// 这两份制品是 shell 脚本，要执行位；systemd 单元不是，给了也不看。
fn is_runscript(path: &str) -> bool {
    [RUNSCRIPT_PATH, PANEL_RUNSCRIPT_PATH]
        .iter()
        .any(|unit| path.ends_with(unit.trim_start_matches('/')))
}

/// [`Svc::sync_rule_sets`] 的失败分两类，对面要做的两件事不同，所以 HTTP 码也不同：
/// - `Gap`：配置声明了本地没有的规则集文件。**这不是同步能修的**——要么人把 `.srs` 放进去，
///   要么在面板里删掉这条声明。对应 409（状态与磁盘的形状对不上）。
/// - `Hard`：抓取 / 编译 / 落盘这一步真失败了，对应 500。
///
/// 两类都带**已经落地的部分报告**（`ruleset::SyncFailure` 那条注释里记的现场：吞掉部分结果
/// 的表现是"点了更新没反应"），所以每一类都有 `report`。
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("{detail}")]
    Gap {
        detail: String,
        report: ruleset::Report,
    },
    #[error(transparent)]
    Hard(#[from] ruleset::SyncFailure),
}

impl SyncError {
    /// 失败时也有一部分内容：CLI 打印它、面板序列化它，两边都从这一个地方取。
    pub fn report(&self) -> &ruleset::Report {
        match self {
            Self::Gap { report, .. } => report,
            Self::Hard(failure) => &failure.report,
        }
    }
}

/// [`Svc::rollback`] 的结果：回到哪一份、以及它是**怎么被选中的**。
///
/// `how` 不是装饰：CLI 打印的"已回到指定修订 / 已回到已知良好修订"和面板回执里那句话必须
/// 是同一个判定的两个写法，而不是两边各自猜一次调用者有没有给修订号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rollback {
    pub id: String,
    pub how: RollbackHow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackHow {
    Specified,
    LastGood,
}

impl RollbackHow {
    pub fn word(self) -> &'static str {
        match self {
            Self::Specified => "指定",
            Self::LastGood => "已知良好",
        }
    }
}

/// [`Svc::rollback`] 的失败分两类，因为对面要做的两件事完全不同：
/// `NotFound` 是"你给的修订号不存在 / 根本没有可回滚的东西"——人要去挑另一个修订；
/// `Failed` 是"提交、重启或探测其中之一坏了"——人要去查数据面。合成一句就等于两条都没法行动。
#[derive(Debug, thiserror::Error)]
pub enum RollbackError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
}

/// [`Svc::enable_hijack`] 的闸门分两类，同理：先确认内核真在听，那一类失败的意思是
/// "别在这里查 nft，去查数据面"；后面两条（`nft -c` 自证、真装载）失败才是劫持层自己的事。
/// 这个区分不是洁癖——§11.7 实测过顺序反了现场长什么样：客户端 `Connection refused`，
/// 而 nft 规则看着一条都不缺。
#[derive(Debug, thiserror::Error)]
pub enum HijackGate {
    #[error("内核没有在听，先把数据面弄起来再开劫持：{0}")]
    NotListening(String),
    #[error("劫持层没能装载：{0}")]
    Load(String),
}

/// 修订号给错时那句话里的清单。空仓库要说"一个都没有"，不能打印一个空字符串——
/// 那句话在现场是唯一的线索。
fn revision_list(revisions: &[Revision]) -> String {
    if revisions.is_empty() {
        return "一个都没有".to_string();
    }
    revisions
        .iter()
        .map(|rev| rev.id.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 规则集到期判定用的时钟。取不到系统时间就按 0：后果是"全都还没到期"，不会误抓。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svc::runner::{Exec, Scripted};
    use std::net::TcpListener;

    fn ports() -> Ports {
        Ports::default()
    }

    fn exec(code: i32, stdout: &str) -> Exec {
        Exec {
            code,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    #[test]
    fn reload_and_probe_use_the_detected_init_systems_verbs() {
        for init in [InitSystem::Systemd, InitSystem::OpenRC] {
            let layout = Layout::default();
            let ports = ports();
            let runner = Scripted::with_results(vec![
                Ok(exec(0, "active\n")),
                Ok(exec(
                    0,
                    &ss_table(&[ports.dns, ports.tproxy, ports.redirect, ports.mixed]),
                )),
            ]);
            let svc = Svc::new(&layout, init, &runner, &ports);
            svc.reload().unwrap();
            svc.probe().unwrap();
            let argv = runner.argv();
            assert_eq!(argv[0], init.control_command(ServiceAction::Restart));
            assert_eq!(argv[1], init.control_command(ServiceAction::Status));
        }
    }

    #[test]
    fn a_service_that_says_active_but_listens_on_nothing_is_a_failure() {
        // "配置下发成功、但什么都没代理"的最小复现：状态命令 rc=0，四个入站一个都没在听
        let layout = Layout::default();
        let ports = ports();
        let runner = Scripted::with_results(vec![
            Ok(exec(0, "active\n")),
            Ok(exec(0, &ss_table(&[22]))), // 现场只有 sshd
        ]);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);
        let err = svc
            .probe_within(Duration::from_millis(300))
            .expect_err("四口全无该判失败");
        assert!(err.contains("服务报告正常"), "{err}");
        assert!(err.contains("DNS"), "{err}");
    }

    #[test]
    fn a_probe_that_cannot_read_sockets_refuses_to_guess() {
        let layout = Layout::default();
        let ports = ports();
        let runner = Scripted::with_results(vec![
            Ok(exec(0, "active\n")),
            Err("`ss`: 这台机器上没有 iproute2".to_string()),
        ]);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);
        let err = svc
            .probe_within(Duration::from_millis(300))
            .expect_err("读不到在听端口就不许下结论");
        assert!(err.contains("读不到"), "{err}");
        assert!(
            !err.contains("多半是端口被占"),
            "把\"读不到\"报成\"没起来\"是最坏的一种错"
        );
    }

    /// 探针只能看、不能摸：实测连一次 TPROXY 端口就自激成环，半秒写出 350MB 日志
    /// （schema §11.8.4）。这里让四个端口真的有人听着，判完之后一家都不许被连过。
    #[test]
    fn the_probe_reads_sockets_and_connects_to_nothing() {
        let layout = Layout::default();
        let listeners: Vec<TcpListener> = (0..4)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let bound: Vec<u16> = listeners
            .iter()
            .map(|l| l.local_addr().unwrap().port())
            .collect();
        let mut ports = ports();
        ports.dns = bound[0];
        ports.tproxy = bound[1];
        ports.redirect = bound[2];
        ports.mixed = bound[3];
        let runner = Scripted::with_results(vec![
            Ok(exec(0, "active\n")),
            Ok(exec(0, &ss_table(&bound))),
        ]);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);
        svc.probe().unwrap();

        for listener in &listeners {
            listener.set_nonblocking(true).unwrap();
            let touched = match listener.accept() {
                // 一次都没连：队列空，WouldBlock
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
                // 连上过（哪怕已经断开，accept 会带 ECONNRESET 回来）
                _ => true,
            };
            assert!(
                !touched,
                "探针连过 :{}，会自激成环",
                listener.local_addr().unwrap()
            );
        }
    }

    /// `ss -lntu` 的列形状照 2026-09-30 guest 实测原文（见 observe.rs 的 SS fixture）。
    fn ss_table(ports: &[u16]) -> String {
        let mut out =
            String::from("Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port\n");
        for port in ports {
            out.push_str(&format!(
                "tcp   LISTEN 0      4096         0.0.0.0:{port:<5}    0.0.0.0:*\n"
            ));
        }
        out
    }

    /// 首次安装的现场：状态命令先回 `started`，入站要过一会儿才绑上。
    /// 一次性检查会把"还没起来"读成"起不来"，进而要求回滚一份还不存在的良好快照。
    #[test]
    fn a_port_that_opens_after_a_beat_is_not_reported_as_dead() {
        let layout = Layout::default();
        let ports = ports();
        let bound = ss_table(&[ports.dns, ports.tproxy, ports.redirect, ports.mixed]);
        let runner = LateSs {
            opened_at: Instant::now() + Duration::from_millis(400),
            bound,
            calls: std::cell::RefCell::new(Vec::new()),
        };
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);
        svc.probe_within(Duration::from_secs(4))
            .expect("预算内绑上的端口不算死");
        assert!(
            runner.ss_reads() >= 2,
            "第一次读不到就该重试，而不是直接判死"
        );
    }

    /// 现场实测的形状：服务状态先回 `started`，`ss` 要过一会儿才看得到入站。
    struct LateSs {
        opened_at: Instant,
        bound: String,
        calls: std::cell::RefCell<Vec<Vec<String>>>,
    }

    impl LateSs {
        fn ss_reads(&self) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|call| call.first().map(String::as_str) == Some("ss"))
                .count()
        }
    }

    impl Runner for LateSs {
        fn exec(&self, argv: &[&str]) -> Result<Exec, String> {
            self.calls
                .borrow_mut()
                .push(argv.iter().map(|a| a.to_string()).collect());
            if argv.first() == Some(&"ss") {
                let body = if Instant::now() >= self.opened_at {
                    self.bound.clone()
                } else {
                    // 只有 sshd：四个入站还没绑上
                    String::from(
                        "Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port\ntcp   LISTEN 0      128          0.0.0.0:22        0.0.0.0:*\n",
                    )
                };
                return Ok(exec(0, &body));
            }
            Ok(exec(0, "started\n"))
        }

        fn exec_stdin(&self, _argv: &[&str], _stdin: &[u8]) -> Result<Exec, String> {
            Err("这条测试不经过标准输入".to_string())
        }

        fn until_started(
            &self,
            _argv: &[&str],
            _redirect: &std::path::Path,
            _kernel_log: Option<&std::path::Path>,
            _deadline: Duration,
        ) -> Result<(), String> {
            Err("这条测试不预跑".to_string())
        }
    }

    #[test]
    fn inactive_service_is_reported_before_any_port_is_probed() {
        let layout = Layout::default();
        let runner = Scripted::with_results(vec![Ok(exec(3, "inactive\n"))]);
        let ports = ports();
        let svc = Svc::new(&layout, InitSystem::Systemd, &runner, &ports);
        assert!(svc.probe().unwrap_err().contains("未处于活动状态"));
        assert_eq!(runner.argv().len(), 1, "状态不对就不必去读在听端口");
    }

    #[test]
    fn install_writes_artifacts_then_runs_enable_commands_in_order() {
        for init in [InitSystem::OpenRC, InitSystem::Systemd] {
            let layout = Layout::default();
            let state = crate::model::config::DesiredState::baseline();
            let artifacts = crate::gen::init::artifacts(init, &state, &layout);
            let dir = std::env::temp_dir().join(format!("zbox-svc-install-{}", service_tag(init)));
            let mapped = relocate(&artifacts, &dir);
            let runner = Scripted::new();
            let ports = ports();
            let svc = Svc::new(&layout, init, &runner, &ports);
            let ran = svc.install(&mapped, true).unwrap();

            let mut want: Vec<Vec<String>> = Vec::new();
            if init == InitSystem::OpenRC {
                // runscript 落在哪儿就 chmod 哪儿，绝不 chmod 硬编码的 /etc 路径。
                // **两份**：数据面与面板各一份，顺序就是制品表的键序。
                for written in mapped
                    .keys()
                    .filter(|p| is_runscript(p))
                    .cloned()
                    .collect::<Vec<_>>()
                {
                    want.push(vec!["chmod".into(), "+x".into(), written]);
                }
                assert_eq!(
                    want.len(),
                    2,
                    "两份 runscript 都该拿到执行位：{:?}",
                    mapped.keys()
                );
            }
            want.extend(init.enable_commands());
            assert_eq!(ran, want, "{init:?} 的启用序列");
            for path in mapped.keys() {
                assert!(std::path::Path::new(path).is_file(), "{path} 没写进去");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// 首次安装（还没有任何配置）只登记服务、不拉起：那条 restart 在配置缺失时**不会**报错，
    /// 而是 rc=0 + `status: started` + 零监听（guest 实测，schema §11.8.9）。宁可少跑一条命令，
    /// 也不要留一条"执行成功而数据面为空"的读数。
    #[test]
    fn first_run_registers_the_service_but_does_not_start_it() {
        for init in [InitSystem::OpenRC, InitSystem::Systemd] {
            let layout = Layout::default();
            let state = crate::model::config::DesiredState::baseline();
            let artifacts = crate::gen::init::artifacts(init, &state, &layout);
            let dir = std::env::temp_dir().join(format!("zbox-svc-firstrun-{}", service_tag(init)));
            let mapped = relocate(&artifacts, &dir);
            let runner = Scripted::new();
            let ports = ports();
            let svc = Svc::new(&layout, init, &runner, &ports);

            let ran = svc.install(&mapped, false).unwrap();
            let start = init.start_command();
            assert!(
                !ran.contains(&start),
                "{init:?} 首次安装不该跑 {start:?}：{:?}",
                ran.iter().map(|a| a.join(" ")).collect::<Vec<_>>()
            );
            // 面板那一条**要**跑：它不等配置，装完就该有人能连上它写第一份状态
            let panel_start = init.panel_start_command();
            assert!(
                ran.contains(&panel_start),
                "{init:?} 首次安装也该把面板开起来，少了 {panel_start:?}：{:?}",
                ran.iter().map(|a| a.join(" ")).collect::<Vec<_>>()
            );
            let chmods = usize::from(init == InitSystem::OpenRC) * 2;
            assert_eq!(
                ran.len(),
                init.enable_commands().len() - 1 + chmods,
                "{init:?} 只该少掉数据面那一条拉起命令，其余照跑：{:?}",
                ran
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// `--prefix` 试跑：制品照样写，启用命令一条都不跑，只原样交回给人看。
    /// 那些命令是按服务名寻址的，制品不在 `/etc` 里时跑它们只会得到"服务不存在"。
    #[test]
    fn stage_writes_artifacts_but_runs_nothing() {
        let layout = Layout::default();
        let state = crate::model::config::DesiredState::baseline();
        let artifacts = crate::gen::init::artifacts(InitSystem::OpenRC, &state, &layout);
        let dir = std::env::temp_dir().join("zbox-svc-stage");
        let mapped = relocate(&artifacts, &dir);
        let runner = Scripted::new();
        let ports = ports();
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);

        let planned = svc.stage(&mapped).unwrap();
        assert!(
            runner.argv().is_empty(),
            "stage 不该执行任何命令：{:?}",
            runner.argv()
        );
        assert_eq!(
            planned.len(),
            2 + InitSystem::OpenRC.enable_commands().len()
        );
        // 两份 runscript 都拿到执行位，顺序按落点排
        assert_eq!(planned[0][0], "chmod", "{:?}", planned[0]);
        assert_eq!(planned[1][0], "chmod", "{:?}", planned[1]);
        assert!(
            planned[0][2].ends_with("etc/init.d/zbox"),
            "{:?}",
            planned[0][2]
        );
        assert!(
            planned[1][2].ends_with("etc/init.d/zbox-panel"),
            "{:?}",
            planned[1][2]
        );
        assert!(
            planned[2].starts_with(&["rc-update".to_string(), "add".to_string()]),
            "{:?}",
            planned[2]
        );
        for path in mapped.keys() {
            assert!(std::path::Path::new(path).is_file(), "{path} 没写进去");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn service_tag(init: InitSystem) -> &'static str {
        match init {
            InitSystem::OpenRC => "openrc",
            InitSystem::Systemd => "systemd",
        }
    }

    /// 实测的形状：1.14.2 的 `log` 没有轮转配置，而 `log.output` 是追加打开的
    /// （旧内容原样留着，第二次启动后 4 行变 7 行，schema §11.8.3），所以容量得由我们兜。
    #[test]
    fn an_over_cap_kernel_log_is_set_aside_before_the_restart() {
        let dir = std::env::temp_dir().join("zbox-svc-logrotate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let layout = Layout {
            root: dir.display().to_string(),
            ..Layout::default()
        };
        let runner = Scripted::new();
        let ports = ports();
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);
        let log = std::path::PathBuf::from(layout.kernel_log_path());
        let prev = std::path::PathBuf::from(format!("{}.1", log.display()));

        // 还没写过日志：不该凭空造文件
        svc.rotate_kernel_log().unwrap();
        assert!(!log.exists(), "轮转不该建一个空日志");

        // 没到上限：原样留着
        std::fs::write(&log, "还没起来").unwrap();
        svc.rotate_kernel_log().unwrap();
        assert!(log.is_file(), "没超就不许动");
        assert!(!prev.exists());

        // 超了：整份挪成 .1，新的一份交给内核重开
        let body = "x".repeat(LOG_CAP as usize + 1);
        std::fs::write(&log, &body).unwrap();
        svc.rotate_kernel_log().unwrap();
        assert!(!log.exists());
        assert_eq!(std::fs::metadata(&prev).unwrap().len(), body.len() as u64);

        // 再超一次：顶掉那一份 .1。容量因此有界在 2 x LOG_CAP
        std::fs::write(&log, &body).unwrap();
        svc.rotate_kernel_log().unwrap();
        assert_eq!(std::fs::metadata(&prev).unwrap().len(), body.len() as u64);
        assert_eq!(runner.argv().len(), 0, "轮转不该跑任何外部命令");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 把 `/etc/...` 形式的制品路径挪进临时目录，模拟 CLI 的 `--prefix`。
    fn relocate(
        artifacts: &BTreeMap<String, String>,
        dir: &std::path::Path,
    ) -> BTreeMap<String, String> {
        artifacts
            .iter()
            .map(|(path, body)| {
                let target = dir.join(path.trim_start_matches('/'));
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                (target.display().to_string(), body.clone())
            })
            .collect()
    }

    #[test]
    fn observe_shares_the_runner_and_the_detected_init_system() {
        let layout = Layout::default();
        let runner = Scripted::with_results(vec![
            Ok(exec(0, "")),
            Ok(exec(
                0,
                "tcp   LISTEN 0      4096         0.0.0.0:53        0.0.0.0:*\n",
            )),
        ]);
        let ports = ports();
        let svc = Svc::new(&layout, InitSystem::Systemd, &runner, &ports);
        let reading = svc.observe().read();
        assert_eq!(reading.service_active, Some(true));
        assert_eq!(reading.listening, Some(vec![53]));
        assert_eq!(
            runner.argv()[0],
            InitSystem::Systemd.control_command(ServiceAction::Status),
            "第一个读数问的就是本机 init 系统的状态动词"
        );
    }

    #[test]
    fn ruleset_sync_shares_the_layout_and_the_runner() {
        let layout = Layout::default();
        let runner = Scripted::new();
        let ports = ports();
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);
        // 清单落在状态目录下，与 sing-box 的 `-D` 同一个基准（§11.7）
        assert_eq!(
            svc.ruleset().manifest_path(),
            std::path::Path::new("/var/lib/zbox/rule-set").join("manifest.json")
        );
    }

    #[test]
    fn provision_shares_the_layout_and_the_runner() {
        let dir = std::env::temp_dir().join("zbox-svc-provision");
        let bin = dir.join("bin");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&bin).unwrap();
        let layout = Layout {
            singbox_bin: bin.join("sing-box").display().to_string(),
            ..Layout::default()
        };
        let runner = Scripted::with_results(vec![
            Ok(exec(0, "Linux\n")),
            Ok(exec(0, "aarch64\n")),
            Ok(exec(0, "ld-musl-aarch64.so.1\n")),
            Ok(exec(1, "")),
            Ok(exec(0, "sing-box version 1.13.0\n")),
            Ok(exec(0, "sing-box version 1.14.2\n")),
        ]);
        let ports = ports();
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);
        let outcome = svc
            .provision()
            .install(
                &crate::svc::provision::Source::Asset(bin.join("asset")),
                false,
            )
            .unwrap_err();
        // 资产对不对由验收说话，这里只确认走的是同一个 layout 与 runner
        assert!(matches!(
            outcome,
            crate::svc::provision::ProvisionError::Asset { .. }
        ));
        assert_eq!(
            runner.argv()[1],
            vec!["uname", "-m"],
            "判目标问的是本机，不是开发机"
        );
        assert_eq!(
            runner.argv()[2],
            vec!["ls", "/lib"],
            "探测走 runner，不是在开发机上读自己的 /lib"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Observer::read()` 七路探针的顺序：服务状态、ss、nft 表、ip rule、两个 sysctl、默认路由。
    fn field(loaded_nft: Result<Exec, String>, rule: &str) -> Vec<Result<Exec, String>> {
        vec![
            Ok(exec(1, "inactive\n")),
            Ok(exec(0, "UNCONN 0 0 127.0.0.1:22\n")),
            loaded_nft,
            Ok(exec(0, rule)),
            Ok(exec(0, "1\n")),
            Ok(exec(0, "2\n")),
            Ok(exec(0, "default via 192.168.219.2 dev eth0\n")),
        ]
    }

    #[test]
    fn hijack_absent_needs_both_probes_to_answer_no() {
        let layout = Layout::default();
        let ports = ports();
        let clean = || -> Vec<Result<Exec, String>> {
            field(
                Err("nft 这个二进制跑不起来".to_string()),
                "0:\tfrom all lookup main\n32000:\tfrom all lookup default\n",
            )
        };
        // nft 对不存在的表 rc=1（实测），ip rule 里没有 fwmark 那一条
        let runner = Scripted::with_results({
            let mut all = clean();
            all[2] = Ok(exec(1, "Error: No such file or directory\n"));
            all
        });
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);
        assert!(svc.hijack_absent(), "两路都说没有，才算确实没装");

        // 表在 -> 不能说没事
        let runner = Scripted::with_results({
            let mut all = clean();
            all[2] = Ok(exec(0, "table ip zbox { }\n"));
            all
        });
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);
        assert!(!svc.hijack_absent(), "nft 表在听见就别断定没事");

        // 有一路答"未知"（nft 命令压根跑不了）-> 不许断定没事
        let runner = Scripted::with_results(clean());
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);
        assert!(
            !svc.hijack_absent(),
            "探针跑不了就是未知，未知不许被读成\"本来就没装\""
        );

        // 策略路由还在（哪怕表不在了）-> 也不算空
        let runner = Scripted::with_results({
            let mut all = clean();
            all[2] = Ok(exec(1, ""));
            all[3] = Ok(exec(0, "0x1:100:\tfrom all fwmark 0x1 lookup 100\n"));
            all
        });
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &ports);
        assert!(!svc.hijack_absent(), "半撤的现场更得走真卸载");
    }
}
