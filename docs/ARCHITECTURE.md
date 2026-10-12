# ZBox 架构设计

状态：草案 v0.1。旁路由 MVP 部分的内核语义待目标机实测确认（`sing-box check` / `nft -c` /
netns 端到端）。目标系统：**Alpine 优先**（虚拟机占用更小），同时兼容 Ubuntu/Debian。

## 1. 定位

ZBox 是一台虚拟机上的**软路由控制面**（Alpine 或 Ubuntu/Debian， systemd 与 OpenRC 两类 init
系统都要能装）。它不实现任何代理协议，流量转发全部交给官方 sing-box 二进制。

```
LAN 设备 ──网关/DNS 指向──> ZBox 旁路由
                              ├── zboxd (Rust 控制面: 配置生成 + 编排 + API + WebUI)
                              ├── sing-box (数据面: tproxy/mixed inbound + DNS + rule_set 分流)
                              └── nftables + ip rule (劫持与策略路由)
```

三个不变式，贯穿所有设计：

1. **数据面永远是被编排的外部二进制**，不是 ZBox 的进程内代码。
2. **任何下发到网络的配置必须先通过内核校验，失败自动回滚**（见 §5）。旁路由配错会让全家断网，这条是底线。
3. **不复制参考项目的源码**。`reference/mosdns-t/` 是 GPL-3.0，只借鉴其数据模型与交互流程；`reference/abox/` 是闭源二进制，只能从符号推断功能面。

## 2. 技术栈

| 层 | 选择 | 理由 |
| --- | --- | --- |
| 控制面 | Rust：`axum` + `tokio` | 配置模型是大型 tagged union，enum + serde 让非法组合不可表达；`match` 穷尽性保证加协议时生成器不漏分支 |
| 配置模型 | `serde` / `serde_json`，internally-tagged enum | 生成 sing-box conf.d 分片；类型即校验 |
| 状态存储 | `rusqlite`(WAL) + 配置快照目录 | 设备策略/节点历史/测速缓存进 DB；生成的配置以不可变快照落盘，便于 diff 与回滚 |
| 编排 | `tokio::process` 调 `systemctl`/`rc-service` + `nft` / `ip` / `sysctl` / `sing-box check` | 不碰 systemd D-Bus；服务层差异只集中在 `gen::init` 生成的制品与启用命令上 |
| 面板 | Vue 3 + Vite，产物 `rust-embed` 塞进单二进制 | 与 MosDNS-T 的部署手感一致：一个可执行文件 = 全套 |
| 目标构建 | `x86_64-unknown-linux-musl` 静态二进制（Alpine 与 Ubuntu 都用它），构建方法见 §2.1 | 替换文件即升级/回滚；musl 静态链接免掉两类发行版的 glibc 版本差异 |
| 目标系统 | Alpine（首选）+ Ubuntu/Debian | Alpine 用 OpenRC、无 systemd-resolved、busybox 工具集；判据与坑见 §6 |

*这一张表是原稿，2026-10-06 有三行已被"零新依赖 + `--offline` 构建"这条定案推翻并落了地*：
控制面没有 `axum`/`tokio`，HTTP/1.1 是手写的（§7.2 第 5 条）；状态存储没有 `rusqlite`，
期望状态是一份 0600 的 JSON（§7.3），修订与快照是目录（§5）；面板不是 Vue 3 + Vite + `rust-embed`，
而是三份 `include_str!` 进二进制的 vanilla 文件（§7.2 第 8 条）——部署手感那一条（一个可执行文件 = 全套）
保留，实现方式换了。留这张表是为了说明"为什么现在没有构建步骤"，不是还没做。

### 2.1 交叉构建：Windows 宿主机直接出 Linux 件（2026-09-30 实测）

不需要 VM、不需要容器、也不需要任何 C 工具链——依赖只有 `serde`/`serde_json`/`base64`/`thiserror`，
全是纯 Rust，musl 目标的 sysroot 自带 `libc.a` 与全套 CRT 对象，缺的只是一个链接器，而
`llvm-tools` 组件里的 `rust-lld` 正好能当它用：

```sh
rustup target add x86_64-unknown-linux-musl
rustup component add llvm-tools
SR=$(rustc --print sysroot)                     # Windows 形式的路径，反斜杠要保住
LR="${SR}\\lib\\rustlib\\x86_64-pc-windows-gnu\\bin\\rust-lld.exe"
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$LR" \
  cargo build --release --target x86_64-unknown-linux-musl --bins
```

产出的件实测（`target/x86_64-unknown-linux-musl/release/zboxd`，1.85MB）：

- ELF64、`e_machine=62`、`e_type=3`（ET_DYN，即 static-PIE），程序头里**没有 `PT_INTERP`**
  ——真的不依赖任何动态库，所以同一个文件在 Alpine 与 Ubuntu/Debian 上都是同一份产物。
  这与 §6 第 1 步对 sing-box 二进制"必须按 libc 选资产"是两回事：那条针对的是**别人发的** glibc
  动态件，我们自己的控制面没有这个问题。
- scp 进 guest 直接 `chmod +x` 就能跑；busybox `ldd` 对它只回一行 `/lib/ld-musl-x86_64.so.1`
  （那是 static-PIE 的默认解释器提示，不是依赖）。

## 3. 目录结构
现状（`src/` 内均已实现并有单测覆盖；标 *待建* 的是后续任务）：

```
src/
  lib.rs             控制面库
  bin/zboxd.rs       CLI 入口：install / apply / rule-sets / enable-hijack / disable-hijack /
                     rollback / status / doctor / serve / panel-token
                     （`--bind`/`--port` 只对 `serve` 有意义，用在别的命令上直接报错）
                     `--root`/`--prefix` 收绝对路径：这两个值会原样进 WorkingDirectory 与 -D
                     install 的顺序是"制品 -> 规则集 -> 首份配置"，反了就是把冷启动扔给网络
  model/
    config.rs        DesiredState = 期望状态（network/ports/dns/policy/nodes/groups/devices/rule_sets）
                     面板、校验器、生成器只认这一个结构；tag 常量也定义在这里
    node.rs          Node：Stream(vless|vmess|trojan|shadowsocks) / Quic(hysteria2|tuic)
                     两层枚举把"QUIC 没有 transport"这类非法组合挡在类型层
    device.rs        LAN 设备与按设备策略（Global|Direct|Proxy|Bypass）
  ingest/
    uri.rs           单节点 URI 解析（vless:// trojan:// ss:// vmess:// hysteria2:// tuic://）
    subscription.rs  订阅正文解析（base64 列表 / 明文链接列表）+ 订阅信息头
    ruleset.rs       在线规则源 -> sing-box 规则集 JSON -> `.srs` 编译计划
                     四种来源（AdGuard 文本 / 域名列表 / 规则集 JSON / 现成 .srs），纯函数
                     空源守卫 + tag 路径校验；抓取与编译执行在 svc 层 *待建*
    *待建*           HTTP 抓取、节点历史
  gen/
    mod.rs           snapshot_files()：一次下发所需的整套文件
    singbox/
      mod.rs         Model -> **单个** JSON 文档（1.14.2 顶层没有 include/conf-directory）
      dns.rs         typed server + fakeip server + dns.rules
      inbound.rs     direct(:53) / tproxy(:9888) / redirect(:9887) / mixed(:8888) / socks
      outbound.rs    节点 -> outbounds，含 selector/urltest 与特殊出站
      route.rs       分流阶梯 + rule_set 引用 + final + default_mark（fwmark 常量在此）
    nft.rs           nftables 劫持层（自证模板，非官方文档）
    netstate.rs      ip rule / ip route / sysctl（同上）；sysctl_pairs() 是内核键的唯一来源
    init.rs          服务制品：数据面 + **面板**两份（systemd 单元 / OpenRC runscript）
                     + /etc/sysctl.d 持久化文件；面板的命令行只有 panel_argv() 一处定义
                     InitSystem::detect() 判不出来就返回 None，svc 层不许猜
  doctor.rs          一键诊断：期望状态 vs 现场读数 -> Finding(code, severity, expected, actual, fix)
                     读数缺失一律不判；严重程度随期望状态变化（见 §5.1）
  apply/
    store.rs         不可变快照、保留 N 份、last-good 指针；content_hash()/digest_of() 是快照指纹
    engine.rs        生成 -> 校验 -> 提交 -> 重载 -> 探测 -> 失败回滚
                     指纹与 last-good 相同则 ApplyOutcome::Unchanged，一步都不做
  svc/               真正动机器的那一侧，全部收在这里（判据来自 §11.1/§11.6/§11.7 实测）
    runner.rs        Runner trait：exec（跑完看 rc）、exec_stdin（rule-set compile 的源 JSON 走管道，
                     不留可预测的临时文件）、until_started（同时读 stderr 重定向文件与内核自己的
                     log.output，等 `sing-box started` 再杀）；
                     System 是目标机实现，Scripted 是测试替身
    validate.rs      check（带 -D）+ 空 inbounds 预跑，用独立状态目录，不碰现役 cache.db
                     预跑刻意不清 `log` 段：那条判据写在 log.output 指的文件里，基准同 -D
    ruleset.rs       抓取（curl/wget 探测）-> 编译到 <tag>.staging.srs -> 两级验收 -> 顶掉现役产物
                     -> rule-set/manifest.json。未命中也 rc=0，所以内容级判 stdout；
                     单条源挂了不覆盖旧产物、也不把整批判死（Report::notes）
    hijack.rs        nft -c 预检 -> netstate.sh -> nft -f；卸载只删自己的对象
    observe.rs       doctor::Reading 的七项现场读数；取不到一律 None
                     `listening()`（ss -lntu）同时是生效性判据的唯一来源，探针不许自己 connect
    topology.rs      §6 第 5 步的网卡形状探测（ip route/addr 两条命令）+ fill() 的覆盖规则：
                     人写过的 network 绝不改，判不出就拦路，只有 `ip` 跑不动才降级继续
    mod.rs           Svc：actions() 三闭包、reload/probe/install/observe/ruleset，
                     init 系统差异只在 control_command 一处收敛；
                     reload 前轮转内核日志（LOG_CAP），probe 靠轮询套接字表判生效
  api/               面板的接口层，五块：mod.rs（请求 -> 响应的处理核心 + 五个路由）、
                     token.rs（机器生成的高熵令牌，门排在路由之前）、http.rs（手写 HTTP/1.1 线格式）、
                     serve.rs（绑定地址的策略 + 单线程顺序 accept）、assets.rs（静态表：三条字面量路径）
assets/              面板本体：panel.html / panel.css / panel.js，`include_str!` 进二进制，
                     没有构建步骤（原来的 `web/` Vue 3 那一档被零依赖档替掉，见 §2 的订正）
deploy/ *待建*       一键装/卸脚本；服务单元与 sysctl 文件由 gen::init 生成，不在脚本里手写
docs/
  ARCHITECTURE.md
  reference/singbox-1.14.2-schema.md   生成器的字段权威，改动前先看它
  reference/localdns-fj945-survey.md   成品包勘察：采纳/抛弃了哪些机制及理由
  reference/mosdns-fakeip-bundle-survey.md  fakeip 分流法三包勘察：OpenRC 假设被推翻、
                     参考包跑的是 MetaCubeX 分支而非官方 sing-box、双 mark 的必要性
  reference/mosdns-t-mapping.md    MosDNS-T → ZBox 映射报告（Task #1）：三条结构性不对称、
                     分流顺序逐条映射、状态落点与 API 面、绝不抄清单；#6/#7 与 api/ 的输入
```

原始设计里 `model/policy.rs`、`model/dns.rs`、`model/subscription.rs` 没有单独成文件：
它们都是"期望状态"的一部分，拆开会逼着生成器跨模块借类型；`apply/snapshot.rs` 落地时叫
`store.rs`。

## 4. 分流模型

sing-box 内置 DNS + `rule_set`（.srs 视为**不透明文件**：ZBox 只负责下载、校验、命名、引用，解读交给内核，Rust 侧不写 .srs 解码器）。

规则序自上而下，命中即定，`final` 兜底（与 `gen::singbox::route::route_json` 一致）：

```
1.  dns-in 来的包                    -> hijack-dns     （交给内置 DNS）
2.  tproxy/redirect 上 protocol=dns  -> hijack-dns
3.  设备级策略（Direct/Proxy）        -> 对应出站        必须先于地理分流
4.  action: sniff                                   给后面的域名规则供数据
5.  私网/组播/局域网 CIDR            -> direct
6.  广告规则集                        -> reject          ★ 仍排在用户名单之前，见下
7.  用户域名名单（一条有序表）        -> 每行自带处置：reject / direct / final 的代理组
8.  geosite-cn / geoip-cn            -> direct
9.  protocol=bittorrent              -> direct
10. geolocation-!cn（non-cn）        -> 代理组        仅当 final 确实是代理时输出
11. fakeip 段 ip_cidr 短路            -> 代理组        两处都是 final 的同一个 tag，见下
12. final                            -> proxy 或 direct（面板一键切换全局模式）
```

**★ 一条有序名单（选项 C，2026-10-05 落地）**：`Policy.domain_rules` 是
`Vec<DomainRule>{pattern, action}`，`action ∈ {direct, proxy, block}`，**行序即优先级**，
与引擎的终止语义同构。v1 的三档扁平名单（`direct_domains` / `proxy_domains` /
`blocked_domains`）没有"跨档谁赢"这个概念，面板只好替引擎解释一层看不见的优先级；
合成一条之后，"谁在前"直接就是用户在面板上看到的顺序。旧文件由状态层 v1→v2 迁移：
按拦截 → 直连 → 代理的档序拼接，与 #18 定案的结论逐条等价。

**唯一一处行为变更**：名单里的"代理"行现在排在 geosite-cn / geoip-cn / bittorrent **之前**
（v1 是代理档排在它们之后）。理由——显式名单压过地理默认：用户既然点名某个域名要走代理，
就不该因为地理库把它判成国内而静默走直连。其余关系（广告在名单前、私网与设备级在名单前、
拦截与直连互斥由行序决定）与 v1 结论一致。
这条变更已在 guest 上真验过，两层都有：DNS 层测的是 fakeip 侧，两个对照组（`www.qq.com` / `test.cn`）
照旧走国内上游（§11.9）；**连接层**测的是同一个 `www.baidu.com`——它在真 `geosite-cn.srs` 里，
本应被第 8 步送直连，实测日志是 `outbound/vless[vless-reality]: outbound connection to www.baidu.com:80`
（逐条判定、原始日志与另外三级"观测不到"见 §11.10）。

**梯子里三级在连接层观测不到，不是 bug 但必须知道**（§11.10，对现役内核跑 `apply` 那一轮量出来的）：

- 第 10、11 步的出口与第 12 步 `final` 是**同一个 tag**（`route.rs:159/198/205` 都取 `final_tag`），
  所以"命中了 non-cn / fakeip 段"与"谁都没命中、落到 final"结果完全一样，连接层无法鉴别。
  它们的真实作用只是省一次 geo 查询和把意图写在配置里。若将来要让它们有意义，
  得先允许"名单行指定自己的策略组"或"final 与代理默认分离"——那是面板的题，不是引擎的题。
- 第 5 步里的 `lan_cidr` 一条：局域网段几乎总是 RFC1918，被同一句里的 `ip_is_private` 遮住；
  而且**同网段目的地的包在桥上二层交换，根本不过路由器**（实测 `lan1` 打 `10.99.0.3:80`
  一条日志都没有）。这对用户是好消息——局域网内互访天然不被接管；
  对测试是提醒：造"命中 `lan_cidr`"的探针必须先把目的地做成要经默认网关才到得了的地址。
- 名单里动作为 direct 的行，若该域名同时在 geosite-cn 里，就与第 8 步同出口，也不可鉴别
  （所以 `route.rs:528-749` 那组索引断言不能因为"实测过了"就删）。

**★ 拦截优先（#18 定案，2026-10-05）**：广告规则集必须排在**整条**用户名单之前，
并且 `dns.rules` 同向（reject 早于直连上游路由）。理由：sing-box 的动作是终止语义，一个域名命中多档
时只有顺序说了算；若路由层把"既在黑名单又在直连名单"的域名送直连，而 DNS 层回绝它，
客户端有缓存、走 IPv6 或命中静态记录时那条"该被拦的"照样连出去——拦截就成了会漏的承诺。
代价是排在拦截行之前的直连/代理条目静默失效，所以 `doctor` 有 `block_never_fires_behind_a_pass`
（该拦的没拦住 = 漏承诺）与 `domain_rule_never_fires`（这一行永远不会命中 = 只是废行）两条 Warn
把它分开点名（覆盖关系只用实测过的 `Suffix`/`Exact` 形状判断，`Keyword`/`Regex` 参与的重叠不报，
见 schema 文档 §4 末）。设备级 Direct（第 3 步）仍在一切拦截之前，这是刻意的：它的语义是
"这台设备不接管"，不该由名单管辖。
这两句都在真机上落到了证据：广告桩里放 `www.ads.stub.test`、名单里给同一域名写**直连**，
实测仍是被拦（0 秒断流、没有任何 outbound 行）；而设备级直连一加上，同一个客户端打
`www.baidu.com` 就从 `outbound/vless…` 变成 `outbound/direct…` 且页面真下载成功（§11.10 二）。

两条实现约束：
- **同一条规则里的多个匹配字段是"与"**，所以一批域名必须按 `domain` / `domain_suffix` /
  `domain_keyword` / `domain_regex` 拆成多条规则（`ordered_domain_matchers`）。合并只在
  **相邻 + 同匹配型 + 同处置**之间发生——处置一变就断开，否则一条规则只能带一个 `outbound`。
- 规则引用的每个 `rule_set` tag 都必须在 `route.rule_set[]` 里定义过，否则 `sing-box check`
  整体失败。ZBox 因此在生成期就检查，缺规则集直接报错而不是下发。

还有一条**没做**的：参考实现允许一个域名同时命中多条名单、把所有命中标签 OR 合并、再由后置 sequence
决定走向。sing-box 1.14.2 的 `route`/`dns.rules` 动作是**终止**语义（§11.7 实测），所以那套"候选 + 后置决策"
在这里既无法表达也不需要——优先级只能、也只需要靠生成顺序体现。代价与后果见
`docs/reference/mosdns-t-mapping.md` §1.3（含"面板不能声称显示了全部命中"这一条）。

正因为顺序就是唯一的优先级，上面这 12 级有一组**索引断言单测**守着（`route.rs:528-749`，
#17）：断言的是先后，不是"这条规则存在"。它们钉住的交叉关系是：名单内部行序决定谁赢、
geosite-cn 压过 non-cn、**用户名单压过地理默认**（选项 C 那条变更）、广告库压过名单、
私网与局域网段压过名单、拦截与设备级直连压过 fakeip 段捷径、以及**同一份名单在两层展开后顺序一致**
（#18）。改阶梯时这组红了就是有人在无意识地改分流语义。

三类判定必须分开，否则规则一改就全量重下：

- **内置地理规则**：远程 `.srs`（在 SagerNet 的 **`rule-set` 分支**上，不是 `releases/latest/download`
  —— 那个路径只有 `.db`，实测 404，见 schema 文档 §4），下载必须走带 tag 的 `http_clients` 条目
  （§11.3）；失败时用上次缓存的文件而不是清空。
- **本地覆盖**：用户在面板手工增删的域名/IP，存 DB，生成时展开成 `domain`/`domain_suffix`/`ip_cidr` 规则，优先级高于内置。
- **策略组**：`selector`（手选）/ `urltest`（自动测速），订阅刷新时保留用户对组的选择。

## 5. 下发与安全模型（本项目的核心）

`apply` 是唯一能改变网络状态的路径：

```
0. 快照指纹（路径+内容全量 FNV-1a）与 last-good 相同 -> ApplyOutcome::Unchanged，直接结束
   面板每改一次设置都会走一遍下发，而重载会掐断所有在用连接；没变就不该动内核
1. Model -> gen::snapshot_files()，写入临时目录 /var/lib/zbox/staging/<revision>/
   （config.json + zbox.nft + netstate.sh + netstate-down.sh 四份，必须原子地一起生效）
2. sing-box check -c <config.json> -D <状态目录>  # 语法/引用预检，便宜；**-D 必须带**
   sing-box run（inbounds 置空 + 独立临时状态目录）# 真正的门槛：check 放行 ≠ 起得来
   #   1.14.2 实测有两条写法 check rc=0 而 run FATAL（detour:"direct"、远端规则集缺显式
   #   http_client）。空 inbounds 的预跑不占任何端口，所以现役内核不用停，
   #   DNS 初始化/规则集下载/出站初始化都会走一遍。详见 schema 文档 §11.1、§11.6、§11.7。
   #   相对路径（`rule_set[].path`/`initial_path`/`cache_file.path`）一律以 -D 为基准，
   #   -D 缺省是 CWD，所以不带 -D 的 check 会报"没有那个文件"这种假故障（§11.7）。
   #   生成的配置带 `log.output`（裸文件名，落在 -D 下），所以"起来了"那行不在 stderr 而在
   #   内核自己的日志里；门两个文件都读。`check` 从不打开这个路径（目录不存在也 rc=0），
   #   预跑是它唯一能被验证的机会 —— schema §11.8.3。
3. nft -c -f <nft 脚本> + sh -n <netstate.sh> # 语法预检，别等劫持层半生效才发现
4. 原子切换：conf 目录符号链接 + systemd-notify reload
   #   快照目录**不可变**，而内核要往里写 cache.db，所以运行目录 = 可写状态目录：
   #   切换时把快照（含 rule-set/*.srs）复制/挂进 <state>/<revision>，-c 用绝对路径。
5. 健康探测（超时默认 15s）：
   - sing-box 进程存活且未 panic
   - tproxy/mixed 端口在听。**判据是读 `ss -lntu` 的套接字表，绝不 `connect()`**：
     本地连自己的 TPROXY 端口会被 `IP_TRANSPARENT` 按原始目的地二次路由回同一个入站，
     自己喂自己无限递归（实测 350MB 日志 / 约 6MB/s / 108 万行，只能靠 stop 掐断）。
     看 UDP 是因为 DNS 入站的 :53 在 ss 里是 `UNCONN` 不是 `LISTEN`；读不到套接字表
     就报"判不了"，不把"没读到"当"没起来"（schema §11.8.4）。
   - 重启前先把内核日志按大小轮转掉（`LOG_CAP`，实测 append 模式 + 1.14.2 的 LogOptions
     没有任何轮转字段）：轮不动（状态目录不可写）就是下发失败，而不是留着写满磁盘。
   - **DNS 侧的判据是"答案变了"而不是"境外站打不开"**：拿客户端向 8.8.8.8 的查询做样本，
     劫持生效时答案是本机 DNS 引擎给的（且日志有 `dns: match[..] => route(..)`），
     未生效时是真实 8.8.8.8 的答案。实测过反例：VMnet8 里把劫持层和内核全关掉，
     google 也一样打不开 —— 所以"境外域名失败"单独不构成任何证据（schema §11.7）。
   - 经直连出口能取到 cn 域名响应
   - 有可读的判决轨迹：日志里 `inbound/tproxy ... connection to <原始目的地>` 与
     `outbound/direct|<组>` 成对出现，这才证明 TPROXY 保住了目的地
6. 任一失败 -> 自动切回 last-good 快照并重载，同时把面板置为"上次下发失败"状态
7. 全程写审计日志：谁、改了什么、生成前后 diff、结果
```

- 快照不可变，保留 N 份（默认 10），`zbox rollback [revision]` 可回到任意一份。
  修订号不给就回到 last-good；目录名是 `<毫秒>-<内容指纹>`，所以同一份内容回滚两次会留下
  两个修订号、指纹后缀相同（实测：eth0 基线被两次下发成两个修订，后缀都是 `681b742c…`）。
- **外部规则源必须经编译才进数据面**：`ingest::ruleset::prepare()` 把来源归一化成 sing-box
  规则集 JSON，`svc::ruleset::Sync` 执行 `sing-box rule-set compile stdin -o <状态目录>/rule-set/<tag>.srs`。
  编译先到 `<tag>.staging.srs`，**两级验收通过才替换旧 `.srs`**：文件合法性判 rc，内容命中判
  stdout——实测 `rule-set match` 未命中也返回 rc=0，只判 rc 等于把"没编进去"当通过。
  解析出 0 条规则视为来源异常，同样拒绝覆盖——下载抖动不该把名单清零。
  落地事实（来源、格式、条数、字节数、落地时刻）记在 `rule-set/manifest.json`，按 `update_interval` 到期才重抓。
- **tag 与落点绑死**：编译产物、清单的键、配置里的 `initial_path` 全是 `rule-set/<tag>.srs` 这一条
  公式（`model::config::rule_set_product`）。上游文件名不参与命名——内置的 `non-cn` 在上游叫
  `geosite-geolocation-!cn`，照抄会让 sing-box 去读一个 ZBox 从不写的路径。`DesiredState::validate`
  钉住这三件事：tag 是安全文件名、tag 不重复、Remote 的 `initial_path` 恰等于该公式。
- **卸载 = 干净回退**：`zbox uninstall` 删 nft 表、删 ip rule、停服务、恢复 DNS，不留残留（旁路由最怕"卸载了还上不了网"）。
- 破坏性操作（下发、卸载、重启内核）在面板上都要走确认气泡，且默认展示即将生效的 diff。

### 5.1 一键诊断（`doctor`）

参考实现的《故障排查》文档有效的原因不是条目多，而是它把"配了但没生效"拆成了
**期望值 vs 实际值**的判据。ZBox 把它做成代码（`src/doctor.rs`），面板一个按钮跑完：

- `self_check(state)` 只看期望状态：入站端口互不相同、fakeip 段是合法 CIDR 且**不与局域网段
  相交**（自己做 IPv4 掩码运算，`model::validate` 只查形状）、最终出口是代理却没有境外 DNS
  上游（= DNS 泄漏，等于没分流）、DNS 上游 `via` 指向不存在的出站，以及**有序名单里的自相矛盾**：
  逐行拿它和排在它前面的行比覆盖关系，被盖住的行按处置分成两条 Warn——该拦却被放行是
  `block_never_fires_behind_a_pass`（漏承诺），其余只是 `domain_rule_never_fires`（一行永远
  命中的废条目）。前者必须单独报，因为它意味着用户的拦截意图静默失效。
- `inspect(state, reading, layout)` 比对现场读数：`ip_forward`/`rp_filter`、nft 表与 divert
  规则是否存在、入站是否真在听、默认路由网卡是否与劫持网卡一致。每条附带可粘贴的命令，
  sysctl 命令的键值取自 `netstate::sysctl_pairs()`，与下发脚本同源。
- v6 那半边按档位门控，且**只报证过的那一层**：`takes_dns()` 两档报
  `dns_port_not_listening_on_ipv6`（DNS 靶子 = oxidns 绑 `network.lan_address6` 的那一对，
  生成物里是 `udp_in6`/`tcp_in6`；**不是** `[::]:ports.dns`，理由见 P2c），`takes_connection()`
  一档另报 `ipv6_nft_table_missing` / `ipv6_divert_rule_missing` / `ipv6_forwarding_off`，
  `off` 一条都不报。转发那条的 fix **成对给** `all.forwarding=1` + `all.accept_ra=2`，
  键值同样与脚本同源（`netstate::v6_host_pairs()`，测试钉住它必须是
  `host_sysctl_pairs(DualStack)` 的末尾两条）；单给前者会把人引进"自己的 v6 默认路由整臂消失"。
  dae 的 `tproxy_port` 在 v6 上绑不绑仍未实测，所以它不许出现在任何判据里。三档实测回执
  见 `docs/reference/ipv6-kernel-measured.md` §7。

两条硬约束：`Reading` 全字段 `Option`，**"没读到"绝不当成"读到了不符合"**（否则在 Windows
开发机上会刷一屏假故障）；同一条现象的严重程度随期望状态变——基线（全直连、还没开劫持）下
"nft 表不存在"只是提示，最终出口已指向代理时它就是"配置下发成功、但什么都没代理"这个最难查
的形态，判 Fatal。

## 6. 旁路由部署时序

Alpine（首选）或 Ubuntu/Debian，单网卡旁路或双网卡均可，但**不做主路由、不发 DHCP**。
两类 init 系统的差别由 `gen::init` 吸收，svc 层只多做一件事：`InitSystem::detect()` 判定，
判不出来就只打印命令清单让人工确认，**不许猜一个往机器上写**。

```
install:
  1. svc::provision 按"架构 + libc"把钉死版本的官方 sing-box 落地到 Layout::singbox_bin：
     `uname -s/-m` + `ls /lib /lib64` 判目标（Alpine -> `-musl` 资产，Debian/Ubuntu -> `-glibc`），
     判不出就停而不是退到裸名包；**验收是真跑 `<落地前的文件> version` 比对 1.14.2**，
     不是校验和（release 里没有 `.sha256` 资产，实测核对过资产表）。
     主路是离线资产（`--asset <本地包>`，scp 过去即可）；没给 `--asset` 才按判出来的目标现抓，
     而这条在 guest 里实测撑不住 31MB（schema §11.8.1），所以抓取失败时报的是"拷过来再用 --asset 指"，
     wget 那侧的空闲上限也特意压到 20 秒以便早失败。
     现役二进制已报 1.14.2 时这一步一个外部请求都不发。zboxd 自身由包管理器/手工部署，不在这里装。
     顶换用同目录 rename：实测（schema §11.8.2）正在跑的 sing-box 握着旧 inode 照常服务、日志零新增，
     所以这一步**不停服务**——新内核真正生效仍然靠第 7 步的 restart。
  2. gen::init::artifacts() 写服务制品（绝对路径，装一次，不进每次下发的快照）。
     **一台机器上是两个服务**：`zbox`（数据面，sing-box）与 `zbox-panel`（面板，`zboxd serve`）。
     拆开而不是让面板 supervise 数据面，是因为面板必须在数据面坏着的时候也起得来——那正是
     需要它的时刻；两份制品之间没有任何依赖关系（单测 `the_panel_service_is_never_chained_to_the_data_plane`）。
     - systemd：/etc/systemd/system/zbox.service（Restart=on-failure，**没有 ExecReload**
       ——sing-box 不原地重载，reload 语义在 svc 层映射成 restart）
       与 /etc/systemd/system/zbox-panel.service（`ExecStart` 由 `panel_argv` 这一个定义生成，
       `Restart=on-failure` + `RestartSec=5` + `[Unit]` 里的 `StartLimitIntervalSec=1800` /
       `StartLimitBurst=12`；stderr 天生进 journal，所以它没有 `error_log` 的对应物。
       **这一份的落点从未在真 systemd 机器上执行过**，见 §8）
     - OpenRC：/etc/init.d/zbox（`supervisor=supervise-daemon` + `respawn_delay/ max/ period`；
       参考包只有后台化、进程死了没人拉，这条是补的。**补的方式实测推翻过一次**：
       `respawn=true` 与 `command_background=true` 在 Alpine 0.55.1 上完全惰性（`openrc-run`
       里 grep 不到 respawn，`start-stop-daemon` 没有 `--respawn`），只有 supervise-daemon
       生效 —— 见 schema §11.8.5）
       与 /etc/init.d/zbox-panel（同一个监督者、同一份 `panel_argv`、`pidfile=/run/zbox-panel.pid`
       与数据面那份分开——同名会让两个服务互相能停掉对方；重试预算比数据面宽，
       `respawn_max=12` 对 5，因为面板最可能的启动故障是临时的：局域网网卡此刻还没地址，
       `resolve_bind` 就拒起。两份 runscript 都在 `start_pre()` 里 `mkdir -p <状态目录>`，
       这一句是 2026-10-06 实测逼出来的，见下面第 3 步末尾）
     - 两者共用：/etc/sysctl.d/99-zbox.conf（**只落整机级的键**，绑网卡的那条留在 netstate.sh
       里当场用 `sysctl -w` 设——开机读这个文件的是 busybox 的 `sysctl -p`，对不认识的键
       静默跳过且 rc=0，网卡名一漂就等于悄悄少一条，见 schema §11.8.6）
     **`--prefix <DIR>` 的边界（2026-09-30 实测后定）**：制品路径是"前缀 + 原绝对路径"，
     而第 3 步那些启用命令**按服务名寻址**（`rc-update add zbox default`），没有任何参数可以把
     目标挪出 `/etc`。所以带 `--prefix` 的 install 只写制品（`svc::Svc::stage`），把本来要跑的
     命令原样打给人看，然后停在这里——规则集与第一份配置都要对着一个真在跑的服务，试跑状态下
     做它们只会得到"服务不存在"那种读不懂的错。
  3. 启用：systemd 走 daemon-reload + `enable zbox` + `enable zbox-panel` + restart 两个服务；
     OpenRC 走 chmod +x（**两份 runscript 都要**）、`rc-update add zbox default`、
     `rc-update add sysctl boot`、`rc-update add zbox-panel default`，然后 restart 两个服务。
     顺序是"两个都先登记、再都拉起"——登记是幂等的改名工作，拉起才有副作用。
     （*2026-09-30 实测订正*：Alpine 3.21.3 基础装机就把 `sysctl` 放在 boot 运行级别里，
     这条是空操作（打印 `already installed in runlevel 'boot'; skipping`、rc=0）。
     原来写在这里的"不加则重启后不生效"是错的，见 schema §11.8.6。留着只当有人手工删过的保险）
     *2026-10-05 实测订正（原话"这条 restart 在首次安装时必然失败一次"是错的）*：它根本**不失败**。
     `rc-service zbox restart` 返回 rc=0、打印 `[ ok ]`，`status` 连着 24 秒报 `started`，但活着的
     只有 supervise-daemon 一个进程、监听数 0；worker 在 5 次重试里一直 FATAL（`current/config.json`
     不存在），约 32 秒后停在 `stopped`（`status` rc=3）。而且**现场查不到那 5 次 FATAL**：内核的日志
     路径写在 config 里，读不到配置就没有日志器（`sing-box.log` 全空），supervise-daemon 只往 syslog
     记子进程的命令行（`grep -c FATAL /var/log/messages` = 0）。空档正好盖在第 4/7 步上，看上去就是
     "装完了但内核不干活"。所以现在 `Svc::install` 带 `start_service` 开关：`<current>/config.json`
     不存在时把 `init.start_command()` 那一条按整条 argv 摘掉（systemd 三条、OpenRC 四条，不按下标猜），
     数据面此刻不拉起，由第 7 步的 apply 启。**面板那一条不摘**，见下面 2026-10-06 那两段。
     见 schema §11.8.9。
     *仍未走到*：runscript 里 `start_pre()` 那 30 秒等默认路由的分支——现场每次都有默认路由。
     **同源缺口当天就补了**：OpenRC 的 runscript 变量 `error_log`（0.55.1 的
     `/usr/libexec/rc/sh/supervise-daemon.sh:31-32` 把它拼成 `--stderr`）现在被写进制品，
     子进程 stderr 落到 `<状态目录>/sing-box.stderr.log`——正常启动 0 字节、异常每次 respawn 追加一行，
     所以不轮转。systemd 侧不接（stderr 进 journal），`doctor` 的 `inbound_not_listening`
     两个处置命令都给。见 schema §11.8.9 第七节。
     *另记一条读数时机*：`restart` 之后立刻 `status` 会读到"服务活跃: 是 / 在听端口: 22"，
     因为内核还没绑端口（实测约 1-2 秒）。判"起来了但没在听"之前先看那个 stderr 文件。
     **面板自己那一份制品落地于 2026-10-06（task #31），同一天被实测推翻一次**：
     - 制品本身成立。guest 上以 `install --prefix /tmp/zbox-stage --root /tmp/zbox-panel18
       --zboxd-bin /root/zbox-rehearse/v15/zboxd` 写出三份制品（`etc/init.d/zbox`、
       `etc/init.d/zbox-panel`、`etc/sysctl.d/99-zbox.conf`）、7 条待执行命令，两份 runscript
       `sh -n` rc=0；把 staged 那份装进 `/etc/init.d` 后 `rc-service zbox-panel start` 的
       面板真在听 `192.168.219.128:9090`，带 `X-ZBox-Token` 的 `GET /api/state` 回
       `"source":"missing"`（那一刻磁盘上没有 `state.json`），`GET /` 静态面 200，
       不带令牌的同一个 URL 401。`kill -9` worker 后 1 秒内没有 worker、7 秒时监督者已拉起
       新的（pid 变了）并重新在听。全程没跑过一次手工 `zboxd serve`。
     - **被推翻的是那句"面板不需要任何配置就能开门"**：v14 那份制品把 `error_log` 写进
       `<状态目录>/panel.stderr.log`，而 supervise-daemon 是在 **fork worker 之前**打开这个文件的
       ——新机器上 `<root>` 还不存在（它是面板写令牌时才顺带建出来的），于是服务表现为
       `status: started`、监督者活着、9090 空听、`panel.stderr.log` 根本没被建出来，
       连"为什么起不来"都无处可查。现场证据是 `ls /tmp/zbox-panel17` 报 No such file
       or directory 而 `pgrep -af` 只有一行 supervise-daemon；我手工跑了一次同一个 serve，
       它把目录和令牌都建了出来，随后监督者的下一次 respawn 就成功了——这一条同时证明了
       根因是目录、不是面板本身。修法：两份 runscript 的 `start_pre()` 里 `mkdir -p <状态目录>`
       （数据面那份是同一个潜伏故障：`<root>` 被 prune 掉后它也会变成"started 而无声"）。
       **这一句要在数据面上执行很容易**：那份 staged 制品的 pidfile 也是 `/run/zbox.pid`，
       真 start 就是拿现役数据面的 pidfile 开玩笑，所以这一轮只验到 `sh -n` rc=0 为止，
       机制本身由面板那一份的线上结果代证。
       用 `mkdir` 不用 `checkpath`：实测裸 shell 里 `which checkpath` 报 not found，
       它是 OpenRC 内部的 shell 函数，不在 runscript 执行时的 PATH 上。
     - 反向的那一条也验了：改端口成一个真被占用的（`--panel-port 9888`，sing-box 的 DNS 在听），
       新根 `/tmp/zbox-panel19` 事先不存在，start 后目录被建出来、`panel.stderr.log` 里
       每次尝试一行 `zboxd: 绑定 192.168.219.128:9888 失败: Address in use (os error 98)`；
       数到第 65 秒共 13 行（= 首次 + `respawn_max=12` 次重试），监督者放弃并退出，
       `rc-service zbox-panel status` 变 `stopped`、rc=3。**重试预算是有限的这件事由此有了实测数**，
       而 `status` 在这一路上一直可以报 `started`——它是监督者的状态，不是面板的状态。
     - 清理与基线：`rc-service zbox-panel stop`、删 `/etc/init.d/zbox-panel`、删 staged 目录与
       两个临时根（里面那份真 `panel.token` 留在 guest，不落宿主机）。复查 `/etc/runlevels/default`
       仍是 acpid chronyd crond open-vm-tools sshd zbox（面板从未 `rc-update add` 过）、
       `pgrep -af zboxd` 空、9090 无监听、`zbox` started、`nft list tables` 空、
       `ip -4 rule` 只有 0/32766/32767、53（tcp+udp）/8888/9887/9888/7890 照旧。**计数：宿主 295 /
       guest musl 296**（这一期 +1 条断言 mkdir 与 error_log 同源的那一条）。
  4. svc::ruleset::Sync 把声明的规则源抓回来、编成 `.srs` 落到状态目录，并写
     `rule-set/manifest.json`（来源、条数、字节数、落地时刻）。**条数对直通的预编译 `.srs` 恒为 0**，
     不是"空表"：ZBox 不解析 SRS，只做字节级顶替 + 真二进制验收（schema §11.8.9）。
     **必须排在下发配置之前**：§11.7 实测 sing-box 初始化规则集只有约 15 秒预算（10s WARN、
     ~16s FATAL），而首次安装恰恰是最可能没网的时候。抓取失败且没有可沿用的旧产物时
     install 就停在这里，而不是让内核启动时现抓。
  5. 探测上游网关/网卡，写入初始 Model      `svc::topology`（落地于 2026-10-05，现场已复抓）
     **这条已经付出过代价**：彩排用的状态里网卡名是默认的 `ens33`，而那台机器是 `eth0`，
     于是持久化 sysctl 文件里绑网卡名的那条键从写下来那天起就没生效过（busybox 的 `-p` 不报），
     `doctor` 也一直报 `lan_iface_mismatch`。见 schema §11.8.6。
     现在的规则（`zboxd detect` 打印、`install`/`apply`/`enable-hijack` 内建执行）：
     - 两条命令读形状：`ip -4 route show default`（上游网卡 + 网关）、`ip -4 -o addr show`（各卡段）。
       只看 stdout，不看合并输出。
     - 候选 = 排除 `lo`、排除 `/32`（点对点隧道给不出局域网段）后、默认路由之外**恰好一张**网卡 → 双网卡；
       一张都不剩 → 单网卡旁路由（进出同卡，实测 VM 就是这个形状）；两张以上 → **停下问人**，不排序挑一张。
     - 只认 `state.network == Network::default()`（等于没人写过）才用探测值。显式写过的一律保留，
       但名字在本机找不到时打警告——正是 `ens33` 那一类现场。
     - 唯一允许降级继续的是 `ip` 根本跑不起来（Windows 开发机、chroot）：保留现值 + 警告。
       "跑成了但判不出"永远拦路，因为那一刻机器是真 Linux，写错就是静默坏一条。
     *2026-10-05 guest 复验*：`ip -4 -o addr show` 的两种现场（单网卡 / 挂着 netns 桥）都逐字抓过，
     fixture 换成原文；`zboxd detect` 在两种现场分别给出 `eth0`（单网卡旁路由）与
     `zbox-lan0 + 上游 eth0`。留内置默认形状的 state.json 走 `install --prefix`/`apply`，
     生成的 `netstate.sh` 是 `net.ipv4.conf.eth0.rp_filter=2`、制品与快照里 `ens33` 零命中；
     显式写一个本机没有的名字则**保留 + 警告**（列出本机真实网卡）。原文与两条实操坑见
     schema §11.8.8。
  6. 生成第一份配置（空节点表 -> 全 direct，确保"装了不断网"）
  7. apply 走 §5 全流程
enable-hijack:
  8. 显式开启后才加载 nft 劫持表 + ip rule fwmark 策略路由
     （默认不劫持，给用户一个可回退的落点）
     取值：nft 打 0x1 -> table 100；sing-box route.default_mark 打 0x2 防二次劫持。
     详见 docs/reference/singbox-1.14.2-schema.md §9。
     **顺序是硬的：必须先确认内核已在听 :53/:9888，再加载劫持层**（反过来实测到的现场是
     lan1 查 DNS 直接 `Connection refused`、HTTP 全挂 —— schema §11.7）。
     卸载 `netstate-down.sh` 不回滚 sysctl（实测确认这就是当前行为），
     且只删自己的对象：同机器上别的 nft 表（如实测脚手架）实测没被碰。
     *2026-09-30 产品路径复测*：以上四条改用 `zboxd enable-hijack` / `disable-hijack`
     加载与卸载**生成出来的** `zbox.nft`/`netstate.sh`（不是手抄的脚本），netns 客户端现场见
     schema §11.8.7 —— 证据齐（DNS 进 `dns-in`、TCP 出 `inbound/tproxy ... connection to
     <原始目的地>` + `outbound/direct` 成对），卸载后 `zboxtest` 那张外来表照常留着、
     客户端照常上网。连开两次也仍然是 2 条 tproxy / 2 条 redirect / 1 条 fwmark 规则 / 1 条路由。
  9. 回滚验证：`zbox rollback` 之后必须重跑 §5 第 5 步的健康探测。
     *已实测*（§11.8.7）：回滚到任意旧修订 + 重启 + 探测全过。**这条以前是假的**——
     CLI 根本不读那个位置参数，`rollback <修订号>` 只会回到 last-good 并报告成功；
     现在按修订号回，给错时报错并按新到旧列出全部现有修订。
```

发行版相关的四处坑，写死在这里免得以后再踩：

- **sing-box 二进制按 libc 选资产，不能一律下 `-amd64`**。官方 v1.14.2 的
  `sing-box-1.14.2-linux-amd64.tar.gz` 实测是 **动态链接 glibc**（`file` 报
  `interpreter /lib64/ld-linux-x86-64.so.2`，包里还带 11MB 的 `libcronet.so`），在 Alpine 上
  直接跑不起来。Alpine 必须用 `-linux-amd64-musl.tar.gz`，Ubuntu/Debian 用 `-glibc` 或裸名都行。
  所以下游抓取逻辑的输入不是"架构"而是"架构 + libc 类型"，libc 由 `/lib`、`/lib64` 里有没有
  `ld-musl-*`/`ld-linux*` 判（实测 Alpine 3.21.3 有 `ld-musl-x86_64.so.1` 且没有 `/lib64`）；
  判不出来就停，不退到裸名包。落地实现见 `src/svc/provision.rs`。
- **musl/busybox**：生成脚本只能是 POSIX sh——`sed` 没有 `-P`、`[[:space:]]` 代替 `\s`、
  `kill -TERM` 代替 `kill -SIGTERM`；`ip rule`/`ip route`/`nft` 属于 iproute2/nftables 包，
  Alpine 基础镜像里**默认没装**，安装步骤要 `apk add iproute2 nftables`。
- **QUIC**：virtio/vmxnet3 虚拟网卡上 Go 的 QUIC GSO 分片会出问题，服务制品里统一
  `QUIC_GO_DISABLE_GSO=true`（DoH3/Hysteria2 首当其冲）。
- **DNS 端口占用**：Ubuntu 上是 `systemd-resolved` 的 53 存根（参考实现里
  `isSystemdResolvedStubDisabled` 就是这个坑）；Alpine 基础系统没有 resolved，但可能装着
  dnsmasq。ZBox 要检测占用者并在面板里给出一键处置，而不是假设某个服务存在。

FakeIP：MVP 允许在 `direct 域名解析` 与 `fakeip` 两种模式间切换，面板"DNS 模式"处成对选项；
生成器据此在 `dns.servers[]` 里放/去 `type:"fakeip"` 服务器，并在 `route.rules[]` 末尾补一条
fakeip 段 `ip_cidr` 短路（见 `gen::singbox::route`）。

## 7. 面板信息架构

依据 `docs/reference/mosdns-t-mapping.md`（Task #1 的产出，含逐条 `file:line` 与"绝不抄"清单）写实。

### 7.1 页面划分

单页 + Tab 状态（**不引入 vue-router**，参考实现的这条判断在嵌入式 WebView 里成立，见映射报告 §0/§6）：

| Tab | 子 Tab | 编辑的是 `DesiredState` 的哪一块 |
|---|---|---|
| 概览 | — | 只读：`svc::observe` 读数 + `doctor` 结论 + 修订历史 |
| 订阅与节点 | 订阅源 / 节点 / 策略组 | `nodes[]` `groups[]`（`ingest::subscription`）|
| 分流策略 | 域名名单（可排序，每行选处置）| `policy.domain_rules[]` `policy.final_outbound` |
| DNS 上游 | 上游 / fakeip / 策略 | `dns.domestic[]` `dns.overseas[]` `dns.fakeip` `dns.strategy` |
| 规则集 | 内置 / 在线源 | `rule_sets[]` + `svc::ruleset` 的清单 |
| 查询日志 | 实时 / 抓取窗口 | 只读，数据源见映射报告 §6 |
| 设备管理 | — | `devices[]`（`DeviceMode`）|
| 系统 | 服务 / 端口 / 诊断 / 下发历史 | `network` `ports` + `apply` 的修订与回滚 |

前端语法沿用模型层已有的约定，不另发明：`+后缀` `~关键词` `/正则/` 裸名=精确、`.` 开头也按后缀、
`#` 注释（`src/model/config.rs:277-298`）。分流策略页因此是**一张可上下移的表**（每行 = 一个模式 +
一个处置），不是三个各自独立的文本框——行序就是引擎里的优先级，面板必须把它显示成可操作的东西，
并且在表头写一句"命中即定，第一个赢"（映射报告 §5.8）。

2026-10-06 这张表落下了两格：第一格是分流策略的四个开关 + 兜底出口 + 那张可上下移的名单，加上
系统格里 `network` 那三个值；同日第二格把 DNS 上游（国内/国外两张七控件的行表）、fakeip、
地址族策略与 `ports` 那五个端口也接上同一节 `#editor`（细节与实测见 §7.2 第 9、10 条）。
2026-10-07 第三格把**规则集**与**设备例外**两张行表也接上了：`rule_sets[]` 两种来源各一套格子
（本地 = tag + 相对路径；在线 = tag + 源地址 + 更新间隔，产物落点由 tag 派生、不给填），
`devices[]` 是 ip + 处置 + MAC + 主机名四格（细节与实测见 §7.2 第 14 条）。
同一天第五格是**节点与订阅**：`nodes[]` 一张行表，六档协议（vless / vmess / trojan /
shadowsocks 走 stream，hysteria2 / tuic 走 quic）各配自己的凭据格，加名字、地址、端口、
伪装域名 SNI、Reality 公钥与 short id（细节与实测见 §7.2 第 16 条）。
这一格和前面四格的表单所有权**不是一种形状**：前两期是"某几个键归表单"，第三、四期是"整份接管"，
而节点这一格是**按名字逐条、按键路径深度合并**——表单只交出"哪一条的哪一格"，
没画出来的那些键（transport 的 path/host/headers、alpn / insecure / fingerprint / ech、
obfs 与带宽、拥塞控制、alter_id / security / method / flow）由服务端那一份原样留下。
换成这个形状的原因就是原来挂在门外那条：**这一格里有凭据**。
"整份接管"意味着凭据要读进页面才能写回去，那等于把 UUID 与密码放进 DOM、放进每一次
`PUT` 的请求体、也放进任何看得到那台机器浏览器的眼里；逐条目合并让凭据格开局为空、
**留空就是"这一条不改"**，于是这一格能落地而凭据从不过网也不落地。
两处措辞在这一期按实测改正了上面那段原稿：Reality 的 `public_key` 是公钥，**不是秘密**，
它照常读进页面、照常渲染（没有 pbk 握不了手，所以它空着要拦）；而 `DesiredState` 里
**根本没有订阅令牌这一栏**——订阅 URL 是 `ingest::subscription` 的输入，一次性展开成节点，
落盘的是节点本身，所以"这一格里有订阅令牌"那句从来就不成立。

同一天还多了一节不在这八格里的**动作区**（同步规则集 / 开启·关闭劫持 / 回到某个修订）：它动的
不是磁盘上那份期望状态，而是现场。所以四个按钮共用两条规矩——令牌不在就全灰（连"下发"一起），
回执之后一律再 `GET /api/status` 取一次读数而不是把读数塞进回执；`rule_sets[]` 的编辑器虽已落地，
"同步规则集"同步的仍是**磁盘上那份状态已声明的清单**（§7.2 第 13 条），
所以这一节的正确用法是"加完格子先保存、再同步"。
2026-10-11 这一叠编辑格分成了两档（U1 / U2，取舍与实测见 `BLUEPRINT.md` §11.6）：默认只露七屏
（连接与令牌 / 订阅与节点 / 默认怎么走 / 某台设备单独走 / 让它生效 / 现在的状态 / 解析记录），
其余八屏（医生 / DNS 上游与 fakeip / 网络 · 路线 · 端口 / 设备组 / 规则集 / 分流情况 / 预览 / 日志）
住在侧栏底部那个"开启高级模式"开关后面。档只住浏览器 `localStorage`，不进磁盘那份期望状态——
一个人开的档不该改到共用的 `state.json`。这一期顺带把屏名换成了人话（"概览"→"现在的状态"、
"DNS 记录"→"解析记录"、"下发 · 同步 · 回滚"→"让它生效"），并在「现在的状态」加了一行只读的
**客户端指到哪儿**：它读的是磁盘上 `network.lan_address`，所以文案必须写清"这是磁盘上写的那一份，
不是网卡此刻的地址"（§7.1 那条"不许把期望态当现场读数"对这一行同样成立）。
2026-10-12 U5 给**普通档**加了第 16 屏「看板」（`board`，属"现在怎么样"那一层所以不在高级档后面），
普通 8 + 高级 8；它是只读屏，取数口与那三条界见 §7.2 第 24 条，第三卡「三台引擎在不在」见第 25 条。

### 7.2 API 面（`src/api/`，处理核心 + 令牌鉴权 + 监听层 + 静态面 + 编辑层六期 + 动作面四条 + 监控面一条 + 订阅导入一条 + 网卡形状一条 + 看板两条 + 设备四栏一条 + 三档开关一条 + 校验清单一次列全一条 + 全量对照一条已落地）

原稿把写面画成"一次 `PUT /api/state` 里顺带 validate → 指纹比对 → apply"。**2026-10-06 落地时
改成两步**，理由就是本节那条硬约束：既然"不做参考实现那种保存后静默重启"，那就不该存在任何
"保存即下发"的代码路径可走——把重载做成另一个端点，误触发的可能性从接口形状上消失，
不依赖前端记得传某个标志位。

已落地（`src/api/mod.rs` **十五个**路由条目 + `src/api/token.rs` + `src/api/http.rs` +
`src/api/serve.rs` + `src/api/assets.rs` + `assets/panel.{html,css,js}`，api 模块 131 个单测、
全量宿主 **515** 绿（2026-10-12 由 `cargo test --offline --lib -- --list` 现数：
gen 168 / api 131 / svc 111 / model 43 / doctor 27 / ingest 23 / apply 12。
上一格记的 115 / 480 是 U3 之前当场数的那一份，此后几轮只往蓝本 §11.x 记增量、没回来更新这里）。
下表只列写面与最早那几条读面；`GET /api/preview`（第 1 栏四份生成物）与
`POST /api/dns-records` / `POST /api/dns-record`（观测面两层）的形状记在 `BLUEPRINT.md` §10
那三份实测回执里，没在这张表上重复。
两平台的差集实测是三条——宿主独有 `token::non_unix_refuses_to_invent_a_weaker_random_source`，
guest 独有 `token::the_real_random_source_gives_distinct_values_and_a_shaped_token` 与
`model::state::the_written_file_is_not_readable_by_anyone_but_the_owner`）：

| 端点 | 动作 | 回执 |
|---|---|---|
| `GET /api/state` | 只读磁盘上那一份 | `{source: saved\|missing\|broken, state, error, etag, last_good, reload_pending, generation_error}`；`state` 是**磁盘形状**（含 `schema` 键），v1 文件在这里已经被迁移成 v2 呈现 |
| `PUT /api/state` | **只写盘，一个 engine 调用都没有** | 413（> `MAX_BODY` = 1 MiB，回执里写清"这里收的是整份期望状态，不是分片"）/ 400（非 UTF-8，或没带 `If-Match`，或 `state::parse` 的报错原样带回）/ 412（`If-Match` 陈旧，带上当前 etag）/ 500（写盘失败）/ 200 `{saved, source, etag, reload_pending, generation_error, warning}` |
| `POST /api/apply` | 显式重载：从磁盘重读 → 生成 → `engine::apply` | `{reloaded, revision, message, last_good}`；内容与已生效修订一致时 `{reloaded: false, unchanged: true}`；失败时用 `ApplyError` 自己的 Display 文案（含自动回滚到了哪一份修订），500 |
| `GET /api/status` | `observe` 读数 + 修订历史 | `{reading, last_good, revisions}`（`/api/revisions` 合到这里，不再单列）|
| `GET /api/doctor` | 结论列表 | `{source, findings, fatal, warn}`；没存过状态时按基线形状诊断，文件坏到读不出时 409 |
| `GET /api/logs` | 只读**五个来源各自的**日志末尾一截（数据面 / 监督者 / 面板 / oxidns / dae），一次给全，五张卡同一个时刻 | **没有参数位**（处理器不看请求体，体的上限只在协议层按 `Content-Length` 卡）；200 `{sources: [{id,title,who,state,file,lines,truncated,total_bytes,cap,note}]}`，`state` 六值 `ok\|empty\|missing\|journal\|no_home\|unreadable`——后四种是状态不是故障，每种各有一句 `note`；`file` 只给文件名，`no_home` 那一路是 `null`，回执里一个路径字节都不给；`POST` 到这一条是 405；**这一条没有 500**——某个来源读不动时它自己那张卡变 `unreadable`，其余四张照常给数 |

| `POST /api/rule-sets/sync` | 抓取 → 编译 → 验收 → 记清单，**不碰数据面、不重启** | 体只认 `{"force": bool}`；200 `{synced, report{refreshed, kept, local_missing, notes, reclaimed}, message}`；409 = `SyncError::Gap`（声明了本地规则集但文件不在，回执点名目录）；500 = `SyncError::Hard`（抓取 / 编译 / IO）。**两类都带部分报告** |
| `POST /api/hijack/enable` | 确认内核在听 → `nft -c -f` + `sh -n` 自证 → `sh netstate.sh` → `nft -f` | **不收任何参数**（多余键 400，且挡在探测之前）；200 `{enabled, message}`；409 = `HijackGate::NotListening`；500 = `HijackGate::Load` |
| `POST /api/hijack/disable` | 只撤 ZBox 自己的对象（`ip rule` / 表 100 / `zbox` 表） | 200 `{removed: true, message}`（带回"sysctl 保持原值"那一句）；200 `{removed: false}` = 空操作——两路探针都明确答"没加载"，卸载命令一条没发；500 = 撤的过程失败 |
| `POST /api/rollback` | 回到某个修订并**重启数据面** | 体只认 `{"revision": "<id>"}`，缺省 / `null` / 空白 = 已知良好那一份；200 `{rolled_back, revision, how: "已知良好"\|"指定", message, last_good}`；400 = 修订号不存在或不是字符串（400 那条按新到旧列出现有的）；500 = 回滚过程失败 |
| `POST /api/subscription` | 抓一次订阅 → 解析成节点 → **只往磁盘那份 `nodes[]` 追加** | 体只认 `{"url": "<订阅链接>"}`（协议只放 `http` / `https`）；200 `{imported, written, source, format, parsed, skipped, added[], already_there[], total_nodes, etag, reload_pending, generation_error, render_errors, warning, message}`；400 = 链接那一格不成形状 / 订阅内容读不成节点（Clash 配置、sing-box 导出）/ 合并后过不了整份 `validate()`；409 = 磁盘那份读不出（**这一支一个进程都不起**）；500 = 抓取本身坏了。**回执只有名字与计数，一条凭据都不带**；一条都没新增时 `written:false` 且不碰磁盘；磁盘上还没有状态文件不是拦路（写的就是第一份） |

原稿里的这四条 mutation 端点在 2026-10-06 落地（task #32）。**动作定义一条都没有新写**：
它们调的就是 CLI 在用的同一批 `Svc` 方法（`sync_rule_sets` / `enable_hijack` / `disable_hijack` /
`rollback`），失败分类也只有一处——"缺口 vs 硬失败""门坏了 vs 装载失败"在 svc 的错误类型里定完，
API 只把类型映射成 HTTP 码，不再各自看一遍结果再猜。`GET /api/logs` 当时按那句交接给了 task #7，
**2026-10-09 以第 18 条落地**：读的就是上面那一句里"读哪一份日志"的答案（`Layout::kernel_log_path()`，
即配置里那条 `log.output` 自己落盘的 `<root>/sing-box.log`，不是监督者另开的 `--stderr` 那一份），
截法是"末尾 64 KiB 的整行"，不需要 ring buffer——监督者那一条 `svc::rotate_kernel_log()` + `LOG_CAP`
已经管住了文件本身的体量，这一层只管一次给多少。

这一版落地说明有十八条，第 1–7 是原稿列的七条约束，第 8 是同日新增的静态面，第 9、10、14、15、16、17 是
编辑层六期（第 14 条是 2026-10-07 落下的规则集与设备例外两张行表，第 15 条是同一天落下的策略组，
第 16 条是同一天落下的节点与订阅，第 17 条是把四张行表统一成按条目合并），
第 11 是这一期之后剩下的那一条**还没定**的判据（不是端点），
第 12 是 2026-10-06 落下的面板服务制品，第 13 是同日落下的动作面四条（含 guest 那一轮里刻意没打的三格），
第 18 是 2026-10-09 落下的监控面第一条（只读日志的末尾一截）。
出处不变："没有路径参数"= 映射报告 §8.2（参考面板把路径当参数收）；
"重载必须显式"= 本文 §5.4 的分级回执与 §8.3；"每个 mutation 都要过鉴权"= §8.1（9099 面板
全无鉴权）与 §8.4；"不许绑通配地址"= §8.1。第 1–5 条与第 8 条每一句都有单测钉着；第 6 条与
第 8 条另外在 Alpine guest 上对着真网卡跑过，下面写的是读数，不是"应当"。第 9、10、14、15、16 条（编辑层五期）
每条也都同时有单测和 guest 真浏览器一轮。

1. **没有任何端点接受目录或路径参数**——`Request` 只有 `method/path/body/token/if_match`，
   五个字段没有一个能拼出路径；路径只从 `Layout` 派生，端点表是 `const ROUTES`，未列出的路径 404、
   列出但动词不对 405。这一层没有 `File::open(req.path)` 这种写法可写。404 的响应体不复述
   `Layout::root`，免得把落点喷进日志。
2. **重载必须显式**（见本节开头的两步拆分），`RELOAD_WARNING` =
   "下发会重启数据面，所有在用连接会被掐断一次"，随 `PUT` 的 200 回执一起给出。
3. **每个 mutation 都要会话**——2026-10-06 定案并落在这一层：`src/api/token.rs` 的**机器生成
   高熵令牌**（16 字节 = 128 位，定宽小写十六进制，落 `Layout::panel_token_path()` =
   `<root>/panel.token`，0600 在 `create` 就给到，走 `state::write_private` 那一套原子写）。
   选它不选"argon2 口令 + cookie"的三条理由：**没有登录端点**，因此没有在线猜口令的面，
   限速 / 口令强度 / "忘了口令怎么办"这三件事一起消失；本地 cargo registry 里
   `argon2`/`blake2`/`getrandom` **一个都没有**（实测：缓存里只有本项目那 14 个 crate），
   走口令档就得为了登录去破坏 `--offline` 构建；而两档在 LAN 上的明文暴露是一样的
   （`http://` 下 cookie 与令牌头同样过网），所以差别只在成本，不在"更安全"。
   配套的三条约束：**鉴权排在路由之前**——没令牌连"这个端点存不存在"都不回答（401 而不是
   404/405），且**只读面也在门内**，因为 `GET /api/state` 的磁盘形状里带着节点凭据
   （UUID / password / Reality 密钥）与规则集源 URL；`PanelToken` 的 `Debug` 手写隐值，
   免得 `{got:?}` 把令牌打进测试输出；**令牌只落文件、不打印到 stdout**——OpenRC 的
   `supervise-daemon` 会把 stdout 收进服务日志，那等于把密钥写进一个默认可读的文件，
   要人看一眼走显式命令（`zboxd panel-token`：读现成的，没有就生成一份再打印，**原值只走 stdout**、
   解释走 stderr；第二次跑同一句拿到的是同一份，悄悄换发会让用户手里的令牌变成"时不时打不开"）。
   随机源走注入：Unix 上是
   `/dev/urandom`，非 Unix（开发机）**报错而不是拿时钟凑一份**——那等于把生产的鉴权强度
   换成一个只在开发机上成立的弱实现。
4. **保存要带前提条件**（丢更新保护，同日落地）：`GET /api/state` 多回一个 `etag`，`PUT` 必须
   带 `If-Match`。指纹是 `store::bytes_digest`——与引擎判"快照有没有变"同一个 FNV-1a 函数，
   两处若是两个数，"面板说变了"和"引擎说没变"就能同时成立；算的是**磁盘原始字节**而不是解析后的
   值，因为坏掉的文件也要有指纹，否则"文件坏了要修"这条正路被自己的保护挡死。没有文件时 etag 是
   一个词 `absent` 而不是 `null`：`null` 分不清"没文件"和"服务端没算"。顺序是 413（体积）→
   400（非 UTF-8 / 没带 If-Match）→ 412（陈旧，回执里带当前值，前端据此重读再放改动）→
   400（校验不过）→ 500（写盘失败）→ 200（回**写完重读算出的**新 etag——落盘的字节经过迁移
   之后不一定等于发送的字节）。
5. **线上格式**（2026-10-06 落地，`src/api/http.rs`，9 个单测）：手写而不是引框架——本地
   registry 里没有任何 HTTP 框架（§2.1 的 `--offline` 约定），而这一层要支持的只是"一个局域网
   里的浏览器对着 `ROUTES` 那张表做 fetch，表里每一条都不带路径参数也不带查询串"。规则里不是随手写的四条：**行尾必须是 CRLF**（只用 LF
   分隔的请求拒收——浏览器不会这么发，宽容在这里换不来兼容，换来的是请求走私的空间）；
   **不收绝对形式与 `*`**（`GET http://host/api/state` 一律 400，这是本节第一条硬约束在协议层
   的落点：客户端一个路径字节都给不进这一层）；**先看 `Content-Length` 再决定读不读体**（超限的
   413 发生在读体之前，对面打不穿内存；出现两个不一致的长度是 400 而不是"取大取小"；
   `Transfer-Encoding: chunked` 是 501——分块意味着长度要一边读一边猜）；**`Expect:
   100-continue` 要回那一句**，否则 curl 一类在 body 超过 1 KiB 时会干等。每个响应都带
   `Connection: close` + `Cache-Control: no-store`（回执里有凭据，不该被浏览器缓存），不做
   keep-alive 就没有"这条连接还能不能复用"那个状态。没有 `Set-Cookie`、没有
   `Access-Control-Allow-*`：面板与 API 同源、令牌走自定义头，于是 cookie 会话与跨源授权两类
   攻击面在这台机器上不存在。每条读写 10 s 超时（见第 7 条为什么这不是体验项）。
6. **绑定地址**（同日落地，`src/api/serve.rs`，9 个单测 + guest 实测）：§8.1 那条"面板只应
   在局域网那一张卡上"以前只是文档，现在是代码里的拒绝。三条规则：通配地址（`0.0.0.0` / `::`）
   **永远拒，显式给也不放行**；地址必须是**这台机器真有的网卡地址**（判据取自
   `Topology::addrs`，即 `ip -4 -o addr show`），报错时把本机有的地址逐个连网卡名列出来；
   不给 `--bind` 时取期望状态里 `network.lan_interface` 那张卡的 IPv4，取不到就**拒绝启动而不是
   退回 `0.0.0.0`**，错误文案里带上网卡名和 `--bind` 这条出路。回环只有显式写才放行（`--bind
   127.0.0.1:9191`，那是"SSH 上去再端口转发"那条路）。默认端口 9090：避开数据面的 53 / 9888 /
   9887 / 8888 / 7890，也避开两份参考面板的 9099。`--bind` / `--port` 用在别的命令上直接报错，
   不静默忽略——"给了新地址、命令行没吭声、面板还在老地址"是最难查的那种没生效。
   2026-10-06 guest（Alpine 3.21.3，eth0 = 192.168.219.128/24）实测：`state.json` 不存在时
   `zboxd serve --root /tmp/zbox-panel --init openrc` 探测填出 eth0 后绑
   `192.168.219.128:9090`（`ss -lnt` 只有那一行，不是 `0.0.0.0`）；`--bind 0.0.0.0`、
   `--bind 10.99.0.9`、`--bind 192.168.219.127`（同段但不是本机地址）三处都在 `bind()` **之前**
   被拒并给出上述文案；`--bind 127.0.0.1:9191` 放行；`--bind 192.168.219.128 --port 9192`（裸地址
   不带端口）绑成的正是 `192.168.219.128:9192`——`--port` 就是那个"bind 里没写端口时的默认值"，
   两处都写时以 bind 为准（这条有单测 `loopback_is_allowed_only_on_the_explicit_path` 钉着）。
   宿主（Windows）上 `panel-token` 与 `serve`
   都在"生成令牌"这一步 rc=1 退出——没有 `/dev/urandom` 就不猜一份弱令牌。
   从宿主经 VMnet8 打到那个 LAN 地址跑了 16 个请求（每个一条独立连接，正好也是 `Connection:
   close` 的形状），逐条对上：无令牌 401（错令牌同一条文案，
   回执里 grep 不到真令牌）、带令牌 200（`source=missing` + `etag=absent`）、`PUT` 无 `If-Match`
   400、`PUT If-Match: absent` 200 并回 `f3397429275e4eb8` + `reload_pending: true` +
   `RELOAD_WARNING`、再拿 `absent` 写一次 412（回执带当前值）、同内容同 etag 再写 200 且 etag
   不变、`/api/nope` 404（回执不含 root）、`GET /api/apply` 405、chunked 501、绝对形式 400、
   请求行残缺 400、`Content-Length: 2000000` 413（实发 9 字节，证明没去收剩下的）、
   `GET /api/status` 与 `GET /api/doctor` 各 200。落盘：`state.json` 与 `panel.token` 都是 0600、
   没有 `.tmp-` 残件、令牌在 `state.json` 和 `serve.log` 里命中 0 次（这就是"只落文件不进日志"
   的可验形式）。撤掉监听进程后 `9090` 消失，`rc-service zbox status` 仍 started、数据面那几个端口
   （53 / 9887 / 9888 / 8888，tcp+udp 共 6 行）照旧在听、`nft list tables` 仍空、
   `ip -4 rule show` 仍只有 `0:`/`32766:`/`32767:`、
   `/var/lib/zbox` 一个文件都没多。**`POST /api/apply` 这一条线上没验**：它会走 `svc.reload()`
   → `rc-service zbox restart`，而服务名寻址不受 `--root` 影响，会把机器上那份活的基线重启一次。
   所以它的覆盖仍是三条单测（`saving_then_applying…` / `an_identical_reapply…` /
   `a_failed_apply…`），要连线上一起验得在没有活服务的机器上跑。
7. **单线程顺序 `accept`** 是选出来的，不是没来得及做多线程：面板是一个人操作的局域网设备，
   换来的好处是没有跨线程共享状态、没有"两个连接同时改同一份状态"要另写保护。代价说明白——
   一个连上来不送字节的客户端能占住整块面板最多 10 秒（读写超时就是为此而设，它不是体验项），
   而能连到的那个地址本来就是自己那张 LAN 网卡。真要做并发得先有 `select`/线程，那不在零依赖档里。
8. **静态面**（同日落地，`src/api/assets.rs` + `assets/panel.{html,css,js}`，新增 4 + 2 + 1 个单测）：
   面板本体走 `include_str!` 那条文内路线——零依赖档里没有 npm 也没有 `rust-embed`，三份文件直接编进
   二进制，升级即换字节。界面因此没有构建步骤，代价是 JS 没有组件化。四条决定了它长成这样：
   - **路径是逐字比对的常量表**（`/`、`/panel.css`、`/panel.js`）。这一层没有 `File::open(req.path)`
     可写，表里没有就是 404，于是 `/../assets/…`、`//panel.css`、`/panel.css?v=1` 连"被拼进路径"的机会
     都没有；**查询串在剥之前先校验、剥完之后只留路径**（2026-10-11 真浏览器那一轮改的：原先
     不剥，`/?from=share` 把首页变成了 401 的 JSON，见 §8 第 3 条 P4-6 那一段；现在这一层没有
     任何端点读查询串，丢掉才是实话，见 `a_query_string_is_dropped_but_still_checked`）。
   - **静态面排在鉴权之前**，它是整块面板唯一不过令牌的门：浏览器加载 `/` 的那一次请求带不上自定义头，
     这一页必须先出得来才有地方填令牌。换来的条件是给的字节必须是编译期常量，而这句话写在测试里：
     字节块里不许有落点、现场读数、凭据，也不许出现"连续 32 个小写十六进制字符"那一串（那正是令牌的形状）。
     动词仍然窄：`/` 人人猜得到，所以对表里那三条、非 GET 的动词在门外面就 405，不掉进"404 还是 401"
     那个含糊分支；表外的写法不落在静态面，于是仍然走门（无令牌 401、带令牌 404）。
   - **`Response` 从此带一个 `content_type`**，取值只有两个来源——`Response::JSON` 与静态表里那一行，
     全是编译期常量，没有一处从请求拼出来。`Content-Length` 取 `String::len()`，那本来就是**字节**数：
     这一页满屏中文，按字符数写就等于浏览器读到截断。
   - **页面自带 CSP**（`default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self';
     base-uri 'none'; form-action 'none'`）。它挡的是这一层真实的 XSS 路径：状态文件里的节点名、
     订阅 URL、`doctor` 的报错文案都是外来文本，所以 JS 只用 `textContent` 建节点（测试直接断言
     `innerHTML` 一次都不出现）；而 `script-src 'self'` 又反过来定死了路由表的形状——不许内联脚本、
     不许 `style=` 属性，于是 CSS 与 JS 必须是两条独立的外链路径。令牌也从地址的 `#` 后面接（片段不进
     请求行、不落访问日志），接上之后立刻 `history.replaceState` 抹掉，存处只有 `sessionStorage`。
     这一条有两种到达方式，都得接：整页加载时脚本开头读一次；页面已经开着再往地址后面粘则是浏览器
     只做片段跳转、不重新加载，所以还要接一句 `hashchange`——`replaceState` 不触发它，没有回环。
   2026-10-06 guest 实测第一轮（v12 产物，`zboxd serve --root /tmp/zbox-panel12 --init openrc --port 9193`
   绑 eth0）：`GET /`、`/panel.css`、`/panel.js` 各 200，类型分别是 `text/html` / `text/css` /
   `text/javascript`（都带 `; charset=utf-8`），声明长度 2106 / 2144 / 8015 与实际字节逐条相等。
   **"字节 vs 字符"的实测形式**：这三份文件的 UTF-8 字符数分别是 1624 / 2082 / 7477，每一条都比字节数小
   （JS 那一版当时是 8015 字节），按字符数写 `Content-Length` 就等于浏览器读到截断；
   `PUT /panel.js` 与 `POST /` 都是 405 且回执在门外面给出；`/panel.png` 与 `/api/state` 无令牌都是 401
   （表外的路径不泄露路由表），带令牌 200。差一点就中的那批写法也打过：`/./panel.js`、`//panel.css`、
   `/../assets/panel.html`、`/%2e%2e/panel.js`、`/panel.css?v=1` 带令牌五条全是 404，
   其中前三条另测了无令牌那一面（401）。协议层只挡绝对形式、`*`、`://` 与反斜杠，
   所以带 `..` 的写法是走到常量表上才不中的——五条的形状都落进了 `api::tests` 的那个循环里钉住。撤掉监听进程后 9193 消失，`/tmp/zbox-panel12` 删净，
   现役基线一个字节没动（服务 started、数据面 53 / 9887 / 9888 / 8888 / 7890 共 7 行照旧、
   `nft list tables` 空、`ip -4 rule show` 仍只有 `0:`/`32766:`/`32767:`、`/var/lib/zbox` 未增一个文件）。
   第二轮在同一天、同一个 guest，但走的是真浏览器（`--root /tmp/zbox-panel14 --port 9195`）：页面出得来、
   样式生效、三份回执都渲染，`source=missing` 时下发按钮正确地是禁用态，控制台零报错。这一轮抓到三个只在
   浏览器里才现形的缺陷，都已修：错令牌时拿着它又敲了另外两扇门（3 次 401，改成 `/api/state` 一不中就返回）；
   部分失败的回执被随后的成功提示盖掉；地址片段只在整页加载时被读，页面已经开着时粘令牌什么也不发生
   （于是接了 `hashchange`）。**这里有一个坑要记下**：令牌那两条路径不能在同一浏览器标签里连着验——
   从一个已经开着的面板页 goto `/#<令牌>` 是**片段跳转，文档不重新加载**，跑的还是内存里那份旧脚本，
   看起来就像"哈希没被吃掉"；要验整页那条得真重载一次。两轮各自的实测还钉住一条：`GET /panel.js` 声明
   8715 字节（= 仓库里那份的字节数，字符数 7477），浏览器拿到的也就是这 8715。撤进程后 9195 消失，
   `/tmp/zbox-panel14`（连同它那一份真令牌 `panel.token`）与 `/tmp/panel14.log` 删净，现役基线照旧
   （服务 started、`rc-service zbox status` 说 started、数据面 53（tcp + udp）/ 8888 / 9888 照旧在听、
   `nft list tables` 空、`ip -4 rule show` 只有 `0:`/`32766:`/`32767:` 三条）。
   到这一层为止，面板"能读、能下发、**不能编辑**"那句话不再成立。
9. **编辑层第一期**（同日落地，`assets/panel.js` 的表单映射 + `panel.html` 的 `#editor` 一节，api 模块 +3 个单测）：
   落地的是 §7.1 八个 Tab 里"分流策略 + 域名名单 + 网卡"这一块，走的仍是那一条 `PUT /api/state`，
   没有新端点。四条决定了它长成这样：
   - **工作副本的规矩**：`diskState` 是上一次 `GET /api/state` 整份的 `state`，表单只拥有
     `policy.{private_direct, cn_direct, block_ads, bittorrent_direct, final_outbound, domain_rules}`
     与 `network.*` 这几条路径；保存时深拷贝整份 `diskState`，只把这几条盖上去，节点、策略组、
     DNS 上游、规则集、端口、设备**原样带回**。不守这条的后果是"改一个开关顺手清空了订阅"，
     而它只在磁盘上看得见，所以 `the_json_the_panel_writes_is_the_shape_the_model_reads` 是把
     面板拼出来的那段 JSON 直接喂给 `PUT`，再把落盘的那份读回来逐条对——对的是模型的读法，不是前端的形状。
   - **`If-Match` 与 412 只重放一次**：`GET` 回 `etag`，`PUT` 必带；412 的意思是"你编辑期间别人写过盘"，
     处置不是覆盖，是 `collect()` 收表单 → 直接读一份新的 → `overlay()`（表单拥有的那几条赢，其余以
     磁盘最新为准）→ 再存**一次**；第二次还 412 就停下问人。**这里有个已经踩过一次的坑**：重放那段
     不能走 `refresh()`——脏表单会被 `openEditor()` 拒绝覆盖，于是新底稿和新指纹都拿不到，重试必然
     再撞一次 412（这一条是顺着代码读出来、再在线上证实的）。
   - **"恢复自动探测"就是写回 `Network::default()` 那三个字面量**（`ens33` / `192.168.1.0/24` / `ens33`）：
     `topology::fill` 只有一个分叉——`network` 跟内置默认形状不一样就永久 `How::KeptExplicit`——所以
     没有别的机制可做。面板里硬写的这三个值由 `the_panel_s_default_network_is_the_model_s` 从
     `Network::default()` 推出来再比对，改了模型默认而面板没跟上，测试当场红。填完还要保存一次才落盘。
   - **看不懂的一律不动**：`rawToPattern` / `patternToRaw` 只镜像 `DomainPattern::parse/raw` 那四种
     形状（`.example.com` 归一成 suffix，回显成 `+example.com`）；未知形状退回 `JSON.stringify` 原样带着走，
     未知 action 在下拉里加一个"其他"项保住它。这一版不认识的东西不许被"顺手规范化"掉。
   **一处被实测推翻的说法**：`final_outbound` 指向不存在的策略组，原来这页写的是"生成阶段会拒收"——不对，
   `validate()` 在**保存这一关**就返回 400（回执原文 `期望状态不成立: 策略引用了不存在的出站/策略组 no-such-group`），
   页面提示已跟着改。单测里同时钉住"被拒之后磁盘字节与 etag 都没变"。
   另两条是把面板和页面钉在一起的：`every_id_the_panel_reaches_for_is_on_the_page` 扫 `panel.js` 里
   每一个 `getElementById("`，要求 `panel.html` 上真有那个 `id=`（JS 与 HTML 没有构建步骤相连，缺一个
   控件只会在线上 `null.addEventListener` 炸）。
   2026-10-06 guest 实测（v12 产物，`zboxd serve --root /tmp/zbox-panel15 --init openrc --port 9196`，真浏览器）：
   指纹走了一条完整的丢更新保护链——`2fc44ef10ee342aa`（读取）→ 外部写者抢先一步到 `3a9d6a2eeb02242d` →
   面板存出 `2dc3432bcbe082ee` → 存默认网卡后 `2544d2cd58bf7909`。网络日志是 `PUT 412 → GET → PUT 200`，
   而落盘那份**同时**有外部写者的 `"socks": 7891` 与表单的 `"block_ads": true`，这就是 `overlay()` 该有的形状。
   制造并发写者用的是宿主上一个小脚本（`.tmp/panel_writer.mjs`，只做 `GET` + 带 `If-Match` 的 `PUT`，
   改的是表单不拥有的 `ports.socks`），比让两个人同时点按钮可复现。控制台只有那两条故意的 412 / 400，
   零 JS 异常、零 CSP 违规；`POST /api/apply` 一次都没点（只观察了下发按钮的启用/禁用）。
   这一份产物的三份字节数是 JS 23542（字符 20317）/ HTML 5550（4282）/ CSS 3210（3072）。
   撤进程与清理：`kill` 那个 serve 的 PID（`pkill -f "zboxd serve"` 会连自己的 `sh -c` 一起匹配，要用
   `"[z]boxd serve"`），`/tmp/zbox-panel15`（含它那份真 `panel.token`）与两份日志删净，现役基线照旧
   （`rc-service zbox status` = started、`nft list tables` 空、`ip -4 rule show` 只有
   `0:`/`32766:`/`32767:`、数据面 53（tcp + udp）/ 8888 / 9888 在听）。
   **一个已知限制**（不是新缺陷）：把网卡存回默认形状再跑一次诊断，`doctor` 又会报
   "期望默认路由网卡 = ens33｜现场：eth0"——因为探测结果从不落盘。它就是下一条要定的那件事。
10. **编辑层第二期：DNS 上游与端口**（同日落地，`#editor` 里再加两节，api 模块 +2 个单测）：
    表单新拥有的路径是 `dns.{domestic, overseas, use_system_local, fakeip, strategy}` 与
    `ports.{dns, tproxy, redirect, mixed, socks}`，`overlay()` 因此从"搬 network + policy 那六条"
    变成按节搬（`DNS_FIELDS` / `FAKEIP_FIELDS` / `PORTS_FIELDS` 三张表，`mergeFields()` 一条实现）。
    四条这一期特有的形状：
    - **一行上游画满模型那七个控件**（`tag host port transport path sni via`）。少画一格，
      用户改这一行时那一格就丢——这正是第一期那条纪律在这里的形式。`via` 是**外部标签**枚举
      （`"direct"` 或 `{"outbound":"…"}`）、`strategy` 是 `{"mode":"…"}` 内部标签枚举，两处键名
      写错的表现都是"200 回执 + 磁盘上这一条消失了"，所以 `the_dns_and_ports_json_the_panel_writes_is_the_shape_the_model_reads`
      断的是**交出去再读回来还是不是它**，不是 JS 长什么样。
    - **新加的空白行先由 `blankUpstream()` 把七个键填齐**：这样"只填了地址"也交得出一条完整上游，
      而不是让服务端回一句 `missing field transport`；而一格没填的新行在 `collect()` 里被丢掉
      （"还没决定要加"比交一条空壳诚实）。
    - **认不出来的值一律不改写**：`transport` / `strategy` 落在枚举外时下拉里加一项"不认识的…"，
      用户不动它就按原样带回；`port` / `path` / `sni` 空串写回 `null`，`socks` 那一栏空串就是
      "不起这个入站"。JS 只多验一条"端口是纯数字"——不是为了代替 `validate`，是因为
      `<input type=number>` 对 `abc` 交出的是空串，静默把 SOCKS 那一格变成 null。
    - **`generation_error` 现在会留在保存提示里**：写盘成功不等于生成得出来。
      guest 实测把这条打出来了——勾上"命中广告规则集就拦截"而 `rule_sets` 是空的，
      `validate()` 不管，200 + 新指纹，而生成器回
      "策略需要规则集 geosite-category-ads-all，但它不在 rule_sets 里"；页面现在按 warn 脸把这句贴出来。
    2026-10-06 guest 实测（**v13 产物**，`serve --root /tmp/zbox-panel16 --init openrc --port 9197`，真浏览器）：
    首轮 `source=missing` 下渲染对得上基线（两行国内 doh、`overseas` 是**空**的——`DesiredState::baseline()`
    刻意 `overseas: Vec::new()`，不是渲染漏了；`fakeip.enabled=false`；五个端口 53/9888/9887/8888/7890）。
    改 `port-dns=5353`、清空 SOCKS、`strategy=ipv4_only`、开 fakeip、加一行 tcp 国内上游、
    加一行 doh 境外上游（经由填 `proxy`）→ 保存 200，磁盘读回来是
    `"socks": null`、`"via": "direct"`、`"via": {"outbound": "proxy"}`、`"strategy": {"mode": "ipv4_only"}`，
    而 `policy` / `devices` / `rule_sets` 一位没动，serve 的日志仍是启动那两行（**PUT 一个命令都没跑**）。
    指纹链：`absent` → `066541dc713ea1a3` → 外部写者把它推到 `f23db6911bbcdea3`（写的是 `devices`，
    表单不拥有的一节）→ 面板 `PUT 412 → GET → PUT 200` 得 `d97a875e819b4c29`，磁盘上
    **同时**有那条设备与表单的 `mixed:8889`、`inet6_range:"fd00::/16"`。三条失败路径也打过：
    勾着 fakeip 而段写成 `198.18.0.0` → 400 `fakeip 段必须是 CIDR 写法`、指纹不动；
    端口写成 `abc` → 表单自己拦住，**网络日志里那一次连请求都没有**；
    加一行什么都不填再存 → 那一行没被交出去。控制台只有那两条故意的 412 / 400，零异常零 CSP 违规。
    **又量到一条不对称**（页面提示已跟着写，收不收货还没定）：`via` 指向不存在的出站，
    `validate()` 在保存这一关**不管**（与第一期那条被推翻的 `final_outbound` 正好相反——那一条是真的会 400），
    报它的是 `doctor`："[致命] DNS 上游 overseas 指定了不存在的出站 proxy"，再往下就是下发时 `sing-box check` 失败。
    撤进程与清理：`kill` serve 的 PID，`/tmp/zbox-panel16`（含它那一份真 `panel.token`）与日志删净，
    现役基线照旧（started、`nft list tables` 空、`ip -4 rule` 只有 0/32766/32767、53（tcp+udp）/8888/9888 在听、
    `/var/lib/zbox` 没多东西）。**一条自伤记录**：这次 scp 直接落进 `/root/zbox-rehearse/`，
    把上一轮那份旧 `zboxd` 覆盖掉了（随后改名成 `v13-zboxd`）——现役服务用的是 `/usr/local/bin/sing-box`，
    没受影响，但以后 push 二进制要先写新名字，别拿旧名字当落点。
    **计数：宿主 288 / guest musl 289，api 模块两平台各 45**（这一期 +2 条；两份产物的字节数
    JS 35542 / 字符 31060，HTML 8975，CSS 4275）。
11. **还差的只剩判据，不缺格子**：能编辑的那几节如今全有表单了——`nodes[]` 在第 16 条落地，
    规则集与设备例外在第 14 条，策略组在第 15 条。`sync` 这条动作端点读的仍是磁盘上那份状态声明的清单，
    所以有了编辑器之后它才第一次能被"面板里加一条 → 保存 → 同步"这条路走通（第 14 条实测的就是这条）。
    还有一条从第一轮起就挂着、编辑层落地后**更该定**的：**新机器上 `doctor` 会报一条假 mismatch**——
    磁盘还没有 `state.json` 时，面板显示与诊断用的 `network` 是基线默认值 `lan_interface = ens33`，
    而 `topology::fill` 探到的真实网卡是 `eth0`，于是渲染出"[提示] 期望 ens33 / 现场 eth0"。
    根因是 `fill_network` 只把探测结果填进内存里那份，从不落盘，所以"期望"永远停在默认形状上。
    第一期之后这件事多了一层紧迫：面板现在真能把默认形状存回磁盘，用户点一次"恢复自动探测 + 保存"
    就等于自己把这条假提示重新装上（上面那轮实测正是这样现形的）。要定的还是那三选一：不比对 /
    拿探测值当期望 / 首次读取就落盘。
12. **面板自己那一份服务制品已落地**（2026-10-06，task #31）。`install` 现在写**三个**文件、
    登记并拉起**两个**服务，`zboxd serve` 不再靠人敲。命令行的形状只有一处定义
    （`gen::init::panel_argv`：`zboxd serve --root <状态目录> --init <systemd|openrc> --port <端口>`），
    两份制品从它生成，单测 `the_panel_command_line_is_one_definition_not_two` 钉住；
    `--init` 那两个词由 `the_init_words_baked_into_the_artifacts_are_the_ones_the_cli_parses`
    拿 `include_str!("../bin/zboxd.rs")` 对着 CLI 源码钉——消费者在二进制里，单测进不去，
    只能这样对名字。端口默认值钉到 `DEFAULT_PANEL_PORT`，且**不进 `state.json`**：
    "这台机器的管理服务开在哪个端口"不是数据面的一部分，写进去等于让一次 `rollback`
    顺手把门牌号也回滚掉。`--zboxd-bin` / `--panel-port` 两个旗标只对 `install` 有意义，
    给在别的命令上会报错。guest 实测（含一次被推翻的 `error_log` 落点）全记在 §6 第 3 步。
    **systemd 那一份仍然只是写出来了**：这台机器上没有 systemd，它与 `start_pre()` 里等默认路由
    那一条分支，同属 §8 里"未在真 systemd 机器上执行过"那一格。
13. **动作面四条已落地**（2026-10-06，task #32；形状见本节开头那张表）。这一条只记单测里看不到的三件事。
    - **面板这一条刻意不顺手先下发**。CLI 的 `enable-hijack` 是 `apply` + 装载两步连起来
      （`src/bin/zboxd.rs:589`），面板的 `POST /api/hijack/enable` 只调 `Svc::enable_hijack`：
      会掐断在用连接的那一步不藏在别的按钮后面（§5.4）。代价写在这里，别让下一个人当 bug 修——
      新机器上要先按一次"显式下发"，再按"开启劫持"，否则内核还没在听，这一条正好 409。
    - **回执不夹带读数**。四条动作都只报"做了什么、属于哪一类"，现场状态由面板随后再
      `GET /api/status` 取一次。服务正在被改动的那一刻把读数塞进回执，等于承诺"读数与回执同期"。
    - **空操作要说清自己空在哪**。`disable` 那条原本写"一条命令都没跑"，guest 那轮把它推翻了：
      `hijack_absent()` 正是要跑 `ss`/`nft`/`ip -4 rule` 几路读探针才敢断定没事。跑的是读、
      没跑的是卸载，所以现在是"一条卸载命令都没发，只读了现场"。单测里那份七条读数的 fixture
      是这句话的证据，不是反证。
    guest 一轮打在一次性根上（`--root /tmp/zbox-32c`、监听 `192.168.219.128:9393`，请求用裸 `nc`
    手写，令牌留在 guest 侧从不过网），十个格子全绿：无令牌 401、`GET /api/rule-sets/sync` 405、
    未知动作路径 404、把 `{"force":true}` 打给 `hijack/enable` 是 400 且回执就是"这个端点不收任何参数"
    （挡在探测之前）、`{"revion":"x"}` 400、`{"revision":123}` 400、声明了却没有文件的本地规则集 409
    并点名 `/tmp/zbox-32c/rule-set`，把文件放上去后同一条请求 200 `kept:["present-one","missing-one"]`、
    `{"revision":"nope"}` 400 报"现有的（新到旧）：一个都没有"、现场没劫持层时 `disable` 200
    `removed:false`。
    **刻意没打的三格**：`POST /api/apply` 与成功的 `POST /api/rollback`（这两条无视 `--root`，
    重启的是活服务）、成功的 `POST /api/hijack/enable`（会在活机器上真装一层，把 §6 第 5 步那条
    顺序基线弄脏）。收尾核对：一次性根已删、9393 无监听、无残留进程，活机器仍是 `sing-box run`
    在跑（pid 16980）、`nft list tables` 空、`ip -4 rule` 只有 0/32766/32767、
    `ip_forward=1` / `rp_filter=2` 不变、`/var/lib/zbox` 的修订目录与 `rule-set` 时间戳一字未动。
    **计数：宿主 305 / guest musl 306，api 模块两平台各 55**（这一期 +10 条）。
14. **编辑层第三期：规则集 + 设备例外**（2026-10-07 落地，`#editor` 里再加两节行表，api 模块 +2 条单测。
    **设备那一节 2026-10-12 起是四栏卡片，不是行表**——见第 26 条；本条里与设备有关的"行表 / 序号"形状已过期，
    规则集那张仍是行表）。
    这一条记单测里看不到的形状事实与实测格子。
    - **表单所有权的形状在这里变了一次**。前两期是"数组里某几个键归表单"（`dns` / `ports` 按字段覆盖），
      这两节是**整份接管**——模型里 `RuleSet` / `Device` 的每一个字段都在格子里画到了，没有"表单没画到"的键，
      所以 `overlay()` 对它们是 `clone()` 而不是逐键搬。由此得一条**每扩一期都要重看一遍**的规矩：
      并发写者探针（宿主机 `.tmp/panel_writer.mjs`）用的那条路径**必须仍在表单之外**——
      第二期后它是 `ports.socks`，本期后被表单接管，于是这一期挪到 `groups[]`。
    - **磁盘上 `Group.kind` 是套一层的**：`{"tag":"…","kind":{"kind":"selector"},"members":[…],"default":null}`。
      字段名 `kind` 与 `GroupKind` 自己的内部标签键同名，所以套两层；写成扁平的 `kind:"selector"`
      serde 直接拒收，整份状态读成 broken。这条是照着 `GET /api/state` 的回执量出来的，不是推出来的——
      它同时纠正了我对 `model/config.rs` 的一次误读：我以为那份夹具的嵌套形状解析不了，
      实测 `source=saved` 且零报错，错的是探针的扁平写法。**写探针之前先 GET 一次看真实形状**。
    - **在线源的产物落点不是输入框**：`initial_path` 由 tag 派生（`rule-set/<tag>.srs`），渲染成 `<output>`、
      用虚线边区别于真格子，`refreshDerived()` 随 tag 实时跟着改，而 `collectRuleSet()` 收网时**按 tag 重算**
      （不信 DOM 里那个显示值）。写别的落点在保存这一关就 400，回执点名"initial_path 应为 …"。
    - **已有的行不给切换来源**：本地↔在线之间没有可留的公共字段，切过去等于把另一边的键静默抹掉；
      要换就删了这条再加一条新的。
    - **设备那一格只看 `ip`**：客户端拒收带 `/段` 的写法（文案直接给后果：`10.99.0.4/24/32`）；
      留空的 MAC / 主机名写成 JSON **null** 而不是 `""`；`bypass` 走 nft 层的 `ip saddr … return`、
      `global` 一条规则都不下——这两档在文案里写明了，因为它们在 `config.json` 里找不到对应行，
      只看生成物的用户会以为面板没生效。`mac` 现在没有任何生成物消费它。
    - guest 一轮（`--root /tmp/zbox-33a`、监听 `192.168.219.128:9401`、
      二进制 `/root/zbox-rounds/v2026-10-06f/g7-zboxd`）在真浏览器里量到的：
      在线行的派生格随 tag 实时动（空 tag 先显示 `rule-set/.srs`，键入 `geosite-cn` 后成 `rule-set/geosite-cn.srs`）；
      设备 IP 填成 `10.99.0.4/24` 时**请求根本没发**，`#save-out` 直接是那句 `/32` 后果；
      保存 200 后概览的"生成报错"从 `策略需要规则集 geosite-cn…` 换成 `… geoip-cn …`——
      **换名词本身就是证据**：写盘那份 `rule_sets` 真被生成器的引用检查吃进去了；
      tag 重复是 400 `期望状态不成立: 规则集 tag 重复：present-one` 且指纹一字未动；
      删 / ↑ / ↓ 之后序号跟着位置重排，声明序存进磁盘、杀掉 `serve` 重启后按同序读回；
      **412 重放**：表单脏着（改 tag + 设备主机名）时先让宿主机探针 `PUT` 进一条 `groups[]`
      （指纹 `57a3c…` → `1eeb77…`），面板那一次保存撞 412 后自己换底稿重放，落盘那份同时带着
      探针的 `writer-probe-3` 与表单的 4 条规则集 / 2 台设备。
    - **同步那一格给的是 500，但带部分报告**。guest 没有 `curl`，在线源走 busybox `wget` 时
      `ssl_client: SSL_read / wget: download timed out`；回执是
      `抓取 … 失败：<wget 原文>｜缺文件：present-one（rule-set/present-one.srs）、missing-one（…）`，
      而**排在失败那条之后的 `manual-extra` 没进报告**——迭代有序，硬失败时那一条还没被看到。
      这正是第 13 条那句"两类都带部分报告"在浏览器里的样子，不是回回执写漏了。
    - **`assets/` 是 `include_str!` 编进二进制的**：改了 html/css/js 不重新交叉编译并 push，
      浏览器看到的还是旧文案（这一轮先撞上一次——页面顶部那句"只落了前两格"直到换上 `g7-zboxd`
      才变成"如今落了六格"；`/` 一份 12965 字节）。
    - **`assets` 那条扫描是逐字面量的**：`nothing_the_page_ships_points_off_this_box` 把占位符和注释一起扫，
      所以在线源那一格的提示词只能写"https 打头的完整地址"，注释里也不能带 scheme。
    - 一条写探针脚本的坑：行内 `querySelectorAll('input')` **不含 `<select>`**，所以设备行的第 3 个 input
      是 hostname 而不是 mac。我这么错过一次，那个值忠实地按我指的那一格落进了磁盘——
      面板没错，是脚本点错了格子。
    - **计数：宿主 307 / guest musl 308，api 模块两平台各 57**（本期 +2：
      `the_panel_s_rule_set_and_device_choice_lists_are_the_model_s` 钉两张选择表与派生目录，
      `the_rule_set_and_device_json_the_panel_writes_is_the_shape_the_model_reads` 钉 JSON 形状、
      三种 400 与"proxy 档没有可用组"那条 200 + `generation_error`）。
    - 收尾核对：一次性根 `/tmp/zbox-33a`（里面 `panel.token` 是一次性真令牌，随目录删、从没拉回宿主机）
      已删、9401 无监听、无残留进程、`/tmp/zbox-state-*` 与 `/tmp/zbox-api-*` 零命中；活机器仍
      `rc-service zbox status` = started、9 行监听、`nft list tables` 空、`ip -4 rule` 只有 0/32766/32767、
      `/var/lib/zbox` 与 `rule-set/*.srs` 的时间戳一字未动。**三格照旧没打**（`POST /api/apply`、
      成功的 `POST /api/rollback`、成功的 `POST /api/hijack/enable`）。

15. **编辑层第四期 A：策略组 `groups[]`**（2026-10-07 落地，`#editor` 里第三张行表——一张表两档，api 模块 +2 条单测）。
    这一条记单测钉不住的那些形状与浏览器实测格子；下一期 B（`nodes[]`）要用的那个决定写在最后一条。
    - **第 14 条量出来的那两层嵌套，这一期变成断言**。`the_panel_s_group_choice_lists_are_the_model_s`
      不写死形状，而是拿模型自己的序列化器 `serde_json::to_string(&Group{…})` 生成的串去查 panel.js 里
      **字面**有没有 `kind: { kind: "urltest"` 与 `kind: { kind: "selector" }`，并把每个 `GroupKind` 变体
      序列化出来的档名逐一对一遍 JS 那张 `GROUP_KINDS` 名单——套几层、叫什么、加了一档没同步面板，都会红。
      字段序也是序列化器给的：`tag` / `kind` / `members` / `default`，内部标签那个键写在最里层对象的第一个键上。
    - **一张表两档，档在加行时定，之后不给换**。两把按钮各造一份自己的空行（选择组只有 `default`，
      测速组只有 `url` / `interval` / `tolerance`），行的档从此锁死——和第 14 条"已有的行不给切换来源"同一个理由：
      两档之间没有公共可留的键，换档等于把另一边的值静默抹掉。要换就删了这条再加一条。
    - **测速组那一行画不出"默认成员"格子，是刻意的**。模型层报 400
      `策略组 {} 是 urltest，1.14.2 没有默认成员这个概念（它按测速自动选）`，而生成器真把 `default` 写进
      urltest outbound 会让 `sing-box check` 整体失败——画不出这一格比画出来再拦一道更诚实。
      实测两行渲染出的格子（selector 行 `url=NOCELL`、urltest 行 `def=NOCELL`）就是这条边界的样子。
    - **成员是一格逗号分隔的名单**：`splitMembers()` 做 trim + 去重，落盘成 JSON 数组。
      合法成员 = 磁盘上的节点 name + 别的组 tag + `direct` / `reject`，所以**组引用组在同一张表里成立**：
      这一轮 `proxy-a` 的成员写成 `auto, direct`，`auto` 就是同表另一行的 tag，一次保存即过。
      浏览器里量到的去重：输入 `auto, direct, auto` → 磁盘 `["auto","direct"]`；`tolerance` 落成**数字** 50 而不是串；
      urltest 的 `default` 落 `null`、selector 的落 `"auto"`。数组序 = 界面序（不是排序：先 `proxy` 后 `auto`，
      ↑/↓ 重排存一次之后磁盘跟着变成 `auto, proxy-a`）。
    - **提示行 `#group-members-hint` 报的是"现在能当成员的名字"，不是"磁盘上有的名字"**：
      取值 = 磁盘节点名 ∪ 本表单每一行的 tag（**含没保存的新行**）∪ `direct` / `reject`，随 `input` 实时跟着改。
      四种读数：零节点零行 → `direct、reject`；敲出一行 tag → `proxy-a、auto、direct、reject`；
      宿主机探针往 `nodes[]` 写进一条节点后重读 → `writer-probe、direct、reject`；两者都在 → 四者并列。
    - **重放分工在这期又收了一格**：`rule_sets` / `devices` / `groups` 三张表现在都是**整份接管**
      （`overlay()` 里 `clone()`），顶层只剩 `nodes` 是"表单没画到的键"——逐键合并的仍是 `dns` / `policy` / `ports`，
      整份搬的从两张变三张。**并发写者探针按第 14 条那条规矩又挪了一次：`groups[]` → `nodes[]`**，
      它现在 push 的是一条真节点，形状用 `cargo run --offline -q --example dump-snapshot -- <目录> full`
      从模型序列化器打出来再抄进脚本（TEST-NET 198.51.100.10、假 UUID 与假密钥），不是手写 JSON——
      第 14 条那次手写就被 serde 拒过。
    - guest 一轮（`--root /tmp/zbox-g7a`、监听 `192.168.219.128:18087`、二进制
      `/root/zbox-rounds/v2026-10-07a/g7a-zboxd`）在真浏览器里量到的，按指纹串起来：
      两行组一次保存 → `2dcd823194fbf514`（磁盘那份 scp 回宿主机 `.tmp` 用 node 解析核对，guest 没有 jq）；
      **412 重放**：表单脏着（改 tag）时先让宿主机探针 PUT 进一条 `nodes[]` 节点
      （`3d3cb4418e15f813` → `566275444df6d74a`），面板那一次保存撞 412 后自己换底稿重放（→ `580fecd11eefccdc`），
      落盘那份**同时**带着探针的节点、表单的两行组和那条规则集；随后 ↑/↓ 重排再存一次（→ `ea9eb9bf1af9c4c4`）。
      中途还有一次"兜底出口切成 proxy 档、组名填面板自己建的组"——保存 200，`generation_error` 当场点名 non-cn（见下下条）。
    - **`validate()` 的报错先后是可以量出来的**：删掉被引用的那一行（`auto`）并把另一行的 tag 改成
      `writer-probe`（正是探针那条节点的名字），保存撞到的 400 是 `策略组 tag "writer-probe" 与节点重名`；
      把名字改回 `proxy-a` 之后才露出 `策略组 proxy-a 引用了不存在的成员 auto`——**tag 冲突排在成员引用之前**。
      "删掉一个被别处引用的组"就靠这条路证的（不是另造夹具），两次 400 之后指纹一字未动。
    - **客户端拦住的三句 throw（请求根本没发）**：`策略组那一行没有 tag：别处引用它靠的就是这个名字`、
      `测速组那一行没有测速地址`、`测速组那一行要有测速间隔，比如 3m`。服务端六句 400 由
      `the_group_json_the_panel_writes_is_the_shape_the_model_reads` 钉：缺少 tag / 没有任何成员 /
      引用了不存在的成员 / 没有默认成员这个概念 / 默认成员不在成员列表里 / interval 不是时长字符串，
      每条都同时断言 etag 未变、`groups` 一字未动。
    - **浏览器 → state.json → 生成器 → 真 1.14.2 内核这一轮接通了**：面板建的 urltest 组 `auto`
      （`interval 3m`、`tolerance 50`）生成成 `{"type":"urltest","url":…,"interval":"3m","tolerance":50,
      "tag":"auto","outbounds":["direct"]}`，含全量 inbound 的配置 `sing-box check` rc=0；
      预跑门（`inbounds: []` + 独立 `-D`）`rc=0`、日志 `sing-box started (0.02s)`，并出现
      `outbound/direct[direct]: outbound connection to www.gstatic.com:80`——**测速组真的发了那一次探测**，
      这一行是"面板写的组被内核认了"的正面证据。读数位置与两条坑见 §11.11（含一条纠正：`log.output` 是相对路径，
      预跑门要看 `<-D>/sing-box.log`，不在 stdout）。
    - **兜底出口选 proxy 档时 `non-cn` 规则集是硬前提**（`gen/singbox/route.rs:193`）：切成 proxy 那一次是
      **200 + `generation_error`**"策略需要规则集 non-cn，但它不在 rule_sets 里"，不是 400——
      状态合法、生成物会让 `check` 整体失败，所以按分级回执只能给这一档。为了让生成物过内核检查，
      在**规则集那张表**里补了一条本地 `non-cn`（产物落点 `rule-set/non-cn.srs`；把现役目录那份 168002 B 的
      `.srs` **只读复制**进一次性 `-D`，`/var/lib/zbox` 一字未动），保存后 `generation_error` 变成"无"。
      **换名词就是证据**，而且这一轮的因果跨了两期落的格子：第 14 条的规则集格子消掉了第 15 条的策略组报错。
    - **计数：宿主 309 / guest musl 310，api 模块两平台各 59**（本期 +2：`the_panel_s_group_choice_lists_are_the_model_s`
      钉那两档的名单与那两层嵌套，`the_group_json_the_panel_writes_is_the_shape_the_model_reads` 钉面板形状的 JSON
      能 round-trip、六种 400 与"每次拒绝 etag 和 `groups` 都不动"）。这一轮产物：release 二进制 2,473,648 B、
      musl 测试制品 31,482,344 B；页面三份 JS 58850 字节（51074 字符）/ HTML 14599 / CSS 4634。
      `assets` 那条字面量扫描照旧是活的——测速地址那一格的提示词只能写"打头是 http 或 https 的完整地址"，
      真 URL 是用户在浏览器里敲的，不进页面源码。
    - 一条工具节奏的坑（免得下次再撞）：`browser-use` 的 `click` 会和页面首屏那条读链抢——首屏是
      `refresh()` 连着三次 GET 再 `openEditor()` → `fillForm()`，读没落地就点"加一个选择组"，
      那一行会被随后的 `fillForm` 静默重渲染抹掉，看着像按钮坏了。反过来，`#revert` 之后新加的行**该**消失：
      它先把 `formDirty` 归零再重读，"放弃改动"本来就是这个契约，不是 bug。这一轮的填格一律改成
      `evaluate_script` 里 `element.click()` + 设 `value` 后 `dispatchEvent(new Event("input"))`。
      另：概览那两格的真实 id 是 `generation-error` 和 `reload-pending`，我探过一次不存在的 `overview-generation`。
    - 收尾核对：guest 四个一次性目录（`/tmp/zbox-g7a`——里面那份 `panel.token` 是一次性真令牌，随目录删、
      从没拉回宿主机——以及 `-snap` / `-run` / `-prerun` 三份 `-D`）全删净、18087 无监听、
      `pgrep -f "[g]7a-zboxd serve"` 零命中；宿主机 scratch（`.tmp/nodeshape`、`.tmp/g7a-snap`、
      两份从 guest 拉回的 state.json）也删了。活机器仍 `rc-service zbox status` = started、
      53（tcp+udp）/9888 在听、`nft list tables` 空、`ip -4 rule` 只有 0/32766/32767、`/var/lib/zbox` 时间戳一字未动。
      **三格照旧没打**（`POST /api/apply`、成功的 `POST /api/rollback`、成功的 `POST /api/hijack/enable`）。
    - **留给第四期 B 的两个决定**（不是这一期改代码的事）：一是 `nodes[]` 里有秘密字段
      （UUID、密码、Reality 公钥、订阅令牌），画成表单之前要先定"留空算不算把那一条改成空"，
      也就是每条目单独合并还是整份接管；二是**如果 `nodes` 也整份接管，第 14 条那条"探针必须写在表单之外"
      就没有落脚点了**——顶层再没有表单不拥有的键，412 那条规矩得跟着换形态（要么写一个面板根本不认识的顶层键，
      要么改成断言"整份接管就该把探针那条抹掉"）。现在的倾向是前者按"留空 = 不改"逐条目合并，
      代价是重放逻辑不能再靠 `clone()` 糊过去。

16. **编辑层第五期：节点与订阅 `nodes[]`**（2026-10-07 落地，task #34；`#editor` 里又一张行表，api 模块 +3 条单测。
    任务清单里它叫"第四期 B"，本文按落地次数编号，因为它换掉的正是第 15 条末尾那个决定）。
    第 14、15 条那三张表在 `overlay()` 里都是 `clone()`（整份接管），这一格是**唯一按条目合并**的那一张，
    所以下面记的全是形状与读数。
    - **合并契约换了形态，接口形状一个字没动**：表单交出的仍是整份 `nodes` 数组（`PUT /api/state`
      收的是整份期望状态，第 1 条那条约束不变），但"这一条的哪些键归表单"改成**按键路径记**：
      每一行除了 `out`（= `clone(original)` + 动过的那几格）还留一份 `patch`，键路径长这样
      `["kind","protocol","password"]`。于是没画出来的那些键（transport 的 path/host/headers、
      alpn / insecure / fingerprint / ech、obfs 与带宽、拥塞控制与中继方式、alter_id / security /
      method / flow）原样留在磁盘上。`the_node_json_the_panel_writes_is_the_shape_the_model_reads`
      钉的就是这一条，而且它钉的不是"面板 JS 长什么样"：先演一遍面板那三份骨架 JSON 能被模型读进来，
      再**手工**往磁盘塞一条每个未画键都取非默认值的节点（`/ray`、`cdn.example.com`、
      `headers:[["User-Agent","zbox-test"]]`、`alpn:["h2","http/1.1"]`、`insecure:true`、
      `fingerprint:"chrome"`、`alter_id:"1"`、`security:"aes-128-gcm"`），只交两格改动，
      读回来那九个键加端口一个都不许掉。第 15 条那句"先 GET 一次看真实形状"在这里仍然有效：
      `kind` 仍是套两层（`kind.kind` = stream / quic，`kind.protocol.protocol` = 六档协议）。
    - **`mergeDeep` 只走普通对象，数组整个换掉**。节点里唯一的数组是 `ws headers` 那类键值对表，
      逐下标合并会把两条不相关的表搅在一起——单测里那句"headers 是数组，逐下标合并会把它搅烂"钉这条。
    - **凭据格从不读进页面**：建控件时交的是 `undefined`，于是开局为空、`li.texts[key]` 是空串，
      "这一格没动"的判据退化成"文本还等于那个空"，placeholder 写的就是
      `磁盘上已有值，留空＝不改这一条`。七格凭据（vless / vmess / tuic 的 UUID、trojan /
      shadowsocks / hysteria2 / tuic 的密码）全按这一条规矩，所以**给一条已有的节点换凭据
      不需要先知道旧值**。实测两边都成立：把 tuic 的密码敲成 `typed-pass-9` 存一次，磁盘那条
      `kind.protocol.password` 真换了，而同一行的 uuid `2c3d4e5f-6a7b-8c9…` 与
      `alpn:["h3"] / cc:bbr / relay:quic / zeroRtt:true / sni:tuic.example.com / insecure:true`
      全在；同一轮里 hy2 只改地址（`198.51.100.77`）和清空 SNI（落成 JSON `null`），
      密码那格空着 → 磁盘仍是 `test-password`；vless-reality 端口 443 → 2053，
      它的 uuid / `flow:"xtls-rprx-vision"` / `alpn:[]` / `insecure:false` / `fp:"chrome"` /
      Reality 公私两格一字未动。
    - **两处措辞按实测改正**（原稿在第 15 条末尾和 §7.1 那一段都写错了）：Reality 的 `public_key`
      是公钥、**不是秘密**，它照常读进页面、照常渲染；`DesiredState` 里**没有订阅令牌这一栏**——
      订阅 URL 是 `ingest::subscription` 的输入，一次性展开成节点，落盘的是节点本身。
    - **两格是条件画出来的**，理由都来自 serde：**内部标签枚举会静默丢掉标签之外的键**。
      SNI 那一格只在 `tls.mode === "tls"` 时出现（往 `{"mode":"disabled"}` 里写 `sni`，请求 200、
      磁盘上根本没有那一格，画出来等于骗人说改过了）；TLS 的落点两层不一样深——stream 在 `kind.tls`、
      quic 在 `kind.protocol.tls`，所以 SNI 与 Reality 那几格记的是**相对**路径，建行时接到本行的
      `tlsBase` 上（单测那句 `nodes[1]["kind"]["protocol"]["tls"]["mode"]` 就是钉这个深度差）。
      同理，`blankNode()` 的骨架必须一次把画出来的键给全，不能靠 `setDeep` 在
      `{"mode":"disabled"}` 里现造一个 `reality`。
    - **一格没填的新行是丢掉的**（`collectNode()` 那句 `if (!touched) return null`）。
      我原本把它当缺陷去测，实测才看清是设计："还没决定要加"不该交一条空壳。所以 (C) 那一轮
      先敲一格才露出按顺序拦的三句 throw（**请求根本没发**）：
      `节点那一行没有名字：策略组的成员就是靠这个名字引用的` → `节点 hy2-new 没有地址` →
      `新建的那一条节点 hy2-new 没有填密码：它没有旧值可留`（只对新行成立，且把缺的格按 label 列出）。
      第四句 Reality 那条是 `节点 X 是 Reality，但公钥空着：没有 pbk 握不了手`，
      只在"这一条是新建的"或"公钥那格刚被人清空"时拦——磁盘上本来就空着公钥的旧行不该拦住一次无关的保存。
      `port` 是 `u16`、那格交的是 Number：写成字符串的代价是**整份**状态读不进来，
      400 原文是 serde 那句 `invalid type: string "6443", expected u16`（单测断言回执含 `expected u16`）。
    - **服务端六句 400 的措辞原样回到回执**，且每条都同时断言 etag 与 `nodes` 一字未动：
      `节点名不能为空` / `缺少服务器地址` / `端口非法` / `缺少 uuid` / `出站 tag 重复：dup` /
      `不是 sing-box 认可的取值`（前四条来自 `model/node.rs`，后两条来自 `config.rs` 的出站表与 flow 名单）。
      (B) 那一轮在浏览器里量的正是最后这类跨表引用：真点掉 `auto` 组还在引用的 `hy2` 行
      （表单行序变 `[vless-reality, vless-ws, tuic]`）→ 保存 400
      `期望状态不成立: 策略组 auto 引用了不存在的成员 hy2`，磁盘那份仍是 6477 B、mtime 停在 13:15、
      md5 `ef4fc4da…` 一字未动。删按钮那一格的 title 早就写着这句后果：
      `去掉这条出站（引用它的策略组会跟着 400 退回）`。
    - **第 14 条那条"探针必须写在表单之外"这一期彻底没有落脚点了**：顶层五张表全归表单，
      所以这一轮的"并发写者"不走 `PUT`（那会被自己的 If-Match 拦住），而是**直接 scp 覆盖
      `<root>/state.json`**。这条顺带把第 4 句"指纹算的是磁盘原始字节"验了一遍——写者改的字节和
      面板撞出来的字节是同一份。整轮的指纹链（每一步都是 `GET` 读回来的值，不是推算的）：
      `157b1c38469249ea`（一次性根的初始那份）→ (A) 四格改动 → `51db6c89dc3b98c1` →
      (C) 新建 hy2-new + 改 auto 成员 → `5ce8266094be93e8` → 写者覆盖（删 vless-ws、改 tuic 地址、
      加 writer-probe）→ `da9bed38caf7b4f8` → (D2a) 重放落地 → `4a5453554e70086a` →
      (E) ↑↑ 重排 → `b0222ffe01ea4baf`。
    - **重放的三种落点，身份按改名前的名字算**（`baseName`；新行是 `null`）：
      (D1) 表单脏着 `hy2-new` 的地址与 `vless-ws` 的端口，写者删掉 `vless-ws` → 412 → 重放
      → 那一条找不到落点（`replayDropped = 1`，**不拿旧值把它复活**）→ 第二次 PUT **400**
      `策略组 manual 引用了不存在的成员 vless-ws`，md5 `17922a52…` 未变。
      (D2a) 表单脏着 `writer-probe` 端口→444 与 `vless-reality` 地址→`198.51.100.11`，
      写者删掉 `writer-probe`、改 tuic 地址→`198.51.100.231`、加 `writer-probe-2`
      （`198.51.100.242:443`，密码 `writer-pass-2`）→ 412 → 重放 → **200**，磁盘那份
      `vless-reality@198.51.100.11:2053`（表单那格落了）、`tuic@198.51.100.231`（写者那格留着）、
      `writer-probe-2`（写者加的那条整条留着，密码没被碰）、`hy2-new@198.51.100.88:8444`，
      而被丢掉的那格（writer-probe 的端口）没有复活成一条同名新记录。
      别人在同一条上换了协议档时按"以磁盘最新的为准"处理（补丁的落点已不是同一层结构，硬合会静默丢），
      同样计入 dropped 而不是假装合上了。
    - **一条实测暴露的丢更新形状（跨表危险），摔的方向是"什么都不写"**：
      `groups` / `rule_sets` / `devices` 三张表仍是整份接管（`overlay()` 里 `clone()`），
      所以写者删/改一条**被组引用着**的节点时，重放会把表单那份**陈旧**的成员名单整个写回去。
      (D2b) 就是刻意把这条推到反面：写者删掉 `tuic`**并**把 `auto` 的成员跟着修好（磁盘那份是自洽的），
      表单只脏着 `tuic` 的端口 → 412 → 重放 → 400
      `策略组 auto 引用了不存在的成员 tuic 另外：有 1 条节点在刚读到的那一份里已经找不到…`，
      md5 仍是写者那份 `66f08e52…`。**写者已经改对的东西会被表单的旧名单盖掉**，是模型层的引用检查
      把它拦住的——拦住不等于没错，第二次 PUT 白跑，人要按"放弃改动重读磁盘"再改一遍。
      修法是把那三张表也改成按条目合并——**这条已在第六期落地并复测（见第 17 条：同一形状的
      Case X 从这一句的 400 变成 200）**；本条 D2b 的 400 是第五期现场的真实历史读数，不改写。
    - **回执覆盖 bug 是量出来的**：412 那几轮改用 `MutationObserver` 录 `#save-out` 的每一次变化，
      读到的是三条一串——412 那句（带丢了几格的计数）被紧接着的成功/400 那句**整条盖掉**，
      "你敲的一格没落盘"在页面上只存在了几毫秒。修法：把那句抽成 `replaySentence()`、存进
      `replayWarning`（每次 `save()` 开头归零），让成功与拒绝两条分支都把它接在后面：
      `另外：有 1 条节点在刚读到的那一份里已经找不到（别人改过名或删了），那几格的改动没有落盘。`
      改完重量的两条读数就是 (D2a) 的 `已写入磁盘，新指纹 4a5453554e70086a。…\n另外：有 1 条…`（回执
      class 从 `ok` 变 `warn`）和 (D2b) 那句 400 + 同一句尾巴。单测
      `the_replay_warning_survives_whatever_message_comes_next` 钉这句话在 `save()` 体内被拼了两次、
      `replayWarning` 有归零、以及那个 `generation || replayWarning ? "warn" : "ok"`。
    - **(E) ↑/↓ 就是磁盘序**：先 `#revert` 重读（行 `[vless-reality, hy2-new, writer-probe-2]`），
      对 `writer-probe-2` 连按两次上移 → 表单序 `[writer-probe-2, vless-reality, hy2-new]` →
      存一次 200 → 磁盘 `nodes` 就是这个序，三条凭据各自跟着自己的行（`writer-pass-2` /
      `9a5cb1d0-1f2e-4c3d…` / `new-hy2-pass`）。这一格没有排序，DOM 序 = 写出的序（同第 14、15 条）。
      成员提示 `#group-members-hint` 这一期改成读**表单上那几行**而不是 `diskState.nodes`，
      所以刚点"加一条"还没保存的出站立刻能当成员：`writer-probe-2、vless-reality、hy2-new、auto、manual、direct、reject`。
    - **`zboxd render` 是这一期新加的 CLI 子命令**（不是端点）。理由在验收纪律那条：
      `sing-box run` 才算验收，而拿到生成物的另外两条路都碰数据面（`apply` / `install` 都要重启服务，
      且按第 13 条那句——它们连 `--root` 都不认）。`render` 只把 `snapshot_files(state)` 里那份
      `config.json` 打到 stdout：不写盘、不校验、不重启。附带一处改正：`fill_network` 那几句提示
      **一律走 stderr**，因为 `render` 的 stdout 是要 `> config.json` 或直接喂 `sing-box check` 的
      那份 JSON，混一行中文进去它就不是合法 JSON。量出来的两份产物：提示留在 stdout 时 8535 B，
      挪走之后 8448 B（差的正好是那一句）。
    - **(F) 内核读数**：`render` 出来的那份（面板这几轮自己写出来的状态）先 `sing-box check` **rc=0**，
      再把 `inbounds` 清空（7 个 outbound 留着）在同一台机器上用独立 `-D` 跑 `run`：
      日志第 3 行 `sing-box started (0.04s)`、`grep -c "inbound/"` = 0、
      `network: updated default interface eth0, index 2` + `gateway 192.168.219.2`，并且出现
      `outbound/hysteria2[hy2-new]: outbound connection to www.gstatic.com:80` 与
      `outbound/vless[vless-reality]: outbound connection to www.gstatic.com:80`——
      urltest 组 `auto` 的启动探测**真的拨了面板自己建的那两条出站**，这是"浏览器里敲的节点
      被 1.14.2 内核认了"的正面证据（第 15 条那次只拨到 direct，这次多了两条真出站）。
      同一目录里重跑第二次是 `sing-box started (0.03s)`。三条坑记在这里：
      `log.output` 是**相对**路径（`sing-box.log`，相对 `-D`），所以 shell 重定向那份是 0 字节，
      读数在 `<-D>/sing-box.log`（2970 B），`cache_file.path`（`cache.db`，32768 B）同理；
      日志尾部 `ERROR router: fetch rule-set geosite-cn: Get "…": context canceled` 出现在 started
      之后 7 秒、正是 `kill` 的那一刻，**那是取消不是抓取失败**，别拿它当"远端规则集起不来"的证据
      （§11 那条结论这轮没有被推翻：`.srs` 已预置在 `-D/rule-set` 里，started 才来得这么快）；
      规则集声明住在 **`route.rule_set`**，顶层没有 `rule_set`、`dns` 也没有那把键，四条各带
      `format:"binary"` + `http_client:"rule-set-downloader"` + `initial_path:"rule-set/<tag>.srs"` +
      `update_interval:"168h"`，全文 `.srs` 出现 8 次（4 个 initial_path + 4 个 url）。
    - 一轮工具节奏的坑（都是这一轮现形的，第 15 条那条同批）：`browser-use` 的 `click` 按 `uid` 点，
      **uid 会过期**——它照样回"Successfully clicked"，点的却是旧元素，我差点把上一次保存留在
      `#save-out` 里的那句"已写入磁盘"读成"这一次空改动也存了一份、指纹没变"。凡是"这一格点没点动"
      都要用 `evaluate_script` 里 DOM `.click()` 并在保存前先把回执文本设成哨兵值，效果靠行序 /
      md5 / `list_network_requests` 三样核对。其余四条：scp 覆盖正在跑的二进制是 ETXTBSY（先按 PID 杀）；
      `pgrep -f "[g]7b-zboxd serve"` 匹配不到真命令行（`--root` 插在中间），要用 `pgrep -af "[g]7b-zboxd"`；
      `pkill -f "<路径>"` 在 `sh -c` 里会连自己这条 wrapper 一起杀掉（ssh 退出 255），改用 `kill $PID`；
      同 URL 带 hash 的 `navigate_page` **不会重载页面**，旧 JS 还在跑，改完 assets 必须
      `location.reload()`（`assets/` 是 `include_str!`，改文案也要重新交叉编译再 push）。
      BusyBox 那边：`ls` 没有 `--time-style`、`pgrep` 没有 `-c`；guest 时钟读数是 10-01，宿主机是 10-07，
      所以上面那些时刻只用来排先后，不用来对表。
    - **计数：宿主 312 / guest musl 313，api 模块两平台各 62**（本期 +3：
      `the_panel_s_node_choice_lists_are_the_model_s` 从模型序列化器反推两层变体名与六格凭据的键路径，
      `the_node_json_the_panel_writes_is_the_shape_the_model_reads` 钉骨架可读、九个未画键在合并后一个不掉、
      `expected u16` 与六道 400，`the_replay_warning_survives_whatever_message_comes_next` 钉那句警告
      活得过后面那条回执）。这一轮产物：release 二进制 2,496,168 B、musl 测试制品 31,735,760 B；
      页面三份 JS 77394 字节（66126 字符）/ HTML 17236 / CSS 4634。
    - 收尾核对：一次性根 `/tmp/zbox-g7b`（含那份一次性 `panel.token`，随目录删、从没拉回宿主机）
      与三份 `-D`（`kernel` / `kernel-run` 等）全删净、18088 无监听、`pgrep -af "[g]7b-zboxd"` 零命中；
      宿主机 scratch（`.tmp/nodeshape`、`.tmp/nodepanel`、`.tmp/g7b-*.json`）也删了。
      活机器仍 `sing-box run`（pid 16980）在跑、53（tcp+udp）/8887/8888/9888 在听、
      `nft list tables` 空、`ip -4 rule` 只有 0/32766/32767、`/var/lib/zbox/sing-box.log` 停在 11:13
      （这一轮一次都没碰现役服务）。**三格照旧没打**（`POST /api/apply`、成功的 `POST /api/rollback`、
      成功的 `POST /api/hijack/enable`）。
17. **编辑层第六期：策略组 / 规则集 / 设备例外也改按条目合并**（2026-10-08 落地，task #35；把第 16 条
    D2b 那句"修法"真正落地——四张行表从此共用 `replayTable` 这一条按条目合并的规则，`overlay()` 里
    `groups` / `rule_sets` / `devices` 不再 `clone()` 整份接管。宿主 lib **313** / api 模块 **63**
    （比第五期基线各 +1：新增 `the_four_row_tables_all_replay_per_item` 钉四张表都走 `replayX` 出口、
    且 `clone(pending.groups)` 那三处整份接管字面量已从 `panel.js` 消失）；另在 `.tmp/merge_harness.mjs`
    纯函数层补一条 Case X 回归。
    - **这一期要钉的是跨表那一格，判据只有真浏览器一条路**：合并逻辑住在 `panel.js`（客户端），Rust 侧那
      63 条只钉机制（源码里那四句 `replayX` 出口在、整份接管字面量不在），钉不出"重放后磁盘自不自洽"。
      所以量法是 guest 一次性 `--root` + `--port`、真浏览器一轮，读数取磁盘 `GET` 回来的 etag，不推算。
    - **Case X（写者删了被引用的节点并把组跟着修好，磁盘那份自洽；表单只脏那条节点的端口、组那一格没动）**
      ——第 16 条 D2b 的同一形状，第六期落地后再量一遍。指纹链（每步都是 `GET` 回来的值）：
      `038cfe6ed478bd8d`（干净种子 `nodes=[n1,n2]`、`g1[n1,n2,direct]`、`g2[n1,n2]`）→ 写者
      `drop-n2-fix`（删 n2 并把 n2 从两张组成员里摘掉，自身 PUT 过 validate → 落 `66f0ab3b012b56c0`，
      磁盘 `nodes=[n1]`、`g1[n1,direct]`、`g2[n1]`）→ 表单带旧 If-Match `038c` 保存 → **412 → 重放 → 200**
      （回执 `已写入磁盘，新指纹 66f0ab3b…。还没下发…` 后接那句 `另外：有 1 条记录…那几格的改动没有落盘`，
      因带警告 class 为 `warn`）。机制：`replayGroups` 里 `g1`/`g2` 补丁为空 → 整行取磁盘最新（写者修好的
      那份，不再点名 n2）；`replayNodes` 里脏着的 n2 找不到落点 → 计数丢弃、不复活（`replayDropped=1`）。
      合并结果因此**自洽**，模型层的引用检查无从拦它——这正是第二次 PUT 能落 200 的前提。**与第五期 D2b
      那句 400 对照，就是这一期修复的全部价值**：写者已经改对的东西不再被表单那份旧成员名单盖掉。落地那份
      磁盘 etag 仍是 `66f0ab3b012b56c0`（合并结果与写者那份逐字节相同——表单唯一的编辑即 n2 端口被丢弃、
      其余全取自磁盘最新，所以内容没变、指纹没变，与第 14 条"空改动写回同一指纹"一致）。
    - **Case Y（面板自己删了一条它自己的组成员还点名的节点）仍是 400，且这是对的**（M3b 那轮量到的）：
      握着"悬空引用"的是**表单自己**、不是并发写者——`collect()` 交上来的那份 `groups[].members` 还点名
      被删的 n2，validate 全有或全无地拒，这不该由本期修复来兜（那是模型层引用完整性的本职）。按"放弃改动、
      把组那一格也删干净"重存 → 200。这轮顺带验到修复的**防静默损坏**那一面：成功那次把写者新加的设备行
      `10.0.0.9` 与同行被改的 hostname `别人改的主机名` 一路带过了整次保存。
    - **validate() 报错顺序（network → nodes → 出站 tag 重复 → groups → rule_sets）本期未动**：Case X 能过
      正因为它落到的是一份每个环节都自洽的状态；那句 `引用了不存在的成员` 来自 `config.rs` 的出站表检查，
      `drop-n2-fix` 的写者之所以能落盘，是因为它删 n2 的同时把成员名单也修好、没触发它。
    - 收尾核对：本轮只用一次性根 `/tmp/zbox-g7c`（port 18089、pid 30313）与写者脚本 `.tmp/g7c_writer.mjs`
      （新增 `drop-n2-fix` 一档）、`.tmp/merge_harness.mjs`（新增 Case X 一条，19 条全绿）；三格照旧没打，
      现役服务（pid 16980）与 `/var/lib/zbox` 一字未碰。**episode 已收尾**：`kill 30313`（`kill -0 30313` 现报无此进程）
      + `rm -r /tmp/zbox-g7c`（含那份真 `panel.token`，随目录删、从没拉回宿主机）；18089 无监听、无残留 g7c 进程，
      活机器仍 `rc-service zbox status`=started、53(tcp+udp)/8888/9888/9887/7890 在听、`nft list tables` 空、
      `ip -4 rule` 只有 0/32766/32767、`/var/lib/zbox`（cache.db/current/preflight/revisions/rule-set/*.log）一字未动。

18. **监控面第一条：`GET /api/logs` 只读日志的末尾一截**（2026-10-09 落地，task #36/#37/#38/#39；
    api 模块 +5 条单测，宿主 318 / guest 319 / api 68）。
    - **落点仍然只从 `Layout` 派生**：读 `kernel_log_path()`（= 配置里那条 `log.output` 自己落盘的
      `<root>/sing-box.log`，**不是**监督者另开的 `--stderr` 那一份），这一层不写文件名、不拼路径，
      所以"数据面换成 dae"时只动 `gen::singbox::LOG_FILE` 一个常量，端点形状不变。第 1 条硬约束在
      监控面的落点是：**回执里一个路径字节都不给**——`missing` 那一路只说状态，500 那两路只说动作。
    - **封顶的是"一次给多少"，不是"文件多大"**：`api::LOG_TAIL_CAP = 64 KiB` 与 `svc::LOG_CAP = 512 KiB`
      是两回事，后者由 `rotate_kernel_log()` 管文件本身体量，这一层不需要 ring buffer，也不需要
      监督者配合——倒着 seek 到 `total - 64 KiB` 读上来即可。
    - **切点会落在行中间、也会落在 UTF-8 字符中间**（中文日志实测必然遇到）：所以先
      `from_utf8_lossy` 再按 `\n` 切，且**只在真的切过头时丢掉第一条**——那个残缺字符只可能在
      被丢的那条里。断言的形状是 `tail_lines_drops_only_the_line_it_could_not_finish`（拿
      `&text.as_bytes()[4..]` 造一个中段起点），不是"看着没乱码就行"。
    - **`missing` 是状态不是故障**：文件不在（新装的机器、服务从没起来过）走 200 +
      `{source:"missing", lines:[], truncated:false, total_bytes:0}`，前端因此显示"数据面还没写过东西"，
      而不是报错。这一条与第 9 条"没存过状态时按基线形状诊断"是同一个脾气。
    - **前端按了才读、动过数据面之后自动补一次**，不轮询：服务是单线程顺序接连接的，首屏那三次 GET
      已经排了一条队，轮询会把它挤长；而日志真正有用的时刻是"下发 / 回滚 / 开劫持之后"，所以自动补读
      挂在 `runAction` 里那条 `touchedKernel` 判据上（`/api/rollback`、`/api/rule-sets/sync`，以及任何
      失败的回执）。三份 `include_str!` 的资产因此被一条断言钉在一起——`the_log_view_is_wired_in_all_three_assets`
      要求 HTML 有 `#logs-read`/`#logs-out`/`#logs-meta`、JS 里既有这三个名字又有那句
      `call("GET", "/api/logs")`、CSS 里有 `#logs-out` 的有界窗口：**少了任何一环都编译不过去，
      而不是等浏览器里一个 `TypeError` 才现形**。
    - **guest 真浏览器一轮**（一次性根 `/tmp/zbox-g7d`、`--bind 192.168.219.128 --port 18090`、pid 35088）：
      首屏无控制台报错、`sessionStorage` 里键名 `zbox.panel.token`、`#logs-meta` 初始"未读取"；
      点了之后 `missing` 那一路文案落对；再在同一个根里造一份 **225786 B / 2000 行**的真日志
      （每行 112.9 B、含中文），读数全是 `GET` 回来的值不推算：**574 条整行、首行 = 第 1427 行、
      末行 = 第 2000 行（1427+574-1 正好闭合）、替换字符 0 个、空行 0 条**，标题行给的是
      `末尾 574 条｜文件共 225786 字节｜前面还有没给的（只取最后 65536 字节）`；窗口是 `max-height: 320px`
      + `overflow-y: auto`，`scrollHeight` 77 万级也没把整页撑长。**没验到的一条要说清**：
      "动过数据面之后自动补读"那一路仍没实机走过——`apply` / 成功的 `rollback` / 成功的 `hijack enable`
      三格照旧一律不打，所以只验了显式按钮与两种状态文案。
    - 收尾：`kill 35088`（`ss -lnt | grep 18090` 已空、无 g7d 进程）+ `rm -r /tmp/zbox-g7d`（含那份真
      `panel.token`，随目录删，宿主机侧 `.tmp/g7d_token.txt` 同日删）；顺手清了 `/tmp` 里历轮留下的
      32 个 `zbox-api-*` / `zbox-svc-hijack-test` 残留（`/tmp` 现只剩 280 KiB）。活机器基线复核：
      `rc-service zbox status` = started、监听计数 7、`nft list table ip zbox` rc=1、`ip -4 rule`
      只有 0/32766/32767、`/var/lib/zbox` 各文件时间戳一字未动。

19. **观测面第二屏：`GET /api/ladder` + `src/gen/ladder.rs`**（2026-10-11 落地，task #76；面板新增一屏
    「分流情况」，api 模块 +5、`gen::ladder` +7、三份资产接线 +1）。这一层只记接口形状，
    五种状态的屏幕读数与那句被推翻的措辞记在 `BLUEPRINT.md` §10 的 P3c 回执里：
    - **`derive(&DesiredState) -> Vec<Row>` 不渲染、不校验、不读盘**，所以任何状态都给一张完整的表；
      "渲染不出"由 `render_errors` 那一格单独说（点名 dae 还是 oxidns，服务端原话递出来），
      不改表的形状也不重复报错。这与第 9 条"没存过状态时按基线形状诊断"是同一个脾气。
    - **序号是两列，不是一个**：`dns`（oxidns 查询链）与 `connection`（dae 的 `routing {}`）各自从 1 起，
      `None` 画成"不落行"并把原因放进 `why`——两台引擎各跑各的，跨层没有统一序号。
      `connection_lines` 说这一格在那份文本里占几行（`policy.cn_direct` 是 2：域名与 IP 各一条），
      所以下一格显示"第 7 条"。`shadowed_by` 给的是**位置**（被表里第 N 行整个盖住），不是一句话，
      面板据此渲染"第 N 条"，医生那一屏给的是句子。
    - **漂移钉子拿渲染文本反查链序**（`the_ladder_lines_up_with_both_renderers`）：连接层只认**顶格**那一个
      `routing {`——`dns {}` 里嵌着 dae 自己的同名 `routing { request { … } }`，按 trim 匹配会把两块并成一块；
      解析层按链上 `exec:` 出现的序。生成器换顺序而梯级没跟着换，这条当场红。
    - **`Row` 九字段全进回执**（`tier`/`when`/`then`/`from`/`dns`/`connection`/`connection_lines`/`why`/
      `shadowed_by`，`None` 落成 `null` 而不是键消失），`probe_*` 那几枚只给测试用、`#[serde(skip)]`。
      回执同时带 `source`（`saved|missing|broken`）、`etag`、`render_errors`、`reload_pending`、
      `last_good`、`generation_error`——**与 `GET /api/state` 同源同词**，两处 `last_good` 由测试钉成相等。
    - **这一屏的界是结构性的**：`snapshot_files()` 只有 `config.json` + `zbox.nft` + `netstate.sh` +
      `netstate-down.sh`，dae / oxidns 两份编排配置的文本**不在修订里**，所以"哪一条已经在数据面里生效"
      没有可比的落点，这一屏只能**整表**说下发过没有。

判定"要不要重载"不能靠这一层自己算，也不能靠前端记：`reload_pending` 由 `App` 拿磁盘状态现生成
一遍配置再比对，且**只在 `source == "saved"` 时给值**——新装的机器没有已生效修订，说"待下发"是假话。
**2026-10-11 P3c 补的下半句**：`saved` 里还分两种来路。`reload_signal()` 在**一个 `last_good` 都没有**时
也给 `pending=true`（"从没下发过"与"上次下发失败"都走这一支），所以光有布尔值不够——屏幕上那句
"与已知良好的那一份不一致"对一台一次都没下发过的机器等于指控它失败过一次。定式是**把事实随回执一起给**：
`GET /api/state` 早带了 `last_good`，`GET /api/ladder` 现在也带，前端按它分两句，**不在 JS 里猜**。
判据是"这一屏有没有同屏的另一行能自清"：概览有 last-good 那一行，梯级没有，所以梯级必须自己带。

20. **概览补上「修订历史」+ 回滚改成从清单里挑，并把 `last_good` 那一支在真机上踩出来**
    （2026-10-11 落地，task #83 与 #77 第二小项；全量宿主 458 绿）。
    - **形状**：`GET /api/status` 一次读取同屏喂两处——概览的 `#revisions` 一行与回滚控件
      `#rollback-revision`（`input` 换成 `select`，要人抄 30 字符修订号这个障碍就没了）。
      "一份来源、两处落地"由 `assets.rs` 的编译期断言钉，不由前端记。脸色三种：读数正常 /
      现场读数没读到 / 退出登录退回开局，第三种必须把清单清空而不是留着上一轮的条目。
    - **验收**（guest 一次性 `--root` + 一次性端口，`POST /api/apply` 一个字都没打）：手工造三份
      留痕修订 + `LAST_GOOD` 走"有清单"那一支，空目录走"一份都没有"那一支，退出走空清单那一支；
      整页重载后读数逐字相同（证明它来自磁盘而不是内存）；`consoleErrors` 为空。
    - **推翻的一条前提**（#77 原描述写的"真实触发不了"是错的）：`reload_signal()`
      （`src/api/mod.rs:1028-1044`）只比 `store::digest_of(last_good.id)` 与
      `content_hash(snapshot_files(state))`，而 `last_good` 读的是 `<root>/revisions/LAST_GOOD`
      这个**普通文本文件**（并要求同名目录存在）。所以 `mkdir revisions/<13 位毫秒>-<16 位十六进制>`
      + 把同一个名字写进 `LAST_GOOD` 就能让面板走到本来只有 apply 才会走到的分支，不碰数据面。
      实测逐字回执：「这张表还没下发：它与机器上那份已知良好的内容（修订
      1790905279555-deadbeef00000001）不一致。按下发才会换上去。」——`deadbeef00000001` 故意不等于
      任何真实内容指纹。第一支（"从没下发过"）同轮验到。
    - **反过来，第三支（`reload_pending == false`，绿色那句"与已经下发的那份内容一致"）确认没有只读出口**：
      要造它得知道 `content_hash(snapshot_files(state))` 的真值，而三个候选数互不相同——
      `snapshot_files()`（`src/gen/mod.rs:20-31`）含 `singbox::generate` 的全部产物加
      `netstate::render_teardown()`，`GET /api/preview` 只给四份且不含 teardown，`etag` 又是
      `bytes_digest(state.json 的原始字节)`（`src/api/mod.rs:329-337`）。手工造 `LAST_GOOD` 只能造出
      "不一致"。这一支继续由编译期 pin 兜，真机要等到 P4 允许一次真下发。

21. **订阅导入 `POST /api/subscription`：那条主路的第二步落在服务端，只做"追加"这一件事**
    （2026-10-11 落地，2026-10-12 真机复验收口，task #92 = U3；全量宿主 474 绿）。
    - **形状**：`src/svc/subscribe.rs`（`check_url` + `Subscribe::load`）管"要字节并读成节点"，
      `src/api/mod.rs::App::subscription` 管"合进磁盘那份 `nodes[]` 并写盘"。抓取用的是规则集
      那同一对判据——`svc::ruleset::download_body` 是从 `sync_entries` 里抽出来的同一段
      （guest 实测只有 busybox wget、它的 `-T` 是**空闲**上限不是总时长，这条实测结论只该有一处来源）。
      `ingest::subscription::parse` 的返回改成 `Parsed { format, nodes, skipped }`，
      `Format::word()` 给回执那句"这一份是什么形状"用词（词表在枚举旁边，不散进面板文案）。
    - **为什么这一步在服务端**：要落盘的正是那批凭据。交给面板再 `PUT` 一次，等于让每个出口的
      UUID 与密码进 DOM、再进一次整份覆盖。**回执只有名字与计数**（`api::tests` 拿夹具里的
      UUID、密码和订阅链接那串 token 逐条断言"回执体里不许出现"）。暂存件 `subscription.download`
      读完即删，读不懂也一样删。
    - **为什么敢不收 `If-Match`**：这一按**只追加**，磁盘上别人写过的键一个都不覆盖，因此不产生
      丢更新；面板若有没保存的改动，保存那一条既有 412 重放路本来就在处理"编辑期间磁盘变了"，
      而新导入的行按名字认行，重放时不会被抹掉（`append_nodes` 的身份规则与 `nodes[]` 编辑器
      **同一套**：同名那条保持磁盘上的原样，包括人在面板上换过的地址与凭据）。
      正因为只追加，前端在 `formDirty` 时**先拒绝**——导入写磁盘而重读不覆盖脏表单，
      两件事叠在一起的现场表现就是"按了导入，名单却没变"。
    - **三个码对应三个动作**：400 = 换一条链接（`file://` 一类根本不放行，也不回显那个路径）或
      换订阅格式（Clash YAML、sing-box 导出）；409 = 磁盘那份读不出，而且**一个进程都不起**
      （不许为一个坏掉的状态联网）；500 = 抓取坏了，去查这台机器的网络。
    - **新机器不是拦路**：磁盘上没有 `state.json` 时以 `baseline()` 写第一份（缺 ≠ 坏）。
      这一条把主路第一步的现场钉住了，并且**当场抓出一个真 bug**——状态目录还不存在时
      `wget -O <那个目录>/subscription.download` 会退出在"找不到路径"（os error 3），
      于是被报成一次网络故障；`Subscribe::load` 现在先把目录要出来。
    - **一条都没新增就不碰磁盘**（`written:false`）：重写一份内容一样的文件会把别人的编辑挤成
      一次 412，而这一按什么都没改，不该留下痕迹。合并后仍过整份 `validate()`，撞破"组名与节点
      重名"那一类界时 400 并写明"磁盘上那一份没动"。
    - **顺手清掉的两条 sing-box 退役后的误导文案**：`Format::ClashYaml` / `SingboxJson` 那两条
      报错原来还在教人"转成 sing-box 配置"，现在说的是"dae 要的是 `vless://` 一类的节点链接"。
      面板里另外三处"订阅导入还没接通 / 还在做"（概览那条主路的第②步、`nodes` 屏的说明、
      页题副标题）一并换成已落地的说法，并由 `assets.rs` 反向断言"资产里不许再出现那两句"。
    - **真机这一轮没有真机场订阅，用的是 guest 自己那台 busybox 桩**（2026-10-12 三轮，
      `.tmp/u3/round{,2,3}.json` 逐字留底）。为什么不抓真的：这一按要验的是"抓回来的字节怎么变成
      `nodes[]`、以及回执与磁盘的形状"，不是"某家机场的订阅长什么样"，而真订阅带着个人令牌。
      桩 = `nc -l -p 8099 -s 127.0.0.1 -e /root/u3/serve.sh` 外面套一个 `while :`，
      `serve.sh` 按 request line 里的 path 挑文件；凭据一律合成值 + TEST-NET 地址，只留在
      `.tmp/u3/` 与一次性 root。踩到的坑值得记：一次性 `read h; do :; done` 吃请求头会把**每一个**
      请求都判成超时——busybox wget 发完请求体不关写端，所以要 `read -t 1` 的有界循环
      （实测 8 行头足够）。这样 busybox wget 是真起进程、真出网，报错文案也是它自己的：
      `wget: can't connect to remote host (127.0.0.1): Connection refused` /
      `wget: server returned error: HTTP/1.1 404 Not Found`，两条都原样进了面板回执。
      八个用例逐字过了：空链接（"先贴进订阅链接"，`warn`）、脏表单拒绝（见下）、
      `file:///etc/passwd` → 400 且**一个进程都不起**（页面上既没有那个路径也没有 `root:x:0`）、
      Clash YAML → 400 点名"这份订阅是 Clash 的配置（YAML）"、死端口 → 500、404 → 500、
      成功导入 3 条 + 1 条读不懂已跳过、同名再导入 `written:false`
      （"磁盘上已经有 3 个同名的，那一份一个字都没改"）、Base64 打包那份**追加**——
      新名 首尔-D 进，同名那条留磁盘上人在面板换过的地址（`192.0.2.11`），没被订阅里那串
      `198.51.100.9` 顶掉。凭据按 7 项标记在页面上逐条 grep（三个口令、UUID、base64 段、
      `/etc/passwd` 那一行、Clash 口令）全部不出现，`consoleErrors` 为空。
      预览那一屏顺带验了一件事：dae 那份里节点渲染成 `node {` 块内一行一条
      `    node1: 'ss://…'`——**单引号**、tag 是 `nodeN`，判 `nodeLines` 要按这个形状数。
    - **真机抓出两条假话，都在这一步的界面上**（这就是"必须真浏览器一轮"的理由，编译期断言看不见）：
      ① `revert`（放弃改动，重读磁盘）按下去之后，那句"表单里还有没保存的改动"还挂在 `#sub-out`——
      而改动正是被这一下丢掉的，那句话当场成了假话；
      ② `logout` 之后屏幕上还写着上一轮"读到 3 个节点……新增的名字：东京-A、圣何塞-B、大阪-C"，
      说的却是这台机器此刻没在连的那件事，而 `#sub-url` 里那串链接（带着机场令牌）留着没清——
      成功那一路本来就当场清空，剩下的是被拒和出错那几路留下的。
      修法是一个 `resetSubReceipt()` 把回执退回 HTML 开局那一句"还没有导入过。"，revert 与 logout
      各调一次，logout 另外清 `#sub-url`（和它旁边清 `#token` 同一个理由：退出登录清的是这一页
      替人持有的秘密，不是人打的字——所以那四个过滤框不清）。
      跨资产那条新断言（`the_import_receipt_returns_to_its_opening_line_when_it_stops_being_true`）
      钉的是"两边逐字同一句"加两个调用点、两处清链接；三条变异（撤掉 revert 那一次调用 /
      少打一个句号 / 退出不清链接）都被抓到。473 → 474。
    - **顺带查出一条已经存在的界，不是这一轮的回归**：`fill_network` 只在 `src/bin/zboxd.rs`
      里跑（install / apply / render / doctor / serve 那几条 CLI），而面板这侧的
      `GET /api/state`、`GET /api/preview`、`POST /api/subscription`、`POST /api/apply` 走的都是
      `App::disk_state()` / `App::preview()` 直接读盘，**一次都没填过网卡形状**。
      现场证据：一次性 root 的页面上 `#net-lan` = `ens33`、`#net-cidr` = `192.168.1.0/24`
      （`Network::default()`，`src/model/config.rs:158-171`），而这台 guest 的网卡是 `eth0`；
      渲染出来的 dae 那份因此写着 `lan_interface: ens33`。同一个 state 交给 CLI `render` 就是另一份。
      §7.3 那条"谁往 `state.json` 写第一份，连同 §6 第 5 步探到的网卡值要不要回写磁盘"的欠账
      就是这一处的根，U3 把它挪到了主路第一步上（新机器：导入订阅 → 直接下发）。**这一处的修法
      落地在第 22 条**（task #99 = U3b）。

22. **网卡形状进面板：`POST /api/subscription` 写第一份状态前先探，`GET /api/status` 与
    `GET /api/doctor` 把它读给人看**（2026-10-12 落地，task #99 = U3b；全量宿主 480 绿）。
    - **为什么这一步非管不可**：新机器上"导入订阅"这一按落的就是**这台机器的第一份期望状态**，
      而 `svc::topology` 的规矩是"显式写过就不再覆盖"——出厂默认的 `ens33` / `192.168.1.0/24`
      一旦写进磁盘，后面 `GET /api/state`、`GET /api/preview`、`POST /api/apply` 全部只读盘，
      那份写着本机根本没有的网卡的期望态就一路生成进 dae 的 `lan_interface`（还有 nft 的
      `oifname`、sysctl 的键名），声响要到数据面起不来才听得见。
      `fill_network` 原来只住 CLI（`src/bin/zboxd.rs`），面板一次都没跑过——第 21 条末尾记的就是这笔账。
    - **用的还是 CLI 那一个 `topology::fill`**（`App::subscription` 里的 `fill_first_network`），
      不是第二份填充逻辑：两份将来就是"面板写的和 CLI 填的不是同一个形状"。判据同样只有一处
      （`doctor::network_shape_finding`），面板那行只报读数、诊断那句只下判断，免得变成
      "这一行说不一致、诊断说没问题"。排在 `merged.validate()` **之前**：探来的 `lan_address`
      也是要过门的那一份，门只认落盘的形状。
    - **位置是三条早退之后**（400 读不懂 / 500 抓坏 / 409 磁盘那份坏）。`GET /api/status` 与
      `GET /api/doctor` 里那一次探测是唯一另一处起 `ip` 进程的地方，因为它们本来就得起进程读数。
      `a_bad_link...` 里 ② 那一句 `fetcher.argv().len() == 2` 钉这个位置——把探测挪到抓取之前，
      它就多两条 `ip`。而"一条都没新增"那一支**不需要**这一道闸：`ingest::subscription::parse`
      拒绝零节点正文，新机器那份底稿又不含节点，所以 `fresh == true` 时 `added` 必非空；
      真正的"不碰网卡"闸门是 `fresh` 本身（磁盘已有状态 → `network_note = None`）。
    - **降级不是故障，三条界各自钉住**：这台机器没有 `ip`（Windows 开发机、chroot）→ 状态照写、
      网卡保留出厂默认、回执明说"没能按本机填"（`a_box_without_ip_...`）；`ip` 跑成了但形状判不出
      （多张卡带段）→ **不猜**，回执把候选清单原样列出、这一份按出厂默认落盘并教人去哪儿填
      （`an_ambiguous_nic_shape_...`）；`GET /api/status` 的 `network` 字段按**字段**degrade
      （探不到时局域网三格 `null`，`wan_interface` / `gateway` 那半截照给，`detail` 带原因），
      面板那一行给 `note` 不给 `warn`（`status_degrades_field_by_field_...`）。
      `doctor_names_a_first_state_still_on_the_factory_nic` 钉诊断那三条脸色：还没人写过 → 报；
      显式写过 → 不报（机器不跟人抢）；没有 `ip` → 不报（没读到 ≠ 配错了）。
    - **附带修掉第 21 条真机验收记下的一笔缺口**：探来的 `lan_address` 让新机器导入订阅后 oxidns
      那份不再渲染失败（解析口要绑的地址有了），概览「客户端指到哪儿」也有地址可抄。
    - **五处变异的读数**（这一轮的牙口检查）：撤掉 panel.js 退出名单里的 `"network-shape"` → 红；
      撤掉 HTML 的 `id` → 红；`setText` 少第三个参数（底色不动）→ 红；把探测挪到
      `added.is_empty()` 之前 → **不红**，于是发现第 21 条末尾那句注释夸了口（改准，见上）；
      把探测塞到抓取之前 → 红（4 条而不止 ② 一处）。

23. **导入订阅即把「默认怎么走」设成智能分流：一次设齐五格，少一格就在某一扇门外报错**
    （2026-10-12 落地，task #95 = U4；全量宿主 491 绿，`api::` 130 / `model::` 37 / `svc::` 104）。
    - **为什么不是"勾两个框"**：`DesiredState::enable_smart_split` 一次动五格——
      `policy.private_direct`、`policy.cn_direct`、`policy.final_outbound = Proxy(tag)`、
      一个同 tag 的 `urltest` 组、`dns.overseas = [overseas_upstream(tag)]`，外加三条 builtin
      rule_sets（`geosite-cn` / `non-cn` / `geoip-cn`，从 `builtin_rule_sets()` 里**筛**出来，
      不另写一份 URL）。这四扇门的报错面各自盯着其中一格：只勾 `cn_direct` 而不带 rule_sets →
      sing-box `route.rs::require_rule_set` 拒；`dns.overseas` 空而链要用 `$overseas` →
      `gen::oxidns::check_available` 整份渲染失败，同时 `doctor::dns_leak_no_overseas` 是 Fatal；
      `final_outbound` 指向一个不存在的组 → `validate()` 拒。少任何一格的屏幕都是"主路第一步
      末尾自相矛盾"，正是 §11.3 D 要防的那种。
    - **组的形状一处定义两处用**：`overseas_upstream(tag)` 既是 `Dns::default()` 里那条境外上游
      （tag 走 `proxy`），也是这一档新写的那条（tag 走这一档的组名）——"出厂默认"与"智能分流"
      因此是同一个形状，不会漂成两份。测试 `the_default_overseas_upstream_and_the_smart_split_one_are_one_shape` 钉这条。
    - **组名撞车让位不让路**：机场里真有一个叫 `auto` 的节点是可达的（G12 那套标识符字符集允许），
      而 `validate()` 不许策略组 tag 与节点重名。`next_smart_split_group` 于是给 `auto-2`，
      回执念的是让位之后的名字。`None` 只留给"一个出口都没有"（`nodes.is_empty()`）——
      那种机器上把兜底出口指向一个空组，恰好把"新装不断网"那条判据弄坏。
    - **`block_ads` 与 `bittorrent_direct` 一个字不碰**：这两格是"人明确选过的立场"，
      静默替人打开就是暗改，与这一档"带回执的自动"是两回事。
    - **geo `.dat` 在位判定不起进程**：`observe::geo_data_present()` 只看 `GEO_DATA_DIRS` 那三个目录里
      有没有"同一个目录两份都在"（`geoip.dat` 与 `geosite.dat` 分在两个目录算没有），纯
      `Path::is_file()`——第 22 条那条 `fetcher.argv() == ["curl","wget","ip","ip"]` 因此没动。
      它查不到 dae 查找序的第一格（配置文件同目录），因为 dae 的落点还没进 `Layout`（#56 前置），
      偏的是保守那一侧：宁可多说一句"这台机器上还没探到"。两支句子由注入的 `geo_on_disk: bool`
      分开，两支都在真机上取到过（见 `BLUEPRINT.md` §11.8）。
    - **第二次导入绝不改档**：闸门还是第 22 条那个 `fresh`（磁盘上有没有 `state.json`）。
      回执分两支——要么"默认怎么走我也替你设成了智能分流…"，要么
      `group_membership_note`：新增的名字不在兜底组 `members` 里时**点名**，不静默补成员
      （那是人编辑过的那一份）。真机核对：盘上四个节点，`auto` 的 `members` 仍是导入那一刻的三个。
    - **三份资产的接线**：`#sub-policy-link` 只在 `value.default_set === true` 时亮，
      每次按「导入订阅」先收回、`resetSubReceipt()` 再收回（`setSubDefaultLink(` = 一个定义 +
      三个调用点，`assets.rs` 钉住），而"默认怎么走"那四个字在导航 label、那个按钮、`PAGES.policy`
      页题三处逐字相同——点它要落到说话的那一屏。

24. **看板 `GET /api/board`：一屏之内的数全说得出取数口，而取数口决定了这一屏不许发任何命令**
    （2026-10-12 落地，task #94 / #101 = U5 的 B 那一半；全量宿主 504 绿，`api::` 137 / `svc::` 110 / `model::` 37）。
    - **两条取数口，一条命令都不发**：硬件那几格是 `src/svc/board.rs` 用 `std::fs` 读 `/proc` 四份
      （`meminfo` / `loadavg` / `uptime` / `net/dev`），解析层那四格读 oxidns 自己那个**只绑回环**的
      管理口（`gen::oxidns::API_LISTEN` = `127.0.0.1:8053`）。这与 `svc::observe` 恰好相反，而且必须相反：
      这一屏在路由器上会被反复刷新，发命令就要选 `df` 还是 `free`、要处理措辞随版本变。
      断言 `the_board_is_behind_the_gate_and_sends_no_command` 钉 `runner.argv()` 恒空。
      磁盘因此**没有读数**（`statvfs` 不在 std），那一格是一句解释，措辞只住 `board::DISK_NOT_READ` 一处。
    - **每格单独降级，且"没读到"分两层说**：管理口整口不通时，四格各写一句"原因见这张卡最后那一句"，
      那串 100 多字符的 `Connection refused (os error 111)` 在卡底只出现一次（真机截图上四格各抄一遍是噪声）；
      单格自己失败（管理口活着、这一格回 404）时那句原因必须就地写出来。
    - **时刻与单位只能有一处来源**：`taken_at_epoch_ms` 由后端盖，面板只按时区**显示**它；
      字节换算只在 `board::human_bytes` 一处，浏览器不再除一次 1024；占比只按上游给的 `share`/`query_share` 画，
      不在这边用 `count / sample_size` 重算。
    - **判空的口径由格子交回，不由卡片收尾那句自己猜**：`paintBoardCell` 每格回 `"live" / "empty" / "missing"`，
      `readBoard` 按它把最后那句分三档。上一版是拿 `dns.records_enabled === false` 直接推"上面四格是空的"——
      那是**把期望态当现场读数**（同一条纪律在「DNS 记录」屏也钉过），真机现场（盘上关着、跑着的那份还在记）
      画出来是一屏自相矛盾。同一条界还管着空库那一格："前 N 名"只在它真画了行时才说，
      因为全新 DB 上 `rcode` 会回 `sample_size: 0` 外加一行 `_UNKNOWN`。
    - **连接层那一格是空的，而且这句写在屏幕上**：`这一屏给不出「哪一条连接走了代理」`——#69 / G4 定案前
      从数据面日志某一行反推"这条走了代理"是明令不许的做法，所以那一格宁可空着并说明为什么空。
      断言同时钉 `!js.contains("走了代理")`：面板不许宣称知道。
    - **`plugins` 那一行的头条与那根条同口径**（`query_total`，不是 `matched`）：理由与逐字实测记在
      `docs/reference/oxidns-plugin-args-verified.md` 那条聚合形状的第 5 点。
    - 三支真机验收（有数 / 管理口不通 / 期望态关着，含关着而四格空与关着而四格有行两种现场）与
      被推翻的三条写法在 `docs/BLUEPRINT.md` §11.9。G 那一格（三台引擎在位 / 版本 / 门）另起 #102。

25. **看板「三台引擎在不在」：三行里只有一行读得到数，所以另两行的"为什么读不到"是产品文案**
    （2026-10-12 落地，task #102 = U5 的 G 那一半；全量宿主 506 绿，`api::` 138 / `svc::` 111）。
    - **仍然一条命令都不发**（第 24 条那条断言覆盖这一格）：oxidns 那一行复用这一轮**已经取到**的
      `/api/health`（进程活着才答 ⇒ "读到了"即"在位"）与 `/api/control` 的 `reload.running_version`
      （它加载进内存那份配置的 SHA-256）。**dae 没有任何可查口**（控制面是进程内对象，listener 只在它
      自建 netns 里绑 tproxy 口，宿主不可达，`Layout` 里也没有它的落点），主路由**还没接通**——
      于是这两行只能写"读不到 + 为什么"，措辞各只住 `svc::board::{DAE_NOT_READ, ROUTER_NOT_READ}` 一处，
      面板只抄不写。断言 `!js.contains(DAE_NOT_READ)`：那句话不许在两份资产里各存一份。
    - **"面板要的那一份"与"解析记录"屏共用同一个 helper**（`Api::oxidns_wants()`：
      `gen::oxidns::render(state)` 那段文本 → SHA-256，渲染不出来则把 detail 带出去）。
      同一条界第 99 条已经立过（面板的生成路径要跟上 CLI），这里是它的第二处落点：
      同一台机器上两屏不许为同一个数说两句话，也不许二次读盘。
    - **`applied` 是三态，`null` 不许画成"没对上"**：渲染不出来 / 没读到 running_version / 管理口不通，
      三种来路各写各的一句人话。真机四支（渲染不出来、`false`、`true`、整行没读到）逐字在
      `docs/BLUEPRINT.md` §11.10，其中最硬的一条核对是：guest 上 `zboxd … --target oxidns render`
      那份文件的 `sha256sum`，与面板 `wants_version` 给的是**同一个数**——两条独立路径算出同一个摘要。
    - **"版本只用来报，不用来放行"写在卡底**：要不要下发归「让它生效」，合不合规矩归「诊断」。
      所以 §8 那条"升级零影响"的契约没被这张卡破掉，而"门过不过"（`dae validate` / `oxidns check` 两条命令）
      按上面第一条仍然**不在看板上**。
    - **两句解释要过"人话"这道门**（`the_not_read_explanations_stay_in_plain_words`）：按字符禁掉
      `` ` ``、`::`、`.rs`、`_`、`netns`、`proc`、`tproxy`、`**`，并要求每句都出现"不是"——
      即每句都得把"这一格空着"与"这台机器没在跑"分开。这是"用户思维"那条改向（§11）在措辞层的落点。

26. **设备那一屏：卡片住在哪一栏，就是它那一档——所以两条改档路径必须落到同一个函数**
    （2026-10-12 落地，task #96 = U6；全量宿主 507 绿。蓝本 §11.3 C 写的"三栏"被模型推翻成**四栏**，
    理由与逐字真机读数在 §11.11）。
    - **这一屏没有序号、没有上移下移**：`devices[]` 的数组序在生成物里没有语义（每台一条独立规则、
      源地址互不重叠、例外整体排在域名名单之前），画序号就是给用户"排在前面的先匹配"的假象。
      断言按禁回归钉：`!js.contains("renumberRows(\"device")`。名单里顺序真有语义的
      `policy.domain_rules[i]` 那一格继续用行号，两屏的界在 HTML 里各写一句。
    - **拖动与下拉是同一件事的两个入口**，不是两种状态：`.dev-grip` 起手的五个 handler 与卡片那一格
      的 `change`，最后都走 `putDeviceCard(card, lane)`（它同时改 DOM 落点**和**那一格的 `value`）。
      留两个入口的理由是触屏与键盘做不了 HTML5 拖放；合成 `DragEvent` 只验 handler 接线，
      原生起手那一下要真鼠标/真触屏才验得到（这一句写在 §11.11 的边界里，不许说过头）。
      `dataTransfer` 只带一个无内容记号，地址与 MAC 绝不过它。
    - **收集走 `collectDeviceLanes()` → 同一个 `collectRows()`**：栏序当数组序不改变任何生成物，
      而 412 逐条重放那份现场（身份 = 改之前的 IP、`patch`、`removed`）不许换地方算。
      `mod.rs` 那条四表断言因此同时钉两处：出口函数名，以及它内部必须调 `collectRows`。
    - **认不出来的那一档不丢、不改、不装**：`li.unknown` 的卡片暂存「智能分流」栏 + 一句 `.dev-warn`
      说明为什么，用户不动它就原样带走（`mode` 是 serde 枚举，非法值在 PUT 那一层就被拒，
      这一支真机造不出来，只有编译期断言钉它）。缺 `mode` 那一格时补 `global`：四档里只有它什么都不做，
      猜别的档会让一次没动过的保存凭空多出一条规则。
    - **"改一格 → 配置里那一行跟着变"这一格第一次有了逐字对照**：拖进「不接管」= `zbox.nft` 多一条
      `ip saddr … return`，下拉改「不科学」= dae 的 `routing {}` 多一行 `sip(…) -> must_direct`；
      而「全局」在基线兜底出口（直连）下保存仍成功、回执与「生成预览」给同一句"没有可去的策略组"。
      这三条都是真机 `GET /api/preview` 取回的原文，不是从渲染函数推的。

27. **「默认怎么走」那一排：三档只是那两格的快捷写法，所以读数必须由那两格反算**
    （2026-10-12 落地，task #100 = U4b；全量宿主 508 绿。形状与六支真机读数在蓝本 §11.12）。
    - **不造第三个状态字段**：模型里没有 `policy.mode`，生成物里没有一行需要读它。
      这一排写的是 `policy.cn_direct` 与 `policy.final_outbound`，读回来的也是这两格
      （加 `groups[]` 有没有可指的 tag）。断言两头都钉：`chooseModeSwitch` 体内必有那两格、
      **必无**另三格（私网 / 广告 / BT），且 `policy.mode` / `default_mode` / `modeSwitch:`
      三个造字段的名字一个都不许出现。往状态里塞一档，就是凭空多出一份没人消费的真相。
    - **读数由格子算，不由"上次按了哪个"记着**：`paintModeSwitch()` 必来自 `modeSwitchNow()`。
      人直接改下面的框，三颗就**都不亮** + 那句"下面那几格被单独拧过"——
      硬选一档会让按钮与它下面的框当场打脸，那是这一屏最容易被忽略的失效。
      `fillForm` 末尾（读盘之后）与 `pol-final` / `pol-cn-direct` / `group-list` 三处变更都触发重算。
    - **缺组那一支：不许静默。** 「智能分流」「全局」要有可指的组 tag（取自 `#group-list` 现值，
      不是取自盘上），一个都没有时**一格都不改**（`formDirty` 仍是假的）而只把那句出路换成警告色打出来。
      "按了没反应"是这一屏最坏的失效，比"按了改错"更难被发现。
    - **它只保证那两格一致，不保证这一档整份成立**（记在边界里，不修）：U4 那条自动改五格
      （还补 `dns.overseas` 与 rule_sets），这一排是手动档、不补那三格。实测缺境外上游时
      `render --target oxidns` **rc=1** 且那句原因点名"兜底出口=策略组 auto"——
      会在「生成预览」/「诊断」出声，不是静默泄漏，但 UI 没提前说要点完这一档还要补哪儿。
    - **排在那四格之前是断言**（`switch_at < cells_at`）：第一眼看见的必须是"挑一档"，不是"一堆框"。
      同一屏上"顺序即优先级"那句话只对下面的域名名单成立，两件事在 HTML 里各写一句界。

28. **校验一次列全：清单只有一份，排版只有一处，屏幕上必须真排成多行**
    （2026-10-12 落地，task #97 = U7；全量宿主 515 绿。形状与七支真机读数在蓝本 §11.13）。
    - **`validate()` 交的是清单不是头寸**：`Result<(), Vec<String>>`，22 处检查各 `push`，
      不再"命中第一条就 `return`"。结尾**保序去重**只压字面完全相同的那几条
      （两个都没填 tag 的组、两台同名同址的坏设备）；各带节点名的报错压不住也不该压，
      几百个坏节点那一份靠 `describe_problems` 的 12 条上限收口。
    - **清单的顺序 = 检查顺序 = 屏序**（网卡 → 节点 → 组 → 规则集 → DNS → 名单 → 设备），
      不按字典序重排。这一条是"用户思维"那把尺的形状要求：人顺着屏幕从上往下改，
      第一条就该是最上面那一格，改一条少一条（真机第 3 支量的就是这个）。
    - **排版只住一个地方**：`model::config::describe_problems`——单条不加帽子，
      多条 `共 N 处：` + 一行一条 `1）2）3）`，超出上限明说"…另有 M 处同类"。
      四条出口（`state::parse`、`zboxd doctor` 的 `invalid_state` finding、`gen::singbox` 那道门、
      `api` 的 400）共用这一份，所以 CLI 与面板对同一份坏状态**说同一句话**；
      每面各有一支断言钉"共 N 处 + 编号 + 屏序"，少任何一面都会在编译期红掉。
    - **面板的 `white-space: pre-line` 是判据的一部分，不是美化**：`<p>` / `<li>` 默认把换行折成空格，
      没有它"一次列全"送到屏幕上仍是一条长句。`assets.rs` 钉 `#save-out` 与 `ul#findings li` 两条规则，
      钉 `setSaveNote` 只写 `textContent`（体内出现 `.replace(` 即失败——把换行换掉等于取消一次列全），
      并钉 400 那一支把 `errorText(result)` 原样递进来。真机读数带的是**量出来的行数**
      （`createRange().getClientRects().length`：一处 = 1 行，三处 = 7 行），不是"看着像多行"。
    - **它管不到渲染层**（定 §11.12 那条界）：`dns.overseas` 空、rule_sets 缺这类"缺料"是
      `gen::oxidns` 渲染时报的，`validate()` 过了仍可能渲染不出——那一半归「生成预览」与「诊断」。
      因此「默认怎么走」那一排**不**改成一次写五格。

29. **日志那一屏从一来源改成五来源一卡：六种状态各有各的一句话，措辞只住 Rust**
    （2026-10-12 落地，task #98 = U8，子任务 #103/#104/#105/#106；全量宿主 519 绿，界面形状在蓝本 §11.14）。
    - **先记被推翻的那条前提**：这一屏原本按"三个日志来源"设计（数据面 / 监督者 / 面板自己）。
      逐条实测之后只有两处成立：`oxidns` 确实自己写一份、而且**按天轮转**（盘上真名是
      `oxidns.log.<UTC 日期>`，`oxidns.log` 那个名字在跑过一天之后根本不存在），`dae` 那一份
      **压根没有落点**（ZBox 给它的配置里只写日志级别，不写往哪写）。所以"三来源"这个说法本身
      就是错的，屏幕上如果只有三张卡，人读到的是"接管层没日志"，而真相是"接管层不归我们落"。
      现在五张卡：`kernel` / `supervisor` / `panel` / `oxidns` / `dae`。
    - **`state` 有六个值，一个都不许合并**：`ok`、`empty`（文件在、0 字节）、`missing`（还没这个文件）、
      `journal`（这台机器上走的是服务运行记录，systemd 那两份 unit 没把 stderr 落成文件）、
      `no_home`（我们的配置没给它落点）、`unreadable`（在、但读不出）。前四种都不是故障，
      画成"空"或画成红的就是这一屏最坏的那一种假话；认不出来的 `state` 原样画那个词（`logState`
      的兜底分支），因为"上游将来多一种落点"应当被看见，不是被静默归进"空"。
    - **措辞只有一处真相**：卡片标题、那句"这一份是谁写的"、以及"为什么这一格是空的"三句全部由
      后端给（`one_log` + 三条 `*_NOTE` 常量）。`assets.rs` 反过来钉 HTML 与 JS 里**不许出现**
      那三句标题——抄一份就是第二处真相，而后端改词的人不会想到回来改资产。
    - **`GET /api/logs` 仍然没有参数位**，一次给五张卡（同一次读取 = 同一个时刻，这是"一来源一卡"
      唯一的前提）。过滤框因此只能是**纯前端重画**：它只动已经读到的那几行，每打一个字问机器一次
      就是这一屏明令禁止的轮询。这条界靠 `readLogs(` 只出现 4 次钉住（一个定义 + 开局 /
      动过数据面之后 / 人按下那一下三个时刻）。
    - **路径一个字节都不给**（第 1 条硬约束在这一屏的落点）：卡上只写文件名，`file` 字段就是
      `file_name`；只有"压根没有落点"那一路（`no_home`）连文件名都给不出，写的是 `null`。
      断言拿
      `body.contains(layout.root)` 反向钉，跨平台造 `unreadable` 用的是"把落点做成目录"，
      不是 root 下造权限位（Windows 上造不出来）。
    - **真机那一轮抓出来的不是样式而是状态**：退出登录把五张卡清掉了，`#logs-filter` 却还是可输入的，
      而它在 HTML 里的开局态是 `disabled` —— 屏上会同时写着"未读取"和"只画带某几个字的行"，
      后者已经没有对象。修在 logout 那一段（清词 + `setLogsEnabled(false)`），并且编译期钉成一条：
      退出那一段必须同时含 `lastLogs = null`、`renderLogs()`、清词、`setLogsEnabled(false)` 四件事，
      而 HTML 那枚 input 必须自带 `disabled`，好让 JS 的开局态有对得了的参照。
    - **guest 真浏览器一轮**（一次性根 `/root/u8`、`--init openrc --bind 192.168.219.128 --port 8123`、
      产物 md5 `4c64a22c12849ab781390dc9ed4bf907`）：现场是"kernel 12 行 / supervisor 0 字节 /
      panel 无文件 / oxidns 两份轮转分片（01 与 02，02 有 166400 B）/ dae 无落点"，
      五张卡一次到位——徽标 `已读取 / 文件是空的 / 还没有文件 / 已读取 / 没有落点`，
      顶部 `有内容 2／5`，`取数 …｜5 张卡说的是同一时刻`；oxidns 那张取到的是 **`oxidns.log.2026-10-02`
      的末尾 1023 条**并明说"前面还有没给的（只取最后 65536 字节）"（glob 走对，01 没混进来）；
      过滤 `ERROR` 时只有 kernel 剩 1 条、oxidns 说"这一份里没有带「ERROR」的行"（按卡各画各的），
      全场零命中时徽标仍是 `2／5`（它数的是状态，不是过滤结果）；420px 窄屏五张卡照常堆叠、
      正文各自滚动；再按一次读取时刻从 `13:44:41` 变到 `13:44:50`；退出后卡 0 张、框回到
      禁用 + 空词。全程 `consoleErrors` 为空。**没验到的一条要说清**：`journal` 那一路只在单测里走过
      （guest 是 OpenRC），真机上那一句仍未在屏幕上见过。

30. **默认档全流程走查（U9）钉下来的三条界**
    （2026-10-12 落地，task #108；全量宿主 521 绿，四处真机读数在蓝本 §11.15）。
    - **一个名字在一屏上只指一件事**：设备那四栏原来有两格写着「智能分流」「全局」，
      而这两个词在这一屏另有其主——它们是「默认怎么走」那颗三档开关的档名。
      `DeviceMode::Global` 说的只是"这台跟着那一排走"，那一排设成不科学时它就是不科学，
      把栏题写成「智能分流」等于替用户预设了默认。于是栏题与卡片下拉那四句一起换成
      「跟着默认走 / 强制走代理」，并由 `assets.rs` **双向**钉：四个新栏题在 HTML 与 JS 逐字相同，
      且 `<h3>智能分流</h3>`、`<h3>全局</h3>` 在设备那一屏**不许出现**。
      人话词表只有一份这件事同理：档名只写在资产里，Rust 侧 `DeviceMode` 仍是四个英文变体。
    - **服务端的内部值不许原样上人脸**：`etag === "absent"` 是新机器的事实，不是给用户读的词。
      两处（连接回执、概览"磁盘指纹"那一格）各给一句中文，且反向钉 `磁盘指纹 absent`、`absent（`
      不许回来。**同一张卡片上那两格还得互相自洽**：`reload_pending=true` 有**两种**来路，
      只按布尔值说话就会对一台"存过盘但从没下发过"的机器（正是主路第②步之后的样子）写
      `是（内容与已知良好修订不一致）`，而旁边那格正写着"已知良好修订：尚无"——前一句是在指控它失败过一次。
      分法照搬「分流梯级」那一屏**同一个判据**（看 `last_good` 在不在），不另造口径。
      **连带改的是落点**：带着令牌打开面板的人要看"机器现在的样子"，
      但对一台一个出口都还没有的机器，那句话是空话——主路第②步在下一屏。
      所以 `setView(noEgress ? "nodes" : "overview")`，而 `source === "broken"` 不参与这个判断
      （读不出来的状态送去「订阅与节点」，那一屏此刻没有内容可看）。
    - **窄屏的导航只许占顶上两行**：侧栏在 56rem 以下靠换行摊成 439px 高，把「订阅与节点」的
      主按钮顶到 y=911（视口 900）——手机上新用户第一屏只有导航。现在是一条只能横向滑的轨
      （实测 35px，主按钮回到 564）。两处形状编译期钉住：`#nav` 那条里必须
      `nowrap` + `overflow-x: auto` + `flex: 1 1 100%` 且**不许** `flex-wrap: wrap`；
      高级那一层摊进轨里必须写 `#nav-advanced:not([hidden]) { display: contents }`，
      `:not([hidden])` 是必须的——`display: contents` 会盖掉 `[hidden]` 的 UA 规则，
      少了它就变成"关着档也看得见高级屏名"。程序换屏时那一格要滑进视野（`revealActiveNav`，
      恰好两处），且用 `inline: "nearest"` 不用 `"center"`：后者会把左端的分组标签滑掉半截；
      滑轨判断 `scrollWidth <= clientWidth` 提前返回，宽屏那条竖轨不许被动。

31. **报错的"动作标签"属于出口，不属于共享串**
    （2026-10-12 落地，task #109，收的是蓝本 §11.13 明写"留待下轮"的两条；全量宿主 522 绿，
    真机读数在 §11.16）。
    `describe_problems` 那一份清单是四条出口共用的（CLI 读盘、面板 PUT 的 400、`doctor`、
    生成器那道门），所以**它自己不许带动作标签**：`state::parse` 那句 `期望状态不成立: ` 前缀
    与 `GenerateError::InvalidState` 的 `#[error("期望状态不成立: {0}")]` 一起摘掉——每条出口
    本来就已经有一句框（面板 `保存被拒（HTTP 400）：`、doctor 那条 `期望状态本身不成立，生成器已拒绝`、
    CLI 是 `zboxd: <路径>: `），叠上去屏幕上就是 `保存被拒（HTTP 400）：期望状态不成立: 共 3 处：`
    这种两层标签，还夹着一个漏出来的 ASCII 冒号。
    摘掉之后"总数那顶帽子"要自己带主语（**`这 N 处不成立，要改：`**），否则 CLI 那一侧前面只有
    文件路径，光"共 3 处："没人知道说的是什么；只有一条时照旧不加帽子。
    钉法是双向的：清单里出现 `期望状态不成立` / `保存被拒` 任一动作标签就红（出口各一条）。
    **同一条规矩的另一半是说清"撞的是谁"**：组撞节点与组撞组原先共用一句"与节点重名"，
    人就跑去「订阅与节点」找一个不存在的重名——现在撞组写"与另一个策略组重名"，
    而**空 tag 不进这道比较**（它谁都没撞，只是没名字，上一行"策略组缺少 tag"已经报过了）。

32. **说明书是另一份接口，要对着现场量，而且要钉住"对得上"这件事**
    （2026-10-12 落地，task #110，读数在蓝本 §11.17；全量宿主 523 绿）。
    连接屏那张「这条路怎么走」讲的全是"别的屏上有什么、按钮住在哪儿"，所以**它自己不会红任何测试**：
    屏改名（U2/U9）、按钮搬家、档位收成三颗（U4b）——每一轮都只改被指的那一处，指人的这一处
    安静地留下成谎话，而读它的正是最没能力自己去核对的那个人。本轮量出三句：③ 还写着三档开关之前的
    "这一屏默认那份名单，通常不用动"（出厂那颗其实是「不科学」）；⑤ 说"保存到磁盘……三步都在这一屏"
    （真机 `{"inTopbar":true,"insideAnyPage":false}`，`#save` 住在 `header.topbar`，不在任何
    `section.page` 里）；顶上那句"其余各屏都住在开启高级模式后面"漏了「现在怎么样」那组三屏。
    **改法之外更重要的是钉法**：五步的 `<dt>` 屏名要逐字等于侧栏导航 label **且顺序逐项相等**、
    说明书用「」点名的按钮要 `>名字</button>` 真有其物（两边都查，只改一处就红）、
    三档档名要逐字是 `>不科学<span>` 那三颗、"出厂是第一颗「不科学」"这句要留在屏上，
    再加反向钉三句谎不许回来。一条判据顺带被这条纠正：面板的控制台**永远不会**只剩"报错为空"——
    浏览器自己会发 `/favicon.ico`，而静态表只有三条字面路径、门又排在路由之前，
    于是那条 401 是**正确行为**；以后"控制台干净"要先排掉 favicon 再说。

33. **对照要么全量要么不算对照；例外必须点名，还要自证理由**
    （2026-10-12 落地，task #111，逐条判定在蓝本 §11.18；全量宿主 525 绿）。
    上一条讲"要钉住对得上"，这一条讲**钉一半等于没钉**：同一类引用（一句"按「某颗按钮」"）
    在整份 `panel.html` 里一共 26 处，只钉那一张卡的话，其余 25 处照旧会安静过期。
    做法是把控件文案表**从页面自己收集**（侧栏分组题 / 导航 label / 按钮 / 卡片标题 / `<dt>`），
    再去掉 HTML 注释后逐个「」比对——收集器不是手抄的名单，所以改控件的人不必记得改测试。
    **两条形状上的教训**：① 第一版留了个宽松逃逸（"这词在 `panel.js` 里也逐字出现"就算过），
    写完就发现它把整条测试写废——按钮改名时 JS 回执里那句旧名多半还留着，每次都能从后门溜走；
    例外必须是**点名的四条**，而且每条当场自证那句理由仍成立（「没有落点」要真在 JS 里画得出来、
    「一键复制」要连"机场控制台"那半句在场、看板那两句要连"给不出 / 明令不许"在场）。
    ② 测试**先证明会红**再谈它绿：临时把 `#sub-import` 改成别的文案，断言必须**点名那个词**而不是
    报"某个 assert 失败"；还原后复核 `grep -c` 为 0，别把改动留在资产里。
    本轮一处资产改动（`assets/panel.html` 把「三档开关」改成「默认怎么走」）⇒ 重编 push + 真机一轮
    （一次性 root `/root/u12`、面板 8132、CDP 9350 纯只读）：产物里 `三档开关` **0 次**、`默认怎么走`
    **9 次**；普通档 8 屏 / 高级档 16 屏；只有 3 条引用只在高级档够得着且都住在自己那一屏 ⇒ 不是谎；
    排掉 favicon 的 401 后控制台 **0 条真错**。
    **措辞/断言轮要分清"改了要不要上机器"**：只动 `#[cfg(test)]` 就不必重编 push；
    但只要动了资产文件，哪怕只改一个字，也要重编 + nc 复核三条字面路径 + 真机一轮。

### 7.3 建面板之前必须先补的三条（2026-10-05 已落地）

1. **状态的规范落点 + 版本门 + 原子写** → `src/model/state.rs`。落点是 `Layout::state_path()`
   （`<root>/state.json`，跟着 `--root` 走），`install/apply/rule-sets/enable-hijack/doctor`
   在不给位置参数时就读它；给了仍用给的，彩排和复现要能指向别的文件。磁盘形状 = `DesiredState`
   的全部字段 + 一个 `schema` 键（`#[serde(flatten)]`），**没有这个键的老文件按 v1 读**，
   所以这道门不破坏任何已存在的状态；比当前版本新的直接拒绝——要挡的是"旧 zboxd 认不出的字段
   被默认值顶掉、再原样写回磁盘"这种静默改写，它比读失败严重。写盘：唯一临时名（带 pid）
   → 写 + fsync → rename；Unix 上权限在 `create` 就给到 0600，因为状态里的订阅 URL 带个人令牌。
   **迁移必须排在反序列化之前**（`STATE_SCHEMA = 2`）：serde 对未知字段是静默忽略的，先反序列化
   就把 v1 的三档名单丢成了空列表，接着一次正常写盘就把用户名单清空——比读失败严重得多。
   v1→v2 按"拦截 → 直连 → 代理"的档序拼成 `domain_rules`，与 #18 的结论逐条等价；三种情况另处理：
   缺 `policy` 对象只盖版本号，v1 键与 `domain_rules` **同时**存在时报错而不是猜该丢哪一档，
   列表里有条目解析不了时点名是哪一档的哪一条。
2. **回收已删除的声明** → `plan_reclaim` + `Report::reclaimed`（`src/svc/ruleset.rs`）。
   判据只有清单：账本外的文件（用户手放的 `.srs`、别的程序的产物）一律不碰；tag 过不了
   `is_safe_rule_set_tag` 时只销账、不拿它去删文件。回收排在一轮同步的**最后**，所以硬失败那轮
   什么都不删。
3. **硬失败也打印部分报告** → `SyncFailure { report, source }`，`sync_rule_sets` 先打印再报错。

还欠一条不属于这三步的：**谁往 `<root>/state.json` 写第一份**。`install` 目前只读不写，
新装的机器上面板要读的那份文件得由某一步落地——连同 §6 第 5 步探到的网卡值要不要回写磁盘，
一起在建面板（task #6）时定，不要在这里顺手加一条 `save` 命令了事。

*2026-10-05 读代码定下来的两条事实*（不是新决策，是把"某一步"的范围收窄）：
`fill_network`（`src/bin/zboxd.rs:217`）只把探测结果用在**内存里**，全仓库对 `state::write` 的调用
只有两个 example（`dump-snapshot`、`ingest-subscription`，都写到自己 out-dir），所以探测值从不落盘，
第一份状态必然出自面板。`src/model/state.rs::a_panel_save_lands_on_the_canonical_path_and_keeps_the_row_order`
把面板这一侧的契约钉住了：落点 = `Layout::state_path()`（与 CLI 默认读的那一份同一个文件）、
目录不存在也要写得下去、磁盘带 `schema` 键、`domain_rules` 的**行序**逐条保住（拖动排序是有语义的
保存动作，任何"顺手排个序"的写法都会红）、落点上 0600、不留 `state.json.tmp-*`。
这条测试在 Windows 宿主（走 `remove_file` + `rename` 那段非原子分支）与 musl guest（走真 `rename`）
都跑过。面板还要单独处理一件事：一旦把 `network` 写成非默认形状，`topology::fill` 就永久走
`How::KeptExplicit`（`src/svc/topology.rs:190-191`），所以界面上得留一个"恢复自动探测"的出口，
也就是能把 `network` 写回 `Network::default()`。

这一条 2026-10-06 由 `src/api/mod.rs` 接上了：面板侧写的就是 `state::write` 到 `Layout::state_path()`
那一份，读也是同一份，所以 `zboxd apply` 与面板保存的必然是同一个文件。配套把
"修订与快照放在哪儿"收进 `Svc::store()` / `Svc::target()`（`src/svc/mod.rs`），CLI 里的本地
`fn store` / `fn target` 已删——两处各自拼路径的话，面板写的修订表和 CLI 回滚读的可能不是同一个目录。
引擎的失败在这里一律走 `ApplyError` 的 Display：四个业务变体（校验没过 / 已回滚到某修订 /
没有可回滚点 / 回滚也失败）自带分级文案，落盘与清理失败则走 `ApplyError::Store`
（`#[error(transparent)]`，`store.rs` 的文案本来就是中文的"快照写入失败: …"），面板拿到的
是"下发失败：<原话>"而不是 Rust 类型名。单测两个平台都跑过：宿主 251 绿（`CopyTarget` 分支），
musl guest 252 绿（多的是 `#[cfg(unix)]` 那条 0600 断言，走真 `rename` 与 `SymlinkTarget`）。

诊断页直接渲染 §5.1 的 `Finding` 列表（结论 + 可粘贴命令），不要把排查文档抄成静态 FAQ。
另：两份参考面板都监听 `0.0.0.0:9099` 且**完全没有鉴权**——局域网里任何设备
都能改网关。ZBox 面板必须有登录（见 §8.4），这一点不属于可选项。

## 8. 尚未确认的点

1. ~~sing-box 版本与配置 schema~~ **已定案**：按 **1.14.2** 的 `option/*.go` + 二进制自带的
   `sing-box schema` 逐字段对齐，落成 `docs/reference/singbox-1.14.2-schema.md`。原 §8 的 5 条
   待实测项里，第 1、2 条（省略 `transport` 等价 TCP、urltest 没有 `default`）已在该文档 §11 定案；
   第 3、5 条（nft/ip rule 劫持层、DNS 劫持与 TPROXY 命中、回滚与重复下发幂等）**2026-09-30 在
   netns 拓扑逐条定案**，见该文档 §11.7；第 4 条（TUN + `auto_redirect` 对比）仍未测。
   生成侧的形状已经过三份配置的真机验收（baseline / 自建 full / 54 节点真实订阅，
   `check` + `run` 全绿，见 schema 文档 §11.3.1），fakeip 形状也另过一轮（§11.7）。
2. TPROXY 与 REDIRECT 的取舍：当前两者**同时**下发（tproxy 覆盖 TCP+UDP，redirect 作 TCP 兜底），
   nft 只把流量交给 tproxy 端口。是否改用官方现在推荐的 TUN + `auto_redirect` 待实测（同文档 §8.4）。
3. **IPv6 接管姿态** —— **2026-10-09 定案：按档位生成，不做一刀切**。原话"要不要补
   `table ip6 zbox`，等 v4 路径在 VM 上跑通再定"的两个前提这次都齐了：VMnet8 开了 NAT66
   （真 RA、真 v6 默认路由、公网 v6 零丢包），v4 路径也早已过完整验收，于是 `table ip6`
   那半边在这台 guest 上真加载过一次。落成的三档见 `network.ipv6_mode`
   （`src/model/config.rs`）与 `docs/reference/ipv6-kernel-measured.md`：
   - `off`（默认）：v6 完全交给主路由。生成物里**一个字节 `ip6` 都不写** —— 留一条空链会让人
     以为它也被管了。这一档必须由 DNS 侧堵泄漏（`dns.strategy` 得是 `*_only`），否则客户端
     拿到 AAAA 就 v6 直连出去。
   - `dns_only`：oxidns 双栈监听 + ip6 nat 链收 53，**连接层不接管**。这一档有一条明写的泄漏
     路径（模型层要求配 `*_only` 的策略），`zbox.nft` 里那条注释就是它。
   - `dual_stack`：`table ip6 zbox` 两条链 + `ip -6 rule/route` + 成对的
     `net.ipv6.conf.*.forwarding=1` / `accept_ra=2`。
   ⚠ 三条实测把原来的纸面设想推翻过：只写转发不写 `accept_ra=2` 会让**这台机器自己的** v6
   默认路由整臂消失且 15 分钟无自愈（§1 臂 A）；`[::]` 能不能收 v4 由 `net.ipv6.bindv6only`
   这个运维可改的 sysctl 决定，所以 v6 监听口必须显式开、不能省掉 v4 那一对（§2）；
   v6 的规则基线比 v4 少一行 `32767:`，doctor 判"劫持层在不在"不能套用 v4 那份（§4）。
   当时仍未测的是"经 v6 监听口进来的查询答得对不对"与"v6 设备豁免链在真流量上命中"（原 §3）。
   —— 这两条 **2026-10-11 已实测收口**（#65，netns 里造了一条真 v6 局域网）：AAAA 经 v6 监听口
   进来得到 `NOERROR + ANSWER: 0 + AUTHORITY: 1`，同一链上的 A 答 3 条，与 v4 逐字同形，
   所以 v4/v6 确实共用一条 `entry: main`；豁免由 `ip6 saddr != <lan_cidr6>` 那一条决定，
   非局域网源的查询根本不进劫持层。同时测出一条**比原设想更糟**的形态：`tproxy` 未命中时
   同条规则后面的 `meta mark set 0x1 accept` 不执行而链继续，包按普通路由转发出去——
   **divert 链本身不是防泄漏保证，上游没起时是静默泄漏而不是黑洞**（§8）。详见该文件 §8、§9、§10。
   fakeip 那一轮里 sing-box 确实会发 `fc00::/18` 的 v6 假地址，但那与这里的接管姿态是两回事。
   **P4-4（同日）可见性已落地**：诊断侧新增/收紧四条 code（`dns_port_not_listening_on_ipv6`、
   `ipv6_nft_table_missing`、`ipv6_divert_rule_missing`、`ipv6_forwarding_off`），面板概览把
   v6 那半边单列一行 `#hijack6`（表在不在 / 策略路由 / `forwarding(all)` / `accept_ra(all)` /
   v6 在听的口计数），`网络` 屏三档下面写了"各自落在哪"。**真机走的是 414 版二进制 + 一次性 root
   + `GET /api/doctor`**，三档逐条见 `ipv6-kernel-measured.md` §7。两条纪律同时钉进了测试：
   判据**只报 DNS 那一个口**——dae 的 `tproxy_port` 后来实测过（#65/§10）：它自己就绑了
   `0.0.0.0` + `[::]` 两只口，但都在它自建的 `daens` netns 里，宿主探针永远查不到，
   写成故障就是每条双栈配置误报一次；doctor 的 fix 与生成物末尾那两条 sysctl 键由
   `netstate::v6_host_pairs()` 同一来源出，改一边忘另一边测试先响。
   测试计数 **宿主 414 / guest musl 415**。
   **P4-6（2026-10-11）预览补上内核层那两份**：`GET /api/preview` 从两格扩到四格
   （`dae` / `oxidns` / `zbox.nft` / `netstate.sh`），后两份取的是 `gen::snapshot_files`
   用的**同一函数**，所以"面板看到的 = 下一次下发写进快照的"
   这一条有测试逐字钉着。扩格的直接理由就是 IPv6 那一档：`ipv6_mode` 动的正是内核层那两份，
   只给编排层时这一屏会在用户换档之后说"逐字没变"。真机那一轮（416 版二进制 + 一次性 root +
   真浏览器 + headless Chrome 1440x900）另外撞出并修掉四条，都是这一屏的判据本身：
   - **带查询串的面板打不开**：`parse_head` 把 `/?from=share` 整条当路径交给路由，于是首页返回
     401 的 JSON。修法是三条校验（`/` 开头、不含 `://`、不含 `\`）**之后**再剥掉第一个 `?`
     之后的一段——顺序反过来就等于给 `GET /?u=http://x/api/state` 开门，测试把这两种都钉住了。
   - **保存之后那一屏本来没有底色**：`save()` 成功后自己重读一次预览，而整页重载后的第一次保存
     里"上一份读到的"还不存在，那一次重读被当成本页第一次读，于是变得最多的那一次反而一个字都没标。
     现在写盘**之前**先取一份基准，标色就在保存之后那一屏上；面板的说明也改成"不用再按读取"
     ——原来那句"上一屏保存过一次再读"教的正是会把底色刷掉的那一下。
   - **差异区间用错了坐标系**：`changedRange` 返回的是两份里较大的那个行号，于是"删行"的那一次
     （双栈→不接管，v6 那半边整个收掉）拿着上一份的行号去标这一份，一行也标不上，而标题却写
     "第 28 到 54 行不同"。现在 from/to 只说这一份里该上底色的行，另给 goneFrom/goneTo/dropped
     说"上一份的第 X 到 Y 行是这一次去掉的"——删掉的行在这一份里不存在，画不出底色，只能在句子里说。
   - **`gen::dae` 里没有 `ipv6_mode` 的分支**，而面板那句三档落点写着"双栈接管再加 dae 的 tproxy 口"。
     预览里 dae 那一格逐字相同是对的，说错的是说明。现在那句话明写"dae 的 tproxy 口不进这份生成物的改动"
     （实测它自己就绑了 v6，口在 `daens` 里，§10），并加了一条断言：dae 那一格**应当**不随 `ipv6_mode` 变，
     哪天接上了先红的是它，要跟着一起改的是 `assets/panel.html`。
   退出登录同时清掉差异基准（`previewPrevious`），否则"未读取"会和"与上一页读到的那份相比"同屏。
   测试计数 **宿主 416 / guest musl 417**。
   **P4-5（2026-10-11）netns 里造出真 v6 局域网，#65 收口，#69 换问题**：三张 netns
   （网关 `v6srv` + 客户端 `lan2` + 宿主侧 `v6h`）跑出上面 §8/§9 那两张计数器表，
   顺带把 dae 的两条 per-iface 前提复现了一遍（`ipv6-kernel-measured.md` §10：
   `net.ipv6.conf.<lan>.forwarding=1` 与 `net.ipv4.conf.<lan>.send_redirects=0`，
   **与 `ipv6_mode` 档位无关**，所以 `off` / `dns_only` 档下 dae 照样要）。这一条当场核完并
   改掉一半：现役 guest 上是 `all.send_redirects=0` 而 `eth0.send_redirects=1`，
   证明内核这一条按接口读、`all` 那份不覆盖，所以 `gen::netstate::sysctl_pairs`
   现在**三档都**多写一条 `net.ipv4.conf.<局域网网卡>.send_redirects=0`
   （持久化的 `/etc/sysctl.d/` 那份仍只收整机级键——绑网卡的键进那个文件会在开机时被
   busybox `sysctl -p` 静默吞掉）。断言在
   `the_send_redirects_gate_is_closed_on_the_interface_not_only_on_all`。
   **没改的另一半是产品决定不是实测问题**：v6 的 per-iface 转发键现在只在 `dual_stack` 档写，
   而 dae 在哪一档都要它——"为 dae 破例写一条与档位无关的键"还是"由 doctor 报出 dae 起不来"，
   挂在 #69。#69 的形状因此被**换掉**：
   不再是"给 dae 补一个 v6 tproxy 口"（它自己开了，而且宿主 ns 的 `ss` 查不到），
   而是"宿主 ns 那条 `tproxy to :<port>` 对 dae 形态必不命中，命中不了就是 §8 那个静默泄漏，
   所以 divert 链与 dae 的 eBPF 接管到底谁负责哪一半"——这条现在挂在
   `ipv6-kernel-measured.md` §3 第 2 条，未决，不许在代码里当已决处理。
   面板文案与五处代码注释按这三条结论同步（`assets/panel.html` 三档落点那段、
   `gen::nft` 头第 5 条、`gen::netstate` 头、`gen::oxidns` 的 V6ONLY 理由、
   `doctor.rs` 那条"只报 DNS 口"的依据），并新增一条断言钉住"面板必须写出未命中的后果"。
   真机那一轮（改过 `assets/` 所以重编 musl 产物再 push，418 版 + 一次性 root + `--port 19099`）
   取到三条：面板那一段在浏览器里逐字渲染出来（`evaluate_script` 读 `.note` 的 innerText 对过）、
   `GET /api/preview` 的 `netstate.sh` 那一格里出现了新的回程注释与
   `net.ipv4.conf.<网卡>.send_redirects=0`、收尾复查现场与基线逐字相同
   （服务 started、六个现役监听一字未动、无 nft 表无 netns、`ip -4 rule` 三行 `ip -6 rule` 两行、
   `/var/lib/zbox` mtime 仍是 `2026-10-01 00:45:59`）。一次性件（`/root/round-418` 含令牌、
   `/root/zboxd-418b`、`/root/libtest_418`）已全部删除。
   测试计数 **宿主 417 / guest musl 418**。
4. ~~面板鉴权（MVP：LAN 绑定 + argon2 口令 + 会话 cookie）~~ **2026-10-06 定案**：令牌档而不是
   口令档——机器生成的 128 位令牌落 `<root>/panel.token`（0600），无登录端点、无新依赖，
   理由与全部约束见 §7.2 第 3 条。**LAN 绑定同日定案并落地**：默认取
   `network.lan_interface` 那张卡的 IPv4，可显式 `--bind <地址>[:端口]`；通配地址永远拒、
   本机没有的地址拒（拒在 `bind()` 之前）、拿不到就拒绝启动而不是退回 `0.0.0.0`，
   回环只有显式给才放行。规则与 guest 实测见 §7.2 第 6 条。这一条从"文档里劝"变成了
   "代码里挡"，§8.1 那句参考面板 `0.0.0.0:9099` 无鉴权的反例至此两面都对上了。
5. 是否要 `abox` 那样的自研 DNS 分流层——当前决策是不做，用 sing-box 内置 DNS。
6. ~~Alpine 侧的服务制品还没真跑过~~ **已定案**（2026-09-30，含一次真重启）。真写进 guest 的
   `/etc/init.d/zbox` 与真 runlevel 之后实测过：`zboxd install` 端到端 rc=0、
   `zboxd status` 打的现场对得上、`rollback` rc=0；制品是 `supervisor=supervise-daemon`
   那一套（原来的 `respawn=true` 是惰性的，见 schema §11.8.5），`kill -9` 工作进程后恰好
   `respawn_delay` 秒被拉回、七个入站全回来，pidfile 里是监督进程的 pid，`stop` 两个都杀并删
   pidfile（status 回 `stopped` rc=3）；runscript 里 `export` 的变量确实传到了工作进程
   （`/proc/<pid>/environ`）。内核日志轮转也真跑过：超限的 `sing-box.log` 变 `.1`
   （598,603 字节），新日志 899 字节后不再涨。
   **2026-10-06 同一套判据也走过了面板那一份**（`/etc/init.d/zbox-panel`）：staged 制品
   `sh -n` rc=0、装进 `/etc/init.d` 后 `start` 即监听、`kill -9` worker 被 respawn、
   绑定被拒时每次尝试一行进 `panel.stderr.log`、13 次后监督者自己退出（`status` rc=3）。
   这一轮把制品里的一处设计错误逼了出来并按实测改写了 runscript——细节见 §6 第 2/3 步。
   仍未走过的只有两格：`start_pre()` 里等默认路由那 30 秒的分支（现场每次都有默认路由），
   以及 systemd 那两份单元本身（这台 guest 是 OpenRC）。
   重启那一轮（schema §11.8.6）：`/etc/sysctl.d/99-zbox.conf` 确实被开机的 `sysctl` 服务读了
   （四项判据全部与 Alpine 自己给的值不同：`ip_forward` 1 对缺省 0、`all.rp_filter` 2 对 1、
   `default.rp_filter` 2 对 1、`all.send_redirects` 0 对缺省 1），服务随开机起来、
   `openrc default` 之后 13 秒 sing-box 就 `started (0.03s)`，七个监听全在，`doctor` 只剩
   "劫持层未加载"两条提示——这正是"重启回到未劫持落点"想要的形状。
   这一轮还顺手挖出一个静默故障（绑网卡名的 sysctl 键在持久化文件里对不上网卡名，busybox 的
   `-p`  rc=0 不报），已按实测改成"文件只落整机级键、绑网卡的由 netstate.sh 当场 `-w`"。
   早期那轮语法级检查（busybox `sh -n` rc=0、`openrc-run describe` rc=0、`command_args`
   的 `-c`/`-D` 都是绝对路径）仍然成立，只是现在被真行为覆盖了。
   剩下没真做过的只有包安装本身：对着 v3.21 仓库索引 `APKINDEX` 逐个确认过存在
   （`openssh`/`iproute2`/`nftables` 都在 main——这三个这台机器早就装着在用；
   Hyper-V 集成服务的包名是 **`hvtools` + `hvtools-openrc`**，不是 `hyperv-tools`，
   后者不存在——换成 VMware 之后这条只是留给别的部署目标的记录）。
   **2026-09-29 已定案的两点**：`-musl` 资产确实是静态链接且在 Alpine 3.21.3 上跑得通
   （`sing-box version` / `check` / `run` 全过），`file` 报 `statically linked`；
   实测 VM 已从 Hyper-V 换成 **VMware Workstation**（NAT/VMnet8、`e1000` 网卡），
   所以 `modprobe hv_netvsc` 那段作废，现在 guest 侧接口就是 `eth0`，
   sing-box 日志自己报 `updated default interface eth0, index 2` + 网关 `192.168.219.2`。
   虚拟网卡上的 GSO 表现仍要盯（见上文的 `QUIC_GO_DISABLE_GSO`）。
7. **rp_filter 的必要性只在"单网卡形状"上还没测**（2026-09-30 netns 实测削弱了原来的说法）：
   局域网与 WAN 分属两块网卡时，把它从 2 压回 1 也没丢包，TPROXY 照常命中（schema §11.7）。
   `sysctl_pairs()` 继续输出 2（放宽是无害方向），但**不再声称它被实测证明过**；真机单网卡
   若出现丢包，判据要写进 §5.1 的诊断项而不是改脚本猜值。
   重启那一轮另外量到两条语义（schema §11.8.6）：写 `conf/all/rp_filter` **不会**回头改已存在
   网卡的存量值（`eth0=0` 时写 `all=2`，回读 eth0 仍是 0），它只是判断时的地板；新网卡在创建时
   从 `conf/default/*` 继承。所以"持久化文件里到底要不要重复一条绑网名的键"这个问题，
   答案是看驱动出现的时机——不可靠，故不留。
8. **劫持层没有 counter**（netns 实测时撞到）：现场分不清"包没进链"和"进链后被 return 放行"，
   当时靠临时 `nft insert rule ip zbox divert iifname "<lan>" ip saddr <client> counter`
   （必须是 `insert`，`add` 只会追加到 return 之后、永远数不到）才拿到证据。
   要么模板出一条带 counter 的诊断形状，要么让 doctor 把这条 insert 当 fix 直接输出。
9. **规则集初始化有约 15s 硬预算**：一条远端规则集拉不动就是启动期 **FATAL**（不是降级运行），
   实测原文见 schema §11.7。所以 task #9 的"预置本地 `.srs` 到 `initial_path`"不是优化，
   是全新离线机器能起得来的前提；`validate` 也据此必须带 `-D`。
