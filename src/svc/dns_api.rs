//! 读 **oxidns 的管理 API**（`docs/BLUEPRINT.md` §6 第一行那个强契约：DNS 解析记录）。
//!
//! 这一层只做一件事：把回环上那个口读回来的 JSON 原样递出去，ZBox 不重新建模上游的字段。
//! 上游升级时改的是它的字段表，不是我们——把回执照成自己的 struct，每次升级都要跟着改一次，
//! 而"升级近乎零影响"是这个项目重定义时写死的判据。
//!
//! 路由**全部由代码拼**，面板给的只有参数值，而且参数值一律经过转义（[`encode`]）：
//! 一个混进 `qname` 的 CRLF 如果不转义，就是往回环那条请求里注入。管理口上有一条能读配置
//! **文件原文**的路由（`/api/config`，那份 YAML 将来带主路由凭据），这一层没有它，
//! 也不许有——加进去之前先想清楚它会不会把凭据拖到局域网面上。
//!
//! ## 线上形状（2026-10-11 guest 逐字抓的原始字节，不是文档）
//!
//! - 每个响应都带 `content-length` 且 `connection: close`，**没有一处用分块传输**——400 与 404
//!   也一样。所以按声明长度读满就够，不必等对面关连接。
//! - 头名全小写。错误有**两种形状**：路由不存在是纯文本 `404 Not Found`（连 `content-type`
//!   都没有），参数错是 JSON `{"ok":false,"code":"limit must be greater than 0"}`。
//! - 这一份构建（1.6.1 full）里 `/healthz`、`/readyz`、`/health` **一律 404**；活着与否要看
//!   `/api/health`（给版本与 `checks`）与 `/api/healthz`（`ok`）。全典 §那条照文档写的路径是错的。
//! - `/api/control` 的 `reload.running_version` 就是**加载进内存的那份配置的 SHA-256**：
//!   `sha256sum ox.yaml` 与 API 给的那串逐字相同，而改盘上文件不重载时 API 那个数**不动**。
//!   所以它说的是"在跑的那一份"——这正是"面板改了、解析器还没换上"那一句的判据（[`running_version`]）。
//! - 记录是**两层**：列表每行没有 `steps`，`steps` 只在 `records/<id>` 单条里；
//!   `outcome` 实测有 `entered` / `error` / `stop` 三种（文档只写了前两种的一半）。
//!   分页游标是字符串 `"<created_at_ms>:<id>"`，给的是**严格早于**它的那一批。
//! - 过滤参数里只有 `status` 是**枚举**（`all` / `error` / `has_response` / `no_response`，
//!   源码 `query_recorder/api.rs:743-753`），别的一律 400；`limit` 封顶 500 且**超出静默取小**
//!   （`:33`、`:718-724`）。这两个数都在源码里核实表那一份里，不是抓包抓出来的——
//!   抓包只能证明这台机器上这一份是这样，源码说的是"参数表长什么样"。

use crate::gen::oxidns::{API_LISTEN, RECORD_TAG};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// 一次上游读最多收多少字节。列表每行实测约 700 字节，`limit` 封顶 200 时约 140 KiB，
/// 这里给的是它的十几倍——撞上限的只可能是有人对着回环灌包，那时报错比读完更诚实。
pub const MAX_RESPONSE: usize = 2 * 1024 * 1024;

/// 头部块的上限。实测这一份只有 5 行、129 字节。
const MAX_HEAD: usize = 16 * 1024;

/// 连接超时。同机回环，起了就该立刻接上；接不上就是没在跑。
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// 读超时。管理口是本地进程，正常回答都在毫秒级；3 秒是给"连着但卡住"那条路留的。
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// 一次 HTTP 往返。真实实现是 [`TcpHttp`]；单测注入 [`Scripted`]，喂 guest 抓下来的逐字夹具。
pub trait Http {
    fn get(&self, path: &str) -> Result<Reply, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 走 TCP 的那一份。地址在构造时就定死（来自 [`API_LISTEN`]，与渲染器共用同一个常量）。
pub struct TcpHttp {
    pub addr: SocketAddr,
}

impl TcpHttp {
    /// [`API_LISTEN`] 写成什么就是什么；解析不出来是一个错误而不是 panic——那个常量在
    /// 生成物里也要落一份，改坏了要能在面板上看见。
    pub fn from_listen() -> Result<Self, String> {
        API_LISTEN
            .parse::<SocketAddr>()
            .map(|addr| Self { addr })
            .map_err(|e| format!("管理口地址 {API_LISTEN} 解析不出来：{e}"))
    }
}

impl Http for TcpHttp {
    fn get(&self, path: &str) -> Result<Reply, String> {
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.addr
        );
        let mut stream = TcpStream::connect_timeout(&self.addr, CONNECT_TIMEOUT)
            .map_err(|e| format!("连不上解析器的管理口 {}：{e}", self.addr))?;
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .map_err(|e| format!("设读超时失败: {e}"))?;
        stream
            .write_all(request.as_bytes())
            .map_err(|e| format!("往管理口写请求失败: {e}"))?;
        let mut buf: Vec<u8> = Vec::with_capacity(1024);
        let mut chunk = [0u8; 512];
        let head_end = loop {
            if let Some(at) = find_head_end(&buf) {
                break at;
            }
            if buf.len() > MAX_HEAD {
                return Err(format!("管理口的响应头超过 {MAX_HEAD} 字节，已断开"));
            }
            let read = stream
                .read(&mut chunk)
                .map_err(|e| format!("读管理口的响应头失败: {e}"))?;
            if read == 0 {
                return Err("管理口在写完头部块之前就断了（没读到 CRLF CRLF）".into());
            }
            buf.extend_from_slice(&chunk[..read]);
        };
        let head: Vec<u8> = buf.drain(..head_end + 4).collect();
        let (status, declared) = parse_head(&head)?;
        // 有 `content-length` 就按它读满（实测每一条都有）；没有才读到 EOF——那是对面
        // 换了行为，读满比猜长度可靠，而 EOF 在 `connection: close` 下总会来。
        match declared {
            Some(want) => {
                let mut have = buf.len();
                while have < want {
                    if have >= MAX_RESPONSE {
                        return Err(format!(
                            "管理口的响应体超过 {MAX_RESPONSE} 字节上限（已读 {have}）"
                        ));
                    }
                    let read = stream
                        .read(&mut chunk)
                        .map_err(|e| format!("读管理口的响应体失败: {e}"))?;
                    if read == 0 {
                        return Err(format!("管理口声明了 {want} 字节却只送了 {have} 就断了"));
                    }
                    buf.extend_from_slice(&chunk[..read]);
                    have += read;
                }
                buf.truncate(want);
            }
            None => {
                while buf.len() < MAX_RESPONSE {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => buf.extend_from_slice(&chunk[..read]),
                        Err(e) => return Err(format!("读管理口的响应体失败: {e}")),
                    }
                }
            }
        }
        Ok(Reply { status, body: buf })
    }
}

/// 单测用的假传输：按调用顺序给回执，用完了还调用就是错。
pub struct Scripted {
    replies: std::cell::RefCell<std::collections::VecDeque<Result<Reply, String>>>,
    pub calls: std::cell::RefCell<Vec<String>>,
}

impl Scripted {
    pub fn new(replies: Vec<Result<Reply, String>>) -> Self {
        Self {
            replies: std::cell::RefCell::new(replies.into()),
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// 一条 200 + JSON 体的回执。
    pub fn ok(body: &str) -> Result<Reply, String> {
        Ok(Reply {
            status: 200,
            body: body.as_bytes().to_vec(),
        })
    }
}

impl Http for Scripted {
    fn get(&self, path: &str) -> Result<Reply, String> {
        self.calls.borrow_mut().push(path.to_string());
        self.replies
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err("假的回执用完了".to_string()))
    }
}

/// 借出来的实现也是同一个口：`Api` 不拥有传输，测试与面板层都能递一个 `&`。
impl<H: Http + ?Sized> Http for &H {
    fn get(&self, path: &str) -> Result<Reply, String> {
        (**self).get(path)
    }
}

/// 从头部块取状态码与 `Content-Length`（大小写不敏感；没有声明就是 `None`）。
pub fn parse_head(head: &[u8]) -> Result<(u16, Option<usize>), String> {
    let text = std::str::from_utf8(head)
        .map_err(|_| "管理口的响应头不是合法 UTF-8，读不了".to_string())?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let code: u16 = status_line
        .split(' ')
        .nth(1)
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| format!("管理口的状态行读不出状态码：{status_line:?}"))?;
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| format!("Content-Length 不是整数：{value:?}"))?,
            );
        }
    }
    Ok((code, length))
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// 一次记录查询的参数。全部可选——缺省就是"最近的一页"。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub qname: Option<String>,
    pub client_ip: Option<String>,
    pub status: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

/// 一页能要多少条。面板只画得下这么多。
///
/// 管理口那边的数是 `api.rs:32-33`：默认 100、封顶 500（超出**静默取小**，不报错）。
/// 这里给 200 是**我们这边**的上限，而且超了要报错不是取小——面板上那个数字框写 5000
/// 的人以为自己要到了 5000，悄悄给他 500 就是"按了没反应"那一类。
pub const LIMIT_MAX: u32 = 200;
const DEFAULT_LIMIT: u32 = 50;

/// `status` 认的四个词，逐字来自源码（`query_recorder/api.rs:743-753`）。
///
/// 别的一律被上游拒成 400 `status must be one of all, error, has_response, no_response`，
/// 所以这一关在我们这边过：拿着一个注定被拒的值去问回环、再把那句英文翻成面板能显示的话，
/// 多一次往返而换来的信息一个字节也没多。面板那个下拉框的选项表就是这一份。
pub const STATUS_WORDS: [&str; 4] = ["all", "error", "has_response", "no_response"];

impl Filter {
    /// 从 `action_body` 已经收好的字段表读参数。**类型不对就报错**：这一份是读面，
    /// 拒掉一次刷新比按一个猜的值去查上游更便宜。多余的键由 `action_body` 那道白名单挡。
    pub fn from_fields(obj: &serde_json::Map<String, Value>) -> Result<Self, String> {
        let text = |key: &str| -> Result<Option<String>, String> {
            match obj.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => {
                    if s.len() > 256 {
                        return Err(format!("{key} 超过 256 字节，不像一个查询条件"));
                    }
                    Ok(Some(s.clone()))
                }
                Some(other) => Err(format!("{key} 应是一个字符串，收到 {other}")),
            }
        };
        let limit = match obj.get("limit") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => {
                let raw = n
                    .as_u64()
                    .ok_or_else(|| format!("limit 应是非负整数：{n}"))?;
                if raw == 0 {
                    return Err("limit 至少 1：0 在管理口那边是一个明确的错误，不是\"全部\"".into());
                }
                if raw > LIMIT_MAX as u64 {
                    return Err(format!(
                        "limit 最多 {LIMIT_MAX}：这一屏要的是最近一段，不是整份库"
                    ));
                }
                Some(raw as u32)
            }
            Some(other) => return Err(format!("limit 应是整数，收到 {other}")),
        };
        // 空串当"没给"：面板那个下拉框的"全部"就是空值，而 `records_path` 本来也跳过空值——
        // 两处对同一个输入说两种话才是问题。
        let status = match text("status")? {
            Some(got) if got.is_empty() => None,
            Some(got) if !STATUS_WORDS.contains(&got.as_str()) => {
                return Err(format!(
                    "status 只认这四种之一：{}，收到 {got}",
                    STATUS_WORDS.join(" / ")
                ))
            }
            got => got,
        };
        Ok(Self {
            qname: text("qname")?,
            client_ip: text("client_ip")?,
            status,
            cursor: text("cursor")?,
            limit,
        })
    }

    fn records_path(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(format!("limit={}", self.limit.unwrap_or(DEFAULT_LIMIT)));
        for (key, value) in [
            ("qname", self.qname.as_deref()),
            ("client_ip", self.client_ip.as_deref()),
            ("status", self.status.as_deref()),
            ("cursor", self.cursor.as_deref()),
        ] {
            if let Some(v) = value {
                if !v.is_empty() {
                    parts.push(format!("{key}={}", encode(v)));
                }
            }
        }
        format!("/api/plugins/{RECORD_TAG}/records?{}", parts.join("&"))
    }
}

/// percent-encode。不在 RFC 3986 的 unreserved 表里的字节一个不剩全转——这样控制字符、
/// 空格、`&`、`=`、`%` 都到不了那条请求的分隔位置。上游对 `%20` 的实测反应是正常过滤。
pub fn encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 只读的那几个路由。每一个都拼成完整路径，参数走 [`Filter`]。
pub struct Api<H: Http> {
    http: H,
}

impl<H: Http> Api<H> {
    pub fn new(http: H) -> Self {
        Self { http }
    }

    pub fn health(&self) -> Result<Value, String> {
        self.json("/api/health")
    }

    pub fn control(&self) -> Result<Value, String> {
        self.json("/api/control")
    }

    pub fn records(&self, filter: &Filter) -> Result<Value, String> {
        let path = filter.records_path();
        self.json(&path)
    }

    /// 单条：`steps` 只有这一层有（实测），所以那一屏必须是两层。
    pub fn record(&self, id: u64) -> Result<Value, String> {
        self.json(&format!("/api/plugins/{RECORD_TAG}/records/{id}"))
    }

    /// 每个执行器被走到几次——「DNS 那一侧的分流命中统计」。
    pub fn plugin_stats(&self) -> Result<Value, String> {
        self.json(&format!("/api/plugins/{RECORD_TAG}/stats/plugins"))
    }

    /// 看板那两格：问得最多的域名 / 问得最多的客户端。形状是
    /// `{ok, sample_size, rows[{key,count,share}]}`（源码 `TopBucketsResponse`，
    /// `sample_size` 是**聚合看过多少条**、不是这台机器一共解析了多少次——
    /// 屏幕上那句"最近 N 条里的前 20 名"就是它的 `sample_size`）。
    pub fn top_qnames(&self) -> Result<Value, String> {
        self.json(&format!("/api/plugins/{RECORD_TAG}/stats/top_qnames"))
    }

    /// [`Self::top_qnames`] 的客户端版。
    pub fn top_clients(&self) -> Result<Value, String> {
        self.json(&format!("/api/plugins/{RECORD_TAG}/stats/top_clients"))
    }

    /// 应答码分布（`{ok, sample_size, rows[{key,count,share}]}`）：看板上"有多少条被打回"
    /// 的那一格。**这是 `rcode` 的分布，不是"命中了哪条规则"**——后者在 #69/G4 定案前给不出。
    pub fn rcode_stats(&self) -> Result<Value, String> {
        self.json(&format!("/api/plugins/{RECORD_TAG}/stats/rcode"))
    }

    fn json(&self, path: &str) -> Result<Value, String> {
        let reply = self.http.get(path)?;
        if reply.status != 200 {
            return Err(non_200(&reply));
        }
        serde_json::from_slice(&reply.body)
            .map_err(|e| format!("管理口 {path} 回来的不是 JSON: {e}"))
    }
}

/// 非 200 的那句话。两种错误形状都要照顾：JSON 的 `{"ok":false,"code":...}` 与
/// 纯文本的 `404 Not Found`（实测都存在）。
fn non_200(reply: &Reply) -> String {
    let text = String::from_utf8_lossy(&reply.body).trim().to_string();
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        if let Some(code) = value.get("code").and_then(Value::as_str) {
            return format!("管理口回 {}：{code}", reply.status);
        }
    }
    if reply.status == 404 {
        return format!(
            "管理口回 404：这一份构建没有这个路由（{text}）。上游改了路由此处应当降级而不是崩"
        );
    }
    format!("管理口回 {}：{}", reply.status, truncate(&text))
}

fn truncate(text: &str) -> String {
    let mut out = text.chars().take(200).collect::<String>();
    if text.chars().count() > 200 {
        out.push('…');
    }
    out
}

/// 在跑的那份配置的指纹（`/api/control` 的 `reload.running_version`）。
/// 读不出来是 `None`——"不知道"在这屏上是一格空，不是一条故障。
pub fn running_version(control: &Value) -> Option<&str> {
    control.get("reload")?.get("running_version")?.as_str()
}

/// 一次刷新从上游读回来的那一份。**每一项单独降级**：`errors` 里有的那几项是 `null`，
/// 而"没读到"不许显示成"不符合期望"（`doctor.rs` 开头那条约定在这里的形状）。
#[derive(Debug, Clone, Default)]
pub struct Screen {
    pub live: bool,
    /// 连不上管理口时的那句话（`live: false` 时其余全是空的）。
    pub detail: Option<String>,
    pub health: Value,
    pub control: Value,
    pub page: Value,
    pub stats: Value,
    pub errors: Vec<(String, String)>,
}

/// 看板那一次刷新读回来的聚合读数（`docs/BLUEPRINT.md` §11.3 B 的第 2、3 件）。
///
/// 为什么单开一条路而不并进 [`Screen`]：`Screen` 是「解析记录」那一页的取数包，每翻一页都要
/// 重新读；看板要的这几份聚合与那一页无关，并进去就是**每翻一页多打四条上游请求**。
/// 两条路共用同一条降级约定：每一项单独 `null` + `errors` 里那一句，"没读到"不许显示成"不合规"。
///
/// **四份都是样本，不是这台机器的总量**：上游回的 `sample_size` 说的是"这次聚合看过多少条记录"
/// （源码 `load_top_qnames`：`ORDER BY created_at_ms DESC LIMIT ?` 那一段的条数），
/// 屏幕上因此写"最近 N 条里的前 20 名"，不写"总共解析了多少次"。
///
/// 三条 2026-10-12 实测（逐字回执与 SQL 出处都记在
/// `docs/reference/oxidns-plugin-args-verified.md`），它们决定了这几格**到了面板上还能不能相加**：
/// - **同一次刷新里各格的 `sample_size` 可以不相等**（实测 `top_qnames` 给 13、另两份给 15），
///   所以那句"最近 N 条"必须每格带自己的 N，不许在卡头写一个共用的数。
/// - **`rcode` 的 `key` 里有上游的合成桶**：`_ERROR` / `_NO_RESPONSE` / `_UNKNOWN` 不是 DNS 应答码的
///   名字，原样显示就是真相，翻译或并进真 rcode 都是编。
/// - **空表不空**：全新的 DB 上 `rcode` 回 `sample_size: 0` 却带一行 `_UNKNOWN` `count: 1`
///   （`totals LEFT JOIN sample_records ON 1=1` 把 `NULL` 接进了 `CASE … ELSE`，`store.rs:1020-1028`）。
///   这一份里 `count` 与 `sample_size` 打架，所以面板的界是 **`sample_size == 0` 时那一格走空态**。
///
/// 至于"哪条连接真走了代理"——那一格不在这里，也不在任何别处：#69/G4 定案前连接层给不出，
/// 面板上明写给不出，**不许从日志反推**（§11.3 B 第 3 件剩下的只有解析层那一份，
/// 而解析层能说的全在这四格里：问了哪些域名、谁在问、应答码怎么分布、每个执行器被走到几次）。
#[derive(Debug, Clone, Default)]
pub struct Tops {
    pub live: bool,
    /// 连不上管理口时的那句话（`live: false` 时其余全空）。
    pub detail: Option<String>,
    /// 不解析、不重算地搬回来的 `{ok, sample_size, rows[{key,count,share}]}`；读不到是 `null`。
    /// "不重算"要说准：`share` 是上游算的，ZBox 只让那个 `Value` 过一遍 serde——**末位会变**
    /// （实测 `0.15384615384615385` 出来成 `0.15384615384615383`，1 个 ulp，屏幕上同一个"30.8%"）。
    pub qnames: Value,
    pub clients: Value,
    /// `{ok, sample_size, rows[...]}`：应答码分布，"被打回"那一格（`share` 之外不另算比例）。
    pub rcodes: Value,
    /// `{ok, query_total, stats[{kind,tag,checked,matched,executed,query_share}]}`：
    /// 哪个执行器被走到几次，就是"这批查询被转向到哪一步"的解析层口径。
    pub plugins: Value,
    /// `/api/health` 那份**原样**搬回来的读数（实测：`{status, version, build_bundle, uptime_ms,
    /// checks, plugins}`）。看板那一格「解析器在不在、什么版本」要的就是这里，而它是**进程活着才答**
    /// 的口——所以"读到了"这一句可以说"在位"，不必再发一条命令去问进程表。
    ///
    /// 为什么并进这一份而不是另开一次刷新：`tops()` 本来就要先探一下 `health` 定"这个口连上没连上"，
    /// 把那份回执丢掉再读第二次，等于同一轮里多打一次上游。
    pub health: Value,
    /// `/api/control` 那一份（实测含 `reload.running_version`——**加载进内存的那份配置的 SHA-256**）。
    /// 看板那一格"跑的是不是盘上这一份"读的是它，与「解析记录」屏那一对 `running_version` /
    /// `wants_version` 同一个来源、同一个约定。
    pub control: Value,
    pub errors: Vec<(String, String)>,
}

/// 一项软失败：读回来就原样搬走，读不回来记一句、给面板留 `null`。
///
/// 单开一个函数是为了 [`Live::screen`] 与 [`Live::tops`] 共用**同一份**降级实现——
/// 两处各写一遍，将来就是"记录那一页说这一项没读到、看板却说它 0"。
fn soft_take(errors: &mut Vec<(String, String)>, key: &str, got: Result<Value, String>) -> Value {
    match got {
        Ok(value) => value,
        Err(detail) => {
            errors.push((key.to_string(), detail));
            Value::Null
        }
    }
}

/// 面板层要的那个口子：一次刷新的四条只读请求 + 单条记录 + 看板那一份聚合。
///
/// 有这一层是因为**单测不能去连回环**：那条路上跑的是真网络，在开发机上永远连不通、
/// 在 guest 上又真有一个 oxidns 正听着（实测就是本轮留下的），两种都会让断言变成环境函数。
pub trait DnsObserver {
    fn screen(&self, filter: &Filter) -> Screen;
    fn record(&self, id: u64) -> Result<Value, String>;
    fn tops(&self) -> Tops;
}

/// 真去连管理口的那一份。
pub struct Live {
    http: TcpHttp,
}

impl Live {
    pub fn new() -> Result<Self, String> {
        TcpHttp::from_listen().map(|http| Self { http })
    }
}

impl DnsObserver for Live {
    fn screen(&self, filter: &Filter) -> Screen {
        let api = Api::new(&self.http);
        let health = match api.health() {
            Ok(got) => got,
            Err(detail) => {
                return Screen {
                    live: false,
                    detail: Some(detail),
                    ..Screen::default()
                }
            }
        };
        let mut errors: Vec<(String, String)> = Vec::new();
        let control = soft_take(&mut errors, "control", api.control());
        let page = soft_take(&mut errors, "records", api.records(filter));
        let stats = soft_take(&mut errors, "stats", api.plugin_stats());
        Screen {
            live: true,
            detail: None,
            health,
            control,
            page,
            stats,
            errors,
        }
    }

    fn record(&self, id: u64) -> Result<Value, String> {
        Api::new(&self.http).record(id)
    }

    /// 看板那一次刷新：先探一次 `health` 定"这个口连上没连上"，再各读一份聚合。
    ///
    /// 为什么要先探那一下：不探的话，"管理口没人听"与"这一份构建没有这条路由"在回执里都是
    /// 三条错误，面板就只能在两种话里挑一句、而且多半挑错。`health` 读不回来直接 `live: false`
    /// 带那一句原因，四项一律空（与 [`Live::screen`] 同一个约定）。
    ///
    /// 探的那一份回执**留着不丢**（`health` / `control` 两个字段）：看板那一格「解析器在不在、
    /// 什么版本、跑的是不是盘上这一份」要的就是它们，而这一屏的约定是一条命令都不发——
    /// 版本只能从进程自己答的口径来，不能靠 `oxidns --version` 那种新起的进程。
    fn tops(&self) -> Tops {
        let api = Api::new(&self.http);
        let health = match api.health() {
            Ok(got) => got,
            Err(detail) => {
                return Tops {
                    live: false,
                    detail: Some(detail),
                    ..Tops::default()
                }
            }
        };
        let mut errors: Vec<(String, String)> = Vec::new();
        let control = soft_take(&mut errors, "control", api.control());
        let qnames = soft_take(&mut errors, "qnames", api.top_qnames());
        let clients = soft_take(&mut errors, "clients", api.top_clients());
        let rcodes = soft_take(&mut errors, "rcodes", api.rcode_stats());
        let plugins = soft_take(&mut errors, "plugins", api.plugin_stats());
        Tops {
            live: true,
            detail: None,
            qnames,
            clients,
            rcodes,
            plugins,
            health,
            control,
            errors,
        }
    }
}

/// SHA-256 的十六进制（小写，64 个字符）。
///
/// 存在的理由只有一个，而且是实测出来的：oxidns 用 SHA-256 报告它加载的那份配置，而
/// [`crate::apply::store::bytes_digest`] 是 FNV-1a——两个数对不上，"跑的是不是这一份"
/// 这句判据就永远没法说。它只用来比对，不用于任何安全场景。
pub fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // 填充：原消息 + 0x80 + 若干 0 到长度 ≡ 56 (mod 64) + 8 字节大端的**位**长。
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut msg = bytes.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (slot, src) in w.iter_mut().take(16).zip(block.chunks_exact(4)) {
            *slot = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for (acc, got) in h.iter_mut().zip(v) {
            *acc = acc.wrapping_add(got);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 2026-10-11 guest 原始字节（`nc` 直连 127.0.0.1:8053 抓的整段响应，含响应头）。
    /// 三条都是逐字粘进来的：`content-length` 与体长要对得上，否则下面的断言就是假的。
    const HEAD_HEALTH: &str = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
        connection: close\r\ncontent-length: 276\r\n\r\n";
    const BODY_HEALTH: &str = "{\"status\":\"ok\",\"version\":\"1.6.1\",\"build_bundle\":\"full\",\"instance_id\":\"000001a0f9899fae-0000eaa4-05436deb9b00b6a807e298c6eaf13804\",\"started_at_ms\":1790892941230,\"uptime_ms\":204409,\"checks\":{\"api\":\"ok\",\"plugin_init\":\"ok\",\"server_startup\":\"ok\"},\"plugins\":{\"total\":7,\"servers\":2}}";
    const HEAD_404: &str =
        "HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 13\r\n\r\n";
    const HEAD_400: &str = "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\n\
        connection: close\r\ncontent-length: 50\r\n\r\n";
    const BODY_400: &str = "{\"ok\":false,\"code\":\"limit must be greater than 0\"}";

    /// 单条记录（id=3）：`steps` 只有这一层有，形状是 cache entered -> cache stop。
    const BODY_RECORD: &str = r#"{"ok":true,"record":{"id":3,"created_at_ms":1790892955162,"elapsed_ms":0,"request_id":15682,"client_ip":"192.168.219.128","questions_json":[{"name":"example.com.","qtype":"AAAA","qclass":"IN"}],"error":null,"has_response":true,"rcode":"No Error","answer_count":0,"steps":[{"event_index":0,"sequence_tag":"main","node_index":1,"kind":"executor","tag":"cache","outcome":"entered"},{"event_index":1,"sequence_tag":"main","node_index":1,"kind":"executor","tag":"cache","outcome":"stop"}]}}"#;

    const BODY_PLUGINS: &str = r#"{"ok":true,"query_total":4,"stats":[{"kind":"executor","tag":"cache","checked":0,"matched":0,"executed":4,"query_total":4,"query_share":1.0},{"kind":"executor","tag":"family","checked":0,"matched":0,"executed":3,"query_total":3,"query_share":0.75}]}"#;

    #[test]
    fn sha256_matches_the_digest_of_the_public_vectors() {
        // 这三条向量是 2026-10-11 用另一套实现（node 的 crypto）算出来的，不是记忆里抄的
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 55/56/64 三个长度卡在填充的那三个分支上（0x80 之后是否要跨一个整块）
        assert_eq!(
            sha256_hex(&[b'a'; 55]),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 215]),
            "cc78a3344348f94dd5a12664b3ee365ea06701fc265ffc176d26e521efc18fb9"
        );
        // 多字节：哈希的是**字节**，不是 char
        assert_eq!(
            sha256_hex("配置由 ZBox 生成".as_bytes()),
            "1652c9b5042ff45479c0fc4be01130d5b9873e92cb9a58edea88aa0624aad3c1"
        );
    }

    #[test]
    fn the_listen_address_the_renderer_writes_is_the_one_we_read() {
        // 生成物里那一行和取数那一头必须是同一个常量，改一边要另一边跟着红
        assert_eq!(API_LISTEN, "127.0.0.1:8053");
        assert!(API_LISTEN.starts_with("127.0.0.1:"), "管理口只绑回环");
        assert!(TcpHttp::from_listen().is_ok());
        let mut state = crate::model::config::DesiredState::baseline();
        // 解析口只绑显式地址（P2c 那道门），空 lan_address 渲染不出来——这一条要验的是
        // 管理口那一行，与解析口绑谁无关，所以给一个地址把门走过去。
        state.network.lan_address = "192.168.219.128".into();
        let text = crate::gen::oxidns::render(&state).expect("填了地址的基线要渲染得出来");
        assert!(
            text.contains(&format!("  http: '{API_LISTEN}'")),
            "生成物里管理口那行变了：\n{text}"
        );
    }

    #[test]
    fn a_head_block_yields_the_status_and_the_declared_length() {
        let (status, len) = parse_head(HEAD_HEALTH.as_bytes()).unwrap();
        assert_eq!((status, len), (200, Some(276)));
        // 404 那一份连 content-type 都没有，实测
        assert_eq!(parse_head(HEAD_404.as_bytes()).unwrap().0, 404);
        // 小写头名要认得——实测响应头全是小写，按 `Content-Length` 逐字比会读成"没声明"
        assert_eq!(parse_head(HEAD_400.as_bytes()).unwrap().1, Some(50));
        assert!(parse_head("HTTP/1.1 x\r\n\r\n".as_bytes()).is_err());
    }

    #[test]
    fn the_body_is_handed_over_verbatim_and_parsed_as_json() {
        let mut http = Vec::new();
        http.push(Ok(Reply {
            status: 200,
            body: BODY_HEALTH.as_bytes().to_vec(),
        }));
        let scripted = Scripted::new(http);
        let api = Api::new(&scripted);
        let health = api.health().expect("200 + JSON");
        assert_eq!(health["version"], "1.6.1");
        assert_eq!(health["checks"]["api"], "ok");
        assert_eq!(
            scripted.calls.borrow().as_slice(),
            ["/api/health".to_string()]
        );
    }

    #[test]
    fn a_non_200_is_a_readable_failure_in_either_of_the_two_shapes() {
        // JSON 那一种：把 `code` 里那句人话带出来（实测 `limit=0` 就是这个形状）
        let api = Api::new(Scripted::new(vec![Ok(Reply {
            status: 400,
            body: BODY_400.as_bytes().to_vec(),
        })]));
        let err = api.records(&Filter::default()).unwrap_err();
        assert!(err.contains("limit must be greater than 0"), "{err}");
        assert!(err.contains("400"), "{err}");

        // 纯文本那一种：404 要说"这一份构建没有这个路由"，因为那是上游改了形状的信号
        let api = Api::new(Scripted::new(vec![Ok(Reply {
            status: 404,
            body: b"404 Not Found".to_vec(),
        })]));
        let err = api.plugin_stats().unwrap_err();
        assert!(err.contains("404"), "{err}");
        assert!(err.contains("这一份构建没有这个路由"), "{err}");
    }

    #[test]
    fn the_records_path_is_built_from_the_constants_and_carries_only_encoded_values() {
        let filter = Filter {
            qname: Some("example.com.".into()),
            client_ip: Some("192.168.219.128".into()),
            cursor: Some("1790892955162:3".into()),
            limit: Some(20),
            status: None,
        };
        assert_eq!(
            filter.records_path(),
            "/api/plugins/record/records?limit=20&qname=example.com.&client_ip=192.168.219.128&cursor=1790892955162%3A3"
        );
        // 空值等于没给：不写进查询串，让上游按自己的默认走
        assert_eq!(
            Filter::default().records_path(),
            "/api/plugins/record/records?limit=50"
        );
    }

    #[test]
    fn a_crlf_in_a_filter_never_reaches_the_request_line() {
        // 面板是令牌保护的，但参数照样是**对面给的**：不转义就是往回环那条请求里注入
        let evil = "a\r\nX-Injected: 1\r\n\r\nGET /api/config";
        let encoded = encode(evil);
        assert!(
            !encoded.contains('\r') && !encoded.contains('\n'),
            "{encoded}"
        );
        assert!(!encoded.contains(' '), "{encoded}");
        assert!(encoded.starts_with("a%0D%0AX-Injected%3A"), "{encoded}");
        // 空格实测要按 %20 给（`rcode=No%20Error` 在 guest 上是能过滤出东西的）
        assert_eq!(encode("No Error"), "No%20Error");
    }

    #[test]
    fn filter_fields_are_typed_and_the_limits_are_enforced_before_asking_upstream() {
        use serde_json::Map;
        let fields: Map<String, Value> =
            serde_json::from_str(r#"{"qname":"example.com","limit":10}"#).unwrap();
        let filter = Filter::from_fields(&fields).unwrap();
        assert_eq!(filter.qname.as_deref(), Some("example.com"));
        assert_eq!(filter.limit, Some(10));
        // 空表：按"最近的一页"办
        assert_eq!(Filter::from_fields(&Map::new()).unwrap(), Filter::default());
        let reject = |raw: &str| {
            let fields: Map<String, Value> = serde_json::from_str(raw).unwrap();
            Filter::from_fields(&fields)
        };
        assert!(reject(r#"{"limit":500}"#).is_err(), "要封顶");
        assert!(reject(r#"{"limit":0}"#).is_err());
        assert!(reject(r#"{"qname":7}"#).is_err());
        assert!(reject(r#"{"limit":"50"}"#).is_err());
        // `status` 的四个词逐个都要过，第五种写法当场拒——上游对未知值回的是英文 400，
        // 而面板听得懂的那句是我们自己的（`STATUS_WORDS` 就是那个下拉框的选项表）
        for word in STATUS_WORDS {
            let fields: Map<String, Value> =
                serde_json::from_str(&format!(r#"{{"status":"{word}"}}"#)).unwrap();
            assert_eq!(
                Filter::from_fields(&fields).unwrap().status.as_deref(),
                Some(word),
                "{word} 应当认"
            );
        }
        assert!(reject(r#"{"status":"failed"}"#).is_err());
        // 空串是"没给"，不是第五种值
        assert_eq!(
            Filter::from_fields(&serde_json::from_str(r#"{"status":""}"#).unwrap())
                .unwrap()
                .status,
            None
        );
    }

    #[test]
    fn the_two_layers_of_a_record_are_read_through_two_paths() {
        // 列表没有 steps、单条有——这一条不是设计，是实测（#59/#60 抓的原始回执）
        let scripted = Scripted::new(vec![Scripted::ok(BODY_RECORD), Scripted::ok(BODY_PLUGINS)]);
        let api = Api::new(&scripted);
        let one = api.record(3).unwrap();
        let steps = one["record"]["steps"].as_array().expect("单条要有 steps");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1]["outcome"], "stop");
        assert_eq!(steps[1]["tag"], "cache");
        assert_eq!(api.plugin_stats().unwrap()["query_total"], 4);
        assert_eq!(
            scripted.calls.borrow().as_slice(),
            [
                "/api/plugins/record/records/3".to_string(),
                "/api/plugins/record/stats/plugins".to_string()
            ]
        );
    }

    #[test]
    fn running_version_comes_from_the_reload_block_and_degrades_to_none() {
        let control = serde_json::from_str::<Value>(
            r#"{"status":"running","uptime_ms":206373,"reload":{"status":"idle","running_version":"31e240f54715aa9bd91b474184c9602d86c2eb0fbea26c5c6464461a75216ffc"}}"#,
        )
        .unwrap();
        assert_eq!(
            running_version(&control),
            Some("31e240f54715aa9bd91b474184c9602d86c2eb0fbea26c5c6464461a75216ffc")
        );
        // 没这个键 / 不是对象：None，不是"指纹不对"
        assert_eq!(running_version(&json!({"status": "running"})), None);
        assert_eq!(running_version(&json!({"reload": null})), None);
    }
}
