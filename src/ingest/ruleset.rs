//! 在线分流规则源 -> sing-box 规则集源文件 -> `.srs` 编译计划。
//!
//! 借鉴 LocalDNS-FJ945 的 provider 层：把任意外部名单（AdGuard 语法、纯域名列表、
//! sing-box 规则集 JSON、已编译的 `.srs`）统一收敛成同一个产物——`rule-set/<tag>.srs`，
//! 面板和生成器只引用这个产物，不需要知道来源是什么语法。
//!
//! 两处刻意不跟它走：
//! - 不自己实现 SRS 编解码。`sing-box rule-set compile` 就是官方编译器（已在 1.14.2
//!   源码核实，见 `docs/reference/singbox-1.14.2-schema.md` §10），自己写等于维护一份
//!   protobuf 格式的平行实现。
//! - 不维护运行时可变的名单。名单变化 = 重新 prepare = 新的期望状态，仍然走快照下发。
//!
//! 本模块全是纯函数：网络抓取在 svc 层，所以语法识别、空源守卫、tag 校验都能单测覆盖。

use crate::model::config::{is_cidr, is_safe_rule_set_tag, rule_set_product, DomainPattern};
use serde_json::{json, Map, Value};

/// sing-box 规则集 JSON 的当前版本（1.14.2 的 `option._PlainRuleSetCompat` 枚举 1..5）。
/// 缺 `version` 键时 compile 直接报 "missing rule-set version"，所以这一步不能省。
const RULE_SET_VERSION: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    /// 已经是 sing-box 编译产物，原样安装
    BinaryRuleSet,
    /// sing-box 规则集 JSON（`{"version":n,"rules":[...]}`）
    SingboxRuleSet,
    /// AdGuard/uBlock 语法文本（`||example.com^`）
    AdGuard,
    /// 一行一条的域名列表，语法与面板输入一致（`+`/`~`/`/re/`/裸域名），允许 CIDR
    DomainList,
}

#[derive(Debug, thiserror::Error)]
pub enum RulesetError {
    #[error(
        "规则集 tag {0} 非法：它会被当作文件名使用，只允许字母、数字、'-'、'_'、'.'，且不得以 '.' 开头"
    )]
    BadTag(String),
    #[error("规则源 {url} 解析出 0 条规则（{reason}）；拒绝用空名单覆盖现有规则集")]
    EmptySource { url: String, reason: String },
    #[error("规则源 {0} 既不是 UTF-8 文本也不是可识别的规则集格式")]
    Unreadable(String),
}

/// 一份来源经归一化后得到的下发计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub tag: String,
    /// 相对快照目录的产物路径，直接填进 `RuleSet::Local.path`
    pub out_path: String,
    pub format: SourceFormat,
    /// 规则条数，写进来源清单用于"下载回来的东西是不是空/塌了"的人工判断。
    /// 直通的二进制产物恒为 0：ZBox 不解析 SRS，条数未知，不做假计数。
    pub rule_count: usize,
    pub action: PlanAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    /// `.srs` 直通：无需编译，落盘后由 sing-box 自己校验
    Install,
    /// 把 `source_json` 从 stdin 交给 `sing-box rule-set compile`
    Compile { source_json: String },
}

impl Plan {
    pub fn needs_compile(&self) -> bool {
        matches!(self.action, PlanAction::Compile { .. })
    }

    /// `sing-box` 之后的参数，由 svc 层拼上可执行文件路径后执行。
    /// `stdin` 是 1.14.2 明确支持的位置参数，避免把来源内容写进一个可预测的临时文件名。
    pub fn compile_args(&self) -> Vec<String> {
        match &self.action {
            PlanAction::Install => Vec::new(),
            PlanAction::Compile { .. } => vec![
                "rule-set".into(),
                "compile".into(),
                "stdin".into(),
                "-o".into(),
                self.out_path.clone(),
            ],
        }
    }
}

/// 归一化一份已抓取到的来源。
pub fn prepare(tag: &str, url: &str, body: &[u8]) -> Result<Plan, RulesetError> {
    validate_tag(tag)?;
    let out_path = rule_set_product(tag);
    let format = SourceFormat::detect(url, body);
    let (rule_count, action) = match format {
        SourceFormat::BinaryRuleSet => (0, PlanAction::Install),
        SourceFormat::SingboxRuleSet => normalize_singbox_json(url, body)?,
        SourceFormat::AdGuard => normalize_lines(url, body, true)?,
        SourceFormat::DomainList => normalize_lines(url, body, false)?,
    };
    Ok(Plan {
        tag: tag.to_string(),
        out_path,
        format,
        rule_count,
        action,
    })
}

impl SourceFormat {
    /// 先看 URL 后缀，再看内容能不能按 UTF-8 解析，最后按行首特征分派。
    /// `.srs` 是 protobuf 编码，几乎必然含 NUL，所以"非 UTF-8"也当作二进制产物。
    pub fn detect(url: &str, body: &[u8]) -> Self {
        let without_fragment = url.split_once('#').map_or(url, |(head, _)| head);
        let path = without_fragment
            .split_once('?')
            .map_or(without_fragment, |(head, _)| head);
        if path.to_ascii_lowercase().ends_with(".srs") {
            return Self::BinaryRuleSet;
        }
        let Ok(text) = std::str::from_utf8(body) else {
            return Self::BinaryRuleSet;
        };
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') {
            return Self::SingboxRuleSet;
        }
        let mut total = 0usize;
        let mut adguard = 0usize;
        for line in text.lines() {
            let line = line.trim();
            if is_skippable(line) {
                continue;
            }
            total += 1;
            if line.starts_with("||") || line.starts_with("@@") || line.starts_with('|') {
                adguard += 1;
            }
            if total >= 64 {
                break;
            }
        }
        if total > 0 && adguard * 2 >= total {
            return Self::AdGuard;
        }
        Self::DomainList
    }
}

fn is_skippable(line: &str) -> bool {
    line.is_empty() || line.starts_with('#') || line.starts_with('!') || line.starts_with('[')
}

/// tag 会被拼成文件路径，这里就是唯一的防线（对应它的 "unsafe provider source path"）。
/// 判据本身在模型层（`is_safe_rule_set_tag`），因为 `DesiredState::validate` 要问同一个问题。
fn validate_tag(tag: &str) -> Result<(), RulesetError> {
    if !is_safe_rule_set_tag(tag) {
        return Err(RulesetError::BadTag(tag.to_string()));
    }
    Ok(())
}

fn normalize_singbox_json(url: &str, body: &[u8]) -> Result<(usize, PlanAction), RulesetError> {
    let text = std::str::from_utf8(body).map_err(|_| RulesetError::Unreadable(url.to_string()))?;
    let mut value: Value =
        serde_json::from_str(text).map_err(|_| RulesetError::Unreadable(url.to_string()))?;
    let count = count_rules(&value).ok_or_else(|| RulesetError::EmptySource {
        url: url.to_string(),
        reason: "JSON 里没有非空的 rules 数组".into(),
    })?;
    // 很多现成规则集文件不写 version，补上当前版本，否则 compile 直接拒绝
    if let Some(object) = value.as_object_mut() {
        if !object.contains_key("version") {
            object.insert("version".into(), json!(RULE_SET_VERSION));
        }
    }
    let source_json =
        serde_json::to_string(&value).map_err(|_| RulesetError::Unreadable(url.to_string()))?;
    Ok((count, compile_action(source_json)))
}

/// 逐条数组长度求和；对 AdGuard/域名列表则是接受下来的行数。
fn count_rules(value: &Value) -> Option<usize> {
    let rules = value.get("rules")?.as_array()?;
    let mut count = 0usize;
    for rule in rules {
        match rule.as_object() {
            Some(fields) => {
                for field in fields.values() {
                    count += match field {
                        Value::Array(items) => items.len(),
                        Value::String(_) => 1,
                        _ => 0,
                    };
                }
            }
            None => count += 1,
        }
    }
    if count == 0 {
        return None;
    }
    Some(count)
}

/// 把域名按 sing-box 的四种匹配类型分组，每组一条规则。
///
/// 规则集载荷里每条规则只描述"怎么算命中"、不带处置，所以这里可以按类型合并；
/// 分流阶梯那边（`gen::singbox::route::ordered_domain_matchers`）不行——那里每一行的
/// 处置可能是三种之一，用户的行序就是优先级。
fn domain_matchers(patterns: &[DomainPattern]) -> Vec<Map<String, Value>> {
    let mut exact = Vec::new();
    let mut suffix = Vec::new();
    let mut keyword = Vec::new();
    let mut regex = Vec::new();
    for pattern in patterns {
        match pattern {
            DomainPattern::Exact(s) => exact.push(s.clone()),
            DomainPattern::Suffix(s) => suffix.push(s.clone()),
            DomainPattern::Keyword(s) => keyword.push(s.clone()),
            DomainPattern::Regex(s) => regex.push(s.clone()),
        }
    }
    let mut out = Vec::new();
    for (key, values) in [
        ("domain", exact),
        ("domain_suffix", suffix),
        ("domain_keyword", keyword),
        ("domain_regex", regex),
    ] {
        if !values.is_empty() {
            let mut m = Map::new();
            m.insert(key.into(), json!(values));
            out.push(m);
        }
    }
    out
}

fn normalize_lines(
    url: &str,
    body: &[u8],
    adguard: bool,
) -> Result<(usize, PlanAction), RulesetError> {
    let text = std::str::from_utf8(body).map_err(|_| RulesetError::Unreadable(url.to_string()))?;
    let mut patterns: Vec<DomainPattern> = Vec::new();
    let mut cidrs: Vec<String> = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if is_skippable(line) {
            continue;
        }
        match classify(line, adguard) {
            Some(Item::Domain(pattern)) => patterns.push(pattern),
            Some(Item::Cidr(cidr)) => cidrs.push(cidr),
            None => skipped += 1,
        }
    }
    if patterns.is_empty() && cidrs.is_empty() {
        return Err(RulesetError::EmptySource {
            url: url.to_string(),
            reason: format!("{skipped} 行候选内容都解析不成域名或 CIDR"),
        });
    }
    // 单条规则内多个匹配字段是"与"，所以四类域名必须拆成多条规则
    let mut rules: Vec<Value> = domain_matchers(&patterns)
        .into_iter()
        .map(Value::Object)
        .collect();
    if !cidrs.is_empty() {
        let mut field = Map::new();
        field.insert("ip_cidr".into(), json!(cidrs));
        rules.push(Value::Object(field));
    }
    let count = patterns.len() + cidrs.len();
    let source_json = serde_json::to_string(&json!({
        "version": RULE_SET_VERSION,
        "rules": rules,
    }))
    .map_err(|_| RulesetError::Unreadable(url.to_string()))?;
    Ok((count, compile_action(source_json)))
}

enum Item {
    Domain(DomainPattern),
    Cidr(String),
}

/// 一行 -> 一条匹配。解析不了的一律丢弃而不是猜：猜错的名单会静默放行流量。
fn classify(line: &str, adguard: bool) -> Option<Item> {
    if is_cidr(line) {
        return Some(Item::Cidr(line.to_string()));
    }
    if !adguard {
        return DomainPattern::parse(line).map(Item::Domain);
    }
    // `@@` 是例外（白名单）、`/re/` 是 URL 正则（不是域名正则），都不属于要拦截的名单
    if line.starts_with("@@") {
        return None;
    }
    let line = line.split_once('$').map_or(line, |(head, _)| head);
    let line = line.trim_end_matches('^');
    if line.starts_with('/') || line.contains('$') || line.contains('*') {
        return None;
    }
    if let Some(rest) = line.strip_prefix("||") {
        return Some(Item::Domain(DomainPattern::Suffix(
            rest.trim_end_matches('.').to_string(),
        )));
    }
    if let Some(rest) = line.strip_prefix('|') {
        return Some(Item::Domain(DomainPattern::Exact(
            rest.trim_end_matches('|').to_string(),
        )));
    }
    // 剩余形式（如 `||a^$important` 已被剥掉、`example.com/path` 带路径）无法映射成域名匹配
    None
}

fn compile_action(source_json: String) -> PlanAction {
    PlanAction::Compile { source_json }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adguard_syntax_becomes_a_singbox_rule_set() {
        let body = "! comment\n[AdGuard]\n||example.com^\n@@||ok.com^\n||sub.cn^$important\n|exact.com|\nbare-line\n";
        let plan = prepare("ads-extra", "https://x/list.txt", body.as_bytes()).unwrap();
        assert_eq!(plan.format, SourceFormat::AdGuard);
        assert_eq!(plan.out_path, "rule-set/ads-extra.srs");
        let PlanAction::Compile { source_json } = &plan.action else {
            panic!("{:?}", plan.action);
        };
        let value: Value = serde_json::from_str(source_json).unwrap();
        assert_eq!(value["version"], json!(RULE_SET_VERSION));
        let rules = value["rules"].as_array().unwrap();
        let text = serde_json::to_string(&value).unwrap();
        // 两类匹配是两条规则（同一条里写多个键是"与"）；谁在前由 domain_matchers 决定
        assert_eq!(rules.len(), 2, "{text}");
        assert!(
            text.contains(r#""domain_suffix":["example.com","sub.cn"]"#),
            "`||d^` 是后缀匹配，`$important` 选项要剥掉：{text}"
        );
        assert!(text.contains(r#""domain":["exact.com"]"#), "{text}");
        // 白名单和无法映射的行丢弃，不能当成域名塞进名单
        assert_eq!(plan.rule_count, 3);
        assert_eq!(
            plan.compile_args(),
            [
                "rule-set",
                "compile",
                "stdin",
                "-o",
                "rule-set/ads-extra.srs"
            ],
            "参数形状按 1.14.2 的 cobra 定义逐字对齐"
        );
    }

    #[test]
    fn plain_list_shares_the_panel_domain_syntax() {
        let body = "+.smtcdns.com\n~porn\n/^[0-9a-f]{32}\\.example\\.com$/\n192.168.0.0/16\n240e::/32\nexact.com\n";
        let plan = prepare("pcdn", "https://x/pcdn.txt", body.as_bytes()).unwrap();
        assert_eq!(plan.format, SourceFormat::DomainList);
        let PlanAction::Compile { source_json } = &plan.action else {
            panic!("{:?}", plan.action);
        };
        let value: Value = serde_json::from_str(source_json).unwrap();
        let rules = value["rules"].as_array().unwrap();
        // 四个匹配键各自成一条规则：同一条里写多个字段是"与"，会把名单整个变成交集
        let keys: Vec<&str> = rules
            .iter()
            .flat_map(|r| r.as_object().unwrap().keys().map(String::as_str))
            .collect();
        assert_eq!(
            keys,
            vec![
                "domain",
                "domain_suffix",
                "domain_keyword",
                "domain_regex",
                "ip_cidr"
            ]
        );
        assert_eq!(
            value["rules"][4]["ip_cidr"],
            json!(["192.168.0.0/16", "240e::/32"])
        );
        assert_eq!(plan.rule_count, 6);
    }

    #[test]
    fn an_empty_source_never_overwrites_a_good_bundle() {
        // 对应它二进制里的 "writing an empty bundle"：宁可不装，也不能把名单清零
        for url in ["https://x/a.txt", "https://x/b.txt", "https://x/c.json"] {
            for body in [
                "",
                "# 全是注释\n\n",
                "{}",
                "{\"rules\":[]}",
                "{\"rules\":[{}]}",
            ] {
                let err = prepare("guard", url, body.as_bytes()).unwrap_err();
                assert!(
                    matches!(
                        err,
                        RulesetError::EmptySource { .. } | RulesetError::Unreadable(_)
                    ),
                    "{url} {body:?} -> {err}"
                );
            }
        }
    }

    #[test]
    fn compiled_srs_is_installed_without_recompiling() {
        let body = [0u8, 1, 2, 0xff, 0xfe];
        let plan = prepare("cn", "https://gh/x/geoip-cn.srs", &body).unwrap();
        assert_eq!(plan.format, SourceFormat::BinaryRuleSet);
        assert_eq!(plan.action, PlanAction::Install);
        assert!(!plan.needs_compile());
        // 非 UTF-8 且 URL 没后缀时同样按二进制处理，不走文本解析
        let plan = prepare("cn2", "https://gh/x/download", &body).unwrap();
        assert_eq!(plan.format, SourceFormat::BinaryRuleSet);
    }

    #[test]
    fn a_tag_that_is_not_a_safe_filename_is_refused() {
        for bad in [
            "",
            "..",
            "../evil",
            "a/b",
            "a\\b",
            ".hidden",
            "中文名",
            "a b",
        ] {
            assert!(
                matches!(
                    prepare(bad, "https://x/l.txt", "||a.com^\n".as_bytes()),
                    Err(RulesetError::BadTag(_))
                ),
                "{bad:?} 应当被拒绝"
            );
        }
        assert!(prepare("geosite-cn_1", "https://x/l.txt", "||a.com^\n".as_bytes()).is_ok());
    }

    #[test]
    fn url_suffix_decides_binary_vs_text_even_with_a_query() {
        assert_eq!(
            SourceFormat::detect("https://x/a.SRS?v=1", b"||a.com^"),
            SourceFormat::BinaryRuleSet
        );
        assert_eq!(
            SourceFormat::detect(
                "https://x/a.json",
                b"{\"rules\":[{\"domain\":[\"a.com\"]}]}"
            ),
            SourceFormat::SingboxRuleSet
        );
        assert_eq!(
            SourceFormat::detect("https://x/a.txt?raw", b"||a.com^\n||b.com^\n"),
            SourceFormat::AdGuard
        );
        assert_eq!(
            SourceFormat::detect("https://x/a.txt", b"a.com\nb.com\n"),
            SourceFormat::DomainList
        );
    }

    #[test]
    fn a_ready_made_rule_set_json_keeps_its_own_version() {
        // 已经标了低版本的现成文件不该被我们抬到 5，那会让 compile 里的降级逻辑白跑
        let body = "{\"version\":3,\"rules\":[{\"domain_suffix\":[\"a.com\"]}]}";
        let plan = prepare("ready", "https://x/rs.json", body.as_bytes()).unwrap();
        let PlanAction::Compile { source_json } = &plan.action else {
            panic!("{:?}", plan.action);
        };
        let value: Value = serde_json::from_str(source_json).unwrap();
        assert_eq!(value["version"], json!(3));
        assert_eq!(plan.rule_count, 1);
    }
}
