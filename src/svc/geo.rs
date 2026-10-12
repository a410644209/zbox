//! dae 的 geo 数据落地：`geoip.dat` 与 `geosite.dat`。
//!
//! 为什么这两份要单独管：dae 的 `geoip:` / `geosite:` 指令在 `validate` 与 `run` 两步都要求
//! 这两份数据在查找序里的**同一个目录**中同时存在，缺任一份直接 rc=1 报
//! `geoip.dat: file does not exist in [...]`（`docs/reference/dae-config-dictionary.md` §5，
//! 2026-10-09 v2.1.1 实测）。而这两份数据加起来 25MB，不是能"顺手"带过的东西。
//!
//! 来源只有本地资产这一条路：dae 官方 release 的 `usr/share/dae/` 里有这两份
//! （geoip 23,330,258 B + geosite 2,288,716 B，v2.1.1 实测），ZBox 不自己去网上抓——
//! 旁路由经常装在没有外网的机器上，而 sing-box 那条在线抓取在 guest 上实测撑不住 31MB
//! （见 `provision.rs` 模块头），dae 的链路不会更好。
//!
//! 落地目标选 `/usr/share/dae`：这是 dae 查找序的最后一格，但也是官方包默认落点，
//! 用户最容易理解"dae 的数据在这里"。要改查找序由 [`crate::svc::observe::GEO_DATA_DIRS`] 给。
//!
//! "已存在就不重放"的判据：文件大小与 v2.1.1 实测值相同。零新依赖约束下不用 SHA-256，
//! 而 dae 的 .dat 文件大小是固定的（官方 release 里 geoip 23,330,258 B、geosite 2,288,716 B），
//! 文件大小对上基本可以认定是同一份。这与 sing-box provision 那条 `--force` 语义对齐。

use crate::svc::observe::GEO_DATA_DIRS;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 两份数据文件的文件名，也是 dae 配置里写的名字。
const GEOIP_NAME: &str = "geoip.dat";
const GEOSITE_NAME: &str = "geosite.dat";

/// 默认落点：官方包默认目录，用户最容易找到。
pub const DEFAULT_TARGET_DIR: &str = "/usr/share/dae";

/// v2.1.1 实测的字节数。换版本要连同这里一起改。
///
/// 为什么用文件大小而不是 SHA-256：零新依赖是硬约束，而 dae 的 .dat 文件大小是固定的
/// （官方 release 里 geoip 23,330,258 B、geosite 2,288,716 B），文件大小对上基本可以认定
/// 是同一份。dae 的报错只说"不存在"，不说"版本不对"，一份被截断的文件大小会对不上。
pub const GEOIP_SIZE: u64 = 23_330_258;
pub const GEOSITE_SIZE: u64 = 2_288_716;

/// 落地结果：要么什么都没动，要么换了东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 两份数据都在位且大小对，一次都没动
    AlreadyCurrent,
    /// 刚落地完成；`replaced` = 之前是否已有旧版本
    Installed { replaced: bool },
}

impl Outcome {
    pub fn summarize(&self) -> String {
        match self {
            Self::AlreadyCurrent => "geoip.dat 与 geosite.dat 已在位且大小对，未替换".into(),
            Self::Installed { replaced } => {
                if *replaced {
                    "已把旧版 geo 数据换成 v2.1.1".into()
                } else {
                    "已装上 geoip.dat 与 geosite.dat v2.1.1".into()
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GeoError {
    #[error("本地资产 {path} 用不上：{detail}", path = path.display())]
    Asset { path: PathBuf, detail: String },
    #[error("写 {path} 失败：{detail}", path = path.display())]
    Write { path: PathBuf, detail: String },
    #[error("大小不对：{file} 期望 {expected} 字节，实际 {actual} 字节")]
    SizeMismatch {
        file: String,
        expected: u64,
        actual: u64,
    },
    #[error("文件操作失败：{0}")]
    Io(#[from] io::Error),
}

/// 两份数据各自的来源。最小版只支持本地资产。
#[derive(Debug, Clone)]
pub struct GeoAssets {
    pub geoip: PathBuf,
    pub geosite: PathBuf,
}

/// geo 数据落地器。
pub struct GeoData {
    target_dir: PathBuf,
}

impl GeoData {
    pub fn new(target_dir: &str) -> Self {
        Self {
            target_dir: PathBuf::from(target_dir),
        }
    }

    /// 默认目标目录。
    pub fn default_target() -> Self {
        Self::new(DEFAULT_TARGET_DIR)
    }

    /// 两份数据是否都已在目标目录里、且大小对。
    pub fn is_current(&self) -> bool {
        self.check_file(GEOIP_NAME, GEOIP_SIZE) && self.check_file(GEOSITE_NAME, GEOSITE_SIZE)
    }

    /// 落地。`force=true` 时无视大小直接覆盖。
    pub fn install(&self, assets: &GeoAssets, force: bool) -> Result<Outcome, GeoError> {
        if !force && self.is_current() {
            return Ok(Outcome::AlreadyCurrent);
        }
        let replaced = self.target_dir.join(GEOIP_NAME).is_file()
            || self.target_dir.join(GEOSITE_NAME).is_file();
        fs::create_dir_all(&self.target_dir)?;
        self.copy_and_verify(&assets.geoip, GEOIP_NAME, GEOIP_SIZE)?;
        self.copy_and_verify(&assets.geosite, GEOSITE_NAME, GEOSITE_SIZE)?;
        Ok(Outcome::Installed { replaced })
    }

    fn check_file(&self, name: &str, expected_size: u64) -> bool {
        let path = self.target_dir.join(name);
        match fs::metadata(&path) {
            Ok(meta) => meta.is_file() && meta.len() == expected_size,
            Err(_) => false,
        }
    }

    fn copy_and_verify(&self, src: &Path, name: &str, expected_size: u64) -> Result<(), GeoError> {
        let meta = fs::metadata(src).map_err(|e| GeoError::Asset {
            path: src.to_path_buf(),
            detail: format!("读不到：{e}"),
        })?;
        if !meta.is_file() {
            return Err(GeoError::Asset {
                path: src.to_path_buf(),
                detail: "不是普通文件".into(),
            });
        }
        let dest = self.target_dir.join(name);
        fs::copy(src, &dest).map_err(|e| GeoError::Write {
            path: dest.clone(),
            detail: e.to_string(),
        })?;
        let actual_size = fs::metadata(&dest)
            .map_err(|e| GeoError::Write {
                path: dest.clone(),
                detail: e.to_string(),
            })?
            .len();
        if actual_size != expected_size {
            let _ = fs::remove_file(&dest);
            return Err(GeoError::SizeMismatch {
                file: name.into(),
                expected: expected_size,
                actual: actual_size,
            });
        }
        Ok(())
    }
}

/// 默认查找序里是否有一份在位（两份都在同一个目录）。
/// 这条与 [`crate::svc::observe::geo_data_present`] 同一个判据，这里再给一份是为了
/// provision 这条路径不依赖 observe 模块的导入序。
pub fn present_in_any_search_dir() -> bool {
    crate::svc::observe::geo_data_present_in(&GEO_DATA_DIRS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zbox-geo-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn already_current_short_circuits_when_both_files_match_size() {
        let dir = host_dir("current");
        fs::write(dir.join(GEOIP_NAME), vec![0u8; GEOIP_SIZE as usize]).unwrap();
        fs::write(dir.join(GEOSITE_NAME), vec![0u8; GEOSITE_SIZE as usize]).unwrap();
        let geo = GeoData::new(dir.to_str().unwrap());
        assert!(geo.is_current(), "两份大小都对，该算在位");
        // 用假资产走 install，应该短路
        let assets = GeoAssets {
            geoip: dir.join("nonexistent-geoip"),
            geosite: dir.join("nonexistent-geosite"),
        };
        let outcome = geo.install(&assets, false).unwrap();
        assert_eq!(outcome, Outcome::AlreadyCurrent);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_with_wrong_size_is_not_current() {
        let dir = host_dir("wrong-size");
        fs::write(dir.join(GEOIP_NAME), b"too-small").unwrap();
        fs::write(dir.join(GEOSITE_NAME), vec![0u8; GEOSITE_SIZE as usize]).unwrap();
        let geo = GeoData::new(dir.to_str().unwrap());
        assert!(!geo.is_current(), "geoip 大小不对，不该算在位");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_copies_both_files_and_verifies_size() {
        let dir = host_dir("install");
        let src_dir = dir.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let geoip_src = src_dir.join(GEOIP_NAME);
        let geosite_src = src_dir.join(GEOSITE_NAME);
        // 用假数据，大小对不上
        fs::write(&geoip_src, b"geoip-too-small").unwrap();
        fs::write(&geosite_src, b"geosite-too-small").unwrap();
        let target = dir.join("target");
        let geo = GeoData::new(target.to_str().unwrap());
        let assets = GeoAssets {
            geoip: geoip_src.clone(),
            geosite: geosite_src.clone(),
        };
        let err = geo.install(&assets, false).unwrap_err();
        assert!(matches!(err, GeoError::SizeMismatch { .. }), "{err}");
        // 失败时应该清掉目标文件
        assert!(!target.join(GEOIP_NAME).exists(), "大小不对时目标文件该被清掉");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_with_correct_size_succeeds() {
        let dir = host_dir("correct");
        let src_dir = dir.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let geoip_src = src_dir.join(GEOIP_NAME);
        let geosite_src = src_dir.join(GEOSITE_NAME);
        // 用正确大小的假数据
        fs::write(&geoip_src, vec![0u8; GEOIP_SIZE as usize]).unwrap();
        fs::write(&geosite_src, vec![0u8; GEOSITE_SIZE as usize]).unwrap();
        let target = dir.join("target");
        let geo = GeoData::new(target.to_str().unwrap());
        let assets = GeoAssets {
            geoip: geoip_src.clone(),
            geosite: geosite_src.clone(),
        };
        let outcome = geo.install(&assets, false).unwrap();
        assert!(matches!(outcome, Outcome::Installed { replaced: false }));
        assert!(target.join(GEOIP_NAME).exists());
        assert!(target.join(GEOSITE_NAME).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_asset_is_reported_with_its_path() {
        let dir = host_dir("missing");
        let target = dir.join("target");
        let geo = GeoData::new(target.to_str().unwrap());
        let assets = GeoAssets {
            geoip: dir.join("nope-geoip"),
            geosite: dir.join("nope-geosite"),
        };
        let err = geo.install(&assets, false).unwrap_err();
        assert!(matches!(err, GeoError::Asset { .. }), "{err}");
        assert!(err.to_string().contains("nope-geoip"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn force_overrides_the_short_circuit() {
        let dir = host_dir("force");
        // 先放一份大小对的
        fs::write(dir.join(GEOIP_NAME), vec![0u8; GEOIP_SIZE as usize]).unwrap();
        fs::write(dir.join(GEOSITE_NAME), vec![0u8; GEOSITE_SIZE as usize]).unwrap();
        let geo = GeoData::new(dir.to_str().unwrap());
        assert!(geo.is_current());
        // 再放一份新的源文件
        let src_dir = dir.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let geoip_src = src_dir.join(GEOIP_NAME);
        let geosite_src = src_dir.join(GEOSITE_NAME);
        fs::write(&geoip_src, vec![1u8; GEOIP_SIZE as usize]).unwrap();
        fs::write(&geosite_src, vec![1u8; GEOSITE_SIZE as usize]).unwrap();
        let assets = GeoAssets {
            geoip: geoip_src.clone(),
            geosite: geosite_src.clone(),
        };
        let outcome = geo.install(&assets, true).unwrap();
        assert!(matches!(outcome, Outcome::Installed { replaced: true }));
        // 验证文件被覆盖了（内容从 0 变成 1）
        let content = fs::read(dir.join(GEOIP_NAME)).unwrap();
        assert_eq!(content[0], 1, "force=true 时该覆盖旧文件");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_default_target_dir_matches_the_official_package() {
        assert_eq!(DEFAULT_TARGET_DIR, "/usr/share/dae");
    }

    #[test]
    fn the_pinned_sizes_match_v2_1_1_release() {
        // 2026-10-09 实测原文（dae v2.1.1 官方 release 的 usr/share/dae/）
        assert_eq!(GEOIP_SIZE, 23_330_258);
        assert_eq!(GEOSITE_SIZE, 2_288_716);
    }

    #[test]
    fn present_in_any_search_dir_delegates_to_observe() {
        // 开发机上三个目录都不存在，所以应该返回 false
        assert!(!present_in_any_search_dir());
    }

    #[test]
    fn outcome_summarize_says_what_happened() {
        assert!(Outcome::AlreadyCurrent.summarize().contains("未替换"));
        assert!(Outcome::Installed { replaced: false }
            .summarize()
            .contains("已装上"));
        assert!(Outcome::Installed { replaced: true }
            .summarize()
            .contains("已把旧版"));
    }
}
