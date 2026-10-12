# RouterOS（MikroTik）对接清单：哪些事人手动点，哪些事 ZBox 自动下发

> 产出定位：ZBox 是编排面板，主路由 MikroTik RouterOS 是唯一的数据面落地通道（dae/oxidns 都在旁路由 VM 里）。
> 本文回答一个问题：**ZBox 需要主路由配合的每一件事，到底是"人在 Winbox 里点几下"，还是"ZBox 通过 API 自动下发"**。
> 所有结论逐条标注一手来源；抓不到一手来源的一律写「未核实」，不猜参数名。

---

## 0. 取证基准与方法（先说清我据以得出版本）

| 项 | 内容 |
|---|---|
| 一手来源 | `help.mikrotik.com/docs` 的 **ROS（RouterOS）空间**，即 MikroTik 官方文档站（当前唯一在维护的官方文档；`wiki.mikrotik.com` 已冻结，实测访问会 301 到 docs 站点，页首原文：*"This documentation site has been frozen, no further edits will be made here!"*） |
| 抓取时间 | 2026-10-09 |
| 页面新鲜度 | 本报告引用的页面最后修订时间在 **2022-07 ~ 2026-04-28** 之间（每条引用附修订日期，见附录 §7）；核心页（API/REST/DHCP/DNS/Firewall/User/Services/Routing）均为 2025-07 之后修订 |
| 版本口径 | 文档站描述的是 **RouterOS v7** 当前行为。文中凡官方页印出最低版本门槛的，我都写成 `v7.x+`/`v6.43+` 形式；**v6 的差异除官方页明示者外一律标未核实**（冻结 wiki 与 archive.org 在本次环境均不可达：`curl: archive.org port 80 failed`） |
| 物证 | 全部正文落盘在 `E:\ZBox\.tmp\ros\p_*.txt`（HTML→文本转换保留 CDATA 代码块，命令原文可直接 grep），抓取脚本 `E:\ZBox\.tmp\ros\ros.mjs`（走 Confluence REST `…/rest/api/content/<id>?expand=body.storage`，非 JS 渲染，取到的是官方 storage 原文） |
| 二次来源 | oxidns 源码与文档本地副本 `E:\ZBox\.tmp\oxrepo\oxidns-main`（**GPL-3.0-or-later**，源码文件头逐行标注 `SPDX-License-Identifier: GPL-3.0-or-later`；只作事实参照，不取代码） |

---

## 1. 结论速览

1. **推荐通道：RouterOS API（裸 TCP 8728），自己实现帧编解码。** 理由是三条硬约束逼出来的：① 零新依赖下，8728 的长度前缀帧协议是官方成表规格、约 150 行 Rust 可写完；② 只有它有流式（`listen` + `.tag` 多路复用 + `/cancel`），REST 被官方明写「无 continuous 命令 + 60 秒超时」；③ TLS（api-ssl 8729）在零依赖下做不到（Rust 标准库无 TLS），所以必须靠**源 IP 白名单 + 专用账号 + 只在可信 LAN 使用**来补偿明文密码风险。
2. **最关键的一条：按设备分别指定网关/DNS 是可行的，而且有官方明文依据。** `/ip dhcp-server option` 一节原文给出选项优先级序列 *"radius, lease, server, network"*，而 `/ip dhcp-server lease`（MAC 绑定的静态租约）自带 `dhcp-option=` / `dhcp-option-set=` 属性 —— 即**租约级选项覆盖网络段级 `gateway`/`dns-server`**。配合官方文档化的 `/ip dhcp-server lease send-reconfigure`（服务端主动 forcerenew），"拖动某台设备切档"有落地手段。第二条更立刻生效的手段是 `/routing rule add src-address=<设备IP>/32 action=lookup-only-in-table table=<旁路由表>`（不依赖客户端续约），但它有回环风险，必须实测（见 §5 生死门）。
3. **DNS 侧最硬的一条结论：任何"在 ROS 上劫持 53 端口"的做法都会让 oxidns 看不见是哪台设备在查。** 客户端与旁路由同网段时，`dst-nat` 必须配 hairpin `masquerade`（官方 NAT 页原文解释：客户端会丢弃非预期源地址的应答），masquerade 之后源 IP 变成 ROS 的 LAN 地址。**所以按设备分 DNS 策略只有一条路：客户端 DNS 直接指向旁路由 VM**（DHCP 下发或人自己填）。
4. **非对称路由在 ROS 侧默认不会因反向路径丢包**：`/ip settings` 有 `rp-filter (loose|no|strict; Default: no)`，默认 `no`。真正的杀手是 `connection-state=invalid` 的丢弃规则 —— 官方 Connection tracking 页原文：*"Such a rule set must not be applied on routers with asymmetric routing, because asymmetrically routed packets may be considered invalid and dropped"*，而 Common Firewall 页同时又写 *"We strongly suggest dropping all connection-state=invalid packets in the firewall filter forward and input chains"*。**这两页互相矛盾，必须按旁路由拓扑显式选边**（解法见 §4.3：对被引流设备的相关流走 `/ip firewall raw … action=notrack`）。
5. **人必须做的步骤可以压到 3 步**（§6 说明书）：进 Winbox → 打开 api 服务并绑定旁路由/PC 的来源地址 → 建一个 ZBox 专用账号。其余全部由 ZBox 下发。

---

## 2. A 段：RouterOS 的自动化通道本身

### 2.1 A1 — RouterOS API（TCP 8728 / api-ssl 8729）逐条核实

来源：官方 API 页（2025-02-27 修订）+ 官方 Python3 Example 页（2026-02-10 修订）。物证 `p_api.txt`、`p_py3.txt`。

**端口与服务前提**（原文）：*"API service must be enabled before trying to establish the API connection. By default, API uses TCP:8728 and TCP:8729 (secure)."* 且 *"API-SSL service is capable of working in two modes - with and without a certificate… no certificate is used… an anonymous Diffie-Hellman cipher has to be used"*。

**帧格式（长度前缀，官方成表，可直接照抄实现）**：

- 词 = `<编码长度><词内容>`；长度编码表（原文照录）：

| 长度范围 | 字节数 | 编码方式 |
|---|---|---|
| `0 <= len <= 0x7F` | 1 | `len` 最低字节 |
| `0x80 <= len <= 0x3FFF` | 2 | `len | 0x8000`，两个低字节 |
| `0x4000 <= len <= 0x1FFFFF` | 3 | `len | 0xC00000`，三个低字节 |
| `0x200000 <= len <= 0xFFFFFFF` | 4 | `len | 0xE0000000` |
| `len >= 0x10000000` | 5 | `0xF0` + 4 字节 `len` |

- 原文补充：*"len bytes are sent most significant first (network order)"*；*"If the first byte of the word is >= 0xF8, then it is a reserved control byte… Currently, control bytes are not used"*（→ 解析器必须对 `>=0xF8` 首字节报错断开，否则协议升级时会静默错位）。
- 句子 = 命令词 + 0 或多个属性词 + **零长词结束**。原文：*"Zero-length word terminates the sentence. If it is not provided router will not start to evaluate sent words"*。
- 词的五种前缀（官方分类）：`/` 命令词、`=name=value` 属性词、`.tag=` API 属性词（官方称"目前唯一此类属性是 tag"）、`?` 查询词、`!` 回复词。
- 属性词顺序不可依赖：*"Order of attribute words and API parameters is not important and should not be relied on"*、*"order and count are changeable by .proplist attribute"*。

**回复语义**（原文逐条）：

- `!done`：*"The last reply for every sentence is the reply that has the first word !done"*。
- `!re`：数据回复；`!trap`：错误，带 `=message=`，部分场景带 `=category=`。
- `!empty`：**v7.18+ 新增**，*"Replies of commands which do not have any data to reply with, begin with !empty (introduced in RouterOS 7.18)"* → ZBox 解析器必须把它当"成功但无数据"，不能当错误。
- `!fatal`：*"If the API connection is closed, RouterOS sends !fatal with a reason as a reply and then closes the connection"*。
- `!trap` 的 `=category=` 取值（官方全表）：`0` 缺项/缺命令、`1` 参数值错误、`2` 执行被中断、`3` 脚本相关、`4` 通用失败、`5` API 相关、`6` TTY 相关、`7` `:return` 生成的值。→ **ZBox 的错误分类和重试策略可直接按 category 决定**（`1`/`0` 不重试，`5`/`4` 可重试）。

**登录握手（新旧两版，都有官方一手依据）**：

- 新版（官方原文标 *"Login method post-v6.43"*）：
  ```
  /login
  =name=admin
  =password=<明文>
  <空词>   →   !done
  ```
  原文：*"Password is sent in plain text."* / *"in case of an error, the reply contains =message=error message."*
- 旧版（<6.43）：先发裸 `/login`，`!done` 里带 `=ret=<32位hex challenge>`；官方 Python3 Example 页给出的算法（原文代码）：
  ```python
  challenge = binascii.unhexlify(response[1][5:])
  md.update(b'\x00' + pwd.encode('utf-8') + challenge)
  send_sentence(s, ['/login', '=name=' + user, '=response=00' + md.hexdigest()])
  ```
  即 **`response = "00" + hex(MD5(0x00 ‖ password ‖ challenge_bytes))`**，注意首字节是字面 `0x00`、且 `=name=` 仍要带。API 页的示例报文里也能看到 `=response=005062f7a5ef…` 形态（1 字节 `00` + 32 字节 hex，符合 MD5）。
- **对 ZBox 的直接结论**：只支持 ≥6.43 的明文登录就**完全不需要 MD5 实现**（零依赖友好）；旧版路径明确报"不支持的 RouterOS 版本"即可，因为 v7 设备不可能早于 6.43。风险在于明文密码过网 —— 见 §2.2 的补偿措施（源白名单 + api-ssl 交给 oxidns 那侧）。

**多路复用与 `.tag`**（原文）：*"It is possible to run several commands simultaneously, without waiting for the previous one to complete… If you include the 'tag' parameter with a non-empty value, then the 'tag' parameter with the same value will be included in all responses"*；不传 tag 则回复也不带 tag。→ ZBox 每个在途命令一个 tag，**读侧要能按 tag 分派到不同的 waiter**。

**流式（这是选 API 而不是 REST 的决定性理由）**：

- `/cancel`：*"optional argument: =tag=tag of command to cancel, without it, cancels all running commands… does not cancel itself… all canceled commands are interrupted and in the usual case generate '!trap' and '!done'"*。官方示例里被 cancel 的 listen 先回 `!trap =category=2 =message=interrupted` 再回 `!done`（**两条都要消费，不能把 trap 当失败**）。
- `listen`：*"listen command is available where console print command is available, but it does not have the expected effect everywhere (i.e. may not work). "!re" sentences are generated as something changes in a particular item list; when an item is deleted or disappears… the '!re' sentence includes the value '=.dead=yes'. This command does not terminate. To terminate it, use /cancel command."*
  → **能力**：租约表/ARP 表/address-list/路由表这类"条目列表"可以用 listen 做增量推送，删除事件用 `.dead=yes` 表达。
  → **未核实**：官方没有逐个菜单列出"哪些菜单 listen 真生效"，`/ip firewall connection`、`/interface` 计数器类能不能 listen **必须实测**（§5）。
- `getall` = `print` 别名；`print` 在 API 侧与 console 的差异（原文）：不支持 `where` 位置参数（要用查询词）、`.proplist` 是逗号分隔属性名列表、回复属性顺序不定义、**".proplist is absent, all properties are included … even those that have slow access time (such as file contents and performance counters). Thus the use of .proplist is encouraged. The omission of .proplist may have a high-performance penalty if the =detail= argument is set."** → **轮询必须带 `.proplist`，这是官方式性能警告，不是我推测**。
- 查询词（`?`）：官方给了完整栈语义表（`?name`、`?-name`、`?name=x`、`?<name=x`、`?>name=x`、`?#ops` 及 `!`/`&`/`|`/`.` 与栈索引规则），并明确 **"The order of query words is significant"** 与 **"Regular expressions are not supported in API, so do not try to send a query with the ~ symbol"**。
- OID 读取：`print oid` 在 API 里是把 `xxx.oid` 加进 `.proplist`（官方示例 `/system/resource/print =.proplist=uptime,cpu-load,uptime.oid,cpu-load.oid`）→ 观测面可复用。
- 登录前的发送限制（原文）：*"There is a limit on the number and size of sentences that the client can send before it has logged in."* → 实现要"先登录再发命令"，别做连接复用预热。

**裸 TCP + 自实现可行性判定**：

| 需要的模块 | 是否可行（零依赖） | 依据 |
|---|---|---|
| 长度前缀编解码 | ✅ 官方成表，~40 行 | API 页表格 |
| 词/句子分帧（增量读、零长词收尾） | ✅ | API 页 |
| `=attr=` / `.tag=` / `?query` / `!reply` 词分类 | ✅ | API 页 |
| 按 tag 分派的 in-flight 表 + `/cancel` | ✅ | API 页 |
| `!empty`（v7.18+）容错 | ✅ | API 页 |
| 登录（≥6.43 明文） | ✅ 无需哈希 | API 页 |
| 登录（<6.43 MD5） | ⚠️ 需手写 MD5（无 crate）→ **建议不支持并报明确错误** | Python3 Example 页 |
| api-ssl 8729（TLS，含匿名 DH） | ❌ 零依赖下做不到（std 无 TLS；oxidns 是靠 `mikrotik_rs` + `rustls` 才实现的） | API 页 + oxidns `Cargo.toml`/`transport.rs` |

结论：**API 通道全部可由 `std::net::TcpStream` + 手写帧达成**，唯一被硬约束否掉的是 TLS。

### 2.2 A2 — REST（`/rest/`）与 netconf：核对与选型

来源：官方 REST API 页（2025-08-12 修订），物证 `p_rest.txt`。

- 定义与前提（原文）：*"Starting from RouterOS v7.1beta4, it is implemented as a JSON wrapper interface of the console"*；*"To start using REST API, the www-ssl or www (starting with RouterOS v7.9) service must be configured and running."* → **v7.9 之前只能用 `https://<ip>/rest`（www-ssl）**，这本身就是一个版本分支点。
- 鉴权：*"Authentication to the REST API is performed via HTTP Basic Auth"*，凭证与控制台用户相同。官方明确不建议开明文 HTTP：*"We do not advise enabling HTTP access (www service). The main risk is that authentication credentials can be read with passive eavesdropping."*
- 方法映射（官方表）：`GET`→`print`、`PUT`→`add`、`PATCH`→`set`、`DELETE`→`remove`、`POST`→ 通用（可打 `/rest/execute`，示例 `--data '{"script":"/log/info test"}'`）。
- JSON 约定（原文）：*"In JSON replies all object values are encoded as strings, even if the underlying data is a number or a boolean."* 支持 `.proplist`、`.query` 两个特殊键。
- **REST 缺的能力（决定性）**：
  1. *"There is no option to run a "continuous" command, like "monitor" via REST API. Use "monitor once" parameter instead to print the result once."* → 观测面板无法流式。
  2. *"If the command runs indefinitely, it will timeout and the connection will be closed with an error. The current timeout interval is 60 seconds."* 且 *"Timeout is not affected by the parameters passed to the commands."*
  3. 无 `.tag` 概念（HTTP 每请求独立），并发只能靠多连接，而连接又受 `/ip service max-sessions`（默认 20）约束。
  4. HTTPS 需要证书：*"You have to set up [certificate] to use secure HTTPS, if self-signed certs are used, then CA must be imported to the trusted root."* → ZBox 侧要引入 TLS 验证，零依赖下同样做不到；明文 HTTP 又被官方劝退。
- **netconf：不存在。** 对官方文档站做**全站全文检索** `text~"netconf"` 与 `title~"netconf"` 均 **0 命中**（2026-10-09 实测，脚本 `ft.mjs`），RouterOS 空间亦无任何 NETCONF/YANG 页面。→ 结论：**RouterOS v7 没有 NETCONF，不进入选型**。（若社区有帖提到，属未核实线索，本报告不采信。）

**选型结论**：

| 维度 | API 8728（裸 TCP + 手写帧） | REST `/rest/`（裸 HTTP + serde_json） |
|---|---|---|
| 零依赖成本 | 帧编解码 ~150 行（官方成表） | HTTP client 也要手写（std 无 HTTP）+ chunked/keep-alive，成本不低于帧 |
| 流式/推送 | ✅ `listen`、`!re`、`.dead=yes`、`/cancel` | ❌ 官方明写不支持 continuous |
| 长命令 | ✅ 自定超时 | ❌ 服务端 60s 强杀 |
| 多路复用 | ✅ `.tag` | 每请求独立，受 max-sessions 限制 |
| 加密 | ❌ 零依赖下无法用 api-ssl | ❌ 零依赖下无法校验 TLS（除非明文 HTTP，官方劝退） |
| 版本稳定性 | ✅ 协议自 v6 前至今；新特性向后兼容（`!empty` 7.18） | ⚠️ v7.1beta4 才引入，路径行为在 v7.9 变过 |
| 幂等/回滚 | ✅ `set`/`remove` 显式 `.id`；`.tag` 可关联一批操作做人工回滚记录 | ✅ 同（背后是同一套 console） |
| 官方账号策略位 | `api` | `rest-api` |

**→ ZBox 主通道：API 8728。REST 只作为"人只肯开 www"时的降级/调试入口**（同一套 `/path` 与属性名可复用，只是没有流式）。
**→ 加密需求由 oxidns 侧承担**：oxidns 的 RouterOS 执行器原生支持 `api-ssl`（`tls:` 配置存在即启用 8729，含 ca/insecure/server_name），把"必须 TLS 的写回"交给它，ZBox 不自建 TLS。

### 2.3 A3 — 设备上必须预先手动打开的东西

来源：官方 Services 页（2026-03-30 修订，物证 `p_services.txt`）、User 页（2026-04-15，`p_user.txt`）。

1. **启用 `api` 服务**：`/ip service` 可配置的默认服务名清单（官方）：`telnet, ftp, www, ssh, www-ssl, api, winbox, api-ssl, reverse-proxy`；原文 *"it is not possible to add new services, only existing service modifications are allowed"*。API 页要求 *"API service must be enabled"*。
   - **关键顺序问题**：ZBox 没法"用 API 打开 API 服务"。若 api 服务默认关闭，人必须先用 **Winbox（走 winbox 服务 / TCP 8291）**打开它。官方 Services 页的真机 `print` 示例里 `api 8728` 未带 `X`（X = DISABLED）而 `www-ssl` 带 `X`，可作为"该样本设备上 api 是启用态"的旁证，但**"出厂默认是否启用 api"官方页未明写 → 未核实**，因此 §6 说明书把"确认 api 已勾选"作为第 1 步（一次点击，零风险）。
   - 启用写法：Winbox → IP → Services → 勾选 api；CLI 对应 `disabled=no`（属性名由 `print` 的 `X - DISABLED` 标志位推得，官方示例未直接印出该写法 → **半核实，实测确认**）。
2. **来源 IP 白名单（确切参数名，官方原文照录）**：`address` 属性：*"List of IP/IPv6 prefixes from which the service is accessible. When this parameter is set, packets are not dropped at the network level, but access to the service is denied for sources not matching the specified addresses. This option is best suited for restricting access within trusted networks. To block access from external or untrusted networks, we recommend using a Firewall instead."*
   官方示例（**这就是要照抄的写法**）：
   ```
   /ip/service/set api address=10.5.101.0/24,2001:db8:fade::/64
   ```
   → 参数名是 `address=`，接受逗号分隔的 v4/IPv6 前缀混合列表；`/ip service print where !dynamic` 可校验。
   注意语义差别：**不是网络层丢包，是会话层拒绝**（对扫描器仍表现为端口开放）。
3. **其他可硬化的服务属性**（官方属性表）：`port=`、`certificate=`（仅 www-ssl/api-ssl 有效）、`tls-version (any|only-1.2; Default: any)`、`max-sessions (1..1000; Default: 20)`、`vrf=`。
4. **专用账号与最小权限（这里有个必须承认的坏消息）**：
   - 组的权限位是**全局粗粒度**的。官方 policy 取值全集：`local telnet ssh ftp reboot read write policy test winbox password web sniff sensitive api rest-api romon`，`!` 表示取反。
   - 与 ZBox 相关的最小集合：`api,read,write`（+ 若要让设备跑 ZBox 下发的 `/tool fetch` 事件脚本则再加 `test`）。原文依据：`write` — *"grants write access to the router's configuration, **except for user management**. This policy does not allow to read the configuration, so make sure to enable read policy as well"*；`test` — *"grants rights to run ping, traceroute, bandwidth-test, wireless scan, snooper, fetch and other test commands"*。
   - **要刻意排除的位**：`policy`（用户管理）、`reboot`、`sniff`、`ftp`、`sensitive`、`web`/`winbox`/`ssh`/`telnet`/`local`、`rest-api`（若不用 REST）、`romon`。
   - **无路径级授权**：官方文档没有任何"按 `/path` 授予权限"的机制；User 页原文还警告 *"even the "read" group includes sensitive, reboot, and other important policies, meaning that this group should not be given to untrusted users. For truly limited groups, make a custom group, defining specific policies. **All groups have access to file operations.**"*
     → **诚实结论：ZBox 的 API 账号一旦有 `write`，就等于对整个配置面有写权限。真正的安全边界只有三条：① `/ip service set api address=` 源前缀白名单；② `/user` 的 `address=`（原文：*"Host or network address from which the user is allowed to log in"*）；③ ZBox 侧自身的操作面收敛 + 变更前备份/回滚。** 不要向用户承诺"ZBox 账号只能改 DHCP"。
   - 另注意提权面：`/system script` 有自己的 `policy=` 列表和 `dont-require-permissions` 属性，`/system script run NAME use-script-permissions` 走的是**脚本自带权限**而非调用者权限（官方 Scripting 页演示：脚本 `policy=""` 时 `use-script-permissions` 报 `not enough permissions (9)`，不带该参数则以 admin 权限执行成功）。→ ZBox 若要下发脚本，必须理解这套权限继承，别把"账号没有 write"当成安全边界。
   - 建议的账号写法（属性名全部官方）：
     ```
     /user group add name=zbox-api policy=api,read,write,!local,!telnet,!ssh,!ftp,!reboot,!policy,!test,!winbox,!password,!web,!sniff,!sensitive,!romon,!rest-api
     /user add name=zbox group=zbox-api address=192.168.1.0/24 comment="ZBox panel"
     ```
     （`/user group` 的 `name`/`policy`、`/user` 的 `name`/`group`/`address`/`comment` 均为官方属性；`!` 取反为官方语法。）
5. **不需要装额外包**：API 与 REST 都属于 base/system 能力，官方页只要求"服务开启"，没有任何"安装 xxx 包"的前提（`/ip service` 的服务清单是内置的）。容器/额外包只在走 Syslog-Elasticsearch、ThingsBoard 等第三方采集路径时才出现，与本报告无关。

### 2.4 A4 — API 明确做不到 / 不建议做的事（逐条出处）

| 事项 | 判定 | 一手依据 |
|---|---|---|
| `export` 导出配置 | **API/REST 侧文档互相矛盾，按不可用处理** | Configuration Management 页（2026-04-17）原文：*"An export command can be executed from each menu … **and is available for CLI only**"*；而 REST 页（2025-08-12）示例给了 `curl -X POST .../rest/export --data '{"compact":"","file":"test.rsc"}'`。→ **未核实冲突点**，ZBox 不要把"备份配置"设计成依赖 export；改用"自建期望态 + 逐条 diff 记录"做回滚。 |
| 固件（RouterBOOT）升级 | **技术上可下发，但必须禁止** | Upgrading 页（2026-03-27）：*"To upgrade the bootloader, execute command "/system routerboard upgrade" in CLI, **followed by a reboot**"*；RouterBOARD 页（2026-04-10）示例带交互确认 *"Do you really want to upgrade firmware? [y/n]"*。非交互确认参数名**未核实**。→ 变砖面 + 需要重启，划入人工操作。 |
| RouterOS 版本升级 | **不建议自动** | Upgrading 页：`/system package update` + `install`；*"by clicking the Download&Install, download will start, and after a successful download, **the device will be rebooted**"*；且 `install ignore-missing` 之类行为随版本变化。→ 与 ZBox 的"能力探测 + validate、不锁版"原则一致：升级交人手动，ZBox 只读 `/system package` 版本。 |
| 无线 / 射频 | **不建议由 ZBox 写** | 官方把 wireless 归入需要现场判断的能力（`/interface wireless …`，且 User 页把 `wireless scan` 归入 `test` 权限工具族）。射频参数改错=覆盖退化/断连，且 ZBox 的编排目标不涉及无线。→ 只读展示，不写。 |
| `/file` 写大文件 | **有硬上限，官方给了替代路径** | Files 页（2026-03-17）：*"It is possible to retrieve content and edit files up to **60KB** in size"*；`get contents` 也只到 60KB；更大文件官方只给**分块读**：`/file read file=text.txt offset=2 chunk-size=3`（`chunk-size [1..32768]`）。→ **分块"写"官方无文档**。落配置文件的正确通道是：FTP/SFTP 上传 + 自动导入 —— *"It is also possible to automatically execute scripts after uploading to the router with FTP or SFTP. The script file must be named with the extension **\*.auto.rsc**. Once the commands in the file are executed, a new \*.auto.log file is created"*（Configuration Management 页）。且注意：*"files which you want to be kept after the system reboot/power cycle must be stored within **[flash]** directory… anything outside of it is kept within a RAM disk and will be lost upon reboot."* |
| 控制台专属/交互式命令 | **按不可用处理** | API 页只把 `/login`、`/cancel` 列为"API 专属命令"，并明确 `print` 在 API 里**不支持 `where` 位置参数**（要用查询词）；`import` 需要设备上的文件（见上）；`/file print detail` 类交互输出无 API 等价物。另外官方 User 页把 `ping/traceroute/bandwidth-test/wireless scan/snooper/fetch` 归入 `test` 权限工具族 → 能用但需要额外权限，ZBox 若要用 `/ping` 做可达性探测必须给账号加 `test`。 |
| `reboot` / `reset-configuration` | **禁止**（副作用） | Upgrading 页示例 `>/system reboot` 带 `Reboot, yes? [y/N]` 交互确认；`/system package update install` 亦自动重启。任何"重启类"动作一律只走人工。 |
| 正则查询 | **明确不支持** | API 页：*"Regular expressions are not supported in API, so do not try to send a query with the ~ symbol"*（`/ip dns static` 的正则能力是 DNS 引擎内部特性，不是 API 查询语法）。 |

---

## 3. B 段前置：旁路由拓扑在 ROS 侧的硬前提

| 前提 | 确切 `/path` 与参数 | 说明 / 做错后果 |
|---|---|---|
| 旁路由 VM 的地址必须避开 DHCP 池 | `/ip pool`，属性 `name=`、`ranges=`（*"non-overlapping IP address ranges in the form of: from1-to1,from2-to2,…"*，官方示例 `ranges=10.0.0.2-10.0.0.99,10.0.0.101-10.0.0.126` 就是"排除中间地址"的标准写法） | VM 地址落在池内 → 被租出去 → IP 冲突、旁路由随机失联 |
| 已用地址审计 | `/ip pool used`（read-only：`address`/`info`/`owner`/`pool`；原文 *"For DHCP MAC address from leases menu…"*） | ZBox 可用来校验"某 IP 是否已被占" |
| LAN 侧要有 DHCP 服务器 | `/ip dhcp-server add address-pool=<pool> disabled=no interface=bridge1 name=dhcp1`（官方示例原文） | 注意官方约束：*"DHCP server requires a real interface to receive raw ethernet packets. If the interface is a Bridge interface, then the Bridge must have a real interface attached as a port… **It cannot function correctly on a dummy (empty bridge) interface.**"* |
| 反向路径过滤保持默认 | `/ip settings`：`rp-filter (loose\|no\|strict; Default: no)` | 默认 `no` = 非对称回程**不会**被 RPF 丢；若被设成 `strict`，*"if the interface is not the best reverse path the packet check will fail. By default failed packets are discarded"* → 旁路由直接断网。ZBox 应把 `rp-filter` 纳入"变更前读一次、变更后校验"的项。 |
| ICMP redirect 默认不收 | `/ip settings`：`accept-redirects (yes\|no; Default: no)` | 别指望 VM 用 redirect 把设备"推"回来 |
| ARP 老化/邻居上限 | `/ip settings`：`arp-timeout`（原文：*"Sets Linux base_reachable_time… default from 15s to 45s"*、*"does not represent a time when an ARP entry is removed from the ARP cache (see max-neighbor-entries setting)"*）、`max-neighbor-entries`（*"Since RouterOS version 7.1, the default value depends on the installed amount of RAM"*） | 观测面"设备在线"判定要按 `arp-timeout` 量级设阈值，不能当秒级实时 |
| FastTrack 依赖 | Connection tracking 页：FastTrack 生效条件之一 *"/ip settings" 里 FastPath 与 Route cache 开启*；`/ip settings route-cache (yes\|no; Default: yes)`，原文 *"disabling the route cache, will also disable the fast path"* | ZBox 若为了 mangle 生效而关 route-cache → **主路由转发性能断崖**（这是"切档不生效"最容易被误修的地方） |

---

## 4. B 段：主路由必须配合的每一件事（逐项核实）

### 4.1 B1 — 把局域网设备的网关/DNS 指向旁路由（含"按设备"能力，最重点）

**官方字段名（v7，全部照录 DHCP 页 2026-03-19 修订）**：

- `/ip dhcp-server network`（*"A network indicating subnets that DHCP-server will lease addresses from, among other information, like a gateway, DNS-server, NTP-server, DHCP options"*）：
  `address=`（IP/netmask）、`gateway (IP; Default: 0.0.0.0)`、`dns-server=`（*"Two comma-separated DNS servers…"*；**注意 v7 名字是 `dns-server` 不是 `dns`**）、`dns-none (yes|no; Default: no)`、`ntp-none`、`ntp-server`、`wins-server`、`domain`、`netmask (0..32)`、`next-server`、`boot-file-name`、`caps-manager`、`dhcp-option=`、`dhcp-option-set=`。
  官方示例：`/ip dhcp-server network add address=192.168.88.0/24 dns-server=192.168.88.1 gateway=192.168.88.1`
  ⚠️ **v6 里同一字段是否叫 `dns`/`gateway`：未核实（仅社区来源/记忆，官方 v7 页只印 `dns-server`）** —— ZBox 实现要在"首次连接时读一次属性名"来自适应，别硬编码。
- `/ip dhcp-server lease`（静态租约 = MAC→IP 绑定）：`address=`、`mac-address=`、`client-id=`、`server=`、`lease-time=`、`comment`、`blocked/block-access`、**`address-list=`（*"Address list to which address will be added if the lease is bound"*）**、**`dhcp-option=`（*"Add additional DHCP options from option list"*）**、**`dhcp-option-set=`**、`rate-limit=`、`queue-type=`、`routes=`、`use-src-mac=`。read-only：`status (waiting|testing|declined|offered|bound|authorizing|conflict)`。
  ⚠️ `print` 输出里出现 `HOST-NAME` 列（官方 rate-limit 示例可见），但 lease 属性表**没有列 host-name** → 能否用 `.proplist=host-name` 取到：**未核实，需实测**。
- 菜单专用命令（对"切档立刻生效"极关键）：`make-static (id)`（动态租约转静态）、**`send-reconfigure (id)`**（*"Send Reconfigure (forcerenew) message"*）、`check-status (id)`。
- 服务端开关：`/ip dhcp-server` 的 `use-reconfigure (yes|no; Default: no)`（*"Allow the server to send Reconfigure (forcerenew) messages to clients, prompting them to renew configuration without waiting for their lease to expire"*）、`lease-script=`、`add-arp`、`authoritative`、`delay-threshold`、`conflict-detection`、`lease-time (Default: 30m)`。

**"能不能按设备分别下发不同网关"——能，三条官方支持的路径**：

| 路径 | 确切写法 | 官方依据 | 生效时机 | 风险 |
|---|---|---|---|---|
| **P1 租约级选项覆盖**（推荐做 UI 切档） | ① `/ip dhcp-server option add code=3 name=gw-vm value='192.168.1.2'`（value 类型：`'10.10.10.10'` → Unicode IP 转 hex，官方原表）② 同样加 `code=6`（DNS）③ `/ip dhcp-server lease set <id> dhcp-option=gw-vm` ④ `/ip dhcp-server lease send-reconfigure <id>` + 服务端 `use-reconfigure=no→yes` | 官方原文给出优先级序列：*"Option precedence is as follows: radius, lease, server, network. This is the order in which the client option request will be filled in."* | 客户端续约/forcerenew 时；`force=yes` 可强制下发客户端没请求的选项（原文：*"since RouterOS v7.1rc5 it is possible to force the DHCP option from the server-side even if the DHCP-client does not request such parameter"*，写法 `ip/dhcp-server/option/set force=yes`） | ①**客户端不支持 forcerenew 就要等半租约**（默认 30m → 15 分钟才切）②同 code 是否真"覆盖"官方只给了那一句 precedence，**实测必须验证**（§5 生死门）③多值选项（option 6 两台 DNS）官方只演示了单值与 hex 原值，逗号写法未核实 |
| **P2 池 + 网段分档**（推荐做"固定档"，最稳） | `/ip pool add name=pool-direct ranges=…` / `pool-vm`；`/ip dhcp-server network add address=192.168.1.128/25 gateway=192.168.1.2 dns-server=192.168.1.2` 与 `address=192.168.1.0/25 gateway=192.168.1.1 dns-server=192.168.1.1`；把设备静态租约 `address=` 放进对应半段 | lease `address=`（*"Specify IP address (or ip pool) for static lease"*）+ network `address=` 定义可租子网 | 续约时 | ⚠️ **多条 network 记录如何择优（最具体匹配 / 顺序）官方未明写 → 未核实，需实测**；切档=改静态租约地址，仍需续约 |
| **P3 选项匹配器（按厂商/主机名自动分档）** | `/ip dhcp-server option-set? `→ 官方写法：`/ip dhcp-server …`；菜单属性：`name`、`code (1..254)`、`value`、`matching-type (exact\|substring)`、`address-pool=`、`option-set=`、`server=` | 官方 Option matcher 一节：*"The Option matcher allows to identify DHCP clients by any of DHCP options and assign IP address from specific IP pool."* 示例 `add address-pool=pool1 code=60 matching-type=exact name=test1 server=dhcp1 value=android-dhcp-11`；`code=61`（Client-Id）、`code=12`（Hostname, substring）亦可 | 首次获取地址即分档 | 官方警告：*"it's possible to define two substring matches for the same option… the matcher that will get applied will be selected randomly"*；*"Clients with a static lease will continue to receive their static address, even when matched by the option matcher."* → **静态租约优先于 matcher**，分档设计要想清楚 |
| **P4 完全不依赖 DHCP（服务端策略路由，秒级）** | `/routing table add name=via-vm fib` + `/ip route add dst-address=0.0.0.0/0 gateway=192.168.1.2 routing-table=via-vm` + `/routing rule add src-address=192.168.1.50/32 action=lookup-only-in-table table=via-vm` | Policy Routing 页 + `/routing rule` 参考页（2026-03-12）；v6→v7 迁移页原文：*"the routing table **must be added to the /routing table menu before** actually referencing it… And **fib** parameter should be specified if the routing table is intended to push routes to the FIB"*、*"the routing rule configuration is the same except for the menu location (instead of **/ip route rule**, now it is **/routing rule**)"*、`/ip route add dst-address=8.8.8.8 gateway=172.16.2.1@main routing-table=myTable` | **立即**（不依赖客户端任何配合） | ⚠️ **回环风险**：设备网关仍是 ROS，包被 rule 送到 VM，VM 又把它转回 ROS，而 src-address 依旧命中同一条 rule。官方无防回环条款 → **必须实测**；破解办法（VM 侧 SNAT / `connection-state=new` 限定 / `min-prefix`）都属推断。`/routing rule` 可用属性官方全表：`action(drop\|lookup\|lookup-only-in-table\|unreachable)`、`chain=`、`dst-address=`、`src-address=`、`interface=`（in-interface）、`routing-mark=`、`table=`、`min-prefix=`、`disabled`、`comment`。表数上限 *"Routing table count is limited to 4096 unique tables"* |

**"UI 上拖动某台设备切档"的落地建议（把上面的核实结果合成）**：

- 稳态：**P1/P2 决定"设备认为自己的网关/DNS 是谁"**（长期、无回环、ROS 无转发负担，但生效慢）。
- 即时：**P4 做"立刻生效"的覆盖层**，同时 ZBox 必须提供"切档后提示：最迟 X 分钟生效 / 点『强制续约』发 forcerenew"（`send-reconfigure` 是官方命令，可下发）。
- 分组语义用 address-list 承载：静态租约的 `address-list=` 在绑定成功时自动把设备 IP 加进列表（官方原话），于是"档=列表"，防火墙/路由规则都能引用 —— **这是 ROS 侧唯一被官方文档化的"设备组"原语**。
- 事件推送（避免轮询）：`/ip dhcp-server set <name> lease-script=<script>`，官方 Scripting 页列出的脚本变量：`leaseBound`、`leaseServerName`、`leaseActMAC`、`leaseActIP`、`lease-hostname`、`lease-options`；脚本里可用 `/tool fetch http-method=post http-data=… url="http://<zbox>:<port>/ros/dhcp-event"`（`/tool fetch` 属 `test` 权限族）。→ **设备上下线可以做到零轮询**。

**做错的后果（务必写进 UI 提示）**：
- `gateway=` 写成不可达地址或 `dns-server=` 写成 WAN 侧地址 → 全 VLAN 设备瞬间断网/解析失败，而 DHCP 客户端不会重试到别的档；ZBox 必须"改前读回原值 + 保存回滚点 + 校验 gateway 在 LAN 子网内且能被 ARP 解析"。
- 忘记 `dns-none`：官方原文 *"By default, if there are no DNS servers configured, then the router dynamic DNS Servers from IP>DNS will be passed to DHCP clients"* → 只清 `dns-server=` 不置 `dns-none=yes`，ROS 仍会把上游 DNS 下发下去，切档"看起来失败"。这是最容易踩的一个。

### 4.2 B2 — 不依赖 DHCP 的引流方式（dst-nat / mangle / 各方案代价）

| 手段 | 确切写法 | 官方依据 | 代价（关键） |
|---|---|---|---|
| **劫持 53 到旁路由** | `/ip firewall nat add chain=dstnat protocol=udp dst-port=53 action=dst-nat to-addresses=192.168.1.2 to-ports=53`（+ `dst-address=!192.168.1.2` 排除自己）；TCP 53 同理 | NAT 页：`dst-nat - replaces the destination address and/or port… with values specified by to-addresses and to-ports`；`to-ports` 适用于 `dst-nat/redirect/masquerade/netmap/same/src-nat`。Packet Flow 页确认 **本地目的流量也走 dstnat**：*"A very similar process happens when a packet's destination is a router (routing input): Packet enters prerouting processing → RAW prerouting → Mangle prerouting → **NATs dst-nat chain** → routing decision"* | **同网段必须 hairpin**：官方 NAT 页原文 *"the client receives the reply packet, but it discards it because it expects a packet back from 172.16.16.1, and not from 10.0.0.3"*，修复写法 `add action=masquerade chain=srcnat dst-address=<VM> out-interface=<LAN> protocol=udp src-address=<LAN网段>`。→ **masquerade 之后 oxidns 看到的源全是 ROS 的 LAN 地址，"按设备 DNS 策略"直接失效**。这是本项最贵的代价 |
| **mangle 强制下一跳** | `/ip firewall mangle add chain=prerouting … action=route …` | Mangle 页 action 表原文：*"route - forces packets to a specific gateway IP by ignoring normal routing decisions (**prerouting chain only**)"* | ⚠️ **该 action 的配套参数名（gateway= / routing-mark= 等）官方 Mangle 页未列表 → 未核实**，实现前必须 `print detail`/Tab 补全实测。另有硬约束：*"Do not apply any other routing marks besides "main" for the packets processed by FastTrack, since **FastTrack can only work in the main routing table**"*，以及 v6→v7 页的坑：*"be careful not to mark "local" traffic… router's ip addresses will not be reachable"*，修法原文给的两种：mangle 规则加 `!dst-address-type=local`，或把 `/routing rule` 首条 `action=mangle` 移到 `action=lookup table=local` 之后 |
| **策略路由（同 P4）** | `/routing table` + `/ip route routing-table=` + `/routing rule` | 见 §4.1 | 有 FastTrack/main 表限制；`lookup-only-in-table` 官方警告：*"If for some reason the gateway used in our table goes down, the whole lookup will fail and the destination will not be reachable. In active-backup setups we want the traffic to be able to fall back to the main table. To do that change the action from lookup-only-in-table to **lookup**"* → **ZBox 默认应该用 `action=lookup`（可回落 main），把 `lookup-only-in-table` 留作"强制"选项**，否则旁路由一宕机就是全网断 |
| **每台设备自己填** | 无 ROS 配置 | — | 官方 DNS 页把"不干扰 DNS 配置"列为三种 LAN DNS 管理方式之一（*"do not interfere with DNS configuration"*）→ 零风险但**不可编排**，只作兜底/单台试验 |

**「一次配置、后面全自动」推荐方案（含代价，坦白写）**：

> **1 次性：DHCP 全局默认档 = 旁路由**（`/ip dhcp-server network set <id> gateway=<VM> dns-server=<VM> dns-none=no`，另把 VM 地址从 pool 排除），
> **之后全自动：切档 = 改"租约级选项(P1)"+ `send-reconfigure`，需要秒级时叠加 `/routing rule`(P4，`action=lookup`)。**
>
> 代价三条，必须如实告诉用户：① 全局默认档一旦旁路由宕机 = **全网断**（缓解：旁路由 VRRP/双实例，或 P4 用 `action=lookup` 允许回落 main；ROS 侧没有"DHCP gateway 健康探测"这种官方特性）；② DHCP 生效有租约延迟（官方：*"The client will try to renew this address after half of this time"*，默认 `lease-time=30m` → 最坏 15 分钟；靠 `send-reconfigure` 缩短，但依赖客户端支持）；③ 走 P4 时 FastTrack/硬件卸载与策略路由互斥，被切档设备的流量会掉到慢路径（官方：FastTrack 只认 main 表 + *"Queues (except Queue Trees parented to interfaces), firewall filter, and mangle rules will not be applied for FastTracked traffic"*）。

### 4.3 B3 — 非对称路由：ROS 侧需要配合的地方

**结论汇总（每条都有官方原文）**：

| 议题 | 事实 | 出处 |
|---|---|---|
| ROS 会不会因反向路径丢包 | 默认**不会**：`/ip settings rp-filter` 默认 `no`。`strict` 才会 *"if the interface is not the best reverse path the packet check will fail. By default failed packets are discarded"* | IP Settings 页（2026-03-20） |
| 真正的丢包来源 = `invalid` 丢弃规则 | 官方给出互斥的两句话：Connection tracking 页 *"**Such a rule set must not be applied on routers with asymmetric routing**, because asymmetrically routed packets may be considered invalid and dropped"*（指 `connection-state=invalid action=drop` + `established,related,untracked accept` 那一套）；Common Firewall 页 *"We strongly suggest dropping all connection-state=invalid packets in the firewall filter forward and input chains"*，并解释 invalid *"will not participate in NAT (as only connection-state=new packets do), and will still contain original source IP address when routed"* | Connection tracking 页（2026-04-08）/ Common Firewall Matchers 页（2026-03-10） |
| 官方给的规避手段 | RAW 表 notrack：`/ip firewall raw add chain=prerouting … action=notrack`（原文 *"notrack - do not send a packet to connection tracking. Useful when you still need to use regular firewall, but do not require connection tracking"*；示例 `add chain=prerouting protocol=ospf action=notrack`）；且 *"Such a rule would make all forwarded traffic bypass the connection tracking, improving packet processing speed"* | Filter 页 / Connection tracking 页 |
| fasttrack 与 mangle 冲突 | FastTrack 旁路清单（原文逐条）：*"firewall, connection tracking, simple queues, queue tree with parent=global, IP accounting, IPSec, hotspot universal client, VRF assignment"*；且 *"Queues… firewall filter, and mangle rules **will not be applied** for FastTracked traffic"*；生效条件：*"no mesh, metarouter interface configuration; sniffer, torch, or traffic generator is not running; /tool mac-scan is not actively used; /tool ip-scan is not actively used; FastPath and Route cache is enabled under IP/Settings"*；生命周期 *"Connection is FastTracked until the connection is closed, timed-out, or router is rebooted"*；默认配置里的写法 `add chain=forward action=fasttrack-connection connection-state=established,related` + 同条件 `action=accept` | Connection tracking 页 |
| 直连流量（VM 发出）会不会被 ROS 丢 | **默认不会**：VM→ROS 的包在 ROS 上是 forward + `srcnat/masquerade`（出厂默认对 LAN 出 WAN 做 masquerade，见 Default configurations 页对默认配置的说明 *"WAN port is protected by firewall"*/`/ip firewall nat` 由 Quick Set 生成），回程由 ROS 撤销 NAT 后走 LAN 连接路由直达设备；`rp-filter=no` 不参与判定。**但**：ZBox 必须校验三件事 —— ① 目标设备的 **forward 链没有被 invalid 规则覆盖**（§4.3 第 2 行）；② 被引流设备不要同时命中 fasttrack 规则（否则 mangle 的 mark 不生效，"切档无效"）；③ conntrack 容量：read-only `max-entries`（*"depends on the installed amount of RAM"*）与 `total-entries`，官方在 invalid 定义里点名 *"in case of a resource over usage on the router"* 会产生 invalid | Connection tracking 页 / Default configurations 页 |
| `/ip dns allow-remote-requests` | 与"直连流量丢弃"无关；它只决定 ROS 自己是否响应远端 53（见 §4.4） | DNS 页 |
| routing mark / lookup 的配合 | v7 必须先建表；`gateway=<next-hop>@<table>` 递归解析写法；`/routing rule` 的 `chain=` 可把用户规则插到内建决策之后（原文 *"If chain name is set the same as one of the built in routing decision names, then user created rules are added right after that routing decision. For example, if chain="mangle"…"*）；`min-prefix` 等价 Linux `suppress_prefixlength`（*"to suppress the default route in the routing decision set the value to 0"*）→ **这是官方给的"旁路由挂了自动回落"工具** | /routing rule 页、v6→v7 页、Policy Routing 页 |
| 桥接侧的可见性 | *"A single packet copy will also reach a bridge input chain as the bridge interface itself is one of the many destinations. **When using vlan-filtering=yes, packets that are not allowed due to the "/interface bridge vlan" table, will be dropped at this stage.**"* → 旁路由若不在允许 VLAN 里，是在 bridge 阶段丢，不在 firewall 阶段 | Packet Flow 页（2025-10-31） |

**给 ZBox 的操作约束**：改引流相关规则前，先读 `/ip firewall filter print`（找 `connection-state=invalid action=drop`）与 `/ip firewall filter` 里的 `fasttrack-connection` 规则，并在 UI 上把"这两条会让切档看起来没生效/让旁路由断流"显式呈现；对每条被引流规则**自动配套生成一条 raw notrack**，而不是建议用户删默认规则。

### 4.4 B4 — DNS 走向与 oxidns 的结果回写通道

**ROS 自身 DNS 需要动的地方（官方 DNS 页 2026-03-23）**：

- `allow-remote-requests (yes|no; Default: no)` — *"Specifies whether to allow router usage as a DNS cache for remote clients. Otherwise, only the router itself will use DNS configuration."* 官方还警告：*"When DNS server allow-remote-requests are used make sure that you limit access to your server over TCP and UDP protocol port 53 only for known hosts."*
  → 旁路由路线里 ROS 不必当 DNS；若出厂配置为 `yes`（Quick Set 常会开），ZBox 应提供"关闭 53 对外应答"的收敛动作，避免与 oxidns 抢职责。
- `servers (list of IPv4/IPv6 addresses@vrf)`、read-only `dynamic-server`（*"List of dynamically added DNS servers from different services, for example, DHCP"*）；官方择优规则原文：*"Servers are processed in a queue order - static servers as an ordered list, dynamic servers as an ordered list… If you use only dynamic servers, the returned results can change after reboot"*、*"When both static and dynamic servers are set, static server entries are preferred"*。→ ROS 自己解析（比如 DoH 域名、下载升级包）走哪台，ZBox 要能显示"静态/动态混用"这个不确定性。
- **v7 的 DoH（只到客户端形态，且互斥）**：`use-doh-server=`（*"Supports only one DoH server"*、*"must be specified with an "https://" prefix"*、*"DoH functionality overrides "servers" usage if specified"*、*"If DoH server is being used then it will be the only DNS service working at the time and standard DNS servers from IP/DNS servers list will not be used"*）+ `verify-doh-cert=` + `doh-timeout`/`doh-max-*`；**官方没有给"DoT（DNS over TLS）上游"的配置位**（该页只在 DoH 段出现 doh 前缀属性）→ 结论：**v7 有 DoH、无 DoT**（DoT 未在文档中出现）。
- 静态记录 `/ip dns static`：`name`/`regexp`/`address`/`type (A|AAAA|CNAME|FWD|MX|NS|NXDOMAIN|SRV|TXT)`/`ttl`/`match-subdomain`/`forward-to`/`disabled`/`comment`，**以及 `address-list=`**（见下）。正则写法官方示例：`/ip dns static add regexp="[*mikrotik*]" address=10.0.0.2`；官方警告 *"Regexp is case-sensitive, but DNS requests are not case sensitive… write regex only with lowercase letters"*、*"Regular expression matching is significantly slower than plain text entries"*、CLI 加正则可能要转义：*"Be careful when you configure regex through mixed user interfaces - CLI and GUI. Adding the entry itself might require escape characters when added from CLI."*
- 缓存：`/ip dns cache`、`/ip dns cache all`、`/ip dns cache flush`（官方菜单专用命令，API 可执行）。

**"oxidns 把结果写回 ROS"这条通道 —— 核实结论：存在，而且落点就是 `/ip firewall address-list`。**

1. ROS 侧原生就有这个特性（官方 DNS 页 `address-list` 属性原文）：*"Name of the Firewall address list to which address must be dynamically added when some request matches the entry. **Entry will be removed from the address list when TTL expires.**"*，配套 `/ip dns address-list-extra-time (time; Default: 0s)` = *"Extra time added to TTL when creating address list entry"*。
   → 但这条要求 **查询经过 ROS 自己的解析器**，与"客户端 DNS 直指 oxidns"的拓扑冲突（除非 oxidns 把 ROS 当上游，ROS 再把 oxidns 当上游 = 回环，不可行）。
2. 因此真正的写回通道是**直接写 address-list**。Address-lists 页（2026-04-01）给出的可写属性完全够用：`list=`、`address=`（*"A single IP address or range of IPs… or **DNS Name**"*，支持 `192.168.0.0-192.168.1.255` 会自动收敛成 /23）、`timeout=`、`comment=`、`disabled=`；read-only：`dynamic=`、`creation-time=`。持久性原文很关键：*"If the timeout parameter is not specified, then the address will be saved to the list **permanently on the disk**. If a timeout is specified, the address will be stored on the **RAM and will be removed after a system's reboot**."*
   → **确切写法（API 侧）**：`/ip/firewall/address-list/add =list=<名字> =address=<IP或网段> =timeout=<秒数> =comment=<前缀>`。列表无需预建，add 即成；**名字冲突问题**：`list=` 是自由字符串、多个 list 同名即为同一列表，官方无"唯一性"约束，所以 ZBox 的命名必须自带命名空间（建议 `zbox-<档名>` / `oxidns-<规则>`），并用 `comment=` 前缀标记归属 —— oxidns 就是这么做的（其 `ros_address_list` 插件默认 `comment_prefix: "oxi"`、`address_list4`/`address_list6` 指定列表名、`min_ttl=60`/`max_ttl=3600` 钳制 TTL、`cleanup_on_shutdown` 默认清理自己拥有的项）。
3. ** oxidns 已经把这轮子实现完了（不用 ZBox 重造）**：本地副本证据
   - `src/plugin/executor/ros_address_list/api.rs`：命令常量 `/ip/firewall/address-list/{print,add,set,remove}` 与 `/ipv6/firewall/address-list/*`，属性常量 `list/address/timeout/comment/.id`，并用 `.proplist` 收窄返回字段。
   - `src/plugin/executor/ros_route/api.rs`：`/ip/route/{print,add,set,remove}`、`/ipv6/route/*`，属性 `dst-address/routing-table/gateway/distance/comment/disabled`，并读 `/ip/firewall/connection` 用于清连接。
   - `src/plugin/executor/routeros/transport.rs`：默认 `127.0.0.1:8728`，`tls:` 存在即切 **api-ssl(8729)**，指数退避 `1..30s`。
   - oxidns 自带文档（`docs/docs/plugin-reference/executor/integrations.mdx`）的运维建议：*"该账户需要具备读取和维护目标 address-list 的权限"*、*"建议为 OxiDNS 使用专用且规模可控的 address-list，不建议接入已有的大型共享列表"*（慢列表查询的性能顾虑）。
   → **架构结论**：把"DNS 结果 → 主路由策略"这条链路交给 oxidns 的执行器（它有 TLS、有重试、有清理），ZBox 只做**配置编排**（生成 oxidns 的 YAML、校验 ROS 侧列表名/服务开启）。这样 ZBox 不必为高频写回引入 TLS 依赖，也不与"零新依赖"打架。
4. ROS 侧这条通道的**前置条件清单**（逐条有出处）：`api`（或 `api-ssl`）服务开启 + `address=` 放行旁路由 IP；账号有 `read,write,api`；网络可达 8728；列表名命名空间约定；`timeout` 语义（RAM 存储、重启即失 → ZBox 要在 UI 上说明"主路由重启后动态列表清空，需要 oxidns 重新填充"）。
5. **不存在 nftset/ipset 通道**：RouterOS v7 官方文档没有暴露任何 nftables 集合/`ipset` 对象给用户写（`/ip firewall address-list` 是唯一的动态地址集合原语；`/ipv6 firewall address-list` 对应 IPv6）。→ 任何"往 ROS 里塞 nft set"的设想都应落到 address-list。

### 4.5 B5 — 观测面：UI 每个字段来自哪个 ROS path

**能力边界先说清**：可流式（API `listen`）的官方语义只承诺"console print 可用处""未必生效"（§2.1）→ 下表"流式"列区分三档：`可尝试(需实测)` / `仅轮询` / `不适用`。轮询代价官方有明示：不带 `.proplist` 会把 *"slow access time (such as file contents and performance counters)"* 的属性全取一遍。

| UI 字段 | ROS path | 属性（官方名） | 流式 | 备注 / 做错代价 |
|---|---|---|---|---|
| 设备列表（谁在网内） | `/ip dhcp-server lease` | `address`, `mac-address`, `server`, `status(waiting\|testing\|declined\|offered\|bound\|authorizing\|conflict)`, `dynamic`(RO), `comment`, `address-list` | 可尝试(需实测) | `host-name` 在 print 列出现但属性表未列 → `.proplist` 能否取到**未核实** |
| 设备在线/存活 | `/ip arp` | `address`, `interface`, `mac-address`, read-only `status (delay\|failed\|incomplete\|permanent\|probe\|reachable\|stale)`, `complete`, `dynamic`, `invalid`, `dhcp` | 可尝试(需实测) | 老化取决于 `/ip settings arp-timeout`（默认 30s → 有效 15~45s）；"stale ≠ 离线"，UI 文案别写死 |
| 设备身份增强（型号/邻居） | `/ip neighbor` | `interface`, `address`, `address6`, `mac-address`, `identity`, `version`, `board`, `age`, `discovered-by (cdp\|lldp\|mnds/mndp)`（**v7.7+**） | 仅轮询 | 官方明确"the list is **read-only**"；前提是发现协议开着：`/ip neighbor discovery-settings set discover-interface-list=…`（默认 `static`）、`discover-interval`（**v7.16+**，5s..9h6m8s，默认 30s）→ **默认只发现 static 接口所属设备，VM 若走动态地址就看不到**，这是常见误判点 |
| 该设备当前档 | ZBox 自有状态 + 回读 `/ip dhcp-server lease`（`dhcp-option`/`dhcp-option-set`）与 `/routing rule`/`/ip dhcp-server network` 校验 | — | — | ROS 不记录"我想给它的档"，只记录"实际下发的选项"；UI 要区分**期望态/实际态**并显示 diff |
| LAN 侧总流量（上下行） | `/interface print stats` / `stats-detail` | `rx-byte`, `tx-byte`, `rx-packet`, `tx-packet`, `tx-queue-drop`, `fp-rx-byte`, `fp-tx-byte`（Fast Path 计数），`link-downs`, `last-link-down-time` | 仅轮询（计数器只能差分） | 官方：*"The values starting with "fp" indicates the Fast Path counters"* → 卸载流量在 fp-* 里，两者都要读，否则"面板显示 0 bps 但网很卡" |
| 实时速率（瞬时） | `/interface monitor-traffic` | 输出 `rx-bits-per-second`, `tx-bits-per-second`, `fp-*-per-second`, `tx-queue-drops-per-second` | 连续命令 | 官方文档只给了 console 用法 `> /interface monitor-traffic [find]`；**API 下 `once=`/`interval=` 参数名未核实**；REST 明确"不支持 continuous" |
| 活跃连接（按设备） | `/ip firewall connection` | 打印列 `PR..(protocol)`, `SRC-ADDRESS`, `DST-ADDRESS`, `TCP-STATE`, `TIMEOUT`，flags `S - seen-reply`, `A - assured` | 可尝试(需实测)，**建议轮询** | 表项数量级大；容量看同页 read-only `max-entries`（依赖 RAM）与 `total-entries`；**别不带 `.proplist` 轮询** |
| 地址占用/冲突排查 | `/ip pool used` | `address`, `info`(DHCP=MAC), `owner`, `pool` | 仅轮询 | 校验旁路由 IP 未被占用 |
| 按设备流量归因 | `/ip traffic-flow` + `/ip traffic-flow target` + `/ip traffic-flow ipfix` | 全局：`interfaces`, `cache-entries`, `active-flow-timeout(默认30m)`, `inactive-flow-timeout(默认15s)`, `packet-sampling`(**v7**), `sampling-interval/space`；target：`dst-address`, `port`(默认 2055/UDP), `src-address`, `version (1\|5\|9\|IPFIX)`, `v9-template-refresh`, `v9-template-timeout` | UDP 推流（不是 ROS API） | **致命限制原文**：*"Traffic Flow can process only that traffic which is processed by the router CPU, thus **HW offloaded traffic will not be seen** in Traffic Flow flows (for example, HW offloaded bridged traffic)"* → 旁路由路线里"设备↔VM"的流量在硬件桥接时根本不可见。要做就得 ZBox 侧收 NetFlow v9/IPFIX（UDP 裸收 + 手写模板解析，非零依赖友好） |
| 历史曲线（CPU/内存/接口/队列） | `/tool graphing` + `/tool graphing interface` + `/tool graphing queue` + `/tool graphing resource` | `store-every (24hours\|5min\|hour)`, `page-refresh`, `allow-address`, `store-on-disk`, `interface(all\|name)`, `simple-queue` | 仅轮询/取页面 | 官方取数方式是 **HTTP 页面** `http://<router_IP>/graphs/`（依赖 www 服务），非 API 结构；官方劝告：*"We suggest not storing graphs on disk for devices with small built-in memory"* → ZBox 宁可在自己侧存时序，也不开 graphing 落盘 |
| 主机可达性事件（可控探测） | `/tool netwatch` | `host`(ipv4/ipv4@vrf/ipv6/domain), `type (icmp\|tcp-conn\|http-get\|https-get\|dns\|simple)`, up/down 脚本；官方：*"Since 7.4, Netwatch functionality has been expanded, prior versions only support simple ICMP probes"* | 事件→脚本→`/tool fetch` 推给 ZBox | 主动发包探测，规模大时是网络噪声源；**旁路由场景优先用 lease/arp，不要拿 netwatch 当主力** |
| 认证类客户端（HotSpot 部署） | `/ip hotspot active` | `uptime`, `session-time-left`, `bytes-in/out/total`, `limit-bytes-*`；官方"the menu is informational (**read-only**)… except user can be logged out with the remove command" | 可尝试(需实测) | 软路由家庭网一般无 HotSpot，列为"存在才展示" |
| 主路由健康 | `/system resource` | 官方 API 页示例可确认的属性：`uptime`, `cpu-load`（+ 对应 `.oid`） | 仅轮询 | 其余属性名（内存等）本次未逐条核对 → 取属性前先 `print` 一次拿真实键名，别硬编码 |
| 变更审计/排错 | `/log`（`/log print`） | — | `/log` 可 listen（**未核实**） | Console 页把 `/system history` 举例为"只读项列表"→ 可作为"谁改了什么"的补充源，API 下可读性未核实 |

**轮询的代价（写给实现者）**：① 每轮都要带 `.proplist`（官方性能警告原文）；② 会话数受 `/ip service max-sessions`（默认 20）约束，ZBox 应复用单连接 + 多 tag，而不是每类数据一条连接；③ 连续型数据（monitor-traffic）在 API 下是"不结束的句子"，必须能被 `/cancel` 掉，否则连接上会留僵尸 in-flight（官方：cancel 不带 tag 会 *"cancels all running commands"*，所以**每个 tag 单独 cancel**）；④ conntrack/arp 这类高频表在大 LAN 上 print 会显著变慢 → 分层刷新（面板打开时才拉，且按设备 `?src-address=…` 过滤，查询词是官方支持的过滤手段）。

---

## 5. 必须实测才能定稿的行为（生死门清单）

官方文档给不出结论、且直接决定 ZBox 能不能自动化的点（按优先级）：

1. **D1-a：租约级 `dhcp-option` 是否真的覆盖 network 级 `gateway`/`dns-server`。** 官方只有那一句 precedence（radius>lease>server>network）。做法：下发 `code=3` 到某静态租约，`send-reconfigure`，从设备侧读回网关。
2. **D1-b：`/ip dhcp-server network` 多条记录的择优规则**（是否最具体前缀胜、顺序是否参与）。
3. **D1-c：`/routing rule` 引到旁路由是否形成回环**（VM 不 SNAT 时）；`min-prefix=0` / `action=lookup` 的回落表现；被 FastTrack 命中的连接是否绕过 routing rule。
4. **D1-d：哪些菜单的 API `listen` 真生效**：`/ip dhcp-server lease`、`/ip arp`、`/ip firewall connection`、`/ip firewall address-list`、`/routing rule`、`/log`。
5. **D1-e：`/interface monitor-traffic` 在 API 下的参数**（`once=` / `interval=` / `filters=` 名字），以及 `!re` 是否持续推送。
6. **D1-f：`mangle action=route` 的配套参数名**（官方 action 表未列）。
7. **D1-g：lease `host-name` 等"print 有列但属性表无"字段能否经 `.proplist` 取得。**
8. **D1-h：`api` 服务在出厂默认配置里是否启用**（决定说明书是 2 步还是 3 步）。
9. **D1-i：REST 与 API 对 `export` 的可用性矛盾**（Configuration Management 说"CLI only"，REST 页给了 `/rest/export` 示例）。
10. **v6 差异基线**：`/ip dhcp-server network` 字段名（`dns` vs `dns-server`）、`/ip route rule` vs `/routing rule`（后者官方已明示）、`/routing table` 是否必需（v7 明示必需）。v6 侧一手来源本次不可达 → 全部标未核实。

---

## 6. C 段：手动 vs 自动 分表（交付核心）

图例：**M** = 人在 Winbox 手动；**A** = ZBox 通过 API(8728) 自动下发；**A?** = 可自动但依赖 §5 实测结论；**H** = 禁止自动（人工专属）。

| 事项 | M/A | API 通道与确切命令 | 手动的话点哪几步 | 做错的后果 | 出处 |
|---|---|---|---|---|---|
| 开启 api 服务 | **M**（一次性，ZBox 无法自举） | 若已开：无需；否则 Winbox 开完 ZBox 可校验 `/ip service print where !dynamic` | IP → Services → 勾 `api`(8728) → Apply | api 未开则 ZBox 全程连不上；开 `telnet`/`ftp` 顺手把攻击面也开大 | API 页"must be enabled"；Services 页属性表+示例 |
| api 来源白名单 | **M**（第 2 步；之后可 A） | `/ip service set api address=<网段/前缀>` | Services 窗选中 api → Address 填 `192.168.1.0/24` | 填漏 ZBox 被拒；填 `0.0.0.0/0` = 明文 API 全网可达 | Services 页（官方原句 + `/ip/service/set api address=10.5.101.0/24,2001:db8:fade::/64`） |
| ZBox 专用账号与组 | **M**（首次，因 `policy` 位才允许建用户；ZBox 自己有 write 后也**不该**再改用户面） | `/user group add name=zbox-api policy=api,read,write,…` + `/user add name=zbox group=zbox-api address=…` | WebFig/Winbox → Tools → Users → 建组(勾 api/read/write，其余不勾) → 建用户 | 直接给 `full` 组 = 面板被攻破即整机接管；给 `read` 组又不含 `write` → 下发全 `!trap category=1` | User 页（policy 全集 + `write` 原文警告 + `address=`） |
| 把旁路由 IP 排除出 DHCP 池 | **A** | `/ip pool set <name> ranges=<分段逗号列表>` | — | 旁路由地址被租走 → 随机失联 | IP Pools 页（ranges 语法与"排除网关/服务器地址"示例） |
| LAN 默认档：网关=旁路由 | **A?**（依赖 D1） | `/ip dhcp-server network set <id> gateway=<VM> dns-server=<VM> dns-none=no` | — | **全网断**（旁路由宕机时无回落）；只清 `dns-server=` 不加 `dns-none=yes` → ROS 继续下发自己的 DNS | DHCP 页（network 属性与默认 DNS 传递原文） |
| 按设备切档（DHCP 路） | **A?**（D1-a） | `/ip dhcp-server option add code=3 name=… value='x.x.x.x'` → `/ip dhcp-server lease set <id> dhcp-option=…` → `/ip dhcp-server lease make-static <id>` → `send-reconfigure <id>` | — | 选项不生效=档位假切换；lease 未静态化 → 设备换 IP 后规则错配 | DHCP 页（precedence 原文、`send-reconfigure`/`make-static` 为菜单专用命令、option value 数据类型表） |
| 按设备切档（服务端秒级路） | **A?**（D1-c） | `/routing table add name=via-vm fib` → `/ip route add dst-address=0.0.0.0/0 gateway=<VM> routing-table=via-vm` → `/routing rule add src-address=<IP>/32 action=lookup table=via-vm` | — | 用 `lookup-only-in-table` = 旁路由挂 → 该设备彻底不可达（官方明说该写法会"整个 lookup 失败"）；不建表直接引用 = `!trap` | Policy Routing 页、`/routing rule` 页、v6→v7 页（**必须先在 /routing table 建表**、fib、`gateway=x@main`） |
| 设备分组（档=列表） | **A** | `/ip dhcp-server lease set <id> address-list=zbox-<档>` | — | 命名不带命名空间 → 与用户既有列表撞名，规则互相污染 | DHCP 页 lease `address-list=`（*"added if the lease is bound"*） |
| 53 端口全局劫持（不依赖客户端） | **A（但默认不推荐）** | `/ip firewall nat add chain=dstnat protocol=udp dst-port=53 action=dst-nat to-addresses=<VM> to-ports=53` + **必须**配 `/ip firewall nat add chain=srcnat action=masquerade src-address=<LAN网段> dst-address=<VM> out-interface=<LAN> protocol=udp` | — | 不配 hairpin → 客户端丢弃应答（官方原话解释了这个失败模式）；配了 → **oxidns 看不到设备源 IP，按设备 DNS 策略失效** | NAT 页（dst-nat/redirect/to-ports 适用集合、Hairpin NAT 全节） |
| 关掉 ROS 自己的 53 对外应答 | **A** | `/ip dns set allow-remote-requests=no` | — | 留着 `yes` 时 oxidns 与 ROS 双解析器并存，排错时"这条记录谁答的"说不清 | DNS 页 |
| 动态结果写回主路由（DNS→策略） | **交给 oxidns，不由 ZBox 直写** | oxidns `ros_address_list`：`/ip/firewall/address-list/add =list=… =address=… =timeout=… =comment=oxi…`；TLS 场景走 api-ssl 8729 | — | 不带 `timeout` → 官方：条目**永久写盘**（污染 flash）；带 timeout → **重启即失**，UI 必须说明 | Address-lists 页（timeout 持久性原文）、oxidns 源码/文档 |
| 非对称路由不被 invalid 规则杀掉 | **A**（自动配套生成） | `/ip firewall raw add chain=prerouting src-address=<设备>/32 action=notrack`（读回：`/ip firewall filter print` 找 `connection-state=invalid action=drop`；`/ip settings` 校验 `rp-filter=no`） | — | 官方两页建议互相矛盾（一边"strongly suggest drop invalid"，一边"asymmetric 路由不能用这套"）→ 不处理就是**间歇性断流**，最难排 | Connection tracking 页、Common Firewall 页、Filter 页（notrack）、IP Settings 页（rp-filter 默认 no） |
| 切档与 FastTrack 冲突防护 | **A**（读+告警，不改默认） | 读 `/ip firewall filter print where action=fasttrack-connection`；提示"被 fasttrack 命中的流量上 mangle/queue 无效" | — | 用户报"切了没效果"，其实是 fasttrack；**不要为了让 mangle 生效去关 route-cache**（会把整机转发拖进慢路径） | Connection tracking 页（旁路清单+生效条件）、Mangle 页（FastTrack 只认 main） |
| 设备上下线事件推送 | **A** | `/system script add name=zbox-lease policy=… source="/tool fetch url=\"http://<zbox>/ros/dhcp-event\" http-method=post http-data=…"` + `/ip dhcp-server set <name> lease-script=zbox-lease` | — | 脚本自带 `policy=` 会决定以谁的身份执行（官方演示了 `use-script-permissions` 报 `not enough permissions (9)`）→ 写错=脚本永不执行或提权 | Scripting 页（lease-script 示例与变量表、script permissions 全节） |
| 观测面板数据 | **A**（轮询为主，流式待 D1-d） | `/ip dhcp-server lease/print =.proplist=address,mac-address,status`；`/ip arp/print .proplist=…`；`/ip firewall connection/print .proplist=…`；`/interface/print =stats` | — | **不带 `.proplist`** = 官方点名的性能惩罚；连接表大轮询 = 主路由 CPU 抖动 | API 页（print 差异与 .proplist 原文） |
| RouterOS 版本升级 | **H** | 禁止：`/system package update install` 会**自动重启**（官方原文） | 人先看 changelog，再决定 | 升级链选错（stable/testing）或包缺失 → 设备起不来 | Upgrading 页 |
| RouterBOOT 固件升级 | **H** | 禁止：`/system routerboard upgrade`（官方示例带 `[y/n]` 交互确认，非交互参数未核实） | System → RouterBOARD → Upgrade → 重启 | 变砖级 | Upgrading 页、RouterBOARD 页 |
| 重启 / 恢复出厂 | **H** | 禁止：`/system reboot`、`/system reset-configuration` | 人自己在 Winbox 点 | 全网瞬断 + 配置清空 | Upgrading 页（`/system reboot` 交互示例） |
| 无线/射频参数 | **H**（只读展示） | 只 `print`，不 `set` | 人自己在 Wireless 页 | 覆盖退化/AP 失联，且与编排目标无关 | Mangle/User 页（wireless scan 属 test 工具族）；ZBox 范围决策 |
| 写大文件到设备（>60KB） | **H**（改走上传+自动导入） | 不用 `/file set contents=`；改为 FTP/SFTP 上传 `*.auto.rsc`，读 `*.auto.log` 校验；注意只有 `flash/` 目录内容重启后保留 | Winbox → Files 拖入 | 官方只保证 60KB 读写；大文件"分块写"无文档 → 静默截断 | Files 页（60KB、`read chunk-size`、flash 目录持久性）、Configuration Management 页（`*.auto.rsc`） |
| 配置备份（export） | **A?/H**（官方两页矛盾） | 不依赖 `/export`；ZBox 用"期望态快照 + 逐条 diff"做回滚 | 人需要整包备份时在 Winbox 里 export | 设计成依赖 export 可能在纯 API 路线上直接失败 | Configuration Management 页 vs REST 页 |

### 真人操作说明书（目标 ≤3 步；开机第一次进 Winbox 只做这些，其余 ZBox 全包）

> 前提：设备已能上网、LAN 侧已跑默认配置（Quick Set 出来的那种）。旁路由 VM 已在同一 LAN，已知其 IP（下称 `<VM>`），ZBox 面板所在机器 IP（下称 `<PANEL>`）。

**第 1 步｜打开 API 并锁来源**（Winbox → `IP` → `Services`）
- 列表里找到 `api`（端口 8728）。若该行有红叉/被标 `X - DISABLED`，点掉叉启用它。
- 双击 `api` → `Address` 框填入允许来源（**空格或逗号分隔前缀，官方写法是逗号**）：`<PANEL>/32,<VM>/32`。
- 顺手把不需要的 `telnet`、`ftp`、`www` 保持关闭（**别为了省事开 www**：REST 在 v7.9+ 才可用，且官方明确警告明文凭证可被窃听）。
- 点 `Apply`。

**第 2 步｜给 ZBox 建一个专用账号**（Winbox → `Tools` → `Users` → 先 `Group` 页签，再 `Users` 页签）
- `Add Group`：名字 `zbox-api`；Policy 只勾 **`api`、`read`、`write`**（其余一律不勾，特别是 `policy`/`reboot`/`sniff`/`ftp`/`local`/`telnet`/`ssh`/`web`/`winbox`；如果 ZBox 要下发"事件推送脚本"再加 `test`）。
- `Add User`：名字 `zbox`，Group 选 `zbox-api`，设密码，**`Allowed Address` 填 `<PANEL>/32,<VM>/32`**（用户级第二道来源锁，官方属性 `address`）。
- 这一步只能人做：建用户需要 `policy` 权限位，而"最小权限账号"本身不该有它 —— **不要**把 admin 的账号密码交给面板。

**第 3 步｜在 ZBox 里填一次连接信息，之后全交出去**
- ZBox 填：主路由 IP、`8728`、`zbox` 账号、旁路由 IP、LAN 网段。ZBox 在此后自动负责：
  DHCP 池排除 `<VM>`、`/ip dhcp-server network` 的默认档与切档选项、`option`/`option-set`/静态租约与 `address-list` 分组、`/routing table`+`/ip route routing-table=`+`/routing rule`（用 `action=lookup` 保留回落）、raw `notrack` 配套、`/ip dns allow-remote-requests`、`/ip service set api address=` 的后续维护、oxidns 的 `ros_address_list` 列表名与 TTL 策略、观测面板轮询（带 `.proplist`）与 `/tool fetch` 事件脚本。
- **ZBox 永不自动做的事**（面板里以"请手动"提示）：RouterOS 版本升级、RouterBOOT 固件升级、重启/恢复出厂、无线射频参数、>60KB 的文件写入、用户与权限变更。

**如果只有 2 步的空闲**：把第 1、2 步合成一次 Winbox 会话即可（都在 `IP/Tools` 两个菜单里）；第 3 步是面板操作，不算设备侧手动。

---

## 7. 附录：来源清单（全部为 MikroTik 官方文档站 ROS 空间；抓取 2026-10-09，本地物证在 `E:\ZBox\.tmp\ros\`）

| # | 页面 | URL | 页面最后修订 | 物证 |
|---|---|---|---|---|
| S1 | API | `https://help.mikrotik.com/docs/spaces/ROS/pages/47579160/API` | 2025-02-27 | `p_api.txt` |
| S2 | REST API | `…/ROS/pages/47579162/REST+API` | 2025-08-12 | `p_rest.txt` |
| S3 | Python3 Example（API 子页） | `…/ROS/pages/47579209/Python3+Example` | 2026-02-10 | `p_py3.txt` |
| S4 | Services（`/ip service`） | `…/ROS/pages/103841820/Services` | 2026-03-30 | `p_services.txt` |
| S5 | User（`/user`、`/user group`） | `…/ROS/pages/8978504/User` | 2026-04-15 | `p_user.txt` |
| S6 | DHCP（client/server/lease/network/option/option sets/matcher/alert） | `…/ROS/pages/24805500/DHCP` | 2026-03-19 | `p_dhcp.txt` |
| S7 | DNS（`/ip dns`、static、DoH、cache） | `…/ROS/pages/37748767/DNS` | 2026-03-23 | `p_dns.txt` |
| S8 | IP Settings（`rp-filter`/`route-cache`/`arp-timeout`/`max-neighbor-entries`） | `…/ROS/pages/103841817/IP+Settings` | 2026-03-20 | `p_ipsettings.txt` |
| S9 | IP Pools | `…/ROS/pages/129531938/IP+Pools` | 2026-01-26 | `p_pools.txt` |
| S10 | NAT（dst-nat/masquerade/Hairpin/actions） | `…/ROS/pages/3211299/NAT` | 2025-10-15 | `p_nat.txt` |
| S11 | Mangle（actions 表、`mark-routing` 与 FastTrack 警告） | `…/ROS/pages/48660587/Mangle` | 2025-10-09 | `p_mangle.txt` |
| S12 | Filter（RAW `notrack`） | `…/ROS/pages/48660574/Filter` | 2026-03-24 | `p_filter.txt` |
| S13 | Common Firewall Matchers and Actions（`dst-address-type`、invalid 建议） | `…/ROS/pages/250708064/Common+Firewall+Matchers+and+Actions` | 2026-03-10 | `p_commonfw.txt` |
| S14 | Connection tracking（FastTrack 清单/条件、`/ip firewall connection`、timeout 属性） | `…/ROS/pages/130220087/Connection+tracking` | 2026-04-08 | `p_conntrack.txt` |
| S15 | Address-lists（`list/address/timeout/creation-time`、动态与持久性） | `…/ROS/pages/130220135/Address-lists` | 2026-04-01 | `p_addrlist.txt` |
| S16 | IP Routing（概念、`suppress-hw-offload`、ECMP） | `…/ROS/pages/328084/IP+Routing` | 2026-03-12 | `p_routing.txt` |
| S17 | Policy Routing（`/routing table`、fib、routing rule vs mangle） | `…/ROS/pages/59965508/Policy+Routing` | 2025-08-06 | `p_polrouting.txt` |
| S18 | `/routing/rule` 参考页（属性全表） | `…/ROS/pages/331612232/routing+rule` | 2026-03-12 | `p_rout.txt` |
| S19 | Moving from ROSv6 to v7 with examples（表/规则位置变更） | `…/ROS/pages/30474256/Moving+from+ROSv6+to+v7+with+examples` | 2026-02-19 | `p_ros6to7.txt` |
| S20 | Packet Flow in RouterOS（prerouting/input/forward 顺序、dstnat 对本地目的生效、bridge vlan-filtering） | `…/ROS/pages/328227/Packet+Flow+in+RouterOS` | 2025-10-31 | `p_pktflow.txt` |
| S21 | Interface stats and monitor-traffic | `…/ROS/pages/139526175/Interface+stats+and+monitor-traffic` | 2022-07-27 | `p_montraffic.txt` |
| S22 | ARP（`/ip arp`、status 值集、ARP 模式） | `…/ROS/pages/100892687/ARP` | 2025-10-10 | `p_arp.txt` |
| S23 | Neighbor discovery（`/ip neighbor` 只读、discovery-settings） | `…/ROS/pages/24805517/Neighbor+discovery` | 2026-04-02 | `p_neighbor.txt` |
| S24 | Netwatch（`/tool netwatch`，v7.4 起多探测类型） | `…/ROS/pages/8323208/Netwatch` | 2025-10-08 | `p_netwatch.txt` |
| S25 | Graphing（`/tool graphing`） | `…/ROS/pages/22773810/Graphing` | 2023-11-21 | `p_graphing.txt` |
| S26 | Traffic Flow（NetFlow v1/5/9/IPFIX、HW 卸载不可见） | `…/ROS/pages/21102653/Traffic+Flow` | 2024-04-17 | `p_flow.txt` |
| S27 | HotSpot（`/ip hotspot active` 只读） | `…/ROS/pages/56459266/HotSpot+-+Captive+portal` | 2026-04-13 | `p_hotspot.txt` |
| S28 | Scripting（`lease-script` 变量、`/system script` policy 与 `use-script-permissions`、`:execute` 64kB 限制） | `…/ROS/pages/47579229/Scripting` | 2026-04-28 | `p_scripting.txt` |
| S29 | Configuration Management（export "CLI only"、import、`*.auto.rsc`） | `…/ROS/pages/328155/Configuration+Management` | 2026-04-17 | `p_cfgmgmt.txt` |
| S30 | Files（60KB 上限、`get contents`、`read chunk-size`、flash 目录） | `…/ROS/pages/2555971/Files` | 2026-03-17 | `p_files.txt` |
| S31 | Upgrading and installation（`/system package update`、bootloader、reboot） | `…/ROS/pages/328142/Upgrading+and+installation` | 2026-03-27 | `p_upgrade.txt` |
| S32 | RouterBOARD（`upgrade-firmware`、`/system routerboard upgrade`、`auto-upgrade`） | `…/ROS/pages/40992878/RouterBOARD` | 2026-04-10 | `p_routerboard.txt` |
| S33 | Upgrading to v7（兼容性表、v6→v7 一次性转换） | `…/ROS/pages/115736772/Upgrading+to+v7` | 2026-02-25 | `p_upg7.txt` |
| S34 | Command Line Interface / Console | `…/ROS/pages/328134`, `…/328098`（`/8978498`） | 2025-08-22 | `p_console.txt` |
| S35 | General Properties（菜单通用属性/命令） | `…/ROS/pages/16351619/General+Properties` | 2026-04-13 | `p_genprops.txt` |
| S36 | Default configurations（出厂默认形态） | `…/ROS/pages/167706788/Default+configurations` | 2025-10-01 | `p_defaultconf.txt` |
| S37 | 二级来源（非 MikroTik，只作实现参照） | oxidns 本地副本 `E:\ZBox\.tmp\oxrepo\oxidns-main`：`src/plugin/executor/{ros_address_list,ros_route,routeros}/`、`docs/docs/plugin-reference/executor/integrations.mdx` | GPL-3.0-or-later（源码头逐行标注） | 不复制代码，仅用于确认"第三方成熟实现选的是 8728/8729 API + address-list 落点" |

### 未核实清单（本报告拒绝给出确定答案的条目）

1. `api` 服务在出厂默认配置里**是否默认启用**（官方无一句"by default enabled"；Services 页真机示例中 api 行无 `X`，www-ssl 有 `X` —— 只是旁证）。
2. `/ip service` 启用写法的确切参数（`disabled=no` 由 `print` 的 `X - DISABLED` 标志推得；官方示例只给了 `address=`）。
3. RouterOS v6 侧一切字段名差异：`/ip dhcp-server network` 是 `dns=` 还是 `dns-server=`、v6 是否存在 `option sets` 菜单、v6 的 `rp-filter` 默认值。一手来源（frozen wiki）本次不可达，archive.org 网络不可达。
4. `/ip dhcp-server network` 多条记录之间的**择优规则**（最具体前缀 vs 顺序）。
5. 租约级 `dhcp-option` 覆盖 network 级 `gateway`/`dns-server` 的**实际行为**（官方只有一句 precedence 序列）。
6. `option sets` 菜单的确切路径写法（官方标题写 `Sub-menu: /ip dhcp-server option sets`，但全页没有给出 `option sets add` 形式的示例；`dhcp-option=` 的示例则有）。
7. 单个 DHCP 选项里**多个 DNS 的 value 写法**（官方给了 `'10.10.10.10'`→hex 与 `0x0a0a…` 原值、`$(VAR)` 变量，未演示逗号多值）。
8. `mangle action=route` 的配套参数名（官方 action 表只有说明句，无参数表）。
9. `/interface monitor-traffic` 在 API 下的 `once=`/`interval=`/`filters=` 参数名（REST 页只泛化说"用 monitor once 参数"，LTE/WiFi 示例里出现过 `once`）。
10. 哪些菜单的 API `listen` 真能流式（`/ip firewall connection`、`/ip arp`、`/log` 等）；`.dead=yes` 在各类表上的可靠性。
11. `export` 在纯 API 路线是否可用（S29 说 CLI-only，S2 给了 `/rest/export` 示例 —— 两页冲突）。
12. `/system routerboard upgrade`、`/system reboot` 在非交互（API）下的确认参数名。
13. `/ip firewall connection` 是否含每连接的字节计数字段（官方示例只印了 PR/SRC/DST/TCP-STATE/TIMEOUT 与 S/A 标志）。
14. `/queue simple` 的 `rx-byte/tx-byte` 等计数字段名（本次未在官方 Queues 页核对到，故不写入正文结论）。
15. `/system resource` 除 `uptime`/`cpu-load` 之外的属性名。
16. lease 的 `host-name`、`comment` 等字段能否用 `.proplist` 取回。
17. RouterOS 是否提供任何"DoT（DNS over TLS）上游"配置：DNS 页只有 DoH 与 `servers=`，**未见 DoT 属性**（"不存在"这一条按"文档未出现"陈述，未做进一步否证）。
18. NETCONF/YANG：全站全文检索 0 命中（2026-10-09 实测）；按"官方无此能力"处理，但这是**检索否证**，非厂商声明。
