# LocalDNS-FJ945 勘察与取舍

对象：`LocalDNS-FJ945_Alpha.zip`（Telegram 分发的成品包，97 条目 / 解包 16MB）。
勘察方式：**全程只读**，未执行包内任何二进制；解包前做过路径穿越/symlink 预检（0 命中）。
解包位置是临时目录，**不进仓库**（结论已经在这里，二进制留着没有额外价值）。

## 它是什么

闭源 Rust DNS 分流器，站在 sing-box **前面**当 DNS 大脑，不是代理内核。

| 证据 | 内容 |
|---|---|
| ELF `.comment` | `Linker: LLD 22.1.8` + `rustc version 1.98.1 (48a229cea 2026-09-01)` + GCC 13.3 |
| `DT_NEEDED` | `libz` `libssl.so.3` `libcrypto.so.3` `libgcc_s` `libm` `libc` → 动态链接，glibc ≥ 2.34 + OpenSSL 3 |
| 运行时字符串 | tokio / hyper / rustls + OpenSSL，含 HTTP/2 与 QUIC 帧断言 |
| 交付物 | `localdns-FJ945-linux-{x86_64-v3,x86_64-v1,arm64}` 三个 CPU 基线构建 |
| UI | 由进程自己嵌着发（`index.html` + MIME 表 + `/ui`），`window.MOSDNS_UI_CONFIG` 说明前端命名空间仍是 mosdns 系 |

它与 sing-box 的接缝在配置里写得很明白：`resolvers.yaml` 的 `foreign_fakeip` 上游是
`udp://127.0.0.1:9333`（sing-box 的 DNS 入站，用它产 fakeip）；`upstream_overrides.json`
里每条国外 DoH 都挂 `socks5: "127.0.0.1:9666"`（sing-box 的 SOCKS 出站）。
判决标签是一组枚举：`block / force_domestic / force_foreign_fakeip / force_foreign_realip /
learned_realip / learned_fakeip / no_v4 / no_v6 / prefer_blocked / not-in-list / adguard / cache-refresh`。

其余机制：mosdns 式 `include:` + `plugins:` 声明流水线；四类 `learned_list`
（domain_stats / realip / fakeip / no_ipv4 / no_ipv6，`max_entries` 1e7~1e8）；
provider 层把 `srs / txt / clash_yaml` 归一化后重建 `rulesets/bundles/*.srs`；
缓存落盘带 CRC32C 与长度校验（`cache-dump-identity-v2`），坏档丢弃而不是崩溃。

## 采纳了什么

| 借鉴点 | 它怎么做 | ZBox 落点 |
|---|---|---|
| **policy hash 短路** | 字符串 `LocalDNS policy hash v1`，用内容指纹避免无谓重载 | `apply::store::content_hash()` + `digest_of()`，`apply::engine::apply()` 在指纹与 last-good 相同时返回 `ApplyOutcome::Unchanged`：不写快照、不校验、不重载、不探测。指纹覆盖**整个快照**（config.json + zbox.nft + netstate*.sh），所以任何一份文件变了都会真下发 |
| **provider 语法收敛** | `detected_format` 支持 srs/txt/clash_yaml，统一编译成 bundle `.srs` | `ingest::ruleset::prepare()`：AdGuard 文本 / 面板同款域名语法列表 / sing-box 规则集 JSON / 已编译 `.srs` 四种来源，收敛成 `rule-set/<tag>.srs`；不自己写 SRS 编解码，交给官方 `sing-box rule-set compile`（命令形状见 schema 文档 §10） |
| **空源守卫** | 二进制里有 `writing an empty bundle`、`downloaded domain rule file is empty` | `RulesetError::EmptySource`：解析出 0 条就报错，绝不用空名单覆盖现有规则集。这是下载抖动最容易造成的静默降级 |
| **tag 当路径用要校验** | `unsafe provider source path`、`provider source must be under rulesets/upstream` | `validate_tag()`：只允许字母数字与 `-_.`、不得以 `.` 开头、不得为 `..`。tag 会被拼成 `rule-set/<tag>.srs`，这里是唯一防线 |
| **来源清单可核对** | manifest 里记 `rule_count` `last_updated` `detected_format` `auto_update` | `Plan.rule_count` + `Plan.format`（二进制产物恒 0，ZBox 不解析 SRS，不做假计数）。清单落盘属 svc 层任务 |
| **失败不留半截产物** | 它靠 provider rollback | compile 本身失败时 sing-box 会 `os.Remove(outputPath)`；svc 层还须"只在编译成功时替换旧 `.srs`"，见任务 #9 |

面板侧的参照（不写代码，只校准 IA）：三段导航 监控/配置/系统、审计接口
`/api/v2/audit/logs|stats|rank/<type>`，与 `docs/ARCHITECTURE.md` §7 已定的结构一致，无需改动。

## 明确抛弃了什么

| 它的机制 | 为什么不进 ZBox |
|---|---|
| **外部 DNS 守护进程前置** | 多一个常驻进程 + sing-box 要开 9333/9666 两个内部端口。ZBox 的定位是"一键配置"，每多一个组件就多一套安装、鉴权、日志、升级面。MVP 继续用 sing-box 内置 DNS |
| **learned_list 记忆名单**（no_ipv4 / no_ipv6 / realip / fakeip） | sing-box 内置 DNS 表达不了"这个域名从来没有 A 记录"这类观测事实，只能靠外部解析器累积。属于后增强的性价比取舍，不是 MVP 缺口 |
| **规则热更新走 HTTP API**（`/plugins/<tag>/post|show|update|flush`） | 与"声明式期望状态 → 不可变快照 → 原子切换"是两套真相源。混用会让"当前配置"无法回答。真需要减少重载时，policy hash 短路已经覆盖了绝大多数无谓重载 |
| **运行时可变的 settings/*.json** | 同上：它的 yaml/json 会被 API 就地改写，ZBox 一切变更都经过期望状态与快照 |
| **自带 Web UI 嵌在二进制里** | ZBox 面板是独立的 Vue 3 前端（clean-room 要求，且 MosDNS-T 是 GPL-3.0；这个包连源码和 license 都没有） |
| **三个 CPU 基线构建** | 数据面是官方 sing-box 二进制，ZBox 控制面自身对 CPU 特性不敏感 |

## 顺带记下的安全事实

`localdns.service` 是 `User=root`，`localdns.yaml` 是 `api.http: "0.0.0.0:9099"`，
而在二进制里搜不到该 API 的任何鉴权实现（`password/token/authorization/csrf/allow-origin`
全无命中，只有 hyper 自带的 `Unauthorized` 状态字）。局域网内任何设备都能读写它的 DNS 配置与名单。
若有人在自己的网络里试跑：不要把设备 DNS 指向它，或先把 9099 限到本机。
