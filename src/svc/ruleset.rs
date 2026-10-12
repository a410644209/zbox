//! 规则集落地：把 `DesiredState.rule_sets` 里声明的远端来源抓回来、编译成 `.srs`，
//! 落在 `Layout::rule_set_dir()`（也就是 sing-box `-D` 基准下的 `rule-set/`）。
//!
//! 语法归一那半在 `crate::ingest::ruleset`（纯函数，已全测覆盖），这里只做五件事：
//! 抓取、执行 `sing-box rule-set compile`、验收、记清单、回收已删除声明留下的产物。
//!
//! 顺序上是 `apply` 的前置条件而不是优化：VM 实测（schema §11.7）里规则集初始化有
//! 约 15 秒硬预算，10s 打 WARN、~16s 直接 FATAL，而 guest 当天抓 GitHub 就超时过一次。
//! 预置本地 `.srs` 之后 `initial_path` 才有的意义，冷启动才不必赌网络。
//!
//! 验收分两级，形状来自实测（§11.8.9，2026-10-05 真二进制逐条抓过）：
//! - **文件是不是合法的 rule-set**：`rule-set match --format binary <f> <probe>` 的 rc。
//!   坏文件 `FATAL[0000] invalid sing-box rule-set file` rc=1，文件没了
//!   `FATAL[0000] read rule-set: open ...: no such file or directory` rc=1。
//! - **我们放进去的值真在名单里**：同一命令的 **stderr**——命中信息全在 stderr，
//!   stdout 无论命中与否都是空的。而**没命中也返回 rc=0**，所以这一级只能判输出内容，
//!   判 rc 会把"没编进去"当成通过。（`Exec::merged()` 拼的就是 stdout+stderr。）
//!
//! 任何一级没过都不会覆盖现役产物：新内容一律先落 `<tag>.staging.srs`，
//! 验收通过后 `promote()` 才顶掉旧的；抓取失败时如果旧产物还在，就记成一条 note
//! 而不是把整批同步判死。

use crate::gen::init::Layout;
use crate::ingest::ruleset::{self, Plan, PlanAction};
use crate::model::config::{is_safe_rule_set_tag, rule_set_product, DesiredState, RuleSet};
use crate::svc::runner::{argv_refs, failure_detail, Runner};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 清单文件名，落在 `rule-set/` 目录里。
pub const MANIFEST_NAME: &str = "manifest.json";
/// 抓取上限。实测 guest 直连 GitHub 会超时，给到 60s 是"这条网络慢但没死"的量级。
pub const FETCH_TIMEOUT: &str = "60";

/// 只验文件合法性时用的探针：一个不可能进任何名单的域名。
/// 用它是因为 `match` 一定要一个待匹配值，而这个值保证命中与否都不影响结论——
/// 我们要的只是"rc 反映的是 sing-box 能不能解析这个文件"。
const INTEGRITY_PROBE: &str = "zbox-integrity-check.invalid";

#[derive(Debug, thiserror::Error)]
pub enum RuleSetError {
    #[error("抓取 {url} 失败：{detail}")]
    Fetch { url: String, detail: String },
    #[error("编译规则集 {tag} 失败：{detail}")]
    Compile { tag: String, detail: String },
    #[error("规则集 {tag} 验收未通过：{detail}")]
    Verify { tag: String, detail: String },
    #[error(transparent)]
    Source(#[from] ruleset::RulesetError),
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// 一条已落地规则集的来历。只记"要不要重抓"需要的字段，不做安全摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub url: String,
    /// `ingest::ruleset::SourceFormat` 的调试名，面板按它显示"列表 / 现成规则集 / 二进制"
    pub format: String,
    /// 归一化后的规则条数；直通的 `.srs` 恒为 0（ZBox 不解析 SRS，不做假计数）
    pub rule_count: usize,
    pub bytes: u64,
    /// 落地时刻（epoch 秒）
    pub updated_at: u64,
}

pub type Manifest = BTreeMap<String, Entry>;

/// 陈旧判定。`interval` 解析不出来时**不**判陈旧——配置里写错个单位不该导致每次下发都联网。
pub fn is_stale(entry: Option<&Entry>, interval: &str, now: u64) -> bool {
    let Some(entry) = entry else {
        return true;
    };
    match parse_interval_secs(interval) {
        Some(secs) => now.saturating_sub(entry.updated_at) >= secs,
        None => false,
    }
}

/// sing-box 的 duration 形状：无单位按秒，支持 `s/m/h/d`。
pub fn parse_interval_secs(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (digits, multiplier) = match raw.chars().last()? {
        's' => (&raw[..raw.len() - 1], 1),
        'm' => (&raw[..raw.len() - 1], 60),
        'h' => (&raw[..raw.len() - 1], 3600),
        'd' => (&raw[..raw.len() - 1], 86400),
        _ => (raw, 1),
    };
    digits.parse::<u64>().ok().map(|n| n * multiplier)
}

/// 一份声明在本次同步里要做什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// 需要抓取并落地
    Refresh {
        tag: String,
        url: String,
    },
    /// 清单说没到期**且产物文件真在**
    Kept {
        tag: String,
    },
    /// 用户自带的本地产物，ZBox 不抓取，只确认文件在不在
    LocalPresent {
        tag: String,
        path: String,
    },
    LocalMissing {
        tag: String,
        path: String,
    },
}

/// 纯函数：声明 + 现况清单 + "哪些文件在" -> 本次要做的事。`force` 忽略到期判定全部重抓。
///
/// `exists` 收的是相对状态目录的路径（清单与配置里的路径都是这个基准，见 §11.7）。
/// 新鲜但文件不在也判成 `Refresh`：用户手动清过 `rule-set/` 之后，"清单说装过"不算装过。
pub fn plan_jobs(
    state: &DesiredState,
    manifest: &Manifest,
    now: u64,
    force: bool,
    exists: &dyn Fn(&str) -> bool,
) -> Vec<Job> {
    state
        .rule_sets
        .iter()
        .map(|set| match set {
            RuleSet::Remote {
                tag,
                url,
                update_interval,
                ..
            } => {
                let product = rule_set_product(tag);
                if force || !exists(&product) || is_stale(manifest.get(tag), update_interval, now) {
                    Job::Refresh {
                        tag: tag.clone(),
                        url: url.clone(),
                    }
                } else {
                    Job::Kept { tag: tag.clone() }
                }
            }
            RuleSet::Local { tag, path } if exists(path) => Job::LocalPresent {
                tag: tag.clone(),
                path: path.clone(),
            },
            RuleSet::Local { tag, path } => Job::LocalMissing {
                tag: tag.clone(),
                path: path.clone(),
            },
        })
        .collect()
}

/// 一份声明在本次同步之外还该回收的东西：**清单里有、声明里没有**。
///
/// `product` 只在 tag 过 `is_safe_rule_set_tag` 时给出。清单是磁盘数据的入口，一条被改坏过的
/// 清单不该换来一次任意路径删除——tag 不可信时只销账、不碰文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaim {
    pub tag: String,
    pub product: Option<String>,
}

/// 纯函数：这次该回收哪些孤儿。
///
/// 判据只有清单，不看目录：清单是"ZBox 自己造过什么"的账本，账本外的文件（用户手放的 `.srs`、
/// 别的程序的产物）一律不碰。删掉的 `RuleSet` 声明若不在这里回收，就会留下一个没有任何配置
/// 引用的 `.srs` 加一条永远不再更新的清单条目——面板按清单显示"已装规则集"时会一直骗人。
pub fn plan_reclaim(state: &DesiredState, manifest: &Manifest) -> Vec<Reclaim> {
    manifest
        .keys()
        .filter(|tag| !state.rule_sets.iter().any(|set| set.tag() == **tag))
        .map(|tag| Reclaim {
            tag: tag.clone(),
            product: is_safe_rule_set_tag(tag).then(|| rule_set_product(tag)),
        })
        .collect()
}

/// 本次同步的结果，直接打印给 CLI，也供面板显示。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub refreshed: Vec<String>,
    pub kept: Vec<String>,
    /// 自带产物缺失的 tag：不是 ZBox 能修的，报出来让人放文件
    pub local_missing: Vec<String>,
    /// 软失败：这一条没落地，但现役产物仍然可用（抓取失败沿用旧文件 / 下载器缺失）
    pub notes: Vec<String>,
    /// 已删除声明的回收结果（产物文件 + 清单条目）
    pub reclaimed: Vec<String>,
}

impl Report {
    /// 有没有"配置声明了但磁盘上没有"的硬缺口。
    pub fn is_clean(&self) -> bool {
        self.local_missing.is_empty()
    }
}

/// 硬失败也要把**已经落地的部分**带出去。
///
/// 加这个结构是因为一条 CLI 现场：`sync(...)?` 之前没有打印，于是任何一次硬失败（死源、
/// 状态目录不可写）都把整份报告吞了——"点了更新没反应"。映射报告 §5.3 记了他们那边的
/// 等价物（上游保存返回分级回执），我们这边的等价物就是"部分成功也要说清楚"。
#[derive(Debug)]
pub struct SyncFailure {
    pub report: Report,
    pub source: RuleSetError,
}

impl std::fmt::Display for SyncFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for SyncFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Downloader {
    Curl,
    Wget,
}

pub struct Sync<'a> {
    layout: &'a Layout,
    runner: &'a dyn Runner,
}

impl<'a> Sync<'a> {
    pub fn new(layout: &'a Layout, runner: &'a dyn Runner) -> Self {
        Self { layout, runner }
    }

    pub fn manifest_path(&self) -> PathBuf {
        Path::new(&self.layout.rule_set_dir()).join(MANIFEST_NAME)
    }

    pub fn load_manifest(&self) -> Manifest {
        read_manifest(&self.manifest_path()).unwrap_or_default()
    }

    /// 配置里的相对路径一律以状态目录为基准——它就是 sing-box 的 `-D`（§11.7）。
    fn resolve(&self, path: &str) -> PathBuf {
        Path::new(&self.layout.state_dir()).join(path)
    }

    fn exists(&self, path: &str) -> bool {
        self.resolve(path).is_file()
    }

    /// 抓取 + 编译 + 验收 + 记清单 + 回收。
    ///
    /// 单条失败不影响其余条目：一批规则集里有一条源挂了，不该把另外三条也拖成"没装"。
    /// 已有的旧 `.srs` 在新内容验收通过之前绝不覆盖（`<tag>.staging.srs` + `promote`）。
    ///
    /// 硬失败返回 [`SyncFailure`]，里面带着已经落地的那部分——调用方要先打印它再退出非 0。
    pub fn sync(&self, state: &DesiredState, now: u64, force: bool) -> Result<Report, SyncFailure> {
        let mut report = Report::default();
        match self.sync_entries(state, now, force, &mut report) {
            Ok(()) => Ok(report),
            Err(source) => Err(SyncFailure { report, source }),
        }
    }

    /// `sync` 的实体：报告由外面递进来，所以失败路径上调用方仍拿着部分结果。
    fn sync_entries(
        &self,
        state: &DesiredState,
        now: u64,
        force: bool,
        report: &mut Report,
    ) -> Result<(), RuleSetError> {
        let dir = PathBuf::from(self.layout.rule_set_dir());
        fs::create_dir_all(&dir)?;
        let mut manifest = self.load_manifest();
        // 全本地/全到期的场景一次进程都不该起，所以下载器探到第一次真要抓时才探
        let mut downloader = None;
        let exists = |path: &str| self.exists(path);

        for job in plan_jobs(state, &manifest, now, force, &exists) {
            match job {
                Job::Refresh { tag, url } => {
                    let dl = *downloader.get_or_insert_with(|| detect_downloader(self.runner));
                    let outcome = self.one(&tag, &url, &dir, now, dl);
                    let product = dir.join(format!("{tag}.srs"));
                    match outcome {
                        Ok(entry) => {
                            manifest.insert(tag.clone(), entry);
                            report.refreshed.push(tag);
                            // 每落一条就写一次清单：中途被打断不该让已经落地的事实重新联网
                            write_manifest(&self.manifest_path(), &manifest)?;
                        }
                        Err(e) if product.is_file() => report
                            .notes
                            .push(format!("{tag}: {e}；沿用 {tag}.srs 现有产物，下次到期再试")),
                        // 没有可沿用的产物，放过去就是让 sing-box 启动时现抓（§11.7 的超时）
                        Err(e) => return Err(e),
                    }
                }
                Job::Kept { tag } => report.kept.push(tag),
                Job::LocalPresent { tag, .. } => report.kept.push(tag),
                Job::LocalMissing { tag, path } => {
                    report.local_missing.push(format!("{tag}（{path}）"))
                }
            }
        }

        // 回收放在最后：一次没跑完的同步不该顺手删东西（上面任何一条 `?` 都会跳到这里之外）。
        let mut reclaimed = Vec::new();
        for item in plan_reclaim(state, &manifest) {
            if let Some(product) = item.product.as_deref() {
                let path = self.resolve(product);
                // 文件已经被手动清掉不算故障：要销的是账，账销了下次就不会再找它
                if path.is_file() {
                    fs::remove_file(&path)?;
                }
            }
            manifest.remove(&item.tag);
            reclaimed.push(item.tag);
        }
        if !reclaimed.is_empty() {
            write_manifest(&self.manifest_path(), &manifest)?;
            report.reclaimed = reclaimed;
        }
        Ok(())
    }

    /// 抓一条 -> 归一化 -> 编译到 staging -> 验收 -> 顶掉现役产物。
    fn one(
        &self,
        tag: &str,
        url: &str,
        dir: &Path,
        now: u64,
        downloader: Downloader,
    ) -> Result<Entry, RuleSetError> {
        let body = self.fetch(tag, url, dir, downloader)?;
        let plan = ruleset::prepare(tag, url, &body)?;
        let out = dir.join(format!("{tag}.srs"));
        let staging = out.with_extension("staging.srs");
        let content_probe = match &plan.action {
            PlanAction::Install => {
                fs::write(&staging, &body)?;
                None
            }
            PlanAction::Compile { source_json } => {
                self.compile(&plan, source_json.as_bytes(), &staging)?;
                probe_value(source_json)
            }
        };
        let outcome = self
            .verify(tag, &staging, content_probe.as_deref())
            .and_then(|bytes| {
                promote(&staging, &out)
                    .map_err(RuleSetError::from)
                    .map(|_| bytes)
            });
        if outcome.is_err() {
            // 没过验收的 staging 不是任何配置引用的路径，留着只是垃圾
            let _ = fs::remove_file(&staging);
        }
        Ok(Entry {
            url: url.to_string(),
            format: format!("{:?}", plan.format),
            rule_count: plan.rule_count,
            bytes: outcome?,
            updated_at: now,
        })
    }

    /// 下到 `<tag>.download` 再读回来：现役产物在本次同步结束前不可变。
    fn fetch(
        &self,
        tag: &str,
        url: &str,
        dir: &Path,
        downloader: Downloader,
    ) -> Result<Vec<u8>, RuleSetError> {
        let tmp = dir.join(format!("{tag}.download"));
        download_body(self.runner, downloader, url, &tmp, FetchLimits::SOURCE).map_err(|detail| {
            RuleSetError::Fetch {
                url: url.to_string(),
                detail,
            }
        })
    }

    /// `sing-box rule-set compile stdin -o <staging>`：源 JSON 走管道，不落可预测的临时名。
    fn compile(&self, plan: &Plan, source_json: &[u8], staging: &Path) -> Result<(), RuleSetError> {
        // `compile_args()` 的末位恒是 `-o` 的目标（ingest 侧有测试钉住这个形状）。这里换成
        // 绝对落点：相对路径会按 CWD 解析，而 CWD 不是 `-D`（§11.7 那条 `no such file` 假故障）。
        let mut args = plan.compile_args();
        if let Some(last) = args.last_mut() {
            *last = staging.display().to_string();
        }
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(self.layout.singbox_bin.as_str());
        argv.extend(argv_refs(&args));
        let result =
            self.runner
                .exec_stdin(&argv, source_json)
                .map_err(|e| RuleSetError::Compile {
                    tag: plan.tag.clone(),
                    detail: e,
                })?;
        if !result.succeeded() {
            return Err(RuleSetError::Compile {
                tag: plan.tag.clone(),
                detail: failure_detail(&result.merged(), result.code),
            });
        }
        Ok(())
    }

    /// 两级验收。返回校验过的文件字节数（写进清单，用于"下载回来的东西塌没塌"）。
    fn verify(
        &self,
        tag: &str,
        out: &Path,
        content_probe: Option<&str>,
    ) -> Result<u64, RuleSetError> {
        let path_text = out.display().to_string();
        let argv: Vec<&str> = vec![
            self.layout.singbox_bin.as_str(),
            "rule-set",
            "match",
            "--format",
            "binary",
            &path_text,
            content_probe.unwrap_or(INTEGRITY_PROBE),
        ];
        let result = self.runner.exec(&argv).map_err(|e| RuleSetError::Verify {
            tag: tag.to_string(),
            detail: e,
        })?;
        let merged = result.merged();
        if !result.succeeded() {
            return Err(RuleSetError::Verify {
                tag: tag.to_string(),
                detail: failure_detail(&merged, result.code),
            });
        }
        match content_probe {
            // 只有我们自己生成的名单才敢要求命中。未命中时 stdout 与 stderr 都是空的而 rc 仍是 0
            // （实测），所以这一级判的是 merged 输出里有没有那句命中信息。
            Some(probe) if !merged.contains("match rules") => Err(RuleSetError::Verify {
                tag: tag.to_string(),
                detail: format!(
                    "探针 {probe} 在编译产物里查不到（命令 rc=0 但无输出）；\
                     这份源没被完整编进去，不能当成有效名单下发"
                ),
            }),
            _ => Ok(fs::metadata(out)?.len()),
        }
    }
}

/// 用哪个下载器。`command -v` 是 shell 内建，`Runner` 不经 shell，所以只能拿真二进制探。
/// 探不到就退回 wget——guest 实测只有 busybox wget。两边都不存在时由抓取本身报错。
/// 做成自由函数是因为 `svc::provision` 抓 sing-box 二进制要用同一个探测器。
pub(crate) fn detect_downloader(runner: &dyn Runner) -> Downloader {
    match runner.exec(&["curl", "--version"]) {
        Ok(exec) if exec.succeeded() => Downloader::Curl,
        _ => Downloader::Wget,
    }
}

/// 用给定下载器把 `url` 抓到 `tmp`、读回来、删掉暂存件。错误就是那一步的原因，不带 URL。
///
/// 从 [`Sync::fetch`] 里抽出来是因为订阅抓取要走同一段：下载器的判据（curl 在位与否、
/// busybox wget 的 `-T` 是空闲上限而不是总时长、`-O` 后面那个位置）全部来自 guest 实测，
/// 抄第二份就等于给同一件事准备两份会各自漂移的写法。**这里不判内容对不对**——
/// 规则集要编译验收，订阅要解析成节点，那一半各自在调用方。
pub(crate) fn download_body(
    runner: &dyn Runner,
    downloader: Downloader,
    url: &str,
    tmp: &Path,
    limits: FetchLimits,
) -> Result<Vec<u8>, String> {
    let _ = fs::remove_file(tmp);
    let args = downloader_args(downloader, url, tmp, limits);
    let result = runner
        .exec(&argv_refs(&args))
        .map_err(|e| format!("{e}（命令：`{}`）", args.join(" ")))?;
    if !result.succeeded() {
        let _ = fs::remove_file(tmp);
        return Err(failure_detail(&result.merged(), result.code));
    }
    let body = match fs::read(tmp) {
        Ok(body) => body,
        Err(e) => {
            // 抓回来的东西读不回就删掉暂存件：订阅原文里全是凭据，留着比多一句报错贵。
            let _ = fs::remove_file(tmp);
            return Err(format!(
                "下载器退出正常，但 {} 里没有文件：{e}",
                tmp.display()
            ));
        }
    };
    let _ = fs::remove_file(tmp);
    if body.is_empty() {
        return Err("下载到 0 字节".to_string());
    }
    Ok(body)
}

/// 用暂存件顶掉现役产物。任何时刻 `out` 要么是没动的旧文件、要么是完整的新文件。
pub(crate) fn promote(staging: &Path, out: &Path) -> std::io::Result<()> {
    if fs::rename(staging, out).is_ok() {
        return Ok(());
    }
    // 改名不成的两个常见原因：跨文件系统（sing-box 二进制在 `/usr/local/bin`、暂存在
    // `/var/lib/zbox`），以及 Windows 开发机上"目标已存在不许覆盖"。Linux 上 rename 本身就是
    // 原子替换，走不到这条。统一用"拷到旁边再改名"收口——直接往 `out` 拷会留下半截文件。
    let side = out.with_extension("zbox-side");
    let landed = (|| -> std::io::Result<()> {
        fs::copy(staging, &side)?;
        if out.exists() {
            fs::remove_file(out)?;
        }
        fs::rename(&side, out)
    })();
    if landed.is_err() {
        let _ = fs::remove_file(&side);
        return landed;
    }
    let _ = fs::remove_file(staging);
    Ok(())
}

/// 从我们**自己生成的**源 JSON 里取一个必然在名单中的值当验收探针。
///
/// 只探第一个匹配项就够：它证明"这份源真被编进去了"，而不是证明整份名单语义正确
/// （后者是 §11.7 那轮 netns 实测的事）。取不到值时不探，退化成"文件合法"这一级。
///
/// 刻意不探 `ip_cidr`：2026-10-05 拿真二进制测过（schema §11.8.9），命中形状是
/// `match rules.[0]: ip_cidr=<binary>`、未命中两条流都空、rc 恒 0——判据本身够用，但
/// **待匹配值必须是一个成员地址**：拿 CIDR 本身（`1.2.4.0/24`）去探，`geoip-cn` 里明明有这条
/// 段却判未命中。从段里取哪个地址是猜测（`+1`？`/32` 才恰好成立），不如停在"文件合法"这一级。
pub fn probe_value(source_json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(source_json).ok()?;
    for key in ["domain", "domain_suffix", "domain_keyword"] {
        if let Some(list) = value
            .get("rules")
            .and_then(Value::as_array)
            .and_then(|rules| rules.iter().find_map(|rule| rule.get(key)))
            .and_then(Value::as_array)
        {
            if let Some(first) = list.first().and_then(Value::as_str) {
                // domain_suffix 的探针要拼成一个真实查询名，裸后缀匹配不到自己
                let probe = match key {
                    "domain_suffix" => format!("www.{first}"),
                    _ => first.to_string(),
                };
                return Some(probe);
            }
        }
    }
    None
}

pub fn read_manifest(path: &Path) -> Result<Manifest, RuleSetError> {
    if !path.is_file() {
        return Ok(Manifest::new());
    }
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|e| RuleSetError::Other(format!("清单解析失败: {e}")))
}

/// 清单写坏不算事故：`load_manifest` 对坏文件按"空清单"处理，后果只是全部重抓。
pub fn write_manifest(path: &Path, manifest: &Manifest) -> Result<(), RuleSetError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        serde_json::to_string_pretty(manifest).unwrap_or_default(),
    )?;
    Ok(())
}

/// 抓取时限。两个值分开给，因为两条下载器的同名概念根本不是一回事：curl 的 `--max-time`
/// 是**整条传输的总上限**，busybox wget 的 `-T` 是**网络空闲上限**。
#[derive(Debug, Clone, Copy)]
pub struct FetchLimits {
    pub curl_total: &'static str,
    pub wget_idle: &'static str,
}

impl FetchLimits {
    /// 名单/规则集源：几 KB 到几 MB。curl 那边 `-fsSL --max-time 60` 是**总时长**；
    /// busybox wget 的 `-T` 却是**每阶段空闲**上限，而且它**没有 `-t/--tries`**
    /// （1.37.0 的 usage 里只有 `-T SEC`，写了 `-t 1` 是静默空操作）。
    /// 2026-10-05 guest 实测：GitHub 的 TLS 链不通时，一条源用 `-T 60` 抓了一次烧掉 **196 秒**
    /// ——连接、TLS、读各算一次空闲超时。所以下限收到 20 秒：正常下载实测 1-2 秒完成，
    /// 慢链（33KB/s 那种数据持续到达的）也不会被空闲判死，却能把死链最坏耗时压到约一分钟。
    pub const SOURCE: FetchLimits = FetchLimits {
        curl_total: FETCH_TIMEOUT,
        wget_idle: "20",
    };
    /// 31MB 的 sing-box 资产：curl 给足总时长，wget 的空闲上限反而要**短**——guest 实测
    /// 连接会整段停住，`-T 60` 那次白等了 168 秒才报 `download timed out`（schema §11.8）。
    pub const ASSET: FetchLimits = FetchLimits {
        curl_total: "900",
        wget_idle: "20",
    };
}

/// 抓取用哪个下载器：guest 实测只有 busybox wget（`command -v curl` 为空）。
pub(crate) fn downloader_args(
    downloader: Downloader,
    url: &str,
    out: &Path,
    limits: FetchLimits,
) -> Vec<String> {
    let out = out.display().to_string();
    match downloader {
        Downloader::Curl => vec![
            "curl".into(),
            "-fsSL".into(),
            "--max-time".into(),
            limits.curl_total.into(),
            "-o".into(),
            out,
            url.into(),
        ],
        // busybox wget：`-q` 只在出错时说话
        Downloader::Wget => vec![
            "wget".into(),
            "-q".into(),
            "-T".into(),
            limits.wget_idle.into(),
            "-O".into(),
            out,
            url.into(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svc::runner::Exec;
    use serde_json::json;
    use std::cell::RefCell;

    #[test]
    fn interval_parsing_covers_the_shapes_the_model_emits() {
        assert_eq!(parse_interval_secs("168h"), Some(604_800));
        assert_eq!(parse_interval_secs("30m"), Some(1800));
        assert_eq!(parse_interval_secs("7d"), Some(604_800));
        assert_eq!(parse_interval_secs("120"), Some(120), "无单位按秒");
        assert_eq!(parse_interval_secs(""), None);
        assert_eq!(parse_interval_secs("每周"), None);
    }

    #[test]
    fn freshness_is_decided_by_the_manifest_not_by_file_existence() {
        let entry = entry_at("https://x/a.srs", 1_000);
        assert!(!is_stale(Some(&entry), "168h", 1_000 + 604_799));
        assert!(is_stale(Some(&entry), "168h", 1_000 + 604_800));
        assert!(is_stale(None, "168h", 0), "清单里没有就是要抓");
        // 单位写错时不联网、也不报错，交给 force
        assert!(!is_stale(Some(&entry), "168小时", 999_999));
    }

    fn remote(tag: &str, interval: &str) -> RuleSet {
        RuleSet::Remote {
            tag: tag.into(),
            url: format!("https://x/{tag}.srs"),
            initial_path: format!("rule-set/{tag}.srs"),
            update_interval: interval.into(),
        }
    }

    /// `remote` 拼出来的 URL 是不存在的 `.srs`；要走文本解析分支的测试得自己指名那份 body
    fn remote_at(tag: &str, url: &str) -> RuleSet {
        RuleSet::Remote {
            tag: tag.into(),
            url: url.into(),
            initial_path: format!("rule-set/{tag}.srs"),
            update_interval: "168h".into(),
        }
    }

    fn local(tag: &str) -> RuleSet {
        RuleSet::Local {
            tag: tag.into(),
            path: format!("rule-set/{tag}.srs"),
        }
    }

    fn state_with(sets: Vec<RuleSet>) -> DesiredState {
        DesiredState {
            rule_sets: sets,
            ..DesiredState::baseline()
        }
    }

    fn entry_at(url: &str, updated_at: u64) -> Entry {
        Entry {
            url: url.into(),
            format: "BinaryRuleSet".into(),
            rule_count: 0,
            bytes: 46,
            updated_at,
        }
    }

    /// 三种声明各判一次：新鲜且在位（Kept）、清单里没有（Refresh）、自带产物（在/不在由探针给）
    #[test]
    fn jobs_split_into_refresh_keep_and_local() {
        let state = state_with(vec![
            remote("geoip-cn", "168h"),
            remote("geosite-cn", "168h"),
            local("mine"),
        ]);
        let fresh = 2_000u64;
        let manifest = Manifest::from([("geoip-cn".into(), entry_at("https://x/a.srs", fresh))]);
        let all_there = |_: &str| true;
        let jobs = plan_jobs(&state, &manifest, fresh, false, &all_there);
        assert_eq!(
            jobs,
            vec![
                Job::Kept {
                    tag: "geoip-cn".into()
                },
                Job::Refresh {
                    tag: "geosite-cn".into(),
                    url: "https://x/geosite-cn.srs".into(),
                },
                Job::LocalPresent {
                    tag: "mine".into(),
                    path: "rule-set/mine.srs".into()
                },
            ],
            "{jobs:?}"
        );
        // 清单新鲜但产物被删了 -> 仍然重抓，"清单说装过"不算装过
        let all_gone = |_: &str| false;
        let jobs = plan_jobs(&state, &manifest, fresh, false, &all_gone);
        assert!(matches!(jobs[0], Job::Refresh { .. }), "{jobs:?}");
        assert!(
            matches!(jobs[2], Job::LocalMissing { .. }),
            "自带产物不见了要报出来，那不是 ZBox 能抓回来的东西：{jobs:?}"
        );
        // force 只影响远端条目，本地产物本来就不抓
        let forced = plan_jobs(&state, &manifest, fresh, true, &all_there);
        assert!(matches!(forced[0], Job::Refresh { .. }), "{forced:?}");
        assert!(matches!(forced[2], Job::LocalPresent { .. }), "{forced:?}");
    }

    #[test]
    fn probe_prefers_a_domain_value_and_builds_a_query_name() {
        let json = r#"{"version":5,"rules":[{"domain_suffix":["example.com"]},{"ip_cidr":["1.2.3.0/24"]}]}"#;
        assert_eq!(probe_value(json).as_deref(), Some("www.example.com"));
        let json = r#"{"rules":[{"domain":["a.com"]}]}"#;
        assert_eq!(probe_value(json).as_deref(), Some("a.com"));
        // CIDR 只有唯一一条时不探：`match` 该拿什么形态的待匹配值去查 IP 型规则集，没实测过
        assert_eq!(
            probe_value(r#"{"rules":[{"ip_cidr":["114.114.114.114/32"]}]}"#),
            None
        );
        // 只有正则的名单没法保证必然命中，同样退回"只验文件合法"
        assert_eq!(
            probe_value(r#"{"rules":[{"domain_regex":["^a.*"]}]}"#),
            None
        );
        assert_eq!(probe_value("not json"), None);
    }

    #[test]
    fn downloader_choice_matches_what_the_guest_actually_has() {
        let args = downloader_args(
            Downloader::Wget,
            "https://x/a.srs",
            Path::new("/tmp/a.download"),
            FetchLimits::SOURCE,
        );
        assert_eq!(args[0], "wget", "Alpine guest 里没有 curl（实测）");
        assert!(
            args.windows(2).any(|w| w == ["-T", "20"]),
            "wget 只认 `-T`（空闲上限），没有 `-t/--tries`，源这条路径要早失败：{args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "-t"),
            "busybox wget 不认 -t，写进去是静默空操作（2026-10-05 实测）：{args:?}"
        );
        let args = downloader_args(
            Downloader::Curl,
            "https://x/a.srs",
            Path::new("/tmp/a.download"),
            FetchLimits::SOURCE,
        );
        assert_eq!(args[0], "curl");
        assert_eq!(args.last().unwrap(), "https://x/a.srs", "URL 必须在最后");
    }

    #[test]
    fn wget_fails_early_while_curl_keeps_the_full_budget() {
        // 两个下载器的超时语义不同：curl 的 `--max-time` 是总时长，busybox wget 的 `-T` 是
        // **每阶段**空闲时长（guest 实测 1.37.0 只有 `-T SEC`，没有 `-t/--tries`）。
        // 31MB 资产整段停住那次 `-T 60` 白等 168 秒（schema §11.8），而 2026-10-05 一条
        // TLS 不通的源用 `-T 60` 烧了 196 秒（连接/TLS/读各一次）——所以两边都把 wget 的
        // 空闲上限压到 20，让 curl 拿满时长。
        let wget = downloader_args(
            Downloader::Wget,
            "https://x/sing-box.tar.gz",
            Path::new("/tmp/a.download"),
            FetchLimits::ASSET,
        );
        assert!(
            wget.windows(2).any(|w| w == ["-T", "20"]),
            "wget 的空闲上限要短：{wget:?}"
        );
        let curl = downloader_args(
            Downloader::Curl,
            "https://x/sing-box.tar.gz",
            Path::new("/tmp/a.download"),
            FetchLimits::ASSET,
        );
        assert!(
            curl.windows(2).any(|w| w == ["--max-time", "900"]),
            "curl 的总时长要给足：{curl:?}"
        );
    }

    #[test]
    fn manifest_round_trips_and_tolerates_absence() {
        let path = std::env::temp_dir().join("zbox-ruleset-manifest-test.json");
        let _ = fs::remove_file(&path);
        assert_eq!(read_manifest(&path).unwrap(), Manifest::new());
        let mut m = Manifest::new();
        m.insert("geoip-cn".into(), entry_at("https://x/a.srs", 7));
        write_manifest(&path, &m).unwrap();
        assert_eq!(read_manifest(&path).unwrap(), m);
        // 坏清单读得出错，但 `load_manifest` 那层按空清单处理 = 全部重抓，不会卡死
        fs::write(&path, "{ not json").unwrap();
        assert!(read_manifest(&path)
            .unwrap_err()
            .to_string()
            .contains("清单解析失败"));
        let _ = fs::remove_file(&path);

        let dir = temp_dir("bad-manifest");
        let layout = layout_at(&dir);
        let sandbox = Sandbox::new(&[]);
        let sync = Sync::new(&layout, &sandbox);
        fs::create_dir_all(dir.join("rule-set")).unwrap();
        fs::write(sync.manifest_path(), "{ not json").unwrap();
        assert_eq!(sync.load_manifest(), Manifest::new());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn promote_replaces_an_existing_product_and_leaves_the_rest_alone() {
        let dir = temp_dir("promote");
        let out = dir.join("a.srs");
        fs::write(&out, b"old").unwrap();
        fs::write(dir.join("a.staging.srs"), b"new").unwrap();
        promote(dir.join("a.staging.srs").as_path(), &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"new");
        assert!(!dir.join("a.staging.srs").exists(), "staging 不该留下");
        let _ = fs::remove_dir_all(dir);
    }

    // ---- 下面这组用 Sandbox 走完整条 sync，判据形状全部照 §11.8.9 的真二进制实测抄 ----

    /// 模仿 `curl`/`wget` 与 `sing-box rule-set` 的**文件副作用**，其余行为照实测抄：
    /// - 下载器把响应体写进 `-o` 目标；URL 不在 `bodies` 里就是 rc=22（模拟 404/超时）
    /// - `compile stdin -o P` 把 stdin 原样存成 P（真 compile 存的是 `.srs`，这里让
    ///   sandbox 的 `match` 读得懂就行，语义等价）
    /// - `match --format binary P probe`：坏文件 rc=1；命中与否 rc 都是 0，
    ///   命中信息在 **stderr**，stdout 恒空（§11.8.9 实测）
    struct Sandbox {
        bodies: BTreeMap<&'static str, Vec<u8>>,
        curl_ok: bool,
        /// 置真时所有 match 都"命中不了"，照抄未命中那条实测形状
        blind: bool,
        calls: RefCell<Vec<Vec<String>>>,
        payloads: RefCell<Vec<Vec<u8>>>,
    }

    const MATCH_HIT: &str = "match rules.[0]: domain/domain_suffix=<binary>\n";
    const BAD_FILE: &str = "FATAL[0000] invalid sing-box rule-set file\n";

    impl Sandbox {
        fn new(bodies: &[(&'static str, &[u8])]) -> Self {
            Self {
                bodies: bodies
                    .iter()
                    .map(|(url, body)| (*url, body.to_vec()))
                    .collect(),
                curl_ok: true,
                blind: false,
                calls: RefCell::new(Vec::new()),
                payloads: RefCell::new(Vec::new()),
            }
        }

        fn argv(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }

        fn payloads(&self) -> Vec<Vec<u8>> {
            self.payloads.borrow().clone()
        }

        /// 位置参数里 `-o`/`-O` 后面那个
        fn output<'a>(argv: &'a [&'a str]) -> Option<&'a str> {
            argv.iter()
                .position(|a| *a == "-o" || *a == "-O")
                .and_then(|i| argv.get(i + 1).copied())
        }

        fn is_sub(argv: &[&str], what: &str) -> bool {
            argv.iter().any(|a| *a == what)
        }

        /// `rule-set match` 的数据参数固定在 5/6 位（0 是二进制名，verify 就按这个顺序拼）
        fn match_args<'a>(argv: &[&'a str]) -> Option<(&'a str, &'a str)> {
            let path = argv.get(5)?;
            let probe = argv.get(6)?;
            Some((*path, *probe))
        }
    }

    impl Runner for Sandbox {
        fn exec(&self, argv: &[&str]) -> Result<Exec, String> {
            self.calls
                .borrow_mut()
                .push(argv.iter().map(|a| a.to_string()).collect());
            if argv.first().copied() == Some("curl") && argv.get(1).copied() == Some("--version") {
                // 真代码只把 rc=0 当 curl 存在；127 与"起不来"都会退成 wget
                return if self.curl_ok {
                    Ok(exec(0, "curl 8.0\n", ""))
                } else {
                    Ok(exec(127, "", "curl: not found"))
                };
            }
            if matches!(argv.first().copied(), Some("curl") | Some("wget")) {
                let out =
                    Self::output(argv).ok_or_else(|| "sandbox: 下载命令没有 -o".to_string())?;
                let url = *argv.last().unwrap();
                return match self.bodies.get(url) {
                    Some(body) => {
                        fs::write(out, body).map_err(|e| e.to_string())?;
                        Ok(exec(0, "", ""))
                    }
                    None => Ok(exec(
                        22,
                        "",
                        "curl: (22) The requested URL returned error: 504",
                    )),
                };
            }
            if Self::is_sub(argv, "compile") {
                // compile 的源 JSON 只能从标准输入进，走到这里说明接线接错了
                return Ok(exec(1, "", BAD_FILE));
            }
            if Self::is_sub(argv, "match") {
                let Some((path, probe)) = Self::match_args(argv) else {
                    return Ok(exec(1, "", "FATAL[0000] insufficient arguments\n"));
                };
                let Ok(data) = fs::read(path) else {
                    return Ok(exec(
                        1,
                        "",
                        "FATAL[0000] read rule-set: open: no such file\n",
                    ));
                };
                if data.starts_with(b"CORRUPT") || data.is_empty() {
                    return Ok(exec(1, "", BAD_FILE));
                }
                let hit = !self.blind
                    && match serde_json::from_str::<Value>(&String::from_utf8_lossy(&data)) {
                        // 直通的二进制产物不是 JSON：文件合法，但我们不知道里面有什么，判未命中
                        Err(_) => false,
                        Ok(value) => hits(&value, probe),
                    };
                // 命中与否 rc 都是 0，差别只在**stderr**里有没有 `match rules...`——
                // 这条是 2026-10-05 真二进制实测的形状（§11.8.9），stdout 两条都是空的
                return Ok(exec(0, "", if hit { MATCH_HIT } else { "" }));
            }
            Ok(exec(0, "", ""))
        }

        fn exec_stdin(&self, argv: &[&str], stdin: &[u8]) -> Result<Exec, String> {
            self.payloads.borrow_mut().push(stdin.to_vec());
            self.calls
                .borrow_mut()
                .push(argv.iter().map(|a| a.to_string()).collect());
            if !Self::is_sub(argv, "compile") {
                return Err("sandbox: 只有 compile 用标准输入".to_string());
            }
            let out = Self::output(argv).ok_or_else(|| "sandbox: compile 没有 -o".to_string())?;
            fs::write(out, stdin).map_err(|e| e.to_string())?;
            Ok(exec(0, "", ""))
        }

        fn until_started(
            &self,
            _argv: &[&str],
            _redirect: &Path,
            _kernel_log: Option<&Path>,
            _deadline: std::time::Duration,
        ) -> Result<(), String> {
            unreachable!("规则集同步不该碰预跑门")
        }
    }

    /// 只实现验收探针需要的那几种匹配语义（后缀要拼成真查询名才命中）。
    fn hits(value: &Value, probe: &str) -> bool {
        let Some(rules) = value.get("rules").and_then(Value::as_array) else {
            return false;
        };
        for rule in rules {
            let list = |key: &str| -> Vec<&str> {
                rule.get(key)
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default()
            };
            if list("domain").iter().any(|v| *v == probe) {
                return true;
            }
            if list("domain_suffix")
                .iter()
                .any(|v| probe == *v || probe.ends_with(&format!(".{v}")))
            {
                return true;
            }
            if list("domain_keyword").iter().any(|v| probe.contains(v)) {
                return true;
            }
        }
        false
    }

    fn exec(code: i32, stdout: &str, stderr: &str) -> Exec {
        Exec {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zbox-ruleset-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn layout_at(root: &Path) -> Layout {
        Layout {
            singbox_bin: "sing-box".into(),
            current_dir: format!("{}/current", root.display()),
            root: root.display().to_string(),
            ..Layout::default()
        }
    }

    const AD_BODY: &[u8] = b"! comment\n||example.com^\n||baidu.com^\n";
    const AD_URL: &str = "https://x/ads.txt";

    #[test]
    fn a_list_source_lands_as_a_compiled_srs_and_is_recorded() {
        let dir = temp_dir("land");
        let layout = layout_at(&dir);
        let sandbox = Sandbox::new(&[(AD_URL, AD_BODY)]);
        let sync = Sync::new(&layout, &sandbox);
        let state = state_with(vec![remote_at("ads", AD_URL)]);
        let report = sync.sync(&state, 1234, false).unwrap();

        assert_eq!(report.refreshed, vec!["ads".to_string()]);
        assert!(report.notes.is_empty(), "{report:?}");
        assert!(dir.join("rule-set/ads.srs").is_file());
        assert!(
            !dir.join("rule-set/ads.download").exists(),
            "下载临时文件要清掉"
        );
        assert!(
            !dir.join("rule-set/ads.staging.srs").exists(),
            "staging 顶替后就该消失"
        );
        let argv = sandbox.argv();
        // curl 探 -> 下载 -> compile(stdin) -> match(内容) 四步，一步不多
        assert_eq!(argv[0], ["curl", "--version"], "{argv:?}");
        assert_eq!(argv[1][0], "curl");
        assert_eq!(argv[1].last().unwrap(), AD_URL);
        let out = &argv[2][5];
        assert!(
            Path::new(out).is_absolute() && Path::new(out).starts_with(Path::new(&layout.root)),
            "-o 必须是状态目录下的绝对落点，相对路径按 CWD 解析就是 §11.7 那条假故障：{out}"
        );
        assert_eq!(Path::new(out).file_name().unwrap(), "ads.staging.srs");
        assert_eq!(
            argv[2][..5],
            ["sing-box", "rule-set", "compile", "stdin", "-o"]
        );
        assert_eq!(
            argv[3][..5],
            ["sing-box", "rule-set", "match", "--format", "binary"]
        );
        assert_eq!(argv[3][6], "www.example.com", "探针来自我们自己生成的名单");
        let payload: Value = serde_json::from_slice(&sandbox.payloads()[0]).unwrap();
        assert_eq!(
            payload["version"],
            json!(5),
            "compile 要求 version，缺了就 FATAL"
        );
        assert_eq!(
            payload["rules"],
            json!([{"domain_suffix": ["example.com", "baidu.com"]}]),
            "stdin 就是 ingest 归一化出来的源 JSON"
        );

        let manifest = sync.load_manifest();
        let entry = manifest.get("ads").expect("清单要记住这一条");
        assert_eq!(entry.url, AD_URL);
        assert_eq!(entry.format, "AdGuard");
        assert_eq!(entry.rule_count, 2);
        assert_eq!(entry.updated_at, 1234);
        assert!(entry.bytes > 0);

        // 第二次同样参数、没到期：一个进程都不起
        let before = sandbox.argv().len();
        let second = sync.sync(&state, 1235, false).unwrap();
        assert_eq!(second.kept, vec!["ads".to_string()], "{second:?}");
        assert_eq!(
            sandbox.argv().len(),
            before,
            "沿用就不该下载器都不探地问一句"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn wget_is_used_when_the_curl_probe_fails() {
        let dir = temp_dir("wget");
        let layout = layout_at(&dir);
        let mut sandbox = Sandbox::new(&[(AD_URL, AD_BODY)]);
        sandbox.curl_ok = false;
        let sync = Sync::new(&layout, &sandbox);
        sync.sync(&state_with(vec![remote_at("ads", AD_URL)]), 1, false)
            .expect("busybox wget 足够");
        let argv = sandbox.argv();
        assert_eq!(
            argv[1][0], "wget",
            "探测失败要换形状而不是失败退出：{argv:?}"
        );
        assert!(argv[1].windows(2).any(|w| w == ["-T", "20"]));
        assert!(dir.join("rule-set/ads.srs").is_file(), "换下载器不影响落地");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_empty_download_never_reaches_the_parser() {
        let dir = temp_dir("empty");
        let layout = layout_at(&dir);
        let url = "https://x/empty.txt";
        let sandbox = Sandbox::new(&[(url, b"")]);
        let sync = Sync::new(&layout, &sandbox);
        let err = sync
            .sync(&state_with(vec![remote_at("empty", url)]), 1, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("0 字节"), "{err}");
        assert!(!dir.join("rule-set/empty.srs").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_binary_source_keeps_the_good_product_in_place() {
        let dir = temp_dir("corrupt");
        fs::create_dir_all(dir.join("rule-set")).unwrap();
        fs::write(dir.join("rule-set/cn.srs"), b"the-good-46-byte-srs").unwrap();
        let layout = layout_at(&dir);
        let url = "https://x/cn.srs";
        let sandbox = Sandbox::new(&[(url, b"CORRUPT-MAGIC-BYTES")]);
        let sync = Sync::new(&layout, &sandbox);
        let state = state_with(vec![remote("cn", "168h")]);
        let report = sync.sync(&state, 1, false).unwrap();

        assert!(report.refreshed.is_empty(), "{report:?}");
        assert_eq!(report.notes.len(), 1, "{report:?}");
        assert!(report.notes[0].contains("沿用"), "{:?}", report.notes);
        assert_eq!(
            fs::read(dir.join("rule-set/cn.srs")).unwrap(),
            b"the-good-46-byte-srs",
            "坏文件绝不能顶掉现役产物"
        );
        assert!(
            !dir.join("rule-set/cn.staging.srs").exists(),
            "验收没过也不能把 staging 留在目录里"
        );
        assert!(
            sync.load_manifest().is_empty(),
            "没落地的条目不该进清单：{:?}",
            sync.load_manifest()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_silent_match_is_a_failure_because_rc_zero_lied() {
        // 实测：`rule-set match` 未命中也返回 rc=0。只看 rc 就等于把"没编进去"当通过。
        let dir = temp_dir("silent");
        let layout = layout_at(&dir);
        let mut sandbox = Sandbox::new(&[(AD_URL, AD_BODY)]);
        sandbox.blind = true;
        let sync = Sync::new(&layout, &sandbox);
        let err = sync
            .sync(&state_with(vec![remote_at("ads", AD_URL)]), 1, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("探针 www.example.com"), "{err}");
        assert!(!dir.join("rule-set/ads.srs").exists());
        assert!(
            !dir.join("rule-set/ads.staging.srs").exists(),
            "编译成功但内容没编进去，staging 也不能留"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_dead_url_without_any_local_copy_is_a_hard_error() {
        // 没有可沿用的产物时报错，而不是让 sing-box 启动时现抓（§11.7 的 ~15s FATAL）
        let dir = temp_dir("dead");
        let layout = layout_at(&dir);
        let sandbox = Sandbox::new(&[]);
        let sync = Sync::new(&layout, &sandbox);
        let err = sync
            .sync(&state_with(vec![remote("ads", "168h")]), 1, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("https://x/ads.srs"), "{err}");
        assert!(err.contains("504"), "{err}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_local_product_is_reported_not_fetched() {
        let dir = temp_dir("local");
        let layout = layout_at(&dir);
        let sandbox = Sandbox::new(&[]);
        let sync = Sync::new(&layout, &sandbox);
        let report = sync
            .sync(&state_with(vec![local("mine")]), 1, false)
            .unwrap();
        assert_eq!(
            report.local_missing,
            vec!["mine（rule-set/mine.srs）".to_string()]
        );
        assert!(!report.is_clean());
        assert!(
            sandbox.argv().is_empty(),
            "本地产物不该起任何进程，连下载器都不探"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_compiled_payload_is_the_normalized_source_json() {
        let dir = temp_dir("stdin");
        let layout = layout_at(&dir);
        let url = "https://x/pcdn.txt";
        let sandbox = Sandbox::new(&[(url, b"+.smtcdns.com\n192.168.0.0/16\n")]);
        let sync = Sync::new(&layout, &sandbox);
        sync.sync(&state_with(vec![remote_at("pcdn", url)]), 1, false)
            .unwrap();
        let value: Value = serde_json::from_slice(&sandbox.payloads()[0]).unwrap();
        assert_eq!(value["rules"][0]["domain_suffix"], json!(["smtcdns.com"]));
        assert_eq!(value["rules"][1]["ip_cidr"], json!(["192.168.0.0/16"]));
        // 混合源里探域名那条；纯 CIDR 源不探（见 `probe_value` 的说明）
        assert_eq!(sandbox.argv()[3][6], "www.smtcdns.com");
        let _ = fs::remove_dir_all(dir);
    }

    // ---- 回收已删除声明的孤儿产物（§7.3 第 2 条）----

    #[test]
    fn plan_reclaim_lists_only_ledger_tags_that_are_no_longer_declared() {
        let mut manifest = BTreeMap::new();
        manifest.insert("ads".into(), entry_at(AD_URL, 1));
        manifest.insert("gone".into(), entry_at("https://x/gone.txt", 1));
        let plan = plan_reclaim(&state_with(vec![remote_at("ads", AD_URL)]), &manifest);
        assert_eq!(
            plan,
            vec![Reclaim {
                tag: "gone".into(),
                product: Some(rule_set_product("gone")),
            }]
        );
    }

    #[test]
    fn a_removed_declaration_loses_its_product_and_its_ledger_entry() {
        let dir = temp_dir("reclaim");
        let layout = layout_at(&dir);
        fs::create_dir_all(dir.join("rule-set")).unwrap();
        // 账本外的文件：别人放的，回收不该碰
        fs::write(dir.join("rule-set/stray.srs"), b"not-mine").unwrap();
        let extra_url = "https://x/extra.txt";
        let sandbox = Sandbox::new(&[(AD_URL, AD_BODY), (extra_url, b"+.extra.com\n")]);
        let sync = Sync::new(&layout, &sandbox);
        let both = state_with(vec![
            remote_at("ads", AD_URL),
            remote_at("extra", extra_url),
        ]);
        sync.sync(&both, 1, false).unwrap();
        assert!(dir.join("rule-set/extra.srs").is_file());

        let report = sync
            .sync(&state_with(vec![remote_at("ads", AD_URL)]), 2, false)
            .unwrap();
        assert_eq!(report.reclaimed, vec!["extra".to_string()], "{report:?}");
        assert_eq!(report.kept, vec!["ads".to_string()], "沿用那条不该被误伤");
        assert!(
            !dir.join("rule-set/extra.srs").exists(),
            "删掉的声明不该留孤儿产物"
        );
        assert!(dir.join("rule-set/ads.srs").is_file());
        assert_eq!(
            fs::read(dir.join("rule-set/stray.srs")).unwrap(),
            b"not-mine",
            "清单是账本，账本外的文件不是 ZBox 造的"
        );
        let manifest = sync.load_manifest();
        assert!(
            !manifest.contains_key("extra"),
            "{:?}",
            manifest.keys().collect::<Vec<_>>()
        );
        assert!(manifest.contains_key("ads"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unsafe_tag_is_dropped_from_the_ledger_without_touching_the_disk() {
        // 清单是磁盘路径的入口：tag 不可信时只销账，不拿它去删文件。
        let dir = temp_dir("unsafe-tag");
        let layout = layout_at(&dir);
        fs::create_dir_all(dir.join("rule-set")).unwrap();
        let outside = dir.join("evil.srs");
        fs::write(&outside, b"someone-elses-file").unwrap();
        let mut manifest = BTreeMap::new();
        manifest.insert("../evil".into(), entry_at("https://x/evil.srs", 1));
        let sandbox = Sandbox::new(&[]);
        let sync = Sync::new(&layout, &sandbox);
        write_manifest(&sync.manifest_path(), &manifest).unwrap();

        let report = sync.sync(&state_with(vec![]), 2, false).unwrap();
        assert_eq!(report.reclaimed, vec!["../evil".to_string()]);
        assert!(sync.load_manifest().is_empty());
        assert!(
            outside.is_file(),
            "不可信的 tag 不该换来一次任意路径删除（路径穿越）"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_incomplete_sync_reclaims_nothing() {
        let dir = temp_dir("reclaim-hold");
        fs::create_dir_all(dir.join("rule-set")).unwrap();
        let layout = layout_at(&dir);
        let orphan = dir.join("rule-set/orphan.srs");
        fs::write(&orphan, b"orphan-product").unwrap();
        let mut manifest = BTreeMap::new();
        manifest.insert("orphan".into(), entry_at("https://x/orphan.txt", 1));
        let sandbox = Sandbox::new(&[]);
        let sync = Sync::new(&layout, &sandbox);
        write_manifest(&sync.manifest_path(), &manifest).unwrap();

        // 这一轮在第一条就硬失败，回收那步根本走不到
        assert!(sync
            .sync(&state_with(vec![remote("dead", "168h")]), 2, false)
            .is_err());
        assert!(orphan.is_file(), "没跑完的同步不该顺手删东西");
        assert!(sync.load_manifest().contains_key("orphan"));
        let _ = fs::remove_dir_all(dir);
    }

    // ---- 硬失败也要把已落地的部分说清楚（§7.3 第 3 条）----

    #[test]
    fn a_hard_failure_still_hands_back_what_already_landed() {
        let dir = temp_dir("partial");
        let layout = layout_at(&dir);
        let sandbox = Sandbox::new(&[(AD_URL, AD_BODY)]);
        let sync = Sync::new(&layout, &sandbox);
        // 声明顺序决定成败：先落地的那条已经进了清单
        let state = state_with(vec![remote_at("ads", AD_URL), remote("dead", "168h")]);
        let failure = sync.sync(&state, 1, false).unwrap_err();
        assert_eq!(failure.report.refreshed, vec!["ads".to_string()]);
        assert!(
            failure.source.to_string().contains("504"),
            "{}",
            failure.source
        );
        assert_eq!(
            failure.to_string(),
            failure.source.to_string(),
            "Display 照旧转达原因，CLI 的报错形状不许变"
        );
        let manifest = sync.load_manifest();
        assert!(
            manifest.contains_key("ads"),
            "已落地的事实不能跟着报告一起被吞"
        );
        assert!(!manifest.contains_key("dead"));
        let _ = fs::remove_dir_all(dir);
    }
}
