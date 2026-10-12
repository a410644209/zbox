//! 下发前的内核语义校验门：`sing-box check` + §11.6 的"空 inbounds 预跑 `run`"。
//!
//! 两条都是实测逼出来的：
//! - `check` rc=0 而 `run` FATAL 的写法至少两条（`detour:"direct"`、远端规则集缺显式
//!   `http_client`，schema §11.1）；
//! - 相对路径一律以 `-D` 为基准，`-D` 缺省是 CWD（§11.7），所以 **`check` 也必须带 `-D`**，
//!   否则会造出"guest 跑得起来、别处 check 说没文件"的假故障。

use crate::gen::init::Layout;
use crate::svc::runner::{failure_detail, Runner};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 预跑上限。实测参照：本地 `.srs` 起得来是 0.02–0.06s，四条远端规则集全下载是 1.40–4.11s，
/// 而规则集初始化超时（必 FATAL）在约 16s。60s 留足重试余量，又不至于让面板卡住不动。
pub const PREFLIGHT_DEADLINE: Duration = Duration::from_secs(60);

/// §11.6 的预跑候选：**只**把 `inbounds` 换成 `[]`，其余一字不动。
///
/// 刻意不清 `log`：判据那行 `sing-box started` 现在写在 `log.output` 指的文件里，
/// 清掉它等于换了判据位置；而且实测 `check` 根本不打开这个路径（目录不存在照样 rc=0），
/// 只有 `run` 会为了它 FATAL（schema §11.8.3），预跑是这条唯一能被门抓住的机会。
/// 追加打开也不会读到上次的旧标记——`prepare_preflight` 每次都把预跑目录清掉重建。
pub fn preflight_candidate(config_json: &str) -> Result<String, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(config_json).map_err(|e| format!("快照 config.json 解析失败: {e}"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "config.json 顶层不是对象".to_string())?;
    object.insert("inbounds".into(), serde_json::Value::Array(Vec::new()));
    serde_json::to_string_pretty(&value).map_err(|e| e.to_string())
}

pub struct Validator<'a> {
    layout: &'a Layout,
    runner: &'a dyn Runner,
}

impl<'a> Validator<'a> {
    pub fn new(layout: &'a Layout, runner: &'a dyn Runner) -> Self {
        Self { layout, runner }
    }

    /// 完整门槛。`snapshot_dir` 是本次待生效的快照目录（不可变）。
    ///
    /// 预跑用独立状态目录（它会真下载规则集并写 `cache.db`，不能污染现役那份），
    /// 但本地 `.srs` 必须让它看得见 —— 相对路径以 `-D` 为基准，所以把状态目录里的
    /// `rule-set/` 复制过去。
    ///
    /// 这条基准同样管 `-c` 自己：实测 `check -c run/min.json -D run` 在文件确实躺着的地方
    /// 报 `open run/min.json: no such file or directory`（schema §11.8.2），所以传进去的
    /// 配置路径必须是绝对路径——`--root` 那条断言就是它的前提，别松。
    pub fn gate(&self, snapshot_dir: &Path) -> Result<(), String> {
        let config = snapshot_dir.join("config.json");
        if !config.is_absolute() {
            return Err(format!(
                "快照目录必须是绝对路径：`-c` 同样按 `-D` 解析，相对路径实测得到 \
                 `open {}: no such file or directory` 这种假故障（schema §11.8.2）",
                config.display()
            ));
        }
        let text = fs::read_to_string(&config)
            .map_err(|e| format!("读取 {} 失败: {e}", config.display()))?;

        let preflight = self.prepare_preflight()?;

        // 1) check：带 -D，路径基准与真跑一致
        let checked = self.runner.exec(&[
            &self.layout.singbox_bin,
            "check",
            "-c",
            &config.display().to_string(),
            "-D",
            &preflight.display().to_string(),
        ])?;
        if !checked.succeeded() {
            return Err(format!(
                "sing-box check 未通过：{}",
                failure_detail(&checked.merged(), checked.code)
            ));
        }

        // 2) 空 inbounds 预跑
        let candidate = preflight_candidate(&text)?;
        let candidate_path = preflight.join("candidate.json");
        fs::write(&candidate_path, &candidate).map_err(|e| format!("写预跑配置失败: {e}"))?;
        let log = preflight.join("preflight.log");
        // 判据在哪个文件里，由配置自己说了算：`log.output` 一旦设置，`sing-box started`
        // 就不再打到 stderr，只看重定向文件会把起得来的配置判成超时（§11.8.3 实测）。
        let kernel_log = kernel_log_of(&candidate, &preflight);
        self.runner.until_started(
            &[
                &self.layout.singbox_bin,
                "run",
                "-c",
                &candidate_path.display().to_string(),
                "-D",
                &preflight.display().to_string(),
            ],
            &log,
            kernel_log.as_deref(),
            PREFLIGHT_DEADLINE,
        )
    }

    /// 预跑目录：先清后建，并把状态目录里已有的 `rule-set/` 复制进去。
    fn prepare_preflight(&self) -> Result<PathBuf, String> {
        let dir = PathBuf::from(self.layout.preflight_dir());
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).map_err(|e| format!("创建预跑目录失败: {e}"))?;
        let live = PathBuf::from(self.layout.rule_set_dir());
        if live.is_dir() {
            copy_rule_sets(&live, &dir)?;
        }
        Ok(dir)
    }
}

/// 把 `log.output` 解析成绝对路径，基准是 `-D`（实测不受进程 cwd 影响，schema §11.8.3）。
/// 没写这条字段就返回 `None`——那时内核把日志打到 stderr，判据只在重定向文件里。
pub fn kernel_log_of(config_json: &str, dir: &Path) -> Option<PathBuf> {
    let output = serde_json::from_str::<serde_json::Value>(config_json)
        .ok()?
        .get("log")?
        .get("output")?
        .as_str()?
        .to_string();
    if output.is_empty() {
        return None;
    }
    // `Path::join` 遇到绝对路径会整个换掉，所以配置里写死绝对路径也是对的
    Some(dir.join(output))
}

/// 只复制一层的普通文件：`.srs` 就是一层（`rule-set/<tag>.srs`），
/// 递归只会把符号链接这类意外拖进来。
pub fn copy_rule_sets(from: &Path, to: &Path) -> Result<usize, String> {
    let dest = to.join("rule-set");
    fs::create_dir_all(&dest).map_err(|e| format!("创建 {} 失败: {e}", dest.display()))?;
    let mut copied = 0usize;
    for entry in fs::read_dir(from).map_err(|e| format!("读取规则集目录失败: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            continue;
        }
        let target = dest.join(entry.file_name());
        fs::copy(entry.path(), &target).map_err(|e| format!("复制规则集失败: {e}"))?;
        copied += 1;
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen;
    use crate::model::config::DesiredState;
    use crate::svc::runner::{Exec, Scripted};
    use serde_json::json;

    fn snapshot(dir: &Path) -> PathBuf {
        let state = DesiredState::baseline();
        let files = gen::snapshot_files(&state).expect("生成快照");
        fs::create_dir_all(dir).unwrap();
        for (rel, body) in &files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, body).unwrap();
        }
        dir.to_path_buf()
    }

    #[test]
    fn candidate_clears_only_inbounds() {
        let state = DesiredState::baseline();
        let files = gen::snapshot_files(&state).unwrap();
        let original: serde_json::Value = serde_json::from_str(&files["config.json"]).unwrap();
        assert!(!original["inbounds"].as_array().unwrap().is_empty());
        let candidate: serde_json::Value =
            serde_json::from_str(&preflight_candidate(&files["config.json"]).unwrap()).unwrap();
        assert_eq!(candidate["inbounds"], json!([]));
        // 其余键必须逐字节等值：dns/route/outbounds 一被改动，预跑抓的就不是同一批 FATAL
        let mut stripped = candidate.clone();
        stripped
            .as_object_mut()
            .unwrap()
            .insert("inbounds".into(), original["inbounds"].clone());
        assert_eq!(stripped, original);
    }

    #[test]
    fn gate_runs_check_then_preflight_with_the_same_directory_basis() {
        let root = std::env::temp_dir().join("zbox-svc-validate-test");
        let _ = fs::remove_dir_all(&root);
        let snapshot = snapshot(&root.join("snapshot"));
        let layout = Layout {
            root: root.display().to_string(),
            current_dir: root.join("current").display().to_string(),
            ..Layout::default()
        };
        let runner = Scripted::new();
        Validator::new(&layout, &runner)
            .gate(&snapshot)
            .expect("门槛该过");

        let argv = runner.argv();
        assert_eq!(argv.len(), 2, "{argv:?}");
        assert_eq!(argv[0][1], "check", "{:?}", argv[0]);
        assert_eq!(argv[1][1], "run", "{:?}", argv[1]);
        for call in &argv {
            let pos = call
                .iter()
                .position(|a| a == "-D")
                .expect("check 与 run 都必须带 -D：相对路径基准不一致会造出假故障");
            assert_eq!(call[pos + 1], layout.preflight_dir(), "{call:?}");
        }
        // 预跑读的是改写后的候选，不是原快照
        assert!(argv[1][3].ends_with("candidate.json"), "{:?}", argv[1]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_relative_snapshot_dir_is_refused_before_it_becomes_a_fake_failure() {
        let layout = Layout::default();
        let runner = Scripted::new();
        let err = Validator::new(&layout, &runner)
            .gate(Path::new("snapshot"))
            .unwrap_err();
        assert!(err.contains("绝对路径"), "{err}");
        assert!(runner.argv().is_empty(), "拒掉之前不该碰任何外部命令");
    }

    /// 判据的位置由配置说了算：`log.output` 一旦设置，`sing-box started` 就不再往 stderr
    /// 抄一份，而实测它的相对基准是 `-D`、不是进程 cwd（schema §11.8.3）。
    /// 门读错文件等于把一份起得来的配置判成超时。
    #[test]
    fn the_gate_reads_the_kernel_log_relative_to_the_same_directory_basis() {
        let dir = Path::new("/var/lib/zbox");
        assert_eq!(
            kernel_log_of(r#"{"log": {"output": "sing-box.log"}}"#, dir),
            Some(PathBuf::from("/var/lib/zbox/sing-box.log"))
        );
        assert_eq!(
            kernel_log_of(r#"{"log": {"output": "/var/log/zbox.log"}}"#, dir),
            Some(PathBuf::from("/var/log/zbox.log")),
            "配置里写死绝对路径时不该跟着 -D 走"
        );
        assert_eq!(kernel_log_of(r#"{"log": {"level": "info"}}"#, dir), None);
        assert_eq!(kernel_log_of(r#"{"log": {"output": ""}}"#, dir), None);
    }

    /// 候选必须原样带着 `log.output`：实测 `check` 对这条路径完全无感（父目录不存在照样
    /// rc=0），只有 `run` 会为它 FATAL，所以门验"日志开不开得出来"的机会只有预跑这一次。
    #[test]
    fn the_candidate_keeps_the_log_section_that_check_cannot_verify() {
        let full = serde_json::to_string(
            &crate::gen::singbox::generate_json(&DesiredState::baseline()).unwrap(),
        )
        .unwrap();
        let candidate = preflight_candidate(&full).unwrap();
        let value: serde_json::Value = serde_json::from_str(&candidate).unwrap();
        assert_eq!(value["inbounds"], serde_json::json!([]), "{candidate}");
        assert_eq!(
            value["log"]["output"].as_str(),
            Some(crate::gen::singbox::LOG_FILE)
        );
        assert_eq!(value["log"]["level"].as_str(), Some("info"), "级别不许动");
    }

    #[test]
    fn check_failure_short_circuits_before_any_preflight_runs() {
        let root = std::env::temp_dir().join("zbox-svc-validate-fail");
        let _ = fs::remove_dir_all(&root);
        let snapshot = snapshot(&root.join("snapshot"));
        let layout = Layout {
            root: root.display().to_string(),
            ..Layout::default()
        };
        let runner = Scripted::with_results(vec![Err("二进制不存在".to_string())]);
        let err = Validator::new(&layout, &runner)
            .gate(&snapshot)
            .unwrap_err();
        assert!(err.contains("二进制不存在"), "{err}");
        assert_eq!(runner.argv().len(), 1, "check 都过不去，不该再预跑");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn non_zero_check_rc_becomes_a_readable_detail() {
        let root = std::env::temp_dir().join("zbox-svc-validate-rc");
        let _ = fs::remove_dir_all(&root);
        let snapshot = snapshot(&root.join("snapshot"));
        let layout = Layout {
            root: root.display().to_string(),
            ..Layout::default()
        };
        let runner = Scripted::with_results(vec![Ok(Exec {
            code: 1,
            stdout: String::new(),
            stderr: "FATAL[0000] decode config: unknown field \"fakeip\"\n".into(),
        })]);
        let err = Validator::new(&layout, &runner)
            .gate(&snapshot)
            .unwrap_err();
        assert!(err.contains("unknown field"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }
}
