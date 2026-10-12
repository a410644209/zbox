//! 期望状态 -> 一整份可 `sing-box check` 的配置文档。
//!
//! sing-box 1.14.2 不接受 `conf-directory`/`include` 之类的多文件机制（顶层
//! `DisallowUnknownFields`），所以 ZBox 必须产出**单个 JSON 文档**；规则集是外部
//! `.srs` 文件。VM 实测：`initial_path` 只会被 sing-box **读**，下载成功的内容落在
//! `cache_file` 里，所以下载失败又没预置文件就是启动期 FATAL——离线文件必须由
//! ZBox 自己放到 `initial_path` 上。

pub mod dns;
pub mod inbound;
pub mod outbound;
pub mod route;

use crate::gen::singbox::outbound::OutboundError;
use crate::gen::singbox::route::RouteError;
use crate::model::config::{
    describe_problems, DesiredState, Group, GroupKind, DIRECT_TAG, REJECT_TAG,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub const CONFIG_PATH: &str = "config.json";

/// 内核自己写的日志文件名。**只能是裸文件名、不许带目录**，两条实测一起逼出来的
/// （schema §11.8.3）：
/// - 相对路径以 `-D` 为基准，不是进程 cwd（cwd 留空、文件落在 `-D` 下）；
/// - sing-box 1.14.2 **不会**创建父目录，写了 `logs/sing-box.log` 就
///   `FATAL[0000] start service: start logger: open logs/sing-box.log: no such file or directory`，
///   rc=1 且什么都没留下。
///
/// `-D` 是 ZBox 自己建的状态目录，一定存在，所以裸文件名是唯一稳的形状。
/// 这条字段的意义在 OpenRC：这台 Alpine 的 openrc-run 没有任何 stdout/stderr 重定向变量
/// （实测遍查 `/sbin/openrc-run`、`/etc/init.d/functions.sh`、`/lib/rcscripts`、
/// `/usr/share/openrc`、`/usr/lib/openrc`，已装的脚本里也无人用），不落到文件就看不见启动失败。
pub const LOG_FILE: &str = "sing-box.log";

#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    /// 不再顶"期望状态不成立:"这层标签：这里的串是 [`describe_problems`] 给的清单本身，
    /// 而每条出口自己都已经有一句框（CLI 是 `zboxd: `，面板是"但生成器对这一份报错："）。
    /// 与 `state::parse` 同一条规矩（§11.13 边界 1：标签归出口，共享串只给清单）。
    #[error("{0}")]
    InvalidState(String),
    #[error(transparent)]
    Outbound(#[from] OutboundError),
    #[error(transparent)]
    Route(#[from] RouteError),
}

/// 生成待下发文件集合，key 为快照内的相对路径。
pub fn generate(state: &DesiredState) -> Result<BTreeMap<String, String>, GenerateError> {
    let config = generate_json(state)?;
    let text = serde_json::to_string_pretty(&config)
        .map_err(|e| GenerateError::InvalidState(format!("序列化失败: {e}")))?;
    let mut files = BTreeMap::new();
    files.insert(CONFIG_PATH.to_string(), text + "\n");
    Ok(files)
}

pub fn generate_json(state: &DesiredState) -> Result<Value, GenerateError> {
    state
        .validate()
        .map_err(|problems| GenerateError::InvalidState(describe_problems(&problems)))?;

    let mut outbounds: Vec<Value> = vec![json!({"type": "direct", "tag": DIRECT_TAG})];
    for node in &state.nodes {
        outbounds.push(outbound::node_outbound(node)?);
    }
    for group in &state.groups {
        outbounds.push(group_json(group));
    }
    // reject 放最后：它只是策略组的候选成员，不参与前面的顺序约定
    outbounds.push(json!({"type": "block", "tag": REJECT_TAG}));

    let mut config = Map::new();
    config.insert(
        "log".into(),
        json!({"level": "info", "timestamp": true, "output": LOG_FILE}),
    );
    config.insert("dns".into(), dns::dns_json(state));
    if let Some(clients) = route::http_clients_json(state) {
        config.insert("http_clients".into(), clients);
    }
    config.insert(
        "inbounds".into(),
        Value::Array(inbound::inbounds_json(state)),
    );
    config.insert("outbounds".into(), Value::Array(outbounds));
    config.insert("route".into(), route::route_json(state)?);
    config.insert(
        "experimental".into(),
        json!({
            "cache_file": {
                "enabled": true,
                "path": "cache.db",
                "cache_id": "zbox",
                "store_fakeip": state.dns.fakeip.enabled,
            }
        }),
    );
    Ok(Value::Object(config))
}

fn group_json(group: &Group) -> Value {
    let mut m = Map::new();
    match &group.kind {
        GroupKind::Selector => {
            m.insert("type".into(), json!("selector"));
            // `default` 只有 selector 有。1.14.2 的 `sing-box schema` 里 urltest 的
            // properties 不含它，多发一个键会让 `sing-box check` 整个失败。
            if let Some(default) = &group.default {
                m.insert("default".into(), json!(default));
            }
        }
        GroupKind::Urltest {
            url,
            interval,
            tolerance,
        } => {
            m.insert("type".into(), json!("urltest"));
            m.insert("url".into(), json!(url));
            m.insert("interval".into(), json!(interval));
            m.insert("tolerance".into(), json!(tolerance));
        }
    }
    m.insert("tag".into(), json!(&group.tag));
    m.insert("outbounds".into(), json!(&group.members));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::uri::parse_link;
    use crate::model::config::{FinalOutbound, Policy};

    fn state() -> DesiredState {
        DesiredState {
            nodes: vec![
                parse_link("vless://11111111-2222-3333-4444-555555555555@node.example.com:443?security=reality&sni=www.microsoft.com&pbk=PUB&sid=ab&flow=xtls-rprx-vision#东京").unwrap(),
                parse_link("hysteria2://pw@hy.example.com:443?sni=hy.example.com#新加坡").unwrap(),
            ],
            groups: vec![
                Group {
                    tag: "auto".into(),
                    kind: GroupKind::Urltest {
                        url: "http://www.gstatic.com/generate_204".into(),
                        interval: "3m".into(),
                        tolerance: 50,
                    },
                    members: vec!["东京".into(), "新加坡".into()],
                    default: None,
                },
                Group {
                    tag: "proxy".into(),
                    kind: GroupKind::Selector,
                    members: vec!["auto".into(), "东京".into(), DIRECT_TAG.into()],
                    default: Some("auto".into()),
                },
            ],
            policy: Policy {
                final_outbound: FinalOutbound::Proxy("proxy".into()),
                ..Policy::default()
            },
            rule_sets: crate::model::config::builtin_rule_sets(),
            ..DesiredState::default()
        }
    }

    #[test]
    fn document_has_only_the_supported_top_level_keys() {
        let got = generate_json(&state()).unwrap();
        let obj = got.as_object().unwrap();
        let allowed = [
            "log",
            "dns",
            "http_clients",
            "inbounds",
            "outbounds",
            "route",
            "experimental",
        ];
        for key in obj.keys() {
            assert!(allowed.contains(&key.as_str()), "多余的顶层键 {key}");
        }
    }

    /// 内核必须自己把日志落到 `-D` 下面。这台 Alpine 的 openrc-run 没有任何 stdout/stderr
    /// 重定向变量（遍查 `/sbin/openrc-run`、`/etc/init.d/functions.sh`、`/lib/rcscripts`、
    /// `/usr/share/openrc`、`/usr/lib/openrc`，已装脚本里也无人用），不写进文件启动失败就看不见；
    /// systemd 那边也不该逼读者去记 unit 名。判据原文见 schema §11.8.3。
    #[test]
    fn the_kernel_writes_its_own_log_into_the_state_dir() {
        let log = &generate_json(&state()).unwrap()["log"];
        assert_eq!(log["output"].as_str(), Some(LOG_FILE));
        // 裸文件名：带目录就是让内核去开一个它不会自建的目录，实测直接 FATAL
        assert!(!LOG_FILE.contains('/'), "{LOG_FILE}");
        assert_eq!(log["level"].as_str(), Some("info"));
        assert_eq!(log["timestamp"].as_bool(), Some(true));
    }

    #[test]
    fn special_outbounds_are_always_present() {
        let got = generate_json(&state()).unwrap();
        let text = got["outbounds"].to_string();
        assert!(text.contains(&format!("\"tag\":\"{DIRECT_TAG}\"")));
        assert!(text.contains(&format!("\"tag\":\"{REJECT_TAG}\"")));
        // 节点出站排在策略组之前，策略组可以引用它们
        let tags: Vec<&str> = got["outbounds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["tag"].as_str().unwrap())
            .collect();
        assert_eq!(
            tags,
            vec!["direct", "东京", "新加坡", "auto", "proxy", "reject"]
        );
    }

    #[test]
    fn every_inbound_a_rule_references_is_emitted() {
        // VM 实测：sing-box 从不校验 `route.rules[].inbound` 指向的入站存不存在
        // ——把 inbounds 整个删空，check 和 run 都照过。写错 tag 就是一条永远不命中
        // 且毫无征兆的规则，内核不会替我们查，只能在生成侧断言。
        let got = generate_json(&state()).unwrap();
        let inbound_tags: Vec<&str> = got["inbounds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["tag"].as_str().unwrap())
            .collect();
        for rule in got["route"]["rules"].as_array().unwrap() {
            let Some(refs) = rule.get("inbound").and_then(|v| v.as_array()) else {
                continue;
            };
            for reference in refs {
                assert!(
                    inbound_tags.contains(&reference.as_str().unwrap()),
                    "规则 {rule} 引用了不存在的入站 {reference}（sing-box 不会报错，只会静默不命中）"
                );
            }
        }
    }

    #[test]
    fn only_selector_carries_a_default_member() {
        // 依据是 VM 上的实测：1.14.2 的 `sing-box schema` 里 urltest 的 properties 没有
        // `default`，带上它 `sing-box check` 直接报 outbounds[i].default: unknown field。
        let got = generate_json(&state()).unwrap();
        let find = |kind: &str| {
            got["outbounds"]
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["type"] == kind)
                .cloned()
                .unwrap()
        };
        assert!(
            find("urltest").get("default").is_none(),
            "urltest 带了 default，check 会失败"
        );
        assert_eq!(find("selector")["default"], json!("auto"));
    }

    #[test]
    fn no_removed_or_renamed_keys_anywhere() {
        let text = serde_json::to_string(&generate_json(&state()).unwrap()).unwrap();
        for banned in [
            "\"so_mark\"",
            "\"download_detour\"",
            "\"independent_cache\"",
            "\"conf-directory\"",
            "\"geosite\":",
            "\"geoip\":",
            "\"alterId\"",
            "\"handshake\"",
            "\"qtype\"",
            "\"default_outbound\"",
            "\"intervals\"",
        ] {
            assert!(!text.contains(banned), "{banned} 出现在生成的配置里");
        }
    }

    #[test]
    fn generated_files_use_snapshot_relative_paths() {
        let files = generate(&state()).unwrap();
        assert_eq!(files.len(), 1);
        let (path, body) = files.iter().next().unwrap();
        assert_eq!(path, CONFIG_PATH);
        assert!(body.ends_with("}\n"));
        // 路径必须能被 apply::Store 接受（相对、无穿越）
        serde_json::from_str::<Value>(body).unwrap();
    }

    #[test]
    fn invalid_state_is_refused_before_generation() {
        let mut s = state();
        s.groups[0].members = vec!["不存在的节点".into()];
        match generate_json(&s) {
            Err(GenerateError::InvalidState(msg)) => assert!(msg.contains("不存在"), "{msg}"),
            other => panic!("{other:?}"),
        }

        // 生成器这道门也走同一份排版：坏三处就列三条，`zboxd apply` 的报错不必跑三遍
        // 才把人带到最后一处。
        let mut s = state();
        s.groups[0].members = vec!["不存在的节点".into()];
        s.network.lan_cidr = "not-a-cidr".into();
        s.dns.records_retention_days = 0;
        match generate_json(&s) {
            Err(GenerateError::InvalidState(msg)) => {
                assert!(msg.starts_with("这 3 处不成立，要改："), "{msg}");
                for expect in ["不存在", "CIDR", "保留天数"] {
                    assert!(msg.contains(expect), "清单里少了 {expect}：{msg}");
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn baseline_document_is_all_direct_and_has_no_proxy_group() {
        let got = generate_json(&DesiredState::baseline()).unwrap();
        assert_eq!(got["route"]["final"], DIRECT_TAG);
        assert_eq!(got["outbounds"].as_array().unwrap().len(), 2);
        assert_eq!(got["experimental"]["cache_file"]["store_fakeip"], false);
    }
}
