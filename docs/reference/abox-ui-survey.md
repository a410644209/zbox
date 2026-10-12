# abox 面板版式勘察与取舍

对象：`reference/abox/abox-ui`（52,297,890 字节，静态链接 Go ELF，stripped）。
勘察方式：**只读字节扫描**——没有反编译、没有执行、没有挂起它的面板。
一次性脚本在 `.tmp/abox/probe.js`，中间产物（`ui.strings` 是二进制的逐字副本、
`cjk-counts.txt` 是中文串计数、`theme.css` 是抽出的那一段样式）都留在 `.tmp/`，**不进仓库**。

## 为什么不能用 `strings | grep '[\xe4-\xe9]'`

第一轮就是这么做的，出了两类假结果：

| 现象 | 原因 |
|---|---|
| `tr -c '[:print:]' '\n'` 之后中文全碎 | 它按字节判定可打印性，UTF-8 的多字节序列被从中间切开 |
| C locale 下 `grep -E '[\xe4-\xe9][\x80-\xbf]…'` 吐出一万行 ASCII | 方括号里 `\xe4` 不是字节转义，退化成 `\\, x, e, 4, -` 这些字面字符的集合 |
| Node 里 `buf.toString('latin1')` 也对不上 | 编码选错，中文变成高位噪声 |

可用的形状：Node 里 `fs.readFileSync(...).toString('utf8')`，再用 `/[\u4e00-\u9fff]/` 按码点取。
这条与 VM 侧那条"别在 `ssh sh -c` 里 `pkill -f`"是同一类教训——工具默认假设不是这里的事实。

## 它是什么（observed）

前端指纹：Next.js App Router 的静态导出，Tailwind 工具类 + shadcn 的 HSL CSS 变量 +
radix 无样式组件 + lucide 图标。整个导出嵌在 Go 二进制里由进程自己发。
它同时是一个"多枢纽"产品（`abox` / `-ai` / `-home` / `-hub` / `-runtime` / `-ui` 六个二进制），
所以版式里有一大半屏（智能家居、统一控制、AI 模型）与旁路由无关，能参考的只是**骨架与配色语言**。

## 页面段清单（observed）

`dist/<segment>/` 在二进制里各出现 6 次（`login_redirect` 7 次）——即每段都真的产出了页面：

| 段 | 出现次数 |
|---|---|
| `dashboard` `subscription` `router` `system` `logs` `navigation` `ai` `ai-models` `home` `home-automation` `smart-devices` `unified-control` `advanced` `login` | 各 6 |
| `login_redirect` | 7 |

**inferred 边界**：段名证明"有这么一屏"，不证明那一屏里长什么样。ZBox 只借它前四组
（仪表盘 / 订阅 / 路由 / 系统 + 日志）的信息架构切法，智能家居那三段与本产品无关。

## 版式原语（observed，逐条给计数）

| 字面 | 命中 | 用在 ZBox 哪里 |
|---|---|---|
| `rounded-lg border` | 142 | `.card`：1px 边框 + 圆角，是它最主要的容器形状 |
| `tracking-tight` | 80 | 标题字距收紧 |
| `grid-cols-3` | 29 | 概览屏的三列指标卡 |
| `sidebar` | 31 | 左侧导航这条轨 |
| `sticky top-0` | 13 | 吸顶顶栏 |
| `min-h-screen` | 5 | 整页高度 |
| `px-6` / `py-6` | 25 / 20 | 卡片内边距 |
| `Sheet` / `TabsTrigger` | 15 / 1 | 抽屉与页签（radix），ZBox 不做 |

**这一条已被推翻**：我先前记的是相邻字面 `px-6 py-6`，实际**零命中**——`px-6` 和 `py-6`
只在不同元素上各自出现。相邻类串不能凭印象拼，要么测出来要么不写。

## 侧栏可收起（observed）

`收起导航` 25 次、`展开导航` 5 次。文案是中文而不是图标 aria-label，说明收起态是**主交互之一**，
不是无障碍附属品。ZBox 照做：`#nav-toggle` + `body.nav-collapsed`。

## 状态词（observed，计数）

`已停止` 10 / `未配置` 9 / `运行中` 9 / `异常` 9 / `待接入` 6。
这是一套五档的、面向非专业的中文状态词。ZBox 的概览与诊断屏沿用同一种取向：
每一栏要么给结论、要么明写"未读取"，不出现空值。

## 主题变量实测值（observed，全份）

```css
.dark{--background:220 12% 6%;--foreground:210 20% 96%;--card:220 10% 10%;
--popover:220 10% 10%;--primary:214 100% 55%;--secondary:220 9% 14%;
--muted:220 8% 12%;--muted-foreground:220 9% 66%;--accent:220 9% 14%;
--destructive:0 72% 56%;--success:142 70% 45%;--warning:38 92% 50%;
--border:220 8% 18%;--input:220 8% 18%;--ring:214 100% 55%}
```

`--radius:0.75rem`。四个自定义类（说明它没有全靠 Tailwind 工具类堆）：

```css
.workspace-surface { border:1px solid #242428; background:#17181b; color:#f4f5f6; box-shadow:none }
.workspace-empty   { border:1px dashed #2a2c31; background:#111216; color:#8f939b }
.workspace-control { border-color:#2a2c31; background:#101114; color:#f4f5f6 }
.subscription-page-card { border:1px solid #24262b;
  background:linear-gradient(180deg,#191b1f,#17191d); box-shadow:none;
  transition:transform .22s ease, border-color .22s ease, background .22s ease }
```

值得记的两条：它**全程 `box-shadow:none`**（深色下投影看不出，靠边框分层），
以及**空状态用 1px 虚线框**而不是灰底块。

## 采纳了什么

| 借鉴点 | 它怎么做 | ZBox 落点 |
|---|---|---|
| 左导航 + 分屏 | `sidebar` 一轨，每屏一段 `dist/<segment>` | `#sidebar` + `nav#nav`（四组「这台机器 / 期望状态 / 只读 / 送到机器上」）+ 13 个 `.nav-item[data-view]`，一屏只有一个 `<section class="page">` 不 `hidden`。原来是**一页长滚**，编辑区十几个小节挤在同一屏里 |
| 吸顶顶栏放动作 | `sticky top-0` | `header.topbar`：`#page-title` + `#page-sub` + 工具条。**保存 / 放弃改动全页只有这一份**，非编辑屏隐藏——改哪一屏存的都是同一个 `state.json`，每屏一个保存按钮会给出"这一屏独立提交"的假象 |
| 卡片形状 | `rounded-lg border` + `box-shadow:none` | `.card{border:1px solid var(--line);border-radius:10px;background:var(--panel)}`，不投影 |
| 强调色只给主动作 | `--primary:214 100% 55%`，`--destructive` 另立一档 | `--accent` 只涂在 `#connect` / `#save` / `#apply`；`#apply` 用 `--bad`（它会掐连接，红是它该红的） |
| 空状态虚线框 | `.workspace-empty` | 名单 / 上游 / 节点为空时那行 `.note` 说明"这一组目前是空的"，配同一语言 |
| 侧栏可收起 | 「收起导航」/「展开导航」 | `#nav-toggle` 切 `body.nav-collapsed`，收起态只剩 4.1rem 一轨。**图标资源带不进来**（CSP `script-src 'self'` + 零构建 + 不放二进制），所以用每屏名字的第一个字当 `.mark`——这个取舍写在 CSS 注释里 |
| 中文状态词五档 | 运行中 / 已停止 / 未配置 / 异常 / 待接入 | 概览每一栏一律"结论或未读取"，诊断屏每条 finding 一句人话；**没有对应上游的键一律直说不支持**（见「生成预览」那屏的措辞） |

## 没采纳什么，以及为什么

| 它的做法 | ZBox 不做的理由 |
|---|---|
| Next.js 静态导出 + Tailwind 编译 | **零构建**是硬约束：三份资产 `include_str!` 进二进制，改完重编就是新版。引入打包器等于把"改一行样式要跑 npm"塞进交付链 |
| shadcn 的 HSL 三元组变量（`--primary:214 100% 55%`） | 那套形状是为浅/深两档各填一遍设计的；ZBox 现在只有深色一套，直接用 hex 变量，省掉 `hsl(var(--x))` 这一层间接 |
| radix 的 `Sheet` / `TabsTrigger` | 无依赖可引；分屏导航用委托点击 + `hidden` 属性就够 |
| 每段一个 URL（App Router 路由） | 面板的路由表是 `const ROUTES` 字面量，它同时是 404/405 的**唯一依据**。加假的路由段会让"这个端点存不存在"泄漏到鉴权之前 |
| 浅色档 | 没测过对应调色板，凭空写一套只会做出两边都难看的中性灰。**只做深色是当前的取舍**，注释里写明 |

## TrueNAS 那一层来自哪里

不是从 abox 抄的。abox 的 `--background:220 12% 6%` 已经很接近，但它整体偏暖、
卡片带渐变；TrueNAS 的观感来自**更冷的蓝灰 + 顶栏吸顶 + 列表表格化 + 强调色只在动作上**。
所以 `assets/panel.css` 的调色板是我自己填的 hex（`--bg:#10151a;--panel:#171e25;--line:#26313a;
--fg:#e6edf3;--muted:#93a1ad;--accent:#2f80ed`），取的是 abox 的**结构与分层语言**、
TrueNAS 的**色温与克制**。这一条属于风格取向，没有实测依据，别当事实引用。

## 怎么复跑

```bash
# 中文串计数（Node 解码，不要用 strings/tr）
node .tmp/abox/probe.js cjk > .tmp/abox/cjk-counts.txt
# 主题那一段
node .tmp/abox/probe.js css
# 相邻类串要单独测，别拼
grep -a -o -F 'px-6 py-6' .tmp/abox/ui.strings | wc -l
```

要扩到组件级交互（哪一级 hover、抽屉里装什么），这份勘察给不出答案——它在 stripped
二进制上不可读。**但"看整屏版式"不需要挂起面板**，见下一节（这条是 2026-10-09 补的，
它推翻了本文原先"只能挂起面板看"的说法）。

## 2026-10-09 补：不跑服务把原版 UI 看成图（15 张 1440 宽）

`.tmp/abox-ui-view/gallery.html` 是成品，`shots/*.png` 是单张。做法（一次性脚本
`.tmp/abox-ui-extract.js` / `-css.js` / `-css2.js` / `-ui-build.js`，都在 `.tmp/`，不进仓库）：

1. **HTML**：内嵌的 Next.js 静态导出是**明文**躺在 rodata 里的，18 份文档按
   `<!DOCTYPE html` → 配对的 `</html>` 切，长度 10,444–85,992 字节。用
   `buf.toString('latin1')` 保字节对齐（位置要换算回偏移），落盘时再转 utf8。
2. **CSS**：Next.js 按路由切 CSS chunk，本例两份——171,022 字节的主 Tailwind 产物
   （`*,:after,:before{--tw-border-spacing-x:0…`，2193 条规则）+ 10,720 字节的登录页
   CSS Module（`.abox-logo_logo__…`，95 条）。**只抽主那份，登录页会渲染成无样式的裸表单**
   （实测过，图会歪）。
3. **切 CSS 的坑**：Go embed 的数据区把文件**首尾相接**放着，中间没有不可打印字节，
   所以"最大可打印串"会把 CSS 和后面的 JS 图标数据并成一块（我第一版就把 21,908 字节的
   混合块当 CSS 写了出去）。可用的收口是：从 CSS 起点往后找第一个 JS 记号（`\"`、
   `!function`、`.push([[`），在它之前最后一个 `}` 处断。
4. **渲染**：剥掉全部 `<script>`、把两份 CSS 内联进 `</head>`，然后
   `chrome --headless=new --window-size=1440,900 --screenshot=… file:///…/page_x.html`。
   **不用起任何服务**，也不碰 abox 后端。窗口宽度就是响应式断点的依据，所以这一版
   顺带补上了"桌面宽屏 ≥896px 的观感"那条欠账（虽然是 abox 的，不是 ZBox 的）。

**看到的、上一轮字符串扫描看不到的四件事**（都是要借的）：

| 观察 | 对 ZBox 的含义 |
|---|---|
| 侧栏**按域分组带小标题**：概览 / 网络服务 / AI Switch / 智能家居 / 系统配置 / 个人导航，每组挂 2–4 项，底部固定「退出登录」 | 我们 13 屏现在是平铺的，这是最该借的一条 |
| 设置页把「访问来源」做成**一卡三档**：仅局域网 / 指定 IP / 不限制，每档下面一句解释 | 与 `--bind` 的 LAN 判定 + `127.0.0.1` 那条路是同一个问题，形状可直接参考 |
| 「配置恢复」一卡放四件：导出配置 / 导入配置 / 恢复订阅配置 / 恢复出厂，各带一句"会做什么" | 对应 rollback + 规则集重抓 + provision 重放 |
| 卡片外壳语言：标题左侧圆形图标底 + **右上角状态徽标**（`未配置` 红 / `可访问` 绿），卡内用虚线子块，危险动作靠**底色**（绿调=备份、红调=恢复出厂）区分 | 我们的回执徽标已经是这个形状，危险档用底色这条是新的 |

**仍然看不到**：实时数据（列表与数字是运行时拉的，静态页里就是「加载中…」或空态）、
hover / 抽屉 / 弹窗（脚本被剥了）。所以这组图只当**版式与措辞**参考，不当交互参考。
（**这一条已被 2026-10-11 那一轮补掉一半**：弹窗与"拿不到数据时的界面"现在都看到了，
见最后一节。实时数据那一半仍然看不到，原因也查清了——它要后端。）

## 2026-10-11 补：把它真跑起来看（截图 + 逐屏文字清单）

### 怎么跑的，以及它留下了什么

`abox-ui`（52,297,890 B，`--version` → `1.1.0`）**scp 进 NAT guest 单独起，不接任何 abox 后端**：

```sh
nohup ./abox-ui > ui.log 2>&1 &      # 起前存 ss -lnt，起后用 comm -13 求新监听口
```

- 只认 `--version`；`--help` / `-h` / `help` / 空参一律**被忽略、直接起服务**（rc=143 是被我 kill 的）。
  所以"先看帮助"这条路不存在，只能起起来再问。
- 绑 `*:80`（不是高位口，起之前必须先看口有没有被占）。
- 足迹：自建 `/opt/abox/`（`app.log` 46 行 + `data/abox-ui.db` 167,936 B + `data/abox-manager.db`
  126,976 B + `hermes-bridge/` + `ui/`）和空目录 `/run/abox/`。**不碰 `/etc`、`/var/lib`、`/root`**
  （`find -newermt` 查过）。stdout/stderr 那份 `ui.log` 是 0 字节——日志走的是它自己的
  `/opt/abox/app.log`（fd 7），库是 fd 3 上的 SQLite。**"一个面板自己开 SQLite"是它的形状，
  不是我们的**：ZBox 的落点只有 `<root>/state.json` + `revisions/`。
- 复原：`kill <PID>`（**别在这台 guest 的 `sh -c` 里 `pkill -f`**）→ `:80` 释放，
  `nft list tables` 空、`ip -4/-6 rule` 只剩 0/32766/32767、`rc-service zbox status` started。
  探针件留在 `/root/abox-probe`（70.5 MB）与 `/opt/abox`（860 KB），**都不再占口**。

### 它是什么（这一轮才确定的）

日志里逐字写着它在 dial `unix /run/abox/abox-home.sock`、`abox-ai.sock`、`abox-hub.sock`，
全部 `no such file or directory` —— **`abox-ui` 是一个 gRPC 客户端**，六个二进制靠 unix socket 拼成一个控制面。
所以"单独跑 UI"看到的恰好是**它拿不到任何数据时的样子**：空态、`待接入`、`同步中`、`--`、
以及把 `rpc error: code = Canceled desc = grpc: the client connection is closing`
**原样吐到仪表盘右下角红 toast** 里。这一半比有数据的一半值钱得多——ZBox 要回答的
"读不到的那一栏该说什么"，它全给出来了。

顺带一条实测：`当前系统：Alpine Linux 3.21.8`、`Core Ultra 7 265K`、`679 MB / 39.2%`、
`主机IP：192.168.219.128` —— **没有后端也拿得到**，即 UI 进程自己读本机（`/proc` 那一类）。
ZBox 的概览只报"服务在听哪些口、劫持层加载没有"，**内存/CPU 占用这一栏我们是空的**。

### 逐屏：真跑起来展示了哪些内容（observed）

侧栏 12 项 + 「退出登录」，分 6 组（概览 / 网络服务 / AI Switch / 智能家居 / 系统配置 / 个人导航）。
`ai` `home` 那几段与旁路由无关，下面只记相关的六屏：

| 屏 | 展示了什么（这就是"内容刚刚好"的那份清单） |
|---|---|
| 仪表盘 | 8 张指标卡（2×4）：当前系统（版本 + 主机 IP + CPU 型号 + 内存%）、AI 网关（状态）、路由器流量（上/下）、Singbox 流量（上/下 + 累计 B）、Singbox（运行状态 + 启动时间 + 运行时间）、订阅（**订阅数 / 节点数 / 国家数 / 规则数**四格 + "暂无国家分组"）、智能设备（设备数量 / 在线 / 离线 / 类型）、路由器（CPU / 内存 / 磁盘 + 两个环）。下面两张 AI 图表 + 一张全宽流量图，**图框与坐标轴照画**，中间写"暂无 AI 请求记录"。 |
| 路由·策略 | 顶栏一句总量（`共 0 个订阅，0 个节点`）+ 两个动作（添加节点 / 添加订阅）。三段：**订阅**（列表）、**地区分流**、**应用分流**（`0 规则` 计数挂在标题下面）。三段的空态都是虚线大框 + 一句"点击上方按钮添加订阅地址"。 |
| 路由器设置 | 7 张卡：路由器接入（开关 + "已停用"）、上网模式（当前模式 / 接入方式 / 网口 + 两个下拉 + 保存）、DHCP 服务（LAN 接口 / 当前网关设置 / 起始地址 / 结束地址 / 下发网关 / 下发 DNS + 加号）、**NAT 转换**（整张空卡只写"当前路由器模式下不支持该功能"）、WireGuard 回家（启用开关 + 二维码位 + "请先接入…并且家里宽带需要支持 IPv6"）、代理 DNS 服务器设置（解析策略(全局) `仅 IPv4` + 代理/直连两个页签 + 服务器 + 类型）、局域网唤醒（设备表）。 |
| 设置 | 8 张卡：版本总览（ABox / Sing-box / 路由器 / Hermes Agent 各挂 `待接入` + 检查更新）、本机网络（网卡 / IP / 网关 / DNS + 两句警告 + 左下"未识别"右下"应用"）、通知消息渠道、登录账号密码、**安全设置**（`当前来源 192.168.219.1` + `可访问` 徽标 + 仅局域网/指定 IP/不限制三档 + 虚线子块逐条解释 + "安全策略保存在本机数据库"）、**配置恢复**（导出/导入=绿底、恢复订阅配置=蓝底、恢复出厂=红底，各一句"会做什么"）、授权激活码（"留空后保存可清除本地已保存的…"）、后台任务。 |
| 日志 | **四个来源各一张卡**（ABox 运行日志 `已接入` / Sing-box `待接入` / 路由器 `待接入` / Hermes Agent `待接入`），选中那张涂蓝。工具条：日志级别下拉（信息）+ `运行中` 徽标 + 复制日志 + 暂停实时流 + 清空视图。元信息行：`服务` `最近刷新 2026-10-02 09:26:22` `实时推送已连接`。正文按级别着色。 |
| 导航 / advanced | 几乎什么都没有（206 / 119 字符，"加载配置中…"）——**没有后端就没有内容，这两屏等于不存在**。 |

**弹窗（点开才看得见的那一半）**：

| 弹窗 | 字段（逐字） |
|---|---|
| 添加订阅 | 订阅名称；订阅地址或内容——一个 textarea 吃四种输入，placeholder 明写"支持 `https://` 订阅地址、明文节点、Base64 订阅内容或 Clash YAML"，另给 `地址↔文本` 切换 + 上传 YAML |
| 添加手动节点 | 添加方式（表单录入 / 文本导入）；节点名称；协议类型（VLESS / VMess / Trojan / Shadowsocks / Hysteria2 / AnyTLS / **WireGuard** / SOCKS / HTTP）；地区（两百多项的国旗下拉，喂的就是"国家分组 / 地区分流"）；服务器；端口；用户标识 (UUID)；流控 (Flow) |
| 新增规则模块 | 模块名称（例如：Youtube / Telegram / CN）；目标出站；节点组模式；匹配模式；规则关系；规则集（一行一条 URL，上面一句"通过 sing-box 规则集匹配一组域名或 IP 规则；支持远程 `.srs` / `.json`；AdGuard txt 会在后端转换为 `.srs`"）。**校验失败时下面一整块红底把每条问题列全**："模块名称不能为空。规则集链接不能为空。" |
| 局域网唤醒 · 添加 | 设备名称；设备 IP；MAC 地址；备注 |

### 与 ZBox 15 屏的差距：抄哪些、不抄哪些

> **这一份清单已被取代（2026-10-11）**：它写于用户定调"abox 没有 dae 和 oxidns，咱们的 UI 没法照抄，
> 要对应到各自的功能上"之前，数的是版式原语与信息架构，没有逐格回答"这一屏对 dae / oxidns 说不说得出口"。
> **逐格的 保留 / 改造 / 撇掉 判定 + 我们真正缺的七块 + 定屏集，在 `docs/BLUEPRINT.md` §11。**
> 本节下面的 observed 记录（截图能看到的形状与逐字文案）**仍然有效且不再重做**，只是结论表换了地方。

| # | 抄 | 落到哪一屏 | 为什么这条值得动 |
|---|---|---|---|
| 1 | **日志来源做成"一来源一卡 + 接入徽标"**，选中态涂底色 | 「日志」 | 我们的日志屏现在只有一个 `#logs-out`。而 ZBox 恰恰有**三个**日志来源（dae / oxidns / 内核层脚本），全挤在一个框里就会重演"两句话都真、说的却是两份生成物"那个坑（见 `assets.rs` 里 `#render-errors` 那条断言）。卡 + 徽标是把这个区分**做成可见的**。 |
| 2 | **版本总览卡：每个被编排的上游一行，各挂 `待接入 / 已接入` + 一个"检查更新"** | 「概览」或「诊断」 | ZBox 编排 dae + oxidns + RouterOS，但**没有任何一屏回答"这三样各自找到了吗、什么版本、能不能跑"**。能力探测的结果现在散在 CLI 输出里。这是差距清单里最大的一条。 |
| 3 | **弹窗式"新增规则模块"的字段集**：名称 + 目标出站 + 匹配模式 + 规则集（一行一条，格式写在说明里） | 「分流策略与名单」/「规则集」 | 这正是 dae 的 `domain:` / `process:` + cluster 的形状，也是"UI 上改的 = 配置文件里那一段"最直接的对应。 |
| 4 | **校验失败一次列全**（一块红底把每条空字段列完，不是逐个弹） | 保存回执 | 我们现在的 400 只回第一条。对不懂程序的人，"还差三条"比"差第一条，改完再报第二条"少跑三个来回。 |
| 5 | **不支持的功能保留空卡并写明为什么不支持**（NAT 转换那张） | 「下发」/第 16 屏 | 与我们"没有对应上游的键一律直说不支持"同语言，但它**给了位置**——藏起来的开关会让人以为产品没这个能力，而不是这台机器现在不行。 |
| 6 | **图框在没数据时照画 + 中间一句"暂无…"** | 概览 | 低成本：我们的四格现在只有文字。 |
| 7 | 本机 CPU / 内存 / 磁盘占用一栏 | 「概览」 | 面板就跑在这台机器上，`/proc/meminfo` + `/proc/loadavg` 是 std 读得到的，且**用户能拿别的工具核对**。 |
| — | **不抄**：每卡一个保存 | — | 它每卡打到不同后端才需要各存各的；ZBox 改哪一屏存的都是同一份 `state.json`，全页一份保存是对的（这条取舍本来就在 `panel.html` 注释里）。 |
| — | **不抄**：把 `rpc error: …` / `manager unavailable: …` 原样上屏 | — | 用户读不懂，也核对不了。我们的"未读取 + 一句为什么"比它诚实。 |
| — | **不抄**：i18n 键漏到屏幕上（`common.emptyRuleModulesTitle`） | — | 这是它的真 bug，正好看板上的**空态文案要写死在页面里**这条纪律——文案要有编译期断言钉着（`assets.rs` 已有同类断言）。 |
| — | **不抄**：实时推送 / 暂停实时流 | 「日志」 | 零依赖 std HTTP 下不做 SSE/WebSocket；"按一下读一次 + 最近刷新时间"够用，那条元信息（**这份读数是几点取的**）倒是直接抄。 |

### 复跑

```sh
# guest 上（口要先看：ss -lnt | tail -n +2 | awk '{print $4}' | sort）
cd /root/abox-probe && nohup ./abox-ui > ui.log 2>&1 &      # 起完 kill <PID> 收
# 宿主：逐屏 1440x2600 拍全 + 文字清单 + 弹窗字段
node E:/ZBox/.tmp/abox-live/cdp-abox-tall.mjs
node E:/ZBox/.tmp/abox-live/cdp-abox-dialog.mjs
```

成品图：**`.tmp/abox-live/gallery.html`**（登录 + 六屏 + 四个弹窗，带一句一屏的说明）；
逐屏纯文本在 `.tmp/abox-live/text/`，结构化字段在 `facts/` 与 `dialog/`。
**注意**：正文在内部滚动容器里，`document.scrollHeight` 恒等于视口高，
所以 1440x900 的"整页截图"只有第一屏——要一次拍全得把视口拉高（`VH=2600`）。

