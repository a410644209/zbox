//! 订阅抓取：把"机场给的那个链接"变成一段字节，再交给 [`crate::ingest::subscription`] 解析成节点。
//!
//! 三层各管一段，这一层只管"要字节"：解析是纯函数（住在 `ingest`，全测覆盖），
//! 合并进 `nodes[]` 与写盘住在 `src/api`（那里才有磁盘指纹与丢更新保护）。
//! 抓取用的是规则集那同一对下载器判据（[`crate::svc::ruleset::download_body`]）——
//! guest 实测只有 busybox wget，而它的 `-T` 是空闲上限不是总时长，这条实测结论只该有一处来源。

use super::ruleset::{detect_downloader, download_body, FetchLimits};
use super::runner::Runner;
use crate::gen::init::Layout;
use crate::ingest::subscription::{self, Meta, Parsed};
use std::fs;

/// 订阅链接的长度上限。机场链接是"一串 query 参数带令牌"，实测几十到几百字节；
/// 两 KiB 已经宽到装得下任何合法形状，而它挡住的是"粘贴错了东西"那一类。
pub const MAX_URL: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum FetchFailure {
    #[error("订阅内容读不成节点：{0}")]
    Unreadable(#[from] subscription::SubscriptionError),
    #[error("抓取订阅失败：{0}")]
    Downstream(String),
}

/// 面板上那一格里的文本 -> 一个允许去抓的 URL。
///
/// 只收 `http` / `https`：下载器是 curl 与 wget，放一个 `file://` 过去就等于让面板用它自己的
/// 权限去读这台机器上的任意文件；`0.0.0.0` 之类倒是拦不住也不必拦——给这台机器自己发一个请求
/// 不换来任何新东西，令牌那一关已经在前面了。
pub fn check_url(raw: &str) -> Result<String, String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err("是空的。订阅链接要从机场的控制台上「一键复制」，粘进来再按导入。".to_string());
    }
    if url.len() > MAX_URL {
        return Err(format!("太长了（{} 字节，上限 {MAX_URL}）：这一格要的是订阅链接，不是订阅内容", url.len()));
    }
    if url.chars().any(char::is_whitespace) {
        return Err("中间有空格：粘贴时多半带进了换行或别的文字，只留链接那一串。".to_string());
    }
    if url.chars().any(|c| c.is_control()) {
        return Err("里面带着控制字符（不可见的东西），粘出来的不是纯链接。".to_string());
    }
    let lower = url.to_ascii_lowercase();
    let rest = if let Some(rest) = lower.strip_prefix("https://") {
        rest
    } else if let Some(rest) = lower.strip_prefix("http://") {
        rest
    } else {
        return Err("开头不是 http:// 或 https://：机场给的订阅链接一定是这两者之一。".to_string());
    };
    // scheme 之后到第一个 `/ ? #` 之前是主机那一截，空的就没有可连的地方
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.is_empty() {
        return Err("协议后面没有主机名（像 `https://` 就停住了）。".to_string());
    }
    Ok(url.to_string())
}

pub struct Subscribe<'a> {
    layout: &'a Layout,
    runner: &'a dyn Runner,
}

impl<'a> Subscribe<'a> {
    pub fn new(layout: &'a Layout, runner: &'a dyn Runner) -> Self {
        Self { layout, runner }
    }

    /// 暂存件落点：状态目录里一个固定名字。它是订阅原文——里面就是那批凭据，
    /// 所以读完立刻删；状态目录本身是这台机器上唯一 0700 的那一个。
    fn temp_path(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.layout.state_dir()).join("subscription.download")
    }

    /// 抓一次订阅并解析成节点和账户元数据。**不落盘、不改状态**——那一步由调用方在磁盘指纹的保护下做。
    pub fn load(&self, url: &str) -> Result<(Parsed, Meta), FetchFailure> {
        let tmp = self.temp_path();
        // 新开的一台机器上状态目录还不存在（`state::write` 也是自己建的），而下拉器是
        // `wget -O <那个目录>/subscription.download`——目录不在它就退出在"找不到路径"上，
        // 报成一次网络故障。主路第一步恰好就是这个现场，所以先把目录要出来。
        if let Some(parent) = tmp.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                FetchFailure::Downstream(format!("建不出暂存件的目录 {}：{e}", parent.display()))
            })?;
        }
        let body = download_body(
            self.runner,
            detect_downloader(self.runner),
            url,
            &tmp,
            FetchLimits::SOURCE,
        )
        .map_err(FetchFailure::Downstream)?;
        let parsed = subscription::parse(&body).map_err(FetchFailure::Unreadable)?;
        let meta = parsed.meta.clone();
        Ok((parsed, meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svc::runner::{Exec, Scripted};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;

    const L1: &str =
        "vless://aaaaaaaa-1111-1111-1111-111111111111@1.2.3.4:443?security=tls&sni=a.com#东京";

    fn layout(name: &str) -> Layout {
        let dir = std::env::temp_dir().join(format!("zbox-sub-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Layout {
            root: dir.display().to_string(),
            ..Layout::default()
        }
    }

    #[test]
    fn only_http_and_https_are_allowed_to_reach_the_downloader() {
        assert!(check_url("https://x.example/sub?token=1").is_ok());
        assert_eq!(
            check_url("HTTPS://X.example/sub").unwrap(),
            "HTTPS://X.example/sub",
            "协议大小写不敏感，但原文要原样交给下载器"
        );
        for bad in [
            "",
            "   ",
            "ftp://x.example/s",
            "file:///etc/passwd",
            "/sub/link.txt",
            "https://",
            "https://x.example/s a",
            "订阅链接：https://x.example/s",
        ] {
            let detail = check_url(bad).err().unwrap_or_else(|| panic!("{bad:?} 竟然放行了"));
            assert!(!detail.is_empty(), "{bad:?} 拦下了却没给原因");
        }
        let long = format!("https://x.example/{}", "a".repeat(MAX_URL));
        assert!(check_url(&long).unwrap_err().contains("上限"));
        // 换行是最常见的一次粘错，而它属于"空白"，不是"控制字符"那一条
        assert!(check_url("https://x.example/s\n#备注")
            .unwrap_err()
            .contains("空格"));
    }

    /// busybox wget 的现场：`curl --version` 非零退出的那一次探测之后才发抓取命令。
    #[test]
    fn the_fetcher_is_probed_then_asked_for_the_subscription_body() {
        let layout = layout("wget");
        // 抓取命令的落点由下载器自己决定（`-O` 后面那个路径），而 Scripted 不起进程，
        // 所以"把响应体写进去"这一步得由这个假下载器自己做。
        struct Writes {
            body: Vec<u8>,
        }
        impl Runner for Writes {
            fn exec(&self, argv: &[&str]) -> Result<Exec, String> {
                if argv.first().copied() == Some("curl") {
                    return Ok(Exec {
                        code: 127,
                        stdout: String::new(),
                        stderr: "curl: not found".into(),
                    });
                }
                assert_eq!(argv.first().copied(), Some("wget"), "{argv:?}");
                let out = argv
                    .iter()
                    .position(|a| *a == "-O")
                    .and_then(|i| argv.get(i + 1))
                    .expect("wget 的落点在 -O 后面");
                std::fs::write(out, &self.body).unwrap();
                Ok(Exec {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            fn exec_stdin(&self, argv: &[&str], _: &[u8]) -> Result<Exec, String> {
                Err(format!("订阅抓取不该走 stdin：{argv:?}"))
            }
            fn until_started(
                &self,
                _: &[&str],
                _: &std::path::Path,
                _: Option<&std::path::Path>,
                _: std::time::Duration,
            ) -> Result<(), String> {
                Err("订阅抓取不预跑任何东西".into())
            }
        }
        let fetcher = Writes {
            body: STANDARD.encode(L1).into_bytes(),
        };
        let subscribe = Subscribe::new(&layout, &fetcher);
        let (parsed, _meta) = subscribe
            .load("https://x.example/sub?token=1")
            .expect("该读成一个节点");
        assert_eq!(parsed.nodes.len(), 1);
        assert_eq!(parsed.nodes[0].name, "东京");
        assert_eq!(parsed.format, subscription::Format::Base64LinkList);
        assert!(
            !std::path::Path::new(&layout.state_dir())
                .join("subscription.download")
                .exists(),
            "订阅原文是凭据，读完必须把暂存件删掉"
        );
    }

    /// 机场返回 Clash 配置时那句话要说清"这不是 ZBox 能吃的形状"，而不是把网络问题报成解析问题。
    #[test]
    fn a_clash_subscription_is_an_unreadable_body_not_a_failed_fetch() {
        struct Clash;
        impl Runner for Clash {
            fn exec(&self, argv: &[&str]) -> Result<Exec, String> {
                // 探测器问的是 `curl --version`，抓取命令也是 curl 起的头——只能按参数分
                if argv.contains(&"--version") {
                    return Ok(Exec {
                        code: 0,
                        stdout: "curl 8.0\n".into(),
                        stderr: String::new(),
                    });
                }
                let out = argv
                    .iter()
                    .position(|a| *a == "-o")
                    .and_then(|i| argv.get(i + 1))
                    .expect("curl 的落点在 -o 后面");
                std::fs::write(out, "mixed-port: 7890\nproxies:\n  - name: a\n").unwrap();
                Ok(Exec {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            fn exec_stdin(&self, argv: &[&str], _: &[u8]) -> Result<Exec, String> {
                Err(format!("{argv:?}"))
            }
            fn until_started(
                &self,
                _: &[&str],
                _: &std::path::Path,
                _: Option<&std::path::Path>,
                _: std::time::Duration,
            ) -> Result<(), String> {
                Err("不预跑".into())
            }
        }
        let layout = layout("clash");
        let subscribe = Subscribe::new(&layout, &Clash);
        let got = subscribe.load("https://x.example/sub").unwrap_err();
        assert!(matches!(got, FetchFailure::Unreadable(_)), "{got}");
        assert!(got.to_string().contains("Clash"), "{got}");
    }

    /// 下载器非零退出是"网络那一侧"：报成 Downstream，让回执说"去查网络"而不是"换个链接"。
    #[test]
    fn a_dead_link_is_a_downstream_failure() {
        let layout = layout("dead");
        let runner = Scripted::with_results(vec![
            Ok(Exec {
                code: 127,
                stdout: String::new(),
                stderr: "curl: not found".into(),
            }),
            Ok(Exec {
                code: 22,
                stdout: String::new(),
                stderr: "wget: server returned error: HTTP/1.1 404 Not Found".into(),
            }),
        ]);
        let subscribe = Subscribe::new(&layout, &runner);
        let got = subscribe.load("https://x.example/gone").unwrap_err();
        assert!(matches!(got, FetchFailure::Downstream(_)), "{got}");
        assert!(got.to_string().contains("404"), "{got}");
    }
}
