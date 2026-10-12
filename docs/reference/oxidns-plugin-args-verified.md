# oxidns 插件 args —— 源码核实表

权威来源：`.tmp/oxrepo/oxidns-main/`（Rust 源码）。版本对齐 v1.6.1 主干。所有路径相对 `oxidns-main/`，格式 `path:line`。仅列 ZBox 会渲染的插件。本文件是渲染器 `src/gen/oxidns.rs` 的输入，非教程。

## 0. 注册机制与顶层 YAML

- 注册入口：`inventory::submit!{ FactoryRegistration { plugin_type, .. } }`，经 `#[plugin_factory("type")]` 属性宏（`crates/macros/src/lib.rs:20`，宏体 `:38-50`）或 `register_plugin_factory!`（`src/plugin/mod.rs:176-189`）。`plugins[].type` 的值 = 宏参数字面量。
- `src/plugin/mod.rs:191-205` 依 `module_path` 判定类别（`::plugin::server::` / `executor` / `matcher` / `provider`）。
- 顶层 `Config`（`src/config/types.rs:79`）：

| YAML key | Rust 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `include` | Vec<String> | 否 | [] | types.rs:82；递归合并于 config/mod.rs:100-114，仅从文件加载时允许 |
| `runtime` | RuntimeConfig | 否 | 默认 | types.rs:86；子字段 `worker_threads: Option<usize>` (:755)，=0 报错 (:112) |
| `api` | ApiConfig | 否 | 无 http | types.rs:90；`api.http` 可为字符串(listen 简写)或对象 (:653) |
| `log` | LogConfig | 否 | level=info | types.rs:94；`level` (:775 默认 `info`)、`file` (:779)、`rotation` (:781，tag=type snake_case: never/minutely/hourly/daily/weekly，各带 `max_files`) |
| `network` | NetworkConfig | 否 | 默认 | types.rs:98；`network.outbound.{default, profiles{name→{resolver,proxy}}}` (:301-357)，profile 结构 `deny_unknown_fields` (:353) |
| `plugins` | Vec<PluginConfig> | 否 | [] | types.rs:102 |

- `PluginConfig`（types.rs:838）：`tag`(:841) | `type`(#[serde(rename="type")] :844) | `args`(Option<Value> :848)。
- **tag 校验**（必须满足，`validate()` types.rs:111）：非空(:164)、≤64 ASCII 字符(:234)、仅 `A-Za-z0-9_.-`(:256)、`.` 分段每段非空且首尾为字母/数字(:261-272)、非保留前缀 `qs.exec.`/`qs.match.`/`qs.cron.`(:282)、全局唯一(:176)。

## 1. server 插件

`udp_server`（udp.rs:281）。无 `allowed_networks` 等客户端 ACL 字段，无 `deny_unknown_fields`（未知键被忽略）。

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `entry` | String | 是 | — | udp.rs:43（引用 executor 的 **裸 tag**，见 config.yaml:41 `entry: main_sequence`） |
| `listen` | String | 是 | — | udp.rs:51（`ip:port` 或 `:port`；`:port`→`[::]:port` 双栈） |

`tcp_server`（tcp.rs:442）。

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `entry` | String | 是 | — | tcp.rs:62 |
| `listen` | String | 是 | — | tcp.rs:69 |
| `cert` | String | 否 | None | tcp.rs:79（PEM；与 key 同现启用 DoT，需 `server-dot` feature，否则硬错误） |
| `key` | String | 否 | None | tcp.rs:84 |
| `idle_timeout` | Duration | 否 | 10s | tcp.rs:90（`deserialize_duration_option`，接受 `"5s"`/`"5"`/秒数） |

**监听口的地址族行为（2026-10-09 guest 实测，oxidns v1.6.1，三臂各起一个进程，端口 15353/15354/15355，`net.ipv6.bindv6only=0`）**：

| 配置 | `ss -lntu` | `nc -w1 127.0.0.1 <port>` | `nc -w1 ::1 <port>` |
|---|---|---|---|
| `0.0.0.0:P` + `[::]:P` 四只（v4/v6 各一对 udp+tcp） | `0.0.0.0:P` 与 `*:P` **同时存在**，四条 | rc=0 | rc=0 |
| 只有 `[::]:P` | `*:P` 一条 | **rc=0** | rc=0 |
| 只有 `0.0.0.0:P` | `0.0.0.0:P` 一条 | rc=0 | **rc=1（refused）** |

三条结论：
1. **oxidns 不设 `IPV6_V6ONLY`** —— 单开 `[::]` 时 v4 也连得上，走的是内核 `bindv6only=0` 的默认双栈。所以"`[::]` 顺带收 v4"是**环境事实而不是插件保证**，防泄漏那条路不许依赖它（换内核、换发行版、运维改了 sysctl 就静默失效）。
2. **v4、v6 各开一对监听不会 EADDRINUSE** —— 同端口上 `0.0.0.0:P` 与 `[::]:P` 实测能同时 bind（先 v4 后 v6 的顺序，14 个插件全部初始化通过）。这一条把我原先担心的"自己埋的第二颗雷"排掉了，`src/gen/oxidns.rs` 的 `render_servers` 保持双份。
3. `0.0.0.0` 那一侧**收不到 v6 查询**（rc=1），所以想让 v6 客户端有落点，`[::]` 那一多是必须显式开的 —— 这就是 `ipv6_mode != off` 时追加 `udp_in6`/`tcp_in6` 的理由。

判据是 TCP 侧的：busybox 没有 `dig`/`drill`，而 UDP 的"无应答"与"ICMP port unreachable"在 `nc -u` 上分不出来；TCP 的 `refused` 是确定的。这只证明**监听层收不收这一族的包**，不证明解析链答得对不对（那一问在 #65）。

## 2. executor：编排 / 转发 / 缓存 / 黑洞

### forward（forward/factory.rs:24）
`ForwardConfig`（config.rs:16，无 deny_unknown）：

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `upstreams` | Vec<UpstreamConfig> | 是(≥1) | — | config.rs:29；空报错 config.rs:47 |
| `concurrent` | usize | 否 | 1 | config.rs:20；clamp `1..=32` 且 ≤上游数（config.rs:11,70-76） |
| `response_selection` | enum | 否 | `balanced` | config.rs:26；snake_case（selection.rs:22）值：`fastest`/`balanced`/`prefer_positive`/`consensus`（selection.rs:24-33） |
| `short_circuit` | bool | 否 | false | config.rs:33 |

`UpstreamConfig`（infra/network/upstream/config.rs:80，无 deny_unknown）——**协议由 `addr` 的 URL scheme 决定，无 `protocol` 字段，无 `server_name` 字段（SNI 从 host 派生，config.rs:523）**：

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `addr` | String | 是 | — | config.rs:92 |
| `tag` | String | 否 | None | config.rs:82 |
| `outbound` | String | 否 | None(取全局默认) | config.rs:99（命名 outbound profile） |
| `dial_addr` | IpAddr | 否 | None | config.rs:107（直连 IP，绕过解析；与 bootstrap 冲突时优先，:455） |
| `port` | u16 | 否 | 协议默认 | config.rs:115（UDP/TCP 53，DoT/DoQ 853，DoH 443，config.rs:32-40） |
| `bootstrap` | String | 否 | None | config.rs:132（须 IP:port，解析 host 用） |
| `bootstrap_version` | u8 | 否 | None | config.rs:138（仅 4/6，:431） |
| `socks5` | String | 否 | None | config.rs:162（`host:port` 或 `user:pass@host:port`） |
| `idle_timeout` | Duration | 否 | 10s | config.rs:175 |
| `max_conns` | usize | 否 | 64 | config.rs:181；0 报错，>4096 报错（:402-413） |
| `min_conns` | usize | 否 | 0 | config.rs:186 |
| `insecure_skip_verify` | bool | 否 | false | config.rs:194 |
| `timeout` | Duration | 否 | 5s | config.rs:209 |
| `enable_pipeline` | bool | 否 | 协议默认 | config.rs:217 |
| `enable_http3` | bool | 否 | false | config.rs:223 |
| `so_mark` | u32 | 否 | None | config.rs:230（仅 Linux） |
| `bind_to_device` | String | 否 | None | config.rs:237（仅 Linux） |

**scheme→协议**（detect_connection_type config.rs:595-613；无 `//` 时自动加 `udp://` :582）：

| 写法 | 协议 | 源码 |
|---|---|---|
| `8.8.8.8` / `udp://8.8.8.8:53` | UDP | config.rs:596 |
| `tcp://…` | TCP | :597 |
| `tcp+pipeline://…` | TCP+强制 pipeline | :598 |
| `tls://dns.google:853` | DoT | :602 |
| `tls+pipeline://…` | DoT+pipeline | :603 |
| `quic://…` / `doq://…` | DoQ | :607 |
| `https://…` / `doh://…` | DoH | :608 |
| `h3://…` | DoH+HTTP/3 | :609 |

DoT 上游：`addr: "tls://1.1.1.1:853"`。DoH 若 host 为域名，宜配 `bootstrap`。

### cache（cache/mod.rs:1684）
`CacheConfig`（mod.rs:86）。**无 `lazy` 布尔键；TTL 覆盖键名为 `lazy_cache_ttl`；落盘路径键名为 `dump_file`（非 dump_path）**：

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `size` | usize | 否 | 1024 | mod.rs:88；默认 mod.rs:45 |
| `lazy_cache_ttl` | u32 | 否 | None | mod.rs:93 |
| `dump_file` | String | 否 | None | mod.rs:96 |
| `dump_interval` | u64(秒) | 否 | 600 | mod.rs:99；默认 mod.rs:49；配 dump_file 时须>0（:1630） |
| `short_circuit` | bool | 否 | false | mod.rs:102 |
| `cache_negative` | bool | 否 | true | mod.rs:105 |
| `max_negative_ttl` | u32(秒) | 否 | 300 | mod.rs:108 |
| `negative_ttl_without_soa` | u32(秒) | 否 | 60 | mod.rs:113（0=不缓存） |
| `max_positive_ttl` | u32 | 否 | None | mod.rs:116 |
| `min_positive_ttl` | u32 | 否 | None | mod.rs:119 |
| `ecs_in_key` | bool | 否 | false | mod.rs:124 |

### black_hole（black_hole.rs:188）
`BlackHoleConfig`（black_hole.rs:40）。args 也可为裸字符串或 IP 数组（简写，:235-253）。

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `mode` | String | 否 | 无 ips→`nxdomain`；有 ips 无 mode→`custom` | black_hole.rs:44；解析 :280-302 |
| `ips` | Vec<String> | 否(仅 custom) | [] | black_hole.rs:50；非 custom 模式带 ips 报错（:287），custom 无 ip 报错（:293） |
| `short_circuit` | bool | 否 | false | black_hole.rs:53 |

`mode` 接受值（大小写不敏感，black_hole.rs:320-330）：`nxdomain` / `nodata` / `null` / `custom` / `refused`。

**`nodata` 的实际应答位**（2026-10-09 guest 实测（guest 时钟显示 10-02，以宿主日期为准），监听 192.168.219.128:10054，查询 example.com / cloudflare.com 的 AAAA）：`rcode=0`、`ANCOUNT=0`、`NSCOUNT=1`（一条合成的 SOA，`TTL=300`）、`ARCOUNT=0`。也就是 RFC 2308 §2.1 的 nodata：域名存在、这一族没有记录，客户端按 300 秒做负面缓存。对照 `nxdomain` 是 `rcode=3`。两者语义不同，所以 ZBox 给广告拦截和地址族抑制画**两个** black_hole（`$blocked` / `$sinkhole`），不共用。

### fallback（fallback.rs:317）
`FallbackConfig`（fallback.rs:45）：

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `primary` | String(executor tag) | 是 | — | fallback.rs:48 |
| `secondary` | String(executor tag) | 是 | — | fallback.rs:50 |
| `threshold` | u64(毫秒) | 否 | 0 | fallback.rs:52-53 |
| `always_standby` | bool | 否 | false | fallback.rs:56 |
| `short_circuit` | bool | 否 | false | fallback.rs:59 |

### sequence（sequence/mod.rs:343）
`args` = `Vec<Rule>`（mod.rs:200, mod.rs:107）。每条规则结构 `Rule`（mod.rs:108）：

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `matches` | String 或 Vec<String> | 否 | 无（恒真） | mod.rs:109-110；单串/数组经 `deserialize_rule_matches`（:100-105，`RawRuleMatchers` untagged :84-89）。数组=逻辑与（chain.rs:386-403 全过才执行） |
| `exec` | String | 是 | — | mod.rs:111；缺失报错 `rule must have 'exec' field`（chain.rs:542-545） |

- **`matches` 元素语法**：可选 `!` 取反前缀（`parse_matcher_expr` matcher/mod.rs:178-192）；随后 `$tag` 引用已注册 matcher（`PluginRef::from_str` mod.rs:533-541）或内联 quick-setup `type 参数`（空格分词，:544-552）。
- **`exec` 取值优先级**：先按内建控制流解析（chain.rs:549,563-590），否则视为 executor 引用（`$tag` 或 quick-setup）。
- **内建控制流词**（BuiltinKind::from_keyword mod.rs:41-52）及语义：

| 词 | 参数 | 行为 | 源码 |
|---|---|---|---|
| `accept` | 无 | 终止本 sequence，Stop，不造响应 | chain.rs:282-292 |
| `return` | 无 | 返回调用方 sequence，Return | chain.rs:293-303 |
| `reject` | `[rcode]` 默认 `refused`(3) | 构造响应并 Stop；rcode 可十进制或助记名(SERVFAIL/NXDOMAIN…)，仅 0..15 | chain.rs:304-315,677-708 |
| `jump` | `<sequence-tag>`(不带 `$`) | 执行子 sequence 后回续本程序 | chain.rs:316-342,580-583 |
| `goto` | `<sequence-tag>`(不带 `$`) | 执行子 sequence 并即时结束 | chain.rs:343-366,584-587 |
| `mark` | `v[,v…]`/空格分隔 u32 | 追加请求本地 mark | chain.rs:367-382,572-575,716-744 |
| `set_mark` | 同上 | 先清空再替换 mark | chain.rs:576-579 |

`jump`/`goto` 目标须为 sequence tag 且拒绝 `$` 前缀（mod.rs:67-82）。

### ttl（ttl.rs:98）

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `fix` | u32 | 否 | None | ttl.rs:58 |
| `min` | u32 | 否 | None | ttl.rs:60 |
| `max` | u32 | 否 | None | ttl.rs:62 |

### prefer_ipv4 / prefer_ipv6（dual_selector.rs:405 / :406）
`DualSelectorConfig`（dual_selector.rs:97）。`prefer_ipv4` 优先 A、`prefer_ipv6` 优先 AAAA（:405-406）。

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `probe_executor` | String(executor tag) | 否 | None | dual_selector.rs:99 |
| `cache` | bool | 否 | None(→false) | dual_selector.rs:101-102 |
| `cache_ttl` | u64(秒) | 否 | None | dual_selector.rs:104 |

### reverse_lookup（reverse_lookup.rs:297）〔feature `plugin-reverse-lookup`〕
`ReverseLookupConfig`（reverse_lookup.rs:52）：`size`(Option<usize> :54)、`handle_ptr`(Option<bool> :56)、`ttl`(Option<u32> 秒 :58)。

### hosts（hosts.rs:249）
`HostsConfig`（hosts.rs:47）：`entries`(Vec<String> :50)、`files`(Vec<String> :53)、`short_circuit`(bool :56)；均默认空/false。规则元素支持 `full:/domain:/keyword:/regexp:`（同 §3 语法，hosts.rs:60-64）。

**每行是 `<域名规则> <IP> [<IP>…]`，域名在前、IP 在后**（`parse_hosts_line` hosts.rs:307-332：首个 token 进 `parse_rule_matcher`，其余逐个 `parse::<IpAddr>()`，任一失败即 `invalid hosts IP '<token>'`；两个列表都空则 `hosts rule contains no valid IP`）。

⚠ **把它指向 `/etc/hosts` 是启动致命项，不是降级**（2026-10-09 guest 实测，oxidns v1.6.1）：/etc/hosts 的顺序正好相反（IP 在前、主机名在后），插件初始化即报错、进程整个起不来。现场证据 —— Alpine 的 `/etc/hosts` 首行是 `127.0.0.1<TAB>localhost.localdomain localhost …`，于是第一个 IP token 之后的 `localhost.localdomain` 被当成 IP 解析：

```
invalid hosts IP 'localhost.localdomain'
```

所以 `dns.use_system_local` 在 oxidns 路线上**没有落点**，渲染器只写注释不写插件（`src/gen/oxidns.rs::render_unexpressible`，单测 `the_system_resolver_option_is_written_as_a_note_not_as_a_broken_plugin` 锁死）。氧化还原：把 `files: ['/etc/hosts']` 那版生成物在 guest 上 `oxidns start` 就是这个 FATAL；改成注释版后 `check` rc=0、`start` 14 个插件全部初始化通过（2026-10-09，guest 时钟比宿主慢 7 天，目录名按宿主日期）。

另外 oxidns **没有**"转给系统解析器"的执行器 —— `forward` 只吃显式 `upstreams`（§executor forward），没有 `system`/`resolv_conf` 一类的取源。

### query_recorder（query_recorder/mod.rs:275）〔feature `plugin-query-recorder`〕
`QueryRecorderConfig`（model.rs:12，字段名即 YAML 键，无 rename）。默认值常量 mod.rs:53-59。

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `path` | String(db 文件) | 是 | — | model.rs:14；空报错 mod.rs:208 |
| `queue_size` | usize | 否 | 8192 | model.rs:15 / mod.rs:53（须>0 :226） |
| `batch_size` | usize | 否 | 256 | model.rs:16 / mod.rs:54 |
| `flush_interval_ms` | u64 | 否 | 200 | model.rs:17 / mod.rs:55 |
| `memory_tail` | usize | 否 | 1024 | model.rs:18 / mod.rs:56 |
| `retention_days` | u64 | 否 | 7 | model.rs:19 / mod.rs:57（须≥1） |
| `cleanup_interval_hours` | u64 | 否 | 1 | model.rs:20 / mod.rs:58 |
| `reader_concurrency` | usize | 否 | 2 | model.rs:21 / mod.rs:59 |

**2026-10-11 guest 实测（v1.6.1，`oxidns-probe`）**，三处源码之外的形状：
- `retention_days: 0` —— `oxidns check` 给 `Configuration is valid` + rc=0，`start` 才 rc=1
  报 `Plugin initialization failed: Plugin error: query_recorder retention_days must be at least 1`
  （构造期拒绝，mod.rs:246-248）。**这是"check 门不验"的第二例**（第一例是 `listen` 的地址合法性）。
  ZBox 因此拦两次：模型 `validate()`（保存 400）+ 渲染器硬错误。
- 记录读回来分两层：`GET /api/plugins/record/records` 的每行是
  `{id, created_at_ms, elapsed_ms, request_id, client_ip, questions_json, req_rd/req_cd/req_ad,
  req_opcode, req_edns_json, error, has_response, rcode, resp_aa/tc/ra/ad/cd, resp_ad,
  answer_count, authority_count, additional_count, answers_json, authorities_json,
  additionals_json, signature_json, resp_edns_json}`，外层 `{ok, next_cursor, records}`；
  **`steps` 只在 `records/<id>` 单条里**（`steps[] = {event_index, sequence_tag, node_index,
  kind, tag, outcome}`，`outcome ∈ entered|stop`）。`stats/top_qnames` 是 `{ok, sample_size, rows[{key,count,share}]}`。
  记录器不记自己：链首那条 `$record` 不出现在 `steps` 里，`node_index` 从 1 起。
- **看板那四份聚合的逐字形状（2026-10-12 guest 实测，v1.6.1，`oxidns-probe/board.yaml` = 面板那份渲染件）**。
  路由是 `GET /api/plugins/record/stats/{top_qnames,top_clients,rcode,qtype,plugins,latency,timeseries,stream}`，
  前四个加 `rcode`/`qtype` 都是 `{ok, sample_size, rows[{key,count,share}]}`，`plugins` 是**另一种形状**
  `{ok, query_total, stats[{kind,tag,checked,matched,executed,query_total,query_share}]}`（`kind` 有
  `executor` 与 `matcher` 两类，`matcher` 的 `executed` 恒 0 而 `checked/matched` 有数）：
  ```
  top_qnames: {"ok":true,"sample_size":13,"rows":[{"key":"chatgpt.com.","count":4,"share":0.3076923076923077}, …]}
  top_clients:{"ok":true,"sample_size":15,"rows":[{"key":"192.168.219.128","count":15,"share":1.0}]}
  rcode:      {"ok":true,"sample_size":15,"rows":[{"key":"_ERROR","count":10,"share":0.6666666666666666},{"key":"No Error","count":5,"share":0.3333333333333333}]}
  qtype:      {"ok":true,"sample_size":15,"rows":[{"key":"A","count":10,"share":0.6666666666666666},{"key":"AAAA","count":5,"share":0.3333333333333333}]}
  ```
  三件从形状里读不出来、只能实测才知道的事：
  1. **`key` 里有三个下划线开头的合成桶**：`_ERROR`（`error` 非空）、`_NO_RESPONSE`（`has_response=0`）、
     `_UNKNOWN`（兜底）。它们不是 DNS 的 rcode 名，面板**原样显示 `key`**、不许翻译也不许并入真 rcode。
  2. **同一次刷新里各格的 `sample_size` 可以不相等**（实测 `top_qnames` 给 13、另外三份给 15）。
     所以"最近 N 条"这句必须**每格带自己的 N**，不许在卡头写一个共用的数，更不许把几格的 count 相加当总量。
  3. **空表不空**：一个全新的 DB、一条查询都没发过时，`rcode` 回
     `{"ok":true,"sample_size":0,"rows":[{"key":"_UNKNOWN","count":1,"share":0.0}]}`（e1/e2 两次新目录都复现）。
     原因在上游那条 SQL：`totals LEFT JOIN sample_records ON 1 = 1`（`store.rs:1020-1028`），左表只有
     `sample_size=0` 这一行时右表全 `NULL`，`CASE … ELSE '_UNKNOWN'` 接住 `NULL` 并 `COUNT(*)` 成 1。
     **面板的界因此是：`sample_size == 0` 时这一格走空态、不画 `rows`**——画出来就是"被打回 1 条"，
     而那台机器上一条查询都没接过。`top_qnames`/`qtype` 同一处不会出这个形状（它们 `JOIN qnames ON 1=1`，
     `qname` 为 `NULL` 时被 `continue` 跳过），所以这条界只针对 `rows` 里 `count` 与 `sample_size` 打架的那种读法。
  4. **ZBox 这一侧搬运时浮点末位会变**（不是上游的事，记在这里是因为它决定了这几格能不能逐字比）：
     `share` 由 `Value` 过一遍 serde 再序列化出去，实测 `0.15384615384615385` 回来成 `0.15384615384615383`
     （1 个 ulp）。屏幕上那是同一个"30.8%"，所以面板照画；但**单测不许按字节比 `share`**——
     `api::tests::the_board_hands_over_the_parse_layer_aggregates_verbatim` 因此比的是结构、顺序、
     `key`/`count` 逐字，加 `share` 的相对误差。
  5. **`plugins` 那两种 `kind` 各只有一半字段有数，而 `query_share` 的分母不是 `query_total`**：
     逐字实测（同一轮，`query_total: 15`）——`executor` 的 `checked`/`matched` 恒 0、只有 `executed` 有数
     （`{"kind":"executor","tag":"cache","checked":0,"matched":0,"executed":10,"query_total":10,"query_share":0.667}`），
     `matcher` 正相反（`{"kind":"matcher","tag":"rule1","checked":10,"matched":0,"executed":0,"query_total":10,"query_share":0.667}`）。
     源码依据 `store.rs:755-800`：`query_share = query_hits / total_records`，也就是**经过这一步的查询占总量多少**。
     **面板的界因此是：那一行的头条必须与那根条同口径**（用 `query_total`），命中/执行另起一句——
     上一版拿 `matched` 当头条，真机画出「匹配 · rule1 ｜ 0 次 · 66.7%」，一行里两个数互相打脸。
- `-d <DIR>` 是 `set_current_dir`，且**发生在读配置文件之前**：`-c` 给相对路径会报
  `failed to read config <名>: No such file or directory`。生成物里的 `path`/`log.file` 因此落在工作目录，
  而起进程的命令行必须用绝对 `-c`。
- 开了 `rotation: daily` 之后盘上的日志名是 `oxidns.log.<UTC 日期>`（本机 05:59 +08:00 起的服务
  写出 `oxidns.log.2026-10-01`），**没有** `oxidns.log` 这个文件；启动那行
  `Error reading the log directory/files: No such file or directory (os error 2)` 是无害噪声（rc 仍 0，文件照建）。

### ros_address_list（ros_address_list/mod.rs:239）〔feature `plugin-ros-address-list`〕
`MikrotikConfigArgs`（config.rs:38，**`#[serde(deny_unknown_fields)]`** :37）。

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `address` | String(`<host>:8728`) | 是 | — | config.rs:40；必填校验 :125 |
| `username` | String | 是 | — | config.rs:42 / :126 |
| `password` | String | 是 | — | config.rs:44 / :127 |
| `connect_timeout` | u64(秒) | 否 | DEFAULT_CONNECT | config.rs:46 / :130 |
| `send_timeout` | u64(秒) | 否 | DEFAULT_SEND | config.rs:48 / :134 |
| `receive_timeout` | u64(秒) | 否 | DEFAULT_RECEIVE | config.rs:50 / :137 |
| `tls` | object | 否 | None(=不启用 TLS) | config.rs:52 |
| `async` | bool | 否 | **true** | config.rs:55-56 `#[serde(rename="async")] async_mode`；默认 :28,:191 |
| `wait_timeout` | Duration | 否 | 8s | config.rs:58-59 |
| `queue_capacity` | usize | 否 | 16384 | config.rs:61 / :172 |
| `address_list4` | String | 视需要 | None | config.rs:63（4/6 至少一个 :150） |
| `address_list6` | String | 视需要 | None | config.rs:65 |
| `comment_prefix` | String | 否 | `oxi` | config.rs:68 / :156（禁含 `;`/`=` :274） |
| `persistent` | object | 否 | None | config.rs:70；子键 `ips`(Vec<String>)、`files`(Vec<String>)（PersistentArgs :84-89） |
| `min_ttl` | u32(秒) | 否 | 60 | config.rs:72 / :24 |
| `max_ttl` | u32(秒) | 否 | 3600 | config.rs:74 / :26（须≥min :162） |
| `fixed_ttl` | u32(秒) | 否 | None | config.rs:77（0=不设 RouterOS timeout） |
| `cleanup_on_shutdown` | bool | 否 | true | config.rs:79 / :30 |

`tls` 子对象 `RouterOsTlsArgs`（routeros/transport.rs:53，deny_unknown :52）：`insecure`(Option<bool> :54)、`server_name`(Option<String> :55)、`ca`(Option<String> :56)。

### ros_route（ros_route/mod.rs:232）〔feature `plugin-ros-route`〕
`MikrotikConfigArgs`（ros_route/config.rs，deny_unknown）。共享 `address/username/password/connect_timeout/send_timeout/receive_timeout/tls/async/wait_timeout/queue_capacity/persistent(ips,files)/min_ttl(60)/max_ttl(3600)/fixed_ttl/cleanup_on_shutdown(true)/comment_prefix(oxi)`，专属字段：

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `routing_table` | String | 是 | — | config.rs:57；必填校验 :149 |
| `gateway4` | String | 视需要 | None | config.rs:59（4/6 至少一个 :157） |
| `gateway6` | String | 视需要 | None | config.rs:61 |
| `distance` | u8 | 否 | 100 | config.rs:66 / :27,:153 |
| `conntrack_guard` | bool | 否 | false | config.rs:80 / :26 |

`async` 键名同样为 `#[serde(rename="async")]`（config.rs，共享 transport），默认 true（:24）。

## 3. matcher：qname / client_ip

`args` 统一为**字符串或字符串数组**（`parse_rule_list_value` rules/config.rs:42-65：单串按 `,`/空白切分 :116-122）。逐 token 分类（`split_rule_sources` rules/sources.rs:98-123）：`$tag`=引用 provider、`&/path/file`=文件源、其余=内联规则。

`qname`（qname.rs:29）内联规则前缀语法（`split_domain_rule_expression` core/rule_matcher/domain.rs:138-149）：

| 前缀/形式 | 语义 | 源码 |
|---|---|---|
| `full:www.a.com` | 精确全名 | domain.rs:139 |
| `domain:a.com` / 裸 `a.com` | 后缀匹配（**默认**，含子域） | domain.rs:141,148 |
| `keyword:foo` | 子串 | domain.rs:143 |
| `regexp:^a\.com$` | 正则 | domain.rs:145 |
| `$provider_tag` | 引用 domain-capable provider | sources.rs:110-113 |
| `&/path/file` | 逐行文件规则源 | sources.rs:114-117 |

须至少一条内联规则或 `$` provider，否则报错（qname.rs:66 `validate_non_empty_domain_rules_or_set_tags`）。

`client_ip`（client_ip.rs:31）规则为 IP/CIDR（`IpPrefixMatcher::add_rule`，sources.rs:66-81）；同样支持 `$ip_set_tag` 引用与 `&file`。须≥1 规则或 `$` ip_set（client_ip.rs:68）。接受裸 IP 与 CIDR。

`qtype`（matcher/qtype.rs）：`args` 是记录类型 token 列表，token 走 `RecordType::from_token`（proto/src/types.rs:482）——`"A"` / `"AAAA"` 大小写不敏感，数字码（`1` / `28`）同样接受。**2026-10-09 guest 实测（guest 时钟显示 10-02，以宿主日期为准）**（oxidns 1.6.1 full，`/root/zbox-rounds/v2026-10-09f/sink-shape.yaml`）：`args: [AAAA]` 过 `oxidns check`（`plugins: 5`，rc=0），并在链首命中 AAAA 查询。

## 4. provider

`domain_set`（domain_set.rs:182，无 feature 门控）。`DomainSetArgs`（domain_set.rs:38）：

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `exps` | Vec<String> | 否 | [] | domain_set.rs:39-42（内联，支持 §3 前缀语法） |
| `sets` | Vec<String> | 否 | [] | domain_set.rs:44-45（引用其它 domain provider tag） |
| `files` | Vec<String> | 否 | [] | domain_set.rs:46-48（逐行文本） |

`ip_set`（ip_set.rs:169，无门控）。`IpSetArgs`（ip_set.rs）：`ips`(Vec<String> IP/CIDR)、`sets`(引用 tag)、`files`(逐行)，均默认 []。

`geosite`（geosite.rs:188）〔feature `provider-protobuf`〕。**读本地 `.dat` 文件路径，不下载**（`DatFileSession::open(Path::new(&args.file))` geosite.rs）。`GeoSiteArgs`（geosite.rs）：

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `file` | String(.dat 路径) | 是 | — | geosite.rs:31；GeoSiteArgs 结构 :30 |
| `selectors` | Vec<String> | 否 | [] | geosite.rs:33（如 `cn`、`Gfw`，选择 geosite 分组） |

`geoip`（geoip.rs:185）〔feature `provider-protobuf`〕。`GeoIpArgs`（geoip.rs:29）：`file`(String 必填 .dat，:30；空报错 :201)、`selectors`(Vec<String> 默认 []，:31)。同为本地 `DatFileSession::open`，不下载。

`dynamic_domain_set`（dynamic_domain_set/mod.rs:112）〔feature `plugin-dynamic-domain`〕。`DynamicDomainSetArgs`（config.rs，deny_unknown :16）：

| YAML key | 类型 | 必填 | 默认 | 源码 |
|---|---|---|---|---|
| `path` | String(可写文件) | 是 | — | config.rs |
| `bootstrap_rules` | Vec<String> | 否 | [] | config.rs（仅 path 不存在时写入） |
| `queue_size` | usize | 否 | 1024 | config.rs:65 / DEFAULT :11 |
| `batch_size` | usize | 否 | 256 | config.rs:66 / :12 |
| `flush_interval_ms` | u64 | 否 | 200 | config.rs:67 / :13 |

`adguard_rule`（adguard_rule/mod.rs:130）〔feature `provider-adguard-rule`〕。`AdGuardRuleConfig`（config.rs:12）：`rules`(Vec<String> 默认 []，:14)、`files`(Vec<String> 默认 []，:16)。

## 5. Feature 门控（默认构建 = `full`）

- `Cargo.toml:158` `default = ["full"]`。`full = standard + server-doh3 + resolver-doh3 + upstream-doh3 + plugin-mikrotik + plugin-ipset`（:202-209）。`standard` 含 `provider-protobuf`、`provider-adguard-rule`、`plugin-dynamic-domain`、`plugin-reverse-lookup`、`plugin-query-recorder` 等（:172-199）。
- 发布主二进制：`cargo build --no-default-features --features full`（.github/workflows/release.yml:209,216）。**无 `oxidns-aio` feature/bundle 名**（全仓 grep 无命中）。

问题项是否默认开启：

| 插件 | 门控 feature | 默认(full) | 源码 |
|---|---|---|---|
| query_recorder | plugin-query-recorder | **是** | executor/mod.rs:63-64 / Cargo.toml:282（含 api） |
| ros_address_list | plugin-ros-address-list | **是**(full→plugin-mikrotik) | executor/mod.rs:75-76 / Cargo.toml:272-275 |
| ros_route | plugin-ros-route | **是** | executor/mod.rs:77-78 / Cargo.toml:278 |
| geosite | provider-protobuf | **是**(standard) | provider/mod.rs:35-36 / Cargo.toml:332 |
| geoip | provider-protobuf | **是** | provider/mod.rs:33-34 |
| adguard_rule | provider-adguard-rule | **是** | provider/mod.rs:28-29 / Cargo.toml:335 |
| dynamic_domain_set | plugin-dynamic-domain | **是** | provider/mod.rs:31-32 / Cargo.toml:292 |
| reverse_lookup | plugin-reverse-lookup | **是** | executor/mod.rs:73-74 / Cargo.toml:329 |
| udp/tcp_server,sequence,fallback,forward,cache,black_hole,ttl,prefer_ipv4/6,hosts,domain_set,ip_set,qname,client_ip | 无门控 | 始终编译 | executor/mod.rs:35-88, matcher/mod.rs:27-47, provider/mod.rs:30-37 |

注：DoT/DoH/DoQ 上游需 `upstream-dot/-doh/-doq`（standard 含），`minimal` bundle 仅 UDP/TCP。

## 6. 最小可接受 YAML 骨架

每行均有源码支撑；协议经 `addr` scheme 选定；`entry` 用裸 tag、`exec`/`matches` 引用用 `$`。

```yaml
log:
  level: info                              # types.rs:775 (默认即 info)
plugins:
  - tag: domestic                          # forward — 纯 UDP IP 上游
    type: forward                          # factory.rs:24
    args:
      upstreams:                           # config.rs:29 (必填≥1)
        - addr: "223.5.5.5"                # config.rs:92；无 // → udp:// (config.rs:582)
  - tag: overseas                          # forward — DoT 上游
    type: forward
    args:
      upstreams:
        - addr: "tls://1.1.1.1:853"        # scheme tls → DoT (config.rs:602)
  - tag: cachex
    type: cache                            # cache/mod.rs:1684
    args:
      size: 4096                           # mod.rs:88
  - tag: blocked
    type: black_hole                        # black_hole.rs:188
    args:
      mode: nxdomain                        # black_hole.rs:322
  - tag: cn_rule                            # matcher qname（独立插件，args=规则列表）
    type: qname                             # qname.rs:29
    args:
      - "domain:cn"                         # domain.rs:141 后缀
  - tag: lan_rule                           # matcher client_ip
    type: client_ip                         # client_ip.rs:31
    args:
      - "192.168.0.0/16"                    # CIDR (sources.rs:66)
  - tag: main                               # sequence 编排
    type: sequence                          # sequence/mod.rs:343
    args:                                   # Vec<Rule> (mod.rs:200)
      - matches: $cn_rule                   # 引用 matcher (mod.rs:533)
        exec: $cachex                       # 引用 executor (chain.rs:611)
      - matches: $lan_rule
        exec: $domestic
      - exec: $overseas                     # 兜底：无 matches 恒真 (mod.rs:109)
      - exec: accept                        # 内建控制流 (chain.rs:282)
  - tag: udp_in
    type: udp_server                        # udp.rs:281
    args:
      entry: main                           # 裸 tag (udp.rs:43 / config.yaml:41)
      listen: "0.0.0.0:5353"                # udp.rs:51
  - tag: tcp_in
    type: tcp_server                        # tcp.rs:442
    args:
      entry: main
      listen: "0.0.0.0:5353"                # tcp.rs:69
```

（black_hole `blocked`、cache `cachex` 已接入 sequence 链以满足“无 live 插件的 provider/executor 会被跳过”规则 registry/mod.rs:238-252；如需广告拦截可将 `$blocked` 挂到某 `matches` 分支。）

## 7. 被推翻（与旧字典 `oxidns-config-dictionary.md` 不一致处）

1. **`oxidns-aio` 不存在**：默认/release bundle 是 `full`（`Cargo.toml:158`，release.yml:209）。字典/任务描述中的 `oxidns-aio` feature 名在源码无命中。
2. **`forward` 无 `protocol`/`server_name` 字段**：协议完全由 `addr` 的 URL scheme 决定（config.rs:595-613），SNI 从 host 派生（:523）。旧字典若暗示显式协议字段则有误；可选字段是 `dial_addr`/`bootstrap`/`port`/`timeout`/`socks5` 等。
3. **`cache` 键名**：无 `lazy` 布尔键（实为 `lazy_cache_ttl` mod.rs:93）；落盘键为 `dump_file`（非 `dump_path`，mod.rs:96）。
4. **`geosite`/`geoip` 不下载**：仅 `file`(必填 .dat 路径) + `selectors`，本地 `DatFileSession::open`；且受 `provider-protobuf` 门控（默认仍开）。旧字典 §8 存疑“是否下载”→答案：不下载，读本地文件。
5. **server 无 `allowed_networks` 等客户端 ACL 字段**：`udp_server` 仅 `entry`/`listen`，`tcp_server` 仅加 `cert`/`key`/`idle_timeout`；且未设 `deny_unknown_fields`（多余键静默忽略，非报错）。
6. **`sequence` 控制流词补全**：除 accept/return/reject/jump/goto/mark 外，还有 **`set_mark`**（替换式，mod.rs:48/chain.rs:576）；`mark`/`set_mark` 值为 u32 列表（`,`或空白分隔）。
7. **`forward` `response_selection` 值确认**：`fastest`/`balanced`(默认)/`prefer_positive`/`consensus`（selection.rs:24-33），非任意名。
8. **ROS `async` 键名确认**：YAML 键就是 `async`，经 `#[serde(rename="async")]` 映射到 Rust `async_mode`，默认 **true**（ros_*/config.rs:55-56）。`tls` 子对象三键：`insecure`/`server_name`/`ca`（transport.rs:54-56），存在即启用 API-SSL。
9. **`ros_route` `routing_table` 为必填**（config.rs:149），旧字典未标注必填性；`gateway4`/`gateway6` 至少其一。
