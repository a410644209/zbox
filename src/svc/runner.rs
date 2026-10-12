//! 进程调用边界。`apply::engine` 的状态机在开发机上靠注入的闭包完整可测，
//! 而"真正动机器"的那一侧全部收在这里：测试用 [`Scripted`]（记录 argv、返回预设结果），
//! 目标机用 [`System`]。
//!
//! 拆成两半是有实测原因的：`sing-box run` 成功时**不会退出**，所以预跑门（schema §11.6）
//! 不能按"等进程结束看 rc"来判，只能"等日志出现标记，然后把进程杀掉"。
//! 这条差异必须待在 `Runner` 后面，不能让状态机去猜。

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// 判据标记。实测三份配置（baseline/full/sub）与 netns 两轮都以这一行为准，
/// 不是"进程活着"，也不是 rc=0。
pub const STARTED_MARKER: &str = "sing-box started";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Exec {
    /// sing-box 的 FATAL 有的走 stdout、有的走 stderr，还带 `[0016]` 这类序号，
    /// 所以详情一律把两路合起来找。
    pub fn merged(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    pub fn succeeded(&self) -> bool {
        self.code == 0
    }
}

/// 从日志文本判定"起没起来"。`exited` 是进程自己退出时的退出码（还没退出就传 `None`）。
///
/// 单独做成纯函数，是因为判据的形状全部来自实测：
/// `check` rc=0 而 `run` 立刻 FATAL 的两条写法（§11.1）、以及规则集超时那条 FATAL（§11.7），
/// 都得在这里给出人能看懂的详情。
pub fn judge(log: &str, exited: Option<i32>) -> Result<(), String> {
    if log.contains(STARTED_MARKER) {
        return Ok(());
    }
    match exited {
        Some(code) => Err(format!(
            "预跑进程已退出（rc={code}）：{}",
            failure_detail(log, code)
        )),
        None => Err(format!(
            "预跑超时仍未见 `{STARTED_MARKER}`：{}",
            failure_detail(log, -1)
        )),
    }
}

/// 挑出最像原因的几行：FATAL 优先，其次是 ERROR，都没有就取尾部。
pub fn failure_detail(log: &str, code: i32) -> String {
    let lines: Vec<&str> = log
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    for marker in ["FATAL", "ERROR"] {
        let hits: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|l| l.contains(marker))
            .collect();
        if !hits.is_empty() {
            return hits.join(" | ");
        }
    }
    let tail = lines
        .iter()
        .rev()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(" / ");
    if tail.is_empty() {
        format!("日志为空（rc={code}），可能是二进制路径或 -D 目录不可用")
    } else {
        tail
    }
}

pub trait Runner {
    /// 跑完一条命令并返回结果。**不在此处判 rc**：什么算失败由调用方的判据决定。
    fn exec(&self, argv: &[&str]) -> Result<Exec, String>;

    /// 带标准输入跑一条命令。`sing-box rule-set compile stdin -o <out>` 靠它——
    /// 源 JSON 不落成一个可预测的临时文件名（schema §10），管道是唯一不留痕的传法。
    fn exec_stdin(&self, argv: &[&str], stdin: &[u8]) -> Result<Exec, String>;

    /// 后台启动，直到 `marker` 出现、进程自己退出、或超过 `deadline`；
    /// 成功时负责把它停掉（预跑门不监听任何端口，但也不该留着占名字）。
    ///
    /// 判据要从**两个地方**读：`redirect` 是我们把子进程 stdout/stderr 接过去的文件，
    /// `kernel_log` 是配置里 `log.output` 指向的那份。`log.output` 一旦设置，
    /// `sing-box started` 就只进后者、不进前者——实测 `check` 根本不打开这个路径
    /// （目录不存在也 rc=0），只有 `run` 会因此 FATAL，所以预跑必须读内核自己的日志，
    /// 否则门会把一份起得来的配置判成超时（schema §11.8.3）。
    /// 传 `None` 表示这份配置没写 `log.output`（内核把日志打在 stderr，只读 `redirect` 就够）。
    fn until_started(
        &self,
        argv: &[&str],
        redirect: &Path,
        kernel_log: Option<&Path>,
        deadline: Duration,
    ) -> Result<(), String>;
}

/// `gen::init` 产出的命令动词是 `Vec<String>`（要拼路径），`Runner` 收 `&[&str]`，
/// 转换点只留这一处。
pub fn argv_refs(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

pub struct System;

impl Runner for System {
    fn exec(&self, argv: &[&str]) -> Result<Exec, String> {
        let (prog, rest) = argv.split_first().ok_or_else(|| "argv 为空".to_string())?;
        let output = Command::new(prog)
            .args(rest)
            .output()
            .map_err(|e| format!("启动 {prog} 失败: {e}"))?;
        Ok(Exec {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn exec_stdin(&self, argv: &[&str], stdin: &[u8]) -> Result<Exec, String> {
        use std::io::Write;
        let (prog, rest) = argv.split_first().ok_or_else(|| "argv 为空".to_string())?;
        let mut child = Command::new(prog)
            .args(rest)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("启动 {prog} 失败: {e}"))?;
        // 先写完再收：本项目的 stdin 载荷（规则集源 JSON）是写入方、其 stdout 只有几行日志，
        // 两个方向都不会把对方的管道缓冲填满。要传双向都大量的载荷就得另起线程。
        child
            .stdin
            .take()
            .expect("piped() 之后一定有句柄")
            .write_all(stdin)
            .map_err(|e| format!("写标准输入失败: {e}"))?;
        let output = child
            .wait_with_output()
            .map_err(|e| format!("等 {prog} 退出失败: {e}"))?;
        Ok(Exec {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn until_started(
        &self,
        argv: &[&str],
        redirect: &Path,
        kernel_log: Option<&Path>,
        deadline: Duration,
    ) -> Result<(), String> {
        let (prog, rest) = argv.split_first().ok_or_else(|| "argv 为空".to_string())?;
        let file = fs::File::create(redirect)
            .map_err(|e| format!("创建 {} 失败: {e}", redirect.display()))?;
        let err_file = file
            .try_clone()
            .map_err(|e| format!("复制日志句柄失败: {e}"))?;
        let mut child = Command::new(prog)
            .args(rest)
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(err_file))
            .spawn()
            .map_err(|e| format!("启动 {prog} 失败: {e}"))?;
        let started = Instant::now();
        loop {
            let text = read_logs(redirect, kernel_log);
            if text.contains(STARTED_MARKER) {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return judge(&text, status.code());
            }
            if started.elapsed() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return judge(&read_logs(redirect, kernel_log), None);
            }
            sleep(Duration::from_millis(100));
        }
    }
}

/// 把两处日志合成一份文本来判。读不到就当空——`judge` 对"日志为空"有自己的说法。
pub fn read_logs(redirect: &Path, kernel_log: Option<&Path>) -> String {
    let mut text = fs::read_to_string(redirect).unwrap_or_default();
    if let Some(path) = kernel_log {
        text.push_str(&fs::read_to_string(path).unwrap_or_default());
    }
    text
}

/// 测试替身：按顺序记录 argv，并按脚本返回预设结果。
#[derive(Default)]
pub struct Scripted {
    pub calls: std::cell::RefCell<Vec<Vec<String>>>,
    /// `until_started` 的返回值，缺省成功
    pub started: bool,
    /// 每次 `exec` 依序取一条；用完就返回 rc=0
    pub results: std::cell::RefCell<Vec<Result<Exec, String>>>,
    /// `exec_stdin` 收到的载荷，按调用顺序记录
    pub payloads: std::cell::RefCell<Vec<Vec<u8>>>,
}

impl Scripted {
    fn record(&self, argv: &[&str]) {
        self.calls
            .borrow_mut()
            .push(argv.iter().map(|a| a.to_string()).collect());
    }

    pub fn new() -> Self {
        Self {
            started: true,
            ..Self::default()
        }
    }

    pub fn with_results(results: Vec<Result<Exec, String>>) -> Self {
        Self {
            started: true,
            results: std::cell::RefCell::new(results),
            ..Self::default()
        }
    }

    pub fn argv(&self) -> Vec<Vec<String>> {
        self.calls.borrow().clone()
    }
}

impl Runner for Scripted {
    fn exec(&self, argv: &[&str]) -> Result<Exec, String> {
        self.record(argv);
        let mut queue = self.results.borrow_mut();
        if queue.is_empty() {
            return Ok(Exec {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        // 取第一条；用完之后剩下的都当成功，测试只关心关键几条
        if queue.len() > 1 {
            queue.remove(0)
        } else {
            queue[0].clone()
        }
    }

    fn exec_stdin(&self, argv: &[&str], stdin: &[u8]) -> Result<Exec, String> {
        self.payloads.borrow_mut().push(stdin.to_vec());
        self.exec(argv)
    }

    fn until_started(
        &self,
        argv: &[&str],
        _redirect: &Path,
        _kernel_log: Option<&Path>,
        _deadline: Duration,
    ) -> Result<(), String> {
        self.record(argv);
        if self.started {
            Ok(())
        } else {
            Err(format!(
                "预跑超时仍未见 `{STARTED_MARKER}`：{FAILURE_FIXTURE}"
            ))
        }
    }
}

/// 实测原文（§11.7 的远端规则集超时），给测试当 fixture 用。
pub const FAILURE_FIXTURE: &str = "FATAL[0016] start service: (initialize rule-set[3]: \
    initial rule-set: geoip-cn: Get \"https://raw.githubusercontent.com/SagerNet/sing-geoip/\
    rule-set/geoip-cn.srs\": context deadline exceeded)";

#[cfg(test)]
mod tests {
    use super::*;

    const STARTED: &str = "+0800 2026-09-30 09:26:24 INFO sing-box started (0.02s)\n";

    #[test]
    fn marker_is_the_only_green_criterion() {
        // check rc=0 但没起来，是这套表里最危险的一类（§11.1）
        assert!(judge("", Some(0)).is_err());
        assert!(judge(STARTED, Some(1)).is_ok(), "标记优先于退出码");
    }

    #[test]
    fn fatal_line_beats_the_exit_code_as_the_detail() {
        let log = format!(
            "WARN router: initialize rule-set take too much time to finish!\n{FAILURE_FIXTURE}\n"
        );
        let err = judge(&log, Some(1)).unwrap_err();
        assert!(err.contains("context deadline exceeded"), "{err}");
        assert!(err.contains("rc=1"), "{err}");
    }

    #[test]
    fn error_is_preferred_over_noise_but_fatal_wins() {
        let log = "ERROR outbound/urltest[auto]: dial tcp 198.51.100.10:443: i/o timeout\n\
                   FATAL[0000] create service: initialize router: parse rule-set[0]: no such file\n";
        let detail = failure_detail(log, 1);
        assert!(detail.starts_with("FATAL"), "{detail}");
        assert!(!detail.contains("urltest"), "{detail}");
    }

    /// 2026-10-11 guest 逐字原文：`lan_address` 过期（机器换了地址、配置没跟上）时 oxidns 的
    /// 失败形状。**同一份配置的 `oxidns check` 给的是 `Configuration is valid` + rc=0**——
    /// 上游那道门不验地址，只有绑定期会炸。所以这几行就是"响亮失败"落地的样子：
    /// rc=1、点名地址与 OS 错号、插件按逆序拆干净，不是半起半不说。
    #[test]
    fn a_stale_bind_address_fails_loudly_and_names_the_address() {
        let log = "2026-10-02T05:30:35+08:00 T+0.040 ERROR oxidns::plugin::server::udp:175:Failed to bind UDP socket to 192.0.2.1:15353: I/O error: Address not available (os error 99)\n\
                   2026-10-02T05:30:35+08:00 T+0.040 INFO oxidns::plugin::registry:662:All plugins destroyed\n\
                   2026-10-02T05:30:35+08:00 T+0.040 ERROR oxidns::app:307:Plugin initialization failed: Plugin error: Failed to bind UDP socket to 192.0.2.1:15353: I/O error: Address not available (os error 99)\n";
        let detail = failure_detail(log, 1);
        assert!(detail.contains("192.0.2.1:15353"), "{detail}");
        assert!(detail.contains("Address not available"), "{detail}");
        assert!(
            !detail.contains("destroyed"),
            "INFO 那行不该混进原因：{detail}"
        );
    }

    /// 2026-10-11 guest 逐字原文：`retention_days: 0`（面板上把保留天数填成 0）时 oxidns 的
    /// 失败形状。**同一份 zero.yaml 的 `oxidns check` 也给了 `Configuration is valid` + rc=0**
    /// ——这是那道门不验的第二例（第一例是绑定地址，见下一条测试）。氧化链在
    /// `[5/10] Initializing plugin: record` 这一步当场断，逆序拆完已起的 4 个，rc=1。
    /// 模型与生成器各拦一次（保存 400 / 渲染硬错误）就是为了不把这一坨留给现场。
    #[test]
    fn a_zero_retention_is_a_loud_upstream_refusal_not_a_silent_default() {
        let log = "2026-10-02T06:01:21+08:00 T+0.040 INFO oxidns::plugin::registry:298:  [5/10] Initializing plugin: record (type: query_recorder)\n\
                   2026-10-02T06:01:21+08:00 T+0.040 INFO oxidns::plugin::registry:641:Destroying 4 plugins in reverse order\n\
                   2026-10-02T06:01:21+08:00 T+0.040 INFO oxidns::plugin::registry:662:All plugins destroyed\n\
                   2026-10-02T06:01:21+08:00 T+0.040 ERROR oxidns::app:307:Plugin initialization failed: Plugin error: query_recorder retention_days must be at least 1\n";
        let detail = failure_detail(log, 1);
        assert!(
            detail.contains("retention_days must be at least 1"),
            "{detail}"
        );
        assert!(
            !detail.contains("Destroying"),
            "INFO 那行不该当原因：{detail}"
        );
    }

    #[test]
    fn empty_log_says_where_to_look_instead_of_nothing() {
        assert!(
            failure_detail("", 127).contains("二进制路径"),
            "{}",
            failure_detail("", 127)
        );
    }

    #[test]
    fn scripted_records_exact_argv() {
        let runner = Scripted::new();
        runner
            .exec(&["/usr/local/bin/sing-box", "check", "-c", "/x/config.json"])
            .unwrap();
        assert_eq!(
            runner.argv(),
            vec![vec![
                "/usr/local/bin/sing-box",
                "check",
                "-c",
                "/x/config.json"
            ]]
        );
    }
}
