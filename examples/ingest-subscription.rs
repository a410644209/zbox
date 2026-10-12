//! 用真实订阅负载走一遍"解析 -> 期望状态 -> 快照"，产物交给 VM 上的 sing-box check 复核。
//!
//! 用法：`cargo run --example ingest-subscription -- <body-file> <out-dir>`
//! `<body-file>` 是订阅接口返回的原始响应体（base64 链接列表或明文链接列表都行）。

use std::{env, fs, process};
use zboxd::gen::snapshot_files;
use zboxd::ingest::subscription;
use zboxd::model::config::{builtin_rule_sets, DesiredState, FinalOutbound, Group, GroupKind};

fn main() {
    let mut args = env::args().skip(1);
    let (Some(body_path), Some(out_dir)) = (args.next(), args.next()) else {
        eprintln!("usage: ingest-subscription <body-file> <out-dir>");
        process::exit(2);
    };
    let body = fs::read(&body_path).unwrap_or_else(|e| {
        eprintln!("read {body_path}: {e}");
        process::exit(1);
    });
    let format = subscription::detect(&body);
    println!("detected format: {format:?}");
    let parsed = match subscription::parse(&body) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("parse failed: {e}");
            process::exit(1);
        }
    };
    let nodes = parsed.nodes;
    println!("parsed {} nodes", nodes.len());
    if parsed.skipped > 0 {
        println!("skipped {} unreadable links", parsed.skipped);
    }
    for node in &nodes {
        let (kind, tls) = match &node.kind {
            zboxd::model::node::NodeKind::Stream {
                protocol,
                transport,
                tls,
            } => (
                format!("{protocol:?} {transport:?}"),
                !matches!(tls, zboxd::model::node::Tls::Disabled),
            ),
            zboxd::model::node::NodeKind::Quic { protocol } => (format!("{protocol:?}"), true),
        };
        println!("  {:32} {:>15} {} tls={tls}", node.name, node.port, kind);
    }

    let members: Vec<String> = nodes.iter().map(|n| n.name.clone()).collect();
    let mut state = DesiredState::baseline();
    // 按实测 VM 的拓扑填，生成的 zbox.nft / netstate.sh 才能一起拿去 nft -c
    state.network.lan_interface = "eth0".into();
    state.network.wan_interface = "eth0".into();
    state.network.lan_cidr = "192.168.219.0/24".into();
    state.nodes = nodes;
    state.groups = vec![
        Group {
            tag: "auto".into(),
            kind: GroupKind::Urltest {
                url: "http://www.gstatic.com/generate_204".into(),
                interval: "3m".into(),
                tolerance: 50,
            },
            members: members.clone(),
            default: None,
        },
        Group {
            tag: "proxy".into(),
            kind: GroupKind::Selector,
            members: vec!["auto".into()],
            default: Some("auto".into()),
        },
    ];
    state.policy.final_outbound = FinalOutbound::Proxy("proxy".into());
    state.rule_sets = builtin_rule_sets();

    if let Err(e) = state.validate() {
        eprintln!("validate failed: {e}");
        process::exit(1);
    }
    let files = match snapshot_files(&state) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("generate failed: {e}");
            process::exit(1);
        }
    };
    fs::create_dir_all(&out_dir).expect("create out dir");
    for (path, content) in &files {
        fs::write(std::path::Path::new(&out_dir).join(path), content).expect("write");
    }
    // 同上：写出的 state.json 与 zboxd 读的是同一种形状
    zboxd::model::state::write(&std::path::Path::new(&out_dir).join("state.json"), &state)
        .expect("write state");
    println!("wrote {} files to {out_dir}", files.len());
}
