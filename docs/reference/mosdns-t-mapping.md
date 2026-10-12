# MosDNS-T → ZBox 映射报告

对象：`reference/mosdns-t/`（fork 自 `yyysuo/mosdns`，**GPL-3.0**）
用途：给 Task #6（Vue 3 面板）、#7（监控与连接面板）和尚未开工的 `api/` 提供**输入清单与取舍依据**，
并回填 `docs/ARCHITECTURE.md` §7。

## 0. 边界与核验方式（先读这段）

**法律边界**：本次是只读勘察。全文只描述"有什么、在哪、什么语义"，不复制其代码、YAML、文案、CSS 或数据结构。
ZBox 侧任何实现都必须是 clean-room 重写；本文里出现的引用形如 `文件:行`，是**定位用**，不是可抄片段。

**核验方式**（决定可信度）：

- 本报告的行号来自 4 个只读勘察子代理 + 我自己对关键条目的逐条复核。我**亲自复核过**的条目：
  `coremain/api_special_groups.go:20-24,236,311,482,539,630,635-640`、仓库内**没有任何配置 YAML**
  （`find` 除 `.github/` 外零命中）、`coremain/` 全局**无鉴权**（只有 GitHub 出向 Bearer）、
  `config_manager.go` 的 `dir` 只查非空、`webui-log` 的依赖表/6 个主 Tab/构建落点/dev 代理/
  `limit=10000`/日志 `items_per_page: 50`/`rank?limit=20`。
  其余（尤其 `plugin/`、`pkg/` 的位命名空间细节、前端交互行号）属**代理勘察结论**，可信但未被二次核对。
- **他们的 `docs/` 不能当事实**：`docs/api_reference.md` 与代码有一批实质冲突（见 §7）。
  引用其文档必须以代码为准。
- **分流顺序表里的 `fast_mark` 数字是"别人线上环境的记录"，不是代码常量**。
  来源是 `docs/mosdns-分流顺序表-10.0.0.3.md:24-41`，它描述的配置包**不在这个仓库里**（见 §1.2）。
  ZBox 不要把这些数字当"上游定义"引用。

---

## 1. 三条结构性不对称 —— 为什么这不是一一对应

### 1.1 他们只动 DNS 应答，我们动整条链路

MosDNS-T 是**纯 DNS 分流器**：它的输出是"这个域名该由哪个上游解析、返回什么 IP/fakeip"，
它**不代理任何流量**，并且这个 fork **主动删掉了上游的 nft / eBPF 方向**
（`reference/mosdns-t/AGENTS.md` 与 `docs/fork_diff_summary_zh.md:97-107`）。
ZBox 的形态是 sing-box 旁路由：DNS 分流只是 `dns.rules`，还有 `route.rules` + TPROXY/REDIRECT 入站 +
nft 劫持层 + fwmark 策略路由（`docs/ARCHITECTURE.md` §4、`src/gen/singbox/route.rs:76`、`src/gen/nft.rs`）。

后果：他们"命中即定"的**终点**是"选一个上游"，我们的终点是"选一个出站"。
他们的 `sequence_ipv4` 那一层在我们这里根本不存在 —— sing-box 的一条规则同时决定解析和路由。

### 1.2 分流真相源不在仓库里

仓库里**没有任何 `sub_config/*.yaml` 或 sequence 模板**（实测：除 `.github/` 外零 YAML 命中）。
`sequence_ipv4 / sequence_ipv6 / sequence_other / rule_set.yaml / process_v4.yaml` 只作为**名字**出现在
文档和代码字符串里；真正的规则顺序、`fast_mark` 编号、每个列表用 `domain_set` 还是 `domain_set_light`，
全在**外部配置包**里（`docs/ai/config-notes.md:31-42`：另一个仓库 `jasonxtt/file` 下的 `config_up.zip`），
部署后落在 `/cus/mosdns`。

这个 fork 自己生成**只有**一份：`sub_config/special_groups.yaml`
（`coremain/api_special_groups.go:22`，渲染器 `:626` 起，第 630 行写着一行"自动生成，不要手动编辑"的标记注释）。

对 ZBox 的直接含义：**我们不存在这个坑**。ZBox 的整套真相源是 `src/model/config.rs::DesiredState`（:42）
→ `src/gen/singbox/`，顺序硬编码在 `gen/singbox/route.rs` 的阶梯里并有单测钉住（`:316-558`）。
他们的架构是"二进制 + 外部可替换配置包"，我们是"模型 + 生成器 + 不可变快照"。
这是**两种世界观**，映射时不要把他们的"配置包热替换"当成要补齐的功能。

### 1.3 分流引擎的语义根本不同（本报告最重要的一条）

- 他们：一条域名可以同时命中多条名单，`domain_mapper` 把所有命中**OR 合并**成一个结果
  （标签拼接 + `fastMark` 位图并集，`plugin/data_provider/domain_mapper/domain_mapper.go:160-219,456-508,610-623`），
  **命中阶段不做决定**；决定由后面的 `sequence_*` 按 `fast_mark` 分支做。
  所以他们的日志里"命中标签"天然是多个，谁生效要靠**顺序 + 事后推导**。
- sing-box 1.14.2：`route` 与 `dns.rules` 的 `action:"route"` 都是**终止语义**，第一条命中即出结果、后面不再看
  （已由实测钉死：`docs/reference/singbox-1.14.2-schema.md:526`；ZBox 侧的单测
  `src/gen/singbox/dns.rs:302` 就是钉"fakeip 那条必须在末尾"）。

四个必须落到产品上的后果：

1. **"多命中再由后置决定"在 ZBox 里无法表达，也不该表达。** 优先级只能靠**生成顺序**实现，
   于是"面板上拖一下顺序"= 改运行时行为（他们也有同一条：`docs/ai/config-notes.md:56` "Rule order is behavior"）。
   ZBox 现在把**层级**顺序写死在 `route.rs`，好处是地理库与拦截层不会被用户改乱；schema v2 之后
   **用户名单那一级内部就是裸列表**，拖一下即改运行时行为——这正是选项 C 换来的能力，
   代价转移到了面板上：必须显示"命中即定、第一个赢"，并且要能预览改完后的结论。
   仍然**没有**做的是"把用户名单插到阶梯的哪一级"——那需要给阶梯加命名层级，一次误拖就等于断网。
2. **面板不能显示"这个域名命中了 3 条名单，最终走了 X"** —— 我们没有那个信息，内核只告诉我们赢的那条。
   能显示的是"生效结论 + 它属于哪一级"。这直接决定了 #7 的查询日志页形状（见 §6）。
   选项 C 之后这条更吃紧：用户名单成了一行一条的可排序表，面板很容易顺手做一个"这个域名被第 3 行
   拦住"的解释，而那正是我们给不出的信息（被盖住的行**不产生任何命中记录**，只有医生能算出来）。
3. **他们的"冲突谁赢"表（`docs/mosdns-分流顺序表-10.0.0.3.md:77-89`）是产品语义，可以直接吸收为 ZBox 的验收单测。**
   例：灰名单压白名单、`cn@!cn` 压 `!cn@cn`、专属组压一切普通名单。
   ZBox 这边最初只有两条断言（"设备级早于地理分流"`route.rs:411`、"白名单晚于拦截"），
   把他们的表逐条翻成 ZBox 的 `route.rs` 顺序断言，是 #6/#7 之前**性价比最高的一小步**。
   ~~（未做）~~ **已做，见 §9 第 4 条与 §3 末**：#17 落了护栏组，顺带挖出 #18 并按"拦截优先"定案；
   schema v2 之后护栏扩到七条（`route.rs:528-749`，新增"名单内行序决定谁赢"、
   "用户名单压过地理默认"、"广告库仍压过名单"、"同一份名单在两层展开顺序一致"），
   断言的仍然是**索引**而不是"这条规则存在"——因为终止语义下只有先后是可验的。
4. 他们的"记忆"类规则（记忆代理/记忆直连/记忆无 v4/v6）之所以要排在普通名单**之前**，
   正是因为在他们的引擎里它会被别的标签淹没；在 sing-box 里只要把它排在前面就自然赢 ——
   也就是说**他们的补丁机制在我们的引擎里是免费的**。这条是"要不要未来做记忆"的判断依据（§5.6）。

---

## 2. 分层映射总表

| MosDNS-T 的东西 | 它的真相源 | ZBox 对应物 | 结论 |
|---|---|---|---|
| `special_groups`（专属分流组）| `webinfo/special_upstream_groups.json` + 生成的 `sub_config/special_groups.yaml` | `model::config::Group`（`src/model/config.rs:365`）+ `Policy.*_domains` + `Device` | **改造吸收**（§5.2） |
| 上游（upstream）管理 + 热重载 | `webinfo/upstream_overrides.json`（覆盖式，不覆盖原 YAML）| `Dns.domestic/overseas[]`（`config.rs:229`）→ `gen/singbox/dns.rs` 的 typed server | **只吸收"分级回执"**；热重载语义不可迁移（§5.4） |
| 在线规则源（sd_set / adguard / *_light）| 插件目录 `config.json` + `.srs` 落地 | `RuleSet::Remote{tag,url,initial_path,interval}`（`config.rs:375`）+ `svc::ruleset::Sync` + `rule-set/manifest.json` | **已实现**，比他们多了"两级验收 + 空源守卫"（`ARCHITECTURE.md` §5） |
| 本地/手工名单 | 外部 YAML 指向的 `rule/*.txt` | `Policy.domain_rules[]`（`config.rs`，每行 `{pattern, action}`）+ `DomainPattern` 前缀语法（`:268-298`）| **已实现**，语法与他们的 `full:/domain:/keyword:/regexp:` 同族（§5.8）；v1 的三档扁平名单已在 schema v2 合成一条有序表（§3 补） |
| 分流顺序 | **外部配置包**（不在仓库）| `gen/singbox/route.rs:87-216` 硬编码阶梯 | **语义吸收**（§3） |
| 查询/审计日志 + 聚合排名 | 内存环形缓冲 + `audit_settings.json` | **无**（#7 的待建项，数据源见 §6） | **缺口** |
| 诊断抓取（capture）| 1–600s 进程日志捕获，`/api/v1/capture/*` | `Layout::worker_stderr_path()`（`src/gen/init.rs:90`）+ `svc/observe.rs` 读数 + `doctor` | **部分**，缺"按时间窗抓一段内核日志"这条 API |
| 外观/背景持久化 + 历史 | 3 个 `appearance_*.json` + 上传的 20MB 图 | 无 | **不做**（§5.9） |
| 域名生成（记忆）开关 | `webinfo/domain_generation_settings.json` + `domain_output` 插件 | 无 | **不做，留接口**（§5.6） |
| 配置包在线更新（schema 门禁 + 白名单 + 回滚）| `requiredConfigSchema`/`requiredConfigPackageID` | `apply::store` 快照 + `validate::gate` + `rollback` + `state.rs` 的 `schema` 门与迁移 | **已补齐**：状态侧的版本门（现 v2）与 v1→v2 迁移已落地，等价于他们的 `requiredConfigSchema`；仍缺 §5.5 那两条（`in_progress` 事务态、备份保留策略）|
| 自升级二进制 | `.mosdns-update-state.json` + `update_guard` 事务态 | `svc::provision` 落 sing-box 二进制（§6 第 1 步） | **改造**：ZBox 只钉版本、不自动升级（§5.10） |
| `nft` / `eBPF` 劫持 | 上游有，这个 fork **删了** | `gen/nft.rs` + `gen/netstate.rs`（已 netns 实测） | **不可参考**，方向相反 |
| AliAPI（阿里云 DNS 自建上游）| `plugin/executable/aliapi/` | 无 sing-box 等价 | **弃** |
| `fast_mark` / `ctx_mark` / switch 位命名空间 | `pkg/query_context/context.go:289-311,79-81` | sing-box 无查询上下文位；只有 fwmark（`route.rs:19-21`） | **弃**（但见 §5.7 的那条教训） |

---

## 3. 分流顺序逐条映射

左：他们 16 级（`docs/mosdns-分流顺序表-10.0.0.3.md:24-41`，数字来源见 §0 警示）。
右：ZBox 12 级阶梯（`docs/ARCHITECTURE.md` §4 = `src/gen/singbox/route.rs` 实际顺序）。

| 他们 | ZBox 落点 | 状态 |
|---|---|---|
| 1 重定向（rewrite，最早且早停）| `dns.rules` 的 `predefined` / `respond` 动作（字段存在：`schema.md:102-103`）| **可做，未做，未实测** —— 用途是"把某域名固定解析成本机 NAS/内网 IP"，这是旁路由上很实在的一条 |
| 2 黑名单（拦截）| 用户名单里 `action:"block"` 的行 → `route` 层 `action:"reject"`（`route.rs:154-169`）+ `dns` 层 reject（`dns.rs:98-102`）| 已实现。#18 的"拦截优先"结论在 v2 里变成两件事：广告库仍排在**整条名单**之前，名单内部的拦截/放行由**行序**决定，被放行的行盖住拦截行时 doctor 报 `block_never_fires_behind_a_pass` |
| 3 广告屏蔽 | `RULESET_ADS`（`route.rs:142-147`、`dns.rs:79-88`）| 已实现，且明确排在用户名单之前 |
| 4 DDNS 域名（强制国内解析后结束）| 名单里 `action:"direct"` 的行（放在需要它赢过 geo 的位置）| **弱等价**：我们有"直连"这个处置，但没有"DDNS 是独立一类"的身份。v2 之后直连行可以排到 geosite-cn 之前，这一类需求不再受档序限制 |
| 5 专属分流组（slot 50-59，早停）| 无。最接近的是 `Device.mode` 的按设备走组（`route.rs:100-120`）| **缺口**，见 §5.2 |
| 6 `!CN fakeip filter`（强制境外**真 IP**，早停）| 语义 = "这个域名不要 fakeip"。ZBox 现在是"fakeip 那条 `query_type` 规则放末尾"（`dns.rs:137-143`），所以前面的直连规则天然拿到真 IP；但**"要境外、但不要 fakeip"没有表达**（v2 的 proxy 行仍指向 fakeip + `query_type` 守卫，`dns.rs:110-116`）| **缺口**，且是 #7 排障时一定会撞到的那条 |
| 7 记忆代理 / 8 记忆直连 | 无（机制本身不做，§5.6）| 引擎差异使然：排在前面即可压过普通名单 |
| 9 灰名单（代理候选）/ 10 `cn@!cn` / 11 白名单 / 12 `!cn@cn` / 13 `CN fakeip filter` / 14 `cn` / 15 `!cn` | 一条有序名单 → geosite/geoip-cn → bittorrent → non-cn → `final`（`route.rs:149-208`）| **候选/决定分离**这一整层不迁移（§1.3）。`cn@!cn`/`!cn@cn` 这种"订阅补充"式反向覆盖，在 v2 里就是"把这一行挪到那一行前面"，不再需要跨档比较 |
| 16 列表外域名（看上游返回 IP 再判）| `ip_is_private` + geoip-cn + `final`；境外域名靠 fakeip 段短路（`route.rs:192-208`）| 已实现，且 fakeip 模式下不需要"看返回 IP" |
| 非 A/AAAA 走更短的顺序 | `dns.rules` 只有 `query_type` 规则会区分类型（`dns.rs:110-116,137-143`）| 天然不同：我们的非 A/AAAA 直接落 `dns.final` |

**注意 `!CN fakeip filter` 那行的缺口的严重性**：他们把"要代理但不要假地址"当成**最高优先级的早停项**
（顺序表 `:31,84`，且审计推导第一个就看它 `coremain/audit.go:620-624`）。原因是有些客户端会缓存 fakeip、
或 fakeip 段被别的中间设备当私有段处理，真 IP 才出得来。ZBox 一旦默认开 fakeip，这一类域名就只能靠
"塞进直连名单"来绕，而直连名单的语义是"直连"，不是"代理但给真 IP"。**建议 #6 期间把它做成显式开关**：
`Policy` 里加一组"强制真实 IP 但仍走代理"的域名，生成时排在 fakeip 规则**之前**、动作是 `route` 到境外上游。
这条要在 guest 上真验一次（fakeip 关闭路径已在 schema §11.7 验过，混合模式没验过）。

**§3 补（#17 落地时发现 → #18 定案 A → 选项 C 已于 2026-10-05 落地）**：把上表逐条变成断言时，撞出一条我们自己的
层间矛盾——同一个域名同时出现在 `direct_domains` 和 `blocked_domains` 时，**两层给的结论相反**：

| 层 | 当时的顺序 | 当时结果 | 定案后 |
|---|---|---|---|
| `route`（`route.rs:87-216`）| 直连名单（旧 `:140`）< 黑名单（旧 `:169-177`）| 判**直连** | 拦截 `route.rs:143-151` → 直连 `:153-159`，判**拦截** |
| `dns.rules`（`dns.rs:19-116`）| 黑名单 reject < 直连名单 route | 判**拦截** | 不变（`dns.rs:64-72` reject → `:84-92` direct），两层同向 |

（这张表里 A 定案后的行号是**当时**的位置，选项 C 之后阶梯整体右移：广告 reject 现在在
`route.rs:142-147`、名单展开在 `:154-169`，DNS 层对应 `dns.rs:79-88` 与 `:98-119`。）

`validate()` 只查每条名单自身的形状（`config.rs:589-595`，仅 `is_empty_name()`），**不查重叠**，
所以这个状态是用户可以合法按出来的。真实后果不是理论问题：DNS 侧回绝并不挡住路由侧直连——
客户端有 DNS 缓存、走 IPv6 邻居、或命中静态记录时，那条"应该被拦掉"的域名会正常连出去。
他们的顺序表里黑名单是第 2 级、直连类（DDNS）在第 4 级（本表第 2/4 行），**方向是拦截优先**；
是否早停没在他们那边实测确认，但对我们的引擎而言早不停不重要——顺序本身就是唯一的优先级。

当时摆的三个选项（留下记录，免得下一轮重新推一遍）：
- **A（先采纳，2026-10-05）**：路由层把 `blocked_domains` / `RULESET_ADS` 提到 `direct_domains` 之前，
  两层统一为"拦截优先"。之所以是必做：路由层放行时"拦截"是个会漏的承诺。
  代价——重叠里那条直连静默失效——由 doctor 补两条 Warn 点名（见下）。
- **B（未采纳，作为补充守卫也不够）**：`validate()` 禁止同一目标出现在两档。只能查"原文相同"那一类，
  跨形式（`+.a.test` vs `a.test`）查不动，"手写名单 vs ads 规则集"更查不动（`.srs` 不透明）。
  而且定案 A 之后重叠不再是两层打架，只是"某条永不生效"——那是 Warn 的量级，不是 Fatal 的量级。
- **C（已落地，2026-10-05，A 之后紧接着做）**：三档名单合成一条 `Vec<{pattern, action}>`
  （`Policy.domain_rules`，`DomainAction::{Direct, Proxy, Block}`），顺序即优先级，与引擎语义同构，
  互斥在模型层消灭而不是靠两层的默契。代价确实如预告：`DesiredState` 形状变更 + `schema` bump 到
  v2 + 迁移（`state.rs:STATE_SCHEMA/LEGACY_SCHEMA/migrate`）。

**A 落地之后 C 改了哪些结论**（面板与医生都要按这个口径写）：
- 三档之间的"看不见的优先级"没了。医生原先点名重叠的两条 Warn 因此改名成按处置分流：
  `block_never_fires_behind_a_pass`（该拦的排在某条放行之之后 = **漏承诺**）与
  `domain_rule_never_fires`（这一行永远命中不了 = 废条目），实现是 `doctor.rs` 里逐行与
  前面所有行比覆盖关系的那一段（原 `blocked_shadows` 泛化成 `covers`）。覆盖关系仍旧只用
  schema §4 末实测过的 `Suffix`/`Exact` 标签边界形状判断，`Keyword`/`Regex` 参与的
  **刻意不报**：判不动就宁可漏报。
- **唯一一处真实的语义变更**：名单里的"代理"行现在排在 geosite-cn / geoip-cn / bittorrent 之前。
  v1 是代理档在它们之后，于是用户点名要代理的国内域名会被地理默认静默吃掉。C 之后是
  "显式名单压过地理默认"。**这条已经在内核上验过**（`www.baidu.com` 同时在真 geosite-cn 里、
  又被名单点名代理 → DNS 评 fakeip 侧；`www.qq.com`/`test.cn` 对照 → 照旧走国内）：
  schema 文档 §11.9 第三节。其余关系（广告库在整条名单之前、私网与设备级在名单之前、
  拦截与直连互斥由行序决定、迁移档序 拦截→直连→代理）与 v1 结论逐条等价。
- DNS 层跟着同构：Block→reject、Direct→route 到国内主上游（没有国内上游时**不产出规则**，
  而不是硬造一个）、Proxy→route 到 `proxy_server`。fakeip 开启时 `proxy_server` 就是 fakeip 段，
  所以代理行必须带 `query_type:["A","AAAA"]` 守卫，否则 HTTPS/SVCB 这类不产地址的查询会被
  fakeip 规则抢走——这与 v1 的行为一致，是新写法里最容易踩空的一处。

顺带定下来的一条边界：**设备级 Direct（阶梯第 3 步）仍在拦截之前**，刻意不改。它的语义是"这台设备
不接管"，本来就不该由域名名单管辖；DNS 层也没有按设备分规则的能力，两层在这件事上不矛盾。

---

## 4. 状态落点映射：他们的多文件 JSON vs 我们的单模型

他们的持久化面（全部 OBSERVED，路径解析集中在 `coremain/state_files.go:14-171`，
`webinfo/` 是当前目录、`state/` 是遗留并在两边都存在时**可能反向覆盖** `webinfo/`，`:240-244`）：

| 文件（`webinfo/` 下）| 背后功能 | 写入者 |
|---|---|---|
| `special_upstream_groups.json` | 专属分流组 | `api_special_groups.go:21,342,417` |
| `upstream_overrides.json` | 上游覆盖（含明文 AK/SK）| `api_upstream.go:18-32` |
| `config_overrides.json` | socks5 / ECS / 应答替换 | `overrides.go:12` |
| `domain_generation_settings.json` | 记忆域名 5 个开关 | `domain_generation_settings.go:14,75-87` |
| `audit_settings.json` | 审计容量 | `audit.go:161` |
| `webui_port_settings.json` | 面板端口（端口可改，故 `api_reference.md` 写的 9099 已非事实）| `webui_port.go:17` |
| `appearance_{,text_,button_}settings.json` + `panel_background.bin` + `panel_background_history/` | 面板外观 | `api_appearance.go:23-27` |
| `config_update_state.json` | 配置包更新事务态（**硬编码走 webinfo，绕过迁移逻辑**，`:110`）| `config_update.go:29,109-111` |
| `sub_config/special_groups.yaml` | 上面第一行的**派生产物** | 生成器 `api_special_groups.go:616-630` |
| `.mosdns-update-state.json` | 二进制自升级态（**在二进制旁边，不在配置目录**）| `update_manager.go:46` |

**ZBox 侧的落点**（2026-10-05 补齐，见 §9 第 1 条；这一节留着是为了记下"为什么是这三件事"）：
`DesiredState` 的磁盘契约集中在 `src/model/state.rs`，规范落点是 `Layout::state_path()`
= `<root>/state.json`，`zboxd` 的位置参数退化成"显式覆盖"（彩排与复现要用），
**任何 API 都不接受调用方传的目录** —— 他们的 `config/export` 和 `update_from_url`
就是只查了非空（`config_manager.go:45-48,148`），等于任意目录读写；这条在 §8 的"绝不抄"里。

1. **规范落点**：如上。形状用 `#[serde(flatten)]` 平铺，所以"加了门之后写的文件"
   和"加门之前写的文件"是同一层结构，老文件只少一个键。
2. **schema 版本**：照他们 `requiredConfigSchema` 的**语义**（`config_update.go:37-40`：二进制-only
   发布不动这两个值；结构性变更才 bump，读旧状态时给出明确结论而不是静默误读）。
   ZBox 需要它的理由和他们一样：快照不可变但**用户状态会变形状**，而 `DesiredState` 加一个字段
   就靠 serde default 兜着——旧二进制读一份新状态会把认不出的字段按默认值补上再写回磁盘，
   那是**静默改写用户配置**，比读失败严重。所以两侧都拒：新于当前版本拒绝、
   老于当前版本拒绝并指名缺哪一步迁移。
3. **原子写**：他们有两个写入器，好的那个是"校验→写临时→chmod→fsync→rename"
   （`config_update.go:681-707`），差的那个是"固定名 `.tmp`/`.bak` + 不 fsync + 0644"
   （`file_save.go:27-48`，且 `upstream_overrides.json` 里装着明文 AK/SK 也照样 0644）。
   ZBox 的状态文件走唯一临时名（带 pid）+ fsync + rename，Unix 上**权限在 create 就给到 0600**
   ——状态里的订阅 URL 带个人令牌，chmod 之前那段窗口在共享主机上就是可读。
   快照目录那边**刻意没改**：`Store::write` 仍是直接 `fs::write`（`src/apply/store.rs:69-88`），
   因为快照是"写完才切 symlink"的不可变目录，不是用户状态。

---

## 5. 值得吸收的机制（逐条给"怎么做"）

### 5.1 只吸收语义，不吸收形状 —— 三条前置纪律

1. 状态与派生产物**分开**：他们的"生成，不要手改"标记（`api_special_groups.go:630`）是好东西，
   但同一份产物又被列进配置包可替换白名单（`config_update.go:446-449`），等于自相矛盾。
   ZBox 侧的纪律已经写在 `ARCHITECTURE.md` §3：`gen/` 只往快照里写，快照不可变。
2. **校验先于写**：他们每个写端点都先 re-unmarshal/查 slot/查重（`api_upstream.go:342-345`、
   `api_special_groups.go:422-453`、`webui_port.go:108-118`）。ZBox 对应的是 `DesiredState::validate`
   （`config.rs:462`），已经在做，并且做得更硬（tag 必须是安全文件名、`initial_path` 必须等于派生公式、
   urltest 不接受 `default` 而是直接报错，理由就写在 `:505-512`："面板上看着设了、实际没生效，比报错更难查"）。
3. **拒绝空值**：他们空文件会重写默认（`domain_generation_settings.go:191-197`）。
   ZBox 的对应物已有：空源守卫（`ARCHITECTURE.md` §5）与"解析出 0 条规则视为来源异常，拒绝覆盖旧产物"。

### 5.2 专属分流组：吸收"绑定四件套"的思想，丢掉 slot 与独占端口

他们的一个组 = 一个在线规则集 + 一个手工名单 + **一套独立上游** + **一套独立缓存** + 一条生成的序列
（生成内容见 `api_special_groups.go:631-686`；聚合器把两套名单以 `ctx_mark: slot` 注册进同一个
domain_mapper，`:689-714`；分发是"匹配 mark N → 进 `$sequence_special_N` → exit"，`:716-730`）。
槽位只有下界（`isValidSpecialSlot` 只判 `>= 50`，`:539`；`docs/api_reference.md:261` 写的"50-59"是**过期文档**，
自动分配是从 50 往上取最小未用，`:482`）。删除会连带清理上游覆盖、`.srs`、规则 txt、缓存 dump
（`:290-311,735-761`）。

**ZBox 的映射**（给 #6 的具体形态）：

- 值得吸收的核心是**"规则集 + 名单 → 特定出站"这条绑定**，以及**删除必须级联清理**——
  后者已按这个思路补齐（`plan_reclaim`：删掉的 `RuleSet` 声明会连带撤掉 `.srs` 与清单条目，
  判据只认清单，账本外的文件不碰；见 §9 第 2 条）。
- **不要抄槽位**。他们要用 50-59 是因为要占 `ctx_mark` 位命名空间；我们的身份是 tag 字符串，
  唯一性由 `validate` 保证（`config.rs:483-490`：组 tag 不得与节点重名）。
- **不要抄"每组一个监听端口"**（`listen_port`、且明确禁 53、`api_special_groups.go:542-552`）：
  那是 DNS 服务器形态的产物。我们的等价物是"局域网里指定设备走这个组"（`Device`）
  或"指定域名走这个组"。schema v2 已经把三档扁平名单变成了**带处置的有序名单**，走完了半步：
  剩下的是把 `DomainAction` 的 `proxy` 从"落到 `final` 那个组"变成"落到指定 tag"
  （新增一个携带目标的处置，迁移时它等价于现在的 proxy 行）。
- 缓存绑定不要抄：sing-box 的 DNS 缓存没有"每组一份"的字段（1.14.2 只有 `cache_capacity`，`dns.rs:130`）。

### 5.3 在线规则"保存即下载 + 前端轮询到计数出现"

他们的 UI 流程：保存 PUT → 后台 goroutine 下载 → 返回 202 → 前端**最多轮询 6 次 × 3s**，
直到 `rule_count`/`last_updated` 出现，并把"还在下载 / 已刷新 / 可能稍后才出现"分开说
（`webui-log/src/components/RulesManager.vue:604-677`）。

ZBox 已经有全部材料，只差这一步接线：`svc::ruleset::Sync` 会报 refreshed/kept/notes，
`rule-set/manifest.json` 记条数与时间。**面板上"保存新订阅规则"必须等于"顺带跑一次 rule-sets"**，
否则用户面对的是"保存成功、但这条规则今天不参与分流"（他们的 `docs/ai/project-context.md:75-77`
把这条列为设计目标，理由一样）。我们侧那条已知的可观测性缺陷也已修掉：**一条源硬失败时整份报告
被吞**（原来写作 `sync(...)?` 后才打印）现在由 `SyncFailure { report, source }` 承载，
CLI 先打印已落地的部分再把原因报出去（§9 第 3 条）——面板要显示的就是同一个 `report`，
不该再出现"点了更新，什么都没回显"。

### 5.4 分级回执（partial-error），和"reload = restart"的产品后果

他们上游保存的回执分三层："已保存，但热重载失败" / "已保存并重载，但刷缓存失败" /
"已保存，但依赖它的专属组刷新失败"（`api_upstream.go:528-567`）。这个**回执形状值得抄** ——
它承认"落盘成功"和"生效成功"是两件事，正是我们 `apply` 链的形状
（`store.write` → `validate` → `reload` → `probe`，`src/apply/engine.rs:160`、`svc::Actions`）。

但他们有 **1500ms 后偷偷重启进程**的三处（`api_special_groups.go:236,311`、
`special_upstream_bindings.go:668`，延迟常量 `:24`；重启实现是优雅关闭缓存后 `syscall.Exec`，
`api_system.go:147-209`）。**这条绝不抄**：ZBox 的重载**必然是整进程重启**（sing-box 不原地重载，
`svc/mod.rs:98-113` 已写明，systemd 单元刻意没有 `ExecReload`），而重启会掐断所有在用连接 ——
这正是 §5 第 0 步"指纹与 last-good 相同就一步都不做"存在的原因。
面板上的翻译：**任何会触发重载的操作，回执里必须明说"这会短暂断连"**，并且用 §5 的 diff 预览把它摆到确认气泡里，
而不是保存后 1.5 秒静默重启。

### 5.5 配置更新的事务性（他们做得好的那半）

`config_update.go` 那条链值得作为 ZBox `apply` 的对照检查表：

- 下载前定容量上限（64 MiB，`:32,259-292`）；zip 解析拒绝重复条目（`:316`）、manifest 未知键（`:343`）、
  校验整包 sha（`:350`）；
- **白名单式落点**：只允许替换 `config_custom.yaml` + `sub_config/*.yaml`，只允许新建内容为单个
  开关字符的 `rule/switchN.txt`，只允许删 `sub_config/*.yaml`，**任何未列出的文件 = 整包拒绝**（`:446-449,407-417,424-442`）；
- 备份 → 置 `in_progress` → 应用 → 校验配置树（include 深度 8、查环/重复 tag/未知类型，`:736-786`）→
  **只有引擎真的构建成功才提交状态**（`run.go:193-204`）；崩溃后开机先恢复 `in_progress`（`:597-604`）；
- 状态文件带 `in_progress|success|failed` 与中文说明，UI 轮询后显示（`api_update.go:77-94`、
  `SystemControlManager.vue:355-366,900-905,925-948`）。

ZBox 已有的对应物：不可变快照 + `check -D` + 空 inbounds 预跑 + 失败自动回 last-good + `rollback`。
**缺的两条**：①"引擎构建成功才算提交"我们其实有（`probe`），但**没有把 `in_progress` 这种中间态落盘**，
断电在"已切 symlink、未确认"之间的形态没人恢复；②**备份无保留策略**——他们 `backup/` 无限增长
（`config_update.go:659-666`、`config_manager.go:123`）是反面教材，我们的 `Store::prune(keep)` 已有，别在
状态文件上再犯一次。

### 5.6 记忆/域名生成（`domain_output`）—— 明确不做，但记下"我们的引擎里它是免费的"

机制：`plugin/executable/domain_output/` 从序列路径非阻塞计数域名，后台按 `dump_interval` tick/信号/Close 落盘，
写"计数文件 + `full:` 名单 + 模式化生成的名单"三份并按命中数排序（`:303-331,439-505,507-556`）；
回灌有两条通道 —— HTTP POST 进某个 set 插件（`:558-596`），或直接
`QuickAdd` 塞进 domain_mapper 的热图并带上目标 mark/tag（`:248-262`）。
UI 是"总开关 + 4 个记忆目标"（`SystemControlManager.vue:248-254`；被管的 tag 集
`domain_generation_settings.go:37-46`），**关掉某个开关会主动 flush 掉它生成的那些名单**（`:149-175`）。

ZBox 决策（`ARCHITECTURE.md` §8.5 已定"不做自研 DNS 分流层"）：**MVP 不做**。
理由：①要持久化"观察结果"这种第二状态源，和我们的"期望状态→快照"单向模型冲突；
②sing-box 侧没有"把一个域名热塞进某条规则"的入口，只能改写 `DesiredState` 再走一次完整下发（含重启），
而学习型规则每分钟触发一次重启是不可接受的；③它带来的收益（少配几条白名单）远小于它的不可解释性。
**要抄的只有那条关闭语义**：任何"开关关掉后还留着影响"的设计都是缺陷 —— 关掉必须连带清掉它生成的东西。

### 5.7 位命名空间的教训 → ZBox 的等价风险

他们最贵的一类 bug 是**编号撞车**：switch 位与查询上下文 `fast_mark` 共享一个 `uint64` 命名空间，
`fast_mark 48` 被统一匹配器当哨兵占用，于是 switch17 只能占 49
（`pkg/query_context/context.go:79-81,289-311`；`plugin/executable/switcher17/switcher17.go:23`；
`docs/ai/config-notes.md:58-73`）。撞车的表现是"跳过 unified_matcher1、规则显示 unmatched_rule、
一路落到 fakeip" —— **静默错分流，且日志看起来像"没命中"**。
另有一条极易抄错：配置里的 `mark: N` 在 mapper 内是第 `N-1` 位，再作为 flag `N` 发出（差一）。

ZBox 没有位命名空间，但有一个**同构风险**，且已经在代码里被钉住：
`DIVERT_MARK 0x1` ≠ `BYPASS_MARK 0x2`（`src/gen/singbox/route.rs:17-21` 的注释 + 单测
`outbounds_get_a_bypass_mark_so_they_are_not_re_intercepted`）。
把它写成一条纪律放进 `AGENTS`/架构文档：**任何两个进同一个内核字段（fwmark、table id、端口）的编号，
新增前必须 grep 现役取值集合**；他们的 `fast_mark 48` 教训对应我们的就是
`:53` 那个内置规则集 tag 守卫、`rule_set_product` 的 tag↔路径绑死、以及 `validate` 里的入站端口互不相同
（`doctor` 的 `port_conflict`）。

### 5.8 名单输入语法

他们用规则前缀类型（`full:` / `domain:` / `keyword:` / `regexp:`，`docs/fork_diff_summary_zh.md:41-44`）。
ZBox 已经有等价且更省字数的语法：`+后缀` / `~关键词` / `/正则/` / 裸名=精确 / `.` 开头也当后缀，
`#` 是注释（`src/model/config.rs:277-298`，单测 `:628-655`）。
面板的输入框**照这个约定写提示文案**即可，不要再引入 `full:` 前缀；
但他们的"每种名单类型下面挂一句语义说明"（`ListManager.vue:39-76`）值得抄 ——
分流类产品里"这个框是白名单还是直连名单"是唯一会让人连错三次的东西。
schema v2 之后这句话的落点变了：不再有"三个框各自的语义"要解释，而是一个**处置下拉**
（直连 / 代理 / 拦截）+ 一个**行序**。要抄的说明因此挪到表头："命中即定，排在前面的那条赢"，
以及每一行下面能就地显示医生的 `domain_rule_never_fires` / `block_never_fires_behind_a_pass`
——被盖住的行在运行时**不留任何痕迹**，面板不点名就等于用户永远不知道那一行是废的。

### 5.9 外观持久化 —— 只抄那条"为什么"

他们把面板背景/文字色/按钮色**存服务端 JSON**（`api_appearance.go:23-27,276,670-676,776-814`），
同时镜像进 localStorage 并在首屏前应用避免闪一下（`App.vue:60-68,205-227`）。
**理由值得记下来**：制品内嵌在二进制里、被多个客户端（含手机 WebView）访问，
localStorage 在那种环境不可靠/易丢（`docs/ai/project-context.md:105`）。
ZBox 面板同样会被手机访问，所以"任何**影响可读性**的偏好要放服务端"；
但上传背景图（20MB multipart + 历史目录，`api_appearance.go:26-27,197`）这种**装饰性**功能不做 ——
它在无鉴权局域网里还是个不小的攻击面（§8）。

### 5.10 自升级 —— 学它的"事务状态机"，不学"自动升级"

他们的二进制更新态是 `staged→installing→testing→verified→rolled_back`
（`update_guard.go:106,166,173,214,273,341`，回滚查询 `:349-376`），并且**只有存在 `verified` 时才清理旧的
txn 目录**（`:378-405`）。ZBox 的策略已定：sing-box **钉死 1.14.2**、`provision` 只按架构+libc 落地并
**用真跑 `version` 验收**（`ARCHITECTURE.md` §6 第 1 步），不做自动升级。
但要吸收"事务状态落盘 + 保留判据"这两点 —— 我们 provision 现在的中间态是目录级、可重入，但没有
"verified"这一层；换二进制时 `syscall.Exec` 式自替换尤其需要一个能回答"上一次替换是不是坏了"的落盘状态。

---

## 6. 查询日志页（#7）：他们有什么，我们能有什么

他们记录的字段（`coremain/audit.go:349-380` 从查询上下文回读，字段名见
`pkg/query_context/context.go:32-41`）：`matched_group` / `final_sequence` / `final_upstream` /
`final_upstream_targets` / `selected_upstream` / `matched_rule_source`，外加 `domain_set`（命中标签拼接）与
事后推导的 `effective_tag`（`computeEffectiveTag`，`audit.go:575-696`：先专属组 → 再一张固定优先级表 →
再 `!CN fakeip filter` → 再记忆 → 再按上游组**名字字符串**分类 domestic/cnfake=直连、
foreign/foreignecs/nocnfake=代理，`:546-555`）。聚合面是 8 个 `/api/v2/audit/*`：
`stats`、`stats/windows`、`rank/{domain,client,domain_set,effective,slowest}`、`logs`（`api_audit_v2.go:73-81`）。

UI 侧明确的一条原则（`docs/ai/config-notes.md:79-87`）：**不要把每个中间标签都当成"生效"来展示**；
展示"生效标签 + 最终上游组 + 最终上游"。他们的列表只放 5 个窄列（时间/域名+应答摘要/类型/延迟/客户端），
其余全在点行弹出的详情里，详情里每个关键字段都带"复制"和"以此值过滤"两个按钮
（`QueryManager.vue:762-803,932-1017,262-271`）。**这套交互形状直接搬**。

**ZBox 的现实约束（必须写清，别拿他们的字段表当需求）**：
我们没有 per-query 审计对象，能拿的只有 sing-box 的文本日志（`log.output`，`Layout::kernel_log_path()`）。
**已在 guest 实测过的日志形状**（`docs/reference/singbox-1.14.2-schema.md` §11.7、§11.8）：

- `dns: match[...] rule_set=... => route(...)` —— 域名命中与最终 DNS 去向；
- `inbound/tproxy[tproxy-in]: inbound connection to <原始目的地>` 与
  `outbound/direct|<组>[...]: outbound connection to ...` —— **成对**出现才证明 TPROXY 保住了目的地；
- `sing-box started (0.0Xs)`、`updated rule-set <tag>`、`updated default interface eth0, index 2` + 网关。

因此 ZBox 的查询日志页能给出的列是：**时间 / 客户端（从 tproxy 行的源地址）/ 目标域名或原始目的地 /
命中的规则来源（rule_set 名或域名）/ 最终出站（outbound 行的 tag）/ 延迟（要自己配对算）**。
拿不到的：上游组层级的"selected vs final"区分、TTL、应答 IP 列表（除非把 `log.level` 提到更细，
而更细的级别在实测里能把磁盘写满 —— 那条 350MB 自激日志就是量级参考，`ARCHITECTURE.md` §5 第 5 步）、
按设备别名聚合。
**待实测确认**（不要先写进设计）：`log.override_prefix` 能否稳定前缀化以便解析；
域名→客户端的反查在 `dns` 入站上是否拿得到源地址（DNAT 之后源地址是否保留，没验过）。

一个直接的产品结论：**#7 的第一版应该是"日志的读与过滤"，不是"新造一个审计层"**。
后者要写第二个状态源、要定容量、要落盘、要处理重启丢数据，收益不如先把现成的判决轨迹读对。
`doctor` 已经是"期望 vs 现场"的读法（`src/doctor.rs:205`，14 条 `Finding::new`，即 14 个 finding code），
监控页应当复用它，而不是平行长出一套判断。

---

## 7. 他们的文档与代码不一致（同一份报告里必须留档，免得下轮又照文档实现）

| `docs/api_reference.md` 的说法 | 代码事实 |
|---|---|
| `:261` slot 50-59 | 只有下界 `>= 50`（`api_special_groups.go:539`）|
| `:491` `update_from_url` "失败时自动回滚" | 先备份、返回错误时**不恢复**（`config_manager.go:117-138`）；备份目录无限增长 |
| `:751-757` §12 aliapi 暴露 `/domains` `/ipv4` `/ipv6` | aliapi 插件**没注册任何 API**（无 `RegAPI`）|
| `:700,740` 添加/更新用 `POST /plugins/{tag}/config` | 实际是 `PUT /config/{name}`（`sd_set_light.go:563` 等）|
| `:731,743` 每个 set 插件都有 `POST /update`（更新全部）| 只有 adguard 有（`adguard.go:848`）|
| `:736-743` domain_set/ip_set 有 `/config` `/update` | 只有 `/show` `/save` `/post` |
| `:765` 所有失败都返回 `{"error":…}` | 三套并存：JSON、手搓 JSON 字符串、**纯文本**（`api.go:33,39`、`config_manager.go:42,46`）|
| `:9` 面板端口 9099 | 端口运行时可改（`webui_port.go:121-144`）|
| 整份文档 | 漏了 `/api/v2/audit/*`(8) 全部、`/api/v1/appearance/*`(12) 全部、`system/health`、`system/webui-port`、`upstream/sources`、`/assets/*`、`/ui` 动态挂载、pprof、绝大多数 `/plugins/*` |
| `docs/ai/config-notes.md:19` "两边都有时优先 webinfo" | 代码可能拿**更新的**遗留文件覆盖 webinfo（`state_files.go:240-244`）|
| `:21` "生成物不要放进 managed_files" | 生成的 `special_groups.yaml` 就在替换白名单里（`config_update.go:446-449`）|

**方法结论**：他们的接口文档只能当**入口线索**，任何据它写进 ZBox 的字段/路由都必须回代码或回实测确认。
同一纪律适用于本报告：§2/§3 的每一行如果要落到代码，先打开那个文件看那几行。

---

## 8. 绝不抄清单（含反面教材）

1. **无鉴权**。`coremain/` 全仓没有任何鉴权中间件（只有 CORS 把 `Access-Control-Allow-Origin` 设为 `*`，
   `mosdns.go:269-284`）；`/debug/pprof/*` 常开（`:402-408`）；未匹配路由的 404 会**打印路由遍历结果**
   （`:411-425`）。任何局域网设备都能改网关、重启服务、读走明文 AK/SK（`upstream_overrides.json` 里存
   AK/SK 且以 0644 落盘）。ZBox 侧的对策已在 `ARCHITECTURE.md` §7/§8.4：**LAN 绑定 + argon2 口令 + 会话 cookie，
   不是可选项**。#6 的接口设计必须从"每个 mutation 都要会话"开始，别做完再补。
2. **调用方传目录**。见 §4.1。
3. **静默重启**。见 §5.4。
4. **固定名临时文件 / 无 fsync / 0644 存密钥**。见 §4.3。
5. **靠上游组名字符串分类判直连/代理**（`audit.go:546-555`）：改名即改语义，是隐藏耦合。
   ZBox 侧等价做法只能是显式字段（我们的 `FinalOutbound` / `DeviceMode` 都是显式枚举）。
6. **装饰性上传端点在无鉴权面上**（背景图 20MB + 历史目录）。真要做也必须先有鉴权和大小/类型约束。
7. **`config_overrides` 保存后要求人工重启**却把这条信息塞在 message 里（`api_overrides.go:130-147`）：
   要么当场生效，要么 UI 明确标"待下发"，不要靠一句提示。
8. **互斥开关只在前端拦**。`IPv4优先` 与 `IPV6屏蔽` 的互斥**只存在于 UI**
   （`SystemControlManager.vue:633-662,747-752`），后端无守卫。
   ZBox 已经用类型层解决：`DnsStrategy` 是一个枚举（`config.rs:209-215`），`prefer_ipv4` 与
   `ipv6_only` 在结构上不可能同时为真。**这条纪律保留：状态互斥要在模型层消灭，不要写在校验里，更不要只写在 UI 里。**

---

## 9. 本报告直接产生的待办（按性价比排）

1. ~~**#6 前置**：`DesiredState` 的规范落点 + schema 版本字段 + 原子写（§4 三条）~~
   **已落地 2026-10-05**：`src/model/state.rs`（落点 `Layout::state_path()`、`schema` 键用
   `flatten` 平铺、缺键按 v1 读、新于 v1 直接拒、唯一临时名 + fsync + rename、Unix 上创建即 0600）。
2. ~~**rule-set 残留**~~ **已落地**：`plan_reclaim` 按清单（不按目录）回收，判据与边界写在
   `src/svc/ruleset.rs` 的 §7.3 那组测试里。当时定位残留的依据仍然记在这儿——同步只往清单里
   **插**现役 tag 的条目（逐条 `manifest.insert` + `write_manifest`），全文没有"回收现役 tag
   之外的产物与条目"的步骤（无 retain/prune/孤儿清理），于是删掉一个 `RuleSet` 之后
   `rule-set/<tag>.srs` 和它的条目都留在原地；guest 上真见过两条来自已消失 tag 的残留。
   思路照他们删除级联的那条（§5.2）。
3. ~~**同步失败时报告被吞**（§5.3 末）~~ **已落地**：`SyncFailure { report, source }`，
   `sync_rule_sets` 先打印已落地的部分再把原因报出去、退出码非 0。
4. ~~**把 §3 的冲突表翻成 `route.rs` 的顺序断言单测**（§1.3.3）~~ **已落地 2026-10-05**：
   `src/gen/singbox/route.rs:528-749` 那组"映射报告 §3 那张冲突表的护栏"测试。断言的是**索引先后**
   而不是"规则存在"——`route` 与 `dns.rules` 的 `action:"route"` 都是终止语义（schema §11.7），
   命中多条名单时谁赢完全由生成顺序决定，配置里不留命中痕迹。最初的四条覆盖：拦截/直连压过代理侧、
   `geosite-cn` 压过 `non-cn`（他们的 `cn@!cn`）、私网与局域网段压过代理名单（他们第 16 级的
   等价物）、拦截与设备级直连压过 fakeip 段捷径。
   **做这件事时挖出一条真问题**：同一个域名同时进 `direct_domains` 和 `blocked_domains` 时，
   路由层判直连（第 5 步 < 第 8 步），DNS 层判拦截（reject 早于 direct）——两层结论相反，
   而 `validate()` 不禁止重叠。已经用 `direct_versus_blocked_is_ordered_inconsistently_across_the_two_layers`
   把**当时的矛盾**钉住；#18 已定案为**拦截优先**（选 A，两层同向 + doctor 两条 Warn），
   那条断言当时改名 `blocked_outranks_direct_in_both_layers`，选项记录与本服参照方向见 §3 末。
   ~~选项 C 仍待 #6 决定~~ **也已落地（2026-10-05，schema v2）**：三档合成 `Policy.domain_rules`
   一条有序名单之后，上面那两条名字**都已不存在**——"拦截 vs 直连"不再是两条档之间的关系，
   而是名单里的行序关系。护栏因此变成七条：`the_list_order_decides_which_action_wins`（名单内部行序）、
   `cn_geo_rules_outrank_the_non_cn_rule`、`the_user_list_outranks_the_geolocation_defaults`（v2 的
   唯一语义变更）、`the_ads_rule_set_still_precedes_the_user_list`、`private_destinations_outrank_the_user_list`、
   `blocking_and_device_rules_outrank_the_fakeip_shortcut`、`the_same_list_expands_in_the_same_order_in_both_layers`
   （拿同一份名单在两层各展开一遍、逐行比对，防止"路由判代理 / DNS 判国内"这种 #18 形态复发）。
5. **`!CN fakeip filter` 的等价开关**（§3 末），并在 guest 上补一轮"混合 fakeip"实测。
6. `api/` 的路由表按 §2 + §6 起草，**且从第一条 commit 就带会话**（§8.1）。
7. 已知仍只在文档层、没落进代码的：apply 的 `in_progress` 中间态落盘与开机恢复（§5.5）、
   provision 的 `verified` 层（§5.10）。这两条都不阻塞 #6，排 #6 之后。

*勘察日期 2026-10-05。参考仓库为 GPL-3.0 只读参照物，未做任何修改；其内容不进 ZBox 构建产物。*
