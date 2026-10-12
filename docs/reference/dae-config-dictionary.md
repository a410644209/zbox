# dae 配置全典（v2.1.1）

优先级：`example.dae` > `dae export outline`（默认值/必填/类型的唯一权威）> 上游 docs > `dae-d1-guest-measurement.md`。社区 emoeem 三份只进 §6。

## 0. 接管机制

tc eBPF 挂在绑定网卡做 L3/L4 判决，结果（outbound index + rule mark）写进 `skb->mark`：命中 `direct` 的流量由内核路由直转、不进用户态；要代理的连接导向 tproxy，Go 用户态按 group `policy` 选节点拨出；cgroupv2 钩子取进程名（pname）与 SO_MARK。tproxy_port 不是 HTTP/SOCKS 端口，只是 eBPF 定位用户态 socket 的锚点。域名分流靠截获 53 端口的 DNS 回答建立"IP→域"归属，DNS 不被接管则 domain() 失效。**没有任何 HTTP/gRPC 查询 API**：control plane 是进程内对象，其 listener 只在 dae 私有 netns 内绑 tproxy_port，宿主不可达——面板要的连接/分流情况只能取日志。

## 1. 配置落点

`/usr/bin/dae`、`/etc/dae/example.dae`、`/usr/share/dae/geoip.dat`+`geosite.dat`(23.1MiB)。unit：`Type=notify`、`ExecStartPre=dae validate -c /etc/dae/config.dae`、`ExecStart=dae run --disable-timestamp -c /etc/dae/config.dae`、`ExecReload=dae reload $MAINPID`、`Restart=on-abnormal`。

- 入口约定 /etc/dae/config.dae，权限必须 0600（或 0640+组）：run 对 0644 FATAL，validate 只告警返回 0。
- include {} 相对路径以入口文件所在目录为基准（非 cwd），被包含文件必须在入口配置目录下，支持 glob。
- /var/run/dae.pid（--disable-pidfile 关、正常退出删）；/var/run/dae.progress 是 reload 结果的带外通道。
- 日志 --logfile（空=stdout/stderr）、--logfile-maxsize 默认 30MB、--logfile-maxbackups 默认 3、--disable-timestamp。
- tag 以 -file 结尾的 subscription 落 config_dir/persist.d/<tag>.sub 作兜底；geo 只从本地解析，validate 永不下载；启动首句 purging stale TC filters（是否只清自己创建的：未核实）。

## 2. 语法形状与块职责

块 + key: value + # 注释；特殊字符值加单引号；逗号分隔串读成数组。v2.1.1 outline 只有 6 个顶层块：global（接管方式，required）、subscription（节点来源，并入全局节点池）、node（手写节点，支持 'tuic://.. -> vmess://..' 链）、group（出站，块名即出站名）、dns（解析 + DNS 分流）、routing（规则数组 + fallback，required）。**external / program 块在 v2.1.1 不存在**——outline 未列、二进制无 mapstructure:"external"|"program"（routing_program.go 是内部实现），生成器不要产出。

## 3. 参数表

### 3.1 global（config.Global，required=true；默认列取 outline defaultValue）

| 字段 | 类型 | 默认 | 含义 |
| --- | --- | --- | --- |
| tproxy_port | uint16 | 12345 | eBPF 的 tproxy 锚点 |
| tproxy_port_protect | bool | true | 防非请求流量 |
| so_mark_from_dae | uint32 | 无默认 | dae 自发流量 socket mark；省略=内部 mark(0x100)+WARN |
| so_mark_from_dae_set | bool | 无默认 | 派生位，非用户字段 |
| log_level | string | info | error/warn/info/debug/trace |
| tcp_check_url | string[] | http://cp.cloudflare.com,+1.1.1.1,+2606:4700:4700::1111 | TCP 探测目标 |
| tcp_check_http_method | string | HEAD | 探测方法 |
| udp_check_dns | string[] | dns.google:53,+8.8.8.8,+v6 | UDP/DNS 探测目标 |
| check_interval | Duration | 30s | 探测间隔 |
| check_tolerance | Duration | 0 | 切节点门槛；example 写 50ms |
| lan_interface | string[] | 无默认 | 代理 LAN 时绑 |
| wan_interface | string[] | 无默认 | 代理本机；auto 自探测 |
| allow_insecure | bool | false | 容忍不安全 TLS |
| dial_mode | string | domain | ip/domain/domain+/domain++；ip 关嗅探 |
| disable_waiting_network | bool | false | 不等网络即拉订阅 |
| disable_thp | bool | false | 进程退出 THP |
| enable_local_tcp_fast_redirect | bool | false | outline 无 desc |
| auto_config_kernel_parameter | bool | false | false 时 ZBox 自补 per-if |
| auto_config_firewall_rule | bool | false | outline 无 desc |
| sniffing_timeout | Duration | 30ms | 嗅探等首包；ip 恒 0 |
| auto_sniff_punt | bool | true | 设备级白名单嗅探 punt |
| tls_implementation | string | tls | tls / utls |
| utls_imitate | string | chrome_auto | uTLS Hello ID |
| tls_fragment | bool | false | Hello 分片绕 SNI |
| tls_fragment_length | string | 50-100 | 分片字节区间 |
| tls_fragment_interval | string | 10-20 | 分片毫秒区间 |
| pprof_port | uint16 | 0 | 非 0 开 pprof |
| mptcp | bool | false | 节点支持时多路径 |
| bootstrap_resolver | string | 无默认 | 未设按序 119.29.29.29:53、223.5.5.5:53 |
| fallback_resolver | string | 8.8.8.8:53 | resolv.conf 兜底 |
| bandwidth_max_tx | string | 0 | 上行带宽（Hysteria2） |
| bandwidth_max_rx | string | 0 | 下行带宽 |
| udphop_interval | Duration | 30s | outline/example 均无说明 |
| bpf_conn_state_map_size | uint32 | 262144 | 连接状态 map 容量；仅新加载/重启生效 |

global 内没有任何字段带 required 标记（全部可选，缺省走 defaultValue）。outline 拿不到默认值：so_mark_from_dae、so_mark_from_dae_set、lan_interface、wan_interface、bootstrap_resolver。outline 无 desc、含义只能回 example 注释：auto_sniff_punt、tls_fragment*、pprof_port、bandwidth_max_*、udphop_interval、fallback_resolver、enable_local_tcp_fast_redirect、auto_config_firewall_rule。

### 3.2 dns

ipversion_prefer:int(无默认，偏好 A/AAAA) · fixed_domain_ttl:{domain:ttl}(0=不缓存) · upstream:{tag:'scheme://host:port'}(scheme tcp/udp/tcp+udp/h3/http3/quic/https/tls，https/h3 可带 path) · bind:string(tcp://127.0.0.1:5353 等) · optimistic_cache:true · optimistic_cache_ttl:60(秒，0=不过期) · optimistic_stale_reply_ttl:30 · max_cache_size:65536。routing.request / routing.response 各为规则数组 + fallback 必填（outline 中仅有的两处 required fallback）。

### 3.3 group（数组，块名即出站名）

policy:string **required** = random / fixed(n) / min / min_avg10 / min_moving_avg（fixed 关闭连通性检查）。filter:[]Function，多条之间 OR；name(key keyword/regex，无 key=全等)、subtag(key regex)、! 取反；尾随 [add_latency: -500ms] 加偏置。tcp_check_url / tcp_check_http_method / udp_check_dns / check_interval / check_tolerance 可选覆盖 global，outline 不给默认（继承）。

### 3.4 routing / subscription / node

routing required；fallback **默认 direct**——不写就是全直连，生成器必须显式写。subscription / node 为 KeyableString[]（tag:'link' 或裸 'link'）。协议面见 proxy-protocols.md（socks5/http/https/ss/ssr/vmess/vless/trojan/tuic/juicity/hysteria2…）。

## 4. routing 规则 DSL 清单

这份清单**不在 outline 的结构化字段里**：outline 只有 mapping/type/defaultValue/required，选择器名仅以散文出现在 routing / dns 节点的 desc 字符串中，程序化取不到。来源：example.dae + outline desc + 上游 docs + 二进制内嵌 BPF C。

- 主 routing{}（outline 列 9 个）：domain(key suffix/keyword/regex/full，无 key=suffix)、sip、dip(CIDR)、sport、dport(区间 8000-9000)、ipversion(4/6)、l4proto(tcp/udp)、pname(仅 WAN、本机进程)、mac(仅 LAN)。geo 入口 domain(geosite:cn)、dip(geoip:cn)、geoip:private。
- dscp()：BPF 有 tuples->dscp = ipv4_get_dscp(iph)，是真选择器但不在任何官方函数列表——用前实测。社区的 protocol()/has_domain()/tcp_conn()/udp_conn() 未获确认。
- dns.routing.request：qname、qtype + 内部选择器 sub/node/subnode（subnode 先于 node；内部选择器只能指向 dns.upstream 的名字、不能用 fallback、不能与 qname/qtype 混写），内置 asis、reject；response：qname、qtype、ip、upstream，内置 accept、reject。
- 组合：&&、!、()、->，块级 fallback:；可附 mark 如 direct(mark: 0x800)（LAN 直连带该 skb->mark；本机流量只有 mark=0 的 direct 才绕开用户态）。
- 内置出站 direct / must_direct / block + group 名；自上而下第一个命中即停。direct 仍把 53 交给 DNS 模块，只有 must_direct 让 53 彻底绕开 dae，代价是学不到"IP→域"、domain() 对该流量失效。

## 5. CLI 与编排契约

| 命令 | 作用 | 备注 |
| --- | --- | --- |
| run -c FILE | 清遗留 tc filter → 挂 cgroup/bpffs → 载 BPF(~120MB) → 规则进内核 | -c 必填；权限过宽 FATAL rc=1 |
| validate -c FILE | 与 run 同管线（别名、geo 展开、规则 lowering），不碰 BPF、无网络 I/O | ~0.08s(3 geo)/~0.5s(28 geo)；不校验权限 |
| reload [pid] | 分阶段热重载，保留连接并刷新订阅；也是 suspend 的恢复手段 | prepare/ready 各 45s、切换预算 10s；成败写 dae.progress |
| suspend [pid] | "no-load state" | SIGUSR2；恢复用 reload(SIGUSR1) |
| sysdump / trace | 导出现场 / 逐包跟踪一条流 | trace 未实测 |
| export outline | 机器可读模式 {version,leaves[],structure[]} | 本文 §3 来源 |
| honk | 本地材料无法确认用途 | 面板不要依赖 |
| -v | 版本；dae version 不存在 | |

信号 SIGINT/SIGTERM/SIGHUP/SIGQUIT/SIGILL/SIGUSR1/SIGUSR2；TERM 快退出（摘自有钩子，连接状态留待下次启动清）。ZBox 三道门：① chmod 0600 并把权限检查当前置；② validate 拿 rc=0；③ 真起 run（权限/BPF/内核参数）；reload 读日志 + dae.progress。

**第 ② 道的覆盖面（2026-10-09 实测，见 §6）**：它管块语法、别名展开、geo 展开、规则 lowering 与"出站引用了谁"，**不管 `node{}` 里那条链接的内容**。所以 validate rc=0 不等于 54 个节点都能连。

## 6. 被推翻 / 存疑

**2026-10-09 P2b 实测（Alpine guest，dae v2.1.1，`dae validate -c` 逐个改动跑，每条都先用 `cmp -l` 证明确实改到了字节）**：

- **validate 不校验 `node{}` 里的链接字符串**。在一份 17,516 字节、54 节点的真订阅生成物上：塞入未知键 `&zzz=1`、把 `type=ws` 改成 `type=qux`、把 `security=reality` 改成 `security=qux`、删掉整个 `&host=…` —— 四种改动**全部 rc=0 且零输出**。反过来 `fallback: nosuchgroup` 报 `rc=1 invalid routing rules: outbound (group) "nosuchgroup" not found; please define it in section "group"`，`filter: name(nodeZZZ)` 指向不存在的节点却是 rc=0（空组不报错）。**结论**：validate 覆盖"语法 + 别名 + geo + 规则 lowering + 出站引用"，节点链接要到 `dae run` 真拨号才见分晓。BLUEPRINT §10 的 P2 判据"两份文本各自过 validate 门"因此**只是必要条件**，别当验收。
- **`geoip.dat` / `geosite.dat` 必须在位**，否则 validate 直接 `geoip.dat: file does not exist in [...]` rc=1——这不是配置形状问题，是数据文件前置。查找序 = **配置文件同目录** → `/root/.local/share/dae` → `/usr/local/share/dae` → `/usr/share/dae`。官方包把这两份放在 `/usr/share/dae/`（geoip 23,330,258 B + geosite 2,288,716 B），所以 **provision 落地 dae 时必须连这两份一起放**，否则面板上任何带 `geoip:`/`geosite:` 的生成物都过不了自己的门。
- **组不能嵌套**（G12 之外新的一条）：`filter` 只从节点池里挑，把另一个组 tag 填进成员位不会报错，但选不出节点。产品面板允许这么填，所以 `gen::dae` 对"成员恰好只有一个组"的 selector 做**可见的别名改写**（文本里留 `#   proxy -> auto`，`fallback` 与所有 `-> 组` 都落到终点，别名那一格不再声明），成环报错；**多组混合或组与节点混填仍然报错**，因为 dae 里把两池拼成一池换了策略，那是悄悄改语义。
- **块名（组 tag / 节点名）的标识符形状 = G12，今天定案**：首字符必须是 ASCII 字母或 `_`，其后可以是 ASCII 字母、数字与 `_ - .`。逐个 `dae validate -c` 量过（同一份 17,516 B 生成物，只改那一行，每条先 `cmp -l` 证明改到了字节）：`g1`、`hk-1_a.b`、`hk_1`、`_x` 过门；`123`、`9hk`、`.hk` 解析就失败；**中文无论裸写还是加引号都是硬解析错误**（`安全` 报 `bad key name`，`"安全"` 也报）。连带两条：`fallback: g1` 这类引用改坏时 validate 会报"outbound (group) not found"，所以**引用是被检查的**；而把中文节点名退化成 ASCII 别名之后，同一份文件 rc=0。产品结论：面板不许把中文原样写进 tag —— `gen::dae::is_identifier` 按上面这条判，报错文案带 G12；面板"按关键词建地区组"那一格把中文关键词退化成 `region1/region2…`，组名一格另用同一条正则当场提示。
- **上游模板 `example.dae:380` 的 `224.0.0.0/3` 是笔误**：那一段覆盖 `0.0.0.0–223.255.255.255`，把几乎整个公网划进"别接管"。ZBox 卫生层写 `224.0.0.0/4, 'ff00::/8'`（真 multicast），并有单测钉住不许抄回 `/3`。
- 顺带证实：`type=ws&path=&host=` 与 `security=reality&pbk=&sid=&fp=` 原样写进链接**不影响 validate**（见上，它根本没看）；G11 里"ws/reality 在 dae 链接里没有对应键"这条**既没被证实也没被推翻**，要等 `dae run` + `trace`（task #43 的 D2）。目前仍然报错的是 tuic、grpc、以及带自定义握手头的 ws（`headers` 非空）。

被推翻（emoeem 社区文档）：① 内置出站写"block, direct, must_rules"——官方是 direct/must_direct/block，must_rules 字符串在二进制存在但无官方清单与语义；② dscp(0x4) 当常规写法——只有 BPF 字段证据；③ 订阅格式 SIP008/txt/base64/urlencode、不兼容 Clash/Sing-box——无官方支撑；④ check_tolerance 默认 outline 0 vs example 50ms，以 outline 为准。⑤ **新**：`domain()` 的键式是 `keyword:` / `regex:` / `suffix=` / 裸全匹配，**没有 `key=` 这个参数**（社区文档写 `domain(key,geosite:cn)` 那种形状在 v2.1.1 不存在）——连带意味着 dae 表达不出"通配符 `*.example.com` 命中子域但不命中根域"（`suffix=example.com` 会把根域一起吃掉），这条精确语义只能由 oxidns 承担。

存疑/未实测：auto_config_kernel_parameter outline 默认 false 而 example 显式 true（ZBox 按 false + 自补 per-if：ipv4/ipv6.conf.<if>.forwarding=1、ipv4.conf.<if>.send_redirects=0；ipv6.conf.<if>.send_redirects 在 Alpine linux-lts 上不存在）；external{}/program{} 属更高版本或 fork；trace/honk 退出码与 sysdump 格式未固化；cgroupv2 能否真取 pname、tproxy 命中与回滚、dae0 veth redirect、reload 保连接后果、suspend 实际状态、与 sing-box 共存、清 TC filter 范围；mark：socket 0x100（实测）、skb 0x8000000 与策略表 2023（文档值）；netkit 在本 guest 失败退 veth 兼容模式，生成与排障按 veth 算。
