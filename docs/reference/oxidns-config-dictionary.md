# oxidns 配置全典（ZBox 参考）

> 对照版本 v1.6.1（`.tmp/oxidns_release.json`）。来源 `.tmp/oxrepo/oxidns-main/` 源码（权威）与 `.tmp/oxdocs/`；后者中 configuration、api、cli、server、migrate 等多份 mdx 是 280 字节 rate-limit 残档，仅 plugin-reference 大多有真实内容。标 [存疑] = 未从源码确认。
>
> **制品侧（2026-10-09 取回）**：Alpine 用 `oxidns-x86_64-unknown-linux-musl.tar.gz`（6,110,603 B，解出 `oxidns` + `config.yaml` + `LICENSE` + **`webui/`**）。`webui/` 说明**它自带一份管理界面**（Next.js 静态导出，与 abox 同一形状）——ZBox 的 DNS 那一屏要么与它划清界限、要么把它当"上游已有面板"处理，别默认用户会同时看到两套。另有 `-minimal-`（1.9 MB）与 `-standard-`（5.8 MB）两档 feature 裁剪，插件集合不同，**这正是能力探测必须先跑的原因**。

## 1. 定位与插件模型

oxidns：Rust 编写的 mosdns 兼容 DNS 转发/分流器。请求路径 `server -> DnsContext -> sequence(matcher/executor/provider) -> upstream/side effect -> response`。以下清单来自 `src/plugin/*/` 源码目录并经官方 overview 证实：

- **server**：`udp_server`、`tcp_server`（配 cert+key 即 DoT）、`http_server`（DoH，H1/H2/可选 H3）、`quic_server`（DoQ）。
- **executor**：编排 `sequence`/`fallback`；解析 `forward`/`cache`/`hosts`/`arbitrary`/`response`/`redirect`/`client_ip_from_ecs`/`ecs_handler`/`forward_edns0opt`；改写 `ttl`/`ip_selector`/`prefer_ipv4`/`prefer_ipv6`/`black_hole`/`drop_resp`/`reverse_lookup`；观测 `query_summary`/`query_recorder`/`metrics_collector`/`debug_print`/`sleep`；联动 `http_request`/`learn_domain`/`script`/`ipset`/`nftset`/`ros_address_list`/`ros_route`；维护 `upgrade`/`download`/`reload_provider`/`reload`/`cron`。多数受 Cargo feature 门控（`Cargo.toml`/`/api/build` 为准）。
- **matcher**：请求维 `qname`/`question`/`qtype`/`qclass`/`client_ip`/`ptr_ip`；响应维 `resp_ip`/`cname`/`rcode`/`has_resp`/`has_wanted_ans`；上下文 `mark`/`env`/`time`/`random`/`rate_limiter`/`string_exp`；组合 `any_match`/`_true`/`_false`（源码另有 `control.rs` matcher [存疑：对外名]）。
- **provider**：`domain_set`/`dynamic_domain_set`/`geosite`/`adguard_rule`/`ip_set`/`geoip`。

## 2. 配置落点与格式

- YAML，单一入口文件。CLI：`oxidns start -c <path>`（默认 **`./config.yaml`**，相对 `-d/--working-dir`）；`check` 同参。service 安装写死 `/etc/oxidns/config.yaml` + `-d /var/lib/oxidns`（systemd 模板）；Windows 服务默认亦 `config.yaml`。
- **子命令全集（v1.6.1 `full` 构建，2026-10-09 guest 实测 `--help`）**：`start` / `check` / **`build-info`**（打印本构建的 feature 与插件能力清单）/ `export-dat`（从 `.dat` 导出规则成文本）/ **`probe`**（探上游 DNS 连通性与行为）/ `service` / `upgrade`。`oxidns -V` → `oxidns 1.6.1 (full)`。
  - **`build-info` 比 `GET /api/build` 更适合当能力探测的门**：不要求服务在跑、不碰网络、不需要令牌。§7 那条"升级零影响"契约的第 ① 步应改成 `build-info`，`/api/build` 退成"服务已在跑时的复核"。
  - `probe` 是现成的上游健康判据，ZBox 的概览/doctor 那一格不必自己写 DNS 探测。
- **`check` 门的覆盖面（同日实测，每条都先证明确实改到了字节）**：管 YAML 反序列化形状（`this is not yaml at all` → rc=1）、**插件类型是否存在**（`type: nosuchplugin` → rc=1 `Plugin error: Unknown plugin type`）、**跨插件引用**（`entry: nosuchtag` → rc=1 `Dependency error: Invalid plugin dependencies: - plugin 'udp_in' field 'args.entry' references missing plugi…`）。**不管**：`listen` 的地址合法性（写成 `not-an-address` rc=0，那是 `start` 时的绑定错误）、`args` 里的未知键（塞 `bogus_key: 1` rc=0）。
  - **空文件 rc=0**，回执 `Configuration is valid: x.yaml (plugins: 0)`。所以 rc=0 单独不能当验收——**要连 `(plugins: N)` 那个数一起判**，它就在 stdout 第一行，可解析。
  - 错误消息带 `文件:行:列`（`t.yaml:39:11:`），比 dae 的 validate 回执好用；面板上"生成物过不了门"那一格应该把这一串原样显示。
- 顶层键（`src/config/types.rs::Config`，全部可选）：`include`（额外配置文件列表，先加载）、`runtime`、`api`、`log`、`network`、`plugins`。
- `plugins: [{tag, type, args}]`。tag：ASCII 字母数字 `_` `-` `.`，`.` 分段首尾须字母数字，≤64 字符，全局唯一；`qs.exec.*`/`qs.match.*`/`qs.cron.*` 为 quick-setup 保留前缀。
- 热重载：`POST /api/reload`，状态查 `GET /api/reload/status`；链路内亦可 `reload` executor。环境变量展开存在（`config/env_expand.rs`）。

## 3. 参数表（global 层）

| 键 | 类型 | 默认 | 必填 | 含义 |
|---|---|---|---|---|
| `log.level` | string | `info` | 否 | off/trace/debug/info/warn/error |
| `log.file` | string | 无 | 否 | 追加文件输出 |
| `log.rotation` | enum | `never` | 否 | type: never/minutely/hourly/daily/weekly + `max_files` |
| `runtime.worker_threads` | int | available_parallelism | 否 | tokio 工作线程，须 >0 |
| `api.http` | string 或 object | 未配则无 API | 否 | 字符串即 listen 简写 |
| `api.http.listen` | string | 示例 `":9199"` | 是(有 http 时) | 管理 API 监听 |
| `api.http.auth` | {type: basic, username, password} | 无 | 否 | 仅 basic，非空校验 |
| `api.http.ssl` | {cert,key,client_ca,require_client_cert} | 无 | 否 | cert/key 成对；require_client_cert 需 client_ca |
| `api.http.cors.allowed_origins` | string[] | 无 | 否 | 精确匹配 Origin，`*` 放行 |
| `api.http.webui.root`/`.index` | string | 无 | root 非空 | 静态 WebUI 目录（相对 working-dir） |
| `network.outbound.default` | string | 无 | 否 | 默认出站 profile 名 |
| `network.outbound.profiles.<name>` | {resolver, proxy} | 无 | 否 | resolver: `"system"` 或 {nameservers:[{addr,dial_addr?}], ip_version:4|6, timeout, proxy: none|profile}（域名 addr 必须配 dial_addr）；proxy: `none`/`direct`/`{socks5:...}`；udp/doq/h3 nameserver 不能走 profile 代理 |

server 插件 args：`udp_server`/`tcp_server`：`entry`（入口 executor tag，必填）、`listen`（`:port` 绑 `[::]` 双栈）；`tcp_server`/`quic_server` 加 `cert`/`key`（PEM；quic 必填）；`http_server`：`entries: [{path, exec}]`、`listen`、client-ip 头名、TLS [存疑：后两者字段名]。

## 4. sequence 语法

`sequence.args` 为顺序规则数组，每条：`matches`（string 或数组，数组为逻辑与，支持 `!` 取反）+ `exec`（`$tag` 引用、内建控制流或 quick-setup 内联表达式）。逐条向下执行（非首中即断）；控制流：`accept`（终止本 sequence，不造响应）、`return`、`reject [rcode]`（默认 REFUSED，生成响应并停）、`mark 1,2,3`、`jump tag`（子过程）、`goto tag`（单向跳转）。matcher 独立成插件时 `args` 为规则列表；`qname` 元素语法：`full:`/`domain:`(默认)/`keyword:`/`regexp:`、`$provider_ref`、`&/path/file`。

```yaml
# 广告拦截（black_hole.mode: nxdomain/nodata/null/custom(ips)/refused，无 ips 时默认 nxdomain）
- {tag: ad_bh, type: black_hole, args: {mode: nxdomain, short_circuit: true}}
- tag: seq_main
  type: sequence
  args:
    - matches: "qname $ad_domain_set"
      exec: $ad_bh
    - exec: $cache
    # 国内外分流：命中 geosite CN 走国内上游，否则走 global
    - matches: "qname $geosite_cn"
      exec: $forward_cn        # forward.args: {upstreams: [{addr: "223.5.5.5"}]}
    - matches: "!qname $geosite_cn"
      exec: $forward_global
    # 按客户端 IP 分流（client_ip 内联 CIDR，或引用 ip_set provider [存疑]）
    - matches: "client_ip 192.168.1.50"
      exec: $forward_safe
```

`fallback.args`: `primary`(必)、`secondary`(必)、`threshold` ms 默认 0、`always_standby`/`short_circuit` 默认 false；主备竞速，先有效者胜、另一路取消。

## 5. query_recorder（查询记录，SQLite）

config（`src/plugin/executor/query_recorder/mod.rs`）：`path`(必填, db 文件)、`queue_size` 8192、`batch_size` 256、`flush_interval_ms` 200、`memory_tail` 1024（内存环形尾，供 SSE/最新查询）、`retention_days` 7、`cleanup_interval_hours` 1、`reader_concurrency` 2。

记录内容（v2 schema，表 records/traces/trace_steps/question_sets/question_items/meta）：`created_at_ms`、`elapsed_ms`、`client_ip`、questions（`name_lc`,`qtype`,`qclass`）、`req_rd/cd/ad`、`req_opcode`、EDNS、`rcode`、各 section count、完整 answers/authorities/additionals（`name,class,ttl,rr_type,payload_text,payload`）、`error`；**命中路径**在 `trace_steps`：每步 `{sequence_tag, node_index, kind(matcher|executor), tag, outcome}` —— 即 ZBox"DNS 解析记录"视图的规则链。

API（挂 `GET /api/plugins/<tag>/`）：`records`（`cursor` 分页、`limit` 默认 100、`since_ms/until_ms`、过滤 `qname/qtype/client_ip/rcode/matcher_tag/status`；DELETE 清空）、`records/{id}`（含 steps）、`stats/plugins`（按 kind/tag 的 checked/matched/executed/query_share）、`stats/top_clients`、`stats/top_qnames`（limit 20）、`stats/qtype`、`stats/rcode`、`stats/latency`、`stats/timeseries`（buckets 60）、`stream`（SSE）。

### 5.1 回执字段名 —— 2026-10-11 guest 逐字实测（1.6.1 full，不再是存疑）

抓法：`/root/p3b2/raw-list.txt`（列表）与 `raw-4.txt`（单条）是原始字节，`src/svc/dns_api.rs` 的夹具就是抄自那里。

- 列表包络：`{"ok":true,"next_cursor":<字符串|null>,"records":[…]}`。**每一行里没有 `steps`** —— "命中了链上哪几步"只在单条里，所以 ZBox 那一屏必须是两层。
- 行字段（逐字）：`id` `created_at_ms` `elapsed_ms` `request_id` `client_ip` `questions_json` `req_rd` `req_cd` `req_ad` `req_opcode` `req_edns_json` `error` `has_response` `rcode` `resp_aa` `resp_tc` `resp_ra` `resp_ad` `resp_cd` `answer_count` `authority_count` `additional_count` `answers_json` `authorities_json` `additionals_json` `signature_json` `resp_edns_json`。
  - `questions_json` 是**真数组** `[{name,qtype,qclass}]`，不是字符串（名字里带 `_json` 也不代表是字符串——`answers_json` 同理）。
  - `rcode` 与 `error` 是两条独立的路：超时那条 `rcode:null` + `error` 有值；正常那条 `error:null` + `rcode:"No Error"`。**"No Error" 是上游枚举的字面量，带空格**，别当自己的词处理。
  - `created_at_ms` 是 epoch 毫秒（UTC）。面板按**浏览器**时钟格式化，所以机器时钟不准时屏幕上是一个"过去"的时刻——那是时钟的事，不是记录器的。
- 单条包络：`{"ok":true,"record":{…, "steps":[…]}}`；step 字段：`event_index` `sequence_tag` `node_index` `kind` `tag` `outcome`。`node_index` 从 **1** 起，而且**记录器自己不在这一段里**（链上第 0 步就是 query_recorder，steps 从它下面的第一个节点开始）。`outcome` 实测三种：`entered` / `error` / `stop`（文档只写了前两种的一半）；一次失败的解析会把**回程**再记几笔 `error`（实测 6 步里有 3 步是 error），所以"看最后一步"看不出是谁拖失败的。
- 分页：`next_cursor` 是字符串 `"<created_at_ms>:<id>"`，下一页给的是**严格早于**它的批次；到最早时 `next_cursor:null`。
- 参数：`limit` 默认 100、封顶 500，**超出静默取小**（不报错，源码 `query_recorder/api.rs:33`、`:718-724`）；`status` 是**枚举**，只认 `all` / `error` / `has_response` / `no_response`（`:743-753`），别的 400；`qname` 服务端会归一成 FQDN（`example.com` → `example.com.`）。ZBox 这一层把 200 作为自己的上限并**报错**（面板只画得下那么多），静默取小留给上游。

## 6. RouterOS 回写（ZBox 对接主路由核心）

共享 `routeros/transport.rs`：RouterOS API 端点写作 **`<host>:8728`**（所有示例显式带端口 [存疑：缺省端口是否自动补]）；`tls` 对象**存在即启用 API-SSL**：`insecure`(默认 false，跳过校验，与 `ca` 互斥)、`ca`(PEM 路径)、`server_name`(SNI)。

| 字段 | 类型 | 默认 | 必填 | 语义 |
|---|---|---|---|---|
| `address` | string | — | 是 | API 端点 host:8728 |
| `username`/`password` | string | — | 是 | API 登录 |
| `connect_timeout`/`send_timeout`/`receive_timeout` | int 秒 | 5/5/5 | 否 | 须 >0 |
| `tls` | object | 无=明文 | 否 | 见上 |
| `async` | bool | **true** | 否 | true：post 阶段入队不阻塞 DNS 应答；false：等待写完成 |
| `wait_timeout` | duration(如 1500ms) | 8s | 否 | 同步模式等待上限 |
| `queue_capacity` | int | 16384 | 否 | 队列各阶段去重 key 上限 |
| `comment_prefix` | string | `oxi` | 否 | 标记本插件所有行；不得含 `;` `=` |
| `persistent.ips` / `persistent.files` | string[] | 无 | 否 | 永不expire的条目/文件；无对应族的项被忽略(warn) |
| `min_ttl`/`max_ttl` | int 秒 | 60/3600 | 否 | 观测 TTL 钳制，min≤max |
| `fixed_ttl` | int 秒 | 无 | 否 | 覆盖动态条目 TTL；address_list 中 0=不写 RouterOS timeout，route 中 0=永不按时间过期 |
| `cleanup_on_shutdown` | bool | true | 否 | 退出时删除自家条目 |

`ros_address_list` 专有：`address_list4`/`address_list6`（至少一个）。`ros_route` 专有：`routing_table`（必填，专用路由表）、`gateway4`/`gateway6`（至少一个）、`distance` 默认 100、`conntrack_guard` 默认 false（RouterOS conntrack 有活跃连接时延迟删路由）。回写经内部 reconcile/throttle/lease 管线去重限速（`routeros/`）。

## 7. 管理 API

挂载于 `api.http.listen`。**2026-10-11 guest 实测修正**：能打的实际路径**全部带 `/api/` 前缀**，下面这份是从原始响应头核对过的；照文档写的裸 `/healthz`、`/readyz`、`/health` 在这一份构建（1.6.1 full）上一律 404。

- GET `/api/healthz`（`ok`）、`/api/health`（版本、`build_bundle`、`uptime_ms`、`checks`、`plugins{total,servers,…}`）、`/api/build`（本构建插件与依赖图）、`/api/control`、`/api/system`、`/api/config`（配置**文件原文** + mtime + path）、`/api/config/check`、`/api/reload/status`、`/api/logs`（ring 尾）、`/api/logs/stream`（SSE）、`/api/metrics`（feature `metrics`）。
- POST `/api/shutdown`、`/api/restart`、`/api/reload`、`/api/config/validate`；PUT `/api/config`（保存）。升级：POST `/api/upgrade/check`、`/api/upgrade/apply`（互斥信号量）、GET `/api/upgrade/status`。
- 插件路由：`/api/plugins/<tag>/<subpath>`（GET/POST/DELETE 精确 + 前缀），由插件自注册（cache flush、cron、download、matcher、provider、reverse_lookup、query_recorder 等），以 `/api/build` 探测为准。
- **响应形状**：每一条都带 `content-length` 且 `connection: close`，**没有一处用分块传输**（400 与 404 也一样）；头名全小写。按声明长度读满即可，不必等对面关连接。
- **错误有两种形状**：路由不存在 = 纯文本 `404 Not Found`（连 `content-type` 都没有）；参数不对 = JSON `{"ok":false,"code":"limit must be greater than 0"}`。读侧要分得开这两种，否则会把"上游没这条路由"说成"参数写错了"。
- `/api/control` 的 `reload.running_version` 是**加载进内存的那份配置的 SHA-256**：与 `sha256sum <那个文件>` 逐字相同；改了盘上的文件而不重载时它**不动**。所以它是"面板改了、解析器还没换上"这一格的判据（ZBox 的 `dns-records` 视图拿它和"面板这份期望态渲染出来的那份"的同一摘要比）。`reload.status` 实测 `idle`。
- ZBox 的读侧**只**取 health / control / plugins record 的 records·stats 那几条：`/api/config` 给的是配置**原文**（将来含主路由凭据），这一条**永不**转发到局域网面上，也不许出现在面板回执里（`src/svc/dns_api.rs` 顶部那条契约）。

ZBox"升级零影响"契约：①`GET /api/build` 取本构建实际插件路由 → ②对可选路由缺失（404）容忍 → ③改动后 `POST /api/config/validate` → `PUT /api/config` → `POST /api/reload` → `GET /api/reload/status` → ④冒烟（dig + `GET /api/plugins/<tag>/records?limit=1`）。

## 8. 存疑条目汇总

除已核实的 black_hole/qname/sequence/fallback/query_recorder（含 §5.1 那份回执字段表）/ros_* 外：其余 matcher、executor（hosts/cache/ttl…）与 provider（ip_set/domain_set…）的完整 args；http_server TLS/头字段名；无端口 address 是否自动补 8728；`/api/readyz` 是否存在（裸 `/readyz` 实测 404，带前缀那条没打）；`control` matcher 对外名；信号类热重载（仅确认 API/executor reload）。
