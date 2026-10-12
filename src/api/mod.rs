//! 面板的接口核心：**只做"请求 -> 响应"这一步，不碰 socket**。
//!
//! 拆成纯函数是有原因的：`§7.2` 那三条硬约束里最容易验、也最容易被 socket 层糊过去的两条是
//! "端点不接受目录或路径参数"和"重载必须显式"。这一层根本没有 `File::open(req.path)` 这种写法
//! 可写——路径只从 [`crate::gen::init::Layout`] 派生，动词与端点的对应关系是一张常量表。
//! socket（`std::net::TcpListener`）与静态资源叠在这层**之上**，所以这一层的测试不需要端口、
//! 不需要 root、也不需要在真机上跑。
//!
//! 八条已经落在这里的语义：
//! - **保存 ≠ 下发**。`PUT /api/state` 只写盘并回报"要不要重载"，重载是独立的
//!   `POST /api/apply`。参考实现那种"保存后 1.5 秒静默重启"是被点名的反例（§5.4）。
//! - **预览只读**。`GET /api/preview` 把磁盘上那份期望态渲染成四份配置文本给面板看（编排层
//!   `dae` / `oxidns` 加内核层 `zbox.nft` / `netstate.sh`），
//!   不写盘、不下发、不碰内核——它是"UI 上那一格确实动了配置里那一行"的可见形态
//!   （`docs/BLUEPRINT.md` §5），所以渲染失败也是一条 200 回执里的一句 `ok:false`，不是 500。
//!   同一句失败还要出现在 `GET /api/state` 与保存回执的 `render_errors` 里：下发读的是另一份
//!   快照，所以它不能并进 `generation_error`（那会变成"下发会被拒"这句假话），但也不能只活在
//!   预览那一屏——2026-10-09 的现场就是保存回执说"已写入磁盘"而概览写着"生成报错：无"。
//! - **梯级只读**。`GET /api/ladder` 把同一份期望态读成一张表：每条规则在解析层排第几行、在连接层
//!   排第几行，或者根本不落行（带一句为什么）。它补的是预览给不出的那一句——四份配置文本对不懂
//!   程序的人不是"所以现在什么流量怎么走"。两列序号与两份渲染文本的逐行对应由 `gen::ladder`
//!   的测试钉着；一份生成物渲染不出来时这张表**照旧完整**，坏的那一份仍走 `render_errors` 那一格说。
//! - **回执分级**。下发的结果分三种：真重载了 / 内容与已知良好快照逐字节一致所以没动内核 /
//!   失败并自动回滚。面板必须能把它们显示成不同的话，不能只报"成功/失败"。
//! - **状态文件坏了要说"坏了"**。缺失（新装的机器，面板写第一份）与解析失败（老版本写的、
//!   被改坏的）在面板上是两个完全不同的界面，所以这里用 `source` 字段分开，不共用一个"空状态"。
//! - **没有令牌连端点存不存在都不告诉你**。见 [`token`]：门在路由之前，只读面也在门内，
//!   因为 `GET /api/state` 的磁盘形状里带着节点凭据。
//! - **保存要带前提条件**。`PUT` 必须 `If-Match` 上一轮读到的 `etag`，否则 412——
//!   两个标签页同开时，后按保存的那个人不该把前一个人的改动静默盖掉。
//! - **静态面是唯一不过令牌的门**，见 [`assets`]：浏览器加载 `/` 时带不上自定义头，这一页必须
//!   先出得来才谈得上填令牌。换来的条件是它给的字节必须是编译期常量、一个现场读数都不许有，
//!   而那件事由测试钉着，不由承诺钉着。
//! - **订阅导入只往 `nodes[]` 追加，回执里一条凭据都不带**。见 [`App::subscription`]：
//!   机场那一份订阅原文就是"所有出口的凭据"，它只在服务端过一遍，不进浏览器、不留暂存件。
//!
//! 监听套接字在 [`serve`] 里，它连带把 §8.1 那条"不许绑通配地址"变成了代码里的拒绝而不是文档里的劝告。

pub mod assets;
pub mod http;
pub mod serve;
pub mod token;

use crate::apply::engine::{self, ApplyOutcome};
use crate::apply::store;
use crate::doctor;
use crate::gen::snapshot_files;
use crate::gen::init::InitSystem;
use crate::model::config::{describe_problems, DesiredState, FinalOutbound, SMART_SPLIT_INTERVAL};
use crate::model::node::Node;
use crate::model::state;
use crate::svc::dns_api;
use crate::svc::observe::geo_data_present;
use crate::svc::runner::Runner;
use crate::svc::ruleset;
use crate::svc::subscribe;
use crate::svc::topology::{self, Topology};
use crate::svc::{now_secs, HijackGate, RollbackError, Svc, SyncError};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use token::PanelToken;

/// 请求体上限。状态文件里装着订阅 URL 展开出的节点表，几十 KB 量级；给到 1 MiB 已经宽到
/// 不可能因为合法配置而撞上，又能挡住对面一个手滑的大 body。
pub const MAX_BODY: usize = 1024 * 1024;

/// 体积超限的那句话。**协议层与应用层共用一条措辞**：对面收到的 413 不该因为
/// "是在 socket 上被挡的还是在这里被挡的"而长得不一样。
pub fn oversize_detail(actual: usize) -> String {
    format!("请求体 {actual} 字节，超过上限 {MAX_BODY}：这里收的是整份期望状态，不是分片")
}

/// 保存之后还没下发时给面板的那句提示原文。措辞是要出现在界面上的，所以只写一处。
pub const RELOAD_WARNING: &str = "下发会重启数据面，所有在用连接会被掐断一次";

/// `GET /api/logs` 一次给回的字节上限，从文件**末尾**往回数。
///
/// 它和 [`crate::svc::LOG_CAP`] 管的是两件不同的事：那边管这份文件最长能攒到多大（轮转），
/// 这边管一次界面刷新往 LAN 上拖多少。监控页要的是"刚刚发生了什么"，半兆不叫日志、叫下载。
pub const LOG_TAIL_CAP: u64 = 64 * 1024;

/// 状态文件还不存在时 `etag` 的值，也是 `PUT` 那条"我以为没人在我读之后动过盘"的前提。
///
/// 用一个词而不是 `null`：`null` 在 JSON 里既可能表示"没文件"也可能表示"服务端没算"，
/// 而这两种情况在丢更新保护里是完全不同的处理。
pub const ABSENT_ETAG: &str = "absent";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Put,
    Post,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<'a> {
    pub method: Method,
    /// 只比路径，不看查询串：所有端点都无参。带 `?` 的路径会原样落进这张表里比不中，
    /// 于是得到一个 404 而不是"参数被忽略"——这是刻意的。
    pub path: &'a str,
    pub body: &'a [u8],
    /// 面板令牌（socket 层从 `X-ZBox-Token` 头上摘下来原样递进来）。`None` 就是没带。
    pub token: Option<&'a str>,
    /// `If-Match`：客户端上一轮读到的那份状态的指纹。`PUT` 必需，见 [`ABSENT_ETAG`]。
    pub if_match: Option<&'a str>,
}

impl<'a> Request<'a> {
    pub fn get(path: &'a str) -> Self {
        Self {
            method: Method::Get,
            path,
            body: &[],
            token: None,
            if_match: None,
        }
    }

    pub fn put(path: &'a str, body: &'a [u8]) -> Self {
        Self {
            method: Method::Put,
            path,
            body,
            token: None,
            if_match: None,
        }
    }

    pub fn post(path: &'a str) -> Self {
        Self::post_body(path, &[])
    }

    /// 带动作参数的 POST。这四个写面端点里有两个要收参数（回滚到哪一份、要不要无视新鲜度），
    /// 而**路径与查询串都不许带参数**（见 [`Request::path`] 那条），所以参数只有一个容身处：体。
    pub fn post_body(path: &'a str, body: &'a [u8]) -> Self {
        Self {
            method: Method::Post,
            path,
            body,
            token: None,
            if_match: None,
        }
    }

    /// 带上令牌。测试里也用它，所以 401 那条路径是显式写出来的，不是"忘了带"。
    pub fn authorized(mut self, token: &'a str) -> Self {
        self.token = Some(token);
        self
    }

    pub fn matching(mut self, etag: &'a str) -> Self {
        self.if_match = Some(etag);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// 响应头的 `Content-Type`。**只有两条来源**：[`Response::JSON`] 与静态表里那一行的
    /// `content_type`——都是编译期常量，没有一处是"从请求里拼出来的"。
    pub content_type: &'static str,
    pub body: String,
}

impl Response {
    /// API 回执一律 JSON。
    pub const JSON: &'static str = "application/json; charset=utf-8";

    pub(crate) fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            content_type: Self::JSON,
            body: value.to_string(),
        }
    }

    pub(crate) fn ok(value: Value) -> Self {
        Self::json(200, value)
    }

    pub(crate) fn fail(status: u16, detail: impl AsRef<str>) -> Self {
        Self::json(status, json!({ "error": detail.as_ref() }))
    }

    /// 静态表里的那一行原样递出去。字节块是 `&'static str`，所以这里连一次复制都没有。
    fn asset(asset: &'static assets::Asset) -> Self {
        Self {
            status: 200,
            content_type: asset.content_type,
            body: asset.body.to_string(),
        }
    }

    /// 测试与 socket 层用它读 JSON 回执，不用第二次解析协议。静态响应的体不是 JSON，别调它。
    pub fn value(&self) -> Value {
        serde_json::from_str(&self.body).expect("API 回执一律是 JSON")
    }
}

/// API 端点每个只认一种动词。这张表也是 API 侧"404 还是 405"的唯一依据；
/// 静态那三条住在 [`assets`] 自己的表上，两条路都是常量表，没有一处拼接。
const ROUTES: &[(&str, Method)] = &[
    ("/api/state", Method::Get),
    ("/api/state", Method::Put),
    ("/api/status", Method::Get),
    ("/api/doctor", Method::Get),
    ("/api/preview", Method::Get),
    ("/api/ladder", Method::Get),
    ("/api/board", Method::Get),
    ("/api/logs", Method::Get),
    ("/api/dns-records", Method::Post),
    ("/api/dns-record", Method::Post),
    ("/api/apply", Method::Post),
    ("/api/subscription", Method::Post),
    ("/api/rule-sets/sync", Method::Post),
    ("/api/hijack/enable", Method::Post),
    ("/api/hijack/disable", Method::Post),
    ("/api/rollback", Method::Post),
];

pub struct App<'a> {
    svc: &'a Svc<'a>,
    token: &'a PanelToken,
    /// 观测面读解析器管理 API 的那条路。默认是 [`dns_api::Live`]（真去连回环上那个口）；
    /// 单测用 [`App::with_dns`] 换成假的——那一头的断言否则就取决于这台机器上有没有
    /// 一个 oxidns 正听着，而实测这一轮 guest 上就有。
    dns: DnsHandle<'a>,
}

enum DnsHandle<'a> {
    /// 构造时就解析一次地址：改坏了那个常量要在这里当场看见，而不是混进"连不上"那句里。
    Live(Result<dns_api::Live, String>),
    Injected(&'a dyn dns_api::DnsObserver),
}

impl<'a> App<'a> {
    pub fn new(svc: &'a Svc<'a>, token: &'a PanelToken) -> Self {
        Self {
            svc,
            token,
            dns: DnsHandle::Live(dns_api::Live::new()),
        }
    }

    /// 换掉观测面那条取数路。只有测试用得到。
    pub fn with_dns(mut self, dns: &'a dyn dns_api::DnsObserver) -> Self {
        self.dns = DnsHandle::Injected(dns);
        self
    }

    /// 这一轮要用的观察者。没有分配、没有泄漏：真那份在 `App` 里活着，假那份是借来的。
    fn dns_observer(&self) -> Result<&dyn dns_api::DnsObserver, String> {
        match &self.dns {
            DnsHandle::Injected(got) => Ok(*got),
            DnsHandle::Live(got) => match got {
                Ok(live) => Ok(live),
                Err(detail) => Err(detail.clone()),
            },
        }
    }
}

impl<'a> App<'a> {
    pub fn handle(&self, req: &Request<'_>) -> Response {
        // 静态面排在鉴权**之前**，这不是漏了：浏览器加载 `/` 的那一次请求带不上自定义头，
        // 所以这一页必须先出得来，人才有地方填令牌。它给的字节是编译期常量（见 `assets`），
        // 里面没有也不许有任何现场状态——那条边界由 `assets` 的测试钉着。
        // 动词仍然是窄的：`/` 这个路径人人猜得到，所以对猜得到的那三条只认 GET，
        // 别的动词在门外面就 405，不掉进"404 还是 401"那个含糊分支。
        if let Some(asset) = assets::lookup(req.path) {
            return match req.method {
                Method::Get => Response::asset(asset),
                other => Response::fail(
                    405,
                    format!(
                        "{} 是静态资源，只接受 GET（收到的动词是 {}）",
                        req.path,
                        verb(other)
                    ),
                ),
            };
        }
        // 鉴权排在 API 路由之前：没令牌的一律 401，连"这个端点存不存在"都不告诉对面。
        // 只读面也在门内，因为 `GET /api/state` 返回的磁盘形状里就带着节点凭据。
        if !self.token.accepts(req.token) {
            return Response::fail(
                401,
                "缺少或错误的访问令牌（X-ZBox-Token）。令牌在机器的 panel.token 里，\
                 用 `zboxd panel-token` 取",
            );
        }
        match (req.method, req.path) {
            (Method::Get, "/api/state") => self.read_state(),
            (Method::Put, "/api/state") => self.save(req),
            (Method::Get, "/api/status") => self.status(),
            (Method::Get, "/api/doctor") => self.doctor(),
            (Method::Get, "/api/preview") => self.preview(),
            (Method::Get, "/api/ladder") => self.ladder(),
            (Method::Get, "/api/board") => self.board(),
            (Method::Get, "/api/logs") => self.logs(),
            (Method::Post, "/api/dns-records") => self.dns_records(req),
            (Method::Post, "/api/dns-record") => self.dns_record(req),
            (Method::Post, "/api/apply") => self.apply(),
            (Method::Post, "/api/subscription") => self.subscription(req),
            (Method::Post, "/api/rule-sets/sync") => self.rule_sets_sync(req),
            (Method::Post, "/api/hijack/enable") => self.hijack_enable(req),
            (Method::Post, "/api/hijack/disable") => self.hijack_disable(req),
            (Method::Post, "/api/rollback") => self.rollback(req),
            _ => {
                let known = ROUTES.iter().any(|(p, _)| *p == req.path);
                if known {
                    let allowed: Vec<&str> = ROUTES
                        .iter()
                        .filter(|(p, _)| *p == req.path)
                        .map(|(_, m)| verb(*m))
                        .collect();
                    return Response::fail(
                        405,
                        format!(
                            "{} 只接受 {}（收到的动词是 {}）",
                            req.path,
                            allowed.join(" / "),
                            verb(req.method)
                        ),
                    );
                }
                Response::fail(404, format!("没有这个端点：{}", req.path))
            }
        }
    }

    fn state_path(&self) -> PathBuf {
        PathBuf::from(self.svc.layout().state_path())
    }

    /// 磁盘上那一份状态的指纹；文件不存在时是 [`ABSENT_ETAG`]。
    ///
    /// 算的是**原始字节**而不是解析后的值：坏掉的文件也要有一个指纹，否则"文件坏了要修"
    /// 这一条路正好被丢更新保护挡死。读不动时报错，不许退化成"那就当没变"。
    fn etag(&self) -> Result<String, String> {
        let path = self.state_path();
        if !path.is_file() {
            return Ok(ABSENT_ETAG.to_string());
        }
        std::fs::read(&path)
            .map(|bytes| store::bytes_digest(&bytes))
            .map_err(|e| format!("读取 {} 失败: {e}", path.display()))
    }

    /// 已知良好快照的 id。取不到就是 `null`——"没下发过"和"上次下发失败"在面板上是
    /// 一句话：修订历史里有没有条目。
    fn last_good_id(&self) -> Value {
        self.svc
            .store()
            .last_good()
            .map(|rev| json!(rev.id))
            .unwrap_or(Value::Null)
    }

    /// `GET /api/state`：面板打开时问的第一句。
    ///
    /// 返回的是**磁盘形状**（带 `schema` 键），面板原样改、原样 PUT 回来即可，不需要知道
    /// v1/v2 的差异——迁移在这一步已经做完了。
    fn read_state(&self) -> Response {
        let etag = match self.etag() {
            Ok(etag) => etag,
            Err(detail) => return Response::fail(500, detail),
        };
        let path = self.state_path();
        let (source, body, error) = if !path.is_file() {
            // 新装的机器：面板要编辑的是基线形状，尤其 `network` 保持默认，
            // 这样下发时 §6 第 5 步的网卡探测才不会被一份没人在意的旧值挡掉。
            (
                "missing",
                Some(disk_value(&DesiredState::baseline())),
                Value::Null,
            )
        } else {
            match state::read(&path) {
                Ok(saved) => ("saved", Some(disk_value(&saved)), Value::Null),
                Err(detail) => ("broken", None, json!(detail)),
            }
        };
        // 磁盘上那份（新机器时是基线形状）能解析出来才谈得上重载与渲染。
        let parsed = body
            .as_ref()
            .and_then(|v| serde_json::from_value::<DesiredState>(v.clone()).ok());
        let (reload_pending, generation_error) = match (source, &parsed) {
            // 只有"磁盘上真有一份"才谈得上要不要重载。基线那份还没存过，
            // 报 reload_pending=true 会让新装的机器一打开面板就顶着一个黄色警告。
            ("saved", Some(state)) => self.reload_signal(state),
            _ => (Value::Null, Value::Null),
        };
        let render_errors = match &parsed {
            Some(state) => render_failures(state),
            // 读不进来 = 两份都渲染不了，原因就是那句读取失败，跟预览那一屏同一说辞
            None => unrenderable_both(&error),
        };
        Response::ok(json!({
            "source": source,
            "state": body,
            "error": error,
            "etag": etag,
            "last_good": self.last_good_id(),
            "reload_pending": reload_pending,
            "generation_error": generation_error,
            "render_errors": render_errors,
        }))
    }

    /// `PUT /api/state`：整份替换，写盘，**不下发**。
    ///
    /// 必须带 `If-Match`：面板是多人同开的（两个标签页、两个人、手机和电脑各一份），
    /// 没有前提条件就是"最后按下保存的那个人覆盖所有人"，而且覆盖得毫无声响。
    /// 412 的回执里带当前的 `etag`，前端拿着它重新 `GET`、把用户这次的改动重放上去再存。
    fn save(&self, req: &Request<'_>) -> Response {
        let body = req.body;
        if body.len() > MAX_BODY {
            return Response::fail(413, oversize_detail(body.len()));
        }
        let Ok(text) = std::str::from_utf8(body) else {
            return Response::fail(400, "请求体不是合法的 UTF-8");
        };
        let current = match self.etag() {
            Ok(current) => current,
            Err(detail) => return Response::fail(500, detail),
        };
        match req.if_match {
            None => return Response::fail(
                400,
                format!("PUT 必须带 If-Match（这台机器当前的值是 {current}）：不带就等于承认可能盖掉别人的保存"),
            ),
            Some(offered) if offered != current => {
                return Response::json(
                    412,
                    json!({
                        "error": format!(
                            "这份状态自你读它之后已经变了（你带的 {offered}，现在 disk 上是 {current}）。\
                             重新读一次再把改动放上去，不要直接盖。"
                        ),
                        "etag": current,
                    }),
                );
            }
            Some(_) => {}
        }
        // 版本门 -> 迁移 -> serde -> validate()，全在 state::parse 里，与 CLI 同一条读路径
        let state = match state::parse(text) {
            Ok(parsed) => parsed,
            Err(detail) => return Response::fail(400, detail),
        };
        let path = self.state_path();
        if let Err(detail) = state::write(&path, &state) {
            return Response::fail(500, detail);
        }
        // 写完重新读一遍算指纹，而不是拿发送体的指纹：`state::write` 会做迁移和规范化，
        // 落盘的那份字节跟收到的那份本来就不一定相同，etag 必须描述磁盘。
        let etag = match self.etag() {
            Ok(etag) => etag,
            Err(detail) => return Response::fail(500, detail),
        };
        let (reload_pending, generation_error) = self.reload_signal(&state);
        let render_errors = render_failures(&state);
        Response::ok(json!({
            "saved": true,
            "source": "saved",
            "etag": etag,
            "reload_pending": reload_pending,
            "generation_error": generation_error,
            "render_errors": render_errors,
            "warning": RELOAD_WARNING,
        }))
    }

    /// `POST /api/apply`：显式下发。走的正是 `zboxd apply` 那条链
    /// （生成 -> stage -> check + 预跑 -> 提交 -> 重启 -> 探测），失败由引擎自己回滚。
    fn apply(&self) -> Response {
        let state = match self.disk_state() {
            Ok(state) => state,
            Err(response) => return response,
        };
        let files = match snapshot_files(&state) {
            Ok(files) => files,
            Err(detail) => {
                return Response::fail(400, format!("生成配置失败：{detail}"));
            }
        };
        let target = self.svc.target();
        match engine::apply(
            &self.svc.store(),
            target.as_ref(),
            &files,
            &self.svc.actions(),
            engine::DEFAULT_KEEP,
        ) {
            Ok(ApplyOutcome::Applied { revision }) => Response::ok(json!({
                "reloaded": true,
                "revision": revision,
                "message": format!("已下发修订 {revision}"),
                "last_good": self.last_good_id(),
            })),
            Ok(ApplyOutcome::Unchanged { revision }) => Response::ok(json!({
                "reloaded": false,
                "unchanged": true,
                "revision": revision,
                "message": format!("内容与 {revision} 完全一致，未触碰内核（不重启、不断流）"),
                "last_good": self.last_good_id(),
            })),
            Err(detail) => Response::fail(500, format!("下发失败：{detail}")),
        }
    }

    /// `POST /api/subscription`：抓一次订阅，把读出来的节点**追加**进磁盘那份 `nodes[]`。
    ///
    /// 为什么这一步落在服务端而不是"解析成节点交给面板、面板再保存一次"：要落盘的正是那批凭据，
    /// 让浏览器把它们一条条填回表单，等于把每个出口的 UUID 与密码塞进 DOM、再塞进一次整份 `PUT`。
    /// 反过来，这一步敢不守 `If-Match`，靠的是它**只追加**：磁盘上别人写过的键一个都不覆盖。
    /// 面板那一侧若真有没保存的改动，下一次保存会撞上既有的 412 重放路——那条路本来就在处理
    /// "编辑期间磁盘变了"这件事，而且新导入的行按名字认行，重放时不会被抹掉。
    ///
    /// 码分开给，因为对面要做的动作不同：
    /// - 400 = 链接那一格本身不成形状，或订阅内容读不成节点（Clash 配置、sing-box 导出、
    ///   全是读不懂的链接）——换一条链接；
    /// - 409 = 磁盘那份读不出来。**这一步一个进程都不起**，不去为一个坏掉的状态联网；
    /// - 500 = 抓取本身坏了（下载器起不来、非零退出、0 字节）——去查这台机器的网络。
    ///
    /// 磁盘上还没有状态文件**不是拦路**：主路第一步就是"新开的一台机器 + 一个订阅链接"，
    /// 那时那份状态还不存在，而这一按写的就是第一份。回执只有名字与计数，一条凭据都不带。
    fn subscription(&self, req: &Request<'_>) -> Response {
        let fields = match action_body(req, &["url"]) {
            Ok(fields) => fields,
            Err(detail) => return Response::fail(400, detail),
        };
        let raw = match url_of(&fields) {
            Ok(raw) => raw,
            Err(detail) => return Response::fail(400, detail),
        };
        let url = match subscribe::check_url(&raw) {
            Ok(url) => url,
            Err(detail) => return Response::fail(400, format!("订阅链接：{detail}")),
        };
        let path = self.state_path();
        // 新机器上这一步是**这台机器的第一份期望状态落地**，所以它带的每一个值从此就是"人写的"
        // ——[`fill_first_network`] 那一段就是为这件事加的（网卡那几格不能拿出厂默认写下去）。
        let fresh = !path.is_file();
        let mut merged = if fresh {
            DesiredState::baseline()
        } else {
            match state::read(&path) {
                Ok(got) => got,
                Err(detail) => return Response::fail(409, detail),
            }
        };
        let parsed = match self.svc.subscription().load(&url) {
            Ok(parsed) => parsed,
            Err(got @ subscribe::FetchFailure::Unreadable(_)) => {
                return Response::fail(400, got.to_string())
            }
            Err(got @ subscribe::FetchFailure::Downstream(_)) => {
                return Response::fail(500, got.to_string())
            }
        };
        let format = parsed.format.word();
        let parsed_count = parsed.nodes.len();
        let skipped = parsed.skipped;
        let (added, already) = append_nodes(&mut merged, parsed.nodes);
        // 一条都没新增就**不碰磁盘**：重写一份内容一样的文件会把别人的编辑挤成一次 412，
        // 而这一按什么都没改，不该留下任何痕迹。
        if added.is_empty() {
            let etag = match self.etag() {
                Ok(etag) => etag,
                Err(detail) => return Response::fail(500, detail),
            };
            return Response::ok(json!({
                "imported": true,
                "written": false,
                "format": format,
                "parsed": parsed_count,
                "skipped": skipped,
                "added": [],
                "already_there": already,
                "total_nodes": merged.nodes.len(),
                "etag": etag,
                "message": import_message(format, parsed_count, skipped, &already, true),
            }));
        }
        // **只有真要写盘时才探网卡**：排在链接读不懂（400）、抓取坏了（500）、状态读不出来（409）
        // 这三条早退之后——钉它的是 `a_bad_link...` 里 ② 那一句"读不懂就只跑那两条下载命令"。
        //
        // 为什么这一步必须管网卡：新机器上这一按写的就是第一份期望状态，从此磁盘上的
        // `network` 就是"人写的"——`svc::topology` 的规矩是显式写过就不再覆盖。所以网卡那几格
        // 不能拿出厂默认（`ens33` / `192.168.1.0/24`）落下去：面板后面那三条读写面全部只读磁盘，
        // 一份写着本机根本没有的网卡的期望态会一路生成进 dae 的 `lan_interface`，而那声响要到
        // 数据面起不来才听得见（§6 第 5 步当年就是为了这件事加的）。
        // 用的还是 CLI 那一个 `topology::fill`，不是第二份填充逻辑——两份，将来就是
        // "面板写的和 CLI 填的不是同一个形状"。
        //
        // 排在 `validate()` **之前**：探来的 `lan_address` 也是要过门的那一份，门只认落盘的形状。
        //
        // `fresh` 这个闸门才是"一个网名字节都不碰"的那一条：`parse` 拒绝零节点正文，而新机器
        // 那份底稿不含节点，所以"一条都没新增"在 `fresh == true` 下走不到——磁盘上已经有状态的
        // 机器一律 `fresh == false`，那才是这一按该躲开网卡的情形。
        let network_note = if fresh {
            fill_first_network(self.svc.runner(), &mut merged)
        } else {
            // 磁盘上已经有人写过：这一步只做追加，一个网名字节都不碰
            None
        };
        // 「导入即把默认怎么走设成智能分流」（§11.3 D）。**只在第一份状态上做**：磁盘上已经有
        // 一份的，那一档是人自己选过的，替他改回来就是暗改——那一种只补一句"新增的没进兜底组"。
        // 五格（cn_direct / private_direct / 兜底出口 / 境外上游 / 规则集声明）由
        // [`DesiredState::enable_smart_split`] 一次设齐，这里只负责把它翻成屏幕上看得见的一句话。
        let default_group = if fresh { merged.enable_smart_split() } else { None };
        // 排在 `validate()` **之前**：这一档动到的正是门要验的那几格（"策略组引用了不存在的出站"、
        // 规则集 `initial_path`），设错了要在这里就响，不能写下去之后让面板开不了。
        if let Err(problems) = merged.validate() {
            // 括号里那句要排在清单**之前**：一次列全之后清单是多行的，"磁盘上那一份没动"
            // 这句安全承诺要是掉在最后，屏幕上就被一堆条目挤没了。
            return Response::fail(
                400,
                format!(
                    "这一批节点放进这份状态里不合法（磁盘上那一份没动）：{}",
                    describe_problems(&problems)
                ),
            );
        }
        if let Err(detail) = state::write(&path, &merged) {
            return Response::fail(500, detail);
        }
        let etag = match self.etag() {
            Ok(etag) => etag,
            Err(detail) => return Response::fail(500, detail),
        };
        let (reload_pending, generation_error) = self.reload_signal(&merged);
        let render_errors = render_failures(&merged);
        let mut message = import_message(format, parsed_count, skipped, &already, false);
        // 网卡那一句排在最后，用 `｜` 分开——面板上 `#sub-out` 只有这一格，
        // 主路第一步"这台机器现在按哪张网卡走"必须和"导入了哪几个节点"出现在同一句里。
        if let Some(note) = &network_note {
            message.push('｜');
            message.push_str(note);
        }
        // 「默认怎么走」那一句排在**最后一位**：它是这一按里唯一"顺手替人改了别的格子"的那件事，
        // 排在节点与网卡之后才不会被前面两句压掉（§11.3 D 要的就是这句话在屏幕上看得见）。
        let default_note = match &default_group {
            Some(tag) => Some(smart_split_note(
                tag,
                merged.nodes.len(),
                geo_data_present(),
            )),
            // 磁盘那份已经有状态：这一按只追加，不改那一档，但新节点没进兜底组是要说出来的
            None => group_membership_note(&merged, &added),
        };
        if let Some(note) = &default_note {
            message.push('｜');
            message.push_str(note);
        }
        Response::ok(json!({
            "imported": true,
            "written": true,
            "source": "saved",
            "format": format,
            "parsed": parsed_count,
            "skipped": skipped,
            "added": added,
            "already_there": already,
            "total_nodes": merged.nodes.len(),
            "etag": etag,
            "reload_pending": reload_pending,
            "generation_error": generation_error,
            "render_errors": render_errors,
            "network_note": network_note,
            "default_set": default_group.is_some(),
            "default_group": default_group,
            "default_note": default_note,
            "warning": RELOAD_WARNING,
            "message": message,
        }))
    }

    /// 动作端点共用的那一步：**从磁盘重读**期望状态，而不是信对面发来的那一份。
    ///
    /// 缺失与损坏都是 409 而不是 400——那不是"你这次发的东西写错了"，是"这台机器还没就位"，
    /// 对面要做的动作（先去保存一次）因此不同。下发、同步规则集两条都走这一句，
    /// 因为面板上的按钮不该在"磁盘上那份"和"编辑区里那份"之间各选一头。
    fn disk_state(&self) -> Result<DesiredState, Response> {
        let path = self.state_path();
        if !path.is_file() {
            return Err(Response::fail(
                409,
                format!(
                    "{} 还不存在，没有可读的期望状态：先在面板上保存一次",
                    path.display()
                ),
            ));
        }
        state::read(&path).map_err(|detail| Response::fail(409, detail))
    }

    /// `POST /api/rule-sets/sync`：抓取 -> 归一化 -> 编译 -> 验收 -> 记清单。
    /// 动作住在 [`Svc::sync_rule_sets`]，CLI 那条 `zboxd rule-sets` 走的是同一个定义。
    ///
    /// 参数只有一个：`{"force": true}` 无视新鲜度全部重抓。不给就是 `false`。
    ///
    /// 两类失败的回执不同码但同带部分报告：`Gap`（声明了本地没有的文件，409——这一步修不了，
    /// 要人放文件或删声明）与 `Hard`（抓取编译落盘坏了，500）。**部分成功也要说清楚**：
    /// 只报错误而不报"另外几条已经装了盘"，现场表现就是"点了更新没反应"。
    fn rule_sets_sync(&self, req: &Request<'_>) -> Response {
        let fields = match action_body(req, &["force"]) {
            Ok(fields) => fields,
            Err(detail) => return Response::fail(400, detail),
        };
        let force = match flag(&fields, "force") {
            Ok(force) => force,
            Err(detail) => return Response::fail(400, detail),
        };
        let state = match self.disk_state() {
            Ok(state) => state,
            Err(response) => return response,
        };
        match self.svc.sync_rule_sets(&state, now_secs(), force) {
            Ok(report) => Response::ok(json!({
                "synced": true,
                "report": report_value(&report),
                "message": format!(
                    "规则集同步跑完：新落地 {}，沿用 {}，回收 {}，提示 {}",
                    report.refreshed.len(),
                    report.kept.len(),
                    report.reclaimed.len(),
                    report.notes.len()
                ),
            })),
            Err(gap @ SyncError::Gap { .. }) => Response::json(
                409,
                json!({
                    "error": gap.to_string(),
                    "report": report_value(gap.report()),
                }),
            ),
            Err(hard @ SyncError::Hard(_)) => Response::json(
                500,
                json!({
                    "error": hard.to_string(),
                    "report": report_value(hard.report()),
                }),
            ),
        }
    }

    /// `POST /api/hijack/enable`：装载 nft + 策略路由。**没有参数，也不下发配置**。
    ///
    /// 与 `zboxd enable-hijack` 唯一的差别就是那条会先 apply 一次：面板上"下发配置"和
    /// "开启劫持"是两个按钮，把会掐断所有连接的那一步藏在另一个按钮后面，正是 §5.4 点名的形状。
    /// 装载顺序本身仍是硬的——内核没在听就下 nft，局域网侧的表现是 `Connection refused`
    /// （schema §11.7），所以 [`Svc::enable_hijack`] 第一道门就是探测。
    ///
    /// 409 = 还没在听（先去按下发）；500 = 装载这一步坏了（`nft -c` 自证或 `netstate.sh`）。
    /// 这两类的判据与那句话都只在 `HijackGate` 里写了一遍。
    fn hijack_enable(&self, req: &Request<'_>) -> Response {
        if let Err(detail) = action_body(req, &[]) {
            return Response::fail(400, detail);
        }
        match self.svc.enable_hijack() {
            Ok(()) => Response::ok(json!({
                "enabled": true,
                "message": "劫持层已装载。挑一台把网关指到本机的设备试一下，DNS 答案该变了。",
            })),
            Err(gate @ HijackGate::NotListening(_)) => Response::fail(409, gate.to_string()),
            Err(gate) => Response::fail(500, gate.to_string()),
        }
    }

    /// `POST /api/hijack/disable`：撤掉劫持层。没有参数。
    ///
    /// `removed: false` 是**空操作而不是失败**：两路探针都明确答"没加载"时一条命令都不该跑
    /// （判据见 [`Svc::disable_hijack`]）。面板要把它显示成"本来就没开"，不能显示成"卸载成功"
    /// ——那是两件不同的事实，而在快照被裁掉的机器上把它们混成一句，就是那条对谁都不可读的
    /// `netstate-down.sh 执行失败`。
    ///
    /// sysctl 不会被这个动作改回去（`ip_forward`、`rp_filter` 是别的程序的公共前提），
    /// 这句话回执里必须带上，否则人会以为回滚把机器还原干净了。
    fn hijack_disable(&self, req: &Request<'_>) -> Response {
        if let Err(detail) = action_body(req, &[]) {
            return Response::fail(400, detail);
        }
        match self.svc.disable_hijack() {
            Ok(true) => Response::ok(json!({
                "removed": true,
                "message": "劫持层已卸载。sysctl 保持原值——脚本从来不动别人的键，回滚也不该顺手改回来。",
            })),
            Ok(false) => Response::ok(json!({
                "removed": false,
                "message": "现场没有 ZBox 的劫持层（nft 表与策略路由都不在），一条卸载命令都没发，只读了现场。",
            })),
            Err(detail) => Response::fail(500, detail),
        }
    }

    /// `POST /api/rollback`：回到某个修订；不给 `revision` 就回到已知良好那一份。
    ///
    /// **这一步会重启数据面**，所以它是显式的一个按钮。回执和 `apply` 同一个形状：只报
    /// "回到了哪一份、是怎么选出来的"，现场读数由面板再 `GET /api/status` 取一次——塞进回执
    /// 就等于承诺"读数与回执是同一瞬间的"，而那一刻服务正在重启。
    ///
    /// 400 = 给的那个修订号不存在，或一个可回滚的东西都没有（对面要去挑另一份）；
    /// 500 = 提交、重启、探测坏在其中一步（对面要去查数据面）。判据与那句"现有的（新到旧）：…"
    /// 都住在 [`Svc::rollback`]，CLI 打印的是同一句。
    fn rollback(&self, req: &Request<'_>) -> Response {
        let fields = match action_body(req, &["revision"]) {
            Ok(fields) => fields,
            Err(detail) => return Response::fail(400, detail),
        };
        let revision = match revision_of(&fields) {
            Ok(revision) => revision,
            Err(detail) => return Response::fail(400, detail),
        };
        match self.svc.rollback(revision) {
            Ok(got) => Response::ok(json!({
                "rolled_back": true,
                "revision": got.id,
                "how": got.how.word(),
                "message": format!("已回到{}修订 {}", got.how.word(), got.id),
                "last_good": self.last_good_id(),
            })),
            Err(RollbackError::NotFound(detail)) => Response::fail(400, detail),
            Err(RollbackError::Failed(detail)) => Response::fail(500, detail),
        }
    }

    /// `GET /api/status`：现场读数 + 修订历史 + 本机网卡形状。读数取不到的一律 `null`，
    /// 面板不许把 `null` 显示成"不符合期望"（`doctor.rs` 开头那条约定）。
    fn status(&self) -> Response {
        let store = self.svc.store();
        Response::ok(json!({
            "reading": self.svc.observe().read(),
            "last_good": store.last_good().map(|rev| rev.id),
            "revisions": store.list().into_iter().map(|rev| rev.id).collect::<Vec<_>>(),
            "network": network_probe(self.svc.runner()),
        }))
    }

    /// `GET /api/doctor`：期望状态 vs 现场。状态文件读不动时直接报错，
    /// 因为那一刻"诊断"给出的每一条都是猜的。
    fn doctor(&self) -> Response {
        let path = self.state_path();
        let (source, state) = if path.is_file() {
            match state::read(&path) {
                Ok(saved) => ("saved", saved),
                Err(detail) => return Response::fail(409, detail),
            }
        } else {
            ("baseline", DesiredState::baseline())
        };
        let reading = self.svc.observe().read();
        let mut findings = doctor::diagnose(&state, &reading, self.svc.layout());
        // 网卡形状这一条只在"磁盘那份还没人写过"时判（判据在 `doctor::network_shape_finding`）。
        // 探不出来就**不加这一条**：`ip` 在这台机器上跑不起来是"没读到"，不是"配错了"——
        // 拿它点亮一块 Windows 开发机的诊断面板，正是 `Reading` 字段全 `Option` 要防的那类假故障。
        if let Ok(probed) = Topology::detect(self.svc.runner()).verdict() {
            findings.extend(doctor::network_shape_finding(&state, &probed));
        }
        Response::ok(json!({
            "source": source,
            "findings": findings,
            "fatal": findings.iter().filter(|f| f.is_fatal()).count(),
            "warn": findings.iter().filter(|f| !f.is_fatal()).count(),
        }))
    }

    /// `GET /api/preview`：把磁盘上那份期望态渲染成**四份待下发的配置文本**，只读。
    ///
    /// 为什么要有这一格：`docs/BLUEPRINT.md` §5 那张映射表唯一的验收判据是"用户在面板上改一格，
    /// 配置里对应的那一行跟着变"，而这一句对一个不懂程序的人只有在一个前提下才可验证——
    /// 他能看见那份配置。所以下发之前先给看，而且给看的路径上一个字节都不写、一条命令都不发
    /// （那条边界由测试钉：预览之后 `runner.argv()` 仍然必须是空的）。
    ///
    /// **四份而不是两份**：`network.ipv6_mode` 那一档的改动实打实落在 `zbox.nft`（`table ip6`
    /// 那两条链）与 `netstate.sh`（`ip -6 rule/route` + 成对的 v6 sysctl）里。预览只给编排层那两份时，
    /// 用户把 `off` 改成 `dual_stack`、这一屏说"与上一页逐字相同"——那是半句假话，而且是判据那一句话的
    /// 反面。sing-box 的 `config.json` 照旧不给入口（2026-10-09 定位重定义里退役的那层）。
    ///
    /// 三份现场状态与 `GET /api/state` 同一套词：`missing`（还没保存过，渲染的是新装基线的形状）、
    /// `saved`、`broken`（读不进来，这时**四份都不渲染**，回执里给同一句原因）。
    /// 渲染失败不走 500：那一份渲染不出来是这一格的事实，不是这次请求的失败，
    /// 面板要能在同一个屏上显示"另一份是好的、这一份为什么不行"。
    fn preview(&self) -> Response {
        let etag = match self.etag() {
            Ok(etag) => etag,
            Err(detail) => return Response::fail(500, detail),
        };
        let path = self.state_path();
        let (source, state, error) = if !path.is_file() {
            ("missing", Some(DesiredState::baseline()), Value::Null)
        } else {
            match state::read(&path) {
                Ok(saved) => ("saved", Some(saved), Value::Null),
                Err(detail) => ("broken", None, json!(detail)),
            }
        };
        // 目标名是面板上那四个窗口的身份，改了名字就是面板上少了内容（见 assets 那条接线测试）
        let targets = match &state {
            Some(state) => preview_targets(state),
            None => {
                let detail = error
                    .as_str()
                    .unwrap_or("磁盘上那份状态读不进来")
                    .to_string();
                ["dae", "oxidns", "zbox.nft", "netstate.sh"]
                    .into_iter()
                    .map(|name| preview_target(name, Err(detail.clone())))
                    .collect()
            }
        };
        Response::ok(json!({
            "source": source,
            "error": error,
            "etag": etag,
            "targets": targets,
        }))
    }

    /// `GET /api/ladder`：把盘上那份期望态读成一张表——"现在什么流量怎么走，各在第几行"。只读。
    ///
    /// 为什么要有这一格：分流相关的格子散在四屏（分流策略与名单 / 设备例外 / 策略组 / 兜底出口），
    /// 改完十几格之后用户拿不到一句总结，而 `GET /api/preview` 给的是**配置文本**，对不懂程序的人
    /// 那不是同一句话。这一屏给的就是那句人话，每一行的措辞由 [`crate::gen::ladder`] 从同一份模型
    /// 派生；它跟两份渲染文本的逐行对应关系钉在 `gen::ladder` 的测试里，不由这里承诺。
    ///
    /// 三份词与 `GET /api/preview` 一致：`missing` / `saved` / `broken`。`broken` 时 `rows` 是
    /// **空数组** + 同一句读取失败的原因——空着一屏与坏着一屏在界面上必须是两句不同的话。
    /// `render_errors` 沿用 `GET /api/state` 的那一份（同一个 `render_failures`），因为"这一份现在
    /// 渲染不出来"时这张表说的是期望的形状而不是将要落地的字节；不并列进 `generation_error`，
    /// 那个字段讲的是下发读的那份快照。
    /// `reload_pending` / `generation_error` 也沿用同一套词与同一个 [`App::reload_signal`]，
    /// 所以这一屏与概览、下发那两屏不会给出两个口径；`missing` / `broken` 时下发没得谈，给 `null`。
    ///
    /// 回执里还要带 `last_good`：`reload_pending=true` 有**两种**来路（与已知良好那份不一致 /
    /// 这台机器一次都没成功下发过），面板只拿布尔值就会对后者说"与机器上那份已知良好的内容不一致"
    /// ——一台新装的机器一打开这一屏就收到一句指控它下发失败过的话。概览那一屏上面一行正好写着
    /// `last_good`，两句同屏自己就把话说清了；这一屏没有那一行，所以事实得跟着回执一起来。
    fn ladder(&self) -> Response {
        let etag = match self.etag() {
            Ok(etag) => etag,
            Err(detail) => return Response::fail(500, detail),
        };
        let path = self.state_path();
        let (source, state, error) = if !path.is_file() {
            ("missing", Some(DesiredState::baseline()), Value::Null)
        } else {
            match state::read(&path) {
                Ok(saved) => ("saved", Some(saved), Value::Null),
                Err(detail) => ("broken", None, json!(detail)),
            }
        };
        let (rows, render_errors, reload_pending, generation_error) = match &state {
            Some(state) => {
                // 只有"磁盘上真有一份"才谈得上要不要重载，与 `GET /api/state` 同一道门：
                // 基线那份还没存过，报 pending=true 会让新装的机器一打开面板就顶着黄色警告。
                let (pending, generation) = if source == "saved" {
                    self.reload_signal(state)
                } else {
                    (Value::Null, Value::Null)
                };
                (
                    json!(crate::gen::ladder::derive(state)),
                    render_failures(state),
                    pending,
                    generation,
                )
            }
            None => (
                json!([]),
                unrenderable_both(&error),
                Value::Null,
                Value::Null,
            ),
        };
        Response::ok(json!({
            "source": source,
            "error": error,
            "etag": etag,
            "rows": rows,
            "render_errors": render_errors,
            "reload_pending": reload_pending,
            "last_good": self.last_good_id(),
            "generation_error": generation_error,
        }))
    }

    /// 期望态那一侧的摘要：面板此刻这份期望态会渲染成哪份解析配置、它的 SHA-256 是什么。
    ///
    /// 从 [`Self::dns_expectation`] 里抽出来是给两处用的——「解析记录」屏和看板的「引擎在位」那一格
    /// 问的是**同一个数**（跑的那份加载的是哪个摘要），两处的算法必须是一条，否则同一台机器上
    /// 两个屏会说两句不一样的话。
    ///
    /// 渲染不出来时摘要是 `null` + 那句原因：面板不许把"渲染失败"显示成"跑的不是这一份"。
    /// `state` 是 `None` 时（磁盘上那份读不进来）两份都给 `null`——那句原因已经在回执的 `error` 里了。
    fn oxidns_wants(state: Option<&DesiredState>) -> (Value, Value) {
        let Some(state) = state else {
            return (Value::Null, Value::Null);
        };
        match crate::gen::oxidns::render(state) {
            Ok(text) => (json!(dns_api::sha256_hex(text.as_bytes())), Value::Null),
            Err(detail) => (Value::Null, json!(detail)),
        }
    }

    /// `GET /api/board`：看板。只读，而且**这一屏一条命令都不发**——硬件那几格从 `/proc` 的
    /// 普通文件读（[`crate::svc::board`]），取数口就是文件名本身，用户能拿 `free`、`top` 核对。
    ///
    /// 三件形状上的决定：
    ///
    /// - **取数时刻由面板后端盖**（`taken_at_epoch_ms`），不是浏览器 `new Date()`。这一屏是"按一次
    ///   读一次"的读数（§11.2 日志卡那条判定），而机器的时钟与浏览器所在机器的时钟可以不同；
    ///   写"这份读数是几点取的"就得写取数那一头的钟点。
    /// - **磁盘不给读数，只给一句为什么**（[`crate::svc::board::DISK_NOT_READ`]）。
    ///   §11.2 的许可是"`df -k` 或明写未读取，不猜"，这里选后者：留一个空着的位置并解释，
    ///   比画一个来源说不清的数字诚实。
    /// - **`lan_interface` 是从盘上那份期望态抄来的**，可能是这台机器上根本没有的名字
    ///   （U3 那条实测：新机器的默认 `ens33` 撞在真实 `eth0` 上）。所以它只是一句
    ///   "盘上写着局域网网卡是 X"，面板不许据此说这块网卡在不在、也不许把别的网卡标成局域网。
    ///
    /// `source` / `error` / `etag` 三份词与 `GET /api/preview`、`GET /api/ladder` 一致；
    /// 硬件读数与"盘上那份读没读到"**无关**，所以 `broken` 时硬件那几格照给。
    /// 这里没有 `render_errors`：这一屏不渲染任何东西，报的是读数。
    fn board(&self) -> Response {
        let etag = match self.etag() {
            Ok(etag) => etag,
            Err(detail) => return Response::fail(500, detail),
        };
        let path = self.state_path();
        let (source, state, error) = if !path.is_file() {
            ("missing", Some(DesiredState::baseline()), Value::Null)
        } else {
            match state::read(&path) {
                Ok(saved) => ("saved", Some(saved), Value::Null),
                Err(detail) => ("broken", None, json!(detail)),
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        // 解析层那四份聚合。管理口连不上是一种**读数**不是这一屏的失败：硬件那几格照给，
        // `dns.live:false` 带那一句原因（与 `POST /api/dns-records` 同一个约定）。
        let tops = match self.dns_observer() {
            Ok(got) => got.tops(),
            Err(detail) => dns_api::Tops {
                live: false,
                detail: Some(detail),
                ..dns_api::Tops::default()
            },
        };
        Response::ok(board_body(
            &crate::svc::board::readings(),
            now,
            source,
            error,
            &etag,
            state.as_ref().and_then(|s| {
                let name = s.network.lan_interface.as_str();
                (!name.is_empty()).then_some(name)
            }),
            &tops,
            state
                .as_ref()
                .map(|s| json!(s.dns.records_enabled))
                .unwrap_or(Value::Null),
            Self::oxidns_wants(state.as_ref()),
        ))
    }

    /// `GET /api/logs`：这台机器上**几个日志来源各自的末尾一截**。只读，无参数。
    ///
    /// 一次给全而不是每来源一条路由，为的是"五张卡说的是同一时刻"：分五次取，
    /// 刷新一次就可能出现"这张有那条新、那张还是上一条"的错位，而这一屏唯一在回答的
    /// 就是"刚刚发生了什么"。
    ///
    /// 三条形状上的决定都有理由，其中两条是被实测教出来的：
    /// - **落点全部取自 [`Layout`]，不在这里写死文件名**。"数据面是 sing-box 还是别人"
    ///   只动 `gen` 那几个常量。
    /// - **倒着读并封顶 [`LOG_TAIL_CAP`]，开头那条被切断的行丢掉**。封顶是从字节切的，切点
    ///   几乎必然落在某一行中间，也可能正好切在一个多字节汉字中间；连着切点的第一条因此
    ///   画出来就是半句乱码。所以宁可少给一行——`truncated` 说的正是"前面还有没给你的"。
    ///   例外只剩一种：整截里连一个换行都没有（最后一条行本身比封顶还长），那就把这一截
    ///   当作一条给回去，至少比空白页更接近真相。
    /// - **响应体里不回显路径**。§7.2 第 1 条：连 404 都不带出 `Layout::root`，一个能被
    ///   轮询的只读端点更不该。所以每个来源只给**文件名**（`sing-box.log`、
    ///   `oxidns.log.2026-10-02`），目录一律不外泄。
    ///
    /// `state` 六个值是这一格的正文，因为"这一张为什么是空的"有三种完全不同的真话：
    /// `missing` 是文件不在（新机器、或那台引擎没在这台机器上跑过），`journal` 是
    /// systemd 上 stderr 天生进 journal（**没有文件不是没在写**），`no_home` 是我们的配置
    /// 根本没给它落点那一节，`unreadable` 才是真的读失败。把它们混成一个"空"，
    /// 面板就会把"这台机器没跑过它"说成"它跑了但没话说"。
    fn logs(&self) -> Response {
        let layout = self.svc.layout();
        let state_dir = PathBuf::from(layout.state_dir());
        // stderr 那两条只在 OpenRC 上有文件：systemd 侧那个单元里刻意没把 stderr 落成文件，
        // 写进 journal 是那边的事实，不是这一屏没读到。
        let journal = self.svc.init() == InitSystem::Systemd;
        let sources = vec![
            self.one_log(
                "kernel",
                "数据面自己写的那一份",
                "按配置里那个日志那一节写，数据面进程自己开口。规则集抓不到、解析起不来这类原因写在这里。",
                LogHome::Fixed(PathBuf::from(layout.kernel_log_path())),
                false,
            ),
            self.one_log(
                "supervisor",
                "监督者替数据面接住的出错",
                "数据面还没读到配置就退掉的那一类（配置坏了、端口被占），只有这里留得下来。",
                LogHome::Fixed(PathBuf::from(layout.worker_stderr_path())),
                journal,
            ),
            self.one_log(
                "panel",
                "面板自己那份出错",
                "面板起不来的原因写在这里（网卡上没地址、端口被占、访问令牌读不动）。",
                LogHome::Fixed(PathBuf::from(layout.panel_stderr_path())),
                journal,
            ),
            // oxidns 那份按前缀取最新：它配的是 daily 轮转，轮转过一次之后盘上**没有**
            // 一个叫 `oxidns.log` 的文件（实测），拼死名字就永远读不到。
            self.one_log(
                "oxidns",
                "解析器自己写的那一份",
                "域名解析走到哪一步、哪条上游答了什么，记在这一份里。它按天轮转，所以这里取的是最新那一天那一份。",
                LogHome::Rotating {
                    dir: state_dir,
                    prefix: crate::gen::oxidns::LOG_FILE,
                },
                false,
            ),
            // dae 今天**没有文件落点**这一说：`gen::dae` 只写日志级别，往哪写归跑它的那一方管
            // （2026-10-12 guest 实测：现役状态目录里一个 dae 的日志文件都没有）。
            // 所以这张卡给的是"没有落点"，不是"落点在但没内容"——那句话差一个字就会变成假话。
            self.one_log(
                "dae",
                "接管层自己写的那一份",
                "分流命中了哪条规则、哪个连接被谁接管，记在这一份里。",
                LogHome::Missing,
                false,
            ),
        ];
        Response::ok(json!({ "sources": sources }))
    }

    /// [`Self::logs`] 那五个来源共用的一个读：分类、封顶、去头全在 [`read_log_tail`]，
    /// 所以五张卡对"这个为什么是空的"说的是同一套话。
    ///
    /// 三格身份（`id` / `title` / `who`）由调用处给，`read_log_tail` 只管文件——
    /// "这一张是谁的"与"这一张读到什么"不许在两个地方各写一遍。
    fn one_log(&self, id: &str, title: &str, who: &str, home: LogHome, journal: bool) -> Value {
        let mut card = match home {
            LogHome::Fixed(path) => read_log_tail(&path, journal),
            LogHome::Rotating { dir, prefix } => match newest_rotated_log(&dir, prefix) {
                Some(path) => read_log_tail(&path, false),
                // 轮转这一支没有"进运行记录"可言（它自己就往文件写），所以文件不在就是不在；
                // 卡上写的名字是那个缺省名，说的仍是"还没有这个文件"。
                None => log_card("missing", Some(prefix), Vec::new(), false, 0, Some(MISSING_NOTE)),
            },
            LogHome::Missing => {
                log_card("no_home", None, Vec::new(), false, 0, Some(NO_HOME_NOTE))
            }
        };
        card["id"] = json!(id);
        card["title"] = json!(title);
        card["who"] = json!(who);
        card
    }

    /// `POST /api/dns-records`：观测面「DNS 记录」那一屏的一页，外加解析器自报的状态。
    ///
    /// 为什么是 POST 而不是 GET：这一层的路由是**字面量表**，路径里没有任何一截是对面给的，
    /// 而查询串在协议层就被丢掉了（`http.rs` 那三条校验）。带参数的读因此只有一个容身处——体。
    /// 它不写任何东西：往机器上落一个字节都不做，四条上游请求全是 `GET`。
    ///
    /// **每一项单独降级**（`dns_api` 开头那条约定在这里落地）：连不上管理口是 `live: false`
    /// 加一句话，一条都读不到；连上了而 `stats` 读失败则只有 `stats` 是 `null` +
    /// `errors.stats` 那一句。200 不是"全都好"，是"这一屏的话说完了"——
    /// `GET /api/logs` 的 `missing` 是同一个道理。
    ///
    /// `running_version` 与 `wants_version` 是这一格最值钱的一对，而且是实测出来的：前者是
    /// oxidns 报告它**加载进内存**的那份配置的 SHA-256（改盘上文件不重载时它不动），后者是
    /// 面板此刻这份期望态渲染出来的那份的同一个摘要。两者不相等 = "面板上改的还没生效"，
    /// 这正是 `docs/BLUEPRINT.md` §5 那句判据能被一个不懂程序的人看见的形状。
    /// 所以这里既不外泄 `config_path`（§7.2：连 404 都不带 `Layout::root`），
    /// 也绝不转发 `/api/config`（那一条给的是配置**原文**，将来带主路由凭据）。
    fn dns_records(&self, req: &Request<'_>) -> Response {
        let fields = match action_body(req, &["qname", "client_ip", "status", "cursor", "limit"]) {
            Ok(fields) => fields,
            Err(detail) => return Response::fail(400, detail),
        };
        let filter = match dns_api::Filter::from_fields(&fields) {
            Ok(filter) => filter,
            Err(detail) => return Response::fail(400, detail),
        };
        let http = match self.dns_observer() {
            Ok(got) => got,
            Err(detail) => return Response::fail(500, detail),
        };
        let (source, wants, records_enabled, generate_error) = self.dns_expectation();
        let screen = http.screen(&filter);
        let mut errors = Map::new();
        for (key, detail) in &screen.errors {
            errors.insert(key.clone(), json!(detail));
        }
        if !screen.live {
            // 管理口连不通：这一屏只有那一句，`records` 是空的而不是"没有查询发生过"
            return Response::ok(json!({
                "live": false,
                "detail": screen.detail,
                "state_source": source,
                "records_enabled": records_enabled,
                "wants_version": wants,
                "generate_error": generate_error,
                "running_version": Value::Null,
                "applied": Value::Null,
                "records": [],
                "next_cursor": Value::Null,
                "stats": Value::Null,
                "errors": Value::Object(errors),
            }));
        }
        let running = dns_api::running_version(&screen.control).map(str::to_string);
        Response::ok(json!({
            "live": true,
            "detail": Value::Null,
            "version": screen.health.get("version"),
            "build_bundle": screen.health.get("build_bundle"),
            "uptime_ms": screen.health.get("uptime_ms"),
            "checks": screen.health.get("checks"),
            "plugins": screen.health.get("plugins"),
            "reload_status": screen.control.get("reload").and_then(|r| r.get("status")),
            "running_version": running.clone().map(|v| json!(v)).unwrap_or(Value::Null),
            // 说不清的时候是 null，不是 false：那一条"没读到"与"跑的不是这一份"是两件事
            "wants_version": wants.clone(),
            "generate_error": generate_error.clone(),
            "applied": match (&running, wants.as_str()) {
                (Some(got), Some(want)) => json!(got == want),
                _ => Value::Null,
            },
            "state_source": source,
            "records_enabled": records_enabled,
            "records": screen
                .page
                .get("records")
                .cloned()
                .unwrap_or_else(|| json!([])),
            "next_cursor": screen
                .page
                .get("next_cursor")
                .cloned()
                .unwrap_or(Value::Null),
            "stats": screen
                .stats
                .get("stats")
                .cloned()
                .unwrap_or(Value::Null),
            "errors": Value::Object(errors),
        }))
    }

    /// `POST /api/dns-record`：一条记录，**带 `steps`**。列表那一层没有这些（实测），
    /// 所以那一屏必须是两层：点开一条才去问"这条命中了链上哪几步"。
    fn dns_record(&self, req: &Request<'_>) -> Response {
        let fields = match action_body(req, &["id"]) {
            Ok(fields) => fields,
            Err(detail) => return Response::fail(400, detail),
        };
        let id = match fields.get("id") {
            Some(Value::Number(n)) => match n.as_u64() {
                Some(0) => {
                    return Response::fail(400, "id 要是记录的真实编号，从 1 起".to_string())
                }
                Some(got) => got,
                // 3.5、-1、大得装不进 u64：那是**类型**不对，不是"编号太小"，两句话得分开
                None => return Response::fail(400, format!("id 应是一个整数编号，收到 {n}")),
            },
            Some(other) => return Response::fail(400, format!("id 应是整数，收到 {other}")),
            None => return Response::fail(400, "少给了 id：这一格取的是单条记录".to_string()),
        };
        let observer = match self.dns_observer() {
            Ok(got) => got,
            Err(detail) => return Response::fail(500, detail),
        };
        match observer.record(id) {
            Ok(got) => Response::ok(json!({
                "found": true,
                "detail": Value::Null,
                "record": got.get("record"),
            })),
            Err(detail) => Response::ok(json!({
                "found": false,
                "detail": detail,
                "record": Value::Null,
            })),
        }
    }

    /// 期望态那一侧：面板此刻会生成哪份解析配置、它的摘要、以及"记下每一次查询"这一格开没开。
    ///
    /// 与 `GET /api/preview` 同一套词：`missing`（还没保存过，渲染的是新装基线的形状）、
    /// `saved`、`broken`。渲染不出来时摘要给 `null` + 那句原因——面板不能把"渲染失败"
    /// 显示成"跑的不是这一份"。
    fn dns_expectation(&self) -> (String, Value, Value, Value) {
        let path = self.state_path();
        let (source, state) = if !path.is_file() {
            ("missing".to_string(), Some(DesiredState::baseline()))
        } else {
            match state::read(&path) {
                Ok(saved) => ("saved".to_string(), Some(saved)),
                Err(detail) => {
                    return (
                        "broken".to_string(),
                        Value::Null,
                        Value::Null,
                        json!(detail),
                    )
                }
            }
        };
        let enabled = state
            .as_ref()
            .map(|s| json!(s.dns.records_enabled))
            .unwrap_or(Value::Null);
        let (wants, generate_error) = Self::oxidns_wants(state.as_ref());
        (source, wants, enabled, generate_error)
    }

    /// "存了但还没下发"的预览。判据和引擎用的是同一个（内容指纹 vs 已知良好快照），
    /// 但这里只是**预览**：真要不要重载由 `POST /api/apply` 的回执说了算——它返回
    /// `unchanged: true` 时一个字节都没动内核。
    ///
    /// 生成失败时给回 `null` + 原因（比如引用了还没同步的规则集）：那不是"不需要重载"，
    /// 面板不能把它显示成绿色。
    fn reload_signal(&self, state: &DesiredState) -> (Value, Value) {
        let files = match snapshot_files(state) {
            Ok(files) => files,
            Err(detail) => return (Value::Null, json!(detail.to_string())),
        };
        let Ok(digest) = store::content_hash(&files) else {
            return (Value::Null, json!("算不出内容指纹"));
        };
        match self.svc.store().last_good() {
            Some(good) => (
                json!(store::digest_of(&good.id).is_none_or(|current| current != digest)),
                Value::Null,
            ),
            // 一个已知良好快照都没有：要么没下发过，要么上次下发失败了
            None => (json!(true), Value::Null),
        }
    }
}

/// 把"可能是从文件中间开始的一截字节"切成行。`dropped_head` 为真时开头那条是被切断的，
/// 给出去只会让人以为数据面写了半句乱码，所以丢掉；整截里一个换行都没有时例外（那说明
/// 最后这一条行本身就比 [`LOG_TAIL_CAP`] 长，`truncated` 已经如实说了不完整）。
fn tail_lines(buf: &[u8], dropped_head: bool) -> Vec<String> {
    let text = String::from_utf8_lossy(buf);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if dropped_head && lines.len() > 1 {
        lines.remove(0);
    }
    // 以换行收尾的文件会被 split 多切出一个空串，那不是日志里的一条
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines.iter().map(|line| line.to_string()).collect()
}

/// 一个来源在这台机器上的**落点形状**。三种，因为"这一张为什么是空的"有三句不同的真话：
/// 名字定死了的那个文件不在（前三张卡）、按前缀找最新一份而一份都没找到（解析器那份，
/// 每天轮转）、我们的配置压根没给它落点（接管层那份）。
enum LogHome {
    Fixed(PathBuf),
    Rotating { dir: PathBuf, prefix: &'static str },
    Missing,
}

/// "这一份还没有文件"那句话。**`missing` 有两条来路**（路径定死了没文件、轮转后一个候选
/// 都没有），所以句子必须是同一个常量，不许在两处各写一遍再漂开。
const MISSING_NOTE: &str = "还没有这个文件——新装的机器，或者这一方从没在这台机器上跑过。";
const JOURNAL_NOTE: &str = "这一份在这台机器上走的是服务的运行记录（systemd 那个单元里没把它落成文件）。要去机器上翻那一条服务的记录，面板替不了。";
const NO_HOME_NOTE: &str = "这一份没有落点：ZBox 给它的配置里只写了日志级别，没写往哪个文件写。它现在归跑它的那一方管（主路由或人），面板读不到。";

/// 一个来源的末尾一截：分类、封顶、去头都在这一条函数里，五张卡因此对"空"这个字说同一套话。
///
/// 三条实测教出来的形状：
/// - **文件不在不是失败**。`missing` 是新机器或那一方从没起来过，`journal` 是 systemd 侧
///   stderr 天生进那份运行记录里（**没有文件不等于没在写**）——这两格都不许亮红。
/// - **只给文件名，绝不给路径**。§7.2 第 1 条：连 404 都不带出 `Layout::root`，而这一条
///   是被面板反复刷新的。
/// - **封顶从末尾往回数，切点那条丢一行**，理由见 [`tail_lines`]。
fn read_log_tail(path: &Path, journal: bool) -> Value {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(detail) if detail.kind() == std::io::ErrorKind::NotFound => {
            return if journal {
                log_card("journal", Some(&name), Vec::new(), false, 0, Some(JOURNAL_NOTE))
            } else {
                log_card("missing", Some(&name), Vec::new(), false, 0, Some(MISSING_NOTE))
            };
        }
        // 权限、把目录当文件读、盘坏了……这些都是真失败，但要说得清是哪一份读不动：
        // `to_string()` 出来的那一句里只有系统给的原因，没有路径。
        Err(detail) => {
            return log_card(
                "unreadable",
                Some(&name),
                Vec::new(),
                false,
                0,
                Some(&detail.to_string()),
            );
        }
    };
    let total = file.metadata().map(|meta| meta.len()).unwrap_or_default();
    let start = total.saturating_sub(LOG_TAIL_CAP);
    if start > 0 && std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start)).is_err() {
        return log_card(
            "unreadable",
            Some(&name),
            Vec::new(),
            false,
            total,
            Some("定位到末尾那一截时读不动"),
        );
    }
    let mut buf = Vec::new();
    let mut limited = std::io::Read::take(&mut file, total.saturating_sub(start));
    if let Err(detail) = std::io::Read::read_to_end(&mut limited, &mut buf) {
        return log_card(
            "unreadable",
            Some(&name),
            Vec::new(),
            start > 0,
            total,
            Some(&detail.to_string()),
        );
    }
    let lines = tail_lines(&buf, start > 0);
    let state = if lines.is_empty() { "empty" } else { "ok" };
    // 空文件与"这一截里一条完整行都没有"是同一种读数：文件在、也没读坏，只是没话说
    log_card(state, Some(&name), lines, start > 0, total, None)
}

/// 一张日志卡的形状，六个 `state` 值全从这里出，所以字段一个不会多、一个不会少
/// （面板不必猜"这一张没给 `cap` 是不是坏了"）。
///
/// `file` 是 `None` = 这一份**连文件名都没有**（没有落点那一支），画上去就该是空着而不是
/// 一个假名字。
fn log_card(
    state: &str,
    file: Option<&str>,
    lines: Vec<String>,
    truncated: bool,
    total_bytes: u64,
    note: Option<&str>,
) -> Value {
    json!({
        "state": state,
        "file": file.map(|file| Value::String(file.to_string())).unwrap_or(Value::Null),
        "lines": lines.into_iter().map(Value::String).collect::<Vec<_>>(),
        "truncated": truncated,
        "cap": LOG_TAIL_CAP,
        "total_bytes": total_bytes,
        "note": note.map(|note| Value::String(note.to_string())).unwrap_or(Value::Null),
    })
}

/// 解析器那份按**前缀**取盘上最新的一个：它配的是每天轮转，转过一次之后盘上就叫
/// `oxidns.log.<日期>` 而没有 `oxidns.log` 这个名字（实测），拼死名字等于永远读不到。
///
/// 先按修改时间、再按名字排——名字里那个日期后缀是字典序有序的，时间相同（同一天写过两下）
/// 时用它兜底。目录读不出来时给 `None`，那一格就落回 `missing`：这一屏问的是"有没有可看的"，
/// 不是"目录为什么坏了"。
fn newest_rotated_log(dir: &Path, prefix: &str) -> Option<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, String, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(prefix) {
                return None;
            }
            let modified = entry.metadata().and_then(|meta| meta.modified()).ok()?;
            Some((modified, name, entry.path()))
        })
        .collect();
    found.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    found.pop().map(|(_, _, path)| path)
}

/// `DesiredState` -> 磁盘形状（含 `schema` 键），面板拿到的就是这个。
fn disk_value(state: &DesiredState) -> Value {
    serde_json::to_value(state::StateFile::new(state.clone())).expect("纯数据，序列化不该失败")
}

/// 两份编排生成物（dae / oxidns）里**渲染失败**的那些，形状 `[{name, error}]`；空数组 = 两份都渲染出来了。
///
/// 为什么单开一个字段而不是并进 `generation_error`：后者说的是"重载要读的那份快照生成不出来"，
/// 而 `POST /api/apply` 消费的正是那一份。把编排生成物的失败并进去，面板那句"下发会被拒"
/// 就成了假话——dae 那一屏渲染不出来时，下发照样过得去。这一字段因此只描述事实：
/// 哪一份渲染不出来、为什么，让它自己那一屏（生成预览）说同样的话。
fn render_failures(state: &DesiredState) -> Value {
    let failures: Vec<Value> = [
        ("dae", crate::gen::dae::render(state)),
        ("oxidns", crate::gen::oxidns::render(state)),
    ]
    .into_iter()
    .filter_map(|(name, rendered)| match rendered {
        Ok(_) => None,
        Err(detail) => Some(json!({ "name": name, "error": detail })),
    })
    .collect();
    json!(failures)
}

/// 磁盘上那份读不进来时两份生成物的状态：两份都渲染不了，原因就是那句读取失败。
/// 和 `GET /api/preview` 在 `broken` 那一档的做法一致——空着一格与坏着一格是两句不同的话。
fn unrenderable_both(error: &Value) -> Value {
    let detail = error
        .as_str()
        .unwrap_or("磁盘上那份状态读不进来")
        .to_string();
    json!([
        { "name": "dae", "error": detail.clone() },
        { "name": "oxidns", "error": detail },
    ])
}

/// 看板那一屏的回执，**取数与拼字分开**：`GET /api/board` 负责去 `/proc` 取，这一层负责
/// 每个数单独降级、单位口径只在这一处格式化（`used_text` / `rx_text` 那几个是
/// [`crate::svc::board`] 里那两个函数给的，面板上不许再做一次算术——一处两套单位是"面板说的
/// 与机器上的对不上"最常见的来路）。
///
/// `taken_at_epoch_ms` 是**参数**不是 `SystemTime::now()`，为的是这一层能被钉住：
/// 那一句"这份读数是几点取的"一旦由浏览器自己的钟给，两台机器钟不同时就成了面板在说谎。
fn board_body(
    readings: &crate::svc::board::BoardReadings,
    taken_at_epoch_ms: u64,
    source: &str,
    error: Value,
    etag: &str,
    lan_interface: Option<&str>,
    tops: &dns_api::Tops,
    records_enabled: Value,
    wants: (Value, Value),
) -> Value {
    let text = |kb: Option<u64>| kb.map(|v| crate::svc::board::human_bytes(v.saturating_mul(1024)));
    let mut errors = Map::new();
    for (key, detail) in &tops.errors {
        errors.insert(key.clone(), json!(detail));
    }
    let (wants_version, generate_error) = wants;
    let running = dns_api::running_version(&tops.control);
    json!({
        "source": source,
        "error": error,
        "etag": etag,
        "taken_at_epoch_ms": taken_at_epoch_ms,
        "lan_interface": lan_interface,
        "memory": {
            "total_kb": readings.mem_total_kb,
            "available_kb": readings.mem_available_kb,
            "used_kb": readings.used_kb(),
            "used_percent": readings.used_percent(),
            "total_text": text(readings.mem_total_kb),
            "available_text": text(readings.mem_available_kb),
            "used_text": text(readings.used_kb()),
        },
        "load": readings.load,
        "uptime": {
            "secs": readings.uptime_secs,
            "text": readings.uptime_secs.map(crate::svc::board::human_duration),
        },
        // 网卡那几行只说"这块网卡过了多少字节"。§11.2 对这张卡的措辞判定就是这条：
        // 连接层（哪条连接走了代理）从 #69/G4 起给不出，这里不猜。
        "interfaces": readings.netdev.iter().map(|net| json!({
            "iface": net.iface,
            "rx_bytes": net.rx_bytes,
            "tx_bytes": net.tx_bytes,
            "rx_text": crate::svc::board::human_bytes(net.rx_bytes),
            "tx_text": crate::svc::board::human_bytes(net.tx_bytes),
        })).collect::<Vec<Value>>(),
        "disk": {
            "read": false,
            "reason": crate::svc::board::DISK_NOT_READ,
        },
        // 解析层那四份聚合（域名 / 客户端 / 应答码 / 执行器）。**每一格都带自己的 `sample_size`**，
        // 面板那句"最近 N 条里的前 20 名"因此只有 `dns.live` 为真时才成立。
        // 「哪条连接真走了代理」不在这里，也不许由日志反推（#69/G4），面板那一行写"给不出"。
        "dns": {
            "live": tops.live,
            "detail": tops.detail,
            "qnames": tops.qnames,
            "clients": tops.clients,
            "rcodes": tops.rcodes,
            "plugins": tops.plugins,
            "errors": Value::Object(errors),
            // 期望态那一格：记录开关关着时上面这几份**必然是空的**，面板不许把"空"讲成
            // "这台机器没人查询过"（§7.2 那条"不许把期望态当现场读数"在这里是反方向的）。
            "records_enabled": records_enabled,
        },
        // §11.3 G：三台引擎在不在、什么版本、跑的是不是盘上这一份。**这一格不发一条命令**——
        // 与这一屏其余部分同一条前提（它会被反复刷新）。
        //
        // 三行里只有 oxidns 那一行读得到数：它自己有一个只绑回环的管理口，而 `/api/health` 是
        // **进程活着才答**的口，所以"读到了"就是"在位"，不必再去问进程表。dae 与主路由两行
        // 各带一句为什么空（措辞只在 [`crate::svc::board`] 一处定义，面板抄，不另写一遍）。
        //
        // `applied` 是"跑的那份加载的是不是面板此刻要的那一份"，它**不是**那道门：这一屏不判
        // 放不放行，门在「让它生效」与「诊断」那两屏（§11.3 G 那条纪律）。说不清时给 `null`，
        // 不给 `false`——"没读到"与"不是这一份"是两件事。
        "engines": {
            "oxidns": {
                "live": tops.live,
                "detail": tops.detail.clone(),
                "version": tops.health.get("version"),
                "build_bundle": tops.health.get("build_bundle"),
                "running_version": running.map(|got| json!(got)).unwrap_or(Value::Null),
                "wants_version": wants_version.clone(),
                "generate_error": generate_error,
                "applied": match (running, wants_version.as_str()) {
                    (Some(got), Some(want)) => json!(got == want),
                    _ => Value::Null,
                },
            },
            "dae": {
                "read": false,
                "reason": crate::svc::board::DAE_NOT_READ,
            },
            "router": {
                "read": false,
                "reason": crate::svc::board::ROUTER_NOT_READ,
            },
        },
    })
}

/// `GET /api/status` 里那一份**本机此刻的网卡形状**——现场这一侧，与磁盘上写的期望态是两码事。
///
/// 为什么要它：`zboxd serve` 起在一台新机器的 root 上时，`GET /api/state` 给出的是磁盘那份
/// （还没有第一份时就是出厂默认的 `ens33` / `192.168.1.0/24`），而面板那几条读面一个进程都不起——
/// 探测只住在 CLI 那侧（`bin/zboxd.rs::fill_network`，填进内存、不落盘）。于是"新机器 → 导入订阅
/// → 让它生效"这条主路上，预览里的 dae 写着这台机器根本没有的网卡名，而屏幕上没有一个地方说得出
/// 这件事。**期望态走 state，现场走 status，两句话分开说**，这一格就是后面那一句。
///
/// 挂在 `status` 而不是 `state` 的理由是边界：这一层本来就要起进程（`observe` 读服务状态），
/// 而 `GET /api/state` 的界是"只读那一个文件、一个进程都不起"（那条界由 `api::tests` 钉着）。
///
/// 形状只走**公开的** `Topology` 字段（`verdict()` / `gateway` / `wan_interface`），
/// 不碰 `lan_candidates()` 那套内部判据——判哪张卡是局域网**只有一个定义**，
/// 这一层重数一遍就是"面板说的局域网网卡与 CLI 填的不是同一张"那种错。
///
/// 探不出来是一种状态不是故障：`ok:false` 带那一句原因，读到的半截（上游网卡、网关）照给，
/// 局域网那三格一律 `null`——面板不许把 `null` 画成"配错了"，更不许拿默认值顶上去。
fn network_probe(runner: &dyn Runner) -> Value {
    let topo = Topology::detect(runner);
    let gateway = topo
        .gateway
        .map(|gw| Value::from(gw.to_string()))
        .unwrap_or(Value::Null);
    let wan = topo
        .wan_interface
        .clone()
        .map(Value::from)
        .unwrap_or(Value::Null);
    match topo.verdict() {
        Ok(network) => json!({
            "ok": true,
            "lan_interface": network.lan_interface,
            "lan_cidr": network.lan_cidr,
            "lan_address": network.lan_address,
            "wan_interface": network.wan_interface,
            "gateway": gateway,
            "detail": Value::Null,
        }),
        Err(detail) => json!({
            "ok": false,
            "lan_interface": Value::Null,
            "lan_cidr": Value::Null,
            "lan_address": Value::Null,
            "wan_interface": wan,
            "gateway": gateway,
            "detail": detail,
        }),
    }
}

/// 那几格网卡在面板上的去处。三句回执共用一个名字，免得同一个屏在三个地方叫三种名字——
/// 名字本身取自 `panel.html`：侧栏标签是「网络 · 路线 · 端口」，段标题是『网卡与网段』，
/// 而它整层住在高级档里，所以"开高级模式"这几个字不能省。
const NETWORK_SCREEN: &str = "高级模式里的「网络 · 路线 · 端口」→『网卡与网段』";

/// 新机器上第一份状态落盘**之前**，把网卡那几格按本机填好；回执里那句说明是返回值。
///
/// 判据一条都不在这里新写：探测走 [`Topology::detect`]，"用不用探来的"走 CLI 那一个
/// [`topology::fill`]，两份形状（`zboxd` 与面板）共用一个定义。这一层只做三件事——
/// 只在磁盘还没有状态文件时调用、把结果写回 `state.network`、把结论翻成面板上的一句话。
///
/// 三种落点各有话（`topology::fill` 的返回形状正好是这三档）：
/// - 探到了：`network` 换成本机形状，回执给那三格读数；
/// - `ip` 压根跑不起来（Windows 开发机、chroot）：`fill` 走 `KeptExplicit` 并给了"沿用现值"那句，
///   照抄出来。**这一支不写盘上的新值**，出厂默认还在那儿，由诊断面板的
///   `network_still_default` 与概览「本机网卡」那一行接着说；
/// - 命令跑成了但形状判不出（几张卡都带段、或没有默认路由）：不猜，把 `verdict` 那句候选清单
///   原样带进回执，并明说下发前要去那一屏填对。
fn fill_first_network(runner: &dyn Runner, state: &mut DesiredState) -> Option<String> {
    match topology::fill(state, &Topology::detect(runner)) {
        Ok(filled) => {
            let note = match filled.how {
                topology::How::Detected => Some(format!(
                    "这是这台机器上的第一份状态，网卡那几格按本机探到的写上：局域网 {}\
                     （段 {}，本机地址 {}）、上游 {}——设备把 DNS 指到这个地址就行。\
                     要改这几格在{NETWORK_SCREEN}。",
                    filled.network.lan_interface,
                    filled.network.lan_cidr,
                    filled.network.lan_address,
                    filled.network.wan_interface
                )),
                // 探测降级那一支：fill 已经给了那句"沿用 state 里的值"，照抄当原因，再补一句
                // 这台机器上那是什么形状（新机器上就是出厂默认）以及去哪儿修。
                topology::How::KeptExplicit => Some(format!(
                    "这是这台机器上的第一份状态，但网卡那几格没能按本机填：{}。\
                     现在写下去的是局域网 {}/段 {}，下发前要在{NETWORK_SCREEN}里填对。",
                    filled
                        .warnings
                        .first()
                        .map(String::as_str)
                        .unwrap_or("没给出原因"),
                    filled.network.lan_interface,
                    filled.network.lan_cidr,
                )),
            };
            state.network = filled.network;
            note
        }
        Err(detail) => Some(format!(
            "这是这台机器上的第一份状态，但网卡形状判不出来：{detail}\
             网卡那几格现在用的还是出厂默认，下发前要在{NETWORK_SCREEN}里填对。"
        )),
    }
}

/// 一次预览给的四格，顺序固定（面板按名字找格子，测试按名字断言，都不靠下标）。
///
/// `zbox.nft` / `netstate.sh` 取的是 [`crate::gen::snapshot_files`] 用的**同一函数**，
/// 所以这一屏渲染出来的就是下一次下发要写进快照的那两份；名字也沿用快照里的键。
/// 这两份的渲染器不接受失败（`nft::render` / `netstate::render_apply` 返回 `String`），
/// 因此 `render_errors` 那个字段只可能点名 dae / oxidns 两份——那不是漏，是这两份本来就没有
/// "渲染不出来"这一说。
fn preview_targets(state: &DesiredState) -> Vec<Value> {
    vec![
        preview_target("dae", crate::gen::dae::render(state)),
        preview_target("oxidns", crate::gen::oxidns::render(state)),
        preview_target("zbox.nft", Ok(crate::gen::nft::render(state))),
        preview_target("netstate.sh", Ok(crate::gen::netstate::render_apply(state))),
    ]
}

/// 一份渲染结果 -> `GET /api/preview` 回执里的那一格。
///
/// 给**行数组**而不是一整块文本：面板要按行号标出"这一段是刚变的"，而服务端不该关心行宽与换行符。
/// 末尾那一个换行的空尾巴在这里就切掉——留着它，面板每份末尾多一个空行、差异区间还会偏一行。
/// 失败给的是 `{ok:false, error}` 而不是把这格删掉：面板要能说出"这一份为什么是空的"，
/// 少一格与坏一格在界面上必须是两句不同的话（少一格 = 服务端回执形状变了，那是另一回事）。
fn preview_target(name: &str, rendered: Result<String, String>) -> Value {
    match rendered {
        Ok(text) => {
            let body = text.strip_suffix('\n').unwrap_or(&text);
            json!({
                "name": name,
                "ok": true,
                "lines": body.split('\n').collect::<Vec<&str>>(),
            })
        }
        Err(detail) => json!({ "name": name, "ok": false, "error": detail }),
    }
}

/// 读动作端点（`ACTIONS` 那五个 POST）的参数。**体是唯一容身处**：路径与查询串都不许带参数
/// （见 [`Request::path`] 那条），而浏览器加载 `/` 时也带不上自定义头以外的东西。
///
/// 空体 = 一个参数都不给，各端点都有自己的默认值。非空体必须是 JSON 对象，
/// 而且**未知键一律 400**：名字写错的参数被静默忽略，在现场读起来就是"按了按钮没反应"，
/// 那正是分级回执要消掉的那类东西——它比报错难查得多。
fn action_body(req: &Request<'_>, allowed: &[&str]) -> Result<Map<String, Value>, String> {
    if req.body.len() > MAX_BODY {
        return Err(oversize_detail(req.body.len()));
    }
    if req.body.is_empty() {
        return Ok(Map::new());
    }
    let Ok(text) = std::str::from_utf8(req.body) else {
        return Err("请求体不是合法的 UTF-8".to_string());
    };
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("请求体不是合法的 JSON：{e}"))?;
    let Value::Object(fields) = value else {
        return Err("动作参数要是一个 JSON 对象，没有参数就给空对象或者干脆不给体".to_string());
    };
    if let Some(unknown) = fields.keys().find(|key| !allowed.contains(&key.as_str())) {
        let known = if allowed.is_empty() {
            "这个端点不收任何参数".to_string()
        } else {
            format!("这里只认 {}", allowed.join(" / "))
        };
        return Err(format!(
            "没有这个参数：{unknown}。{known}——写错的参数名会被静默忽略，那比报错难查"
        ));
    }
    Ok(fields)
}

/// 布尔参数：没给就是 `false`，给了必须是真布尔。`"true"` 这种字符串不算。
fn flag(fields: &Map<String, Value>, name: &str) -> Result<bool, String> {
    match fields.get(name) {
        None => Ok(false),
        Some(Value::Bool(given)) => Ok(*given),
        Some(other) => Err(format!("{name} 要的是 true 或 false，收到的是 {other}")),
    }
}

/// `revision`：没给、给 `null`、给的是空白，都是"回到已知良好那一份"。
///
/// 把空白也算"没给"是因为那个输入框留空就是这个意思；把它当成一个不存在的修订号，
/// 回执会去把现有修订列一遍——那是对着一个根本没填的框说话。
fn revision_of(fields: &Map<String, Value>) -> Result<Option<&str>, String> {
    match fields.get("revision") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(given)) => Ok(if given.trim().is_empty() {
            None
        } else {
            Some(given.as_str())
        }),
        Some(other) => Err(format!("revision 要的是一个修订号字符串，收到的是 {other}")),
    }
}

/// 订阅链接那一格：只收字符串。空串与不给都交给 [`subscribe::check_url`] 去说同一句话，
/// 这一层不复制"没填"那句——两处说同一件事迟早说得不一样。
fn url_of(fields: &Map<String, Value>) -> Result<String, String> {
    match fields.get("url") {
        None => Err("没有 url：这一屏要的是机场给的那一整串订阅链接".to_string()),
        Some(Value::String(given)) => Ok(given.clone()),
        Some(other) => Err(format!("url 要的是一串地址，收到的是 {other}")),
    }
}

/// 把订阅读出来的节点**追加**进这份状态：名字已经在的那条跳过（磁盘上原样不动，
/// 包括面板不拥有的那些键），新增的排在末尾。返回（新增的名字，已经在了的名字）。
///
/// 按名字认行就是 `nodes[]` 那一格编辑用的同一套身份规则——两条路共用一个身份，
/// 才不会发生"重新导入把人在面板上换过的密码顶回订阅里的旧值"。
fn append_nodes(state: &mut DesiredState, incoming: Vec<Node>) -> (Vec<String>, Vec<String>) {
    let mut added: Vec<String> = Vec::new();
    let mut already: Vec<String> = Vec::new();
    for node in incoming {
        if state.nodes.iter().any(|have| have.name == node.name) {
            already.push(node.name);
            continue;
        }
        added.push(node.name.clone());
        state.nodes.push(node);
    }
    (added, already)
}

/// 回执那一句人话。计数在这里拼而不是让面板拼，是因为"这一按有没有真写盘"只有服务端知道，
/// 而这一句就是用户接下来按哪个按钮的唯一依据。名单在 `added` / `already_there` 里另给。
fn import_message(
    format: &str,
    parsed: usize,
    skipped: usize,
    already: &[String],
    nothing_new: bool,
) -> String {
    let unread = if skipped == 0 {
        String::new()
    } else {
        format!("另有 {skipped} 条链接 ZBox 读不懂，已跳过。")
    };
    if nothing_new {
        return format!(
            "读到 {parsed} 个节点（{format}），磁盘上已经有 {} 个同名的，那一份一个字都没改。\
             想让这一批顶掉旧线路，先在「订阅与节点」那一屏把要换的那几条删掉再导入。{unread}",
            already.len()
        );
    }
    format!(
        "读到 {parsed} 个节点（{format}），其中 {} 个磁盘上已经有了；新增的那些已写进磁盘，\
         还没下发——数据面要等「让它生效」那一屏按下发才会走这些出口。{unread}",
        already.len()
    )
}

/// 「导入即把默认怎么走设成智能分流」那一句（§11.3 D：**带回执的自动，不是暗改**）。
///
/// 三件事按顺序写在同一句里：改成了什么、这一步**依赖**什么、以及"要改回来"的去处。
/// 中间那一条是这句里唯一会随机器变的：`cn_direct` 在 dae 那边落成的两行是
/// `domain(geosite:cn)` 与 `dip(geoip:cn)`，而这两份数据 ZBox 现在还不落地（#56 未落地）。
/// 实测的形状是缺数据时 `dae validate` 直接 `geoip.dat: file does not exist in [...]` rc=1
/// （`docs/reference/dae-config-dictionary.md` §5，2026-10-09 v2.1.1）——所以缺的时候必须明说，
/// 不能留一句"设好了"让人去撞那条对谁都不可读的错误。
///
/// 判据不在这里写：机器上有没有那两份数据是 [`geo_data_present`] 给的，这一层只把它翻成话。
fn smart_split_note(tag: &str, nodes: usize, geo_on_disk: bool) -> String {
    let data = if geo_on_disk {
        "这台机器上那份中国大陆数据（geoip.dat / geosite.dat）在位。".to_string()
    } else {
        format!(
            "但这一档要 dae 那份中国大陆数据（geoip.dat 与 geosite.dat），这台机器上还没探到——\
             ZBox 现在不会自己去下载它，下发前得先把这两份放进 dae 的数据目录同一个位置里，\
             否则数据面装载那一关就过不去。"
        )
    };
    let interval = SMART_SPLIT_INTERVAL;
    format!(
        "默认怎么走我也替你设成了智能分流：中国大陆的域名与 IP 直连，其余走 {tag}\
         （{nodes} 个出口，每 {interval} 测一次速自动挑一个）。{data}要改回来点这里。"
    )
}

/// 磁盘上已经有状态那一支：这一按**不动**「默认怎么走」，但兜底组是按名字列成员的死名单，
/// 新导入的名字不在里面的表现是"导入了、也设了智能分流，这几个出口却永远不走代理"。
/// 说出来（不静默把人编辑过的组成员顶掉），并点名去哪儿加。
fn group_membership_note(state: &DesiredState, added: &[String]) -> Option<String> {
    let FinalOutbound::Proxy(tag) = &state.policy.final_outbound else {
        return None;
    };
    let group = state.groups.iter().find(|group| &group.tag == tag)?;
    let missing: Vec<&str> = added
        .iter()
        .map(String::as_str)
        .filter(|name| !group.members.iter().any(|member| member == name))
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "默认怎么走没替你改（磁盘上那一版是你选过的），但新增的 {} 不在兜底组 {tag} 的成员里，\
         它们不会走代理——要在高级模式里的「策略组」把成员补上。",
        missing.join("、")
    ))
}

/// 同步报告序列化出去。给的是**逐条列表**而不是四个数字：一条 note（"抓取失败沿用旧文件"）
/// 和一条 kept 在界面上是两种颜色，面板拿计数画不出这两者。
fn report_value(report: &ruleset::Report) -> Value {
    json!({
        "refreshed": report.refreshed,
        "kept": report.kept,
        "local_missing": report.local_missing,
        "notes": report.notes,
        "reclaimed": report.reclaimed,
    })
}

fn verb(method: Method) -> &'static str {
    match method {
        Method::Get => "GET",
        Method::Put => "PUT",
        Method::Post => "POST",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::init::{InitSystem, Layout};
    use crate::ingest::subscription;
    use crate::model::config::{
        DomainAction, DomainPattern, DomainRule, Group, GroupKind, Network, RuleSet,
    };
    use crate::model::state::STATE_SCHEMA;
    use crate::svc::runner::{Exec, Runner, Scripted};
    use std::fs;

    /// 状态目录还没建的现场——面板第一次打开就是这个样子（2026-10-05 guest 实测）。
    fn fixture(name: &str) -> Layout {
        let dir = std::env::temp_dir().join(format!("zbox-api-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let root = dir.join("var-lib-zbox");
        Layout {
            root: root.display().to_string(),
            current_dir: root.join("current").display().to_string(),
            ..Layout::default()
        }
    }

    /// 四个入站都在听的 `ss -lntu`。表头那行会被解析器跳过（第 5 列不是 `地址:端口`），
    /// 这是实测的形状，不是随手写的样例。
    fn listening(state: &DesiredState) -> String {
        let p = &state.ports;
        format!(
            "State Recv-Q Send-Q Local Address:Port Peer Address:Port\n\
             udp UNCONN 0 0 0.0.0.0:{dns} 0.0.0.0:*\n\
             tcp LISTEN 0 128 0.0.0.0:{tproxy} 0.0.0.0:*\n\
             tcp LISTEN 0 128 0.0.0.0:{redirect} 0.0.0.0:*\n\
             tcp LISTEN 0 128 0.0.0.0:{mixed} 0.0.0.0:*\n",
            dns = p.dns,
            tproxy = p.tproxy,
            redirect = p.redirect,
            mixed = p.mixed,
        )
    }

    /// 一台"命令都能跑成、内核也在听"的机器。队列只剩一条时 `Scripted` 会把它当默认值反复用，
    /// 所以这一条同时喂给 check / restart / status / ss。
    fn green_runner(state: &DesiredState) -> Scripted {
        Scripted::with_results(vec![Ok(Exec {
            code: 0,
            stdout: listening(state),
            stderr: String::new(),
        })])
    }

    fn edited() -> DesiredState {
        let mut state = DesiredState::baseline();
        // 解析口要显式绑定地址（`gen::oxidns::dns_binds`）：这份夹具要能整份渲染，
        // 所以带着本机局域网 IPv4。RFC 保留段，不指向任何真实机器。
        state.network.lan_address = "192.0.2.10".into();
        state.policy.domain_rules = vec![DomainRule::new(
            DomainPattern::parse("+www.example.com").unwrap(),
            DomainAction::Direct,
        )];
        state
    }

    /// 固定的测试令牌。这里直接 `PanelToken::new`，不造文件——令牌的落盘形状与 0600
    /// 由 `token.rs` 自己的用例管，这一层只验"门有没有挡人"。
    const TOKEN: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

    fn token() -> PanelToken {
        PanelToken::new(TOKEN).expect("测试令牌形状")
    }

    /// 服务动词挑出来，好按顺序断言"下发到底跑了哪几步"。
    fn verbs(calls: &[Vec<String>]) -> Vec<String> {
        calls
            .iter()
            .map(|argv| {
                let program = argv.first().map(String::as_str).unwrap_or("");
                let index = if program == "rc-service" || program == "systemctl" {
                    2
                } else {
                    1
                };
                argv.get(index).cloned().unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn a_fresh_box_gets_the_baseline_and_does_not_claim_a_pending_reload() {
        let layout = fixture("fresh");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(&Request::get("/api/state").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        assert_eq!(body["source"], "missing", "新装的机器要能说\"没存过\"");
        assert_eq!(body["error"], Value::Null);
        assert_eq!(
            body["etag"], ABSENT_ETAG,
            "没有文件时 etag 是一个词，不是 null——null 分不清\"没文件\"和\"服务端没算\""
        );
        assert_eq!(body["state"]["schema"], STATE_SCHEMA);
        // 基线的 network 必须是默认形状，否则 §6 第 5 步的网卡探测会被一份没人在意的旧值挡掉
        assert_eq!(
            body["state"]["network"],
            disk_value(&DesiredState::baseline())["network"]
        );
        assert_eq!(body["last_good"], Value::Null);
        assert_eq!(body["reload_pending"], Value::Null);
        // 基线那一份里唯一渲染不出来的是 oxidns：本机局域网地址还没探（那一格空着，
        // 而解析口不许绑 0.0.0.0，见 `gen::oxidns::dns_binds`）。所以新机器一打开面板
        // 看到的是一条点名点得准的编排报错，而不是一份开在上游那一侧的配置——
        // 空数组说的是"查过，四份都好"，缺键说的是"服务端没查"，界面上是两句话。
        let errors = body["render_errors"]
            .as_array()
            .expect("render_errors 是数组");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0]["name"], json!("oxidns"), "{errors:?}");
        assert!(
            errors[0]["error"]
                .as_str()
                .unwrap_or_default()
                .contains("lan_address"),
            "要指到人能改的那一格：{errors:?}"
        );
    }

    #[test]
    fn saving_then_applying_are_two_calls_and_only_the_second_touches_the_machine() {
        let layout = fixture("save-apply");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let body = disk_value(&state).to_string();
        let saved = app.handle(
            &Request::put("/api/state", body.as_bytes())
                .authorized(TOKEN)
                .matching(ABSENT_ETAG),
        );
        assert_eq!(saved.status, 200, "{}", saved.body);
        let saved = saved.value();
        assert_eq!(saved["saved"], true);
        assert_eq!(
            saved["reload_pending"], true,
            "还没有已知良好快照，当然要重载"
        );
        assert_eq!(saved["warning"], RELOAD_WARNING);
        assert!(runner.argv().is_empty(), "保存这一步不许起任何进程");
        assert!(app.svc.store().list().is_empty(), "保存也不许写修订");
        assert!(PathBuf::from(layout.state_path()).is_file());

        let applied = app.handle(&Request::post("/api/apply").authorized(TOKEN));
        assert_eq!(applied.status, 200, "{}", applied.body);
        let applied = applied.value();
        assert_eq!(applied["reloaded"], true);
        assert_eq!(applied["unchanged"], Value::Null);
        let revision = applied["revision"]
            .as_str()
            .expect("{applied:?}")
            .to_string();
        assert_eq!(applied["last_good"], revision.as_str());
        assert_eq!(
            verbs(&runner.argv()),
            ["check", "run", "restart", "status", "-lntu"],
            "下发必须先过 check + 空 inbounds 预跑，再重启并探测（§11.6 那道门）"
        );

        // 面板重开一次：同一个状态，此时不该再喊"待重载"
        let again = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(again["source"], "saved");
        assert_eq!(again["reload_pending"], false);
        assert_eq!(again["last_good"], revision.as_str());
    }

    #[test]
    fn an_identical_reapply_says_unchanged_and_runs_nothing_new() {
        let layout = fixture("reapply");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let body = disk_value(&state).to_string();
        app.handle(
            &Request::put("/api/state", body.as_bytes())
                .authorized(TOKEN)
                .matching(ABSENT_ETAG),
        );
        let first = app
            .handle(&Request::post("/api/apply").authorized(TOKEN))
            .value();
        let calls = runner.argv().len();

        let second = app.handle(&Request::post("/api/apply").authorized(TOKEN));
        assert_eq!(second.status, 200, "{}", second.body);
        let second = second.value();
        assert_eq!(second["reloaded"], false);
        assert_eq!(second["unchanged"], true);
        assert_eq!(second["revision"], first["revision"]);
        assert!(
            second["message"]
                .as_str()
                .unwrap_or("")
                .contains("未触碰内核（不重启、不断流）"),
            "{second:?}"
        );
        assert_eq!(
            runner.argv().len(),
            calls,
            "内容一致时引擎要在任何进程之前返回（§5.4）"
        );
    }

    #[test]
    fn a_state_file_that_does_not_parse_is_broken_not_empty() {
        let layout = fixture("broken");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let path = PathBuf::from(layout.state_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"nodes\": 5}").unwrap();

        let got = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(got["source"], "broken");
        assert!(
            got["error"].as_str().unwrap_or("").contains("解析失败"),
            "{got:?}"
        );
        assert_eq!(got["state"], Value::Null, "坏了就不能假装有内容");

        // 下发与诊断都不许拿基线顶上这份坏文件
        let apply = app.handle(&Request::post("/api/apply").authorized(TOKEN));
        assert_eq!(apply.status, 409, "{}", apply.body);
        assert_eq!(
            app.handle(&Request::get("/api/doctor").authorized(TOKEN))
                .status,
            409
        );
    }

    #[test]
    fn a_v1_file_is_served_already_migrated() {
        let layout = fixture("v1");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        // 磁盘上真存在过的 v1 形状：三档名单、没有 schema 键、没有 domain_rules。
        // 名单里的每一项是 `DomainPattern` 的外部标签形状（`{"exact": ...}`），不是裸字符串——
        // 这正是 `.tmp/cli-state/state.json` 那种文件实际长什么样。
        let mut legacy = disk_value(&state);
        legacy.as_object_mut().unwrap().remove("schema");
        let policy = legacy["policy"].as_object_mut().unwrap();
        policy.remove("domain_rules");
        policy.insert(
            "blocked_domains".into(),
            json!([{"exact": "www.taobao.com"}]),
        );
        policy.insert("direct_domains".into(), json!([{"suffix": "www.qq.com"}]));
        let path = PathBuf::from(layout.state_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, legacy.to_string()).unwrap();

        let got = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(got["source"], "saved", "迁移成功不算坏：{got:?}");
        assert_eq!(got["state"]["schema"], STATE_SCHEMA);
        assert_eq!(got["state"]["policy"]["blocked_domains"], Value::Null);
        let rows = got["state"]["policy"]["domain_rules"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        // 迁移的档序 = v1 阶梯里的实际生效顺序（#18：拦截优先）
        assert_eq!(rows[0]["action"], "block");
        assert_eq!(rows[1]["action"], "direct");
    }

    #[test]
    fn an_unknown_endpoint_or_verb_never_touches_the_machine() {
        let layout = fixture("routes");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let gone = app.handle(&Request::get("/api/nope").authorized(TOKEN));
        assert_eq!(gone.status, 404, "{}", gone.body);
        // 关键的一条：路径永远不会被当文件读。这一句里连 "var-lib-zbox" 都不该出现。
        assert!(
            !gone.body.contains(&layout.root),
            "回显里不许带出状态目录：{}",
            gone.body
        );

        let wrong_verb = app.handle(&Request::get("/api/apply").authorized(TOKEN));
        assert_eq!(wrong_verb.status, 405, "{}", wrong_verb.body);
        assert!(wrong_verb.body.contains("POST"), "{}", wrong_verb.body);
        // 日志只读：POST 一样在路由表上撞回去，不许"顺手写一行日志"
        let logged_verb = app.handle(&Request::post("/api/logs").authorized(TOKEN));
        assert_eq!(logged_verb.status, 405, "{}", logged_verb.body);
        assert!(logged_verb.body.contains("GET"), "{}", logged_verb.body);
        assert!(runner.argv().is_empty(), "路由阶段不该动机器");
    }

    /// 五张卡按 `id` 找——面板按顺序画，测试认的却是身份而不是下标：换顺序是排版，
    /// 少一张卡才是断事。
    fn card<'a>(value: &'a Value, id: &str) -> &'a Value {
        value["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|one| one["id"] == json!(id))
            .unwrap_or_else(|| panic!("五张卡里没有 {id}：{value}"))
    }

    /// `GET /api/logs`：一次给回**五个来源各自的末尾一截**。这一屏是监控面（§7.1 的
    /// "查询日志"那一格）唯一的数据口，所以它的四条形状约束都在这一轮钉住：落点取自
    /// `Layout`、五张卡身份固定、措辞由后端单点给、读日志不动机器。
    #[test]
    fn the_log_endpoint_gives_the_tail_of_the_kernels_own_log() {
        let layout = fixture("logs-small");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let dir = PathBuf::from(layout.state_dir());
        fs::create_dir_all(&dir).unwrap();
        let body = "INFO[0000] sing-box started (0.03s)\nWARN[0001] fetch rule-set failed\n";
        fs::write(dir.join(crate::gen::singbox::LOG_FILE), body).unwrap();

        let got = app
            .handle(&Request::get("/api/logs").authorized(TOKEN))
            .value();
        let ids: Vec<&str> = got["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|one| one["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["kernel", "supervisor", "panel", "oxidns", "dae"],
            "一次给全就得有个固定的读法：{ids:?}"
        );

        let kernel = card(&got, "kernel");
        assert_eq!(kernel["state"], "ok", "{kernel}");
        // 只给文件名，目录不外泄
        assert_eq!(kernel["file"], "sing-box.log", "{kernel}");
        assert_eq!(kernel["truncated"], false, "{kernel}");
        let lines = kernel["lines"].as_array().unwrap();
        // 末尾那个换行不许变成第三条空行
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], "INFO[0000] sing-box started (0.03s)");
        assert_eq!(lines[1], "WARN[0001] fetch rule-set failed");
        assert_eq!(kernel["total_bytes"].as_u64().unwrap(), body.len() as u64);
        assert_eq!(kernel["cap"].as_u64().unwrap(), LOG_TAIL_CAP);
        // 标题与"这一份是谁写的"由后端单点给：HTML 里再抄一份就是第二处真相
        for id in ids {
            let one = card(&got, id);
            assert!(
                one["title"].as_str().is_some_and(|title| !title.is_empty()),
                "{id} 那张卡没给标题：{one}"
            );
            assert!(
                one["who"].as_str().is_some_and(|who| !who.is_empty()),
                "{id} 那张卡没给那句解释：{one}"
            );
        }
        assert!(runner.argv().is_empty(), "读日志不该动机器");
    }

    /// "还没写过日志"是一个**状态**，不是故障：新装的机器、或者内核从没起来过，面板要的
    /// 是一句"这里没有日志"，不是一个 5xx（那会被显示成"面板坏了"）。
    ///
    /// 这一支不是编出来的边角——2026-10-12 guest 实测的现役形状就是它：状态目录里只有
    /// `sing-box.log` 与 `sing-box.stderr.log` 两个文件，`panel.stderr.log` **从来没有过**。
    #[test]
    fn a_missing_log_is_a_state_not_a_fault_and_leaks_no_path() {
        let layout = fixture("logs-missing");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(&Request::get("/api/logs").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let value = got.value();
        let kernel = card(&value, "kernel");
        assert_eq!(kernel["state"], "missing", "{kernel}");
        assert_eq!(kernel["lines"].as_array().unwrap().len(), 0, "{kernel}");
        assert_eq!(kernel["truncated"], false, "{kernel}");
        assert_eq!(kernel["total_bytes"], 0, "{kernel}");
        assert_eq!(kernel["file"], "sing-box.log", "{kernel}");
        // "有落点但还没写"（其余四张）与"根本没有落点"（接管层）是两句不同的话。
        // 后者是 2026-10-12 实测推翻任务前提那一支：`gen::dae` 只写日志级别，不写往哪写。
        assert_eq!(card(&value, "dae")["state"], "no_home", "{value}");
        assert!(
            card(&value, "dae")["file"].is_null(),
            "没有落点那一支连文件名都不许编一个：{}",
            card(&value, "dae")
        );
        // 空目录读不成失败，而每一张非 `ok` 的卡都得自带那一句"为什么是空的"
        for one in value["sources"].as_array().unwrap() {
            let read = one["state"].as_str().unwrap();
            assert_ne!(read, "unreadable", "一个文件都没有不该读成失败：{one}");
            if read != "ok" {
                assert!(!one["note"].is_null(), "{read} 那一支没给原因：{one}");
            }
        }
        // §7.2 第 1 条：响应体不喷落点。这一条比 404 那句更要紧，因为它会被轮询。
        assert!(
            !got.body.contains(&layout.root),
            "回显里不许带出状态目录：{}",
            got.body
        );
    }

    /// 文件比封顶长：只给末尾那截，`truncated` 说"前面还有"，而且给出来的每一条都是整行。
    #[test]
    fn a_log_bigger_than_the_cap_returns_only_whole_lines_from_the_end() {
        let layout = fixture("logs-big");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        // 每条正好 52 字节（含换行），而 65536 % 52 = 16，所以切点必然落在某一条中间。
        let line = |i: usize| format!("line-{i:05}:{}", "x".repeat(40));
        let dir = PathBuf::from(layout.state_dir());
        fs::create_dir_all(&dir).unwrap();
        let written: Vec<String> = (0..1400).map(line).collect();
        fs::write(
            dir.join(crate::gen::singbox::LOG_FILE),
            format!("{}\n", written.join("\n")),
        )
        .unwrap();

        let value = app
            .handle(&Request::get("/api/logs").authorized(TOKEN))
            .value();
        let kernel = card(&value, "kernel");
        assert_eq!(kernel["state"], "ok", "{kernel}");
        assert_eq!(kernel["truncated"], true, "{kernel}");
        let lines = kernel["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1260, "末尾一截里该有整 1260 条");
        assert_eq!(lines[0], line(140), "开头那条被切断的必须丢掉");
        assert_eq!(lines.last().unwrap(), &Value::String(line(1399)));
        assert!(
            lines
                .iter()
                .all(|l| l.as_str().unwrap().starts_with("line-")),
            "不该有半行混进来"
        );
    }

    /// 解析器那份配的是**每天轮转**，转过一次之后盘上没有一个叫 `oxidns.log` 的文件
    /// （实测），所以那一格必须按前缀取最新的一份，不许拼死名字。
    #[test]
    fn the_oxidns_card_follows_daily_rotation_to_the_newest_file() {
        let layout = fixture("logs-rotated");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let dir = PathBuf::from(layout.state_dir());
        fs::create_dir_all(&dir).unwrap();
        // 先写昨天那一份、再写今天那一份：mtime 因此是单调的，"最新"才有唯一答案
        //（同名轮转文件在同一天里被写两下时，退到那个日期后缀的字典序，也是同一个结论）。
        fs::write(dir.join("oxidns.log.2026-10-01"), "昨天的那一条\n").unwrap();
        fs::write(dir.join("oxidns.log.2026-10-02"), "今天的那一条\n").unwrap();
        // 同一个目录里别人家的文件不许混进这一张卡
        fs::write(dir.join(crate::gen::singbox::LOG_FILE), "数据面的那一条\n").unwrap();

        let value = app
            .handle(&Request::get("/api/logs").authorized(TOKEN))
            .value();
        let oxidns = card(&value, "oxidns");
        assert_eq!(oxidns["state"], "ok", "{oxidns}");
        assert_eq!(oxidns["file"], "oxidns.log.2026-10-02", "{oxidns}");
        let lines = oxidns["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0], "今天的那一条");
        // 卡上给的是文件名：分隔符一个都不许有，目录一律不外泄
        let file = oxidns["file"].as_str().unwrap();
        assert!(!file.contains('/') && !file.contains('\\'), "{file}");
        // 而前缀过滤不是"把目录里所有文件都收进来"：数据面那张卡读的仍是自己那一份
        let kernel = card(&value, "kernel");
        assert_eq!(kernel["file"], "sing-box.log", "{kernel}");
        assert_eq!(kernel["lines"].as_array().unwrap().len(), 1, "{kernel}");
    }

    /// systemd 侧那两个 stderr **天生不落文件**（那个单元里刻意没有 `StandardError=`），
    /// 所以"文件不在"在那一侧是"这一份在服务的运行记录里"，不是"没在写"、更不是读失败。
    /// 同一个空目录在 OpenRC 上必须说 `missing`——两张脸靠的是 `Svc::init()` 那一个事实，
    /// 不是靠面板猜。
    #[test]
    fn stderr_on_systemd_reads_as_journal_not_as_missing() {
        let state = DesiredState::baseline();
        let token = token();
        for (init, want) in [
            (InitSystem::Systemd, "journal"),
            (InitSystem::OpenRC, "missing"),
        ] {
            let layout = fixture(&format!("logs-journal-{}", match init {
                InitSystem::Systemd => "systemd",
                InitSystem::OpenRC => "openrc",
            }));
            let runner = green_runner(&state);
            let svc = Svc::new(&layout, init, &runner, &state.ports);
            let app = App::new(&svc, &token);
            let value = app
                .handle(&Request::get("/api/logs").authorized(TOKEN))
                .value();
            for id in ["supervisor", "panel"] {
                let one = card(&value, id);
                assert_eq!(one["state"], want, "{init:?} 上的 {id}：{one}");
                assert!(
                    !one["note"].is_null(),
                    "{want} 那一支必须自带一句为什么：{one}"
                );
            }
            // 数据面自己那份走的是配置里的日志文件，两边都是"文件不在"，不许跟着变 journal
            let kernel = card(&value, "kernel");
            assert_eq!(kernel["state"], "missing", "{init:?} 上的 kernel：{kernel}");
        }
    }

    /// 真读不进去时才亮 `unreadable`，而且那句原因里不许带路径。
    /// 造这一支用的是最省事也最跨平台的办法：把落点做成一个**目录**——Windows 上 `open`
    /// 就给回拒绝，Linux 上撑到 `read` 才报"不是那种文件"，两条路都落到同一个分类。
    /// （权限位那种写法在 root 的 guest 上根本造不出来，所以不拿它当测法。）
    #[test]
    fn an_unreadable_source_says_which_one_and_leaks_no_path() {
        let layout = fixture("logs-unreadable");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let dir = PathBuf::from(layout.state_dir());
        fs::create_dir_all(dir.join(crate::gen::singbox::LOG_FILE)).unwrap();

        let got = app.handle(&Request::get("/api/logs").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let value = got.value();
        let kernel = card(&value, "kernel");
        assert_eq!(kernel["state"], "unreadable", "{kernel}");
        assert_eq!(kernel["file"], "sing-box.log", "要说清是哪一份读不动");
        assert!(
            kernel["lines"].as_array().unwrap().is_empty(),
            "读不动那一截不许有半行混进来：{kernel}"
        );
        // 这一条是这一支存在的理由：报错那句里没有落点
        assert!(
            !got.body.contains(&layout.root),
            "回显里不许带出状态目录：{}",
            got.body
        );
    }

    /// 六只字段每张卡都得有：面板画五张卡走的是同一段代码，少一格就是"这张卡看起来坏了"。
    #[test]
    fn every_log_card_carries_the_same_fields() {
        let layout = fixture("logs-shape");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let dir = PathBuf::from(layout.state_dir());
        fs::create_dir_all(&dir).unwrap();
        // 让四种读数在同一轮里各占一支：有内容的、空的、有落点没文件的、没有落点的
        fs::write(dir.join(crate::gen::singbox::LOG_FILE), "有内容的那一条\n").unwrap();
        fs::write(dir.join("panel.stderr.log"), "").unwrap();

        let value = app
            .handle(&Request::get("/api/logs").authorized(TOKEN))
            .value();
        let keys = [
            "id",
            "title",
            "who",
            "state",
            "file",
            "lines",
            "truncated",
            "cap",
            "total_bytes",
            "note",
        ];
        for one in value["sources"].as_array().unwrap() {
            let object = one.as_object().unwrap();
            for key in keys {
                assert!(object.contains_key(key), "少一格 {key}：{one}");
            }
            assert_eq!(object.len(), keys.len(), "多一格也是不一致：{one}");
            assert_eq!(one["cap"].as_u64().unwrap(), LOG_TAIL_CAP, "{one}");
        }
        assert_eq!(card(&value, "kernel")["state"], "ok", "{value}");
    }

    /// `tail_lines` 的三条边角，用真文件试太贵、用纯函数试太便宜，所以都钉在这里。
    #[test]
    fn tail_lines_drops_only_the_line_it_could_not_finish() {
        // 切点落在一个汉字中间（"第" 的三个字节里取第二个起）：那半个字符只可能出现在
        // 被丢掉的第一条里，于是 `from_utf8_lossy` 的替换符不会进到给面板看的任何一条上。
        let text = "第一条 中文\n第二条 世界\n";
        let chunk = &text.as_bytes()[4..];
        let lines = tail_lines(chunk, true);
        assert_eq!(lines, vec!["第二条 世界".to_string()], "{lines:?}");
        assert!(lines.iter().all(|l| !l.contains('\u{fffd}')), "{lines:?}");

        // 整截里一个换行都没有：最后这一条本身就比封顶长，给出来胜过给空白。
        assert_eq!(
            tail_lines(b"a fragment with no newline", true),
            vec!["a fragment with no newline".to_string()]
        );
        // 空文件 / 只有一条没写完的行。
        assert!(tail_lines(b"", false).is_empty());
        assert_eq!(tail_lines(b"one line", false), vec!["one line".to_string()]);
    }

    #[test]
    fn a_failed_apply_returns_the_engines_own_words_and_leaves_no_last_good() {
        let layout = fixture("failed");
        let state = edited();
        // 只有一条结果时 Scripted 拿它当默认值：这里让它每次都失败，模拟机器上没有二进制
        let runner = Scripted::with_results(vec![Err("open /usr/local/bin/sing-box: \
             No such file or directory (os error 2)"
            .into())]);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let body = disk_value(&state).to_string();
        app.handle(
            &Request::put("/api/state", body.as_bytes())
                .authorized(TOKEN)
                .matching(ABSENT_ETAG),
        );

        let failed = app.handle(&Request::post("/api/apply").authorized(TOKEN));
        assert_eq!(failed.status, 500, "{}", failed.body);
        let detail = failed.value()["error"].as_str().unwrap_or("").to_string();
        assert!(detail.contains("配置未通过校验"), "{detail}");
        assert!(
            app.svc.store().last_good().is_none(),
            "失败的修订不能成为已知良好"
        );
        // 修订目录确实写了（引擎在校验之前先 stage），面板因此能列出"这次失败的是哪一版"
        assert_eq!(app.svc.store().list().len(), 1);
    }

    #[test]
    fn an_impossible_or_huge_body_is_refused_before_it_reaches_disk() {
        let layout = fixture("refused");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let path = PathBuf::from(layout.state_path());

        // 版本门：比本程序新的状态一律拒收（旧版本写回会静默改坏用户配置）
        let mut newer = disk_value(&state);
        newer
            .as_object_mut()
            .unwrap()
            .insert("schema".into(), json!(99));
        let body = newer.to_string();
        let refused = app.handle(
            &Request::put("/api/state", body.as_bytes())
                .authorized(TOKEN)
                .matching(ABSENT_ETAG),
        );
        assert_eq!(refused.status, 400, "{}", refused.body);
        assert!(refused.body.contains("v99"), "{}", refused.body);

        // 校验器：不合法的 CIDR 由 `validate()` 顶出来
        let mut bad = disk_value(&state);
        bad["network"]
            .as_object_mut()
            .unwrap()
            .insert("lan_cidr".into(), json!("not-a-cidr"));
        let body = bad.to_string();
        let refused = app.handle(
            &Request::put("/api/state", body.as_bytes())
                .authorized(TOKEN)
                .matching(ABSENT_ETAG),
        );
        assert_eq!(refused.status, 400, "{}", refused.body);
        assert!(refused.body.contains("CIDR"), "{}", refused.body);

        // 一次列全在面板这条路也成立：三处各坏一点，这一发 400 要一次说完三条，
        // 而不是让用户"改一条、按一次保存、再撞出下一条"。
        let mut many = disk_value(&state);
        {
            let root = many.as_object_mut().unwrap();
            root["network"].as_object_mut().unwrap()["lan_cidr"] = json!("not-a-cidr");
            root["dns"].as_object_mut().unwrap()["records_retention_days"] = json!(0);
            root["devices"] = json!([{"ip": "tv.lan", "mode": "global"}]);
        }
        let body = many.to_string();
        let refused = app.handle(
            &Request::put("/api/state", body.as_bytes())
                .authorized(TOKEN)
                .matching(ABSENT_ETAG),
        );
        assert_eq!(refused.status, 400, "{}", refused.body);
        let detail = refused.value()["error"].as_str().unwrap_or("").to_string();
        assert!(detail.starts_with("这 3 处不成立，要改："), "{detail}");
        // §11.13 边界 1：这一发 400 的正文**自己不许带动作标签**。面板在那句前面已经有
        // "保存被拒（HTTP 400）："了，后端再顶一层"期望状态不成立:"就是两层标签叠在一句里，
        // 而屏幕上那一句当时真长这样：`保存被拒（HTTP 400）：期望状态不成立: 共 3 处：`。
        assert!(!detail.contains("期望状态不成立"), "{detail}");
        assert!(!detail.contains("保存被拒"), "{detail}");
        for expect in ["1）", "2）", "3）", "CIDR", "保留天数", "既不是 IPv4"] {
            assert!(detail.contains(expect), "这一发 400 里少了 {expect}：{detail}");
        }
        assert!(
            !path.exists(),
            "拒收之后磁盘上不该有任何一份状态：{}",
            path.display()
        );

        // 体积上限：不解析、不落盘。**故意不带 If-Match**——这条要验的正是"体量检查排在
        // 前提条件之前"，一个 1 MiB 的 body 不该先被要求补 header。
        let huge = vec![b' '; MAX_BODY + 1];
        let refused = app.handle(&Request::put("/api/state", &huge).authorized(TOKEN));
        assert_eq!(refused.status, 413, "{}", refused.body);
        assert!(refused.body.contains("不是分片"), "{}", refused.body);
        assert!(!path.exists(), "三种拒收都不许留下状态文件");
    }

    /// 门排在路由之前：没令牌时连"这个端点存不存在"都不该回答。
    ///
    /// 这条不只是洁癖——`GET /api/state` 的响应体里带着节点凭据，一个能被匿名探到的
    /// `/api/state` 等于把配置文件挂到 LAN 上；而 404/405 与 401 的区别会泄露端点表。
    #[test]
    fn without_the_token_not_even_the_routing_happens() {
        let layout = fixture("unauth");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let body = disk_value(&state).to_string();

        for req in [
            Request::get("/api/state"),
            Request::get("/api/status"),
            Request::get("/api/doctor"),
            Request::get("/api/ladder"),
            Request::get("/api/board"),
            Request::get("/api/logs"),
            Request::post("/api/apply"),
            // 只读端点也在门内：`/api/dns-records` 那两条回的是解析器的现场读数
            Request::post("/api/dns-records"),
            Request::post("/api/dns-record"),
            // 订阅导入也在门内：它读的是磁盘那份状态，还要往网络发一个真请求
            Request::post("/api/subscription"),
            Request::put("/api/state", body.as_bytes()),
            // 未知端点在没有令牌时也是 401，不是 404
            Request::get("/api/nope"),
        ] {
            let got = app.handle(&req);
            assert_eq!(got.status, 401, "{} {}", req.path, got.body);
            assert!(
                !got.body.contains(TOKEN),
                "401 的回执里不许出现真令牌：{}",
                got.body
            );
        }
        // 错令牌同拒；差一位也不行
        let mut off_by_one = TOKEN.as_bytes().to_vec();
        let last = off_by_one.len() - 1;
        off_by_one[last] = if off_by_one[last] == b'0' { b'1' } else { b'0' };
        let wrong = std::str::from_utf8(&off_by_one).unwrap();
        assert_eq!(
            app.handle(&Request::get("/api/state").authorized(wrong))
                .status,
            401
        );

        assert!(runner.argv().is_empty(), "鉴权失败不该动机器");
        assert!(
            !PathBuf::from(layout.state_path()).exists(),
            "鉴权失败的 PUT 不许落盘"
        );
    }

    /// 丢更新保护：两个标签页同开时，后写的要撞在 412 上，而不是安静地把前一份盖掉。
    #[test]
    fn a_stale_save_is_refused_and_shows_the_current_etag() {
        let layout = fixture("stale");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        // 第一个标签页：读到"没文件"，存下去，拿到磁盘那份的指纹
        let body = disk_value(&state).to_string();
        let saved = app
            .handle(
                &Request::put("/api/state", body.as_bytes())
                    .authorized(TOKEN)
                    .matching(ABSENT_ETAG),
            )
            .value();
        let first = saved["etag"].as_str().unwrap_or("").to_string();
        assert!(
            first != ABSENT_ETAG
                && first.len() == 16
                && first.chars().all(|c| c.is_ascii_hexdigit()),
            "etag 要能当 If-Match 用：{first}"
        );

        // 第二个标签页拿的还是那份旧的"没文件"：拒收，并把当前值回给它
        let stale = app.handle(
            &Request::put("/api/state", body.as_bytes())
                .authorized(TOKEN)
                .matching(ABSENT_ETAG),
        );
        assert_eq!(stale.status, 412, "{}", stale.body);
        assert_eq!(
            stale.value()["etag"],
            first.as_str(),
            "412 要把当前值带上，前端才知道该重新读哪一份"
        );

        // 干脆不带前提条件的，单独报一条错——不许悄悄当成"无条件通过"
        let no_precondition =
            app.handle(&Request::put("/api/state", body.as_bytes()).authorized(TOKEN));
        assert_eq!(no_precondition.status, 400, "{}", no_precondition.body);
        assert!(
            no_precondition.body.contains("If-Match"),
            "{}",
            no_precondition.body
        );

        // 磁盘上仍是第一份：412 那一次没写进去
        let read_back = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(read_back["etag"], first.as_str());
        assert_eq!(
            read_back["state"]["policy"]["domain_rules"],
            disk_value(&state)["policy"]["domain_rules"],
            "被拒的那次保存不该动磁盘上那份名单"
        );

        // 重新读到当前值再存，就过得去
        let mut other = edited();
        other.policy.domain_rules.push(DomainRule::new(
            DomainPattern::parse("+www.other.example").unwrap(),
            DomainAction::Direct,
        ));
        let second_body = disk_value(&other).to_string();
        let ok = app.handle(
            &Request::put("/api/state", second_body.as_bytes())
                .authorized(TOKEN)
                .matching(&first),
        );
        assert_eq!(ok.status, 200, "{}", ok.body);
        let second_etag = ok.value()["etag"].as_str().unwrap_or("").to_string();
        assert_ne!(second_etag, first, "换了内容就该换指纹");
        assert!(
            runner.argv().is_empty(),
            "三次保存（含被拒的两次）都不该动机器"
        );
    }

    /// 静态面排在鉴权**之前**：浏览器加载 `/` 的那一次请求带不上自定义头，这一页必须先出得来，
    /// 人才有地方填令牌。代价写在断言里——过门的三条路给的全是表里那份编译期字节，
    /// 没有一处内容来自磁盘、现场读数或请求本身。
    #[test]
    fn the_page_itself_is_served_before_the_token_gate() {
        let layout = fixture("static");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        for (path, kind) in [
            ("/", "text/html; charset=utf-8"),
            ("/panel.css", "text/css; charset=utf-8"),
            ("/panel.js", "text/javascript; charset=utf-8"),
        ] {
            let got = app.handle(&Request::get(path));
            assert_eq!(got.status, 200, "{path}: {}", got.body);
            assert_eq!(got.content_type, kind, "{path} 要给页面自己的类型");
            let asset = assets::lookup(path).expect("这三条都在表上");
            assert_eq!(got.body, asset.body, "{path} 要给表里那份字节");
        }
        // 门还在 API 前面：把静态面提前之后，唯一必须证明没被带走的东西就是这条
        assert_eq!(app.handle(&Request::get("/api/state")).status, 401);
        assert!(runner.argv().is_empty(), "读页面不该动机器");
    }

    /// 静态那三条只认 GET，而且这个 405 也在门外面——路径 `/` 人人都猜得到，
    /// "知道路径"在这里不构成信息。表外的写法不落在静态面，于是仍然走门。
    #[test]
    fn a_static_path_refuses_every_verb_but_get() {
        let layout = fixture("static-verb");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        for req in [Request::post("/"), Request::put("/panel.js", b"x")] {
            let got = app.handle(&req);
            assert_eq!(got.status, 405, "{} {}", req.path, got.body);
            assert!(got.body.contains("只接受 GET"), "{}", got.body);
            assert!(!got.body.contains(TOKEN), "405 的回执里不许出现真令牌");
        }

        // 表外的写法不落在静态面，于是仍然走门。协议层只挡绝对形式 / `*` / `://` / 反斜杠，
        // `..` 与 `//` 都走得进来，然后在这张常量表上不中——所以给的是 401（无令牌）而不是
        // "被拼进路径"。注意这里比的是**路由层**看到的字符串：真 socket 上 `?x` 早在
        // `parse_head` 就被剥掉了（那条剥之前先校验，见 http.rs 的同名测试），所以线上打
        // `/panel.css?v=1` 拿到的是样式表，只有直接构造 `Request` 才能把 `?` 送进这张表。
        for near in [
            "/panel.css?v=1",
            "/./panel.js",
            "//panel.css",
            "/../assets/panel.html",
            "/%2e%2e/panel.js",
        ] {
            assert_eq!(app.handle(&Request::get(near)).status, 401, "{near}");
            let behind_the_gate = app.handle(&Request::get(near).authorized(TOKEN));
            assert_eq!(
                behind_the_gate.status, 404,
                "{near}: {}",
                behind_the_gate.body
            );
            assert!(
                !behind_the_gate.body.contains(&layout.root),
                "404 的回执只复述路径，不许把落点喷出去：{}",
                behind_the_gate.body
            );
        }
    }

    /// 面板的 JS 里每个 `getElementById("x")` 都得在页面上真找得到 `id="x"`。
    /// CSP 禁内联脚本与内联事件，页面结构与行为因此是两份文件，靠字符串对上——
    /// 拼错一个 id 在浏览器里的表现是"那个控件永远不动"（`null` 上的赋值报错），
    /// 而不是构建失败，所以这条对齐必须由测试来管。
    #[test]
    fn every_id_the_panel_reaches_for_is_on_the_page() {
        let js = assets::lookup("/panel.js").expect("表上有这份").body;
        let page = assets::lookup("/").expect("表上有这份").body;
        let needle = "getElementById(\"";
        let mut seen: Vec<&str> = Vec::new();
        let mut rest = js;
        while let Some(at) = rest.find(needle) {
            let after = &rest[at + needle.len()..];
            let end = after
                .find('"')
                .unwrap_or_else(|| panic!("JS 里 getElementById( 的引号没关上：{after:?}"));
            let id = &after[..end];
            assert!(
                page.contains(&format!("id=\"{id}\"")),
                "面板 JS 取了 #{id}，但页面上没有这个 id"
            );
            seen.push(id);
            rest = &after[end + 1..];
        }
        assert!(
            seen.len() > 20,
            "一条都没扫到等于这条测试没在做事：{}",
            seen.len()
        );
        // 编辑层是那批新 id，少了任何一个就是表单静默失效
        for id in [
            "editor",
            "rule-list",
            "new-rule-pattern",
            "add-rule",
            "pol-final",
            "net-lan",
            "net-cidr6",
            "restore-autodetect",
            "save",
            "revert",
        ] {
            assert!(seen.contains(&id), "JS 里没有取 #{id}");
        }
        // DNS 与端口这两节的 id 是**从表里查出来的**（`PORT_INPUTS[key]`、
        // `UPSTREAM_LISTS[which][0]`），上面那个扫描器看不见动态拼出来的实参，
        // 所以这一节双向钉：JS 里提到过这个词，页面上也真有这个 id。
        for id in [
            "mode-switch-note",
            "domestic-list",
            "domestic-empty",
            "overseas-list",
            "overseas-empty",
            "add-domestic",
            "add-overseas",
            "dns-use-system-local",
            "dns-strategy",
            "net-ipv6-mode",
            "net-addr",
            "net-addr6",
            "dns-fakeip-enabled",
            "dns-fakeip-v4",
            "dns-fakeip-v6",
            "dns-records-enabled",
            "dns-records-retention",
            "port-dns",
            "port-tproxy",
            "port-redirect",
            "port-mixed",
            "port-socks",
            "ruleset-list",
            "ruleset-empty",
            "add-ruleset-local",
            "add-ruleset-remote",
            "device-lanes",
            "device-empty",
            "add-device",
            "group-list",
            "group-empty",
            "group-members-hint",
            "add-group-selector",
            "add-group-urltest",
            "node-list",
            "node-empty",
            "add-node-vless",
            "add-node-hysteria2",
            "add-node-tuic",
        ] {
            assert!(
                js.contains(&format!("\"{id}\"")),
                "JS 里再也没有 {id} 这个词，那一格控件是死的"
            );
            assert!(
                page.contains(&format!("id=\"{id}\"")),
                "页面上没有 #{id}，JS 拿着它只会撞到 null"
            );
        }
    }

    /// 面板那句"恢复自动探测"填的是**硬编码**的那几格——它不知道模型的默认值是什么，
    /// 只能抄一份。抄错一个字符的后果是静默的：`network` 仍不等于 `Network::default()`，
    /// `topology::fill` 就永远走 `How::KeptExplicit`，探测再也接不上手。
    /// 所以这一条从这里（模型的默认值）去核对那里（页面里的字面量）。
    #[test]
    fn the_panel_s_default_network_is_the_model_s() {
        let js = assets::lookup("/panel.js").expect("表上有这份").body;
        let net = crate::model::config::Network::default();
        for pair in [
            ("lan_interface", &net.lan_interface),
            ("lan_cidr", &net.lan_cidr),
            // 两个解析口地址也在"恢复自动探测"要填回去的名单里：留着值，探测就永远
            // 不再接管（同上一条理由），所以它们同样是"抄一份"的硬编码。
            ("lan_address", &net.lan_address),
            ("lan_address6", &net.lan_address6),
            ("wan_interface", &net.wan_interface),
        ] {
            assert!(
                js.contains(&format!("{}: \"{}\"", pair.0, pair.1)),
                "面板里 DEFAULT_NETWORK.{} 与模型默认值 {:?} 对不上",
                pair.0,
                pair.1
            );
        }
        // 路线那一格是 `恢复自动探测` 一起填回去的第四个值，同一条理由钉在这里
        assert_eq!(json!(net.route_mode), json!("inject"));
        assert!(
            js.contains("route_mode: \"inject\""),
            "面板里 DEFAULT_NETWORK.route_mode 与模型默认值 {:?} 对不上",
            net.route_mode
        );
        // 两张下拉写出去的那几个字符串必须是模型读的进的那几个（枚举的 serde 名）
        assert_eq!(
            json!(crate::model::config::ApplyPolicy::Restart),
            json!("restart")
        );
        assert_eq!(
            json!(crate::model::config::RouteMode::Takeover),
            json!("takeover")
        );
        assert!(
            js.contains("const ROUTE_MODES = [\"inject\", \"takeover\"];"),
            "路线那张下拉与模型枚举不同名，用户选了等于没选"
        );
        assert!(
            js.contains("const APPLY_POLICIES = [\"reload\", \"restart\"];"),
            "下发节奏那张下拉与模型枚举不同名，用户选了等于没选"
        );
        // IPv6 那一格同一套：面板字面量 == 模型默认值 == 枚举的 serde 名
        assert!(
            js.contains("ipv6_mode: \"off\""),
            "面板里 DEFAULT_NETWORK.ipv6_mode 与模型默认值 {:?} 对不上",
            net.ipv6_mode
        );
        assert!(
            js.contains("lan_cidr6: \"\""),
            "面板里 v6 前缀那栏的默认值要与模型的空串一致：{:?}",
            net.lan_cidr6
        );
        assert_eq!(json!(net.ipv6_mode), json!("off"));
        assert_eq!(
            json!(crate::model::config::Ipv6Mode::DnsOnly),
            json!("dns_only")
        );
        assert_eq!(
            json!(crate::model::config::Ipv6Mode::DualStack),
            json!("dual_stack")
        );
        assert!(
            js.contains("const IPV6_MODES = [\"off\", \"dns_only\", \"dual_stack\"];"),
            "IPv6 那张下拉与模型枚举不同名，用户选了等于没选"
        );
    }

    /// 面板 `collect()` 写出去的那份 JSON 必须正好是模型读得进来的形状——这是两边唯一的接缝。
    /// 名单那四条尤其要紧：`DomainPattern` 是四个同名键的枚举，写错键名就等于用户加的规则
    /// 静静消失；而 `domain_rules` 的**数组顺序就是引擎的命中顺序**，读回来还得逐条对得上。
    #[test]
    fn the_json_the_panel_writes_is_the_shape_the_model_reads() {
        let layout = fixture("panel-shape");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        // 底稿是 GET 给出去的那一份（含 schema），面板就是拿着它改的
        let read = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        let mut body = read["state"].clone();
        body["policy"]["private_direct"] = json!(false);
        body["policy"]["cn_direct"] = json!(true);
        body["policy"]["block_ads"] = json!(false);
        body["policy"]["bittorrent_direct"] = json!(false);
        body["policy"]["final_outbound"] = json!("direct");
        body["policy"]["domain_rules"] = json!([
            { "pattern": { "suffix": "example.com" }, "action": "direct" },
            { "pattern": { "exact": "a.test" }, "action": "proxy" },
            { "pattern": { "keyword": "kw" }, "action": "block" },
            { "pattern": { "regex": "^ad.*" }, "action": "block" },
        ]);
        body["network"] = json!({
            "lan_interface": "eth0",
            "lan_cidr": "192.168.219.0/24",
            "wan_interface": "eth0",
            // 面板的 collect() 现在会带这一格（路线那张下拉），它跟下面那条 apply_policy
            // 一起是"两张下拉写出去的词 = 模型读得进的词"这半条判据的实测形态
            "route_mode": "takeover",
        });
        body["apply_policy"] = json!("restart");

        let saved = app.handle(
            &Request::put("/api/state", body.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(read["etag"].as_str().expect("读回来的指纹是个词")),
        );
        assert_eq!(saved.status, 200, "{}", saved.body);
        assert_eq!(saved.value()["saved"], json!(true));
        let back = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        let rules = back["state"]["policy"]["domain_rules"]
            .as_array()
            .expect("名单读回来还得是数组");
        assert_eq!(rules.len(), 4, "少了就是键名对不上：{rules:?}");
        for pair in [
            (0, "suffix", "direct"),
            (1, "exact", "proxy"),
            (2, "keyword", "block"),
            (3, "regex", "block"),
        ] {
            let rule = &rules[pair.0];
            assert_eq!(
                rule["pattern"][pair.1].as_str().map(str::to_string),
                match pair.1 {
                    "suffix" => Some("example.com".to_string()),
                    "exact" => Some("a.test".to_string()),
                    "keyword" => Some("kw".to_string()),
                    _ => Some("^ad.*".to_string()),
                },
                "第 {} 条的模式写不回原样：{rule:?}",
                pair.0 + 1
            );
            assert_eq!(
                rule["action"],
                json!(pair.2),
                "第 {} 条的处置丢了",
                pair.0 + 1
            );
        }
        assert_eq!(back["state"]["policy"]["final_outbound"], json!("direct"));
        assert_eq!(back["state"]["network"]["lan_interface"], json!("eth0"));
        assert_eq!(back["state"]["policy"]["private_direct"], json!(false));
        // 两格新开关：面板发得出去、模型收得回来，且都落在磁盘形状里
        assert_eq!(back["state"]["network"]["route_mode"], json!("takeover"));
        assert_eq!(back["state"]["apply_policy"], json!("restart"));
        // 保存≠下发：表单写过一次之后，机器上仍然一次命令都没跑过
        assert!(
            runner.argv().is_empty(),
            "PUT 不该动机器：{:?}",
            runner.argv()
        );

        // 兜底出口指向一个不存在的组：这一条在**保存这一关**就被 validate 挡住，不用等生成。
        // （面板上那句提示原来写的是"生成阶段会拒"，被这一条实测推翻了。）
        let mut dangling = body.clone();
        dangling["policy"]["final_outbound"] = json!({ "proxy": "no-such-group" });
        let refused = app.handle(
            &Request::put("/api/state", dangling.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(back["etag"].as_str().unwrap()),
        );
        assert_eq!(refused.status, 400, "{}", refused.body);
        assert!(
            refused.body.contains("no-such-group"),
            "400 要说清是哪个 tag 不存在：{}",
            refused.body
        );
        let still = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(still["state"]["policy"]["final_outbound"], json!("direct"));
        assert_eq!(still["etag"], back["etag"], "被拒的那一次一个字节都没写");
    }

    /// 面板里那两张选择表是从模型抄来的（零依赖档里没有反射，抄是唯一的办法）。
    /// 这条管的是**改名**：`DoH3` 的 serde 名一旦变成 `do_h3`，面板上那条 doh3 上游
    /// 就成了"不认识的传输"、被原样带回，用户以为选了、磁盘上其实没有。
    #[test]
    fn the_panel_s_dns_choice_lists_are_the_model_s() {
        use crate::model::config::{DnsStrategy, UpstreamTransport};
        let js = assets::lookup("/panel.js").expect("表上有这份").body;
        assert!(js.contains("const TRANSPORTS ="), "TRANSPORTS 那张表不在了");
        assert!(js.contains("const DNS_MODES ="), "DNS_MODES 那张表不在了");
        for transport in [
            UpstreamTransport::Udp,
            UpstreamTransport::Tcp,
            UpstreamTransport::Dot,
            UpstreamTransport::DoH,
            UpstreamTransport::DoH3,
        ] {
            let name = serde_json::to_value(transport)
                .expect("枚举序列化成词")
                .as_str()
                .expect("序列化名是个词")
                .to_string();
            assert!(
                js.contains(&format!("\"{name}\"")),
                "面板的 TRANSPORTS 里少了 {name}（模型里有这一档传输）"
            );
        }
        for strategy in [
            DnsStrategy::PreferIpv4,
            DnsStrategy::Ipv4Only,
            DnsStrategy::Ipv6Only,
            DnsStrategy::PreferIpv6,
        ] {
            let value = serde_json::to_value(strategy).expect("策略序列化成一个带 mode 的对象");
            let mode = value["mode"].as_str().expect("mode 是个词").to_string();
            assert!(
                js.contains(&format!("\"{mode}\"")),
                "面板的 DNS_MODES 里少了 {mode}（模型里有这一档策略）"
            );
        }
    }

    /// 规则集与设备这两张表的选择项也是抄来的，而这里改名比 DNS 那两张更阴：
    /// `RuleSet` 的 tag 变体名一旦从 `local` 变成别的，每一条本地声明都会掉进"不认识的形状"，
    /// 面板只把它显示成 JSON、一格都改不动，而保存的 200 回执照样好看。
    /// 派生落点那一条更要紧：面板写死的 `RULE_SET_DIR` 必须是 `rule_set_product()` 用的那个目录，
    /// 差一个字符就等于每一条在线规则集都在保存这一关 400。
    #[test]
    fn the_panel_s_rule_set_and_device_choice_lists_are_the_model_s() {
        use crate::model::config::RuleSet;
        use crate::model::device::DeviceMode;
        let js = assets::lookup("/panel.js").expect("表上有这份").body;
        assert!(
            js.contains("const RULESET_SOURCES ="),
            "RULESET_SOURCES 那张表不在了"
        );
        assert!(
            js.contains("const DEVICE_MODES ="),
            "DEVICE_MODES 那张表不在了"
        );

        for (rule_set, name) in [
            (
                RuleSet::Local {
                    tag: "t".into(),
                    path: "rule-set/t.srs".into(),
                },
                "local",
            ),
            (
                RuleSet::Remote {
                    tag: "t".into(),
                    url: "https://example.test/t.srs".into(),
                    initial_path: "rule-set/t.srs".into(),
                    update_interval: "168h".into(),
                },
                "remote",
            ),
        ] {
            let value = serde_json::to_value(rule_set).expect("规则集序列化成对象");
            let source = value["source"].as_str().expect("source 是个词").to_string();
            assert_eq!(source, name, "模型那边的变体名改了，这里的对照要一起改");
            assert!(
                js.contains(&format!("\"{source}\"")),
                "面板的 RULESET_SOURCES 里少了 {source}"
            );
        }

        for mode in [
            DeviceMode::Global,
            DeviceMode::Direct,
            DeviceMode::Proxy,
            DeviceMode::Bypass,
        ] {
            let name = serde_json::to_value(mode)
                .expect("枚举序列化成词")
                .as_str()
                .expect("序列化名是个词")
                .to_string();
            assert!(
                js.contains(&format!("\"{name}\"")),
                "面板的 DEVICE_MODES 里少了 {name}（模型里有这一档处置）"
            );
        }

        // 派生落点：从模型算出一条真实产物路径，再核对面板写死的那个目录是它的前缀。
        let product = crate::model::config::rule_set_product("pin");
        let (dir, file) = product.split_once('/').expect("产物路径是 目录/文件名");
        assert_eq!(file, "pin.srs", "产物文件名不再是 <tag>.srs");
        assert!(
            js.contains(&format!("const RULE_SET_DIR = \"{dir}\"")),
            "面板的 RULE_SET_DIR 与模型的 {dir} 对不上，在线规则集会全部 400"
        );
    }

    /// 策略组那一节的选择表同样是抄来的，而这里改名比前两节更彻底：`GroupKind` 的变体名一旦
    /// 变了，每一条已有的组都会掉进"不认识的形状"，面板把它显示成 JSON、一格都改不动，
    /// 保存的 200 回执照样好看。
    /// 形状更要紧：`Group` 自己有个字段叫 `kind`，`GroupKind` 又拿 `kind` 当标签键，
    /// 于是磁盘上是**套一层**的 `{"kind":{"kind":"urltest",…}}`。面板照扁平形状写，
    /// 服务端只会回一句读不进——所以这里从模型的序列化结果反推面板里那几个字面量。
    #[test]
    fn the_panel_s_group_choice_lists_are_the_model_s() {
        use crate::model::config::{Group, GroupKind};
        let js = assets::lookup("/panel.js").expect("表上有这份").body;
        assert!(
            js.contains("const GROUP_KINDS ="),
            "GROUP_KINDS 那张表不在了"
        );

        let urltest = GroupKind::Urltest {
            url: "http://www.gstatic.com/generate_204".into(),
            interval: "3m".into(),
            tolerance: 50,
        };
        for kind in [GroupKind::Selector, urltest.clone()] {
            let value = serde_json::to_value(&kind).expect("组档位序列化成对象");
            let name = value["kind"].as_str().expect("kind 是个词").to_string();
            assert!(
                js.contains(&format!("\"{name}\"")),
                "面板的 GROUP_KINDS 里少了 {name}（模型里有这一档）"
            );
        }

        // 套一层由序列化器说了算，不由这里的人猜
        let group = Group {
            tag: "auto".into(),
            kind: urltest,
            members: vec!["direct".into()],
            default: None,
        };
        let text = serde_json::to_string(&group).expect("组序列化成词");
        assert!(
            text.contains("\"kind\":{\"kind\":\"urltest\""),
            "磁盘形状不再是套一层的 kind：{text}"
        );
        assert!(
            js.contains("kind: { kind: \"urltest\""),
            "面板 blankGroup 里的 urltest 形状与模型的嵌套写法对不上"
        );
        assert!(
            js.contains("kind: { kind: \"selector\" }"),
            "面板 blankGroup 里的 selector 形状与模型的嵌套写法对不上"
        );
    }

    /// 面板 `collect()` 交出去的那份 `groups` 必须正好是模型读得进来的**嵌套**形状，
    /// 而模型层那六道针对组的拦截，措辞要原样回到回执里——用户在面板上看得懂才有救。
    /// 这里断言的不是面板 JS 长什么样，而是**照面板那份 JSON 走一遍 PUT**：
    /// 上一轮实测就是这么发现 `kind` 得套一层的（照源码猜的扁平形状被 400 挡在门外）。
    #[test]
    fn the_group_json_the_panel_writes_is_the_shape_the_model_reads() {
        let layout = fixture("panel-groups");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let read = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        let mut body = read["state"].clone();
        // 逐字照 `groupRow` + `collectGroup` 交出去的形状：`members` 是数组、
        // `default` 空着是 null、urltest 的三格在 `kind` 里面。
        body["groups"] = json!([
            { "tag": "auto",
              "kind": { "kind": "urltest", "url": "http://www.gstatic.com/generate_204",
                        "interval": "3m", "tolerance": 50 },
              "members": ["direct"],
              "default": null },
            { "tag": "proxy",
              "kind": { "kind": "selector" },
              "members": ["auto", "direct"],
              "default": "auto" },
        ]);
        let saved = app.handle(
            &Request::put("/api/state", body.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(read["etag"].as_str().expect("读回来的指纹是个词")),
        );
        assert_eq!(saved.status, 200, "{}", saved.body);
        assert_eq!(saved.value()["saved"], json!(true));
        let back = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        let groups = back["state"]["groups"]
            .as_array()
            .expect("组读回来还得是数组");
        assert_eq!(groups.len(), 2, "少了就是键名对不上：{groups:?}");
        assert_eq!(groups[0]["kind"]["kind"], json!("urltest"));
        assert_eq!(
            groups[0]["kind"]["url"],
            json!("http://www.gstatic.com/generate_204"),
            "测速地址丢了：{}",
            groups[0]
        );
        assert_eq!(groups[0]["kind"]["interval"], json!("3m"));
        assert_eq!(
            groups[0]["kind"]["tolerance"],
            json!(50),
            "容差不是数字就是丢了"
        );
        assert_eq!(groups[0]["default"], json!(null), "urltest 不许带 default");
        assert_eq!(groups[0]["members"], json!(["direct"]));
        assert_eq!(
            groups[1]["kind"],
            json!({ "kind": "selector" }),
            "selector 那一档不该带别的键：{}",
            groups[1]
        );
        assert_eq!(groups[1]["members"], json!(["auto", "direct"]));
        assert_eq!(groups[1]["default"], json!("auto"));
        assert!(
            runner.argv().is_empty(),
            "PUT 不该动机器：{:?}",
            runner.argv()
        );

        // 六道拦截各来一次：措辞要指名，回执要指着被拒那一次没写盘
        let cases: Vec<(&str, Value)> = vec![
            (
                "缺少 tag",
                json!([{ "tag": "", "kind": { "kind": "selector" }, "members": ["direct"], "default": null }]),
            ),
            (
                "没有任何成员",
                json!([{ "tag": "empty", "kind": { "kind": "selector" }, "members": [], "default": null }]),
            ),
            (
                "引用了不存在的成员",
                json!([{ "tag": "proxy", "kind": { "kind": "selector" }, "members": ["没这个出站"], "default": null }]),
            ),
            (
                "没有默认成员这个概念",
                json!([{ "tag": "auto", "kind": { "kind": "urltest", "url": "http://www.gstatic.com/generate_204",
                        "interval": "3m", "tolerance": 50 }, "members": ["direct"], "default": "direct" }]),
            ),
            (
                "不在成员列表里",
                json!([{ "tag": "proxy", "kind": { "kind": "selector" }, "members": ["direct"], "default": "别的" }]),
            ),
            (
                "不是时长字符串",
                json!([{ "tag": "auto", "kind": { "kind": "urltest", "url": "http://www.gstatic.com/generate_204",
                        "interval": "180", "tolerance": 50 }, "members": ["direct"], "default": null }]),
            ),
        ];
        for (want, groups_body) in cases {
            let mut bad = body.clone();
            bad["groups"] = groups_body;
            let refused = app.handle(
                &Request::put("/api/state", bad.to_string().as_bytes())
                    .authorized(TOKEN)
                    .matching(back["etag"].as_str().unwrap()),
            );
            assert_eq!(
                refused.status, 400,
                "{want} 这一条该被拦住：{}",
                refused.body
            );
            assert!(
                refused.body.contains(want),
                "{want} 的措辞没回到回执里：{}",
                refused.body
            );
            let still = app
                .handle(&Request::get("/api/state").authorized(TOKEN))
                .value();
            assert_eq!(still["etag"], back["etag"], "被拒的那一次写了盘");
            assert_eq!(still["state"]["groups"], back["state"]["groups"]);
        }
    }

    /// 节点那一格的名单比策略组那节更长，也更经不起改名：外层 `kind.kind` 两档、
    /// 内层 `kind.protocol.protocol` 六档，两层里任一名对不上，已有的每一条出站都会掉进
    /// "不认识的形状"，那一行只画成 JSON、一格都改不动，而 200 回执照样好看。
    /// 凭据那两键更要紧：面板从不把磁盘值读进页面，靠的是**键路径**——路径写错，
    /// 换协议档时那一格就落在别处。所以这里从模型的序列化结果反推面板里的字面量，
    /// 不照面板源码抄。
    #[test]
    fn the_panel_s_node_choice_lists_are_the_model_s() {
        use crate::model::node::{NodeKind, QuicProtocol, StreamProtocol, Tls, Transport};
        let js = assets::lookup("/panel.js").expect("表上有这份").body;
        assert!(js.contains("const NODE_KINDS ="), "NODE_KINDS 那张表不在了");

        let stream = |protocol: StreamProtocol| {
            serde_json::to_value(NodeKind::Stream {
                protocol,
                transport: Transport::Tcp,
                tls: Tls::Disabled,
            })
            .expect("stream 节点序列化成对象")
        };
        let quic = |protocol: QuicProtocol| {
            serde_json::to_value(NodeKind::Quic { protocol }).expect("quic 节点序列化成对象")
        };
        let cases: Vec<Value> = vec![
            stream(StreamProtocol::Vless {
                uuid: "u".into(),
                flow: None,
            }),
            stream(StreamProtocol::Vmess {
                uuid: "u".into(),
                alter_id: "0".into(),
                security: "auto".into(),
            }),
            stream(StreamProtocol::Trojan {
                password: "p".into(),
            }),
            stream(StreamProtocol::Shadowsocks {
                method: "m".into(),
                password: "p".into(),
            }),
            quic(QuicProtocol::Hysteria2 {
                password: "p".into(),
                obfs: None,
                obfs_password: None,
                disable_path_mtu_discovery: false,
                up_mbps: None,
                down_mbps: None,
                tls: Tls::Disabled,
            }),
            quic(QuicProtocol::Tuic {
                uuid: "u".into(),
                password: "p".into(),
                alpn: vec![],
                congestion_control: None,
                udp_relay_mode: None,
                udp_over_stream: false,
                zero_rtt_handshake: false,
                tls: Tls::Disabled,
            }),
        ];

        // 外层两档、内层六档：名字一律问序列化器
        let mut outer: Vec<&str> = Vec::new();
        let mut inner: Vec<String> = Vec::new();
        let mut credentials = 0;
        for value in &cases {
            let kind = value["kind"].as_str().expect("外层标签是个词");
            assert!(
                ["stream", "quic"].contains(&kind),
                "模型里多了一档 {kind}，面板的 NODE_KINDS 得跟着加"
            );
            if !outer.contains(&kind) {
                outer.push(kind);
            }
            assert!(
                js.contains(&format!("\"{kind}\"")),
                "面板的 NODE_KINDS 里少了 {kind}"
            );
            let name = value["protocol"]["protocol"]
                .as_str()
                .expect("内层标签是个词")
                .to_string();
            assert!(!inner.contains(&name), "模型里 {name} 出现了两次");
            inner.push(name.clone());
            assert!(
                js.contains(&format!("\"{name}\"")),
                "面板的节点协议表里少了 {name}（模型里有这一档）"
            );
            // 凭据键路径：模型里这一档有哪几个字符串凭据，面板就得有哪几条路径
            for leaf in ["uuid", "password"] {
                if value["protocol"][leaf].is_string() {
                    credentials += 1;
                    assert!(
                        js.contains(&format!("[\"kind\", \"protocol\", \"{leaf}\"]")),
                        "面板没有 {name} 这一档 {leaf} 的键路径"
                    );
                }
            }
        }
        assert_eq!(inner.len(), 6, "六档协议都要过一遍");
        assert_eq!(outer.len(), 2, "外层不是 stream / quic 两档了");
        // 反向也要钉住：面板上凭据格的**条数**必须等于模型里凭据键的总数。
        // 模型给某一档新添一条凭据（或把某条改名）而格子没跟上，后果是那一格
        // 从界面上静默消失，用户再也改不了它——只查正向看不出来。
        let cells = js.match_indices("secret: true").count();
        assert_eq!(
            cells, credentials,
            "面板的凭据格有 {cells} 条，模型里的 uuid / password 键有 {credentials} 条"
        );
        // TLS 的落点分两层：stream 在 kind.tls，quic 在 kind.protocol.tls。
        // 面板那两格记的是相对路径，正因为这里两档不同——同一份 ["kind","tls"] 用到
        // hysteria2 上会写到一个根本不存在的位置。
        assert!(
            cases[0]["tls"].is_object()
                && cases[4]["tls"].is_null()
                && cases[4]["protocol"]["tls"].is_object(),
            "TLS 的层数变了：stream 在 kind.tls / quic 在 kind.protocol.tls 之外的位置"
        );
        for path in ["[\"kind\", \"tls\"]", "[\"kind\", \"protocol\", \"tls\"]"] {
            assert!(js.contains(path), "面板里没有 TLS 落点 {path} 这个字面量");
        }
        // reality 的两格挂在 TLS 下面
        assert!(
            js.contains("[\"reality\", \"public_key\"]")
                && js.contains("[\"reality\", \"short_id\"]"),
            "面板没有 Reality 公钥 / short id 的相对路径"
        );

        // 套两层由序列化器说了算，不由这里的人猜
        let node = crate::model::node::Node {
            name: "n".into(),
            server: "198.51.100.1".into(),
            port: 443,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Vless {
                    uuid: "u".into(),
                    flow: None,
                },
                transport: Transport::Tcp,
                tls: Tls::Disabled,
            },
        };
        let text = serde_json::to_string(&node).expect("节点序列化成词");
        assert!(
            text.contains("\"kind\":{\"kind\":\"stream\",\"protocol\":{\"protocol\":\"vless\""),
            "磁盘形状不再是套两层的 kind：{text}"
        );
        assert!(
            js.contains("kind: { kind: \"stream\"")
                || js.contains("kind: {\n      kind: \"stream\""),
            "面板 blankNode 的 vless 骨架与模型的嵌套写法对不上：外层得是 kind.kind"
        );
        // 骨架里那两样是"面板决定给你的默认值"，写错不会报错、只会静默改变行为
        assert!(
            js.contains("flow: \"xtls-rprx-vision\""),
            "面板新建 VLESS 行的骨架没带 flow"
        );
        assert!(
            js.contains("transport: { type: \"tcp\" }"),
            "面板新建行的 transport 不是模型里那个 Tcp 形状"
        );
    }

    /// 412 重放丢了哪几格，那句话必须活到**最后一条**回执里。上一轮在浏览器里量到的是
    /// 反过来的现场：重放那句先亮出来，紧跟着被成功或 400 的回执覆盖掉，用户只看见
    /// "已写入磁盘"，看不见"你敲的那一格其实没落盘"。这条钉的是拼装，不是措辞。
    /// 钉里那句措辞现在覆盖四张行表（节点、策略组、规则集、设备例外），因为计数是四张表
    /// 加起来的——只写"节点"的话，丢的是别人的策略组时页面会说谎。
    #[test]
    fn the_replay_warning_survives_whatever_message_comes_next() {
        let js = assets::lookup("/panel.js").expect("表上有这份").body;
        assert_eq!(
            js.match_indices("条记录（出站/策略组/规则集/设备例外）在刚读到的那一份里已经找不到")
                .count(),
            1,
            "丢条数那句话在 JS 里出现了好几处：拼装迟早会重一份或漏一份"
        );
        assert!(
            js.contains("function replaySentence()"),
            "那句话没有 replaySentence() 这一个出口"
        );
        let body = js
            .split("async function save(replay)")
            .nth(1)
            .and_then(|rest| rest.split("async function refresh()").next())
            .expect("JS 里找不到 save() 的函数体");
        for want in [
            "\"\\n另外：\" + replayWarning",
            "\" 另外：\" + replayWarning",
        ] {
            assert!(body.contains(want), "保存回执有一条没拼上重放警告：{want}");
        }
        assert!(
            body.contains("replayWarning = \"\""),
            "save() 开头没清掉上一次的重放警告，下一次无关的保存会替它背锅"
        );
        // 三条警告都得拼进最后那条消息、并把色调带成 warn：这一轮加的是编排渲染那一条，
        // 它要是只进 `setSaveNote` 的文本而不进色调判断，页面就写着红字却亮着绿灯。
        assert!(
            body.contains("+ broken"),
            "保存回执没拼上渲染失败那一句：只看见\"已写入磁盘\"的人不知道该去哪一屏"
        );
        assert!(
            body.contains("generation || broken || replayWarning ? \"warn\" : \"ok\""),
            "带着警告的成功回执还是绿色 ok"
        );
    }

    /// 策略组 / 规则集 / 设备例外这三张表从前是"整份接管"的：412 重放把表单开局那一份
    /// `clone()` 搬过去。第五期量到它两种**静默**的错法——别人新加的那一行被抹掉、
    /// 别人在同一行其它键上的改动被表单的旧值盖回去（节点那张表因为整份接管会把"表单
    /// 根本不认识的键"按旧值复活，才先改的）。这条钉的是"重放四张表都走按条目那一条路"，
    /// 钉法是出口函数名加那几处落点，不钉注释。
    #[test]
    fn the_four_row_tables_all_replay_per_item() {
        let js = assets::lookup("/panel.js").expect("表上有这份").body;
        let cut = |head: &str, tail: &str| -> String {
            js.split(head)
                .nth(1)
                .unwrap_or("")
                .split(tail)
                .next()
                .unwrap_or("")
                .to_string()
        };

        let body = cut(
            "function overlay(pending, fresh) {",
            "\nasync function putState",
        );
        for want in [
            "next.rule_sets = replayRuleSets(fresh.rule_sets)",
            "next.devices = replayDevices(fresh.devices)",
            "next.groups = replayGroups(fresh.groups)",
            "next.nodes = replayNodes(fresh.nodes)",
            "replayDropped = 0",
        ] {
            assert!(body.contains(want), "412 重放有一条不走按条目合并：{want}");
        }
        for gone in [
            "clone(pending.rule_sets)",
            "clone(pending.devices)",
            "clone(pending.groups)",
        ] {
            assert!(!js.contains(gone), "整份接管又回来了：{gone}");
        }

        // 每一行都得给出"身份"和"档位读数"：前者是 replayTable 配落点靠的键，
        // 后者决定别人换了这一行的档位时是合还是记成没落盘。缺一个就是整张表静默错位。
        for (head, base, shape) in [
            (
                "function rulesetRow(item, fresh) {",
                "li.original.tag",
                "li.shape = source;",
            ),
            (
                "function deviceRow(item, fresh) {",
                "li.original.ip",
                "li.shape = null;",
            ),
            (
                "function groupRow(item, fresh) {",
                "li.original.tag",
                "li.shape = kind;",
            ),
        ] {
            let row = cut(head, "\n}\n");
            assert!(!row.is_empty(), "JS 里找不到 {head}");
            assert!(
                row.contains("li.base =") && row.contains(base),
                "{head} 没给这一行的身份"
            );
            assert!(row.contains(shape), "{head} 没给这一行的档位读数");
        }

        // 收集那一步必须与重放共用同一份现场：PUT 正文是给人看的，entries 是 412 用的，
        // 分两处算迟早算歪。设备那一屏住在四栏里而不是一张平表上，所以它走自己的出口
        // `collectDeviceLanes`——但那一步照样把卡片交给同一个 `collectRows`，
        // 逐条合并要的现场（身份 / 改过的那几格 / 被去掉的那些）没换地方算。
        let collect = cut("function collect() {", "\n// 412 之后用");
        for want in [
            "collectTable(\"ruleset-list\", rulesetEdit)",
            "collectDeviceLanes(deviceEdit)",
            "collectTable(\"group-list\", groupEdit)",
        ] {
            assert!(
                collect.contains(want),
                "收集那一步没记下重放要的现场：{want}"
            );
        }
        let lanes = cut("function collectDeviceLanes(edit) {", "\n}\n");
        assert!(!lanes.is_empty(), "JS 里没有 collectDeviceLanes");
        assert!(
            lanes.contains("collectRows(rows, edit)"),
            "设备那四栏没有把卡片交给逐条合并那一步，收集会退回整份接管"
        );
    }

    /// 面板 `collectNode()` 交出去的那份 `nodes` 必须正好是模型读得进来的**嵌套**形状，
    /// 而"只写动过的那一格"这条合并契约要有服务端一侧的凭据：没画出来的那些键
    /// （transport 的 path/host/headers、alpn / insecure / fingerprint、obfs 与带宽、flow）
    /// 在一个来回之后必须原样还在。这里断言的不是面板 JS 长什么样，而是
    /// **照面板那份 JSON 走一遍 PUT**——上一轮就是这么发现 `kind` 得套两层的。
    #[test]
    fn the_node_json_the_panel_writes_is_the_shape_the_model_reads() {
        let layout = fixture("panel-nodes");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let read = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        let mut body = read["state"].clone();
        // 三条新建行逐字照 `blankNode()` 的骨架：只带画出来的那些键，
        // 其余（alpn / insecure / fingerprint / ech / obfs / 带宽 / 拥塞控制）整个不写，
        // 由 serde 的 default 补齐——"面板没画"不等于"这条节点不合法"。
        body["nodes"] = json!([
            { "name": "vless-1", "server": "198.51.100.10", "port": 443,
              "kind": { "kind": "stream",
                        "protocol": { "protocol": "vless",
                                      "uuid": "9a5cb1d0-1f2e-4c3d-9a8b-7c6d5e4f3a2b",
                                      "flow": "xtls-rprx-vision" },
                        "transport": { "type": "tcp" },
                        "tls": { "mode": "tls", "sni": null,
                                 "reality": { "public_key": "e6fWR7Cvs4AUijRLNmKxdo7zc3w7zcca0QdUv8YDp1w",
                                              "short_id": "0123456789abcdef" } } } },
            { "name": "hy2-1", "server": "198.51.100.20", "port": 8443,
              "kind": { "kind": "quic",
                        "protocol": { "protocol": "hysteria2", "password": "p",
                                      "tls": { "mode": "tls", "sni": null } } } },
            { "name": "tuic-1", "server": "198.51.100.30", "port": 10443,
              "kind": { "kind": "quic",
                        "protocol": { "protocol": "tuic", "uuid": "2c3d4e5f-6a7b-8c9d-0e1f-2a3b4c5d6e7f",
                                      "password": "q", "tls": { "mode": "tls", "sni": null } } } },
        ]);
        let saved = app.handle(
            &Request::put("/api/state", body.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(read["etag"].as_str().expect("读回来的指纹是个词")),
        );
        assert_eq!(saved.status, 200, "{}", saved.body);
        assert_eq!(saved.value()["saved"], json!(true));
        let back = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        let nodes = back["state"]["nodes"]
            .as_array()
            .expect("节点读回来还得是数组");
        assert_eq!(nodes.len(), 3, "少了就是键名对不上：{nodes:?}");
        assert_eq!(nodes[0]["kind"]["kind"], json!("stream"));
        assert_eq!(nodes[0]["kind"]["protocol"]["protocol"], json!("vless"));
        assert_eq!(
            nodes[0]["kind"]["protocol"]["flow"],
            json!("xtls-rprx-vision"),
            "骨架里那条 flow 丢了：Reality + Vision 节点丢掉它会让 0-RTT 分流退化"
        );
        assert_eq!(nodes[0]["kind"]["transport"]["type"], json!("tcp"));
        assert_eq!(
            nodes[0]["kind"]["tls"]["reality"]["public_key"],
            json!("e6fWR7Cvs4AUijRLNmKxdo7zc3w7zcca0QdUv8YDp1w")
        );
        assert_eq!(
            nodes[0]["kind"]["tls"]["alpn"],
            json!([]),
            "没写的键要由 default 补"
        );
        assert_eq!(nodes[0]["kind"]["tls"]["insecure"], json!(false));
        assert_eq!(
            nodes[1]["kind"]["protocol"]["tls"]["mode"],
            json!("tls"),
            "QUIC 的 TLS 落在 kind.protocol.tls，不是 kind.tls"
        );
        assert_eq!(nodes[1]["kind"]["protocol"]["obfs"], json!(null));

        // 现在按面板的合并契约改一条：只交动过的那三格（地址、端口、伪装域名），
        // 其余连读回来的原样一起带走。
        let mut patched = back["state"].clone();
        patched["nodes"][1]["server"] = json!("198.51.100.99");
        // `digitsOnly()` 交的是 Number，不是词：模型里 port 是 u16，
        // 把格子里那串字直接当字符串发出去，整份状态会以 400 挡在门外（下面单验一次）。
        patched["nodes"][1]["port"] = json!(6443);
        patched["nodes"][1]["kind"]["protocol"]["tls"]["sni"] = json!("hy.example.com");
        let ok = app.handle(
            &Request::put("/api/state", patched.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(back["etag"].as_str().unwrap()),
        );
        assert_eq!(ok.status, 200, "{}", ok.body);
        let after = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        let one = &after["state"]["nodes"][1];
        assert_eq!(one["server"], json!("198.51.100.99"));
        assert_eq!(one["port"], json!(6443), "改过的端口要落得下来");
        assert_eq!(
            one["kind"]["protocol"]["tls"]["sni"],
            json!("hy.example.com")
        );
        assert_eq!(
            one["kind"]["protocol"]["password"],
            json!("p"),
            "凭据格留空就是不改这一条——没交上去的键必须还在"
        );
        assert_eq!(after["state"]["nodes"][0]["name"], json!("vless-1"));
        assert_eq!(after["state"]["nodes"][2]["name"], json!("tuic-1"));

        // 再演一遍"有人直接编辑过 state.json"那条路：这一份里每一个**没画出来**的键
        // 都带着非默认值。面板式合并（读回来的整条 + 只动过的那一格）之后它们必须一个都不掉；
        // 若哪天建行改回从骨架重拼，这些键会静默退回 serde 的默认值，界面上看不出任何异常。
        let mut hand = after["state"].clone();
        hand["nodes"].as_array_mut().expect("节点是数组").push(
            json!({ "name": "vmess-ws", "server": "198.51.100.40", "port": 2087,
                          "kind": { "kind": "stream",
                                    "protocol": { "protocol": "vmess",
                                                  "uuid": "1b2d3e4f-5a6b-7c8d-9e0f-1a2b3c4d5e6f",
                                                  "alter_id": "1", "security": "aes-128-gcm" },
                                    "transport": { "type": "ws", "path": "/ray",
                                                   "host": "cdn.example.com",
                                                   "headers": [["User-Agent", "zbox-test"]] },
                                    "tls": { "mode": "tls", "sni": "cdn.example.com",
                                             "alpn": ["h2", "http/1.1"], "insecure": true,
                                             "fingerprint": "chrome",
                                             "reality": null, "ech": null } } }),
        );
        let added = app.handle(
            &Request::put("/api/state", hand.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(after["etag"].as_str().unwrap()),
        );
        assert_eq!(added.status, 200, "{}", added.body);
        let full = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(
            full["state"]["nodes"][3]["kind"]["transport"]["headers"],
            json!([["User-Agent", "zbox-test"]]),
            "先确认写进去的是那对键值数组：{}",
            full["state"]["nodes"][3]
        );
        // 只动两格：地址、伪装域名。其余七个键（alter_id / security / transport 的
        // path / host / headers、alpn / insecure / fingerprint）与端口一律没交。
        let mut only = full["state"].clone();
        only["nodes"][3]["server"] = json!("198.51.100.77");
        only["nodes"][3]["kind"]["tls"]["sni"] = json!("new.example.com");
        let merged = app.handle(
            &Request::put("/api/state", only.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(full["etag"].as_str().unwrap()),
        );
        assert_eq!(merged.status, 200, "{}", merged.body);
        let now = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        let kept = &now["state"]["nodes"][3];
        assert_eq!(kept["server"], json!("198.51.100.77"));
        assert_eq!(kept["kind"]["tls"]["sni"], json!("new.example.com"));
        assert_eq!(kept["port"], json!(2087), "没动过的那一格也要留在原处");
        assert_eq!(kept["kind"]["protocol"]["alter_id"], json!("1"));
        assert_eq!(kept["kind"]["protocol"]["security"], json!("aes-128-gcm"));
        assert_eq!(kept["kind"]["transport"]["path"], json!("/ray"));
        assert_eq!(kept["kind"]["transport"]["host"], json!("cdn.example.com"));
        assert_eq!(
            kept["kind"]["transport"]["headers"],
            json!([["User-Agent", "zbox-test"]]),
            "headers 是数组，逐下标合并会把它搅烂"
        );
        assert_eq!(kept["kind"]["tls"]["alpn"], json!(["h2", "http/1.1"]));
        assert_eq!(kept["kind"]["tls"]["insecure"], json!(true));
        assert_eq!(kept["kind"]["tls"]["fingerprint"], json!("chrome"));

        // 端口写成字符串要挡得住：面板那格交的是 Number，写错的代价是整份状态读不进
        let mut textual = kept.clone();
        textual["port"] = json!("2087");
        let mut bad_body = now["state"].clone();
        bad_body["nodes"][3] = textual;
        let refused = app.handle(
            &Request::put("/api/state", bad_body.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(now["etag"].as_str().unwrap()),
        );
        assert_eq!(
            refused.status, 400,
            "端口写成字符串该被挡住：{}",
            refused.body
        );
        assert!(
            refused.body.contains("expected u16"),
            "回执要说清是类型不对，不是只说读不进：{}",
            refused.body
        );
        assert!(
            runner.argv().is_empty(),
            "PUT 不该动机器：{:?}",
            runner.argv()
        );

        // 模型层那六道针对节点的拦截，措辞要原样回到回执里
        let cases: Vec<(&str, Value)> = vec![
            (
                "节点名不能为空",
                json!([{ "name": "", "server": "198.51.100.1", "port": 443,
                         "kind": { "kind": "quic",
                                   "protocol": { "protocol": "hysteria2", "password": "p" } } }]),
            ),
            (
                "缺少服务器地址",
                json!([{ "name": "n", "server": "", "port": 443,
                         "kind": { "kind": "quic",
                                   "protocol": { "protocol": "hysteria2", "password": "p" } } }]),
            ),
            (
                "端口非法",
                json!([{ "name": "n", "server": "198.51.100.1", "port": 0,
                         "kind": { "kind": "quic",
                                   "protocol": { "protocol": "hysteria2", "password": "p" } } }]),
            ),
            (
                "缺少 uuid",
                json!([{ "name": "n", "server": "198.51.100.1", "port": 443,
                         "kind": { "kind": "stream",
                                   "protocol": { "protocol": "vless", "uuid": "" },
                                   "transport": { "type": "tcp" },
                                   "tls": { "mode": "disabled" } } }]),
            ),
            (
                "出站 tag 重复",
                json!([
                    { "name": "dup", "server": "198.51.100.1", "port": 443,
                      "kind": { "kind": "quic",
                                "protocol": { "protocol": "hysteria2", "password": "p" } } },
                    { "name": "dup", "server": "198.51.100.2", "port": 443,
                      "kind": { "kind": "quic",
                                "protocol": { "protocol": "hysteria2", "password": "q" } } },
                ]),
            ),
            (
                "不是 sing-box 认可的取值",
                json!([{ "name": "n", "server": "198.51.100.1", "port": 443,
                         "kind": { "kind": "stream",
                                   "protocol": { "protocol": "vless", "uuid": "u",
                                                 "flow": "xtls-rprx-vision-udp999" },
                                   "transport": { "type": "tcp" },
                                   "tls": { "mode": "disabled" } } }]),
            ),
        ];
        // 六道拦截的对照物换成现场最新那一份（前面已经写过三次盘，旧指纹只会撞 412）
        let disk = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        for (want, node_body) in cases {
            let mut bad = disk["state"].clone();
            bad["nodes"] = node_body;
            let refused = app.handle(
                &Request::put("/api/state", bad.to_string().as_bytes())
                    .authorized(TOKEN)
                    .matching(disk["etag"].as_str().unwrap()),
            );
            assert_eq!(
                refused.status, 400,
                "{want} 这一条该被拦住：{}",
                refused.body
            );
            assert!(
                refused.body.contains(want),
                "{want} 的措辞没回到回执里：{}",
                refused.body
            );
            let still = app
                .handle(&Request::get("/api/state").authorized(TOKEN))
                .value();
            assert_eq!(still["etag"], disk["etag"], "被拒的那一次写了盘");
            assert_eq!(still["state"]["nodes"], disk["state"]["nodes"]);
        }
    }

    /// DNS 与端口这两节的接缝，比名单那节更窄：`via` 是**外部标签**枚举
    /// （`"direct"` 或 `{"outbound":"…"}`）、`strategy` 是 `{"mode":"…"}` 内部标签枚举，
    /// 键名写错就等于这一条上游/这一档策略消失，而 200 回执照样好看。
    /// 所以这里断言的不是"面板 JS 长什么样"，而是**面板交出去的那份 JSON 读回来还是它**。
    #[test]
    fn the_dns_and_ports_json_the_panel_writes_is_the_shape_the_model_reads() {
        let layout = fixture("panel-dns");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let read = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        let mut body = read["state"].clone();
        // 第一行是"用户只填了地址"那一格。新加的空白行由 `blankUpstream()` 把七个键填齐，
        // 所以交出去的是一条完整的上游（udp / via:"direct" / 其余 null），不是缺键的半条。
        body["dns"]["domestic"] = json!([
            { "tag": "主", "host": "223.5.5.5", "port": null, "transport": "udp",
              "path": null, "sni": null, "via": "direct" },
            { "tag": "备", "host": "dns.alidns.com", "port": 443, "transport": "doh",
              "path": "/dns-query", "sni": null, "via": { "outbound": "proxy" } },
        ]);
        body["dns"]["overseas"] = json!([
            { "tag": "境外", "host": "8.8.8.8", "port": null, "transport": "dot",
              "path": null, "sni": "dns.google", "via": { "outbound": "proxy" } },
        ]);
        body["dns"]["use_system_local"] = json!(false);
        body["dns"]["fakeip"] = json!({
            "enabled": true, "inet4_range": "198.18.0.0/15", "inet6_range": "fc00::/18",
        });
        body["dns"]["strategy"] = json!({ "mode": "ipv4_only" });
        body["ports"] = json!({
            "dns": 5353, "tproxy": 9888, "redirect": 9887, "mixed": 8888, "socks": null,
        });

        let saved = app.handle(
            &Request::put("/api/state", body.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(read["etag"].as_str().expect("指纹是个词")),
        );
        assert_eq!(saved.status, 200, "{}", saved.body);
        let back = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        let saved_state = &back["state"];
        assert_eq!(saved_state["dns"]["use_system_local"], json!(false));
        assert_eq!(
            saved_state["dns"]["strategy"],
            json!({ "mode": "ipv4_only" })
        );
        assert_eq!(saved_state["dns"]["fakeip"]["enabled"], json!(true));
        assert_eq!(
            saved_state["dns"]["fakeip"]["inet4_range"],
            json!("198.18.0.0/15")
        );
        assert_eq!(saved_state["ports"]["dns"], json!(5353));
        assert_eq!(
            saved_state["ports"]["socks"],
            Value::Null,
            "SOCKS 那一栏留空的意思就是不起这个入站"
        );
        let domestic = saved_state["dns"]["domestic"]
            .as_array()
            .expect("国内那组读回来还得是数组");
        assert_eq!(domestic.len(), 2, "少一条就是某个键名对不上：{domestic:?}");
        assert_eq!(domestic[0]["transport"], json!("udp"));
        assert_eq!(domestic[0]["port"], Value::Null);
        assert_eq!(domestic[0]["via"], json!("direct"), "外部标签的直连那一档");
        assert_eq!(domestic[1]["path"], json!("/dns-query"));
        assert_eq!(domestic[1]["via"], json!({ "outbound": "proxy" }));
        let overseas = saved_state["dns"]["overseas"]
            .as_array()
            .expect("境外那组读回来还得是数组");
        assert_eq!(overseas[0]["sni"], json!("dns.google"));
        assert_eq!(overseas[0]["transport"], json!("dot"));
        // 这一节改完，别的节一位都不许动（工作副本规矩在测试里的形式）
        assert_eq!(
            saved_state["policy"]["domain_rules"],
            read["state"]["policy"]["domain_rules"]
        );
        assert_eq!(saved_state["network"], read["state"]["network"]);
        assert!(
            runner.argv().is_empty(),
            "PUT 不该动机器：{:?}",
            runner.argv()
        );

        // 三条"表单不冒充内核"的账：勾了 fakeip 而段不是 CIDR、上游地址是域名却没人能先解析它、
        // 被拒之后磁盘一个字节没变。前两条都在保存这一关由 validate 给出可读的 400。
        let etag = back["etag"].as_str().expect("指纹是个词");
        let mut broken_cidr = body.clone();
        broken_cidr["dns"]["fakeip"]["inet4_range"] = json!("198.18.0.0");
        let refused = app.handle(
            &Request::put("/api/state", broken_cidr.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(etag),
        );
        assert_eq!(refused.status, 400, "{}", refused.body);
        assert!(
            refused.body.contains("CIDR"),
            "400 要说清是 CIDR 写法的事：{}",
            refused.body
        );

        let mut orphan = body.clone();
        orphan["dns"]["domestic"] = json!([
            { "tag": "只有域名", "host": "dns.example.com", "port": null, "transport": "doh",
              "path": "/dns-query", "sni": null, "via": "direct" },
        ]);
        orphan["dns"]["overseas"] = json!([]);
        let refused = app.handle(
            &Request::put("/api/state", orphan.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(etag),
        );
        assert_eq!(refused.status, 400, "{}", refused.body);
        assert!(
            refused.body.contains("dns.example.com"),
            "400 要点名是哪一条上游 bootstrap 不了：{}",
            refused.body
        );

        let still = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(still["etag"], json!(etag), "被拒的两次一个字节都没写");
        assert_eq!(still["state"]["ports"]["dns"], json!(5353));
        assert!(
            runner.argv().is_empty(),
            "被拒同样不该动机器：{:?}",
            runner.argv()
        );
    }

    /// 规则集与设备这两节的接缝比 DNS 更直白：两张平表，模型侧的每一个字段都画进了格子，
    /// 所以面板交出去的那份 JSON 必须整条读得回来——少一个键是 400，多一个键也是 400
    /// （两边都 `deny_unknown_fields`）。而"这一格没动"的合并逻辑一旦写错，后果是
    /// 用户改了设备模式、磁盘上却多出一条重复 IP 或干脆少了那台设备。
    #[test]
    fn the_rule_set_and_device_json_the_panel_writes_is_the_shape_the_model_reads() {
        let layout = fixture("panel-rulesets");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let read = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(
            read["state"]["rule_sets"]
                .as_array()
                .expect("空表读回来也该是数组")
                .len(),
            0,
            "底稿该是两张空表，这两节才有得验"
        );
        let mut body = read["state"].clone();
        // 这两行就是「加一个本地规则集」「加一个在线规则集」各自填完之后 collect() 交出的样子：
        // 本地那行只有 tag + path 两格，在线那行四格全在（落点由 tag 派生，界面不给填）。
        body["rule_sets"] = json!([
            { "source": "local", "tag": "geoip-my", "path": "rule-set/geoip-my.srs" },
            { "source": "remote", "tag": "geosite-cn", "url": "https://example.test/geosite-cn.srs",
              "initial_path": "rule-set/geosite-cn.srs", "update_interval": "168h" },
        ]);
        // 设备那四格：mac 与主机名留空就是 null（模型要看得见这两键），数组顺序即名单顺序。
        body["devices"] = json!([
            { "ip": "10.99.0.3", "mode": "bypass", "mac": null, "hostname": null },
            { "ip": "10.99.0.8", "mode": "direct", "mac": "aa:bb:cc:dd:ee:ff", "hostname": "tv" },
        ]);

        let saved = app.handle(
            &Request::put("/api/state", body.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(read["etag"].as_str().expect("指纹是个词")),
        );
        assert_eq!(saved.status, 200, "{}", saved.body);
        let back = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        let saved_state = &back["state"];
        let rule_sets = saved_state["rule_sets"]
            .as_array()
            .expect("规则集读回来还得是数组");
        assert_eq!(
            rule_sets.len(),
            2,
            "少一条就是某个键名对不上：{rule_sets:?}"
        );
        assert_eq!(rule_sets[0]["source"], json!("local"));
        assert_eq!(rule_sets[0]["path"], json!("rule-set/geoip-my.srs"));
        assert_eq!(rule_sets[1]["source"], json!("remote"));
        assert_eq!(rule_sets[1]["update_interval"], json!("168h"));
        assert_eq!(
            rule_sets[1]["url"],
            json!("https://example.test/geosite-cn.srs")
        );
        let devices = saved_state["devices"]
            .as_array()
            .expect("设备读回来还得是数组");
        assert_eq!(devices.len(), 2, "少一台就是某个键名对不上：{devices:?}");
        assert_eq!(devices[0]["mode"], json!("bypass"));
        assert_eq!(
            devices[0]["mac"],
            Value::Null,
            "留空的意思是没有，不是空字符串"
        );
        assert_eq!(devices[1]["hostname"], json!("tv"));
        assert_eq!(devices[1]["mac"], json!("aa:bb:cc:dd:ee:ff"));
        // 改这两节不许动别的节（工作副本规矩在测试里的形式）
        assert_eq!(
            saved_state["policy"]["domain_rules"],
            read["state"]["policy"]["domain_rules"]
        );
        assert_eq!(saved_state["network"], read["state"]["network"]);
        assert!(
            runner.argv().is_empty(),
            "PUT 不该动机器：{:?}",
            runner.argv()
        );

        // 三条 400 都是**保存这一关**给的，不会等到下发才由内核说。
        // 断言的是模型那句话里的关键词：tag 重复、tag 被当作文件名字符集拒、落点没按 tag 派生。
        let etag = back["etag"].as_str().expect("指纹是个词");
        for (label, rule_sets) in [
            (
                "重复",
                json!([
                    { "source": "local", "tag": "dup", "path": "rule-set/a.srs" },
                    { "source": "local", "tag": "dup", "path": "rule-set/b.srs" },
                ]),
            ),
            (
                "只允许",
                json!([
                    { "source": "local", "tag": "bad/../escape", "path": "rule-set/x.srs" },
                ]),
            ),
            (
                "initial_path 应为",
                json!([
                    { "source": "remote", "tag": "geosite-cn",
                      "url": "https://example.test/geosite-cn.srs",
                      "initial_path": "rule-set/other.srs", "update_interval": "168h" },
                ]),
            ),
        ] {
            let mut broken = body.clone();
            broken["rule_sets"] = rule_sets;
            let refused = app.handle(
                &Request::put("/api/state", broken.to_string().as_bytes())
                    .authorized(TOKEN)
                    .matching(etag),
            );
            assert_eq!(refused.status, 400, "{label}：{}", refused.body);
            assert!(
                refused.body.contains(label),
                "那一条 400 该说清是哪儿的事（关键词 {label}）：{}",
                refused.body
            );
        }

        // 强制代理而磁盘上没有任何代理组：这不是 400（那一份状态本身合法），
        // 而是 200 收下 + 生成器报错，回执里看得见。面板上面那句解释就是这么来的。
        let mut proxyless = body.clone();
        proxyless["devices"] =
            json!([ { "ip": "10.99.0.9", "mode": "proxy", "mac": null, "hostname": null } ]);
        let accepted = app.handle(
            &Request::put("/api/state", proxyless.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(etag),
        );
        assert_eq!(accepted.status, 200, "{}", accepted.body);
        let reported = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(
            reported["state"]["devices"][0]["mode"],
            json!("proxy"),
            "设备这一格要真落在磁盘上"
        );
        let error = reported["generation_error"].as_str().unwrap_or_default();
        assert!(
            error.contains("代理策略组"),
            "200 的回执要把生成器那句带出来：{error}"
        );
        assert_eq!(
            reported["state"]["rule_sets"], saved_state["rule_sets"],
            "设备那一改不该把规则集一起带走"
        );
        assert!(
            runner.argv().is_empty(),
            "这一路五次 PUT 一次机器都不该碰：{:?}",
            runner.argv()
        );
    }

    /// `GET /api/preview` 的三条判据，都是这一屏存在的理由本身：
    /// 四份都渲染出来、渲染不出来也要说清是哪一份、而且这一步一个字节都不写机器也不碰。
    #[test]
    fn the_preview_renders_all_four_targets_from_the_saved_state() {
        let layout = fixture("preview");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        put_state(&layout, &state);

        let got = app.handle(&Request::get("/api/preview").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let value = got.value();
        assert_eq!(value["source"], json!("saved"), "{value:?}");
        assert_eq!(
            value["etag"],
            app.handle(&Request::get("/api/state").authorized(TOKEN))
                .value()["etag"],
            "预览说的指纹必须和状态那一份是同一个"
        );
        let targets = value["targets"].as_array().expect("targets 是数组");
        assert_eq!(targets.len(), 4, "{targets:?}");
        let text = |name: &str| -> Vec<String> {
            let target = targets
                .iter()
                .find(|t| t["name"] == json!(name))
                .unwrap_or_else(|| panic!("少了 {name} 那一格"));
            assert_eq!(target["ok"], json!(true), "{target:?}");
            target["lines"]
                .as_array()
                .expect("行数组")
                .iter()
                .map(|l| l.as_str().expect("行是字符串").to_string())
                .collect()
        };
        let dae = text("dae");
        let oxidns = text("oxidns");
        let nft = text("zbox.nft");
        let netstate = text("netstate.sh");
        for lines in [&dae, &oxidns, &nft, &netstate] {
            assert!(!lines.is_empty(), "渲染出来是空的？");
            assert_ne!(
                lines.last().map(String::as_str),
                Some(""),
                "末尾那个换行的空尾巴要在服务端切掉，不然面板的差异偏一行"
            );
            // 行里不许再带换行：面板是按行画 div 的，带一个 \n 就是一行变两行
            assert!(lines.iter().all(|line| !line.contains('\n')), "{lines:?}");
        }
        assert!(
            dae.iter().any(|l| l.contains("fallback: direct")),
            "{dae:?}"
        );
        assert!(
            oxidns.iter().any(|l| l.contains("type: sequence")),
            "{oxidns:?}"
        );
        // 名单那一格在两份里都要看得见——这一条就是"改一格动一行"的判据形状
        assert!(
            dae.iter().any(|l| l.contains("domain(www.example.com)")),
            "{dae:?}"
        );
        assert!(
            oxidns.iter().any(|l| l.contains("domain:www.example.com")),
            "{oxidns:?}"
        );
        // 内核层那两份也在同一屏上：这一档（ipv6_mode = off）下 v6 的表一个字都不该出现
        assert!(nft.iter().any(|l| l.contains("table ip zbox {")), "{nft:?}");
        assert!(
            !nft.iter().any(|l| l.contains("table ip6 zbox {")),
            "off 档却渲染出 ip6 表：预览就把人骗进一条没写的接管路线"
        );
        assert!(
            netstate.iter().any(|l| l.contains("ip -4 rule add")),
            "{netstate:?}"
        );
        assert!(
            !netstate.iter().any(|l| l.contains("ip -6 rule add")),
            "{netstate:?}"
        );

        // 只读：一条命令都没发，磁盘上除了那份状态什么都没多
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
        let root = PathBuf::from(layout.root.clone());
        let entries: Vec<String> = fs::read_dir(&root)
            .unwrap_or_else(|e| panic!("读 {} 失败: {e}", root.display()))
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            entries,
            vec!["state.json".to_string()],
            "预览不许造快照目录"
        );
        assert!(
            !PathBuf::from(layout.config_path()).exists(),
            "预览不许写现役配置"
        );
    }

    /// 状态还没保存过的机器：预览给的是新装基线的形状，而且要明说是基线。
    #[test]
    fn the_preview_on_a_fresh_machine_shows_the_baseline_shape() {
        let layout = fixture("preview-missing");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let value = app
            .handle(&Request::get("/api/preview").authorized(TOKEN))
            .value();
        assert_eq!(value["source"], json!("missing"), "{value:?}");
        assert_eq!(value["etag"], json!(ABSENT_ETAG));
        let targets = value["targets"].as_array().expect("targets 是数组");
        assert_eq!(targets.len(), 4, "{targets:?}");
        // 一台没跑过一键准备的机器上 IPv4 那一格是空的：这一条断言的正是"oxidns 那一格
        // 红着并说清缺哪一格"，而不是悄悄给出一份绑 0.0.0.0 的开放解析器。
        let oxidns = targets
            .iter()
            .find(|t| t["name"] == json!("oxidns"))
            .expect("少 oxidns 那一格");
        assert_eq!(oxidns["ok"], json!(false), "{oxidns:?}");
        assert!(
            oxidns["error"]
                .as_str()
                .unwrap_or_default()
                .contains("lan_address"),
            "{oxidns:?}"
        );
        assert!(
            oxidns["error"]
                .as_str()
                .unwrap_or_default()
                .contains("provision"),
            "错误要给出下一步去哪儿：{oxidns:?}"
        );
        for name in ["dae", "zbox.nft", "netstate.sh"] {
            let target = targets
                .iter()
                .find(|t| t["name"] == json!(name))
                .unwrap_or_else(|| panic!("少了 {name} 那一格"));
            assert_eq!(target["ok"], json!(true), "{target:?}");
        }
    }

    /// IPv6 那一档的改动必须在预览里看得见。这一条是这一屏从"两份"扩到"四份"的全部理由：
    /// `dual_stack` 动的是 `zbox.nft`（`table ip6` 两条链）与 `netstate.sh`（`ip -6 rule/route`
    /// + 成对的 v6 sysctl），编排层那两份跟着变的只是 DNS 监听口。预览只给编排层时，
    /// 面板那句"与上一页逐字相同"对着的是一份**没算内核层**的比较——半句假话。
    #[test]
    fn the_ipv6_mode_switch_shows_up_in_the_kernel_layer_panes() {
        use crate::model::config::Ipv6Mode;

        let layout = fixture("preview-v6");
        let base = DesiredState::baseline();
        let runner = green_runner(&base);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &base.ports);
        let panel_token = token();
        let app = App::new(&svc, &panel_token);
        let read = |name: &str| -> String {
            let value = app
                .handle(&Request::get("/api/preview").authorized(TOKEN))
                .value();
            value["targets"]
                .as_array()
                .expect("targets 是数组")
                .iter()
                .find(|t| t["name"] == json!(name))
                .unwrap_or_else(|| panic!("少了 {name} 那一格"))
                .pointer("/lines")
                .and_then(Value::as_array)
                .map(|lines| {
                    lines
                        .iter()
                        .map(|l| l.as_str().unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| panic!("{name} 那一格没给行数组"))
        };

        put_state(&layout, &base);
        let nft_off = read("zbox.nft");
        let netstate_off = read("netstate.sh");
        let dae_off = read("dae");
        assert!(!nft_off.contains("table ip6 zbox {"), "{nft_off}");
        assert!(!netstate_off.contains("ip -6 rule"), "{netstate_off}");

        let mut dual = DesiredState::baseline();
        dual.network.ipv6_mode = Ipv6Mode::DualStack;
        dual.network.lan_cidr6 = "fd15:4ba5:5a2b:1008::/64".into();
        put_state(&layout, &dual);

        let nft_dual = read("zbox.nft");
        let netstate_dual = read("netstate.sh");
        assert!(nft_dual.contains("table ip6 zbox {"), "{nft_dual}");
        assert!(
            nft_dual.contains("ip6 saddr != fd15:4ba5:5a2b:1008::/64"),
            "v6 的豁免范围要按局域网段限定，与 v4 那条同形：{nft_dual}"
        );
        assert!(
            netstate_dual.contains("ip -6 rule add fwmark 0x1 table 100 priority 100"),
            "{netstate_dual}"
        );
        // 成对给的那两条 v6 sysctl，与 doctor 的 fix 同源
        for pair in [
            "net.ipv6.conf.all.forwarding=1",
            "net.ipv6.conf.all.accept_ra=2",
        ] {
            assert!(netstate_dual.contains(pair), "缺 {pair}：{netstate_dual}");
        }
        assert_ne!(
            nft_off, nft_dual,
            "换档后预览里 v6 那半边一个字都没变，等于没给用户看"
        );
        assert_ne!(netstate_off, netstate_dual);

        // dae 那一格在换档时**应当**逐字相同：`gen::dae` 里没有 `ipv6_mode` 的分支——实测 dae 自己
        // 就绑了 v6 口（`ipv6-kernel-measured.md` §10），不需要这一层替它补。面板上那句说明按这个写。
        // 磁盘上此刻是 dual，红的是这一条，要跟着一起改的是 `assets/panel.html` 里那三档落点那句话。
        assert_eq!(
            dae_off,
            read("dae"),
            "dae 那一格跟着 ipv6_mode 变了，可面板的说明写着它不进这份生成物的改动"
        );

        // 预览给的就是要下发的那两份：`render_target` 与 `snapshot_files` 走的是同一个函数，
        // 这一条钉住"面板看到的 = 内核将执行的"，两份各比一次。
        for (name, shown) in [
            ("zbox.nft", nft_dual.clone()),
            ("netstate.sh", netstate_dual.clone()),
        ] {
            let applied = crate::gen::render_target(&dual, name).expect("这一档渲染得出来");
            let applied = applied.strip_suffix('\n').unwrap_or(&applied);
            assert_eq!(applied, shown, "{name}：预览与快照不是同一份");
        }
    }

    /// 一份渲染得出来、一份渲染不出来：坏的那一格必须还在回执里，带着原因。
    /// 这一条是这一屏的诚实性所在——面板要能说出"这一份为什么是空的"，
    /// 而"少一格"与"坏一格"是两件不同的事（前者是服务端形状变了）。
    #[test]
    fn a_target_that_cannot_render_still_arrives_with_a_reason() {
        use crate::model::config::{FinalOutbound, Group, GroupKind};
        use crate::model::node::{Node, NodeKind, StreamProtocol, Tls, Transport};
        let layout = fixture("preview-partial");
        let mut state = DesiredState::baseline();
        state.nodes.push(Node {
            name: "n1".into(),
            server: "10.0.0.1".into(),
            port: 8388,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Shadowsocks {
                    method: "aes-256-gcm".into(),
                    password: "x".into(),
                },
                transport: Transport::Tcp,
                tls: Tls::Disabled,
            },
        });
        state.groups.push(Group {
            tag: "auto".into(),
            kind: GroupKind::Selector,
            members: vec!["n1".into()],
            default: None,
        });
        // 兜底指向策略组，但境外上游一条都没填：oxidns 的默认分支没有可去的地方，
        // 而 dae 那一份照旧成立（它只管连接，DNS 交给 oxidns）
        state.policy.final_outbound = FinalOutbound::Proxy("auto".into());
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        put_state(&layout, &state);

        let got = app.handle(&Request::get("/api/preview").authorized(TOKEN));
        assert_eq!(got.status, 200, "渲染失败不是请求失败：{}", got.body);
        let value = got.value();
        let targets = value["targets"].as_array().expect("targets 是数组");
        let dae = &targets[0];
        let oxidns = &targets[1];
        assert_eq!(dae["name"], json!("dae"));
        assert_eq!(oxidns["name"], json!("oxidns"));
        assert_eq!(dae["ok"], json!(true), "{dae:?}");
        assert_eq!(oxidns["ok"], json!(false), "{oxidns:?}");
        // 内核层那两份没有"渲染不出来"这一说（两个渲染器都不返回错误），所以编排层坏掉时
        // 它们照旧在场——面板那句"只渲染出 N 份"因此必须按四份算，不是两份。
        assert_eq!(targets[2]["name"], json!("zbox.nft"), "{:?}", targets[2]);
        assert_eq!(targets[3]["name"], json!("netstate.sh"), "{:?}", targets[3]);
        assert_eq!(targets[2]["ok"], json!(true), "{:?}", targets[2]);
        assert_eq!(targets[3]["ok"], json!(true), "{:?}", targets[3]);
        assert_eq!(oxidns["ok"], json!(false), "{oxidns:?}");
        let reason = oxidns["error"].as_str().expect("原因是一句话");
        assert!(reason.contains("dns.overseas"), "{reason}");
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
    }

    /// 状态文件坏了：两份都不渲染，但回执仍然是 200 加同一句原因。
    /// 拿基线顶上就等于在面板上展示一份机器上并不存在的配置——那是这一屏最坏的一种假话。
    #[test]
    fn a_broken_state_gives_no_preview_but_says_why() {
        let layout = fixture("preview-broken");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let path = PathBuf::from(layout.state_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"nodes\": 5}").unwrap();

        let got = app.handle(&Request::get("/api/preview").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let value = got.value();
        assert_eq!(value["source"], json!("broken"));
        assert!(
            value["error"].as_str().unwrap_or("").contains("解析失败"),
            "{value:?}"
        );
        let targets = value["targets"].as_array().expect("targets 是数组");
        assert_eq!(targets.len(), 4, "{targets:?}");
        for target in targets {
            assert_eq!(target["ok"], json!(false), "{target:?}");
            assert!(
                target["error"].as_str().unwrap_or("").contains("解析失败"),
                "四份都要带上同一句原因：{target:?}"
            );
        }
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
    }

    /// 新端点也要过同一道门，而且只认 GET：405 的措辞由 `ROUTES` 那张表给，
    /// 所以这里同时钉住"这一条只有一行两个动词的余地"。
    #[test]
    fn the_preview_is_behind_the_gate_and_takes_only_get() {
        let layout = fixture("preview-gate");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        assert_eq!(app.handle(&Request::get("/api/preview")).status, 401);
        let wrong_verb =
            app.handle(&Request::post_body("/api/preview", b"{\"write\": true}").authorized(TOKEN));
        assert_eq!(wrong_verb.status, 405, "{}", wrong_verb.body);
        assert!(wrong_verb.body.contains("GET"), "{}", wrong_verb.body);
        assert!(
            !wrong_verb.body.contains("POST /api/preview"),
            "回执不该像用法说明：{}",
            wrong_verb.body
        );
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
    }

    /// `GET /api/ladder` 的三条判据，都是这一屏存在的理由本身：读的是盘上那一份期望态、
    /// 「还没下发」与「哪一份渲染不出来」两格跟概览用的是**同一句**（同一个函数，不是同一个措辞），
    /// 而且这一路一个字节都不写、一条命令都不发。
    #[test]
    fn the_ladder_reads_the_saved_state_and_shares_the_overview_wording() {
        let layout = fixture("ladder");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        put_state(&layout, &state);

        let got = app.handle(&Request::get("/api/ladder").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let value = got.value();
        assert_eq!(value["source"], json!("saved"), "{value:?}");
        let overview = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(value["etag"], overview["etag"], "两屏说的指纹必须是一个");
        assert_eq!(
            value["reload_pending"], overview["reload_pending"],
            "「还没下发」在梯级与概览上不能一个亮一个不亮"
        );
        assert_eq!(
            value["render_errors"], overview["render_errors"],
            "哪一份现在落不了地，两屏必须是同一句"
        );
        assert_eq!(
            value["last_good"], overview["last_good"],
            "「与已知良好那份一致」在梯级与概览上不能各说一套"
        );

        let rows = value["rows"].as_array().expect("rows 是数组");
        assert!(!rows.is_empty(), "{value:?}");
        // 首行永远是那条不受任何开关影响的卫生层
        assert_eq!(rows[0]["tier"], json!("fixed"), "{:?}", rows[0]);
        assert_eq!(rows[0]["connection"], json!(1), "{:?}", rows[0]);
        assert!(
            rows.iter()
                .any(|r| r["tier"] == json!("domain") && r["when"] == json!("+www.example.com")),
            "用户自己写的那条名单要带着他写的那个样子出现在表里：{rows:?}"
        );
        let fallbacks: Vec<&Value> = rows
            .iter()
            .filter(|r| r["tier"] == json!("fallback"))
            .collect();
        assert_eq!(fallbacks.len(), 1, "兜底只能有一行：{rows:?}");
        assert_eq!(fallbacks[0]["from"], json!("policy.final_outbound"));
        // 两列序号各自是 1 起的连续编号。这份夹具里 cn_direct 是关的，所以没有占两行的格子
        for column in ["dns", "connection"] {
            let got: Vec<u64> = rows.iter().filter_map(|r| r[column].as_u64()).collect();
            let want: Vec<u64> = (1..=got.len() as u64).collect();
            assert_eq!(got, want, "{column} 那一列的序号不连续：{rows:?}");
        }
        // 不落行的格子必须自带一句为什么，否则面板只能画成"这一格没生效"
        for row in rows
            .iter()
            .filter(|r| r["dns"].is_null() && r["connection"].is_null())
        {
            assert!(
                !row["why"].as_str().unwrap_or("").is_empty(),
                "两层都不落行却没说为什么：{row:?}"
            );
            assert_eq!(row["connection_lines"], json!(0), "{row:?}");
        }
        for row in rows.iter() {
            assert!(!row["when"].as_str().unwrap_or("").is_empty(), "{row:?}");
            assert!(!row["then"].as_str().unwrap_or("").is_empty(), "{row:?}");
            assert!(!row["from"].as_str().unwrap_or("").is_empty(), "{row:?}");
            // applied 在 P4 之前永远是 null——回执里必须有这个键（面板按字段名取值，少一个键就是少一列）
            assert!(
                row.as_object().unwrap().contains_key("applied"),
                "回执少了 applied 字段：{row:?}"
            );
            assert!(
                row["applied"].is_null(),
                "applied 在 P4 之前必须是 null，这一行却是 {}：{row:?}",
                row["applied"]
            );
        }

        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
        let root = PathBuf::from(layout.root.clone());
        let entries: Vec<String> = fs::read_dir(&root)
            .unwrap_or_else(|e| panic!("读 {} 失败: {e}", root.display()))
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            entries,
            vec!["state.json".to_string()],
            "梯级不许造快照目录"
        );
    }

    /// 还没保存过的机器：给的是新装基线的形状，明说是 `missing`，而且**不报"还没下发"**——
    /// 那份期望态根本还没落盘，报 pending=true 会让新装的机器一打开面板就顶着一个黄色警告。
    /// 基线的 IPv4 那一格是空的（没跑过一键准备），所以这一屏同时钉住"oxidns 现在落不了地"那句。
    #[test]
    fn the_ladder_on_a_fresh_machine_is_a_shape_not_a_pending_warning() {
        let layout = fixture("ladder-missing");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let value = app
            .handle(&Request::get("/api/ladder").authorized(TOKEN))
            .value();
        assert_eq!(value["source"], json!("missing"), "{value:?}");
        assert_eq!(value["etag"], json!(ABSENT_ETAG));
        assert_eq!(value["reload_pending"], json!(null), "{value:?}");
        assert_eq!(value["generation_error"], json!(null), "{value:?}");
        // 一台什么都没下发过的机器上这一格必须是"没有"，不能是空串或 0——面板靠它分开
        // "还没下发过"与"与已知良好那份不一致"这两句话，把前者说成后者是指控用户。
        assert_eq!(value["last_good"], json!(null), "{value:?}");
        let rows = value["rows"].as_array().expect("rows 是数组");
        assert!(!rows.is_empty(), "{value:?}");
        assert!(
            rows.iter().any(|r| r["tier"] == json!("note")),
            "基线里 use_system_local 是开的，表尾要有一行说明它没有落点：{rows:?}"
        );
        for row in rows {
            assert!(
                row["applied"].is_null(),
                "新机器基线的 applied 也必须是 null：{row:?}"
            );
        }
        let errors = value["render_errors"].as_array().expect("render_errors");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0]["name"], json!("oxidns"), "{errors:?}");
    }

    /// 状态文件坏了：表是**空的**，原因给同一句话，两份生成物都标坏。
    /// 拿基线顶上等于在这一屏画出一条机器上并不存在的梯度——那是这一屏最坏的一种假话。
    #[test]
    fn a_broken_state_gives_an_empty_ladder_with_the_reason() {
        let layout = fixture("ladder-broken");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let path = PathBuf::from(layout.state_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"nodes\": 5}").unwrap();

        let got = app.handle(&Request::get("/api/ladder").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let value = got.value();
        assert_eq!(value["source"], json!("broken"));
        assert!(
            value["error"].as_str().unwrap_or("").contains("解析失败"),
            "{value:?}"
        );
        assert_eq!(
            value["rows"]
                .as_array()
                .expect("空表也得是数组，缺字段与空表是两件事")
                .len(),
            0,
            "{value:?}"
        );
        assert_eq!(value["reload_pending"], json!(null), "{value:?}");
        let errors = value["render_errors"].as_array().expect("render_errors");
        assert_eq!(errors.len(), 2, "{errors:?}");
        for error in errors {
            assert!(
                error["error"].as_str().unwrap_or("").contains("解析失败"),
                "两份都要带上同一句原因：{error:?}"
            );
        }
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
    }

    /// dae 那一格渲染不出来（组名占了 dae 的内置出站名）时，这一屏**不许跟着空掉**：它读的是期望态
    /// 自己的形状，而"哪一份现在落不了地"由 `render_errors` 那一格单独说——两句话混在一起，
    /// 概览与这一屏就有一个在撒谎。
    #[test]
    fn a_ladder_stays_whole_when_one_renderer_refuses() {
        use crate::model::config::{Group, GroupKind};
        use crate::model::node::{Node, NodeKind, StreamProtocol, Tls, Transport};
        let layout = fixture("ladder-partial");
        let mut state = edited();
        state.nodes.push(Node {
            name: "n1".into(),
            server: "10.0.0.1".into(),
            port: 8388,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Shadowsocks {
                    method: "aes-256-gcm".into(),
                    password: "x".into(),
                },
                transport: Transport::Tcp,
                tls: Tls::Disabled,
            },
        });
        // 模型认这个名字（合法的标识符），dae 不认：它跟内置出站 `direct` 重名
        state.groups.push(Group {
            tag: "direct".into(),
            kind: GroupKind::Selector,
            members: vec!["n1".into()],
            default: None,
        });
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        put_state(&layout, &state);

        let value = app
            .handle(&Request::get("/api/ladder").authorized(TOKEN))
            .value();
        assert_eq!(value["source"], json!("saved"), "{value:?}");
        let errors = value["render_errors"].as_array().expect("render_errors");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0]["name"], json!("dae"), "{errors:?}");
        let rows = value["rows"].as_array().expect("rows 是数组");
        assert!(
            rows.iter()
                .any(|r| r["tier"] == json!("domain") && r["when"] == json!("+www.example.com")),
            "一份生成物坏了不该让整张表消失：{rows:?}"
        );
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
    }

    /// 新端点同样在门内，而且只认 GET。405 的措辞由 `ROUTES` 那张表给，
    /// 所以这里同时钉住"这一条只有一行一个动词的余地"。
    #[test]
    fn the_ladder_is_behind_the_gate_and_takes_only_get() {
        let layout = fixture("ladder-gate");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        assert_eq!(app.handle(&Request::get("/api/ladder")).status, 401);
        let wrong_verb =
            app.handle(&Request::post_body("/api/ladder", b"{\"write\": true}").authorized(TOKEN));
        assert_eq!(wrong_verb.status, 405, "{}", wrong_verb.body);
        assert!(wrong_verb.body.contains("GET"), "{}", wrong_verb.body);
        assert!(
            !wrong_verb.body.contains("POST /api/ladder"),
            "回执不该像用法说明：{}",
            wrong_verb.body
        );
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
    }

    /// 看板：门内、只认 GET、**一个进程都不起**，而且磁盘那一格是"明写没读"不是空着。
    ///
    /// 这一条真正钉的是那句"这一屏一条命令都不发"——它一破，`/api/board` 就跟
    /// `GET /api/status` 成了同一个东西（后者本来就要起进程读服务状态），而看板是按一次
    /// 读一次、会被反复刷新的那一屏。顺带钉住路径不外泄（§7.2 第 1 条：连 404 都不带
    /// `Layout::root`，一个能被轮询的只读端点更不该）。
    #[test]
    fn the_board_is_behind_the_gate_and_sends_no_command() {
        let layout = fixture("board-gate");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        assert_eq!(app.handle(&Request::get("/api/board")).status, 401);
        let wrong_verb = app.handle(
            &Request::post_body("/api/board", b"{\"write\": true}")
                .authorized(TOKEN),
        );
        assert_eq!(wrong_verb.status, 405, "{}", wrong_verb.body);
        assert!(wrong_verb.body.contains("GET"), "{}", wrong_verb.body);

        let got = app.handle(&Request::get("/api/board").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
        let value = got.value();
        assert!(
            !value.to_string().contains(&layout.root),
            "看板不许把状态目录的路径带出去：{}",
            value
        );
        assert_eq!(value["source"], "missing");
        assert!(value["etag"].is_string());
        // 取数时刻由后端盖，且是个像样的毫秒纪元值（不是 0、不是浏览器给的）。
        assert!(value["taken_at_epoch_ms"].as_u64().unwrap_or(0) > 1_700_000_000_000);
        assert_eq!(value["disk"]["read"], false);
        assert_eq!(value["disk"]["reason"], crate::svc::board::DISK_NOT_READ);
        assert!(value["interfaces"].is_array());
        // 「盘上写着局域网网卡是 X」这一句在机器上没有真网卡时也给得出，且只回 baseline 那个值——
        // 面板不许据此判定这块网卡在不在（U3 实测：新机器的 `ens33` 与真实 `eth0` 会撞车）。
        assert_eq!(
            value["lan_interface"],
            json!(DesiredState::baseline().network.lan_interface)
        );
    }

    /// 每个数**单独**降级：`/proc` 整个读不到（Windows 开发机）时不许出现"0"或空字符串，
    /// 因为面板要把它们画成「未读到」，而一根长度为 0 的条说的是"没怎么用"——两句话。
    #[test]
    fn the_board_degrades_each_reading_on_its_own() {
        let empty = crate::svc::board::BoardReadings::default();
        let value = board_body(
            &empty,
            1_760_000_000_000,
            "saved",
            Value::Null,
            "etag",
            None,
            &dns_api::Tops::default(),
            json!(true),
            (Value::Null, Value::Null),
        );
        for key in ["total_kb", "available_kb", "used_kb", "used_percent"] {
            assert!(value["memory"][key].is_null(), "{key} 该是 null");
        }
        assert!(
            value["memory"]["used_text"].is_null() && value["memory"]["total_text"].is_null(),
            "数字没读到，配它的人话也不许有：{}",
            value["memory"]
        );
        assert!(value["load"].is_null());
        assert!(value["uptime"]["secs"].is_null());
        assert!(value["uptime"]["text"].is_null());
        assert!(value["interfaces"].as_array().unwrap().is_empty());
        assert!(value["lan_interface"].is_null());

        // 只缺 `MemAvailable`（老内核）：总量照给，百分比与"已用"两头一起落 null，不拿 MemFree 顶。
        let partial = crate::svc::board::BoardReadings {
            mem_total_kb: Some(694832),
            ..Default::default()
        };
        let value = board_body(
            &partial,
            1_760_000_000_000,
            "saved",
            Value::Null,
            "etag",
            Some("eth0"),
            &dns_api::Tops::default(),
            json!(true),
            (json!("e3b0c44298fc1c149afbf4c8996fb924"), Value::Null),
        );
        assert_eq!(value["memory"]["total_kb"], 694832);
        assert_eq!(value["memory"]["total_text"], "678.5 MB");
        assert!(value["memory"]["used_percent"].is_null());
        assert!(value["memory"]["used_text"].is_null());
        assert_eq!(value["lan_interface"], "eth0");
        // 引擎那一格同理：`wants_version` 是**盘上那份**的摘要，与那个口连不连得上无关，所以照给；
        // 而"跑的是不是这一份"要两头都有数才说得出——一头没读到就是 null，不是 false。
        assert_eq!(value["engines"]["oxidns"]["live"], false);
        assert!(value["engines"]["oxidns"]["version"].is_null());
        assert!(value["engines"]["oxidns"]["running_version"].is_null());
        assert_eq!(
            value["engines"]["oxidns"]["wants_version"],
            "e3b0c44298fc1c149afbf4c8996fb924"
        );
        assert!(
            value["engines"]["oxidns"]["applied"].is_null(),
            "没读到不许报成「不是这一份」"
        );
    }

    /// §11.3 G 那一格：三台引擎在不在、什么版本、跑的是不是盘上这一份。
    ///
    /// 三行里**只有 oxidns 读得到数**，而这不是这一屏的偷懒：`/api/health` 是"进程活着才答"的口，
    /// 读到就算在位，所以不必再发一条命令去问进程表（看板那条"一条命令都不发"的前提因此不破）。
    /// dae 没有任何可查口（控制面在它自己那个网络命名空间里，`docs/reference/dae-config-dictionary.md`），
    /// 主路由那一半还没接通，两行因此各写一句为什么空。
    ///
    /// 两份回执是 guest 逐字抓的（`.tmp/oxapi/cap/_api_health.txt`、`/api/control`），那个
    /// `running_version` 就是当时盘上那份配置的 `sha256sum`。三条断言钉的是三种说不清：
    /// 对上 / 没对上 / 一头没数。
    #[test]
    fn the_engine_row_reports_oxidns_and_names_the_two_it_cannot_read() {
        let health = json!({
            "status": "ok", "version": "1.6.1", "build_bundle": "full",
            "uptime_ms": 179143,
            "checks": {"api": "ok", "plugin_init": "ok", "server_startup": "ok"},
            "plugins": {"total": 4, "servers": 1}
        });
        let running = "31e240f54715aa9bd91b474184c9602d86c2eb0fbea26c5c6464461a75216ffc";
        let control = json!({
            "status": "running", "uptime_ms": 206373,
            "reload": {"status": "idle", "running_version": running}
        });
        let tops = dns_api::Tops {
            live: true,
            health,
            control,
            ..dns_api::Tops::default()
        };
        let engines = |wants: (Value, Value)| {
            board_body(
                &crate::svc::board::BoardReadings::default(),
                1_760_000_000_000,
                "saved",
                Value::Null,
                "e",
                None,
                &tops,
                json!(true),
                wants,
            )["engines"]
                .clone()
        };

        let got = engines((json!(running), Value::Null));
        assert_eq!(got["oxidns"]["live"], true, "{got}");
        assert_eq!(got["oxidns"]["version"], "1.6.1");
        assert_eq!(got["oxidns"]["build_bundle"], "full");
        assert_eq!(got["oxidns"]["running_version"], running);
        assert_eq!(got["oxidns"]["applied"], true, "两头一样才算对上");

        // 跑的不是这一份：这一格说的是"面板改了、解析器还没换上"，而它**不是**那道门——
        // 门在「让它生效」与「诊断」那两屏，这一屏只报数。
        let got = engines((json!("另一个摘要"), Value::Null));
        assert_eq!(got["oxidns"]["applied"], false);

        // 面板这份渲染不出来：摘要 null + 那句原因原样带上，`applied` 回到 null 而不是 false。
        let got = engines((Value::Null, json!("这一份渲染不出来：缺一个策略组")));
        assert!(got["oxidns"]["wants_version"].is_null());
        assert_eq!(
            got["oxidns"]["generate_error"],
            "这一份渲染不出来：缺一个策略组"
        );
        assert!(
            got["oxidns"]["applied"].is_null(),
            "说不清时不许报成「没对上」"
        );

        // 另两行：读不到 + 那一句为什么。措辞只在 `crate::svc::board` 一处定义，面板抄。
        assert_eq!(got["dae"]["read"], false);
        assert_eq!(got["dae"]["reason"], crate::svc::board::DAE_NOT_READ);
        assert_eq!(got["router"]["read"], false);
        assert_eq!(got["router"]["reason"], crate::svc::board::ROUTER_NOT_READ);
    }

    /// 读数齐全那份（2026-10-12 guest 逐字）走完整路径：单位口径只在 Rust 一处，
    /// 面板拿到的就是能直接印出来的字符串 + 一个算好的百分比。
    #[test]
    fn the_board_formats_the_guest_capture_once_and_only_in_rust() {
        let readings = crate::svc::board::BoardReadings {
            mem_total_kb: Some(694832),
            mem_available_kb: Some(419312),
            load: Some([0.15, 0.10, 0.03]),
            uptime_secs: Some(163051),
            netdev: vec![crate::svc::board::NetDev {
                iface: "eth0".into(),
                rx_bytes: 1_721_523_045,
                tx_bytes: 100_617_238,
            }],
        };
        let value = board_body(
            &readings,
            1_760_000_000_000,
            "saved",
            Value::Null,
            "e",
            None,
            &dns_api::Tops::default(),
            Value::Null,
            (Value::Null, Value::Null),
        );
        assert_eq!(value["memory"]["used_percent"], 39);
        assert_eq!(value["memory"]["used_text"], "269.1 MB");
        assert_eq!(value["uptime"]["text"], "1 天 21 小时");
        assert_eq!(value["load"], json!([0.15, 0.10, 0.03]));
        assert_eq!(value["interfaces"][0]["iface"], "eth0");
        assert_eq!(value["interfaces"][0]["rx_text"], "1.6 GB");
        assert_eq!(value["interfaces"][0]["tx_text"], "96.0 MB");
        // 原始字节同样在：面板画柱要比例，不许靠解析自己那句人话。
        assert_eq!(value["interfaces"][0]["rx_bytes"], 1_721_523_045);
    }

    /// 磁盘上那份读不进来时**硬件读数照给**：看板上那几格来自 `/proc`，与 state.json 没关系。
    /// `lan_interface` 因此落 `null`（那一句抄自盘上那份，读不到就不编），而 `error` 那句原样带过去。
    #[test]
    fn a_broken_state_file_does_not_blank_the_hardware_cells() {
        let layout = fixture("board-broken");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let dir = PathBuf::from(layout.state_path());
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        fs::write(&dir, "这不是 JSON").unwrap();

        let value = app
            .handle(&Request::get("/api/board").authorized(TOKEN))
            .value();
        assert_eq!(value["source"], "broken");
        assert!(value["error"].is_string());
        assert!(value["lan_interface"].is_null());
        assert!(value["memory"].is_object(), "硬件那几格与 state.json 无关");
        assert!(value["disk"]["read"].is_boolean());
        assert_eq!(
            value["taken_at_epoch_ms"].as_u64().unwrap_or(0),
            value["taken_at_epoch_ms"].as_u64().unwrap(),
            "读数时刻该是个毫秒数"
        );
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
    }

    /// 看板上解析层那四格：原样搬、各读各的，而且**不借解析记录那一页的路**。
    ///
    /// 四份 JSON 是 2026-10-12 guest 逐字抓的（`oxidns-probe/board.yaml`，那份就是面板渲染件），
    /// 抓的过程记在 `docs/reference/oxidns-plugin-args-verified.md`。这一条钉四件事，前三件都是
    /// 被实测教出来的：
    ///
    /// 1. **一格都不解析、不重算**：`share` 那个比例是上游算的（`count / sample_size`），ZBox
    ///    只把 `Value` 搬过去。但"搬"不等于"逐字"——一次 serde 往返就会让浮点末位变 1 个 ulp
    ///    （实测，见下面那条注释），所以这一条比的是结构与算式没被动过，不比字节。
    /// 2. **各格自己的 `sample_size` 可以不相等**（实测同一轮里 `top_qnames` 给 13、另两份给 15）。
    ///    所以后端不许替它们算一个共用的数，面板那句"最近 N 条"必须每格带自己的 N。
    /// 3. **`rcode` 那一格会给出上游的合成桶**（实测 `_ERROR`、`_NO_RESPONSE`、`_UNKNOWN`）。
    ///    它们不是 DNS 应答码的名字，翻译或并入真 rcode 都是编。
    /// 4. 看板刷新只打 `tops()` 那一条路，`screen()` 一次都不调——并进记录那一页就是
    ///    **每翻一页多打四条上游请求**（`Tops` 的注释里那条理由在这里落地）。
    #[test]
    fn the_board_hands_over_the_parse_layer_aggregates_verbatim() {
        let qnames = json!({"ok":true,"sample_size":13,"rows":[
            {"key":"chatgpt.com.","count":4,"share":0.3076923076923077},
            {"key":"a.test.","count":3,"share":0.23076923076923078},
            {"key":"www.example.com.","count":3,"share":0.23076923076923078},
            {"key":"example.com.","count":2,"share":0.15384615384615385},
            {"key":"b.test.","count":1,"share":0.07692307692307693}]});
        let clients = json!({"ok":true,"sample_size":15,"rows":[
            {"key":"192.168.219.128","count":15,"share":1.0}]});
        // 合成桶 `_ERROR` 是原样搬的（这一份里 10 条查询压根没拿到应答，上游就是这么记的）。
        let rcodes = json!({"ok":true,"sample_size":15,"rows":[
            {"key":"_ERROR","count":10,"share":0.6666666666666666},
            {"key":"No Error","count":5,"share":0.3333333333333333}]});
        let plugins = json!({"ok":true,"query_total":15,"stats":[
            {"kind":"executor","tag":"cache","checked":0,"matched":0,"executed":10,
             "query_total":10,"query_share":0.6666666666666666},
            {"kind":"matcher","tag":"qtype_sink","checked":15,"matched":5,"executed":0,
             "query_total":15,"query_share":1.0}]});
        let observer = fake_tops(dns_api::Tops {
            live: true,
            detail: None,
            qnames: qnames.clone(),
            clients: clients.clone(),
            rcodes: rcodes.clone(),
            plugins: plugins.clone(),
            errors: vec![("clients".to_string(), "管理口回 404：这一份构建没有这个路由".into())],
            ..dns_api::Tops::default()
        });
        let layout = fixture("board-tops");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token).with_dns(&observer);

        let value = app
            .handle(&Request::get("/api/board").authorized(TOKEN))
            .value();
        assert_eq!(value["dns"]["live"], true, "{value}");
        // **不比浮点的末位**：这一格走的是"`Value` 进、`Value` 出"，而 `share` 那样的浮点
        // 在一次 serde 往返里会变最后一个数字（实测 `0.15384615384615385` 出来成
        // `0.15384615384615383`，1 ulp；屏幕上那是同一句"30.8%"）。
        // 所以这里比的是"没被改写"的那一层：结构、顺序、`key`/`count` 逐字，`share` 按相对误差。
        for (cell, given) in [
            ("qnames", &qnames),
            ("clients", &clients),
            ("rcodes", &rcodes),
            ("plugins", &plugins),
        ] {
            let no_rows: Vec<Value> = Vec::new();
            let got = &value["dns"][cell];
            assert_eq!(got["ok"], given["ok"], "{cell}");
            assert_eq!(
                got["sample_size"],
                given["sample_size"],
                "{cell}：样本数不许换成算出来的"
            );
            let got_rows = got["rows"].as_array().unwrap_or(&no_rows);
            let given_rows = given["rows"].as_array().unwrap_or(&no_rows);
            assert_eq!(got_rows.len(), given_rows.len(), "{cell}");
            for (index, (got_row, given_row)) in
                got_rows.iter().zip(given_rows.iter()).enumerate()
            {
                assert_eq!(got_row["key"], given_row["key"], "{cell} 第 {index} 行");
                assert_eq!(got_row["count"], given_row["count"], "{cell} 第 {index} 行");
                let got_share = got_row["share"].as_f64().unwrap_or_default();
                let given_share = given_row["share"].as_f64().unwrap_or_default();
                assert!(
                    (got_share - given_share).abs() <= given_share.abs() * 1e-9 + 1e-15,
                    "{cell} 第 {index} 行的比例被改算过了：{got_share} vs {given_share}"
                );
            }
            assert_eq!(
                got["query_total"], given["query_total"],
                "{cell}：`plugins` 那一份用的是 `query_total` 而不是 `sample_size`"
            );
        }
        // 13 与 15 是两个数，后端没把它们揉成一个"总量"。
        assert_eq!(value["dns"]["qnames"]["sample_size"], 13);
        assert_eq!(value["dns"]["rcodes"]["sample_size"], 15);
        // `_ERROR` 这种合成桶原样在屏幕上（翻译它或把它并进真 rcode 都是编）。
        assert_eq!(value["dns"]["rcodes"]["rows"][0]["key"], "_ERROR");
        // 单项软失败：那一句进 `errors`，**这一格**为 null，其余三格照给。
        assert_eq!(
            value["dns"]["errors"]["clients"],
            "管理口回 404：这一份构建没有这个路由"
        );
        // 「记下每一次查询」抄自盘上那份期望态：面板要靠它区分"没开记录"与"没人查询过"。
        assert_eq!(value["dns"]["records_enabled"], json!(state.dns.records_enabled));

        assert_eq!(*observer.tops_calls.borrow(), 1, "看板刷新读聚合的次数");
        assert!(
            observer.asked.borrow().is_empty(),
            "看板不该走解析记录那一页的取数路：{:?}",
            observer.asked.borrow()
        );
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
    }

    /// 管理口连不上时看板**不是整屏失败**：硬件那几格照给，解析层四格一起空掉并带那一句原因。
    ///
    /// 分得开这两句话是这一屏的底线——"这一格没读到"与"这台机器不合规"在屏幕上必须长得不一样，
    /// 而 `detail` 那句来自上游（连不上回环上的 8053 是真在开发机上每天发生的事）。
    #[test]
    fn an_unreachable_management_port_blanks_only_the_parse_layer() {
        let observer = fake_tops(dns_api::Tops {
            live: false,
            detail: Some("连不上管理口：Connection refused (os error 10061)".to_string()),
            ..dns_api::Tops::default()
        });
        let layout = fixture("board-offline");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token).with_dns(&observer);

        let value = app
            .handle(&Request::get("/api/board").authorized(TOKEN))
            .value();
        assert_eq!(value["dns"]["live"], false, "{value}");
        assert!(value["dns"]["detail"]
            .as_str()
            .unwrap()
            .contains("Connection refused"));
        for cell in ["qnames", "clients", "rcodes", "plugins"] {
            assert!(value["dns"][cell].is_null(), "{cell} 该是 null：{value}");
        }
        assert_eq!(value["dns"]["errors"], json!({}), "整口不通不是一格一格的错");
        // 引擎那一格走同一批数：oxidns 那行跟着落"没读到"，而**另外两行那句为什么照旧给得出**——
        // 它说的不是"这一轮没读到"，是"这一屏没有能读到它的口"，与管理口通不通无关。
        assert_eq!(value["engines"]["oxidns"]["live"], false);
        assert!(value["engines"]["oxidns"]["version"].is_null());
        assert!(value["engines"]["oxidns"]["applied"].is_null());
        assert_eq!(
            value["engines"]["dae"]["reason"],
            crate::svc::board::DAE_NOT_READ
        );
        assert_eq!(
            value["engines"]["router"]["reason"],
            crate::svc::board::ROUTER_NOT_READ
        );
        // 硬件那一侧不受影响（这一条断言的是"两句话分开说"，不是数值）。
        assert!(value["memory"].is_object());
        assert!(value["taken_at_epoch_ms"].is_number());
    }

    /// 磁盘上那份状态渲染得出快照、渲染不出 dae：这是面板最容易说假话的一格。
    ///
    /// 真现场（2026-10-09 guest 实测，组名 `1HK`）：保存回执一句"已写入磁盘"、概览的
    /// 「生成报错」写着"无"，而生成预览那一屏的 dae 是坏的。`generation_error` 说的是
    /// "下发要读的那份快照生成不出来"，`apply` 消费的正是那一份，所以它没有说错——
    /// 缺的是另一格。这一条钉住两件事：那一格存在，而且它不许去改写前一格。
    #[test]
    fn the_overview_names_the_orchestration_file_that_cannot_render() {
        use crate::model::config::{Group, GroupKind};
        use crate::model::node::{Node, NodeKind, StreamProtocol, Tls, Transport};
        let layout = fixture("render-errors");
        let mut state = DesiredState::baseline();
        // 解析口地址给好，让"渲染不过的那一格"只剩策略组名那一条——这一条要断言的是
        // "点名点得准"，不是"数错格子"。
        state.network.lan_address = "192.0.2.10".into();
        state.nodes.push(Node {
            name: "n1".into(),
            server: "10.0.0.1".into(),
            port: 8388,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Shadowsocks {
                    method: "aes-256-gcm".into(),
                    password: "x".into(),
                },
                transport: Transport::Tcp,
                tls: Tls::Disabled,
            },
        });
        state.groups.push(Group {
            tag: "1HK".into(),
            kind: GroupKind::Selector,
            members: vec!["n1".into()],
            default: None,
        });
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        put_state(&layout, &state);

        let value = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(value["source"], json!("saved"), "{value:?}");
        assert_eq!(
            value["generation_error"],
            Value::Null,
            "快照那一份渲染得出来，这一格不许替它报错：{value:?}"
        );
        let failures = value["render_errors"].as_array().expect("数组");
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0]["name"], json!("dae"), "{failures:?}");
        assert!(
            failures[0]["error"]
                .as_str()
                .unwrap_or_default()
                .contains("不是 dae 认的标识符"),
            "{failures:?}"
        );
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
    }

    /// 保存回执也要带这一格：按下保存的人就是写出这一份的人，"已写入磁盘"后面不接这句，
    /// 他下一次看见它已经是别人来问"为什么配置里那一行没了"。改回合法组名再存一次，
    /// 这一格要清空——留着上一次的报错比缺失更难查。
    #[test]
    fn the_save_receipt_carries_it_and_a_good_save_clears_it() {
        use crate::model::config::{Group, GroupKind};
        use crate::model::node::{Node, NodeKind, StreamProtocol, Tls, Transport};
        let layout = fixture("render-errors-save");
        let mut state = DesiredState::baseline();
        state.network.lan_address = "192.0.2.10".into();
        state.nodes.push(Node {
            name: "n1".into(),
            server: "10.0.0.1".into(),
            port: 8388,
            kind: NodeKind::Stream {
                protocol: StreamProtocol::Shadowsocks {
                    method: "aes-256-gcm".into(),
                    password: "x".into(),
                },
                transport: Transport::Tcp,
                tls: Tls::Disabled,
            },
        });
        state.groups.push(Group {
            tag: "1HK".into(),
            kind: GroupKind::Selector,
            members: vec!["n1".into()],
            default: None,
        });
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let etag = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value()["etag"]
            .as_str()
            .expect("新机器也给 etag")
            .to_string();
        let mut body = disk_value(&state);
        let saved = app.handle(
            &Request::put("/api/state", body.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(&etag),
        );
        assert_eq!(saved.status, 200, "{}", saved.body);
        let reported = saved.value();
        assert_eq!(reported["saved"], json!(true), "{reported:?}");
        assert_eq!(reported["generation_error"], Value::Null, "{reported:?}");
        let failures = reported["render_errors"].as_array().expect("数组");
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0]["name"], json!("dae"), "{failures:?}");

        // 同一个键位改成合法形状再存：这一格要是空数组，而不是"没有这个键"
        body["groups"][0]["tag"] = json!("HK");
        let again = app.handle(
            &Request::put("/api/state", body.to_string().as_bytes())
                .authorized(TOKEN)
                .matching(reported["etag"].as_str().expect("新 etag")),
        );
        assert_eq!(again.status, 200, "{}", again.body);
        let cleared = again.value();
        assert_eq!(
            cleared["render_errors"],
            json!([]),
            "改对了还在报错，面板就只能一直挂着一条假警告：{cleared:?}"
        );
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
    }

    /// 状态文件读不进来时不许说"两份都渲染出来了"：磁盘上那份谁也读不出，
    /// 两份编排配置因此都渲染不了，回执要带上同一句原因（和 `GET /api/preview` 一致）。
    #[test]
    fn a_broken_state_reports_both_as_unrenderable() {
        let layout = fixture("render-errors-broken");
        let state = DesiredState::baseline();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let path = PathBuf::from(layout.state_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"nodes\": 5}").unwrap();

        let value = app
            .handle(&Request::get("/api/state").authorized(TOKEN))
            .value();
        assert_eq!(value["source"], json!("broken"), "{value:?}");
        let failures = value["render_errors"].as_array().expect("数组");
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert_eq!(failures[0]["name"], json!("dae"), "{failures:?}");
        assert_eq!(failures[1]["name"], json!("oxidns"), "{failures:?}");
        for failure in failures {
            assert!(
                failure["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("解析失败"),
                "两份都要带上那句读取失败：{failure:?}"
            );
        }
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
    }

    /// 磁盘上放一份可读的状态，绕过 `PUT`（这几条要验的是动作端点，不是编辑契约）。
    fn put_state(layout: &Layout, state: &DesiredState) {
        let path = PathBuf::from(layout.state_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, disk_value(state).to_string()).unwrap();
    }

    fn exec(code: i32, stdout: &str) -> Result<Exec, String> {
        Ok(Exec {
            code,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    const ACTIONS: [&str; 5] = [
        "/api/rule-sets/sync",
        "/api/hijack/enable",
        "/api/hijack/disable",
        "/api/rollback",
        "/api/subscription",
    ];

    /// 五个写面端点共用同一道门、同一个动词。没令牌时连"这个端点存不存在"都不告诉对面，
    /// GET 打到 POST 上则是 405——但两条路都不许起一个进程。
    #[test]
    fn the_write_side_action_endpoints_share_the_token_gate_and_the_verb() {
        let layout = fixture("actions-gate");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        for path in ACTIONS {
            assert_eq!(app.handle(&Request::post(path)).status, 401, "{path}");
        }
        for path in ACTIONS {
            let got = app.handle(&Request::get(path).authorized(TOKEN));
            assert_eq!(got.status, 405, "{path}：{}", got.body);
            assert!(got.body.contains("POST"), "{path}：{}", got.body);
        }
        assert!(runner.argv().is_empty(), "路由阶段不该动机器");
    }

    /// 动作参数**只有体一个容身处**（路径与查询串都不许带参数），而写错名字的键一律 400。
    /// 静默忽略一个拼错的 `force`，在现场就是"按了按钮没反应"——那比报错难查得多。
    #[test]
    fn an_action_param_that_is_not_the_narrow_object_is_refused_before_anything_runs() {
        let layout = fixture("action-body");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let cases = [
            ("/api/rollback", r#"{"revion":"x"}"#, "revision"),
            ("/api/rule-sets/sync", r#"{"forcer":true}"#, "force"),
            // 不收参数的端点也要拒：这条不是洁癖，是把"参数名写错"这条路整个封掉
            ("/api/hijack/enable", r#"{"force":true}"#, "不收任何参数"),
            ("/api/rollback", r#"{"revision":true}"#, "修订号字符串"),
            ("/api/rule-sets/sync", r#"{"force":"yes"}"#, "true 或 false"),
            ("/api/rollback", "not json", "不是合法的 JSON"),
            ("/api/rollback", "[]", "JSON 对象"),
            // 订阅那一屏只有一个参数，而它的键名是面板与后端之间唯一的契约
            ("/api/subscription", r#"{"urls":"https://x/s"}"#, "url"),
            ("/api/subscription", r#"{"url":["https://x/s"]}"#, "url 要的是一串地址"),
        ];
        for (path, body, want) in cases {
            let got = app.handle(&Request::post_body(path, body.as_bytes()).authorized(TOKEN));
            assert_eq!(got.status, 400, "{path} {body} -> {}", got.body);
            assert!(
                got.body.contains(want),
                "{body} 的回执该提 {want}：{}",
                got.body
            );
        }
        assert!(
            runner.argv().is_empty(),
            "{} 次拒收一次机器都不该碰：{:?}",
            cases.len(),
            runner.argv()
        );
    }

    /// 同步规则集对着的是**磁盘上那份状态**声明的清单，不是面板编辑区里那份。
    /// 所以没存过、或坏到读不出，都是 409：这一刻根本没有清单可同步。
    #[test]
    fn syncing_without_a_readable_state_is_a_conflict_not_a_guess() {
        let layout = fixture("sync-nostate");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(&Request::post("/api/rule-sets/sync").authorized(TOKEN));
        assert_eq!(got.status, 409, "{}", got.body);
        assert!(got.body.contains("先在面板上保存一次"), "{}", got.body);

        let path = PathBuf::from(layout.state_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"nodes\": 5}").unwrap();
        let broken = app.handle(&Request::post("/api/rule-sets/sync").authorized(TOKEN));
        assert_eq!(broken.status, 409, "{}", broken.body);
        assert!(runner.argv().is_empty(), "两次 409 都不该起进程");
    }

    /// 声明了本地产物而文件不在：这一步修不了（ZBox 不去猜别人的 `.srs` 从哪来），
    /// 所以是 409 而不是 500，回执要报出**该把文件放哪**。报告跟着错误一起出去——
    /// 只说失败不说"另外几条已经装了盘"，表现就是"点了更新没反应"。
    #[test]
    fn a_declared_local_rule_set_that_is_absent_is_a_409_that_names_the_directory() {
        let layout = fixture("ruleset-gap");
        let mut state = DesiredState::baseline();
        state.rule_sets = vec![RuleSet::Local {
            tag: "mine".into(),
            path: "rule-set/mine.srs".into(),
        }];
        put_state(&layout, &state);
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let gap = app.handle(&Request::post("/api/rule-sets/sync").authorized(TOKEN));
        assert_eq!(gap.status, 409, "{}", gap.body);
        let body = gap.value();
        let detail = body["error"].as_str().unwrap_or_default();
        assert!(detail.contains("mine（rule-set/mine.srs）"), "{detail}");
        assert!(detail.contains(&layout.rule_set_dir()), "{detail}");
        assert_eq!(body["report"]["local_missing"].as_array().unwrap().len(), 1);
        assert!(
            runner.argv().is_empty(),
            "全本地的清单一条命令都不该起：{:?}",
            runner.argv()
        );

        // 把文件放上去之后同一条请求就该是 200，且回执说清它只是"在位"，没有重抓
        let dir = PathBuf::from(layout.rule_set_dir());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("mine.srs"), "不是真产物，形状对就行").unwrap();
        let ok = app.handle(&Request::post("/api/rule-sets/sync").authorized(TOKEN));
        assert_eq!(ok.status, 200, "{}", ok.body);
        let ok = ok.value();
        assert_eq!(ok["synced"], true);
        assert_eq!(ok["report"]["kept"], json!(["mine"]));
        assert_eq!(ok["report"]["local_missing"], json!([]));
    }

    /// 抓取真失败（源挂了、下载器非零退出）是 500，不是 409——那一类要人去查网络，
    /// 而不是去改声明。**部分结果照样要出去**：另外那条本地声明其实已经核过在位了，
    /// 只报错误就把它吞掉，面板上看到的还是"点了更新没反应"。
    #[test]
    fn a_hard_sync_failure_is_a_500_that_still_carries_the_part_that_landed() {
        let layout = fixture("ruleset-hard");
        let mut state = DesiredState::baseline();
        state.rule_sets = vec![
            RuleSet::Local {
                tag: "mine".into(),
                path: "rule-set/mine.srs".into(),
            },
            RuleSet::Remote {
                tag: "dead".into(),
                url: "https://rules.example.invalid/dead.json".into(),
                initial_path: "rule-set/dead.srs".into(),
                update_interval: "168h".into(),
            },
        ];
        put_state(&layout, &state);
        let dir = PathBuf::from(layout.rule_set_dir());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("mine.srs"), "在位").unwrap();
        // 下载器（curl 探测失败 -> wget）非零退出
        let runner = Scripted::with_results(vec![exec(1, "Connection refused\n")]);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(&Request::post("/api/rule-sets/sync").authorized(TOKEN));
        assert_eq!(got.status, 500, "{}", got.body);
        let body = got.value();
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("rules.example.invalid"),
            "500 要点名是哪一条源挂了：{}",
            got.body
        );
        assert_eq!(body["report"]["kept"], json!(["mine"]), "{body:?}");
        assert!(!runner.argv().is_empty(), "这一条是真的去抓过才失败的");
    }

    // ---------- U3 订阅导入 ----------

    /// 两条形状真实的订阅链接，凭据是编出来的。名字故意用中文：机场订阅里这是常态，
    /// 而 `gen::dae` 会把不认的标识符换成 `node<N>` 标签，所以这一份顺带钉住那条路。
    const SUB_LINK_A: &str =
        "vless://aaaaaaaa-1111-1111-1111-111111111111@1.2.3.4:443?security=tls&sni=a.example#东京-A";
    const SUB_LINK_B: &str = "trojan://SuperSecret123@5.6.7.8:8443#圣何塞-B";
    const SUB_URL: &str = "https://airport.example.invalid/sub?token=abc";

    fn sub_body() -> String {
        format!("{SUB_LINK_A}\n{SUB_LINK_B}\n")
    }

    fn sub_request(body: &str) -> Vec<u8> {
        json!({ "url": body }).to_string().into_bytes()
    }

    /// guest 逐字原文（VMnet8 NAT 那台 Alpine，单网卡）。与 `svc::topology::tests` 里的
    /// `ROUTE` / `ONE_NIC` 是同一次抓取——那两个常量住在那一层的私有 `mod tests` 里，
    /// 跨模块拿不到，所以这里留一份副本，改动时两处一起改。
    /// `-o` 把续行折成"反斜杠 + 一串空格"，这个形状不许"顺手整理"掉。
    const NIC_ROUTE: &str = "default via 192.168.219.2 dev eth0 metric 202 \n";
    const NIC_ONE: &str = "\
1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: eth0    inet 192.168.219.128/24 brd 192.168.219.255 scope global eth0\\       valid_lft forever preferred_lft forever
";

    /// 同一台 guest、netns 拓扑加载着的时候抓的：局域网侧是桥 `zbox-lan0`，桥地址**没有 `brd`
    /// 段**，所以解析不许依赖它存在。上面那三条"形状判不出"的用例就是在它后面再补一张卡。
    const NIC_TWO: &str = "\
1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
2: eth0    inet 192.168.219.128/24 brd 192.168.219.255 scope global eth0\\       valid_lft forever preferred_lft forever
8: zbox-lan0    inet 10.99.0.1/24 scope global zbox-lan0\\       valid_lft forever preferred_lft forever
";

    /// 测试里的下载器 + 网卡形状。
    ///
    /// `curl --version` 那一次探测答"没有"（guest 实测只有 busybox wget），wget 那一次把响应体
    /// 写进 `-O` 的落点。它把每一条 argv 都记下来，好让"这一步不许起进程"那几条断言有一个
    /// 可数的对象。
    ///
    /// `ip` 那两条是 `subscription()` 在新机器真要写盘**之前**的那一次网卡探测
    /// （`Topology::detect`，两条按 `route` → `addr` 的顺序发）。默认答"这台机器上没有 ip"——
    /// 那是 Windows 开发机的形状，探测降级、不点亮假故障；要验"探到了就按本机填"那一条用
    /// [`Fetcher::with_nic`] 喂 guest 原文。
    struct Fetcher {
        body: Vec<u8>,
        nic: Option<(String, String)>,
        calls: std::cell::RefCell<Vec<Vec<String>>>,
    }

    impl Fetcher {
        fn new(body: &str) -> Self {
            Self {
                body: body.as_bytes().to_vec(),
                nic: None,
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }

        /// 带网卡现场的一份：`route` 是 `ip -4 route show default` 的 stdout，
        /// `addr` 是 `ip -4 -o addr show` 的 stdout。
        fn with_nic(body: &str, route: &str, addr: &str) -> Self {
            Self {
                nic: Some((route.into(), addr.into())),
                ..Fetcher::new(body)
            }
        }

        fn argv(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }

        fn not_found(&self, program: &str) -> Exec {
            Exec {
                code: 127,
                stdout: String::new(),
                stderr: format!("sh: {program}: not found"),
            }
        }
    }

    impl Runner for Fetcher {
        fn exec(&self, argv: &[&str]) -> Result<Exec, String> {
            self.calls
                .borrow_mut()
                .push(argv.iter().map(|a| a.to_string()).collect());
            if argv.contains(&"--version") {
                return Ok(self.not_found("curl"));
            }
            if argv.first().copied() == Some("ip") {
                let index = if argv.contains(&"route") { 0 } else { 1 };
                return Ok(match &self.nic {
                    Some((route, addr)) => Exec {
                        code: 0,
                        stdout: if index == 0 { route.clone() } else { addr.clone() },
                        stderr: String::new(),
                    },
                    None => self.not_found("ip"),
                });
            }
            assert_eq!(argv.first().copied(), Some("wget"), "{argv:?}");
            let out = argv
                .iter()
                .position(|a| *a == "-O")
                .and_then(|i| argv.get(i + 1))
                .expect("wget 的落点在 -O 后面");
            fs::write(out, &self.body).map_err(|e| e.to_string())?;
            Ok(Exec {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        fn exec_stdin(&self, argv: &[&str], _: &[u8]) -> Result<Exec, String> {
            Err(format!("订阅抓取不走 stdin：{argv:?}"))
        }
        fn until_started(
            &self,
            argv: &[&str],
            _: &std::path::Path,
            _: Option<&std::path::Path>,
            _: std::time::Duration,
        ) -> Result<(), String> {
            Err(format!("订阅导入不预跑任何东西：{argv:?}"))
        }
    }

    /// 主路第一步的现场：新开的一台机器，磁盘上还没有状态文件，面板上只填了一串订阅链接。
    /// 这一按写的就是第一份状态，而回执**只给名字与计数**——UUID 与密码留在服务端。
    /// 网卡那几格走的是本机探到的形状，不是出厂默认的 `ens33`（对照的三条用例在本节末尾）。
    #[test]
    fn the_first_subscription_on_a_fresh_box_becomes_the_first_state() {
        let layout = fixture("sub-fresh");
        let state = DesiredState::baseline();
        let fetcher = Fetcher::with_nic(&sub_body(), NIC_ROUTE, NIC_ONE);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &fetcher, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(
            &Request::post_body("/api/subscription", &sub_request(SUB_URL)).authorized(TOKEN),
        );
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        assert_eq!(body["imported"], json!(true));
        assert_eq!(body["written"], json!(true));
        assert_eq!(body["format"], json!("节点链接列表"));
        assert_eq!(body["parsed"], json!(2));
        assert_eq!(body["skipped"], json!(0));
        assert_eq!(body["added"], json!(["东京-A", "圣何塞-B"]));
        assert_eq!(body["already_there"], json!([]));
        assert_eq!(body["total_nodes"], json!(2));
        assert_eq!(
            body["reload_pending"],
            json!(true),
            "写完就是「期望态与已知良好那份不一致」，这一格不能沉默"
        );
        assert_eq!(body["warning"], json!(RELOAD_WARNING));
        let message = body["message"].as_str().unwrap_or_default();
        assert!(message.contains("还没下发"), "{message}");
        assert!(message.contains("让它生效"), "{message}");

        // 凭据一条都不出去：UUID、密码、还有订阅链接里那串 token
        for secret in [
            "aaaaaaaa-1111-1111-1111-111111111111",
            "SuperSecret123",
            "token=abc",
        ] {
            assert!(!got.body.contains(secret), "回执里出现了 {secret}：{}", got.body);
        }

        // 落盘的是同一个形状：读回来两条，顺序是订阅里的顺序
        let read_back = state::read(&PathBuf::from(layout.state_path())).expect("该读得回来");
        assert_eq!(read_back.nodes.len(), 2);
        assert_eq!(read_back.nodes[0].name, "东京-A");
        assert_eq!(read_back.nodes[1].name, "圣何塞-B");
        // 暂存件是订阅原文，读完必须删掉
        assert!(
            !PathBuf::from(layout.state_dir())
                .join("subscription.download")
                .exists(),
            "暂存件留下来了"
        );
        // 落盘的那一份网卡是**本机探到的形状**，不是出厂默认的 ens33 / 192.168.1.0/24。
        // 这一条是 #99 的全部理由：新机器上这一按写的就是第一份状态，此后 `svc::topology`
        // 的规矩是"显式写过就不再覆盖"，所以默认值一旦落盘就会一路生成进 dae 的 lan_interface。
        assert_eq!(read_back.network.lan_interface, "eth0");
        assert_eq!(read_back.network.lan_cidr, "192.168.219.0/24");
        assert_eq!(read_back.network.lan_address, "192.168.219.128");
        assert_eq!(read_back.network.wan_interface, "eth0");
        // 一次探 curl + 一次抓取 + 两条探网卡，没有第五步。程序名逐个列出来，
        // 多一个别的（尤其下发那几条动词）就叫人看得见。
        assert_eq!(
            fetcher
                .argv()
                .iter()
                .map(|argv| argv.first().cloned().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["curl", "wget", "ip", "ip"],
            "{:?}",
            fetcher.argv()
        );
        // 顺序也是断言：两条探测排在抓取**之后**——链接读不懂时一个 `ip` 都不该跑
        let calls = fetcher.argv();
        assert_eq!(
            calls[2].join(" "),
            "ip -4 route show default",
            "{calls:?}"
        );
        assert_eq!(calls[3].join(" "), "ip -4 -o addr show", "{calls:?}");
        let note = body["network_note"].as_str().unwrap_or_default();
        for fragment in [
            "第一份状态",
            "局域网 eth0（段 192.168.219.0/24，本机地址 192.168.219.128）、上游 eth0",
            "设备把 DNS 指到这个地址就行",
            "「网络 · 路线 · 端口」",
        ] {
            assert!(note.contains(fragment), "回执那句少了「{fragment}」：{note}");
        }
        assert!(
            message.contains("局域网 eth0"),
            "网卡那一句要和节点那一句同排在一条 message 里，面板上只有那一格：{message}"
        );
    }

    /// 同一条链接再按一次：磁盘上那一份**一个字节都不许变**。重写一份内容一样的文件会把
    /// 别人的编辑挤成一次 412，而这一按什么都没改，不该留下任何痕迹。
    #[test]
    fn reimporting_the_same_subscription_touches_no_disk() {
        let layout = fixture("sub-twice");
        let mut state = DesiredState::baseline();
        state.network.lan_address = "192.0.2.10".into();
        state.nodes = subscription::parse(sub_body().as_bytes())
            .expect("夹具该读得通")
            .nodes;
        put_state(&layout, &state);
        let path = PathBuf::from(layout.state_path());
        let before = fs::read(&path).unwrap();

        let fetcher = Fetcher::new(&sub_body());
        let svc = Svc::new(&layout, InitSystem::OpenRC, &fetcher, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let got = app.handle(
            &Request::post_body("/api/subscription", &sub_request(SUB_URL)).authorized(TOKEN),
        );
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        assert_eq!(body["written"], json!(false));
        assert_eq!(body["added"], json!([]));
        assert_eq!(body["already_there"], json!(["东京-A", "圣何塞-B"]));
        assert_eq!(body["total_nodes"], json!(2));
        // 没写盘就没有新的指纹，回执上的 etag 仍是磁盘那一份
        assert_eq!(
            body["etag"].as_str().unwrap_or_default(),
            store::bytes_digest(&before),
            "回执该给磁盘上那一份的指纹"
        );
        let message = body["message"].as_str().unwrap_or_default();
        assert!(message.contains("一个字都没改"), "{message}");
        assert!(message.contains("订阅与节点"), "{message}");
        assert_eq!(fs::read(&path).unwrap(), before, "磁盘上那一份被动过");
    }

    /// §11.3 D 那一按：**带回执的自动**。新开的一台机器导入订阅，除了节点还要把「默认怎么走」
    /// 设成智能分流，而动的是哪几格必须在屏幕上看得见——这一条存在的理由就是
    /// "只有节点、默认还是全直连"那个自相矛盾的第一屏。
    #[test]
    fn a_fresh_import_also_sets_the_default_to_smart_split() {
        let layout = fixture("sub-smart");
        let state = DesiredState::baseline();
        let fetcher = Fetcher::new(&sub_body());
        let svc = Svc::new(&layout, InitSystem::OpenRC, &fetcher, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(
            &Request::post_body("/api/subscription", &sub_request(SUB_URL)).authorized(TOKEN),
        );
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        assert_eq!(body["default_set"], json!(true));
        assert_eq!(body["default_group"], json!("auto"));
        let note = body["default_note"].as_str().unwrap_or_default();
        for fragment in [
            "中国大陆的域名与 IP 直连",
            "其余走 auto",
            "2 个出口，每 3m 测一次速自动挑一个",
            "要改回来点这里",
        ] {
            assert!(note.contains(fragment), "回执那句少了「{fragment}」：{note}");
        }
        let message = body["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("要改回来点这里"),
            "这一句要和节点那句同排在一条 message 里，面板上只有 `#sub-out` 那一格：{message}"
        );

        // 落盘的是同一个判断，不是只有回执好看：五格读回来都在
        let read_back = state::read(&PathBuf::from(layout.state_path())).expect("该读得回来");
        assert_eq!(
            read_back.policy.final_outbound,
            FinalOutbound::Proxy("auto".into())
        );
        assert!(read_back.policy.private_direct);
        assert!(read_back.policy.cn_direct);
        assert_eq!(read_back.groups.len(), 1);
        assert_eq!(
            read_back.groups[0].members,
            vec!["东京-A".to_string(), "圣何塞-B".to_string()]
        );
        assert_eq!(
            read_back.dns.overseas,
            vec![crate::model::config::overseas_upstream("auto")],
            "境外上游空着的话 oxidns 整份渲染失败、医生报 Fatal"
        );
        assert_eq!(read_back.rule_sets.len(), 3, "这一档引用的三条声明");
        // 节点仍然只有订阅那两条，顺序也没被这一档动过
        assert_eq!(read_back.nodes.len(), 2);
        // 这一档一个外部命令都不许多要：还是 curl 探测 + 抓取 + 两条探网卡
        assert_eq!(fetcher.argv().len(), 4, "{:?}", fetcher.argv());
    }

    /// `.dat` 在位与不在位是两句话，而真实机器上给哪一支不由测试决定（开发机上那三个目录
    /// 根本不存在，guest 上有没有取决于装过什么），所以判据是**注入**进来的一件事。
    /// 缺数据那一支不许写成"照样能跑，只是分不出来"——实测是装载那一关过不去。
    #[test]
    fn the_smart_split_note_changes_its_wording_when_the_geo_data_is_missing() {
        let on_disk = smart_split_note("auto", 7, true);
        assert!(on_disk.contains("在位"), "{on_disk}");
        assert!(!on_disk.contains("还没探到"), "{on_disk}");
        assert!(on_disk.contains("其余走 auto（7 个出口"), "{on_disk}");

        let missing = smart_split_note("auto-2", 3, false);
        for fragment in [
            "geoip.dat 与 geosite.dat",
            "还没探到",
            "不会自己去下载",
            "其余走 auto-2",
            "要改回来点这里",
        ] {
            assert!(
                missing.contains(fragment),
                "缺数据那一支少了「{fragment}」：{missing}"
            );
        }
        // 撞名让位之后的名字要出现在句子里，否则回执说的和磁盘上的不是同一个组
        assert!(!missing.contains("其余走 auto（"), "{missing}");
    }

    /// 磁盘上已经有状态：那一档是人自己选过的，这一按**不许**替他改。
    /// 默认全直连的那一份，导入完还是全直连，回执也不带那一句"我替你设了"。
    #[test]
    fn a_second_import_never_moves_the_default_outbound() {
        let layout = fixture("sub-keep-default");
        let mut state = DesiredState::baseline();
        state.nodes = subscription::parse(sub_body().as_bytes())
            .expect("夹具该读得通")
            .nodes
            .into_iter()
            .take(1)
            .collect();
        put_state(&layout, &state);

        let fetcher = Fetcher::new(&sub_body());
        let svc = Svc::new(&layout, InitSystem::OpenRC, &fetcher, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let got = app.handle(
            &Request::post_body("/api/subscription", &sub_request(SUB_URL)).authorized(TOKEN),
        );
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        assert_eq!(body["added"], json!(["圣何塞-B"]), "这一按确实新增了一条");
        assert_eq!(body["default_set"], json!(false));
        assert_eq!(body["default_group"], Value::Null);
        assert_eq!(body["default_note"], Value::Null);

        let read_back = state::read(&PathBuf::from(layout.state_path())).expect("该读得回来");
        assert_eq!(read_back.policy.final_outbound, FinalOutbound::Direct);
        assert!(!read_back.policy.cn_direct);
        assert!(read_back.groups.is_empty());
        assert!(read_back.rule_sets.is_empty());
        assert_eq!(read_back.nodes.len(), 2);
    }

    /// 同一台机器上兜底组已经是智能分流、新导入的名字却不在成员名单里——
    /// 这一批不会走代理。不静默把人编辑过的成员顶掉，但必须点名说出来。
    #[test]
    fn new_nodes_outside_the_fallback_group_are_named_not_merged() {
        let layout = fixture("sub-group-members");
        let mut state = DesiredState::baseline();
        state.nodes = subscription::parse(sub_body().as_bytes())
            .expect("夹具该读得通")
            .nodes
            .into_iter()
            .take(1)
            .collect();
        state.policy.final_outbound = FinalOutbound::Proxy("auto".into());
        state.policy.cn_direct = true;
        state.groups = vec![Group {
            tag: "auto".into(),
            kind: GroupKind::Urltest {
                url: "http://www.gstatic.com/generate_204".into(),
                interval: "3m".into(),
                tolerance: 50,
            },
            members: vec!["东京-A".into()],
            default: None,
        }];
        state.dns.overseas = vec![crate::model::config::overseas_upstream("auto")];
        state.rule_sets = crate::model::config::smart_split_rule_sets();
        put_state(&layout, &state);

        let fetcher = Fetcher::new(&sub_body());
        let svc = Svc::new(&layout, InitSystem::OpenRC, &fetcher, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let got = app.handle(
            &Request::post_body("/api/subscription", &sub_request(SUB_URL)).authorized(TOKEN),
        );
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        assert_eq!(body["default_set"], json!(false), "那一档不动");
        let note = body["default_note"].as_str().unwrap_or_default();
        for fragment in ["圣何塞-B", "不在兜底组 auto 的成员里", "不会走代理", "「策略组」"] {
            assert!(note.contains(fragment), "少了「{fragment}」：{note}");
        }
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("不在兜底组"),
            "面板上只有 `#sub-out` 那一格，这句话得排在 message 里"
        );

        let read_back = state::read(&PathBuf::from(layout.state_path())).expect("该读得回来");
        assert_eq!(read_back.nodes.len(), 2, "节点照常追加");
        assert_eq!(
            read_back.groups[0].members,
            vec!["东京-A".to_string()],
            "成员名单是人编辑过的，这一按不许替人补"
        );
    }

    /// 磁盘那份读不出来是 409，而且**一个进程都不起**——不许为一个坏掉的状态去联网。
    /// 新机器没有状态文件走的是另一支（上面那条），所以"缺"与"坏"在这里也是两个界面。
    #[test]
    fn a_broken_state_on_disk_is_a_conflict_and_no_fetch_starts() {
        let layout = fixture("sub-broken");
        let state = DesiredState::baseline();
        let path = PathBuf::from(layout.state_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"nodes\": 5}").unwrap();

        let runner = Scripted::with_results(vec![exec(0, "")]);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(
            &Request::post_body("/api/subscription", &sub_request(SUB_URL)).authorized(TOKEN),
        );
        assert_eq!(got.status, 409, "{}", got.body);
        assert!(runner.argv().is_empty(), "连下载器都不该探：{:?}", runner.argv());
        assert_eq!(fs::read(&path).unwrap(), b"{\"nodes\": 5}", "409 不许顺手改写");
    }

    /// 三种失败三个码，因为对面要做的动作不同：**链接那一格不成形状**（400，重粘）、
    /// **订阅内容读不成节点**（400，换订阅格式）、**抓取本身坏了**（500，去查网络）。
    /// 把 404 报成"链接不对"会让人反复重粘，把 Clash 配置报成网络故障会让人反复重按。
    #[test]
    fn a_bad_link_a_foreign_body_and_a_dead_fetch_get_three_different_receipts() {
        let token = token();
        let state = DesiredState::baseline();

        // ① 不是 http(s) 的链接：拦在门前，一个下载器都不探
        let layout = fixture("sub-scheme");
        let runner = Scripted::with_results(vec![exec(0, "")]);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let app = App::new(&svc, &token);
        let got = app.handle(
            &Request::post_body(
                "/api/subscription",
                br#"{"url":"file:///etc/passwd"}"#,
            )
            .authorized(TOKEN),
        );
        assert_eq!(got.status, 400, "{}", got.body);
        assert!(got.body.contains("http"), "{}", got.body);
        assert!(
            !got.body.contains("/etc/passwd"),
            "回执不许把别人机器上的路径回显出去：{}",
            got.body
        );
        assert!(runner.argv().is_empty(), "{:?}", runner.argv());
        assert!(!PathBuf::from(layout.state_path()).exists());

        // ② Clash 的配置：抓回来了，但读不成节点
        let layout = fixture("sub-clash");
        let fetcher = Fetcher::new("mixed-port: 7890\nproxies:\n  - name: a\n");
        let svc = Svc::new(&layout, InitSystem::OpenRC, &fetcher, &state.ports);
        let app = App::new(&svc, &token);
        let got = app.handle(
            &Request::post_body("/api/subscription", &sub_request(SUB_URL)).authorized(TOKEN),
        );
        assert_eq!(got.status, 400, "{}", got.body);
        assert!(got.body.contains("Clash"), "{}", got.body);
        assert!(got.body.contains("订阅格式"), "{}", got.body);
        // 读不懂就走：只跑那两条下载命令（curl 的版本探测 + wget），网卡探测一步都不该跑。
        // 这一句钉的是 `subscription()` 里"探测排在所有早退之后"那个位置——挪到前面去，
        // 这里就多两条 `ip`；而那两条在 Windows 开发机上是"为了一个不会用上的读数去起进程"。
        assert_eq!(
            fetcher.argv().len(),
            2,
            "读不懂的正文后面一步都不该多走：{:?}",
            fetcher.argv()
        );
        assert!(!PathBuf::from(layout.state_path()).exists(), "读不懂不该留下状态文件");
        assert!(
            !PathBuf::from(layout.state_dir())
                .join("subscription.download")
                .exists(),
            "订阅原文不留暂存件，读不懂也一样"
        );

        // ③ 下载器非零退出：网络那一侧
        let layout = fixture("sub-dead");
        let runner = Scripted::with_results(vec![
            exec(127, "sh: curl: not found\n"),
            exec(
                22,
                "wget: server returned error: HTTP/1.1 404 Not Found\n",
            ),
        ]);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let app = App::new(&svc, &token);
        let got = app.handle(
            &Request::post_body("/api/subscription", &sub_request(SUB_URL)).authorized(TOKEN),
        );
        assert_eq!(got.status, 500, "{}", got.body);
        assert!(got.body.contains("抓取订阅失败"), "{}", got.body);
        assert!(got.body.contains("404"), "{}", got.body);
        assert!(!PathBuf::from(layout.state_path()).exists());
        // 抓取坏了就走 500 那一条路，网卡探测排在"真要写盘"那一步之后：这里一步都不该多走
        // （多走的那一步在 Windows 开发机上是 `ip` 不存在，在面板上则是一格没人要的读数）
        assert_eq!(runner.argv().len(), 2, "{:?}", runner.argv());
    }

    /// 合并之后仍然要过整份 `validate()`。这一支是真能撞上的：磁盘上有人把策略组起名
    /// 和机场节点一样（组名与节点重名是这条界管的事），追加进去的那条就会顶穿它。
    /// 写下去的是一份再也读不回来的状态——那时整个面板都开不了，不只这一屏。
    #[test]
    fn a_merge_that_breaks_the_whole_state_is_refused_and_leaves_the_disk_alone() {
        let layout = fixture("sub-collide");
        let mut state = DesiredState::baseline();
        state.network.lan_address = "192.0.2.10".into();
        state.groups = vec![Group {
            tag: "东京-A".into(),
            kind: GroupKind::Selector,
            members: vec!["direct".into()],
            default: Some("direct".into()),
        }];
        put_state(&layout, &state);
        let path = PathBuf::from(layout.state_path());
        let before = fs::read(&path).unwrap();

        let fetcher = Fetcher::new(&sub_body());
        let svc = Svc::new(&layout, InitSystem::OpenRC, &fetcher, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        let got = app.handle(
            &Request::post_body("/api/subscription", &sub_request(SUB_URL)).authorized(TOKEN),
        );
        assert_eq!(got.status, 400, "{}", got.body);
        assert!(got.body.contains("磁盘上那一份没动"), "{}", got.body);
        assert!(got.body.contains("重名"), "{}", got.body);
        assert_eq!(fs::read(&path).unwrap(), before, "拒收之后磁盘该一个字节都不差");
    }

    /// 追加那一步的身份规则：**按名字认行**，与 `nodes[]` 编辑器同一套。
    /// 已经在的那条保持磁盘上那一份原样——包括人在面板上换过的地址与凭据，
    /// 否则"重新导入一次"就会把用户改过的东西顶回订阅里的旧值。
    #[test]
    fn appending_keeps_the_disk_copy_of_a_name_that_is_already_there() {
        let parsed = subscription::parse(sub_body().as_bytes())
            .expect("夹具该读得通")
            .nodes;
        let mut mine = parsed[0].clone();
        mine.server = "198.51.100.7".into();
        let mut state = DesiredState::baseline();
        state.nodes = vec![mine];

        let (added, already) = append_nodes(&mut state, parsed);
        assert_eq!(added, vec!["圣何塞-B".to_string()]);
        assert_eq!(already, vec!["东京-A".to_string()]);
        assert_eq!(state.nodes.len(), 2);
        assert_eq!(state.nodes[0].server, "198.51.100.7", "磁盘上那条被订阅里的旧值顶掉了");
        assert_eq!(state.nodes[1].name, "圣何塞-B", "新增的排在末尾");
    }

    /// 那两句人话是"接下来按哪个按钮"的唯一依据，所以它住服务端而不是面板：
    /// **有没有真写盘**、**要不要去下发**、**有几条没读懂**，三件事一次说全。
    #[test]
    fn the_import_receipt_says_whether_the_data_plane_has_to_move() {
        let already = vec!["东京-A".to_string()];
        let wrote = import_message("节点链接列表", 3, 1, &already, false);
        assert!(wrote.contains("读到 3 个节点"), "{wrote}");
        assert!(wrote.contains("1 个磁盘上已经有了"), "{wrote}");
        assert!(wrote.contains("还没下发"), "{wrote}");
        assert!(wrote.contains("另有 1 条链接 ZBox 读不懂"), "{wrote}");

        let kept = import_message("Base64 打包的节点链接", 1, 0, &already, true);
        assert!(kept.contains("一个字都没改"), "{kept}");
        assert!(kept.contains("订阅与节点"), "{kept}");
        assert!(
            !kept.contains("还没下发"),
            "什么都没写的时候不该让人去找下发按钮：{kept}"
        );
        assert!(!kept.contains("读不懂"), "没有跳过就别提：{kept}");
    }

    // ---------- #99 网卡形状：面板的生成路径也要跟上 ----------

    /// 这台机器上压根没有 `ip`（Windows 开发机、chroot 里跑面板）。
    ///
    /// 这一支是**降级不是故障**：状态照写（节点是那一步的正题），网卡那几格保留出厂默认，
    /// 回执明说"没能按本机填"并把去处指出来。把"没探到"写成"配错了"是 `Reading` 那条约定
    /// 反对的事，而拿探到的值去覆盖人显式写过的网卡是 `svc::topology` 开头那条硬规则反对的事。
    #[test]
    fn a_box_without_ip_still_gets_a_first_state_but_not_a_fake_nic() {
        let layout = fixture("sub-no-ip");
        let state = DesiredState::baseline();
        // Fetcher::new 的 nic 是 None：两条 `ip` 答 127 not found
        let fetcher = Fetcher::new(&sub_body());
        let svc = Svc::new(&layout, InitSystem::OpenRC, &fetcher, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(
            &Request::post_body("/api/subscription", &sub_request(SUB_URL)).authorized(TOKEN),
        );
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        assert_eq!(body["written"], json!(true), "节点那一步不该被网卡探不到拖成失败");
        let note = body["network_note"].as_str().unwrap_or_default();
        assert!(note.contains("没能按本机填"), "{note}");
        assert!(note.contains("沿用 state 里的值"), "降级那句要照抄 fill 的措辞：{note}");
        assert!(note.contains("ens33"), "要指出现在写的是哪一张：{note}");
        assert!(
            note.contains("192.168.1.0/24"),
            "出厂默认的段也要照给，人才能一眼看出这一格还没定：{note}"
        );
        assert!(note.contains("「网络 · 路线 · 端口」"), "{note}");

        let read_back = state::read(&PathBuf::from(layout.state_path())).expect("该读得回来");
        assert_eq!(
            read_back.network,
            Network::default(),
            "探不到的时候一个网名字节都不许改"
        );
        assert_eq!(read_back.nodes.len(), 2);
        // 探测确实跑了，只是答"没有这个命令"
        assert_eq!(
            fetcher
                .argv()
                .iter()
                .map(|argv| argv.first().cloned().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["curl", "wget", "ip", "ip"]
        );
    }

    /// 命令都跑成了，但形状判不出（几张卡都带段，机器上不止一张局域网形状的网卡）。
    ///
    /// `svc::topology` 的规矩是**不猜**，所以这一支落到磁盘上的仍然是出厂默认，而回执必须把
    /// 候选清单原样带回来——那正是"该填哪一张"的下一步依据。这里最坏的形状是：悄悄挑一张、
    /// 写下去、面板再说"这是这台机器的局域网"。
    #[test]
    fn an_ambiguous_nic_shape_is_named_in_full_and_written_down_as_undecided() {
        let layout = fixture("sub-ambiguous");
        let state = DesiredState::baseline();
        // eth0 是上游（默认路由那张），另外两张都带段：判据不许在这两张里挑
        let fetcher = Fetcher::with_nic(
            &sub_body(),
            NIC_ROUTE,
            &format!(
                "{}\
7: lan1    inet 10.77.0.1/24 scope global lan1\\       valid_lft forever preferred_lft forever
",
                NIC_TWO
            ),
        );
        let svc = Svc::new(&layout, InitSystem::OpenRC, &fetcher, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(
            &Request::post_body("/api/subscription", &sub_request(SUB_URL)).authorized(TOKEN),
        );
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        let note = body["network_note"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(note.contains("判不出来"), "{note}");
        assert!(note.contains("zbox-lan0=10.99.0.0/24"), "{note}");
        assert!(note.contains("lan1=10.77.0.0/24"), "{note}");
        assert!(note.contains("不猜"), "{note}");

        let read_back = state::read(&PathBuf::from(layout.state_path())).expect("该读得回来");
        assert_eq!(read_back.network, Network::default(), "判不出的时候不许挑一张");
    }

    /// `GET /api/status` 的「本机网卡」那一格：探到了就给全套读数，**包括默认网关**。
    /// 这一格是现场那一句，与 `GET /api/state` 的期望态分家——两个都叫 `network` 但一个是磁盘、
    /// 一个是本机，面板把它们混说就是 §11.7 那句"不能静默下发一份指向不存在的网卡的配置"的反面。
    #[test]
    fn status_reports_the_nic_shape_probed_from_this_very_machine() {
        let layout = fixture("net-status");
        let state = edited();
        let box_ = NicBox::with_nic(NIC_ROUTE, NIC_ONE);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &box_, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(&Request::get("/api/status").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        let network = &body["network"];
        assert_eq!(network["ok"], json!(true), "{}", got.body);
        // guest 上这是单网卡旁路由：进出同卡，局域网与上游都是 eth0
        assert_eq!(network["lan_interface"], json!("eth0"));
        assert_eq!(network["lan_cidr"], json!("192.168.219.0/24"));
        assert_eq!(network["lan_address"], json!("192.168.219.128"));
        assert_eq!(network["wan_interface"], json!("eth0"));
        assert_eq!(network["gateway"], json!("192.168.219.2"));
        assert_eq!(network["detail"], Value::Null);
        // 一次 status 里那两条探测排在最后发完，而 `addr` 那一条全场只发一次
        // （`observe` 只读默认路由那张卡的名字，它不需要地址）。
        // 更要紧的是这一条路径上没有任何写动作：读现场那一格不许顺手 restart 或 `nft -f`。
        let calls = box_.argv();
        assert_eq!(
            calls[calls.len() - 2].join(" "),
            "ip -4 route show default",
            "{calls:?}"
        );
        assert_eq!(
            calls[calls.len() - 1].join(" "),
            "ip -4 -o addr show",
            "{calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|argv| argv.join(" ").contains("-o addr"))
                .count(),
            1,
            "{calls:?}"
        );
        for argv in &calls {
            let line = argv.join(" ");
            assert!(
                !line.contains("restart") && !line.contains(" -f "),
                "读现场那一格不许带动作：{line}"
            );
        }
    }

    /// 探不到时这一格是 `ok:false` + 那句原因，局域网那三格一律 `null`，而**读到的半截照给**
    /// （上游网卡与网关各自是各自跑出来的，一条命令坏了不连坐另一条）。
    /// `null` 与 `false` 的区别就是面板那句"没探出来"和"探到的是另一张"的区别。
    #[test]
    fn status_degrades_field_by_field_when_the_box_cannot_read_its_nics() {
        let layout = fixture("net-no-ip");
        let state = edited();
        let box_ = NicBox::without_ip();
        let svc = Svc::new(&layout, InitSystem::OpenRC, &box_, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(&Request::get("/api/status").authorized(TOKEN));
        let body = got.value();
        let network = &body["network"];
        assert_eq!(network["ok"], json!(false), "{}", got.body);
        for field in ["lan_interface", "lan_cidr", "lan_address"] {
            assert_eq!(
                network[field], Value::Null,
                "探不到时{field}必须是 null，不能拿默认值顶上去"
            );
        }
        assert!(
            network["detail"].as_str().unwrap_or_default().contains("跑不了"),
            "{network:?}"
        );

        // 同一格的另一支：route 跑成了、addr 跑成了但形状判不出——半截读数照给，不整体作废
        let layout = fixture("net-ambiguous");
        let box_ = NicBox::with_nic(
            NIC_ROUTE,
            &format!(
                "{}\
7: lan1    inet 10.77.0.1/24 scope global lan1\\       valid_lft forever preferred_lft forever
",
                NIC_TWO
            ),
        );
        let svc = Svc::new(&layout, InitSystem::OpenRC, &box_, &state.ports);
        let app = App::new(&svc, &token);
        let network = app
            .handle(&Request::get("/api/status").authorized(TOKEN))
            .value()["network"]
            .clone();
        assert_eq!(network["ok"], json!(false));
        assert_eq!(network["lan_interface"], Value::Null, "判不出就不能编一张");
        assert_eq!(network["wan_interface"], json!("eth0"), "默认路由那条读到了");
        assert_eq!(network["gateway"], json!("192.168.219.2"), "网关那条也读到了");
    }

    /// 诊断面板那一条：磁盘上还是出厂默认、而本机探到的是另一张。
    ///
    /// 判据住在 `doctor::network_shape_finding`，这一层只负责"探到的那份从哪儿来"，
    /// 所以「本机网卡」那一行与诊断结论**不会各说一套**（这正是 §11.7 里那句"宁可明说"的落地）。
    /// 另外两支是它的边界：人显式写过就不抢（面板那一行给读数，人自己比），`ip` 跑不起来
    /// 就不点亮——那是开发机的常态，报出来是一屏假故障。
    #[test]
    fn doctor_names_a_first_state_still_on_the_factory_nic() {
        let token = token();

        // ① 磁盘 = 出厂默认，本机 = eth0 → 报这一条
        let layout = fixture("net-doctor-default");
        let state = DesiredState::baseline();
        let box_ = NicBox::with_nic(NIC_ROUTE, NIC_ONE);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &box_, &state.ports);
        let app = App::new(&svc, &token);
        // 状态文件还没有：诊断读的是基线形状，那一格同样是出厂默认，所以这一条照报
        let got = app.handle(&Request::get("/api/doctor").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        let findings = findings_of(&body);
        let found = findings
            .iter()
            .find(|f| f["code"].as_str() == Some("network_still_default"))
            .unwrap_or_else(|| panic!("出厂默认撞上另一张网卡时该点名：{}", got.body));
        assert_eq!(found["severity"], json!("warn"));
        assert!(
            found["expected"].as_str().unwrap_or_default().contains("eth0"),
            "{found:?}"
        );
        assert!(
            found["actual"]
                .as_str()
                .unwrap_or_default()
                .contains("ens33/段 192.168.1.0/24（本机地址没填）"),
            "要把人现在用的那一份原样说出来：{found:?}"
        );
        assert!(
            found["fix"]
                .as_array()
                .map_or(true, |v| v.is_empty()),
            "这一条给不出可粘贴的命令，那几格是人自己填的：{found:?}"
        );

        // ② 人显式写过 → 不报（机器不跟人的判断抢，读数那一行归那一行说）
        let layout = fixture("net-doctor-explicit");
        let mut state = edited();
        state.network.lan_interface = "br0".into();
        state.network.lan_cidr = "10.10.0.0/24".into();
        let box_ = NicBox::with_nic(NIC_ROUTE, NIC_ONE);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &box_, &state.ports);
        let app = App::new(&svc, &token);
        put_state(&layout, &state);
        let body = app
            .handle(&Request::get("/api/doctor").authorized(TOKEN))
            .value();
        assert!(
            !codes(&body).contains(&"network_still_default".to_string()),
            "显式写过的网卡不该被这一条顶回去：{body:?}"
        );

        // ③ `ip` 跑不起来 → 一个进程都探不出结论，也就不报
        let layout = fixture("net-doctor-no-ip");
        let state = DesiredState::baseline();
        let box_ = NicBox::without_ip();
        let svc = Svc::new(&layout, InitSystem::OpenRC, &box_, &state.ports);
        let app = App::new(&svc, &token);
        let body = app
            .handle(&Request::get("/api/doctor").authorized(TOKEN))
            .value();
        assert!(
            !codes(&body).contains(&"network_still_default".to_string()),
            "开发机上\"没探到\"不许长成\"配错了\"：{body:?}"
        );
    }

    /// 诊断回执里的 `findings`（那一格是数组，坏形状时给空数组，测试只管点名的那几条）。
    fn findings_of(body: &Value) -> Vec<Value> {
        body["findings"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// 只取 `code`，给"这一条在不在"那种断言用（owned，不留借用）。
    fn codes(body: &Value) -> Vec<String> {
        findings_of(body)
            .iter()
            .filter_map(|f| f["code"].as_str().map(|s| s.to_string()))
            .collect()
    }

    /// 一台能读服务状态、又能答网卡形状的机器，专门给 `status` / `doctor` 那三条用。
    ///
    /// 为什么不复用 [`Fetcher`]：那一份是**订阅那一步的现场**，它对"这一步只该起 curl 和 wget"
    /// 是硬的（答错了就 panic），而 `status` 与 `doctor` 本来就要起服务与内核那几条读探针。
    /// 这里其余命令一律 rc=0 + 空 stdout——`observe` 读不出形状就逐字段降级成 `None`，
    /// 正好是这几条用例不关心的那一半。
    struct NicBox {
        route: Option<String>,
        addr: Option<String>,
        calls: std::cell::RefCell<Vec<Vec<String>>>,
    }

    impl NicBox {
        fn with_nic(route: &str, addr: &str) -> Self {
            Self {
                route: Some(route.into()),
                addr: Some(addr.into()),
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }

        /// 这台机器上没有 `ip`（`route` 与 `addr` 都答"跑不了"）
        fn without_ip() -> Self {
            Self {
                route: None,
                addr: None,
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn argv(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }
    }

    impl Runner for NicBox {
        fn exec(&self, argv: &[&str]) -> Result<Exec, String> {
            self.calls
                .borrow_mut()
                .push(argv.iter().map(|a| a.to_string()).collect());
            if argv.first().copied() == Some("ip") {
                // 两条探测各自降级：route 与 addr 是分别跑的，坏一条不连坐另一条
                let field = if argv.contains(&"addr") {
                    &self.addr
                } else {
                    &self.route
                };
                return match field {
                    Some(stdout) => Ok(Exec {
                        code: 0,
                        stdout: stdout.clone(),
                        stderr: String::new(),
                    }),
                    // 这台机器上没有 `ip`：`Topology` 把这一支记成 probe_error（"跑不了"），
                    // 与"跑成了但判不出"是两个界面
                    None => Err("No such file or directory (os error 2)".to_string()),
                };
            }
            // 其余是 `observe` 那几条读探针：答"跑成了、什么都没读出来"，逐字段降级成 None
            Ok(Exec {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        fn exec_stdin(&self, argv: &[&str], _: &[u8]) -> Result<Exec, String> {
            self.exec(argv)
        }
        fn until_started(
            &self,
            argv: &[&str],
            _: &std::path::Path,
            _: Option<&std::path::Path>,
            _: std::time::Duration,
        ) -> Result<(), String> {
            Err(format!("这三条用例不预跑任何东西：{argv:?}"))
        }
    }

    /// 内核没在听就下 nft，局域网侧的表现是 `Connection refused`（schema §11.7），
    /// 所以开劫持的第一道门是探测。**门坏了，一条装载命令都不许跑。**
    #[test]
    fn enabling_hijack_while_the_kernel_is_not_listening_loads_nothing() {
        let layout = fixture("hijack-gate");
        let state = edited();
        let runner = Scripted::with_results(vec![exec(1, "inactive\n")]);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(&Request::post("/api/hijack/enable").authorized(TOKEN));
        assert_eq!(got.status, 409, "{}", got.body);
        assert!(got.body.contains("内核没有在听"), "{}", got.body);
        assert_eq!(
            verbs(&runner.argv()),
            ["status"],
            "探测失败之后不许接着下 nft：{:?}",
            runner.argv()
        );
    }

    /// 门过了才装载，而且顺序仍是"策略路由在前、nft 表在后"（§11.7）。
    /// 回执只报"装载了"，现场读数由面板再问一次 `/api/status`——和 `apply` 同一个形状。
    #[test]
    fn enabling_hijack_loads_policy_routing_before_the_nft_table() {
        let layout = fixture("hijack-on");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(&Request::post("/api/hijack/enable").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        assert_eq!(got.value()["enabled"], true);

        let argv = runner.argv();
        let netstate = argv
            .iter()
            .position(|c| {
                c.first().map(String::as_str) == Some("sh") && c[1].ends_with("netstate.sh")
            })
            .expect("该跑 netstate.sh");
        let loaded = argv
            .iter()
            .position(|c| c.first().map(String::as_str) == Some("nft") && c[1] == "-f")
            .expect("该跑 nft -f");
        assert!(
            netstate < loaded,
            "策略路由必须在 nft 之前：{:?}",
            argv.iter().map(|a| a.join(" ")).collect::<Vec<_>>()
        );
        // 预检（nft -c 加两条 sh -n）排在任何装载命令之前
        let precheck = argv
            .iter()
            .position(|c| c.first().map(String::as_str) == Some("nft") && c[1] == "-c")
            .expect("该有 nft -c 自证");
        assert!(precheck < netstate, "{:?}", argv);
    }

    /// 现场本来就没开过劫持时卸载是**空操作**，而且要说明白是空操作。
    /// 判据是两路探针都答"没加载"；拿一条 `netstate-down.sh 执行失败` 回报"本来就没装"
    /// 是那条对谁都不可读的错误，正是要避免的形状。
    #[test]
    fn disabling_an_absent_layer_is_an_empty_operation_and_says_so() {
        let layout = fixture("hijack-absent");
        let state = edited();
        // observe 的七条读数，按调用顺序：服务状态、ss、nft 表（rc=1=不在）、
        // ip rule（基线三个前缀）、两条 sysctl、默认路由
        let runner = Scripted::with_results(vec![
            exec(0, ""),
            exec(0, &listening(&state)),
            exec(1, "Error: No such file or directory\n"),
            exec(
                0,
                "0:\tfrom all lookup local\n32766:\tfrom all lookup main\n",
            ),
            exec(0, "1\n"),
            exec(0, "2\n"),
            exec(0, "default via 192.168.219.2 dev eth0 metric 202 \n"),
        ]);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(&Request::post("/api/hijack/disable").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        assert_eq!(body["removed"], false, "两路探针都答没加载就该报空操作");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("一条卸载命令都没发"),
            "{body:?}"
        );
        assert!(
            !runner
                .argv()
                .iter()
                .any(|c| c.join(" ").contains("netstate-down.sh")),
            "本来就没装过就不该去跑卸载脚本：{:?}",
            runner.argv()
        );

        // 反过来的形状：nft 表在（rc=0），那就要真撤，而且撤完那句话得带上 sysctl 不被还原
        let layout = fixture("hijack-present");
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let app = App::new(&svc, &token);
        let got = app.handle(&Request::post("/api/hijack/disable").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let body = got.value();
        assert_eq!(body["removed"], true);
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("sysctl 保持原值"),
            "回执要说不被还原的那部分，否则人会以为机器还原干净了：{body:?}"
        );
        let argv = runner.argv();
        assert!(
            argv.last()
                .map(|c| c.join(" ").contains("netstate-down.sh"))
                .unwrap_or(false),
            "{argv:?}"
        );
    }

    /// 给错修订号那句话里要按新到旧列出有的那些，"一个都没有"也要说出口；
    /// 这一句在 CLI 与面板上是同一个定义（`Svc::rollback`）。400 不是 500：
    /// 对面要做的是换一个修订号，不是查数据面。
    #[test]
    fn a_rollback_to_a_revision_that_is_not_there_is_a_400_listing_what_is() {
        let layout = fixture("rollback-none");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        let got = app.handle(
            &Request::post_body("/api/rollback", br#"{"revision":"nope"}"#).authorized(TOKEN),
        );
        assert_eq!(got.status, 400, "{}", got.body);
        assert!(got.body.contains("没有修订 nope"), "{}", got.body);
        assert!(got.body.contains("一个都没有"), "{}", got.body);

        let none = app.handle(&Request::post("/api/rollback").authorized(TOKEN));
        assert_eq!(none.status, 400, "{}", none.body);
        assert!(
            none.body.contains("没有已知良好快照"),
            "没下发过与给错号是两句话：{}",
            none.body
        );
        assert!(runner.argv().is_empty(), "两次 400 都不该动机器");
    }

    /// 回滚成功：不给 `revision` 就是回到已知良好那一份，`how` 报的是**怎么选出来的**，
    /// 而不是让面板再猜一次调用方有没有填框。跑的那几步是提交 -> 重启 -> 探测。
    #[test]
    fn rolling_back_to_the_last_good_revision_commits_reloads_and_probes() {
        let layout = fixture("rollback-good");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);
        put_state(&layout, &state);
        let applied = app
            .handle(&Request::post("/api/apply").authorized(TOKEN))
            .value();
        let revision = applied["revision"]
            .as_str()
            .expect("{applied:?}")
            .to_string();
        runner.calls.borrow_mut().clear();

        // 空对象、null、留空白的框，三种写法都是"没给修订号"= 回到已知良好那一份
        for body in ["{}", r#"{"revision":null}"#, r#"{"revision":"  "}"#] {
            let got =
                app.handle(&Request::post_body("/api/rollback", body.as_bytes()).authorized(TOKEN));
            assert_eq!(got.status, 200, "{body} -> {}", got.body);
            let value = got.value();
            assert_eq!(value["rolled_back"], true);
            assert_eq!(value["revision"], revision.as_str());
            assert_eq!(value["how"], "已知良好");
            assert_eq!(value["last_good"], revision.as_str());
        }
        assert_eq!(
            verbs(&runner.argv()),
            [
                "restart", "status", "-lntu", "restart", "status", "-lntu", "restart", "status",
                "-lntu"
            ],
            "回滚 = 提交现役（写文件，不起进程）+ 重启 + 探测，三次循环各一轮：{:?}",
            runner.argv()
        );

        // 指明那一份：how 要跟着换成"指定"，同一条判定不能两边各猜一次
        let named = app.handle(
            &Request::post_body(
                "/api/rollback",
                format!(r#"{{"revision":"{revision}"}}"#).as_bytes(),
            )
            .authorized(TOKEN),
        );
        assert_eq!(named.status, 200, "{}", named.body);
        assert_eq!(named.value()["how"], "指定");
    }

    // ---------- 观测面「DNS 记录」：两条只读端点 ----------

    use std::cell::RefCell;

    /// 假的解析器观察者。这两条端点的断言**全部**走它。
    ///
    /// 理由不是省事：真那一份（[`dns_api::Live`]）读的是回环上那个口——在开发机上永远连不通，
    /// 而这一轮的 guest 上又真有一个 oxidns 正听着（实测留下的）。两种情况都会把
    /// "这一屏该说什么"变成这台机器的函数，那不叫断言。
    struct FakeDns {
        screen: dns_api::Screen,
        record: Result<Value, String>,
        /// 端点有没有把体里那几个参数一路传到取数那一步
        asked: RefCell<Vec<dns_api::Filter>>,
        ids: RefCell<Vec<u64>>,
        /// 看板那一次刷新的回执。默认 `Tops::default()`（`live:false`、没有原因），
        /// 用它的测试都在验别的格；只有看板那几条显式给一份。
        tops: dns_api::Tops,
        /// 看板那次刷新被调了几次——钉的是"看板自己读聚合，不借解析记录那一页的路"。
        tops_calls: RefCell<usize>,
    }

    /// `/api/control` 实测就带 `config_path`（值取的是真在 guest 上看到的那种落点）。
    /// §7.2 第 1 条要求它出不了面板回执，所以下面拿它当探针去查响应体。
    const LEAK_PROBE: &str = "/var/lib/oxidns/config/oxidns.yaml";

    /// 列表页，实测形状：**每行没有 `steps`**（那只在单条里），所以那一屏必须是两层。
    fn page_two_rows() -> Value {
        json!({
            "ok": true,
            "next_cursor": "1790892955162:3",
            "records": [
                {"id": 5, "created_at_ms": 1790892999000i64, "elapsed_ms": 12,
                 "client_ip": "192.0.2.30", "has_response": true, "rcode": "No Error",
                 "error": null, "answer_count": 1,
                 "questions_json": [{"name": "www.example.com.", "qtype": "A", "qclass": "IN"}],
                 "answers_json": [{"name": "www.example.com.", "type": "A", "data": "1.2.3.4"}]},
                {"id": 3, "created_at_ms": 1790892955162i64, "elapsed_ms": 88,
                 "client_ip": "192.0.2.31", "has_response": false, "rcode": null,
                 "error": "all upstreams failed", "answer_count": 0,
                 "questions_json": [{"name": "chatgpt.com.", "qtype": "AAAA", "qclass": "IN"}],
                 "answers_json": []},
            ]
        })
    }

    /// 一份"管理口连上了、记录也在"的完整读数。
    ///
    /// `running` 是 `reload.running_version`——传 `None` 就是**上游没报这个键**（ reload 块读不到、
    /// 或者对面是还没有这一格的版本），那和"报了一个不相等的"是两件事。
    fn live_screen(running: Option<&str>) -> dns_api::Screen {
        let mut reload = Map::new();
        reload.insert("status".to_string(), json!("idle"));
        if let Some(got) = running {
            reload.insert("running_version".to_string(), json!(got));
        }
        dns_api::Screen {
            live: true,
            detail: None,
            health: json!({
                "status": "ok", "version": "1.6.1", "build_bundle": "full", "uptime_ms": 204409,
                "checks": {"api": "ok", "plugin_init": "ok", "server_startup": "ok"},
                "plugins": {"total": 7, "servers": 2},
            }),
            control: json!({
                "status": "running", "uptime_ms": 206373,
                "config_path": LEAK_PROBE, "reload": reload,
            }),
            page: page_two_rows(),
            stats: json!({
                "ok": true, "query_total": 9,
                "stats": [{"kind": "server", "tag": "record", "checked": 9, "matched": 9,
                    "executed": 9, "query_total": 9, "query_share": 1.0}],
            }),
            errors: Vec::new(),
        }
    }

    fn fake(screen: dns_api::Screen) -> FakeDns {
        FakeDns {
            screen,
            record: Err("这一条不在这里验".to_string()),
            asked: RefCell::new(Vec::new()),
            ids: RefCell::new(Vec::new()),
            tops: dns_api::Tops::default(),
            tops_calls: RefCell::new(0),
        }
    }

    /// 带看板聚合读数的那一份夹具。
    fn fake_tops(tops: dns_api::Tops) -> FakeDns {
        FakeDns {
            tops,
            ..fake(dns_api::Screen::default())
        }
    }

    impl dns_api::DnsObserver for FakeDns {
        fn screen(&self, filter: &dns_api::Filter) -> dns_api::Screen {
            self.asked.borrow_mut().push(filter.clone());
            self.screen.clone()
        }

        fn record(&self, id: u64) -> Result<Value, String> {
            self.ids.borrow_mut().push(id);
            self.record.clone()
        }

        fn tops(&self) -> dns_api::Tops {
            *self.tops_calls.borrow_mut() += 1;
            self.tops.clone()
        }
    }

    /// 面板上那一句"跑的不是这一份"的判据，就是这一对数：`running_version`（在内存里的那份的
    /// SHA-256，2026-10-11 实测：改盘上文件不重载时它**不动**）与 `wants_version`（面板此刻这份
    /// 期望态渲染出来的同一摘要）。这一条同时钉住三件不是随手写下的事：两者同数时 `applied` 是真、
    /// 回执照给上游那三块（版本 / `checks` / 插件计数）、以及 `config_path` 出不了这一层。
    #[test]
    fn the_dns_screen_names_the_config_running_and_the_one_the_panel_wants() {
        let layout = fixture("dns-applied");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        put_state(&layout, &state);
        let wants = dns_api::sha256_hex(
            crate::gen::oxidns::render(&state)
                .expect("这份夹具渲染得出来")
                .as_bytes(),
        );
        let observer = fake(live_screen(Some(&wants)));
        let app = App::new(&svc, &token).with_dns(&observer);

        let got = app
            .handle(&Request::post("/api/dns-records").authorized(TOKEN))
            .value();
        assert_eq!(got["live"], true, "{got}");
        assert_eq!(got["detail"], Value::Null, "{got}");
        assert_eq!(got["version"], "1.6.1", "{got}");
        assert_eq!(got["build_bundle"], "full", "{got}");
        assert_eq!(got["checks"]["server_startup"], "ok", "{got}");
        assert_eq!(got["plugins"]["servers"], 2, "{got}");
        assert_eq!(got["reload_status"], "idle", "{got}");
        assert_eq!(got["running_version"], wants.as_str(), "{got}");
        assert_eq!(got["wants_version"], wants.as_str(), "{got}");
        assert_eq!(got["applied"], true, "跑的就是面板这一份：{got}");
        assert_eq!(got["generate_error"], Value::Null, "{got}");
        assert_eq!(got["state_source"], "saved", "{got}");
        assert_eq!(got["records_enabled"], true, "{got}");
        assert_eq!(got["records"].as_array().unwrap().len(), 2, "{got}");
        assert_eq!(got["next_cursor"], "1790892955162:3", "{got}");
        assert_eq!(got["stats"][0]["tag"], "record", "{got}");
        assert_eq!(got["errors"], json!({}), "{got}");

        // 空体 = 一个条件都不给，而且确实原样递到了取数那一步
        assert_eq!(*observer.asked.borrow(), vec![dns_api::Filter::default()]);

        let raw = app
            .handle(&Request::post("/api/dns-records").authorized(TOKEN))
            .body;
        assert!(!raw.contains(LEAK_PROBE), "落点出不了这一层：{raw}");
        assert!(
            !raw.contains(&layout.root),
            "连 404 都不回显 root，这里更要紧：{raw}"
        );
        assert!(runner.argv().is_empty(), "读这一屏不该动机器");
    }

    /// `applied` 的三种说法必须是三个不同的值：`true`（跑的是这一份）、`false`（面板改了、
    /// 解析器还没换上）、`null`（**说不清**）。第三个不是和稀泥——把"没读到"显示成"不符合期望"
    /// 会让一个连不上管理口的机器满屏报假故障，而这一格的存在意义恰恰是"这话可信用"。
    #[test]
    fn applied_is_true_false_or_a_sober_none_never_a_guess() {
        let layout = fixture("dns-applied-three");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        put_state(&layout, &state);
        let wants = dns_api::sha256_hex(
            crate::gen::oxidns::render(&state)
                .expect("这份夹具渲染得出来")
                .as_bytes(),
        );

        let cases = [
            (Some(wants.as_str()), Value::Bool(true), "同数"),
            (
                Some("0000000000000000000000000000000000000000000000000000000000000000"),
                Value::Bool(false),
                "面板改了而解析器还跑着上一份",
            ),
            (None, Value::Null, "上游没报这一格"),
        ];
        for (running, want, why) in cases {
            let observer = fake(live_screen(running));
            let app = App::new(&svc, &token).with_dns(&observer);
            let got = app
                .handle(&Request::post("/api/dns-records").authorized(TOKEN))
                .value();
            assert_eq!(got["applied"], want, "{why}：{got}");
            // 三种说法里记录照给：这一格说不清不等于那一屏没东西
            assert_eq!(got["records"].as_array().unwrap().len(), 2, "{why}：{got}");
        }

        // 第四种：期望态本身坏到渲染不出来。这时 `wants_version` 是 null + 一句原因，
        // 而 `applied` 也必须 null——面板不许把"渲染失败"显示成"跑的不是这一份"。
        let dir = PathBuf::from(layout.state_dir());
        fs::create_dir_all(&dir).unwrap();
        fs::write(PathBuf::from(layout.state_path()), "{ 这不是状态文件").unwrap();
        let observer = fake(live_screen(Some(&wants)));
        let app = App::new(&svc, &token).with_dns(&observer);
        let got = app
            .handle(&Request::post("/api/dns-records").authorized(TOKEN))
            .value();
        assert_eq!(got["state_source"], "broken", "{got}");
        assert_eq!(got["wants_version"], Value::Null, "{got}");
        assert!(got["generate_error"].is_string(), "{got}");
        assert_eq!(got["applied"], Value::Null, "{got}");
        // 而坏状态下面板还是要能看到机器上真在跑什么
        assert_eq!(got["running_version"], wants.as_str(), "{got}");
    }

    /// 连不上管理口是一句**状态**，不是 5xx（那会被读成"面板坏了"），也不是空列表
    /// （那会被读成"这台机器上没发生过查询"——两句话在用户那里完全不同）。
    ///
    /// 每一项单独降级同一条道理：`stats` 那一条失败只有 `stats` 是 null，
    /// 记录、版本、期望/实际那一对都照给。
    #[test]
    fn a_dns_screen_that_cannot_reach_the_resolver_still_answers_200() {
        let layout = fixture("dns-dead");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        put_state(&layout, &state);

        let dead = dns_api::Screen {
            live: false,
            detail: Some("连不上 127.0.0.1:8053：连接被拒绝（解析器没在跑，或管理口没开）".into()),
            ..dns_api::Screen::default()
        };
        let observer = fake(dead);
        let app = App::new(&svc, &token).with_dns(&observer);
        let got = app.handle(&Request::post("/api/dns-records").authorized(TOKEN));
        assert_eq!(got.status, 200, "{}", got.body);
        let value = got.value();
        assert_eq!(value["live"], false, "{value}");
        assert!(value["detail"]
            .as_str()
            .unwrap()
            .contains("连不上 127.0.0.1:8053"));
        assert_eq!(value["records"].as_array().unwrap().len(), 0, "{value}");
        assert_eq!(value["running_version"], Value::Null, "{value}");
        assert_eq!(value["applied"], Value::Null, "{value}");
        assert_eq!(value["version"], Value::Null, "{value}");
        // 连不上不影响期望态那一侧：面板还是能看到"我这份会生成什么、记录那格开没开"
        assert_eq!(value["state_source"], "saved", "{value}");
        assert!(value["wants_version"].is_string(), "{value}");

        // 只有一条读失败：其余照给，`errors` 里点名那一条
        let partial = dns_api::Screen {
            errors: vec![("stats".to_string(), "上游回了 404".to_string())],
            stats: Value::Null,
            ..live_screen(None)
        };
        let observer = fake(partial);
        let app = App::new(&svc, &token).with_dns(&observer);
        let value = app
            .handle(&Request::post("/api/dns-records").authorized(TOKEN))
            .value();
        assert_eq!(value["live"], true, "{value}");
        assert_eq!(value["stats"], Value::Null, "{value}");
        assert_eq!(value["errors"]["stats"], "上游回了 404", "{value}");
        assert_eq!(value["records"].as_array().unwrap().len(), 2, "{value}");
        assert_eq!(value["version"], "1.6.1", "{value}");
        assert!(runner.argv().is_empty(), "两条只读端点一个进程都不该起");
    }

    /// 参数只有体一个容身处，而**白名单之外的键一律 400**：这两条是只读端点，
    /// 少这一道就会变成"面板上那个框写错了名字，于是一屏空白"。
    /// 类型与上限也在问上游之前就拒掉——管理口对 `limit=0` 回的是 400，我们不该把
    /// 对面给的值原样递过去再翻译回来。
    #[test]
    fn the_dns_filter_takes_only_those_five_keys_and_checks_them_locally() {
        let layout = fixture("dns-filter");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        put_state(&layout, &state);

        for (body, want) in [
            (r#"{"qnme":"a.com"}"#, "qname"),
            (r#"{"limt":20}"#, "limit"),
            (r#"{"limit":0}"#, "至少 1"),
            (r#"{"limit":99999}"#, "最多 200"),
            (r#"{"limit":"20"}"#, "整数"),
            (r#"{"qname":20}"#, "字符串"),
            // `status` 是枚举，不是自由文本（源码那四个词）：这一关在我们这边过
            (r#"{"status":"failed"}"#, "has_response"),
            ("[]", "JSON 对象"),
            ("这不是 JSON", "合法的 JSON"),
        ] {
            let observer = fake(live_screen(None));
            let app = App::new(&svc, &token).with_dns(&observer);
            let got = app
                .handle(&Request::post_body("/api/dns-records", body.as_bytes()).authorized(TOKEN));
            assert_eq!(got.status, 400, "{body} -> {}", got.body);
            assert!(
                got.body.contains(want),
                "{body} 该提到 {want}：{}",
                got.body
            );
            assert!(observer.asked.borrow().is_empty(), "{body} 竟问到了上游");
        }

        // 五个键都给上：一路传到取数那一步，一个都没丢
        let body = r#"{"qname":"www.example.com","client_ip":"192.0.2.30","status":"error","cursor":"1790892955162:3","limit":20}"#;
        let observer = fake(live_screen(None));
        let app = App::new(&svc, &token).with_dns(&observer);
        assert_eq!(
            app.handle(&Request::post_body("/api/dns-records", body.as_bytes()).authorized(TOKEN))
                .status,
            200
        );
        assert_eq!(
            observer.asked.borrow()[0],
            dns_api::Filter {
                qname: Some("www.example.com".into()),
                client_ip: Some("192.0.2.30".into()),
                status: Some("error".into()),
                cursor: Some("1790892955162:3".into()),
                limit: Some(20),
            },
            "体里的五个键该一个不少地到了取数那一步"
        );

        // 单条那一条也一样：`id` 必需、要 ≥1
        for (body, want) in [
            ("{}", "少给了 id"),
            (r#"{"id":0}"#, "从 1 起"),
            (r#"{"id":"3"}"#, "整数"),
            (r#"{"id":3.5}"#, "整数"),
            (r#"{"iw":3}"#, "id"),
        ] {
            let observer = fake(live_screen(None));
            let app = App::new(&svc, &token).with_dns(&observer);
            let got = app
                .handle(&Request::post_body("/api/dns-record", body.as_bytes()).authorized(TOKEN));
            assert_eq!(got.status, 400, "{body} -> {}", got.body);
            assert!(
                got.body.contains(want),
                "{body} 该提到 {want}：{}",
                got.body
            );
            assert!(observer.ids.borrow().is_empty(), "{body} 竟问到了上游");
        }
    }

    /// 单条记录是这一屏的第二层：`steps`（"这条查询命中了链上哪几步"）**只在这里有**。
    ///
    /// 取不到那一条也是 200 + `found:false` + 那句话：面板按"这一条没了"显示，
    /// 而不是按"这一屏坏了"显示——记录会按保留天数滚掉，点了半天之后撞上一条没了是正常路径。
    #[test]
    fn one_record_arrives_with_the_chain_it_walked() {
        let layout = fixture("dns-record");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();

        // 实测形状：失败会沿链往回传播，`outcome` 有 entered / error / stop 三种，
        // 记录器自己不出现在 steps 里，`node_index` 从 1 起。
        let found = json!({
            "ok": true,
            "record": {
                "id": 5, "created_at_ms": 1790892999000i64, "client_ip": "192.0.2.30",
                "rcode": "No Error", "answer_count": 1,
                "questions_json": [{"name": "www.example.com.", "qtype": "A", "qclass": "IN"}],
                "steps": [
                    {"event_index": 0, "sequence_tag": "a", "node_index": 1, "kind": "server", "tag": "domestic", "outcome": "entered"},
                    {"event_index": 1, "sequence_tag": "a", "node_index": 2, "kind": "executor", "tag": "family", "outcome": "error"},
                    {"event_index": 2, "sequence_tag": "a", "node_index": 3, "kind": "server", "tag": "fallback", "outcome": "stop"},
                ],
            }
        });
        let mut observer = fake(live_screen(None));
        observer.record = Ok(found);
        let app = App::new(&svc, &token).with_dns(&observer);
        let got = app.handle(
            &Request::post_body("/api/dns-record", r#"{"id":5}"#.as_bytes()).authorized(TOKEN),
        );
        assert_eq!(got.status, 200, "{}", got.body);
        let value = got.value();
        assert_eq!(value["found"], true, "{value}");
        assert_eq!(value["detail"], Value::Null, "{value}");
        assert_eq!(value["record"]["id"], 5, "{value}");
        let steps = value["record"]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 3, "{value}");
        assert_eq!(steps[1]["outcome"], "error", "{value}");
        assert_eq!(*observer.ids.borrow(), vec![5]);
        assert!(runner.argv().is_empty(), "读一条记录不该动机器");

        // 记录滚掉了 / id 不存在：200 + found:false + 那句上游的话
        let mut observer = fake(live_screen(None));
        observer.record = Err("404 Not Found".to_string());
        let app = App::new(&svc, &token).with_dns(&observer);
        let value = app
            .handle(
                &Request::post_body("/api/dns-record", r#"{"id":99999}"#.as_bytes())
                    .authorized(TOKEN),
            )
            .value();
        assert_eq!(value["found"], false, "{value}");
        assert_eq!(value["record"], Value::Null, "{value}");
        assert!(value["detail"].as_str().unwrap().contains("404"), "{value}");
    }

    /// 两条只读端点也在令牌门**之内**：没令牌时连"这个端点存在"都不说，
    /// 而 GET 打到 POST 上不给任何一次上游请求。
    #[test]
    fn the_dns_read_endpoints_are_behind_the_gate_and_take_only_post() {
        let layout = fixture("dns-gate");
        let state = edited();
        let runner = green_runner(&state);
        let svc = Svc::new(&layout, InitSystem::OpenRC, &runner, &state.ports);
        let token = token();
        let app = App::new(&svc, &token);

        for path in ["/api/dns-records", "/api/dns-record"] {
            assert_eq!(app.handle(&Request::post(path)).status, 401, "{path}");
            let got = app.handle(&Request::get(path).authorized(TOKEN));
            assert_eq!(got.status, 405, "{path}：{}", got.body);
            assert!(got.body.contains("POST"), "{path}：{}", got.body);
        }
        assert!(runner.argv().is_empty(), "路由阶段不该动机器");
    }
}
