//! 期望状态的磁盘形状：规范落点、schema 版本门、原子写。
//!
//! 三件事各防一种故障，别混着看：
//!
//! - **规范落点**（[`crate::gen::init::Layout::state_path`]）：在这之前 `state.json` 只是
//!   `zboxd` 的一个位置参数，"当前那份状态在哪"没有答案。面板要读的、CLI 要写的必须是同一个文件。
//! - **版本门**：映射报告 §4 记下他们靠 `requiredConfigSchema` 挡不认识的配置包。ZBox 的状态文件
//!   迟早由面板写进新字段，没有门的话**旧 zboxd 读一份新状态会把认不出的字段按 serde 默认值补上、
//!   再原样写回磁盘**——用户的配置不是读失败，是被静默改掉了。这一条比读失败严重得多。
//! - **原子写**：同样来自 §4，他们那条链路上有两种写法，其中一种是固定名 `.tmp` + 不 fsync +
//!   0644。这里用唯一临时名、写完 fsync、**创建时就 0600**：状态文件里的订阅 URL 带着个人令牌
//!   （见 `docs/ARCHITECTURE.md` 的硬约束），0644 等于把令牌摊给同机的每个账号。

use crate::model::config::{
    describe_problems, DesiredState, DomainAction, DomainPattern, DomainRule,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::Path;

/// 磁盘状态文件的格式版本。改 `DesiredState` 的**语义**（不只是加一个能缺省的字段）时 +1，
/// 并在 [`migrate`] 里补上对应的迁移。
///
/// v2 = 三档域名名单（`direct_domains` / `proxy_domains` / `blocked_domains`）合成一条
/// 有序 `domain_rules`，顺序即优先级。
pub const STATE_SCHEMA: u32 = 2;

/// 没有 `schema` 键的文件是哪个版本：版本门是后加的，加之前只有 v1 一种磁盘形状，
/// 所以"缺键"读成 v1 不是让步，而是唯一正确的读法。
const LEGACY_SCHEMA: u32 = 1;

/// 磁盘上的形状 = `DesiredState` 的全部字段 + 一个 `schema`。
///
/// 用 `flatten` 而不是套一层壳，是为了让"加这道门之前写过的文件"和"现在写的文件"是同一个形状：
/// 老文件只是少一个键，不是少一层。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateFile {
    /// 缺省 = [`LEGACY_SCHEMA`]，见上。真正的读路径在 [`parse`] 里自己取这个键再迁移。
    #[serde(default = "legacy_schema")]
    pub schema: u32,
    #[serde(flatten)]
    pub state: DesiredState,
}

fn legacy_schema() -> u32 {
    LEGACY_SCHEMA
}

impl StateFile {
    pub fn new(state: DesiredState) -> Self {
        Self {
            schema: STATE_SCHEMA,
            state,
        }
    }
}

/// 文本 -> 期望状态：读版本 -> 迁移 -> 反序列化 -> `validate()`。
///
/// 迁移必须排在反序列化**之前**：v2 的 `Policy` 里没有 `direct_domains` 这些键，而 serde
/// 默认忽略未知字段。反着来的话一份 v1 文件会"读成功"但名单被静默清空，用户下一次保存
/// 就把他的分流配置写没了——那比读失败严重。
///
/// 版本不对时先报"版本不认"再谈字段名，因为那些名字在新版本里可能已经变了。
///
/// 不成立那一条给的是 [`describe_problems`] 的**清单本身**，这里不顶"期望状态不成立:"那种帽子：
/// 这一句要同时走 CLI（前面只有文件路径）与面板 PUT（面板自己那句"保存被拒（HTTP 400）："），
/// 共享串里再放一层标签，屏幕上就是两层标签叠在一句里（§11.13 边界 1）。
pub fn parse(text: &str) -> Result<DesiredState, String> {
    let mut value: Value =
        serde_json::from_str(text).map_err(|e| format!("状态文件解析失败: {e}"))?;
    let found = read_schema(&value)?;
    if found > STATE_SCHEMA {
        return Err(format!(
            "状态文件的 schema 是 v{found}，这个 zboxd 只认到 v{STATE_SCHEMA}：它由更新版本的 \
            ZBox 写过。请用新版 zboxd 读写它——旧版本认不出的字段会被默认值顶掉再写回磁盘。"
        ));
    }
    if found < STATE_SCHEMA {
        migrate(&mut value, found)?;
    }
    let file: StateFile =
        serde_json::from_value(value).map_err(|e| format!("状态文件解析失败: {e}"))?;
    file.state
        .validate()
        .map_err(|problems| describe_problems(&problems))?;
    Ok(file.state)
}

fn read_schema(value: &Value) -> Result<u32, String> {
    match value.get("schema") {
        None => Ok(LEGACY_SCHEMA),
        Some(raw) => raw
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| format!("状态文件的 schema 键不是一个非负整数: {raw}")),
    }
}

/// 把老版本的磁盘形状就地迁到当前版本。只认识 v1 -> v2；更老的版本在这里明说没有迁移，
/// 比让反序列化去猜"少了哪个键"有用。
fn migrate(value: &mut Value, from: u32) -> Result<(), String> {
    if from != 1 {
        return Err(format!(
            "状态文件的 schema 是 v{from}：ZBox 写过 v1 和 v{STATE_SCHEMA}，\
            v{from} -> v{STATE_SCHEMA} 没有对应的迁移。"
        ));
    }
    let Value::Object(root) = value else {
        return Err("状态文件顶层不是一个对象，没法迁移".into());
    };
    let Some(Value::Object(policy)) = root.get_mut("policy") else {
        // 没有 policy 就没什么可迁的，缺字段由后面的反序列化报出来
        root.insert("schema".into(), json!(STATE_SCHEMA));
        return Ok(());
    };
    let mut merged: Vec<Value> = Vec::new();
    const LEGACY: [(&str, DomainAction); 3] = [
        ("blocked_domains", DomainAction::Block),
        ("direct_domains", DomainAction::Direct),
        ("proxy_domains", DomainAction::Proxy),
    ];
    let present: Vec<&str> = LEGACY
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| policy.contains_key(*key))
        .collect();
    if present.is_empty() {
        // 一档老名单都没有，说明这份已经是 v2 的形状（多半是手写的、忘了写 schema 键），
        // 只补版本键，不碰内容
        root.insert("schema".into(), json!(STATE_SCHEMA));
        return Ok(());
    }
    if policy.contains_key("domain_rules") {
        return Err(format!(
            "v1 的分档名单（{}）和 v2 的 domain_rules 同时存在：两套格式混在一份文件里，\
             不敢猜该丢哪一档",
            present.join("、")
        ));
    }
    // 展开顺序 = v1 阶梯里的实际生效顺序（#18 定案"拦截优先"），所以迁上来的老配置行为不变
    for (key, action) in LEGACY {
        let Some(found) = policy.remove(key) else {
            continue;
        };
        let Value::Array(items) = found else {
            return Err(format!("v1 的 {key} 不是一个数组: {found}"));
        };
        for item in items {
            let pattern: DomainPattern = serde_json::from_value(item)
                .map_err(|e| format!("v1 的 {key} 里有一条认不出来的域名规则: {e}"))?;
            merged.push(rule_value(pattern, action)?);
        }
    }
    policy.insert("domain_rules".into(), Value::Array(merged));
    root.insert("schema".into(), json!(STATE_SCHEMA));
    Ok(())
}

/// 一条迁移后的规则。走 `DomainRule` 的 Serialize 而不是手搓 JSON，
/// 是为了让"迁出来的形状"和"v2 自己写的形状"不可能各走各的。
fn rule_value(pattern: DomainPattern, action: DomainAction) -> Result<Value, String> {
    serde_json::to_value(DomainRule::new(pattern, action))
        .map_err(|e| format!("迁移后的域名规则写不出磁盘形状: {e}"))
}

/// 期望状态 -> 磁盘文本（含 `schema`）。
pub fn to_text(state: &DesiredState) -> String {
    // 纯数据（没有 f64、没有非字符串键的 map），序列化在这里不可能失败；
    // 真要炸了也是形状问题，值得在开发期就叫出来，而不是伪装成一次写盘故障。
    serde_json::to_string_pretty(&StateFile::new(state.clone()))
        .expect("DesiredState 是纯数据，序列化不该失败")
}

/// 读一个状态文件：落点会出现在每条报错里，因为 `--root` 一改就有多个候选文件。
pub fn read(path: &Path) -> Result<DesiredState, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// 原子写：唯一临时名 -> 写入 + fsync -> rename 顶替。
///
/// fsync 落在**文件**上；父目录没 fsync，所以断电时"rename 已生效"这一步理论上还能丢——
/// 路由器上的状态文件不是数据库，这里要的是"别写出半份文件"，不是"别丢最后一次写"。
pub fn write(path: &Path, state: &DesiredState) -> Result<(), String> {
    write_private(path, to_text(state).as_bytes())
}

/// 上面那套纪律的公共部分，**带字节的**状态之外还要用它落盘一份凭据（面板令牌，
/// `src/api/token.rs`）：两处共用一条"0600 在 create 就给、不留临时文件"的实现，
/// 免得令牌走的那条路径比状态文件松。
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "创建 {} 所在目录 {} 失败: {e}",
                path.display(),
                parent.display()
            )
        })?;
    }
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} 没有文件名，写不了", path.display()))?
        .to_string_lossy()
        .into_owned();
    // 唯一名（带 pid）：他们那套固定 `.tmp` 在两个写者并发时会互相踩（§4）
    let temp = path.with_file_name(format!("{name}.tmp-{}", std::process::id()));
    match write_then_replace(&temp, path, bytes) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 尽力清掉，清不掉也要把真故障报出来
            let _ = fs::remove_file(&temp);
            Err(e)
        }
    }
}

fn write_then_replace(temp: &Path, path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file =
        create_private(temp).map_err(|e| format!("创建临时文件 {} 失败: {e}", temp.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("写入临时文件 {} 失败: {e}", temp.display()))?;
    drop(file);
    replace(temp, path)
        .map_err(|e| format!("把 {} 顶替成 {} 失败: {e}", temp.display(), path.display()))
}

/// 权限在 `create` 上就给到位，而不是写完再 chmod：中间那段窗口在共享主机上就是可读。
#[cfg(unix)]
fn create_private(temp: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(temp)
}

#[cfg(not(unix))]
fn create_private(temp: &Path) -> std::io::Result<fs::File> {
    fs::File::create(temp)
}

#[cfg(unix)]
fn replace(temp: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(temp, path)
}

#[cfg(not(unix))]
fn replace(temp: &Path, path: &Path) -> std::io::Result<()> {
    // Windows 的 rename 不许顶替已存在的目标，而开发机就是 Windows：不处理的话
    // 第一次写成功、之后每次写都报"文件已存在"。代价是这里有一小段非原子的窗口，
    // 生产目标（Linux）走上面那条真 rename。
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zbox-state-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> DesiredState {
        let mut state = DesiredState::baseline();
        state.network.lan_interface = "eth0".into();
        state.network.wan_interface = "eth0".into();
        state.network.lan_cidr = "192.168.219.0/24".into();
        state
    }

    /// 只动 schema 键，别的字段一概照原样保留。
    fn with_schema(state: &DesiredState, schema: u32) -> String {
        let mut value: serde_json::Value = serde_json::to_value(state).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("schema".into(), schema.into());
        serde_json::to_string_pretty(&value).unwrap()
    }

    #[test]
    fn every_write_carries_the_schema_key() {
        let text = to_text(&sample());
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema"], STATE_SCHEMA);
        // flatten 的作用：字段和 schema 平铺在同一层，没有多出来的 "state" 壳
        assert!(value.get("state").is_none());
        assert!(value.get("network").is_some());
    }

    #[test]
    fn a_file_written_before_the_gate_still_reads_as_v1() {
        // 加门之前 zboxd 写下的形状：没有 schema 键，而且是三档名单（那是 v1 的特征）。
        // 注意不能拿 v2 的字段去掉这个键——那份文件真的没有版本可言，只能按 v1 读，
        // 读到 `domain_rules` 就该走下面那条"半成品"报错。
        let text = v1_text(
            r#"{"blocked_domains": [], "direct_domains": [], "proxy_domains": []}"#,
            None,
        );
        assert!(!text.contains("schema"));
        let state = parse(&text).unwrap();
        assert_eq!(state, sample());
    }

    #[test]
    fn a_v2_shaped_file_without_the_schema_key_is_read_as_v2() {
        // 手写一份 v2 状态、忘了写 schema 键：没有可合并的分档名单，就只补版本键。
        let mut value: serde_json::Value = serde_json::to_value(&sample()).unwrap();
        value.as_object_mut().unwrap().remove("schema");
        assert!(value["policy"].get("domain_rules").is_some());
        let state = parse(&value.to_string()).unwrap();
        assert_eq!(state, sample());
    }

    /// 磁盘上真存在过的 v1 形状（`.tmp/cli-state/state.json`、`.tmp/env-*.json` 就是这样）：
    /// 三档名单各占一个键，没有 `domain_rules`。`schema` 传 None 就是加门之前的老文件。
    fn v1_text(lists: &str, schema: Option<u32>) -> String {
        let mut value: serde_json::Value = serde_json::to_value(&sample()).unwrap();
        let policy = value["policy"].as_object_mut().unwrap();
        policy.remove("domain_rules");
        let extra: serde_json::Value = serde_json::from_str(lists).unwrap();
        for (key, item) in extra.as_object().unwrap() {
            policy.insert(key.clone(), item.clone());
        }
        if let Some(found) = schema {
            value
                .as_object_mut()
                .unwrap()
                .insert("schema".into(), found.into());
        }
        serde_json::to_string(&value).unwrap()
    }

    fn rules_of(state: &DesiredState) -> Vec<String> {
        state
            .policy
            .domain_rules
            .iter()
            .map(DomainRule::describe)
            .collect()
    }

    #[test]
    fn a_v1_file_migrates_into_one_ordered_list_that_keeps_who_wins() {
        // 展开顺序必须是 拦截 -> 直连 -> 代理：那是 v1 阶梯里实际的生效顺序（#18 定案
        // "拦截优先"）。顺序错了，迁上来的老配置就会今天直连、昨天拦截。
        let text = v1_text(
            r#"{
                "blocked_domains": [{"suffix": "ads.test"}],
                "direct_domains": [{"exact": "ads.test"}, {"exact": "ok.test"}],
                "proxy_domains": [{"suffix": "a.ads.test"}]
            }"#,
            Some(1),
        );
        let state = parse(&text).unwrap();
        assert_eq!(
            rules_of(&state),
            [
                "+ads.test（拦截）",
                "ads.test（直连）",
                "ok.test（直连）",
                "+a.ads.test（代理）",
            ],
            "档位顺序或处置映射被动过"
        );
    }

    #[test]
    fn a_v1_file_without_the_schema_key_takes_the_same_migration() {
        let lists = r#"{
            "blocked_domains": [{"keyword": "ad"}],
            "direct_domains": [],
            "proxy_domains": [{"regex": "^v[0-9]+\\."}]
        }"#;
        assert_eq!(
            rules_of(&parse(&v1_text(lists, Some(1))).unwrap()),
            rules_of(&parse(&v1_text(lists, None)).unwrap())
        );
        assert_eq!(
            rules_of(&parse(&v1_text(lists, None)).unwrap()),
            ["~ad（拦截）", "/^v[0-9]+\\./（代理）"]
        );
    }

    #[test]
    fn a_migrated_state_is_written_back_as_v2_and_reads_identically() {
        // 迁移只在读的时候发生，写出去的一定是 v2：面板从此只有一种形状要面对。
        let dir = temp_dir("migrate-back");
        let path = dir.join("state.json");
        let migrated = parse(&v1_text(
            r#"{"direct_domains": [{"exact": "a.test"}]}"#,
            None,
        ))
        .unwrap();
        write(&path, &migrated).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema"], STATE_SCHEMA);
        assert_eq!(value["policy"]["domain_rules"].as_array().unwrap().len(), 1);
        for legacy in ["direct_domains", "proxy_domains", "blocked_domains"] {
            assert!(
                value["policy"].get(legacy).is_none(),
                "v2 文件里不该再有 {legacy}"
            );
        }
        assert_eq!(read(&path).unwrap(), migrated);
    }

    #[test]
    fn a_v1_file_that_already_carries_domain_rules_is_refused_not_guessed() {
        // 半成品文件：删掉哪一档都是猜。报错比猜便宜。
        let mut value: serde_json::Value =
            serde_json::from_str(&v1_text(r#"{"blocked_domains": []}"#, Some(1))).unwrap();
        value["policy"]["domain_rules"] = json!([]);
        let err = parse(&value.to_string()).unwrap_err();
        assert!(err.contains("domain_rules"), "{err}");
    }

    #[test]
    fn a_legacy_list_that_is_not_an_array_is_named() {
        let err = parse(&v1_text(r#"{"blocked_domains": "ads.test"}"#, Some(1))).unwrap_err();
        assert!(err.contains("blocked_domains"), "{err}");
    }

    #[test]
    fn an_unknown_older_schema_names_the_missing_migration() {
        let err = parse(&with_schema(&sample(), 0)).unwrap_err();
        assert!(err.contains("迁移"), "{err}");
        assert!(err.contains("v0"), "{err}");
    }

    #[test]
    fn a_newer_schema_is_refused_instead_of_silently_rewritten() {
        let text = with_schema(&sample(), STATE_SCHEMA + 1);
        let err = parse(&text).unwrap_err();
        assert!(err.contains("更新版本"), "{err}");
        assert!(err.contains(&format!("v{}", STATE_SCHEMA + 1)), "{err}");
    }

    #[test]
    fn the_gate_runs_before_validation() {
        // 版本不对时不该先报一堆字段名——那些名字在新版本里可能已经变了。
        let mut state = sample();
        state.network.lan_cidr = "not-a-cidr".into();
        let err = parse(&with_schema(&state, STATE_SCHEMA + 1)).unwrap_err();
        assert!(err.contains("schema"), "{err}");
        assert!(!err.contains("CIDR"), "{err}");
    }

    #[test]
    fn an_impossible_state_is_rejected_with_the_validator_reason() {
        let mut state = sample();
        state.network.lan_cidr = "not-a-cidr".into();
        let err = parse(&to_text(&state)).unwrap_err();
        assert!(err.contains("CIDR"), "{err}");
    }

    /// 读盘这条路是 CLI 与面板 PUT **共用**的那一条，所以"一次列全"必须在这里就成立：
    /// 三处各坏一点，屏幕上要同时看见三条，而不是改一条撞一条。
    /// 单条时不加帽子（上面那条测试钉的就是单条的形状）。
    ///
    /// 后半支钉的是 §11.13 边界 1 收口后的形状：这一句要跟着**出口**的框一起读
    /// （面板那句是"保存被拒（HTTP 400）："，CLI 那句前面只有文件路径），所以共享串自己
    /// 不许再顶一层"期望状态不成立:"那种标签——两层标签叠在一句里，屏幕上还得数两遍冒号。
    #[test]
    fn the_read_path_lists_every_problem_not_just_the_first() {
        use crate::model::device::{Device, DeviceMode};

        let mut state = sample();
        state.network.lan_cidr = "not-a-cidr".into();
        state.dns.records_retention_days = 0;
        state.devices = vec![Device {
            ip: "tv.lan".into(),
            mac: None,
            hostname: None,
            mode: DeviceMode::Global,
        }];
        let err = parse(&to_text(&state)).unwrap_err();
        assert!(err.starts_with("这 3 处不成立，要改："), "{err}");
        assert!(!err.contains("期望状态不成立"), "{err}");
        for expect in ["1）", "2）", "3）", "CIDR", "保留天数", "既不是 IPv4"] {
            assert!(err.contains(expect), "清单里少了 {expect}：{err}");
        }
    }

    #[test]
    fn a_round_trip_through_disk_keeps_the_state_identical() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested").join("state.json");
        write(&path, &sample()).unwrap();
        assert_eq!(read(&path).unwrap(), sample());
    }

    #[test]
    fn a_second_write_replaces_the_first_and_leaves_no_temp_file() {
        let dir = temp_dir("replace");
        let path = dir.join("state.json");
        write(&path, &sample()).unwrap();
        let mut changed = sample();
        changed.network.lan_cidr = "10.30.0.0/24".into();
        write(&path, &changed).unwrap();
        assert_eq!(read(&path).unwrap(), changed);
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["state.json".to_string()], "{left:?}");
    }

    /// 面板的保存流程：从"这台机还没写过状态"开始，落到 `Layout::state_path()` 那一份。
    ///
    /// 起点不是假设：2026-10-05 在 guest 上实测过，`/var/lib/zbox/state.json` 并不存在
    /// （`zboxd` 只读不写它），所以第一份状态必然由面板写下。这条测试钉的是面板与 CLI
    /// 必须认同一个文件——否则面板保存完，`zboxd apply` 不带位置参数时读到的还是老那份。
    #[test]
    fn a_panel_save_lands_on_the_canonical_path_and_keeps_the_row_order() {
        use crate::gen::init::Layout;

        let dir = temp_dir("panel");
        // root 目录刻意还没建：面板可以在 `provision` 之前就打开并保存
        let layout = Layout {
            root: dir.join("var-lib-zbox").display().to_string(),
            ..Layout::default()
        };
        let path = PathBuf::from(layout.state_path());

        // 还没有状态时读它：报错里要带落点，面板才分得清"这台没存过"和"文件坏了"
        let err = read(&path).unwrap_err();
        assert!(
            err.contains(&path.display().to_string()),
            "报错要说清读的是哪一份：{err}"
        );
        assert!(err.contains("state.json"), "{err}");

        let mut state = sample();
        state.policy.domain_rules = vec![
            DomainRule::new(
                DomainPattern::parse("www.taobao.com").unwrap(),
                DomainAction::Block,
            ),
            DomainRule::new(
                DomainPattern::parse("+www.qq.com").unwrap(),
                DomainAction::Direct,
            ),
            DomainRule::new(
                DomainPattern::parse("+www.baidu.com").unwrap(),
                DomainAction::Proxy,
            ),
        ];
        write(&path, &state).expect("面板第一次保存");
        #[cfg(unix)]
        {
            // 面板写的这一份装着订阅 URL 里的个人令牌，落点上的权限就是它的保密边界
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{path:?}");
        }
        assert_eq!(read(&path).unwrap(), state, "读回来必须是 CLI 要用的那一份");
        let disk: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(disk["schema"], STATE_SCHEMA, "面板写的这份也得带版本键");

        // 拖动排序：集合没变、顺序变了，而顺序就是优先级 ⇒ 磁盘和读回来都必须跟着变，
        // 任何"顺手排个序"的写法都会在这里红掉
        state.policy.domain_rules.rotate_left(1);
        write(&path, &state).expect("面板保存排序结果");
        assert_eq!(
            rows_on_disk(&path),
            [
                "+www.qq.com（直连）",
                "+www.baidu.com（代理）",
                "www.taobao.com（拦截）"
            ],
            "磁盘上的行序必须就是面板拖出来的那个顺序"
        );
        assert_eq!(
            read(&path).unwrap().policy.domain_rules,
            state.policy.domain_rules
        );

        // 每次保存都换一份临时名，所以目录里不该攒下 `state.json.tmp-*`
        let left: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["state.json".to_string()], "{left:?}");
    }

    /// 磁盘上 `policy.domain_rules` 的**行序**，用 `describe()` 印成人看得懂的样子。
    /// 走 JSON 而不是 `read()`，是为了让"顺序"这件事由磁盘自己说话。
    fn rows_on_disk(path: &Path) -> Vec<String> {
        let text = fs::read_to_string(path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let array = value["policy"]["domain_rules"]
            .as_array()
            .unwrap_or_else(|| panic!("磁盘上没有 domain_rules 数组：{text}"));
        array
            .iter()
            .map(|raw| {
                let rule: DomainRule = serde_json::from_value(raw.clone()).unwrap();
                rule.describe()
            })
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn the_written_file_is_not_readable_by_anyone_but_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let path = dir.join("state.json");
        write(&path, &sample()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "订阅令牌不该落进组或其他人可读的文件");
    }
}
