//! 不可变配置快照存储。每份下发的配置都以独立目录落盘，供 diff 与回滚使用。

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LAST_GOOD: &str = "LAST_GOOD";
/// id 里时间戳段的位数（毫秒，定宽保证字典序即时间序）
const MILLIS_LEN: usize = 13;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("快照写入失败: {0}")]
    Io(#[from] io::Error),
    #[error("快照文件路径非法: {0}")]
    InvalidPath(String),
    #[error("快照 {0} 不存在")]
    Missing(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub id: String,
    pub path: PathBuf,
}

pub struct Store {
    root: PathBuf,
}

/// 一份字节的指纹。
///
/// 与 [content_hash] 同一个函数：面板拿它当 `If-Match` 的 etag，引擎拿它判断快照有没有变，
/// 两处若是两个数，"面板说变了"和"引擎说没变"就能同时成立。
///
/// 用途只有**变更检测**，不是安全摘要——FNV-1a 没有抗碰撞性，别拿它证明"这份文件没被人改过"。
pub fn bytes_digest(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a(bytes))
}

/// 快照指纹：对按路径排序后的 `path + body` 做 FNV-1a。
///
/// 存在的意义是**去重**——下发层用它比较新旧快照，内容没变就不重载内核
/// （重载会掐断所有在用连接，而面板每点一次保存都会走一遍下发）。
pub fn content_hash(files: &BTreeMap<String, String>) -> Result<String, StoreError> {
    let mut content = String::new();
    for (path, body) in files {
        validate_rel(path)?;
        content.push_str(path);
        content.push('\u{1}');
        content.push_str(body);
        content.push('\u{2}');
    }
    Ok(bytes_digest(content.as_bytes()))
}

/// 取 revision id 的指纹段。id 形状由 [looks_like_revision] 保证，返回 None 只可能是手工改过的目录名。
pub fn digest_of(id: &str) -> Option<&str> {
    if looks_like_revision(id) {
        Some(&id[14..])
    } else {
        None
    }
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 写一份新快照，返回其 id 与目录。
    ///
    /// id 为 `{毫秒时间戳}-{指纹}`：时间戳保证字典序即时间序，指纹保证同一毫秒内的不同
    /// 配置不会相撞。内容没变时 [crate::apply::engine::apply] 会在这里之前就短路掉，
    /// 所以走到 `write` 的都是一份真正需要留痕的新快照。
    pub fn write(&self, files: &BTreeMap<String, String>) -> Result<Revision, StoreError> {
        let digest = content_hash(files)?;
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        let id = format!("{millis:013}-{digest}");
        let dir = self.root.join(&id);
        fs::create_dir_all(&dir)?;
        for (path, body) in files {
            let dest = dir.join(path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, body)?;
        }
        let manifest = files.keys().cloned().collect::<Vec<String>>().join("\n");
        fs::write(dir.join("manifest.txt"), manifest)?;
        Ok(Revision { id, path: dir })
    }

    pub fn get(&self, id: &str) -> Result<Revision, StoreError> {
        let dir = self.root.join(id);
        if dir.is_dir() {
            Ok(Revision {
                id: id.into(),
                path: dir,
            })
        } else {
            Err(StoreError::Missing(id.into()))
        }
    }

    /// 由新到旧。id 以毫秒时间戳开头，字典序即时间序。
    pub fn list(&self) -> Vec<Revision> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return out;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().is_dir() && looks_like_revision(&name) {
                out.push(Revision {
                    path: entry.path(),
                    id: name,
                });
            }
        }
        out.sort_by(|a, b| b.id.cmp(&a.id));
        out
    }

    pub fn last_good(&self) -> Option<Revision> {
        let id = fs::read_to_string(self.root.join(LAST_GOOD)).ok()?;
        self.get(id.trim()).ok()
    }

    pub fn set_last_good(&self, id: &str) -> Result<(), StoreError> {
        fs::write(self.root.join(LAST_GOOD), format!("{id}\n"))?;
        Ok(())
    }

    /// 删除超出保留份数的旧快照。最近一次成功下发的快照永不删除。
    pub fn prune(&self, keep: usize) -> Result<Vec<String>, StoreError> {
        let all = self.list();
        let protected = self.last_good().map(|r| r.id);
        let mut removed = Vec::new();
        for (index, rev) in all.iter().enumerate() {
            let over_limit = index >= keep;
            if over_limit && Some(&rev.id) != protected.as_ref() {
                fs::remove_dir_all(&rev.path)?;
                removed.push(rev.id.clone());
            }
        }
        Ok(removed)
    }
}

fn looks_like_revision(name: &str) -> bool {
    // {MILLIS_LEN} 位毫秒 + '-' + 16 位十六进制指纹
    let bytes = name.as_bytes();
    bytes.len() == MILLIS_LEN + 1 + 16
        && bytes[..MILLIS_LEN].iter().all(u8::is_ascii_digit)
        && bytes[MILLIS_LEN] == b'-'
        && bytes[MILLIS_LEN + 1..].iter().all(u8::is_ascii_hexdigit)
}

fn validate_rel(path: &str) -> Result<(), StoreError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains("..")
        || Path::new(path).is_absolute()
    {
        return Err(StoreError::InvalidPath(path.to_string()));
    }
    Ok(())
}

fn fnv1a(input: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    fn temp_store() -> (Store, PathBuf) {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("zbox-store-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (Store::new(&dir), dir)
    }

    fn files(one: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("conf/config.json".to_string(), one.to_string())])
    }

    #[test]
    fn write_then_read_and_id_carries_content_hash() {
        let (store, dir) = temp_store();
        let a = store.write(&files("alpha")).unwrap();
        let b = store.write(&files("alpha")).unwrap();
        // 时间戳段可以不同（每次下发都是独立快照），内容哈希段必须一致
        assert_eq!(hash_of(&a.id), hash_of(&b.id));
        assert_ne!(
            hash_of(&a.id),
            hash_of(&store.write(&files("beta")).unwrap().id)
        );
        assert_eq!(
            fs::read_to_string(a.path.join("conf/config.json")).unwrap(),
            "alpha"
        );
        assert!(a.id.len() == 30 && looks_like_revision(&a.id));
        let _ = fs::remove_dir_all(dir);
    }

    fn hash_of(id: &str) -> &str {
        id.split('-').nth(1).unwrap()
    }

    #[test]
    fn content_hash_matches_the_digest_embedded_in_the_id() {
        // apply 层拿 content_hash 和 digest_of 做比较，两者必须是同一个数
        let (store, dir) = temp_store();
        let f = files("alpha");
        let digest = content_hash(&f).unwrap();
        let rev = store.write(&f).unwrap();
        assert_eq!(digest_of(&rev.id), Some(digest.as_str()));
        assert_eq!(
            digest_of(&rev.id).unwrap().len(),
            16,
            "指纹必须是定宽的十六进制"
        );
        // 非法路径在算指纹时就要被拦住，而不是等 write 落盘
        assert!(matches!(
            content_hash(&BTreeMap::from([("../evil".to_string(), "x".into())])),
            Err(StoreError::InvalidPath(_))
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn digest_of_rejects_handmade_names() {
        assert_eq!(
            digest_of("0000000000000-deadbeefdeadbeef"),
            Some("deadbeefdeadbeef")
        );
        assert_eq!(digest_of("current"), None);
        assert_eq!(digest_of("0000000000000-zzzzzzzzzzzzzzzz"), None);
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        let (store, dir) = temp_store();
        assert!(matches!(
            store.write(&BTreeMap::from([("../evil".to_string(), "x".into())])),
            Err(StoreError::InvalidPath(_))
        ));
        assert!(matches!(
            store.write(&BTreeMap::from([("/etc/passwd".to_string(), "x".into())])),
            Err(StoreError::InvalidPath(_))
        ));
        assert!(store.list().is_empty(), "非法路径不得落盘");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_keeps_last_good_even_if_oldest() {
        let (store, dir) = temp_store();
        let oldest = store.write(&files("v1")).unwrap();
        store.write(&files("v2")).unwrap();
        store.write(&files("v3")).unwrap();
        store.set_last_good(&oldest.id).unwrap();
        let removed = store.prune(1).unwrap();
        assert!(store.get(&oldest.id).is_ok(), "last-good 不得被清理");
        assert!(!removed.contains(&oldest.id));
        assert_eq!(store.list().len(), 3 - removed.len());
        let _ = fs::remove_dir_all(dir);
    }
}
