//! 离线预置：用产品自己的 `ingest::ruleset::prepare()` 把本地名单归一成 sing-box 规则集
//! 源 JSON，交给 `sing-box rule-set compile` 编成 `.srs`。
//!
//! 用法：`cargo run --example prepare-rulesets -- <out-dir>`
//! 打印的每条编译命令（stdin 形式）与 `svc::ruleset::Sync` 在线跑的逐字相同，
//! 所以这份产物可以直接放进 `Layout::rule_set_dir()`；对应的 `rule-set/manifest.json`
//! 条目要手写，在线路径才会把它当成"已落地"。

use std::{env, fs, path::Path, process};
use zboxd::ingest::ruleset::{self, PlanAction};

/// 实测/离线安装用的最小名单：只放真正能影响判定的条目，
/// 让 netns 探测的结论不依赖 GitHub 此刻通不通。
const SOURCES: &[(&str, &str, &str)] = &[
    (
        "geosite-cn",
        "list",
        "+baidu.com\n+qq.com\n+taobao.com\n+example.direct\n",
    ),
    (
        "geoip-cn",
        "list",
        "110.242.68.0/24\n111.13.100.0/24\n123.125.114.0/24\n220.181.38.0/24\n",
    ),
    (
        "non-cn",
        "list",
        "+google.com\n+github.com\n+gstatic.com\n+example.com\n",
    ),
];

fn main() {
    let Some(out_dir) = env::args().nth(1) else {
        eprintln!("usage: prepare-rulesets <out-dir>");
        process::exit(2);
    };
    fs::create_dir_all(&out_dir).expect("create out dir");
    for (tag, _kind, body) in SOURCES {
        let plan = ruleset::prepare(tag, &format!("file://{tag}.txt"), body.as_bytes())
            .unwrap_or_else(|e| {
                eprintln!("prepare {tag} failed: {e}");
                process::exit(1)
            });
        let PlanAction::Compile { source_json } = &plan.action else {
            eprintln!("{tag} 应当走编译分支");
            process::exit(1);
        };
        let source_path = Path::new(&out_dir).join(format!("{tag}.source.json"));
        fs::write(&source_path, source_json).expect("write source json");
        println!(
            "{}: {} 条规则 -> {}",
            tag,
            plan.rule_count,
            source_path.display()
        );
        println!(
            "  sing-box {}",
            plan.compile_args()
                .join(" ")
                .replace("stdin", &format!("(stdin: {})", source_path.display()))
        );
    }
}
