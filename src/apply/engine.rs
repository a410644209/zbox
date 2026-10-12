//! 下发的唯一路径：生成 -> 校验 -> 提交 -> 重载 -> 健康探测 -> 失败回滚。
//!
//! 校验、重载、探测都以闭包注入，因此这条状态机可以在任何平台上完整单测；
//! 真正调用 `sing-box check` / `systemctl` 的实现在 `svc` 层。

use crate::apply::store::{self, Store, StoreError};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_KEEP: usize = 10;

/// 一次下发的结果。
///
/// `Unchanged` 是有意为之的快路径：面板每改一次设置都会走一遍下发，而重载会掐断所有
/// 在用连接。快照逐字节相同（`nft` 脚本、`netstate.sh` 都在指纹里）时，什么都没变，
/// 就不该动内核。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied {
        revision: String,
    },
    /// `revision` 为沿用不变的已知良好快照 id
    Unchanged {
        revision: String,
    },
}

impl ApplyOutcome {
    pub fn revision(&self) -> &str {
        match self {
            Self::Applied { revision } | Self::Unchanged { revision } => revision,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("配置未通过校验，网络状态未做任何变更: {detail}")]
    Validation { detail: String },
    #[error(
        "下发失败，已自动回滚到已知良好配置 {rolled_back_to}（本次修订 {revision}）: {detail}"
    )]
    RolledBack {
        detail: String,
        rolled_back_to: String,
        revision: String,
    },
    #[error(
        "下发失败，且不存在可回滚的已知良好配置，需要人工介入（本次修订 {revision}）: {detail}"
    )]
    NoRollbackPoint { detail: String, revision: String },
    #[error(
        "回滚同样失败，网络可能处于不可用状态，请立即人工检查（本次修订 {revision}）: {detail}"
    )]
    RollbackFailed { detail: String, revision: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub struct Actions<'a> {
    /// 内核语义校验；参数为待生效的快照目录。
    ///
    /// **只跑 `sing-box check` 不够**：VM 实测有两条写法 `check` 放行、`run` 才 FATAL
    /// （`detour:"direct"`、远端规则集缺显式 `http_client`），见
    /// `docs/reference/singbox-1.14.2-schema.md` §11.1/§11.6。svc 层的实现要在这一步
    /// 做 `check` + "空 inbounds 预跑 `run`"（§11.6 定案：不监听任何端口也能走完
    /// DNS 初始化、规则集下载、出站初始化，因而能在不停现役内核的前提下抓到同一批 FATAL）。
    /// 实现见 `crate::svc::validate::Validator::gate`。
    validate: Box<dyn Fn(&Path) -> Result<(), String> + 'a>,
    /// 让内核重新加载配置
    reload: Box<dyn Fn() -> Result<(), String> + 'a>,
    /// 生效性探测（进程存活、端口在听、直连与代理双向可达）
    probe: Box<dyn Fn() -> Result<(), String> + 'a>,
}

impl<'a> Actions<'a> {
    pub fn new(
        validate: impl Fn(&Path) -> Result<(), String> + 'a,
        reload: impl Fn() -> Result<(), String> + 'a,
        probe: impl Fn() -> Result<(), String> + 'a,
    ) -> Self {
        Self {
            validate: Box::new(validate),
            reload: Box::new(reload),
            probe: Box::new(probe),
        }
    }
}

pub trait Target {
    fn commit(&self, revision_dir: &Path) -> Result<(), String>;
    fn read(&self, rel: &str) -> Option<String>;
}

/// 复制到目录。跨平台，供测试与非 symlink 环境使用。
pub struct CopyTarget {
    dir: PathBuf,
}

impl CopyTarget {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

impl Target for CopyTarget {
    fn commit(&self, revision_dir: &Path) -> Result<(), String> {
        let manifest = fs::read_to_string(revision_dir.join("manifest.txt"))
            .map_err(|e| format!("读取快照清单失败: {e}"))?;
        for rel in manifest.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let dest = self.dir.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("创建 {} 失败: {e}", rel))?;
            }
            fs::copy(revision_dir.join(rel), &dest).map_err(|e| format!("写入 {rel} 失败: {e}"))?;
        }
        Ok(())
    }

    fn read(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.dir.join(rel)).ok()
    }
}

/// 符号链接切换。生产环境使用，`rename` 覆盖同名链接在 POSIX 上是原子的。
/// 本模块的单测跑不到这个实现（开发机是 Windows），需在目标机上验证。
#[cfg(unix)]
pub struct SymlinkTarget {
    link: PathBuf,
}

#[cfg(unix)]
impl SymlinkTarget {
    pub fn new(link: impl Into<PathBuf>) -> Self {
        Self { link: link.into() }
    }

    fn staging(&self) -> PathBuf {
        PathBuf::from(format!("{}.new", self.link.display()))
    }
}

#[cfg(unix)]
impl Target for SymlinkTarget {
    fn commit(&self, revision_dir: &Path) -> Result<(), String> {
        let staging = self.staging();
        let _ = fs::remove_file(&staging);
        std::os::unix::fs::symlink(revision_dir, &staging)
            .map_err(|e| format!("创建临时链接失败: {e}"))?;
        fs::rename(&staging, &self.link).map_err(|e| format!("切换 current 链接失败: {e}"))
    }

    fn read(&self, rel: &str) -> Option<String> {
        let current = fs::read_link(&self.link).ok()?;
        fs::read_to_string(current.join(rel)).ok()
    }
}

pub fn apply(
    store: &Store,
    target: &dyn Target,
    files: &BTreeMap<String, String>,
    actions: &Actions<'_>,
    keep: usize,
) -> Result<ApplyOutcome, ApplyError> {
    let digest = store::content_hash(files)?;
    if let Some(good) = store.last_good() {
        if store::digest_of(&good.id).is_some_and(|d| d == digest) {
            return Ok(ApplyOutcome::Unchanged { revision: good.id });
        }
    }

    let revision = store.write(files)?;

    if let Err(detail) = (actions.validate)(&revision.path) {
        return Err(ApplyError::Validation { detail });
    }

    // 只有已知良好配置才值得回滚到，本次提交前的状态才是安全落点
    let baseline = store.last_good();

    if let Err(detail) = target
        .commit(&revision.path)
        .and_then(|()| (actions.reload)())
        .and_then(|()| (actions.probe)())
    {
        return Err(match baseline {
            None => ApplyError::NoRollbackPoint {
                detail,
                revision: revision.id,
            },
            Some(base) => match target
                .commit(&base.path)
                .map_err(|e| format!("回滚提交失败: {e}"))
                .and_then(|()| (actions.reload)())
            {
                Ok(()) => ApplyError::RolledBack {
                    detail,
                    rolled_back_to: base.id,
                    revision: revision.id,
                },
                Err(rollback_detail) => ApplyError::RollbackFailed {
                    detail: format!("{detail}；{rollback_detail}"),
                    revision: revision.id,
                },
            },
        });
    }

    store.set_last_good(&revision.id)?;
    store.prune(keep)?;
    Ok(ApplyOutcome::Applied {
        revision: revision.id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    struct Harness {
        store: Store,
        target: CopyTarget,
        dir: PathBuf,
        calls: Rc<RefCell<Vec<&'static str>>>,
    }

    fn harness() -> Harness {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("zbox-apply-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Harness {
            store: Store::new(dir.join("revisions")),
            target: CopyTarget::new(dir.join("current")),
            dir,
            calls: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn files(body: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("config.json".to_string(), body.to_string())])
    }

    /// 全 pass 的动作集，可单项替换为失败；reload 记录调用次数
    fn actions<'a>(
        h: &'a Harness,
        validate: impl Fn(&Path) -> Result<(), String> + 'a,
        probe: impl Fn() -> Result<(), String> + 'a,
    ) -> Actions<'a> {
        let reload_calls = Rc::clone(&h.calls);
        Actions::new(
            validate,
            move || {
                reload_calls.borrow_mut().push("reload");
                Ok(())
            },
            probe,
        )
    }

    fn pass_probe() -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn happy_path_commits_and_marks_last_good() {
        let h = harness();
        let outcome = apply(
            &h.store,
            &h.target,
            &files("v1"),
            &actions(&h, |_| Ok(()), pass_probe),
            DEFAULT_KEEP,
        )
        .unwrap();
        assert_eq!(h.target.read("config.json").as_deref(), Some("v1"));
        assert_eq!(
            h.store.last_good().unwrap().id,
            outcome.revision(),
            "{outcome:?}"
        );
        assert_eq!(*h.calls.borrow(), vec!["reload"], "正常路径只重载一次");
        fs::remove_dir_all(h.dir).unwrap();
    }

    /// 借鉴 LocalDNS-FJ945 的 policy hash：内容未变就完全不碰内核。
    #[test]
    fn identical_snapshot_never_reloads_the_kernel() {
        let h = harness();
        let first = apply(
            &h.store,
            &h.target,
            &files("v1"),
            &actions(&h, |_| Ok(()), pass_probe),
            DEFAULT_KEEP,
        )
        .unwrap();
        assert!(matches!(first, ApplyOutcome::Applied { .. }));
        h.calls.borrow_mut().clear();

        let again = apply(
            &h.store,
            &h.target,
            &files("v1"),
            &actions(&h, |_| Ok(()), pass_probe),
            DEFAULT_KEEP,
        )
        .unwrap();
        assert_eq!(
            again,
            ApplyOutcome::Unchanged {
                revision: first.revision().to_string()
            },
            "{again:?}"
        );
        assert!(h.calls.borrow().is_empty(), "内容未变不得校验/重载/探测");
        assert_eq!(h.store.list().len(), 1, "也不该堆一份无信息量的快照");

        // 改一个字节就必须真的下发
        let changed = apply(
            &h.store,
            &h.target,
            &files("v2"),
            &actions(&h, |_| Ok(()), pass_probe),
            DEFAULT_KEEP,
        )
        .unwrap();
        assert!(matches!(changed, ApplyOutcome::Applied { .. }));
        assert_eq!(*h.calls.borrow(), vec!["reload"]);
        fs::remove_dir_all(h.dir).unwrap();
    }

    #[test]
    fn unchanged_short_circuit_is_keyed_on_the_whole_snapshot() {
        // 只有 config.json 变、nft 脚本没变，或反之，都必须触发下发：
        // 劫持层和内核配置是一体的，短路判据必须覆盖整个快照
        let h = harness();
        let mixed = |dns: &str, nft: &str| {
            BTreeMap::from([
                ("config.json".to_string(), dns.to_string()),
                ("zbox.nft".to_string(), nft.to_string()),
            ])
        };
        apply(
            &h.store,
            &h.target,
            &mixed("v1", "nft-a"),
            &actions(&h, |_| Ok(()), pass_probe),
            DEFAULT_KEEP,
        )
        .unwrap();
        h.calls.borrow_mut().clear();
        let outcome = apply(
            &h.store,
            &h.target,
            &mixed("v1", "nft-b"),
            &actions(&h, |_| Ok(()), pass_probe),
            DEFAULT_KEEP,
        )
        .unwrap();
        assert!(
            matches!(outcome, ApplyOutcome::Applied { .. }),
            "{outcome:?}"
        );
        assert_eq!(*h.calls.borrow(), vec!["reload"]);
        fs::remove_dir_all(h.dir).unwrap();
    }

    #[test]
    fn validation_failure_changes_nothing() {
        let h = harness();
        // 先建立一份已知良好配置
        apply(
            &h.store,
            &h.target,
            &files("good"),
            &actions(&h, |_| Ok(()), pass_probe),
            DEFAULT_KEEP,
        )
        .unwrap();
        h.calls.borrow_mut().clear();

        let err = apply(
            &h.store,
            &h.target,
            &files("broken"),
            &actions(
                &h,
                |_| Err("sing-box check: unknown field".into()),
                pass_probe,
            ),
            DEFAULT_KEEP,
        )
        .unwrap_err();

        assert!(matches!(err, ApplyError::Validation { .. }), "{err}");
        assert_eq!(h.target.read("config.json").as_deref(), Some("good"));
        assert!(h.calls.borrow().is_empty(), "校验失败不得触发重载");
        fs::remove_dir_all(h.dir).unwrap();
    }

    #[test]
    fn probe_failure_rolls_back_to_previous_good_config() {
        let h = harness();
        let good = apply(
            &h.store,
            &h.target,
            &files("good"),
            &actions(&h, |_| Ok(()), pass_probe),
            DEFAULT_KEEP,
        )
        .unwrap();
        let good = good.revision().to_string();
        h.calls.borrow_mut().clear();

        let err = apply(
            &h.store,
            &h.target,
            &files("bad"),
            &actions(&h, |_| Ok(()), || Err("经代理出口取不到响应".into())),
            DEFAULT_KEEP,
        )
        .unwrap_err();

        match err {
            ApplyError::RolledBack { rolled_back_to, .. } => assert_eq!(rolled_back_to, good),
            other => panic!("{other}"),
        }
        assert_eq!(
            h.target.read("config.json").as_deref(),
            Some("good"),
            "必须回到旧配置"
        );
        assert_eq!(h.store.last_good().unwrap().id, good);
        // 一次坏下发 + 一次回滚 = 两次重载
        assert_eq!(h.calls.borrow().len(), 2);
        fs::remove_dir_all(h.dir).unwrap();
    }

    #[test]
    fn first_apply_failure_has_no_rollback_point() {
        let h = harness();
        let err = apply(
            &h.store,
            &h.target,
            &files("bad"),
            &actions(&h, |_| Ok(()), || Err("探测超时".into())),
            DEFAULT_KEEP,
        )
        .unwrap_err();
        assert!(matches!(err, ApplyError::NoRollbackPoint { .. }), "{err}");
        assert!(h.store.last_good().is_none());
        fs::remove_dir_all(h.dir).unwrap();
    }

    #[test]
    fn rollback_that_fails_is_reported_separately() {
        let h = harness();
        apply(
            &h.store,
            &h.target,
            &files("good"),
            &actions(&h, |_| Ok(()), pass_probe),
            DEFAULT_KEEP,
        )
        .unwrap();
        // 破坏目标目录，使回滚提交也失败
        fs::remove_file(h.store.list()[0].path.join("manifest.txt")).unwrap();

        let err = apply(
            &h.store,
            &h.target,
            &files("bad"),
            &actions(&h, |_| Ok(()), || Err("探测失败".into())),
            DEFAULT_KEEP,
        )
        .unwrap_err();
        assert!(matches!(err, ApplyError::RollbackFailed { .. }), "{err}");
        fs::remove_dir_all(h.dir).unwrap();
    }
}
