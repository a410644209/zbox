//! 订阅负载解析：把机场返回的一段响应体变成节点表。
//!
//! 只做纯函数解析，网络抓取在别处，这样格式识别和去重逻辑可以完全单测覆盖。

use super::uri::{decode_b64, parse_link, LinkError};
use crate::model::node::Node;
use std::collections::BTreeMap;

const KNOWN_SCHEMES: &[&str] = &[
    "vless://",
    "vmess://",
    "trojan://",
    "ss://",
    "shadowsocks://",
    "hysteria2://",
    "hy2://",
    "tuic://",
];

#[derive(Debug, thiserror::Error)]
pub enum SubscriptionError {
    #[error("订阅内容为空")]
    Empty,
    #[error("订阅编码无法识别")]
    UnknownFormat,
    #[error("这份订阅是 Clash 的配置（YAML），不是节点链接列表：ZBox 编排的 dae 要的是 \
             `vless://` 一类的节点链接，得让机场换一种订阅格式（或用订阅转换工具转一次）")]
    ClashYaml,
    #[error("这份订阅是 sing-box 导出的出站 JSON，ZBox 不认这一形状：要节点链接订阅（`vless://` 一类）")]
    SingboxJson,
    #[error("订阅中没有任何可解析的节点（成功 {ok} 个，失败 {failed} 个）")]
    NoValidNode { ok: usize, failed: usize },
    #[error("节点解析失败: {0}")]
    Link(#[from] LinkError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    LinkList,
    Base64LinkList,
    ClashYaml,
    SingboxJson,
    Unknown,
}

impl Format {
    /// 回执里说"这一份订阅是什么形状"用的词。列在这里而不是散在面板文案里：
    /// 屏幕上的词必须和服务端认的那个枚举一起改。
    pub fn word(self) -> &'static str {
        match self {
            Format::LinkList => "节点链接列表",
            Format::Base64LinkList => "Base64 打包的节点链接",
            Format::ClashYaml => "Clash 配置",
            Format::SingboxJson => "sing-box 导出的出站",
            Format::Unknown => "读不出的形状",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    pub title: Option<String>,
    pub expire: Option<i64>,
    pub total: Option<i64>,
    pub used: Option<i64>,
}

impl Meta {
    /// 机场通常用 `profile-title` / `expire` / `total` / `used` 响应头传递剩余流量与到期时间。
    pub fn from_headers(headers: &BTreeMap<String, String>) -> Self {
        let get = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| headers.get(*k))
                .cloned()
                .map(|v| percent_decode_basic(&v))
        };
        let num = |keys: &[&str]| get(keys).and_then(|v| v.parse().ok());
        Meta {
            title: get(&["profile-title", "_PROFILE_TITLE"]),
            expire: num(&["expire", "EXP_IN"]),
            total: num(&["total", "TOT_AL"]),
            used: num(&["used", "US_ED"]),
        }
    }

    pub fn days_left(&self, now: i64) -> Option<i64> {
        self.expire.map(|e| (e - now) / 86_400)
    }
}

pub fn detect(body: &[u8]) -> Format {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return Format::Unknown;
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        // JSON 形态既可能是 clash 的 proxies 列表，也可能是 sing-box 的 outbounds
        if contains_ci(trimmed, "\"outbounds\"") {
            return Format::SingboxJson;
        }
        if contains_ci(trimmed, "\"proxies\"") {
            return Format::ClashYaml;
        }
        // 少数机场把节点链接数组直接 JSON 化
        if contains_ci(trimmed, "://") {
            return Format::LinkList;
        }
        return Format::Unknown;
    }
    if starts_line_with(trimmed, "proxies:")
        || starts_line_with(trimmed, "proxy-groups:")
        || starts_line_with(trimmed, "mixed-port:")
    {
        return Format::ClashYaml;
    }
    if count_links(trimmed) > 0 {
        return Format::LinkList;
    }
    if let Some(bytes) = decode_b64(trimmed) {
        if let Ok(decoded) = String::from_utf8(bytes) {
            if count_links(&decoded) > 0 {
                return Format::Base64LinkList;
            }
        }
    }
    Format::Unknown
}

/// 一次订阅解析的结果。
#[derive(Debug, Clone)]
pub struct Parsed {
    pub format: Format,
    pub nodes: Vec<Node>,
    /// 形状像链接、但 ZBox 读不懂的那几条。它们不挡导入（机场加一种新协议是常态），
    /// 但必须报得出数——只说"成功 12 个"，人就永远看不到少了 3 个这件事。
    pub skipped: usize,
}

pub fn parse(body: &[u8]) -> Result<Parsed, SubscriptionError> {
    let text = String::from_utf8_lossy(body).into_owned();
    let format = detect(body);
    let payload = match format {
        Format::LinkList => text,
        Format::Base64LinkList => {
            let bytes = decode_b64(text.trim()).ok_or(SubscriptionError::UnknownFormat)?;
            String::from_utf8(bytes).map_err(|_| SubscriptionError::UnknownFormat)?
        }
        Format::ClashYaml => return Err(SubscriptionError::ClashYaml),
        Format::SingboxJson => return Err(SubscriptionError::SingboxJson),
        Format::Unknown if body.is_empty() => return Err(SubscriptionError::Empty),
        Format::Unknown => return Err(SubscriptionError::UnknownFormat),
    };

    let mut nodes = Vec::new();
    let mut failed = 0usize;
    for link in split_links(&payload) {
        match parse_link(link) {
            Ok(node) => {
                if is_info_entry(&node.name) {
                    continue;
                }
                nodes.push(node);
            }
            Err(_) => failed += 1,
        }
    }
    if nodes.is_empty() {
        return Err(SubscriptionError::NoValidNode { ok: 0, failed });
    }
    dedup_names(&mut nodes);
    Ok(Parsed {
        format,
        nodes,
        skipped: failed,
    })
}

/// 机场把"剩余流量 / 到期时间"这类账户信息也编成节点链接塞在列表里（2026-09-29 在真实订阅上
/// 实测到三条：剩余流量、距离下次重置剩余、套餐到期，且都指向第一个节点的出口）。
/// 它们不是可选线路，留在列表里会变成面板上的假节点、并被 urltest 拿去测速。
fn is_info_entry(name: &str) -> bool {
    const MARKERS: &[&str] = &[
        "剩余流量",
        "已用流量",
        "流量",
        "套餐到期",
        "到期时间",
        "距离下次重置",
        "在线设备",
        "官网",
        "公告",
        "客服",
    ];
    MARKERS.iter().any(|m| name.contains(m))
}

/// 同名节点在订阅里非常常见（多条线路同名），面板需要可区分的名字。
fn dedup_names(nodes: &mut Vec<Node>) {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for node in nodes.iter_mut() {
        *seen.entry(node.name.clone()).or_default() += 1;
        let n = seen[&node.name];
        if n > 1 {
            node.name = format!("{} ({n})", node.name);
        }
    }
}

fn split_links(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for raw_line in text.split(['\n', '\r']) {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        // 同一行塞多条链接的订阅存在，按后续出现的 scheme:// 边界切开
        let cuts: Vec<usize> = find_scheme_positions(line)
            .into_iter()
            .filter(|pos| *pos > 0)
            .collect();
        if cuts.is_empty() {
            if KNOWN_SCHEMES.iter().any(|s| line.starts_with(s)) {
                out.push(line);
            }
            continue;
        }
        let mut start = 0usize;
        for cut in cuts {
            let piece = line[start..cut].trim();
            if !piece.is_empty() {
                out.push(piece);
            }
            start = cut;
        }
        let tail = line[start..].trim();
        if !tail.is_empty() {
            out.push(tail);
        }
    }
    out
}

fn find_scheme_positions(line: &str) -> Vec<usize> {
    let bytes = line.as_bytes();
    let mut hits = Vec::new();
    for scheme in KNOWN_SCHEMES {
        for (pos, _) in line.match_indices(*scheme) {
            // `vless://` 内含子串 `ss://`，只有前面不是链接合法字符时才算一条链接的起点
            let boundary_ok = pos == 0
                || !matches!(
                    bytes[pos - 1],
                    b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'+' | b'-' | b'.' | b'_'
                );
            if boundary_ok {
                hits.push(pos);
            }
        }
    }
    hits.sort_unstable();
    hits
}

fn count_links(text: &str) -> usize {
    find_scheme_positions(text).len()
}

fn starts_line_with(text: &str, token: &str) -> bool {
    text.split(['\n', '\r'])
        .any(|l| l.trim_start().starts_with(token))
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn percent_decode_basic(s: &str) -> String {
    // 部分机场的 profile-title 形如 URL 编码 + 前缀 "attachment; filename*=UTF-8''"
    let tail = match s.rsplit_once("''") {
        Some((_, v)) => v,
        None => s,
    };
    let bytes = tail.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&tail[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;

    const L1: &str =
        "vless://aaaaaaaa-1111-1111-1111-111111111111@1.2.3.4:443?security=tls&sni=a.com#东京";
    const L2: &str = "trojan://pw@5.6.7.8:8443?sni=b.com&security=tls#首尔";

    #[test]
    fn plain_link_list_and_base64_both_detected() {
        let plain = format!("{L1}\n{L2}\n");
        assert_eq!(detect(plain.as_bytes()), Format::LinkList);
        let packed = STANDARD.encode(plain.clone());
        assert_eq!(detect(packed.as_bytes()), Format::Base64LinkList);
    }

    #[test]
    fn base64_subscription_yields_nodes() {
        let packed = STANDARD.encode(format!("{L1}\n{L2}"));
        let parsed = parse(packed.as_bytes()).unwrap();
        assert_eq!(parsed.format, Format::Base64LinkList);
        assert_eq!(parsed.nodes.len(), 2);
        assert_eq!(parsed.nodes[0].name, "东京");
        assert_eq!(parsed.nodes[1].server, "5.6.7.8");
        assert_eq!(parsed.skipped, 0);
    }

    #[test]
    fn duplicate_names_get_suffixes() {
        let body = format!("{L1}\n{L1}\n{L1}");
        let nodes = parse(body.as_bytes()).unwrap().nodes;
        let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["东京", "东京 (2)", "东京 (3)"]);
    }

    #[test]
    fn two_links_on_one_line_are_split() {
        let nodes = parse(format!("{L1} {L2}").as_bytes()).unwrap().nodes;
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn account_info_entries_are_not_kept_as_nodes() {
        // 真实订阅里这三条和真节点形状完全一样（同一个 uuid/出口），只能靠名字认
        let base = L1.split_once('#').unwrap().0;
        let info = [
            "剩余流量：1016.75 GB",
            "距离下次重置剩余：15 天",
            "套餐到期：2027-01-14",
        ]
        .iter()
        .map(|name| format!("{base}#{name}"))
        .collect::<Vec<_>>()
        .join("\n");
        let body = format!("{info}\n{L1}\n{L2}");
        let nodes = parse(body.as_bytes()).unwrap().nodes;
        let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["东京", "首尔"]);
    }

    #[test]
    fn clash_and_singbox_formats_are_rejected_explicitly() {
        let yaml = "mixed-port: 7890\nproxies:\n  - name: a\n    type: vless\n";
        assert_eq!(detect(yaml.as_bytes()), Format::ClashYaml);
        assert!(matches!(
            parse(yaml.as_bytes()),
            Err(SubscriptionError::ClashYaml)
        ));
        let json = "{\"outbounds\":[{\"type\":\"vless\",\"tag\":\"a\"}]}";
        assert!(matches!(
            parse(json.as_bytes()),
            Err(SubscriptionError::SingboxJson)
        ));
        assert!(matches!(
            parse(b"garbage without any link".as_slice()),
            Err(SubscriptionError::UnknownFormat)
        ));
    }

    #[test]
    fn partially_broken_subscription_keeps_good_nodes() {
        // 计数只管"形状像一条链接但读不懂"的那些。整行没有协议头的（机场加的说明文字、
        // 订阅地址本身）不进这个数——那些本来就不是线路，报成"读不懂的节点"是人话对不上事实。
        let body = format!("{L1}\nnot a link at all\n{L2}\ntrojan://@5.6.7.8:8443#空密码");
        let parsed = parse(body.as_bytes()).unwrap();
        assert_eq!(parsed.nodes.len(), 2);
        assert_eq!(parsed.skipped, 1);
    }

    #[test]
    fn subscription_info_headers_are_parsed() {
        let mut h = BTreeMap::new();
        h.insert(
            "profile-title".into(),
            "attachment; filename*=UTF-8''%E6%9C%BA%E5%9C%BA".into(),
        );
        h.insert("expire".into(), "1893456000".into());
        h.insert("total".into(), "161061273600".into());
        h.insert("used".into(), "10737418240".into());
        let meta = Meta::from_headers(&h);
        assert_eq!(meta.title.as_deref(), Some("机场"));
        assert_eq!(meta.total, Some(161061273600));
        assert!(meta.days_left(1_893_456_000 - 86_400 * 30).unwrap() > 29);
    }
}
