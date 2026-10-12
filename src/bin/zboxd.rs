//! `zboxd`：控制面命令行入口，把 `docs/ARCHITECTURE.md` §6 那套路数原样落成子命令。
//!
//! 刻意不引参数解析库：子命令就这几个，手写 `while` 更好读，也少一个依赖。
//!
//! 目标机上以 root 运行。`--root` 只挪状态目录（期望状态、快照、规则集、sing-box 的 `-D`）；
//! 服务制品写的是 `/etc/...` 绝对路径，得用 `--prefix` 才躲得开——那是给非生产环境试跑的。

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use zboxd::api::serve::{listen, resolve_bind, DEFAULT_PANEL_PORT};
use zboxd::api::token::{self, PanelToken};
use zboxd::api::App;
use zboxd::apply::engine;
use zboxd::doctor::{self, Finding, Severity};
use zboxd::gen::init::{self as gen_init, InitSystem, Layout};
use zboxd::gen::snapshot_files;
use zboxd::model::config::DesiredState;
use zboxd::svc::provision::{Provision, Source};
use zboxd::svc::ruleset::Report;
use zboxd::svc::runner::{Runner, System};
use zboxd::svc::topology::{self, Topology};
use zboxd::svc::{now_secs, Svc};

const USAGE: &str = "\
usage: zboxd [flags] <command> [state.json]

flags:
  --root <DIR>             状态目录，默认 /var/lib/zbox（期望状态、快照、规则集、sing-box 的 -D）
  --prefix <DIR>           服务制品的写入前缀，默认真写 /etc；给了它 install 就只写制品，
                           不启用服务、不抓规则集、不下发配置（启用命令按服务名寻址，改不了目标）
  --init <systemd|openrc>  跳过探测；探测不出来时必须显式给
  --singbox-bin <PATH>     sing-box 二进制，默认 /usr/local/bin/sing-box
  --zboxd-bin <PATH>       zboxd 自己的路径，只进面板那份服务制品，默认 /usr/local/bin/zboxd
  --panel-port <PORT>      面板服务制品里写的端口，默认 9090（serve 自己的 --port 只管当场那次）
  --asset <PATH>           provision 用本地资产（官方 .tar.gz 或已解出的可执行文件）
  --bind <IP[:PORT]>       serve 的监听地址。默认取期望状态里局域网网卡上的那个 IPv4 地址。
                           只能写这台机器真有的地址；0.0.0.0 与 :: 即使显式给也拒（§8.1）。
                           127.0.0.1 只有显式写才放行——那是 SSH 端口转发那条路
  --port <PORT>            serve 的端口，默认 9090。--bind 里带了端口时以 bind 为准
  --force                  忽略规则集的新鲜度判定全部重抓；也让 provision 无视版本重放二进制
  --target <NAME>          render 打哪一份生成物：config.json（默认）| dae | oxidns |
                           zbox.nft | netstate.sh。只对 render 有意义，给别的命令时报错而不是忽略

位置参数:
  state.json               install/apply/rule-sets/enable-hijack/doctor 的期望状态。
                           不给就读规范落点 <root>/state.json（面板读写的那一份）；
                           给了就用给的，彩排和复现要能指向别的文件。
                           对 rollback 它是修订号，不是状态文件。
                           对 serve 它是可选的：那份文件还没写过时面板照样要能打开
                           （第一份状态本来就是面板写的），坏到读不出才拦。

commands:
  provision      按架构 + libc 把钉死版本的 sing-box 落地（install 会先跑这一步）
  detect         打印本机网卡形状（上游网关、默认路由网卡、各卡 IPv4 段）与结论；判不出来退出码非 0
  panel-token    打印面板访问令牌；<root>/panel.token 不存在时先生成一份（0600）再打印
  serve          起面板的 HTTP 监听。绑定地址按 --bind / 局域网网卡判定，绑不上就报错退出
  install        落地内核 + 探测网卡 + 写服务制品 + 落地规则集 + 启用 + 下发第一份配置
  apply          生成 -> 校验 -> 提交 -> 重启 -> 探测，失败自动回滚
  render         把一份生成物打到 stdout（--target config.json|dae|oxidns|zbox.nft|netstate.sh，
                 默认 config.json）：
                 不写盘、不校验、不重启。dae 与 oxidns 那两份要能单独拿出来，是为了拿它们去过
                 各自上游的 validate 门，而这一步按纪律不许碰在跑的数据面；zbox.nft 与
                 netstate.sh 同理，它们的上游就是内核，门是 `nft -c -f` 与 `sh -n`
  rule-sets      抓取声明的规则源、编译成 .srs、验收并记清单
  enable-hijack  先确认内核在听，再装 nft 劫持层与策略路由
  disable-hijack 撤掉劫持层（只删 ZBox 自己的对象）
  rollback [修订号] 回到该修订并重启；不给就回到已知良好快照。
                 修订号给错时报错并按新到旧列出全部现有修订
  status         打印服务状态、劫持层与在听端口
  doctor         期望状态 vs 现场读数，输出结论与可粘贴的命令
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("zboxd: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mut layout = Layout::default();
    let mut prefix = String::new();
    let mut forced_init = None;
    let mut command = None;
    let mut state_path = None;
    let mut force = false;
    let mut asset = None;
    let mut bind = None;
    let mut port: Option<u16> = None;
    let mut zboxd_bin: Option<String> = None;
    let mut panel_port: Option<u16> = None;
    let mut target: Option<String> = None;

    while let Some(arg) = args.next() {
        // 只认 `--flag value` 这一种写法：`--flag=value` 看着方便，多一份解析不值得
        let mut take = |what: &str| -> Result<String, Box<dyn Error>> {
            args.next()
                .ok_or_else(|| format!("{what} 需要一个值").into())
        };
        match arg.as_str() {
            "--root" => layout.root = take("--root")?,
            "--prefix" => prefix = take("--prefix")?,
            "--singbox-bin" => layout.singbox_bin = take("--singbox-bin")?,
            "--zboxd-bin" => zboxd_bin = Some(take("--zboxd-bin")?),
            "--panel-port" => {
                let raw = take("--panel-port")?;
                let given: u16 = raw
                    .parse()
                    .map_err(|_| format!("--panel-port 要的是 1-65535 的整数，收到 {raw:?}"))?;
                if given == 0 {
                    return Err("--panel-port 不接受 0（制品里要的是一个定值端口）".into());
                }
                panel_port = Some(given);
            }
            "--asset" => asset = Some(PathBuf::from(take("--asset")?)),
            "--bind" => bind = Some(take("--bind")?),
            "--port" => {
                let raw = take("--port")?;
                let given: u16 = raw
                    .parse()
                    .map_err(|_| format!("--port 要的是 1-65535 的整数，收到 {raw:?}"))?;
                // 0 在 socket 层是"随便给我一个空闲端口"，对面板这种要被人连的地址没意义
                if given == 0 {
                    return Err(
                        "--port 不接受 0（那是\"随机挑一个端口\"的写法，面板要的是一个定值）"
                            .into(),
                    );
                }
                port = Some(given);
            }
            "--force" => force = true,
            "--target" => target = Some(take("--target")?),
            "--init" => {
                forced_init = Some(match take("--init")?.as_str() {
                    "systemd" => InitSystem::Systemd,
                    "openrc" => InitSystem::OpenRC,
                    other => return Err(format!("--init 只认 systemd|openrc，收到 {other}").into()),
                })
            }
            "help" | "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            other if other.starts_with('-') => {
                return Err(format!("未知参数 {other}\n{USAGE}").into())
            }
            other if command.is_none() => command = Some(other.to_string()),
            other if state_path.is_none() => state_path = Some(other.to_string()),
            other => return Err(format!("多余的参数 {other}").into()),
        }
    }
    layout.current_dir = format!("{}/current", layout.root.trim_end_matches('/'));
    if !layout.root.starts_with('/') || (!prefix.is_empty() && !prefix.starts_with('/')) {
        // 这两个值会原样进 systemd 的 WorkingDirectory 和 sing-box 的 -D：
        // 相对路径在服务的工作目录下另起炉灶，实测过的现场是 `rule-set: no such file`。
        return Err("--root 与 --prefix 必须是绝对路径".into());
    }

    let command = command.ok_or_else(|| USAGE.to_string())?;
    // `--bind` / `--port` 只有一处消费者。别的命令收到它们时报错而不是忽略：
    // "我给了面板一个新地址，命令行啥也没说，面板还在老地址上"是最难查的那种没生效。
    if command != "serve" {
        for (flag, given) in [("--bind", bind.is_some()), ("--port", port.is_some())] {
            if given {
                return Err(format!("{flag} 只对 serve 有意义，收到的命令是 {command}").into());
            }
        }
    }
    // `--zboxd-bin` / `--panel-port` 唯一的消费者是 install 写进去的那两份面板制品：
    // 给别的命令时报错而不是忽略，理由与上面那两条一样——"我给了个新值，命令行啥也没说"最难查。
    if command != "install" {
        for (flag, given) in [
            ("--zboxd-bin", zboxd_bin.is_some()),
            ("--panel-port", panel_port.is_some()),
        ] {
            if given {
                return Err(format!(
                    "{flag} 只对 install 有意义（它进的是面板那份服务制品），收到的命令是 {command}"
                )
                .into());
            }
        }
    }
    // `--target` 唯一的消费者是 render（打哪一份生成物）。给别的命令时报错而不是忽略，
    // 理由与上面两条相同。
    if command != "render" && target.is_some() {
        return Err(format!("--target 只对 render 有意义，收到的命令是 {command}").into());
    }
    if let Some(path) = zboxd_bin {
        // 绝对路径是硬要求：这一条要进 ExecStart / command=，相对路径会在服务自己的
        // 工作目录下另起炉灶——和 --root/--prefix 那两条同一个理由。
        if !path.starts_with('/') {
            return Err("--zboxd-bin 要的是绝对路径（它会被写进服务制品）".into());
        }
        layout.zboxd_bin = path;
    }
    if let Some(port) = panel_port {
        layout.panel_port = port;
    }
    let runner = System;
    // 落地二进制与 init 系统、与期望状态都无关，所以它分流在最前面：
    // 一台还没探测出 init 的机器（chroot、装到一半）也要能先把内核放好。
    if command == "provision" {
        provision(Provision::new(&layout, &runner), asset.as_deref(), force)?;
        return Ok(ExitCode::SUCCESS);
    }
    // 网卡形状与 init 系统、与 sing-box 都无关，`detect` 因此也不许先探测 init：
    // 面板第一步就是它，一台还没装任何东西的机器上也要能问出答案。
    if command == "detect" {
        return detect_network(&runner);
    }
    // 取令牌同理：它只读 `<root>/panel.token`，与 init、与 sing-box、与期望状态都无关。
    if command == "panel-token" {
        return panel_token(&layout);
    }
    let init = detect_init(forced_init)?;
    // 只有真要生成/比对配置的命令才需要状态文件；status/rollback/disable-hijack 不用。
    // 不给位置参数时读规范落点 `<root>/state.json`（面板写的就是这一份）；给了就用给的，
    // 彩排和复现都得能指到别的文件上。
    let state = match command.as_str() {
        "install" | "apply" | "enable-hijack" | "rule-sets" | "doctor" | "render" => {
            Some(load_state(
                state_path
                    .as_deref()
                    .unwrap_or(layout.state_path().as_str()),
            )?)
        }
        "serve" => Some(load_state_or_baseline(
            state_path
                .as_deref()
                .unwrap_or(layout.state_path().as_str()),
        )?),
        _ => None,
    };
    // §6 第 5 步：state 里的 network 还等于内置基线形状（没人写过）时就地探真网卡。
    // `doctor` 与 `serve` 不拦路：前者只拿期望值比现场，后者的默认绑定地址虽然也来自这张
    // 网卡，但探不出形状时人还能用 `--bind` 显式给——一条错误不该把出口一起堵掉。
    let state = match state {
        Some(s) => Some(fill_network(
            &runner,
            &s,
            matches!(command.as_str(), "doctor" | "serve" | "render"),
        )?),
        None => None,
    };
    let ports = state.clone().unwrap_or_default().ports;
    let svc = Svc::new(&layout, init, &runner, &ports);

    let code = match command.as_str() {
        "install" => {
            install(
                &svc,
                &state.expect("上面取过"),
                &prefix,
                asset.as_deref(),
                force,
            )?;
            ExitCode::SUCCESS
        }
        "apply" => {
            apply(&svc, &state.expect("上面取过"))?;
            ExitCode::SUCCESS
        }
        "rule-sets" => {
            sync_rule_sets(&svc, &state.expect("上面取过"), force)?;
            ExitCode::SUCCESS
        }
        "enable-hijack" => {
            enable_hijack(&svc, &state.expect("上面取过"))?;
            ExitCode::SUCCESS
        }
        "disable-hijack" => {
            disable_hijack(&svc)?;
            ExitCode::SUCCESS
        }
        "rollback" => {
            // 位置参数对 rollback 是修订号，不是 state.json（它不生成配置）
            rollback(&svc, state_path.as_deref())?;
            ExitCode::SUCCESS
        }
        "status" => {
            status(&svc)?;
            ExitCode::SUCCESS
        }
        "doctor" => doctor(&svc, &state.expect("上面取过"), &layout)?,
        // 存在的理由是验收纪律那一条：`sing-box run` 才算验收，而拿到生成物又不能碰
        // 在跑的数据面——apply / install 都要重启服务，这一条只出不写。
        "render" => {
            let state = state.expect("上面取过");
            match zboxd::gen::render_target(&state, target.as_deref().unwrap_or("config.json")) {
                Ok(body) => {
                    print!("{body}");
                    ExitCode::SUCCESS
                }
                // 报错走 Err 而不是打空文本到 stdout：拿它去 validate 的人若只看到一个空文件，
                // 下一步就会去怀疑上游，而真正的原因在这句话里。
                Err(detail) => return Err(detail.into()),
            }
        }
        "serve" => {
            let state = state.expect("上面取过");
            serve_panel(&svc, &layout, bind.as_deref(), port, &state, &runner)?
        }
        other => return Err(format!("未知命令 {other}\n{USAGE}").into()),
    };
    Ok(code)
}

/// 状态文件入口：schema 版本门 -> serde -> `validate()`，全在 `model::state::read`。
fn load_state(path: &str) -> Result<DesiredState, Box<dyn Error>> {
    Ok(zboxd::model::state::read(Path::new(path))?)
}

/// `serve` 用的那一份状态：**文件不存在时退化成基线形状**，坏到读不出仍然报错。
///
/// 之所以要退化：面板的第一件正事就是把第一份期望状态写下去，所以一台什么都没装的机器上
/// 它必须能打开（`GET /api/state` 会照实回报 `source: missing`）。而"文件在但解析不过"
/// 是另一种现场——那种时候要看的恰好是面板里的 broken 界面，CLI 替它猜一份基线会把问题
/// 从界面上抹掉。
fn load_state_or_baseline(path: &str) -> Result<DesiredState, Box<dyn Error>> {
    if Path::new(path).is_file() {
        return load_state(path);
    }
    Ok(DesiredState::baseline())
}

/// `panel-token`：读现成的，没有就生成一份再打印。令牌的**唯一**取面。
///
/// 原值只走 stdout（好让人 `zboxd panel-token | xclip` 这类用法成立），解释性的一句话走
/// stderr。`serve` 自己不打令牌是有原因的：OpenRC 的 `supervise-daemon` 把 stdout 收进
/// 服务日志，那等于把密钥写进一个默认可读的文件。
fn panel_token(layout: &Layout) -> Result<ExitCode, Box<dyn Error>> {
    let path = PathBuf::from(layout.panel_token_path());
    let existed = path.is_file();
    let token = PanelToken::load_or_create(&path, token::urandom)?;
    println!("{}", token.secret());
    eprintln!(
        "令牌取自 {}（{}）。面板里以 X-ZBox-Token 头带上；换发就删掉这个文件再跑一次。",
        path.display(),
        if existed {
            "原有那一份，未换发"
        } else {
            "本命令新生成，已按 0600 落盘"
        }
    );
    Ok(ExitCode::SUCCESS)
}

/// `serve`：判定该绑在哪，读令牌，然后监听。
///
/// 绑定规则本身一条都不在这里（在 [`resolve_bind`]），CLI 这一层只做输入输出——
/// 否则"面板起不来"就有两处可能的措辞，而其中一处是错的。
/// 网卡形状这里第二次探一遍：`fill_network` 探过是为了填期望状态，那次没把形状留下。
fn serve_panel(
    svc: &Svc,
    layout: &Layout,
    bind: Option<&str>,
    port: Option<u16>,
    state: &DesiredState,
    runner: &dyn Runner,
) -> Result<ExitCode, Box<dyn Error>> {
    let addr = resolve_bind(
        bind,
        port.unwrap_or(DEFAULT_PANEL_PORT),
        state,
        &Topology::detect(runner),
    )?;
    let token =
        PanelToken::load_or_create(&PathBuf::from(layout.panel_token_path()), token::urandom)?;
    listen(&App::new(svc, &token), addr)?;
    Ok(ExitCode::SUCCESS)
}

/// 探测不出来就停下来给人看清单，**不许猜一个往机器上写**（§6 的硬约束）。
fn detect_init(given: Option<InitSystem>) -> Result<InitSystem, Box<dyn Error>> {
    if let Some(init) = given {
        return Ok(init);
    }
    InitSystem::detect(&|p| Path::new(p).exists()).ok_or_else(|| {
        format!(
            "认不出 init 系统（/run/systemd/system 与 /run/openrc/softlevel 都不在）。\
             确认后加 --init systemd|openrc：这是两套服务制品的分岔点，猜错就是往机器上\
             写一份跑不起来的服务文件。"
        )
        .into()
    })
}

/// §6 第 5 步：state 里的 `network` 还等于内置基线形状（`ens33` / `192.168.1.0/24`）时，
/// 当场探本机网卡填进去；显式写过的一律不动，但网卡名在本机找不到时会打警告。
///
/// 为什么要有这条：彩排那次 state 里躺着默认值 `ens33`，机器上是 `eth0`，写出来的
/// 持久化 sysctl 文件因此有一条永远不生效的键——busybox 的 `sysctl -p` 对未知键只打
/// 一行 stderr 就 rc=0（schema §11.8.6）。静默写错比拒绝执行贵得多。
fn fill_network(
    runner: &dyn Runner,
    state: &DesiredState,
    lenient: bool,
) -> Result<DesiredState, Box<dyn Error>> {
    let topo = Topology::detect(runner);
    let filled = match topology::fill(state, &topo) {
        Ok(filled) => filled,
        // 诊断不许被探测拦停：现场读不到就说读不到，判据照常跑
        Err(detail) if lenient => {
            eprintln!("提示: 网卡形状没探出来，诊断按 state.json 里写的值比：{detail}");
            return Ok(state.clone());
        }
        Err(detail) => return Err(detail.into()),
    };
    // 这几句一律 stderr：`render` 的 stdout 是要 `> config.json` 或直接喂给
    // `sing-box check` 的那一份 JSON，混一行中文进去它就不是合法 JSON 了。
    for warning in &filled.warnings {
        eprintln!("提示: {warning}");
    }
    let n = &filled.network;
    match filled.how {
        topology::How::Detected => eprintln!(
            "网卡形状探得：局域网 {}（段 {}）、上游 {}，上游网关 {}",
            n.lan_interface,
            n.lan_cidr,
            n.wan_interface,
            topo.gateway
                .map(|g| g.to_string())
                .unwrap_or_else(|| "未读到".into())
        ),
        topology::How::KeptExplicit => eprintln!(
            "网卡形状取自 state.json：局域网 {}（段 {}）、上游 {}",
            n.lan_interface, n.lan_cidr, n.wan_interface
        ),
    }
    let mut filled_state = state.clone();
    filled_state.network = filled.network;
    Ok(filled_state)
}

/// `detect`：把现场打印出来，结论能不能给也一并给出退出码。面板预填表单跑这条。
fn detect_network(runner: &dyn Runner) -> Result<ExitCode, Box<dyn Error>> {
    let topo = Topology::detect(runner);
    // 命令根本没跑起来时别再打印"没有默认路由"：那是另一回事，写在一起会把人引去查路由表
    if let Some(detail) = &topo.probe_error {
        println!("读不到现场: {detail}");
    } else {
        println!(
            "默认路由网卡: {}",
            topo.wan_interface
                .as_deref()
                .unwrap_or("未读到（没有 IPv4 默认路由）")
        );
        println!(
            "上游网关: {}",
            topo.gateway
                .map(|g| g.to_string())
                .unwrap_or_else(|| "未读到".into())
        );
    }
    for addr in &topo.addrs {
        println!(
            "网卡 {}: 地址 {}/{}，段 {}",
            addr.interface,
            addr.address,
            addr.prefix,
            addr.network_cidr()
        );
    }
    match topo.verdict() {
        Ok(n) => {
            let single = n.lan_interface == n.wan_interface;
            println!(
                "结论: 局域网网卡 {}，段 {}，上游网卡 {}{}",
                n.lan_interface,
                n.lan_cidr,
                n.wan_interface,
                if single {
                    "（单网卡旁路由）"
                } else {
                    ""
                }
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(detail) => {
            eprintln!("判不出: {detail}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn apply(svc: &Svc, state: &DesiredState) -> Result<(), Box<dyn Error>> {
    let files = snapshot_files(state)?;
    let outcome = engine::apply(
        &svc.store(),
        svc.target().as_ref(),
        &files,
        &svc.actions(),
        engine::DEFAULT_KEEP,
    )?;
    match outcome {
        engine::ApplyOutcome::Applied { revision } => println!("已下发修订 {revision}"),
        engine::ApplyOutcome::Unchanged { revision } => {
            println!("内容与 {revision} 完全一致，未触碰内核（不重启、不断流）")
        }
    }
    Ok(())
}

fn install(
    svc: &Svc,
    state: &DesiredState,
    prefix: &str,
    asset: Option<&Path>,
    force: bool,
) -> Result<(), Box<dyn Error>> {
    // §6 第 1 步：内核先就位。后面每一步（`sing-box check`、预跑门、规则集编译）都要执行它，
    // 拿一份跑不动或版本不对的二进制去跑，报错会全都变成看不懂的形状。
    provision(svc.provision(), asset, force)?;
    let artifacts = relocate(
        &gen_init::artifacts(svc.init(), state, svc.layout()),
        prefix,
    );
    for path in artifacts.keys() {
        println!("已写入 {path}");
    }
    if !prefix.is_empty() {
        // 启用命令是按服务名寻址的（`rc-update add zbox default`），制品挪到试跑前缀里就没有
        // 对象可启用；后面的规则集与第一份配置也都要对着一个真在跑的服务。所以带 --prefix 的
        // install 到"制品写好了"为止，把剩下的原样列出来给人看。
        for argv in svc.stage(&artifacts)? {
            println!("待执行（--prefix 不启用服务）：`{}`", argv.join(" "));
        }
        println!("试跑到此为止：服务未启用、规则集未抓、第一份配置未下发。");
        return Ok(());
    }
    // 机器上已经有配置时才让 install 去拉起服务。首次安装不拉：那条 restart 在
    // `<current>/config.json` 不存在时照样 rc=0 且 `status` 报 started（schema §11.8.9），
    // 起不来的只有 worker，留给人的读数就是"执行成功了但一个端口都没在听"。
    // 第 7 步的 apply 会带着刚生成的配置重启，服务照样起得来。
    let config_ready = std::path::Path::new(&svc.layout().current_dir)
        .join("config.json")
        .is_file();
    let written = svc.install(&artifacts, config_ready)?;
    for argv in &written {
        println!("已执行 `{}`", argv.join(" "));
    }
    if !config_ready {
        println!(
            "首次安装：数据面已登记但未拉起（此刻还没有配置），下面下发第一份配置时再启；\
             面板已拉起，它不需要配置就能开门——令牌取法：zboxd panel-token"
        );
    }
    // 顺序是硬的：先把规则集落到状态目录，再下发第一份配置。反了就是让 sing-box 在启动那
    // 约 15 秒预算里现抓 GitHub（§11.7 实测超时直接 FATAL），而首次安装往往正是最没网的时候。
    sync_rule_sets(svc, state, false)?;
    // §6 第 6 步：第一份配置得是"装了不断网"的那份，所以直接下发起始状态
    apply(svc, state)
}

/// §6 第 1 步：把钉死版本的官方 sing-box 放到 `--singbox-bin` 上。
///
/// 主路是本地资产（`--asset`，scp 过去就行）：32MB 的二进制在 guest 里实测抓不动（schema §11.8），
/// 没给 `--asset` 才去现抓。现役二进制已经报 1.14.2 时这一步连网络都不碰，所以 install 每次都带上它。
fn provision(p: Provision<'_>, asset: Option<&Path>, force: bool) -> Result<(), Box<dyn Error>> {
    let target = p.detect_target()?;
    println!(
        "本机判为 {}（{:?}），对应资产 {}",
        target.arch,
        target.libc,
        target.asset_name()
    );
    let source = match asset {
        Some(path) => Source::Asset(path.to_path_buf()),
        None => Source::Url,
    };
    let outcome = p.install(&source, force)?;
    println!("{}", outcome.summarize());
    Ok(())
}

/// 抓取 -> 归一化 -> 编译 -> 两级验收 -> 清单。note 不算失败（现役产物还在位），但一定打出来。
/// 抓一轮规则集并把结果说完。
///
/// 硬失败也先打印**已经落地的部分**再报错：之前这里写作 `sync(...)?`，一次死源就把整份报告
/// 吞了，另外几条其实已经装了盘，现场表现却是"点了更新没反应"（映射报告 §5.3）。
fn sync_rule_sets(svc: &Svc, state: &DesiredState, force: bool) -> Result<(), Box<dyn Error>> {
    // 动作本身在 `Svc::sync_rule_sets`（面板用的是同一个）。这一层只做两件事：
    // 把部分结果打印出来，以及把错误原话交给 ExitCode。
    let failure = match svc.sync_rule_sets(state, now_secs(), force) {
        Ok(report) => {
            print_report(&report);
            return Ok(());
        }
        Err(failure) => failure,
    };
    print_report(failure.report());
    Err(failure.to_string().into())
}

fn print_report(report: &Report) {
    for tag in &report.refreshed {
        println!("规则集已落地 {tag}");
    }
    for tag in &report.kept {
        println!("规则集沿用 {tag}");
    }
    for tag in &report.reclaimed {
        println!("规则集已回收 {tag}（声明删掉了，产物与清单条目一起撤）");
    }
    for note in &report.notes {
        println!("提示: {note}");
    }
}

// 时钟住在 `zboxd::svc::now_secs`：面板那条路也要用同一个，一个时钟不许有两个定义。

fn enable_hijack(svc: &Svc, state: &DesiredState) -> Result<(), Box<dyn Error>> {
    // CLI 这一条是"下发 + 开劫持"两步连起来，面板把它们做成两个按钮是有意的（§5.4 分级回执：
    // 会重启数据面的那一步不许藏在别的动作里）。顺序仍然硬：内核没在听就下 nft，
    // 局域网侧的表现是 Connection refused（schema §11.7）。
    apply(svc, state)?;
    svc.enable_hijack()?;
    println!("劫持层已装载。挑一台把网关指到本机的设备试一下，DNS 答案该变了。");
    Ok(())
}

fn disable_hijack(svc: &Svc) -> Result<(), Box<dyn Error>> {
    if !svc.disable_hijack()? {
        println!("现场没有 ZBox 的劫持层（nft 表与策略路由都不在），什么都不用撤。");
        return Ok(());
    }
    println!("劫持层已卸载。sysctl 保持原值——脚本从来不动别人的键，回滚也不该顺手改回来。");
    Ok(())
}

fn rollback(svc: &Svc, revision: Option<&str>) -> Result<(), Box<dyn Error>> {
    // 选哪一份、怎么报错，全在 `Svc::rollback`（面板用的是同一条）。这里只打印。
    let got = svc.rollback(revision)?;
    println!("已回到{}修订 {}", got.how.word(), got.id);
    Ok(())
}

fn status(svc: &Svc) -> Result<(), Box<dyn Error>> {
    let reading = svc.observe().read();
    println!(
        "init 系统: {}",
        match svc.init() {
            InitSystem::Systemd => "systemd",
            InitSystem::OpenRC => "OpenRC",
        }
    );
    println!("服务活跃: {}", tri(reading.service_active));
    println!(
        "nft 表已加载: {} / 策略路由已加载: {}",
        tri(reading.nft_table_loaded),
        tri(reading.divert_rule_loaded)
    );
    println!(
        "ip_forward: {} / rp_filter(all): {} / 默认路由网卡: {}",
        num(reading.ip_forward),
        num(reading.rp_filter_all),
        reading
            .default_route_iface
            .unwrap_or_else(|| "未读到".into()),
    );
    println!(
        "在听端口: {}",
        reading
            .listening
            .map(|v| v.iter().map(u16::to_string).collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| "未读到".into())
    );
    Ok(())
}

/// 有致命项时退出码非 0，方便脚本和面板把"跑一遍诊断"当成门。
fn doctor(svc: &Svc, state: &DesiredState, layout: &Layout) -> Result<ExitCode, Box<dyn Error>> {
    let findings = doctor::diagnose(state, &svc.observe().read(), layout);
    if findings.is_empty() {
        println!("没有发现问题。");
        return Ok(ExitCode::SUCCESS);
    }
    for f in &findings {
        print_finding(f);
    }
    let fatal = findings.iter().filter(|f| f.is_fatal()).count();
    if fatal > 0 {
        eprintln!("诊断: {fatal} 条致命项");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_finding(f: &Finding) {
    let tag = match f.severity {
        Severity::Fatal => "致命",
        Severity::Warn => "提示",
    };
    println!("[{tag}] {} — {}", f.code, f.summary);
    println!("  期望: {}", f.expected);
    println!("  实际: {}", f.actual);
    for cmd in &f.fix {
        println!("  处置: {cmd}");
    }
}

fn tri(v: Option<bool>) -> String {
    match v {
        Some(true) => "是".into(),
        Some(false) => "否".into(),
        None => "未读到".into(),
    }
}

fn num(v: Option<u8>) -> String {
    v.map_or_else(|| "未读到".into(), |n| n.to_string())
}

/// 把 `/etc/...` 形式的制品路径挪到前缀下，供非生产环境试跑。
fn relocate(artifacts: &BTreeMap<String, String>, prefix: &str) -> BTreeMap<String, String> {
    if prefix.is_empty() {
        return artifacts.clone();
    }
    artifacts
        .iter()
        .map(|(path, body)| {
            (
                Path::new(prefix)
                    .join(path.trim_start_matches('/'))
                    .display()
                    .to_string(),
                body.clone(),
            )
        })
        .collect()
}
