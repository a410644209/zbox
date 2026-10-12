# IPv6 落地面 —— guest 实测记录

权威来源：2026-10-09 在 Alpine 3.21.3 VMware guest（VMnet8 NAT，宿主侧网关 192.168.219.1，
内核 6.12.111-0-lts）上跑出来的现场，不是从文档推的。guest 时钟比宿主慢 7 天（显示 10-02），
本文一律按宿主日期记。目录名 `v2026-10-09f` / `v2026-10-09g` 按宿主日期取。

这一份是 `src/gen/netstate.rs`（sysctl + `ip -6 rule`）、`src/gen/nft.rs`（`table ip6 zbox`）、
`src/gen/oxidns.rs`（v6 监听）与 `src/svc/observe.rs` + `src/doctor.rs`（v6 探针与判据）四处接线的依据。
§8 那份 tproxy 语句级结论管的是"劫持层到底拦没拦住"，也就是防泄漏的成色；§10 那份管 dae 的
前提条件与它的监听族。凡是这里没写明的 v6 行为，代码里就不许当成前提。

## 0. 这台 VM 的 v6 现场（用户 2026-10-09 在 VMnet8 上开了 IPv6 之后）

| 项 | 实测值 |
|---|---|
| 前缀 | `fd15:4ba5:5a2b:1008::/64`（ULA，由 VMnet8 的 NAT66 给出） |
| 本机地址 | `fd15:4ba5:5a2b:1008:20c:29ff:fea1:1609/64 proto kernel_ra`（**SLAAC 来的，不是手工配的**） |
| 默认路由 | `default via fe80::250:56ff:fec0:2222 dev eth0 proto ra metric 1024 expires 29xxsec hoplimit 64 pref medium` |
| 公网可达 | `ping6 2606:4700:4700::1111` 零丢包（Cloudflare 的 v6 地址，经宿主 NAT66 出去） |
| `expires` | 会被新 RA 刷新（观测到 2996→2976→2991→2997→…），所以它是活着的 RA 的证据，不是残留 |

结论：**这台 VM 能当 v6 局域网用** —— 有真的 RA、真的 v6 默认路由、真的公网 v6 出口。
`docs/ARCHITECTURE.md` 未决项第 3 条（"要不要补 `table ip6 zbox`，等 v4 路径跑通再定"）
的前提条件已经具备，v6 半边不再是"纸上形状"，可以实测。

现役监听（对照用）：`0.0.0.0` 上的 53 / 9887 / 9888 / 8888 / 7890 / 22，加上 `[::]:22`（sshd）。
也就是说现役数据面（sing-box）一个 v6 口都不开 —— v6 客户端现在是**完全绕过**它的。

## 1. `forwarding=1` 会把 SLAAC 打死：三臂 A/B/C + 还原臂 Z

这一条决定 `netstate.sh` 里 v6 sysctl 的写法。ZBox 的旁路由路线要把 `net.ipv4.ip_forward` 压成 1，
而 Linux 的 `net.ipv6.conf.all.forwarding` 是**整机一个键**（不是按网卡），一旦置 1 就改变主机对 RA
的处理。三臂各跑 15 分钟、每 20 秒记一行（`fwd_all/fwd_eth0/accra_all/accra_eth0 | 默认路由 | ping6`）。
**每臂都写四个键** —— `all.forwarding`、`eth0.forwarding`、`all.accept_ra`、`eth0.accept_ra` 各一次，
所以实测过的组合就是这四条，下面"落地的写法"按它原样抄，不做"其实只写 all 那份也够"这种没证过的化简：

| 臂 | 写入 | 默认路由 | ping6 | 判定 |
|---|---|---|---|---|
| A | `forwarding=1` + `accept_ra=1`（ZBox 若只写转发的样子） | **`ip -6 route show default` 当场为空** | **fail**，整臂 15 分钟无自愈 | 致命：本机自发地址还在，但学不到路由，v6 静默断网 |
| B | `forwarding=0` + `accept_ra=1`（对照，同时证明 RA 周期 < 窗口） | 20 秒内 `default via fe80::… proto ra` 回来 | ok | A 里的"expires 不刷新"是**被内核忽略**，不是 RA 没来 |
| C | `forwarding=1` + `accept_ra=2`（要验的写法） | 全程在，`expires` 持续被刷新（2980/2992/2972/2984/2997…） | ok 全程 | **定案**：中继姿态与自学路由可以共存 |
| Z | 还原成进实验前的实测基线（`forwarding=0` + `accept_ra=1`） | 在 | ok | 实验不留残余 |

**落地的写法（臂 C 是唯一同时满足"当路由器"和"自己不断网"的组合）**：
把 v6 转发置 1 的那一处，必须**成对**写 `accept_ra=2`，且 `all` 与局域网网卡两份都写。
只写 forwarding 不写 accept_ra，代价是这台机器自己的 v6 出站静默死掉（臂 A：整臂零路由、零自愈），
而且它不影响 v4，所以"v4 一切正常"的表象会把这个故障完全盖住 —— 这正是防泄漏最不想见到的
"看起来在管、其实整个族没人管"。

`net.ipv6.conf.default.accept_ra` 全程保持 1（臂 C 没动它）—— 它只管**新出现**的网卡，
与在用的 `all`/`eth0` 无关，写它没有收益，还会把"后来插上的网卡"悄悄变成 RA=2 的中继姿态。

## 2. 监听层的地址族：`[::]` 收不收 v4，是环境事实不是插件保证

（完整表与判据在 `oxidns-plugin-args-verified.md` §1；`singbox-1.14.2-schema.md` §5 有 sing-box 侧的对应条目。）

- 这台 guest 的 `net.ipv6.bindv6only = 0`，所以 `AF_INET6` + `in6addr_any` 且不显式设
  `IPV6_V6ONLY` 的 socket 是**双栈**的。
- **oxidns 1.6.1 不设 V6ONLY**：单开 `[::]:15354` 时 `nc -w1 127.0.0.1 15354` rc=0（v4 连得上）。
- **sing-box 1.14.2 同理**：`listen: "::"` / 19888 → `ss` 报 `*:19888`，v4 与 v6 两侧 `nc` 都 rc=0。
- **`0.0.0.0` 那一侧收不到 v6**：单开 `0.0.0.0:15355` 时 `nc -w1 ::1 15355` rc=1（refused）。
  所以 v6 客户端能不能进，取决于有没有人显式开 `[::]`，"开了 v4 口顺带就有 v6"是反的。
- **同一端口上 `0.0.0.0:P` 与 `[::]:P` 能同时 bind，不 EADDRINUSE**（先 v4 后 v6，14 个插件全过）。

由此定的形状，2026-10-11 改过一次（#55 / P2c，`src/gen/oxidns.rs::dns_binds`）：解析口**不再绑
通配地址**——v4 那一对绑 `network.lan_address`，`ipv6_mode != off` 时追加
`udp_in6`/`tcp_in6`（`listen: '[<lan_address6>]:<port>'`），v4 那一对原样保留。上面那五条实测
就是这条决定的两面：单开 `[::]` 收不收 v4 由 `bindv6only` 决定，所以"只留 v6 口"这种省法不许用；
反过来把 `[::]` 当唯一的 v6 口，等于同时开在这台机器将来会有的每一个地址上——oxidns 的 server
插件**没有客户端 ACL**，`listen` 就是唯一的闸，那道闸只能是"局域网那个地址"。

本轮在同一台 guest 上把新形状跑了一遍（一次性 root `/root/round-p2c/`、端口 15353，用完即删）：
`ss -lntu` 报的是 `192.168.219.128:15353` 与 `[fd15:4ba5:5a2b:1008:20c:29ff:fea1:1609]:15353`
各 udp/tcp 一只，`*:15353` 一只都没有；`nslookup -port=15353 example.com` 打这两个地址都拿到
`172.66.147.243`，打 `127.0.0.1` 是 `connection refused`。**目的地址就是那道闸**，而且绑了显式
v6 地址之后 `bindv6only` 那一栏不再参与这件事——那只 v6 socket 收不到任何 v4，无论 sysctl 怎么写。

"地址会过期"这一支也测了：`lan_address` 填本机没有的地址时，`oxidns check` 照样给
`Configuration is valid` + rc=0（**它不验 `listen` 的地址合法性**），而 `start` 给的是
`Failed to bind UDP socket to 192.0.2.1:15353: I/O error: Address not available (os error 99)`
+ rc=1，插件按逆序拆干净。所以这道闸坏起来是响亮的：可见路径是渲染期那条警告（`svc::topology`）
+ doctor 的 `inbound_not_listening`，不是"面板改了、解析静默少一层"。

## 3. 仍未证的（不许写进代码当前提）

原先列在这里的第 1、2 条（`nodata` 黑洞在 v6 查询上的应答位、v6 设备豁免链在真流量上的命中）
已经在 2026-10-11 那轮 netns 实测里答掉了，结论在 **§8**（劫持层是否真把包收进透明套接字）
和 **§9**（经 v6 监听口的查询应答位 + 豁免链命中）。那两条的答卷都推翻了本文早前的写法，
所以正文改在 §8/§9，这里只留还欠着的：

1. **RP filter / `disable_ipv6`**：v4 侧 `rp_filter=2` 是实测过的；v6 没有对应的 rp_filter 键，
   而 `net.ipv6.conf.*.disable_ipv6` 在这台机器上从没被 ZBox 写过 —— 要不要写成 0 未定，
   至少 doctor 得先把现状读出来再说（#64）。
2. **连接靶子的归属（#69 的形状，本轮只开了个头）**：§10 实测到 dae 的 tproxy 口在它自己的
   `daens` netns 里，宿主 ns 查不到该 socket，于是生成物 `zbox.nft` 里那条**宿主 ns** 的
   `tproxy to :9888` 对 dae 形态**必然不命中**——按 §8 的语义就是不置 mark、走普通路由，
   也就是静默泄漏那一支。**这条不命中怎么收口（要么不生成宿主 ns 的 divert 链、要么让 dae 在
   宿主 ns 也开口）本文没有答案**，它是 #69 的待决项，不许在代码里当已决处理。
   同一问还带着一条没定的产品决定：dae 要 `net.ipv6.conf.<lan>.forwarding=1` 才起得来，
   而这个键现在只在 `dual_stack` 档写（`off` / `dns_only` 档的语义是"v6 侧一个字都不写"）。
   要么为 dae 破例写一条与档位无关的键，要么由 doctor 把"dae 起不来"报出来——
   见 §10 末段。v4 那半条（`send_redirects` 按接口）本轮已按实测补齐。
3. **v6 侧 fakeip**：本文所有 v6 实测都在"DNS 答真地址 / nodata"这条链上，
   fakeip 的 v6 地址池在 `dual_stack` 档下发不发、劫持层认不认，一台机器上都没证过。

## 4. v6 策略路由：与 v4 共用表号与优先级，因为是两套路由数据库

这一节是 `src/gen/netstate.rs::render_apply` 里那三行 `ip -6 …` 的依据（同文件头引的就是这里）。
实验只建对象再删干净，没加载劫持层、没动 sysctl、现役 v6 默认路由全程未受影响。

| 命令 | rc | `show` 出来的样子 |
|---|---|---|
| `ip -6 rule add fwmark 0x1 table 100 priority 100` | 0 | `100:	from all fwmark 0x1 lookup 100` |
| `ip -6 route replace local default dev lo table 100` | 0 | `local default dev lo metric 1024 pref medium` |
| `ip -6 rule show`（干净机器） | 0 | **只有 `0:` 与 `32766:` 两行** |

三条结论：

- `local` 这个路由类型关键字在 `-6` 下同样接受，`default` 可以当"目的地址通配"写，
  所以 v4 那份 `ip -4 route replace local default dev lo table 100` 的形状可以逐字搬到 v6。
- v6 读的是独立的路由数据库，所以**同一个表号 100、同一个优先级 100 不与 v4 相撞**，
  两份共用常量（`DIVERT_TABLE` / `DIVERT_PRIORITY`），不需要给 v6 另开一套号。
- ⚠ **v6 的基线比 v4 少一行**：`ip -4 rule show` 出厂是 `0:` / `32766:` / `32767:` 三行，
  `ip -6 rule show` 只有 `0:` / `32766:` 两行。doctor 判"劫持层在不在"时如果把 v4 那三行
  当成两族共用的基线，就会在 v6 侧永远报一条不存在的缺失（#64 落这条时要抄这里）。

## 5. `table ip6 zbox` 真加载过一次：两族独立、幂等成立、nft 会重写输出

`nft -c -f` 六道（三档 × 两份文件）全 rc=0 且零 stderr，但 `-c` 只到"检查"这一层，
所以又把 `dual_stack` 那份里切出来的 **ip6 半边**（`sed -n '/^table ip6 zbox { }/,$p'`，896 字节 / 29 行）
在这台 guest 上真 `nft -f` 了一次，验完 `nft delete table ip6 zbox` 删干净。
现场前置：`net.ipv4.ip_forward=1`、`net.ipv6.conf.all.forwarding=0`（§1 的臂 Z 末态），
所以那两条 v6 链挂上后**看不到任何流量** —— 这是一次"建对象"的实验，不是一次"看命中"的实验
（命中那一问的答卷在 §8、§9，2026-10-11 才补上）。

| 判据 | 实测 |
|---|---|
| `type nat hook prerouting priority dstnat` 在 ip6 家族 | 接受，rc=0 |
| `type filter hook prerouting priority mangle` 在 ip6 家族 | 接受，rc=0 |
| `redirect to :15353` / `tproxy to :9888` 在 v6 链 | 都接受，`tproxy to` 计数 = 2 |
| `fib daddr type local` 在 v6 链 | 接受，这个键不是 v4 专属 |
| 连下两次同一份 | `tproxy to` 仍是 2、链仍是 2 —— 顶部的 `table{ }` + `flush` 在 ip6 上同样管用 |
| 只加载 ip6 那份后 `nft list table ip zbox` | `rc=1` + `Error: No such file or directory`，两族确实是两张独立的表 |
| `nft delete table ip6 zbox` | rc=0，随后 `nft list table ip6 zbox` rc=1、`nft list tables` 空 |

**输出重写比原先已知的更多，`nft list` 的文本一律不许拿来比对生成物**：

- 十六进制照旧：源里 `meta mark != 0x0` 列成 `0x00000000`、`mark set 0x1` 列成 `0x00000001`。
- **单元素集合会把花括号去掉**：源里 `ip6 saddr { fd15:…::88 } return` 列成
  `ip6 saddr fd15:…::88 return`，`ip6 daddr { ff00::/8 }` 同理；多元素集合（`udp dport { 546, 547 }`）
  保留括号。
- 源里 `meta l4proto udp udp dport { 546, 547 }` 列出来是 `udp dport { 546, 547 }`，
  前半截被判定冗余后去掉 —— 内核认这个写法，只是不照原样吐回来。

#63b 的 `table ip6` 那半边到此不再是纸上形状。靶子与命中在 §8、§9（2026-10-11 实测收口）。
**这一段原先写的形状作废**：它当时把"两个劫持靶子"当作 sing-box 的 `dns-in` / `tproxy-in`，
而 sing-box 在 2026-10-09 的定位重定义里已经退役。这两张口现在分属两个组件——
DNS 靶子是 **oxidns** 的 `udp_in`/`tcp_in`（`ports.dns`，见 §2），连接靶子是 **dae** 的
`tproxy_port`（`src/gen/dae.rs`）。所以"把 `listen` 换成 `::"`这条改法不再对任何现役生成物成立。

## 6. iproute2 怎么打印一个 IPv6 socket：族只能由过滤器决定

这一节是 `src/svc/observe.rs` 那三条 v6 探针（`ss -lntu -6` / `nft list table ip6 zbox` /
`ip -6 rule show`）与 `src/doctor.rs::dns_port_not_listening_on_ipv6` 的依据。
2026-10-09 在同一台 guest 上测，被测的 socket 是一只绑 `::` 的监听（用谁造的都不重要，
下面钉的是 **iproute2 的打印规则**，与被编排的是 oxidns、dae 还是别的组件无关）。

| 探针 | 实测输出 | 结论 |
|---|---|---|
| `ss -lntu`（无族过滤器） | 那只 `::` socket 打成 `*:19889` | 双栈 socket 在合并视图里是 `*:端口`，**地址字面量判不出族** |
| `ss -lntu -6` | 同一只 socket 也打出来（`*:19889`） | `-6` 认它是 v6 监听 → v6 侧的"在听"必须有这条 |
| `ss -lntu -4` | 同一只 socket **一行都不输出** | ⚠ `-4` 会把双栈口读成"没在听" |
| `ss -lntu -6`（sshd，V6ONLY） | `[::]:22` | 显式 V6ONLY 的口打的是 `[::]:端口`，与双栈那只形状不同 |

由此定的两条读法（**不对称是有意的，别"顺手改对称"**）：

- v6 侧新增 `ss -lntu -6`，因为它是唯一能把双栈 socket 认作 v6 监听的过滤器。
- v4 侧**继续用不带过滤器的 `ss -lntu`**。换成 `-4` 会让"oxidns 只开 `[::]`"这种配置在 v4 侧
  被读成"DNS 没在监听"，凭空造一条致命故障。
- 判据里不许出现"地址字面量是 `[::]` 才算 v6"这种写法：同一只 socket 在两张视图里分别是
  `*:19889` 与 `[::]:22` 两种形状，取决于 socket 自己的选项而不是取决于有没有人听。

顺带记三条同场实测的坑：

1. **busybox 的 `nc` 不能当 v6 监听靶子**：`nc -6 -l -p 19535` 静默返回、`ss` 里查不到任何 socket，
   也不报错。要造 v6 靶子得有真的监听者（#65 的 netns 轮直接用组件本身）。
2. **`net.ipv6.bindv6only = 0`**（这台 guest 的出厂值）—— §2 那条"双栈默认"的环境事实来源。
3. `ip -6 rule show` 的出厂基线只有 `0:` / `32766:` 两行（§4 已记），所以 v6 的 divert 行
   `100: from all fwmark 0x1 lookup 100` 与 v4 逐字同形，`has_divert_rule()` 一个解析器两族共用。

**doctor 在 v6 监听这一层只报一条**（`dns_port_not_listening_on_ipv6`，两档 `takes_dns()` 都触发）：
它的依据只有 §2（oxidns 在 `takes_dns()` 时生成 `[::]:ports.dns`）这一条。
**dae 的 `tproxy_port` 在 v6 上绑不绑，2026-10-11 实测过（§10）：绑，但口在它自己的 `daens` netns 里**
——所以它到现在仍然不许进 `dns_port_not_listening_on_ipv6` 那类宿主 ns 探针的判据，
只是理由换了：不是"没证过"，而是"**证过的形状决定了宿主 ns 查不到它**"（§10）。

## 7. `ipv6_forwarding_off`：双栈档下"接管静默没发生"的形状，此前一个字都不报

这台 guest 出厂（§1 的臂 Z 末态）`net.ipv6.conf.all.forwarding = 0`、`accept_ra = 1`。
#63 把双栈档的 sysctl 接进生成物之后，`doctor` 里只有一条 `ipv6_forwarding_drops_ra`
（转发开着 + `accept_ra=1` 才报），于是**反过来**那个现场——用户把 IPv6 切成双栈接管、
生成物里写着要转发，机器上却没转——诊断面板是**安静**的。这正是"接管静默没发生"：
客户端的 v6 流量既不本机直连也不中继，routed 那半条路线整个不存在，而界面没有任何一条
说得出原因的故障。2026-10-09 这轮真机验收（一次性 root + 414 版二进制 + `GET /api/doctor`）实测到：

| 档位 | `/api/doctor` 里 v6 侧报出的 code |
|---|---|
| `dns_only` | `dns_port_not_listening_on_ipv6` 一条 |
| `dual_stack` | 上面那条 + `ipv6_nft_table_missing`、`ipv6_divert_rule_missing`、**`ipv6_forwarding_off`** |
| `off` | 一条都不报（§1 臂 Z 的现值 0/1 逐字照读，但这一档不接管连接，转发是 0 才对） |

补的判据是 `v6_connection_expected`（= `takes_connection()`，只有双栈档）门控下的
`ipv6_forwarding_off`，`expected` 与 fix 都**成对给两条键**（`all.forwarding=1` 与
`all.accept_ra=2`），因为 §1 的三臂实测过：只补转发就落进臂 A 那个静默断网。
读不到（`v6_forward_all` 是 `None`，Windows 开发机就是这形状）不报，
`off` / `dns_only` 也不报——这条判据的成立面只有"这一档要中继 v6 流量"。

同轮加的守卫在 `src/gen/netstate.rs::v6_host_pairs()`：doctor 的 fix 与生成物末尾那两条
键由同一个来源出，测试钉死"恰为两条、是 `host_sysctl_pairs(DualStack)` 的末尾两条、
且不出现在 `Off` 那份里"。以后谁改生成物的键名而忘了改诊断，测试会先响。

## 8. `tproxy` 命中与否的语句级形态：**divert 链本身不构成防泄漏保证**

2026-10-11 在 netns 里造的真 v6 局域网（拓扑见 §9）上测的，用的是**探针表**，不是生成物：
每条链把生成物里那一行

```
meta l4proto tcp tproxy to :9888 meta mark set 0x1 accept
```

展开成四条同 dport 的规则，以便分别计数——到达 / tproxy 语句本身 / 置位后再看 mark / 走过 tproxy 之后：

```
iifname != "eth0" return
meta l4proto tcp tcp dport N counter comment "REACH"
meta l4proto tcp tcp dport N tproxy to :9888 counter meta mark set 0x1 accept
meta l4proto tcp tcp dport N meta mark 0x1 counter comment "MARKED"
meta l4proto tcp tcp dport N counter comment "PAST"
```

每一臂用**独立的 dport**（9101…9106，一臂一格）而不是重载归零：`pt.nft` 只在开头 `nft -f` 一次，
六趟查询各打自己的端口号，所以谁计数多少不需要清零也读得准。生产生成物那一行不带计数器，
计数器是为读语义临时加的。
夹具是 sing-box 的 tproxy 入站，**只用来提供 `IP_TRANSPARENT` 套接字，不是被测件**
（sing-box 在 2026-10-09 的重定义里已退役，本轮没动它的任何生成物）。

| 臂 | 透明套接字 | 策略路由 | REACH | tproxy 行内 counter | MARKED | PAST | 客户端/夹具侧 |
|---|---|---|---|---|---|---|---|
| MISS_TCP（dport 9101） | 无 | 无 | `1pkt 80B` | `0pkt 0B` | `0pkt 0B` | **`1pkt 80B`** | 包穿过网关到达 lan2，dig 打向那个真目的 |
| MISS_UDP（9104） | 无 | 无 | `1pkt 102B` | `0pkt 0B` | `0` | **`1pkt 102B`** | 同形 |
| HIT_NOROUTE_TCP（9102） | `[::]:9888` | 无 | `3pkt 240B` | **`3pkt 240B`** | `0` | **`0`** | dig 超时，夹具**零**连接日志 |
| HIT_ROUTE_TCP（9103） | `[::]:9888` | 有 | `6pkt 560B` | `6pkt 560B` | `0` | `0` | 夹具日志 `open connection to [fd65:7:1:7::2]:9103 … connection refused` |
| HIT_ROUTE_UDP（9105） | `[::]:9888` | 有 | `1pkt 102B` | `1pkt 102B` | `0` | `0` | 夹具日志 `inbound packet connection from [fd65:7:1:1::fd]:50812` → `… to [fd65:7:1:7::2]:9105` |
| HIT_ROUTE_V4_TCP（9106，`table ip`） | `[::]:9888` | 有 | 未读到 | 未读到 | 未读到 | 未读到 | 夹具日志 `inbound connection from 192.0.2.1:42373 to 192.0.7.2:9106 using outbound/direct[dir]` |

⚠ 最后一行那四个"未读到"是我的脚本缺陷：`nft list chain ip4 pt c6` 报
`syntax error, unexpected string`（v4 的族名是 `ip`，不是 `ip4`），所以 v4 侧只有夹具日志那条
正面证据，语句级计数器**没测**。v4 与 v6 在这一层是否逐字同形，仍是**外推不是实测**。

### 三条定案

1. **未命中 = 本条规则后面的语句不执行，但链继续走下一条**（MISS：行内 counter 0，PAST 1pkt）。
   于是生成物那句 `tproxy … meta mark set 0x1 accept` 在上游没起时**不置 mark、不 accept**，
   包按普通路由转发出去 —— 这是**静默泄漏**，不是黑洞。"上游挂了会掉进 `local default dev lo`
   那个安全黑洞"这个假设**作废**：黑洞只在包真被 tproxy 收下时才成立。
2. **命中 = 行内 counter 计数，且 `accept` 终止本链**（HIT_NOROUTE：counter 与 REACH 同为 3pkt，
   PAST 为 0）。所以 `MARKED` 那一格在两臂都是 0 不是矛盾：命中时被 `accept` 拦在链里，
   未命中时 mark 根本没置。别把"MARKED=0"读成"mark 机制坏了"。
3. **命中还不够，握手要策略路由那半边**：HIT_NOROUTE 三趟包全被收进透明套接字，
   客户端仍超时、夹具一条连接日志都不出；补上 §4 那两行（`ip -6 rule add fwmark 0x1 table 100
   priority 100` + `ip -6 route replace local default dev lo table 100`）之后才出现
   `inbound packet connection …` 这类正面证据。**劫持层是 nft 链 + 策略路由两半，缺一半的现场
   长得和"完全没接管"一样。**

### 被推翻的旧解释（写在这里防止再犯）

上一轮（2026-10-11 同日上午，t-transit3）我据"tproxy 之后的 counter 读 0"下过结论
"tproxy 命中即终止本条求值"。**那是错的**，错因是拓扑坏了一半：目的地址 `fd65:7:1:7::2`
当时配在**同一个 netns** 的 dummy 上，于是它在网关自己看来就是 `daddr type local`，
包停在 `fib daddr type local return` 那条上，**根本没走到 tproxy 语句**。
"远端网段"必须是**另一个 netns**，否则过境用例测的是 local 分支。
教训：counter 读 0 有两种成因（语句没执行 / 包没到这条），必须先看它前面那条 `return` 的计数。

### 顺带钉住的 nft 语法（本文其它节都要用）

- `counter comment "X"` 只能放在规则**末尾**；放中段报 `syntax error, unexpected tproxy,
  expecting newline or semicolon`。
- 纯 `counter`（不带 comment）可以放在 `return` / `redirect to` / `accept` **之前**，管用。
- `counter name X`（命名计数器）这个版本（nft 对应内核 6.12.111-0-lts）不接受。
- `nft insert rule … position <handle>` 不支持；要链首计数就 `nft insert rule`（插到最前）。
- 生成物里那行 tproxy 不许直接改成"展开成四条"的形状来凑计数器——上面这份是**探针表**，
  生产 `zbox.nft` 仍是合写的一行；两者等价性由本节计数器证明，不要反向把探针当生成物。

## 9. 经 v6 监听口的查询应答位，与豁免链在真流量上的命中

拓扑（2026-10-11，全在 netns 里，`eth0` 一个字节没动过）：

```
宿主 ns v6h(fd65:7:1:1::fd / fd65:7:1:3::fd / 192.0.2.1)
        ↕ veth
netns v6srv eth0(fd65:7:1:1::2 / 192.0.2.2)   ← 被测的"网关"，oxidns/夹具跑在这里
        ↕ veth t2h ↔ t2c
netns lan2  t2c(fd65:7:1:7::2 / 192.0.7.2)     ← 真 v6 客户端
```

宿主侧两条明细路由把"远端网段"的下一跳指进 `v6srv`（`ip -6 route add fd65:7:1:7::/64 via
fd65:7:1:1::2 dev v6h`、v4 同理），这样从客户端发往 `fd65:7:1:7::2` 以外的地址才会**过境**网关。
oxidns 用一次性 `--root` + 一次性端口 15353/15354 跑在 `v6srv` 里，`[::]:15353` 与
`0.0.0.0:15353` 两份监听都由它自己生成（§2 的形状）；nat 链把 53 的查询重定向到它。
下面那些探针的**源地址**是宿主侧 veth 上的 v6/v4 地址（`-b` 指定），也就是"接在同一张局域网上的
主机"；`@…::2 -p 53` 打到的是网关自己，走 `redirect to :15353` 那一条，
而 §8 的过境探针打的是 `lan2` 的地址，走 `tproxy` 那一条。两类都算"从局域网进来的真包"，
但停在不同的规则上，别混着读。

### 应答位（§3 原第 1 条的答卷）

| 探针 | 停在哪条规则 | dig 结果 |
|---|---|---|
| `hij6_aaa_LAN`（`-6 -b fd65:7:1:1::fd @fd65:7:1:1::2 -p 53 … AAAA`） | nat 链 `redirect to :15353`，落 v6 监听口 | `status: NOERROR, ANSWER: 0, AUTHORITY: 1` |
| `hij6_aaa_LAN_tcp`（`+tcp` 同一条） | 同上，TCP 那一侧 | `NOERROR, ANSWER: 0, AUTHORITY: 1` |
| `hij6_a_LAN`（同链问 A） | 同上 | `NOERROR, ANSWER: 3` |
| `hij4_aaa_LAN` / `hij4_a_LAN` | 同链，v4 源 | `ANSWER: 0, AUTHORITY: 1` / `ANSWER: 3` —— **与 v6 逐字同形** |
| `hij6_aaa_FOREIGN`（`-b fd65:7:1:3::fd`，不在局域网段） | 被 `ip6 saddr != <lan>` 豁免，没进重定向 | `rc=9` `communications error to fd65:7:1:1::2#53` |
| `hij4_aaa_FOREIGN`（`-b 198.51.100.99`） | 同上，v4 那份 | `rc=9` 同形 |

结论：**`black_hole(nodata)` 那条抑制在经 v6 监听口进来的查询上照常生效**，AAAA 得
NOERROR + 0 answer + 1 authority，同一链上的 A 照常答 3 条，v4/v6 共用一条 `entry: main`
（不是"v6 走了另一条没配抑制的链"）。非局域网源的查询**不进劫持层**，超时就是它自己往
上游发的结果，与黑洞无关 —— 这正是豁免该有的样子。

### 豁免链的命中（§3 原第 2 条的答卷）

生成物 `dual_stack` 那份逐条加上 counter 之后（`t-mkcountered2.sh`，两份 `nft -c -f` 都 rc=0），
在真流量上读到的计数：

| 规则 | 局域网源过境（T7_LAN_TCP） | 非局域网源（T7_FOREIGN） |
|---|---|---|
| `iifname != "eth0"` | `2pkt 120B`（**回程 RST** 被豁免，不是入站） | — |
| `ip6 saddr != fd65:7:1:1::/64 return` | 0 | **`8pkt 608B`** |
| `fib daddr type local return` | 停在打本机地址那一趟（T6_LOCAL） | — |
| `REACH_*_TPROXY` | 有计数（包确实走到了 tproxy 语句） | `0` |

决定者就是 `ip6 saddr != <lan_cidr6>` 这一条，`[::]` 双栈监听口进来的 v6 包与 v4 包在同一条
链上得到同样的处置。**§5 当时"能建、能列出"的疑问到此关闭。**

### netns 的坑（重建拓扑时会再遇到，别再猜一遍）

- veth 跨 ns 移动 + `ip addr flush` 会把 **link-local 地址**清掉且内核不自动补 →
  NDP 永远 `INCOMPLETE`、全局地址 `tentative`、看起来像 `disable_ipv6` 在作怪
  （实测三条 `disable_ipv6` 都是 0，那个假设作废）。修法是显式补：
  `ip -6 addr replace fe80::1/64 dev t2h`（对端 `fe80::2` 同理）。
- 发过境包前先 `ping6` 一次网关把邻居条目焐热，否则第一个 SYN 卡在 NUD 上，会被读成"包没进链"。
- busybox `nc` 不能当 v6 监听靶子（§6 已记），靶子一律用组件本身。

## 10. dae 实测：内核参数门是**逐个 FATAL 报出**的，tproxy 口在它自己的 `daens` 里

2026-10-11 在同一台 guest 上，复用常驻的 `/root/dae-probe`（dae v2.1.1），只绑 dummy 网卡
`dae-probe`（`192.0.2.10` / `fd65:7:1:5::1`），**绝不碰 eth0**，`cgroup2` 与 `bpffs` 一律临时挂载。
这一节补 `docs/reference/dae-d1-guest-measurement.md` 那条门表，并把 #69 的形状问题摆实。

### 两条硬前提与档位无关

| 现场 | dae 的自述 |
|---|---|
| `net.ipv6.conf.dae-probe.forwarding` 不为 1 | FATAL `ipforward on dae-probe is off` |
| `net.ipv4.conf.dae-probe.send_redirects` 不为 0 | FATAL `send_directs ... is on` |

门是**逐个**报的：修掉第一条才看见第二条。过了之后 `Bind to LAN` + `Bind to WAN` +
`Control plane built in 916ms`。写 sysctl 时按 §1 成对写 `accept_ra=2`，且**只写 per-iface 键**
（本轮没写 `all.forwarding`，故意避开 §1 那个整机开关的后果）。
含义：这两条 per-iface 前提不是 `dual_stack` 档独有的，**`off` / `dns_only` 档下 dae 照样要**——
下一小节就是"我们的生成物到底写了哪些键"这条的核法与结果。

### 这一条已经核完，并且当场改掉了一半（2026-10-11）

现役 guest（ZBox 服务在跑、劫持层未加载）上读到的现值：

| 键 | 实测 |
|---|---|
| `net.ipv4.conf.all.send_redirects` | **0**（ZBox 的 `/etc/sysctl.d/99-zbox.conf` 写的） |
| `net.ipv4.conf.eth0.send_redirects` | **1** |
| `net.ipv6.conf.all.forwarding` / `eth0.forwarding` | 0 / 0（这一档不该转发，对） |

也就是说 **`all` 那份不覆盖按接口的读值**，而 dae 按接口读——上面那对 0/1 就是当场证据。
所以 `gen::netstate::sysctl_pairs` 现在多写一条 `net.ipv4.conf.<局域网网卡>.send_redirects=0`，
**三档都写**（与 `ipv6_mode` 无关），持久化的 `/etc/sysctl.d/` 那份仍只收整机级键
（绑网卡的不进那个文件：Alpine 的 `sysctl -p` 对认不出的键名只打一行 stderr 就 rc=0，
等于开机静默少一条，理由见 `netstate.rs` 里 `sysctl_pairs` 的注释）。
断言在 `the_send_redirects_gate_is_closed_on_the_interface_not_only_on_all`。

**没改的那一半**：`net.ipv6.conf.<lan>.forwarding=1` 只在 `dual_stack` 档写。
dae 在 `off` / `dns_only` 档同样要它，但那两档的产品语义是"v6 侧一个字都不写"——
要么让 ZBox 为 dae 破例开一条与档位无关的 v6 转发，要么由 doctor 报"dae 起不来是因为这条没开"。
这是**产品决定，不是实测问题**，挂在 §3 第 2 条（#69）下面。

（`net.ipv6.conf.<if>.send_redirects` 在这台 Alpine linux-lts 上根本不存在，
`dae-d1-guest-measurement.md` 已记；本轮复现到的只有 v4 那一份。）

### 监听族（§6 那句"从没实测过"的答卷）

dae 自建 netns **`daens`**，tproxy 口 `12345` 在**那个 ns 里面**：

| 探针 | 实测 |
|---|---|
| `ip netns exec daens ss -lntu`（无过滤器） | `tcp LISTEN 0.0.0.0:12345` **和** `tcp LISTEN [::]:12345`、`udp UNCONN *:12345` |
| `ip netns exec daens ss -lntu -4` / `-6` | v4、v6 两条 LISTEN 各自被自己的过滤器认到（§6 的读法在 dae 身上同样成立） |
| 宿主 ns `ss -lntu \| grep -c 12345` | **0** |

三条结论：

1. **dae 的 v6 是绑的**，而且是 `0.0.0.0` + `[::]` **两口并开**（UDP 那只本身双栈）——
   与 §2 给 oxidns 定的"追加而非替换"形状一致，所以 #69 里"给 dae 补一个 v6 口"这个需求
   **不存在**，dae 自己就开了。
2. 但它在 `daens` 里，所以生成物 `zbox.nft` 那条**宿主 ns** 的 `tproxy to :<dae 的 tproxy_port>`
   对 dae 形态**必然不命中**。按 §8 第 1 条，不命中的后果是静默泄漏，不是黑洞。
3. 于是 #69 的真实形状不是"补 v6 监听"，而是**divert 链与 dae 的接管机制到底谁负责哪一半**
   （dae 用 eBPF 自己引流，我们那条 nft tproxy 是否还要生成、要不要退回"只做 DNS 劫持"）。
   本文不给答案，见 §3 第 2 条。

收尾（`v6-down.sh`，逐项复查而非看脚本 rc）：`ip netns list` 空、veth/dummy 全无、
宿主 `nft list tables` 空（外来的 `zboxtest` 表先证实存活再按名删除）、
`ip -4 rule` 三行 / `ip -6 rule` 两行、sysctl 回到 `1 0 0 0 1 1 0`、eth0 的 SLAAC 地址在、
现役监听 53/9887/9888/8888/7890/22 与 `[::]:22` 一字未动、15353/15354 无人监听、
`/var/lib/zbox` mtime 仍是 `2026-10-01 00:45:59.024918647 +0800`、`rc-service zbox status: started`。

