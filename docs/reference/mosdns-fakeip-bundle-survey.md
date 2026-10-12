# 勘察：mosdns + sing-box「fakeip 分流法」三个成品包

对象（`reference/` 下的三个 zip，Telegram 渠道分发）：

| 包 | 体积 | 里面到底是什么 |
| --- | --- | --- |
| `mosdns ui + fakeip分流法全套.zip` | 89 MB / 1257 文件 | 两套完整落地目录（`openwrt/`、`一个mosdns+sb虚拟机/`）+ 9 份 docx/html 说明书 + iOS 端配置 |
| `mosdns-ph-20251029全新单独部署版本.zip` | 17 MB / 101 文件 | 只有 mosdns 那一半：`cus/bin/mosdns` 二进制 + `cus/mosdns/**` 规则与 yaml + `cus/etc init.d/mosdns` |
| `alpine-mos+sb虚拟机.zip` | 226 MB / 17 文件 | **纯 VMware 虚拟机镜像**（flat.vmdk 4 GB 展开 + snapshot + vmx + 日志），没有任何配置文件 |

处理约定：只解包、只读，**绝不执行其中任何二进制或脚本**；解包前用 `unzip -Z1` 逐个确认无
`..`/绝对路径/符号链接条目，且只挑选需要的小文本文件解到临时目录（`cus/bin/*`、`.vmdk`、UI 资源
一律不解），成品包不进版本库。

---

## 1. 被推翻的结论（先写这条）

**先前假设"参考包里有 OpenRC runscript 的形状可以借鉴"是错的。**

三个包里能找到的所有服务脚本只有 4 份，全部是 OpenWrt 的
`#!/bin/sh /etc/rc.common`（`START=90`/`START=92`、`pidfile` + `pgrep -f` 双保险、`kill` 失败再
`kill -9`），没有一处 `#!/sbin/openrc-run`，也没有一个 systemd unit：

```
一个mosdns+sb虚拟机/cus/init/{mosdns,sb}        # rc.common
openwrt/cus/init/{mosdns,sb}                   # 同上（两份目录内容一致）
cus/etc init.d/mosdns                          # ph 包，同上
```

后果与处置：

- `src/gen/init.rs` 里的 OpenRC runscript（`command_background` / `pidfile=/run/zbox.pid` /
  `depend(){need net; after net}` / `start_pre()` 轮询默认路由）是**按 OpenRC 官方惯例写的，
  不是借鉴物**——那条注释已改成如实描述。它是否能在真 Alpine 上跑起来，仍属未实测（见
  `docs/ARCHITECTURE.md` §8.6）。
- 从这些包能借鉴的只有一件事：**它们都没有进程监督**。rc.common 只管 start/stop，进程死了没人拉，
  `QUIC_GO_DISABLE_GSO=true` 也是塞在启动命令行里的。ZBox 在 systemd 侧补 `Restart=on-failure`、
  OpenRC 侧补 `respawn=true`、两侧都补 `/etc/sysctl.d/` 持久化，是**补齐缺陷而不是抄形状**。

## 2. 参考包跑的不是官方 sing-box

`一个mosdns+sb虚拟机/cus/singbox/config.json` 里出现了这些键，它们在 1.14.2 的
`option/*.go` 中**不存在**（对照 `reference/singbox-1142/`，那里只有 `certificate_providers`）：

| 观察到的键 | 出处 | 判定 |
| --- | --- | --- |
| 顶层 `providers[]`（`type: remote` + `url` + `path`） | config.json:120 | MetaCubeX 分支特性 |
| selector/urltest 的 `use_all_providers`、`include`、`exclude`（正则筛节点名） | config.json:158,160 | 同上 |
| `interrupt_exist_connections` | 几乎每个策略组 | 同上 |
| `dns.independent_cache` | config.json:53 | 1.11 起移除，官方改 `cache_id` |
| `rule_set[].download_detour` | 每个 remote 规则集 | 1.14.2 里字段还在但带 `schema:"omit"`，官方写法是 `http_client{detour}` |

**结论**：这套配置的转发内核是 MetaCubeX 的 sing-box 分支（和 `reference/abox` 里链了
`github.com/metacubex/sing` 是同一谱系，见 [[zbox-references]]）。所以它的 config.json **整体不可抄**，
ZBox 只能借拓扑；抄过去会直接撞在 `DisallowUnknownFields` 上。
反过来也验证了一件事：`route.rules[]` 里的 `{"inbound": ..., "action": "hijack-dns"}` 是**官方 1.14.2
就有的**（`option/rule_action.go:19` 的 enum 里），ZBox 的 `gen::singbox::route` 已经在用它。

## 3. 它的 DNS 拓扑（observed）

```
LAN 设备 ──DNS──> mosdns :53 (udp+tcp)              ← 分流决策全在这里
                    │  境内域名 → 上游 2222/3333/4444 (本地/境外/ECS)
                    │  境外域名 → udp://127.0.0.1:7874
                    └ 环回上还开着 5656 / 7777 / 8888（for_singbox.yaml、forward_*.yaml）
                                      │                        │
                              sing-box in-dns(direct 入站)   localDns/nodedns
                              dns.rules: inbound in-dns → fakeipDNS
                              fakeip 28.0.0.0/8 + fc00::/18
```

- `dns.final = localDns(127.0.0.1:8888)`，`route.default_domain_resolver = nodedns(7777)`。
- mosdns 额外监听 `:66`（"刷新服务器"），是 requery 任务的触发口。
- 数据面劫持：UDP 走 `tproxy :7895`（mangle prerouting），TCP 走 **nat redirect `:7899`**，
  另有 socks `7891` 和一个 `shadowsocks` 入站 `10813`（给 iPhone 无公网 IP 时反向用）。

**这条拓扑正是 ZBox 明确不做的形态**：DNS 决策放在 sing-box 外面的第二个守护进程里，面板要同时管
两套进程、两份配置、两个重启顺序，且"配置下发→回滚"的原子性被打散（和
[[zbox-references]] 里 LocalDNS-FJ945 的结论一致，见 `docs/reference/localdns-fj945-survey.md`）。
MVP 仍然用 sing-box 内置 DNS + `dns.rules` 分流。

## 4. 它的 nftables 层（observed，决定了 ZBox 必须保留双 mark）

`nftables.conf`（顶层与 `cus/` 下两份内容相同）：

- 单表 `table inet sing-box`，四个 set：`local_ipv4/local_ipv6`（fakeip 段 + 公共 DNS + 一批
  Netflix/Telegram 网段）、`router_ipv4/router_ipv6`（fakeip 段 + 公共 DNS）。
- **分流判据是目的地址命中 set，不是 fwmark**：`preraw`(priority raw -10) 把非 tcp/udp 和
  `daddr != @local_ipv4` 全部 `accept` 掉，只有命中 set 的包才进 mangle/nat 链。
- `meta mark set 1` 只出现在 tproxy 与 output 两条链上，且 `route.default_mark` 也是 **1**。
  它没出事，是因为它压根不靠 mark 做策略路由——文件里**没有任何 ip rule / 路由表**，也没有任何
  `sysctl`（它是主路由侧的角色，不转发别人的流量）。
- ZBox 是旁路由：nft 打的 mark 必须被 `ip rule fwmark → table 100` 接住，而出站又要避免被自己
  二次劫持，所以 `DIVERT_MARK=0x1` 与 `BYPASS_MARK=0x2` **必须不同**（见
  `docs/reference/singbox-1.14.2-schema.md` §9）。照抄它的单 mark 会让两者相等，防回环失效。
- TCP 用 nat redirect、UDP 用 tproxy 的混搭，ZBox 暂不采用：redirect 只能拿到目的 IP，配合
  fakeip 时要靠 `sniff`/`resolve` 反查域名，而它的配置里所有入站都是 `"sniff": false`，境外
  域名识别完全依赖 mosdns 预先写好 ipset——这正是 ZBox 用 `rule_set` + sing-box DNS 直接省掉的环节。
- 一条值得记住的防泄漏手法：`route.rules` 里有一条把 8.8.8.8/8.8.4.4/1.1.1.1/1.0.0.1/9.9.9.9
  的 `ip_cidr` 规则强制指向代理组，即"设备绕过面板手填公共 DNS"也要被接管。ZBox 用
  `dns.overseas` + doctor 的 `dns_leak_no_overseas` 判据覆盖同一类问题，**不**采纳它另一条
  `{"network":"udp","port":443,"action":"reject","method":"drop"}`——那会顺手打死 QUIC/Hysteria2。

## 5. 面板与运行时可变文件（rejected）

- "UI" 是 sing-box `experimental.clash_api`：`external_controller` 绑 `0.0.0.0:9090`，
  `external_ui` 直接加载现成的 **zashboard**（`external_ui_download_url` 指向
  Zephyruso/zashboard 的 release），`secret` 是弱口令级别的占位值。
  → ZBox 面板自研、必须鉴权，且**不把数据面 API 暴露到 0.0.0.0**（`docs/ARCHITECTURE.md` §7）。
- 规则侧的运行时可变文件：`rule/switch1..9.txt`（A/B 热切换）、`gen/`（自学习产物）+
  `genblank/`（出厂空白模板）+ `unpack/`（下载的规则正文，带 `.bak`）、`domain_output` 插件把
  学到的域名 POST 回面板、requery 定时重查（限 QPS、带 `task_state`/progress）。
  → 与"期望状态是唯一真值 + 快照指纹 + 失败回滚"直接冲突：面板上看到的是快照，生效的却是磁盘上
  另一个随时被进程改写的文件。ZBox 不引入这类热文件；规则更新走 `ingest::ruleset` →
  `sing-box rule-set compile` → 进快照。
- `adguard/<UUID>.rules` + `adguard/config.json` 是 mosdns 的 AdGuard 规则插件目录，
  已在 `ingest::ruleset::SourceFormat::AdGuard` 里以纯函数形式支持（不依赖任何外部进程）。

## 6. 敏感物（只记类型与位置，值不落盘）

这些包是别人线上环境的快照，**不要把任何值抄进 ZBox 的代码、文档或配置**：

- `alpine-mos+sb虚拟机.zip`：整块虚拟机磁盘（含 `/etc/shadow` 级别的口令哈希），以及
  `alpine-dns-Snapshot12.vmsn` —— 快照文件带**内存镜像**，可能含运行时的会话密钥。
- `一个mosdns+sb虚拟机/cus/singbox/config.json`：`inbounds` 里 shadowsocks 入站的口令（模板占位）、
  `outbounds` 里一个自建 shadowsocks 出站的真实服务器地址与口令、`clash_api.secret`。
- `route.rules` 的逻辑条件暴露了它的内网管理段（`10.10.10.0/24`），`outbounds` 里
  `bind_interface` 暴露了 OpenWrt 网桥命名。
- ph 包的 `cus/mosdns/rule/client_ip.txt`、`gen/top_domains.txt`：内部设备 IP→别名字典与
  学习到的域名统计，属于真实局域网的清单。

勘察用的小文本文件只解到临时目录，看完即弃；三个 zip 本体留在 `reference/` 但**不加进版本库**。

## 7. Alpine 侧唯一有用的硬事实：那份 vmx

镜像本身读不了内容，但 `.vmx` 里有 VMware Tools 上报的 guest 信息，可反推这套方案的目标系统：

```
distroName='Alpine Linux'  distroVersion='3.22.2'  kernelVersion='6.12.55-0-virt'
numvcpus=4  memSize=1024  ethernet0.virtualDev=vmxnet3  scsi0.virtualDev=pvscsi
virtualHW.version=15  guestOS=other4xlinux-64
```

也就是说：Alpine 3.22 + `linux-virt` 内核 + 4 vCPU / 1 GB 就能跑 mosdns+sing-box，这正是用户想
把 VM 换成 Alpine 的动机（占用更小）。但要拿去 Hyper-V 里复现，需要：vmdk→vhdx 转换；内核侧
`hyperv-*` 模块（virt 内核默认带）；网卡换成 Hyper-V 合成网卡后确认 GSO 行为（ZBox 的服务制品
统一注入 `QUIC_GO_DISABLE_GSO=true`）。这些都在 `docs/ARCHITECTURE.md` §8.6 挂着待实测。

## 8. 本次勘察落成的代码

| 采纳 | 落在哪 |
| --- | --- |
| OpenRC runscript（后台化 + pidfile + `depend need net`） | `src/gen/init.rs::openrc_runscript` |
| 崩溃拉起（参考包没有） | 同上 `respawn=true`；systemd 侧 `Restart=on-failure` |
| sysctl 持久化（参考包完全没有） | `init.rs::sysctl_conf` + `netstate::sysctl_pairs` 同源，附漂移测试 |
| 启动前等默认路由 | `openrc_runscript` 的 `start_pre()` |
| `QUIC_GO_DISABLE_GSO=true` | 两侧服务制品都注入 |
| 公共 DNS 也要被接管这一类泄漏 | `src/doctor.rs` 的 `dns_leak_no_overseas` / `dns_upstream_bypasses_proxy` |
| 期望 vs 实际的一键排查 | `src/doctor.rs`（源自 `x.1.故障排查.docx` 的组织方式） |

抛弃：外部 mosdns 前置 DNS、daddr-set 分流、单 mark、TCP nat redirect、`providers` 体系、
switchN 热文件 / learned_list / requery、zashboard + 0.0.0.0 clash_api、OpenWrt 的 UCI+ipset 路径。
