# ZBox 编排蓝图：流量路线、分流分层、DNS 走向，以及它们如何一一对应到 UI

状态：**定稿待实测**（2026-10-09）。输入是四份参考文档——`reference/dae-config-dictionary.md`（数据面能配什么）、
`reference/oxidns-config-dictionary.md`（DNS 面能配什么）、`reference/routeros-integration.md`（主路由哪些能自动、
哪些必须人点）、`reference/dae-d1-guest-measurement.md`（eBPF 在这台内核上能不能加载）。
本文只回答一件事：**这三样东西各负责哪一段，ZBox 改一个开关时它们各自的哪一行跟着变**。

三份全典里凡是标 `[存疑]` / "未实测" 的条目，在本文里都只作为**候选**出现，落到 §9 的生死门编号上；
本文不把它们当既成事实。

---

## 1. 定位：ZBox 是编排面板，不是第三台代理

| 平面 | 承担者 | 负责 | 不负责 |
|---|---|---|---|
| 主路由 | RouterOS（先）/ iKuai / 硬路由（滞后） | 谁在网内、给谁发什么网关与 DNS、把该分流的地址送进旁路由 | 分流判决、代理拨出 |
| DNS 面 | **oxidns**（跑在旁路由上） | 解析策略：按域名/按客户端选上游、广告拦截、把解析结果回写成主路由的动态条目 | 转发 TCP/UDP 流量 |
| 数据面 | **dae**（跑在旁路由上，tc eBPF） | 连接判决：直连 / 代理 / 拦截，并按策略组选节点拨出 | 谁的 DNS 该怎么解 |
| 编排 | **ZBox** | 期望态、生成两份配置、下发、把三个面的观测拼成"按设备"的一张图 | —— |

`sing-box` 从这一版起**不再是数据面**（用户 2026-10-09 定案）。代码里 `src/gen/singbox/` 那一条渲染路径
在 P4 之前保留作为对照与回退，但**新生成的默认产物是 dae + oxidns 两份配置**。

一句话的可测判据：**面板上没有任何一个设置是"只改数据库"的**——每一格都要落到 §5 表格里的一行配置文本，
并在"生成预览"里看得见。

---

## 2. 两条流量路线，默认走 R2

### R1 接管网关（devices 的网关 = 旁路由）

全部 LAN 流量先到 dae，由 dae 逐条判决。

- 好处：`mac()` 与 `sip()` 都能拿到设备真身；dae 的 domain 归属天然完整；主路由侧零动态条目。
- 代价：**旁路由挂了全网断**。要在 ROS 侧做默认路由回落，那是又一层状态机。

### R2 域名驱动的路由注入（默认）

设备的网关仍是 ROS；**oxidns 解析出结果后，把"该走代理的那些 IP"作为带 TTL 的动态路由/地址条目写回 ROS**，
ROS 于是只把这一部分目的地址转给旁路由。

- 好处：爆炸半径 = 只有科学流量依赖旁路由，与一个正常代理等价；设备侧"无感"（不改网关、不改客户端、不装证书）。
- 好处：`/ip dhcp-server lease` 一栏保持干净，设备识别不依赖旁路由。
- 代价：① ROS 上有一张会呼吸的表（重启即失、TTL 到期即删，UI 必须说明）；
  ② 旁路由看到的包源 MAC 是 ROS 的，**设备身份只剩 `sip()`**（src IP 不被 NAT，仍然可用）；
  ③ 回程非对称，要显式压掉 `connection-state=invalid` 那一类丢弃规则（routeros §4.3）。
- 上游有现成机制，不用 ZBox 自己发明：oxidns 的 `ros_address_list` / `ros_route` 两个执行器
  自带队列、去重、TTL 钳制（min 60 / max 3600）、`comment_prefix` 标记自家条目、退出时清理。

### 定案

**R2 为全局默认；R1 作为"全局科学"这一档的设备级手段**——把某台设备的网关整个交给旁路由，
正好就是 R1 的形状，作用域只有它自己。这样两路线不是二选一，而是分别占住一档（§4）。

面板上这条要能一眼看出：**路线本身是一个选择项（`network.route_mode`），不藏在规则里**。

---

## 3. DNS 走向：oxidns 是唯一策略脑，dae 只做传输与归属登记

```
客户端 ──DNS(VM,ROS)──► oxidns ──国内上游──► 运营商/223.5.5.5（直发）
                         │  └──国外上游──► 8.8.8.8 DoT/DoH ──┐
                         │                                  │ dae 按自己的规则代理出去
                         └──ros_address_list / ros_route──► RouterOS（动态条目，带 TTL）
```

四条定案，每条都有出处：

1. **不在 ROS 上劫持 53**。同网段 dst-nat 必须配 hairpin masquerade，之后源地址变成 ROS 的 LAN 地址
   （`routeros-integration.md` §1.3 原文），oxidns 的 `client_ip` 匹配器就此失效——**按设备分 DNS 策略只有一条路：
   客户端的 DNS 直接指向旁路由**。这条不是权衡，是否决。
2. **DHCP 下发两个 DNS 服务器：`<旁路由>, <ROS>`**。旁路由是首选，ROS 是兜底。
   旁路由挂了只是退回"没有智能 DNS"，不是没有网。这是 R2 "无感"的另一半。
3. **oxidns 的国外上游要让 dae 带出去**，靠 dae 对 53 端口的既有拦截（`direct` 仍把 53 交给 DNS 模块）。
   于是分工很干净：**oxidns 决定"这个域名该问谁"，dae 决定"这一问走不走梯子"**。
   代价是两层都对 53 有主张，必须实测不形成回环（G3）。
4. **不用 fakeip**。dae 的域名归属是靠**截获 53 端口的真实应答**建立"IP→域"登记的
   （`dae-config-dictionary.md` §0），fakeip 会把真实 IP 换成地址池，反而打断这个登记；
   oxidns 侧也没有 fakeip 执行器。所以模型里 `dns.fakeip` 这一格在新路线下**不参与生成**，
   面板显式标灰并写明"这条属于 sing-box 老路线"（P4 删）。

`FinalOutbound` / `Policy` / `DomainRule` 这些既有模型字段的语义不变，只是**渲染目标从一份 JSON 变成两份文本**。

---

## 4. 分层：全局层 + 设备层，各自落在哪一行

规则自上而下第一个命中即停，所以**设备层必须排在全局层前面**（与现有 `route.rs` 的名单顺序同一个规矩）。

| 档（UI 上就这四个字） | `Device.mode` | dae `routing` 里生成的行 | oxidns 里的形状 | 主路由侧动作 |
|---|---|---|---|---|
| 跟随全局（=智能分流） | `global` | *不生成设备行*，交给全局层 | 走默认 sequence | 无（R2：只有动态路由条目） |
| 强制直连 | `direct` | `sip(<ip>) -> must_direct`（连 53 一起绕开 dae，不给它做归属登记） | `matches: "client_ip <ip>"` → 只转国内上游、不写 ros_* | 无 |
| 全局科学 | `proxy` | `sip(<ip>) -> <final_outbound 组名>` | 该客户端所有 qname → 国外上游 | `/ip dhcp-server lease` 该项 `dhcp-option` 把**网关**指到旁路由 + `make-static` + `send-reconfigure`（G1） |
| 完全不经本机 | `bypass` | 无（流量根本不来） | 无 | 该设备网关/DNS 都保持 ROS；并从 oxidns 的 `persistent` 名单里排除 |

全局层（所有 `global` 档设备共用的那一段），按这个顺序生成到 dae 的 `routing`：

1. `ipversion(4)`/`ipversion(6)` 之类的形状规则**不预设**，只在全局开关要求时加；
2. `dip(geoip:private) -> must_direct`（`policy.private_direct`）；
3. `domain(geosite:cn) || dip(geoip:cn) -> direct`（`policy.cn_direct`）；
4. 广告：`domain(<广告集合>) -> block`（`policy.block_ads`）；
5. `dport(...)` / `pname(...)` 一类进程与端口例外（本轮不开放给 UI）；
6. **`Policy.domain_rules` 逐条展开**：`domain(...) -> direct|<组>|block`；
7. `policy.bittorrent_direct` → `dport(6881-6889) -> direct`（示例值，UI 上可改）；
8. **`fallback:` 必须显式写**（`FinalOutbound::Direct` → `fallback: direct`；`Proxy(组)` → `fallback: <组>`）——
   全典 §3.4 明写"不写就是全直连"，生成器不能省这一行。

DNS 层同理分两段：oxidns 先按**客户端**（`client_ip`）分，再按**域名**（provider/geosite/adblock 集合）分；
顺序冲突的裁决与 `route.rs` 里那一张索引断言表同一形状（P2 立表）。

---

## 5. UI ↔ 配置 双向映射（ask ④ 要的那张表）

"UI 上改了 → 配置文件哪一行跟着变"与反向"配置里这一行是谁画的"都要能查得出。
下表是判据，实现时每格都要有测试钉住。

| 面板位置 | 模型字段 | dae 生成的那一行 | oxidns 生成的那一块 | ROS 侧 |
|---|---|---|---|---|
| 分流策略 · 私网直连 | `policy.private_direct` | `dip(geoip:private) -> must_direct, mark: …` | `ip_set` provider + 一条 `black_hole` 无关 | — |
| 分流策略 · CN 直连 | `policy.cn_direct` | `domain(geosite:cn),dip(geoip:cn) -> direct` | `geosite:cn` / `geoip:cn` provider | — |
| 分流策略 · 拦截广告 | `policy.block_ads` | `domain(<ad>) -> block` | `adguard_rule` provider → `black_hole` | — |
| 域名名单（有序） | `policy.domain_rules[i]` | `domain(key suffix/keyword/regex/full) -> …` 第 i 行 | `qname <pattern>` 第 i 条 sequence 项 | — |
| 兜底出口 | `policy.final_outbound` | `fallback:` 那一句 | 国外分支是否为默认 | R2 下决定 `ros_route` 的目标表 |
| 策略组 | `groups[]` | `group <名> { policy: min_avg10 / filter: … }` | — | — |
| 节点 / 订阅 | `nodes[]` / `subscription` | `node 'ss://…'` / `subscription <tag> 'link'`（落 `persist.d/<tag>.sub`） | — | — |
| 规则集 | `rule_sets[]` | `domain(geosite:<x>)` 引用 | `domain_set`/`dynamic_domain_set`/`adguard_rule` provider + `download` 执行器 | — |
| DNS 上游（国内） | `dns.domestic[]` | `dns.upstream.<tag> = 'udp://<host>:<port>'` | `forward.args.upstreams` + `network.outbound.profiles` | — |
| DNS 上游（国外） | `dns.overseas[]` | 同上（`tls://`/`https://` 形状） | 同上，并挂 proxy profile | 由 dae 带出去 |
| fakeip | `dns.fakeip` | **不生成**（§3.4） | 无对应 | — |
| 网卡 | `network.lan_interface/wan_interface` | `lan_interface:` / `wan_interface:` | `bind` 不涉及；oxidns 的 `api.http.listen` 另配 | — |
| 设备三档 | `devices[].mode` | §4 表里的 `sip(ip) -> …` 行 | `client_ip <ip>` matcher | `proxy`/`bypass` 档动 lease 的 `dhcp-option` |
| 端口 | `ports.tproxy` | `tproxy_port:` | — | — |
| 端口 | `ports.dns` | `dns.bind` | `udp_server/tcp_server` 的 `listen` | ROS 侧 DHCP DNS 指向 |
| **新增**：路线 | `network.route_mode = inject\|takeover` | （只影响 oxidns 是否挂 `ros_*` 执行器 + ROS 侧是否动网关） | `ros_route`/`ros_address_list` 是否生成 | 决定是否写 lease 选项 |
| **新增**：下发节奏 | `apply_policy`（reload vs restart） | `dae reload <pid>`（G2 未验才退回 restart） | `POST /api/reload` + `/api/reload/status` | 无需重启主路由 |

**"生成预览"是这张表的可见形态**：面板加一个只读区，展示当前期望态渲染出的两份文本，
并在每次保存后标出与磁盘上现役配置的差异行。用户不需要看懂语法，只需要能确认
"我刚才那一下，确实动了这一行"。这一格是 P1 的第一优先项，因为它是唯一一个非程序员也能验收的判据。

---

## 6. 观测面：三个视图的数据从哪来（诚实版）

| 视图 | 权威来源 | 契约强度 | 缺的东西 |
|---|---|---|---|
| DNS 解析记录 | oxidns `query_recorder`：`GET /api/plugins/<tag>/records`（分页、按 `qname`/`client_ip`/`rcode` 过滤）、`/stats/top_clients`、`/stats/top_qnames`、`/stream`（SSE），`trace_steps` 就是"这条查询命中了哪几步" | **强**：有路由、有字段、有 schema | ~~回执 JSON 的字段序列化名标了 [存疑]（G5）~~ 已于 2026-10-11 P3b-0/P3b-1 逐字取回（见 §10 那两份回执）；新的一条要记：**列表端点不带 `steps`**，命中链只在 `records/<id>` 单条里，所以那一屏要做两层 |
| 连接情况 | **dae 日志**（没有任何 HTTP/gRPC 查询 API，全典 §0）+ ROS `/ip firewall connection`、`/ip arp` 轮询 | **弱**：日志行格式不在 outline 里，版本一变就可能漂 | 归因只能靠日志里的连接五元组与设备 IP 对齐（G4） |
| 分流情况 | 期望态 = ZBox 自己那份；实际态 = `dae sysdump`（未固化）+ 日志 + ROS 回读 lease/routing rule | **弱** | 期望/实际必须分两栏显示 diff，**不许从日志行反推规则归属**。**2026-10-11 P3c：期望态那一栏已落地**（`GET /api/ladder` + 面板「分流情况」屏，两层各自序号 + 落不成行的那句为什么，见 §10 那份回执）；实际态那一栏给不出来——修订里存的是下发读的那一份快照，没有 dae / oxidns 两份编排配置的文本，所以这一屏只能**整表**说"下发过没有" |

**join key 一律取设备 IP**（`devices[].ip`），MAC 只作为增强：R2 下数据面拿不到真 MAC（§2）。
主路由侧的字段清单、`.proplist` 与轮询代价照 `routeros-integration.md` §4.5 那张表抄，
不在本文里重述。

---

## 7. 一次配置、无感使用的落地顺序

装机（ZBox 自动，五步，沿用现有 CLI 骨架）：

1. **探测与落点**：确认 oxidns / dae 二进制与 geo 数据在位，`Layout` 派生 `/etc/dae/config.dae`（**0600**）
   与 `/etc/oxidns/config.yaml`（全典 §1 的权限门：`run` 对 0644 是 FATAL，`validate` 却返回 0——第三道门）；
2. **能力探针**（§8）：`dae export outline` + `GET /api/build`，取回这一版真正有什么；
3. **生成两份配置**并各自过门：`dae validate -c` rc=0；`POST /api/config/validate`；
4. **主路由侧下发**：按 `routeros-integration.md` §6 的 A 段清单逐条下发（池排除旁路由 IP、
   DHCP 双 DNS、`/ip dns allow-remote-requests`、raw `notrack`、`/ip service set api address=` 维护）；
5. **拉起与验收**：oxidns 先起、dae 后起（与现有"内核先在听、劫持层后上"同一个顺序规矩），
   验收判据取日志行，不取 `check`。

真人只做三步（Winbox）：开 `api` 服务并绑来源、建 `zbox-api` 组 + `zbox` 用户（只勾 api/read/write）、
在面板填一次主路由地址与账号。其余全交给 ZBox。**ZBox 永不自动**：版本/固件升级、重启、恢复出厂、
射频参数、>60KB 文件写入、用户与权限变更。

---

## 8. 升级零影响契约（上游改版不许波及面板）

**不用版本号做判断**，用能力探针：

1. 每次生成前先取一次模式：`dae export outline`（机器可读，含 defaultValue/required/type）
   与 oxidns **`build-info`**（离线打印本构建真正带哪些插件；2026-10-09 实测到该子命令存在，
   不再要求服务在跑，`GET /api/build` 退成"服务已在跑时的复核"）；
2. **未知参数一律不写**：生成器只渲染"模型字段 → outline 里确实存在的那个叶子"的映射表；
   上游新增参数保持其默认值，ZBox 不认识也不需要认识；
3. 上游删掉/改名一个叶子 → 映射表那一格标为不可用，面板上对应控件灰掉并写明原因，
   **不许悄悄不生成**；
4. 任何改动都过 validate 门才落盘（dae rc=0 + 权限 0600 + oxidns `/api/config/validate`）。
   **但这两道门都比想象中松，判据不许只写退出码**（2026-10-09 实测）：`dae validate` 完全不看
   `node{}` 里的链接内容，`oxidns check` 对空文件也回 rc=0。所以 dae 侧要另加"节点数与组数与期望态一致"
   的自检，oxidns 侧要判 stdout 里的 `(plugins: N)`；
5. 热重载优先：`dae reload`（读 `/var/run/dae.progress` 判成败）/ `POST /api/reload` + `/api/reload/status`；
   只有 reload 语义未确认（G2）时才退回重启，并在回执里说清"在用连接会被掐断一次"。

新特性的接法因此是固定三步：**跑探针 → 学那一个叶子的语义 → 在映射表里加一行 + 面板加一格**。

---

## 9. 必须实测才能定稿的生死门

| 编号 | 问题 | 不通的话怎么办 |
|---|---|---|
| G1 | ROS 租约级 `dhcp-option` 是否真能覆盖 network 级 `gateway`/`dns-server`，`make-static` + `send-reconfigure` 后客户端多久生效 | 全局科学档改走 `/routing rule src-address=/32 action=lookup`（保留回落，别用 `lookup-only-in-table`） |
| G2 | `dae reload` 是否真保留连接、失败后现场是什么 | 编排层只用 restart，UI 明说每次改规则都掐连接 |
| G3 | oxidns 的国外上游被 dae 的 53 拦截接管后，会不会形成回环或双重缓存 | 给 oxidns 的出站配 socks 直连旁路，或把它挪到与 dae 不同的机器 |
| G4 | dae 日志能否稳定归因到"哪台设备"，行格式在多版本下漂不漂 | 连接视图退化为"整机计数 + 每设备 DNS/分流期望态"，不假装是实测 |
| G5 | oxidns `/api/plugins/*/records` 回执的字段序列化名 | 先按源码字段名写死一版，加断言测试，漂了改一处 |
| G6 | cgroupv2 pname 钩子在目标内核能不能取到进程名（`pname()` 是否可用于本机 DNS 归属） | 本机流量不分进程，全部交给全局规则（task #43 的 D2） |
| G7 | R2 回程非对称在 ROS 默认防火墙下是否被 `invalid` 丢弃（`/ip settings rp-filter` 默认 no 不是主因） | 对被引流设备加 `/ip firewall raw … notrack`；仍不通则该档退化到 R1 |
| G8 | 动态路由条目规模与 ROS 内存：一所房子（30~80 台）一天会产生多少条 `/32` | 改走 `address-list` + `route-lookup`，或按域名聚合成前缀 |

**这一张表是 P3 的验收清单**。在此之前，本文所有依赖这些结论的地方都以"候选"身份出现，
不进入生成器的默认路径。

---

## 10. 分期：到"第一版 UI 成型"

| 期 | 交付 | 判据 |
|---|---|---|
| **P1** | 模型加 `route_mode` + `apply_policy`；**"生成预览"只读视图**（先只渲染，不落盘不重启） | 真浏览器里改一格，预览里那一行跟着变——这一条过了才算"UI 成型"的第一步 |
| **P2** | `src/gen/dae.rs` + `src/gen/oxidns.rs` 渲染器 + 映射表 + 顺序冲突断言（对齐 `route.rs` 的形状） | 每格都有单测钉"字段 → 那一行"；两份文本各自过 validate 门 |
| **P3** | 观测三视图（DNS 记录走 oxidns API，连接/分流走日志 + ROS 轮询），设备为中心的一屏 | 拿真设备点三档，每档能说出"哪条规则动了、当前实际是哪条" |
| **P4** | ROS 控制通道（8728 裸 TCP + 手写帧，零新依赖）+ lease 级切档真正下发；删掉 sing-box 渲染路径与 fakeip 那一格 | §7 的自动步骤全绿；真人三步之外无手工 |
| **P5** | 异地组网 NAT1：模型加 `overlay { stance/tools/router_hint }`，两层落点各生成各的（nft divert 链在 tproxy 前插 `udp dport {…} return` + dae `routing{}` 的 `must_direct`），第 16 屏「组网与 NAT1」；形状与证据分级见 `docs/reference/nat1-overlay-survey.md` §7。**（这一处"第 16 屏"的编号已在 §11.4 作废：§11 定的屏集把第 16 位给了「看板」，组网那一屏落进**高级档**，今后按名字引用。）** | 三落点**各自可核对**：①"不打坏"= 同一客户端在劫持层开/关/加例外三种状态下出口源端口一致 ②"让主路由开"= 生成或建议的那两条 EINAT 规则在 ROS 上 `print` 得到 ③"测出来"= 面板那一格只有读得到的才画，读不到必须画成读不到。**"NAT1 由上游主路由决定"这条界不许含糊成"支持 NAT1"** |

P5 的两处前置：#69 未定案 ⇒ ①那一半只能先写成风险而不是修好；装 CHR 属 VM 配置变更，**要先征得用户同意**（它是 ② 唯一能定案的路）。另外**"飞鼠组网"这一档身份至今无法核实**，ZBox 对它只按"任意 UDP 端口"的通用形状支持，不承诺 NAT1（survey §3 末行）。

G1/G7/G8 卡 P4，G2/G3 卡 P2 的默认路径，G4/G5/G6 卡 P3。顺序上不阻塞 P1。

### P1 实测回执（2026-10-09，Alpine guest，真浏览器一轮）

产物 `zboxd`（musl，2,640,312 字节）跑一次性 `--root` + `--port 9090`，`/var/lib/zbox` 未触碰
（`current` 仍是 10-01 那份）。判据逐字取回：

1. 勾「中国大陆域名与 IP 直连」→ 保存 → `已写入磁盘，新指纹 616796de3f9d2559。还没下发——`，
   并诚实带上生成器报错（`策略需要规则集 geosite-cn，但它不在 rule_sets 里`）；预览自动读一次，
   这一次**没有差异基准**，回执明说"渲染自磁盘上那一份期望态｜指纹 616796de3f9d2559"。
2. 再加一条 `+example.com → 直连` → 保存 → **两份生成物同时标出差异行**：
   dae `与上一页读到的那份相比：第 42 到 43 行不同` → `# 域名名单（顺序即优先级）` /
   `domain(example.com) -> direct`；oxidns `第 47 到 58 行不同` →
   `- tag: rule1 / type: qname / args: / - 'domain:example.com'`。

**这一条就是 P1 的判据，过了。** 测试基线 364 passed / 0 failed。

同轮把面板外壳从"一页长滚"改成 abox 式的左导航分屏 + 吸顶顶栏（配色走 TrueNAS 冷蓝灰），
13 屏各自一屏，`#save` / `#revert` 全页只有一份（非编辑屏隐藏）。版式来源与取舍、
以及一条被推翻的印象（相邻类串 `px-6 py-6` 实测零命中）记在
`docs/reference/abox-ui-survey.md`。桌面宽屏（≥896px）下的观感**还没有人看过**——
in-app 浏览器视口 613px，只能渲染移动端布局，那一轮要留给人眼。
**（这一条已于同日被推翻，2026-10-11 定案：宽屏观感不必等人眼。** `.tmp/wide/cdp-shot.mjs` 与其后的
`cdp-dnsrec.mjs` / `cdp-ladder.mjs` 用 headless Chrome `--window-size=1440,900` + CDP 自己驱动一轮，
既截图给人看、又把 DOM 读数结构化抓回来；P3b-2 与 P3c 两屏的宽屏版式都是这么过的，无溢出、无 JS 报错。）

### P2 实测回执（2026-10-09，两份生成物各自过了上游的门——但门比想象中松）

**取出口**：`zboxd --target <config.json|dae|oxidns> render [state.json]` 把三份文本之一打到 stdout，
**不落盘、不重启、不碰机器**（`--target` 只对 `render` 有意义，其他命令带着它直接报错）。
`gen::snapshot_files()` 里 sing-box 那四件的原子快照假设一个字没动——dae/oxidns 走的是另一条口。
测试基线 **371 passed / 0 failed**（新增：目标名清单、默认目标与快照逐字相同、坏状态带原因返回、
策略组别名三条）。

**门 ①（dae v2.1.1 `validate`）**：真订阅状态（54 节点 = 33 vless + 21 hysteria2）渲染出 17,516 字节，
`geoip.dat`/`geosite.dat` 就位后 **rc=0**。但同一轮把它拆开测，发现**它对 `node{}` 里的链接内容完全不看**：
未知键、`type=qux`、`security=qux`、删掉 `&host=` 四种改动全 rc=0；只有 `fallback:` 引用不存在的组才 rc=1。
→ **"过了 validate 门"只覆盖语法/别名/geo/规则 lowering/出站引用，不覆盖节点能不能连**；
G11（ws/reality/grpc/tuic 的键名）因此**一条也没被这次测量证实或推翻**，仍等 `dae run` + `trace`（#43 D2）。
另两条同时定案：组不能嵌套 → `gen::dae` 做**可见的别名改写**（多组混合仍报错）；
上游模板 `224.0.0.0/3` 是笔误，卫生层改 `/4` 并钉了单测。细节在 `dae-config-dictionary.md` §5/§6。

**门 ①-补（G12 定案，同日第二轮）**：块名的词法逐个 `dae validate -c` 量过——首字符要 ASCII 字母或 `_`，
其后 ASCII 字母数字与 `_ - .`；`g1`/`hk-1_a.b`/`_x` 过门，`123`/`9hk`/`.hk` 与**任何中文（裸写或加引号）**
解析即失败。我们原先的判据允许数字开头，会渲染出一份 dae 读不进的文件，已收紧并拿七个形状钉住单测。
这条顺带抓出面板一处**说假话**：组名坏掉时保存回执写"已写入磁盘"、概览「生成报错」写"无"，
而预览那一屏的 dae 是坏的——`generation_error` 说的是"下发要读的那份快照生成不出来"，
`apply` 消费的正是那一份，所以它没错，错在少了一格。现在 `GET /api/state` 与保存回执都带
`render_errors`（`[{name, error}]`，空数组=两份都好），概览多一行「编排生成物」，保存那句据实追加
"渲染不出编排配置……下发不受这一条影响"。**判据**：一份生成物坏掉时，不许有任何一屏显示全绿。
测试基线 **378 passed / 0 failed**（本轮 +3 条 `render_errors` 的 api 用例、+1 条 assets 接线）。

**门 ②（oxidns v1.6.1 `check`）**：同一状态补上境外上游后渲染出 3,423 字节 / **8 个插件**，
`oxidns check -c` → `Configuration is valid: … (plugins: 8)` rc=0。它的门比 dae 那道硬：
未知插件类型、跨插件引用（`entry` 指向不存在的 tag）都 rc=1 且带 `文件:行:列`；
但 **空文件也 rc=0**、`listen` 地址非法 rc=0、`args` 未知键 rc=0。
→ 验收判据要写成"rc=0 **且** `(plugins: N)` 里 N 与预期一致"，不能只看退出码。
新发现 `oxidns build-info` 子命令（离线打印本构建能力清单）——**§8 契约第 ① 步由 `GET /api/build` 改用它**，
`/api/build` 退成服务在跑时的复核。细节在 `oxidns-config-dictionary.md` §2。

**这一轮欠的两件**：① 生成物里 `udp_server`/`tcp_server` 的 `listen: '0.0.0.0:53'` 是**开放解析器**
（文本里已带警告注释），下一期收成局域网地址（**①已于 2026-10-11 P2c 收口，见文末**）；
② `oxidns` 自带 `webui/`（Next.js 静态导出），
ZBox 的 DNS 屏与它的边界要写一句话，别让用户同时看到两套面板（**②已于 2026-10-11 P3b-1 文末写定**）。

### P2c 实测回执（2026-10-11，解析口收成局域网地址）

**这一期还的是上面那件 ①**。oxidns 的 `server` 插件**没有客户端 ACL**（源码核实表 §1 第 5 条），
所以 `listen` 是唯一的闸——绑 `0.0.0.0` 就等于在上游那一侧开了一台开放解析器，光在文本里写警告不算闸住。

**落地的形状**：模型加两格 `network.lan_address` / `network.lan_address6`；`gen::oxidns::dns_binds()`
把解析口绑成 `'<lan_address>:<port>'`（udp/tcp 各一只），`takes_dns()` 的两档另加
`'[<lan_address6>]:<port>'` 一对。四条形态是**渲染期硬错误**而不是注记：v4 空、v4 不是 IPv4 地址、
该有 v6 时 v6 空、v6 不是 IPv6 地址。第 2 与第 4 条的理由是门 ② 那句"`listen` 地址非法 rc=0"——
上游那道门根本不验，写错了只有绑定期才炸。

**一条被这次改动推翻的写法**：v6 那一侧不写 `[::]`。上一期（#63a）就是这么写的，理由是"单开
`[::]` 收不收 v4 取决于 `bindv6only`"；但反过来看同一句实测——oxidns 不设 `IPV6_V6ONLY`，所以
`[::]` 会把 **v4 那半个口子**一起收走，"只绑局域网地址"这道闸当场作废。所以只收紧 v4、留 v6 通配
是**假收紧**，两族都得是显式地址。绑了显式 v6 地址之后 `bindv6only` 就不再参与：那只 socket 收不到任何 v4。

**v6 那一格不参与自动探测**（v4 由 `svc::topology` 从现场填）：guest 上 `ip -6 -o addr show` 给的是
`fd15:4ba5:5a2b:1008:20c:29ff:fea1:1609/64 ... dynamic ... valid_lft 86390sec` —— SLAAC 地址会到期，
把它写进期望状态等于埋一颗"地址换了、配置没跟上"的雷。所以那一格只接受人显式填的固定地址。
地址真过期时坏得响亮（下面第 5 条），并且探测会警告"你写的地址不在本机任何网卡上"（复用 ens33 那次的形状）。

**guest 实测**（一次性 root `/root/round-p2c/`、`ports.dns=15353`，用完连目录删；现役数据面一个字没动）：
1. 三档渲染：`off` 只有 v4 那一对；`dns_only`（配 `ipv4_only`）与 `dual_stack` 各出两对，
   `listen:` 四行分别是 `192.168.219.128:15353` ×2 与 `[fd15:…:1609]:15353` ×2。
2. 门 ②：三份都 `Configuration is valid`，`(plugins: 7 / 10 / 9)`——按门 ② 的判据看的是 N，不是 rc。
3. 四条错误路径原样落地：v4 空 → rc=1 带"跑一次 provision"那句；`not-an-address` → rc=1 且**同时**
   打出"不在本机任何网卡上"的警告；v6 空而档位要 v6 → rc=1 且文案点名 `[::]` 为什么不算答复。
4. 真起一次：`ss -lntu` 报 `192.168.219.128:15353`（udp+tcp）与 `[fd15:…:1609]:15353`（udp+tcp），
   `*:15353` **一只都没有**；`nslookup -port=15353 example.com` 打这两个地址都拿到 `172.66.147.243`，
   打 `127.0.0.1` 是 `connection refused`——目的地址就是那道闸，实测成立。
5. 过期地址：同一份配置的 `oxidns check` 仍给 `Configuration is valid` + rc=0，`start` 给
   `Failed to bind UDP socket to 192.0.2.1:15353: I/O error: Address not available (os error 99)` + rc=1，
   插件逆序拆干净。这条已作为逐字夹具钉进 `svc::runner` 的 `failure_detail` 单测。
6. 面板一轮（真浏览器、1440 宽）：新机器两格是空的 → 预览 oxidns 那格红、点名 `lan_address` 并指
   provision，其余三格照常（徽标"只渲染出 3 / 4 份"）；填值保存后**整页重载**再读，值从磁盘回来；
   再读预览 → 四行 `listen` 就是那两个显式地址、徽标"已读取"；「恢复自动探测」把两格一起清空。
   顺带测到 `dual_stack` 保存要求 `lan_cidr6`（P4-3 既有的模型门），回执文案可读。

测试基线 **宿主 lib 420 / guest musl 421 passed，0 failed**。

### P3b-1 实测回执（2026-10-11，观测面落地：查询记录 + 日志落盘）

§6 那张表里"DNS 记录"这一视图的**唯一数据源**是 oxidns 的 `query_recorder`，而上一期的生成物里
根本没有它（只有一句"等下发层接手时一并给"的注记）。这一期把那一句换成真东西：模型加两格
`dns.records_enabled`（**默认开**——用 `#[serde(default)]` 会得到 `false`，等于每台升级上来的机器
静默丢掉观测面）与 `dns.records_retention_days`（默认 7）；生成物因此多三块——
`log.file` + `rotation: daily / max_files: 3`、`plugins:` 里一个 `tag: record / type: query_recorder`、
链上**第一条** `- exec: $record`。面板 DNS 屏多一张「查询记录」卡片（勾选 + 天数）。

**为什么是链首**：`query_recorder` 与 `prefer_*` 同形状，是 `with_next() == true` 的执行器
（`query_recorder/mod.rs:150-153`），放在第一条才包住后面整条链。实测证据是一条 AAAA 查询的
`steps`：`[{cache, node_index 1, entered}, {family, node_index 2, entered}, {family, stop}, {cache, stop}]`
——`node_index` 从 1 起（0 就是记录器本身，它不记自己），而这一条应答是 `answer_count: 0 / No Error`，
也就是**被排序档停掉的那一支照样留了痕**。排在 `$sinkhole` 之前同理：`*_only` 那一档压掉的族要看起来"有查询"。

**门 ② 的第二例（`oxidns check` 不验的那类）**：`retention_days: 0` 同一份配置
`check` 给 `Configuration is valid` + rc=0，`start` 给 rc=1 且
`Plugin initialization failed: Plugin error: query_recorder retention_days must be at least 1`，
插件逆序拆干净（源码 `mod.rs:246-248` 的构造期拒绝）。所以这一条在 ZBox 拦两次：
模型 `validate()` 保存那一关 400 退回（面板上实测文案可读），渲染器再兜一次硬错误。
0 在直觉里既像"永久"又像"不留"，而两个都不是。

**一条被这次实测推翻的写法**：渲染器原来那句注释写"面板『日志』那一屏读的就是 `oxidns.log`"——
**盘上没有叫 `oxidns.log` 的文件**。开了 daily 轮转之后真名带日期后缀，而且按 **UTC** 切：
guest 本地 05:59（+08:00）起的服务写出的是 `oxidns.log.2026-10-01`。读它的那一屏要按
`oxidns.log.*` 取最新（跨 UTC 零点还可能两份并存），不能拼死一个名字。注释已改成这个形状。

**guest 实测**（一次性 root `/root/p3b1/`、`ports.dns=15353`、`api=127.0.0.1:8053`，用完连目录删）：
1. 门 ② 按已定判据看 N 不看 rc：`Configuration is valid: oxidns.yaml (plugins: 10)`，
   与 `grep -c '^  - tag:'` 的 10 逐字相等（多一个少一个都是渲染器与门对不上）。
2. 真起一次：`ss -lntup` 五只 socket——v4/v6 各一对解析口（都是显式地址）+ `127.0.0.1:8053` 管理口，
   局域网面上没有第二个口。工作目录里当场长出 `query-records.db(-shm/-wal)` 与 `oxidns.log.2026-10-01`。
3. 发三条查询（A / AAAA / 第二条 A）→ 记录 5 条，字段是 `client_ip`、`questions_json`、`rcode`、
   `answer_count`、`answers_json`、`elapsed_ms`；`stats/top_qnames` 给 `sample_size: 5` 与 share。
   **列表端点没有 `steps`**，命中链那一层只在 `records/<id>` 单条里——面板要做两层视图。
4. `-d` 实测就是 `set_current_dir`，而且发生在读配置之前：`-c zero.yaml`（相对）报
   `failed to read config zero.yaml: No such file or directory`。下发层（P4）起 oxidns 时 `-c` 必须给绝对路径。
5. busybox `nc` 对这台管理口直接发请求拿到 **0 字节**（连接被先关掉）；`{ printf 请求; sleep 3; } | nc`
   才有完整回执，`wget -qO-` 一次就成。这一条与"令牌不过网"那条不冲突——ZBox 自己的面板用 nc 仍然好。

**面板一轮**（真浏览器 1440 宽，一次性 root `/root/panel-p3b1/`）：新机器上两格是"勾着 + 7"
（`etag: absent（还没有文件）`）；改 30 天保存 → 预览 oxidns 里 `retention_days: 30`、
`- exec: $record` 在 `- exec: $cache` 之前、`file: 'oxidns.log'` + `rotation: daily`；
**整页重载**再读，30 从磁盘回来；勾掉保存 → 预览既无插件也无链上那一条，但多一行
`# dns.records_enabled = false`（那一屏的空是这一格造成的，不是采集坏了）；
天数填 0 保存 → `保存被拒（HTTP 400）：期望状态不成立: 查询记录的保留天数不能是 0…`。

**顺手还掉 P2 欠的 ②**（oxidns 自带 `webui/` 与 ZBox DNS 屏的边界）：ZBox 不嵌、不链、不代理那个
webui——管理口只绑 `127.0.0.1:8053`，局域网面上用户能看到的只有 ZBox 一个面板；oxidns 自己的界面
要看得人去机器上开，属于现场工具不属于产品界面。

测试基线 **宿主 lib 427 / guest musl 428 passed，0 failed**。

### P3b-2 实测回执（2026-10-11，观测面第一屏：DNS 记录的两层视图）

三件东西一起落地：`src/svc/dns_api.rs`（std-only 的最小 HTTP 客户端 + 两层 seam `Http`/`DnsObserver`）、
两条只读端点 `POST /api/dns-records` 与 `POST /api/dns-record`、面板第 14 屏「DNS 记录」。
数据源只有 oxidns 的管理口（`127.0.0.1:8053`，`API_LISTEN` 与渲染器共用同一个常量），ZBox 不直读那份 SQLite。

**把 §7 与全典 §5.1 从"照文档写"改成"照字节写"**：裸 `/healthz`、`/readyz`、`/health` 在这一份构建里
一律纯文本 404（真路径全带 `/api/`）；错误有**两种形状**（未知路由 = 无 `content-type` 的纯文本，
参数错 = `{"ok":false,"code":"…"}`）；每条响应都带 `content-length` 且 `connection: close`，全程没有分块；
`/api/control` 的 `reload.running_version` 就是**加载进内存那份配置的 SHA-256**（改盘上文件不重载时它不动）。

**这一屏最值钱的那一格，四种状态都在真机上出现过**（判据：`null` 永远不许被画成 `false`）：

1. **一致** —— 在「网络 · 路线 · 端口」把本机地址填成 `192.168.219.128` 并保存，面板这份期望态渲染出来的
   摘要与解析器在跑的那份逐字相同（`31e240f5…`），这一格说"一致"。**全程没按下发**——它本来就一致。
   这就是本项目那条生死判据在 DNS 面上的形状。
2. **不一致** —— 勾掉「记下每一次查询」保存：期望摘要变 `28afa4f1…`，在跑的仍是 `31e240f5…`；
   再勾回来保存：回到一致。两个数都在屏幕上，人一眼看得出"改的是哪一格"。
3. **说不清（渲染失败）** —— 一次性 root 那份 `state.json` 的 `network.lan_address` 是空的，渲染器在这里
   给的是硬错误（不绑 `0.0.0.0`，server 插件没有客户端 ACL），这一格原样递出那句话而不是"不一样"。
   CLI 的 `render` 看不见这个状态，因为它先跑 `fill_network` 把地址探出来；`dns_expectation` 与 `preview`
   一样读**盘上那一份**，不替用户猜一个地址。
4. **说不清（管理口连不上）** —— 停掉解析器再读：徽标"解析器没在跑"、meta 带
   `Connection refused (os error 111)`、列表那句"这一条列表不是『没有查询发生过』"，HTTP 仍然 200。
   降级按条走，一条读不到不毁掉整屏。

**两层是真两层**：点第一行（编号 4，一条 5003 ms 超时的 A 查询）才去问第二条，`steps` 回来 6 条——
`#3 domestic 这一步出错` 之后跟着 `#2 family`、`#1 cache` 两条回程 error。只看最后那个应答码看不出是谁拖失败的，
所以那三步 error 都标出来。列表那一层没有 `steps`（实测），这是形状决定的，不是设计偏好。

**过滤与分页都验到"条件真带给了上游"**：`qname=<第一行那个域名>` + `status=error` → 4 条变 1 条；
换成一个不存在的名字 → 空列表那句是"筛完是空的…不等于机器上没查询过"；`limit=1` → 第一页 1 条 +
"下面还有更早的"，按「看更早的一页」拿到编号 3，页句变"这是接着上一问更早的一页"；
`limit=999` → 屏幕上原样出现服务端那句"limit 最多 200：这一屏要的是最近一段，不是整份库"。

**两条被这次实测推翻的面板写法**（都是"把期望态当成现场读数"这一类）：
① 空列表原来那句"「记下每一次查询」现在是关着的——这一屏永远是空的"，在**带着过滤条件**读时是假话，
而且那一句说的是期望态里那一格。现在分四支（管理口没读到 / 筛完是空 / 期望态开关是关着的 / 真的一条没有），
并且 HTML 那两处承诺都补了"下发之后"这个限定。② `applied=false` 原来只说"面板改了，解析器还跑着上一份"，
对"这台机器上的解析器根本不是 ZBox 装的"那类机器等于指控用户没下发——现在两种来路都写出来。

**时钟**：guest 比宿主慢 9 天，屏幕上记录时间显示 `2026/10/2 06:15:55`。那是记录器按自己机器时钟给的
epoch 毫秒、面板按**浏览器**时钟格式化的结果，**不是缺陷**，别去"修"。

**收尾核对**：一次性 root `/root/p3b2/panel/`（连 `panel.token`）随目录删；`serve` 与探测中的 oxidns 都用
`kill <PID>` 停掉；9104 / 8053 / 15353 三只口都没了；`rc-service zbox status` 仍 started、
`nft list tables` 空、`ip -4 rule show` 只有 `0:`/`32766:`/`32767:`、`/var/lib/zbox` mtime 未变。
夹具与原始抓取（`/root/p3b2/*.txt`、`ox.yaml`）留在机上——全典 §5.1 引用的就是它们。

测试基线 **宿主 lib 444 passed / 0 failed**（427 → 444 这一期加的 17 条：`dns_api` 10、api 层观测面 6、
三份资产接线 1；guest 侧那条 unix-only 的计数这期没重跑）。

### P3c 实测回执（2026-10-11，观测面第二屏：分流梯级，只做了期望态那一栏）

三件东西一起落地：`src/gen/ladder.rs`（`derive(&DesiredState) -> Vec<Row>`，全文 766 行里测试占 366 行）、
一条只读端点 `GET /api/ladder`、面板新增一屏「分流情况」（侧栏 `data-view="ladder"`，徽标 + 指纹 +
那句"还没下发" + 那份 26rem 有界列表）。**§6 表格里这一行"缺的东西"从此有了边界**：期望态这一栏齐了，
实际态那一栏**这一期给不出**，而且给不出的原因是结构性的（见下面"这一屏的界"）。

**这一屏的形状**：`Row` 九字段都进回执（`tier` / `when` / `then` / `from` / `dns` / `connection` /
`connection_lines` / `why` / `shadowed_by`，`probe_*` 那几枚漂移钉子 `#[serde(skip)]` 不给前端）。
`tier` 六段：`fixed` / `device` / `global` / `domain` / `fallback` / `note`（"没有落点"那一档）。
序号是**两列**：`dns` 是解析层（oxidns 查询链）里的序，`connection` 是连接层（dae 的 `routing {}`）里的序，
各自从 1 起、`None` 画成"不落行"——**两台引擎各跑各的，跨层没有统一序号**，所以一行带两个数而不是一个。
`policy.cn_direct` 在连接层**占 2 行**（域名与 IP 各一条），于是它的下一格显示"第 7 条"而不是第 6 条，
`connection_lines` 就是用来把这一格说成"（占 2 行）"的。`derive` 不渲染、不校验、不读盘，所以
**任何状态都能出一张完整的表**：渲染不过的那几格由「生成预览」说，这里不重复报错也不改口径。

**漂移钉子是真钉子**：`the_ladder_lines_up_with_both_renderers` 拿 `dae::render` / `oxidns::render` 的
**产物文本**反查链序——连接层只认**顶格那一个** `routing {`（`dns {}` 里嵌着 dae 自己的同名 `routing { request { … } }`，
按 trim 匹配会把两个块并成一个），解析层按链上 `exec:` 出现的序。生成器换了顺序而这里没跟着换，那条测试当场红。

**guest 真浏览器（headless Chrome 1440×900 + CDP，`.tmp/wide/cdp-ladder.mjs`）跑了九种状态**，全程
`consoleErrors` 为空：

1. **saved 富表**（一次性 root `/root/p3c/rich`，端口 9106）：徽标"已读取 20 行"，无落点 3 行，
   `errors` 那句 ok 是"两份编排配置（dae / oxidns）都渲染出来了。"。
2. **改一格再保存**——故意**不在编辑屏按保存**：在「策略与域名名单」关掉 CN 直连、加一条 `+example.com`→代理，
   然后**切回这一屏**按顶部「保存」。底色 8 行（第 10 到 17 行），那句说明在屏幕上原样出现：
   "与上一次读到的那张相比：第 10 到 17 行上了底色。这一张共 20 行；**中间插入或删掉一行会把下面那些行的
   序号整体推后，所以那一整段都会上底色**"。`saveOut`："已写入磁盘，新指纹 8112568e213e3680。还没下发——
   数据面要按下发那个按钮才会重启。"这一条验的是"底色当场出现在眼前"，不是切回来才补画。
3. **手按「读取分流梯级」**→"与上一次读到的那张逐行相同。"，底色清零、行仍 20（读的是盘上那份，不是缓存）。
4. **改回原状**→ 21 行、底色 9（第 10 到 18）——反向也成立。
5. **退出登录**→ 这一屏回到"未读取"、读取键禁用、`view` 换成 `connect`。
6. **`ipv4_only` + 关掉记录器**（夹具 m1）→ 19 行：链首那格整个换掉（"有人问 AAAA 记录时不给答案（这一族整个压掉）"
   成为解析层第 1 条），记录器那一行**整个消失**而不是显示成关着。
7. **一份渲染不出**（夹具 m2：策略组名撞 dae 内置出站）→ **表仍完整 20 行**，`errors` 点名 dae 并把服务端原话
   递出来："策略组名 `direct` 与 dae 的内置出站重名"，收尾那句是"下发不受这一条影响，它读的是另一份快照"。
8. **盘上那份读不进**（夹具 m3：`nodes` 写成了 `5`）→ 徽标"没读到"、meta 改口"盘上那份状态读不进来，所以这一屏
   没有读数，原因见下面两行"、pending 那句变"谈不上『下发过没有』"、列表空 + 那句"这张表是空的（原因见上面两行）"，
   `errors` 带原话 `invalid type: integer 5, expected a sequence`。HTTP 仍 200。
9. **还没有状态文件**（另一只口 9107，空 root）→ 基线 **7 行**、meta "指纹 `absent`"、pending 说"磁盘上还没有
   状态文件，谈不上『下发过没有』"，`errors` 点名 oxidns（`lan_address` 空 → 不绑 `0.0.0.0`），表照样完整。

**这一屏的界（结构性，不是没做完）**：`snapshot_files()` 只有 **`config.json` + `zbox.nft` + `netstate.sh` +
`netstate-down.sh`** 四份，修订里存的就是这一份快照，**没有 dae / oxidns 两份编排配置的文本**，所以
逐行"这一条已经在数据面里生效"没有可比的落点。这一屏因此只能**整表**说下发过没有；现场在跑哪一份去
「概览」与「DNS 记录」那两屏看。HTML 里那段边界句钉死了这句（assets 测试 pin 的正是
`说不了"哪一条已经在数据面里生效"`）。

**这次实测抓出的一处假话**（本轮最值钱的一格，与 P3b-2 那条同源）：`reload_signal()` 在**一个已知良好
快照都没有**时也返回 `pending=true`（`None => (json!(true), Null)`——没下发过和上次下发失败都走这一支），
而屏幕上原来只有一句话，把它说成"与已知良好的那一份不一致"。对一台**一次都没下发过**的新机器，
那句等于当着用户的面指控它下发失败过一次。概览那一屏同屏有 last-good 那一行，两句话自己就把话说清了；
**这一屏没有那一行，所以事实必须跟着回执一起来**：`GET /api/ladder` 补 `"last_good": self.last_good_id()`
（与 `GET /api/state` 同一格同源，api 测试断言两处 `last_good` 相等），JS 按它分两句——
null 时"这台机器一次都没成功下发过，所以没有『已知良好的那一份』可比"，有值时"它与机器上那份已知良好的
内容（修订 X）不一致"。顺带修掉同屏另两处措辞：broken 档的 meta 原来仍称"读的是面板的期望态（盘上那份）"
（那份根本读不进来），以及 `render_errors` 拼接出"。。"（服务端那句原因自己以句号收尾，这里补第二个）。

**覆盖缺口，明写**：`"已知良好的内容（修订 X）"` 这一分支**在 guest 上没法真实触发**——造 `last_good` 要
`POST /api/apply`，那是本轮纪律禁止打的口子。它现在只由编译期的三条断言兜着（assets 那条要求 JS 里两句都在、
且取的是 `value.last_good`；api 那两条要求梯级与概览的 `last_good` 相等、基线那台给 `null`）
加一次纯字符串回显保证。同理 `generation_error` 那格（那份状态连快照都算不出时给的话）
这一轮九种状态里没出现过。

**测试基线 宿主 lib 457 passed / 0 failed**（444 → 457 这 13 条：`gen::ladder` 7、api 观测面 5、三份资产接线 1；
guest 那条 unix-only 的计数这期没重跑，恒比宿主多 1）。三份资产的 JS 用 `node --check` 过一遍；
musl 产物 2,880,880 B push 到 `/root/p3c/zboxd`。

**命令面被这轮再确认的几条**：`--root` 必须 `/` 开头（Windows 宿主上用盘符相对形式 `/ZBox/.tmp/…`）；
`--port` / `--bind` 只对 `serve` 有意义、`--target` 只对 `render` 有意义，宿主跑 `render` 要带 `--init openrc`；
`serve` 自己 `load_or_create(<root>/panel.token)`。guest 侧两条新坑进记忆：busybox ash 把
`cd X && nohup A & nohup B &` 解析成 `(cd&&A)&` + `B&`（一律用绝对路径），以及**在跑的二进制 scp 覆盖会
`Failure`**、而 `pgrep -f "p3c/zboxd"` 抓不到真 PID（cmdline 是 `./zboxd … serve --port 9106`），
要扫 `/proc/*/cmdline` 匹配再 `kill <PID>`。夹具的 `state.json` 得能过 dae 渲染器：带自定义 `headers` 的
ws/http_upgrade、`tuic` 节点、组名撞内置出站，这三种形状都在做夹具时被当场拒过（原因都写进那一行的 `why` 或
`errors`，不是夹具作者的临时决定）。

**收尾核对**：两只一次性 panel（9106 / 9107）用 `kill <PID>` 停掉，`/root/p3c/`（连两份 `panel.token` 与三份夹具
`state*.json`）随目录删；二进制另存 `/root/zboxd-p3c`（2,880,880 B，457 版）当下一轮的取出口。
`rc-service zbox status` 仍 started、`nft list tables` 空、`ip -4 rule show` 只有 `0:`/`32766:`/`32767:`、
`/var/lib/zbox` mtime 未变（`Oct 1 00:45`）、`:53` 仍由现役 sing-box（pid 16980）听着、机器上再无 `zboxd` 进程。
夹具与截图只留在 `E:/ZBox/.tmp/`（`p3c*/state*.json`、`wide/ladder-*.png|txt`），不进 `src` / `docs`。

---

## 11. 界面取舍：abox 那套已实现的界面逐格判定，以及 ZBox 真正要的屏

定稿 2026-10-11。顺序按用户的纠正走：**先把 abox 已经做出来的界面摊开**（它是可复刻的事实，不是设想），
**再逐格判「保留 / 修改替换 / 撇掉」**，**最后按我们自己的实际情况（dae + oxidns + RouterOS）补要新增的屏**。

本节**取代** `docs/reference/abox-ui-survey.md` 里那份"7 抄 / 4 不抄"差距清单：那份写于本轮取向之前，
它数的是版式原语与信息架构，没有回答"这一屏对 dae 和 oxidns 说不说得出口"。survey 里那两轮
**observed 记录本身仍然有效**（逐屏清单、弹窗字段、真跑起来的空态），本文只取代它的**结论表**。

### 11.0 判定用的那一把尺

abox 没有 dae、没有 oxidns，DNS 是它自己的内置逻辑，代理走 sing-box——**所以它的屏不能照抄，只能对着我们的三台引擎改**。
每一格只问一个问题：**这一格屏幕上那句话，ZBox 说不说得出口？**

| 判定 | 含义 |
|---|---|
| **保留** | 它的形状与那句人话，ZBox 用同一形状也说得出口 → 照搬版式与信息切法 |
| **改造** | 这一格问的问题是真的，但它给的答案属于 sing-box / 内置 DNS / 智能家居 → 换成 dae + oxidns 的对应物 |
| **撇掉** | ZBox 没有任何上游消费这一格 → 画出来就是一个骗人的框 |

另立一档 **缺**：abox 没做过、ZBox 必须有的（§11.3）。那几块是"用户思维"要的主路与看板，
**不是从 abox 抄来的**，别去 abox 的截图里找它们。

### 11.1 外壳与版式（结构性）

| abox 屏幕上真有的东西 | 判定 | ZBox 落点 / 理由 |
|---|---|---|
| 侧栏按域分组、每组带小标题、每组 2–4 项、底部固定「退出登录」 | 保留 | 已落地（#58）。**分组名要换**：现在是「这台机器 / 期望状态 / 只读 / 送到机器上」，那是按**我们的代码层**分的；用户不问"哪一屏是只读的"，他问"我该点哪儿" → 换成 §11.5 的「照着做 / 现在怎么样 / 高级：改规则本身」 |
| 吸顶顶栏放页题 + 动作 | 保留 | `header.topbar` 已是这形状。全页**一份**保存不变（改哪一屏存的都是同一个 `state.json`） |
| 卡片标题左侧圆形图标底 | 改造 | 图标资源进不来（CSP `script-src 'self'` + 零构建 + 不放二进制）。已用每屏名首字当 `.mark`：形状在，来源换了 |
| 卡片右上角状态徽标（`未配置` 红 / `可访问` 绿） | 保留 | 我们的回执徽标已是这形状 |
| 五档状态词：运行中 / 已停止 / 未配置 / 异常 / 待接入 | 改造 | 前四档我们读得到（服务 active、端口、劫持层、doctor 致命项）。**"待接入"不该说**：ZBox 不轮询上游、也不代用户装东西，说"待接入"等于承诺一件我们没有的动作 → 改成 **未读取 + 一句为什么**（P3b-2、P3c 两次实测都是这句救的场） |
| 空状态 = 1px 虚线大框 + 「点击上方按钮添加订阅地址」 | 保留（**文案要等 §11.3 A**） | 名单/上游/节点为空那一行 `.note` 用同一语言。但"指向上的按钮"必须真存在——现在点不到任何"添加订阅" |
| 危险动作靠底色分级（绿=备份、蓝=订阅、红=恢复出厂），各带一句"会做什么" | 改造 | 我们的红档动作是**下发 / 回滚 / 装卸劫持层**三个，集中在一屏（「让它生效」），不散在各卡。底色分级 + "会做什么"那一句这两件照搬 |
| 卡内虚线子块逐条解释 | 保留 | 安全设置那卡就是这个形状；设备例外与路线说明正缺它 |
| `rounded-lg border` + 全程 `box-shadow:none`（靠 1px 边框分层） | 保留 | 已落地 `.card`。调色板仍是我自己填的冷色（survey「TrueNAS 那一层」一节已声明无实测依据） |
| 每卡一个保存 | 撇掉 | 它每卡打到不同后端才需要各存各的；ZBox 只有一份盘上状态 |
| 实时推送 / 暂停实时流（SSE、WebSocket） | 撇掉（**但抄它那一行元信息**） | 零依赖 std HTTP 下不做。它日志屏的 `最近刷新 2026-10-02 09:26:22` 单独抄——"这份读数是几点取的"正是我们缺的诚实位 |
| `rpc error: code = Canceled desc = …` 原样进红色 toast；i18n 键 `common.emptyRuleModulesTitle` 漏上屏 | 撇掉 | 用户读不懂也核对不了。后一条是它的真 bug，正好钉住**空态文案写死在页面里 + 编译期断言**（`src/api/assets.rs` 已有同类断言） |

### 11.2 abox 的六屏与四类弹窗，逐格

**仪表盘**（实跑为 8 张卡，2×4）：

| 它卡上真写的东西 | 判定 | ZBox 落点 / 理由 |
|---|---|---|
| 「当前系统」：`Alpine Linux 3.21.8` + `主机IP 192.168.219.128` + `Core Ultra 7 265K` + `679 MB / 39.2%` | **缺**（新格） | 面板就跑在这台机器上，这一卡的数**用户能拿别的工具核对**。现在 `Reading`（`src/doctor.rs:77-98`）12 个字段全是服务/端口/路由，**src 里没有一处读 `/proc`** → 硬件占用是新增取数：`/proc/meminfo`、`/proc/loadavg`、`/proc/uptime`、`/proc/net/dev`（std 读得到）。**磁盘**要 `statvfs`（std 无）→ 走 `Runner` 的 `df -k`（与规则集抓取同一个进程边界），或明写未读取，不猜 |
| 「路由器」卡：CPU / 内存 / 磁盘 + 两个环 | 改造 | 数字进「现在的状态」。**环（甜甜圈）先不做**：零构建下手写 SVG，代价全在样式，而数字 + 一句人话已经可核对 |
| 「Singbox」运行状态 / 启动时间 / 运行时间 | 改造 | 三台引擎各一行：dae / oxidns 在不在、什么版本、门过不过（§11.3 G）。起停时长读 init 系统的 `status`（**注意已实测的界：`status` 只报监督者，不报进程**） |
| 「Singbox 流量」上/下 + 累计 B、「路由器流量」 | 改造（措辞要换） | 字节数不属于任何一台引擎的**管理口**；`/proc/net/dev` 是按网卡的收发，说的是"这块网卡过了多少字节"，**不是"代理了多少"**。屏幕必须写成前者——后者从 #69/G4 起就给不出 |
| 「订阅」：订阅数 / 节点数 / 国家数 / 规则数 | 改造 | 节点数、规则数有；**订阅数在 ZBox 恒为 0**（模型没有 subscription 字段，§11.3 A）。**国家数没有**：`Node` = name/server/port/kind（`src/model/node.rs:6-24`），节点身上没有国家属性——但**"地区"这个概念我们有**，只是它绑在组上而不是节点上：「按名字里的词建一个地区组」那两个按钮（锁定组 `policy: fixed` / 自动组 `min_avg10`，靠关键词挑成员）。所以四格改成 **节点数 / 规则集数 / 名单条数 / 地区组数** |
| 「智能设备」：设备数量 / 在线 / 离线 / 类型 | 改造（**在线/离线不许画**） | 我们只有 `devices[]` 例外清单。ARP 表归主路由（P4 才有），R2 下数据面拿不到真 MAC（§2）→ 这一格现在只能说"设了例外的设备 N 台" |
| 两张 AI 图表 + 一张全宽流量图：**图框与坐标轴照画**，中间一句「暂无 AI 请求记录」 | 改造 | "空着也照画这块区域"的取向对（比灰块诚实）。图框内容只留给 oxidns `stats/timeseries`（buckets 60，文档有形状）；流量图不做 |
| AI 网关 / AI 请求记录 / 智能设备类型 | 撇掉 | 无对应上游 |

**路由·策略**：

| 它屏上真有的 | 判定 | ZBox 落点 |
|---|---|---|
| 顶栏一句总量「共 0 个订阅，0 个节点」+ 两个动作（添加节点 / 添加订阅） | 保留（那两个动作正是我们缺的） | 顶栏那句总量放「订阅与节点」；**两个动作 = §11.3 A** |
| 三段：订阅（列表）/ 地区分流 / 应用分流，`0 规则` 计数挂在标题下面 | 改造 | 三段对我们是：节点（`nodes[]`）、按域名的分流（`policy.domain_rules` + 规则集）、设备级（`devices[]`）。**计数挂标题下**这条保留——它是"这一组是空的"最低成本的可见化。它的"地区分流"是 sing-box rule-sets，我们那格是 `geosite` 走 dae `domain()` + oxidns `domain_set` |
| 三段空态都是虚线大框 + 一句"点击上方按钮添加" | 保留形状（文案等 A） | 同上 |

**路由器设置**（7 卡）：

| 它卡上真写的 | 判定 | ZBox 落点 |
|---|---|---|
| 路由器接入（开关 + "已停用"） | 改造 | = ROS 控制通道（§7 的真人三步），**P4 未落地** → 现在不画假开关，画"填主路由地址与账号"并明写还没接通 |
| 上网模式：先显示`当前模式 / 接入方式 / 网口`，下面才给两个下拉 + 保存 | 保留（这个**顺序**值得抄） | = `network.route_mode`（R1 接管 / R2 注入）+ 网卡。**先说什么现在是什么，再给改** |
| DHCP 服务：LAN 接口 / 当前网关 / 起始 / 结束 / 下发网关 / 下发 DNS | **撇掉**（但问题要留） | DHCP 归主路由，ZBox 不接管地址池。它背后那个问题正是主路第三步"客户端 DNS 或网关改成 zbox 的 IP" → 改成现场读数（本机局域网地址）+ 那两句"改哪里"（R2 不需要动客户端；R1 才要）+ P4 之后由 ROS 的 `dhcp-option` 下发 |
| **NAT 转换：整张空卡只写"当前路由器模式下不支持该功能"** | **保留（这一条最值得抄）** | 位置留着、明写为什么不行——正是我们"没有对应上游的键一律直说不支持"的形状。**藏起来的开关会让人以为产品没这个能力，而不是这台机器现在不行**。落点：「让它生效」与组网那屏的 overlay 档 |
| WireGuard 回家：启用开关 + 二维码位 + "请先接入…并且家里宽带需要支持 IPv6" | 改造（后置） | = P5 组网（easytier / zerotier / wireguard / frpc / 飞鼠）。二维码不做（要密钥与编码，零依赖档下不成比例）。**"前置条件一句话"照抄**——NAT1 那一半正好是这种缺了就直说缺的句子 |
| 代理 DNS 服务器设置：`解析策略(全局) 仅 IPv4` + **代理 / 直连两个页签** + 服务器 + 类型 | **保留改造，这就是我们 DNS 屏该长的样子** | = `dns.strategy`（含 `ipv4_only`，P4-2）+ `domestic` / `overseas` 两组上游。它把两组做成**两个页签**，比我们现在并列两个列表好懂；页签里"服务器 + 类型"与 `Upstream{host,port,transport}` 一一对上 |
| 局域网唤醒：设备表 + 弹窗（名称 / 设备 IP / MAC / 备注） | 撇掉 | 没有任何生成物消费它。**那四栏的形状给设备例外用**——我们设备例外现在正好是"MAC 只出现在报错文案里、没有落点"那一格 |

**设置**（8 卡）：

| 它卡上真写的 | 判定 | ZBox 落点 |
|---|---|---|
| **版本总览**：ABox / Sing-box / 路由器 / Hermes Agent 各挂 `待接入` + 一个「检查更新」 | **保留改造（本轮判定里最大的一条）** | 三行：dae / oxidns / RouterOS，各答"找到了吗、什么版本、门过不过"。用户点名要"更新 dae 和 oxidns 到最新版"，这一屏是入口。**边界**：代码里**没有任何一处定位这两个二进制或读它们的版本**（`Layout` 只有 `singbox_bin`/`zboxd_bin`，`src/gen/init.rs:55,61`；doctor 那 22 条 code 里也没有"二进制缺失/版本不符"）。取法已实测（2026-10-11 guest 逐字）：`dae --version` rc=0 → `dae version v2.1.1`（**单横线 `-version` rc=1**，cobra 拆成 `-e rsion`）；`oxidns --version` / `-V` rc=0 → `oxidns 1.6.1 (full)`（**`oxidns version` rc=2**）。**"检查更新"不进默认档**：它要出网打上游仓库（匿名配额 60 次/小时，全机共享），是个**动作**不是读数 |
| 本机网络：网卡 / IP / 网关 / DNS + 两句警告 + 左下「未识别」右下「应用」 | 保留改造 | = `svc/topology.rs` 的探测（§6 第 5 步：探不出来就停下问人，不拿默认值往机器上写）。"未识别 / 应用"这两个按钮就是它的形状 |
| 通知消息渠道 | 撇掉 | 无对应 |
| 登录账号密码 | 改造 | ZBox 的口令是机器生成的高熵令牌（`<root>/panel.token`，不进浏览器地址栏）。这一格要变成**"换一张令牌"**：要么给动作，要么明写"重置要在这台机器上跑 `zboxd panel-token`"——**现在这两句在哪儿都没写** |
| 安全设置：`当前来源 192.168.219.1` + `可访问` 徽标 + 仅局域网 / 指定 IP / 不限制三档 + 虚线子块逐条解释 + "安全策略保存在本机数据库" | **保留** | 我们有 `--bind` + LAN 判定 + 令牌头，缺的正是这一卡的形状：**把"当前你是从哪个地址进来的"读出来给用户看**。最后一句改成我们的真话：令牌与状态都在 `<root>` 目录下，没有数据库 |
| 配置恢复：导出 / 导入 / 恢复订阅配置 / 恢复出厂，底色分级（绿绿蓝红）各一句"会做什么" | 保留改造 | 我们的四件：导出 `state.json` / 导入 / 回滚到某个修订（#83 已有修订历史与清单里的回滚）/ 清空回基线。**"恢复出厂"必须逐字说清它掐不掐连接**（它要重下规则集与重启数据面） |
| 授权激活码（placeholder："留空后保存可清除本地已保存的…"） | 撇掉（**但记住这句的形状**） | ZBox 无授权。注意：它那句说的是"**留空 = 清除**"，而我们的节点凭据是"**留空 = 这一条不改**"（nodes 屏现有文案）——**两条语义相反**。这是"看着一样、其实相反"的坑，抄串了会静默清掉用户的东西 |
| 后台任务 | 撇掉 | 没有任务队列；同步规则集是同步动作，跑完给回执 |

**日志**：

| 它屏上真有的 | 判定 | ZBox 落点 |
|---|---|---|
| **四个来源各一张卡** + `已接入 / 待接入` 徽标，选中那张涂蓝 | **改造保留** | 我们有**三个**来源（dae 日志 / oxidns 日志 / 内核层脚本 `--stderr`）。现在全挤在 `#logs-out` 一框——一框就会重演"两句话都真、说的却是两份生成物"那个坑（`src/api/assets.rs` 里 `#render-errors` 那条断言管的就是它）。徽标换成"这个口连上没连上"的实测态。**这句"三个来源"本轮被推翻，见 §11.14 开头：落地是五张卡，而"三个"里有一个（dae）压根没有落点** |
| 级别下拉（信息） | 改造 | 两个上游的日志行级别字段形状不统一（与 #69/G4 同类风险）→ 先给**子串过滤输入框**（原样匹配，不假装懂它的级别） |
| 复制日志 / 清空视图 | 保留 | "清空"要说清是**清视图**，不动盘上那一份 |
| 元信息 `服务` / `最近刷新 2026-10-02 09:26:22` / `实时推送已连接` | 保留前两条，撇掉第三条 | 按一次读一次的屏必须说这份读数是几点取的。oxidns 那张卡 glob `oxidns.log.*`（daily 轮转后盘上**没有** `oxidns.log`，实测） |
| 正文按级别着色 | 暂缓 | 依赖上面那条级别判定先实测准 |

**与本产品无关的段**（整档撇掉，一次性列完免得将来又想去抄）：`ai` / `ai-models` / `home` /
`home-automation` / `smart-devices` / `unified-control` / `navigation` / `login_redirect`——AI 模型、智能家居、
个人导航，ZBox 没有这些上游。实跑里 `navigation` 与 `advanced` 两屏只有 206 / 119 个字符（"加载配置中…"），
**没有后端它们就等于不存在**，连版式都没得看。

**四类弹窗**：

| 它弹窗里的字段（逐字） | 判定 | ZBox 落点 |
|---|---|---|
| 添加订阅：订阅名称 + 一个 textarea 吃四种输入，placeholder 明写"支持 `https://` 订阅地址、明文节点、Base64 订阅内容或 Clash YAML" + 地址↔文本切换 + 上传 YAML | **改造，且 placeholder 不许照抄** | 四种里 ZBox 真正认的是三种：链接 / 明文 / Base64（`ingest::subscription::detect`/`parse`）；**Clash YAML 会被拒**（那格错误文案还写着"ZBox 的数据面是 sing-box"，是 sing-box 时代残留，A 落地时一并改）。照抄四种 = 屏幕承诺一件代码里会拒绝的事 |
| 添加手动节点：添加方式（表单录入 / 文本导入）+ 协议类型（VLESS / VMess / Trojan / Shadowsocks / Hysteria2 / AnyTLS / **WireGuard** / SOCKS / HTTP）+ **地区（200+ 国旗下拉）** + 服务器 + 端口 + UUID + 流控 | **改造 + 撇掉地区** | 协议集要按 **dae v2.1.1 真认的那些**（`socks5/http/https/ss/ssr/vmess/vless/trojan/tuic/juicity/hysteria2`，`docs/reference/dae-config-dictionary.md` §3.4）：**AnyTLS 与 WireGuard 不在里面**，画出来就是 400 退回。**"地区"那一整格撇掉，但地区这件事搬到组上去**：`Node` 没有国家字段，abox 把国家绑在每个节点上（喂它的"国家分组 / 地区分流"），我们绑在组上——「按名字里的词建一个地区组」那两个按钮就是这一格在我们这边的对应物（`policy: fixed` 锁定组 / `min_avg10` 自动组）。在节点表单里放一个 200+ 国家的下拉，会让用户以为"选了地区就按地区分流"，而我们的地区分流是 `geosite` 域名名单驱动、地区组是名字关键词挑出来的。**"表单录入 / 文本导入"两档保留**（文本导入正是粘 `ss://…` 那条路） |
| 新增规则模块：模块名称 + 目标出站 + 节点组模式 + 匹配模式 + 规则关系 + 规则集（一行一条 URL，上面一句"支持远程 `.srs` / `.json`；AdGuard txt 会在后端转换"）+ **校验失败时一整块红底把每条问题列全** | **保留改造** | = 我们的规则集 + 域名名单两格，字段名换成我们的（目标出站→兜底出口/策略组；匹配模式→后缀/关键词/正则/整名）。**"一次列全"是新的而且要改后端**：`validate()` 是 `Result<(), String>`、命中第一条就 `return`（`src/model/config.rs:638` 起），所以 400 现在只带一条 → §11.3 F |
| 局域网唤醒 · 添加：设备名称 + 设备 IP + MAC 地址 + 备注 | 撇掉那格 | 四栏形状给设备例外用（备注这一栏要真落到某个生成物里才算数，否则别加） |

### 11.3 ZBox 实际缺的七块（用户点名，abox 没做过）

**A. 订阅进得来——主路第一步，现在根本没有落点**（本轮最贵的一条发现，比任何版式问题都靠前）

三条证据，逐处可查：

1. 模型 `DesiredState` 的字段是 network / ports / dns / policy / nodes / groups / devices / rule_sets /
   apply_policy（`src/model/config.rs:41-56`），**订阅不在里面**；
2. `ingest::subscription::{detect, parse, Meta}`（`src/ingest/subscription.rs`）在 `src/` 里**零调用方**；
3. 而节点屏的文案写着「订阅导入进来的节点不需要在这里新建」（`assets/panel.html:421`）——
   **那句话指着一条不存在的路**。

定案：抓取**复用** `svc::ruleset::detect_downloader`（curl / busybox wget 两条都实测过，https 靠外部下载器，
零新依赖成立）→ 解析**复用** `ingest::subscription::parse` → 落成 `nodes[]`。
**不走 dae 自己的 `subscription` 块**（v2.1.1 outline 确有此块，`KeyableString[]`，落 `persist.d/<tag>.sub`）：
那样 ZBox 列不出 dae 拉回来的节点，用户就看不见"我导入的这些到底是什么"，也没法把它们放进策略组
（组的成员是 tag）。dae 那份留作**兜底**，不是主路。

判据：真浏览器里粘一个订阅链接 → 节点列表长出 N 行 → 「生成预览」里能看到这 N 条 `node '…'` → 概览那句"节点数"跟着变。

**B. 看板**（用户点名的"看板"四件事，全部只读、全部说得出取数口）

1. **硬件占用**：`/proc/meminfo` + `/proc/loadavg` + `/proc/uptime` + `/proc/net/dev`（按网卡收发字节）。
   磁盘见 §11.2 仪表盘那格（`statvfs` 不在 std，走 `df -k` 或明写未读取）。
2. **哪些域名各解析了几次**：oxidns `GET /api/plugins/<tag>/stats/top_qnames` 与 `/stats/top_clients`，
   实测形状 `{ok, sample_size, rows[{key,count,share}]}`（`docs/reference/oxidns-plugin-args-verified.md:231`）。
   `src/svc/dns_api.rs:350-363` 现在只取 records / records/`<id>` / stats/plugins → 这是**两行新取数，不是新架构**。
   **诚实边界**：`sample_size` 说明它是**样本**，屏幕上要写"最近 N 条里的前 20 名"，不许写成"总共解析了多少次"。
3. **哪些被转向、哪些被打回**：解析层说得出口——记录的 `answers_json`/`rcode` + 单条里的
   `steps[]`（`kind`/`tag`/`outcome ∈ entered|stop`）就是"命中了链上哪一步、最后怎么收场"，
   「DNS 记录」屏已是这个两层形状（#73）。聚合成"这些被转向 / 这些被打回"**只能按 `sequence_tag` 归类**，
   是解析层的事。**连接层（dae 实际把哪条连接走了代理）在 #69 与 G4 定案前不许从日志反推**——
   那条纪律在看板上同样成立，所以这一格只有解析层，连接层写"给不出"。
4. **三台引擎在位 / 版本 / 门**：见 G。
5. 外加一行：**这份读数是几点取的**（抄它日志屏那句 `最近刷新`）。

**C. 设备三档卡片**（用户点名"以卡片拖动的形式决定哪台设备走 不科学 / 智能分流 / 全局"）

形状 = 设备卡**拖进栏，栏就是档位**。落地时这一格被模型推翻了一处：**蓝本写三栏，实做四栏**。
理由只有一条——`DeviceMode` 本来就有第四个值 `bypass`（模型 `src/model/config.rs`，生成物 `gen/nft.rs`
给它一条 `ip saddr … return` 豁免），三栏的形状会让它**没有地方住**：要么静默丢掉一台设备的例外，
要么在界面上造一个模型里没有的档。少一栏是数据丢失，多一栏只是一栏多说一句话，所以留四栏：
**不科学（direct）/ 智能分流（global）/ 全局（proxy）/ 不接管（bypass）**，
每栏底下写清它和相邻那一栏的区别（"不接管"与"不科学"差在"主不主经过这台机器"）。
**〔这一栏题的写法已被 §11.15 推翻：`global` 那栏改叫「跟着默认走」、`proxy` 那栏改叫「强制走代理」。
理由不是措辞偏好，是那两个字在说谎——`global` 只是"跟着「默认怎么走」那一排走"，而默认那一排本身是三档，
把它写成「智能分流」等于替用户预设了默认那一排；「全局」更是和那一排的第三档同名不同义。〕**
用户点名的三档一个没少，第四档是"这台设备根本别碰"，对不懂程序的人同样是一句人话。
**这条界必须写清**：拖的是"哪一档"，**不是"哪一条"**——`devices[]` 的数组序在我们这里**没有语义**
（每台一条独立规则、源地址互不重叠、例外整体排在域名名单之前，见 §4），所以绝不做"列表上下拖动排序"，
那会给用户"排在前面的先匹配"的假象，而这件事在我们的生成物里不成立。
**名单里顺序真有语义的是 `policy.domain_rules[i]`**（顺序即优先级），那一格继续用行号显示。
落点：`Device{ip, mode}`，`bypass` 走 nft `ip saddr … return`、`global` 不下规则、`proxy` 要有组可走（devices 屏现有那两句已是这个边界）。
join key 一律取 IP（§6）。现阶段**没有 LAN 客户端清单**（ARP 归主路由，P4）→ 卡片只从"已设例外"开始 + 一个「加一台」入口，**不许画在线/离线**。

**D. 导入订阅之后把默认设成智能分流**（用户点名"默认智能分流"）

冲突事实：新装基线是**全直连**——`policy.private_direct=true`、`cn_direct=false`、
`final_outbound=Direct`，测试 `baseline_is_all_direct` 钉着（`src/model/config.rs:903-913`），
**而那条测试自己写了理由**："基线不该依赖任何外部规则集，否则首次下发会在没网时失败"。

定案：**基线不动**（那条理由仍然成立：一台刚装、还没网的机器不该因为 `geosite` 缺失而下发失败）。
改的是**导入成功那一刻**——A 落地之后，回执里直接把默认档设上，并原样打给用户：
「我把默认怎么走设成了：中国大陆域名与 IP 直连，其余走代理。要改回来点这里。」
**带回执的自动，不是暗改**：动的是 `state.json` 里哪两格要在屏幕上看得见。
前置：`cn_direct` 需要 geo `.dat` 在位（**#56 未落地**），缺数据时那句要改口成
"这一档要 geosite-cn / geoip-cn 那份数据，机器上还没有"（生成器已经会报这句，概览会显示）。

**E. 高级模式分档**（用户点名"平时不显示，有单独的开启按钮"）

`assets/panel.html` 已改一半（侧栏 `data-tier` + `#nav-advanced` + `#mode-toggle`），缺 JS / CSS / 断言。
规则定死：高级档整层住 `#nav-advanced`（默认 `hidden`）；侧栏底部**唯一**一个开关 `#mode-toggle`
（「开启高级模式」↔「关闭高级模式」）；状态存 `localStorage`、**不进 `state.json`**——它是"这台浏览器怎么看面板"，
不是"这台机器怎么配"；未开启时任何指向高级屏的事件一律回落「现在的状态」；默认档不含任何"改规则本身"的屏。

**F. 校验一次列全**（抄它弹窗那块红底）

`validate()` 改成收集成 `Vec<String>` 一次回全 → 牵动 CLI 与 api 两个调用面和既有断言，**单独一格做**（#97），不夹在版式改动里。

**G. 引擎在位与版本**（抄它「版本总览」那一卡，落到我们的三台）

缺的东西见上表。三行各挂：在不在 / 什么版本 / 那道门（`dae validate` 与 `oxidns check`）过不过。
**"版本只用来报，不用来放行"**——§8 那条升级零影响的契约不许被这卡破掉：能力探测 + validate 门才是门，
版本号是给用户核对用的读数。RouterOS 那一行现在只能是"没接通"（P4）。

### 11.4 定屏集：普通档 8 + 高级档 8 = 16 屏

| 档 | 分组 | 屏（新名） | 这一屏回答的那一句 |
|---|---|---|---|
| 普通 | 照着做 | 连接与令牌 `connect` | 我能不能进这台机器、我是从哪个地址进来的 |
| 普通 | 照着做 | 订阅与节点 `nodes` | 我要走的出口从哪儿来：粘一个订阅链接，或者一条一条加 |
| 普通 | 照着做 | 默认怎么走 `policy` | 大陆直连不直连、广告拦不拦、其余走哪儿 |
| 普通 | 照着做 | 某台设备单独走 `devices` | 这一台跟别人不一样 |
| 普通 | 照着做 | 让它生效 `deploy` | 改动怎么送到机器上，那三步各掐不掐连接 |
| 普通 | 现在怎么样 | 现在的状态 `overview` | 盘上那份、在听哪些口、劫持层加载没有、修订历史 |
| 普通 | 现在怎么样 | 看板 `board`（**新**） | 硬件占用、哪些域名解析了几次、哪些被转向 / 被打回、三台引擎在不在 |
| 普通 | 现在怎么样 | 解析记录 `dns-records` | 这一次查询问了谁、答了什么、命中链上哪几步 |
| 高级 | 改规则本身 | 诊断 `doctor` | 期望态与生成物对不上时，是哪一条对不上 |
| 高级 | 改规则本身 | DNS 上游与 fakeip `dns` | 两组上游、本机兜底、地址族策略、fakeip |
| 高级 | 改规则本身 | 网络 · 路线 · 端口 `network` | 流量路线与下发节奏、网卡网段、数据面监听口 |
| 高级 | 改规则本身 | 策略组 `groups` | 把若干出站捆成一个出口 |
| 高级 | 改规则本身 | 规则集 `rulesets` | 「同步规则集」的数据来源 |
| 高级 | 改规则本身 | 分流情况 `ladder` | 盘上那份读成一张表：每条规则在两层各排第几 |
| 高级 | 改规则本身 | 生成预览 `preview` | 我改的那一下，动的是配置文件里哪一行 |
| 高级 | 改规则本身 | 日志 `logs` | 五张卡、一次读取、同一时刻，各自末尾一截（原写"三台各自那一份"，§11.14 已推翻） |

**三条连带变更，别漏**：

1. 屏数从 15 变 16（新增 `board`），`src/api/assets.rs` 那条外壳断言
   （`every_nav_item_has_a_page_and_both_are_in_the_js_table`，要求每个 `data-view` 在 HTML 里恰好出现 2 次）
   与 `PAGES` / `EDITOR_VIEWS` 要一起扩；
2. **P5 那句"第 16 屏「组网与 NAT1」"的编号作废**（本表占了第 16 位）——今后**按名字引用，不按编号**；
   组网那一屏按 §11.2「WireGuard 回家」的判定属**高级档**；
3. 加一屏永远要同时动 HTML / `panel.js` 的 `PAGES` / `panel.css` 三份，少一份由 `assets.rs` 在编译期拦。

### 11.5 落地顺序

| 序 | 内容 | 为什么排这儿 | 判据 |
|---|---|---|---|
| U1（#89 #90） | E 分档收尾：HTML 已改的那半 + `ADVANCED_VIEWS` / `applyMode()` / `#mode-toggle` / CSS / 断言 | 三份资产此刻不一致，后面每屏都要重排 | 真浏览器：默认看不到高级屏、开关能进、刷新后记住 |
| U2（#93） | 屏名与文案换成任务口径（含 `connect` 屏 `:85-91` 那段程序员口径） | 普通用户失效的根因是**句子**不是形状；HTML 已改的两处 `<h2>` 要与 `PAGES` 逐字同步 | 每屏标题是一句人话，断言钉住两边逐字相同 |
| U3（#92） | A 订阅进得来 | 主路第一步没有落点，后面全都走不通 | §11.3 A 那四步判据 |
| U4（#95） | D 导入即设智能分流 | 依赖 A | 回执里那两句原样出现；缺 `.dat` 时改口 |
| U5（#94） | G 引擎在位与版本 + B 看板 | 用户点名的看板；取数口全部已知、增量小 | 每个数字都说得出是哪个口给的；硬件那一栏用户能拿别的工具核对 |
| U6（#96） | C 设备三档卡片 | 依赖"设备侧现在只有例外清单"这个现实 | 拖进档=改 `mode`；屏幕上不出现"第 N 优先" |
| U7（#97） | F 校验一次列全 | 要动 `validate()` 形状与两个调用面 | 故意漏三条 → 一块回执列全三条 |
| U8（#98） | 日志一来源一卡 + 取数时刻 | 内容正确性优先于外壳；A / B 之前它只会更吵 | **已落地，见 §11.14**：五张卡各说各的（"三张"那条前提被推翻）；oxidns 卡 glob `oxidns.log.*`；dae 那张说的是"没有落点" |

**这一节的边界**：以上判定全部建立在 abox **实跑无后端**时给的那些内容与两轮字节扫描的版式原语上。
它带数据时的样子（有节点的列表、有历史的图）我们没看到，所以 §11.2 每格的判定对象是"它问的问题 + 它的形状"，
不是"它填的数据"。**#69、#56、P4、G4 未定案**这件事不会因为本文选了哪几屏而改变。

### 11.6 U1 / U2 已落地（2026-10-11 实测）

屏数此刻是 **15**（普通 7 + 高级 8）。§11.4 那张表里的 `board` 属 U5，加它那天才变 16。

落地内容与被推翻的结论：

- **两档 + 一个开关**：`#nav-advanced`（默认 `hidden`）、`ADVANCED_VIEWS` 8 项、`#mode-toggle`、
  `localStorage` 键 `zbox.panel.mode`（**不进 `state.json`**）。真浏览器（1440×900，headless Chrome + CDP）
  量到的：开局只见 7 项；开关一按 15 项全在；**整页重载后仍是高级档**；关档时人站在 `network` 上被带回
  「现在的状态」；未开档时程序化 `setView('logs')` 也落回 `overview`；七屏逐一点过去，
  每次 `.page:not([hidden])` 恰好 1 张，页题逐字等于导航项那句 label。
- **主路那一步补上了落点**。§11.2 判定"要保留『客户端怎么指过来』这个问题"时，它**没有任何落点**——
  能填 `lan_address` 的那一屏（`network`）按 §11 归进了高级档，于是普通用户按那句去做时无处可抄。
  现在「现在的状态」多一行**只读**的 `#client-target`：填过 → `192.168.219.128 — 设备把 DNS 指到这里`
  （`route_mode=takeover` 时多一句"网关也指到这里"）；没填 → 「还没定下局域网地址…要填它得开高级模式」；
  退出登录 → 退回"未读取"。那一行末尾固定写**"这是磁盘上写的那一份，不是网卡此刻的地址"**：
  它是期望态，不是现场读数。
- **被真机推翻的一条**：`setText(id, text)` 少第三个参数＝**底色不动**。地址行因此出现过
  "句子已经是填好的、颜色还留着上一轮『还没填』那一次的红"——保存路径会重跑一次渲染，正是这一刻看得见。
  已由断言钉住（那个调用必须显式给空 class）。
- **文案换成任务口径**：`connect` 屏那段"参照物那八个 Tab 的表单映射，如今落了八格"整段撤掉，
  换成「这条路怎么走」五步 + 客户端那一步 + 保存≠下发；`订阅与节点` 那屏的三段（键路径合并 / 412 重放 /
  `alpn`·`ech`·`alter_id` 清单）压成"没画的细节不会因为保存而丢"；`某台设备单独走` 的 nft 层细节
  （`ip saddr … return`）撤掉。旧的跨屏引用逐处改名：`下发 · 同步 · 回滚`→`让它生效`、
  `概览`→`现在的状态`、`DNS 记录`→`解析记录`、`节点与订阅`→`订阅与节点`、`DNS`→`DNS 上游与 fakeip`。
- **新断言三条**（`src/api/assets.rs`，每条都做过变异验它不空过）：两档与开关在三份资产里的接线
  （含"JS 的高级档名单与 HTML 标了 advanced 的那些屏**逐项相等**"）、`#client-target` 那一行、
  **每屏导航项那句 label 逐字等于 JS 页题**。宿主库测试 **458 → 461**。
- 宽屏驱动新增 `.tmp/wide/cdp-mode.mjs`（专用 profile 每次先删，保证开局是默认档）。

**没做的**：`board` 那屏、订阅导入（#92）、导入即设智能分流（#95）、设备三档卡片（#96）、
看板硬件栏（#94）、`validate` 一次列全（#97）、日志一来源一卡（#98）。§11.5 那张顺序表仍然有效。

### 11.7 U3 已落地（2026-10-12 实测，task #92）

§11.3 A 那四步判据，此刻的结果：**① 粘链接 ② 节点列表长 N 行 ③ 生成预览里数得到那几条 node 都过了；
④「概览那句节点数跟着变」无处可验**——概览上从来没有"节点数"这一格（`grep 节点数 assets/` 为空），
它属 §11.2 判定要改造的那四格，落地在 U5（#94）。所以 A 的 ④ 排到 U5 之后补验，
这一轮给的替代证据是：**整页重载**后 3 行仍在（说明真在磁盘上），预览里 dae 那份数得出 4 条 node。

- **③ 那句形状写错了，被实测推翻**：判据说的是"能看到这 N 条 `node '…'`"，实际渲染是
  `node {` 块内一行一条 `    node1: 'ss://…'`（单引号、tag 为 `nodeN`，`src/gen/dae.rs::render_nodes`）。
  数 node 要按这个形状数，按 `^node ` 数恒为 0——第一轮就是这么漏判的。
- **导入的落点是一整条服务端路**（`POST /api/subscription`，只做追加、回执只给名字与计数、
  凭据不过页面），形状与三轮真机结论记在 `ARCHITECTURE.md` §7.2 第 21 条，不在这里重复。
- **真机又抓出两句假话**（延续 §11.6 那一条"被真机推翻的一条"的同一纪律）：
  「放弃改动」按下去之后那句"表单里还有没保存的改动"还挂着；退出登录之后上一轮导入的回执还挂着，
  而那串带机场令牌的链接留在输入框里。现在都由 `resetSubReceipt()` 退回 HTML 开局那一句
  "还没有导入过。"，并由 `assets.rs` 钉住"两边逐字同一句"。宿主库测试 **473 → 474**。
- **A 落地之后新暴露的那条界，已经修掉了**（不是 U3 的回归，是它把老问题挪到了主路上）：
  `fill_network` 原来只在 CLI 那侧跑，面板的 `GET /api/state` / `GET /api/preview` / `POST /api/apply`
  直接用磁盘那份，于是**新机器上生成的 dae 配置写着 `lan_interface: ens33`，而这台机器的网卡是 `eth0`**。
  修法与五条界记在 `ARCHITECTURE.md` §7.2 第 22 条（task #99 = U3b）。真机两相对照（一次性 root +
  真浏览器，图在 `.tmp/u3b/shots/`）：
  - **探得到的那台**：概览「本机网卡」= `局域网网卡 eth0（段 192.168.219.0/24，本机地址
    192.168.219.128）｜上游网卡 eth0｜默认网关 192.168.219.2`；导入回执带上"网卡那几格按本机探到的
    写上"那一句；磁盘那份 `network` 落 `eth0`；预览四份全渲染出来，nft 里是 `iifname != "eth0"`、
    `ip saddr != 192.168.219.0/24`，sysctl 键名是 `net.ipv4.conf.eth0.rp_filter`；
    诊断从 5 条降到 3 条，`network_still_default` 与 `lan_iface_mismatch` 同时消失。
    顺带补上 U2 记的那笔缺口：概览「客户端指到哪儿」从"还没定下局域网地址"变成有地址可抄，
    oxidns 那份不再渲染失败。
  - **探不到的那台**（`/sbin/ip` 不在 PATH 里 → 面板只能绑 127.0.0.1；另一台用 `ip route` 退 255
    的包装器，能绑 LAN 地址所以浏览器看得见）：概览那一行给的是 `本机网卡形状没探出来：…`，
    底色 `note` **不是** `warn`；导入回执明说"没能按本机填，沿用 state 里的值…现在写下去的是
    局域网 ens33/段 192.168.1.0/24，下发前要在『网卡与网段』里填对"；预览徽标"只渲染出 3 / 4 份"，
    oxidns 那一格红着写原因；诊断**不**点 `network_still_default`（没读到 ≠ 配错了，`findings:[]`）。
    这一台就是"宁可明说也不能静默下发"那条判据的反面样本：机器上确实生成着指向 `ens33` 的配置，
    而屏幕上三处（回执 / 概览 / 预览）都在说这件事。
  - 还有一条界是这一轮顺带撞出来的：**没有 `ip` 的机器上面板连 `--bind <LAN 地址>` 都过不去**
    （绑定前的"这台机器真有这个地址吗"那一步也要枚举网卡），只能显式绑 127.0.0.1。
    这一条不是 U3b 加的，是 `serve` 早就有的判据，本轮只是第一次把它撞到屏幕上。

**没做的**（更新）：见本节末尾——U4 落地后重列在 §11.8 最后。

### 11.8 U4 已落地（2026-10-12 实测，task #95）

§11.5 给 U4 的判据是"**回执里那两句原样出现；缺 `.dat` 时改口**"。两支都在真机上取到了，
不是只跑其中一支：

- **缺数据那一支**（这台 guest 的真实形状：三个数据目录里都没有 `geo*.dat`）——回执末尾是
  「默认怎么走我也替你设成了智能分流：中国大陆的域名与 IP 直连，其余走 auto（3 个出口，每 3m
  测一次速自动挑一个）。**但这一档要 dae 那份中国大陆数据（geoip.dat 与 geosite.dat），
  这台机器上还没探到**——ZBox 现在不会自己去下载它，下发前得先把这两份放进 dae 的数据目录
  同一个位置里，否则数据面装载那一关就过不去。要改回来点这里。」
- **在位那一支**（临时把 `/root/.local/share/dae` 链到 guest 上那对**真** `.dat`
  （`/root/dae-probe/geo/`，23MB + 2.2MB），事后只删链不删件）——同一句的后半变成
  「这台机器上那份中国大陆数据（geoip.dat / geosite.dat）在位。」
  两支用的是同一个句子构造器、同一个注入位（`geo_on_disk`），所以"改口"不是两条各写一遍。
- **点「要改回来点这里」真落到那一屏**：回执下面那颗 `#sub-policy-link` 只在 `default_set`
  为真时亮，按下去顶栏页题就是「默认怎么走」，屏上四格读回来是
  `私网/回环/组播直连 ✓`、`中国大陆域名与 IP 直连 ✓`、`命中广告规则集就拦截 ✗`、`BT 下载直连 ✗`、
  兜底出口 `proxy — 交给一个策略组` + 组 tag `auto`，且 `formDirty` 是假的（读的是磁盘那份，
  不是内存里的表单）。**广告与 BT 没被顺手打开**——这一档只动它说得出口的那几格。
- **整页重载之后五格仍在**（落盘判据）：`state.json` 里 `private_direct: true`、`cn_direct: true`、
  `final_outbound: {"proxy":"auto"}`、组 `auto` 是 `urltest`（`http://www.gstatic.com/generate_204`
  / `3m` / `tolerance 50`）、`dns.overseas` 那条 `via: {"outbound":"auto"}`、rule_sets 三条
  `geosite-cn` / `non-cn` / `geoip-cn`。
- **第二次导入不改档、只点名**：换一批含新名字的链接（`首尔-D`）→ 回执是「默认怎么走没替你改
  （磁盘上那一版是你选过的），但新增的 首尔-D 不在兜底组 auto 的成员里，它们不会走代理——
  要在高级模式里的「策略组」把成员补上。」那颗按钮收回；盘上四个节点而 `auto` 的 `members`
  仍是原来三个（**不静默改写人编辑过的名单**）。同一批原样再导一次 → 既不改档也不点名。
- U3 那两支没被带坏：退出登录把回执退回"还没有导入过。"、输入框清空、按钮收回；控制台零报错。
- 顺带撞到的：误拿第一个实例的令牌去开第二个实例，面板如实回 `HTTP 401` 并把那串链接留在框里
  ——鉴权那一关在真机上没松（不是本轮判据，记一笔免得以后当它不存在）。

形状与"为什么是五格"记在 `ARCHITECTURE.md` §7.2 第 23 条。图与逐字回执在 `.tmp/u4/`
（`round-u4.json` + `shots/` + `shots-geo/`），驱动脚本 `.tmp/wide/cdp-smart.mjs`
（`FRESH_ONLY=1` 只跑"新机器导入"那一段，换一台机器复跑同一支用）。宿主库测试 **480 → 491**。

**这一轮新暴露的一条界**（**已落地，见 §11.12**）：「要改回来点这里」指的确实是对的那一屏，
可"改回来"在那一屏上要动两格（兜底出口 → `direct`、再取消大陆直连那个勾）。用户口述的那三档
（不科学 / 智能分流 / 全局）落地成一颗三档开关之后，这一句才是字面上的一下。挂在 #96 之后做。

**没做的**（更新）：`board` 那屏与那四格总量（#94）、设备三档卡片（#96）、`validate` 一次列全（#97）、
日志一来源一卡（#98）、dae 的 `.dat` provision（#56——U4 只是把"缺数据"说出来，没有把它补上）。

### 11.9 U5 的 B 那一半已落地（2026-10-12 实测，task #94 / #101）

§11.3 B 那四件事做成了第 16 屏「看板」（**普通档**——它是"现在怎么样"那一层，不是"改规则本身"），
屏数 15 → **16**（普通 8 + 高级 8）。G 那一格（引擎在位 / 版本 / 门）没在这一轮，挂在 #102。

**取数口，逐格说得出**：`GET /api/board` 一条命令都不发（`runner.argv()` 恒空，编译期钉）。
硬件那几格是 `std::fs` 读 `/proc` 四份（`meminfo` / `loadavg` / `uptime` / `net/dev`），
所以那句"几点取的"由**后端**盖（`taken_at_epoch_ms`），面板只按时区显示它——浏览器自己的钟一上来，
两台机器钟不同时就成了面板在说谎。解析层那四格读的是 oxidns 自己那个**只绑回环**的管理口
（`127.0.0.1:8053`），面板不直读它那份 SQLite。磁盘那一格**明写没读**（`statvfs` 不在 std，
读它就要发命令），措辞只住后端一处（`svc::board::DISK_NOT_READ`）。
连接层那句「这一屏给不出「哪一条连接走了代理」」**写在屏幕上**而不是只写在注释里——#69 / G4 定案前
这一格就该是空的，用户要看得见它为什么是空的。

**三支真机验收**（Alpine guest，一次性 root `/root/u5c/root3` + 一次性端口 9114，
探针 `/root/oxidns-probe/board.yaml` 的 DNS 口 5353 / 管理口 127.0.0.1:8053；驱动
`.tmp/wide/cdp-board{,-down,-off,-empty}.mjs`，图 `board-1440.png` / `board-off.png` / `board-empty2.png`）：

1. **有数那一支**：内存 / 负载 / 运行时长 / `eth0` 收发字节全给了，四格聚合各带**自己的**样本数
   （实测同一轮里 `top_qnames` 给 13、另三份给 15，所以卡头不写共用数、格子也不把几格相加当总量）。
2. **管理口不通那一支**：badge 退成「部分读到」，硬件那几格照给，四格各写一句"原因见这张卡最后那一句"，
   长原因（`Connection refused (os error 111)`）在这张卡最后只出现**一次**。
3. **期望态把记录关掉那一支**：见下面第二条被推翻的写法，两种现场（跑着的那份还在记 / 那份也是空的）都取了。

**这一轮被实测推翻的三条写法**（都是"屏幕上自己跟自己打脸"，静默，所以逐条钉进断言）：

1. `plugins` 那一格头条拿 `matched` 当数，而那根条画的是 `query_share` → 真机画出
   「匹配 · rule1 ｜ 0 次 · 66.7%」，一行里两个数互相打脸。查 `store.rs:755-800`
   （`query_share = query_hits / total_records`）+ 逐字实测：两种 `kind` 各只有一半有数
   （executor 只有 `executed`、matcher 只有 `checked`/`matched`）。头条改成与条**同口径**的
   `query_total`，命中/执行另起一句，并把"那根条画的是什么"写在格子顶上。
2. 那句收尾拿 `records_enabled === false` 直接推出"上面四格是空的"——**期望态不是现场读数**。
   真机现场（盘上关着、跑着的那份还在记）画出来就是自相矛盾的一屏。现在四格各自把形状
   （有行 / 空 / 没读到）交回一处，那句按它分三档：
   「有 N 格读到了行——盘上这份和跑着的那份还没对上」/「有 N 格这一轮没读到，所以这里不替那份期望态作证」/
   「都读到了、也都是空的，这两句不矛盾；但「空」不等于这台机器没人查询过」。
3. 空库那一格说"前 1 名"：全新 DB 上 `rcode` 会回 `sample_size: 0` **外加一行** `count: 1` 的
   `_UNKNOWN`（上游那条 SQL 的 `LEFT JOIN`），于是同一格里"前 1 名"与"最近没有查询记录"同时出现。
   判空只认 `sample_size`（这条一开始就有），"前 N 名"现在也只在这一格真画了行时才说。

顺带一条：`**…**` 那种 markdown 强调在 `textContent` 里会**字面出现在屏幕上**（面板没有 markdown 渲染器），
所以非注释行里不许有它——断言按行扫。

接线钉在 `api::assets::tests::the_board_view_is_wired_in_all_three_assets`（三份资产 + 措辞与口径，
不钉样式细节），宿主库测试 **491 → 504**。

**没做的**（更新）：G 那一格＝三台引擎在位 / 版本 / 门（#102）、设备三档卡片（#96）、
`validate` 一次列全（#97）、日志一来源一卡（#98）、dae 的 `.dat` provision（#56）、
「默认怎么走」收成三档开关（上面那条界）。

### 11.10 U5 的 G 那一半已落地（2026-10-12 实测，task #102）

看板第三卡「三台引擎在不在」：三行各给"在不在 / 什么版本 / 面板这份和它加载那份是不是同一个"，
卡底一句**版本这一格只用来报，不用来放行**——§8 那条升级零影响的契约不被这张卡破掉，
要不要下发仍由「让它生效」那一屏说、合不合规矩仍由「诊断」那一屏说。

**取数口，仍然逐格说得出，一条命令都不发**（`/api/board` 的 `runner.argv()` 恒空那条断言照旧覆盖这一格）：

| 行 | 取数口 | 读不到时说什么 |
|---|---|---|
| 域名解析（oxidns） | 复用这一轮**已经取到**的那两份：`/api/health`（进程活着才答 ⇒ "读到了"即"在位"，给 `version` / `build_bundle`）与 `/api/control` 的 `reload.running_version`（它加载进内存那份配置的 SHA-256）；"面板要的那一份"= `gen::oxidns::render(state)` 那段文本的 SHA-256 | 管理口连不上 ⇒ 整行"没读到"，并明说这不说明这台机器没在解析 |
| 分流（dae） | **没有可查口**（控制面是进程内对象，listener 只在它自建 netns 里绑 tproxy 口，宿主不可达；`Layout` 里也没有它的落点） | `svc::board::DAE_NOT_READ` 那一句 |
| 主路由 | **还没接通**（RouterOS 对接面排在后面） | `svc::board::ROUTER_NOT_READ` 那一句 |

三条落地要点：

1. **摘要只有一处算法**。看板与「解析记录」屏现在共用 `Api::oxidns_wants()`（渲染那份文本 → SHA-256，
   渲染不出来则把 detail 带出去），所以同一台机器上两屏不会为同一个数说两句话，也不二次读盘。
2. **`applied` 是三态**（`true` / `false` / `null`）。`null` 说的是"两头有一头没数"，
   **不许画成"没对上"**——渲染不出来、没读到 running_version、管理口不通，四种来路各写各的一句人话。
3. **两句"为什么读不到"是人话不是术语**，并且各只住后端一处（面板只抄）。新增
   `the_not_read_explanations_stay_in_plain_words` 按字符禁掉 `` ` ``、`::`、`.rs`、`_`、`netns`、`proc`、
   `tproxy`、`**`，并要求每句都把"空着"与"结论"分开（必须出现"不是"）。

**四支真机读数**（Alpine guest，一次性 root `/root/u5c/root3` + 一次性端口 9114；驱动
`.tmp/wide/cdp-board-engines.mjs`（纯读）与 `.tmp/wide/cdp-board-fill.mjs`（唯一写面 = 网络屏四个框 + 保存）；
图 `.tmp/u5d/shots/board-{up,filled,applied-true,down}`，逐字回执 `.tmp/u5d/round-u5d.jsonl`）：

1. **渲染不出来那一支**（夹具 `lan_address` 是空的）：`wants_version: null` + `generate_error` 有内容 ⇒
   `applied: null`，屏幕上写"面板这一份渲染不出来，没得比"。**这是一台没填过地址的机器本来就该有的样子**，
   不是新缺陷：`fill_network` 只住 CLI 侧（填内存不落盘），面板渲染的是盘上那份原始 `state.json`。
2. **没对上那一支**：面板填 `eth0` / `192.168.219.0/24` / `192.168.219.128` / DNS 口 5353 → 保存 →
   **整页重载**核对四个框都落盘 → `wants_version: e7805631…` vs `running_version: b7624cd1…` ⇒
   `applied: false`，屏幕上一句"面板改了、还没生效"。
3. **对上那一支**：guest 上 `zboxd … --target oxidns render` 出的那份，`sha256sum` 逐字等于面板给的
   `e7805631…`（**两条独立路径算出同一个数**，这是这一格最硬的一条核对）；用它重启探针后
   `applied: true`，屏幕上一句"它加载的那一份，就是面板这份期望态要的那一份"。
4. **没读到那一支**：停掉探针 ⇒ `live: false`、`version` / `running_version` 全 null、`wants_version` 照给、
   badge 退成「部分读到」，卡底那句换成"三行全是空的——空着的格子读起来像「没有」，这里说的却是「读不到」"。
   dae / 主路由那两句**逐字不变**。

宿主库测试 **504 → 506**；musl 产物 3,104,936 B、md5 `b43d57519e23a9b2a5cb2e6395fa6f06`（宿主与 guest 一致）。
**没做的**（更新）：设备三档卡片（#96）、`validate` 一次列全（#97）、日志一来源一卡（#98）、
dae 的 `.dat` provision（#56）、「默认怎么走」收成三档开关（上面那条界）。
G 这一格里"门过不过"仍然**不在看板上**——那是 `dae validate` / `oxidns check` 两条命令，
按上面第 0 条（一条命令都不发）的界，它归「诊断」那一屏。



### 11.11 U6 设备卡片四栏已落地（2026-10-12 实测，task #96）

> **读这一格前先知道：下面正文里的两个栏题已被 §11.15 推翻**（「智能分流」→「跟着默认走」、
> 「全局」→「强制走代理」）。四栏的形状、拖与不拖落同一处、那几支真机读数都仍然成立。

被推翻的结论先说：**蓝本 §11.3 C 写的"三栏"不成立**，实做四栏（理由见该格第一段）。
第二处被推翻的是我自己第一轮的验收脚本：`#device-lanes` 里卡片的"第几张"是**按栏的先后**数的，
不是"哪一台先加"，所以第一轮的 `dragTo(0, …)` 拖的根本不是我以为那台，
**一轮跑完盘上没有任何一台是非 global 的**——那一轮只证明了栏会跟着动，没证明档位落盘。
第二轮换成交互一律**按 IP 认卡**，才算把这一格验完。

落地内容（三份资产 + 两处编译期断言）：

1. HTML：`#device-list` 那张平表没了，换成 `#device-lanes` 里四个 `.lane[data-mode]`，
   每栏一个 `<h3>` 栏题、一句 `.lane-count`（"N 台"）、一段说明、一个 `.lane-cards` 容器。
   栏题与说明与 `PAGES` 的副标题**逐字同源**，由 `assets.rs` 钉。
2. JS：`DEVICE_MODES = ["direct","global","proxy","bypass"]`（**顺序就是栏的顺序，不是优先级**）；
   两条改档路径都收进同一个 `putDeviceCard(card, lane)`——拖动（`.dev-grip` 起手，`dataTransfer`
   只带一个无内容记号，地址与 MAC 绝不过 `dataTransfer`）与卡片上那一格下拉（`change`）。
   **两条路都要**：触屏与键盘做不了 HTML5 拖放，而"卡在哪栏"与"那一格写什么"必须永远一致，
   否则屏幕上会出现"卡在智能分流、那一格写着全局"这种自己跟自己打脸的样子。
   收集走 `collectDeviceLanes(edit)` → 同一个 `collectRows`，所以 412 逐条重放那份现场没换地方算。
3. CSS：`.lanes` 四列栅格，`70rem` 以下两列、`40rem` 以下一列（窄屏真机看过，见下）。
4. 断言：`assets.rs` 新增 `the_device_lanes_are_wired_in_all_three_assets`
   （四栏一档、三句界、四个栏题在 HTML 与 JS 里逐字相同、五个 drag handler、`putDeviceCard(` 恰好三处、
   九个 CSS 选择器，以及**禁回归**：`!renumberRows("device`、`!device-list`、三条旧程序员口径句子）。
   两条既有断言跟着改形状：`mod.rs` 的 id 清单 `device-list` → `device-lanes`；
   四表逐条重放那条认 `collectDeviceLanes(deviceEdit)`，并新钉"它必须把卡片交给 `collectRows`"。

**四支真机读数**（Alpine guest，一次性 root `/root/u6/root` + 一次性端口 9115，现役服务未受影响；
驱动 `.tmp/wide/cdp-devices{,2,3}.mjs`，图 `.tmp/u6/shots/`，逐字 `.tmp/u6/round-u6.jsonl`）：

1. **开局**（新机器）：四栏各"0 台"，`#device-empty` 那句"还没有设备例外"在位；
   点三次「加一台设备」→ 三张卡全在「智能分流」栏（那一档什么都不做 = 还没决定）。
2. **两条路各自改档 + 真的落盘**：下拉把 `fd00::50` 改成「不科学」、拖 `.dev-grip` 把 `10.99.0.11`
   拖进「不接管」→ 保存 → **整页重载**后卡片还在原来那栏、那一格还是那个值，
   `GET /api/state` 的 `devices[].mode` 逐字为 `direct` / `bypass`。
3. **§5 那句判据（UI 改一格 = 配置里对应那一行跟着变）**：同一份状态下的 `GET /api/preview` 里，
   dae 的 `routing {}` 出现 `sip(fd00::50) -> must_direct # fd00::50`，
   `zbox.nft` 出现 `ip saddr { 10.99.0.11 } return`（两条链各一条），
   oxidns 那份出现 `- 'fd00::50/128'`。**这一格第一次有了"拖一下 → 生成物多一行"的逐字对照。**
4. **「全局」那一档在基线兜底出口（直连）下**：保存仍然成功，回执同时给两句——
   "设备 10.99.0.11 被设为强制代理，但当前没有任何可用的代理策略组 下发会被拒" +
   dae 渲染失败那句"要\"全局科学\"，但兜底出口是直连，没有可去的策略组"，
   「生成预览」那一屏该份 `ok: false` 带同一句原因。
   另两支：只点「加一台设备」什么都不填就保存 → 盘上台数不变（空白卡不是一条新例外）；
   把卡片全「去掉」再保存 → 盘上 `[]`、那句"还没有设备例外"回到屏上，
   指纹退回首轮填完网卡那次的 `7e01d2cc8107c5e8`（同一份状态两次算出同一个数）。

**这一轮的边界（不许说过头）**：拖动那一路用的是页面内合成的 `DragEvent`（CDP 的
`Input.dispatchMouseEvent` 不触发 HTML5 拖放）。它证明的是那五个 handler 的接线与落点，
**不证明浏览器原生按住起手的那一下**；原生那一下只能在真触屏/真鼠标上验，而下拉那一路是真实事件、
两条路共用同一个 `putDeviceCard`，所以"卡在哪个栏"这个结论不受影响。
`li.unknown`（模型里出现这一屏不认识的档名）那一支在真机上**造不出来**：`mode` 是 serde 枚举，
非法值在 PUT 那一层就被拒，走不到渲染——它是给"以后模型加了第五档、旧面板读到新状态"留的，
只有编译期断言钉它。

宿主库测试 **506 → 507**；musl 产物 3,115,824 B、md5 `8e94ba4d0f1fa505405d49d165bbd90a`（宿主与 guest 一致）。

### 11.12 U4b「默认怎么走」收成三档开关已落地（2026-10-12 实测，task #100）

§11.8 末尾那条界（"要改回来"在那一屏上是两格，不是一下）到这一轮才算字面上成立。
蓝本排在 #96 之后的那一项做完了，**没有新被推翻的结论**——只更正一条上一轮的收尾核对：
上一轮写"9115 已空"，实际当时 guest 上还有一个绑 `127.0.0.1:9115`、读 `/var/lib/zbox` 的
`zboxd panel serve`；本轮开局第一次 `ss -lntu` 里它已经不在了（不是我杀的）。
**端口占用要在起一次性实例之前当场看，别拿上一轮的核对当本轮的前提。**

**形状：那一排不造第三个状态字段。** 三档（`不科学` / `智能分流` / `全局`）只是
`policy.cn_direct` 与 `policy.final_outbound` 那两格的**快捷写法**——模型里没有
`policy.mode` 这种东西，生成物也就没有一行需要读它。两个方向都是真的：

- 写：点一档 → 只派发那两格的 `change`（`chooseModeSwitch`），另外三格（私网 / 广告 / BT）一个字不碰；
- 读：哪颗亮由那两格**反算**（`modeSwitchNow()`），不是"上一次按了哪个"记着。
  所以人直接改下面的框，按钮会退成**三颗都不亮** + 那句"下面那几格被单独拧过"，
  而不是硬选一档骗人——这一条是这一屏最容易长歪的地方，钉了编译期断言。

**缺组那一支：一格都不改，但必须出声。** 「智能分流」「全局」要把兜底出口交给一个策略组，
而这一屏上可指的组 tag 来自 `#group-list` 现值（`switchProxyTag()`）；一个都没有时按下去
**不改那两格**（`formDirty` 仍是假的），只把那句"没有一个组 tag 可指，先导入或建一个"
换成**警告色**打在下面。"按了没反应"是这一屏最坏的失效，所以这一支不许静默。

落地内容（三份资产 + 两处编译期断言）：

1. HTML：`<h2>分流策略</h2>` 之后、四格之前插 `#mode-switch`（三颗 `.mode-opt[data-mode]`，
   每颗两行：档名 + 一句"这一档是干什么的"）与 `#mode-switch-note`，外加两句界
   （"先点下面这一排就够了：它改的是背后那两格"、"私网直连、广告拦截、BT 直连那三格各管各的"）。
   **排在四格之前**是断言（`switch_at < cells_at`）：第一眼看见的必须是"挑一档"，不是"一堆框"。
2. JS：`MODE_SWITCH`（三档 → 那两格的目标值）+ `groupTagsOnForm` / `switchProxyTag` /
   `modeSwitchNow` / `modeSwitchSentence` / `paintModeSwitch` / `chooseModeSwitch` /
   `paintModeSwitchNoTag`；`fillForm` 末尾读盘之后重算一次读数；`pol-final`、`pol-cn-direct`、
   `group-list` 三处变更都触发 repaint（**读数要跟着那两格与那份组名单走**，少一处就会跟下面的框不一致）。
   `PAGES.policy` 的副标题换成"先点那一排：不科学 / 智能分流 / 全局。…"。
3. CSS：`.mode-switch` / `.mode-opt` / `.mode-opt.on`（accent 边 + 18% accent 底）/
   `#mode-switch-note.muted-note` / `40rem` 以下单列。
4. 断言：`assets.rs` 新增 `the_three_way_default_switch_writes_only_the_two_cells`
   （容器与三档在 HTML、三颗按钮接了点击、顺序、两句界、`chooseModeSwitch` 体内**必有**那两格且
   **必无**另三格、禁造第三个字段（`policy.mode` / `default_mode` / `modeSwitch:`）、
   `paintModeSwitch` 必来自 `modeSwitchNow()` + `switchProxyTag()`、custom 与 no-tag 两支各一句、
   五个 CSS 选择器）。`mod.rs` 的 id 清单加 `mode-switch-note`。

**六支真机读数**（Alpine guest，一次性 root `/root/u4b/root` + 一次性端口 9115，
订阅桩 = `nc -l -p 8099 -s 127.0.0.1 -e /root/u4b/serve.sh` 外套 `while :`，喂的还是 `/root/u3b/links.txt`；
驱动 `.tmp/wide/cdp-modeswitch.mjs`，图 `.tmp/u4b/shots/01..08`，逐字 `.tmp/u4b/round-u4b.jsonl`；
全程未打 `apply` / `rollback` / `hijack enable`，现役 sing-box 监督进程与 `/var/lib/zbox` 未动）：

1. **开局（新机器，无节点无组）**：亮的是「不科学」，note 普通色、那句"这一档背后是两格：
   中国大陆…＝不勾，兜底出口＝全部直连"。点「智能分流」、「全局」→ 读数里
   `cnDirect:false`、`final:"direct"`、`dirty:false` **一字未动**，而 `noteClass` 成了 `note warn`、
   那句出路在位。点回「不科学」→ note 退回普通色。**缺组那一支按预期出声不改格。**
2. **导入订阅之后**（U4 那条自动，走的就是这一屏那两格）：整页重载后亮的是「智能分流」，
   `cnDirect:true` / `final:"proxy"` / 组 `auto`，盘上 `cn_direct:true` + `final_outbound:{"proxy":"auto"}`。
   **U4 的自动与这一排读的是同一份两格**，没有第二套真相。
3. **§11.8 那条界落地的那一下**：点一颗「不科学」→ 两格同时跟着变（勾取消、兜底退回全部直连、
   组那一格收回隐藏）→ 保存（指纹 `65b9c0a29f3f429b`）→ **整页重载**后仍亮「不科学」，
   `GET /api/state` 逐字 `cn_direct:false` / `final_outbound:"direct"`。
   改回来是一下，不再是两格。
4. **「全局」**：`cnDirect:false` + `final:"proxy"` + 组 `auto`，那句"这一档背后是一格"；
   落盘指纹 `f12f2eb9d6f363d1`。**再点回「智能分流」**落盘的指纹是 `de54402b9f15c42a`——
   与第 2 步导入之后那一份是**同一个数**（同一档点回来算出同一个哈希，没有留下多余的字节）。
5. **自定义那一支**：手动把两格拧成"大陆直连 ✓ + 兜底全部直连"（三档之外的组合）→
   `lit: []`（三颗都不亮）、`noteClass: note muted-note`、那句"下面那几格被单独拧过…要点某一档就点它"。
6. **窄屏 420px**：三档竖排单列、高亮那颗仍看得出来、note 与那两格都在位；**控制台零报错**。

**§5 那句判据在这一屏上第一次有了三档两两对照**（用 CLI 取出口，不碰机器：
`zboxd --root /tmp --init openrc render --target <dae|oxidns> <state.json>`；
宿主跑要带 `MSYS_NO_PATHCONV=1`，否则 Git Bash 把 `/tmp` 换成 Windows 路径，
报的是"`--root` 与 `--prefix` 必须是绝对路径"。夹具 = 盘上那份**只改那两格**）：

| 改的那一格 | dae `routing {}` | oxidns 链尾 |
|---|---|---|
| 兜底出口 `direct` ↔ `proxy: auto` | `fallback: direct` ↔ `fallback: auto`（**一行之差**） | `- exec: $domestic # 兜底出口是直连` ↔ `- exec: $overseas # 兜底出口是策略组 auto` |
| 大陆直连 ✗ ↔ ✓ | 多出 `domain(geosite:cn) -> direct` + `dip(geoip:cn) -> direct` 两行 | **只多出四行"为什么没有 cn 分支"的注释**（`.dat` 缺位，#56 那条界，本轮原样复现、没有修） |

**这一轮的边界（两条，都不修，先记着）**：

1. 那一排只保证**那两格一致**，不保证**这一档整份成立**。U4 那条自动改的是五格
   （还补 `dns.overseas` 与 rule_sets）；这一排是手动档，它不补那三格。
   实测这一支（CLI 夹具：智能分流 + `dns.overseas = []`）：dae 仍然渲染得出（`fallback: auto`），
   而 `render --target oxidns` **rc=1**、整份不出来，那句原因是
   「链上有分支要转 dns.overseas（境外上游），但那一组一条上游都没填——这些解析没有可去的地方：
   （兜底出口=策略组 auto，所以至少要有境外上游）」。
   也就是说这一档缺料时**会在「生成预览」与「诊断」上出声**，不是静默泄漏；
   但"点一颗按钮 → 要再补两格才成立"这件事，UI 上没有提前说。要不要让这一排也走五格那条路，
   挂在 #97（`validate` 一次列全）之后一起定——**§11.13 已定案：不走**（那条缺料是渲染层的，
   清单管不到它）。
2. 上面那张表的 oxidns 那一列，大陆那一格在**解析层**当前不产出规则、只产出说明——
   这是 #56（dae 的 `.dat` provision）的形状，不是这一轮的失手。

宿主库测试 **507 → 508**；musl 产物 3,123,336 B、md5 `498f2088fb4287bfef26e8fdb5dc4fb7`（宿主与 guest 一致）。
**没做的**：`validate` 一次列全（#97，**下一节 §11.13 已落地**）、日志一来源一卡（#98）、dae 的 `.dat` provision（#56）。

### 11.13 U7 校验一次列全已落地（2026-10-12 实测，task #97）

先把 §11.12 挂着的那条界收掉：**那一排不走五格那条路。** 理由是这一轮量出来的——
"点一颗按钮 → 要再补两格才成立"里那两格缺的料（`dns.overseas` 空、rule_sets 缺）
**不在 `validate()` 的清单里**，它是 `gen::oxidns` 渲染时报的（`src/gen/oxidns.rs:254`），
一次列全帮不上它；而把五格塞进一颗按钮会同时撞两条已经钉住的东西
（`chooseModeSwitch` 体内必无那三格、"改回来是一下"）。出路本来就在那两屏上，
而「生成预览」与「诊断」会指名道姓说缺哪一格——所以这一条界**定案为不改**，不是搁下。

**被推翻的结论（一条）**：上一轮我自己给 `validate()` 写的收尾注释说去重是为了
"几百个节点的订阅里那些同字段的报错"。不成立——节点那类报错**各带节点名**，字面就不相同，
去重压不住它们；真正压住的是"两个都没填 tag 的策略组""两台同名同址的坏设备"这种
**字面完全相同**的重复行。几百个坏节点那一份靠的是 `describe_problems` 的 12 条上限，不是去重。
注释已按实测改写。

**形状：清单只有一份，排版只有一处。**

1. `DesiredState::validate()` 的返回从 `Result<(), String>` 换成 `Result<(), Vec<String>>`：
   命中第一条不再 `return`，22 处检查各 `problems.push(...)`，结尾**保序去重**。
   顺序 = 检查顺序 = **屏序**（网卡 → 节点 → 组 → 规则集 → DNS → 名单 → 设备），
   不按字典序重排——人顺着屏幕从上往下改，第一条就是最上面那一格。
2. 措辞与编号的排版不住 `validate()`，住 `model::config::describe_problems`（四条出口共用）：
   单条就是那一句、**不加帽子**；多条是 `共 N 处：` + 一行一条 `1）2）3）`；
   超过 `PROBLEM_CAP = 12` 只列前 12 条，末尾明说"…另有 M 处同类，改完上面这些再看下一批"。
3. 四条出口各自新增/追加断言，钉的是同一份形状：`model::state::parse`（CLI 的 `load_state`
   与面板 PUT 共用）、`zboxd doctor` 的 `invalid_state` 那一条 finding（清单整份落在 `actual` 里）、
   `gen::singbox` 那道门（`GenerateError::InvalidState`）、以及 `api` 的 400 回执。
4. 面板那一侧：`#save-out` 与 `ul#findings li` 各加 `white-space: pre-line`。
   **这一条是判据的一部分，不是美化**——`<p>` / `<li>` 默认把换行折成空格，
   没有它，"一次列全"送到屏幕上就还是那条长句，列全白做。
   `assets.rs::the_save_note_keeps_every_problem_on_its_own_line` 钉那两条 CSS 规则、
   钉 `setSaveNote` 只写 `textContent`（**体内出现 `.replace(` 即失败**：把换行换掉等于取消一次列全）、
   钉 400 那一支把 `errorText(result)` 原样递进来。
5. 顺带修一处措辞：未命名设备的地址报错原本打出来是 `设备 （未命名） 的地址`，
   两侧空格在屏幕上看着像排版坏了；改成 `设备（未命名）的地址`，钉在"同一句话只列一次"那支测试里。

**新增七支测试**（`config.rs` 五支：`one_pass_lists_every_broken_cell_in_screen_order`、
`the_same_wording_from_two_cells_is_listed_once`、`a_single_problem_prints_as_a_plain_sentence`、
`several_problems_print_one_per_line_with_a_count`、`a_long_list_is_capped_but_still_tells_the_true_total`；
`state.rs` 一支读路径列全；`assets.rs` 一支面板排版），
另把 15 处旧断言从"拿一条错误字符串"改成走 `problems(&state)`（并成串再查那一句在不在）。

**七支真机读数**（Alpine guest，一次性 root `/root/u7/root` + 一次性端口 9116，
驱动 `.tmp/wide/cdp-problems.mjs`，图 `.tmp/u7/shots/`，逐字 `.tmp/u7/round-u7.jsonl`，
上一轮的读数与截图归档在 `.tmp/u7/r1/`；第一轮产物 3,131,128 B / md5 `d1846b36fd4191c7e32cfc4e288072f3`，
修措辞后重编 3,130,968 B / md5 `5955f6fc5b33284978f554a902f7b826`，宿主与 guest 一致，
**两轮都跑过**；全程未打 `apply` / `rollback` / `hijack enable`，现役监督进程与 `/var/lib/zbox` 未动）：

1. **只坏一处**（`#net-cidr` = `not-a-cidr`）→ 400 回执是**一句**，
   `期望状态不成立: 局域网 CIDR "not-a-cidr" 写法非法`，没有"共 1 处："那顶帽子；
   量出来 `lineBoxes: 1`、`height: 21`。
2. **三处各坏一点**（CIDR + `records_retention_days = 0` + 一台地址填 `tv.lan` 的设备）→
   一次列全三条，`共 3 处：` + `1）2）3）`，`whiteSpace: pre-line`、`lineBoxes: 7`（三条各占多行）、
   红字（`.warn`）。**这一格的判据就在这条：三行各自成行，不是一条长句。**
3. **只修好一条**（CIDR 改回）→ 清单跟着少一条，剩 `共 2 处：`、`lineBoxes: 5`。
4. **全修好** → 保存成功，落盘指纹 `9d7fae0e62adf4ca`，`devices: ["192.168.219.77"]`；
   回执仍是那两句（写入 + 这一份渲染不出，因为 `network.lan_address` 是空的——
   面板从不填那一格，#99 的形状，本轮原样复现、没有修）。
5. **每一支 400 之后磁盘那份一字未动**：`etag` 仍是 `9d7fae0e…`，`cidr` / `retention` / `devices`
   三格读数与改之前逐字相同。两轮开局不同正好各是一支持平证据：
   第一轮 `source: "missing"` + baseline 形状（新机器），第二轮 `source: "saved"`。
6. **窄屏 420px**：两条各占行、`lineBoxes: 7`、红底没被挤掉；**控制台零报错**。
7. **CLI 那两条出口同一份现场**（不碰机器）：`zboxd --root /root/u7/root doctor /root/u7/broken.json`
   与 `render --target <dae|oxidns|config.json> …` 打出的都是**同一份三条清单**、`rc=1`，
   跑完 `--root` 目录里仍只有 `panel.token` 与 `state.json`，那一份没被改写。
   （`rc` 要在没管道的时候取：`… | sed -n '1,6p'; echo rc=$?` 量到的是 `sed` 的 0。）

**这一轮的边界（四条，其中 1 与 3 已在 §11.16 收口）**：

1. **两层标签叠在一句里**：屏幕上打的是 `保存被拒（HTTP 400）：期望状态不成立: 共 3 处：`——
   `期望状态不成立: ` 是 `state::parse` 的 `map_err` 前缀（还是 ASCII 冒号），四条出口共用。
   要收成一层得同时动 CLI 那一句，不在这一格的范围。**〔已收口，见 §11.16：共享串不再带动作标签，
   帽子自己带主语"这 N 处不成立，要改："〕**
2. **去重只压字面相同的**：各带节点名的报错（一份坏订阅能出几百条）压不住也不该压，
   靠 12 条上限收口。上限之后要改完这一批再看下一批，这是故意的——
   一屏 200 行红字不可读，而"共 214 处"这句话已经把规模说清了。
3. **`策略组 tag "" 与节点重名` 这一句在两个匿名组相撞时说错了对象**（撞的是组不是节点，
   `src/model/config.rs:822`）。真机上造不出这个现场：组卡片不填 tag 时先报"缺少 tag"，
   屏幕上看不见这一句，只有编译期断言那支测试里它出现了。留待下轮措辞一起收。
   **〔已收口，见 §11.16：空 tag 不进撞名比较，撞组说撞组、撞节点说撞节点〕**
4. **渲染层的缺料不在清单里**（见本节开头那条定案）：`validate()` 过了仍可能渲染不出，
   那一半归「生成预览」与「诊断」。

宿主库测试 **508 → 515**；musl 产物 3,130,968 B、md5 `5955f6fc5b33284978f554a902f7b826`（宿主与 guest 一致）。
**没做的**：日志一来源一卡（#98，**下一节 §11.14 已落地**）、dae 的 `.dat` provision（#56）、上面边界 1 与 3 那两处措辞。

### 11.14 U8 日志一来源一卡已落地（2026-10-12 实测，task #98 / #103–#107）

**先记被推翻的那条前提。** §11.2 判定那一格时写的是"我们有**三个**来源（dae 日志 / oxidns 日志 /
内核层脚本 `--stderr`）"，§11.4 定屏集那句"三台各自自己写的那一份"是它的简称。
本轮逐条对过自己那份生成器之后，这句话两处都不成立，而落地的五张卡是把这两处都改对之后的形状：

1. **`dae` 那一份压根没有落点**。`gen::dae` 写进配置里的只有日志级别，没有文件路径（本轮 grep 自己那份
   生成器确认）。所以屏幕上如果给它一张"空文件"的卡，人读到的是"接管层没日志"，
   而真相是"接管层这份归跑它的那一方管，面板读不到"——两句话在诊断上是完全不同的两件事。
2. **`oxidns` 那一份按天轮转，盘上没有 `oxidns.log` 这个名字**（更早那轮实测就记着，本轮才落到读法上）。
   按字面路径读它，跑过一天之后这一张卡永远是"还没有文件"，而机器上明明有昨天的和今天的。
3. **`--stderr` 那一份在 systemd 下进的是服务运行记录**，只有 OpenRC 那份 runscript 把它落成文件。
   同一张卡在两种机器上"读不到"的原因不同，所以它们必须是两种状态，不是一个"空"。
4. 反过来，**数据面自己写的那一份**（配置里 `log.output` 落的那个文件）原本不在那三个里，
   而它是这一屏最常被看的那一份——它和 `--stderr` 那一份的分工是"读到配置之前"与"之后"。

**形状：五张卡、一次读取、一个时刻。**

1. `GET /api/logs` **仍然没有参数位**，一次返回 `{sources: [五张卡]}`。为什么不做成五端点或带参数：
   这一屏唯一在回答的是"刚刚发生了什么"，分五次取就会出现这张新、那张旧，而屏幕上看不出来。
   卡顶那句"取数 …｜5 张卡说的是同一时刻"就是这一条承诺的可见形态。
2. **`state` 六个值，一个都不许合并**：`ok` / `empty`（文件在、0 字节）/ `missing`（还没这个文件）/
   `journal`（这台机器上走的不是文件）/ `no_home`（我们的配置没给它落点）/ `unreadable`（在、读不出）。
   前四种都不是故障，画成红的或画成"空"就是这一屏最坏的那一种假话；
   认不出来的 `state` 由 `logState` 兜底**原样画那个词**——上游将来多一种落点要被看见，不能被静默归进"空"。
3. **措辞只住 Rust**：`title` / `who` / 三条 `*_NOTE` 全在后端，`assets.rs` 反向钉 HTML 与 JS 里
   **不许出现**那三句标题（抄一份就是第二处真相，改后端词的人不会回头改资产）。
4. **只有"文件在、读到了"那两支摆正文**（`ok` / `empty`），其余三支收成一行。
   摆一个空框看上去像"这里本来该有东西，但没显示出来"，而那一句在本屏是假话。
5. **过滤框是纯前端重画**（`#logs-filter`，子串，不是级别下拉）：两个上游的日志级别形状不统一，
   画一个下拉框等于承诺"服务端会按级别滤"，而它不会。这条界编译期钉两处——
   `readLogs(` 在 JS 里恰好出现 **4 次**（一个定义 + 开局 / 动过数据面之后 / 人按下那一下），
   以及 HTML 里那枚 input 开局自带 `disabled`、JS 的开局态与之配对。
6. **有界窗口从整屏一条变成每卡一条**：`.log-src-body { max-height: 12rem; overflow-y: auto }`，
   五份正文叠成一列会把"先看哪一格"挤出首屏。字节上限仍是 `LOG_TAIL_CAP = 64 KiB`（每份各自）。
7. **落点派生自 `Layout`，文件名不在这一层拼**：`kernel` / `supervisor` / `panel` 读
   `kernel_log_path()` / `worker_stderr_path()` / `panel_stderr_path()`，
   `oxidns` 用 `LogHome::Rotating { dir, prefix }` 按 `gen::oxidns::LOG_FILE` 前缀 glob（那个常量本轮改 `pub(crate)`，
   名字只写一遍），`dae` 是 `LogHome::Missing`。回执里 `file` 只给 `file_name`，`no_home` 那一路给 `null`。

**测试：后端 3 改 + 4 新，`assets.rs` 1 支重写 + 3 条新断言。**
新增的四支是 `the_oxidns_card_follows_daily_rotation_to_the_newest_file`（写 10-01 与 10-02 两份，
断取到 02、且 `sing-box.log` 不混进来）、`stderr_on_systemd_reads_as_journal_not_as_missing`
（同一份现场按 `InitSystem` 两种分类）、`an_unreadable_source_says_which_one_and_leaks_no_path`
（**把落点做成目录**来跨平台造 `unreadable`，不靠 root 下造不出来的权限位）、
`every_log_card_carries_the_same_fields`（十键逐卡相等，缺键与多键都红）。

**真机一轮**（Alpine guest，一次性根 `/root/u8/root`、`--init openrc --bind 192.168.219.128 --port 8123`，
驱动 `.tmp/wide/cdp-logcards.mjs`，图 `.tmp/wide/logcards3*.png`；
产物 3,172,216 B / md5 `c984bb8b565db99b61c3496f31c3332c` 为改面板前那一版，
修完 logout 与按钮措辞后重编 3,172,568 B / md5 `4c64a22c12849ab781390dc9ed4bf907`，**两轮都跑过**；
全程未打 `apply` / `rollback` / `hijack enable`，现役 `supervise-daemon zbox` 与 `/var/lib/zbox` 未动）：

1. 现场 = kernel 12 行（含 3 行"规则集"、1 行 ERROR）+ supervisor 0 字节 + panel 无文件 +
   oxidns 两份轮转分片（01 短、02 有 166400 B）+ dae 无落点。
   五张卡徽标一次到位：`已读取 / 文件是空的 / 还没有文件 / 已读取 / 没有落点`，顶部 `有内容 2／5`。
2. **oxidns 那张取到的是 `oxidns.log.2026-10-02` 的末尾 1023 条**，并明说
   "前面还有没给的（只取最后 65536 字节）"——glob 走对，10-01 那份没混进来，`sing-box.log` 也没有。
3. 过滤 `ERROR`：只有 kernel 剩 1 条，oxidns 那张说"这一份里没有带「ERROR」的行"，
   两张没文件的卡措辞不变——**按卡各画各的，不是一句全局"无匹配"**。
   过滤 `规则集`：kernel 3 条、oxidns 1023 条同时收窄。全场零命中时徽标仍是 `2／5`（它数的是状态不是过滤结果）。
4. 清空过滤词不产生第二次读取（"取数"那一句时刻没变）；再按一次读取，时刻从 `13:44:41` 变到 `13:44:50`。
5. 420px 窄屏：五张卡堆叠、卡宽 352px、正文各自滚动、读数行自然换行；**控制台零报错**。
6. 裸 HTTP 侧同场复核（guest 本机 `nc -w`，打在它绑的那个地址上）：`sources` 五张、
   六种 `state` 里这一场出现四种、`grep -c 'root/u8'` 为 **0**（回执不带路径）。

**这一轮真机抓出来的问题（一处，已修）**：退出登录把五张卡清掉了，`#logs-filter` 却还是**可输入**的，
而它在 HTML 里的开局态是 `disabled`。屏上于是同时写着"未读取"和"只画带某几个字的行"，
后者已经没有对象。修在 logout 那一段（清词 + `setLogsEnabled(false)`），并钉成编译期一条：
退出那一段必须同时含 `lastLogs = null`、`renderLogs()`、清词、`setLogsEnabled(false)` 四件事。
顺带把按钮那句"读取末尾日志"改成"读取这几份日志"——它从一来源变成五来源之后，
单数那句说的是假话（`GET /` 回来的 HTML 里逐字复核过）。

**这一轮的边界（三条，都不修，先记着）**：

1. **`journal` 那一路真机没走过**：guest 是 OpenRC，systemd 那一句只在单测里成立。
   换到 Ubuntu/Debian 上跑第一版时要专门看一眼那一张卡。
2. **`dae` 那句"没有落点"是当下事实，不是永久形状**：如果将来 #56 那类改动给 dae 配了日志文件，
   要改的是 `logs()` 里那一条 `LogHome::Missing` → `LogHome::Fixed`，卡片文案由后端换，前端不用动。
3. **这一屏仍然只有"末尾一截"**：往上翻、按时间跳、多日拼接都不在这里，也不打算加——
   要看整份的出口是机器上那份文件本身，面板明说"不动它们也不删它们"。

宿主库测试 **515 → 519**（后端 4 支新增，`assets.rs` 那支是重写不是新增）。
**没做的**：dae 的 `.dat` provision（#56）、#69、#77 的 last_good 分支、#7 剩下的运行监控面。

### 11.15 U9 默认档全流程走查已落地（2026-10-12 实测，task #108）

**这一格不是一轮改动，是一次"以用户身份把那条主路走完"。** 判据换成能核对的读数而不是感觉：
默认档那 8 屏逐屏量**字数**、每个按钮的**可用态与 y 坐标**、**文档高**、程序换屏后的**落点**、
以及四栏上屏的**措辞**。驱动 `.tmp/wide/cdp-journey{,2}.mjs`（两支，CDP 9338/9339），
逐字读数 `.tmp/u9/round-u9{,b}.jsonl`（20 + 32 支），图 `.tmp/u9/shots/`。
现场 = 两个一次性根同跑：`/root/u9/root`（已导入 3 出口 + 已加一台设备，端口 8124）与
`/root/u9/fresh`（磁盘上还没有状态文件，端口 8125），订阅由 guest 本机 busybox `nc -e` 桩喂（8099）。
**主路第④步这一轮第一次走通到落盘**：加一台 `10.99.0.77` → 改档 → 保存 → **整页重载**仍在
「不科学」栏、那一格仍是 `direct`（回执 `已写入磁盘，新指纹 cb2a24f792bb0e92`）。

**真机抓出来四处说谎／挡住路的东西，全部当场修掉并钉成编译期断言：**

1. **设备栏两个栏题在说谎**（§11.3 C 与 §11.11 那两处旧名一并作废）。
   `global` 那栏原来叫「智能分流」——它其实只是"这台跟着「默认怎么走」那一排走"，
   而那一排是**三档**：默认设成不科学时这一栏的设备就是不科学。
   `proxy` 那栏原来叫「全局」，和那一排的第三档**同名不同义**，两个人话词指两件事。
   改成 **跟着默认走 / 强制走代理**，卡片下拉那四句同步（`不科学：全部直连 / 跟着默认走：不单独定 /
   强制走代理：全部走代理 / 不接管：完全不经这台机器`）。断言双向：四个新栏题在 HTML 与 JS 逐字相同，
   且 `!html.contains("<h3>智能分流</h3>")`、`!html.contains("<h3>全局</h3>")`——
   那两格是「默认怎么走」那一排的档名，**一条界：一个名字在一屏上只指一件事**（§11.8 那颗三档开关没动，
   它仍然叫 不科学 / 智能分流 / 全局，本轮实测三档在位、`smart` 仍 `on`）。
2. **服务端的内部值 `absent` 漏上人脸**，两处：连接回执那句"磁盘指纹 absent"、
   概览那一格的值。对新用户这两个英文词不携带任何信息，而它恰好是"接下来该做什么"的唯一提示。
   改成 概览格 `还没有（磁盘上没写过状态文件）` + 回执
   `已读取。磁盘上还没写过状态文件，表单里是内置底稿——改任何一格再按「保存到磁盘」就写下第一份。`
3. **新机器连上之后被扔在读数屏**。旧落点是无条件 `setView("overview")`，
   而"带着令牌打开面板的人要看机器现在的样子"这一句，对一台**一个出口都还没有**的机器是空话：
   主路的第②步（贴订阅链接）在下一屏。改成按**有没有出口**分流——
   `noEgress ? "nodes" : "overview"`，且 `source === "broken"` 不参与这个判断
   （读不出来的那份状态送去「订阅与节点」，那一屏此刻没有内容可看）。
4. **窄屏（420px）主按钮在折线外**：导航那条侧栏靠换行摊成 **439px 高**，
   「订阅与节点」的 `#sub-url` 落到 y=907、导入按钮 y=911，而视口只有 900——
   手机上新用户打开面板，第一屏**只有导航，没有要做的事**。
   改成**顶上两行**：品牌 + 退出/开档一行，八个屏名一条只能横向滑的轨（35px）。
   实测 `#sub-url` 回到 **560**、导入按钮 **564**，文档高 1289 → 首屏就看得见那一栏。

**滑轨这一条的形状，钉四处（`assets.rs::the_narrow_nav_is_one_scrollable_strip_not_a_pile`）：**
`#nav` 在 56rem 那条里必须含 `flex-wrap: nowrap` / `overflow-x: auto` / `flex: 1 1 100%`，
且**不许**再出现 `flex-wrap: wrap`（那正是摊成 439px 的那个形状）；
高级那一层要 `#nav-advanced:not([hidden]) { display: contents }` 摊进轨里——
`:not([hidden])` 不是讲究，是 `display: contents` 会**盖掉** `[hidden]` 的 UA 规则，
少了它就成了"关着档也看得见高级屏名"。真机两侧都量过：关档时那一格里第一格宽 **0**，
开档后 **76px** 排进轨里、轨高仍是 35px（多八格不会把内容重新推下去）。
JS 那一半钉 `revealActiveNav(` 恰好两处（定义 + `setView`）、`scrollIntoView`、
以及 `inline: "nearest"` 且**不是** `"center"`：程序换屏（连上之后落到第②步）时那一格可能在轨外，
看不见等于没有高亮；而 `center` 会把够得着的那一格也推到轨中间，顺带把左端的「照着做」滑掉半截
（实测 `scrollLeft` 0 → 21，截图上标签缺字）。`nearest` 之后第②步 `scrollLeft: 0` 标签完整，
跳到轨尾那一屏 `scrollLeft: 1626`（= 最大可滑量）且 `activeInside: true`。
**宽屏那条竖轨不许被动**：这条判断靠 `nav.scrollWidth <= nav.clientWidth` 提前返回，
1440px 实测 `scrollWidth == clientWidth == 228`、`scrollLeft: 0`，未受影响。全场**控制台零报错**。

**这一轮的边界（三条，都不修，先记着）：**

1. ~~新机器概览自己跟自己打架~~ —— **当轮就修了，见下面第 5 条**（走查记下来时以为要放下一轮，
   结果同一台机器上两个分支都量到了，就一并改掉）。
2. **走查只覆盖到"保存落盘"，没覆盖到"生效"**：第⑤步（让它生效）按的是既有纪律——
   `apply` / `rollback` / `hijack enable` 一律不打，所以主路最后那一格在默认档上仍是**没走过真机**的。
3. **窄屏只量了 420px 一档**：360px 与横屏手机（键盘弹出后的可视高）没量；
   滑轨这一条在极窄处会只剩三四格可见，靠的是"能滑"这个前提，而滑动手势本身没被任何断言钉住。

**第 5 处修正（同一轮补做）：概览「待下发」那一格不许对从没下发过的机器说"不一致"。**
现场是走查记下来的：一台**存过盘、但从没成功下发过**的机器（正是主路第②步之后的样子），
同一张卡片上两格分别写着 `待下发 是（内容与已知良好修订不一致）` 与 `已知良好修订 尚无`。
前一句是在指控这台机器失败过一次，而它一次都没试过。
修法是照搬「分流梯级」那一屏**同一个分法**（都看 `last_good` 在不在，见 §11.11 那两句），
而不是再造一套口径：
`pending=true` 拆成 是（这台机器一次都没成功下发过，没有"已知良好的那一份"可比） /
是（内容与已知良好修订不一致） 两句，`pending=null` 那第三句（`未读到…`）本来就诚实、没动。
**两个分支都在真机读到**（一次性根 `/root/u9v2/fresh` + 端口 8126，驱动 `.tmp/wide/cdp-pending.mjs`，
逐字 `.tmp/u9/round-pending.jsonl`）：存过盘而没有任何修订时上屏第一句，
手工造出 `revisions/<修订号>` + 把同名写进 `revisions/LAST_GOOD` 之后上屏第二句、
且那一格旁边就是那个修订号——两句话不再打架。
顺带把这条也量实了：**一台全新的根上「保存到磁盘」确实写得下第一份**
（回执 `已写入磁盘，新指纹 1032274eccb13bac`，同屏第二句照旧是渲染不出编排配置那句常态警告），
上面第 1 条修正里那句"改任何一格再按「保存到磁盘」就写下第一份"因此不是空话。

宿主库测试 **519 → 521**（`assets.rs` 新增 2 支：新机器那两句话与落点、窄屏那条轨；
第 5 处修正的两条断言并在那支新机器测试里，不另计数）；
musl 产物 3,175,448 B / md5 `12f02508c94b15ca7bafcaba273d3e2a`，改完 `nearest` 后
`e3e6cf778564607a6514bc920c4e2bd4`，补完「待下发」那一格后 `15a17cf6ba0c08a90c57e1a215e17d8c`，
**三轮都跑过真机**，字面量从 `GET /` 与 `GET /panel.js` 复核。
**没做的**：§11.13 边界 1 与 3 两处措辞（**下一轮 §11.16 已收口**）、dae 的 `.dat` provision（#56）、#69、
#77 的 last_good 分支、#7 剩下的运行监控面。

### 11.16 §11.13 边界 1 与 3 两处措辞已收口（2026-10-12 实测，task #109）

一句话：**报错的"动作标签"属于出口，不属于共享串**。这两条都是 §11.13 明写着"留待下轮"的，
改动全在后端串的形状上，前端三份资产一个字没动。

**边界 1：`保存被拒（HTTP 400）：期望状态不成立: 共 3 处：` 那句里有两层标签。**
`期望状态不成立: ` 是 `state::parse` 的 `map_err` 前缀，四条出口共用（CLI 读盘、面板 PUT、
`doctor`、生成器那道门），而每条出口自己**本来就已经有一句框**——面板那句是
`保存被拒（HTTP 400）：`，doctor 那条是 `期望状态本身不成立，生成器已拒绝`，CLI 那句前面是
`zboxd: <文件路径>: `。所以：

- `state::parse` 与 `GenerateError::InvalidState` 都不再顶自己的标签（后者原来是
  `#[error("期望状态不成立: {0}")]`，那条 Display 会拼到面板"但生成器对这一份报错："后面，
  同一类叠法），只把 [`describe_problems`] 的清单原样交出去；
- 帽子自己带主语：`共 3 处：` → **`这 3 处不成立，要改：`**。这一句要能**单独**跟在
  文件路径后面（CLI）也读得通，因为那里没有别的框；又要跟在"保存被拒（HTTP 400）："后面
  不多出一层标签。带总数那条上限的句子照旧说真数（13 条只列 12 条时帽子仍写"这 13 处"）。
- 只有一条时**仍然不加帽子**（§11.13 那条"一条就是平时那一句报错"的界没动）。

真机读数（一次性根 `/root/u10`、端口 8128、驱动 `.tmp/wide/cdp-problems10.mjs` = U7 那支的副本，
逐字 `.tmp/u10/round-problems.jsonl`，图 `.tmp/u10/shots/`；全程没打 `apply` / rollback / hijack enable）：
一处坏 → `保存被拒（HTTP 400）：局域网 CIDR "not-a-cidr" 写法非法`（`lineBoxes: 1`，没有帽子）；
三处坏 → `保存被拒（HTTP 400）：这 3 处不成立，要改：` + 三条各一行（`lineBoxes: 7`、`height: 82`）；
修好一条 → `这 2 处不成立，要改：`（`lineBoxes: 5`）；窄屏 420px 同一份排 `lineBoxes: 7`。
**400 之后磁盘那份一个字节没动**（三次读数 `etag` 都停在 `89633d2fc7b2ed56`），全修好才写下去
（新指纹 `9d7fae0e62adf4ca`，与 U7 那一轮同内容同指纹——指纹是内容函数这一点顺带又被证了一次）；
`控制台报错` 为空。

**边界 3：`策略组 tag "" 与节点重名` 说错了对象。** 两种撞法原来共用一句话，而撞组那一种
报的是"节点"，用户会去「订阅与节点」那一屏找一个根本不存在的重名。改成撞谁说什么：
组撞节点 → `与节点重名`；组撞组 → **`与另一个策略组重名`**；**空 tag 不进这道比较**——
它谁都没撞，只是没名字，而"策略组缺少 tag"上一行已经报过了。
顺带把 §11.13 那条注释里"两个空 tag 互相撞本来就是另一句话，不该被压掉"钉反了的说法改过来。

钉成编译期的三条（全在 `src/model/config.rs` 与 `src/api/mod.rs`、`src/model/state.rs`）：
清单里出现 `期望状态不成立` / `保存被拒` 那种动作标签就红（反向断言，两处）；
两个匿名组的清单里出现任何"重名"就红；组撞组必须写"另一个策略组"且**不许**写"节点"。

宿主库测试 **521 → 522**（新增 `a_tag_collision_names_the_side_it_collided_with`；
措辞改动让 6 支既有测试跟着换断言，不是新增）；musl 产物 3,176,824 B / md5
`baab5e40cb7e75b1afa5ee5325b281ef`，push 后跑的那一轮就是上面这些读数。

**这一轮的边界（一条，已核完，结论是"不是缺口"）**：面板那三次"0 磁盘 / A 磁盘 / D 磁盘"读数里
`lan_interface` 一直是出厂的 `ens33`（这台机器实际是 `eth0`）——`fill_first_network` 只排在
**订阅导入**那一条写盘路径上（`api/mod.rs:604`），而"新机器直接按「保存到磁盘」写下第一份"这一条
不走它。**下一轮读代码定案（不必再开真机）**：`src/bin/zboxd.rs:246` 那一处 `fill_network` 覆盖
`install|apply|enable-hijack|rule-sets|doctor|render|serve` 七条命令，**探测只进内存、从不写盘**——
这正是 §11.14/U3b（task #99）的定案，不是它漏了另一半。而出厂 `ens33` 本身就是
"这一格没人写过、交给现场"的哨兵值（`topology::How::Detected` 与 `KeptExplicit` 的分别），
面板两处措辞也已经按这个口径写实：看板栏题「局域网网卡（**盘上那份写的**）」与
`assets/panel.js:1139` 那句"措辞是'盘上写的是 X'，不判 X 在不在"，「网卡与网段」的 note 又明说
"只要有一格跟内置默认形状不一样，网卡探测就不再覆盖它们"。
**所以这一格唯一在效的要求是：磁盘上那个占位值不许被写成现场读数**（现已如此）；
要动它得先决定"落盘的是探测结果还是哨兵"，那是模型语义改动，不在措辞轮里顺手做。

### 11.17 U11（2026-10-12，task #110）：主路第①步读的那张说明书，三句谎

**一句话定案**：说明书自己不会红任何测试——它讲的是"别的屏上有什么"，别的屏一改它就安静地变成谎话，
而读它的正是最没耐心去核对的那个人。所以这轮不改界面，只把连接屏那张「这条路怎么走」逐句对着现 UI 走一遍，
**并把"对得上"这件事本身钉进编译期**。

三句谎（都是前几轮改名留下的，不是新写的）：

1. **③ 默认怎么走**还写着三档开关之前的话——"要的'智能分流'就是这一屏默认那份名单，通常不用动"。
   两句都不真了：智能分流现在是 U4b 那一排里的一颗钮（不是"名单"），而新机器出厂那一颗是「不科学」
   （`gen` 的兜底出口默认 `direct`，屏上那句"新装就是这个，装了不断网"是同一件事）。
   现在写成：三颗钮逐个点名 + "要科学上网就按「智能分流」那一颗" + "上一屏贴订阅按「导入订阅」时
   ZBox 会顺手把它设好" + "新机器出厂是第一颗「不科学」，那一档装上也不断网"。
2. **⑤ 让它生效**说"保存到磁盘、下发到数据面、要退回时挑回上一好的那一份——**三步都在这一屏**"。
   真机量出来是 `{"inTopbar":true,"insideAnyPage":false}`：`#save` 与 `#revert` 住在 `header.topbar`，
   **不在任何 `section.page` 里**。人在那一屏找不到保存，就会以为"这一屏还没到保存那步"。
   现在 ⑤ 只管下发与回滚，另起一句实话：**「保存到磁盘」不在哪一屏里面**，它挂在页面顶上那一排，
   并把原来那条重复的"保存到磁盘不等于下发"合并进同一句（两处只留一处）。
3. **顶上那句**"其余各屏都住在侧栏底部那个「开启高级模式」后面"——漏了「现在怎么样」那一组三屏
   （现在的状态 / 看板 / 解析记录），它们是普通档、不开高级模式就在。改成三档分明：
   必经五屏 / 只看不改的三屏 / 要动规则本身的才在高级模式后面。

**真机逐字读数**（`.tmp/u11/round-guide.jsonl` + `.tmp/u11/shots/guide-connect.png`，
驱动 `.tmp/wide/cdp-guide.mjs`，CDP 9349 / 面板 8131 / 一次性 root `/root/u11`）：
五步的 `<dt>` 逐一点过导航，顶栏页题**逐字等于**那一行的屏名
（`① 连接与令牌 → 连接与令牌`…`⑤ 让它生效 → 让它生效`）；说明书点名的五颗按钮
`导入订阅 / 保存到磁盘 / 放弃改动，重读磁盘 / 开启高级模式 / 显式下发当前期望状态`
在页面上各命中 **1** 次；「那三个「加一条」按钮」= `加一条 VLESS+Reality / 加一条 Hysteria2 / 加一条 TUIC`；
`#mode-switch` 三颗 = `不科学全部直连 / 智能分流大陆直连，其余走代理 / 全局所有域名都走代理`。
**控制台只剩一条**：`401 http://…:8131/favicon.ico`——浏览器自己发的，面板的静态表只有三条字面路径，
而门排在路由之前（§11 那条纪律），所以未带令牌的 `/favicon.ico` 得到 401 而不是 404。
**这是正确行为不是坏了**，但记下来：以后谁的驱动里"控制台报错为空"这条判据要先把 favicon 排掉。

**钉法**（`src/api/assets.rs::the_five_step_guide_names_only_screens_and_buttons_that_exist`）：
① 五步的 `<dt>` 屏名逐字是侧栏 `<span class="label">` 的内容，**且顺序逐项相等**（说明书的顺序就是主路的顺序）；
② 说明书里用「」引的按钮名，既要 `>名字</button>` 真有其物、也要卡片里确实这么引着（搬家了不会没人知道）；
③ 三档档名逐字是 `>不科学<span>` 那三颗，卡片里要用「」引全，且"出厂是第一颗「不科学」"这句要留在屏上；
④ 反向钉三句谎：`三步都在这一屏` / `其余各屏都住在` / `这一屏默认那份名单` 任一出现就红。

宿主库测试 **522 → 523**；musl 产物 3,177,656 B（改 ③ 引号后重编）/ md5
`d4f3e1a3fdfded7b0331ff9e026996e1`，push 后 guest `md5sum` 同值，上面那些读数出自跑这份产物的面板。
边界照旧：没打 `apply` / rollback / hijack enable；收尾 `kill` 掉一次性实例、`rm -rf /root/u11`，
活机器复核 `status: started`、`nft list tables` 空、`ip -4 rule` 三行基线、8131 已无人听。
**新坑一条**：`ps -o pid,args | grep "[u]11/zboxd"` 抓不到那个进程（它的参数是 `./zboxd --root /root/u11/root …`，
`u11/zboxd` 这个连法压根不存在），于是 `kill` 收到空参数报 `you need to specify whom to kill`、
而同一行后面的 `rm -rf` 照跑——**目录先没了、进程还在听**。要么按端口找（`nc -z` 复核 + `ps` 里 grep `--port 8131`），
要么把 kill 与 rm 分成两条、中间用 `nc -z` 确认端口已空。

### 11.18 U12（2026-10-12，task #111）：把 U11 那套对照从一张卡扩到整份页面

**一句话定案**：U11 只钉住了「这条路怎么走」那一张卡，而**同一类谎住在整份文件里**——
任何一句"按「某颗按钮」""那一屏「某个屏名」"都是对照点。这一轮把判据扩成全量扫描，
并**先证明这条测试会红**，再谈它绿。

**全量对照跑出来的现场**（宿主机，不碰机器）：`panel.html` 去掉 HTML 注释后用「」引起来的名号
**26 个**，控件文案表（侧栏分组题 3 + 导航 label 16 + 按钮 52 + 卡片标题 + `<dt>` 25，去重共 **149** 条）
逐字能对上 **22 条**；剩下 4 条全部**不是错**，但每一条都要单独交代：

| 那一条 | 为什么它可以不是控件名 | 怎么钉住 |
|---|---|---|
| 「没有落点」 | 它是「分流情况」那张表自己画出来的**行状态词**，由 `panel.js` 给 | 断言 `js.contains("没有落点")` |
| 「一键复制」 | 那是**机场控制台**上的按钮，不是 ZBox 的 | 断言那句必须连前半句一起在场：`机场控制台「一键复制」` |
| 「哪一条连接走了代理」 | 看板明写"这一屏**给不出**"，引的是那句话本身 | 断言 `这一屏给不出「哪一条连接走了代理」` |
| 「这条走了代理」 | 同上，且它引的是"本项目明令不许的做法" | 断言 `「这条走了代理」是本项目明令不许的做法` |

**这轮最重要的一条是"后门要不要留"**：第一版实现给了个宽松逃逸——"这个词在 `panel.js` 里逐字也出现"
就算通过。写完就发现它把整条测试写废了：**按钮改了名，JS 回执里那句旧名多半还留着**，
于是每一次改名都能从后门溜走。删掉后门，改成例外必须**点名 + 当场自证那句理由仍然成立**
（上表最后那一列）。例外表同时**双向**：多一种新引法要红，那句被改掉后表上留着的旧条目也要红。

**第二条新钉的**（`the_sidebar_groups_have_the_names_and_counts_the_guide_claims`）：说明书顶上那两句
是**数得出来**的承诺——「照着做」五屏、「现在怎么样」三屏。侧栏三个分组题逐字是
`照着做 / 现在怎么样 / 高级：改规则本身`，各组导航项数 **5 / 3 / 8**，`data-tier` 的 basic 与 advanced
各 **8**。这一条正是 U11 第③句谎的形状（"其余各屏都住在开启高级模式后面"漏了那三屏）——
上一轮靠人读出来，这一轮把它变成数数。

**先证明它会红**（负向对照，做法与结论都记下来）：把 `#sub-import` 那颗按钮文案从「导入订阅」
临时改成「导入链接」，跑两条测试 →
`every_quoted_name_on_the_page_resolves_to_something_real ... FAILED`，
panicked 的话是 `页面上有「」引着的名号不是任何控件的文案：["导入订阅"]`——**它点名的是那个词，
不是"某个断言失败"**；还原后 **525 全绿**。没有这一步，"全量对照通过"这四个字不值钱。

**边界与产物**：本轮**没有改任何资产**（`panel.html` / `panel.js` / `panel.css` 逐字未动，
那次改名是测试用的临时改动、已还原并用 `grep -c 导入链接` = **0** 复核），所以 musl 产物
**md5 仍是 `d4f3e1a3fdfded7b0331ff9e026996e1`**、与 guest 上跑着那份同一个字节串——
**本轮不需要重编、不需要 push、也没开真机**（改动只在 `#[cfg(test)]` 里，不进二进制）。
宿主库测试 **523 → 525**（`api` 138 → **140**，其余 gen 168 / svc 111 / model 44 / doctor 27 /
ingest 23 / apply 12 未动）。留一条**没做**的：`panel.js` 侧那 50 处「」只做了宿主机扫描、
没进编译期断言（JS 里绝大多数引号是注释和字符串拼接，先要一个能分注释的收集器才有意义）。
