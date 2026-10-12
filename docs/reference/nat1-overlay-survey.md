# NAT1 与异地组网：调研 + 定形状（2026-10-11，task #66）

需求原话：**"对异地组网软件要支持 NAT1（可以单独支持，也可以全局支持）"**，点名  
easytier / frpc / 飞鼠组网 / zerotier / wireguard；同一条需求的前半句是"全面支持 IPv6、  
重点研究 IPv6 防泄漏"。这一篇只回答一件事：**ZBox 作为旁路由，能对 NAT1 负责到哪一格**，  
以及那一格在 UI 与生成物上长什么形状。不写实现。

## 0. 证据等级（本文每一块都标了级）

| 级       | 含义                                                                                                                                                                                                   |
| ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 一手·本地缓存 | 官方页原文抓在 `.tmp/ros/p_nat.txt`（2025-10-15）、`.tmp/ros/cl_7.10.txt` / `cl_7.11.txt` / `cl_7.12.txt`（changelog，EINAT 那三条就在这三份里）、`.tmp/net5/wg_man8_archlinux.txt`（man wg(8) 整页，2026-10-11 取回），均可 grep 到行号 |
| 一手·源码副本 | 官方仓库源文件抓在 `.tmp/net5/`（easytier_cli.rs / easytier_common.proto / zt_one.cpp / frpc_full_example.toml）                                                                                                |
| 一手·在线   | help.mikrotik.com / easytier.cn / docs.zerotier.com / gofrp.org 当次读到的原文                                                                                                                              |
| **未实测** | 手上没有 RouterOS 真机、也没有跑起来的 dae，凡是"落到机器上会怎样"的都标这一级                                                                                                                                                      |
| 仅二手     | 只有博客/论坛/视频教程支撑                                                                                                                                                                                       |

## 1. NAT1 = 端点无关映射 + 端点无关过滤（不是"端口能复用"）

RFC 5128 §2.2.3 / §2.2.5 的两条性质，MikroTik 官方 NAT 页直接引用了这两节（一手·本地缓存，  
`p_nat.txt` 的 "Endpoint-Independent NAT" 一节与末尾的 rfc5128 链接）：

- **EIM**（endpoint-independent mapping）：同一 `(源 IP, 源端口)` 不管发给谁，映射到**同一个**公网 `(地址, 端口)`；
- **EIF / source-independent filtering**（一手·本地缓存，原文 *"This mapping allows running  
  source-independent filtering, which allows forwarding packets from any source from WAN to  
  mapped internal IP and port"*）：任意外部源都能从那个公网端口**打进**来。

两条都成立才是 NAT1（full cone）。**只有 EIM 没有 EIF 是Restricted 家族，打洞仍然要靠对端先发包。**

EasyTier 的枚举把这套分类写死在协议里（一手·源码副本，`easytier_common.proto` `enum NatType`）：  
`Unknown=0 / OpenInternet=1 / NoPAT=2 / FullCone=3 / Restricted=4 / PortRestricted=5 /
Symmetric=6 / SymUdpFirewall=7 / SymmetricEasyInc=8 / SymmetricEasyDec=9`。  
**这是五个工具里唯一一份机读的 NAT 类型**（见 §3）。

## 2. 决定 NAT1 的是上游主路由，不是旁路由

包被谁改写，映射就属于谁。旁路由（ZBox）在这条链上只有一个位置：**转发出 LAN 之前的那一跳**。  
所以对 NAT1 而言 ZBox 只有三件事可做，而且**必须按这三件分档写进 UI**，不能含糊成"支持 NAT1"：

| 落点          | ZBox 能做的                                       | 负责方               |
| ----------- | ---------------------------------------------- | ----------------- |
| ① **不打坏**   | 别让组网软件的 UDP 被本机接管（tproxy/divert）、别让它的发现域名走错解析层 | ZBox 全权，可测        |
| ② **让主路由开** | 生成 RouterOS 规则（或给非 ROS 主路由的手动清单 + 超时设置）        | 主路由，ZBox 只出配置     |
| ③ **测出来**   | 一次主动探测，把 NAT 类型显示在面板上，并说明测的是哪台机器的哪条路线          | ZBox 出探针，环境决定能不能测 |

需求里"**单独支持 / 全局支持**"正好落在这三档的粒度上：全局 = 一个总开关（所有 UDP 组网端口都留例外 + 主路由整体开 EINAT）；  
单独 = 按工具/按设备各一格。

## 3. 五个工具的事实表

| 工具             | 监听                                                                                                                                                                                                     | NAT 类型可读吗                                                                                                                                                                                                                                                                              | 与 NAT1 相关的官方原文要点                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **EasyTier**   | 默认 `tcp/udp 11010`、`wg 11011`、`ws 11011`、`wss 11012`；`-l/--listeners` 可用 `<11010>` 简写或 `protocol://0.0.0.0:port`（protocol ∈ tcp/udp/ring/wg/ws/wss/quic/faketcp），env `ET_LISTENERS`（一手·在线 easytier.cn） | **能**：`easytier-cli peer` 有 `NAT` 列（取 `udp_nat_type`）、`node` 有 "UDP Stun Type"、还有 `easytier-cli stun` 自测子命令（一手·源码副本 `easytier_cli.rs:1878/1907/3353`）                                                                                                                                  | 官方 P2P 优化页：NAT1 的对端可以是 NAT1~4；`--mapped-listeners` 手动声明公网地址；`--need-p2p`/`--lazy-p2p`/`--p2p-only`/`--disable-{udp,tcp,sym}-hole-punching`；**并且明确警告"上网辅助工具会影响 STUN 测试的结果"**（一手·在线）——这一句就是 §4 的出处                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| **ZeroTier**   | 默认 UDP 9993，`local.conf` 的 `primaryPort` / `secondaryPort` / `tertiaryPort`（一手·在线 docs.zerotier.com/config）                                                                                            | **CLI 读不到**：`zerotier-cli peers` 表头是 `<ztaddr> <ver> <role> <lat> <link> <lastTX> <lastRX> <path>`，`link` 只有 DIRECT/RELAY；`Peer.hpp` / `Path.hpp` / `one.cpp` 里没找到 NAT 类型字段（一手·源码副本，dev 分支）。**原以为"peers 里有 NAT 类型"这条印象被推翻**                                                            | 官方 protocol 页只说 "hole punching of any NATs or stateful firewalls" 与 "port prediction for traversal of symmetric IPv4 NATs"，**没有出现 full cone / NAT1 字样**（一手·在线）；MikroTik 官方 ZeroTier 页反而建议 full cone 且**映射超时不少于 60s**（一手·本地缓存，属转述）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| **WireGuard**  | 配置文件里 `ListenPort`（`Optional; if not specified, chosen randomly.`，一手·本地缓存 `wg_man8_archlinux.txt:190-191`）                                                                                             | **不探测也不上报**：wg(8) 全文只出现 4 次 "NAT"，且全在同一段 `PersistentKeepalive` 里（两处重复）；Interface 段只有 `PrivateKey`/`Address`/`ListenPort`/`FwMark`，Peer 段只有 `PublicKey`/`PresharedKey`/`AllowedIPs`/`Endpoint`/`PersistentKeepalive`——**没有任何 NAT / conn-type 字段**（一手·本地缓存 `wg_man8_archlinux.txt`，逐字核过） | wg(8) 逐字原文：`PersistentKeepalive` = "a seconds interval, between 1 and 65535 inclusive, of how often to send an authenticated empty packet to the peer **for the purpose of keeping a stateful firewall or NAT mapping valid persistently**"，举例正是 "persistent keepalive interval of 25 seconds"，且 "By default or when unspecified, this option is off. Most users will not need this."；`Endpoint` = "an endpoint IP or hostname, followed by a colon, and then a port number. **This endpoint will be updated automatically to the most recent source IP address and port of correctly authenticated packets from the peer.** Optional."（一手·本地缓存 `:211-223`）。**所以对 WG 说"NAT1"实际含义是：入站映射端点无关，可以免 keepalive 被握手**，否则退化成"25s keepalive + 对端持有你的公网 Endpoint"——那句 `Endpoint` 原文正是"对端先连也能把你的端点学进去"的机制出处                                                                          |
| **frpc / frp** | **默认不监听任何端口**（纯出站；只有 `[[visitors]]` 有 `bindAddr`/`bindPort`，注释写明 `bindPort<0` 即不绑定，一手·源码副本）                                                                                                            | 读不到类型；能读**成功率**：xtcp 打洞成功与否                                                                                                                                                                                                                                                            | P2P 模式叫 **`xtcp`**（`stcp` 是加密中转）；`[[proxies]] type="xtcp"` + `secretKey`，对端 `[[visitors]] type="xtcp"` 带 `keepTunnelOpen` / `maxRetriesAnHour` / `minRetryInterval` / `fallbackTo` / `fallbackTimeoutMs`；顶层 `natHoleStunServer = "stun.easyvoip.com:3478"`（注释 "STUN server to help penetrate NAT hole"）；另有 `[proxies.natTraversal]` / `[visitors.natTraversal]`（一手·源码副本 `frpc_full_example.toml`：`natHoleStunServer` 注释在 :15，`[[proxies]]` 块起 :397、其中 `type = "xtcp"` 在 :399，`[[visitors]]` 块起 :435、`type = "xtcp"` 在 :437，`keepTunnelOpen`/`maxRetriesAnHour`/`minRetryInterval`/`fallbackTo`/`fallbackTimeoutMs` 在 :447-452，`[proxies.natTraversal]` 与 `[visitors.natTraversal]` 在 :408 与 :455）。官方原话：**"xtcp is not suitable for all types of NAT devices. If penetration fails, you can try using stcp proxy."**（一手·在线 gofrp.org）。"必须 full-cone"这句话**官方没写**，是社区推论（仅二手） |
| **飞鼠组网**       | —                                                                                                                                                                                                      | —                                                                                                                                                                                                                                                                                      | **名称无法核实，需要向用户确认**。唯一查到的是官网 `feishunet.com`（静态资源域 `dow.feishuzuwang.com`，产品名"飞鼠组网"，文案"自研引擎/设备直连/硬抗外部 QoS"），页面**未公开底层协议，也没有任何 NAT/打洞文档**（一手·在线，副本 `.tmp/net5/feishunet.html`）。社区三种说法互斥：基于 WireGuard / 替代 ZeroTier 的 P2P / fnOS 组网基于 EasyTier（全部**仅二手**）。**结论：ZBox 对它不承诺 NAT1，只能按"任意 UDP 端口"这一通用形状支持**                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |

## 4. ZBox 现在会打坏 NAT1 的确切位置（读代码得到的事实）

`src/gen/nft.rs` 生成的两份劫持层都**只挂 `prerouting`**（一手·代码）：

- `chain dns` = `type nat hook prerouting priority dstnat` → `udp/tcp dport 53 redirect to :<ports.dns>`；
- `chain divert` = `type filter hook prerouting priority mangle` → 先几条豁免（`meta mark`、`iifname`、  
  局域网网段、Bypass 设备、`fib daddr type local return`（`nft.rs:73`）、组播/广播、DHCP（v4 链 67/68、v6 链 546/547，`nft.rs:78-79`）、53），  
  然后 **`meta l4proto tcp|udp tproxy to :<ports.tproxy>`**——**除上述之外全 UDP 全 TCP 一律进 tproxy**。

于是：

1. **跑在旁路由本机的组网软件不受这条链影响**（本机进程走 output，不进 prerouting；且  
   `fib daddr type local return` 已保护指向本机的入站）。**这一条是好消息，也是"全局支持"最省事的一半。**
2. **跑在局域网客户端上的组网软件，一旦被引流，它的 UDP 打洞流量就进了本机 tproxy**：  
   外部看到的公网 `(地址, 端口)` 变成**接管者拨出的那个**，不再是客户端自己的映射——  
   **EIM 直接失去意义，STUN 观测到的是别人的地址**。这正是 EasyTier 官方警告的"上网辅助工具影响 STUN 结果"。  
   **#69（宿主 ns 的 divert 链 vs dae 的接管机制谁负责哪一半）没定案，所以这一条只能写成风险，不能写成已修。**
3. **解析层这一侧反而干净**：`dns.fakeip` 在现路线下**不参与生成**（`src/gen/oxidns.rs:366-369`  
   原文注释：dae 靠截获 53 的真实应答建立域名归属，fakeip 会打断它；oxidns 也没有 fakeip 执行器）。  
   所以 NAT1 不需要"fakeip 例外"这一格，别照 sing-box 老路线的形状发明一个。
4. **dae 侧有原生表达**：`routing{}` 的选择器含 `l4proto` / `dport`（支持区间）/ `sip` / `mac` / `pname`，  
   内置出站有 `direct` / **`must_direct`** / `block`，自上而下第一个命中即停；  
   差别&#x662F;**`direct` 仍然把 53 交给 dae 的 DNS 模块，只有 `must_direct` 让 53 彻底绕开 dae**  
   （一手：`docs/reference/dae-config-dictionary.md` §"内置出站"，其"存疑/未实测"清单里已声明  
   tproxy 命中、cgroupv2 pname、reload 保连接等都还没实测）。**[未实测]**

## 5. 主路由侧：RouterOS 是**原生支持**的，但有三道限制

一手·本地缓存（`p_nat.txt`，"Endpoint-Independent NAT" 一节，原文照引）：

```
/ip firewall nat add action=endpoint-independent-nat chain=srcnat out-interface=WAN protocol=udp
/ip firewall nat add action=endpoint-independent-nat chain=dstnat in-interface=WAN protocol=udp
```

- 第 1 条建**映射**（EIM），第 2 条开**过滤**（EIF）——**两条都要**，只写第一条只是 Restricted；
- **"Endpoint-independent NAT works only with UDP protocol. IPv4 only."**（原文）；
- 可选 `randomize-port`："randomize to which public port connections will be mapped"，  
  **默认值官方没写**（未证实），且它一开就**与"端口可预测"的对称 NAT 打洞手法互相矛盾**；
- 版本线（一手·本地缓存 changelog）：**7.10beta5 首现、7.10 stable 起有**，  
  `cl_7.10.txt:35` = `*) firewall - added "endpoint-independent-nat" support;`；  
  `cl_7.11.txt:50` = 稳定性修复；`cl_7.12.txt:54` = 新增 `ein-snat` / `ein-dnat` 两个  
  connection NAT state 匹配器给 filter/mangle 用；**7.13–7.24 再无任何 EINAT 行**；  
  官方 NAT 页 58 个修订里**从没出现过 "experimental"** 字样。
- **`to-addresses` / `to-ports` 都不适用于这个 action**（官方属性表把它排除在外，示例也不带），  
  而 `ein-snat`/`ein-dnat` 至今**没进** connection tracking 页的 `connection-nat-state` 取值表  
  （只写 `srcnat|dstnat`）→ **CLI 实际接受面 `[未实测]`，要真机 `print` 一遍才算数**。

**映射活得多久 = NAT1 稳不稳**（一手·本地缓存 connection tracking 页）：  
`udp-timeout` 默认 30s（只见过单向）、`udp-stream-timeout` 默认 3m（双向），7.14 才把  
`udp-timeout` 从 10s 提到 30s。**ZeroTier 官方建议映射超时不少于 60s**（MikroTik 的 ZeroTier 页转述）。  
→ 面板上这一格不能只给"开/关"，要给"**建议 keepalive < 30s**"这类可核对的数。

**两道必须实测才知道的风险**（都没有官方说法，`[未实测]`）：  
① RouterOS 默认配置自带 `action=fasttrack-connection`，而 packet flow 页说 fasttrack  
**支持 NAT**、只是后续包不再评估规则——**EINAT × fasttrack 是否仍保持端点无关，未证实**；  
② 默认防火墙入站有 `connection-nat-state=!dstnat` 的 drop，**它会不会把 EINAT 的入站包一起挡掉，未证实**。  
这两条正是 §7 里"探测 + 清单"要覆盖的对象，也是**必须有一台真 ROS（或 CHR 试用版）才能定案**的部分——  
装 ROS 属于 VM 配置变更，**要先征得用户同意**。

## 6. 与"全面 IPv6"相撞的那一格（需求 A 的两半在这里冲突）

- **IPv6 上没有 NAT1 可做**：RouterOS 官方 NAT 页开篇 *"In RouterOS NAT is supported for IPv4.  
  RouterOS does not support NAT64."*，全站搜 NPT/NPT66 = 0 页（一手·本地缓存/在线）；  
  EINAT 又明写 IPv4 only。所以**给 v6"开 NAT1"是个伪目标**，真目标是**端到端可入站**。
- 于是 v6 侧 ZBox 负责的是**另外两件已有形状的事**：**(a) 别自己把 v6 关掉**——  
  `network.ipv6_mode` 三档（off / dns_only / dual_stack，`src/gen/nft.rs` 的 v6 分支与那句  
  "dns_only 不生成 divert 链"的注释）与 **(b) 解析层别压掉 AAAA**——  
  `dns.strategy = ipv4_only` 会生成 `qtype → black_hole(nodata)`（P4-2 落地的），  
  **这一条与 EasyTier 官方建议"监听 `[::]`、开启 IPv6"直接矛盾**：客户端拿不到 AAAA，  
  组网软件就只剩 IPv4 那条会被 NAT 的路线。**面板必须能把这个矛盾说出来**，  
  而不是让它变成"设置了却不直连"的玄学。这是本轮找到的**最有价值的一条交叉**。
- 防泄漏那一半仍然成立：v6 直连出去是**明写的泄漏路径**（`nft.rs` 注释原文），  
  所以"NAT1 例外"与"防泄漏"是**同一格里对立的两端**，UI 上要并排显示，不能分两屏各说一遍。

## 7. 定形状（下一步的实现题，本轮不落地）

**模型层**新增一段，和现有 `policy` / `dns` 平级，形状尽量复用已有的格子语义：

```
overlay {
  stance: off | global | per_tool      # 需求里的"全局支持 / 单独支持"
  tools: [ { kind: easytier|zerotier|wireguard|frpc|other,   # 封闭枚举 + other 兜住"飞鼠"这类黑盒
             protocol: udp|tcp, ports: […], inbound: bool,
             devices: […] } ]         # 复用 devices[].ip 的 join key（§2 R2）
  router_hint: bool                    # 是否在主路由那一屏生成 EINAT 清单/规则
}
```


- **两层落点，各生成各的**（与「分流情况」屏的两列序号同一形状）：  
  连接层 = `nft` 的 divert 链在 tproxy **之前**插 `udp dport {…} return`（与现有 67/68、53 豁免同形），  
  以及 dae `routing{}` 里 `l4proto(udp) && dport(…) -> must_direct`（**为什么是 must_direct 而不是 direct，见 §4 第 4 点**）；  
  解析层 = 组网/发现域名进 `policy.domain_rules[]` 的 `direct`（复用现成格子，不发明新概念）。
- **能力探测 + validate 门**（项目一贯的"不锁版本"路线）：ROS 版本 ≥ 7.10 才敢生成 EINAT 规则，  
  否则只出"手动清单 + 你的版本可能不支持"；`ein-snat/ein-dnat` 在真机 `print` 之前**只出现在建议里、不出现在生成规则里**。
- **UI 一屏**（第 16 屏，侧栏「组网与 NAT1」）三格：姿态开关（off/全局/按工具）、每工具一行（端口 / 是否入站 /  
  例外落在哪两层 / 该工具能不能上报 NAT 类型）、**实测那一格**（`nat_type` / `DIRECT|RELAY` / xtcp 成功与否 /  
  WG 读不到就写"这一格只能由组网软件自己答"）。**读不到必须画成读不到，不许画成"没问题"**——  
  这是 P3b-2 与 P3c 两轮连续踩到的同一类假话。
- **IPv6 矛盾要显式**：`dns.strategy = ipv4_only` 或 `ipv6_mode = off` 时，这一屏顶着一句 warn：  
  "本机正在压掉 AAAA / 关掉 v6，组网软件此时只剩 IPv4 那条会被 NAT 的路线。"

## 8. 实测计划（下一轮，按代价排序）

1. **[便宜，先做]** guest 上验 §4 第 1、2 点：netns 里造客户端，`nc -u` 发一次，比较  
   **劫持层开 / 关 / 加了 `udp dport return` 例外**三种情况下，出口看到的**源端口**是否一致；  
   用现成脚手架（`/root/zbox-test/topo-up.sh`）。这直接测"不打坏"这一半，不依赖任何上游。
2. **[中]** 探测器纯逻辑：ZBox 用 std `UdpSocket` 做 EIM/EIF 四象限判定，**被测 NAT 由 guest 自己在  
   netns 里用 nft 规则扮演**（VMware VMnet8 测不了真 EIM：guest↔host 同网段**根本不经过 NAT 设备**，  
   这是本轮定下的一条环境边界，别再想在 VMnet8 上验它）。
3. **[要用户同意]** 装一台 CHR 试用版，把 §5 的两条未证实（EINAT × fasttrack、默认入站 drop）测成结论，  
   并 `print` 出该 action 的真实属性面。
4. **[卡 #69]** dae 跑起来后测 `must_direct` 是否真让 53 与打洞流量都绕开接管。

## 9. 出处

| 主题                                                                     | 出处                                                                                                                                                                                                                                                                                                                                   |
| ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| EINAT 两条规则 / UDP only / IPv4 only / `randomize-port` / RFC 5128        | 官方 NAT 页本地缓存 `.tmp/ros/p_nat.txt`（2025-10-15，源 `help.mikrotik.com/docs/spaces/ROS/pages/3211299/NAT`）                                                                                                                                                                                                                                |
| 版本线 7.10 / 7.11 / 7.12                                                 | `.tmp/ros/cl_7.10.txt:35`、`cl_7.11.txt:50`、`cl_7.12.txt:54`（`mikrotik.com/download/changelogs`）                                                                                                                                                                                                                                      |
| `udp-timeout` 30s / `udp-stream-timeout` 3m / fasttrack 支持 NAT         | `.tmp/ros/p_conntrack.txt`、`.tmp/ros/p_pktflow.txt`、`.tmp/ros/p_defaultconf.txt`                                                                                                                                                                                                                                                     |
| EasyTier 参数 / NatType / "影响 STUN 测试的结果"                                | easytier.cn `guide/network/p2p-optimize.html`、`configurations.html`、`quick-networking.html`；`.tmp/net5/easytier_cli.rs`、`easytier_common.proto:177-189`                                                                                                                                                                              |
| ZeroTier 端口与 peers 表头                                                  | docs.zerotier.com `config/`、`protocol/`；`.tmp/net5/zt_one.cpp`、`zt_Peer.hpp`、`zt_Path.hpp`                                                                                                                                                                                                                                           |
| WireGuard `PersistentKeepalive` / `Endpoint` / `ListenPort`            | man wg(8)（`https://man.archlinux.org/man/wg.8.en`，2026-10-11 宿主 `curl` 取回，**一手·本地缓存副本 `.tmp/net5/wg_man8_archlinux.txt`（20,465 B）**，上表引文逐字对过该副本）。`man7.org` 当次连不上（`curl: (28) Failed to connect ... after 21051 ms`）；wireguard.com/faq 当次 fetch 失败，`.tmp/net5/wg_faq_mirror.txt` **只是那一行的失败记录，不含任何原文**——第一次写这张表时把 wg(8) 的出处错指向了它，已改正 |
| frp `xtcp` / `natHoleStunServer` / "not suitable for all types of NAT" | gofrp.org `docs/examples/xtcp/`；`.tmp/net5/frpc_full_example.toml`（:15 / :397-401 / :435-452 / :408 / :455，逐行对过）                                                                                                                                                                                                                     |
| 飞鼠组网                                                                   | `https://www.feishunet.com/`（`.tmp/net5/feishunet.html`）——**身份仍待用户确认**                                                                                                                                                                                                                                                               |
| dae 选择器 / `must_direct` / 无查询 API                                      | `docs/reference/dae-config-dictionary.md` §0 与 §"routing{}"                                                                                                                                                                                                                                                                          |
