# sing-box 1.14.2 配置字段对照表（ZBox 生成器唯一依据）

来源：本地留档 `reference/singbox-1142/sing-box-1.14.2/`。字段名以 `option/*.go` 的
`json:` struct tag 为准（下表每条都附了文件:行），语义以同目录 `docs/configuration/**` 为准。

**2026-09-29 起多了一条更强的依据**：钉死的 1.14.2 二进制自带 `sing-box schema` 子命令，
它从**这个版本自己的类型注册表**吐出完整 JSON Schema（draft 2020-12，444,895 字节，93 个
`$defs`，留档在 `.tmp/sb1142-schema.json`）。某个键"在这个版本到底存不存在"以后不用猜也不用
读文档，`sing-box schema` 里 `properties` 有没有它就是结论。§11 记录了已经这么定案的条目。

**为什么字段名不能凭记忆写**：`option/options.go:41` 用了 `decoder.DisallowUnknownFields()`，
配置里任何一个多余或改名掉的键都会让 `sing-box check` 整体失败；在旁路由上这意味着
sing-box 起不来、全家断网。所以本表的用途是"照抄"，不是"参考"。

> **`check` 通过 ≠ 起得来。** VM 实测已经抓到两类 `check` 放行、`sing-box run` 才 FATAL 的写法
> （`detour:"direct"`、缺 `default_domain_resolver`、远端规则集没显式 `http_client`）。
> 验收命令必须是 `sing-box run`，不是 `sing-box check`。

> `reference/singbox/sing-box-testing/` 是 testing 分支，它把 DNS 改成了 typed server 之外的
> 另一套结构、加了 `dns.final` 与逻辑规则 `mode: and/or`。**不要照它写**，产出的配置装不上稳定版。

---

## 1. 顶层

`option/options.go:14-31`：`log` `dns` `ntp` `certificate` `certificate_providers`
`http_clients` `network_namespaces` `endpoints` `inbounds` `outbounds` `route`
`services` `experimental`，全部可选。

- 没有 `extension` 顶层键。
- **没有 `conf-directory`/`include`**：多文件不是配置 schema 的一部分，生成器必须产出单文档。

## 2. 已被删除的写法（生成器禁止输出）

| 禁止 | 取代 | 依据 |
|---|---|---|
| `dns.servers[].address` / 无 `type` 的 legacy server | `{"type":"udp","server":..,"server_port":..}` | dns.go:32,131 |
| `dns.fakeip{}` 块 | `dns.servers[]` 里的 `{"type":"fakeip","tag":..,"inet4_range":..}` | dns.go:31,45 |
| `dns.independent_cache` | 每个 `domain_resolver` 自己的缓存项（1.14 起 deprecated） | dns.go:67 |
| `type:"dns"` 出站 | 路由规则 `{"action":"hijack-dns"}` | outbound.go:44 |
| 入站上的 `sniff` / `sniff_override_destination` / `sniff_timeout` / `domain_strategy` / `udp_disable_domain_unmapping` | `{"action":"sniff"}` 与 `route-options` | inbound.go:50-77 |
| `direct` 出站的 `override_address`/`override_port` | `route-options` 动作 | direct.go:19-24 |
| 规则里裸的 `"outbound":"tag"` 配一个非 route 的动作 | `outbound` 是 route 动作的字段；`action` 可整个省略（`rule_action.go:70-72` 空 action→route），所以 `{"ip_is_private":true,"outbound":"direct"}` 合法，而 `{"action":"reject","outbound":..}` 非法 | rule_action.go:70-72 |
| 规则里的 `geosite` / `geoip` / `source_geoip`（`schema:"omit"`） | `rule_set` + `.srs` | route/geoip.md:5（1.12 移除） |
| `route.geoip{}` / `route.geosite{}` | 同上 | route.go:6-7 |
| `transport.type:"tcp"` | **整个省略 `transport`** | v2ray_transport.go:11 枚举 `http,ws,quic,grpc,httpupgrade` |
| `transport` 里 `ws` 带 `host` | 折叠成 `headers.Host` | v2ray_transport.go:94 |
| 客户端 `tls.reality.handshake{}` | 握手目标=出站 `server:server_port`，伪装域名=`tls.server_name` | tls.go:252 |
| `so_mark` | `routing_mark` | 全仓 grep 只有 `routing_mark`（inbound.go:83 / outbound.go:91） |
| `urltest.intervals` / `filter` / `interrupt_only` | `interval`（单数，duration 字符串）/ `interrupt_exist_connections` | group.go:11-18 |
| `urltest.url` 数组 | 单个字符串 | group.go:14 |
| `rule_set` 的 `download_detour` / `download_url` / `format:"raw"` | `http_client`（**引用一个 `http_clients` 里的 tag，且不写 `detour`**，见 §11）/ 无 / `source\|binary` | rule_set.go:184-187 / 124-136 |
| `experimental.clash_api.{cache_file,cache_id,store_mode,store_selected,store_fakeip}` | `experimental.cache_file` | experimental.go:32-41 |

## 2.5 两条经源码复核、直觉容易搞反的事实

1. **逻辑规则在 1.14.2 就已存在**，不是 testing 分支专属：`{"type":"logical","mode":"and|or",
   "rules":[...],"invert":bool}`（`option/rule.go:211-215`、`option/rule_dns.go:237-241`）。
   ZBox 暂时不用它，但生成器不必刻意回避。
2. **`geosite('ads')` 这类函数式写法在 1.14.2 完全不存在**（全树 grep 零命中）。
   规则集匹配只能写 tag 字符串数组：`"rule_set": ["geosite-cn", "geoip-cn"]`，
   且这些 tag 必须在 `route.rule_set[]` 里定义过，否则 check 失败。

## 3. `dns`

顶层（`option/dns.go:18-24,62-71`）：`servers[]` `rules[]` `final`(server tag)
`reverse_mapping` `strategy`(`prefer_ipv4|prefer_ipv6|ipv4_only|ipv6_only`) `timeout`
`disable_cache` `disable_expire` `cache_capacity` `optimistic`(bool 或 `{enabled,timeout}`)
`client_subnet`。

server = typed 对象：`type` + `tag` + 各自字段（`option/dns.go:108-146`）。

| type | 字段 | 依据 |
|---|---|---|
| `local` | Dial Fields + `prefer_go` + `neighbor_domain[]` | dns.go:184-192 |
| `hosts` | `path`(string\|[]) `predefined`(map) | dns.go:179-182 |
| `udp` `tcp` | `server`(必填) `server_port`(默认 53) + Dial Fields | dns.go:194-197 |
| `tls` `quic` | 上面 + `tls{}`（默认口 853） | dns.go:199-202 |
| `https` `h3` | 上面 + `path`(默认 `/dns-query`) `method` `headers` | dns.go:204-209 |
| `fakeip` | `inet4_range` `inet6_range` | dns.go:211-214 |
| `dhcp` | local 字段 + `interface`(string) | dns.go:216-219 |
| `mdns` | local 字段 + `interface`(string[]) | dns.go:221-224 |
| `resolved` | `service`(tag，必填) `accept_default_resolvers` | server/resolved.md |
| `tailscale` `openconnect` | `endpoint`(tag，必填) `accept_default_resolvers` `accept_search_domain` | 对应 docs |

地址键就叫 `server` / `server_port`（`DNSServerAddressOptions`，dns.go:160-163）。
server 上**没有** `variant`/`record_type`/`address_family`/`timeout`/`client_subnet`/`domain_suffix`；
按域名挑 server 只能靠 `dns.rules`。

Dial Fields（`option/outbound.go:80-112`，可用于 local/udp/tcp/tls/quic/https/h3/dhcp/mdns）：
`detour`(出站 tag) `bind_interface` `inet4_bind_address` `inet6_bind_address`
`bind_address_no_port` `protect_path` `routing_mark` `reuse_addr` `connect_timeout`
`tcp_fast_open` `tcp_multi_path` `disable_tcp_keep_alive` `tcp_keep_alive`
`tcp_keep_alive_interval` `udp_fragment` `domain_resolver`(tag 字符串或对象)
`network_strategy` `network_type` `fallback_network_type` `fallback_delay`。

dns 规则（`option/rule_dns.go:16-20,137-196`）：匹配键与路由规则几乎同名，注意
`query_type`（`A`/`AAAA`/… 或整数）、`query_client_subnet`、`query_dnssec`、`source_ip_cidr`、
`source_ip_is_private`、`ip_cidr`、`ip_is_private`、`ip_accept_any`、`rule_set`、
`rule_set_ip_cidr_match_source`、`match_response`、`response_rcode|answer|ns|extra`、`invert`、
`preferred_by`(server tag 列表)、`inbound`。
动作（`rule_action.go:102`）：`route`(默认，需 `server`) `evaluate` `respond` `route-options`
`reject`(`method: default|drop`) `predefined`(`rcode` NOERROR/FORMERR/SERVFAIL/NXDOMAIN/…)。
**DNS 规则没有 `direct` / `sniff` / `hijack-dns` / `replay` / `preload`**——那些是 ≤1.10 的旧集合，
`hijack-dns` 只存在于 **route** 规则的动作里。
`route` 动作内可带 `speculative` `race` `disable_cache` `rewrite_ttl` `timeout`
`client_subnet` `remove_client_subnet`；其 `strategy` 已 deprecated（1.16 移除），不要用。

## 4. `route`

顶层（`option/route.go:5-24`）：`rules[]` `rule_set[]` `final`(出站 tag) `find_process`
`find_neighbor` `dhcp_lease_files[]` `auto_detect_interface` `override_android_vpn`
`default_interface` `default_mark`(int 或 `"0x2023"`) `default_domain_resolver`
`default_network_strategy` `default_network_type` `default_fallback_network_type`
`default_fallback_delay` `default_http_client`。

规则匹配键（`option/rule.go:140-191`）：`inbound[]` `ip_version`(4|6) `network[]`(`tcp,udp,icmp`)
`auth_user` `protocol[]`(`tls,http,quic,dns,stun,bittorrent,dtls,ssh,rdp,ntp`) `client[]`
`domain` `domain_suffix` `domain_keyword` `domain_regex` `source_ip_cidr` `source_ip_is_private`
`ip_cidr` `ip_is_private` `source_port` `source_port_range` `port` `port_range` `process_name`
`process_path` `process_path_regex` `package_name` `package_name_regex` `user` `user_id`
`clash_mode` `network_type` `network_is_expensive` `network_is_constrained` `wifi_ssid`
`wifi_bssid` `interface_address` `network_interface_address` `default_interface_address`
`source_mac_address` `source_hostname` `preferred_by` `rule_set[]`(tag) 
`rule_set_ip_cidr_match_source` `invert`。

动作（`rule_action.go:19`）：`route` `route-options` `direct` `bypass` `reject` `hijack-dns`
`sniff` `resolve`。
- `{"action":"route","outbound":"tag"}`，`outbound` 在动作对象里（rule_action.go:174-177）；
  `action` 可省略（默认即 `route`）。
- `route-options` 字段（rule_action.go:179-195）：`override_address` `override_port`
  `network_strategy` `fallback_delay` `udp_disable_domain_unmapping` `udp_connect`
  `udp_timeout` `tls_fragment` `tls_fragment_fallback_delay` `tls_record_fragment`
  `tls_spoof` `tls_spoof_method`。
- `reject`：`method` = `default|drop|reply`。`sniff`：`sniffer` + `timeout`。
  `resolve`：`server` `timeout` `strategy` `disable_cache` `rewrite_ttl` `client_subnet`。

rule_set（`option/rule_set.go:22-29,177-188`）：`type` = `inline|local|remote`；
`tag` 可为 string 或 string[]（多 tag 时 `path`/`url` 必须含 `{tag}`）；`format` =
`source|binary`，且会按扩展名自动判定（`.json`→source，`.srs`→binary）；**没有 `raw` 格式，
也没有 `behavior` 字段**（那是规则集文件内部格式的键，1.14.2 的 `route.rule_set` 上没有）。
`path`(local) `url`/`initial_path`/`http_client`/`update_interval`(remote)。

`.srs` 的下载源（**2026-09-29 VM 实测，把本表先前那条结论推翻了**）：编译产物就发布在
SagerNet/sing-geosite、sing-geoip 的 **`rule-set` 分支**上，直接
`https://raw.githubusercontent.com/SagerNet/<repo>/rule-set/<file>.srs` 取；
**release 附件里只有 `.db`**，`releases/latest/download/<file>.srs` 一律 404。
（此前本表写的正好相反，是照文档猜的，没实测。）

- URL 里的 `!`（`geosite-geolocation-!cn.srs`）必须是字面量：写成 `%21` 是 404。
- **没有 `geoip-private.srs`**（该分支 238 个文件全列过，上游就没做这个集合）。私网段只能靠
  规则里的 `ip_is_private`，不要为它生成规则集。
- `tag` 写成数组时，`url` 与 `initial_path` 必须含 `{tag}` 占位符
  （`constant/rule.go`：`RuleSetTagPlaceholder = "{tag}"`；`option/rule_set.go:107-118` 校验）。
  单 tag 不校验，所以这条容易一直不触发。
- `initial_path` **只会被读、不会被写**（`route/rule/rule_set_remote.go:120-135`）。下载到的内容
  落在 `experimental.cache_file` 里。因此首次启动要么 `initial_path` 上已经有文件，要么必须联网；
  两者都没有而下载又失败 = 启动期 FATAL。详见 §11。
- **`domain_suffix` 是标签边界，不是裸字符串后缀**（2026-10-05 guest 逐字抓取，1.14.2，源
  `{"version":5,"rules":[{"domain_suffix":["test"]},{"domain_suffix":["cn"]}]}`；判据是合并输出里
  有没有 `match rules.[N]`——命中与未命中 rc 都是 0，命中行在 **stderr**，stdout 恒空）：

  | 待匹配值 | 命中 | 结论 |
  |---|---|---|
  | `test` | rules.[0] | 顶点域本身算命中 |
  | `a.test` / `sub.a.test` | rules.[0] | 任意层子域 |
  | `mytest` / `a-test` / `abcn` | **无** | 不是 `strings.HasSuffix`；分隔符必须是点 |
  | `x.test.evil` | **无** | 后缀必须在末尾 |
  | `example.cn` | rules.[1] | 于是 `gen/singbox/dns.rs` 那条 `domain_suffix:["cn"]` 兜底成立 |

  反向猜测也一并否证：把 `cn` 写进 `domain_suffix` **不会**误伤 `abcn` 这类名字，
  所以"单列 .cn"没有额外的噪音面。
  **测量面是 `rule-set match`（`HeadlessRule.domain_suffix`），不是内联 route 规则**——两处同一个键名，
  但内联路径这轮没单独跑；要收掉这个缺口得在 netns 里发真查询、看 `dns: match[..]` 日志。
  ZBox 用这条的地方：`doctor::covers`（schema v2 前叫 `blocked_shadows`）只在"标签边界"这一形状下才报
  "排在前面的那一行盖住了后面这行"；判不动的形状（`Regex`，以及 `Keyword` 作为被盖住的一方以外的组合）
  一律不报——误报会让人不再相信这条判据，漏报只是少句提示。
  **已验（2026-10-05，netns 里真查询）**：v2 的 DNS 层新写法（域名 + `query_type` 同时出现在
  一条规则里、`server` 指向 fakeip）在这台 1.14.2 上跑过了：`A` 查询回 `198.18.0.2`，`TXT` 查询
  **不**落 fakeip 而是继续走到 `dns.final` —— **域名与 `query_type` 取交集**成立。
  上面那张表测的仍是 `rule-set match` 的 `HeadlessRule` 路径；内联 `dns.rules` 这条独立路径的
  九条探针原始应答与日志见 §11.9 第三节。

## 5. `inbounds`

公共 Listen Fields（`option/inbound.go:79-103`）：`listen` `listen_port` `bind_interface`
`routing_mark` `reuse_addr` `tcp_fast_open` `tcp_multi_path` `tcp_keep_alive`
`tcp_keep_alive_interval` `disable_tcp_keep_alive` `udp_fragment` `udp_timeout` `netns`
`detour`(入站 tag)。

**已验（2026-10-09，guest 直连）**：`listen: "::"` 是**一只双栈 socket**，不是 v6 专用。现场是
`direct` 入站 `::`/19888（临时端口、独立 `-D`、不加载劫持层）—— `ss -lntu` 报 `*:19888`，日志
`tcp server started at [::]:19888`，`nc -w1 127.0.0.1 19888` 与 `nc -w1 ::1 19888` **都 rc=0**。
所以 v6 客户端能进、v4 客户端也能进；而 `listen: "0.0.0.0"` 那一侧收不到 v6（对照见
[[oxidns-plugin-args-verified]] §1 那张表）。两条约束因此成立：
① 要让入站接住 v6，必须把 `listen` 写成 `"::"` 而不是再加一条 v6 入站；
② 1.14.2 已核实的 Listen Fields 里**没有** `v6_only` 一类的键，不许凭空写 ——
   想让 v4/v6 各占一只 socket 只能靠绑具体地址，不能靠 socket 选项。

| type | 专有字段 | 依据 |
|---|---|---|
| `tproxy` | `network[]` `udp_mapping` `udp_filtering`(`endpoint_independent|address_dependent|address_and_port_dependent`) `udp_nat_max`；仅 Linux | redir.go:9 |
| `redirect` | 无专有字段（只有 Listen Fields） | redir.go:4 |
| `direct` | `network[]` `override_address` `override_port` | direct.go:10 |
| `mixed` | `users[{username,password}]` `domain_resolver` `set_system_proxy` `tls{}` | simple.go:14 |
| `socks` | `users[]` `domain_resolver` | simple.go:8 |
| `http` | 同 mixed | simple.go:14 |
| `tun` | **不继承 Listen Fields**（无 `listen`/`listen_port`/`routing_mark`）。网卡名是 `interface_name`（不是 `name`）；协议栈是 `stack: "system"|"gvisor"|"mixed"`（**不是布尔 `system`**）；地址用 `address[]`（`inet4_address`/`inet6_address` 已并入）；`mtu` `dns_mode`(`disabled|native|hijack`) `dns_address[]` `auto_route` `auto_redirect`(1.13) + `auto_redirect_input_mark`(默认 `0x2023`) `auto_redirect_output_mark`(`0x2024`) `auto_redirect_reset_mark`(`0x2025`) `auto_redirect_nfqueue`(100) `iproute2_table_index`(2022) `iproute2_rule_index`(9000) `strict_route` `route_address[]` `route_exclude_address[]` `route_address_set[]` `include_interface[]`/`exclude_interface[]`（**顶层，不在 platform 下**） `platform`（Linux 下只有 `http_proxy{enabled,server,server_port,bypass_domain[],match_domain[]}`） | tun.go:14-53 / tun_platform.go:5-14 |

`tun` 上已移除、禁止输出：`gso`、`endpoint_independent_nat`、`inet4_address`、
`inet6_address`、`inet4_route_address`、`inet6_route_address`、
`inet4_route_exclude_address`、`inet6_route_exclude_address`（tun.go:56-72）。

`so_mark` 不存在，出站打标（防回环）用 `routing_mark`。

## 6. 出站协议字段（已按 struct 复核）

| type | 字段 | 依据 |
|---|---|---|
| `vless` | `uuid` `flow`(`xtls-rprx-vision|xtls-rprx-vision-udp443`) `packet_encoding`(`packetaddr|xudp`) `network` `tls{}` `multiplex{}` `transport{}` + Dial Fields | vless.go:26 |
| `vmess` | `uuid` `alter_id` `security`(`auto,none,zero,aes-128-cfb,aes-128-gcm,chacha20-poly1305`，**无 omitempty**) `global_padding` `authenticated_length` `packet_encoding` | vmess.go:19 |
| `trojan` | `password` `network` `tls{}` `multiplex{}` `transport{}` | trojan.go:22 |
| `shadowsocks` | `method` `password` `plugin` `plugin_opts` `network` `udp_over_tcp{}` `multiplex{}` | shadowsocks.go:29 |
| `hysteria2` | `password` `obfs{type,password}`(`salamander|gecko`) `up_mbps` `down_mbps` `server_ports[]` `hop_interval` `disable_path_mtu_discovery` `bbr_profile` `brutal_debug` `disable_chrome_parrot` `tls{}` | hysteria2.go:~120 |
| `tuic` | `uuid` `password` `congestion_control`(`cubic,new_reno,bbr`) `udp_relay_mode`(`native,quic`) `udp_over_stream` `zero_rtt_handshake` `heartbeat` `network` `tls{}` | tuic.go:22 |
| `socks` | `server` `server_port` `version`(`4,4a,5`) `username` `password` `network` `udp_over_tcp{}` | simple.go:22 |
| `direct` | 只有 Dial Fields | direct.go |
| `selector` | `outbounds[]`(必填) `default` `interrupt_exist_connections` | group.go:5 |
| `urltest` | `outbounds[]`(必填) `url`(string) `interval`(duration 串) `tolerance`(int) `idle_timeout` `interrupt_exist_connections` | group.go:11 |

`tls{}`（`option/tls.go:OutboundTLSOptions`）：`enabled` `disable_sni` `server_name` `insecure`
`alpn[]` `min_version` `max_version` `cipher_suites` `curve_preferences` `certificate[]`
`certificate_path` `client_certificate*` `fragment` `fragment_fallback_delay` `record_fragment`
`spoof` `spoof_method` `handshake_timeout` `ech{enabled,config[],config_path,query_server_name}`
`utls{enabled,fingerprint}` `reality{enabled,public_key,short_id}`。
`utls.fingerprint` 枚举：`chrome,firefox,edge,safari,360,qq,ios,android,random,randomized,
chrome_psk,chrome_psk_shuffle,chrome_padding_psk_shuffle,chrome_pq,chrome_pq_psk`。

`multiplex{}`（`option/multiplex.go`）：`enabled` `protocol`(`h2mux|smux|yamux`)
`max_connections` `min_streams` `max_streams` `padding` `brutal{enabled,up_mbps,down_mbps}`。
→ 混合 brutal 走 `multiplex.brutal`，Hysteria2 自身的带宽走顶层 `up_mbps/down_mbps`。

transport 子对象（`option/v2ray_transport.go:85-160`）：
`http{host[],path,method,headers,idle_timeout,ping_timeout}` `ws{path,headers,max_early_data,
early_data_header_name}` `grpc{service_name,idle_timeout,ping_timeout,permit_without_stream}`
`httpupgrade{host,path,headers}` `quic{}`（无字段）。

## 7. `experimental`（`option/experimental.go`）

- `cache_file`：`enabled` `path` `cache_id` `store_fakeip` `store_dns` `rdrc_timeout`
  （`store_rdrc` 已 omit）。
- `clash_api`：仍然存在 — `external_controller` `external_ui` `external_ui_download_url`
  `external_ui_download_detour` `secret` `default_mode` `access_control_allow_origin[]`
  `access_control_allow_private_network`。
- `v2ray_api`：`listen` `stats{enabled,inbounds,outbounds,users}`。
- `debug`：`listen` `gc_percent` `max_stack` `max_threads` `panic_on_fault` `trace_back`
  `memory_limit` `oom_killer`。
- 另有 `services[]` + `option/api.go` 的新 API 服务（`dashboard` 等），但它的 `type` 字符串
  要靠 `box/` 注册表确认，而本地留档只含 `option/`，**用到前必须在 VM 上实测**。

## 8. 必须去 VM 上实测才能定案的项

> 2026-09-30：第 1、2 项已定案（见 §11），第 3 项已在 netns 拓扑逐条实测定案（见 §11.7），
> 第 4、5 项仍未定（第 5 项是 Ubuntu ops，本环境是 Alpine）。

1. `transport.type:"tcp"` 是否真的被拒。→ **已定案：真被拒**，`unknown transport type: tcp`，
   整个 `transport` 键省略。见 §11。
2. `urltest`/`selector` 的注册与默认值。→ **已定案**：`sing-box schema` 里
   `urltest` 的 properties 恰好是 `type tag outbounds url interval tolerance
   idle_timeout interrupt_exist_connections`，**没有 `default`**；
   `selector` 是 `type tag outbounds default interrupt_exist_connections`——`default` 只有它有。
   两者仍是 `outbounds[]` 里的出站类型（顶层 `endpoints` 只有 openconnect/openvpn-client/
   openvpn-server/tailscale/wireguard）。见 §11。
3. TPROXY 的 `ip rule fwmark` / `ip route local` / nftables 规则：**1.14.2 的 tproxy 文档
   完全不讲防火墙与路由**（`inbound/tproxy.md` 只有 41 行：type/tag/Listen Fields/network/
   UDP-NAT 三项 + "Only supported on Linux"）。这部分只能来自我们自己验证过的模板，
   不能声称来自官方文档。sing-box 侧只负责按 `routing_mark` 给出站打标记。
   → 模板已落成 `src/gen/nft.rs` + `src/gen/netstate.rs`，取值见 §9。
   → **2026-09-30 定案（§11.7）**：DNS redirect、tcp/udp 两条 tproxy 的原始目的地保留、
   `ip rule fwmark 0x1 table 100`、Bypass 的 `return` 作用域、卸载与重复下发幂等，全部在
   netns 拓扑里拿到了正面证据。唯一没被证实的是 `rp_filter=2` 的必要性（见 §9 的改写）。
4. 官方文档现在明确推荐 TUN + `auto_redirect`（`tun.md:346-352`：路由更好、性能优于 tproxy、
   避开 TUN/Docker 网桥冲突），并由 sing-box 自己下 nftables（默认
   `auto_redirect_input_mark:"0x2023"` 等）。ZBox 当前按 TPROXY 走，若 VM 实测发现旁路由
   场景下 auto_redirect 更稳，可整体替换转发层而不动面板。
5. Ubuntu 的 systemd-resolved `127.0.0.53:53` stub 与 sing-box 抢 53 端口（ops 问题，非 schema）。

## 9. ZBox 自证的劫持层取值（**不是官方文档**，落地在 `gen/nft.rs` + `gen/netstate.rs`）

两个 fwmark 必须不同值，这是整套设计里最容易自己踩自己的地方：

| 常量 | 值 | 谁打 | 用途 |
|---|---|---|---|
| `DIVERT_MARK` | `0x1` | nft 的 `tproxy ... meta mark set 0x1` | 被 `ip rule` 引进 table 100 才会交给本机 tproxy 入站 |
| `BYPASS_MARK` | `0x2` | sing-box `route.default_mark` | 本机自己出去流量打标，nft 里 `meta mark != 0x0 return` 据此排除，防二次劫持 |

主机侧（`netstate.sh`，先 `del` 后 `add` 保证可重复执行）：
```
sysctl -w net.ipv4.ip_forward=1
sysctl -w net.ipv4.conf.{all,default,<lan>}.rp_filter=2   # 宽松模式；必要性见 §11.7 的负向实验
sysctl -w net.ipv4.conf.all.send_redirects=0
ip -4 rule add fwmark 0x1 table 100 priority 100
ip -4 route replace local default dev lo table 100
```

nftables：`table ip zbox`（**只做 IPv4**，IPv6 暂不劫持 —— 已知取舍，见下），
文件头 `table ip zbox { }` + `flush table ip zbox` 再重建，因为 `nft -f` 对已存在的链是追加语义。

- `chain dns`：`type nat hook prerouting priority dstnat`，`udp/tcp dport 53 redirect to :<ports.dns>`。
  匹配的是客户端查的 53，重定向目标才是我们配置的监听口，两者不能混。
- `chain divert`：`type filter hook prerouting priority mangle`，先 `meta mark != 0x0 return` /
  `iifname != "<lan>" return` / `ip saddr != <lan_cidr> return`（Bypass 设备再单列一条 return），
  然后 `fib daddr type local return`（保护面板与显式代理端口）、组播/广播 return、DHCP(67/68) return、
  `udp/tcp dport 53 return`（已被 nat 链接管），最后 tcp/udp 各一条 `tproxy to :<tproxy> meta mark set 0x1 accept`。
- 卸载 = `ip rule del` + `ip route flush table 100` + `nft delete table ip zbox`，全部 `|| true`
  （脚本是 `set -eu`，不容错会在"本来就没装过"时中断）。

**已知取舍**：IPv6 不在劫持范围内，局域网若同时有 v6 路由，境外 v6 站点会直连（漏代理，不断网）。
VM 实测确认后再决定是否补 `table ip6 zbox`。§11.7 的实测全部是 IPv4，这条取舍仍未定案。

**可诊断性缺口**（§11.7 撞到过一次）：这份模板里没有任何 `counter`，所以"客户端的包没被劫持"
在面板上分不清"包没进链"和"进链后被某条 return 放行"。实测时的办法是临时
`nft insert rule ip zbox divert iifname "<lan>" ip saddr <client> counter`（insert 才是链首，
`add` 只会追加在 return 之后、永远数不到）。要么模板里给一条带 counter 的诊断形状，
要么让 doctor 直接生成这条插入命令。

## 10. 规则集编译（`sing-box rule-set`）

证据取自 tag `v1.14.2` 的源码 `cmd/sing-box/cmd_rule_set_compile.go` 与 `option/rule_set.go`
（2026-09-28 经 api.github.com 原文核对，非文档转述）。
本节只管**命令行形状**；抓取、验收、顶掉现役产物、清单落盘那一条链在真二进制上的表现见 §11.8.9。

命令形状：

```
sing-box rule-set compile <source-path> [-o <out>.srs]
```

- `Args: cobra.ExactArgs(1)` —— 只接受**一个**位置参数；`-o/--output` 缺省时按
  `<file_name>.srs` 规则从输入名推导。
- 位置参数可以写字面量 `stdin`，源码里显式 `if sourcePath == "stdin"` 走标准输入。
  ZBox 用 `rule-set compile stdin -o rule-set/<tag>.srs`，避免把外部内容落成可预测的临时文件名。
- 编译失败时源码会 `os.Remove(outputPath)`，**不会留下半截 `.srs`**。

输入 JSON 的硬约束（`option._PlainRuleSetCompat`）：

- 顶层 `version` 必填，枚举 `1,2,3,4,5`；缺失时报 `missing rule-set version`。
- 五个版本都用同一套 `PlainRuleSet{ rules: [HeadlessRule] }` 结构解析，
  所以 ZBox 统一产 `version: 5`，由 `downgradeRuleSetVersion` 按实际用到的特性自动降版本
  （纯域名名单会降到很低的版本，产物更小、兼容性更好）。
- `HeadlessRule` 的键：`domain` `domain_suffix` `domain_keyword` `domain_regex`
  `ip_cidr` `source_ip_cidr` `query_type` `network` `port` `source_port` `process_*` `package_name*`
  ……全部 `omitempty`。**同一条 rule 里写多个键是"与"**，所以四类域名匹配必须拆成四条 rule
  （与 §4 路由规则同一个坑）。

实测补三条 CLI 形状（2026-09-30，1.14.2，用 §11.7 那批 fixture `.srs` 跑）：

- `compile <path>` 的位置参数写字面量 `stdin` 时配合 shell 重定向可用，`-o` 指哪儿写哪儿。
- `decompile <file>.srs` **不写标准输出**，而是在当前目录生成同名 `<file>.json`，脚本要按文件名读。
  顺带能看到降版本：纯 `domain_suffix` 的 version 5 源，编出来再反编译是 `"version": 2`。
- `match` 的默认 `--format source`，拿它读二进制会 `FATAL invalid character 'S' looking for
  beginning of value: row 1, column 1`（rc=1）。验收必须写
  `sing-box rule-set match --format binary <file>.srs <域名|IP>`。
- **`match` 没命中也返回 rc=0**。命中信息在 **stderr** 上、`stdout` 恒空（2026-10-05 逐字复抓推翻
  了本节原来"stdout 才是判据"那句）：命中时一行 `match rules.[0]: domain/domain_suffix=<binary>`
  （IP 规则集是 `ip_cidr=<binary>`，形状本轮也真跑出来了，见下面那条推论的订正），
  未命中是空 stderr。所以验收判据是**合并输出**里有没有 `match rules.`（`Exec::merged()` = stdout+stderr），
  判 rc 会把"没编进去"当成通过，只读 stdout 会把每一次命中都当成失败。
- **`ip_cidr=<binary>` 那条推论已复抓，但结论反而更硬**（2026-10-05，`geoip-cn.srs`）：拿成员地址去问
  （`1.2.4.8`、`223.5.5.5`）**命中**；拿表里确实存在的**段本身**去问（`1.2.4.0/24`）**不命中**——
  待匹配值必须是一个地址，不是 CIDR。所以 `svc::ruleset::probe_value` 仍然**不**对纯 IP 源做内容级验收，
  理由从"形状没逐字验过"换成了实测的第二条：我们无从预知任何一份 CIDR 名单里的成员地址
  （用户自建的源更是），而猜错的探针只会造出"没命中"的假阳性。域名侧可以，因为 `www.baidu.com`
  这种值在任何 `domain_suffix: baidu.com` 形状的源里都在。
  跨类型探针（域名问 IP 表、IP 问域名表）也是空输出而不是报错，所以探针必须与源的形状配对。

## 11. VM 实测定案（2026-09-29，Alpine 3.21.3 / sing-box 1.14.2 `-musl` / VMware NAT）

命令形状：`sing-box run -c <绝对路径> -D <运行目录>`。`-D` 会**改 CWD**，所以 `-c` 写相对路径
会得到 `open config.json: no such file or directory`；`initial_path`、`cache_file.path` 这类相对
路径都以 `-D` 的目录为基准。

### 11.1 `check` 放行、只有 `run` 才炸的两条

这几条是这套表里最危险的一类，因为生成的配置在 CI 里看起来是干净的：

1. `detour` 指向 `direct` 这个空出站 -> `detour to an empty direct outbound makes no sense`。
   DNS 服务器和 `http_clients` 都拒（源码里只有废弃的 `download_detour` 路径会把
   `DisableEmptyDirectCheck` 置真）。**正确写法是整个省略 `detour`**——不写才是"走默认路由的
   直连"，写了 `direct` 反而是错误。实测：`check` rc=0，`run` 才 FATAL。
2. 远端规则集没有显式 `http_client` -> 启动只 WARN，但它实际把下载绑到**默认出站**，
   也就是策略组；下载一失败就是 FATAL。见 11.3。

### 11.2 被 `check` 直接拒掉的写法（复核 §2）

- **缺 `route.default_domain_resolver`（真实配置上）** ->
  `missing 'route.default_domain_resolver' or 'domain_resolver' in dial fields is deprecated in
  sing-box 1.12.0 and will be removed in sing-box 1.14.0`，紧跟
  `FATAL to continuing using this feature, set environment variable
  ENABLE_DEPRECATED_MISSING_DOMAIN_RESOLVER=true`。
  这条**不指名是哪个拨号字段**，`check` 也没有 `-v` 可问；实测 54 节点订阅配置删掉该键
  = `check` rc=1 且 `run` FATAL，把全部节点出站地址换成 IP 也照样 rc=1（真正触发它的组合
  没有定位到单个键，别再花时间猜）。逃生阀环境变量只是把 FATAL 降成 WARN，不是放行。
  反过来：**上游全是裸 IP、没有远端规则集的最小配置可以不写这个键**（实测 check rc=0 且
  `run` 正常起来）。结论是生成器一律输出，不试图省。
- **地址是域名的 DNS 上游没有自己的 `domain_resolver`** ->
  `initialize DNS server[n]: missing domain resolver for domain server address`，
  `check` rc=1 与 `run` FATAL 都报。**`route.default_domain_resolver` 救不了它**：实测里
  `route.default_domain_resolver` 已经指向一个地址为 IP 的上游，域名地址的 DoH 上游照样失败。
  引导链必须写在 `dns.servers[].domain_resolver` 上，并最终落到一个地址为 IP 的上游
  （或 `type:"local"`）。
- `transport.type:"tcp"` -> `unknown transport type: tcp`（§8 第 1 项定案：省略整个 `transport`）。
- `urltest` 上的 `default` -> `outbounds[5].default: unknown field`（§8 第 2 项定案）。
- Reality 的 `public_key` 不是合法 X25519 公钥 -> `initialize outbound[1]: invalid public_key`。
  占位值过不了，测试用的密钥对必须真生成。

### 11.3 远端规则集的下载路径（决定 task #9 的形状）

WARN 原文：`implicit default HTTP client using default outbound for remote rule-sets is
deprecated in sing-box 1.14.0 and will be removed in sing-box 1.16.0.`

它不是"以后要改"的建议，是现在就有的功能差异：54 节点的真实订阅配置里 `route.final` = 代理组，
四条 `.srs` 全部 `unexpected HTTP response status: 403` 并 **启动期 FATAL**（机场出口 IP 被
GitHub 拒），而同一个 URL 在 guest 里用 wget 直连是 `200 / 56190 字节`。

定案下来的形状（改完实测：4/4 下载成功、`sing-box started`）：

```json
"http_clients": [ { "tag": "rule-set-downloader" } ],
"route": { "rule_set": [ { "type": "remote", "http_client": "rule-set-downloader", ... } ] }
```

- 必须是**引用一个 `http_clients` 里的 tag**。内联 `"http_client": {}` 不算显式：
  `resolveTransport` 用 `HTTPClientOptions.IsEmpty()` 判断，空对象照样落回隐式客户端，WARN 照旧。
- 该条目**不写 `detour`**（11.1 第 2 条）。实测它能在 `route.final` 指向一个必然失败的代理组时
  仍然 6 秒下载成功，说明它真的绕开了默认出站。
- 它解析 URL 主机名用的是 `route.default_domain_resolver`，实测能拿到正确答案，所以不需要单独
  给它配 `domain_resolver`。

11.3.1 三配置的验收跑（同一台 guest、同一个 1.14.2，命令 `check` 然后 `run`）

| 配置 | `route.final` | `http_clients` | 结果 |
| --- | --- | --- | --- |
| baseline（全直连、无规则集） | `direct` | 不输出这个键 | `check` rc=0，`sing-box started (0.01s)` |
| full（自建节点 + 4 条远端规则集） | 代理组 | `[{"tag":"rule-set-downloader"}]` | `check` rc=0，四条全部 `INFO router: updated rule-set <tag>`，`started (1.40s)` |
| sub（54 节点真实订阅） | 代理组 | 同上 | `check` rc=0，四条同样全部 `updated`，`started (0.52s)`，urltest 逐个节点探 `www.gstatic.com:80` 无 ERROR |

三份日志里 **没有任何 `implicit default HTTP client` WARN**（修好之前的形状必然带它），
也没有 ERROR；剩下的 WARN 只有 11.4 那条"没预置 `.srs`"。验收判据按这两条写：
`grep -c 'updated rule-set'` = 规则集条数，`grep implicit` 必须为空。

### 11.4 `initial_path` 与 `cache_file` 的分工（离线安装的依据）

- `initial_path` **只读不写**：下载成功后 sing-box 从不往那儿落盘。实测跑完
  `run/rule-set/` 目录是空的，只有 `cache.db`（524KB，装着 4 条规则集）。
- 全新机器（`initial_path` 上没有文件）的启动日志里，每条远端规则集都会先给一行
  `WARN router: load initial rule-set from <绝对路径>: open <绝对路径>: no such file or directory`，
  然后照常走下载。它是**预期噪声**，不是失败——但它是"启动期依赖网络"的明示：
  那 4 行 WARN 之后如果跟着 `unexpected HTTP response status` 才是真故障（11.3）。
  路径是 `-D` 目录 + `initial_path` 拼出来的绝对路径，所以安装器放文件要按运行目录算。
- 有 `cache.db` 的第二次启动：`sing-box started (0.01s)`，零下载、零 WARN。所以
  `experimental.cache_file.enabled` 是重启不依赖网络的前提，不能关。
- 预置过 `initial_path` 文件的启动：这三条不再报 WARN，sing-box 仍然会在启动阶段尝试更新，
  但那时失败只是 `ERROR fetch rule-set ...` 而**不是 FATAL**。
  → **ZBox 安装器把 `.srs` 放到 `initial_path` 上 = 全新离线机器也能起来**，这是 task #9 的目标形状。
- 别把"能下载"当稳定前提：guest 直连 Fastly 段也出现过
  `read tcp ...->185.199.109.133:443: read: connection reset by peer`。
- **`initial_path` 的字符串完全由 ZBox 决定，所以必须钉在 tag 上**（`rule-set/<tag>.srs`）。
  sing-box 只按 `-D` + 这个字符串去 `open`，它不关心上游叫什么名；而内置项里 `non-cn` 的上游
  文件名是 `geosite-geolocation-!cn`，**两者不同**。照抄文件名时 `check` 照样 rc=0，冷启动才
  现形（就是上面那几行 WARN + §11.7 的预算）。URL 仍写上游原名（§8 实测：`!` 写成 `%21` 是 404），
  改的只是本地落点。2026-09-30 接 task #9 写落地路径时撞见这个分歧，
  已由 `DesiredState::validate` 拦住：Remote 的 `initial_path` 必须等于 `rule-set/<tag>.srs`，
  tag 限定在文件名安全字符集内（`!` 不在内）、不得重复。
  *状态*：**tag 命名那一半已在 2026-10-05 的 v9 预跑里起过一次**（空 `inbounds` 候选 +
  独立 `-D`，四条远端集全部按 `rule-set/<tag>.srs` 找文件：日志先给四行
  `WARN router: load initial rule-set from .../rule-set/non-cn.srs: ... no such file or directory`，
  随后 8.80s 内四条 `updated rule-set` 齐、`sing-box started`，零 ERROR/FATAL，跑完
  `out-prerun/rule-set/` **目录都不存在**——本节"只读不写"再加一次独立复证，也复证了
  `http_clients:[{tag:"rule-set-downloader"}]` 不写 `detour` 就是直连下载、没走代理组）。
  仍然没验的是"**预置过** tag 命名文件"的冷启动，那条要靠 `zboxd rule-sets` 真的把产物放到
  `<root>/rule-set/<tag>.srs` 上再启一次。→ **已补，见 §11.12**（2026-10-07 那轮用现役目录的 `.srs`
  只读复制进全新 `-D`，`started (0.04s)` 且 started 之前零 `load initial rule-set` WARN）。

### 11.5 上游形态复核

DoH 用 **IP 地址** + `tls.server_name` 写成同一个 IP 是能工作的（223.5.5.5 的证书带 IP SAN；
busybox wget 直连 `https://223.5.5.5/dns-query` 拿到 400 而不是证书错误）。把 `server_name`
换成别的域名就查不出结果——证书校验是真生效的，没有被跳过。ZBox 默认的国内上游就是这个形状。

同族子命令（`cmd/sing-box/cmd_rule_set_*.go` 实际存在）：
`compile` `decompile` `format` `match` `merge` `convert` `upgrade`。
VM 实测时用 `rule-set match` 验证编译产物真的含目标域名，比数文件大小可靠。

### 11.6 下发前的预跑门（不需要停现役内核）

11.1 那两条是 `check` 抓不到的，所以下发流程必须真跑一次 `run`。但现役内核占着 :53/:9888/:9887/:8888/:7890，
直接 `run` 候选配置就是端口冲突。实测结论：**把 `inbounds` 换成 `[]` 再 `run` 就够了**——

- `"inbounds": []` 时 `check` rc=0、`run` 也能走到 `sing-box started`（4.11s），
  而 DNS 引擎初始化、4 条规则集下载、出站初始化（urltest 照样对每个节点探
  `www.gstatic.com:80`，日志里能看到 3 条 `outbound connection`）全都执行过了。
- 拿它抓 11.1 第 1 条是有效的：给 `http_clients[0]` 加上 `detour:"direct"`，
  `check` 仍然 rc=0，空 inbounds 的预跑 `run` 立即 rc=1 +
  `FATAL start service: (initialize rule-set[3]: ... detour to an empty direct outbound makes no sense | ...)`
  （四条规则集的错误被合并在一条 FATAL 里报出来）。
- 预跑要用**独立的 `-D` 临时目录**：它会真下载规则集并写 `cache.db`，不能指到现役的运行目录上。
- 判据同 11.3.1：日志里有 `sing-box started` 才算过。

代价：预跑要联网、约 1.5–4s。换来的是"候选配置有 run-only FATAL 时现役内核完全不动"，
比"先停再试、失败回滚"少一次断流。端口占用与 nft/TPROXY 命中仍然只能靠切换后的探测（§5 第 5 步）。

顺带一条由这个实验暴露出来的事实：**sing-box 从不校验 `route.rules[].inbound` 指向的入站是否存在**——
`inbounds: []` 配上引用 `dns-in`/`tproxy-in` 的规则照样 `started`。写错 tag 就是一条永不命中、
毫无征兆的规则，内核不会报错，只能由 ZBox 生成侧自己断言（`gen::singbox::mod` 有对应测试）。

### 11.7 netns 局域网实测（2026-09-30，§8 第 3 项与回滚/幂等定案）

拓扑（脚本在 `.tmp/netns/topo-up.sh`，全程不碰物理 LAN）：宿主命名空间里建桥 `zbox-lan0`
= 10.99.0.1/24 当"局域网网卡"，两个 netns 客户端 —— `lan1`=10.99.0.2（被劫持）、
`lan2`=10.99.0.3（配成 Bypass，天然是对照组）。WAN 仍是 `eth0`（VMnet8）。
另建了一张**不属于产品产物**的 `table ip zboxtest` 做 masquerade：VMnet8 只有 guest 一个出口，
客户端绕过劫持层的流量必须 SNAT 才能出网，这张表正好用来证明"Bypass 客户端照常上网"。

这一轮把规则集全换成**本地 `.srs`**（`type:"local"` + `path:"rule-set/<tag>.srs"`，用 §10 的
`rule-set compile stdin -o` 现场编）。原因是同一天早上远端规则集在 netns 拓扑下真炸了一次，
`/root/zbox-test/plain/sb.log` 原文（时间戳是真实间隔）：

```
09:12:17 DEBUG dns: lookup succeed for raw.githubusercontent.com: 185.199.109.133 ...
09:12:27 WARN router: initialize rule-set take too much time to finish!
09:12:33 FATAL[0016] start service: (initialize rule-set[3]: initial rule-set: geoip-cn:
  Get "https://raw.githubusercontent.com/...geoip-cn.srs": context deadline exceeded |
  initialize rule-set[2]: ... lookup raw.githubusercontent.com: (exchange6: context canceled |
  exchange4: context canceled) | initialize rule-set[1]: ... | initialize rule-set[0]: ...)
```

三条结论：**(a)** 规则集初始化有硬预算，WARN 在 10s、整体 FATAL 在约 16s；**(b)** DNS 解析成功
并不等于能启动，炸的是随后的 GET；**(c)** 启动期只要有一条远端规则集拉不动就是 **FATAL**，
不是降级运行。这把 §11.4 的"安装器预置 `.srs` 到 `initial_path`"从优化**升为必需**（task #9）。

三类**正证据**（这个环境里"境外站点打不开"不能当判据：关掉劫持层、连 sing-box 都不跑，
VMnet8 出去 google 也打不开，所以只有下面三类是有效的）：

| 判据 | 实测 |
|---|---|
| DNS 劫持 | lan1 向 **8.8.8.8** 查询，日志先给 `inbound/direct[dns-in]: inbound packet connection to 0.0.0.0:53`，再给 `dns: match[3] rule_set=geosite-cn => route(domestic)`，答案是 domestic DoH 给的；同一时刻 lan2 拿到的是真实 8.8.8.8 的答案（CNAME 链到 `www.wshifen.com`）。**劫持层已装载但内核没起**时 lan1 查询直接 `nslookup: write to '8.8.8.8': Connection refused`，lan2 不受影响 —— 这正是 §5 里"下发成功但什么都没代理"那个形态在 DNS 侧的表现，doctor 判 Fatal 是对的。 |
| TPROXY 命中 | `inbound/tproxy[tproxy-in]: inbound connection from 10.99.0.2:52182` → `inbound connection to 180.101.51.73:80` → `outbound/direct[direct]: outbound connection to 180.101.51.73:80`：原始目的地完整保留，分流判决落在 direct，HTTP 拿到 2381 字节。UDP 分支同形状：对 `114.114.114.114:5353` 发一个 UDP 包，日志 `inbound packet connection to 114.114.114.114:5353` → `outbound/vless[vless-reality]`（非 CN 目的地按 `route.final` 走代理组）。**测 udp 的 tproxy 规则必须换端口号**，53 的包在 nat 链就被接管了。 |
| Bypass 作用域 | 在两条链**链首**临时插 `ip saddr 10.99.0.3 counter`（必须 `insert`，`add` 只会追加到 return 之后、永远数不到）：计数器记到 3/10 个包 —— 包确实进了链；sing-box 日志里 `10.99.0.3` 出现 **0** 次 —— 是被 `return` 放行而不是没走到。 lan2 的 DNS/HTTP 行为与内核装卸完全无关。 |

回滚与重复下发（§8 第 5 项）：

- `netstate-down.sh` 跑完：`nft list table ip zbox` → `Error: No such file or directory`、
  `ip -4 rule` 只剩 local/main/default、table 100 空，而脚手架的 `zboxtest` 表**原样还在**
  （"只删自己建的东西"是断言，不是愿望）。
- **sysctl 不回滚**：`ip_forward=1`、`rp_filter=2` 原地留着。`render_teardown()` 本来就没写回滚，
  实测确认这就是当前行为。要不要回滚留给 svc 层定案（倾向不回滚：转发开着无害，
  关掉反而可能影响别的东西）。
- 卸载后 lan1 的 HTTP 仍然 200/2381 字节、DNS 回到真实 8.8.8.8 的答案 —— 回滚的语义是
  "退回纯路由"，不是断网；重新装载后劫持恢复。
- **幂等实测**（`netstate.sh` + `nft -f zbox.nft` 各跑两遍）：tproxy 规则 2 条（不是 4）、
  `redirect to :53` 2 行、`fwmark 0x1` 1 条、table 100 1 条路由。`zbox.nft` 开头那句
  `table ip zbox { }` + `flush` 和 netstate 的先删后加，两个设计都按预期生效。

FakeIP 语义（`netns-local-fakeip` 快照）：**`dns.rules` 的 `action:"route"` 是终止语义**，
所以末条 `query_type:[A,AAAA] => fakeip` 只接管前面没命中的域名。这个判据在这个拓扑里是二值的
（代理组是死节点），实测结果：

- `www.baidu.com` → `match[3] rule_set=geosite-cn => route(domestic)` → 真实 IP →
  `outbound/direct` → HTTP 200。
- `www.google.com` → `match[5] query_type=[A AAAA] => route(fakeip)` → `198.18.0.2` / `fc00::2`
  → 命中 `route.rules` 末条 `ip_cidr:[198.18.0.0/15, fc00::/18]` 短路 → `outbound auto`。

也就是 §6 说的"fakeip 段补一条 ip_cidr 短路"的形状成立，且国内域名没有被 fakeip 污染。
`experimental.cache_file.store_fakeip=true` 的映射写在 `-D` 目录的 `cache.db`（实测 32KB）。
路由末条里的 `fc00::/18` 在只劫持 IPv4 的模板下拿不到流量，属于 §9 那条已知取舍的一部分。

**路径基准（svc 层必须照这个实现）**：`rule_set[].path`（local）、`initial_path`、
`cache_file.path` 全部相对 **`-D`** 解析，而 `-D` 缺省就是 CWD。实测踩过两次：
`run -D <dir>` 时报 `open <dir>/rule-set/geosite-cn.srs: no such file or directory`，
而 `check -c <绝对路径>` 不带 `-D` 时报 `open /root/rule-set/geosite-cn.srs: ...` ——
两者基准一致，所以**校验也要带 `-D`**，否则会造出"guest 跑得起来、CI 里 check 说没文件"的假故障。
由此定下的目录布局：**快照目录不可变，`-D` 必须是可写状态目录**，`.srs` 随快照复制进去、
`cache.db` 写在那里、`-c` 用绝对路径。预跑门（§11.6）用**另一个**独立状态目录，实测两者可以并存：
现役内核在跑，预跑 `sing-box started (0.06s)`、日志里 `inbound/` 行数为 0（一个端口都没监听）、
两边各写各的 `cache.db` 互不污染。

`rp_filter` 的断言被削弱（§9 已按此改写）：把 `net.ipv4.conf.all.rp_filter` 和
`...conf.zbox-lan0.rp_filter` 从 2 压回 1（严格模式），tproxy 命中、`outbound/direct`、
HTTP 2381 字节**全部照常**，`nstat IpReversePathFilterFailure` 也没给出可读计数。
原因是这个拓扑里局域网与 WAN 分属 `zbox-lan0`/`eth0`，反向路径查表天然对称，
严格模式无从判 spoofed。**"单网卡旁路由必须 2"这句既没被证实也没被证伪**：要证它得把
客户端与出网放在同一张网卡上，那只能在真实单网卡机器上测。当前 `sysctl_pairs()` 继续
输出 2（放宽是无害方向），但文档不再声称它被实测证明过。

仍未定案：IPv6 劫持（§9 取舍）、TUN + `auto_redirect` 的对比（§8 第 4 项）、
Ubuntu 的 systemd-resolved 抢 53（§8 第 5 项，本环境是 Alpine）。

### 11.8 官方 release 资产与二进制落地判据（2026-09-30，task #11 的依据）

来源不是推测：`api.github.com/repos/SagerNet/sing-box/releases/tags/v1.14.2` 的资产表原文，
包内布局用**范围请求 + 流式解 tar** 取的（只下前 64KB，不搬 31MB）：
`curl -sSL -r 0-65535 <浏览器下载 URL>`（HTTP 206）后接 `gzip -dc | tar -tv`。

- URL 形状：`https://github.com/SagerNet/sing-box/releases/download/v1.14.2/<资产名>`。
- 资产表里**没有 `.sha256` 条目**（逐条查过）。所以落地验收只能是功能性的
  （真跑 `<暂存件> version` 比对版本），ZBox 不假装有校验和。
- (架构 × 变体) 矩阵——`-glibc`/`-musl` 只在这些架构上存在，别的只有裸名：
  - `386`：glibc、musl、裸名、softfloat
  - `amd64` `arm64` `armv7` `loong64` `riscv64`：glibc、musl、裸名
  - `armv5` `armv6` `ppc64le` `s390x`：只有裸名
  - ZBox 只自动处理 `x86_64->amd64` 与 `aarch64->arm64`，别的架构**明确报错**让人手工放二进制
    （没有实测环境可以验收，就不闭着眼睛下发）。
- amd64 三条的字节数：裸名 31,680,686；`-glibc` 32,500,990；`-musl` 32,525,286。
- 包内布局（`-musl` 实测）：**只有一个顶层目录**，名字 = 资产名去掉 `.tar.gz`：
  ```
  sing-box-1.14.2-linux-amd64-musl/LICENSE   791
  sing-box-1.14.2-linux-amd64-musl/sing-box  91,951,072  -rwxr-xr-x
  ```
  所以解包用 `tar -xzf <包> -C <暂存>` 再取 `<顶层目录>/sing-box`，不对 tar 的改名行为
  做任何假设（`--strip-components` 那条路一旦包内布局变了就静默拿到错文件）。
  tar 的 mode 已带 0755，"用户直接给可执行文件"那条路靠 `make_executable` 补。
- `sing-box version` 的输出形状（第一行才是版本号，取版本只看它）：
  ```
  sing-box version 1.14.2
  Environment: go1.26.8 linux/amd64
  Tags: with_vision,with_musl,…
  Revision: af6e64c3b69e6132ebaee0e1a3d24e93903f6709
  CGO: enabled
  ```

#### 11.8.1 guest 侧与抓取有关的实测

- Alpine 3.21.3 x86_64：`/lib/ld-musl-x86_64.so.1` 在，**`/lib64` 不存在**。glibc 侧的对应物是
  `ld-linux-x86-64.so.2`。这就是 `target_from` 的两个判据；判不出必须报错，不许退到裸名包
  （裸名是 glibc 动态链接的，实测在 Alpine 上跑不起来）。
- busybox `tar` 1.37.0：`-x -z -f -C` 都支持。
- 有 `wget`（busybox，连 `--version` 都不支持），**没有 `curl`**。
- **被推翻的结论：在线抓取 31MB 二进制在这套环境里不可行。**
  `wget -q -T 60 <musl 资产>` 跑了 `2m 48.47s` 后报 `wget: download timed out`；
  分档探测 15/30/45/60 秒只拿到 655KB/1.18MB/1.84MB/2.00MB（约 33KB/s）然后 `Read timeout`。
  也就是说 `-T 60` 这种"看起来给了余量"的设置在这里是**白等 168 秒**。
  结论落到设计上：provision 以离线资产（`--asset`，scp 过来即可）为主路，在线抓取留作
  链路好的机器上的可选路径，并且 **curl 用总时长 900s、wget 用空闲上限 20s**——
  两个下载器的"超时"语义不同（`--max-time` 是整条传输的上限，`-T` 是网络空闲上限），
  同一个数字不是一回事，所以 `FetchLimits` 拆成两个字段，不复用名单抓取那档的 60。

#### 11.8.2 离线资产在真机上落地，与"不停服务换二进制"（2026-09-30，task #11 收口）

在 Alpine 3.21.3 guest 里把 `src/svc/provision.rs` 的三条调用形状逐条重放了一遍。
全程只动 `/root/zbox-provision-test/`，`/usr/local/bin/sing-box` 只被读（当替身的来源），没被写。

- 资产传输：宿主机 `.tmp/sb-musl.tar.gz` → `scp` → guest，两端 `sha256sum` 都是
  `8f6cb4bcf94d2b33c65d52e0d5b142db29a938336f1ff7267f397ac3758fc297`，长度 32,525,286，
  与 §11.8 记的 amd64 `-musl` 字节数一致。一条 scp 就到位——这就是"离线为主路"的全部依据。
- `tar -xzf sing-box-1.14.2-linux-amd64-musl.tar.gz -C staged`（busybox tar 1.37）产出：

  ```
  staged/sing-box-1.14.2-linux-amd64-musl/LICENSE    791       -rw-r--r--  1001:1001
  staged/sing-box-1.14.2-linux-amd64-musl/sing-box   91951072  -rwxr-xr-x  1001:1001
  ```

  两条此前只在宿主机上"流式解 tar 推出来"的说法就此定案：**顶层目录名 = 资产名去掉 `.tar.gz`**
  （`member_dir()` 取的就是它），以及 **tar 的 mode 自带 0755**（所以 `land()` 走 rename 那条
  分支不需要 `make_executable`；那个补位只服务"用户把可执行文件直接交给 `--asset`"的拷贝路）。
  没预料到的一条：解出来的属主是包里的 **uid 1001**，不是 root，rename 顶上之后现役路径上就
  留着这个属主。root 起的服务执行它没有障碍，ZBox 不改（改了等于抹掉"这就是官方原件"的痕迹）。
- 验收 `<暂存>/sing-box version` → rc=0，第一行 `sing-box version 1.14.2`，`Tags:` 里有
  `with_musl`——libc 的判断与"这份在这台机器上真跑得动"就此缝在一起。
  反面**没有**实测：裸名（glibc 动态链接）那份在 Alpine 上跑不动，是从链接方式加
  `/lib64` 不存在（§11.8.1）推出来的，手上没有那份资产就没验。这恰恰是 `verify()` 存在的理由：
  推断不许直接上线。
- **rename 顶掉正在运行的内核是安全的，所以换二进制不必先停服务**：

  ```
  bin/sing-box run -c <绝对>/run/min.json -D <绝对>/run      # inbounds: [] 的最小件
  +0800 2026-09-30 15:37:48 INFO sing-box started (0.00s)
  293 -rwxr-xr-x 1 root root 91951072 ... bin/sing-box        # 正在跑的那份
  mv run/replacement bin/sing-box                            # 顶换（替身只差一字节，纯为可辨）
  296 -rwxr-xr-x 1 root root 91951073 ... bin/sing-box        # 同一路径，换了 inode
  /proc/11125/exe -> /root/zbox-provision-test/bin/sing-box (deleted)
  kill -0 11125 → alive=yes；日志自 started 之后零新增行
  ```

  进程继续握旧 inode（POSIX rename 语义），现役转发不断；新二进制生效仍然靠 §5 的 restart。
  这和 §11.6"预跑门不必停内核"是同一类收益：**换件与换配置都不该制造第二次断流**。
- **`-D` 是所有相对路径的基准，`-c` 自己也在其内**（§11.7 只对 `rule_set`/`cache_file` 说过）：

  ```
  cwd=/root/zbox-provision-test，ls 看得见 ./run/min.json
  sing-box check -c run/min.json -D run
  FATAL[0000] read config at run/min.json: open run/min.json: no such file or directory
  ```

  `-D run` 时它实际找的是 `<cwd>/run/run/min.json`。落到代码：`Validator::gate` 和 systemd/OpenRC
  制品里的 `-c` 一律给绝对路径，`zboxd` 那条"`--root`/`--prefix` 必须是绝对路径"的断言是这个
  不变量的前提，不许放宽。
- 顺手把 §2 表里那条 DNS 映射的**报错原文**钉下来（1.14.2 对旧形状是硬失败，`check` 与 `run` 都拒）：

  ```
  FATAL[0000] decode config at .../min.json: dns.servers[0]: legacy DNS server formats are
  deprecated in sing-box 1.12.0 and removed in sing-box 1.14.0, checkout migration:
  https://sing-box.sagernet.org/migration/#migrate-to-new-dns-server-formats
  ```

  生成侧已是新形式（`gen::singbox::dns` 出 `{"type":"udp","tag":..,"server":..}`），
  这条留作"写错就内核拒"的判据。

#### 11.8.3 `log.output`：路径基准、追加语义，与"`check` 根本不看它"（2026-09-30，OpenRC 侧日志可见性）

动机：这台 Alpine 的 OpenRC **拿不到启动失败**。遍查 `/sbin/openrc-run`、`/etc/init.d/functions.sh`、
`/lib/rcscripts`、`/usr/share/openrc`、`/usr/lib/openrc`，没有任何 stdout/stderr 重定向变量
（`stdout_log` / `redir_stdout` / `output_log` 一个都不存在，已装的脚本里也无人用；`start-stop-daemon`
自己有 `-1/--stdout`、`-2/--stderr`，但 openrc-run 不传），而我们生成的配置只有
`{"level":"info","timestamp":true}`，日志全打在 stderr 上被丢掉。所以 `log.output` 必须自己写。四条实测：

1. **相对路径以 `-D` 为基准，不是进程 cwd**。cwd=`$d/cwd`、`-D=$d/state`、`output="sing-box.log"`
   → 文件落在 `$d/state/sing-box.log`，`$d/cwd` 一个文件都没有。§11.7 那条基准延伸到 `log`。
2. **不会自建父目录**，写 `logs/sing-box.log` 就是启动期硬失败：

   ```
   FATAL[0000] start service: start logger: open logs/sing-box.log: no such file or directory
   rc=1
   ```

   所以配置里只能写**裸文件名**，落点交给 `-D`（ZBox 自己建的状态目录，一定存在）。
3. **追加打开，不截断**：先写入 `OLD-MARKER-LINE` 再跑一次，标记仍在、其后追加三行；
   同一个 `-D` 第二次跑，4 行变 7 行。而 1.14.2 没有任何轮转配置——`sing-box schema` 的
   `$defs/LogOptions` 只有 `disabled`/`level`/`output`/`timestamp` 四个键，`additionalProperties: false`。
   容量只能由控制面兜：落到 `svc::LOG_CAP`（512 KiB，重启前整份挪成 `<name>.1`，磁盘上界 2 x 上限）。
4. **`check` 完全不打开这条路径**：同一份配置、把 `logs/` 挪走，`check` 照样 rc=0，`run` 才是上面那条
   FATAL。所以日志路径可不可写只有预跑门（`run`）验得到——预跑候选因此**刻意不清 `log`**，
   判据改成同时读"stderr 重定向文件"与"内核自己那份日志"（`Runner::until_started` 多收一个
   `kernel_log` 参数，路径按第 1 条以 `-D` 解析）。不这么改的后果很隐蔽：加上 `log.output` 之后
   `sing-box started` 不再出现在重定向文件里，门会把每一份**正常**配置判成 60 秒超时。

内核日志里那行判据的原文：`+0800 2026-09-30 16:34:35 INFO sing-box started (0.00s)`。

#### 11.8.4 连一下 TPROXY 端口就自激成环：生效性探测不能靠 connect（2026-09-30，首次真机 install）

`Svc::probe` 原本用 `TcpStream::connect_timeout(127.0.0.1:<端口>)` 判"入站在不在听"。
第一次把日志接出来跑真机 install，`/var/lib/zbox/sing-box.log` 在半秒内涨到 **350MB、约 6MB/秒**，
其中 `inbound connection to 127.0.0.1:9888` 有 **1,087,442** 行，`rc-service zbox stop` 才停住。原文逐字：

```
INFO [1328017243 0ms] inbound/tproxy[tproxy-in]: inbound connection from 127.0.0.1:39870
INFO [1328017243 0ms] inbound/tproxy[tproxy-in]: inbound connection to 127.0.0.1:9888
INFO [1328017243 0ms] outbound/direct[direct]: outbound connection to 127.0.0.1:9888
INFO [2791120148 0ms] inbound/tproxy[tproxy-in]: inbound connection from 127.0.0.1:39884
```

机制：TPROXY 入站是 `IP_TRANSPARENT` 监听器，接受任意目的地址的连接并**把原始目的地址当作路由目标**。
本机 connect 到 9888 时，原始目的就是 9888，direct 出站再连回 9888，递归没有界。
对照实测（同一份现场，四个端口都连过）：

- REDIRECT(:9887) **不成环**——没有 netfilter REDIRECT 目标可查时它干净地失败：
  `ERROR [1921265451 0ms] inbound/redirect[redirect-in]: process connection from 127.0.0.1:40962: get redirect destination: no such file or directory`
- mixed(:8888)、socks(:7890)、dns(:53) 各只留下一行 `inbound connection from`，无环。

定案两条，都进代码：

1. **探测只看不动**：`probe` 改读 `ss -lntu`（复用 `Observer::listening`），四个入站端口一次都不连。
   UDP（53 与 9888 的另一半）本来也连不出结论，这样 TCP/UDP 判据统一。
   测试 `the_probe_reads_sockets_and_connects_to_nothing` 真绑四个监听，判完之后断言一家都没被 accept 过。
2. `ss` 读不到就判"读不到"，不许降级成"没起来"（`a_probe_that_cannot_read_sockets_refuses_to_guess`）。

边界说明：这条环只在**本机进程直接连 TPROXY 端口**时出现。真劫持层下 9888 收到的是被分流的局域网包，
原始目的是客户端要去的地址，不会自激。但它定下一条纪律：控制面（以及以后面板上的"测端口"按钮）
只能用 `ss` 判，永远不要对劫持端口发 connect。



#### 11.8.5 OpenRC 0.55.1 的崩溃拉起：`respawn=true` 是死的，只有 supervise-daemon 读得通（2026-09-30，§6 服务制品彩排）

先记录**被推翻的写法**。我们的 runscript 原本按常见惯例写：

```
command_background=true
respawn=true
respawn_delay=5
```

实测它不成立，两条独立证据：

- `/sbin/openrc-run` 是一个 38816 字节的 ELF，`grep -a respawn` 与 `grep -a supervisor`
  在这个文件里 **0 命中**；`start-stop-daemon --help` 也没有 `--respawn` 这一档。
  全机只有 `/sbin/supervise-daemon` 和 OpenRC 自己的 `/etc/init.d/runsvdir` 里出现 supervisor。
- 行为：`kill -9` 现役 sing-box 之后等满 12 秒，进程没有回来，
  `rc-service zbox status` 稳定停在 ` * status: crashed`（rc=32），`ss -lnt` 只剩 sshd。
  也就是说 ARCHITECTURE 里"OpenRC 侧等价物是 respawn=true"那句当时是**没验过的推断**。

换上 OpenRC 唯一的监督路径之后（`supervisor=supervise-daemon` + `respawn_delay/period/max`），
实测到的命令行形状（逐字，`ps -o pid,ppid,args`）：

```
13426     1 supervise-daemon zbox --start --pidfile /run/zbox.pid --respawn-delay 5 \
              --respawn-max 5 --respawn-period 1800 /usr/local/bin/sing-box -- \
              run -c /var/lib/zbox/current/config.json -D /var/lib/zbox
13427 13426 /usr/local/bin/sing-box run -c ... -D ...
```

六条结论，全部有现场：

1. **真的会拉回来**：`kill -9 13427` → 第 5 秒出现新 worker（`respawn_delay=5` 就是这 5 秒；
   早先手写的试制品用 `respawn_delay=2`，第 2 秒就回来了，两档都对得上）。
   新 worker 的父进程是同一个 supervise-daemon，`status` 全程 `started`（rc=0），
   七个监听（53 tcp+udp、9888 tcp+udp、9887、8888、7890）全部重新在听，内核日志里多一次完整启动。
2. **pidfile 的含义变了**：`/run/zbox.pid` 装的是**监督者**的 pid（13426），不是 sing-box 的。
   以后任何"按 pidfile 找进程"的代码都不能再把它当数据面 pid 用。
3. **stop 是干净的**：`rc-service zbox stop` 把监督者与 worker 一起收掉、删掉 pidfile，
   `status` 转 `stopped`（rc=3）。不存在残留 supervise-daemon 空转的情况。
4. **runscript 里的 `export` 会传到 worker**：读 `/proc/<worker>/environ` 拿到
   `QUIC_GO_DISABLE_GSO=true`、`ZBOX_LAN_IFACE=ens33`、`RC_SVCNAME=zbox`。
   这条不必再找别的通道注入环境变量。
5. **`respawn_max` 有意取有限值**（5 次 / 1800 秒窗口）：`supervise-daemon` 自己会说
   `respawned "%s" too many times, exiting`。配置坏到起不来时停在可诊断状态，
   与 systemd 侧 StartLimit 同形，比无限重启风暴好。
6. **`start_pre()` 在这条路上仍然被调用**——但它"等默认路由"的那 30 秒分支这次没被真正走到
   （现场一直有默认路由，函数第一拍就 `return 0`）。等待分支留给集中实测用断网场景补。

systemd 侧未受影响（本来就没有 `command_background`/`respawn` 这套变量），
两台 init 家族的"进程死了有人拉"到这里才算**两边都有实证**。

### 11.8.6 sysctl 持久化文件在 Alpine 上到底被谁读（2026-09-30 真重启实测）

§11.8.5 留了一条"重启那一轮再补"。补了，而且它先推翻了两条写在 ARCHITECTURE 里的说法，
然后暴露出一个静默故障。现场全部是 2026-09-30 在 Alpine 3.21.3 guest 上跑出来的。

**一、读这个文件的是 busybox，不是 procps**

`/sbin/sysctl -> /bin/busybox`（BusyBox v1.37.0，没装 procps）。同一个不认识的键，
`-p` 与 `-w` 的退出码**不一样**（逐字抄的现场）：

```
# sysctl -p /etc/sysctl.d/99-zbox.conf        # 第 4 行的键名在这台机器上不存在
net.ipv4.ip_forward = 1
net.ipv4.conf.all.rp_filter = 2
net.ipv4.conf.default.rp_filter = 2
net.ipv4.conf.all.send_redirects = 0
sysctl: error: 'net.ipv4.conf.ens33/rp_filter' is an unknown key
rc=0                                          # <-- 整份文件只应用了 4/5 条，rc 仍然是 0
```

单独喂一个只含坏键的文件（`net.ipv4.conf.nosuch.rp_filter = 2`）也是 `rc=0`。
而 `sysctl -q -w`、`sysctl -w`、`sysctl -n` 对同一个坏键都是 **rc=1**。
注意坏键不阻断后面的行——它按自己的顺序把能用的都用上了才打那一行 stderr。

⇒ 结论：**持久化文件里写错的键是静默的**，`install`/开机都没法靠退出码发现它。
下发脚本（netstate.sh）用的是 `-w`，那条路是响的。

**二、绑网卡名的那条键从持久化文件里拿掉了**，理由是两条实测语义：

```
# all=2 不会把已存在网卡的存量值抬上去
sysctl -w net.ipv4.conf.eth0.rp_filter=0   ->  eth0 = 0
sysctl -w net.ipv4.conf.all.rp_filter=2    ->  回读 eth0 仍然是 0
# 但重启之后 eth0 读到 2（重启前手工值是 1）
```

也就是说 `conf/all/*` 是**判断时的地板**，不是**写入时的传播器**；重启后 eth0 之所以是 2，
生效的是 `conf/default/rp_filter = 2`——e1000 驱动在 `sysctl` 服务之后才创建 eth0，
新网卡从 `default` 继承。⇒ 绑网卡名的写法同时依赖"名字对得上"和"网卡比 sysctl 晚出现"，
而 `all`/`default` 两条都不依赖名字。名字从哪来？从 `DesiredState.network.lan_interface` 来，
而 §6 第 5 步（探测真实网卡写进初始 Model）还没落地，现场那份状态里是默认的 `ens33`
——这台机器的网卡是 `eth0`。所以那个键在这台机器上从来没生效过，只是没人知道。

**三、`rc-update add sysctl boot` 这条在 Alpine 3.21.3 上是空操作**

`/etc/runlevels/boot/sysctl` 的时间戳（Sep 29 15:41）与同目录其他十项一致 = 基础装机自带。
`docs/ARCHITECTURE.md` §6 第 3 步原来写的"不加则 sysctl 文件重启后不生效"不成立。
重复执行实测：

```
# rc-update add sysctl boot
 * rc-update: sysctl already installed in runlevel `boot'; skipping
rc=0
```

命令暂时留着（一行幂等，防的是有人手工删过），但它不再是"必须"的理由了。

**四、`/etc/init.d/sysctl` 的读取顺序**（脚本本体 2024-10-09，逐段看过）：
`/lib/sysctl.d/*.conf` → `/usr/lib/sysctl.d/*.conf` → `/etc/sysctl.d/*.conf` →
`/etc/sysctl.conf`（该文件自带的注释就写着 "content of this file will override /etc/sysctl.d/*"）
→ `/run/sysctl.d/*.conf`。⇒ 用户只要往 `/etc/sysctl.conf` 里写同名键就能盖掉 ZBox，
这不是 bug，但 `doctor` 想知道"为什么重启后值不对"时得记着这一层。

**五、判据是怎么成立的**：`/usr/lib/sysctl.d/00-alpine.conf` 设
`default.rp_filter = 1`、`all.rp_filter = 1`，**不设** `ip_forward`，也不设 `send_redirects`。
所以重启后这四项只可能来自我们的文件：

| 键 | Alpine 自己给的 | ZBox 文件 | 重启后实测 |
| --- | --- | --- | --- |
| `net.ipv4.ip_forward` | 缺省 0 | 1 | **1** |
| `net.ipv4.conf.all.rp_filter` | 1 | 2 | **2** |
| `net.ipv4.conf.default.rp_filter` | 1 | 2 | **2** |
| `net.ipv4.conf.all.send_redirects` | 缺省 1 | 0 | **0** |

顺带这一轮把 §11.8.5 没验的开机路径也走完了：17:14:02 下令重启 → 17:14:15 `openrc default`
→ 17:14:28 `supervise-daemon` 起来（worker pid 3991，父 3990），`sing-box started (0.03s)`，
`network: updated default interface eth0, index 2`，七个监听全在，`rc-service zbox status` =
`started`（rc=0），`zboxd status` 与 `zboxd doctor` 都判不出致命项（只有劫持层未加载两条提示，
符合"重启 = 回到未劫持的可回退落点"）。`start_pre()` 那 30 秒等待分支仍然没被逼出来
（日志里没有 `ewarn` 那条），断网场景留给以后。

### 11.8.7 产品路径复测：加载的是生成物，不是手抄脚本（2026-09-30）

§11.7 那轮用的是当场手写的 nft/ip rule 脚手架。这一轮换成 `zboxd enable-hijack` 与
`zboxd disable-hijack` 加载 `/var/lib/zbox/current/` 里**生成出来的** `zbox.nft` 与
`netstate.sh`，netns 客户端 `lan1`（10.99.0.2，网关指向 10.99.0.1 = 桥 `zbox-lan0`）。
结论：§11.7 定下的形状在生成物上原样成立，没有新的反例。

生成出来的表（`nft list table ip zbox` 逐字，注意 nft 会把十六进制补零写回来）：

```
table ip zbox {
	chain dns {
		type nat hook prerouting priority dstnat; policy accept;
		meta mark != 0x00000000 return
		iifname != "zbox-lan0" return
		ip saddr != 10.99.0.0/24 return
		udp dport 53 redirect to :53
		tcp dport 53 redirect to :53
	}
	chain divert {
		type filter hook prerouting priority mangle; policy accept;
		meta mark != 0x00000000 return
		iifname != "zbox-lan0" return
		ip saddr != 10.99.0.0/24 return
		fib daddr type local return
		ip daddr { 224.0.0.0/4, 255.255.255.255 } return
		udp dport { 67, 68 } return
		udp dport 53 return
		tcp dport 53 return
		meta l4proto tcp tproxy to :9888 meta mark set 0x00000001 accept
		meta l4proto udp tproxy to :9888 meta mark set 0x00000001 accept
	}
}
```

三条正证一次跑齐（客户端侧 `nslookup … 8.8.8.8` 与 `wget http://180.101.49.44/`）：

```
INFO [3738960296 0ms] inbound/direct[dns-in]: inbound packet connection from 10.99.0.2:52268
INFO [3738960296 0ms] inbound/direct[dns-in]: inbound packet connection to 0.0.0.0:53
INFO [3738960296 94ms] dns: exchanged A www.a.shifen.com. 60 IN A 180.101.49.44
INFO [894677448 0ms] inbound/tproxy[tproxy-in]: inbound connection from 10.99.0.2:56186
INFO [894677448 0ms] inbound/tproxy[tproxy-in]: inbound connection to 180.101.49.44:80
INFO [894677448 0ms] outbound/direct[direct]: outbound connection to 180.101.49.44:80
```

- 客户端问的是 **8.8.8.8**，日志里出现的是 `dns-in` + `dns: exchanged` ⇒ 53 被 `nat` 链接走了，
  答案是本机 DNS 引擎给的（这一轮 `final_outbound = direct`，所以最终值与真实值相同，
  判据只能是"进没进我们的引擎"，不是"答得对不对"）。
- `inbound/tproxy … connection to 180.101.49.44:80` 与 `outbound/direct … 180.101.49.44:80`
  成对 ⇒ TPROXY 保住了原始目的地。
- 表里没有 counter（§11.8 之外的老问题，ARCHITECTURE §8.8），所以当场补一条
  `nft insert rule ip zbox divert ip saddr 10.99.0.2 meta l4proto tcp counter`，
  读到 `packets 5 bytes 357` 才算"包确实进链"。用完按 handle 删掉。

四条行为定案：

1. **幂等**：连开两次 `enable-hijack`，计数仍是 tproxy 2、dns redirect 2、`ip rule` fwmark 1、
   table 100 路由 1。第二次 `enable-hijack` 直接报"内容与 <当前修订> 完全一致，未触碰内核"，
   这是 §5 第 0 步指纹短路在真机上的样子。
2. **卸载只删自己的**：`disable-hijack` 之后 `table ip zbox` 没了、`ip rule` 回到
   `0:`/`32766:`/`32767:`、table 100 空，而外来的 `table ip zboxtest`（脚手架的 masquerade）
   照常留着；sysctl 保持 `ip_forward=1`、`all.rp_filter=2`——这是明写的故意不回滚。
3. **卸载不断网**：拆完劫持层，`lan1` 的 `wget http://180.101.49.44/` 仍然 rc=0
   （走 `zboxtest` 的 SNAT 出 eth0）。旁路由最怕的就是这条，实测过。
4. **回滚要按修订号**：`zboxd rollback <修订号>` 以前**根本不读那个参数**，只会回到 last-good
   还报"已回到已知良好快照"（现场：`current` 指的就是 last-good，等于什么都没做却 rc=0）。
   现在按 `store.list()` 找修订号（只认 `<毫秒>-<十六进制>` 形状的目录，顺带堵死路径拼接），
   给错就报错并新到旧列出全部修订。回到旧修订后 reload + probe 全过、客户端照常。

一个容易误判的现象记在这里：`ip netns exec lan1 wget http://www.baidu.com/` 有一次**只出
`dns-in` 不出 `tproxy`**，看起来像 TCP 没被劫持；重跑就成对了，raw IP 的 wget 也每次都成对。
按现在的证据只能判"日志落盘与取样时刻差了不到一秒"，不是链路问题——`tail -c +N` 紧跟在
wget 后面读，边界那次赶上了。以后取证据要么 sleep 一拍，要么按连接 id 找。

另外脚手架自己的坑：`ip -n lan1 ping -c1 10.99.0.1` 在这台机器的 iproute2 上是
`Object "ping" is unknown`（`topo-up.sh` 最后一行因此 rc=1，而拓扑其实已经起来了）。
要 ping 客户端得写 `ip netns exec lan1 ping …`。

### 11.8.8 网卡形状探测的输入原文（2026-10-05 guest 逐字抓取，§6 第 5 步的依据）

`ip -4 -o addr show`，单网卡现场（重启后只有 lo 与 eth0）：

```
1: lo    inet 127.0.0.1/8 scope host lo\       valid_lft forever preferred_lft forever
2: eth0    inet 192.168.219.128/24 brd 192.168.219.255 scope global eth0\       valid_lft forever preferred_lft forever
```

同一台机器、netns 拓扑加载着（桥 `zbox-lan0` = 10.99.0.1）：

```
8: zbox-lan0    inet 10.99.0.1/24 scope global zbox-lan0\       valid_lft forever preferred_lft forever
```

逐字读出来的三条形状事实：

1. `-o` 的续行折成"**反斜杠 + 一串空格**"（实测 7 个），不是真换行；解析按整行 `split_whitespace`
   找 `inet` 就天然 immune，但**不许**按"行尾是 `\`"去截。
2. **桥上的地址没有 `brd` 段**，物理口的有。所以解析里任何"`inet x` 后面第三个 token 是 brd"
   之类的定位都是错的，只认 `inet` 与其后第一个 `addr/prefix`。
3. 宿主 ns 里 `ip -4 -o addr show` 打印的网卡名**不带 `@ifN`**；`@ifN` 是 netns 内 veth 的形状
   （`ip -br addr` 给 `lan1-eth@if9`）。`topology::addrs()` 仍然在 `@` 处截断，这是保险而非实测必需，
   因为截出来的名字要进 `net.ipv4.conf.<name>.rp_filter` 和 nft 的 `oifname`，带 `@` 内核不认。

判据在真实输出上两条分支都走通了（`zboxd detect`，guest 上跑的静态 musl 构建）：

- 单网卡：`结论: 局域网网卡 eth0，段 192.168.219.0/24，上游网卡 eth0（单网卡旁路由）`，
  上游网关 `192.168.219.2`，rc=0。**主机位被抹成 `.0/24` 而不是留 `.128/24`** —— 这是
  `network_cidr()` 的算术，不是 iproute2 给的。
- 双网卡（拓扑在）：`局域网网卡 zbox-lan0，段 10.99.0.0/24，上游网卡 eth0`，没有"单网卡"后缀。

把这次修掉的静默缺陷按产品路径复验了一遍：state.json 故意留内置默认形状
（`ens33` / `192.168.1.0/24` / `ens33`），`zboxd --prefix /tmp/... install` 打印
`网卡形状探得：局域网 eth0（段 192.168.219.0/24）`，写出来的制品里 `ens33` **零命中**；
再用 `--root /tmp/zboxapply apply` 生成快照，`netstate.sh` 第 9 行是
`sysctl -q -w 'net.ipv4.conf.eth0.rp_filter=2'`，`netstate.sh` 与 `zbox.nft` 的 `ens33` 命中数都是 0。
反向那条也验了：显式写一个本机没有的名字（`br-lan`）时**不覆盖**，只警告
`state.json 写的局域网网卡 br-lan 在本机找不到（本机有：lo、eth0、zbox-lan0）`。

两条实操坑记下来免得重踩：

- **`zboxd … | head -N` 会把 zboxd 打死在半路**（SIGPIPE）。有两轮 `install --prefix` 就是这样
  只打印了前两行、制品一个没写，`rm` 时报"目录不存在"才看穿。改成 `> /tmp/x.log 2>&1` 再读，
  rc 与产物都齐（复验 rc=0、`etc/init.d/zbox` + `etc/sysctl.d/99-zbox.conf` 都写了）。
- 拿**另一个 `--root`** 去 `apply` 可以只看生成物、不动现役快照：`current` 是按 root 算的，
  所以 `/var/lib/zbox/current` 全程没被碰。但要清楚服务是按**名字**寻址的，那一次 apply
  仍然 `rc-service zbox restart` 了现役内核（重启后 `status: started`、监听 5 条行全在），
  这不是"只读"操作。

### 11.8.9 规则集那一条链在真二进制上的形状，加上首次安装那次 restart 的定性（2026-10-05，task #9 与 §6 第 3/4 步收口）

现场：guest 回到裸基础机之后按产品流程重建（`provision` → `install` → `rule-sets` → `apply`），
下面每一条都是这份现场上跑出来的，不是推的。

#### 一、`sing-box rule-set match` 把命中信息打在 **stderr** 上

抓法是把 stdout / stderr / rc 分三段包起来打印，所以 `<binary>` 是**记录时的替换记号**
（命中信息里跟的是压缩规则集的原始字节，不可打印），其余逐字照抄：

```
### sing-box rule-set match --format binary /var/lib/zbox/rule-set/cn-ip.srs 1.2.4.8
rc=0
stdout>><<
stderr>>match rules.[0]: ip_cidr=<binary><<
### sing-box rule-set match --format binary /var/lib/zbox/rule-set/cn-ip.srs 223.5.5.5
rc=0
stdout>><<
stderr>>match rules.[0]: ip_cidr=<binary><<
### sing-box rule-set match --format binary /var/lib/zbox/rule-set/cn-ip.srs 8.8.8.8
rc=0
stdout>><<
stderr>><<
### sing-box rule-set match --format binary /var/lib/zbox/rule-set/cn-ip.srs 1.2.4.0/24
rc=0
stdout>><<
stderr>><<
### sing-box rule-set match --format binary /var/lib/zbox/rule-set/cn-ip.srs zbox-integrity-check.invalid
rc=0
stdout>><<
stderr>><<
### sing-box rule-set match --format binary /var/lib/zbox/rule-set/cn.srs www.baidu.com
rc=0
stdout>><<
stderr>>match rules.[0]: domain/domain_suffix=<binary> domain_regex=[^r+[0-9]+(---|\.)sn-(2x3|ni5|j5o)\w{5}\.xn--ngstr-lra8j\.com$ ^cdn\d-epicgames-\d+\.file\.myqcloud\.com$ .+\.awsdns-cn-[0-9][a-e0-9]\.cn$]<<
### sing-box rule-set match --format binary /var/lib/zbox/rule-set/cn.srs example.com
rc=0
stdout>><<
stderr>><<
### sing-box rule-set match --format binary /var/lib/zbox/rule-set/cn.srs 1.2.4.8
rc=0
stdout>><<
stderr>><<
### sing-box rule-set match --format binary /tmp/bad.srs baidu.com
rc=1
stdout>><<
stderr>>FATAL[0000] invalid sing-box rule-set file<<
### sing-box rule-set match --format binary /var/lib/zbox/rule-set/nope.srs baidu.com
rc=1
stdout>><<
stderr>>FATAL[0000] read rule-set: open /var/lib/zbox/rule-set/nope.srs: no such file or directory<<
```

读出来的五条，每条都改了一处判据：

1. **命中与否不改退出码**：命中和未命中都是 `rc=0`，`stdout` 恒空。所以验收**不能**看 rc，
   只能看 `match rules.` 这段文本在不在——它在 stderr 上，这就是 `verify()` 用
   `Exec::merged()`（stdout+stderr，`svc/runner.rs:29`）而不是 stdout 的全部理由。
   rc 只用来区分"文件坏/不存在"（`rc=1` + FATAL）。
2. **两种命中形状**：纯 IP 源是 `ip_cidr=<binary>`，域名源是 `domain/domain_suffix=<binary> domain_regex=[…]`。
   两边都被 `<binary>`（压缩规则集的原始字节）填过，所以只匹配前缀 `match rules.`，任何往后截取的写法都会读到垃圾。
3. **待匹配值必须是成员地址**。`1.2.4.0/24` 这个段**确在** geoip-cn 里，拿它自己当探针却判未命中——
   CIDR 不是地址。这就是 `probe_value()` 对 IP 源一条都不探的原因（探了只会造出"没命中"的假阳性），
   也是它挑 `1.2.4.8` / `www.baidu.com` 这种"必在表里"的具体值的理由。
4. **跨类型探针返回空**，不报错：域名问 IP 表、IP 问域名表都是 `rc=0` + 空。
   所以"空"要么是"没命中"要么是"问错了表"，探针值必须跟源的形状配对。
5. `--format` 默认是 `source`（帮助原文 `-f, --format string rule-set format (default "source")`），
   读 `.srs` **必须显式给 `binary`**，否则同样的文件一律问不出命中。
   `rule-set` 下可用的子命令：`compile / convert / decompile / format / match / merge / upgrade`。

#### 二、抓取 → 验收 → 顶掉现役 → 记清单：成功与两条失败分支都走完了

把 `rule-set/` 清空后从零同步（三条源全是内置 tag，`zboxd-v5` = 带 `-T 20` 的那次构建）：

```
rc=0 耗时=2秒
网卡形状探得：局域网 eth0（段 192.168.219.0/24）、上游 eth0，上游网关 192.168.219.2
规则集已落地 geoip-cn
规则集已落地 geosite-cn
规则集已落地 non-cn
total 264                                    ← ls -l，下面四行去掉了时间戳
geoip-cn.srs     34185
geosite-cn.srs   56145
non-cn.srs      168002
manifest.json      660
```

清单三条长这样（**`rule_count: 0` 不是缺陷**）：

```
"geosite-cn": { "url": ".../geosite-cn.srs", "format": "BinaryRuleSet",
                "rule_count": 0, "bytes": 56145, "updated_at": 1790766484 }
```

预编译 `.srs` 走的是 `PlanAction::Install`：字节原样落地，不经 `compile`，也就没有"条数"可读——
`rule_count` 记的是**归一化后的规则条数**，只有经 `rule-set compile` 的源（我们自己的 JSON、AdGuard 名单）
才非零，直通的 `.srs` 恒为 0（`ruleset.rs:65` 那条注释就是防"把 0 读成空表"的）。同一轮还确认了
中间件不残留：`ls *.download` / `ls *.staging.srs` 都是 `No such file or directory`。

失败分支（对着黑洞地址 `https://10.255.255.1/dead.srs`，本地地址不发外网包）：

- **没有可沿用的产物 → 整个同步停在这里**（rc=1）。日志：
  `zboxd: 抓取 https://10.255.255.1/dead.srs 失败：wget: download timed out`，总耗时 20 秒，
  目录里没多出 `dead.srs`，清单里没有 `dead` 条目。放过去等于让 sing-box 启动时现抓（§11.7 的 15 秒预算），
  所以故意不停在这里是错的。
  顺带订正一处**我自己的读法**：那一轮 state 里 `geoip-cn` 声明在 `dead` 之前，它其实被走过——
  作为"沿用"（新鲜，不起进程）。看不见 `规则集沿用 geoip-cn` 是因为 `sync_rule_sets` 把
  `sync(…)?` 放在打印**之前**（`zboxd.rs:410`），硬失败时整份报告都不出口。
  条目顺序也是这么来的：报告按 refreshed → kept → notes 三段打，不是按声明顺序混排。
  要查"这一轮到底落了哪几条"得看 `rule-set/manifest.json`，它的每条目写入是即时的。
- **有可沿用的产物 → 只提示，继续跑完其余条目**（rc=0）。同一份 state 里把 geosite-cn 的 URL 换成黑洞、
  其余两条照旧，加 `--force`：
  ```
  rc=0 总耗时=21秒
  规则集已落地 geoip-cn
  规则集已落地 non-cn
  提示: geosite-cn: 抓取 https://10.255.255.1/dead.srs 失败：wget: download timed out；沿用 geosite-cn.srs 现有产物，下次到期再试
  ```
  事后清单里 `geosite-cn` 的 `updated_at` 还是上一轮那个（1790766484），而另外两条是新时刻（1790766647）。
  两件事一起验到了：**每落一条写一次清单**（打断不会让已落地的重新联网），以及
  **失败的那条连 url 字段都不改**——清单里的 `url` 是"产出当前这份产物的来源"，不是"state 声明的来源"。
  这有实际后果：把一个源换掉但新源抓不通时，清单仍指向旧源，下次到期会拿**旧 URL** 去续期。
- `--force` 的作用面确认：无视新鲜度全部重抓（上面那轮三条都进了网络路径）。

一条**不是缺陷但会脏**的实测：清单按 tag 键控，state 里删掉的 tag 不会被回收。
我早前那版夹具用了自造 tag `cn` / `cn-ip`，同步真就把 `cn.srs`(56145) 和 `cn-ip.srs`(34185) 落了地、
写进清单；后来把夹具改对，那两个条目和两个文件仍在盘上，得手工删（本轮已删）。

#### 三、busybox wget 没有 `-t/--tries`，`-T` 是每阶段空闲 —— 一条源能烧 196 秒

`wget --help` 全文里的网络相关项只有 `-T SEC   Network read timeout is SEC seconds`，
`Usage` 行是 `wget [-cqS] [--spider] [-O FILE] [-o LOGFILE] [--header STR] … [-T SEC] URL...`：
**没有 `-t`**。写 `-t 1` 是静默空操作，实测对着黑洞比时长：

```
wget -q -T 20 -O /tmp/a   https://10.255.255.1/x.srs: rc=1 耗时=20s
wget -q -T 20 -t 1 -O /tmp/b https://10.255.255.1/x.srs: rc=1 耗时=20s     ← 加了 -t 一模一样
```

推翻的是我自己那条假设（"196 秒是因为没限制重试次数"）。真原因是 `-T` 按**阶段**计空闲：
连接、TLS 握手、读各算一次。旧值 `-T 60` 那轮抓 GitHub 的 TLS 链不通时实测烧了 196 秒
（`rc=1 耗时=196s`，`ssl_client: SSL_connect` + `wget: download timed out`；原始记录在
`.tmp/retry.out`，同一条链在 31MB 资产上的对应物是 §11.8.1 那条"白等 168 秒"）。
改动：`FetchLimits::SOURCE.wget_idle` 60 → **20**（curl 那边 `-fsSL --max-time 60` 不动，它是总时长，两个概念不是一回事）。
按新值复测，最坏一轮 20 秒：`rc=1 总耗时=20秒`，`rc=0 总耗时=21秒`。测试
`downloader_choice_matches_what_the_guest_actually_has` 里除了断言 `["-T","20"]`，
还断言参数里**不出现** `-t`，防止"顺手加个重试"把它改回静默空操作。

**2026-10-07 补一条同一条链的另一种失败**：面板那一轮用真地址 `geosite-cn.srs` 走在线源，
在 `-T 20` 下失败于**读阶段**而不是握手阶段——回执原文
`抓取 https://raw.githubusercontent.com/SagerNet/sing-geosite/rule-set/geosite-cn.srs 失败：ssl_client: SSL_read / wget: download timed out`。
上一条里的 `rc=0 总耗时=21秒` 与 §11.3 那句"wget 直连 200 / 56190 字节"都还是真的，
但两者加起来不构成"guest 抓得动 GitHub"这个前提——同一个 URL 同一条链三种结局（200 / SSL_connect 超时 / SSL_read 超时）都观测到了。
**在线源的验收不能拿"这一轮抓下来了"当判据**，要判的是失败分类与回执形状：硬失败时
`POST /api/rule-sets/sync` 报 500 **并带部分报告**，其中 `缺文件：…` 只列**排在那条失败之前已经看过**的本地声明
（迭代按 `rule_sets` 的声明序，失败之后还没被看到的那几条不在报告里）——这是设计，别当漏报修。

#### 四、规则集先落地之后，冷启动不付 §11.7 那 15 秒

清空 `/var/lib/zbox/sing-box.log` 后 `rc-service zbox restart`：

```
+0800 2026-09-30 19:08:59 INFO inbound/direct[dns-in]: tcp server started at 0.0.0.0:53
+0800 2026-09-30 19:08:59 INFO inbound/direct[dns-in]: udp server started at 0.0.0.0:53
+0800 2026-09-30 19:08:59 INFO inbound/tproxy[tproxy-in]: tcp server started at 0.0.0.0:9888
+0800 2026-09-30 19:08:59 INFO inbound/tproxy[tproxy-in]: udp server started at 0.0.0.0:9888
+0800 2026-09-30 19:08:59 INFO inbound/redirect[redirect-in]: tcp server started at 0.0.0.0:9887
+0800 2026-09-30 19:08:59 INFO inbound/mixed[mixed-in]: tcp server started at 0.0.0.0:8888
+0800 2026-09-30 19:08:59 INFO inbound/socks[socks-in]: tcp server started at 0.0.0.0:7890
+0800 2026-09-30 19:08:59 INFO network: updated default interface eth0, index 2
+0800 2026-09-30 19:08:59 INFO network: updated network environment: gateway 192.168.219.2, gateway_mac 00:50:56:e8:bd:9f
+0800 2026-09-30 19:08:59 INFO sing-box started (0.05s)
```

`:9888` 在听用时 1 秒；日志里 `wget|download|fetch` 命中数 **0**。同一条判据在带正确 tag 的首次
apply 那轮也一样（`sing-box started (0.02s)`）。这就是 §6 第 4 步排在第 7 步之前的实测收益。

#### 五、首次安装那次 `restart`：不是"失败一次"，是"报成功而什么都没起"

ARCHITECTURE 原话"这条 restart 在首次安装时必然失败一次"**被推翻**。把 `current` 改名让配置消失
（可逆操作），受控复现首次安装那一刻的形状：

```
########## 无配置时 restart rc=0 耗时=0s
 * Starting zbox ... [ ok ]
== 服务状态
status rc=0
 * status: started
== 进程/监听
10562      ← 只有 supervise-daemon
pgrep rc=0
0          ← 监听数
```

再按时间取样，`respawn_delay=5 / respawn_max=5` 那 5 次重试的空档全在 `started` 里过：

```
t=8s  status_rc=0 状态= * status: started 进程数=1 监听=0
t=16s status_rc=0 状态= * status: started 进程数=1 监听=0
t=24s status_rc=0 状态= * status: started 进程数=1 监听=0
t=32s status_rc=3 状态= * status: stopped  进程数=0 监听=0
```

`rc-service zbox restart` 的 rc **不是**判据（它只报告"监督者装上了"），`status` 才是：
`rc=0` 表示 started、`rc=3` 表示 stopped（实测）。把配置放回去再 restart：`监听=3`、`status: started`，
恢复无损。

**而且这一轮在机器上查不到任何错误文本**——那 24 秒是静默的：
- `sing-box.log` 空。清空它再 restart，`cat` 出来一个字符都没有：内核自己的 `log.output` 是写在
  `config.json` 里的，配置读不到就还没有日志器，FATAL 无处落。
- `/var/log/messages` 里 `grep -c FATAL` = **0**。supervise-daemon 往 syslog 记的是命令行与退出码，
  **不转记子进程的 stderr**：
  ```
  Sep 30 19:22:20 localhost daemon.info supervise-daemon[11744]: Child command line: /usr/local/bin/sing-box run -c /var/lib/zbox/current/config.json -D /var/lib/zbox
  ```
  （`--respawn-delay 5 --respawn-max 5 --respawn-period 1800` 也在这条的 Supervisor 行里逐字可见；
  退出码是另起一行的 `daemon.warn … exited with return code 1`，看得见"死了"，看不见"为什么"。）
- 唯一的读法是手工跑同一条 argv，那时才拿得到 worker 原话：
  ```
  rc=1
  stdout:[]
  stderr:[FATAL[0000] read config at /var/lib/zbox/current/config.json: open /var/lib/zbox/current/config.json: no such file or directory]
  ```
  这是一条**可诊断性缺口**（和 §8.8 的 nft 无 counter 同族）：现役 runscript 没给 supervise-daemon 指定
  子进程输出的落点，所以"起了但没在听"这类故障在现场只能靠 `zboxd status` 的读数反推。
  要补的话得先测 supervise-daemon 的输出重定向字段在 0.55.1 上叫什么、读不读，别照文档猜。

后果与改动：首次安装时这条 restart 会在约 25 秒里挂着"服务已启动"的假象，worker 在后台把
上面那条 FATAL 试满 5 次，而**这 5 次一个字都不落盘**——正好覆盖在第 4、7 步（抓规则集 + 下发第一份配置）
正在跑的那段时间，看上去像"装完了但内核不干活"。
现在 `Svc::install(&artifacts, start_service)` 多带这个开关，`zboxd` 在 `<current>/config.json`
不存在时传 `false`，`enable_commands()` 末尾那条由新的 `start_command()` 单独提供、按整条 argv
比对摘掉（不按下标假设 systemd 与 OpenRC 的命令数不同）。首次安装的日志成了这样：

```
已执行 `rc-update add zbox default`
已执行 `rc-update add sysctl boot`
首次安装：服务已登记但未拉起（此刻还没有配置），下面下发第一份配置时再启
```

`status` 紧接着报 `服务活跃: 否 / 在听端口: 22`（也就是"确实没起"，而不是"起了但没听"）。
**仍没走到的分支**：runscript 里 `start_pre()` 等默认路由那 30 秒——现场每次都有默认路由，
要验它得在开机早期或拔网，留给真机。

#### 六、内置规则集 tag 的名字是**生成器**守的，不是抓取守的

夹具里把 tag 写成 `cn` / `cn-ip`（上游文件名 `geosite-geolocation-!cn` 之类同理，`!` 过不了
`is_safe_rule_set_tag`）时，`rule-sets` 照样抓取、验收、落地成功；拦住的是 `apply`：

```
策略需要规则集 geosite-cn，但它不在 rule_sets 里——引用未定义的规则集会让 sing-box check 直接失败
```

（`src/gen/singbox/route.rs:33`。）这条分工是有意的：抓取层只管"tag 能不能当文件名 + 产物能不能用"，
策略引用的闭合性在生成时判——所以那次报错是**守卫在正常工作**，不是缺陷，产品代码没动。

另记一条现场噪声：guest 时钟与宿主不同步（宿主 2026-10-05，guest `ls` 显示 Sep 30 19:xx），
清单里的 `updated_at` 是 guest 的 epoch（本轮 1790766484 这一族）。新鲜度判定是 guest 自己
now 与自己 now 比，所以不受影响；但把清单时间拿去和宿主对表做报表时要记得这一层。

#### 七、把子进程的 stderr 接到能查的地方：`error_log` 这个变量名是读源码读出来的

上面那条"现场无错可查"当天就补了。机制不在 man page 里，在
`/usr/libexec/rc/sh/supervise-daemon.sh`（0.55.1，第 31-32 行逐字）：

```
		${output_log+--stdout} ${output_log} \
		${error_log+--stderr} $error_log \
```

所以 runscript 里能用的变量叫 `error_log` / `output_log`（映射到 `--stdout` / `--stderr`），
写 `stderr=` 是没人读的。生成出来的制品多了这一条：

```
error_log="/var/lib/zbox/sing-box.stderr.log"
```

真机上验到的四件事（同一轮，用产品生成的 runscript，不是手改的）：

1. 配置缺失时重启，6 秒后文件里就是 worker 的原话，且**每次 respawn 各追加一行**：
   `FATAL[0000] read config at /var/lib/zbox/current/config.json: open ...: no such file or directory`（274 B = 两次）。
   syslog 里同时能看到退出码那行（`exited with return code 1`），所以更正上面第五节里
   "syslog 只有命令行"的说法：**退出码在，stderr 文本原本不在**。
2. 文件由 supervise-daemon 在每次受管启动时创建，模式 `-rw-------`（root 0600），**正常启动一个字节都不写**；
   `stop` 不删它。所以它是"只记异常"的追加式落点，量级由 `respawn_max` 兜着（每 `respawn_period`
   最多 5 行 × 137B），**故意不做轮转**。
3. 落点跟着 `Layout::worker_stderr_path()` 走，即状态目录的兄弟文件，`--root` 一挪它跟着挪；
   systemd 侧不接这个文件（stderr 默认进 journal），所以那条路径只出现在 OpenRC 制品里。
4. `zboxd doctor` 的 `inbound_not_listening` 从此带处置命令：
   ```
   [致命] inbound_not_listening — 配置里的入站没有监听起来，通常是 sing-box 启动失败或端口被占用
     期望: dns:53、tproxy:9888、mixed:8888 都在监听
     实际: 实际在听 [22]
     处置: cat /var/lib/zbox/sing-box.stderr.log
     处置: journalctl -u zbox -n 50
   ```
   两条都给是因为按 init 系统分支要额外穿参，而这两条命令各自在对面机器上都是无害的。

顺带一条**新查到的读数为时过早**的坑（当天差点把我带偏）：`rc-service zbox restart` 之后**立刻**跑
`zboxd status`，会读到 `服务活跃: 是 / 在听端口: 22`——那正是本节第五节描述的形状，可这次配置是好的，
只是 sing-box 还没绑上端口。实测 `install` 全程 0 秒返回，而内核日志里 `sing-box started (0.03s)`
出现在同一秒内，`status` 的 `ss` 快照就在这之前。等 2 秒再读，六个端口全在（复现两次，同一台机器）。
所以"起来了但没在听"这句话有两种成因：**真没起来**（stderr 文件里有 FATAL），或者**读得太早**
（文件是 0 字节 / 不存在）。处置命令按这个顺序读就对了。

#### 八、`error_log` 的目录前提，与 `respawn_max` 的真实预算（2026-10-06，Alpine 3.21.3 / OpenRC 0.55.1，面板那份 runscript 的验收轮）

上面第 2 条那句"文件由 supervise-daemon 在每次受管启动时创建"少说了一个前提：**父目录必须在**。
补上它的不是文档，是一次起不来的服务。

1. **`--stderr` 是在 fork worker 之前打开的**。`error_log` 指到 `/tmp/zbox-panel17/panel.stderr.log`
   而那个目录不存在时，现场形状是：`rc-service zbox-panel start` rc=0、`status` 报 `started`、
   `pgrep -af` 只有一行 `supervise-daemon zbox-panel --start --stderr ...`、没有任何 worker 进程、
   9090 不在听，而且**`/tmp/zbox-panel17` 整个不存在**——所以连"为什么起不来"都没被记下来。
   同一轮里手工执行完全相同的 `zboxd serve --root /tmp/zbox-panel17 --init openrc --port 9090`
   能把目录、令牌（0600）都建出来并打印 `面板已监听 ...`（rc=143 是被 timeout 收的），
   随后监督者的下一次 respawn 自己就成功了：这条同时证明根因是目录，不是被管的程序。
   处置写进制品：runscript 的 `start_pre()` 里 `mkdir -p <状态目录>`。
   **执行范围要说清**：线上走的是面板那一份（新根、无 `state.json`、start 即监听）。
   数据面那一份加的是同一句，但本轮只过了 `sh -n`（rc=0）——staged 的那份 pidfile 也是
   `/run/zbox.pid`，真 start 它就是在动现役数据面的服务，代价不对等，所以机制由面板那份代证。
2. **`checkpath` 不能用在 `start_pre()` 里**（至少不能指望它）：guest 上 `which checkpath`
   报 `checkpath: not found`——它是 OpenRC 的 shell 函数，不在 runscript 执行时的 PATH 上；
   busybox 的 `mkdir` 在。
3. **`respawn_max=N` 的真实含义是"再拉 N 次"，总启动次数是 N+1**。端口撞车那一轮（把面板
   指到 sing-box 正在听的 9888，`respawn_delay=5`、`respawn_max=12`、`respawn_period=1800`）：
   `panel.stderr.log` 里每次尝试各一行 `zboxd: 绑定 192.168.219.128:9888 失败: Address in use (os error 98)`，
   65 秒后数到 **13** 行，监督者进程消失，`rc-service zbox-panel status` = `stopped`、**rc=3**，
   此后不再增长。也就是说这条链不会变成重启风暴，也不会自愈成"悄悄在重启"。
4. **`status` 在这整条路上都可以报 `started`**，它是监督者的状态不是被管进程的状态。
   判据只能用这两条之一：`ss -lntu` 看端口，或读那个 stderr 文件的行数。
5. 顺带把 `pidfile` 那条也钉一下：面板与数据面各用各的（`/run/zbox-panel.pid` / `/run/zbox.pid`）。
   同名会让 `rc-service <a> stop` 停掉对面那个进程——两个服务共用一个 pidfile 没有任何好处。

### 11.9 schema v2 生成形状的真机验收（2026-10-05，Alpine 3.21.3 / sing-box 1.14.2 `-musl` / v10 二进制）

先说清楚**这次没做什么**：全程没有 `apply`、没有碰 `/var/lib/zbox`、没有重启现役内核
（开工前后都是同一个 `pid=13582`，`current` 软链还是 `1790765940501-2640bddc7283fd1b`，
劫持层依旧未加载）。做法是把迁移后的配置丢进一个**独立 netns 里第二个 sing-box 实例**——
netns 有自己的端口空间，所以它能占 `127.0.0.1:53` 而不与宿主冲突，测完 `ip netns del` 即可。

#### 一、迁移：v1 三档名单在真二进制上变成一条有序名单

`.tmp/env-v1.json`（由真 v2 快照反推：去掉 `schema` 键、把 `domain_rules` 拆回
`blocked_domains`/`direct_domains`/`proxy_domains`）喂给 `dump-snapshot`（它走的就是
`model::state::read` 这条真入口）：

```
read state from env-v1.json
wrote 4 files to out-v1
rc=0
--- 落盘的 out-v1/state.json 开头
{
  "schema": 2,
```

名单顺序 = **拦截 → 直连 → 代理**（blocked 两条、direct 一条、proxy 一条），
生成的两层规则逐行同向：

```
dns.rules   1 reject [geosite-category-ads-all]   2 reject example.blocked
            3 reject +direct                      4 route example.direct  => domestic
            5 route example.proxy => fakeip (带 query_type)   6 route geosite-cn => domestic
            7 route .cn => domestic               8 route (A,AAAA) => fakeip
route.rules 6 reject [ads]  7 reject example.blocked  8 reject +direct
            9 route example.direct => direct   10 route example.proxy => auto
            11 geosite-cn => direct  12 geoip-cn => direct  13 bittorrent => direct
            14 non-cn => auto  15 fakeip 段 => auto
```

`zboxd --init openrc doctor out-v1/state.json` 在真机上点出了迁移的代价：

```
[提示] domain_rule_never_fires — 有几条域名规则永远不会生效：前面已有一行把它们的域名整个覆盖
  实际: example.direct（直连） 被排在它前面的 +direct（拦截） 盖住
```

（同一条判据的反向也在真二进制上验过：把 `+direct（拦截）` 挪到 `example.direct（直连）` 之后，
报的就是 `block_never_fires_behind_a_pass`，文案"这些域名照样连得出去"。
`Keyword`/`Regex` 参与的重叠依旧不报——这条是刻意的，见 §4 末。）

#### 二、`check` 放行，`run` 也起得来（预跑门）

```
sing-box check -c .../prunerun/config.json -D .../prunerun     rc=0
sing-box run   -c .../prunerun/config-empty.json -D .../prunerun
  -> INFO sing-box started (0.08s)      ← 判据，不是 check
```

`config-empty.json` = 同一份配置把 `inbounds` 清空，`-D` 另起目录。
四条 `.srs` 里 `geosite-category-ads-all` 是真存库里没有的（现役配置 `block_ads` 关着），
临时用 `sing-box rule-set compile` 编了个两条规则的桩顶上；**这个桩顺带也证明了 §11.4 的
`initial_path` 离线路径**，见下面第四条。

#### 三、新写法 `{domain_suffix, query_type, server: fakeip}` 在 1.14.2 上按预期取交集

这是本次真正要测的那一条——v2 的"代理"行第一次同时带域名匹配和 `query_type`。
在 netns 里跑只监听 `127.0.0.1:53` 的实例，用 busybox `nslookup` 问
（注意 busybox 的 `nslookup` **不接受 `IP:端口`** 形式，`DNS_SERVER` 参数只能给裸 IP，
所以要占 53 只能靠 netns 隔离，这是本轮第一次摸索出来的操作约束）：

| 探针 | 应答 | 日志 | 结论 |
|---|---|---|---|
| `A example.proxy`（名单里的代理行）| `198.18.0.2` | `dns: exchanged A example.proxy. 600 IN A 198.18.0.2` | 域名 + `query_type` **取交集**成立 |
| `TXT example.proxy`（同域名、非地址类型）| 超时 | `outbound/vless...: outbound connection to 1.1.1.1:443` | **没有**落 fakeip，继续走到 `dns.final=overseas` → `query_type` 守卫有效 |
| `A nobody.test`（不在任何名单）| `198.18.0.3` | `dns: exchanged A nobody.test. 600 IN A 198.18.0.3` | 末尾那条兜底 fakeip 规则没被新写法挤坏 |
| `A example.direct`（被 `+direct` 盖住的直连行）| `REFUSED` | — | 引擎侧承认了迁移的档序：reject 在前，后面的 route 根本不参与 |
| `A example.blocked` | `REFUSED` | — | 名单里的拦截行 |
| `A ads.stub.test` | `REFUSED` | — | `RULESET_ADS` 的 reject 走 `initial_path` 命中桩文件 |
| `A www.baidu.com`（**在真 geosite-cn 里**，同时是名单里的代理行）| `198.18.0.2` | `dns: exchanged A www.baidu.com. 600 IN A 198.18.0.2` | ★ **用户名单压过地理默认**，见下 |
| `A www.qq.com`（在 geosite-cn、不在名单）| 超时（去 domestic）| — | 对照组：地理规则照旧生效 |
| `A test.cn`（`.cn` 后缀兜底）| 超时（去 domestic）| — | 对照组同上 |

**★ 选项 C 那处唯一的语义变更已经在内核上落实**：`www.baidu.com` 既在真 `geosite-cn.srs` 里、
又被用户名单点名为代理，实测评的是 fakeip（境外那侧），不是 domestic。
两侧对照（`www.qq.com` / `test.cn` 都往 domestic 走）排除了"geosite-cn 根本没加载"这种假阳性——
它确实加载了、也确实被名单压过。busybox 没有 `HTTPS`/`SVCB` 查询类型，
所以守卫用 `TXT` 当"非地址类型"的代表来打。

#### 四、顺带量到的一条：`initial_path` 不挡启动，但**照旧会去下载**

netns 里没有路由，四条远端规则集全部抓取失败，而实例仍然起来且 `rule_set` 判据能命中
（`ads.stub.test` 被拒）：

```
ERROR router: fetch rule-set geosite-cn: Get "https://raw.githubusercontent.com/...": ... network is unreachable
ERROR router: fetch rule-set geosite-category-ads-all: ...
ERROR router: fetch rule-set non-cn: ...
ERROR router: fetch rule-set geoip-cn: ...
INFO sing-box started (0.10s)
```

所以 §11.4 的分工要说准：`initial_path` 保证的是**离线也能起来 + 规则可用**，
不是"有本地文件就跳过抓取"。另外 `ERROR network: missing default interface` 这一行
在空路由表的 netns 里出现但不是致命的——以后在 netns 里排错别把它当启动失败。

#### 五、这一节**没有**验的，以及后来在哪儿补的

- **路由层**（`route.rules` 那 16 条）的命中顺序：上面全部是 DNS 层结论。→ **已补，见 §11.10**
  （那一轮就是"对现役内核 `apply` + 重架劫持层 + netns 客户端发真流量"，逐条给了归属判定）。
- 幂等复测（连跑两次 `apply`）属于 §11.7 第 6 条那组。→ **已补，见 §11.10 五**；
  "迁移过的状态再读一次逐字节等价"这条已有单测
  （`state.rs::a_migrated_state_is_written_back_as_v2_and_reads_identically`）。

仍**没有**验的只剩两条：

- `query_type` 与 `rule_set` 出现在同一条规则里的交集（我们目前不这么生成，跳过）。
- 预置过的 tag 命名的 `initial_path` 在完全冷启动（没有 `cache.db`）下到底省不下载。

### 11.10 路由层那 16 条的连接级定案（2026-10-05，对现役内核跑 `apply` 的那一轮）

同一台机、同一个 v10 二进制（guest 上 `sha256sum` = `1d1f18e7…52cb0`，与宿主机
`target/x86_64-unknown-linux-musl/release/zboxd` 逐字节一致）。这轮**动了真机**，
所以先把现场交代清楚：

- 开工前：`current` = `1790765940501-2640bddc7283fd1b`，服务 started，`nft list tables` 空，
  `ip -4 rule` 只有 0/32766/32767；`/var/lib/zbox/state.json` **不存在**
  （这台机的期望状态一直住在 `/root/zbox-rehearse/` 的夹具里，不在状态目录——面板 task #6
  还得回答"谁写第一份 `<root>/state.json`"）。
- 两轮下发：**Round A** = 13 条形状（本地 `.srs`、无广告库、无设备行、`10.99.0.3` 是 Bypass），
  修订 `1790786111827-0da065f0513d3268`；**Round B** = 16 条形状（加广告桩规则集 + 设备级直连行，
  Bypass 行撤掉，于是 `zbox.nft` 里那条 `ip saddr { 10.99.0.3 } return` 也随之消失），
  修订 `1790786637346-84e0bfaf93997e4f`。两轮各有 `sing-box started (0.08s)` / `(0.06s)`。
- 收尾：`disable-hijack` → 逐项复查 → `topo-down.sh` → `rollback` 回原修订，
  回到"内核在跑、劫持层未加载"，`nft list tables` 空、七个监听全在。

#### 一、判据：把日志行数当游标，不拿"打不开"当证据

每条探针先 `wc -l < /var/lib/zbox/sing-box.log`，动作（`ip netns exec lanN wget/nslookup …`），
再 `tail -n +<起点>` 只打新增行——§11.7 早就说过这里 google 本来就打不开，
"失败"不构成证据，**能归到某一条规则上的那几行日志**才是证据。客户端侧的准备：
`/etc/netns/lan1/{resolv.conf,hosts}`（`ip netns exec` 会把这些文件 bind-mount 覆盖宿主 `/etc/`），
resolv.conf 一律写 `nameserver 8.8.8.8`——这样"客户端自己指公网 DNS"这条漏路径也被 nat 链拽回来；
hosts 里塞静态记录（TEST-NET 地址），用来造"DNS 层从没见过这个连接"的场景。

#### 二、16 条逐条判定（编号是 Round B 生成物的实际下标）

| # | 生成的规则 | 探针 | 判定 |
|---|---|---|---|
| R0 | `hijack-dns`，`inbound:[dns-in]` | lan1/lan2 `nslookup www.baidu.com 10.99.0.1` | ✅ `inbound/direct[dns-in]: inbound packet connection from 10.99.0.3:52487` + `dns: exchanged A www.baidu.com … 198.18.0.2` |
| R1 | `hijack-dns`，`inbound:[tproxy-in,redirect-in]`，`protocol:[dns]` | N12：客户端 DNS 指向公网 8.8.8.8 | ✅ 照样进 `dns-in` 并回 fakeip。对照组是 Round A 的 A5：Bypass 客户端同一个查询拿到真答案 `104.244.42.197` |
| R2 | `route`，`source_ip_cidr:["10.99.0.2/32"]` → direct（设备级） | N2 与 A7 是**同一台客户端、同一个目的地**，差别只在有没有这一行 | ✅ 设备行压过名单代理行：A7 是 `outbound/vless[vless-reality]: outbound connection to www.baidu.com:80`（随后 urltest 超时），N2 是 `outbound/direct[direct]: outbound connection to www.baidu.com:80` 且页面真下载成功 |
| R3 | `{"action":"sniff"}` | N4/N5/N6/N7/N8，目的地全是 TEST-NET 的 IP 字面量、没有 fakeip 记录 | ✅ Host 头被嗅出来并驱动域名规则：同一个 `/24` 里 google(203.0.113.10)→direct、github(203.0.113.30)→vless，IP 层面挑不出差别，只能是域名 |
| R4 | `ip_is_private` → direct | N10：`10.98.7.5`（私网、**不在** lan_cidr、必须经路由器转发） | ✅ `outbound/direct[direct]: outbound connection to 10.98.7.5:80`，随后 `connect: no route to host` |
| R5 | `ip_cidr:[lan_cidr]` → direct | — | ❌ **观测不到**。两层遮挡：`10.99.0.0/24` ⊂ `10/8`，R4 先命中；而且同网段目的地根本不经过路由器（见三之三）。只有当 `lan_cidr` 落在非私网段时这一行才可能生效 |
| R6 | `rule_set:[geosite-category-ads-all]` → reject | N6：`www.ads.stub.test` 在用户名单里写的恰恰是**直连** | ✅ 仍被拦（0 秒 `Connection reset by peer`，无任何 outbound 行）⇒ "广告整条压在名单之上"在连接层成立 |
| R7 | 名单直连行（`www.ads.stub.test` + `www.google.com` 被相邻同动作合并成一条） | N4 / N6 | ✅ google 走 direct（压过 R14 的代理默认）；ads.stub.test 那一行被 R6 吃掉——这正是 `domain_rule_never_fires` 该点名的形状 |
| R8 | 名单代理行 `www.baidu.com` → `final_tag` | N3（客户端无设备规则） | ✅ 走 `auto`。baidu 在真 `geosite-cn.srs` 里，若没有这一行就由 R11 送 direct ⇒ 名单压过地理默认，选项 C 那处唯一的语义变更在**连接层**成立 |
| R9 | 名单拦截行 `www.taobao.com` → reject | N7 / A11 | ✅ 静态记录、非 fakeip、DNS 层从未见过它，仍 0 秒断且无 outbound 行 |
| R10 | 名单直连行 `www.qq.com` → direct | — | ❌ 与 R11 同出口，连接层不可鉴别（只能靠 `route.rs:528-749` 那组索引断言守住） |
| R11 | `rule_set:[geosite-cn]` → direct | N8：`www.sogou.com` 静态指到 `203.0.113.40`（非 CN IP） | ✅ geoip 帮不上忙仍走 direct ⇒ 命中的是**域名级** geosite-cn |
| R12 | `rule_set:[geoip-cn]` → direct | N9 / A10：`114.114.114.114` | ✅ `outbound/direct … 114.114.114.114:80`；这一轮 `final` 是 `auto`，所以 direct 只可能来自这条 |
| R13 | `protocol:[bittorrent]` → direct | — | ⏳ 没测：没造 BT 握手。要收它就欠一条 `printf` 拼 68 字节握手 + `nc` 的探针 |
| R14 | `rule_set:[non-cn]` → `final_tag` | N5 | ⚠️ 与 `route.final` 同 tag，**结构上不可鉴别**（见三之一） |
| R15 | `ip_cidr:[fakeip 段]` → `final_tag` | N11 / A12：直连一个没有记录的 fakeip | ❌ 走不到（见三之二），且与 final 同 tag |

#### 三、这轮真正的收获是三条结构性结论

1. **名单的代理行、`non-cn`、fakeip 段快捷方式三处的出口都取 `final_tag`**
   （`route.rs:159`、`:198`、`:205`），而 `route.final` 也是 `final_tag`。
   所以在当前生成形状下，R14/R15 与"落到 final"**结果永远相同**，连接层不可能鉴别；
   它们的实际作用只是"少查一次 geo / 把意图写在脸上"。
   要让它们可观测，模型上得允许"名单行指定自己的策略组"或"final 与代理默认分离"，
   那是面板（task #6）的问题，不是这轮的问题。
2. **fakeip 的反写发生在路由判定之前，而且它会改写日志里的目的地**：目的地在 fakeip 段且有记录 →
   日志里出现的是域名（N2/N3 写 `www.baidu.com:80` 而不是 `198.18.0.2:80`）；
   目的地在 fakeip 段但**没有记录** → sing-box 当场断流并打
   `router: missing fakeip record, try enable \`experimental.cache_file\``，一条 outbound 都没有。
   我们的 `cache_file` 是开的（`store_fakeip:true`），所以这条只在
   "客户端拿着上一代内核/别的机器的 fakeip 缓存"时出现。
   由此：**R15 那条 `ip_cidr:[fakeip 段]` 在两个方向上都碰不到**——有记录时被换成域名交给域名规则，
   无记录时连接已经在反写阶段断了。这条是"N11 的日志里没有任何 outbound 行"加上
   "R15 与 final 同 tag"两件事合起来的推论，不是单独一条正向观测；
   要正向观测得再造一条"目的地是有记录的 fakeip、而它的域名不在任何规则集里"的探针
   （本轮没做，因为结论无论如何都落在 `auto`，改不了路由结果，只改得了日志里印 IP 还是印域名）。
3. **同网段的目的地不经过路由器**：`lan1` 打 `10.99.0.3:80` 时桥接在 L2 直接交换，
   `prerouting` 里没有这个包，于是 A6 **一条日志都没有**（不是"命中了直连"，是"根本没进来"）。
   这条对真实旁路由是好消息（局域网内互访天然不被接管），
   但它同时意味着 R5 的测试必须把目的地做成"要经默认网关才到得了"的地址，
   否则测的是桥而不是 ZBox。

#### 四、#18 欠的那条承诺，在路由层成立

`docs/ARCHITECTURE.md` §4 写"拦截优先"时留了一句：DNS 层回绝不等于连接被拦，
因为客户端有缓存、走 IPv6 或命中静态记录。本轮 N6/N7 就是刻意用 `/etc/hosts` 静态记录
+ TEST-NET 地址造的"DNS 层完全没见过这个连接"，路由层仍按域名拦下 ⇒ 两层的结论真的同向。
一条排错上的坑要记下：**嗅探到的域名不会改写日志里的目的地**（N4 显示
`outbound/direct … 203.0.113.10:80`），只有 fakeip 反写才把目的地换成域名。
所以"日志里是 IP"不能当作"没嗅到域名"的证据——连接面板（task #7）若要显示命中依据，
得自己带 `rule_set`/sniff 的来源信息，不能靠读日志行。

#### 五、幂等：`apply` 连跑两次，劫持层连跑两次

```
网卡形状取自 state.json：局域网 zbox-lan0（段 10.99.0.0/24）、上游 eth0
内容与 1790786111827-0da065f0513d3268 完全一致，未触碰内核（不重启、不断流）
```

第二次 `apply`（这里是 `enable-hijack` 内部那次）打印的就是上面那两行，
`rc-service zbox status` 全程 started——这是"没断流"的实测面。至于"内核进程 PID 也没换"，
本轮**没有**在基线上量过，只能从代码路径与单测得出同一结论：`engine.rs:168-172` 在
`store.write` 之前、也就在任何 `validate` / 重载 / 端口探测动作之前就 `return`
`ApplyOutcome::Unchanged`，单测（`engine.rs:311-323`）断言的正是这条快路径
`h.calls.borrow().is_empty()`——"内容未变不得校验/重载/探测"。
想在基线上直接量 PID 现在做不到：手头的 state 制品渲染出的 `config.json` / `zbox.nft` /
`netstate.sh` 与现役那份 Sep-30 修订逐一比对全部不一致（选项 C 之后生成器换了形状，
现役那份是旧形状），只有 `netstate-down.sh` 相同；所以拿它跑 `apply` 会真改动现役配置，
守卫脚本据此拒绝执行（rc=3）。精确还原现役配置的唯一手段仍是 `rollback <修订号>`。
劫持层连跑两遍之后逐项计数：
`grep -c "tproxy to"` 2、`grep -c "redirect to :53"` 2、`ip -4 rule | grep -c fwmark` 1、
`ip -4 route show table 100` 1 条——与 §11.7 第 6 条一致，形状换了（Bypass 撤掉）也没叠加。
外来的 `table ip zboxtest` 活过了 `disable-hijack`（"只删自己建的对象"复验成立）。

#### 六、`rollback` 不刷新 `revisions/LAST_GOOD`（一条 CLI 缺口）

本轮 `rollback 1790765940501-2640bddc7283fd1b` 之后：`current` 回到了原修订、内核重启成功，
但 `revisions/LAST_GOOD` 里仍写着探针修订 `1790786637346-84e0bfaf93997e4f`。
后果不是当场坏，而是"下一次不带参数的 `rollback` 会把探针配置当已知良好顶上来"。
本轮手工把 `LAST_GOOD` 写回原修订 id。**该不该让 `rollback` 顺手更新 `LAST_GOOD`**
（以及 `apply` 失败自动回滚那条路径是否也更新），留给 task #6/#12 定夺，不是本轮改代码的事。

#### 七、这轮仍欠的

- R13 bittorrent 的协议级探针（造握手）。
- `lan_cidr` 为非私网段时 R5 是否会生效（本拓扑造不出来，得靠真机或改网段规划）。
- systemd 那条线（本轮全是 OpenRC）；§6 的服务制品在 Ubuntu/Debian 上仍只做过静态核对。

### 11.11 面板写出去的策略组在真内核上的形状（2026-10-07，Alpine 3.21.3 / sing-box 1.14.2 `-musl` / 编辑层第四期 A 那一轮）

一、**磁盘形状是套一层的**。`Group` 有一个字段就叫 `kind`，而 `GroupKind` 自己又拿 `kind` 当内部标签键，
于是序列化结果是 `{"tag":"auto","kind":{"kind":"urltest","url":…,"interval":"3m","tolerance":50},
"members":[…],"default":null}`；选择组那一条是 `{"kind":{"kind":"selector"}}`。
写成扁平的 `"kind":"urltest"` 会让**整份** state.json 读不进来（不是那一条被忽略）。
本轮起这条由单测从模型的序列化结果反着钉住：`api::tests::the_panel_s_group_choice_lists_are_the_model_s`
与 `the_group_json_the_panel_writes_is_the_shape_the_model_reads`（后者是拿面板交出去的那份 JSON 走 PUT）。

二、**预跑门这一轮的量法纠正了"看 stdout"的想法**：把 `inbounds` 置空、给一个独立的 `-D` 目录跑 `run`，
实测 `rc=0`（不是被 timeout 打到的 124），stdout 与 stderr 一个字都没有，
而 `sing-box started (0.02s)` 写在 `<-D>/sing-box.log` 里。
原因在生成物自己身上：`config.log.output` 是**相对路径** `"sing-box.log"`，它跟着 `-D` 走
（`check` 用的 `rule_set[].path: "rule-set/non-cn.srs"` 同一条基准）。
所以判"起没起来"要读那个文件，不能只 `cat` 重定向出来的 stdout。

三、**面板建的 urltest 组，内核一起来就真去测速了**。紧随 started 的那一行是
`outbound/direct[direct]: outbound connection to www.gstatic.com:80`——
测速地址由 `direct` 出站发出，不需要任何 inbound，也不碰现役内核。
含全部 inbound 的那一份 `check` 同样 `rc=0`。

四、**兜底出口选成策略组，生成器就硬要 `non-cn` 有声明**：`gen/singbox/route.rs` 只在
`final_outbound` 确实是 Proxy 时 `require_rule_set(RULESET_NON_CN)`（第 193 行那一档，
"境外全部走代理"这条规则本身要用非中国库来判定）。
在面板上的表现是保存**仍然 200**、回执里 `generation_error` 写着
「策略需要规则集 non-cn，但它不在 rule_sets 里——引用未定义的规则集会让 sing-box check 直接失败」，
下发会被拒。本轮为了拿到一份内核肯收的配置，是在「规则集」那一节加了一条**本地** `non-cn`
指向 `rule-set/non-cn.srs`（现役目录里那份 168002 字节的产物，只读复制进临时 `-D`，`/var/lib/zbox` 一个字节没动）。

### 11.12 面板写出去的节点在真内核上，外加 §11.4 那条"没验的冷启动"补上了（2026-10-07，Alpine 3.21.3 / sing-box 1.14.2 `-musl` / 编辑层第五期 `nodes[]` 那一轮）

一、**§11.4 末尾那条"仍然没验的是预置过 tag 命名文件的冷启动"，这一轮验了**。
四份产物按 tag 命名（`geosite-cn.srs`、`geosite-category-ads-all.srs`、`non-cn.srs`、`geoip-cn.srs`）
只读复制进一个全新 `-D` 的 `rule-set/` 里（`cache.db` 当场新建，所以确实是冷启动），
`inbounds: []` 跑 `run`：日志头六行是
`network: updated default interface eth0, index 2` → `network: updated network environment: gateway 192.168.219.2, …`
→ `sing-box started (0.04s)` → 两行 outbound → 7 秒后一行 `ERROR router: fetch rule-set geosite-cn: …`，
**started 之前一行 `load initial rule-set from …: no such file or directory` 都没有**（§11.4 说的那几行预期噪声
在有文件时确实不出现）。含全部 inbound 的那一份 `check` 同样 `rc=0`。

二、**started 不等远端更新**：`(0.04s)` 那一条出现时，抓取还在飞——它的证据是同一份日志里
7 秒后（13:44:33 → 13:44:40）才写出 fetch 那一行。也就是说"`initial_path` 有文件 → 启动期不依赖网络"
这条比 §11.4 原来写的还要强一档：不仅失败不是 FATAL，**成功与否根本不在启动的关键路径上**。
（同一目录里重跑第二次是 `(0.03s)`，量级一致。）

三、**`ERROR router: fetch rule-set …: context canceled` 这一行不能当失败证据**。它落在 `kill` 的那一刻，
是取消；§11.3 那条"一条够不着的远端规则集（且没有本地件）= 启动 FATAL"**没有被这一轮推翻**，
两者的形状本来就该分开读：启动期失败的原文是 `initialize rule-set[N]: …`（§11.3 那段），
运行中被取消才写 `fetch rule-set …: context canceled`。以后拿日志判失败要先看时间戳与 started 的先后。

四、**测速组的启动探测是真出站、真发包，而且拨的就是面板自己建的那两条**：
`outbound/hysteria2[hy2-new]: outbound connection to www.gstatic.com:80` 与
`outbound/vless[vless-reality]: outbound connection to www.gstatic.com:80`。
§11.11 那轮只看到 `outbound/direct[direct]`，因为当时组里只有 `direct` 一个成员——
所以这一行是"浏览器里敲的节点被 1.14.2 内核收下并真的用了"的正面证据通道，
不需要任何 inbound、不碰现役内核（现役 pid 16980 全程在跑）。

五、**这一轮拿生成物用的是新加的 `zboxd render` 子命令**（只把 `config.json` 打到 stdout：不写盘、不校验、
不重启）。它是验收纪律的产物而不是 schema 事实，记在这里是因为**没有它这一步只能靠 `apply`**，
而 `apply` 会重启现役数据面。附带一条与 sing-box 无关但会咬人的：`render` 的 stdout 必须是纯 JSON，
CLI 里任何"网卡形状探得…"这类提示都得走 stderr——混一行中文进去 `check` 就报 JSON 解析失败。
两份产物的差就是那一句：8535 B（含提示）→ 8448 B（纯件）。





