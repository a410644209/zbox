//! 监听这一圈：绑定地址的判定 + 顺序 `accept` 的循环。
//!
//! 这一层薄是刻意的——协议细节全在 [`super::http`]，业务语义全在 [`super::App`]，
//! 这里只做三件事：把 socket 绑到一个**站得住的地址**、一次处理一个连接、把错误写成响应
//! 而不是 panic。单线程顺序 accept 够用（面板是一个人操作的局域网设备），换来的好处是
//! 没有跨线程共享状态，也没有"两个连接同时改同一份状态"这种要另写保护的东西。
//!
//! 绑定地址是唯一一条**必须在代码里挡住**而不是靠文档劝的约束：参考实现的两个面板都监听
//! `0.0.0.0:9099` 且无鉴权，等于把网关交给整个局域网（§8.1）。这里的规则是
//! "只能绑这台机器真有的地址，通配地址一律拒"，见 [`resolve_bind`]。

use super::http;
use super::App;
use crate::model::config::DesiredState;
use crate::svc::topology::Topology;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// 面板默认端口。既避开数据面自己用的那几个（53 / 9888 / 9887 / 8888 / 7890），
/// 也避开两份参考面板的 9099——同一台机器上可能还挂着别人家的面板，
/// 而"面板打不开"里最难查的一种就是端口撞车。
pub const DEFAULT_PANEL_PORT: u16 = 9090;

/// 单次读写超时。循环是顺序的，一个不送完字节的客户端就能占住整块面板，
/// 所以这个时限不是体验项而是可用性项。
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// 判定面板该绑在哪。
///
/// - 给了 `--bind`：可以写 `192.168.1.10` 或 `192.168.1.10:9090`（裸地址按 `default_port`）。
///   地址必须是**这台机器上真有的网卡地址**（`topo.addrs` 里数得出），或是回环
///   （显式给才允许——`127.0.0.1` 是给"SSH 上去再端口转发"这条路留的）。
/// - 没给：取 `state.network.lan_interface` 那张卡上的 IPv4 地址。旁路由单网卡时
///   LAN 与 WAN 是同一张卡（2026-10-05 guest 实测的形状），这时取到的就是那张卡唯一的地址，
///   不需要特判。
/// - 通配地址（`0.0.0.0` / `::`）**永远拒**，显式给也不放行。
pub fn resolve_bind(
    explicit: Option<&str>,
    default_port: u16,
    state: &DesiredState,
    topo: &Topology,
) -> Result<SocketAddr, String> {
    let (ip, port) = match explicit {
        Some(raw) => parse_explicit(raw, default_port)?,
        None => {
            let iface = state.network.lan_interface.as_str();
            let addr = topo
                .addrs
                .iter()
                .find(|a| a.interface == iface)
                .ok_or_else(|| {
                    format!(
                        "网卡 {iface} 上没有 IPv4 地址，面板不知道该绑哪里；\
                         用 --bind <地址>[:{}] 显式指定（不给通配地址）",
                        default_port
                    )
                })?;
            (IpAddr::V4(addr.address), default_port)
        }
    };
    if ip.is_unspecified() {
        return Err(format!(
            "拒绝绑到通配地址 {ip}：那等于把面板交给局域网里每一台设备，\
             也交给任何能路由到这台机器的人（§8.1 点名的就是这一条）。\
             给一个本机网卡上的地址。"
        ));
    }
    if ip.is_loopback() {
        if explicit.is_none() {
            // 默认路径不会走到这里（网卡地址不是回环），出现了就是探测给出了奇怪的答案
            return Err("探测到的局域网地址是回环地址，这不像是能对外服务的那一张网卡".to_string());
        }
        return Ok(SocketAddr::from((ip, port)));
    }
    if !topo.addrs.iter().any(|a| IpAddr::V4(a.address) == ip) {
        let known: Vec<String> = topo
            .addrs
            .iter()
            .map(|a| format!("{}（{}）", a.address, a.interface))
            .collect();
        return Err(format!(
            "这台机器上没有地址 {ip}；本机网卡上的地址是：{}",
            if known.is_empty() {
                "一个都没有（`ip -4 -o addr show` 没给出结果）".to_string()
            } else {
                known.join("、")
            }
        ));
    }
    Ok(SocketAddr::from((ip, port)))
}

/// `--bind` 的两种写法。带端口就按给的，不带就用默认端口。
fn parse_explicit(raw: &str, default_port: u16) -> Result<(IpAddr, u16), String> {
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Ok((addr.ip(), addr.port()));
    }
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Ok((ip, default_port));
    }
    Err(format!(
        "--bind 要的是网卡地址，可以带端口：`192.168.1.10` 或 `192.168.1.10:{default_port}`\
         （v6 用 `[::1]:{default_port}` 这种写法），收到 {raw:?}"
    ))
}

/// 生产入口：绑上去并按顺序服务，直到进程被停掉。
///
/// 返回 `Err` 只发生在 bind 这一步；单个连接的任何故障都写成响应，不退出进程——
/// 一个手滑的客户端不该把面板弄没。
pub fn listen(app: &App<'_>, addr: SocketAddr) -> Result<(), String> {
    let listener = TcpListener::bind(addr)
        .map_err(|e| format!("绑定 {addr} 失败: {e}（地址得是这台机器真有的网卡地址）"))?;
    let got = listener.local_addr().map_err(|e| e.to_string())?;
    println!("面板已监听 http://{got}，访问令牌取法：zboxd panel-token");
    accept_all(listener, app, usize::MAX)
}

/// `accept` 循环。`max` 是给测试用的开关（跑完就返回），生产传 `usize::MAX`。
///
/// 拆出来还有一个用处：测试因此能在**没有线程**的前提下跑通整条 socket 路径——
/// 先连上（connect 会进 backlog），写完请求，再让这一句去 accept，请求已经在缓冲里等着。
pub fn accept_all(listener: TcpListener, app: &App<'_>, max: usize) -> Result<(), String> {
    for stream in listener.incoming().take(max) {
        match stream {
            Ok(stream) => handle(stream, app),
            // 监听套接字本身出错（fd 耗尽一类）才值得把话抛回调用方
            Err(e) => return Err(format!("accept 失败: {e}")),
        }
    }
    Ok(())
}

/// 处理一个连接。任何一条路径都以"写一个响应"结束，写不出去就算了（对面已经断了）。
fn handle(mut stream: TcpStream, app: &App<'_>) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let response = match http::read_request(&mut stream) {
        Ok(request) => app.handle(&request.request()),
        Err(err) => err.to_response(),
    };
    let _ = http::write_response(&mut stream, &response);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::token::PanelToken;
    use crate::gen::init::{InitSystem, Layout};
    use crate::svc::runner::{Exec, Scripted};
    use crate::svc::topology::Addr;
    use crate::svc::Svc;
    use std::io::{Read, Write};
    use std::net::Ipv4Addr;

    const TOKEN: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

    fn addr(text: &str) -> Ipv4Addr {
        text.parse().expect("测试里的地址")
    }

    /// 单网卡旁路由的形状（2026-10-05 guest 实测：LAN 与 WAN 同一张卡）。
    fn one_card() -> Topology {
        Topology {
            gateway: Some(addr("192.168.219.2")),
            wan_interface: Some("eth0".into()),
            addrs: vec![Addr {
                interface: "eth0".into(),
                address: addr("192.168.219.128"),
                prefix: 24,
            }],
            probe_error: None,
        }
    }

    /// 两张卡的形状：局域网那张才是面板该绑的。
    fn two_cards() -> Topology {
        let mut topo = one_card();
        topo.addrs.push(Addr {
            interface: "eth1".into(),
            address: addr("10.14.0.1"),
            prefix: 24,
        });
        topo
    }

    fn state_with_lan(iface: &str) -> DesiredState {
        let mut state = DesiredState::baseline();
        state.network.lan_interface = iface.into();
        state
    }

    #[test]
    fn the_default_bind_is_the_lan_interfaces_own_address() {
        let state = state_with_lan("eth1");
        let got = resolve_bind(None, DEFAULT_PANEL_PORT, &state, &two_cards()).unwrap();
        assert_eq!(
            got.ip(),
            IpAddr::V4(addr("10.14.0.1")),
            "单网卡之外的现场要绑局域网那张"
        );
        assert_eq!(got.port(), DEFAULT_PANEL_PORT);

        // 单网卡：LAN 与 WAN 同一张卡，取到的就是它唯一的地址，不需要特判
        let single = resolve_bind(
            None,
            DEFAULT_PANEL_PORT,
            &state_with_lan("eth0"),
            &one_card(),
        )
        .unwrap();
        assert_eq!(single.ip(), IpAddr::V4(addr("192.168.219.128")));
    }

    #[test]
    fn a_wildcard_is_refused_even_when_given_explicitly() {
        // 这一条是整个绑定规则的意义所在：参考面板就是 0.0.0.0
        for bad in ["0.0.0.0", "0.0.0.0:9090", "::", "[::]:9090"] {
            let err = resolve_bind(
                Some(bad),
                DEFAULT_PANEL_PORT,
                &state_with_lan("eth0"),
                &one_card(),
            )
            .unwrap_err();
            assert!(err.contains("通配地址"), "{bad} 竟被收下：{err}");
        }
    }

    #[test]
    fn an_address_this_box_does_not_own_is_refused_and_shows_what_it_has() {
        let err = resolve_bind(
            Some("10.99.0.9"),
            DEFAULT_PANEL_PORT,
            &state_with_lan("eth0"),
            &one_card(),
        )
        .unwrap_err();
        assert!(err.contains("10.99.0.9"), "{err}");
        assert!(err.contains("192.168.219.128"), "要报出本机有的地址：{err}");
        assert!(err.contains("eth0"), "{err}");
        // 一个地址都没探到时也要说人话，而不是"是空字符串"
        let empty = resolve_bind(
            Some("10.99.0.9"),
            DEFAULT_PANEL_PORT,
            &state_with_lan("eth0"),
            &Topology::default(),
        )
        .unwrap_err();
        assert!(empty.contains("一个都没有"), "{empty}");
    }

    #[test]
    fn loopback_is_allowed_only_on_the_explicit_path() {
        let got = resolve_bind(
            Some("127.0.0.1:9191"),
            DEFAULT_PANEL_PORT,
            &state_with_lan("eth0"),
            &one_card(),
        )
        .unwrap();
        assert_eq!(got.port(), 9191, "显式端口要赢过默认端口");
        assert!(got.ip().is_loopback());
        // 网卡上探到回环（不会自然发生，但那是"绑上去也没人能连"的形状）要拦
        let mut weird = one_card();
        weird.addrs[0].address = addr("127.0.0.1");
        assert!(resolve_bind(None, DEFAULT_PANEL_PORT, &state_with_lan("eth0"), &weird).is_err());
    }

    #[test]
    fn a_missing_lan_interface_names_itself_and_the_way_out() {
        let err = resolve_bind(
            None,
            DEFAULT_PANEL_PORT,
            &state_with_lan("br0"),
            &one_card(),
        )
        .unwrap_err();
        assert!(err.contains("br0"), "{err}");
        assert!(err.contains("--bind"), "{err}");
    }

    #[test]
    fn bind_text_that_is_neither_address_nor_address_port_is_rejected() {
        for bad in ["eth0", "192.168.219.128:not-a-port", "localhost"] {
            assert!(
                resolve_bind(
                    Some(bad),
                    DEFAULT_PANEL_PORT,
                    &state_with_lan("eth0"),
                    &one_card()
                )
                .is_err(),
                "{bad} 竟被收下"
            );
        }
    }

    /// 起一个只服务一个连接的面板，返回客户端侧的流。
    ///
    /// 顺序是先连、先写、后 accept——`connect` 会进 backlog，请求因此已经躺在内核缓冲里，
    /// 这一句不需要线程也不会死锁。
    fn roundtrip(raw: &str, layout: &Layout, state: &DesiredState) -> String {
        let runner = Scripted::with_results(vec![Ok(Exec {
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })]);
        let svc = Svc::new(layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = PanelToken::new(TOKEN).unwrap();
        let app = App::new(&svc, &token);
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(raw.as_bytes()).unwrap();
        accept_all(listener, &app, 1).expect("一个连接要能服务完");
        let mut got = String::new();
        client.read_to_string(&mut got).unwrap();
        got
    }

    fn fixture(name: &str) -> Layout {
        let dir = std::env::temp_dir().join(format!("zbox-serve-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        Layout {
            root: dir.join("var-lib-zbox").display().to_string(),
            current_dir: dir.join("var-lib-zbox/current").display().to_string(),
            ..Layout::default()
        }
    }

    #[test]
    fn a_socket_round_trip_without_the_token_is_a_401_json() {
        let layout = fixture("unauth");
        let state = DesiredState::baseline();
        let got = roundtrip(
            "GET /api/state HTTP/1.1\r\nHost: x\r\n\r\n",
            &layout,
            &state,
        );
        assert!(got.starts_with("HTTP/1.1 401 "), "{got}");
        assert!(got.contains("application/json"), "{got}");
        assert!(!got.contains(TOKEN), "回执里不许出现真令牌");
    }

    #[test]
    fn a_socket_round_trip_with_the_token_reads_the_baseline_state() {
        let layout = fixture("auth");
        let state = DesiredState::baseline();
        let got = roundtrip(
            "GET /api/state HTTP/1.1\r\nHost: x\r\nX-ZBox-Token: 0f1e2d3c4b5a69788796a5b4c3d2e1f0\r\n\r\n",
            &layout,
            &state,
        );
        assert!(got.starts_with("HTTP/1.1 200 OK\r\n"), "{got}");
        let body_at = got.find("\r\n\r\n").unwrap() + 4;
        let value: serde_json::Value = serde_json::from_str(&got[body_at..]).unwrap();
        assert_eq!(value["source"], "missing", "这台机还没存过状态");
        assert_eq!(value["etag"], crate::api::ABSENT_ETAG);
    }

    #[test]
    fn a_garbled_request_line_becomes_a_400_not_a_panic() {
        let layout = fixture("garbled");
        let state = DesiredState::baseline();
        let got = roundtrip("GET /api/state\r\n\r\n", &layout, &state);
        assert!(got.starts_with("HTTP/1.1 400 "), "{got}");
        assert!(got.contains("HTTP/1.x"), "{got}");
    }

    /// 静态面在 socket 上的形状。**没有** `X-ZBox-Token` 这一行也要能拿到 200，
    /// 而 `Content-Length` 必须等于体的**字节**数：这一页满屏中文，按字符数写就等于
    /// 浏览器读到截断——所以这里比的是 `body.len()`，不是 `chars().count()`。
    #[test]
    fn a_socket_round_trip_reads_the_page_without_any_token() {
        let layout = fixture("static");
        let state = DesiredState::baseline();
        let got = roundtrip("GET / HTTP/1.1\r\nHost: x\r\n\r\n", &layout, &state);
        assert!(got.starts_with("HTTP/1.1 200 OK\r\n"), "{got}");
        let head_end = got.find("\r\n\r\n").expect("响应头要有结束空行");
        let head = &got[..head_end];
        let body = &got[head_end + 4..];

        let declared = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                (name.eq_ignore_ascii_case("content-length"))
                    .then(|| value.trim().parse::<usize>().ok())
            })
            .and_then(|n: Option<usize>| n)
            .expect("要带 Content-Length");
        assert_eq!(declared, body.len(), "Content-Length 是字节数");
        assert!(
            head.lines().any(|line| line
                .to_ascii_lowercase()
                .contains("content-type: text/html; charset=utf-8")),
            "{head}"
        );
        assert!(
            head.to_lowercase().contains("cache-control: no-store"),
            "{head}"
        );

        let asset = crate::api::assets::lookup("/").expect("页面在表上");
        assert_eq!(body, asset.body, "socket 上给出的就是表里那份字节");
        // 页面自己声明的那句 CSP 与两个外链是这一层的契约，掉了就等于面板打不开
        assert!(body.contains("script-src 'self'"), "{body}");
        assert!(body.contains("/panel.js"), "{body}");
    }
}
