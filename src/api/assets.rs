//! 面板本体的字节，以及"路径 -> 字节"这张表。
//!
//! 三条决定了它长成这样的理由：
//! - **路径是逐字比对的常量**。§7.2 第一条硬约束在这一层的落点是：这里没有 `include_str!(拼接)`
//!   可写，也没有一条 `File::open`——三个字节块在编译期就钉死，运行时只查表。表里没有就是 404，
//!   于是 `//panel.css`、`/panel.JS` 这类写法连"被拼进路径"的机会都没有。查询串在协议层就剥掉了
//!   （`/?from=share` 曾让整个面板打不开，理由与顺序见 `src/api/http.rs` 的
//!   `a_query_string_is_dropped_but_still_checked`），剥之前先校验，所以 `?` 后面藏 schemes
//!   或反斜杠的照样 400。
//!   带 `..` 的写法协议层并不挡（那里只挡绝对形式、`*`、`://` 与反斜杠），它一路走到这里，
//!   然后在常量表上不中，于是掉进门里面的 API 路由——无令牌 401、带令牌 404，
//!   两种都不是"被拼进某个路径"。
//! - **静态面是整块面板里唯一不过令牌的门**。浏览器加载 `/` 时带不上自定义头，而这一页必须能先出来
//!   才能让人填令牌。代价就是它给的字节必须是编译期常量：里面一个现场读数、一条凭据、一个落点都不许有。
//!   下面那几个测试钉的就是这条边界，不是装饰。
//! - **没有构建步骤**。零依赖档里没有 npm，本地 registry 那 14 个 crate 里也没有 `rust-embed`
//!   或任何 HTTP 框架（实测：缓存目录里 grep `embed`/`http`/`axum`/`tokio` 零命中），所以
//!   HTML/CSS/JS 三份文件直接 `include_str!` 进二进制，升级即换字节；`Cache-Control: no-store`
//!   保证界面不会停在上一版。

pub struct Asset {
    pub content_type: &'static str,
    pub body: &'static str,
}

const ASSETS: &[(&str, Asset)] = &[
    (
        "/",
        Asset {
            content_type: "text/html; charset=utf-8",
            body: include_str!("../../assets/panel.html"),
        },
    ),
    (
        "/panel.css",
        Asset {
            content_type: "text/css; charset=utf-8",
            body: include_str!("../../assets/panel.css"),
        },
    ),
    (
        "/panel.js",
        Asset {
            content_type: "text/javascript; charset=utf-8",
            body: include_str!("../../assets/panel.js"),
        },
    ),
];

/// 逐字比对。不做规范化、不解 percent-encoding：表就是一切。
pub fn lookup(path: &str) -> Option<&'static Asset> {
    ASSETS
        .iter()
        .find(|(name, _)| *name == path)
        .map(|(_, asset)| asset)
}

/// 表里全部路径。测试用它把"只有这三条"钉住，免得以后有人往表里加一条带变量的。
pub fn paths() -> impl Iterator<Item = &'static str> {
    ASSETS.iter().map(|(name, _)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(path: &str) -> &'static str {
        lookup(path).expect("测试里这三条都在表上").body
    }

    /// 取 `assets/panel.js` 里某一个函数体：按函数头切一刀，到列 0 的 `}` 收尾
    /// （本仓库那份 JS 的函数体一律这样收尾，内部的括号都带缩进）。
    /// 断言"这一支只改了那两格"要的是函数级范围，整份 JS 里搜到不等于这个函数里有。
    fn body_of<'a, 'b: 'a>(src: &'a str, head: &'b str) -> &'a str {
        let rest = src.split(head).nth(1).unwrap_or("");
        match rest.find("\n}\n") {
            Some(at) => &rest[..at],
            None => rest,
        }
    }

    /// 连续 32 个小写十六进制字符——令牌的形状。页面里出现这个形状就等于有人把真令牌抄进了字节块。
    fn looks_like_a_token(text: &str) -> bool {
        let lowercase_hex = |b: &u8| b.is_ascii_hexdigit() && !b.is_ascii_uppercase();
        text.as_bytes()
            .windows(32)
            .any(|window| window.iter().all(&lowercase_hex))
    }

    #[test]
    fn the_table_holds_exactly_three_literal_paths() {
        let listed: Vec<&str> = paths().collect();
        assert_eq!(
            listed,
            vec!["/", "/panel.css", "/panel.js"],
            "静态面就这三条"
        );
        for path in &listed {
            assert!(
                path.starts_with('/') && !path.contains('?') && !path.contains(".."),
                "{path}"
            );
            assert!(lookup(path).is_some(), "{path} 应当查得回来");
        }
        // 差一个字节就不中：这一条是"没有路径参数"在静态面的形式
        for near in [
            "",
            "/index.html",
            "//panel.css",
            "/panel.css?v=1",
            "/panel.JS",
            "../assets/panel.html",
            "/assets/panel.html",
            "/%2fpanel.css",
        ] {
            assert!(lookup(near).is_none(), "{near:?} 竟查得到东西");
        }
    }

    #[test]
    fn nothing_the_page_ships_points_off_this_box() {
        // 一台旁路由上的面板不许在加载时抓 CDN：断了网也要能打开界面
        for path in ["/", "/panel.css", "/panel.js"] {
            let text = body(path);
            for forbidden in ["http://", "https://", "//unpkg", "cdn.", "googleapis"] {
                assert!(
                    !text.contains(forbidden),
                    "{path} 里出现了 {forbidden:?}：静态面不许引用站外的东西"
                );
            }
        }
    }

    #[test]
    fn the_assets_carry_no_state_from_the_machine() {
        // 这一条把"唯一不过令牌的门"的前提钉住：字节块里没有落点、没有凭据、没有令牌形状
        for path in ["/", "/panel.css", "/panel.js"] {
            let text = body(path);
            for forbidden in ["/var/lib", "/etc/"] {
                assert!(!text.contains(forbidden), "{path} 里出现了 {forbidden:?}");
            }
            assert!(
                !looks_like_a_token(text),
                "{path} 里有一段 32 位小写十六进制，像令牌"
            );
        }
    }

    /// 日志那一屏是"三份资产要同时改"的那类功能，而且从一来源改成**五来源一卡**之后，
    /// 会断的地方从三处变成六处：锚点、取的那条路、卡片的措辞来源、过滤框不许变成第二次读取、
    /// 有界窗口、退出时那句退回。少任何一份，现场表现都是"面板没反应"或"五张卡只画出一张"，
    /// 所以钉在编译期而不是等真机那一轮。
    #[test]
    fn the_log_view_is_wired_in_all_three_assets() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");
        for anchor in [
            "logs-read",
            "logs-cards",
            "logs-filter",
            "logs-out",
            "logs-meta",
        ] {
            assert!(
                html.contains(&format!("id=\"{anchor}\"")),
                "HTML 少了 #{anchor}"
            );
            assert!(js.contains(&format!("\"{anchor}\"")), "JS 没引用 #{anchor}");
        }
        // 只读：这一屏唯一的调用是 GET；POST 到 /api/logs 在路由表上是 405，界面上不该有这条路
        assert!(
            js.contains("call(\"GET\", \"/api/logs\")"),
            "JS 没去取 /api/logs，那一屏五张卡就是一块死字"
        );
        assert!(
            js.contains("value.sources"),
            "JS 读的还是旧的那一份单来源形状（`value.lines`），五张卡画不出来"
        );
        // 卡片标题与那句解释只有一个来源：服务端。HTML 里抄一份就是同一台机器上的第二处真相，
        // 而改后端措辞的人不会想到回来改 HTML。
        for wording in ["数据面自己写的那一份", "监督者替数据面接住的出错", "接管层自己写的那一份"] {
            assert!(
                !html.contains(wording) && !js.contains(wording),
                "两份资产里抄了一句卡片措辞：{wording}"
            );
        }
        // 有界窗口：五张卡各自一个，而不是整屏一条 64 KiB 的长条
        assert!(
            css.contains(".log-src-body"),
            "五份正文叠成一列会把这一屏要人先看的那一格挤出首屏"
        );
        // 过滤是**纯前端**的重画：它只动已经读到的那几行。这条界靠"读一次"只有三处调用钉着——
        // 定义一处、按下读取一处、下发/同步/回滚之后各一处，多出来的每一次都该被看见。
        assert!(
            js.contains("readLogs") && js.contains("renderLogs"),
            "读取与重画必须是两个函数，否则过滤框只能靠再问机器一次来生效"
        );
        assert!(
            !js.contains("EventSource") && !js.contains("setInterval"),
            "这一屏是\"按一下读一次\"：SSE 或定时器会把首屏那几次读取挤到后面去"
        );
        // 级别下拉换成子串过滤（#98 定案）：两个上游的日志级别形状不统一，
        // 画一个下拉框就等于承诺"服务端会按级别滤"，而它不会。
        assert!(
            !html.contains("id=\"logs-level\""),
            "级别下拉不许写回来——它要的那一条判据后端没有"
        );
        // 「读一次」只有三个时刻：开局、下发/同步之后、人按下那一下。加上定义本身四处。
        // 过滤框每多接一次读取就会变成第四次调用点，而那时它就是"每次打字问机器一次"的轮询。
        assert_eq!(
            js.matches("readLogs(").count(),
            4,
            "readLogs 应当是「一个定义 + 三个读取时刻」，现在是 {}\
             多一处就是有人在把这一屏改成轮询，少一处是某个时刻不再刷新日志",
            js.matches("readLogs(").count()
        );
        // 开局那个过滤框是禁用的（HTML 写死），退出登录也必须退回禁用 + 空词。
        // 这一条是真机那一轮抓出来的：卡清掉了、框还能打字，屏上就同时写着"未读取"
        // 和"只画带某几个字的行"——后者已经没有对象。
        let input_at = html
            .find("id=\"logs-filter\"")
            .expect("HTML 里没有 #logs-filter");
        let input_tag = &html[input_at..html[input_at..].find('>').unwrap() + input_at + 1];
        assert!(
            input_tag.contains("disabled"),
            "HTML 里那枚 #logs-filter 开局就该是禁用的：JS 那一份开局态要有对得了的参照"
        );
        let out_at = js
            .find("document.getElementById(\"logout\").addEventListener")
            .expect("退出登录那一段不在了");
        let block = &js[out_at..out_at + js[out_at..].find("\n});").expect("退出那一段没有收尾") + 4];
        assert!(
            block.contains("lastLogs = null")
                && block.contains("renderLogs()")
                && block.contains("document.getElementById(\"logs-filter\").value = \"\";")
                && block.contains("setLogsEnabled(false)"),
            "退出登录没把日志那一屏整屏退回开局：手里那份、五张卡、过滤词、框的可用性是一件事，\
             只做前三样会留下一个还能打字、却没有对象的框"
        );
    }

    /// 「DNS 记录」这一格是同一类三份资产功能，而且比日志那一格多三处会断的地方：
    /// 它是**两层**（列表点一行才去问第二条）、有四个过滤框、还有一张上游枚举的下拉框。
    /// 少任何一份，现场表现都是"点了没反应"或"给的词服务端不认"，所以钉在编译期。
    #[test]
    fn the_dns_records_view_is_wired_in_all_three_assets() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");
        for anchor in [
            "dnsrec-badge",
            "dnsrec-read",
            "dnsrec-more",
            "dnsrec-meta",
            "dnsrec-apply",
            "dnsrec-version",
            "dnsrec-errors",
            "dnsrec-qname",
            "dnsrec-client",
            "dnsrec-status",
            "dnsrec-limit",
            "dnsrec-page",
            "dnsrec-list",
            "dnsrec-empty",
            "dnsrec-detail-meta",
            "dnsrec-detail",
            "dnsrec-steps",
        ] {
            assert!(
                html.contains(&format!("id=\"{anchor}\"")),
                "HTML 少了 #{anchor}"
            );
            assert!(js.contains(&format!("\"{anchor}\"")), "JS 没引用 #{anchor}");
        }
        // 两层：列表与单条是两条路，缺一条就只剩半屏（点行没内容 / 列表出不来）
        for call in [
            "call(\"POST\", \"/api/dns-records\", JSON.stringify(fields))",
            "call(\"POST\", \"/api/dns-record\", JSON.stringify({ id: id }))",
        ] {
            assert!(js.contains(call), "JS 少了这一问：{call}");
        }
        // 下拉框的选项表只许有**一份**：HTML 里那一格是空的，词由 JS 从 DNS_STATUS 灌进去。
        // 写进 HTML 就会漂成"面板给的四词与服务端认的四词不是一回事"，而那要 400 才看得见。
        assert!(
            html.contains("<select id=\"dnsrec-status\"></select>"),
            "HTML 里给 dnsrec-status 写死了选项，词表就成了两份"
        );
        assert!(
            js.contains("DNS_STATUS.forEach"),
            "JS 没把枚举灌进下拉框，那一格永远只有空值"
        );
        // 列表是每次重建的，所以点行靠委托；给每行各挂监听会随翻页越挂越多
        assert!(
            js.contains("closest(\"li[data-id]\")"),
            "列表没有事件委托，点一行不会去问那一条的 steps"
        );
        for selector in ["#dnsrec-list", "#dnsrec-steps", ".rec-row", ".step-row"] {
            assert!(
                css.contains(selector),
                "{selector} 没有样式：一屏记录会把整页撑长，两层也分不出行"
            );
        }
        // 空列表那一句的四种来路：管理口没读到 / 筛完是空 / 期望态里开关是关着的 / 真的一条没有。
        // 少了"筛完是空"这一支，带着过滤条件读到的空页会被说成"没在采集"。
        for branch in [
            "解析器的管理口没读到",
            "筛完是空的",
            "说的是期望态",
            "一条记录也没有",
        ] {
            assert!(js.contains(branch), "JS 的空列表分支少了那一句：{branch}");
        }
        // 「记下每一次查询」那一格说的是**期望态**，不是跑着的那份：说明句必须带"下发之后"
        // 这个限定，否则没下发时读到旧记录，同一屏就同时写着"永远是空的"和一堆记录。
        assert!(
            html.contains("下发之后"),
            "HTML 的承诺没带\"下发之后\"这个限定"
        );
        // 不一致也有两种来路（刚改过没下发 / 这台机器上的解析器根本不是 ZBox 装的），
        // 只写前一种会对后一种机器指控用户"你改了没下发"。
        assert!(
            js.contains("不是 ZBox 装的"),
            "JS 的\"不一致\"那句只说了一种来路"
        );
        // 退出退回的那几句要和 HTML 开局逐字相同——这里读的是**同一句**，两边不能各写一遍
        for sentence in [
            "未读取。按一下上面「读取 DNS 记录」。",
            "点上面任意一行来看这一条查询走过的每一步，以及它最后是怎么收场的。",
            "还没点某一行",
        ] {
            assert!(html.contains(sentence), "HTML 的开局句不是这句：{sentence}");
            assert!(
                js.contains(sentence),
                "退出的退回句与 HTML 漂了：{sentence}"
            );
        }
    }

    /// 生成预览那一格同样是三份资产要同时改的功能：HTML 给锚点、JS 给取数与标行、CSS 给
    /// 有界窗口与底色。这一格是"UI 上改了 = 配置里那一行跟着变"那条判据的可见形态，
    /// 少了哪一份都只长成"看着没反应"，所以钉在编译期。
    ///
    /// **四格**：编排层那两份（dae / oxidns）之外还要有内核层那两份（`zbox.nft` / `netstate.sh`）。
    /// IPv6 接管姿态那一档动的正是后两份，缺了它们，这一屏会在用户换档之后说"逐字没变"。
    #[test]
    fn the_preview_view_is_wired_in_all_three_assets() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");
        for anchor in [
            "preview-read",
            "preview-meta",
            "preview-dae",
            "preview-oxidns",
            "preview-nft",
            "preview-netstate",
        ] {
            assert!(
                html.contains(&format!("id=\"{anchor}\"")),
                "HTML 少了 #{anchor}"
            );
            assert!(js.contains(&format!("\"{anchor}\"")), "JS 没引用 #{anchor}");
        }
        // 只读：唯一的调用是 GET。预览不许有第二条路（落盘与下发都不在这一屏）
        assert!(
            js.contains("call(\"GET\", \"/api/preview\")"),
            "JS 没去取 /api/preview，那四个 <pre> 就是死字"
        );
        // 服务端回执里的目标名与面板的格子一一对应：改了名字等于面板上少了内容
        for name in [
            "dae: \"preview-dae\"",
            "oxidns: \"preview-oxidns\"",
            "\"zbox.nft\": \"preview-nft\"",
            "\"netstate.sh\": \"preview-netstate\"",
        ] {
            assert!(js.contains(name), "JS 的格子表少了 {name}");
        }
        for selector in ["#preview-dae", "#preview-nft", "#preview-netstate"] {
            assert!(
                css.contains(selector),
                "{selector} 没有有界窗口，整份配置会把整页撑长"
            );
        }
        assert!(css.contains(".diff"), "差异那一段的底色没定义");
        // 新增的两格（流量路线 / 下发节奏）：锚点在 HTML、读写都在 JS。缺一格就是
        // "看着能选，其实那一下没带到磁盘上"——正是这一屏要防的那类假象
        for anchor in ["net-route-mode", "apply-policy"] {
            assert!(
                html.contains(&format!("id=\"{anchor}\"")),
                "HTML 少了 #{anchor}"
            );
            assert!(js.contains(&format!("\"{anchor}\"")), "JS 没引用 #{anchor}");
        }
        // 保存那条路的基准：写盘之前必须先取一份生成物。整页重载后的第一次保存里，上一份
        // 读到的还不存在，那时"保存后自动重读"那一次会被当成本页第一次读，一行底色都标不出来
        // ——这一屏正好在它该说话的时候闭了嘴，而这条判据是这一屏存在的全部理由。
        let baseline_reads = js.matches("await readPreview();").count();
        assert!(
            baseline_reads >= 2,
            "保存路径少了写盘前那一次基准读取（只找到 {baseline_reads} 处调用，要写盘前后各一次）"
        );
        // HTML 那句说明要跟着真实行为走：教人"保存过一次再按读取"会把刚标出来的底色刷掉，
        // 而它下一句就是"没有底色是应该的"——照着做的人只会以为自己的改动没落进配置。
        assert!(
            html.contains("不用再按"),
            "HTML 的说明还在教人保存之后再按一次读取"
        );
        // 退出要把差异基准一起清掉：留着它，"未读取"和"与上一页读到的那份相比"会同屏矛盾。
        assert!(
            js.contains("previewPrevious = {};"),
            "退出的名单没清差异基准"
        );
        // 缩小的那一半换档（双栈→不接管）删掉的行在这一份里不存在、画不出底色，只能在句子里说。
        // 这一条分支要是漂了，变得最多的那一次换档又会长成"一行都没变"。
        assert!(
            js.contains("标不出底色"),
            "JS 少了\"删掉的行标不出底色\"那一句"
        );
        assert!(
            html.contains("画不出底色"),
            "HTML 的说明没讲删掉的行为什么标不出底色"
        );
        // 三档那句说明不能再承诺"双栈接管再加 dae 的 tproxy 口"：`gen::dae` 里根本没有这一档的
        // 分支，预览因此说"逐字相同"——承诺没生成的东西，等于把一次正确的显示说成缺陷。
        // 2026-10-11 实测补上两层：dae 自己绑了 v6 口但在它的 netns 里（宿主探针查不到，
        // 所以这条不许变成故障判据），而 divert 链未命中时是**静默泄漏**不是黑洞
        // （docs/reference/ipv6-kernel-measured.md §8、§10），面板要敢照着说。
        assert!(
            html.contains("dae 的 tproxy 口不进这份生成物的改动"),
            "HTML 还在承诺 dae 的 v6 tproxy 口，而生成物里没有它"
        );
        assert!(
            html.contains("包会按普通路由直接出去"),
            "HTML 没写清 divert 链未命中的后果，用户会把\"劫持层已加载\"当成防泄漏成功"
        );
        // 页题那一句也要跟着四格：副标题还写"两份"时，同一屏上明明摆着四格——2026-10-11 宽屏
        // 那一张截图里就是这样的，所以这条钉在编译期而不是等人眼再撞一次。
        assert!(
            js.contains("渲染成四份配置文本"),
            "PAGES 表里预览的副标题没跟上四格"
        );
    }

    /// 「分流情况」是同一类"三份资产要同时改"的功能，并多两条只有这一屏才有的判据：
    /// ① 两列序号来自**两份**生成物，跨层没有统一顺序，所以"这一层不落行"要显式写出来——
    /// 空着会被读成"没读到"；② 这张表读的是期望态，机器上一条都没问，所以"还没下发"那一句
    /// 必须留在屏上，否则期望会被读成现场。少任何一份资产，现场表现都只长成"这一屏没入口"。
    #[test]
    fn the_ladder_view_is_wired_in_all_three_assets() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");
        for anchor in [
            "ladder-badge",
            "ladder-read",
            "ladder-meta",
            "ladder-pending",
            "ladder-errors",
            "ladder-rows",
            "ladder-empty",
            "ladder-diff",
        ] {
            assert!(
                html.contains(&format!("id=\"{anchor}\"")),
                "HTML 少了 #{anchor}"
            );
            assert!(js.contains(&format!("\"{anchor}\"")), "JS 没引用 #{anchor}");
        }
        // 只读：唯一的调用是 GET。这一屏不该有第二条路（落盘与下发都不在它这一层）
        assert!(
            js.contains("call(\"GET\", \"/api/ladder\")"),
            "JS 没去取 /api/ladder，那张表就是块死字"
        );
        // 服务端回执里那九个字段，JS 少读一个就少画一列，而"少画一列"在现场读起来像"这一格没内容"
        for field in [
            "tier",
            "when",
            "then",
            "from",
            "dns",
            "connection",
            "connection_lines",
            "why",
            "shadowed_by",
        ] {
            assert!(js.contains(field), "JS 没读回执里的 {field}");
        }
        // 六格分段名一份不少（认不出的词由 JS 原样画出来，不静默变空白）
        for tier in ["fixed", "device", "global", "domain", "fallback", "note"] {
            assert!(
                js.contains(&format!("{tier}: \"")),
                "LADDER_TIERS 少了 {tier} 那一段"
            );
        }
        for selector in [
            "#ladder-rows",
            ".ladder-row",
            ".ladder-why",
            ".ladder-row--gone",
        ] {
            assert!(css.contains(selector), "CSS 里没有 {selector}");
        }
        // 「不落行」要说出口，不许留空：留空与"这一层没有这一行"是两句不同的话
        assert!(js.contains("不落行"), "JS 把\"这一层不落行\"画成了空格");
        // 屏头那句必须与 HTML 逐字同一句：这一屏读的是期望态，不是现场读数
        for sentence in ["读的是面板的期望态（盘上那份）"] {
            assert!(html.contains(sentence), "HTML 的屏头句不是这句：{sentence}");
            assert!(js.contains(sentence), "JS 的读数句与 HTML 漂了：{sentence}");
        }
        // broken 那一档不能沿用上面那句：盘上那份读不进来时，"读的是面板的期望态"与
        // 下面那句"这张表是空的"同屏就是自相矛盾。
        assert!(
            js.contains("盘上那份状态读不进来，所以这一屏没有读数"),
            "JS 的 broken 档没换屏头那句"
        );
        // 服务端那句原因自己以句号收尾，直接拼上去会得到"。。"
        assert!(
            js.contains(".replace(/。$/, \"\")"),
            "JS 拼 render_errors 那句时没去尾巴上的句号"
        );
        // "还没下发"在三种状态下是三句话（要下发 / 一致 / 谈不上），少了第三种会把
        // missing 那一档画成"一致"，新装的机器一打开就被告知一切都已生效。
        // 而 pending=true 这一档自己还要再分两种（与已知良好那份不一致 / 这台机器一次都没
        // 成功下发过），回执因此要带 `last_good`：只给布尔值就会对新机器说"你上次下发的内容
        // 跟现在这份不一致"——它一次都没下发过，那句是指控。
        for branch in [
            "这张表还没下发",
            "与已经下发的那份内容一致",
            "谈不上",
            "一次都没成功下发过",
            "已知良好的内容（修订 ",
            "value.last_good",
        ] {
            assert!(js.contains(branch), "JS 的下发那一句少了分支：{branch}");
        }
        // 空表的四种来路，各是一句不同的话（读不进来 / 回执形状变了 / 真的一行没有 / 还没读）
        for branch in [
            "磁盘上那份状态读不进来，所以这张表是空的",
            "服务端这次没给这张表",
            "这张表一行都没有",
            "未读取。按一下上面「读取分流梯级」。",
        ] {
            assert!(js.contains(branch), "JS 的空表分支少了那一句：{branch}");
        }
        // 开局那一句两边逐字相同，否则退出会把界面停进第三种状态
        assert!(
            html.contains("未读取。按一下上面「读取分流梯级」。"),
            "HTML 的开局句不是 JS 退回的那句"
        );
        // 保存路径的基准与预览同一条纪律：写盘前先读一次，否则整页重载后的第一次保存
        // 会被当成"本页第一次读"，用户正好看不见他最该看的那件事（刚才那一下动了哪几行）。
        assert!(
            js.matches("await readLadder();").count() >= 2,
            "保存路径少了写盘前那一次基准读取"
        );
        // 基准本身也是一次读数，退出时跟着退场（留着它会同屏写着"未读取"和"与上一次相比"）
        assert!(
            js.contains("ladderPrevious = null;"),
            "退出没清梯级的差异基准"
        );
        // 梯级摘要那一行：HTML 有锚点、JS 往里面填字、函数名能对上
        assert!(
            html.contains("ladder-summary"),
            "HTML 少了梯级摘要的锚点"
        );
        assert!(
            js.contains("ladderSummary"),
            "JS 少了梯级摘要函数"
        );
        // ladderKey 必须把 `applied` 算进差异比对——P4 填真值后两次读之间 applied 从 null 变 true
        // 若不进 key，差异高亮不会亮，用户以为"什么都没变"。钉死 join 前那个位置，漏掉就红。
        assert!(
            js.contains("row.applied].join("),
            "ladderKey 没把 row.applied 算进差异比对：P4 之后逐行已生效的变化会静默不标底色"
        );
        // 删掉的行在这一张里不存在、画不出底色，只能在句子里说明——预览那屏是被实测教出来的，
        // 同一件事在这一屏同样是"变得最多的一次看上去像一行都没变"。
        assert!(
            js.contains("标不出底色"),
            "JS 少了\"删掉的行标不出底色\"那一句"
        );
        // 导航项与页题这一屏都要在场（外壳那条测试只管 data-view 对得上，这一条钉的是名字与档位：
        // 「分流情况」属高级档，收起导航时那个记号必须是屏名的第一个字，否则轨上认不出点了会去哪）
        assert!(
            html.contains(
                "data-view=\"ladder\" data-tier=\"advanced\"><span class=\"mark\">分</span><span class=\"label\">分流情况</span>"
            ),
            "HTML 的导航项不是「分 · 分流情况」（高级档）"
        );
        assert!(
            js.contains("ladder: [\"分流情况\""),
            "PAGES 表里这一屏的页题不是「分流情况」"
        );
        // 整表 vs 逐行的那条界限要写在屏上：修订里没有这两份编排文本，逐行"已生效"给不出
        assert!(
            html.contains("逐行没有可比的落点"),
            "HTML 还在让人以为这张表能逐行对照已下发的那份"
        );
    }

    /// 看板那一屏。除了"三份资产都接线"，这一条钉的全是**它凭什么在屏幕上这么说**：
    /// 这一屏卖的是"每个数字都说得出取数口"，所以它的失效方式一律是静默的——
    /// 少一条 CSS 选择器只是那根比例条不见了，少一个 `-sample` 锚点只是那句"最近 N 条"不见了，
    /// 而"不见了"在用户眼里等于"这台机器就是这样"。编译期拦。
    #[test]
    fn the_board_view_is_wired_in_all_three_assets() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");

        // 硬件那一格 + 解析层那四格：锚点两边都要在
        for anchor in [
            "board-badge",
            "board-read",
            "board-meta",
            "board-taken",
            "board-mem",
            "board-load",
            "board-uptime",
            "board-lan-iface",
            "board-ifaces",
            "board-ifaces-note",
            "board-disk",
            "board-dns-meta",
            "board-dns-note",
            "board-qnames",
            "board-qnames-sample",
            "board-clients",
            "board-clients-sample",
            "board-rcodes",
            "board-rcodes-sample",
            "board-plugins",
            "board-plugins-sample",
            "board-engine-oxidns",
            "board-engine-dae",
            "board-engine-router",
            "board-engine-note",
        ] {
            assert!(
                html.contains(&format!("id=\"{anchor}\"")),
                "HTML 少了 #{anchor}"
            );
            assert!(js.contains(&format!("\"{anchor}\"")), "JS 没引用 #{anchor}");
        }

        // 只读，而且只有一条路。这一屏不发命令、不写盘，所以它唯一该做的是取那一份读数
        assert!(
            js.contains("call(\"GET\", \"/api/board\")"),
            "JS 没去取 /api/board，这一屏就是块死字"
        );
        assert_eq!(
            js.matches("\"/api/board\"").count(),
            1,
            "/api/board 在 JS 里出现不止一次：这一屏只该有一条取数路"
        );
        assert!(
            !js.contains("POST\", \"/api/board"),
            "看板往 /api/board 发了写请求：这一屏的约定是不动机器上的任何东西"
        );
        // 没连上也该能按：这一屏要的只是"你在这台机器上登录过"，不是"磁盘那份读到了"
        assert!(
            js.contains("\"dnsrec-read\", \"board-read\""),
            "board-read 没进那个按令牌启用/停用的名单"
        );
        assert!(
            js.contains("document.getElementById(\"board-read\").addEventListener(\"click\""),
            "HTML 有 #board-read 而 JS 没给它注册点击：那颗按钮是块装饰"
        );

        // 四格每格带自己的样本数。实测同一次刷新里 top_qnames 给 13、另外三份给 15，
        // 所以卡头不许写一个共用的数，格子也不许把几格的 count 相加当总量。
        assert_eq!(
            html.matches("-sample\"").count(),
            4,
            "看板那四格的样本数锚点不是四个"
        );
        assert!(
            js.contains("这一份是样本，不是这台机器的总量"),
            "样本数那句被改成了总量的口径"
        );
        // 空表那一档只认 sample_size：全新 DB 上上游会回 `sample_size: 0` 外加一行 `_UNKNOWN`
        // `count: 1`（实测两次复现），照着 rows 画就当着用户说"这台机器被打回过 1 次"
        assert!(
            js.contains("size === 0 || rows.length === 0"),
            "判空不再只看 sample_size 了"
        );
        assert!(
            js.contains("最近没有查询记录。"),
            "JS 少了看板空表那一句"
        );
        // 同一格顶上那句"前 N 名"也只在**真的画了行**时才说：空库上上游给的是
        // `sample_size: 0` 外加那一行 `_UNKNOWN`，于是屏幕上会同时出现"前 1 名"和"最近没有查询记录"
        assert!(
            js.contains("size === 0 || named === null"),
            "空表那一格还会说「前 N 名」：它与下面那句「最近没有查询记录」当场打脸"
        );

        // 记录开关那一格：`records_enabled` 是**盘上那份期望态**，不是四格的现场读数，
        // 所以那句收尾必须由四格实际画出来的形状来分档（都空 / 有行 / 有格没读到）。
        // 上一版拿 `records_enabled === false` 直接推"上面四格是空的"，真机上（期望态关着、
        // 跑着的那份还在记）画出来的是一屏自相矛盾：四格有行，同一张卡最后一句说它们是空的。
        // 这一支的三种形状各自在屏幕上说什么，是本屏最容易被改回去又最难看出来的一处，钉住。
        assert!(
            js.contains("盘上这份和跑着的那份还没对上"),
            "JS 少了「期望态关着而四格读到了行」那一支——它正是上面那个矛盾"
        );
        assert!(
            js.contains("既不能算在记、也不能算没在记"),
            "JS 把「有格没读到」并进了「都是空的」：没读到不能替期望态作证"
        );
        assert!(
            js.contains("「空」不等于这台机器没人查询过"),
            "JS 丢了空表那一句的保留——开着而恰好没人查询过也是同一个形状"
        );
        // 判空的口径只许有一处来源：四格的形状由 `paintBoardCell` 交回来，句子在 `readBoard` 分档
        assert_eq!(
            js.matches("return \"live\";").count(),
            2,
            "看板格子交回「有行」的出口不是两处（plugins 与 rows 各一处）"
        );
        assert!(
            js.contains("BOARD_CELLS.map(function (cell)"),
            "四格的形状不再被收集，那句收尾就退回去只看 records_enabled 了"
        );
        // `**…**` 这种 markdown 强调在 textContent 里会**字面出现在屏幕上**：面板没有 markdown 渲染器。
        // 注释里满屏都是星号强调，所以只看非注释行。
        let stray: Vec<&str> = js
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains("**"))
            .collect();
        assert!(
            stray.is_empty(),
            "JS 的非注释行里混进了 markdown 星号，它会原样出现在那一格上：{stray:?}"
        );

        // 回执里那些字段，少读一个就少画一格
        for field in [
            "taken_at_epoch_ms",
            "lan_interface",
            "interfaces",
            "used_percent",
            "rx_text",
            "tx_text",
            "sample_size",
            "query_total",
            "records_enabled",
            "errors",
            "build_bundle",
            "running_version",
            "generate_error",
        ] {
            assert!(js.contains(field), "JS 没读回执里的 {field}");
        }

        // 「几点取的」只能由后端盖：这句一旦来自浏览器自己的钟，两台机器钟不同时就成了面板说谎
        assert!(
            !js.contains("Date.now()"),
            "JS 用了浏览器自己的钟：这一屏的时刻应该出自 taken_at_epoch_ms"
        );
        // 磁盘那一格的句子单一定义在后端（`crate::svc::board::DISK_NOT_READ`），
        // 面板只是把它搬进那一格；这里再抄一遍就是两处两套措辞
        assert!(
            js.contains("disk.reason"),
            "JS 没读后端给的那句「为什么没读」"
        );
        assert!(
            !js.contains(crate::svc::board::DISK_NOT_READ),
            "JS 把磁盘那句抄了一份：措辞只该住在后端一处"
        );
        // 单位换算只在后端一处；浏览器再除一次 1024 就是"面板说的与机器上的对不上"
        assert!(
            !js.contains("/ 1024") && js.contains(" 字节\""),
            "JS 里出现了第二套字节换算，或是丢了那句退回人话"
        );
        // 网卡那几行的界：数的是过了多少字节，不是代理了多少
        assert!(
            js.contains("不是「代理了多少」"),
            "JS 少了网卡行数的那句界"
        );

        // §11.3 G 那一卡（三台引擎在不在）。三行里只有 oxidns 有可查的口，另两行的"为什么读不到"
        // 同样只许住在后端一处（`crate::svc::board`）——面板抄一份，将来改一句就成两处两套话。
        assert!(
            html.contains("<h2>三台引擎在不在</h2>"),
            "HTML 少了那一卡，或者它改名了（面板上要说得出「三行各是谁」）"
        );
        assert!(
            html.contains("<dt>域名解析（oxidns）</dt>")
                && html.contains("<dt>分流（dae）</dt>")
                && html.contains("<dt>主路由</dt>"),
            "那一卡的三行不是「域名解析 / 分流 / 主路由」"
        );
        assert!(
            js.contains("engines.oxidns") && js.contains("value.engines"),
            "JS 没读 engines 那一份"
        );
        assert!(
            js.contains("engines.dae") && js.contains("engines.router"),
            "JS 少读了读不到的那两行：它们因此会一直写着「未读取」，那一句为什么永远上不了屏"
        );
        for phrase in [
            crate::svc::board::DAE_NOT_READ,
            crate::svc::board::ROUTER_NOT_READ,
        ] {
            assert!(
                !js.contains(phrase),
                "JS 把后端那句「为什么读不到」抄了一份：措辞只该住在后端一处"
            );
        }
        // `applied` 是三态不是布尔：对上了 / 没对上 / 有一头没数。最后一头**不许**画成"没对上"——
        // "跑的那份读不到"与"跑的不是这一份"是两句话，所以后端给的是 null。
        assert!(
            js.contains("ox.applied === true") && js.contains("ox.applied === false"),
            "那一格不再分三态了，null 会被并成一句结论"
        );
        // 那句纪律留在屏幕上，不只留在注释里：这一格只报数，放行是另两屏的事
        assert!(
            js.contains("只用来报，不用来放行"),
            "JS 少了「版本这一格不判放行」那一句"
        );
        assert!(
            js.contains("不是那台机器上没在跑"),
            "JS 少了那一句界：读不到 ≠ 没在跑"
        );

        // 比例条的宽度只能由 share 给，而且读不出时那一根**根本不该存在**：
        // 一根 0 长的条说的是"占比 0"，与"这一格没读到"是两句不同的话。
        assert_eq!(
            js.matches("board-fill").count(),
            1,
            "board-fill 在 JS 里建了不止一处：第二处多半是给不出的那一格补了个默认宽度"
        );
        assert!(
            js.contains("Math.max(0, Math.min(1, n))"),
            "条的宽度没按占比夹在 0–100% 之间"
        );
        // 走到链上哪一步那一格：条画的是"多少比例的查询经过这一步"，所以那一行的头条必须是
        // 同一个口径的数。上一版拿 matcher 的 `matched` 当头条，真机画出来是
        // 「匹配 · rule1 ｜ 0 次 · 66.7%」——一行里两个数互相打脸。这句界要留在屏幕上。
        assert!(
            js.contains("那根条画的就是「经过这一步的查询占多少」"),
            "链上那一格没在屏幕上说清那根条画的是什么"
        );
        for selector in [
            "ul.board-rows",
            ".board-row",
            ".board-key",
            ".board-num",
            ".board-bar",
            ".board-fill",
        ] {
            assert!(css.contains(selector), "CSS 里没有 {selector}");
        }
        // 那一卡的三行走全局 `dl/dt/dd`（与硬件那几行同一套），不新开选择器；钉一下这三条在，
        // 否则删掉一条的后果是"名字和那一长句读不到挤成一行"，而那种事在预览里最容易看漏。
        for selector in ["dl {", "dt {", "dd {"] {
            assert!(
                css.contains(selector),
                "CSS 里没有 {selector}：那一卡的三行没样式了"
            );
        }

        // 这两句是这一屏的边界，必须留在屏幕上而不是只留在注释里：
        // 连接层给不出"哪一条走了代理"，而面板不许假装知道
        assert!(
            html.contains("这一屏给不出「哪一条连接走了代理」"),
            "HTML 少了连接层那句边界；只剩注释里的话，用户看不见这一格为什么是空的"
        );
        assert!(
            !js.contains("走了代理"),
            "JS 里出现了「走了代理」：这一屏不许宣称知道哪一条走了代理"
        );

        // 导航项与页题：看板属**普通档**（它是"现在怎么样"那一层，不是"改规则本身"）
        assert!(
            html.contains(
                "data-view=\"board\" data-tier=\"basic\"><span class=\"mark\">看</span><span class=\"label\">看板</span>"
            ),
            "HTML 的导航项不是「看 · 看板」（普通档）"
        );
        assert!(js.contains("board: [\"看板\""), "PAGES 表里这一屏的页题不是「看板」");
    }

    /// 「某台设备单独走」那一屏的形状（§11.3 C）：一栏就是一档，卡片**拖进栏 = 改 `mode`**，
    /// 而这一屏没有序号、也没有上下移动。三份资产各管一头：HTML 给四栏与那句界，
    /// JS 给"拖与不拖落到同一处"，CSS 给栏的形状。少一份的现场是"这块面板没反应"或
    /// "卡在跟着默认走那一栏、那一格却写着强制走代理"，所以钉在编译期。
    #[test]
    fn the_device_lanes_are_wired_in_all_three_assets() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");

        // 三栏路由：直连 / 智能分流 / 全局
        assert!(
            html.contains("class=\"routing-lanes\""),
            "HTML 没有三栏路由容器"
        );
        for mode in ["direct", "smart", "global"] {
            assert!(
                html.contains(&format!("id=\"lane-{mode}\"")),
                "HTML 少了「{mode}」这一栏"
            );
            assert!(js.contains(&format!("\"{mode}\"")), "JS 那三档名单里没有 {mode}");
        }

        // 栏题
        for head in ["直连", "智能分流", "全局"] {
            assert!(html.contains(&format!("<h3>{head}</h3>")), "HTML 的栏题少了「{head}」");
        }

        // 设备卡片样式
        assert!(css.contains(".routing-card"), "CSS 没有设备卡片样式");
        assert!(css.contains(".routing-lanes"), "CSS 没有三栏布局样式");
    }

    /// 「默认怎么走」那一屏顶上的三档开关（§11.8 那条界的落地）：用户口述的
    /// 不科学 / 智能分流 / 全局 是**一颗按钮改背后那两格**，不是模型里的第三个字段。
    /// 钉三件事：① 那一排排在四格勾选之前（先看见的是选一档，不是一堆框）；
    /// ② 它写的仍然只有 `cn_direct` 与 `final_outbound` 那两格，**没有往 PUT 里塞新键**；
    /// ③ 两个不说谎的分支——组合落不到任何一档上要显示"自定义"而不是硬选一档，
    ///    要交给组而这一屏一个组都没有时要出声（底色换成警告色）而不是按下去没反应。
    #[test]
    fn the_three_way_default_switch_writes_only_the_two_cells() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");

        assert!(
            html.contains("id=\"mode-switch\" class=\"mode-switch\""),
            "HTML 没有那一排按钮的容器"
        );
        for mode in ["direct", "smart", "global"] {
            assert!(
                html.contains(&format!("class=\"mode-opt\" data-mode=\"{mode}\"")),
                "那一排少了「{mode}」这一档"
            );
            assert!(js.contains(&format!("  {mode}: {{")), "JS 的档位表少了 {mode} 那一行");
        }
        for head in ["不科学", "智能分流", "全局"] {
            assert!(
                html.contains(&format!(">{head}<span>")),
                "那一排的按钮上少了「{head}」"
            );
        }
        assert!(
            html.contains("id=\"mode-switch-note\""),
            "HTML 没有那句\"这一档背后是哪两格\"的落点"
        );
        // 三个按钮的点击是接在同一个选择器上的（栏那份清单由 HTML 给，脚本不抄档名）
        assert!(
            js.matches("#mode-switch .mode-opt").count() >= 2,
            "JS 里没有\"给那一排按钮接点击\"的那一处"
        );

        // 排在四格之前：先给"挑一档"，再给"单独拧"。
        let switch_at = html
            .find("id=\"mode-switch\"")
            .expect("那一排在页面上");
        let cells_at = html
            .find("id=\"pol-private-direct\"")
            .expect("四格在页面上");
        assert!(
            switch_at < cells_at,
            "三档开关排到了四格后面——第一眼看见的还是一堆框"
        );
        for sentence in [
            "先点下面这一排就够了",
            "私网直连、广告拦截、BT 直连那三格各管各的",
        ] {
            assert!(html.contains(sentence), "HTML 少了那一句界：{sentence}");
        }
        assert!(js.contains("先点那一排"), "屏题副标题没跟上这一排");

        // 它只写那两格。往 PUT 里塞一个新键就是造第三个状态，生成物没人读它。
        let choose = body_of(&js, "function chooseModeSwitch(mode) {");
        assert!(
            choose.contains("pol-cn-direct") && choose.contains("pol-final"),
            "那一排没写回那两格，那它改的是什么"
        );
        assert!(
            !choose.contains("pol-private-direct")
                && !choose.contains("pol-block-ads")
                && !choose.contains("pol-bittorrent"),
            "那一排动了它不该动的格子"
        );
        for invented in ["policy.mode", "default_mode", "modeSwitch:"] {
            assert!(
                !js.contains(invented),
                "JS 往状态里造了第三个字段 {invented}——那一排只是那两格的快捷写法"
            );
        }

        // 读数由那两格算出来，不是由"上一次按了哪个"记着：否则人直接改框之后按钮会骗人。
        let paint = body_of(&js, "function paintModeSwitch() {");
        assert!(
            paint.contains("modeSwitchNow()") && paint.contains("switchProxyTag()"),
            "那一排的亮起来历不是那两格与组，会跟下面的框不一致"
        );
        assert!(
            js.contains("formDirty = false;\n  paintModeSwitch();"),
            "读完盘上那份之后没重算那一排的读数"
        );
        assert!(
            js.contains("下面那几格被单独拧过"),
            "组合落不到任何一档上时那一排不许硬选一档骗人"
        );

        // 缺组那一支：不改格子、但要出声（警告色），因为"按了没反应"是这一屏最坏的失效。
        assert!(
            choose.contains("paintModeSwitchNoTag();") && choose.contains("return;"),
            "没有一个组可指时那一排静默吞掉了这一下"
        );
        let notag = body_of(&js, "function paintModeSwitchNoTag() {");
        assert!(
            notag.contains("classList.add(\"warn\")"),
            "缺组那句没换底色，读起来像一条普通说明"
        );
        assert!(
            js.contains("这一屏上没有一个组 tag 可指"),
            "缺组那句没写给用户看的那半句出路"
        );

        for selector in [
            ".mode-switch {",
            ".mode-opt {",
            ".mode-opt.on {",
            ".mode-opt.on span {",
            "#mode-switch-note.muted-note {",
        ] {
            assert!(css.contains(selector), "CSS 里没有 {selector}");
        }
    }

    /// 「保存被拒」那一格是**清单**的落点：`validate()` 一次列全之后送来的是多行文本，
    /// 屏幕要是把它折成一条长句，"一次列全"就白做了。CSS 按 id 命中（`setSaveNote` 会改
    /// `className`，按 class 写的规则会被换掉），JS 那一支必须把 `errorText(result)` 原样递进来。
    ///
    /// 「诊断」那一屏收的是同一份清单（`doctor` 的 `invalid_state` 一条里就是"这 N 处不成立，要改："
    /// 那几行；帽子自己带主语、不带动作标签，见 `describe_problems` 那条），
    /// 所以 `ul#findings li` 也一起钉住——两处只钉一处的话，另一处照样把清单压成一句长话。
    #[test]
    fn the_save_note_keeps_every_problem_on_its_own_line() {
        let js = body("/panel.js");
        let css = body("/panel.css");

        for head in ["#save-out {", "ul#findings li {"] {
            let at = css.find(head).unwrap_or_else(|| panic!("CSS 里没有 {head}"));
            let rule = &css[at..at + css[at..].find('}').unwrap() + 1];
            assert!(
                rule.contains("white-space: pre-line"),
                "{head} 没带 pre-line，多行清单会被折成一条长句：{rule}"
            );
        }

        let setter = body_of(js, "function setSaveNote(message, klass) {");
        assert!(setter.contains("node.textContent = message"), "{setter}");
        assert!(setter.contains("node.className = klass"), "{setter}");
        assert!(
            !setter.contains(".replace("),
            "把换行换掉等于取消一次列全：{setter}"
        );

        let rejected = body_of(js, "async function save(replay) {");
        assert!(
            rejected.contains("setSaveNote(\"保存被拒（HTTP \" + result.status + \"）：\" + errorText(result)"),
            "400 那一条没把后端给的清单原样送到那一格：{rejected}"
        );
    }

    /// 外壳借来的那三件（底部退出、卡片右上角徽标、危险档底色）与"按关键词建地区组"那一格，
    /// 都是"HTML 给锚点、JS 给行为、CSS 给形状"的三份资产功能。缺一边在现场只长成
    /// "这块面板没反应"，所以同样钉在编译期。
    #[test]
    fn the_borrowed_shell_pieces_are_wired_in_all_three_assets() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");
        for anchor in [
            "logout",
            "overview-badge",
            "doctor-badge",
            "preview-badge",
            "logs-badge",
            "group-region-key",
            "add-group-region-lock",
            "add-group-region-auto",
            "group-region-out",
        ] {
            assert!(
                html.contains(&format!("id=\"{anchor}\"")),
                "HTML 少了 #{anchor}"
            );
            assert!(js.contains(&format!("\"{anchor}\"")), "JS 没引用 #{anchor}");
        }
        // 徽标与危险档要有 CSS，否则它们只是两段没有样式的文字
        for selector in [
            ".badge",
            ".badge--ok",
            ".badge--bad",
            ".card--danger",
            ".sub",
            ".side-foot",
        ] {
            assert!(css.contains(selector), "CSS 里没有 {selector}");
        }
        // 退出这条不许碰任何写路径：它只清 sessionStorage 里那份令牌
        assert!(
            js.contains("rememberToken(\"\")"),
            "#logout 没清令牌，那这个按钮说的是假的"
        );
    }

    /// 「导入订阅」是那条主路的第二步，也是"三份资产要同时改"的又一格：HTML 给锚点与承诺、
    /// JS 给行为、CSS 给那一行的形状。它比只读那几格多两条**只属于它**的判据：
    /// ① 它写的是磁盘而不是表单，所以脏表单那一支必须先拦——不然现场就是"按了导入，名单没变"
    /// （重读不覆盖脏表单，这是编辑层的既有纪律）；
    /// ② 它的输入本身就是那批凭据，所以成功后输入框要清空，而这一问只带一个 `url`、
    /// 不带 `If-Match`（只追加就不覆盖别人写过的键）。
    #[test]
    fn the_subscription_import_is_wired_in_all_three_assets() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");
        for anchor in ["sub-url", "sub-import", "sub-out"] {
            assert!(
                html.contains(&format!("id=\"{anchor}\"")),
                "HTML 少了 #{anchor}"
            );
            assert!(js.contains(&format!("\"{anchor}\"")), "JS 没引用 #{anchor}");
        }
        assert!(
            html.contains(">导入订阅</button>"),
            "按钮上写的不是「导入订阅」，那两处提到它的说明就指错了名字"
        );
        // 唯一的这一问
        assert!(
            js.contains("call(\"POST\", \"/api/subscription\", JSON.stringify({ url: url }))"),
            "JS 没去 POST /api/subscription，那 #sub-out 就是一块死字"
        );
        // 回执那三格：说了没说的话在 message 里，名单在 added 里，"到底写盘了没有"在 written 里
        for field in ["String(value.message)", "value.added", "value.written"] {
            assert!(js.contains(field), "JS 没读回执里的 {field}");
        }
        // ① 脏表单那一支
        assert!(
            js.contains("表单里还有没保存的改动"),
            "JS 少了脏表单那一支：导入会写磁盘，而重读不覆盖表单"
        );
        // ② 成功后清空输入框
        assert!(
            js.contains("document.getElementById(\"sub-url\").value = \"\";"),
            "导入成功后没清空那串链接，它就带着机场的令牌留在页面上"
        );
        // HTML 那两句承诺：不碰数据面、凭据不过页面。这两句是这一屏对用户的承诺，
        // 服务端那边由 `App::subscription` 的回执与 `api::tests` 那几条钉着。
        for sentence in [
            "只写磁盘，不动数据面",
            "既不留在页面上，也不留在磁盘上",
            "追加新名字",
        ] {
            assert!(html.contains(sentence), "HTML 少了那句承诺：{sentence}");
        }
        // 面板不能再自称"导入还没做"——这两句是 U3 之前留下的，留着就是把第一步说没了
        for stale in ["还没接通", "还在做"] {
            assert!(
                !html.contains(stale) && !js.contains(stale),
                "资产里还写着「{stale}」：订阅导入已经落地了"
            );
        }
        // 「导入订阅」这四个字在概览那条主路、那一屏与页题里指的是同一个按钮
        assert!(html.contains("按「导入订阅」"), "概览那条主路没指向这个按钮");
        assert!(js.contains("按「导入订阅」"), "页题那句没指向这个按钮");
        assert!(css.contains(".rule-row"), "那一行「输入框 + 按钮」的容器没有样式");
    }

    /// 这一轮真机验收抓出来的两句假话（一次导入回执 + 一次脏表单拒绝，在它们各自之后
    /// 那一步按下去就变成假的）：「放弃改动，重读磁盘」把改动丢掉之后，屏幕上还写着
    /// "表单里还有没保存的改动"；退出登录之后还写着上一轮导入了哪几个名字，而这一页
    /// 此刻连不上那台机器。两条都由 `resetSubReceipt()` 退回 HTML 开局那一句，
    /// 而"开局那一句"在两处（HTML 与 JS）——差一个字就又是两份不同的话，所以钉在编译期。
    /// 第三条是这一屏特有的：退出登录要清掉输入框里那串链接，它带着机场的令牌，
    /// 和上面清 `#token` 同一个理由。
    #[test]
    fn the_import_receipt_returns_to_its_opening_line_when_it_stops_being_true() {
        let html = body("/");
        let js = body("/panel.js");
        const OPENING: &str = "还没有导入过。";
        assert!(
            html.contains(&format!("id=\"sub-out\">{OPENING}<")),
            "HTML 里 #sub-out 开局写的不是那句"
        );
        assert!(
            js.contains(&format!("\"{OPENING}\"")),
            "JS 没把 #sub-out 退回开局那一句，两边就不是同一句话"
        );
        // 一处定义、两处落地：放弃改动 与 退出登录
        assert_eq!(
            js.matches("resetSubReceipt(").count(),
            3,
            "resetSubReceipt 应当是「一个定义 + 两个调用点」，现在是 {}\n\
             少一边就回到那两句假话：拒绝的话还挂在已经干净的表单上，\
             或者退出之后屏幕上还写着上一轮的导入",
            js.matches("resetSubReceipt(").count()
        );
        // 清空那串链接：成功那一路一次、退出登录再一次
        assert_eq!(
            js.matches("getElementById(\"sub-url\").value = \"\";").count(),
            2,
            "输入框里那串带令牌的链接要在「导入成功」与「退出登录」两处都清掉"
        );
    }

    /// §11.3 D 那一句「要改回来点这里」的落地：回执说动了「流量怎么走」那一档，
    /// 屏幕上就得有一个真能按的去处，而且三处名字要相同（导航 label、页题、按钮文字）——
    /// 按钮写着「流量怎么走」而点开那一屏的页题是别的名字，就是那句话指错了地方。
    /// 第二条判据只属于这一按：它是**常驻还是收回**。这一按没动那一档的时候
    /// （第二次导入、导入失败、脏表单被拒）按钮必须收起来，否则它指的是上一次的回执。
    #[test]
    fn the_import_receipt_points_at_the_screen_it_changed() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");
        // 三处名字逐字相同
        assert!(
            html.contains("<span class=\"label\">流量怎么走</span>"),
            "导航里那一屏不叫「流量怎么走」，按钮和回执那一句就都指错了"
        );
        assert!(
            html.contains("id=\"sub-policy-link\" class=\"mini\" type=\"button\" hidden>流量怎么走</button>"),
            "HTML 里那枚按钮不写「流量怎么走」，或开局不是 hidden"
        );
        assert!(
            js.contains("routing: [\"流量怎么走\","),
            "JS 的 PAGES 页题不是「流量怎么走」，点进去顶栏会换一个名字"
        );
        // 按下去真的去那一屏
        assert!(
            js.contains("document.getElementById(\"sub-policy-link\").addEventListener(\"click\"")
                && js.contains("setView(\"routing\");"),
            "JS 没把这枚按钮接到「流量怎么走」那一屏，它就是块死字"
        );
        // 回执那个字段被读了：这一按**有没有**动那一档只有服务端知道
        assert!(
            js.contains("value.default_set === true"),
            "JS 没读 default_set，按钮就只能常驻——那等于每轮回执都说我替你设过"
        );
        // 一个定义 + 三个收回点：每次按下去先收、成功那一路再决定、退回开局那一句时一起收
        assert_eq!(
            js.matches("setSubDefaultLink(").count(),
            4,
            "setSubDefaultLink 应当是「一个定义 + 三个调用点」（开头收回 / 成功那一路 / 退回开局），\
             现在是 {}\n少一边就回到那个假象：按钮留在屏幕上，指的是上一次那一按",
            js.matches("setSubDefaultLink(").count()
        );
        assert!(
            css.contains("#sub-policy-link"),
            "CSS 没有这一枚按钮的位置——它要贴在回执那一句下面，隔一屏就等于没给"
        );
    }

    /// 概览那一行 `#render-errors` 是这一轮真机验收抓出来的：预览那一屏说"dae 没渲染出来"，
    /// 而概览的「生成报错」写着"无"——两句话都是真的，说的却是两份不同的生成物。
    /// 锚点在 HTML、写字的代码在 JS，少一边就回到那个假象，所以钉在编译期。
    #[test]
    fn the_overview_has_a_row_for_the_orchestration_renders() {
        let html = body("/");
        let js = body("/panel.js");
        assert!(
            html.contains("id=\"render-errors\""),
            "HTML 少了 #render-errors"
        );
        assert!(js.contains("\"render-errors\""), "JS 没写 #render-errors");
        assert!(js.contains("render_errors"), "JS 没读回执里那一格");
        // 保存回执也要带上它：按下保存的人就是写出这一份的人
        assert!(
            js.contains("已写入磁盘"),
            "保存那句不在 JS 里，这条附带的句子无处可接"
        );
        // 「退出登录」把每张只读卡片退回开局那一句，而退回的措辞必须和 HTML 里那句逐字一样：
        // 漂了的话退出会把界面停进第三种状态——既不是初始态，也不是读数。
        assert!(
            js.contains("\"未读取。这台机器上几方各自写的日志在这里看，面板不动它们也不删它们。\"")
                && html.contains("未读取。这台机器上几方各自写的日志在这里看，面板不动它们也不删它们。"),
            "退出时要退回 logs-out 的那句在两份资产里不是同一句"
        );
        // 退回那句的同时，五张卡与"手里那一份"必须一起清掉：留着它们，徽标写着"未读取"而
        // 卡上还印着上一轮的取数时刻与字节数——同一屏上两句都真，合起来是假的。
        assert!(
            js.contains("lastLogs = null") && js.contains("renderLogs()"),
            "退出没清掉那五张卡"
        );
    }

    /// IPv6 那一行（`#hijack6`）是同一类"三份资产要同时改"的锚点，而且它多一条理由：
    /// v4 与 v6 的 nft 表、策略路由、监听口是**各自独立**的（实测：删掉 `ip6 zbox` 时
    /// `ip zbox` 一行不动，v6 侧的读数只能来自 `ss -lntu -6`），所以概览里必须有两行，
    /// 合成一行就会把"v4 全好、v6 一条都没加载"读成"劫持层已加载"。
    /// 退出登录那张清空的名单也要带上它，否则同一屏会同时写着"没读到"和"IPv6 在听 22"。
    #[test]
    fn the_overview_keeps_the_ipv6_half_on_its_own_row() {
        let html = body("/");
        let js = body("/panel.js");
        assert!(html.contains("id=\"hijack6\""), "HTML 少了 #hijack6");
        assert!(js.contains("\"hijack6\""), "JS 没写 #hijack6");
        // 读的是 v6 那五格，不是 v4 那几格的别名
        for field in [
            "v6_table_loaded",
            "v6_divert_rule_loaded",
            "v6_forward_all",
            "v6_accept_ra_all",
            "v6_listening",
        ] {
            assert!(js.contains(field), "JS 没读回执里的 {field}");
        }
        // 清空名单（退出登录退回开局那一句）与 v4 那行同批，漏一个就留旧读数。
        // 认的是那一整段——`"service-active"` 在文件里出现好几次，单认一个词会认错。
        assert!(
            js.contains(
                "\"render-errors\", \"listening\", \"service-active\", \"hijack\", \"hijack6\","
            ),
            "退出的名单没带上 hijack6"
        );
    }

    /// 那条主路的最后一步——"把设备的 DNS 或网关指到这台 ZBox"——要有一个地址可抄。
    /// 能填这一格的 `network` 屏在 §11 的分档里住**高级档**，所以普通档必须有一个**只读**的落点，
    /// 否则用户得先开高级模式才知道该抄什么。
    /// 同一行还必须带"这是磁盘上写的那一份"那句限定：`lan_address` 是期望态里的字段，
    /// 网卡上此刻长着它是另一回事，把前者说成后者是这一类界面最容易犯的错。
    #[test]
    fn the_basic_tier_shows_which_address_clients_should_point_at() {
        let html = body("/");
        let js = body("/panel.js");
        assert!(html.contains("id=\"client-target\""), "HTML 少了 #client-target");
        assert!(js.contains("\"client-target\""), "JS 没写 #client-target");
        assert!(js.contains("network.lan_address"), "JS 没读 state.network.lan_address");
        assert!(
            js.contains("network.route_mode === \"takeover\""),
            "接管网关那一档没说清\"网关也要指到这里\"，只说 DNS 会让人以为改一半就够"
        );
        assert!(
            js.contains("这是磁盘上写的那一份"),
            "这一行少了\"期望态不是现场读数\"那句限定"
        );
        assert!(
            js.contains("\"hijack6\", \"client-target\","),
            "退出登录的清空名单没带上 client-target：徽标改口\"未读取\"旁边会留着一个旧地址"
        );
        // 真机那一轮抓出来的：`setText` 少第三个参数＝**底色不动**，于是填好地址之后那一行还留着
        // "还没填"那一次的 warn 红（保存路径会重跑一次渲染，正是这一刻看得见）。
        // 认的是那一次调用里显式给了空 class，不认整段字面——排版一改就误红。
        let at = js
            .find("不是网卡此刻的地址。")
            .expect("地址那一句不在了，这一条断言要跟着改");
        let tail = &js[at..];
        let between = &tail[..tail.find(");").expect("那一次 setText 没有收尾")];
        assert!(
            between.contains("\"\""),
            "地址那一句落地时没显式把底色清回去：修好之后它还会留着\"还没填\"那一次的红色"
        );
    }

    /// 「本机网卡」那一行：概览里**唯一**一处现场网卡读数。
    ///
    /// 它钉的是四条已经付过学费的界：① 数据只许来自 `GET /api/status` 那一次读取（再来一次
    /// 探测就是第二份真相，"屏幕上的网卡"与"下发用的网卡"会分开）；② 探不到是 `note` 不是
    /// `warn`——"没读到"永远不许长成"不合规"；③ 两支都要**显式**给底色（`setText` 少第三个
    /// 参数就是颜色不动，同一 id 跨分支换色必须自己清）；④ 退出登录时它得跟着徽标一起退场，
    /// 否则"未读取"旁边立着一张上一轮的网卡名。
    #[test]
    fn overview_reports_the_probed_nic_shape_without_inventing_a_second_reading() {
        let html = body("/");
        let js = body("/panel.js");
        assert!(html.contains("id=\"network-shape\""), "HTML 少了 #network-shape");
        // 读的是 status 回执里那一份，不是新开一个端点
        assert!(
            js.contains("const probe = result.value.network || {};"),
            "JS 没把 #network-shape 接在 GET /api/status 的 network 上"
        );
        let at = js
            .find("const probe = result.value.network")
            .expect("那一段不在了，这一条断言要跟着改");
        let region = &js[at..js[at..].find("renderRevisions(").expect("后面没有修订历史那一步") + at];
        let calls: Vec<&str> = region.split("setText(").skip(1).collect();
        assert_eq!(calls.len(), 2, "探到/探不到两支里有一支不再是 setText");
        for call in &calls {
            let until = call.find(");").expect("一次 setText 没有收尾");
            assert!(
                call[..until].contains("\"\"") || call[..until].contains("\"note\""),
                "#network-shape 有一支没显式给底色：跨分支换色时上一支的颜色会留下"
            );
        }
        assert!(
            region.contains("\"note\"") && !region.contains("\"warn\""),
            "探不到网卡形状那一支给的必须是 note；warn 是\"配错了\"的脸色，而没探到不是"
        );
        assert!(
            region.contains("本机网卡形状没探出来"),
            "探不到那一句要明说\"没探出来\"，不能写成不合规"
        );
        // 退出登录那一份清空名单：认"这个名字出现在那一次清空的名单里"，不认换行与缩进——
        // 排版一改就误红，而这一条要钉的只是"它在名单里"。
        let at = js
            .find("setText(id, \"未读取\"")
            .expect("退出登录那一次读数清空不在了");
        let window = &js[at.saturating_sub(600)..at];
        assert!(
            window.contains("\"network-shape\""),
            "退出登录的清空名单没带上 network-shape：徽标改口\"未读取\"旁边会留着一张上一轮的网卡"
        );
    }

    /// 修订历史这一格是概览欠着的：`GET /api/status` 一直把 `store.list()` 带回来（最新在前），
    /// 而屏幕上只写得出"已知良好那一份"是哪一个，说不出机器上共有哪几份。
    /// 更要紧的是回滚那一步：原先要人把 30 个字符的修订号抄进输入框，而这一按会掐断在用连接——
    /// 抄错的代价恰好是最大的那一种。所以清单只许有一份来源（同一次 status 读取），
    /// 概览那一行与回滚那个下拉框必须由同一次渲染落地；选项由 JS 灌，HTML 里的 `<select>` 是空壳
    /// （写死一份就会漂成"屏幕上的清单与机器上的不是一份"）。
    #[test]
    fn the_revision_history_is_one_reading_shown_in_two_places() {
        let html = body("/");
        let js = body("/panel.js");
        // 概览那一行
        assert!(html.contains("id=\"revisions\""), "HTML 少了 #revisions");
        assert!(js.contains("\"revisions\""), "JS 没写 #revisions");
        // 读的是 status 带回的那一份 list()，不是第二处读取
        assert!(
            js.contains("renderRevisions(result.value.revisions, result.value.last_good, \"\")"),
            "JS 没把 status 回执里的 revisions 与 last_good 一起喂给那一次渲染"
        );
        // 三种脸色都要有话：读数正常 / 这一次没读到 / 退出登录退回开局
        assert!(
            js.contains("renderRevisions(null, null, \"failed\")")
                && js.contains("renderRevisions(null, null, \"fresh\")"),
            "renderRevisions 少了 degrade 或退场那一支"
        );
        // 回滚是一张清单，不是一个抄写框
        assert!(
            html.contains("<select id=\"rollback-revision\"></select>"),
            "回滚的控件不是那只空壳 select"
        );
        assert!(
            !html.contains("input id=\"rollback-revision"),
            "回滚又变回让人手抄 30 个字符的输入框"
        );
        // 退出的名单带上它，否则同一屏会同时写着"没读到"和上一轮的几份修订
        assert!(
            js.contains("\"source\", \"etag\", \"last-good\", \"revisions\","),
            "退出登录的清空名单没带上 revisions"
        );
        // 那句时钟话只在 HTML 里有一份，别让 JS 再写一遍漂开
        assert!(
            html.contains("时间按你这台浏览器的时钟渲染，机器自己的表可能与之不同"),
            "清单里那句时间的出处说明不在 HTML 里"
        );
    }

    /// 外壳是一等公民：导航项、内容块、JS 里那张页题表必须一一对得上。
    /// 三边少任何一边的现场形状都是"点了没反应"或"这一屏没有入口"，
    /// 而那两种都只长成"面板坏了"，所以钉在编译期而不是等真机那一轮。
    #[test]
    fn every_nav_item_has_a_page_and_both_are_in_the_js_table() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");
        let mut views: Vec<String> = Vec::new();
        let mut rest = html;
        while let Some(at) = rest.find("data-view=\"") {
            rest = &rest[at + "data-view=\"".len()..];
            let end = rest.find('"').expect("data-view 的值要有收尾的引号");
            views.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        views.sort();
        views.dedup();
        assert!(
            views.len() >= 15,
            "分屏外壳只剩 {} 屏：{}",
            views.len(),
            views.join(", ")
        );
        for view in &views {
            let marker = format!("data-view=\"{view}\"");
            let hits = html.matches(marker.as_str()).count();
            assert_eq!(
                hits, 2,
                "{view} 那一屏该有一处导航项加一处内容块，实际 {hits} 处"
            );
            // 键名带连字符时必须加引号（`"dns-records": [...]`），所以两种写法都算对上
            let keyed =
                js.contains(&format!("{view}: [\"")) || js.contains(&format!("\"{view}\": [\""));
            assert!(keyed, "JS 的 PAGES 表里没有 {view}，那一屏的页题就是空的");
        }
        for hook in [
            "nav",
            "nav-toggle",
            "editor-locked",
            "page-title",
            "page-sub",
        ] {
            assert!(
                html.contains(&format!("id=\"{hook}\"")),
                "HTML 的外壳少了 #{hook}"
            );
            assert!(js.contains(hook), "JS 没引用 #{hook}");
        }
        assert!(
            html.contains("class=\"nav-item\"") && js.contains(".nav-item"),
            "导航项与那段切换代码对不上：一个用 .nav-item，另一个不认它"
        );
        for selector in [
            "#sidebar",
            ".nav-item",
            ".card",
            ".topbar",
            "body.nav-collapsed",
        ] {
            assert!(
                css.contains(selector),
                "CSS 里没有 {selector}，外壳就没有形状"
            );
        }
        // 保存/放弃改动挪到了顶部工具条：它们管的是同一份 state.json，一屏一个会给出
        // "这一屏独立提交"的假象。所以整页只许各有一份。
        for id in ["save", "revert", "save-out"] {
            assert_eq!(
                html.matches(&format!("id=\"{id}\"")).count(),
                1,
                "#{id} 出现不止一次：浏览器只会拿到第一条，其余几屏的按钮是死的"
            );
        }
    }

    /// 面板分两档（取舍与屏集见 `docs/BLUEPRINT.md` §11.4）：普通档只给那条主路
    /// （连上 → 填出口 → 默认怎么走 → 哪台例外 → 让它生效）加读数那几屏，
    /// "改规则本身"整层藏进侧栏底部唯一一个开关。
    ///
    /// 这一格的失效方式**全是静默的**：少一份资产时面板照样打开，只是要么高级屏永远进不去
    /// （用户以为功能没做），要么普通用户一进来就撞见八屏规则（就是这次要改掉的那个样子）。
    /// 所以名单怎么分、默认哪一档、开关在哪、关掉之后人站在那屏上怎么回落，四件事都钉在编译期。
    #[test]
    fn the_two_tiers_and_the_single_mode_switch_are_wired_in_all_three_assets() {
        let html = body("/");
        let js = body("/panel.js");
        let css = body("/panel.css");

        // JS 那张高级名单，原样读出来
        let at = js
            .find("const ADVANCED_VIEWS = [")
            .expect("JS 少了 ADVANCED_VIEWS 这张高级档名单");
        let list = &js[at + "const ADVANCED_VIEWS = [".len()..js[at..].find(']').unwrap() + at];
        let advanced: Vec<String> = list
            .split(',')
            .map(|item| item.trim().trim_matches('"').to_string())
            .filter(|item| !item.is_empty())
            .collect();
        assert!(!advanced.is_empty(), "高级档名单是空的，那一层就没有东西可藏");

        // HTML 里每一屏都要有一个档位，而且只能有一个：导航项带 data-tier，内容块不带
        let mut rest = html;
        let mut views: Vec<String> = Vec::new();
        let mut html_advanced: Vec<String> = Vec::new();
        let mut html_basic: Vec<String> = Vec::new();
        while let Some(found) = rest.find("data-view=\"") {
            rest = &rest[found + "data-view=\"".len()..];
            let end = rest.find('"').expect("data-view 的值要有收尾的引号");
            let view = rest[..end].to_string();
            if !views.contains(&view) {
                let in_advanced = {
                    let block = html.find("<div id=\"nav-advanced\" hidden>").unwrap();
                    let tail = &html[block..];
                    let stop = tail.find("</div>").unwrap();
                    tail[..stop].contains(&format!("data-view=\"{view}\" data-tier=\"advanced\""))
                };
                let marked_basic =
                    html.contains(&format!("data-view=\"{view}\" data-tier=\"basic\""));
                let marked_advanced =
                    html.contains(&format!("data-view=\"{view}\" data-tier=\"advanced\""));
                assert!(
                    marked_basic != marked_advanced,
                    "{view} 那一屏的档位标了两遍或者压根没标"
                );
                assert_eq!(
                    in_advanced, marked_advanced,
                    "{view} 的 data-tier 与它实际住在哪一层对不上：\
                     名单和容器不一致时，收起的那一层会露出别人的屏"
                );
                if marked_advanced {
                    html_advanced.push(view.clone());
                } else {
                    html_basic.push(view.clone());
                }
                views.push(view);
            }
        }
        // 两档并起来就是全部屏（上面那个循环按导航项去重，所以一处一屏）
        let mut union = views.clone();
        union.sort();
        union.dedup();
        assert_eq!(
            union.len(),
            views.len(),
            "有一屏既被算进普通档又被算进高级档"
        );
        assert!(
            union.len() >= 15,
            "两档并起来只剩 {} 屏，外壳是不是少了屏：{}",
            union.len(),
            union.join(", ")
        );
        // JS 那张名单必须与 HTML 的 data-tier **逐项相等**（不只是"这一屏存在"）：
        // 两边各数各的时候，把 policy 划进高级档也能全绿——而那一屏正是主路上的一步。
        let mut js_advanced = advanced.clone();
        js_advanced.sort();
        let mut html_sorted = html_advanced.clone();
        html_sorted.sort();
        assert_eq!(
            js_advanced, html_sorted,
            "JS 的高级档名单与 HTML 标了 advanced 的那些屏不是同一批"
        );
        assert!(
            html_basic
                .iter()
                .all(|view| !html_advanced.contains(view)),
            "有一屏同时被算进两档"
        );
        // 普通档 = HTML 里没被划进高级层的那些屏，数量要守住（主路那几步必须默认就在眼前）
        let basic_count = html_basic.len();
        assert!(
            basic_count >= 5 && basic_count <= 9,
            "普通档 {basic_count} 屏：主路那几步加上读数应该在 5–9 屏之间，再多就又长成\"程序员思维\"那一版了"
        );

        // 开关与那一层：三份资产都要认
        assert!(
            html.contains("<div id=\"nav-advanced\" hidden>"),
            "高级那一层要么没有容器，要么默认不是 hidden——普通用户开局就该只看到主路"
        );
        assert!(
            html.contains("id=\"mode-toggle\""),
            "HTML 少了 #mode-toggle：没有那一个开关，高级模式就只能靠改代码进"
        );
        for hook in ["mode-toggle", "nav-advanced", "ADVANCED_VIEWS", "applyMode"] {
            assert!(js.contains(hook), "JS 没引用 {hook}，那一档在运行时是死的");
        }
        assert!(
            css.contains(".side-foot"),
            "CSS 里没有 .side-foot：退出、开关、收起这三个动作在侧栏底部就没有位置"
        );
        assert!(
            css.contains("body.nav-collapsed .side-foot button"),
            "CSS 少了收起态那条：导航收成一条轨时，开关会跟着漂走或者撑破那一轨"
        );
        // 开关的两句文案都要在 JS 里；HTML 只许带"没开档"那一句，因为 JS 起来之前浏览器
        // 已经把按钮画出去了——两边说的必须是同一件事，否则点一下先跳一下字。
        for words in ["开启高级模式", "关闭高级模式"] {
            assert!(js.contains(words), "JS 少了开关的那句「{words}」");
        }
        assert!(
            html.contains("id=\"mode-toggle\" type=\"button\">开启高级模式</button>"),
            "HTML 开局那个开关的默认句子不是「开启高级模式」：默认档就不再是普通档了"
        );
        // 关掉档的时候人若正站在高级屏上，必须被带回「现在的状态」；没开档时手打的地址片段
        // 也进不去。两句回落少任何一句，都会留下"顶栏写着屏名、内容再也点不到"那种假象。
        assert!(
            js.contains("currentView = \"overview\""),
            "JS 少了未开档时回落到「现在的状态」那一句"
        );
        assert!(
            js.contains("if (!advancedOn && ADVANCED_VIEWS.indexOf(currentView) >= 0) setView(\"overview\");"),
            "关掉高级模式时没把人从那屏上带下来"
        );
        // 模式只住浏览器，不进磁盘那份期望态：MODE_KEY 只在"定义 + 读 + 写"三处出现。
        // 多一处就说明它被塞进了某个请求体——那会让一个人开的档改到共用的 state.json。
        assert_eq!(
            js.matches("MODE_KEY").count(),
            3,
            "MODE_KEY 出现的次数不是三处（定义 / localStorage 读 / localStorage 写），\
             开关有可能被写进了发给磁盘的那一份"
        );
        assert!(
            js.contains("window.localStorage") && !js.contains("state.mode"),
            "开关的落点应该是 localStorage，不是状态里那个 mode 字段"
        );
    }

    /// 一屏的名字写两处：HTML 导航项里那句 label，和 JS `PAGES` 里那个页题。
    /// §11 那一轮改名（分流 → 默认怎么走、节点与订阅 → 订阅与节点……）只要漏了一处，
    /// 现场表现就是"按的是 A，顶栏写着 B"——对一个不懂程序的人来说，这等于面板在说两件不同的事。
    /// 所以逐字比，不比"看起来差不多"。
    #[test]
    fn each_nav_label_is_verbatim_the_js_page_title() {
        let html = body("/");
        let js = body("/panel.js");
        let items = html.matches("class=\"nav-item\"").count();
        assert!(items >= 15, "HTML 只剩 {items} 个导航项");
        let mut compared = 0usize;
        let mut rest = html;
        while let Some(at) = rest.find("data-view=\"") {
            rest = &rest[at + "data-view=\"".len()..];
            let end = rest.find('"').expect("data-view 的值要有收尾的引号");
            let view = &rest[..end];
            rest = &rest[end..];
            // 导航项一行一条，label 就在这一行里；内容块那一处没有 label，跳过
            let line_end = rest.find('\n').unwrap_or(rest.len());
            let line = &rest[..line_end];
            let Some(label_at) = line.find("<span class=\"label\">") else { continue };
            let after = &line[label_at + "<span class=\"label\">".len()..];
            let label = &after[..after.find("</span>").expect("label 要有收尾")];
            let keyed = js.find(&format!("{view}: [\"")).or_else(|| js.find(&format!("\"{view}\": [\"")));
            let at = keyed.unwrap_or_else(|| panic!("JS 的 PAGES 表里没有 {view}，那句 label 没人对"));
            let tail = &js[at..];
            let head = &tail[tail.find("[\"").unwrap() + 2..];
            let title = &head[..head.find('"').expect("页题要有收尾的引号")];
            assert_eq!(
                title, label,
                "{view} 这一屏两处名字漂了：导航项写着「{label}」，页题却是「{title}」"
            );
            compared += 1;
        }
        assert_eq!(
            compared, items,
            "只比了 {compared} 屏，HTML 却有 {items} 个导航项：有一屏的 label 不在那一行上"
        );
    }

    /// 连接屏那张「这条路怎么走」是主路的**第①步就读到**的说明书（§11.15 的 U9 走查）。
    /// 它讲的是"别的屏上有什么、按钮住在哪儿"，所以它自己永远不会红——别的屏改名、按钮搬家，
    /// 它只会安静地变成一句谎话，而读它的正好是最不该被谎话对待的那个人（不懂程序、只照着做的用户）。
    /// 本轮就抓出三句谎：③ 还写着三档开关之前的"这一屏默认那份名单，通常不用动"（出厂那一颗是
    /// 「不科学」，智能分流要么靠导入订阅顺手设、要么自己按）；⑤ 说"保存到磁盘……三步都在这一屏"
    /// （「保存到磁盘」住在页面顶上那一排，不在任何一屏里）；顶上那句"其余各屏都住在开启高级模式后面"
    /// 漏了「现在怎么样」那组三屏（它们是普通档）。
    ///
    /// 三条判据：① 五步的 `<dt>` 名字逐字是侧栏的导航 label；② 说明书里用「」点名的按钮，
    /// 逐字是页面上某个按钮的文案；③ 三档的档名逐字是 `mode-switch` 里那三颗。
    /// 加反向钉：拆掉的三句谎不许回来。
    #[test]
    fn the_five_step_guide_names_only_screens_and_buttons_that_exist() {
        let html = body("/");
        let after_head = html
            .split("<h2>这条路怎么走</h2>")
            .nth(1)
            .expect("连接屏上没有「这条路怎么走」那张卡");
        let card = &after_head[..after_head.find("</div>").expect("那张卡的 </div> 收尾没了")];

        // ① 五步的屏名逐字要在侧栏导航里
        let mut steps = Vec::new();
        for piece in card.split("<dt>").skip(1) {
            let named = &piece[..piece.find("</dt>").expect("<dt> 要有收尾")];
            // 去掉 ①～⑤ 那个圈号与它后面的空格，剩下的就是屏名
            let label = named
                .split_once(char::is_whitespace)
                .map(|(_, rest)| rest)
                .unwrap_or(named)
                .trim();
            assert!(
                html.contains(&format!("<span class=\"label\">{label}</span>")),
                "说明书里的第⑤步之外又多了一屏，或那一屏改了名：{label} 在侧栏找不到"
            );
            steps.push(label.to_string());
        }
        assert_eq!(
            steps,
            vec![
                "连接与令牌",
                "订阅与节点",
                "流量怎么走",
                "让它生效"
            ],
            "必经那四屏的名字或**先后**变了：说明书的顺序就是主路的顺序"
        );

        // ② 点名的按钮既要真有其物，也要说明书里确实这么引的
        for name in [
            "导入订阅",
            "保存到磁盘",
            "放弃改动，重读磁盘",
            "开启高级模式",
            "显式下发当前期望状态",
        ] {
            assert!(
                html.contains(&format!(">{name}</button>")),
                "说明书引了「{name}」，页面上却没有这颗按钮"
            );
            assert!(
                card.contains(&format!("「{name}」")),
                "「{name}」这颗按钮在说明书里没人指它——搬家了不会有人知道"
            );
        }
        // 「下面那三个「加一条」按钮」这句要真有三个
        assert_eq!(
            html.matches(">加一条 ").count(),
            3,
            "「订阅与节点」那一屏的出口类型增删过，说明书那句「那三个」就得跟着改"
        );

        // ③ 三档的档名逐字是 mode-switch 里那三颗，说明书里也要用「」引着点名
        for name in ["不科学", "智能分流", "全局"] {
            assert!(
                html.contains(&format!(">{name}<span>")),
                "「默认怎么走」那一排的档位改了名：说明书里还是「{name}」"
            );
            assert!(
                card.contains(&format!("「{name}」")),
                "说明书里没再引「{name}」这一档，或引号丢了（屏上其余按钮名都是带「」的）"
            );
        }
        // 出厂那一颗是「不科学」不是「智能分流」——这句是③的全部信息量，别说反了。
        assert!(
            card.contains("出厂是第一颗「不科学」"),
            "说明书又不肯说新机器默认全部直连"
        );

        // ④ 保存那颗住顶栏，这句实话要留在屏上（它本轮被写成"三步都在这一屏"）
        assert!(
            card.contains("「保存到磁盘」不在哪一屏里面"),
            "说明书又不肯说保存按钮住在页面顶上——人会在那一屏找它"
        );
        for lie in [
            "三步都在这一屏",
            "其余各屏都住在",
            "这一屏默认那份名单",
        ] {
            assert!(!html.contains(lie), "旧那句谎又回来了：{lie}");
        }
    }

    /// 摘掉 HTML 注释。注释里那些话是给下一轮改代码的人看的设计笔记，不是给用户读的说明书，
    /// 拿它一起对照会把笔记判成谎话——人就去改笔记而不是改屏，修错了地方。
    fn visible_page(html: &str) -> String {
        let mut out = String::with_capacity(html.len());
        let mut rest = html;
        while let Some(start) = rest.find("<!--") {
            out.push_str(&rest[..start]);
            let Some(end) = rest[start..].find("-->") else {
                break;
            };
            rest = &rest[start + end + "-->".len()..];
        }
        out.push_str(rest);
        out
    }

    /// `<open …>` 到 `</close>` 之间的内容，按出现顺序全抓。
    /// `open` 以 `>` 收尾时内容直接从头算（`<dt>` 这类），否则先跳过属性（`<button …>` 这类）。
    fn between_all(text: &str, open: &str, close: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find(open) {
            let after = &rest[at + open.len()..];
            let skip = if open.ends_with('>') {
                0
            } else {
                after.find('>').map(|n| n + 1).unwrap_or(0)
            };
            let inner = &after[skip..];
            let Some(end) = inner.find(close) else { break };
            out.push(inner[..end].to_string());
            rest = &inner[end + close.len()..];
        }
        out
    }

    /// 剥掉标签、把空白压成单空格：按钮文案里套着提示用的 `<span>`，
    /// 要逐字比的是人眼在那颗按钮上看到的那串字，不是它的字节。
    fn plain(fragment: &str) -> String {
        let mut out = String::new();
        let mut in_tag = false;
        for ch in fragment.chars() {
            if ch == '<' {
                in_tag = true;
                continue;
            }
            if ch == '>' {
                in_tag = false;
                continue;
            }
            if !in_tag {
                if ch.is_whitespace() {
                    if !out.is_empty() && !out.ends_with(' ') {
                        out.push(' ');
                    }
                } else {
                    out.push(ch);
                }
            }
        }
        out.trim().to_string()
    }

    /// 页面上**人眼看得见的控件文案**：侧栏导航 label、侧栏分组题、按钮、卡片标题、`<dt>`。
    /// 这一份是从页面自己收集的，不是手抄的名单——所以改控件的人不必记得改这里。
    fn control_texts(page: &str) -> Vec<String> {
        let mut names: Vec<String> = between_all(page, "<span class=\"label\">", "</span>")
            .into_iter()
            .chain(between_all(page, "<p class=\"nav-group\">", "</p>"))
            .map(|raw| raw.trim().to_string())
            .collect();
        for raw in between_all(page, "<button", "</button>")
            .into_iter()
            .chain(between_all(page, "<h2", "</h2>"))
            .chain(between_all(page, "<h3", "</h3>"))
            .chain(between_all(page, "<dt>", "</dt>"))
        {
            let seen = plain(&raw);
            if !seen.is_empty() {
                names.push(seen);
            }
        }
        names.sort();
        names.dedup();
        names
    }

    /// 文本里所有用「」引起来的名号（去重、排序；跨行的和超过 30 字的不要——那是引号用错了）。
    fn quoted_names(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find('「') {
            let inner = &rest[at + '「'.len_utf8()..];
            let Some(close) = inner.find('」') else { break };
            let name = inner[..close].trim();
            if !name.is_empty() && !name.contains('\n') && name.chars().count() <= 30 {
                out.push(name.to_string());
            }
            rest = &inner[close + '」'.len_utf8()..];
        }
        out.sort();
        out.dedup();
        out
    }

    /// U11 只钉住了「这条路怎么走」那一张卡。这一条把同一套对照扩到**整份页面**：
    /// 任何一个用「」引起来的名号，要么逐字是某个控件的文案（侧栏题、导航项、按钮、卡片标题、`<dt>`），
    /// 要么在这条例外表上——每条都单独写明它为什么可以不是一处可点的东西。
    ///
    /// 为什么需要它：`#save`、三档开关、设备四栏那些名字在 U2/U4b/U9 里各改过一轮，
    /// 每次只改了被指着的那处，别处引用它的那句话安静地过期了——而对用户来说，
    /// "按「读取分流梯级」"点不到东西，就等于面板在指一个不存在的地方。
    ///
    /// 这里**不留**"这个词在 `panel.js` 里也有"这种后门：按钮改了名，JS 里那句回执多半还留着旧名，
    /// 后门会让整条测试白写。例外必须是点名的一条，还要当场自证那句理由仍然成立。
    /// 例外表**双向**：多一种新引法要红，那句话被改掉之后表上留着的旧条目也要红。
    #[test]
    fn every_quoted_name_on_the_page_resolves_to_something_real() {
        let html = body("/");
        let js = body("/panel.js");
        let page = visible_page(html);
        let names = control_texts(&page);
        let quoted = quoted_names(&page);
        // 下面两个数是当场量出来的（现页面：**26** 条引用、**129** 条文案）。
        // 门都放在"漂移"而不是"精确"上：加一条引用不必动这里，但引号被换成别的字形、
        // 或 HTML 结构变了导致收集器抓空——那两种情况下这条测试会**一声不响地一直绿**，
        // 那才是它真正的失效方式。
        assert!(
            quoted.len() >= 20,
            "整页只抓到 {} 个「」名号：引号大概被换成别的了，这条测试会安静地什么都不管",
            quoted.len()
        );
        assert!(
            names.len() >= 100,
            "只收集到 {} 条控件文案（实测 129）：多半是 HTML 结构改了、收集器跟着漂了一半，\
             这时每条引用都会被误报成\"不是控件文案\"，别看错方向",
            names.len()
        );

        let mut left = [
            "没有落点",
            "一键复制",
            "哪一条连接走了代理",
            "这条走了代理",
        ]
        .to_vec();
        let mut unexplained: Vec<String> = Vec::new();
        for name in &quoted {
            if names.iter().any(|n| n.contains(name.as_str())) {
                continue;
            }
            match left.iter().position(|w| w == name) {
                Some(at) => {
                    left.remove(at);
                }
                None => unexplained.push(name.clone()),
            }
        }
        assert!(
            unexplained.is_empty(),
            "页面上有「」引着的名号不是任何控件的文案：{unexplained:?}——要么那处东西确实不存在，要么它改了名"
        );
        assert!(
            left.is_empty(),
            "例外表上的条目已经用不到了：{left:?}——屏上那句话改了词，这张表要跟着缩"
        );

        // 四条例外各自的那句理由，也要当场核一遍，不然它们会变成永久免检
        // 「没有落点」是分流梯级那张表自己画出来的那一行状态词，由 JS 给（不是 HTML 抄的）
        assert!(
            js.contains("没有落点"),
            "「没有落点」不再是 JS 画出来的那个词了：那句话得改成控件名或删掉"
        );
        // 「一键复制」是**机场控制台**上的按钮，不是我们的；那句引号里外都得说清是谁家的
        assert!(
            page.contains("机场控制台「一键复制」"),
            "「一键复制」这一引没了前半句，读起来就像 ZBox 自己有这么一颗按钮"
        );
        // 那两句是看板上明写"这一屏给不出"的那句话**本身**：引它是为了说它给不出
        assert!(
            page.contains("这一屏给不出「哪一条连接走了代理」"),
            "看板那句「给不出」改写了：这两条例外要跟着重新看一眼"
        );
        assert!(
            page.contains("「这条走了代理」是本项目明令不许的做法"),
            "「这条走了代理」这一引离开了那句\"不许\"，就变成一句普通的按钮名引用了"
        );
    }

    /// 说明书顶上那两句是**数得出来**的承诺：「照着做」五屏、「现在怎么样」三屏。
    /// 侧栏加一屏、或把一屏从普通档挪进高级档，这两句当场过期；上一轮就是这么错的
    /// （它写"其余各屏都住在开启高级模式后面"，漏了普通档里「现在怎么样」那三屏）。
    #[test]
    fn the_sidebar_groups_have_the_names_and_counts_the_guide_claims() {
        let html = body("/");
        let page = visible_page(html);
        let nav_start = page.find("<nav id=\"nav\"").expect("侧栏那个 nav 容器没了");
        let from_nav = &page[nav_start..];
        let nav = &from_nav[..from_nav.find("</nav>").expect("侧栏的 nav 没有收尾")];

        let titles: Vec<String> = between_all(nav, "<p class=\"nav-group\">", "</p>")
            .iter()
            .map(|t| t.trim().to_string())
            .collect();
        assert_eq!(
            titles,
            vec!["照着做", "现在怎么样", "高级：改规则本身"],
            "侧栏分组的题改了名或换了顺序：说明书顶上那句指着这两个组的话要跟着改"
        );

        let counts: Vec<usize> = nav
            .split("<p class=\"nav-group\">")
            .skip(1)
            .map(|group| group.matches("class=\"nav-item\"").count())
            .collect();
        assert_eq!(
            counts,
            vec![4, 3, 8],
            "侧栏三组各几屏变了：说明书那句「五屏」「三屏」得跟着改数"
        );
        assert_eq!(
            page.matches("data-tier=\"basic\"").count(),
            7,
            "普通档的屏数不是 7：上面那 4 + 3 与档位开关是同一件事的两处写法"
        );
        assert_eq!(
            page.matches("data-tier=\"advanced\"").count(),
            8,
            "高级档的屏数不是 8：那颗开关藏起来的东西变了，说明书那句「剩下那些」要跟着改"
        );

        // 两句数逐字在说明书那一屏上，别只留在测试的断言里
        let after_head = page
            .split("<h2>这条路怎么走</h2>")
            .nth(1)
            .expect("连接屏上没有「这条路怎么走」那张卡");
        let card = &after_head[..after_head.find("</div>").expect("那张卡的 </div> 收尾没了")];
        assert!(
            card.contains("「照着做」四屏"),
            "说明书不再逐字说「四屏」：上面数出来的四屏与它说的话脱钩了"
        );
        assert!(
            card.contains("「现在怎么样」三屏"),
            "说明书不再逐字说「三屏」：那组三屏是普通档，不是高级档"
        );
    }

    #[test]
    fn the_page_defers_to_the_csp_it_declares() {
        // 页头的 CSP 是 `script-src 'self'; style-src 'self'`：一旦有人写内联脚本或 style 属性，
        // 那句 CSP 就成了假话（要么浏览器静默不执行，要么以后有人为它加 unsafe-inline）。
        let html = body("/");
        assert!(
            html.contains("script-src 'self'"),
            "CSP 里这句是内联不执行的依据"
        );
        assert!(
            html.contains("<script src=\"/panel.js\"></script>"),
            "{html}"
        );
        assert!(!html.contains("<script>"), "不许内联脚本");
        assert!(!html.contains(" style=\""), "不许 style 属性");
        assert!(!html.contains("onclick="), "不许内联事件");
        // JS 侧只用 textContent 建节点：状态文件里的节点名与报错文案都是外来文本
        assert!(
            !body("/panel.js").contains("innerHTML"),
            "外来内容不许进 innerHTML"
        );
    }

    /// 新机器（磁盘上还没写过状态文件）那两句话与那一步落点（§11.15，U9 走查抓出来的两处）。
    /// 为什么钉在编译期：这两处都只在**第一次**打开面板时出现一次，改坏了不会红任何一条现有测试，
    /// 而看懂它的正好是最没有耐心的那个人。
    /// ① 服务端那句 `etag: "absent"` 不许原样上屏——它今天写在"已读取。磁盘指纹 absent。"里，
    ///    而那一句正是新机器上唯一一句"接下来该做什么"；
    /// ② 连上之后不许把没有出口的人扔在「现在的状态」：主路的第②步在下一屏。
    #[test]
    fn a_brand_new_machine_says_so_in_chinese_and_lands_on_step_two() {
        let js = body("/panel.js");

        // ① 两处都换成中文，且旧那两句字面量不许回来
        assert!(
            js.contains("磁盘上还没写过状态文件，表单里是内置底稿"),
            "连接回执不再对新机器说\"磁盘上还没有状态文件\"——它会把服务端的 absent 原样递到人脸上"
        );
        assert!(
            js.contains("还没有（磁盘上没写过状态文件）"),
            "「现在的状态」那一格的磁盘指纹对新机器不给中文说法"
        );
        for old in ["磁盘指纹 absent", "absent（"] {
            assert!(!js.contains(old), "屏幕上还留着服务端的内部值 {old:?}");
        }

        // ② 落点：出口为空 → 「订阅与节点」，有出口 → 「现在的状态」；broken 不参与这个判断
        assert!(
            js.contains("setView(noEgress ? \"nodes\" : \"overview\");"),
            "连上之后的落点不再是\"有没有出口\"那一条判断：新机器会被扔在读数屏上"
        );
        assert!(
            js.contains("value.source !== \"broken\""),
            "noEgress 没排除读不出来的那份状态：broken 机器会被送到「订阅与节点」，\
             而那一屏此刻根本没有内容可看"
        );
        assert!(
            !js.contains("if (currentView === \"connect\") setView(\"overview\");"),
            "无条件回落 overview 那一句又回来了"
        );

        // ③ 同一张卡片上那两格不许打架：`待下发 是（内容与已知良好修订不一致）` 与
        //   `已知良好修订 尚无` 同屏，前一句就是在指控一台从没下发过的机器失败过一次。
        //   分法与「分流梯级」那一屏必须同一个（都看 last_good 在不在），只按布尔值说话就会漂。
        assert!(
            js.contains("是（这台机器一次都没成功下发过，没有"),
            "概览那一格对新机器不说\"这台一次都没成功下发过\"——它只按布尔值说话"
        );
        assert!(
            !js.contains("setText(\"reload-pending\", \"是（内容与已知良好修订不一致）\", \"warn\")"),
            "概览那一格又退回成\"pending=true 只有一句话\"的形状了"
        );
    }

    /// 窄屏那条主路的界（U9 实测）：导航只占顶上两行，第一件要做的事落在折线内。
    /// 钉的是"窄屏的导航是一条横向轨道"这个**形状**——它一旦退回成换行的按钮堆，
    /// 420px 下会摊成 439px 高，把「订阅与节点」的主按钮顶到 y=911（视口只有 900），
    /// 手机上打开面板的新用户看见的只有导航，没有要做的事。JS 那一半钉"程序换屏时
    /// 把当前那一格滑进视野"：滑轨装不下才滑，宽屏那条竖轨不许被动。
    #[test]
    fn the_narrow_nav_is_one_scrollable_strip_not_a_pile() {
        let css = body("/panel.css");
        let js = body("/panel.js");

        let start = css
            .find("@media (max-width: 56rem)")
            .expect("窄屏那条 media query 整块没了");
        let narrow = &css[start..];

        let nav_at = narrow.find("#nav {").expect("窄屏没有给 #nav 定形状");
        let nav_rule = &narrow[nav_at..nav_at + narrow[nav_at..].find('}').expect("#nav 规则没有收尾")];
        for needed in ["flex-wrap: nowrap", "overflow-x: auto", "flex: 1 1 100%"] {
            assert!(
                nav_rule.contains(needed),
                "窄屏的 #nav 少了 {needed:?}：它就不是那条只占一行、可以横着滑的轨道"
            );
        }
        assert!(
            !nav_rule.contains("flex-wrap: wrap"),
            "窄屏的 #nav 又要换行堆按钮——那一条正是把主按钮顶到折线外的形状"
        );

        // 高级那一层摊平进轨上，否则它整块挤成一格、里面的屏名点不到；
        // `:not([hidden])` 是必须的——`display: contents` 会盖掉 [hidden] 的 UA 规则，
        // 少了它就变成"关着档也看得见高级屏名"。
        assert!(
            narrow.contains("#nav-advanced:not([hidden])"),
            "窄屏摊平高级那层时丢了 :not([hidden])，或者根本没摊平"
        );
        assert!(
            narrow.contains("display: contents"),
            "窄屏没把高级那一层摊进轨道里"
        );

        assert!(
            js.contains("function revealActiveNav("),
            "JS 没有那个把当前屏滑进视野的函数"
        );
        assert_eq!(
            js.matches("revealActiveNav(").count(),
            2,
            "revealActiveNav 不止「定义 + setView 里一处」，有人另起了一条换屏的路"
        );
        assert!(
            js.contains("scrollIntoView"),
            "换屏没走 scrollIntoView：窄屏上程序把人送到某一屏时，那一格还在轨道外"
        );
        assert!(
            js.contains("inline: \"nearest\"") && !js.contains("inline: \"center\""),
            "滑轨又改成居中了——居中会把左端的分组标签滑掉半截（实测「照着做」缺字）"
        );
        assert!(
            js.contains("nav.scrollWidth <= nav.clientWidth"),
            "滑轨没判断就滑——宽屏那条竖轨会被它一起动到"
        );
    }
}
