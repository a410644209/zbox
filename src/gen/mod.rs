//! 生成层：期望状态 -> 一份待下发的快照文件集合。
//!
//! 快照里同时包含 sing-box 配置和劫持层脚本，因为它们**必须原子地一起生效**：
//! 只换 config.json 而不刷新 nft/ip rule，会出现"规则变了但流量根本没进来"；
//! 反过来则会劫持到一个还没校验过的内核上。

pub mod dae;
pub mod init;
pub mod ladder;
pub mod netstate;
pub mod nft;
pub mod oxidns;
pub mod singbox;

use crate::gen::singbox::GenerateError;
use crate::model::config::DesiredState;
use std::collections::BTreeMap;

/// 一次下发的全部文件，key 为快照目录内的相对路径。
pub fn snapshot_files(state: &DesiredState) -> Result<BTreeMap<String, String>, GenerateError> {
    let mut files = singbox::generate(state)?;
    files.insert(nft::NFT_PATH.to_string(), nft::render(state));
    files.insert(
        netstate::APPLY_PATH.to_string(),
        netstate::render_apply(state),
    );
    files.insert(
        netstate::TEARDOWN_PATH.to_string(),
        netstate::render_teardown(),
    );
    // dae/oxidns 也存进快照：逐行对照"已生效"要从 last_good 修订里读这两份文本。
    // 渲染失败时不阻塞整份快照（sing-box 退役中，dae/oxidns 可能还不可用），
    // 缺的那一份在 ladder 侧按"无从判断"处理。
    if let Ok(text) = dae::render(state) {
        files.insert(dae::DAE_PATH.to_string(), text);
    }
    if let Ok(text) = oxidns::render(state) {
        files.insert(oxidns::OXIDNS_PATH.to_string(), text);
    }
    Ok(files)
}

/// `render` 那一步认的目标名。面板的预览**只给 dae/oxidns 两个名字**（sing-box 那条路径到 P4
/// 退役，不给它多一个入口），CLI 五个都给：`config.json` 要拿去过 `sing-box run` 那道门，
/// `zbox.nft` / `netstate.sh` 要拿去过 `nft -c -f` 与 `sh -n` 那道门——这两份的"上游"就是内核，
/// 而按纪律那道门不许碰在跑的数据面。
pub const RENDER_TARGETS: [&str; 5] = ["config.json", "dae", "oxidns", "zbox.nft", "netstate.sh"];

/// 按目标名取一份生成物文本：不写盘、不校验、不重启。
pub fn render_target(state: &DesiredState, target: &str) -> Result<String, String> {
    match target {
        "config.json" => {
            let files = snapshot_files(state).map_err(|e| e.to_string())?;
            files
                .get("config.json")
                .cloned()
                .ok_or_else(|| "生成物里没有 config.json".to_string())
        }
        "dae" => dae::render(state),
        "oxidns" => oxidns::render(state),
        "zbox.nft" => {
            // 与快照里那一份必须是同一函数：两条路径各渲一遍，验过的就不是要下发的那份
            let files = snapshot_files(state).map_err(|e| e.to_string())?;
            files
                .get(nft::NFT_PATH)
                .cloned()
                .ok_or_else(|| "生成物里没有 zbox.nft".to_string())
        }
        "netstate.sh" => {
            let files = snapshot_files(state).map_err(|e| e.to_string())?;
            files
                .get(netstate::APPLY_PATH)
                .cloned()
                .ok_or_else(|| "生成物里没有 netstate.sh".to_string())
        }
        other => Err(format!(
            "--target 只认 {}，收到 {other}",
            RENDER_TARGETS.join("|")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_carries_the_config_and_the_hijack_layer_together() {
        let files = snapshot_files(&DesiredState::baseline()).unwrap();
        let keys: Vec<&str> = files.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "config.json",
                "dae.conf",
                "netstate-down.sh",
                "netstate.sh",
                "zbox.nft"
            ],
            "快照内容变了就要同步改 apply/svc 层的假设"
        );
        for (path, body) in &files {
            assert!(!path.starts_with('/') && !path.contains(".."), "{path}");
            assert!(body.ends_with('\n'), "{path}");
        }
    }

    #[test]
    fn a_state_the_hijack_layer_cannot_render_is_refused() {
        // 裸地址会被原样写进 nft 规则，下发后 nft -f 直接报错、劫持层整个失效
        let mut state = DesiredState::baseline();
        state.network.lan_cidr = "192.168.1.0".into();
        assert!(snapshot_files(&state).is_err());

        let mut state = DesiredState::baseline();
        state.network.lan_interface = String::new();
        assert!(snapshot_files(&state).is_err());
    }

    /// 基线状态缺 `lan_address`，oxidns 渲染不出来，快照里就没有 `oxidns.yaml`；
    /// 补上之后两份都在。dae/oxidns 渲染失败不阻塞整份快照（`snapshot_files` 用 `if let Ok`），
    /// 但逐行对照那一侧会因为缺文本而报"无从判断"。
    #[test]
    fn a_fully_renderable_state_carries_both_dae_and_oxidns_in_the_snapshot() {
        let state = a_renderable_baseline();
        let files = snapshot_files(&state).unwrap();
        let keys: Vec<&str> = files.keys().map(String::as_str).collect();
        assert!(keys.contains(&"dae.conf"), "dae.conf 不在快照里: {keys:?}");
        assert!(
            keys.contains(&"oxidns.yaml"),
            "oxidns.yaml 不在快照里: {keys:?}"
        );
    }

    /// 这一条钉的是 CLI 那一步的名字表：`render --target` 拼错时给的是**能粘回去的名单**，
    /// 而不是一个空 stdout。空 stdout 在验收现场最容易被读成"这份生成物就是空的"。
    #[test]
    fn an_unknown_target_names_the_list_it_accepts_and_prints_nothing() {
        let err = render_target(&DesiredState::baseline(), "dae-conf").unwrap_err();
        for name in RENDER_TARGETS {
            assert!(err.contains(name), "名单里少了 {name}：{err}");
        }
        assert!(err.contains("dae-conf"), "{err}");
    }

    fn a_renderable_baseline() -> DesiredState {
        let mut state = DesiredState::baseline();
        // 解析口要显式绑定地址（`oxidns::dns_binds`），所以"能整份渲染"的状态得带着
        // 本机局域网 IPv4。用 RFC 保留段，不指向任何真实机器。
        state.network.lan_address = "192.0.2.10".into();
        state
    }

    /// 五个目标名各取到一份**非空且以换行收尾**的文本，且 `config.json` / `zbox.nft` /
    /// `netstate.sh` 三条与快照里的同名字节逐字相同——`render` 的既有行为（拿它去过
    /// `sing-box run`）不许被这一步改动，而新增那两条的理由是同一个：在 guest 上
    /// `nft -c -f` / `sh -n` 验过的那一份，必须就是下一次下发的那一份。
    #[test]
    fn each_target_renders_a_whole_text_and_the_default_is_the_snapshot() {
        let state = a_renderable_baseline();
        let files = snapshot_files(&state).unwrap();
        for name in ["config.json", "zbox.nft", "netstate.sh"] {
            let body = render_target(&state, name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                body,
                files.get(name).unwrap().clone(),
                "{name}：render 出来的与快照里的不是同一份字节"
            );
        }
        for name in ["dae", "oxidns", "zbox.nft", "netstate.sh"] {
            let body = render_target(&state, name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!body.trim().is_empty(), "{name} 打出的是空文本");
            assert!(
                body.ends_with('\n'),
                "{name} 末行没有换行，重定向进文件会是半截"
            );
        }
    }

    /// 渲染不过的那一份状态，`render_target` 带出来的是**一句原因**而不是空文本——
    /// 拿它重定向进文件去 validate 的人，若只看到一个空文件，下一步就会去怀疑上游。
    /// （各渲染器自己的报错形状在 `dae.rs` / `oxidns.rs` 里钉，这一条只钉这一层的转发生。）
    #[test]
    fn a_state_a_renderer_refuses_arrives_with_a_reason_not_an_empty_text() {
        use crate::model::config::{Group, GroupKind};
        let mut state = a_renderable_baseline();
        state.groups.push(Group {
            tag: "empty".into(),
            kind: GroupKind::Selector,
            members: vec![],
            default: None,
        });
        let err = render_target(&state, "dae").unwrap_err();
        assert!(err.contains("empty"), "{err}");
        assert!(!err.trim().is_empty(), "报错却是空文案");
        // 同一个坏状态不拦另一条：oxidns 不消费策略组，它照旧出整份文本
        assert!(render_target(&state, "oxidns").unwrap().ends_with('\n'));
    }
}
