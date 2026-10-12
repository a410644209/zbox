//! 劫持层的装卸（nft 表 + 策略路由）。
//!
//! 顺序在这里是硬约束，来自 §11.7 的实测：
//! **先确认内核在听 :53/:9888，再装载劫持层**。反过来的现场是 lan1 查 DNS 直接
//! `nslookup: write to '8.8.8.8': Connection refused`、HTTP 全挂 —— 也就是
//! "配置下发成功、但什么都没代理"这个最难查的形态，只是这次由 DNS 先暴露。
//! 卸载同理先拆 nft（不再送新包进来），再撤策略路由。

use crate::gen::{netstate, nft};
use crate::svc::runner::{failure_detail, Runner};
use std::path::Path;

pub struct Hijack<'a> {
    runner: &'a dyn Runner,
    singbox_bin: String,
}

impl<'a> Hijack<'a> {
    pub fn new(runner: &'a dyn Runner, singbox_bin: impl Into<String>) -> Self {
        Self {
            runner,
            singbox_bin: singbox_bin.into(),
        }
    }

    /// 语法预检：别等劫持层半生效才发现。`nft -c` 是带内核校验的 dry-run，
    /// `sh -n` 只解析不执行（musl/busybox 下生成脚本必须是 POSIX）。
    pub fn check(&self, snapshot: &Path) -> Result<(), String> {
        for script in [nft::NFT_PATH, netstate::APPLY_PATH, netstate::TEARDOWN_PATH] {
            let path = snapshot.join(script);
            let path_text = path.display().to_string();
            let argv: &[&str] = if script == nft::NFT_PATH {
                &["nft", "-c", "-f", &path_text]
            } else {
                &["sh", "-n", &path_text]
            };
            let result = self.runner.exec(&argv)?;
            if !result.succeeded() {
                return Err(format!(
                    "{script} 预检失败：{}",
                    failure_detail(&result.merged(), result.code)
                ));
            }
        }
        Ok(())
    }

    /// 装载：策略路由先就位，再下 nft 表。
    ///
    /// 生成的 `zbox.nft` 头部是 `table ip zbox { }` + `flush table ip zbox`，
    /// `netstate.sh` 对 `ip rule` 是先删后加 —— 两者都实测过"连跑两遍规则数不变"，
    /// 所以这里可以直接重复调用（重复下发不该堆出重复规则）。
    pub fn enable(&self, snapshot: &Path) -> Result<(), String> {
        self.shell(snapshot, netstate::APPLY_PATH)?;
        let nft_path = snapshot.join(nft::NFT_PATH);
        let nft_text = nft_path.display().to_string();
        let loaded = self.runner.exec(&["nft", "-f", &nft_text])?;
        if !loaded.succeeded() {
            return Err(format!(
                "nft 装载失败：{}",
                failure_detail(&loaded.merged(), loaded.code)
            ));
        }
        Ok(())
    }

    /// 卸载。脚本内部每条都带 `2>/dev/null || true`（`set -eu` 之下"本来就没装过"不算失败），
    /// 实测确认它只删自己的对象：同机器上另一张测试用的 nft 表原样还在。
    pub fn disable(&self, snapshot: &Path) -> Result<(), String> {
        self.shell(snapshot, netstate::TEARDOWN_PATH)
    }

    fn shell(&self, snapshot: &Path, script: &str) -> Result<(), String> {
        let path = snapshot.join(script);
        let result = self.runner.exec(&["sh", &path.display().to_string()])?;
        if !result.succeeded() {
            return Err(format!(
                "{script} 执行失败：{}",
                failure_detail(&result.merged(), result.code)
            ));
        }
        Ok(())
    }

    /// sing-box 二进制路径目前没被劫持层用到，但 doctor 的处置命令要复用同一个布局，
    /// 保留访问器免得 svc 反过来依赖字段可见性。
    pub fn singbox_bin(&self) -> &str {
        &self.singbox_bin
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::DesiredState;
    use crate::svc::runner::{Exec, Scripted};
    use std::fs;
    use std::path::PathBuf;

    fn snapshot() -> PathBuf {
        let dir = std::env::temp_dir().join("zbox-svc-hijack-test");
        let files = crate::gen::snapshot_files(&DesiredState::baseline()).unwrap();
        let _ = fs::create_dir_all(&dir);
        for (rel, body) in &files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::write(path, body).unwrap();
        }
        dir
    }

    #[test]
    fn check_covers_every_hijack_artifact_before_touching_the_kernel() {
        let snapshot = snapshot();
        let runner = Scripted::new();
        Hijack::new(&runner, "/usr/local/bin/sing-box")
            .check(&snapshot)
            .unwrap();
        let argv = runner.argv();
        assert_eq!(argv.len(), 3, "{argv:?}");
        assert_eq!(argv[0][..3], ["nft", "-c", "-f"], "{:?}", argv[0]);
        assert!(argv[1][1] == "-n" && argv[0][0] == "nft", "{argv:?}");
        assert!(argv[1][2].ends_with("netstate.sh"), "{:?}", argv[1]);
        assert!(argv[2][2].ends_with("netstate-down.sh"), "{:?}", argv[2]);
    }

    #[test]
    fn enable_loads_routing_before_the_nft_table() {
        let snapshot = snapshot();
        let runner = Scripted::new();
        Hijack::new(&runner, "/usr/local/bin/sing-box")
            .enable(&snapshot)
            .unwrap();
        let argv = runner.argv();
        assert_eq!(argv.len(), 2, "{argv:?}");
        assert!(argv[0][1].ends_with("netstate.sh"), "{:?}", argv[0]);
        assert_eq!(argv[1][0], "nft", "{:?}", argv[1]);
        assert_eq!(argv[1][1], "-f", "{:?}", argv[1]);
    }

    #[test]
    fn a_half_loaded_layer_is_reported_not_swallowed() {
        let snapshot = snapshot();
        // netstate.sh 成功、nft -f 失败（比如内核没开 nft tables）
        let runner = Scripted::with_results(vec![
            Ok(Exec {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
            Ok(Exec {
                code: 1,
                stdout: String::new(),
                stderr: "Error: Kernel doesn't support another address family layer: \
                          Operation not supported\n"
                    .into(),
            }),
        ]);
        let err = Hijack::new(&runner, "/usr/local/bin/sing-box")
            .enable(&snapshot)
            .unwrap_err();
        assert!(err.contains("nft 装载失败"), "{err}");
        assert!(err.contains("Operation not supported"), "{err}");
    }

    #[test]
    fn disable_is_a_single_teardown_call() {
        let snapshot = snapshot();
        let runner = Scripted::new();
        Hijack::new(&runner, "/usr/local/bin/sing-box")
            .disable(&snapshot)
            .unwrap();
        let argv = runner.argv();
        assert_eq!(argv.len(), 1);
        assert!(argv[0][1].ends_with("netstate-down.sh"), "{:?}", argv[0]);
    }
}
