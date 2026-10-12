//! §6 第 1 步：把官方 sing-box 二进制装到 `Layout::singbox_bin` 上。
//!
//! 为什么要"架构 + libc"两个维度：官方 `sing-box-1.14.2-linux-amd64.tar.gz` 实测是
//! **动态链接 glibc** 的包（schema §11.8），在 Alpine 上跑不起来；Alpine 要的是同一次
//! release 里的 `-linux-amd64-musl.tar.gz`。两条资产都在 v1.14.2 的资产表里（2026-09-30
//! 经 api.github.com 原文核对），所以不能"下裸名试试"。
//!
//! 为什么默认走离线：2026-09-30 在 guest 实测，31MB 的资产直连 GitHub 60 秒只走了 2.0MB
//! 然后连接停住，busybox wget 报 `Read timeout`；换 `-T 60` 更糟——168 秒后报
//! `download timed out`。也就是说"install 时现抓二进制"在这套环境里根本不可靠，而旁路由
//! 恰恰经常装在没网的机器上。所以 `Source` 有两条路：本地资产（`--asset`， scp 过来即可）
//! 和在线抓取（链路好的机器上仍可用，且 curl 在时能吃到 `--max-time` 的总时长上限）。
//!
//! 抓来/拷来的东西要不要信，只问一句：**它跑得起来且版本正是钉死的那一版**。
//! release 里没有 `.sha256` 资产（同一份资产表逐条查过），所以 ZBox 不假装有校验和；
//! 换成"执行 `sing-box version` 比对版本"这种功能性验收——它同时也是唯一能证明
//! "libc 挑对了"的判据：挑错时连执行都做不到，报出来的是跟真实原因毫无关系的路径错误。
//!
//! 落地次序照 `svc::ruleset`：先进二进制旁边的暂存目录，验收通过才顶掉现役二进制。
//! 现役进程正在跑也不妨碍改名（inode 还在），所以不需要先停服务。

use crate::gen::init::Layout;
use crate::svc::ruleset::{detect_downloader, downloader_args, FetchLimits};
use crate::svc::runner::{failure_detail, Runner};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// ZBox 钉死的 sing-box 版本，与 `docs/reference/singbox-1.14.2-schema.md` 同一事实。
/// 换版本要连同生成器过一遍 schema，所以这里不存在"取最新版"这条路径。
pub const PINNED_VERSION: &str = "1.14.2";

/// 包内成员名，也是落地后的文件名。实测（schema §11.8）tar 顶层目录里就叫这个名字。
const BIN_NAME: &str = "sing-box";

/// 二进制来自哪里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// 本地资产：`.tar.gz`（官方原样）或已经解出来的 sing-box 可执行文件
    Asset(PathBuf),
    /// 按判出来的架构 + libc 现抓。guest 实测这条链路撑不住 31MB（见模块头）
    Url,
}

/// 目标机的 C 库形态，决定取哪一条资产。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Libc {
    /// Alpine / musl 系
    Musl,
    /// Debian / Ubuntu 系
    Glibc,
}

impl Libc {
    fn suffix(self) -> &'static str {
        match self {
            Self::Musl => "-musl",
            Self::Glibc => "-glibc",
        }
    }
}

/// 该装哪一条资产。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// 资产名里的架构段
    pub arch: &'static str,
    pub libc: Libc,
}

impl Target {
    /// 包内顶层目录名。实测（schema §11.8）等于资产名去掉 `.tar.gz`。
    pub fn member_dir(&self) -> String {
        format!(
            "sing-box-{PINNED_VERSION}-linux-{}{}",
            self.arch,
            self.libc.suffix()
        )
    }

    pub fn asset_name(&self) -> String {
        format!("{}.tar.gz", self.member_dir())
    }

    pub fn url(&self) -> String {
        format!(
            "https://github.com/SagerNet/sing-box/releases/download/v{PINNED_VERSION}/{}",
            self.asset_name()
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    #[error("认不出该装哪个 sing-box 资产：{detail}")]
    UnknownTarget { detail: String },
    #[error("抓取 {url} 失败：{detail}")]
    Fetch { url: String, detail: String },
    #[error("本地资产 {} 用不上：{detail}", path.display())]
    Asset { path: PathBuf, detail: String },
    #[error("解包 {asset} 失败：{detail}")]
    Extract { asset: String, detail: String },
    #[error("下载来的 sing-box 没通过验收：{detail}")]
    Reject { detail: String },
    #[error("文件操作失败：{0}")]
    Io(#[from] io::Error),
}

/// 装完之后要么真换了东西，要么什么都没动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 现役二进制正是钉死的那版，一次网络请求都没发、一个文件都没动
    AlreadyCurrent { version: String },
    /// 换成了钉死版本；`replaced` 是被换掉的版本（装之前根本没有二进制时为 None）
    Installed {
        version: String,
        detail: String,
        replaced: Option<String>,
    },
}

impl Outcome {
    pub fn summarize(&self) -> String {
        match self {
            Self::AlreadyCurrent { version } => {
                format!("sing-box {version} 已经是钉死的那版，未联网、未替换")
            }
            Self::Installed {
                version,
                detail,
                replaced,
            } => match replaced {
                Some(old) => format!("已把 {old} 换成 {version}（{detail}）"),
                None => format!("已装上 {version}（{detail}）"),
            },
        }
    }
}

/// 从 `uname -s`/`uname -m` 与 `/lib`+`/lib64` 的条目名判目标。纯函数，两头都有实测原文。
///
/// musl 侧看 `ld-musl-*`（Alpine 3.21.3 实测 `/lib` 里有 `ld-musl-x86_64.so.1`，且没有
/// `/lib64`），glibc 侧看 `ld-linux*`。认不出就报错而不是猜：猜错的直接后果是装一个
/// 跑不起来的二进制，而报错信息比现场表现好懂得多。
pub fn target_from(
    sysname: &str,
    machine: &str,
    lib_entries: &[&str],
) -> Result<Target, ProvisionError> {
    let unknown = |detail: String| ProvisionError::UnknownTarget { detail };
    if sysname.trim() != "Linux" {
        return Err(unknown(format!(
            "`uname -s` 报的是 {}，ZBox 的数据面只装 Linux 资产",
            sysname.trim()
        )));
    }
    let arch = match machine.trim() {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => {
            return Err(unknown(format!(
                "`uname -m` 报的是 {other}，ZBox 只认 x86_64(amd64) 与 aarch64(arm64)。\
                 别的架构官方 release 里确有资产，但 ZBox 没有实测环境可以验收，\
                 请手工把 sing-box 放到目标路径后用 --singbox-bin 指过去"
            )))
        }
    };
    let has = |prefix: &str| lib_entries.iter().any(|e| e.starts_with(prefix));
    let libc = if has("ld-musl-") {
        Libc::Musl
    } else if has("ld-linux") {
        Libc::Glibc
    } else {
        return Err(unknown(format!(
            "/lib 与 /lib64 里既没有 ld-musl-* 也没有 ld-linux*（一共看到 {} 个条目），\
             判不出 C 库形态",
            lib_entries.len()
        )));
    };
    Ok(Target { arch, libc })
}

/// 从 `sing-box version` 的输出取版本号。
///
/// 实测形状：第一行 `sing-box version 1.14.2`，其后还有 `Environment:`/`Tags:`/`Revision:`
/// 等若干行（schema §11.8）。取不到返回 None，由调用方按"没有可用二进制"处理。
pub fn reported_version(text: &str) -> Option<&str> {
    let line = text.lines().find(|l| l.starts_with("sing-box version"))?;
    line.split_whitespace().last()
}

/// 版本号是不是 ZBox 钉死的那一版。容忍 `-musl` 这类后缀：那正是我们要的资产。
pub fn is_pinned(version: &str) -> bool {
    version == PINNED_VERSION || version.starts_with(&format!("{PINNED_VERSION}-"))
}

pub struct Provision<'a> {
    layout: &'a Layout,
    runner: &'a dyn Runner,
}

impl<'a> Provision<'a> {
    pub fn new(layout: &'a Layout, runner: &'a dyn Runner) -> Self {
        Self { layout, runner }
    }

    /// 本机该装哪一条资产。
    pub fn detect_target(&self) -> Result<Target, ProvisionError> {
        let sysname = self.stdout(&["uname", "-s"])?;
        let machine = self.stdout(&["uname", "-m"])?;
        let mut entries = self.dir_entries("/lib");
        entries.extend(self.dir_entries("/lib64"));
        let names: Vec<&str> = entries.iter().map(String::as_str).collect();
        target_from(&sysname, &machine, &names)
    }

    /// 现役二进制的版本；没装、跑不动、输出不认，一律 None。
    pub fn current_version(&self) -> Option<String> {
        let exec = self
            .runner
            .exec(&[self.layout.singbox_bin.as_str(), "version"])
            .ok()?;
        if !exec.succeeded() {
            return None;
        }
        reported_version(&exec.merged()).map(str::to_string)
    }

    /// 就位：拿到一份验收过的 sing-box 摆在 `layout.singbox_bin` 上。
    ///
    /// 现役版本已经对就直接短路（`force` 用来覆盖这个判断，比如怀疑二进制被动过）。
    /// 短路这条是整个模块能默认打开的原因：guest 里 `/usr/local/bin/sing-box` 已经在，
    /// 每次 install 都去重下 31MB 是不能接受的。
    pub fn install(&self, source: &Source, force: bool) -> Result<Outcome, ProvisionError> {
        let target = self.detect_target()?;
        let current = self.current_version();
        if !force && current.as_deref().is_some_and(is_pinned) {
            return Ok(Outcome::AlreadyCurrent {
                version: current.unwrap_or_default(),
            });
        }
        // 暂存目录选在二进制的**旁边**而不是状态目录里：跨文件系统的改名会退化成拷贝，
        // 而 90MB 的可执行文件必须要么整体换上、要么根本没动过。
        let out = PathBuf::from(&self.layout.singbox_bin);
        let home = out.parent().map(Path::to_path_buf).unwrap_or_default();
        let dir = home.join(format!("{}.provision", BIN_NAME));
        self.reset_dir(&dir)?;
        let detail = match source {
            Source::Url => target.url(),
            Source::Asset(path) => path.display().to_string(),
        };
        // 暂存目录一经建立就必须在每条路径上被清掉——包括抓取和解包失败这两条：
        // 里面可能躺着半截 32MB 的资产，留着下次会被当成"已有产物"。
        let staged = (|| -> Result<String, ProvisionError> {
            let staged = match source {
                Source::Url => self.fetch(target, &dir)?,
                Source::Asset(path) => self.stage_local(path, &dir)?,
            };
            // 验收不过就不许碰现役二进制
            let version = self.verify(&staged)?;
            self.land(&staged, &dir)?;
            Ok(version)
        })();
        self.cleanup(&dir);
        let version = staged?;
        Ok(Outcome::Installed {
            version,
            detail,
            replaced: current.filter(|v| !is_pinned(v)),
        })
    }

    /// 在线抓资产再解包。`Source::Url` 专用；链路撑不住时用 `Source::Asset`。
    fn fetch(&self, target: Target, dir: &Path) -> Result<PathBuf, ProvisionError> {
        let archive = dir.join(target.asset_name());
        let downloader = detect_downloader(self.runner);
        let args = downloader_args(downloader, &target.url(), &archive, FetchLimits::ASSET);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let detail = |text: String| ProvisionError::Fetch {
            url: target.url(),
            detail: text,
        };
        let result = self
            .runner
            .exec(&argv)
            .map_err(|e| detail(format!("{e}（命令：`{}`）", args.join(" "))))?;
        if !result.succeeded() {
            let _ = fs::remove_file(&archive);
            return Err(detail(format!(
                "{}；32MB 的资产对链路很敏感，抓不动就先把 {} 拷到本机再用 --asset 指过去",
                failure_detail(&result.merged(), result.code),
                target.asset_name()
            )));
        }
        if !archive.is_file() {
            return Err(detail(format!(
                "下载器退出正常，但 {} 里没有文件",
                archive.display()
            )));
        }
        self.unpack(&archive, dir)
    }

    /// 本地资产：官方 `.tar.gz` 直接解，已经解出来的可执行文件拷进暂存目录。
    /// 两种都拷/解到 `<provision>/` 再去顶，用户的文件永远不被搬走。
    fn stage_local(&self, path: &Path, dir: &Path) -> Result<PathBuf, ProvisionError> {
        let asset = |detail: String| ProvisionError::Asset {
            path: path.to_path_buf(),
            detail,
        };
        let meta = fs::metadata(path).map_err(|e| asset(format!("读不到：{e}")))?;
        if !meta.is_file() {
            return Err(asset(
                "不是普通文件（--asset 指官方 .tar.gz 或已经解出来的那个 sing-box）".to_string(),
            ));
        }
        if meta.len() == 0 {
            return Err(asset("0 字节".to_string()));
        }
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("gz"))
        {
            return self.unpack(path, dir);
        }
        let staged = dir.join(BIN_NAME);
        fs::copy(path, &staged).map_err(|e| asset(format!("拷进 {} 失败：{e}", dir.display())))?;
        self.make_executable(&staged)?;
        Ok(staged)
    }

    /// 解包。`-C <dir>` 而不是 `--strip-components`：实测（schema §11.8）包内只有一个顶层
    /// 目录，直接解进暂存目录再取里面的 `sing-box`，不需要对 tar 的改名行为做任何假设。
    fn unpack(&self, archive: &Path, dir: &Path) -> Result<PathBuf, ProvisionError> {
        let asset = archive
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let failed = |detail: String| ProvisionError::Extract {
            asset: asset.clone(),
            detail,
        };
        let dir_text = dir.display().to_string();
        let archive_text = archive.display().to_string();
        // busybox tar 1.37（guest 实测）支持 -x -z -f -C
        let argv = ["tar", "-xzf", &archive_text, "-C", &dir_text];
        let result = self
            .runner
            .exec(&argv)
            .map_err(|e| failed(format!("`{}` 跑不了：{e}", argv.join(" "))))?;
        if !result.succeeded() {
            return Err(failed(failure_detail(&result.merged(), result.code)));
        }
        let staged = dir.join(self.member_dir(archive)).join(BIN_NAME);
        if !staged.is_file() {
            return Err(failed(format!(
                "解包退出正常，但 {} 不在里面（包内布局与实测不一致，别接着装）",
                staged.display()
            )));
        }
        self.make_executable(&staged)?;
        Ok(staged)
    }

    /// 顶层目录名：实测等于资产名去掉 `.tar.gz`。`--asset` 指的文件名不规范时退回
    /// 本机判出来的目标——真正的兜底是 `unpack` 里那句"成员不在里面"的硬失败。
    fn member_dir(&self, archive: &Path) -> String {
        let stem = archive
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = stem
            .strip_suffix(".tar")
            .map(str::to_string)
            .unwrap_or(stem);
        if stem.starts_with("sing-box-") {
            return stem;
        }
        self.detect_target()
            .map(|t| t.member_dir())
            .unwrap_or_default()
    }

    /// 功能性验收：真跑一次。libc 挑错、包被截断、架构不对都会在这一步现形，
    /// 而不是等到 `sing-box check` 报一个看不出原因的路径错误。
    fn verify(&self, staged: &Path) -> Result<String, ProvisionError> {
        let staged_text = staged.display().to_string();
        let result = self
            .runner
            .exec(&[&staged_text, "version"])
            .map_err(|e| ProvisionError::Reject { detail: e })?;
        let merged = result.merged();
        if !result.succeeded() {
            return Err(ProvisionError::Reject {
                detail: format!(
                    "{staged_text} 跑不动（rc={}）：{}",
                    result.code,
                    failure_detail(&merged, result.code)
                ),
            });
        }
        let version = reported_version(&merged).ok_or_else(|| ProvisionError::Reject {
            detail: format!("输出里没有 `sing-box version` 这一行，不像官方二进制：{merged}"),
        })?;
        if !is_pinned(version) {
            return Err(ProvisionError::Reject {
                detail: format!(
                    "报的是 {version}，不是钉死的 {PINNED_VERSION}；\
                     版本对不上就下发配置，等于拿一份没核对过的内核去跑生成器写死的字段"
                ),
            });
        }
        Ok(version.to_string())
    }

    /// 顶掉现役二进制。
    ///
    /// 暂存件就在目标旁边（同一个目录树），所以这里永远是改名而不是拷贝。
    /// Linux 上改名不影响正在跑的进程——旧 inode 活到它自己退出，所以不必先停服务。
    fn land(&self, staged: &Path, dir: &Path) -> Result<(), ProvisionError> {
        // 解包那条路成员在 `<dir>/<顶层目录>/sing-box`，先提到暂存目录根上
        let next = dir.join(BIN_NAME);
        if next != *staged {
            fs::rename(staged, &next).map_err(|e| ProvisionError::Reject {
                detail: format!("把 {} 提到 {} 失败：{e}", staged.display(), next.display()),
            })?;
        }
        let out = Path::new(&self.layout.singbox_bin);
        crate::svc::ruleset::promote(&next, out).map_err(|e| ProvisionError::Reject {
            detail: format!("把 {} 放到 {} 失败：{e}", next.display(), out.display()),
        })
    }

    /// 解包本身带 0755（实测 tar 列表里就是 `-rwxr-xr-x`），这一步给拷贝那条路兜底。
    /// Windows 开发机上没有权限位，走空实现。
    fn make_executable(&self, path: &Path) -> Result<(), ProvisionError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path)?.permissions().mode();
            fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o111))?;
        }
        #[cfg(not(unix))]
        let _ = path;
        Ok(())
    }

    /// 暂存目录每次重装前先清空：上一轮失败的残留会被下一次当成"已有产物"。
    fn reset_dir(&self, dir: &Path) -> Result<(), ProvisionError> {
        let _ = fs::remove_dir_all(dir);
        fs::create_dir_all(dir)?;
        Ok(())
    }

    fn stdout(&self, argv: &[&str]) -> Result<String, ProvisionError> {
        let result = self
            .runner
            .exec(argv)
            .map_err(|e| ProvisionError::UnknownTarget {
                detail: format!("`{}` 跑不了：{e}", argv.join(" ")),
            })?;
        if !result.succeeded() {
            return Err(ProvisionError::UnknownTarget {
                detail: format!(
                    "`{}` rc={}：{}",
                    argv.join(" "),
                    result.code,
                    failure_detail(&result.merged(), result.code)
                ),
            });
        }
        Ok(result.stdout)
    }

    /// 目录条目名一律问 runner，不走 `fs::read_dir`：这一段判的是**目标机**的 `/lib`，
    /// 开发机上读自己的文件系统只会判出与目标无关的形态，测试也会变成平台相关。
    /// 读不到就按空处理——`/lib64` 在 Alpine 上本就不存在（实测）。
    fn dir_entries(&self, dir: &str) -> Vec<String> {
        let Ok(exec) = self.runner.exec(&["ls", dir]) else {
            return Vec::new();
        };
        if !exec.succeeded() {
            return Vec::new();
        }
        exec.stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn cleanup(&self, dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svc::runner::{Exec, Scripted};
    use std::cell::RefCell;
    use std::time::Duration;

    fn exec(code: i32, stdout: &str) -> Exec {
        Exec {
            code,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// guest 实测原文（Alpine 3.21.3，schema §11.8）：`/lib` 里有 `ld-musl-x86_64.so.1` 且没有 `/lib64`。
    const ALPINE_LIB: &[&str] = &["ld-musl-x86_64.so.1", "libc.so", "libm.so.2"];
    const UBUNTU_LIB: &[&str] = &["ld-linux-x86-64.so.2", "libc.so.6"];

    /// guest 实测原文（`sing-box version`，schema §11.8）
    const VERSION_FIXTURE: &str = "sing-box version 1.14.2\nEnvironment: go1.26.8 linux/amd64\n\
        Tags: with_vision,with_musl\nRevision: af6e64c3b69e6132ebaee0e1a3d24e93903f6709\nCGO: enabled\n";

    /// 暂存与落地都在真实文件系统上走，所以每个用例各自占一个临时目录。
    fn host_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zbox-provision-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn musl_and_glibc_hosts_pick_different_assets() {
        assert_eq!(
            target_from("Linux", "x86_64", ALPINE_LIB).unwrap(),
            Target {
                arch: "amd64",
                libc: Libc::Musl
            }
        );
        assert_eq!(
            target_from("Linux", "x86_64", UBUNTU_LIB).unwrap(),
            Target {
                arch: "amd64",
                libc: Libc::Glibc
            }
        );
        // 裸名那条是动态链接 glibc 的包（实测），在 Alpine 上跑不起来，所以判不出 libc 时不许退到它
        assert_eq!(
            target_from("Linux", "x86_64", &[]).unwrap_err().to_string(),
            "认不出该装哪个 sing-box 资产：/lib 与 /lib64 里既没有 ld-musl-* 也没有 ld-linux*（一共看到 0 个条目），判不出 C 库形态"
        );
    }

    #[test]
    fn the_pinned_asset_names_match_the_release_table() {
        // 两条资产都在 v1.14.2 的资产表里（api.github.com 原文核对，字节数见 schema §11.8）
        let musl = Target {
            arch: "amd64",
            libc: Libc::Musl,
        };
        assert_eq!(musl.asset_name(), "sing-box-1.14.2-linux-amd64-musl.tar.gz");
        assert_eq!(musl.member_dir(), "sing-box-1.14.2-linux-amd64-musl");
        let glibc = Target {
            arch: "arm64",
            libc: Libc::Glibc,
        };
        assert_eq!(
            glibc.url(),
            "https://github.com/SagerNet/sing-box/releases/download/v1.14.2/sing-box-1.14.2-linux-arm64-glibc.tar.gz"
        );
    }

    #[test]
    fn hosts_we_cannot_verify_are_refused_not_guessed() {
        let err = target_from("Linux", "mips64", ALPINE_LIB)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mips64"), "{err}");
        assert!(err.contains("--singbox-bin"), "得给出路：{err}");
        assert!(target_from("Darwin", "x86_64", UBUNTU_LIB)
            .unwrap_err()
            .to_string()
            .contains("Linux"));
    }

    #[test]
    fn version_output_is_read_from_the_first_line_only() {
        // 实测形状：`sing-box version 1.14.2` 之后还有 Environment/Tags/Revision/CGO 数行
        let log = "sing-box version 1.14.2\nEnvironment: go1.26.8 linux/amd64\nTags: with_vision,with_musl\nRevision: af6e64c3b69e6132ebaee0e1a3d24e93903f6709\nCGO: enabled\n";
        assert_eq!(reported_version(log), Some("1.14.2"));
        assert_eq!(reported_version(""), None);
        assert_eq!(reported_version("Usage: sing-box [-V]command\n"), None);
        assert!(is_pinned("1.14.2"));
        assert!(is_pinned("1.14.2-musl"));
        assert!(!is_pinned("1.15.0"), "没有'取最新版'这条路径");
    }

    /// 解包认成员的那条路径必须与实测包内布局逐字一致（schema §11.8）。
    /// `-C` 而不是 `--strip-components`：不对 tar 的改名行为做任何假设。
    #[test]
    fn unpack_resolves_the_member_at_the_measured_layout() {
        let dir = host_dir("unpack");
        let staged_root = dir.join("sing-box.provision");
        let top = "sing-box-1.14.2-linux-amd64-musl";
        // 替 tar 做它该做的副作用：按实测布局把成员放好
        let member = staged_root.join(top).join(BIN_NAME);
        fs::create_dir_all(member.parent().unwrap()).unwrap();
        fs::write(&member, b"stub").unwrap();
        let runner = Scripted::new();
        let archive = dir.join(format!("{top}.tar.gz"));
        fs::write(&archive, b"stub").unwrap();
        let found = Provision::new(&Layout::default(), &runner)
            .unpack(&archive, &staged_root)
            .unwrap();
        assert_eq!(found, member);
        let calls = runner.argv();
        assert_eq!(
            calls[0].join(" "),
            format!(
                "tar -xzf {} -C {}",
                archive.display(),
                staged_root.display()
            )
        );
        // 包内布局跟实测对不上时必须硬失败，不能"接着往下装"
        fs::remove_file(&member).unwrap();
        let err = Provision::new(&Layout::default(), &runner)
            .unpack(&archive, &staged_root)
            .unwrap_err();
        assert!(format!("{err}").contains("别接着装"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 判目标那四条探测的顺序固定：`uname -s`、`uname -m`、`ls /lib`、`ls /lib64`。
    /// `/lib64` 在 Alpine 上不存在（实测），所以它必须报失败而不被当成判不出。
    fn probes(current: Exec) -> Vec<Result<Exec, String>> {
        vec![
            Ok(exec(0, "Linux\n")),
            Ok(exec(0, "x86_64\n")),
            Ok(exec(0, "ld-musl-x86_64.so.1\nlibc.so\n")),
            Ok(exec(1, "")),
            Ok(current),
        ]
    }

    #[test]
    fn a_binary_that_cannot_run_never_touches_the_live_one() {
        let dir = host_dir("reject");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let live = bin.join(BIN_NAME);
        fs::write(&live, "live").unwrap();
        let layout = Layout {
            singbox_bin: live.display().to_string(),
            ..Layout::default()
        };
        let asset = dir.join("staged-asset");
        fs::write(&asset, "staged").unwrap();
        // 最后一条是 `<staged> version`：libc 挑错时操作系统直接拒（rc=127）
        let mut results = probes(exec(1, ""));
        results.push(Ok(exec(127, "No such file or directory")));
        let runner = Scripted::with_results(results);
        let err = Provision::new(&layout, &runner)
            .install(&Source::Asset(asset), false)
            .unwrap_err();
        assert!(matches!(err, ProvisionError::Reject { .. }), "{err}");
        assert!(err.to_string().contains("rc=127"), "{err}");
        assert_eq!(
            fs::read_to_string(&live).unwrap(),
            "live",
            "验收不过就不许顶掉现役"
        );
        assert!(!bin.join("sing-box.provision").exists(), "暂存目录要清干净");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_already_current_kernel_short_circuits_without_the_network() {
        let dir = host_dir("current");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let layout = Layout {
            singbox_bin: bin.join(BIN_NAME).display().to_string(),
            ..Layout::default()
        };
        let runner = Scripted::with_results(probes(exec(0, VERSION_FIXTURE)));
        let outcome = Provision::new(&layout, &runner)
            .install(&Source::Url, false)
            .unwrap();
        assert_eq!(
            outcome,
            Outcome::AlreadyCurrent {
                version: "1.14.2".into()
            }
        );
        assert!(outcome.summarize().contains("未联网"));
        assert_eq!(
            runner.argv().len(),
            5,
            "版本已经对，探测之外一个外部命令都不该发"
        );
        assert!(!bin.join("sing-box.provision").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn force_overrides_the_short_circuit() {
        let dir = host_dir("force");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let layout = Layout {
            singbox_bin: bin.join(BIN_NAME).display().to_string(),
            ..Layout::default()
        };
        let asset = dir.join("staged-asset");
        fs::write(&asset, "staged").unwrap();
        let mut results = probes(exec(0, VERSION_FIXTURE));
        results.push(Ok(exec(0, VERSION_FIXTURE)));
        let runner = Scripted::with_results(results);
        let outcome = Provision::new(&layout, &runner)
            .install(&Source::Asset(asset), true)
            .unwrap();
        assert!(matches!(outcome, Outcome::Installed { .. }), "{outcome:?}");
        assert_eq!(fs::read_to_string(bin.join(BIN_NAME)).unwrap(), "staged");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_asset_branch_lands_and_says_what_it_replaced() {
        let dir = host_dir("install");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let live = bin.join(BIN_NAME);
        fs::write(&live, "old").unwrap();
        let layout = Layout {
            singbox_bin: live.display().to_string(),
            ..Layout::default()
        };
        let asset = dir.join("staged-asset");
        fs::write(&asset, "new").unwrap();
        // 现役报 1.13.0（不是钉死版本）→ 必须换；验收认的是暂存件自己报的版本
        let mut results = probes(exec(0, "sing-box version 1.13.0\n"));
        results.push(Ok(exec(0, VERSION_FIXTURE)));
        let runner = Scripted::with_results(results);
        let outcome = Provision::new(&layout, &runner)
            .install(&Source::Asset(asset.clone()), false)
            .unwrap();
        match &outcome {
            Outcome::Installed {
                version,
                detail,
                replaced,
            } => {
                assert_eq!(version, "1.14.2");
                assert_eq!(replaced.as_deref(), Some("1.13.0"));
                assert_eq!(detail, &asset.display().to_string());
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(fs::read_to_string(&live).unwrap(), "new");
        assert!(asset.exists(), "用户给的文件不能被搬走");
        assert!(!bin.join("sing-box.provision").exists());
        assert_eq!(
            outcome.summarize(),
            format!("已把 1.13.0 换成 1.14.2（{}）", asset.display())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_wrong_version_is_refused_even_when_it_runs() {
        let dir = host_dir("wrong-version");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let layout = Layout {
            singbox_bin: bin.join(BIN_NAME).display().to_string(),
            ..Layout::default()
        };
        let asset = dir.join("staged-asset");
        fs::write(&asset, "new").unwrap();
        let mut results = probes(exec(0, "sing-box version 1.13.12\n"));
        results.push(Ok(exec(0, "sing-box version 1.15.0\n")));
        let runner = Scripted::with_results(results);
        let err = Provision::new(&layout, &runner)
            .install(&Source::Asset(asset), false)
            .unwrap_err();
        let detail = err.to_string();
        assert!(detail.contains("1.15.0"), "{detail}");
        assert!(detail.contains("没核对过的内核"), "{detail}");
        assert!(!bin.join(BIN_NAME).exists(), "版本不对就不落地");
        assert!(!bin.join("sing-box.provision").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zero_byte_asset_is_refused_by_name() {
        let dir = host_dir("empty-asset");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let layout = Layout {
            singbox_bin: bin.join(BIN_NAME).display().to_string(),
            ..Layout::default()
        };
        let empty = dir.join("empty");
        fs::write(&empty, "").unwrap();
        let runner = Scripted::with_results(probes(exec(1, "")));
        let err = Provision::new(&layout, &runner)
            .install(&Source::Asset(empty), false)
            .unwrap_err();
        assert!(matches!(err, ProvisionError::Asset { .. }), "{err}");
        assert!(err.to_string().contains("0 字节"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_asset_is_reported_with_its_own_path() {
        let dir = host_dir("missing-asset");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let layout = Layout {
            singbox_bin: bin.join(BIN_NAME).display().to_string(),
            ..Layout::default()
        };
        let missing = dir.join("nope");
        let runner = Scripted::with_results(probes(exec(1, "")));
        let err = Provision::new(&layout, &runner)
            .install(&Source::Asset(missing.clone()), false)
            .unwrap_err();
        assert!(
            err.to_string().contains(&missing.display().to_string()),
            "{err}"
        );
        assert!(!bin.join("sing-box.provision").exists(), "失败不留暂存目录");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn online_fetch_fails_fast_and_points_at_the_offline_route() {
        let dir = host_dir("fetch-fail");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let layout = Layout {
            singbox_bin: bin.join(BIN_NAME).display().to_string(),
            ..Layout::default()
        };
        // 实测原文（schema §11.8）：guest 上 busybox wget 抓 31MB 报的就是这句
        let mut results = probes(exec(1, ""));
        results.push(Ok(exec(127, ""))); // curl 不在（实测）
        results.push(Ok(exec(1, "wget: download timed out\n")));
        let runner = Scripted::with_results(results);
        let err = Provision::new(&layout, &runner)
            .install(&Source::Url, false)
            .unwrap_err();
        let detail = err.to_string();
        assert!(detail.contains("download timed out"), "{detail}");
        assert!(detail.contains("--asset"), "要给出离线这条走：{detail}");
        assert!(
            detail.contains("sing-box-1.14.2-linux-amd64-musl.tar.gz"),
            "{detail}"
        );
        let calls = runner.argv();
        let wget = calls
            .iter()
            .find(|a| a.first().map(String::as_str) == Some("wget"))
            .expect("探测失败后该用 wget");
        assert!(
            wget.windows(2).any(|w| w == ["-T", "20"]),
            "资产用短空闲上限：{wget:?}"
        );
        assert_eq!(
            wget.last().unwrap(),
            "https://github.com/SagerNet/sing-box/releases/download/v1.14.2/sing-box-1.14.2-linux-amd64-musl.tar.gz"
        );
        assert!(!bin.join("sing-box.provision").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// 照 §11.8 实测形状模仿目标机的在线那条路：wget 真的写资产文件、tar 真的按包内
    /// 布局放成员、只有落地的二进制会答 version。
    struct Guest {
        /// `Layout::singbox_bin`，用来把"现役"和"暂存件"分开回答
        live_path: PathBuf,
        /// 现役二进制会答的版本（None 等于没装或跑不动）
        live: Option<&'static str>,
        /// 暂存件会答的版本——挑错 libc 时它就是"跑不动"
        staged: Option<&'static str>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl Guest {
        fn output<'a>(argv: &'a [&'a str]) -> Option<&'a str> {
            argv.iter()
                .position(|a| *a == "-O" || *a == "-o")
                .and_then(|i| argv.get(i + 1).copied())
        }
    }

    impl Runner for Guest {
        fn exec(&self, argv: &[&str]) -> Result<Exec, String> {
            self.calls
                .borrow_mut()
                .push(argv.iter().map(|a| a.to_string()).collect());
            match argv.first().copied() {
                Some("uname") => Ok(exec(
                    0,
                    if argv.get(1) == Some(&"-s") {
                        "Linux\n"
                    } else {
                        "x86_64\n"
                    },
                )),
                Some("ls") => match argv.get(1).copied() {
                    Some("/lib") => Ok(exec(0, "ld-musl-x86_64.so.1\nlibc.so\n")),
                    // guest 实测：/lib64 在 Alpine 上根本不存在
                    other => Ok(exec(1, &format!("ls: {other:?}: No such file\n"))),
                },
                // guest 实测：没有 curl，探测这一步连进程都起不来
                Some("curl") => Err("启动 curl 失败: No such file or directory".to_string()),
                Some("wget") => {
                    if let Some(out) = Self::output(argv) {
                        fs::write(out, "asset").map_err(|e| e.to_string())?;
                    }
                    Ok(exec(0, ""))
                }
                Some("tar") => {
                    // tar -xzf <archive> -C <dir>：成员路径照实测包内布局
                    let stem = Path::new(argv[2])
                        .file_stem()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned();
                    let top = stem.strip_suffix(".tar").unwrap_or(&stem).to_string();
                    let member = Path::new(argv[4]).join(&top).join(BIN_NAME);
                    fs::create_dir_all(member.parent().unwrap()).map_err(|e| e.to_string())?;
                    fs::write(&member, "asset").map_err(|e| e.to_string())?;
                    Ok(exec(0, ""))
                }
                Some(bin) => {
                    let path = Path::new(bin);
                    let answered = if path == self.live_path {
                        self.live
                    } else {
                        self.staged
                    };
                    match answered {
                        Some(v) => Ok(exec(0, &format!("sing-box version {v}\n"))),
                        None => Ok(exec(127, "No such file or directory\n")),
                    }
                }
                None => Err("argv 为空".to_string()),
            }
        }

        fn exec_stdin(&self, _argv: &[&str], _stdin: &[u8]) -> Result<Exec, String> {
            Err("provision 不走标准输入".to_string())
        }

        fn until_started(
            &self,
            _argv: &[&str],
            _redirect: &Path,
            _kernel_log: Option<&Path>,
            _deadline: Duration,
        ) -> Result<(), String> {
            Err("provision 不预跑".to_string())
        }
    }

    #[test]
    fn the_online_branch_unpacks_the_asset_for_this_host_and_lands_it() {
        let dir = host_dir("online");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let live = bin.join(BIN_NAME);
        let layout = Layout {
            singbox_bin: live.display().to_string(),
            ..Layout::default()
        };
        let guest = Guest {
            live_path: live.clone(),
            live: None,
            staged: Some(PINNED_VERSION),
            calls: RefCell::new(Vec::new()),
        };
        let outcome = Provision::new(&layout, &guest)
            .install(&Source::Url, false)
            .unwrap();
        assert!(matches!(outcome, Outcome::Installed { .. }), "{outcome:?}");
        assert_eq!(fs::read_to_string(&live).unwrap(), "asset");
        // 抓的是 musl 那条，不是裸名（裸名在 Alpine 上跑不动，实测）
        let wget = guest
            .calls
            .borrow()
            .iter()
            .find(|a| a.first().map(String::as_str) == Some("wget"))
            .expect("该有抓取")
            .last()
            .unwrap()
            .clone();
        assert!(wget.ends_with("-musl.tar.gz"), "{wget}");
        assert!(!bin.join("sing-box.provision").exists(), "暂存目录要清干净");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_online_branch_refuses_a_binary_that_reports_another_version() {
        let dir = host_dir("online-wrong");
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let live = bin.join(BIN_NAME);
        let layout = Layout {
            singbox_bin: live.display().to_string(),
            ..Layout::default()
        };
        let guest = Guest {
            live_path: live.clone(),
            live: None,
            // 架构判对了、版本不是钉死那一版（比如资产表被改过）
            staged: Some("1.13.12"),
            calls: RefCell::new(Vec::new()),
        };
        let err = Provision::new(&layout, &guest)
            .install(&Source::Url, false)
            .unwrap_err();
        assert!(matches!(err, ProvisionError::Reject { .. }), "{err}");
        assert!(!live.exists(), "验收不过就不该有任何现役二进制");
        let _ = fs::remove_dir_all(&dir);
    }
}
