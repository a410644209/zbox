//! 把期望状态渲染成快照文件，交给 VM 上的真实 sing-box / nft 复核。
//!
//! 用法：`cargo run --example dump-snapshot -- <out-dir> [baseline|full|<state.json>]`
//! 同时写出 `state.json`，这样 VM 上测出来的问题能对着同一份输入复现。
//! 第二个参数给的是**已存在的文件**时，走 `model::state::read` 读它——于是这份产物
//! 是从磁盘上那份文件（可能是 v1）经迁移得到的，跟真机上 `zboxd apply` 看到的是同一条入口。
//! 这个例子只写文件、不碰任何服务：内核的 `check` / 预跑由脚本在 guest 上单独发起。

use std::{env, fs, path::Path, process};
use zboxd::gen::snapshot_files;
use zboxd::model::config::{
    builtin_rule_sets, DesiredState, DomainAction, DomainPattern, DomainRule, FinalOutbound, Group,
    GroupKind, RuleSet, Upstream, UpstreamTransport, Via,
};
use zboxd::model::device::{Device, DeviceMode, IpType};
use zboxd::model::node::{
    Node, NodeKind, QuicProtocol, Reality, StreamProtocol, Tls, TlsConfig, Transport,
};

fn main() {
    let mut args = env::args().skip(1);
    let Some(out_dir) = args.next() else {
        eprintln!("usage: dump-snapshot <out-dir> [baseline|full|<state.json>]");
        process::exit(2);
    };
    let given = args.next();
    let state = match given.as_deref() {
        Some(p) if Path::new(p).is_file() => match zboxd::model::state::read(Path::new(p)) {
            Ok(s) => {
                println!("read state from {p}");
                s
            }
            Err(e) => {
                eprintln!("read state failed: {e}");
                process::exit(1);
            }
        },
        _ => match given.as_deref() {
            Some("baseline") => DesiredState::baseline(),
            Some("netns") => netns_state(false),
            Some("netns-fakeip") => netns_state(true),
            Some("netns-local") => netns_local_state(false),
            Some("netns-local-fakeip") => netns_local_state(true),
            Some("full") | None => full_state(),
            Some(other) => {
                eprintln!("unknown profile {other}");
                process::exit(2);
            }
        },
    };
    if let Err(e) = state.validate() {
        eprintln!("state invalid: {e:?}");
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
    for (path, body) in &files {
        fs::write(std::path::Path::new(&out_dir).join(path), body).expect("write snapshot file");
    }
    // 走 model::state::write：彩排目录里的 state.json 和真机上的是同一种形状（带 schema 键）
    zboxd::model::state::write(&std::path::Path::new(&out_dir).join("state.json"), &state)
        .expect("write state");
    println!("wrote {} files to {out_dir}", files.len());
}

/// 覆盖生成器能走到的每条分支：TCP 系传输（含省略 transport 的裸 TCP、ws）、Reality、
/// QUIC 两族、fakeip、远程规则集、按设备分流。
fn full_state() -> DesiredState {
    let mut state = DesiredState::baseline();
    state.network.lan_interface = "eth0".into();
    state.network.wan_interface = "eth0".into();
    state.network.lan_cidr = "192.168.219.0/24".into();

    state.nodes = vec![
        Node {
            name: "vless-reality".into(),
            server: "198.51.100.10".into(),
            port: 443,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Vless {
                    uuid: "9a5cb1d0-1f2e-4c3d-9a8b-7c6d5e4f3a2b".into(),
                    flow: Some("xtls-rprx-vision".into()),
                },
                transport: Transport::Tcp,
                tls: Tls::Tls(TlsConfig {
                    sni: Some("www.microsoft.com".into()),
                    fingerprint: Some("chrome".into()),
                    reality: Some(Reality {
                        // 真由 guest 里 `sing-box generate reality-keypair` 生成的，
                        // 编出来的假公钥会在 `check` 的值校验阶段被拒（invalid public_key）
                        public_key: "e6fWR7Cvs4AUijRLNmKxdo7zc3w7zcca0QdUv8YDp1w".into(),
                        short_id: "0123456789abcdef".into(),
                    }),
                    ..Default::default()
                }),
            },
            delay_ms: None,
        },
        Node {
            name: "vless-ws".into(),
            server: "ws.example.com".into(),
            port: 443,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Vless {
                    uuid: "1b2d3e4f-5a6b-7c8d-9e0f-1a2b3c4d5e6f".into(),
                    flow: None,
                },
                transport: Transport::Ws {
                    path: "/ray".into(),
                    host: Some("cdn.example.com".into()),
                    headers: vec![("User-Agent".into(), "zbox-test".into())],
                },
                tls: Tls::Tls(TlsConfig {
                    sni: Some("cdn.example.com".into()),
                    alpn: vec!["h2".into(), "http/1.1".into()],
                    insecure: true,
                    ..Default::default()
                }),
            },
            delay_ms: None,
        },
        Node {
            name: "hy2".into(),
            server: "198.51.100.20".into(),
            port: 8443,
            kind: NodeKind::Quic {
                protocol: QuicProtocol::Hysteria2 {
                    password: "test-password".into(),
                    obfs: Some("salamander".into()),
                    obfs_password: Some("obfs-pass".into()),
                    disable_path_mtu_discovery: true,
                    up_mbps: Some(50),
                    down_mbps: Some(500),
                    tls: Tls::Tls(TlsConfig {
                        sni: Some("hy.example.com".into()),
                        insecure: true,
                        ..Default::default()
                    }),
                },
            },
            delay_ms: None,
        },
        Node {
            name: "tuic".into(),
            server: "198.51.100.30".into(),
            port: 10443,
            kind: NodeKind::Quic {
                protocol: QuicProtocol::Tuic {
                    uuid: "2c3d4e5f-6a7b-8c9d-0e1f-2a3b4c5d6e7f".into(),
                    password: "tuic-pass".into(),
                    alpn: vec!["h3".into()],
                    congestion_control: Some("bbr".into()),
                    udp_relay_mode: Some("quic".into()),
                    udp_over_stream: false,
                    zero_rtt_handshake: true,
                    tls: Tls::Tls(TlsConfig {
                        sni: Some("tuic.example.com".into()),
                        insecure: true,
                        ..Default::default()
                    }),
                },
            },
            delay_ms: None,
        },
    ];

    state.groups = vec![
        Group {
            tag: "auto".into(),
            kind: GroupKind::Urltest {
                url: "http://www.gstatic.com/generate_204".into(),
                interval: "3m".into(),
                tolerance: 50,
            },
            members: vec!["vless-reality".into(), "hy2".into(), "tuic".into()],
            // urltest 没有 default：1.14.2 的 `sing-box schema` 里就没有这个键
            default: None,
        },
        Group {
            tag: "manual".into(),
            kind: GroupKind::Selector,
            members: vec!["vless-reality".into(), "vless-ws".into(), "direct".into()],
            default: Some("direct".into()),
        },
    ];

    state.dns.fakeip.enabled = true;
    state.dns.overseas = vec![Upstream {
        tag: "overseas".into(),
        host: "1.1.1.1".into(),
        port: None,
        transport: UpstreamTransport::DoH,
        path: Some("/dns-query".into()),
        sni: None,
        via: Via::Outbound("auto".into()),
    }];

    state.policy.cn_direct = true;
    state.policy.block_ads = true;
    state.policy.bittorrent_direct = true;
    state.policy.final_outbound = FinalOutbound::Proxy("auto".into());
    state.policy.domain_rules = vec![
        DomainRule::new(
            DomainPattern::Keyword("example.blocked".into()),
            DomainAction::Block,
        ),
        // 盖住下面那行直连的一条（`example.direct` 命中这个后缀）：选项 C 之后"谁赢"就看行序，
        // 生成的 config.json 里 reject 必须排在送直连那条之前，doctor 也得把这行点名。
        DomainRule::new(DomainPattern::Suffix("direct".into()), DomainAction::Block),
        DomainRule::new(
            DomainPattern::Suffix("example.proxy".into()),
            DomainAction::Proxy,
        ),
        DomainRule::new(
            DomainPattern::Exact("example.direct".into()),
            DomainAction::Direct,
        ),
    ];

    state.devices = vec![
        Device {
            ip: "192.168.219.50".into(),
            mac: Some("aa:bb:cc:dd:ee:01".into()),
            hostname: Some("printer".into()),
            mode: DeviceMode::Bypass,
            ip_type: IpType::Dhcp,
            display_name: None,
        },
        Device {
            ip: "192.168.219.60".into(),
            mac: None,
            hostname: Some("tv".into()),
            mode: DeviceMode::Direct,
            ip_type: IpType::Dhcp,
            display_name: None,
        },
    ];

    state.rule_sets = builtin_rule_sets();

    state
}

/// netns 实测拓扑专用（结论回写在 `docs/reference/singbox-1.14.2-schema.md` §11.7）：
/// 局域网侧不再是真实网卡，而是宿主命名空间里的一头 veth `zbox-lan0`（10.99.0.1/24），
/// 客户端在另一个 netns 里。这样 §8 的 nft/ip rule/TPROXY/DNS 劫持都能在 VM 内部闭环验证，
/// 完全不碰物理 LAN。
///
/// 所有节点地址都是 TEST-NET 的假地址（必然不可达），所以判据是反过来的：
/// **国内域名能通（走 direct）、境外域名不通（走死掉的代理组）= 分流正确**；
/// 反过来如果境外也"通"了，就说明劫持层或规则没生效、流量从主路由直接漏出去了。
fn netns_state(fakeip: bool) -> DesiredState {
    let mut state = full_state();
    state.network.lan_interface = "zbox-lan0".into();
    state.network.wan_interface = "eth0".into();
    state.network.lan_cidr = "10.99.0.0/24".into();
    state.dns.fakeip.enabled = fakeip;
    // 第二台客户端设成 Bypass，用来验证 nft 的作用域限定真的放行了它
    state.devices = vec![Device {
        ip: "10.99.0.3".into(),
        mac: None,
        hostname: Some("bypass-client".into()),
        mode: DeviceMode::Bypass,
        ip_type: IpType::Dhcp,
        display_name: None,
    }];
    state
}

/// netns 实测的第二种形状：规则集改成**本地 `.srs`**（由 `prepare-rulesets` 出源 JSON、
/// 再在 guest 里 `sing-box rule-set compile` 编出来），于是启动不再赌 GitHub 可达。
/// 这正是 task #9 的目标形状，也是刚才那次 FATAL
/// （`initialize rule-set[0]: ... context deadline exceeded` -> 启动失败）唯一可行的解法。
fn netns_local_state(fakeip: bool) -> DesiredState {
    let mut state = netns_state(fakeip);
    state.policy.block_ads = false;
    state.rule_sets = ["geosite-cn", "geoip-cn", "non-cn"]
        .into_iter()
        .map(|tag| RuleSet::Local {
            tag: tag.into(),
            path: format!("rule-set/{tag}.srs"),
        })
        .collect();
    state
}
