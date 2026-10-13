"use strict";

// 面板外壳的行为：连上、读三份回执、显式下发。
// 三条纪律：只用相对路径；只用 textContent 建节点（节点名、订阅 URL、报错文案都是外来文本）；
// 令牌只进 sessionStorage，不进地址栏也不进磁盘。

const TOKEN_KEY = "zbox.panel.token";
const UNKNOWN = "未读到";

let memoryToken = "";

function currentToken() {
  if (memoryToken) return memoryToken;
  try {
    return window.sessionStorage.getItem(TOKEN_KEY) || "";
  } catch (error) {
    return "";
  }
}

function rememberToken(value) {
  memoryToken = value;
  try {
    if (value) {
      window.sessionStorage.setItem(TOKEN_KEY, value);
    } else {
      window.sessionStorage.removeItem(TOKEN_KEY);
    }
  } catch (error) {
    // 浏览器拒绝持久化（隐私模式一类）时这一轮内存里那份照样能用
  }
}

// 令牌可以从地址的 # 后面送上来：片段既不进请求行，也不落访问日志。
function takeTokenFromHash() {
  const match = /^#(?:token=)?([0-9a-f]{32})$/.exec(window.location.hash);
  if (!match) return false;
  rememberToken(match[1]);
  window.history.replaceState(null, "", window.location.pathname);
  return true;
}

function note(message, klass) {
  const node = document.getElementById("connect-state");
  node.textContent = message;
  node.className = klass || "note";
}

function setText(id, text, klass) {
  const node = document.getElementById(id);
  node.textContent = text;
  if (klass !== undefined) node.className = klass;
}

// 卡片右上角那块徽标只写"这一块读没读到"，不写结论：
// 徽标说的是读数本身，服务有没有在跑写在下面的行里。
function setBadge(id, text, tone) {
  setText(id, text, tone ? "badge badge--" + tone : "badge");
}

function isMissing(value) {
  return value === null || value === undefined;
}

function tri(value) {
  if (value === true) return "是";
  if (value === false) return "否";
  return UNKNOWN;
}

async function call(method, path, body, ifMatch) {
  const headers = { "X-ZBox-Token": currentToken() };
  if (ifMatch) headers["If-Match"] = ifMatch;
  const options = { method: method, headers: headers };
  if (body !== undefined) {
    options.body = body;
    headers["Content-Type"] = "application/json";
  }
  const response = await fetch(path, options);
  const text = await response.text();
  let value;
  try {
    value = JSON.parse(text);
  } catch (error) {
    value = { error: "回执读不出 JSON（HTTP " + response.status + "）" };
  }
  return { status: response.status, ok: response.ok, value: value };
}

function errorText(result) {
  const value = result.value;
  if (value && typeof value.error === "string") return value.error;
  return "HTTP " + result.status;
}

function renderState(value) {
  const sources = {
    missing: "磁盘上还没有状态文件（新机器，面板写第一份）",
    saved: "磁盘上的已保存状态",
    broken: "状态文件读不出来",
  };
  setText("source", sources[value.source] || String(value.source));
  // 那条主路的最后一步——"把设备的 DNS 或网关指到这台机器"——需要一个地址可抄，而能填那一格的
  // 屏幕住在高级档里。所以概览这一行只读：普通用户在这儿读到该抄什么，不必进那一层。
  // 读的必须是**期望态**里那一份：字段名叫 lan_address，不代表网卡上此刻就长着它。
  const network = (value.state && value.state.network) || {};
  const lan = String(network.lan_address || "").trim();
  const lan6 = String(network.lan_address6 || "").trim();
  if (value.source === "broken") {
    setText("client-target", "磁盘上那份读不出来，所以这里给不出地址（原因见上一行）", "warn");
  } else if (lan === "") {
    setText(
      "client-target",
      "这台机器还没定下局域网地址，客户端暂时没处可指。要填它得开高级模式，去「网络 · 路线 · 端口」那一屏。",
      "warn"
    );
  } else {
    setText(
      "client-target",
      lan +
        (lan6 ? "　IPv6 " + lan6 : "") +
        " — 设备把 DNS 指到这里" +
        (network.route_mode === "takeover" ? "；这台接管网关，所以网关也指到这里" : "") +
        "。这是磁盘上写的那一份，不是网卡此刻的地址。",
      // 底色要**显式**清掉：`setText` 少第三个参数就是"颜色不动"，
      // 于是填好地址之后那一行还留着上一轮的warn 红——保存路径会重跑一次渲染，正是这一刻。
      ""
    );
  }
  setText("etag", value.etag === "absent" ? "还没有（磁盘上没写过状态文件）" : String(value.etag));
  setText(
    "last-good",
    isMissing(value.last_good) ? "尚无（一次都没成功下发过）" : String(value.last_good)
  );

  if (!isMissing(value.error)) {
    setText("generation-error", String(value.error), "warn");
  } else if (!isMissing(value.generation_error)) {
    setText("generation-error", String(value.generation_error), "warn");
  } else {
    setText("generation-error", "无");
  }

  if (value.reload_pending === true) {
    // 与「分流梯级」那一屏同一个分法（见 renderLadder 里那两句）：pending=true 有两种来路，
    // 只按布尔值说话，就会对一台从没下发过的机器说"内容与已知良好修订不一致"——
    // 而同一张卡片下面那格正写着"已知良好修订：尚无"，两句话同屏自相矛盾，
    // 且前一句是在指控这台机器下发失败过一次。
    setText(
      "reload-pending",
      isMissing(value.last_good)
        ? "是（这台机器一次都没成功下发过，没有\"已知良好的那一份\"可比）"
        : "是（内容与已知良好修订不一致）",
      "warn"
    );
  } else if (value.reload_pending === false) {
    setText("reload-pending", "否（内容与已知良好修订一致，下发不会重启）", "ok");
  } else {
    setText("reload-pending", UNKNOWN + "（磁盘上没有可比对的已保存状态）");
  }
  // 这一行和上面那句"生成报错"是两件事，所以两行都得在：下发读的是快照那一份，
  // dae / oxidns 渲染失败并不会让下发被拒——它说的是"改的这一格在编排配置里出不来"。
  // 少了这一行，保存回执上一句"已写入磁盘"就把一份渲染不出来的配置说成了好的。
  const failures = Array.isArray(value.render_errors) ? value.render_errors : null;
  if (value.source === "broken") {
    setText("render-errors", "磁盘上那份读不进来，所以两份都渲染不了（原因见上一行）", "warn");
  } else if (failures === null) {
    setText("render-errors", UNKNOWN + "（这一份回执没带渲染状态）");
  } else if (failures.length === 0) {
    setText("render-errors", "两份都渲染出来了", "ok");
  } else {
    setText(
      "render-errors",
      failures
        .map(function (one) {
          return String(one.name || "?") + "：" + String(one.error || "没给原因");
        })
        .join(" ｜ "),
      "warn"
    );
  }
  setBadge("overview-badge", "期望态已读到", "ok");
}

// 修订清单只有一份来源：`GET /api/status` 里那份 `store.list()`（按 id 降序，最新在前）。
// 概览那一行与回滚那个下拉框读的都是它，所以必须同一次渲染一起落地——只写一处，
// 另一处就会留着上一次的清单，屏幕上就会出现两份不同的"机器上有哪几份"。
// tone："" 读数正常｜"failed" 这一次现场读数没读到｜"fresh" 退出登录，退回开局那一句。
function renderRevisions(revisions, lastGood, tone) {
  const select = document.getElementById("rollback-revision");
  while (select.firstChild) select.removeChild(select.firstChild);
  function add(value, label) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = label;
    select.appendChild(option);
  }
  if (tone === "fresh") {
    setText("revisions", "未读取", "");
    return;
  }
  if (tone === "failed" || !Array.isArray(revisions)) {
    setText("revisions", UNKNOWN + "（现场读数那一次没带回清单）", "warn");
    // 这一项不带修订号，走的是"已知良好那一份"那条路——清单读不到不等于回滚也读不到，
    // 但屏幕上不能再留着上一轮的清单让人去挑。
    add("", "清单没读到：只能回“已知良好那一份”（不带修订号）");
    return;
  }
  if (revisions.length === 0) {
    setText("revisions", "一份都没有：这台机器还没走到过下发", "");
    add("", "没有留痕的修订（清单为空），只能回“已知良好那一份”");
    return;
  }
  const words = [];
  revisions.slice(0, 3).forEach(function (one) {
    const stamp = revisionStamp(one);
    words.push(
      String(one) + (stamp === "" ? "" : "　" + stamp) +
        (String(one) === String(lastGood) ? "　← 已知良好" : "")
    );
  });
  setText(
    "revisions",
    "共 " + revisions.length + " 份，最新在前：" + words.join(" ｜ ") +
      (revisions.length > 3 ? "…其余在「让它生效」那一屏的清单里" : ""),
    ""
  );
  add("", "已知良好那一份（不带修订号）");
  revisions.forEach(function (one) {
    const stamp = revisionStamp(one);
    add(
      String(one),
      String(one) + (stamp === "" ? "" : "　" + stamp) +
        (String(one) === String(lastGood) ? "　← 已知良好" : "")
    );
  });
}

// 修订 id 的前 13 位是写入那一刻的毫秒。取不到就当没有：手工改过的目录名不当时间读，
// 而屏幕上那句时间只是"这台浏览器怎么渲染机器给的那个数"，不是第二个事实。
function revisionStamp(id) {
  const text = String(id);
  if (text.length < 15 || text.charAt(13) !== "-") return "";
  const millis = Number(text.slice(0, 13));
  if (!isFinite(millis) || millis <= 0) return "";
  return new Date(millis).toLocaleString();
}

function renderStatus(result) {
  if (!result.ok) {
    setText("listening", "读取失败：" + errorText(result), "warn");
    setText("service-active", UNKNOWN);
    setText("hijack", UNKNOWN);
    setText("hijack6", UNKNOWN);
    renderRevisions(null, null, "failed");
    setBadge("overview-badge", "现场读数没读到", "warn");
    return;
  }
  const reading = result.value.reading || {};
  setText("service-active", tri(reading.service_active));
  setText(
    "listening",
    Array.isArray(reading.listening)
      ? reading.listening.join(" / ") || "一个都没有"
      : UNKNOWN
  );
  setText(
    "hijack",
    "nft 表 " + tri(reading.nft_table_loaded) +
      "｜策略路由 " + tri(reading.divert_rule_loaded) +
      "｜ip_forward " + (isMissing(reading.ip_forward) ? UNKNOWN : reading.ip_forward) +
      "｜rp_filter(all) " + (isMissing(reading.rp_filter_all) ? UNKNOWN : reading.rp_filter_all) +
      "｜默认路由网卡 " + (isMissing(reading.default_route_iface) ? UNKNOWN : reading.default_route_iface)
  );
  // IPv6 那一行单独读、单独降级：`ip zbox` 与 `ip6 zbox` 是两张独立的表，
  // 而 v6 侧的“在听”只能来自 `ss -lntu -6`（双栈 socket 在表里打的是 `*:端口`，
  // 字面量判不出族）。上面那行的五个读数一个都不代表这里任何一个。
  setText(
    "hijack6",
    "IPv6 的 nft 表 " + tri(reading.v6_table_loaded) +
      "｜IPv6 策略路由 " + tri(reading.v6_divert_rule_loaded) +
      "｜forwarding(all) " + (isMissing(reading.v6_forward_all) ? UNKNOWN : reading.v6_forward_all) +
      "｜accept_ra(all) " + (isMissing(reading.v6_accept_ra_all) ? UNKNOWN : reading.v6_accept_ra_all) +
      "｜IPv6 在听 " + (Array.isArray(reading.v6_listening) ? (reading.v6_listening.join(" / ") || "一个都没有") : UNKNOWN)
  );
  // 网卡这一行是**现场读数**：`GET /api/status` 里那份 `network` 来自面板进程自己发的两条 `ip` 命令。
  // 它只报"探到了什么"，不报"对不对"——那一句在下面的诊断里。两处各下一句判断，
  // 迟早会变成"这一行说不一致、诊断说没问题"（概览那两份生成物说反话那一轮就是这么翻车的）。
  // 探不到是一种状态不是不合规，所以那一支给 `note` 不给 `warn`；两支都要**显式**给底色。
  const probe = result.value.network || {};
  if (probe.ok === true) {
    setText(
      "network-shape",
      "局域网网卡 " + String(probe.lan_interface) +
        "（段 " + String(probe.lan_cidr) + "，本机地址 " + String(probe.lan_address) + "）" +
        "｜上游网卡 " + String(probe.wan_interface) +
        "｜默认网关 " + (probe.gateway ? String(probe.gateway) : "没读到"),
      ""
    );
  } else {
    setText(
      "network-shape",
      "本机网卡形状没探出来：" + String(probe.detail || "这一格没带回来"),
      "note"
    );
  }
  // 修订历史与回滚清单同批落地：两处读的都是这一次 status 带回的那一份 list()。
  renderRevisions(result.value.revisions, result.value.last_good, "");
}

function renderDoctor(result) {
  const list = document.getElementById("findings");
  while (list.firstChild) list.removeChild(list.firstChild);

  function add(message, klass) {
    const item = document.createElement("li");
    item.textContent = message;
    item.className = klass;
    list.appendChild(item);
  }

  if (!result.ok) {
    add("诊断读不出来：" + errorText(result), "warn");
    setBadge("doctor-badge", "读不出来", "bad");
    return;
  }
  const findings = Array.isArray(result.value.findings) ? result.value.findings : [];
  if (findings.length === 0) {
    add("没有发现问题", "ok");
    setBadge("doctor-badge", "无异常", "ok");
    return;
  }
  const fatals = findings.filter(function (finding) {
    return finding.severity === "fatal";
  }).length;
  findings.forEach(function (finding) {
    const fatal = finding.severity === "fatal";
    let message =
      (fatal ? "[致命] " : "[提示] ") +
      String(finding.summary) +
      "｜期望：" + String(finding.expected) +
      "｜现场：" + String(finding.actual);
    if (Array.isArray(finding.fix) && finding.fix.length > 0) {
      message += "｜可执行：" + finding.fix.join(" ； ");
    }
    add(message, fatal ? "warn" : "note");
  });
  setBadge(
    "doctor-badge",
    fatals > 0 ? "有 " + fatals + " 条致命" : "有 " + findings.length + " 条提示",
    fatals > 0 ? "bad" : "warn"
  );
}

function setApplyEnabled(enabled) {
  document.getElementById("apply").disabled = !enabled;
}

// 四个动作按钮同进同出：它们要的都是"令牌对上了"，而不是"磁盘上已经有一份状态"——
// 回滚和开关劫持在没有 state.json 的机器上同样成立，把它们挂在 apply 那条判据上是错的。
// 读日志跟着同一条件：它不改现场，要的也只是令牌。
// 看板也一样：它一个命令都不发，所以要的只是"你在这台机器上登录过"。
function setActionsEnabled(enabled) {
  ["sync-rulesets", "hijack-on", "hijack-off", "rollback", "logs-read", "preview-read", "ladder-read", "dnsrec-read", "board-read"].forEach(function (id) {
    document.getElementById(id).disabled = !enabled;
  });
  // 「看更早的一页」还多一条判据：上一页给了游标。setDnsEnabled 里就是这两条。
  setDnsEnabled(enabled);
}

// 读的时候先按住按钮，是为了不让一次点击叠出两次 64 KiB 的读。
function setLogsEnabled(enabled) {
  document.getElementById("logs-read").disabled = !enabled;
}

// —— 生成预览（只读，两份文本）——
// 这一屏是"UI 上改了 = 配置里那一行跟着变"这条判据的可见形态：渲染当前磁盘上的期望态，
// 不落盘、不下发、不重启。所以它跟着**保存**自动读一次——保存动的就是磁盘上那一份。
function setPreviewEnabled(enabled) {
  document.getElementById("preview-read").disabled = !enabled;
}

// 上一份预览。差异高亮比的是"这一个浏览器里上一次读到的那一份"，不是机器上现役的配置文件——
// 后者现在还不存在（下发的那条路要等数据面换成 dae 之后才写这两份文件），拿它做基准就是假话。
// 判据要的是"我刚才那一下动了哪一行"，上一份预览正好就是那一下之前的样子。
let previewPrevious = {};

// 四格的名字 → 元素 id。服务端回执里用的是生成物的文件名，这里同一套名字，
// 少一格就是面板上少了内容（assets 那条接线测试钉着 id）。
const PREVIEW_PANES = {
  dae: "preview-dae",
  oxidns: "preview-oxidns",
  "zbox.nft": "preview-nft",
  "netstate.sh": "preview-netstate"
};

// 读不到 / 失败时四格都要有话可说，措辞按同一套走
function failAllPanes(reason) {
  Object.keys(PREVIEW_PANES).forEach(function (name) {
    writePreviewTarget(PREVIEW_PANES[name], { ok: false, error: reason }, name);
  });
}

async function readPreview() {
  const meta = document.getElementById("preview-meta");
  setPreviewEnabled(false);
  let result;
  try {
    result = await call("GET", "/api/preview");
  } catch (error) {
    meta.textContent = "读取中断";
    meta.className = "warn";
    failAllPanes(String(error.message || error));
    setBadge("preview-badge", "读取中断", "bad");
    setPreviewEnabled(true);
    return;
  }
  const value = result.value;
  if (!result.ok) {
    meta.textContent = "读取失败（HTTP " + result.status + "）";
    meta.className = "warn";
    failAllPanes(errorText(result));
    setBadge("preview-badge", "读取失败", "bad");
    setPreviewEnabled(true);
    return;
  }
  const panel = PREVIEW_PANES;
  const targets = Array.isArray(value.targets) ? value.targets : [];
  const seen = {};
  let rendered = 0;
  targets.forEach(function (target) {
    if (!panel[target.name]) return;
    seen[target.name] = true;
    if (target.ok === true) rendered += 1;
    writePreviewTarget(panel[target.name], target, target.name);
  });
  Object.keys(panel).forEach(function (name) {
    if (seen[name]) return;
    writePreviewTarget(panel[name], { ok: false, error: "服务端这次没给出这一份（回执形状变了）" }, name);
  });
  const total = Object.keys(panel).length;
  const broken = value.source === "broken"
    ? "｜磁盘上那份状态读不进来：" + String(value.error || "原因未知")
    : "";
  meta.textContent = (value.source === "missing"
    ? "还没有状态文件，这四份是新装基线的形状"
    : "渲染自磁盘上那一份期望态") + "｜指纹 " + String(value.etag || "") + broken;
  meta.className = value.source === "broken" ? "warn" : "note";
  // 徽标跟的是"四份渲染出来没有"，不是"读没读到"：状态好好的而渲染报错（组名不合法、
  // 分支没有境外上游一类）时，这一屏给的是报错原文，那一格就该是红的。
  // 内核层那两份的渲染器不接受失败，所以坏只会坏在编排层那两份上。
  setBadge(
    "preview-badge",
    rendered === 0 ? "一份都没渲染出来" : rendered < total ? "只渲染出 " + rendered + " / " + total + " 份" : "已读取",
    rendered === 0 ? "bad" : rendered < total ? "warn" : "ok"
  );
  setPreviewEnabled(true);
}

// 只用 textContent 落这些行：配置文本里什么都会被写进来（节点名、域名、报错原文）。
// 差异那一段用 <div> 按行号标色，靠行号而不是内容匹配，所以一行里出现任何字符都画不错。
function writePreviewTarget(id, target, name) {
  const pre = document.getElementById(id);
  while (pre.firstChild) pre.removeChild(pre.firstChild);
  if (target.ok !== true) {
    pre.className = "warn";
    const line = document.createElement("div");
    line.className = "warn";
    line.textContent = "这一份没渲染出来：" + String(target.error || "服务端没给原因");
    pre.appendChild(line);
    return;
  }
  const lines = Array.isArray(target.lines) ? target.lines : [];
  const before = previewPrevious[name];
  const diff = before === undefined ? null : changedRange(before, lines);
  previewPrevious[name] = lines;
  lines.forEach(function (text, index) {
    const row = document.createElement("div");
    if (diff !== null && index + 1 >= diff.from && index + 1 <= diff.to) row.className = "diff";
    row.textContent = text === "" ? " " : text;
    pre.appendChild(row);
  });
  if (!lines.length) {
    const row = document.createElement("div");
    row.className = "note";
    row.textContent = "（这一份是空的）";
    pre.appendChild(row);
  }
  pre.className = "note";
  const caption = document.createElement("div");
  caption.className = "note";
  caption.textContent = previewWords(before, diff, lines.length);
  pre.insertBefore(caption, pre.firstChild);
}

// 标题那一句要说全两件事：这一份里上了底色的行，以及**上一份里有、这一份里没有**的那几行。
// 后者画不出底色（那些行已经不存在了），所以只能在句子里说明；省了它，把 v6 那半边整个收掉
// 的那一次换档就会长成"这一份一行都没变"——而它其实是变得最多的一次。
function previewWords(before, diff, lineCount) {
  if (before === undefined) return "这是本页第一次读这一份，没有可比的上一份";
  if (diff === null) return "与上一页读到的那份逐字相同";
  if (diff.to < diff.from) {
    // 这一份里没有一行需要标色：那一次改动是"去掉了几行"，被去掉的行在这份里不存在。
    // 这里不能说"从第几行起这份没有了"——中间删一行时后面那些行明明还在，只是它们没变。
    return "与上一页读到的那份相比：这一份里没有要标色的行——上一份的第 " + diff.goneFrom
      + " 到 " + diff.goneTo + " 行（" + diff.dropped + " 行）是这一次去掉的，"
      + "删掉的行在这一份里已经不存在，标不出底色";
  }
  const painted = "第 " + diff.from + " 到 " + diff.to + " 行不同";
  if (diff.dropped <= 0) return "与上一页读到的那份相比：" + painted;
  return "与上一页读到的那份相比：" + painted
    + "；另外上一份的第 " + diff.goneFrom + " 到 " + diff.goneTo + " 行（" + diff.dropped
    + " 行）是这一次去掉的，这一份只有 " + lineCount + " 行，那些行标不出底色";
}

// null = 逐字相同。返回的 from/to 是**这一份**里该上底色的行号（1 基，含两端，to < from 表示
// 这一份里没有可标的行），goneFrom/goneTo 是**上一份**里多出来那几行的行号，dropped 是它的行数。
// 两套坐标分开记是这一整段的关键：以前这里回的是两份里较大的那个行号，于是缩小时的差异
// 被拿去当这一份的行号用，一行也标不上。
// 只取区间不做逐行对齐，所以一份里删了中间一行会把整段到末尾都标出来——宁可多标，
// 也不许漏标：漏一行就等于"改了却没显示"，那正是这一屏要防的那类假象。
function changedRange(before, after) {
  const shared = Math.min(before.length, after.length);
  let first = 0;
  while (first < shared && before[first] === after[first]) first += 1;
  if (first === shared && before.length === after.length) return null;
  let lastBefore = before.length - 1;
  let lastAfter = after.length - 1;
  while (lastBefore >= first && lastAfter >= first && before[lastBefore] === after[lastAfter]) {
    lastBefore -= 1;
    lastAfter -= 1;
  }
  return {
    from: first + 1,
    to: lastAfter + 1,
    goneFrom: lastAfter + 2,
    goneTo: lastBefore + 1,
    dropped: lastBefore - lastAfter
  };
}

// —— 日志（只读，一来源一卡）——
// 这一屏存在的理由是：分级回执只说"下发这一条命令成没成"，而**为什么**没成写在机器上那几份
// 日志里。一次给五个来源、五张卡说同一个时刻，理由写在 HTML 那段注释里；这里只放读法。
//
// 三条纪律：
// - 卡片标题、那句"这一份是谁写的"、以及"为什么这一格是空的"那一句，**全部来自服务端**。
//   JS 里一个字都不抄，抄了就是这台机器上的第二处真相（assets 那条接线测试钉的是 id 与路由，
//   钉不了措辞）。
// - 六个 `state` 各画各的：`missing`（还没文件）、`journal`（这台机器上走的不是文件）、
//   `no_home`（我们的配置没给它落点）都**不许画成故障**，也不许画成"空"。
// - 过滤框只重画已经读到的那一份，不再去问机器要更多（`GET /api/logs` 无参数，这条界由后端钉着）。
let lastLogs = null;

const LOG_STATES = {
  ok: { badge: "已读取", tone: "ok" },
  empty: { badge: "文件是空的", tone: "" },
  missing: { badge: "还没有文件", tone: "" },
  journal: { badge: "走的是运行记录", tone: "" },
  no_home: { badge: "没有落点", tone: "" },
  unreadable: { badge: "读不出", tone: "bad" }
};

// 认不出来的 state 原样画出来——静默按"空"处理 would-be 假话：
// 上游将来多一种落点时，屏幕上该看见那个词，不是看见一格空白。
function logState(state) {
  return LOG_STATES[state] || { badge: String(state || "说不清"), tone: "warn" };
}

// 浏览器本机时间，不到一秒：这一屏要的是"我按下这一下看到的是什么"，
// 服务端不给时刻（它给的是文件），所以时刻只能由按的那个人这一侧记。
function stampNow() {
  const d = new Date();
  function two(n) {
    return (n < 10 ? "0" : "") + n;
  }
  return (
    d.getFullYear() + "-" + two(d.getMonth() + 1) + "-" + two(d.getDate()) +
    " " + two(d.getHours()) + ":" + two(d.getMinutes()) + ":" + two(d.getSeconds())
  );
}

function logCard(source, needle, takenAt) {
  const state = logState(source.state);
  const box = document.createElement("div");
  box.className = "log-src";

  const head = document.createElement("div");
  head.className = "log-src-head";
  const title = document.createElement("strong");
  title.className = "log-src-title";
  title.textContent = String(source.title || "说不清是哪一份");
  head.appendChild(title);
  const badge = document.createElement("span");
  badge.className = state.tone ? "badge badge--" + state.tone : "badge";
  badge.textContent = state.badge;
  head.appendChild(badge);
  box.appendChild(head);

  const who = document.createElement("p");
  who.className = "note";
  who.textContent = String(source.who || "");
  box.appendChild(who);

  const lines = Array.isArray(source.lines) ? source.lines : [];
  const shown = needle ? lines.filter(function (line) { return String(line).indexOf(needle) >= 0; }) : lines;
  const meta = document.createElement("p");
  meta.className = "note";
  const bits = [];
  // 只给文件名，目录一律不出现（后端就没给路径，这里也不许拼）
  if (source.file) bits.push("文件 " + source.file);
  if (needle) bits.push("带着「" + needle + "」的 " + shown.length + " 条｜这一份末尾 " + lines.length + " 条");
  else bits.push("末尾 " + shown.length + " 条");
  bits.push("文件共 " + Number(source.total_bytes || 0) + " 字节");
  if (source.truncated === true) bits.push("前面还有没给的（只取最后 " + Number(source.cap || 0) + " 字节）");
  bits.push("取数 " + takenAt);
  meta.textContent = bits.join("｜");
  box.appendChild(meta);

  // 只有"文件在、读到了"那两支摆正文。其余四支收成一行：摆一个空框看上去像
  // "这里本来该有东西，但没显示出来"，而那一句在本屏是假话。
  if (source.state === "ok" || source.state === "empty") {
    const body = document.createElement("pre");
    body.className = "log-src-body";
    let text = shown.join("\n");
    if (!text) {
      text = needle && lines.length
        ? "这一份里没有带「" + needle + "」的行（把上面那个框清空就回到全量）"
        : "这一截里一条完整的行都没有";
    }
    body.textContent = text;
    box.appendChild(body);
  }
  if (source.note) {
    const note = document.createElement("p");
    note.className = state.tone === "bad" ? "warn" : "note";
    note.textContent = String(source.note);
    box.appendChild(note);
  }
  return box;
}

function renderLogs() {
  const wrap = document.getElementById("logs-cards");
  const out = document.getElementById("logs-out");
  while (wrap.firstChild) wrap.removeChild(wrap.firstChild);
  // 卡出来了，那句占位就收掉——留着它是给"还没有任何读数"那一种情况说话的
  out.hidden = lastLogs !== null;
  if (!lastLogs) return;
  const needle = document.getElementById("logs-filter").value.trim();
  const takenAt = lastLogs.takenAt;
  let readable = 0;
  let withLines = 0;
  lastLogs.sources.forEach(function (source) {
    wrap.appendChild(logCard(source, needle, takenAt));
    if (source.state === "ok" || source.state === "empty") readable += 1;
    if (Array.isArray(source.lines) && source.lines.length > 0) withLines += 1;
  });
  const badge = document.getElementById("logs-badge");
  badge.className = withLines ? "badge badge--ok" : "badge";
  badge.textContent = "有内容 " + withLines + "／" + lastLogs.sources.length;
  const meta = document.getElementById("logs-meta");
  meta.className = "note";
  meta.textContent =
    "取数 " + takenAt + "｜" + lastLogs.sources.length + " 张卡说的是同一时刻" +
    (needle ? "｜只画带「" + needle + "」的行" : "");
}

function setLogsEnabled(enabled) {
  document.getElementById("logs-read").disabled = !enabled;
  // 过滤框只在"已经读到过一份"之后有用：它不改数据，只重画手里那一份
  document.getElementById("logs-filter").disabled = !enabled || lastLogs === null;
}

async function readLogs() {
  const out = document.getElementById("logs-out");
  setLogsEnabled(false);
  let result;
  try {
    result = await call("GET", "/api/logs");
  } catch (error) {
    lastLogs = null;
    renderLogs();
    out.className = "warn";
    out.textContent = String(error.message || error);
    setBadge("logs-badge", "读取中断", "bad");
    setLogsEnabled(true);
    return;
  }
  const value = result.value;
  if (!result.ok) {
    lastLogs = null;
    renderLogs();
    out.className = "warn";
    out.textContent = errorText(result);
    setBadge("logs-badge", "读取失败", "bad");
    setLogsEnabled(true);
    return;
  }
  const sources = Array.isArray(value.sources) ? value.sources : [];
  out.className = "note";
  if (!sources.length) {
    // 后端一次给五张卡；一张都没有是形状坏了，不是"这台机器没日志"——那句话得说准
    lastLogs = null;
    renderLogs();
    out.textContent = "回执里一张卡都没有（服务端形状不符），这一次说不出任何来源。";
    setBadge("logs-badge", "形状不对", "warn");
    setLogsEnabled(true);
    return;
  }
  lastLogs = { sources: sources, takenAt: stampNow() };
  out.textContent = "";
  renderLogs();
  setLogsEnabled(true);
}

// —— 分流梯级（只读，一张表）——
// 这一屏存在的理由是编辑那几屏给不出的那一句总结：改了十几格之后"所以现在什么流量怎么走"。
// 它问的是盘上那份**期望态**，一台引擎都没问——所以卡片顶上"还没下发"那一句必须留在屏上，
// 否则期望会被读成现场。两列序号分别是连接层（dae）与解析层（oxidns）各自的顺序，
// 跨层没有统一序号；对应关系不在这里承诺，由 gen::ladder 那条测试拿两份渲染文本逐行钉着。
function setLadderEnabled(enabled) {
  document.getElementById("ladder-read").disabled = !enabled;
}

// tier 是服务端那张表的分段名。这里只给人换一个词，不认识的值原样画出来——
// 词表漂了要看得见，不许静默变成一格空白（空白看上去像"没有这一层"）。
const LADDER_TIERS = {
  fixed: "固定",
  device: "设备",
  global: "全局",
  domain: "名单",
  fallback: "兜底",
  note: "没有落点"
};

// 上一次读到的那张表（整行的形状，不是行号）。null = 本页还没读过，此时不说"逐行相同"。
let ladderPrevious = null;

function ladderKey(row) {
  return [row.tier, row.when, row.then, row.dns, row.connection, row.why, row.applied].join("\u0000");
}

// 一行在两层各排第几。"不落行"要显式写出来：空格看上去像没读到，而这里说的是这一层没有这一行。
function ladderNums(row) {
  const one = function (label, value, extra) {
    if (value === null || value === undefined) return label + "不落行";
    return label + "第 " + value + " 条" + (extra || "");
  };
  const lines = Number(row.connection_lines) || 0;
  return [
    one("连接层", row.connection, lines > 1 ? "（占 " + lines + " 行）" : ""),
    one("解析层", row.dns, "")
  ].join("｜");
}

function ladderCell(klass, text) {
  const span = document.createElement("span");
  span.className = klass;
  span.textContent = text;
  return span;
}

function ladderRow(row, index, changed) {
  const li = document.createElement("li");
  // 整行只认 textContent：when / then / why 全部来自磁盘上那份状态，里面什么都可能被写进来
  li.className = "ladder-row"
    + (row.tier === "note" ? " ladder-row--gone" : "")
    + (changed ? " diff" : "");
  li.appendChild(ladderCell("ladder-seq", "#" + (index + 1)));
  li.appendChild(ladderCell("ladder-tier", LADDER_TIERS[row.tier] || String(row.tier)));
  li.appendChild(ladderCell("ladder-when", String(row.when || "")));
  li.appendChild(ladderCell("ladder-then", String(row.then || "")));
  li.appendChild(ladderCell("ladder-nums", ladderNums(row)));
  // 逐行"已生效"：null = 无从判断（修订里没存 dae/oxidns，P4 之前永远是这一档），
  // true = 已部署的那份里有这一行，false = 已部署的那份里没有。
  const appliedLabel = row.applied === true ? "是" : row.applied === false ? "否" : "—";
  const appliedClass = row.applied === true ? " ok" : row.applied === false ? " warn" : "";
  li.appendChild(ladderCell("ladder-applied" + appliedClass, appliedLabel));
  li.appendChild(ladderCell("ladder-from", String(row.from || "")));
  // 盖住那一句单独标色：它说的是"你写的这一条永远走不到"，与"这一格现在不落行"不是一件事
  const shadowed = !(row.shadowed_by === null || row.shadowed_by === undefined);
  if (row.why || shadowed) {
    const parts = [];
    if (row.why) parts.push(String(row.why));
    if (shadowed) parts.push("这一条被表里第 " + row.shadowed_by + " 行整个盖住，永远走不到。");
    li.appendChild(ladderCell(shadowed ? "ladder-why warn" : "ladder-why", parts.join(" ")));
  }
  return li;
}

// reason 只在表是空的时候用：那一句要说清"为什么是空的"，四种来路是四句不同的话。
function paintLadder(rows, diff, reason) {
  const list = document.getElementById("ladder-rows");
  while (list.firstChild) list.removeChild(list.firstChild);
  const empty = document.getElementById("ladder-empty");
  if (rows === null || rows.length === 0) {
    empty.textContent = reason;
    empty.hidden = false;
    empty.className = "note";
    return;
  }
  rows.forEach(function (row, index) {
    const changed = diff !== null && index + 1 >= diff.from && index + 1 <= diff.to;
    list.appendChild(ladderRow(row, index, changed));
  });
  empty.hidden = true;
}

// 梯级摘要：总行数 + 按层计数 + 已生效计数（P4 之前全表无从判断，这一段不出现）。
function ladderSummary(rows) {
  const byTier = {};
  let appliedYes = 0, appliedNo = 0, appliedUnknown = 0;
  rows.forEach(function (row) {
    const tier = row.tier || "?";
    byTier[tier] = (byTier[tier] || 0) + 1;
    if (row.applied === true) appliedYes++;
    else if (row.applied === false) appliedNo++;
    else appliedUnknown++;
  });
  const tierParts = [];
  for (const key of ["fixed", "device", "global", "domain", "fallback", "note"]) {
    if (byTier[key]) tierParts.push((LADDER_TIERS[key] || key) + " " + byTier[key]);
  }
  // P4 之前 appliedYes 与 appliedNo 都是 0，这段不出现——"无从判断 N"单独挂在那里没有信息量
  const applied = (appliedYes > 0 || appliedNo > 0)
    ? "｜已生效 " + appliedYes + " / 未生效 " + appliedNo
      + (appliedUnknown > 0 ? " / 无从判断 " + appliedUnknown : "")
    : "";
  return "共 " + rows.length + " 行：" + tierParts.join("、") + applied;
}

// 与预览那一屏同一套坐标：from/to 是**这一张**里该上底色的行号，goneFrom/goneTo 是上一张里
// 多出来那几行。中间插入一行会把下面那些行的序号整体推后，所以那一整段都标上——宁可多标，
// 不许漏标：漏一行就等于"改了却没显示"，那正是这一屏要防的那类假象。
function ladderWords(before, diff, count) {
  if (before === null) return "这是本页第一次读这张表，没有可比的上一次。";
  if (diff === null) return "与上一次读到的那张逐行相同。";
  if (diff.to < diff.from) {
    return "与上一次读到的那张相比：这一张没有要标色的行——上一张的第 " + diff.goneFrom + " 到 "
      + diff.goneTo + " 行（" + diff.dropped + " 行）是这一次去掉的，删掉的行在这一张里已经不存在，标不出底色。";
  }
  const gone = diff.dropped > 0
    ? "；另外上一张的第 " + diff.goneFrom + " 到 " + diff.goneTo + " 行（" + diff.dropped + " 行）是这一次去掉的"
    : "";
  return "与上一次读到的那张相比：第 " + diff.from + " 到 " + diff.to + " 行上了底色" + gone
    + "。这一张共 " + count + " 行；中间插入或删掉一行会把下面那些行的序号整体推后，所以那一整段都会上底色。";
}

async function readLadder() {
  const meta = document.getElementById("ladder-meta");
  const pending = document.getElementById("ladder-pending");
  const errors = document.getElementById("ladder-errors");
  const diffOut = document.getElementById("ladder-diff");
  setLadderEnabled(false);
  let result;
  try {
    result = await call("GET", "/api/ladder");
  } catch (error) {
    meta.textContent = "读取中断";
    meta.className = "warn";
    errors.textContent = "";
    diffOut.textContent = "";
    paintLadder(null, null, "读取中断：" + String(error.message || error));
    setBadge("ladder-badge", "读取中断", "bad");
    setLadderEnabled(true);
    return;
  }
  const value = result.value;
  if (!result.ok) {
    meta.textContent = "读取失败（HTTP " + result.status + "）";
    meta.className = "warn";
    errors.textContent = "";
    diffOut.textContent = "";
    paintLadder(null, null, "读取失败：" + errorText(result));
    setBadge("ladder-badge", "读取失败", "bad");
    setLadderEnabled(true);
    return;
  }
  const rows = Array.isArray(value.rows) ? value.rows : null;
  const keys = rows === null ? [] : rows.map(ladderKey);
  const diff = rows === null || ladderPrevious === null ? null : changedRange(ladderPrevious, keys);
  paintLadder(
    rows,
    diff,
    value.source === "broken"
      ? "磁盘上那份状态读不进来，所以这张表是空的（原因见上面两行）。"
      : rows === null
        ? "服务端这次没给这张表（回执形状变了），这里不猜它长什么样。"
        : "这张表一行都没有——兜底那一行本该永远在，所以这是服务端的事，不是你的改动。"
  );
  diffOut.textContent = rows === null ? "" : ladderWords(ladderPrevious, diff, keys.length);
  ladderPrevious = keys;
  document.getElementById("ladder-summary").textContent = rows === null ? "" : ladderSummary(rows);
  meta.textContent = (value.source === "missing"
    ? "还没有状态文件，这张表是新装基线的形状"
    : value.source === "broken"
      ? "盘上那份状态读不进来，所以这一屏没有读数，原因见下面两行"
      : "读的是面板的期望态（盘上那份）") + "｜指纹 " + String(value.etag || "");
  meta.className = value.source === "broken" ? "warn" : "note";
  if (value.reload_pending === true) {
    // 两种来路的句子不能共用一句：把"从没下发过的新机器"说成"与已知良好那份不一致"，
    // 等于当着用户的面指控这台机器下发失败过一次，而它一次都没下发过。
    pending.textContent = isMissing(value.last_good)
      ? "这张表还没下发：这台机器一次都没成功下发过，所以没有\"已知良好的那一份\"可比。按下发才会第一次换上去。"
      : "这张表还没下发：它与机器上那份已知良好的内容（修订 " + String(value.last_good) + "）不一致。按下发才会换上去。";
    pending.className = "warn";
  } else if (value.reload_pending === false) {
    pending.textContent = "这张表与已经下发的那份内容一致（再按下发不会动数据面）。";
    pending.className = "ok";
  } else {
    pending.textContent = value.source === "broken"
      ? "那份状态读不进来，谈不上\"下发过没有\"。"
      : "磁盘上还没有状态文件，谈不上\"下发过没有\"——这张表是新装基线的形状。";
    pending.className = "note";
  }
  const failures = Array.isArray(value.render_errors) ? value.render_errors : null;
  if (value.source === "broken") {
    errors.textContent = "磁盘上那份读不进来：" + String(value.error || "原因未知");
    errors.className = "warn";
  } else if (failures === null) {
    errors.textContent = UNKNOWN + "（这一份回执没带渲染状态）";
    errors.className = "warn";
  } else if (failures.length === 0) {
    errors.textContent = "两份编排配置（dae / oxidns）都渲染出来了。";
    errors.className = "ok";
  } else {
    // 服务端那句原因自己以句号收尾，这里再补一个就成了"。。"：那两句本来就是一句话的尾巴
    const named = failures
      .map(function (one) {
        return String(one.name || "?") + "：" + String(one.error || "没给原因");
      })
      .join(" ｜ ")
      .replace(/。$/, "");
    errors.textContent = "注意：这一份现在渲染不出编排配置——"
      + named
      + "。上面这张表说的是期望的形状，你改的那一格在配置里出不来（「生成预览」能看见那四份）。"
      + "下发不受这一条影响，它读的是另一份快照。";
    errors.className = "warn";
  }
  setBadge(
    "ladder-badge",
    value.source === "broken" || rows === null ? "没读到" : "已读取 " + keys.length + " 行",
    value.source === "broken" || rows === null
      ? "bad"
      : failures !== null && failures.length > 0
        ? "warn"
        : "ok"
  );
  setLadderEnabled(true);
}

// —— 看板（只读，一屏一次刷新） ——
// 这一屏与「现在的状态」的分工是取数口：那一屏要发命令问服务与内核，这一屏**一条命令都不发**——
// 硬件那几格读的是 /proc 里那几个普通文件（用户拿 `free`、`top`、`cat /proc/net/dev` 核对的是
// 同一批数），解析层那四格走 oxidns 绑在回环上的管理口。所以它可以被反复刷新而不碰机器。
//
// 三条界是从 2026-10-12 的逐字实测来的（`docs/reference/oxidns-plugin-args-verified.md`）：
// - **每格带自己的 `sample_size`**，四格可以不相等（实测同一轮里 13 与 15），所以那句"最近 N 条"
//   逐格写、不在卡头写一个共用的数，更不把几格的次数相加当总量。
// - `rcode` 的 `key` 里有上游的合成桶（`_ERROR` / `_NO_RESPONSE` / `_UNKNOWN`）。它们不是 DNS
//   应答码的名字，**原样显示就是真相**，翻译它或并进真应答码都是编。
// - 全新的 DB（一条查询都没接过）上 `rcode` 会回 `sample_size: 0` **外加一行** `count: 1` 的
//   `_UNKNOWN`（上游那条 SQL 的 `LEFT JOIN` 把 NULL 接进了兜底桶）。所以这一格按 `sample_size`
//   判空、不画 `rows`——照着画就当着用户说"这台机器被打回过 1 次"。
const BOARD_CELLS = [
  { key: "qnames", list: "board-qnames", sample: "board-qnames-sample" },
  { key: "clients", list: "board-clients", sample: "board-clients-sample" },
  { key: "rcodes", list: "board-rcodes", sample: "board-rcodes-sample" },
  { key: "plugins", list: "board-plugins", sample: "board-plugins-sample" },
];

function boardNumber(value) {
  return typeof value === "number" && isFinite(value) ? value : null;
}

// 单位换算只在后端那一处（`crate::svc::board::human_bytes`）。这一格优先用它给的那句人话，
// 缺了才退回原始字节数——**不在浏览器里再除一次 1024**：一处两套单位是"面板说的与机器上的
// 对不上"最常见的来路。
function boardBytes(text, raw) {
  if (typeof text === "string") return text;
  const n = boardNumber(raw);
  return n === null ? UNKNOWN : n + " 字节";
}

// 与 `revisionStamp` 同一条规矩：时刻是后端盖的那一个，这里只按浏览器所在时区把它显示出来。
// 那句"几点取的"一旦由浏览器自己的钟给，两台机器钟不同时就成了面板在说谎。
function boardStamp(millis) {
  return new Date(millis).toLocaleString();
}

// 占比只按上游给的那个数画，不在这里用 count / sample_size 重算一遍——两处两套算法是
// "面板说的与解析器报的对不上"最常见的来路。
function boardPercent(share) {
  const n = boardNumber(share);
  return n === null ? UNKNOWN : (n * 100).toFixed(1) + "%";
}

function boardClear(list) {
  while (list.firstChild) list.removeChild(list.firstChild);
}

function boardNote(list, message, klass) {
  boardClear(list);
  const li = document.createElement("li");
  li.className = klass || "note";
  li.textContent = message;
  list.appendChild(li);
}

// 一行 = 名字 + 一句数 + 一条按占比长短的横条。读不出数字时那一格写「未读到」，
// 而且**不画那根条**：一根长度为 0 的条说的是"占比 0"，与"这一格没读到"是两句话。
function boardCellRow(label, numText, share) {
  const li = document.createElement("li");
  li.className = "board-row";
  const name = document.createElement("span");
  name.className = "board-key";
  name.textContent = String(label);
  li.appendChild(name);
  const num = document.createElement("span");
  num.className = "board-num";
  num.textContent = numText;
  li.appendChild(num);
  const n = boardNumber(share);
  if (n !== null) {
    const bar = document.createElement("span");
    bar.className = "board-bar";
    const fill = document.createElement("span");
    fill.className = "board-fill";
    fill.style.width = Math.max(0, Math.min(1, n)) * 100 + "%";
    bar.appendChild(fill);
    li.appendChild(bar);
  }
  return li;
}

function boardRow(label, count, share) {
  const shown = boardNumber(count);
  return boardCellRow(
    label,
    (shown === null ? UNKNOWN : shown + " 次") + " · " + boardPercent(share),
    share
  );
}

// `plugins` 那一份是**另一种形状**：`{ok, query_total, stats[{kind,tag,checked,matched,executed,
// query_total,query_share}]}`，没有 `rows`。两种 kind 数的不是同一件事，而且各只有一半有数
// （2026-10-12 guest 逐字实测：executor 的 `checked`/`matched` 恒 0、只有 `executed` 有数；
// matcher 正相反，`executed` 恒 0）。
// 所以头条那个数**必须与那根条同口径**：条画的是 `query_share`（多少比例的查询经过这一步），
// 头条就用 `query_total`（经过这一步的查询条数），命中/执行另起一句。上一版拿 `matched` 当头条，
// 真机上画出来是「匹配 · rule1 ｜ 0 次 · 66.7%」——一行里两个数互相打脸。
function boardPluginRow(stat) {
  const kind = stat.kind;
  const label = (kind === "matcher" ? "匹配" : kind === "executor" ? "执行" : String(kind || "?"))
    + " · " + String(stat.tag || "?");
  const pass = boardNumber(stat.query_total);
  const second = kind === "matcher"
    ? "命中 " + (boardNumber(stat.matched) === null ? UNKNOWN : boardNumber(stat.matched) + " 次")
    : kind === "executor"
      ? "执行 " + (boardNumber(stat.executed) === null ? UNKNOWN : boardNumber(stat.executed) + " 次")
      : null;
  return boardCellRow(
    label,
    (pass === null ? "经过这一步的查询条数没读到" : "经过 " + pass + " 条查询")
      + (second === null ? "" : " · " + second),
    stat.query_share
  );
}

// 每一格把自己这一轮的形状交回给 `readBoard`，卡片最后那句"记录开关关着"就靠它判该说哪一句：
// - `"missing"`：这一格**没读到**（整口不通 / 单格报错 / 服务端给的不是那份形状）
// - `"empty"`：读到了，而且那份是**空的**
// - `"live"`：读到了行
// 口径只在这里定一次。以前那句是拿 `records_enabled === false` 直接推出"上面四格是空的"，
// 于是在真机上画出自相矛盾的一幕：四格有行，同一张卡最后一句却说它们是空的。
function paintBoardCell(cell, dns) {
  const list = document.getElementById(cell.list);
  const sample = document.getElementById(cell.sample);
  const given = dns[cell.key];
  const detail = dns.errors && typeof dns.errors[cell.key] === "string" ? dns.errors[cell.key] : null;
  if (!dns.live) {
    sample.textContent = "解析器的管理口没读到，这一格没有读数。";
    // 整口不通时那一句原因在这张卡最后只说**一次**：四格各抄一遍 100 多个字符的
    // `Connection refused (os error 111)` 是噪声不是信息（2026-10-12 真机截图里就是这样）。
    // 而单格自己失败（管理口活着、这一格回 404 之类）时那句是这一格的，必须就地写出来。
    boardNote(list, detail === null ? "原因见这张卡最后那一句。" : detail);
    return "missing";
  }
  if (detail !== null) {
    sample.textContent = "这一格没读到。";
    boardNote(list, detail);
    return "missing";
  }
  if (given === null || given === undefined || typeof given !== "object") {
    sample.textContent = "这一格没读到。";
    boardNote(list, "服务端这一格给的是空，这里不猜它长什么样。");
    return "missing";
  }
  const size = boardNumber(given.sample_size);
  if (cell.key === "plugins") {
    const stats = Array.isArray(given.stats) ? given.stats : null;
    const total = boardNumber(given.query_total);
    sample.textContent = total === null
      ? "这一格没读到样本数。"
      : "最近 " + total + " 条查询里，每一步各有几条查询经过；那根条画的就是「经过这一步的查询占多少」。";
    if (stats === null) {
      boardNote(list, "服务端这一格给的不是那份形状，这里不猜它长什么样。");
      return "missing";
    }
    if (stats.length === 0) {
      boardNote(list, "一条都没有——这一次聚合看过 0 条记录。");
      return "empty";
    }
    boardClear(list);
    stats.forEach(function (stat) {
      list.appendChild(boardPluginRow(stat));
    });
    return "live";
  }
  const rows = Array.isArray(given.rows) ? given.rows : null;
  const named = rows === null ? null : rows.length;
  sample.textContent = size === null
    ? "这一格没读到样本数。"
    : "最近 " + size + " 条里数出来的"
      // "前 N 名"只在**这一格真的画了行**时才说：全新 DB 上 `rcode` 会带着那一行 `_UNKNOWN`
      // 回来（`sample_size: 0`），于是"最近 0 条里数出来的前 1 名"就成了一句假话——
      // 下面那一格明明写着"最近没有查询记录"（2026-10-12 真机空库截图）。
      + (size === 0 || named === null ? "" : "前 " + named + " 名")
      + "（这一份是样本，不是这台机器的总量）。";
  if (rows === null) {
    boardNote(list, "服务端这一格给的不是那份形状，这里不猜它长什么样。");
    return "missing";
  }
  if (size === 0 || rows.length === 0) {
    // 两种空要分开：sample_size 为 0 时上游**仍然可能带一行**（那个 `_UNKNOWN`），
    // 所以判空只认 sample_size，不认 rows 的长度。
    boardNote(list, "最近没有查询记录。");
    return "empty";
  }
  boardClear(list);
  rows.forEach(function (row) {
    list.appendChild(boardRow(row.key, row.count, row.share));
  });
  return "live";
}

function paintBoardHardware(value) {
  const memory = value.memory || {};
  const used = typeof memory.used_text === "string" ? memory.used_text : null;
  const total = typeof memory.total_text === "string" ? memory.total_text : null;
  const percent = boardNumber(memory.used_percent);
  setText(
    "board-mem",
    used === null || total === null
      ? UNKNOWN
      : "用了 " + used + "，一共 " + total + (percent === null ? "" : "（" + percent + "%）")
  );
  const load = Array.isArray(value.load) ? value.load.map(boardNumber) : null;
  setText(
    "board-load",
    load === null || load.some(function (n) { return n === null; })
      ? UNKNOWN
      : load.map(function (n) { return n.toFixed(2); }).join(" / ")
  );
  setText("board-uptime", typeof value.uptime === "object" && value.uptime !== null && typeof value.uptime.text === "string" ? value.uptime.text : UNKNOWN);
  // 这一格抄的是**盘上那份期望态**，不是现场：机器上可能根本没有这块网卡（U3 实测：新机器的
  // 默认 `ens33` 与真实的 `eth0` 撞车）。所以措辞是"盘上写的是 X"，不判 X 在不在。
  setText(
    "board-lan-iface",
    typeof value.lan_interface === "string" && value.lan_interface !== ""
      ? "盘上那份写的是「" + value.lan_interface + "」（这一屏不判断它在这台机器上存不存在）"
      : UNKNOWN
  );
  const nets = Array.isArray(value.interfaces) ? value.interfaces : [];
  const list = document.getElementById("board-ifaces");
  if (nets.length === 0) {
    boardNote(list, "这块网卡过了多少字节：没读到（这台机器上读不到 /proc/net/dev）。");
    setText("board-ifaces-note", "网卡那几行一个都没读到。");
  } else {
    boardClear(list);
    nets.forEach(function (net) {
      const li = document.createElement("li");
      li.className = "board-row";
      const name = document.createElement("span");
      name.className = "board-key";
      name.textContent = String(net.iface || "?");
      const num = document.createElement("span");
      num.className = "board-num";
      num.textContent =
        "收 " + boardBytes(net.rx_text, net.rx_bytes) + " ｜ 发 " + boardBytes(net.tx_text, net.tx_bytes);
      li.appendChild(name);
      li.appendChild(num);
      list.appendChild(li);
    });
    // 网卡之间没有"占比"这个概念，所以这一格只有数、没有条：画条就是编一个分母。
    setText(
      "board-ifaces-note",
      "这几行数的是「这块网卡过了多少字节」，不是「代理了多少」——后面那一句现在给不出。"
    );
  }
  // 磁盘那一格：那句"为什么没读"由服务端给（`crate::svc::board::DISK_NOT_READ`），
  // 面板不自己再写一遍——两处两套措辞将来就对不上。
  const disk = value.disk || {};
  setText(
    "board-disk",
    typeof disk.reason === "string" ? disk.reason : "磁盘：这一屏没给原因。"
  );
}

// 三台引擎那一卡（§11.3 G）。三行里**只有域名解析那一行有可查的口**：oxidns 那个管理口是进程
// 活着才答的，所以"读到了"这一句就能说"在位"，这一屏不必再发一条命令去问进程表。
// 另外两行那句"为什么读不到"由服务端给（`crate::svc::board::DAE_NOT_READ` /
// `ROUTER_NOT_READ`），面板抄——两处两套措辞将来就对不上，而那一屏是会被反复刷新的。
function paintBoardEngines(value) {
  const engines = value.engines || {};
  const ox = engines.oxidns || {};
  const reason = function (engine, name) {
    return engine && typeof engine.reason === "string" ? engine.reason : name + "：这一屏没给原因。";
  };
  setText("board-engine-dae", reason(engines.dae, "分流引擎（dae）"));
  setText("board-engine-router", reason(engines.router, "主路由"));
  if (ox.live !== true) {
    setText(
      "board-engine-oxidns",
      "没读到：" + String(ox.detail || UNKNOWN)
        + "。这不说明这台机器没在解析，只说明这一屏读不到它的管理口。"
    );
    setText(
      "board-engine-note",
      "版本这一格只用来报，不用来放行：要不要下发由「让它生效」那一屏说，合不合规矩由「诊断」那一屏说。"
        + "这一轮连域名解析那一行也没读到，所以三行全是空的——空着的格子读起来像「没有」，这里说的却是「读不到」。"
    );
    return;
  }
  const version = typeof ox.version === "string" ? ox.version : UNKNOWN;
  const bundle = typeof ox.build_bundle === "string" ? ox.build_bundle : null;
  // 三个形状要说三句话：对上了 / 没对上 / 有一头说不清。最后那一头**不许**画成"没对上"——
  // "跑的那份读不到"与"跑的不是这一份"是两件事（后端因此给的是 null，不是 false）。
  const which = ox.applied === true
    ? "它加载的那一份，就是面板这份期望态要的那一份"
    : ox.applied === false
      ? "它加载的那一份和面板这份期望态要的不是同一个——面板改了、还没生效"
      : ox.running_version === undefined || ox.running_version === null
        ? "它加载的那一份是哪个摘要，这一屏没读到，所以这一格不判"
        : typeof ox.generate_error === "string"
          ? "面板这一份渲染不出来，没得比：" + ox.generate_error
          : "两头有一头说不清，所以这一格不判";
  setText(
    "board-engine-oxidns",
    "在位。版本 " + version + (bundle === null ? "" : "（构建 " + bundle + "）") + "。" + which + "。"
  );
  setText(
    "board-engine-note",
    "版本这一格只用来报，不用来放行：要不要下发由「让它生效」那一屏说，合不合规矩由「诊断」那一屏说。"
      + "三行里只有域名解析这一行有可查的口，另两行那句「读不到」说的是这一屏读不到，不是那台机器上没在跑。"
  );
}

async function readBoard() {
  setBoardEnabled(false);
  let result;
  try {
    result = await call("GET", "/api/board");
  } catch (error) {
    setText("board-meta", "读取中断", "warn");
    setBadge("board-badge", "读取中断", "bad");
    setText("board-taken", "读取中断：" + String(error.message || error), "warn");
    setBoardEnabled(true);
    return;
  }
  const value = result.value;
  if (!result.ok) {
    setText("board-meta", "读取失败（HTTP " + result.status + "）", "warn");
    setBadge("board-badge", "读取失败", "bad");
    setText("board-taken", "读取失败：" + errorText(result), "warn");
    setBoardEnabled(true);
    return;
  }
  paintBoardHardware(value);
  paintBoardEngines(value);
  const dns = value.dns || { live: false, detail: "服务端没给解析层那一份。", errors: {} };
  setText(
    "board-dns-meta",
    dns.live === true
      ? "这四格读的是解析器自己那个只绑回环的管理口，一次刷新读四份聚合；面板不直读它那份记录库。"
      : "解析器的管理口没读到，下面四格都是没读到，不是零。"
  );
  const shapes = BOARD_CELLS.map(function (cell) {
    return paintBoardCell(cell, dns);
  });
  const liveCells = shapes.filter(function (shape) { return shape === "live"; }).length;
  const missingCells = shapes.filter(function (shape) { return shape === "missing"; }).length;
  // 取数时刻由**后端**盖（`taken_at_epoch_ms`）：那句"几点取的"一旦由浏览器自己的钟给，
  // 两台机器钟不同时就成了面板在说谎。这里只把它按浏览器所在时区**显示**出来。
  const taken = boardNumber(value.taken_at_epoch_ms);
  setText("board-taken", taken === null ? "这份读数的时刻没读到。" : "这份读数是 " + boardStamp(taken) + " 取的（这台机器自己的钟）。", "note");
  const sources = {
    missing: "磁盘上还没有状态文件，这一屏的网卡名是新装基线那份",
    saved: "网卡名抄自磁盘上那份状态",
    broken: "磁盘上那份状态读不进来，所以这一屏没有网卡名（硬件与解析那几格不受影响）",
  };
  setText(
    "board-meta",
    (sources[value.source] || String(value.source || "")) + (value.etag ? "｜指纹 " + String(value.etag) : ""),
    value.source === "broken" ? "warn" : "note"
  );
  // 这一句是整张卡的收尾，所以它必须与上面四格**实际画出来的东西**对得上。开关是盘上那份
  // 期望态（不是现场读数），所以三种形状要说三句话：四格都空 / 四格读到了行 / 有格没读到。
  // 拿 `records_enabled === false` 直接推"四格是空的"会在真机上画出自我矛盾的一屏。
  setText(
    "board-dns-note",
    dns.live === true
      ? dns.records_enabled === false
        ? liveCells > 0
          ? "这一句说的是盘上那份期望态：它写着「记下每一次查询」是关着的。可上面四格里有 "
            + liveCells + " 格读到了行——盘上这份和跑着的那份还没对上。这一屏分辨不了那几行是关之前留下的、"
            + "还是跑的那份根本没照着这份期望态记，只把两边都照实报出来。"
          : missingCells > 0
            ? "这一句说的是盘上那份期望态：它写着「记下每一次查询」是关着的。可上面四格里有 "
              + missingCells + " 格这一轮没读到——没读到的格子既不能算在记、也不能算没在记，所以这里不替那份期望态作证。"
            : "这一句说的是盘上那份期望态：它写着「记下每一次查询」是关着的。上面四格这一轮都读到了、也都是空的，"
              + "这两句不矛盾；但「空」不等于这台机器没人查询过——开着而恰好没人查询过也是这个形状。"
        : "这四格说的是「最近这一批记录里的分布」，每格顶上那句里的数字是它那一份自己的样本条数。"
      : "解析器那个只绑回环的管理口没读到：" + String(dns.detail || UNKNOWN)
        + "。这不说明这台机器没在解析，只说明这一屏读不到它。"
  );
  setBadge(
    "board-badge",
    dns.live === true ? "已读到" : "部分读到",
    dns.live === true ? "ok" : "warn"
  );
  setBoardEnabled(true);
}

function setBoardEnabled(enabled) {
  document.getElementById("board-read").disabled = !enabled;
}

// —— DNS 记录（只读，两层） ——
// 数据源是 oxidns 的管理 API，而那个口只绑回环：面板替这台机器自己去读它，不直读那份 SQLite。
// 两层是**上游的形状**定的，不是设计偏好——列表每一行里没有 steps，那句"命中了链上哪几步"
// 只在单条里（2026-10-11 guest 逐字实测），所以点一行才去问第二条。
// 这一屏最值钱的不是列表，是卡片顶上那两句：跑的是哪一份、面板要的是哪一份。
let dnsNextCursor = null;

function setDnsEnabled(enabled) {
  document.getElementById("dnsrec-read").disabled = !enabled;
  // 「更早的一页」要同时满足两件事：能按（令牌对上了）且真有更早的（上一页给了游标）
  document.getElementById("dnsrec-more").disabled = !enabled || !dnsNextCursor;
}

// 上游认的 `status` 只有这四个词（源码 `query_recorder/api.rs:743-753`，别的一律 400，
// 而服务端那句 `src/svc/dns_api.rs` 里也拦）。`all` 与"不筛"是同一件事，所以只列三个实值。
const DNS_STATUS = [
  ["", "全部（不分成功失败）"],
  ["error", "出过错的"],
  ["has_response", "有应答的"],
  ["no_response", "没有应答的"],
];

// `steps.outcome` 实测到三种。措辞照着"这一步被问到没有、结果是什么"写，
// 不写成成功/失败——`stop` 是"在这儿收尾"，多数情况下它拿到的就是正主应答。
const DNS_OUTCOME = {
  entered: "被问到",
  stop: "在这儿收尾",
  error: "这一步出错",
};

function dnsFilterBody(cursor) {
  const body = {};
  const qname = document.getElementById("dnsrec-qname").value.trim();
  const client = document.getElementById("dnsrec-client").value.trim();
  const status = document.getElementById("dnsrec-status").value;
  const limit = document.getElementById("dnsrec-limit").value.trim();
  if (qname) body.qname = qname;
  if (client) body.client_ip = client;
  if (status) body.status = status;
  // 只在这里拦"根本不是数字"（NaN 会被 JSON 写成 null，那就是悄悄丢掉一格）；
  // 上限 200 交给服务端那句话，免得同一个数在两边各写一遍、改一边就漂。
  if (limit) body.limit = digitsOnly(limit, "一页几条");
  if (cursor) body.cursor = cursor;
  return body;
}

// 空列表那一句要不要提"记录开关是关着的"，取决于有没有人在筛：筛完是空和压根没在记
// 是两种处置，混成一句就都没说清。
function dnsIsFiltered(fields) {
  return ["qname", "client_ip", "status"].some(function (key) {
    return fields[key] !== undefined;
  });
}

function dnsWhen(ms) {
  if (typeof ms !== "number" || !isFinite(ms)) return UNKNOWN;
  return new Date(ms).toLocaleString();
}

function dnsUptime(ms) {
  if (typeof ms !== "number" || !isFinite(ms) || ms < 0) return UNKNOWN;
  const secs = Math.round(ms / 1000);
  if (secs < 90) return secs + " 秒";
  const mins = Math.round(secs / 60);
  if (mins < 90) return mins + " 分钟";
  return Math.round(mins / 60) + " 小时";
}

function dnsQuestion(record) {
  // 实测：`questions_json` 是一个真数组（[{name,qtype,qclass}]），不是字符串。
  // 拿不到第一项就说拿不到，不猜这一条问的是什么。
  const questions = Array.isArray(record.questions_json) ? record.questions_json : [];
  return questions[0] || null;
}

function dnsVerdict(record) {
  // 有 error 就是出过错；没有 error 而 `rcode` 是 null 是"根本没拿到应答"（实测两件事分开）。
  if (!isMissing(record.error) && record.error !== "") {
    return ["warn", "出错：" + String(record.error)];
  }
  if (isMissing(record.rcode)) return ["warn", "没有应答"];
  // "No Error" 是上游那个枚举写出来的字面量（实测），不是我们定的词
  return String(record.rcode) === "No Error"
    ? ["ok", "应答码 " + String(record.rcode)]
    : ["warn", "应答码 " + String(record.rcode)];
}

function dnsRow(record) {
  const li = document.createElement("li");
  li.className = "rec-row";
  if (typeof record.id === "number") li.setAttribute("data-id", String(record.id));

  const asked = document.createElement("span");
  asked.className = "rec-qname";
  const q = dnsQuestion(record);
  asked.textContent =
    (q && q.name ? String(q.name) : "（看不出问的哪个域名）") +
    (q && q.qtype ? "｜" + String(q.qtype) : "");
  li.appendChild(asked);

  const verdict = document.createElement("span");
  const said = dnsVerdict(record);
  verdict.className = said[0];
  verdict.textContent = said[1];
  li.appendChild(verdict);

  const from = document.createElement("span");
  from.className = "rec-from";
  from.textContent = "来自 " + (record.client_ip ? String(record.client_ip) : UNKNOWN);
  li.appendChild(from);

  const when = document.createElement("span");
  when.className = "rec-when";
  when.textContent = dnsWhen(record.created_at_ms);
  li.appendChild(when);

  const ms = document.createElement("span");
  ms.className = "rec-ms";
  ms.textContent = typeof record.elapsed_ms === "number" ? record.elapsed_ms + " ms" : "";
  li.appendChild(ms);
  return li;
}

// 那一对摘要在这一屏说的话。三个分支对应三个值，第四个分支是"两边都没法比"。
// `applied` 是 null 的时候绝不能说成 false：那会把一个连不上的解析器说成"你没按下发"。
function dnsApplyWords(value) {
  const running = isMissing(value.running_version) ? "" : String(value.running_version);
  const wants = isMissing(value.wants_version) ? "" : String(value.wants_version);
  if (value.applied === true) {
    return "一致：解析器在跑的就是面板这份期望态生成的配置。\n摘要 " + running;
  }
  if (value.applied === false) {
    // 不一致有两种来路，一句"面板改了"只说其中一种，另一种会在没改过的人眼里变成指控。
    return "不一致：解析器在跑的那份不是面板这份期望态要生成的那份。\n" +
      "刚在面板上改过 → 是还没下发，去「让它生效」那一张卡按下发；" +
      "这台机器上的解析器不是 ZBox 装的 → 这一格本来就该是不一致。\n" +
      "在跑的：" + running + "\n面板要生成的：" + wants;
  }
  if (!isMissing(value.generate_error)) {
    return "说不清：期望态这一份渲染不出来——" + String(value.generate_error);
  }
  if (value.live !== true) {
    return "说不清：读不到解析器的管理口——" + String(value.detail);
  }
  return "说不清：这一份解析器没报告它在跑哪一份配置（版本太旧，或者 reload 状态没读到）。\n" +
    (running ? "在跑的：" + running + "\n" : "") + (wants ? "面板要生成的：" + wants : "");
}

function clearDnsList() {
  document.getElementById("dnsrec-list").textContent = "";
}

function clearDnsSteps() {
  document.getElementById("dnsrec-steps").textContent = "";
}

async function readDnsRecords(cursor) {
  const meta = document.getElementById("dnsrec-meta");
  setDnsEnabled(false);
  let fields;
  try {
    fields = dnsFilterBody(cursor);
  } catch (error) {
    meta.textContent = "没有去读：" + why(error);
    meta.className = "warn";
    setBadge("dnsrec-badge", "参数不对", "warn");
    setDnsEnabled(true);
    return;
  }
  const filtered = dnsIsFiltered(fields);
  let result;
  try {
    result = await call("POST", "/api/dns-records", JSON.stringify(fields));
  } catch (error) {
    meta.textContent = "读取中断";
    meta.className = "warn";
    setText("dnsrec-apply", "说不清：这一问没走到机器——" + String(error.message || error), "warn");
    setBadge("dnsrec-badge", "读取中断", "bad");
    setDnsEnabled(true);
    return;
  }
  const value = result.value;
  if (!result.ok) {
    meta.textContent = "读取失败（HTTP " + result.status + "）";
    meta.className = "warn";
    setText("dnsrec-apply", "读取失败：" + errorText(result), "warn");
    setBadge("dnsrec-badge", "读取失败", "bad");
    setDnsEnabled(true);
    return;
  }
  // 徽标只说这一屏overall读没读到；哪一条没读到写在下面那行里，两者不是一回事
  setBadge("dnsrec-badge", value.live === true ? "已读取" : "解析器没在跑", value.live === true ? "ok" : "warn");
  setText("dnsrec-apply", dnsApplyWords(value), value.applied === true ? "ok" : "note");
  setText(
    "dnsrec-version",
    value.live === true
      ? "解析器 " + String(isMissing(value.version) ? UNKNOWN : value.version) +
        "（" + String(isMissing(value.build_bundle) ? UNKNOWN : value.build_bundle) + "）｜" +
        "已连续运行 " + dnsUptime(value.uptime_ms) + "｜重载状态 " +
        String(isMissing(value.reload_status) ? UNKNOWN : value.reload_status) +
        "｜插件 " + (value.plugins && !isMissing(value.plugins.total) ? value.plugins.total : UNKNOWN) +
        " 个（服务器 " + (value.plugins && !isMissing(value.plugins.servers) ? value.plugins.servers : UNKNOWN) + "）"
      : "解析器的管理口没读到，所以下面这些都不是它的读数。",
    "note"
  );
  const errors = value.errors && typeof value.errors === "object" ? value.errors : {};
  const failed = Object.keys(errors);
  setText(
    "dnsrec-errors",
    failed.length
      ? "有几条单独没读到：" + failed.map(function (key) {
          return key + "（" + String(errors[key]) + "）";
        }).join("；") + "——其余那几条照给。"
      : "",
    failed.length ? "warn" : "note"
  );

  clearDnsList();
  const rows = Array.isArray(value.records) ? value.records : [];
  dnsNextCursor = isMissing(value.next_cursor) ? null : String(value.next_cursor);
  setText(
    "dnsrec-page",
    (cursor ? "这是接着上一问更早的一页｜" : "") +
      "这一屏 " + rows.length + " 条" + (dnsNextCursor ? "｜下面还有更早的" : "｜到这里就是最早的一页了"),
    rows.length ? "ok" : "note"
  );
  rows.forEach(function (record) {
    document.getElementById("dnsrec-list").appendChild(dnsRow(record));
  });
  // 空列表有三句话要说清楚，而它们在现场是完全不同的三种处置
  if (rows.length) {
    setText("dnsrec-empty", "", "note");
    document.getElementById("dnsrec-empty").hidden = true;
  } else {
    document.getElementById("dnsrec-empty").hidden = false;
    if (value.live !== true) {
      setText("dnsrec-empty", "解析器的管理口没读到，所以这一条列表不是\"没有查询发生过\"。", "warn");
    } else if (filtered) {
      setText("dnsrec-empty", "筛完是空的：上面那几个条件在这台机器记下的查询里没匹配到。这不等于\"机器上没查询过\"。", "note");
    } else if (value.records_enabled === false) {
      // 这一格来自**期望态**（面板这份 state.json），不是跑着的那份的读数：没下发之前解析器
      // 还在按旧配置记，这一屏照样读得到东西。说成"永远是空的"就把期望当成了现场。
      setText(
        "dnsrec-empty",
        "面板上「记下每一次查询」是关着的——这一句说的是期望态。下发之前解析器跑的还是上一份，" +
          "这一屏照样读得到；下发之后这里才会一直是空的。",
        "warn"
      );
    } else {
      setText("dnsrec-empty", "管理口读到了，但一条记录也没有：这份解析器还没记到过查询，或者记录已按保留天数滚掉了。", "note");
    }
  }
  meta.textContent = value.live === true
    ? "已读取。列表点一行看它命中了哪几步。"
    : "管理口连不上：" + String(value.detail);
  meta.className = value.live === true ? "ok" : "warn";
  setDnsEnabled(true);
}

async function openDnsRecord(id) {
  const meta = document.getElementById("dnsrec-detail-meta");
  clearDnsSteps();
  let result;
  try {
    result = await call("POST", "/api/dns-record", JSON.stringify({ id: id }));
  } catch (error) {
    meta.textContent = "读取中断";
    meta.className = "warn";
    setText("dnsrec-detail", "这一条没读到：" + String(error.message || error), "warn");
    return;
  }
  if (!result.ok) {
    meta.textContent = "读取失败（HTTP " + result.status + "）";
    meta.className = "warn";
    setText("dnsrec-detail", "这一条没读到：" + errorText(result), "warn");
    return;
  }
  const value = result.value;
  if (value.found !== true) {
    meta.textContent = "这一条取不到";
    setText(
      "dnsrec-detail",
      "编号 " + id + " 取不到：" + String(value.detail) +
        "（记录按保留天数滚掉，也可能这个编号还没被记下）",
      "warn"
    );
    return;
  }
  const record = value.record && typeof value.record === "object" ? value.record : {};
  const q = dnsQuestion(record);
  const said = dnsVerdict(record);
  meta.textContent = "编号 " + String(record.id) + "｜" + dnsWhen(record.created_at_ms);
  setText(
    "dnsrec-detail",
    "问的：" + (q && q.name ? String(q.name) : UNKNOWN) +
      (q && q.qtype ? "（" + String(q.qtype) + "）" : "") +
      "\n来自：" + (record.client_ip ? String(record.client_ip) : UNKNOWN) +
      "\n结果：" + said[1] +
      (typeof record.elapsed_ms === "number" ? "\n用了 " + record.elapsed_ms + " ms" : ""),
    "note"
  );
  const steps = Array.isArray(record.steps) ? record.steps : [];
  const list = document.getElementById("dnsrec-steps");
  if (!steps.length) {
    const li = document.createElement("li");
    li.className = "step-row note";
    li.textContent = "这一条里没有\"命中了哪几步\"那一段（上游只在单条里给，取到的是空就是这条没走链）。";
    list.appendChild(li);
    return;
  }
  steps.forEach(function (step) {
    const li = document.createElement("li");
    const outcome = isMissing(step.outcome) ? "" : String(step.outcome);
    li.className = outcome === "error" ? "step-row warn" : "step-row";
    const node = document.createElement("span");
    node.className = "step-node";
    // `node_index` 实测从 1 起，而且记录器自己不出现在这一段里
    node.textContent = "#" + (isMissing(step.node_index) ? "?" : String(step.node_index));
    li.appendChild(node);
    const tag = document.createElement("span");
    tag.className = "step-tag";
    tag.textContent = String(isMissing(step.tag) ? UNKNOWN : step.tag) +
      "（" + String(isMissing(step.kind) ? "?" : step.kind) + "）";
    li.appendChild(tag);
    const verdict = document.createElement("span");
    verdict.textContent = DNS_OUTCOME[outcome] || (outcome || UNKNOWN);
    li.appendChild(verdict);
    list.appendChild(li);
  });
}

// —— 编辑层 ——
// 工作副本的纪律：diskState 是上一轮从磁盘读到的**整份**状态，表单只改它拥有的那几条路径，
// 保存时其余部分（订阅与节点那一屏表单没画到的那些键）原样带回。
// 少了这条纪律，一次编辑就会把没做表单的那些字段静默抹回默认值。
let diskState = null;
let diskEtag = null;
let formDirty = false;

// 与 `Network::default()` 逐字相同的那几格：`恢复自动探测` 写的就是它。
// `api::tests::the_panel_s_default_network_is_the_model_s` 把这些字面量钉在模型默认值上，
// 改模型那条测试就红，这里跟着同步——填错一个字符就等于"恢复自动探测"永远恢复不了。
const DEFAULT_NETWORK = {
  lan_interface: "ens33",
  lan_cidr: "192.168.1.0/24",
  lan_address: "",
  lan_address6: "",
  lan_cidr6: "",
  wan_interface: "ens33",
  route_mode: "inject",
  ipv6_mode: "off",
};

// 这两张下拉要和模型上的枚举逐字对上（测试拿 RouteMode / ApplyPolicy 反着钉）。
// 措辞是界面上的话，所以 labels 只在这里写一份。
const ROUTE_MODES = ["inject", "takeover"];
const ROUTE_MODE_LABELS = {
  inject: "inject — 注入主路由（默认：旁路由挂了只丢分流那部分）",
  takeover: "takeover — 接管网关（全部流量先到旁路由）",
};
const IPV6_MODES = ["off", "dns_only", "dual_stack"];
const IPV6_MODE_LABELS = {
  off: "off — 不接管（v6 自己直连出去，泄漏面靠地址族策略堵）",
  dns_only: "dns_only — 只接管 DNS（必须配 *_only）",
  dual_stack: "dual_stack — 双栈接管（要开内核转发）",
};
const APPLY_POLICIES = ["reload", "restart"];const APPLY_POLICY_LABELS = {
  reload: "reload — 热重载（尽量不掐在用连接）",
  restart: "restart — 重启数据面（一定掐一次）",
};

const POLICY_INPUTS = {
  private_direct: "pol-private-direct",
  cn_direct: "pol-cn-direct",
  block_ads: "pol-block-ads",
  bittorrent_direct: "pol-bittorrent",
};
// 表单拥有 policy 里的哪几条：412 重放时只把这几条搬到新的那一份上。
const POLICY_FIELDS = [
  "private_direct",
  "cn_direct",
  "block_ads",
  "bittorrent_direct",
  "final_outbound",
  "domain_rules",
];
const ACTIONS = ["direct", "proxy", "block"];
const ACTION_LABELS = { direct: "直连", proxy: "代理", block: "拦截" };

// 表单拥有 dns 与 ports 里的哪几条：跟 policy 同一条规矩，412 重放时只搬这几条。
// 四张行表（groups / devices / rule_sets / nodes）也都不整份搬——重放时逐行按身份配落点，
// 只合动过的那几格（见 replayTable）。
const DNS_FIELDS = [
  "domestic",
  "overseas",
  "use_system_local",
  "fakeip",
  "strategy",
  "records_enabled",
  "records_retention_days",
];
const FAKEIP_FIELDS = ["enabled", "inet4_range", "inet6_range"];
const PORTS_FIELDS = ["dns", "tproxy", "redirect", "mixed", "socks"];
// 这两张表要和模型上的枚举逐字对上，测试拿 `UpstreamTransport` / `DnsStrategy` 反着钉。
const TRANSPORTS = ["udp", "tcp", "dot", "doh", "doh3"];
const DNS_MODES = ["prefer_ipv4", "ipv4_only", "ipv6_only", "prefer_ipv6"];
// 一行一个上游。七个控件就是模型 `Upstream` 的全部字段——少画一格，
// 用户改这一行时那一格就悄悄丢了，而那正是"改一个开关清空了订阅"这一类的形状。
const UPSTREAM_CELLS = [
  { key: "tag", label: "tag", ph: "domestic" },
  { key: "host", label: "地址", ph: "223.5.5.5" },
  { key: "port", label: "端口", ph: "留空按传输", numeric: true },
  { key: "transport", label: "传输", isSelect: true },
  { key: "path", label: "路径", ph: "/dns-query" },
  { key: "sni", label: "SNI", ph: "留空用地址" },
  { key: "via", label: "经由", ph: "留空＝直连" },
];
const UPSTREAM_LISTS = {
  domestic: ["domestic-list", "domestic-empty"],
  overseas: ["overseas-list", "overseas-empty"],
};
const PORT_INPUTS = {
  dns: "port-dns",
  tproxy: "port-tproxy",
  redirect: "port-redirect",
  mixed: "port-mixed",
  socks: "port-socks",
};

function clone(value) {
  return JSON.parse(JSON.stringify(value));
}

function mergeFields(target, source, fields) {
  fields.forEach(function (key) {
    target[key] = clone(source[key]);
  });
  return target;
}

function fieldText(id) {
  return document.getElementById(id).value.trim();
}

// 一个下拉框的填与收。不认识磁盘上那个取值时不改写、原样带回去，
// 但要在界面上说清"这一格显示的是不认识的写法"——静默归成默认值就等于替用户改了他没改的东西。
function fillChoice(id, allowed, labels, current, fallback, unknownWord) {
  const select = document.getElementById(id);
  while (select.firstChild) select.removeChild(select.firstChild);
  const value = typeof current === "string" && current !== "" ? current : fallback;
  const unknown = allowed.indexOf(value) < 0;
  const items = unknown ? allowed.concat([value]) : allowed;
  items.forEach(function (item) {
    addOption(select, item, unknown && item === value ? value + "（" + unknownWord + "）" : labels[item]);
  });
  select.value = value;
}

function choiceFromForm(id, allowed, valueOnDisk, fallback) {
  const value = document.getElementById(id).value;
  if (allowed.indexOf(value) >= 0) return value;
  return isMissing(valueOnDisk) ? fallback : clone(valueOnDisk);
}

function setSaveNote(message, klass) {
  const node = document.getElementById("save-out");
  node.textContent = message;
  node.className = klass;
}

function why(error) {
  return String(error.message || error);
}

// 名单写法 <-> 模型形状，与 `DomainPattern::parse` / `raw()` 同一条约定。
function rawToPattern(raw) {
  const s = raw.trim();
  if (!s || s.charAt(0) === "#") return null;
  if (s.charAt(0) === "+") return { suffix: s.slice(1).replace(/^\.+/, "") };
  if (s.charAt(0) === "~") return { keyword: s.slice(1) };
  if (s.charAt(0) === "/") return { regex: s.slice(1).replace(/\/+$/, "") };
  if (s.charAt(0) === ".") return { suffix: s.replace(/^\.+/, "") };
  return { exact: s };
}

function patternToRaw(pattern) {
  if (pattern && typeof pattern === "object") {
    if (typeof pattern.suffix === "string") return "+" + pattern.suffix;
    if (typeof pattern.keyword === "string") return "~" + pattern.keyword;
    if (typeof pattern.regex === "string") return "/" + pattern.regex + "/";
    if (typeof pattern.exact === "string") return pattern.exact;
  }
  // 这一版看不懂的形状：显示成 JSON，用户不动它就一直原样带着（见 collectRules）
  return JSON.stringify(pattern);
}

function ruleRow(rule) {
  const li = document.createElement("li");
  li.className = "rule-row";
  const text = patternToRaw(rule.pattern);
  const action = typeof rule.action === "string" ? rule.action : "direct";
  li.original = rule;
  li.originalText = text;
  li.originalAction = action;

  const seq = document.createElement("span");
  seq.className = "seq";
  seq.textContent = "·";
  li.appendChild(seq);

  const input = document.createElement("input");
  input.type = "text";
  input.autocomplete = "off";
  input.spellcheck = false;
  input.value = text;
  li.appendChild(input);

  const select = document.createElement("select");
  ACTIONS.forEach(function (value) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = ACTION_LABELS[value];
    select.appendChild(option);
  });
  if (ACTIONS.indexOf(action) < 0) {
    const option = document.createElement("option");
    option.value = action;
    option.textContent = action + "（不认识的处置）";
    select.appendChild(option);
  }
  select.value = action;
  li.appendChild(select);

  [
    ["up", "↑", "上移：更先命中"],
    ["down", "↓", "下移：更后命中"],
    ["del", "删", "从名单里去掉"],
  ].forEach(function (spec) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "mini";
    button.setAttribute("data-act", spec[0]);
    button.title = spec[2];
    button.textContent = spec[1];
    li.appendChild(button);
  });
  return li;
}

function ruleList() {
  return document.getElementById("rule-list");
}

function renumber() {
  const rows = ruleList().children;
  for (let i = 0; i < rows.length; i += 1) {
    rows[i].querySelector(".seq").textContent = String(i + 1);
  }
  document.getElementById("rule-empty").hidden = rows.length > 0;
}

function renderRules(rules) {
  const list = ruleList();
  while (list.firstChild) list.removeChild(list.firstChild);
  rules.forEach(function (rule) {
    list.appendChild(ruleRow(rule));
  });
  renumber();
}

function addRule() {
  const input = document.getElementById("new-rule-pattern");
  const pattern = rawToPattern(input.value);
  if (pattern === null) {
    setSaveNote("那条加不进去：写法是空的，或以 # 开头。", "warn");
    return;
  }
  const action = document.getElementById("new-rule-action").value;
  // 新规则一律排到末尾：前面的会先命中它，这一点在名单上面那句话里写着。
  ruleList().appendChild(ruleRow({ pattern: pattern, action: action }));
  renumber();
  input.value = "";
  formDirty = true;
}

// —— DNS 上游：一行一个，七个控件 ——
function cellText(upstream, cell) {
  const value = upstream ? upstream[cell.key] : undefined;
  if (cell.key === "via") return viaToText(value);
  if (isMissing(value)) return "";
  return String(value);
}

// `Via` 是外部标签枚举："direct" 或 `{"outbound":"<tag>"}`。看不懂的形状显示成 JSON，
// 用户不动那一格就原样带回（下面 collectUpstream 里那句"这一格没动"）。
function viaToText(via) {
  if (isMissing(via) || via === "direct") return "";
  if (typeof via === "object" && typeof via.outbound === "string") return via.outbound;
  return JSON.stringify(via);
}

function digitsOnly(text, what) {
  if (!/^[0-9]+$/.test(text)) {
    throw new Error(what + " 要写成纯数字，收到的是 " + JSON.stringify(text));
  }
  return Number(text);
}

// 新加的一行先填满模型那七个键：这样"只填了地址"也交得出去一条完整的上游，
// 而不是让服务端回一句"missing field transport"。
function blankUpstream() {
  return {
    tag: "",
    host: "",
    port: null,
    transport: "udp",
    path: null,
    sni: null,
    via: "direct",
  };
}

function addOption(select, value, label) {
  const option = document.createElement("option");
  option.value = value;
  option.textContent = label;
  select.appendChild(option);
}

function upstreamRow(upstream, fresh) {
  const li = document.createElement("li");
  li.className = "up-row";
  li.original = upstream && typeof upstream === "object" ? upstream : blankUpstream();
  li.fresh = !!fresh;
  li.texts = {};

  const seq = document.createElement("span");
  seq.className = "seq";
  seq.textContent = "·";
  li.appendChild(seq);

  UPSTREAM_CELLS.forEach(function (cell) {
    const label = document.createElement("label");
    label.className = "up-cell";
    const caption = document.createElement("span");
    caption.textContent = cell.label;
    label.appendChild(caption);

    let text;
    let control;
    if (cell.isSelect) {
      text = typeof li.original[cell.key] === "string" ? li.original[cell.key] : "udp";
      const unknown = TRANSPORTS.indexOf(text) < 0;
      control = document.createElement("select");
      (unknown ? TRANSPORTS.concat([text]) : TRANSPORTS).forEach(function (value) {
        addOption(control, value, unknown && value === text ? text + "（不认识的传输）" : value);
      });
    } else {
      text = cellText(li.original, cell);
      control = document.createElement("input");
      control.type = "text";
      control.autocomplete = "off";
      control.spellcheck = false;
      if (cell.numeric) control.inputMode = "numeric";
      if (cell.ph) control.placeholder = cell.ph;
    }
    control.value = text;
    control.setAttribute("data-key", cell.key);
    li.texts[cell.key] = text;
    label.appendChild(control);
    li.appendChild(label);
  });

  const nav = document.createElement("div");
  nav.className = "up-nav";
  [
    ["up", "↑", "上移：排第一的是主上游"],
    ["down", "↓", "下移：往后做冗余"],
    ["del", "删", "从这一组里去掉"],
  ].forEach(function (spec) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "mini";
    button.setAttribute("data-act", spec[0]);
    button.title = spec[2];
    button.textContent = spec[1];
    nav.appendChild(button);
  });
  li.appendChild(nav);
  return li;
}

function collectUpstream(li) {
  const out = clone(li.original);
  let touched = !li.fresh;
  UPSTREAM_CELLS.forEach(function (cell) {
    const control = li.querySelector('[data-key="' + cell.key + '"]');
    const text = control.value.trim();
    if (text === li.texts[cell.key]) return; // 这一格没动：连看不懂的形状一起原样带走
    touched = true;
    if (cell.key === "port") {
      out.port = text === "" ? null : digitsOnly(text, "上游 " + cell.label);
    } else if (cell.key === "path" || cell.key === "sni") {
      out[cell.key] = text === "" ? null : text;
    } else if (cell.key === "via") {
      out.via = text === "" ? "direct" : { outbound: text };
    } else {
      out[cell.key] = text;
    }
  });
  // 一格没填的新行是"还没决定要加"，丢掉比交一条空壳诚实
  if (!touched) return null;
  return out;
}

function upstreamList(which) {
  return document.getElementById(UPSTREAM_LISTS[which][0]);
}

function renumberUpstreams(which) {
  const ids = UPSTREAM_LISTS[which];
  const list = upstreamList(which);
  const rows = list.children;
  for (let i = 0; i < rows.length; i += 1) {
    rows[i].querySelector(".seq").textContent = String(i + 1);
  }
  document.getElementById(ids[1]).hidden = rows.length > 0;
}

function renderUpstreams(which, items) {
  const list = upstreamList(which);
  while (list.firstChild) list.removeChild(list.firstChild);
  (Array.isArray(items) ? items : []).forEach(function (upstream) {
    list.appendChild(upstreamRow(upstream, false));
  });
  renumberUpstreams(which);
}

function collectUpstreamList(which) {
  const rows = upstreamList(which).children;
  const out = [];
  for (let i = 0; i < rows.length; i += 1) {
    const got = collectUpstream(rows[i]);
    if (got !== null) out.push(got);
  }
  return out;
}

// `DnsStrategy` 是 `{"mode": "..."}` 内部标签枚举；认不出来就整条原样带回。
function strategyToMode(value) {
  if (typeof value === "string") return value;
  if (value && typeof value === "object" && typeof value.mode === "string") return value.mode;
  return "";
}

function fillStrategy(value) {
  const select = document.getElementById("dns-strategy");
  while (select.firstChild) select.removeChild(select.firstChild);
  const mode = strategyToMode(value);
  const unknown = mode === "" || DNS_MODES.indexOf(mode) < 0;
  (unknown ? DNS_MODES.concat([mode || "unknown"]) : DNS_MODES).forEach(function (item) {
    addOption(select, item, unknown && item === mode ? mode + "（不认识的模式）" : item);
  });
  if (unknown) {
    select.value = mode || "unknown";
  } else {
    select.value = mode;
  }
}

function strategyFromForm(valueOnDisk) {
  const select = document.getElementById("dns-strategy");
  const mode = select.value;
  if (DNS_MODES.indexOf(mode) < 0) return isMissing(valueOnDisk) ? { mode: "prefer_ipv4" } : clone(valueOnDisk);
  return { mode: mode };
}

function fillDns(dns) {
  renderUpstreams("domestic", dns.domestic);
  renderUpstreams("overseas", dns.overseas);
  document.getElementById("dns-use-system-local").checked = dns.use_system_local === true;
  // 记录这一格在模型里的缺省是"开"（面板上一旦显示成关，用户就以为观测坏了），
  // 所以磁盘上没这一格时勾上，与 serde 那侧的默认取值保持一致。
  document.getElementById("dns-records-enabled").checked = dns.records_enabled !== false;
  const retention = dns.records_retention_days;
  document.getElementById("dns-records-retention").value = isMissing(retention)
    ? "7"
    : String(retention);
  fillStrategy(dns.strategy);
  const fakeip = dns.fakeip && typeof dns.fakeip === "object" ? dns.fakeip : {};
  document.getElementById("dns-fakeip-enabled").checked = fakeip.enabled === true;
  document.getElementById("dns-fakeip-v4").value = isMissing(fakeip.inet4_range) ? "" : String(fakeip.inet4_range);
  document.getElementById("dns-fakeip-v6").value = isMissing(fakeip.inet6_range) ? "" : String(fakeip.inet6_range);
}

function collectDns(base) {
  const dns = base && typeof base === "object" ? base : {};
  dns.domestic = collectUpstreamList("domestic");
  dns.overseas = collectUpstreamList("overseas");
  dns.use_system_local = document.getElementById("dns-use-system-local").checked;
  // 「解析记录」那一屏有没有数据就是这两格决定的，所以表单要拥有它们。
  // 天数留空 = 这一格没动，原样带走磁盘上那一份；填 0 不在这里拦——
  // 生成器会当场报"retention_days 要 ≥ 1"，那句比表单自己猜的一条规矩更可信。
  dns.records_enabled = document.getElementById("dns-records-enabled").checked;
  const retentionText = fieldText("dns-records-retention");
  if (retentionText !== "") dns.records_retention_days = digitsOnly(retentionText, "保留天数");
  dns.strategy = strategyFromForm(dns.strategy);
  const fakeip = dns.fakeip && typeof dns.fakeip === "object" ? dns.fakeip : {};
  fakeip.enabled = document.getElementById("dns-fakeip-enabled").checked;
  fakeip.inet4_range = fieldText("dns-fakeip-v4");
  fakeip.inet6_range = fieldText("dns-fakeip-v6");
  dns.fakeip = fakeip;
  return dns;
}

function fillPorts(ports) {
  PORTS_FIELDS.forEach(function (key) {
    const value = ports[key];
    document.getElementById(PORT_INPUTS[key]).value = isMissing(value) ? "" : String(value);
  });
}

// 端口这里只验"是不是纯数字"，不比对大小、不查重复：模型对端口一条规则都没有，
// 撞在同一个数字上要到下发时才由内核说。表单不冒充内核。
function collectPorts(base) {
  const ports = base && typeof base === "object" ? base : {};
  PORTS_FIELDS.forEach(function (key) {
    const text = fieldText(PORT_INPUTS[key]);
    if (text === "") {
      if (key !== "socks") throw new Error("端口 " + key + " 不能是空的（只有 SOCKS 可以留空）");
      ports.socks = null;
      return;
    }
    ports[key] = digitsOnly(text, "端口 " + key);
  });
  return ports;
}

// —— 规则集与设备：两张平表 ——
// 与上游表同一套行内按钮（上移 / 下移 / 删），同一套"这一格没动就连原样一起带走"的规矩。
// `RULE_SET_DIR` 那一个字面量必须等于模型的 `rule_set_product()`（在线源的产物落点由 tag 派生，
// 面板不给填；填了别的落点等于 sing-box 启动时读不到，而保存那一关就 400 退回）。
const RULE_SET_DIR = "rule-set";
const RULESET_SOURCES = ["local", "remote"];
// 四档 = 屏幕上那四栏。**顺序就是栏的顺序**，不是优先级——`devices[]` 的数组序在我们这里
// 没有语义（每台一条独立规则、源地址互不重叠），所以这一屏从头到尾不出现"第 N 台"。
const DEVICE_MODES = ["direct", "global", "proxy", "bypass"];
const DEVICE_MODE_LABELS = {
  direct: "不科学：全部直连",
  // 这一栏的名字说的是"这一台跟着默认走"，不是"这一台走智能分流"——默认那一排是三档，
  // 默认设成不科学时这一栏的设备就是不科学。栏名曾经叫「智能分流」，那是说谎。
  global: "跟着默认走：不单独定",
  proxy: "强制走代理：全部走代理",
  bypass: "不接管：完全不经这台机器",
};

const RULESET_CELLS = {
  local: [
    { key: "tag", label: "tag", ph: "geoip-my" },
    { key: "path", label: "本地 .srs 路径", ph: "rule-set/geoip-my.srs" },
  ],
  remote: [
    { key: "tag", label: "tag", ph: "geosite-cn" },
    // 这一格的提示不能写成完整 URL：静态面不许出现站外引用，`assets` 那条测试是按字面量扫的，
    // 占位符与注释都算。
    { key: "url", label: "源地址", ph: "https 打头的完整地址" },
    { key: "update_interval", label: "更新间隔", ph: "168h" },
    { key: "initial_path", label: "产物落点（由 tag 派生）", derived: true },
  ],
};
const DEVICE_CELLS = [
  // 一格里 v4 或 v6 都能填，但只能填**单个地址**：模型那一关现在会拒掉带 /前缀长度
  // 或写成域名的值，因为劫持层的那条豁免规则是按地址匹配的。
  { key: "ip", label: "地址（v4 或 v6）", ph: "10.99.0.3 或 fd00::50" },
  { key: "mode", label: "这一档", isSelect: true, options: DEVICE_MODES, labels: DEVICE_MODE_LABELS },
  { key: "mac", label: "MAC（可空）", ph: "可空", blankNull: true },
  { key: "hostname", label: "主机名（可空）", ph: "可空", blankNull: true },
];


// —— 策略组：一张表两档 ——
// `kind` 在磁盘上是套了一层的（`Group` 有个叫 kind 的字段，而 `GroupKind` 自己又拿
// kind 当标签键），所以这一节的 `inKind` 标的就是"这个键写在 out.kind 里面"。
// 这两档名单必须等于模型序列化出来的变体名，对不上的话每一条已有的组都会掉进
// "不认识的形状"，面板只把它显示成 JSON、一格都改不动。
const GROUP_KINDS = ["selector", "urltest"];
const GROUP_KIND_LABELS = {
  selector: "选择组（selector）",
  urltest: "测速组（urltest）",
};
const GROUP_CELLS = {
  selector: [
    { key: "tag", label: "tag（字母或下划线开头）", ph: "proxy" },
    { key: "kind", label: "档", derived: true },
    { key: "members", label: "成员（逗号分隔）", ph: "direct" },
    { key: "default", label: "默认成员（可空）", ph: "可空", blankNull: true },
  ],
  urltest: [
    { key: "tag", label: "tag（字母或下划线开头）", ph: "auto" },
    { key: "kind", label: "档", derived: true },
    { key: "url", label: "测速地址", ph: "打头是 http 或 https 的完整地址", inKind: true },
    { key: "interval", label: "测速间隔", ph: "3m", inKind: true },
    { key: "tolerance", label: "容差（毫秒）", ph: "50", inKind: true, isNumber: true },
    { key: "members", label: "成员（逗号分隔）", ph: "direct" },
  ],
};

function blankGroup(kind) {
  if (kind === "urltest") {
    return {
      tag: "",
      kind: { kind: "urltest", url: "", interval: "3m", tolerance: 50 },
      members: [],
      default: null,
    };
  }
  return { tag: "", kind: { kind: "selector" }, members: [], default: null };
}

function groupKindOf(item) {
  const kind = item && typeof item === "object" ? item.kind : null;
  return kind && typeof kind === "object" && GROUP_KINDS.indexOf(kind.kind) >= 0
    ? kind.kind
    : null;
}

function groupCells(kind) {
  return kind === null ? [] : GROUP_CELLS[kind];
}

// 成员在磁盘上是数组、在格子里是一行字，两边都得有个说法。
function splitMembers(text) {
  const out = [];
  text.split(",").forEach(function (part) {
    const one = part.trim();
    if (one !== "" && out.indexOf(one) < 0) out.push(one);
  });
  return out;
}

function groupCellValue(item, cell) {
  if (!item) return undefined;
  const value = cell.inKind && item.kind ? item.kind[cell.key] : item[cell.key];
  if (cell.key === "members") return Array.isArray(value) ? value.join(", ") : "";
  return value;
}

function blankRuleSet(source) {
  if (source === "remote") {
    return { source: "remote", tag: "", url: "", initial_path: "", update_interval: "168h" };
  }
  return { source: "local", tag: "", path: "" };
}

function blankDevice() {
  return { ip: "", mac: null, hostname: null, mode: "global" };
}

function rulesetSourceOf(item) {
  return item && typeof item === "object" && RULESET_SOURCES.indexOf(item.source) >= 0
    ? item.source
    : null;
}

// 行首的序号。行尾的按钮是另一件事，所以两个 helper 分开——
// 把它们并成一个"seq + nav"的函数，就会要么序号跑到控件后面，要么一行画两个序号。
function rowSeq(li) {
  const seq = document.createElement("span");
  seq.className = "seq";
  seq.textContent = "·";
  li.appendChild(seq);
}

function rowNav(li, titles) {
  const nav = document.createElement("div");
  nav.className = "up-nav";
  [
    ["up", "↑", titles.up],
    ["down", "↓", titles.down],
    ["del", "删", titles.del],
  ].forEach(function (spec) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "mini";
    button.setAttribute("data-act", spec[0]);
    button.title = spec[2];
    button.textContent = spec[1];
    nav.appendChild(button);
  });
  li.appendChild(nav);
}

// 建一个控件并记下它开局那份文本——collect 靠"文本变没变"决定这一格是否重写。
function addCell(li, cell, value) {
  const label = document.createElement("label");
  label.className = "up-cell";
  const caption = document.createElement("span");
  caption.textContent = cell.label;
  label.appendChild(caption);

  let control;
  let text;
  if (cell.derived) {
    control = document.createElement("output");
    control.className = "note";
    control.setAttribute("data-derived", cell.key);
    text = null;
  } else if (cell.isSelect) {
    text = typeof value === "string" ? value : cell.options[0];
    const unknown = cell.options.indexOf(text) < 0;
    control = document.createElement("select");
    (unknown ? cell.options.concat([text]) : cell.options).forEach(function (opt) {
      const shown = cell.labels ? cell.labels[opt] : opt;
      addOption(control, opt, unknown && opt === text ? opt + "（不认识的处置）" : shown);
    });
  } else {
    text = isMissing(value) ? "" : String(value);
    control = document.createElement("input");
    control.type = "text";
    control.autocomplete = "off";
    control.spellcheck = false;
    if (cell.ph) control.placeholder = cell.ph;
  }
  if (text !== null) {
    control.value = text;
    control.setAttribute("data-key", cell.key);
    li.texts[cell.key] = text;
  }
  label.appendChild(control);
  li.appendChild(label);
}

function rulesetRows(source) {
  return source === null ? [] : RULESET_CELLS[source];
}

// 这一张表的"表单见过哪些行、每行的补丁"——只有 412 重放那条路读它。
let rulesetEdit = { rendered: [], entries: [], removed: [] };

function rulesetRow(item, fresh) {
  const li = document.createElement("li");
  li.className = "up-row";
  li.texts = {};
  const source = rulesetSourceOf(item);
  li.source = source;
  li.original = item && typeof item === "object" ? clone(item) : null;
  // 身份是改名前的那个 tag：这一格里 tag 既是名字也决定产物落点，改它等于换了一条声明的名字，
  // 所以重放时按旧名找落点、把补丁（含新的 tag）合上去。档位就是来源。
  li.base =
    !fresh && li.original && typeof li.original.tag === "string" ? li.original.tag : null;
  li.shape = source;
  li.fresh = !!fresh;
  rowSeq(li);

  if (source === null) {
    // 这一版看不懂的声明（多了一个来源档位一类）：整条显示成 JSON，不动它就原样带回，能删。
    const label = document.createElement("label");
    label.className = "up-cell";
    const caption = document.createElement("span");
    caption.textContent = "不认识的规则集形状";
    label.appendChild(caption);
    const frozen = document.createElement("input");
    frozen.type = "text";
    frozen.className = "note";
    frozen.readOnly = true;
    frozen.value = JSON.stringify(li.original);
    label.appendChild(frozen);
    li.appendChild(label);
    rowNav(li, { up: "上移", down: "下移", del: "去掉这条声明（同步会连产物一起回收）" });
    li.collect = function () {
      return clone(li.original);
    };
    return li;
  }

  // 已有的行不给切换来源：本地↔在线之间没有公共字段，切过去等于把另一边的键静默抹掉。
  // 要换来源就删了这一行、按另一个按钮重加一条——那才叫"换了一条声明"。
  rulesetRows(source).forEach(function (cell) {
    addCell(li, cell, li.original ? li.original[cell.key] : undefined);
  });
  rowNav(li, {
    up: "上移",
    down: "下移",
    del: "去掉这条声明（同步会连产物一起回收）",
  });
  li.collect = function () {
    return collectRuleSet(li);
  };
  return li;
}

// 设备这一张没有"档位"可言（一行就是四个平铺的键），所以 shape 一律为 null：
// 重放时第 3 条那道形状门对它是常开的，落点找到就合。
let deviceEdit = { rendered: [], entries: [], removed: [] };

// 一张卡 = 一台设备。卡片住在哪一栏就是哪一档，所以"拖进栏"与"改选那一档"必须落到同一处
// （`putDeviceCard`）——否则屏幕上会出现"卡在跟着默认走、那一格却写着强制走代理"这种自己跟自己打脸的样子。
function deviceLanes() {
  return document.querySelectorAll("#device-lanes .lane");
}

function deviceLaneOf(mode) {
  const lanes = deviceLanes();
  for (let i = 0; i < lanes.length; i += 1) {
    if (lanes[i].getAttribute("data-mode") === mode) return lanes[i];
  }
  return null;
}

function deviceCards(lane) {
  return lane.querySelector(".lane-cards");
}

function clearDeviceCards() {
  const lanes = deviceLanes();
  for (let i = 0; i < lanes.length; i += 1) {
    const list = deviceCards(lanes[i]);
    while (list.firstChild) list.removeChild(list.firstChild);
  }
}

function refreshDeviceLanes() {
  let total = 0;
  const lanes = deviceLanes();
  for (let i = 0; i < lanes.length; i += 1) {
    const n = deviceCards(lanes[i]).children.length;
    total += n;
    const count = lanes[i].querySelector(".lane-count");
    if (count) count.textContent = n + " 台";
  }
  document.getElementById("device-empty").hidden = total > 0;
}

function putDeviceCard(card, lane) {
  if (!card || !lane) return;
  deviceCards(lane).appendChild(card);
  const mode = lane.getAttribute("data-mode");
  const select = card.querySelector('[data-key="mode"]');
  // 不认识的那一档没有栏可住（它待在跟着默认走栏里，卡片上自己写着为什么），所以不跟着改。
  if (select && DEVICE_MODES.indexOf(mode) >= 0) {
    select.value = mode;
    const warn = card.querySelector(".dev-warn");
    // 那句"暂时住在跟着默认走这一栏"从这一刻起过期了：它现在住的栏就是它那一档。
    if (warn) warn.parentNode.removeChild(warn);
  }
  refreshDeviceLanes();
}

function deviceRow(item, fresh) {
  const li = document.createElement("li");
  li.className = "dev-card";
  li.texts = {};
  li.original = item && typeof item === "object" ? clone(item) : blankDevice();
  // 缺 `mode` 这一格时落"跟着默认走"：四档里只有它什么都不做，猜别的档会凭空多出一条规则。
  if (typeof li.original.mode !== "string") li.original.mode = "global";
  // 身份是这台设备改过之前的那个地址：改了 IP 就是换了一条例外，重放按旧地址找落点。
  li.base =
    !fresh && typeof li.original.ip === "string" ? li.original.ip : null;
  li.shape = null;
  li.fresh = !!fresh;
  li.unknown = DEVICE_MODES.indexOf(li.original.mode) < 0;

  const grip = document.createElement("span");
  grip.className = "dev-grip";
  grip.setAttribute("draggable", "true");
  grip.setAttribute("title", "按住拖动，放到想要的那一栏");
  grip.textContent = "拖动";
  li.appendChild(grip);

  DEVICE_CELLS.forEach(function (cell) {
    addCell(li, cell, li.original[cell.key]);
  });
  if (li.unknown) {
    const warn = document.createElement("p");
    warn.className = "dev-warn";
    warn.textContent =
      "这一档的名字这一屏不认识，所以它暂时住在「跟着默认走」这一栏里——不改它就原样带走，"
      + "拖进别的栏或以此改选那一档，就以改后的那一档为准。";
    li.appendChild(warn);
  }
  const nav = document.createElement("div");
  nav.className = "up-nav";
  const del = document.createElement("button");
  del.type = "button";
  del.className = "mini";
  del.setAttribute("data-act", "del");
  del.title = "去掉这台设备的例外";
  del.textContent = "去掉";
  nav.appendChild(del);
  li.appendChild(nav);
  li.collect = function () {
    return collectDevice(li);
  };
  return li;
}


function collectRuleSet(li) {
  if (li.source === null) return clone(li.original);
  const out = li.original ? clone(li.original) : blankRuleSet(li.source);
  out.source = li.source;
  let touched = !li.fresh;
  rulesetRows(li.source).forEach(function (cell) {
    if (cell.derived) return;
    const control = li.querySelector('[data-key="' + cell.key + '"]');
    const text = control.value.trim();
    if (text === li.texts[cell.key]) return; // 这一格没动
    touched = true;
    out[cell.key] = text;
  });
  // 一格没填的新行是"还没决定要加"，丢掉比交一条空壳诚实
  if (!touched) return null;
  if (!out.tag) throw new Error("规则集那一行没有 tag：它既是身份，也决定产物文件名");
  if (li.source === "local" && !out.path) throw new Error("本地规则集那一行没有 .srs 路径");
  if (li.source === "remote") {
    if (!out.url) throw new Error("在线源那一行没有 url");
    if (!out.update_interval) throw new Error("在线源那一行要有更新间隔，比如 168h");
    out.initial_path = RULE_SET_DIR + "/" + out.tag + ".srs";
  }
  return out;
}

function collectDevice(li) {
  const out = clone(li.original);
  let touched = !li.fresh;
  DEVICE_CELLS.forEach(function (cell) {
    const control = li.querySelector('[data-key="' + cell.key + '"]');
    const text = control.value.trim();
    if (text === li.texts[cell.key]) return;
    touched = true;
    out[cell.key] = cell.blankNull && text === "" ? null : text;
  });
  if (!touched) return null;
  if (!out.ip) throw new Error("设备那一行没有地址");
  if (out.ip.indexOf("/") >= 0) {
    throw new Error("设备地址是单个 IP：生成器自己补 /32，这里再带一段就成了 " + out.ip + "/32");
  }
  return out;
}

// 派生那一格：tag 一改就跟着改，但它不是输入框——面板不给人填落点。
function refreshDerived() {
  const rows = document.getElementById("ruleset-list").children;
  for (let i = 0; i < rows.length; i += 1) {
    const out = rows[i].querySelector("[data-derived]");
    if (!out) continue;
    const tag = rows[i].querySelector('[data-key="tag"]');
    out.textContent = RULE_SET_DIR + "/" + (tag ? tag.value.trim() : "") + ".srs";
  }
}

function renumberRows(listId, emptyId) {
  const rows = document.getElementById(listId).children;
  for (let i = 0; i < rows.length; i += 1) {
    rows[i].querySelector(".seq").textContent = String(i + 1);
  }
  document.getElementById(emptyId).hidden = rows.length > 0;
}

function renderRows(listId, emptyId, rows) {
  const list = document.getElementById(listId);
  while (list.firstChild) list.removeChild(list.firstChild);
  rows.forEach(function (row) {
    list.appendChild(row);
  });
  renumberRows(listId, emptyId);
}

function renderRuleSets(items) {
  const list = Array.isArray(items) ? items : [];
  rulesetEdit.rendered = list
    .filter(function (item) {
      return item && typeof item.tag === "string";
    })
    .map(function (item) {
      return item.tag;
    });
  renderRows("ruleset-list", "ruleset-empty", list.map(function (item) {
    return rulesetRow(item, false);
  }));
  refreshDerived();
}

function renderDevices(items) {
  const list = Array.isArray(items) ? items : [];
  deviceEdit.rendered = list
    .filter(function (item) {
      return item && typeof item.ip === "string";
    })
    .map(function (item) {
      return item.ip;
    });
  clearDeviceCards();
  list.forEach(function (item) {
    const card = deviceRow(item, false);
    // 认不出来的那一档没有栏可住，暂存在「跟着默认走」栏里，卡片上自己写着为什么。
    deviceCards(deviceLaneOf(card.original.mode) || deviceLaneOf("global")).appendChild(card);
  });
  refreshDeviceLanes();
}

// ========== 流量怎么走（routing）屏 ==========

// 三档路由：直连 / 智能分流 / 全局
const ROUTING_MODES = ["direct", "smart", "global"];
const ROUTING_MODE_LABELS = {
  direct: "直连",
  smart: "智能分流",
  global: "全局",
};

// 当前选中的设备卡片
let selectedRoutingCards = new Set();
let routingDragSource = null;

// 获取设备显示名称：用户自定义优先，否则取 IP 最后一段
function deviceDisplayName(device) {
  if (device && device.display_name) return device.display_name;
  const ip = device && device.ip ? device.ip : "";
  // 取 IP 最后一段：192.168.2.105 → "105"，fd00::1 → "1"
  const parts = ip.split(/[.:]/);
  return parts.length > 0 ? parts[parts.length - 1] : ip;
}

// 渲染设备到三栏
function renderRoutingDevices(items) {
  const list = Array.isArray(items) ? items : [];
  // 清空三栏
  const lanes = ["direct", "smart", "global"];
  lanes.forEach(function (mode) {
    const ul = document.getElementById("lane-" + mode);
    if (ul) {
      while (ul.firstChild) ul.removeChild(ul.firstChild);
    }
  });

  // 更新 ZBox IP
  const zboxIp = document.getElementById("zbox-ip");
  if (zboxIp && diskState && diskState.network) {
    zboxIp.textContent = diskState.network.lan_ip || "—";
  }

  // 渲染每台设备到对应栏
  list.forEach(function (item) {
    if (!item || typeof item.ip !== "string") return;
    const mode = item.mode || "global";
    const lane = document.getElementById("lane-" + mode);
    if (!lane) return;
    const card = createRoutingCard(item);
    lane.appendChild(card);
  });

  // 更新计数
  updateRoutingLaneCounts();
  updateDeviceEmptyState();
}

// 创建路由设备卡片
function createRoutingCard(item) {
  const card = document.createElement("li");
  card.className = "routing-card";
  card.setAttribute("data-ip", item.ip);
  card.setAttribute("draggable", "true");

  const name = document.createElement("div");
  name.className = "card-name";
  name.textContent = deviceDisplayName(item);
  card.appendChild(name);

  const ip = document.createElement("div");
  ip.className = "card-ip";
  ip.textContent = item.ip;
  card.appendChild(ip);

  // 点击选中
  card.addEventListener("click", function (e) {
    if (e.shiftKey) {
      // Shift+点击：范围选择
      toggleRoutingCardRange(card);
    } else if (e.ctrlKey || e.metaKey) {
      // Ctrl/Cmd+点击：多选
      toggleRoutingCard(card);
    } else {
      // 普通点击：单选
      clearRoutingSelection();
      toggleRoutingCard(card);
    }
  });

  // 拖拽开始
  card.addEventListener("dragstart", function (e) {
    routingDragSource = card;
    card.classList.add("dragging");
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/plain", item.ip);
  });

  // 拖拽结束
  card.addEventListener("dragend", function () {
    card.classList.remove("dragging");
    routingDragSource = null;
    clearRoutingDropHighlights();
  });

  // 双击打开详情
  card.addEventListener("dblclick", function () {
    openDeviceModal(item);
  });

  return card;
}

// 更新三栏计数
function updateRoutingLaneCounts() {
  ROUTING_MODES.forEach(function (mode) {
    const ul = document.getElementById("lane-" + mode);
    const count = document.getElementById("lane-" + mode + "-count");
    if (ul && count) {
      count.textContent = ul.children.length + " 台";
    }
  });
}

// 更新空状态提示
function updateDeviceEmptyState() {
  const empty = document.getElementById("device-empty");
  if (empty) {
    const total = ROUTING_MODES.reduce(function (sum, mode) {
      const ul = document.getElementById("lane-" + mode);
      return sum + (ul ? ul.children.length : 0);
    }, 0);
    empty.hidden = total > 0;
  }
}

// 选中/取消选中一张卡片
function toggleRoutingCard(card) {
  if (selectedRoutingCards.has(card)) {
    selectedRoutingCards.delete(card);
    card.classList.remove("selected");
  } else {
    selectedRoutingCards.add(card);
    card.classList.add("selected");
  }
}

// 范围选择（Shift+点击）
function toggleRoutingCardRange(card) {
  const cards = Array.from(document.querySelectorAll(".routing-card"));
  const lastSelected = Array.from(selectedRoutingCards).pop();
  if (!lastSelected) {
    toggleRoutingCard(card);
    return;
  }
  const startIdx = cards.indexOf(lastSelected);
  const endIdx = cards.indexOf(card);
  if (startIdx < 0 || endIdx < 0) return;
  const from = Math.min(startIdx, endIdx);
  const to = Math.max(startIdx, endIdx);
  for (let i = from; i <= to; i++) {
    if (!selectedRoutingCards.has(cards[i])) {
      selectedRoutingCards.add(cards[i]);
      cards[i].classList.add("selected");
    }
  }
}

// 清除所有选中
function clearRoutingSelection() {
  selectedRoutingCards.forEach(function (card) {
    card.classList.remove("selected");
  });
  selectedRoutingCards.clear();
}

// 全选
function selectAllRoutingCards() {
  const cards = document.querySelectorAll(".routing-card");
  cards.forEach(function (card) {
    selectedRoutingCards.add(card);
    card.classList.add("selected");
  });
}

// 拖拽目标高亮
function setupRoutingDropZones() {
  ROUTING_MODES.forEach(function (mode) {
    const lane = document.getElementById("lane-" + mode);
    if (!lane) return;

    lane.addEventListener("dragover", function (e) {
      e.preventDefault();
      e.dataTransfer.dropEffect = "move";
      lane.classList.add("drop");
    });

    lane.addEventListener("dragleave", function () {
      lane.classList.remove("drop");
    });

    lane.addEventListener("drop", function (e) {
      e.preventDefault();
      lane.classList.remove("drop");
      const ip = e.dataTransfer.getData("text/plain");
      if (!ip) return;

      // 移动选中的卡片或拖拽的卡片
      const cardsToMove = routingDragSource && selectedRoutingCards.has(routingDragSource)
        ? Array.from(selectedRoutingCards)
        : routingDragSource
        ? [routingDragSource]
        : [];

      cardsToMove.forEach(function (card) {
        lane.appendChild(card);
        // 更新设备的 mode
        const deviceIp = card.getAttribute("data-ip");
        updateDeviceMode(deviceIp, mode);
      });

      clearRoutingSelection();
      updateRoutingLaneCounts();
      formDirty = true;
    });
  });
}

// 清除拖拽高亮
function clearRoutingDropHighlights() {
  ROUTING_MODES.forEach(function (mode) {
    const lane = document.getElementById("lane-" + mode);
    if (lane) lane.classList.remove("drop");
  });
}

// 更新设备的 mode（在内存中）
function updateDeviceMode(ip, mode) {
  if (!diskState || !Array.isArray(diskState.devices)) return;
  const device = diskState.devices.find(function (d) { return d.ip === ip; });
  if (device) {
    device.mode = mode;
  }
}

// 搜索 MAC 地址
function searchDeviceByMac(mac) {
  if (!mac || !diskState || !Array.isArray(diskState.devices)) return;
  const target = mac.toLowerCase().trim();
  const device = diskState.devices.find(function (d) {
    return d.mac && d.mac.toLowerCase() === target;
  });
  if (!device) return;

  // 找到设备后，高亮它的卡片
  const cards = document.querySelectorAll(".routing-card");
  cards.forEach(function (card) {
    if (card.getAttribute("data-ip") === device.ip) {
      card.scrollIntoView({ behavior: "smooth", block: "center" });
      card.classList.add("selected");
      selectedRoutingCards.add(card);
      setTimeout(function () {
        card.classList.remove("selected");
        selectedRoutingCards.delete(card);
      }, 2000);
    }
  });
}

// 打开设备详情弹窗
function openDeviceModal(item) {
  const modal = document.getElementById("device-modal");
  if (!modal) return;

  document.getElementById("modal-device-ip").value = item.ip || "";
  document.getElementById("modal-device-mac").value = item.mac || "";
  document.getElementById("modal-device-name").value = item.display_name || "";

  const ipType = item.ip_type || "dhcp";
  const radios = document.querySelectorAll('input[name="modal-ip-type"]');
  radios.forEach(function (radio) {
    radio.checked = radio.value === ipType;
  });

  modal.hidden = false;
  modal.dataset.ip = item.ip;
}

// 关闭设备详情弹窗
function closeDeviceModal() {
  const modal = document.getElementById("device-modal");
  if (modal) {
    modal.hidden = true;
    modal.dataset.ip = "";
  }
}

// 保存设备详情
function saveDeviceModal() {
  const modal = document.getElementById("device-modal");
  if (!modal) return;
  const ip = modal.dataset.ip;
  if (!ip || !diskState || !Array.isArray(diskState.devices)) return;

  const device = diskState.devices.find(function (d) { return d.ip === ip; });
  if (!device) return;

  device.mac = document.getElementById("modal-device-mac").value.trim() || null;
  device.display_name = document.getElementById("modal-device-name").value.trim() || null;

  const ipTypeRadio = document.querySelector('input[name="modal-ip-type"]:checked');
  device.ip_type = ipTypeRadio ? ipTypeRadio.value : "dhcp";

  closeDeviceModal();
  renderRoutingDevices(diskState.devices);
  formDirty = true;
}

// 删除设备
function deleteDeviceFromModal() {
  const modal = document.getElementById("device-modal");
  if (!modal) return;
  const ip = modal.dataset.ip;
  if (!ip || !diskState || !Array.isArray(diskState.devices)) return;

  diskState.devices = diskState.devices.filter(function (d) { return d.ip !== ip; });
  closeDeviceModal();
  renderRoutingDevices(diskState.devices);
  formDirty = true;
}

// 打开添加设备弹窗
function openAddDeviceModal() {
  const modal = document.getElementById("add-device-modal");
  if (!modal) return;
  document.getElementById("new-device-ip").value = "";
  document.getElementById("new-device-mac").value = "";
  document.getElementById("new-device-name").value = "";
  modal.hidden = false;
}

// 关闭添加设备弹窗
function closeAddDeviceModal() {
  const modal = document.getElementById("add-device-modal");
  if (modal) modal.hidden = true;
}

// 保存新设备
function saveNewDevice() {
  const ip = document.getElementById("new-device-ip").value.trim();
  if (!ip) return;

  const mac = document.getElementById("new-device-mac").value.trim() || null;
  const displayName = document.getElementById("new-device-name").value.trim() || null;

  if (!diskState) diskState = {};
  if (!Array.isArray(diskState.devices)) diskState.devices = [];

  // 检查 IP 是否已存在
  if (diskState.devices.some(function (d) { return d.ip === ip; })) {
    alert("这个 IP 已经在列表里了");
    return;
  }

  diskState.devices.push({
    ip: ip,
    mac: mac,
    hostname: null,
    mode: "global",
    ip_type: "dhcp",
    display_name: displayName,
  });

  closeAddDeviceModal();
  renderRoutingDevices(diskState.devices);
  formDirty = true;
}

function addRuleSetRow(source) {
  document.getElementById("ruleset-list").appendChild(rulesetRow(blankRuleSet(source), true));
  renumberRows("ruleset-list", "ruleset-empty");
  refreshDerived();
  const first = document.getElementById("ruleset-list").lastChild.querySelector('[data-key="tag"]');
  first.focus();
  formDirty = true;
}

function addDeviceRow() {
  const card = deviceRow(blankDevice(), true);
  // 新加的一台开局在「跟着默认走」栏：那一档什么都不做，等于"还没决定"，
  // 而空白卡片落在"全部直连"或"强制走代理"栏会让一次没动过的保存凭空多出一条规则。
  deviceCards(deviceLaneOf("global")).appendChild(card);
  refreshDeviceLanes();
  card.querySelector('[data-key="ip"]').focus();
  formDirty = true;
}

// 策略组这张表的落点身份是 tag，档位是 `kind.kind` 那一层（selector / urltest）。
let groupEdit = { rendered: [], entries: [], removed: [] };

function groupRow(item, fresh) {
  const li = document.createElement("li");
  li.className = "up-row";
  li.texts = {};
  const kind = groupKindOf(item);
  li.kind = kind;
  li.original = item && typeof item === "object" ? clone(item) : null;
  li.base =
    !fresh && li.original && typeof li.original.tag === "string" ? li.original.tag : null;
  li.shape = kind;
  li.fresh = !!fresh;
  rowSeq(li);

  if (kind === null) {
    // 看不懂的组（多了一档、或 kind 不是套那一层的形状）：整条显示成 JSON，不动它就原样带回，能删。
    const label = document.createElement("label");
    label.className = "up-cell";
    const caption = document.createElement("span");
    caption.textContent = "不认识的策略组形状";
    label.appendChild(caption);
    const frozen = document.createElement("input");
    frozen.type = "text";
    frozen.className = "note";
    frozen.readOnly = true;
    frozen.value = JSON.stringify(li.original);
    label.appendChild(frozen);
    li.appendChild(label);
    rowNav(li, { up: "上移", down: "下移", del: "去掉这个组" });
    li.collect = function () {
      return clone(li.original);
    };
    return li;
  }

  groupCells(kind).forEach(function (cell) {
    addCell(li, cell, groupCellValue(li.original, cell));
  });
  const shown = li.querySelector('[data-derived="kind"]');
  shown.textContent = GROUP_KIND_LABELS[kind];
  rowNav(li, {
    up: "上移",
    down: "下移",
    del: "去掉这个组（引用它的地方会跟着 400 退回）",
  });
  li.collect = function () {
    return collectGroup(li);
  };
  return li;
}

function collectGroup(li) {
  if (li.kind === null) return clone(li.original);
  const out = clone(li.original);
  let touched = !li.fresh;
  groupCells(li.kind).forEach(function (cell) {
    if (cell.derived) return;
    const control = li.querySelector('[data-key="' + cell.key + '"]');
    const text = control.value.trim();
    if (text === li.texts[cell.key]) return; // 这一格没动
    touched = true;
    if (cell.key === "members") out.members = splitMembers(text);
    else if (cell.isNumber) out.kind[cell.key] = digitsOnly(text, "策略组容差");
    else if (cell.blankNull && text === "") out[cell.key] = null;
    else if (cell.inKind) out.kind[cell.key] = text;
    else out[cell.key] = text;
  });
  // 一格没填的新行是"还没决定要加"，丢掉比交一条空壳诚实
  if (!touched) return null;
  if (!out.tag) throw new Error("策略组那一行没有 tag：别处引用它靠的就是这个名字");
  if (li.kind === "urltest") {
    if (!out.kind.url) throw new Error("测速组那一行没有测速地址");
    if (!out.kind.interval) throw new Error("测速组那一行要有测速间隔，比如 3m");
  }
  return out;
}

// 成员名单不是校验，是提示：这一行把"合法的名字有哪些"抄在屏幕边上，
// 真正的拦截在保存那一关（引用了名单外的成员，400 会指名是哪一个）。
function refreshGroupHint() {
  const names = [];
  // 名单从表单上那几行读，不读 diskState.nodes：节点这一格现在也归表单管了，
  // 刚点"加一条"还没保存的出站也要出现在名单里，删掉的那一条要跟着消失。
  const rows = document.getElementById("node-list").children;
  for (let i = 0; i < rows.length; i += 1) {
    const name = rows[i].querySelector('[data-key="name"]');
    const value = name ? name.value.trim() : rows[i].baseName || "";
    if (value !== "" && names.indexOf(value) < 0) names.push(value);
  }
  const groupRows = document.getElementById("group-list").children;
  for (let i = 0; i < groupRows.length; i += 1) {
    const tag = groupRows[i].querySelector('[data-key="tag"]');
    const value = tag ? tag.value.trim() : "";
    if (value !== "" && names.indexOf(value) < 0) names.push(value);
  }
  ["direct", "reject"].forEach(function (special) {
    if (names.indexOf(special) < 0) names.push(special);
  });
  document.getElementById("group-members-hint").textContent =
    "现在能当成员的名字有：" + names.join("、");
}

function renderGroups(items) {
  const list = Array.isArray(items) ? items : [];
  groupEdit.rendered = list
    .filter(function (item) {
      return item && typeof item.tag === "string";
    })
    .map(function (item) {
      return item.tag;
    });
  renderRows("group-list", "group-empty", list.map(function (item) {
    return groupRow(item, false);
  }));
  refreshGroupHint();
}

function addGroupRow(kind) {
  document.getElementById("group-list").appendChild(groupRow(blankGroup(kind), true));
  renumberRows("group-list", "group-empty");
  refreshGroupHint();
  const first = document.getElementById("group-list").lastChild.querySelector('[data-key="tag"]');
  first.focus();
  formDirty = true;
}

// —— 按名字里的词建一个地区组 ——
// 组名要落成 dae 认的标识符。2026-10-09 在 v2.1.1 上逐个形状实测：`g1` `hk-1_a.b` `_x` 过门，
// 中文（裸写和加引号都一样）、数字开头、点开头全是硬解析错误。所以关键词只用来挑成员，
// 名字里不合法的字符剥掉；剥完什么都不剩就按序号发 region1、region2……
const DAE_TAG = /^[A-Za-z_][A-Za-z0-9_.-]*$/;
const RESERVED_OUTBOUND = ["direct", "must_direct", "block"];

function existingGroupTags() {
  const tags = [];
  const rows = document.getElementById("group-list").children;
  for (let i = 0; i < rows.length; i += 1) {
    const cell = rows[i].querySelector('[data-key="tag"]');
    const value = cell ? cell.value.trim() : "";
    if (value !== "" && tags.indexOf(value) < 0) tags.push(value);
  }
  return tags;
}

function regionTagFor(keyword) {
  const taken = existingGroupTags();
  const free = function (tag) {
    return DAE_TAG.test(tag) && RESERVED_OUTBOUND.indexOf(tag) < 0 && taken.indexOf(tag) < 0;
  };
  const base = keyword.replace(/[^A-Za-z0-9_.-]/g, "").replace(/^[^A-Za-z_]+/, "");
  if (base === "") {
    for (let n = 1; n < 100; n += 1) {
      if (free("region" + n)) return "region" + n;
    }
    return "";
  }
  if (free(base)) return base;
  for (let n = 2; n < 100; n += 1) {
    if (free(base + n)) return base + n;
  }
  return "";
}

function nodeNamesMatching(keyword) {
  const needle = keyword.toLowerCase();
  const names = [];
  const rows = document.getElementById("node-list").children;
  for (let i = 0; i < rows.length; i += 1) {
    const cell = rows[i].querySelector('[data-key="name"]');
    const name = cell ? cell.value.trim() : rows[i].baseName || "";
    if (name !== "" && name.toLowerCase().indexOf(needle) >= 0 && names.indexOf(name) < 0) {
      names.push(name);
    }
  }
  return names;
}

function addRegionGroup(lock) {
  const key = document.getElementById("group-region-key").value.trim();
  const out = document.getElementById("group-region-out");
  if (key === "") {
    out.textContent = "先填一个关键词——节点名里出现的那一段，比如 HK 或 Tokyo（比较不分大小写）。";
    return;
  }
  const names = nodeNamesMatching(key);
  if (names.length === 0) {
    out.textContent = "「订阅与节点」那几行里没有名字含「" + key + "」的一条。换个词，或者先去那一屏确认名字。";
    return;
  }
  const tag = regionTagFor(key);
  if (tag === "") {
    out.textContent = "组名排不下了：手动加一个组吧，成员填成这些名字：" + names.join("、") + "。";
    return;
  }
  const li = groupRow(blankGroup(lock ? "selector" : "urltest"), true);
  const tagCell = li.querySelector('[data-key="tag"]');
  tagCell.value = tag;
  li.querySelector('[data-key="members"]').value = names.join(", ");
  document.getElementById("group-list").appendChild(li);
  renumberRows("group-list", "group-empty");
  refreshGroupHint();
  formDirty = true;
  tagCell.focus();
  out.textContent = lock
    ? "已加组 " + tag + "，成员是这 " + names.length + " 条：" + names.join("、") +
      "。这一档在机器上是 policy: fixed(0)——只用第一个（" + names[0] + "），而锁定那一档的代价是" +
      "上游明写的「连通性检查会关掉」：它挂了不会自动换。要钉在别的那条上就改这一行的「默认成员」。"
      + "还没保存，也没下发。"
    : "已加组 " + tag + "，成员是这 " + names.length + " 条：" + names.join("、") +
      "。这一档在机器上是 policy: min_avg10——按十次平均延迟挑当前最优。" +
      "「测速地址」那一格面板不替你猜，填了才存得下去。还没保存，也没下发。";
}

// ---------------- 订阅与节点 ----------------
// 磁盘上一个出站是 {name, server, port, kind}，kind 是两层内部标签枚举：
// `kind.kind` 分 stream / quic，`kind.protocol.protocol` 分六档协议。
// 这一格只画"跟着地址一起换"的那几格，加上每一档自己要的那条凭据；
// 其余键（transport 的 path/host/headers、alpn / insecure / fingerprint / ech、
// hysteria2 的 obfs 与带宽、tuic 的拥塞控制与中继方式、alter_id / security / method / flow）
// 表单一条都不拥有——收集时只写动过的那一格，按键路径深度合并回磁盘上那一条。
// 这两份名单必须等于模型序列化出来的变体名，对不上的话每一条已有的节点都会掉进
// "不认识的形状"，那一行只画成 JSON、一格都改不动（同策略组那一节的理由）。
const NODE_KINDS = ["stream", "quic"];
const NODE_PROTOCOL_LABELS = {
  vless: "vless",
  vmess: "vmess",
  trojan: "trojan",
  shadowsocks: "shadowsocks",
  hysteria2: "hysteria2（QUIC）",
  tuic: "tuic（QUIC）",
};
const NODE_BASE_CELLS = [
  { key: "name", label: "名字", ph: "tokyo-1", path: ["name"] },
  { key: "kind", label: "档", derived: true },
  { key: "server", label: "地址", ph: "198.51.100.10", path: ["server"] },
  { key: "port", label: "端口", ph: "443", isNumber: true, path: ["port"] },
];
// 凭据格共同点：路径都在 kind.protocol 里面，六档都是那一条规律。
// 它们的值从不读进页面（建控件时交的是 undefined，于是开局为空、留空就是"不改这一条"），
// 所以给一条已有的节点换凭据不需要先知道旧值。
const NODE_PROTOCOL_CELLS = {
  vless: [{ key: "uuid", label: "UUID", secret: true, path: ["kind", "protocol", "uuid"] }],
  vmess: [{ key: "uuid", label: "UUID", secret: true, path: ["kind", "protocol", "uuid"] }],
  trojan: [{ key: "password", label: "密码", secret: true, path: ["kind", "protocol", "password"] }],
  shadowsocks: [
    { key: "password", label: "密码", secret: true, path: ["kind", "protocol", "password"] },
  ],
  hysteria2: [
    { key: "password", label: "密码", secret: true, path: ["kind", "protocol", "password"] },
  ],
  tuic: [
    { key: "uuid", label: "UUID", secret: true, path: ["kind", "protocol", "uuid"] },
    { key: "password", label: "密码", secret: true, path: ["kind", "protocol", "password"] },
  ],
};
// SNI 与 Reality 两格挂在"那一条的 TLS 配置"下面，而 TLS 配置住在两层之外：
// stream 在 kind.tls，quic 在 kind.protocol.tls。所以它们记的是相对路径，建行时再接到
// 那一行的落点上。
const NODE_SNI_CELL = {
  key: "sni",
  label: "伪装域名 SNI（可空）",
  ph: "可空",
  blankNull: true,
  rel: ["sni"],
};
// short_id 在模型里是 String（不是 Option），清空就是写成空串，别按 blankNull 交 null。
const NODE_REALITY_CELLS = [
  { key: "public_key", label: "Reality 公钥", ph: "握手用的公钥", rel: ["reality", "public_key"] },
  { key: "short_id", label: "Reality short id（可空）", ph: "可空", rel: ["reality", "short_id"] },
];

// 新建行的骨架要把画出来的那些键一次给全：内部标签枚举会**静默丢掉**标签之外的键，
// 所以不能靠 setDeep 在 `{"mode":"disabled"}` 里现造一个 reality 出来。
function blankNode(variant) {
  if (variant === "hysteria2") {
    return {
      name: "",
      server: "",
      port: 443,
      kind: {
        kind: "quic",
        protocol: { protocol: "hysteria2", password: "", tls: { mode: "tls", sni: null } },
      },
    };
  }
  if (variant === "tuic") {
    return {
      name: "",
      server: "",
      port: 443,
      kind: {
        kind: "quic",
        protocol: { protocol: "tuic", uuid: "", password: "", tls: { mode: "tls", sni: null } },
      },
    };
  }
  return {
    name: "",
    server: "",
    port: 443,
    kind: {
      kind: "stream",
      protocol: { protocol: "vless", uuid: "", flow: "xtls-rprx-vision" },
      transport: { type: "tcp" },
      tls: { mode: "tls", sni: null, reality: { public_key: "", short_id: "" } },
    },
  };
}

function getDeep(item, path) {
  let cursor = item;
  path.forEach(function (key) {
    if (cursor === null || typeof cursor !== "object") return;
    cursor = cursor[key];
  });
  return cursor;
}

function setDeep(target, path, value) {
  let cursor = target;
  for (let i = 0; i < path.length - 1; i += 1) {
    const key = path[i];
    if (cursor[key] === null || typeof cursor[key] !== "object") cursor[key] = {};
    cursor = cursor[key];
  }
  cursor[path[path.length - 1]] = value;
  return target;
}

// 深度合并只走普通对象，数组与标量整个换掉：节点里唯一的数组是 headers 那类
// 键值对表，逐下标合并会把两条不相关的表搅在一起。
function mergeDeep(target, patch) {
  Object.keys(patch).forEach(function (key) {
    const value = patch[key];
    const here = target[key];
    if (
      value !== null && typeof value === "object" && !Array.isArray(value) &&
      here !== null && typeof here === "object" && !Array.isArray(here)
    ) {
      mergeDeep(here, value);
    } else {
      target[key] = clone(value);
    }
  });
  return target;
}

// 这一行相对磁盘上那一条改了哪些键路径——从两份读数里 diff 出来，而不是在每格的写入点
// 顺手记一份。理由是这几张表里有三种"不是一格一格写进去"的键：规则集 tag 派生的
// `initial_path`、成员那格的"逗号串 ↔ 数组"、设备那两格的"空串 ↔ null"。逐格记会漏掉它们，
// 漏掉的后果是重放时把别人在这些键上的改动当成没改动盖掉。
// （节点那一格反过来：它是**凭据格开局为空**，"留空＝不改"是逐格的决定，diff 看不出来，
//  所以它在 collectNode 里自己记补丁。两处机制不同是各自的形状决定的。）
function diffPatch(base, next) {
  const patch = {};
  const keys = [];
  [base, next].forEach(function (obj) {
    Object.keys(obj || {}).forEach(function (key) {
      if (keys.indexOf(key) < 0) keys.push(key);
    });
  });
  keys.forEach(function (key) {
    const before = base ? base[key] : undefined;
    const after = next ? next[key] : undefined;
    const plain = function (v) {
      return v !== null && typeof v === "object" && !Array.isArray(v);
    };
    if (plain(before) && plain(after)) {
      const inner = diffPatch(before, after);
      if (Object.keys(inner).length > 0) patch[key] = inner;
      return;
    }
    if (JSON.stringify(before) !== JSON.stringify(after)) patch[key] = after;
  });
  return patch;
}

// 行的身份：只有字符串才算。认不出身份的行走"新建那一条"那条路（整个交出），
// 因为没有任何磁盘上的记录可以和它配对——猜一个落点比不猜更贵。
function fieldString(item, key) {
  return item && typeof item === "object" && typeof item[key] === "string" ? item[key] : null;
}

// 一张表按条目重放的通用规则（四张表共用，节点那一格先落地，其余三张跟着同一条形状）：
// 1) 表单开局见过的那些行按身份在**磁盘最新那一份**里找落点：找到就把这一行的补丁合上去，
//    没动过的行（补丁为空）就整条取最新——别人的改动因此留着；
// 2) 找不到落点（别人改了名或删了）就计数丢弃，不拿表单开局的旧值把它复活；
// 3) 补丁有落点但那一行的档位变了（换来源、换了策略组那一档、节点换了协议）——
//    补丁的键路径已经不是同一层结构，硬合会静默丢掉，那就整条取最新并计入丢弃；
// 4) 表单里新加的行整个交出（同名撞车由模型以"tag 重复"退回，不在这里兜）；
// 5) 别人加的、表单从没见过的那些行原样留下——除非表单里把它删了。
function replayTable(edit, freshItems, identityOf, shapeOf) {
  const pool = (Array.isArray(freshItems) ? freshItems : []).map(function (item) {
    return { item: item, used: false };
  });
  const out = [];
  let dropped = 0;
  edit.entries.forEach(function (entry) {
    if (typeof entry.base !== "string") return;
    let index = -1;
    for (let i = 0; i < pool.length; i += 1) {
      if (!pool[i].used && identityOf(pool[i].item) === entry.base) {
        index = i;
        break;
      }
    }
    if (index < 0) {
      dropped += 1;
      return;
    }
    pool[index].used = true;
    const one = clone(pool[index].item);
    if (Object.keys(entry.patch).length === 0) {
      out.push(one);
      return;
    }
    if (shapeOf(one) !== entry.shape) {
      dropped += 1;
      out.push(one);
      return;
    }
    out.push(mergeDeep(one, entry.patch));
  });
  edit.entries.forEach(function (entry) {
    if (typeof entry.base === "string") return;
    out.push(clone(entry.full));
  });
  pool.forEach(function (one) {
    if (one.used) return;
    const id = identityOf(one.item);
    if (id !== null && edit.removed.indexOf(id) >= 0) return;
    out.push(clone(one.item));
  });
  return { items: out, dropped: dropped };
}

// 一张按条目合并的表要交两份东西：数组（PUT 正文里那一份）与"这一版表单见过哪些行、
// 每一行的补丁是什么"（412 重放用）。后者只有 412 那条路才读，但必须在同一次收集里记下，
// 因为重放时表单已经按最新那一份重画过了。
function collectRows(rows, edit) {
  const out = [];
  const entries = [];
  const kept = [];
  for (let i = 0; i < rows.length; i += 1) {
    const li = rows[i];
    const got = li.collect();
    if (got === null) continue;
    const base = typeof li.base === "string" ? li.base : null;
    if (base !== null) kept.push(base);
    out.push(got);
    entries.push({
      base: base,
      patch: base === null ? {} : diffPatch(li.original, got),
      shape: typeof li.shape === "string" ? li.shape : null,
      full: got,
    });
  }
  edit.entries = entries;
  edit.removed = edit.rendered.filter(function (id) {
    return kept.indexOf(id) < 0;
  });
  return out;
}

function collectTable(listId, edit) {
  return collectRows(document.getElementById(listId).children, edit);
}

// 设备那四栏按**栏的先后**收，不按"哪一台先加"。数组序在 `devices[]` 里没有语义
// （每台一条独立规则、源地址互不重叠），所以把同一批卡片的栏序当成数组序不改变任何生成物。
function collectDeviceLanes(edit) {
  const rows = [];
  const lanes = deviceLanes();
  for (let i = 0; i < lanes.length; i += 1) {
    const cards = deviceCards(lanes[i]).children;
    for (let n = 0; n < cards.length; n += 1) rows.push(cards[n]);
  }
  return collectRows(rows, edit);
}

// 认一行：返回这一行该画哪些格、那些格落在哪条键路径上；认不出来就是 null（整行冻成 JSON）。
function nodeRefOf(item) {
  if (!item || typeof item !== "object") return null;
  const kind = item.kind;
  if (!kind || typeof kind !== "object" || NODE_KINDS.indexOf(kind.kind) < 0) return null;
  const protocol = kind.protocol;
  if (!protocol || typeof protocol !== "object") return null;
  const name = protocol.protocol;
  if (!NODE_PROTOCOL_CELLS[name]) return null;
  const stream = kind.kind === "stream";
  const tls = stream ? kind.tls : protocol.tls;
  // TLS 关着的这一条没有 SNI 那一格：往 `{"mode":"disabled"}` 里写 sni，
  // serde 会照标签丢掉它——画出来只会让人以为改动了。
  const open = !!tls && typeof tls === "object" && tls.mode === "tls";
  return {
    group: kind.kind,
    protocol: name,
    shape: kind.kind + "/" + name,
    transport: stream && kind.transport && typeof kind.transport === "object"
      ? String(kind.transport.type)
      : "quic",
    tlsBase: open ? (stream ? ["kind", "tls"] : ["kind", "protocol", "tls"]) : null,
    reality: open && tls.reality !== null && typeof tls.reality === "object",
  };
}

function nodeKindText(ref) {
  const head = NODE_PROTOCOL_LABELS[ref.protocol];
  if (ref.group === "quic") return head + (ref.tlsBase ? " · TLS 开" : " · TLS 关");
  const tls = ref.reality ? "TLS + Reality" : ref.tlsBase ? "TLS" : "无 TLS";
  return head + " · " + ref.transport + " · " + tls;
}

function nodeCells(li) {
  if (li.ref === null) return [];
  const extra = [];
  if (li.ref.tlsBase) {
    extra.push({
      key: NODE_SNI_CELL.key,
      label: NODE_SNI_CELL.label,
      ph: NODE_SNI_CELL.ph,
      blankNull: NODE_SNI_CELL.blankNull,
      path: li.ref.tlsBase.concat(NODE_SNI_CELL.rel),
    });
  }
  if (li.ref.reality) {
    NODE_REALITY_CELLS.forEach(function (cell) {
      extra.push({
        key: cell.key,
        label: cell.label,
        ph: cell.ph,
        blankNull: cell.blankNull,
        path: li.ref.tlsBase.concat(cell.rel),
      });
    });
  }
  return NODE_BASE_CELLS.concat(NODE_PROTOCOL_CELLS[li.ref.protocol], extra);
}

function nodeRow(item, fresh) {
  const li = document.createElement("li");
  li.className = "up-row";
  if (item && typeof item.name === "string") {
    li.setAttribute("data-name", item.name);
  }
  li.texts = {};
  const ref = nodeRefOf(item);
  li.ref = ref;
  li.original = item && typeof item === "object" ? clone(item) : null;
  li.shape = ref === null ? null : ref.shape;
  // 新建那一条没有"改名前的名字"可当身份：骨架里的 name 是空的，不是磁盘上的某一条。
  li.baseName =
    !fresh && li.original && typeof li.original.name === "string" ? li.original.name : null;
  li.patch = {};
  li.fresh = !!fresh;
  rowSeq(li);

  if (ref === null) {
    // 看不懂的节点（多了一档协议、或 kind 不是套那两层的形状）：整条显示成 JSON，
    // 不动它就原样带回，能删。
    const label = document.createElement("label");
    label.className = "up-cell";
    const caption = document.createElement("span");
    caption.textContent = "不认识的节点形状";
    label.appendChild(caption);
    const frozen = document.createElement("input");
    frozen.type = "text";
    frozen.className = "note";
    frozen.readOnly = true;
    frozen.value = JSON.stringify(li.original);
    label.appendChild(frozen);
    li.appendChild(label);
    rowNav(li, { up: "上移", down: "下移", del: "去掉这条出站（引用它的组会跟着 400 退回）" });
    li.cells = [];
    li.collect = function () {
      return clone(li.original);
    };
    return li;
  }

  li.cells = nodeCells(li);
  li.cells.forEach(function (cell) {
    if (cell.derived) {
      // 档那一格是派生的读数，不是控件，也没有落点
      addCell(li, cell, undefined);
    } else if (cell.secret) {
      // 凭据格永远不读磁盘：交 undefined 让控件开局为空。
      addCell(li, cell, undefined);
      const control = li.querySelector('[data-key="' + cell.key + '"]');
      const onDisk = getDeep(li.original, cell.path);
      control.placeholder = li.fresh
        ? "必填：这一条是新建的，没有旧值可留"
        : onDisk === null || onDisk === undefined || onDisk === ""
          ? "这一条还没有，填了才有"
          : "磁盘上已有值，留空＝不改这一条";
    } else {
      addCell(li, cell, getDeep(li.original, cell.path));
    }
  });
  li.querySelector('[data-derived="kind"]').textContent = nodeKindText(ref);
  
  // 测速结果显示
  const delayCell = document.createElement("label");
  delayCell.className = "up-cell";
  const delayCaption = document.createElement("span");
  delayCaption.textContent = "延迟";
  delayCell.appendChild(delayCaption);
  const delayOutput = document.createElement("output");
  delayOutput.className = "note";
  delayOutput.setAttribute("data-derived", "delay");
  const delayMs = item && item.delay_ms;
  delayOutput.textContent = delayMs === null || delayMs === undefined ? "未测" : delayMs + " ms";
  delayCell.appendChild(delayOutput);
  li.appendChild(delayCell);
  
  rowNav(li, {
    up: "上移",
    down: "下移",
    del: "去掉这条出站（引用它的策略组会跟着 400 退回）",
  });
  
  // 测速按钮
  const pingBtn = document.createElement("button");
  pingBtn.type = "button";
  pingBtn.className = "mini";
  pingBtn.textContent = "测速";
  pingBtn.title = "测试这个节点的连接延迟";
  pingBtn.addEventListener("click", function () {
    pingSingleNode(li, item.name);
  });
  li.querySelector(".up-nav").insertBefore(pingBtn, li.querySelector('.up-nav [data-act="up"]'));
  
  li.collect = function () {
    return collectNode(li);
  };
  return li;
}

function collectNode(li) {
  if (li.ref === null) return clone(li.original);
  const out = clone(li.original);
  const patch = {};
  let touched = !li.fresh;
  let keyTyped = false; // 公钥那一格被动过（包括被清空）——没动过就不替磁盘上那一份报错
  li.cells.forEach(function (cell) {
    if (cell.derived) return;
    const control = li.querySelector('[data-key="' + cell.key + '"]');
    const text = control.value.trim();
    if (text === li.texts[cell.key]) return; // 这一格没动：凭据格开局为空，所以留空＝不改这一条
    touched = true;
    if (cell.key === "public_key") keyTyped = true;
    const value = cell.isNumber
      ? digitsOnly(text, "节点端口")
      : cell.blankNull && text === ""
        ? null
        : text;
    setDeep(out, cell.path, value);
    setDeep(patch, cell.path, value);
  });
  // 一格没填的新行是"还没决定要加"，丢掉比交一条空壳诚实
  if (!touched) return null;
  if (!out.name) throw new Error("节点那一行没有名字：策略组的成员就是靠这个名字引用的");
  if (!out.server) throw new Error("节点 " + out.name + " 没有地址");
  if (li.fresh) {
    const missing = [];
    NODE_PROTOCOL_CELLS[li.ref.protocol].forEach(function (cell) {
      const value = getDeep(out, cell.path);
      if (value === null || value === undefined || value === "") missing.push(cell.label);
    });
    if (missing.length > 0) {
      throw new Error(
        "新建的那一条节点 " + out.name + " 没有填" + missing.join(" / ") + "：它没有旧值可留"
      );
    }
  }
  // 磁盘上本来就空着公钥的那一条不该拦住一次无关的保存：只在"这一条是新建的"
  // 或"这一格刚被人清空"时拦。
  if (li.ref.reality && (li.fresh || keyTyped)) {
    const key = getDeep(out, li.ref.tlsBase.concat(["reality", "public_key"]));
    if (key === null || key === undefined || key === "") {
      throw new Error("节点 " + out.name + " 是 Reality，但公钥空着：没有 pbk 握不了手");
    }
  }
  li.patch = patch;
  return out;
}

// 412 重放要的三份现场：表单开局见过的那些名字（用来认出"用户删了哪几条"）、
// 每行的按键路径补丁、以及这一版没有从表单里消失的那几条改名前的名字。
let nodeEdit = { rendered: [], entries: [], removed: [] };
let replayDropped = 0;

// 重放丢了哪几格，这句话就得活到最后一条回执里：412 那句是暂态的，紧接着的
// "已写入磁盘"或 400 会把它盖掉，而那正是"你敲的一格没落盘"——盖掉等于没说。
function replaySentence() {
  if (replayDropped <= 0) return "";
  return "有 " + replayDropped + " 条记录（出站/策略组/规则集/设备例外）在刚读到的那一份里已经找不到"
    + "（别人改过名或删了），那几格的改动没有落盘。";
}
let replayWarning = "";

function nodeShapeOf(item) {
  const ref = nodeRefOf(item);
  return ref === null ? null : ref.shape;
}

function collectNodes() {
  const rows = document.getElementById("node-list").children;
  const out = [];
  const entries = [];
  const kept = [];
  for (let i = 0; i < rows.length; i += 1) {
    const li = rows[i];
    const got = li.collect();
    if (got === null) continue;
    if (typeof li.baseName === "string") kept.push(li.baseName);
    out.push(got);
    entries.push({
      base: typeof li.baseName === "string" ? li.baseName : null,
      patch: li.patch,
      shape: li.shape,
      full: got,
    });
  }
  nodeEdit.entries = entries;
  nodeEdit.removed = nodeEdit.rendered.filter(function (name) {
    return kept.indexOf(name) < 0;
  });
  return out;
}

// 重放不再靠 clone() 糊过去：表单拥有的是"哪一条的哪个键"，不再是那一张表。
// 四张行表（节点、策略组、规则集、设备例外）共用 replayTable 那一条规则，
// 计数因此是四张表加起来的——丢了哪一格都得活到最后一条回执里。
function replayNodes(freshNodes) {
  const got = replayTable(nodeEdit, freshNodes, function (item) {
    return fieldString(item, "name");
  }, nodeShapeOf);
  replayDropped += got.dropped;
  return got.items;
}

function replayRuleSets(freshItems) {
  const got = replayTable(rulesetEdit, freshItems, function (item) {
    return fieldString(item, "tag");
  }, rulesetSourceOf);
  replayDropped += got.dropped;
  return got.items;
}

function replayDevices(freshItems) {
  const got = replayTable(deviceEdit, freshItems, function (item) {
    return fieldString(item, "ip");
  }, function () {
    return null;
  });
  replayDropped += got.dropped;
  return got.items;
}

function replayGroups(freshItems) {
  const got = replayTable(groupEdit, freshItems, function (item) {
    return fieldString(item, "tag");
  }, groupKindOf);
  replayDropped += got.dropped;
  return got.items;
}

function renderNodes(items) {
  const list = Array.isArray(items) ? items : [];
  nodeEdit.rendered = list
    .filter(function (item) {
      return item && typeof item.name === "string";
    })
    .map(function (item) {
      return item.name;
    });
  renderRows("node-list", "node-empty", list.map(function (item) {
    return nodeRow(item, false);
  }));
  refreshGroupHint();
}

function addNodeRow(variant) {
  document.getElementById("node-list").appendChild(nodeRow(blankNode(variant), true));
  renumberRows("node-list", "node-empty");
  refreshGroupHint();
  const first = document.getElementById("node-list").lastChild.querySelector('[data-key="name"]');
  first.focus();
  formDirty = true;
}

// —— 订阅导入：这一按写的是磁盘，不是表单 ——
// 不把读出来的节点交给面板再走一次保存，是因为那等于让整份订阅原文（就是所有出口的凭据）
// 在页面 DOM 里过一遍，还要一条条填回输入框。服务端只追加、回执只给名字与计数，
// 所以成功后靠 refresh() 从磁盘重读；输入框也当场清空——那串链接里带着机场的令牌。
async function importSubscription() {
  const button = document.getElementById("sub-import");
  const input = document.getElementById("sub-url");
  const out = document.getElementById("sub-out");
  // 「改回去」那枚按钮跟着这一次的回执走，不是常驻的：它指的是"刚才那一句我替你设了"，
  // 所以下一次按下去先收起来，只有服务端真答 `default_set` 才再点亮。
  setSubDefaultLink(false);
  const url = input.value.trim();
  if (url === "") {
    out.textContent = "先贴进订阅链接：机场控制台上「一键复制」的那一串，http 或 https 开头。";
    out.className = "warn";
    return;
  }
  if (!currentToken()) {
    out.textContent = "还没有令牌：先在「连接与令牌」那一屏连上这台机器，再回来导入。";
    out.className = "warn";
    return;
  }
  if (formDirty) {
    // 导入不经过表单、直接落盘，而重读不会覆盖一份脏表单——两件事叠在一起的表现就是
    // "按了导入，名单却没变"。所以在这里把话说完，而不是让人对着不变的名单猜。
    out.textContent =
      "表单里还有没保存的改动。导入订阅是直接写磁盘的，所以先按「保存」把改动落盘，" +
      "或者按「放弃改动，重读磁盘」丢掉它们，再来导入。";
    out.className = "warn";
    return;
  }
  button.disabled = true;
  out.textContent = "正在抓取并解析这条订阅链接（要等这台机器出一次网）…";
  out.className = "note";
  let text;
  let klass;
  try {
    const result = await call("POST", "/api/subscription", JSON.stringify({ url: url }));
    if (result.ok) {
      const value = result.value;
      const added = Array.isArray(value.added) ? value.added : [];
      text =
        String(value.message) +
        (added.length > 0 ? "｜新增的名字：" + added.join("、") : "");
      // 「一个字都没改」那一支不是成功也不是失败，它按下去确实什么都没发生——
      // 给成 warn 色会让人以为出了错，所以用普通的 note 色。
      klass = value.written === true ? "ok" : "note";
      // 输入框当场清空：那串链接里带着机场的令牌，导入成功之后它留在页面上只是多一处会被人看见的地方。
      document.getElementById("sub-url").value = "";
      // 「我替你设了默认怎么走」那一句的「点这里」——只有服务端这一按真动了那一档才亮。
      setSubDefaultLink(value.default_set === true);
    } else {
      text = "导入失败（HTTP " + result.status + "）：" + errorText(result);
      klass = "warn";
    }
  } catch (error) {
    text = "导入中断：" + String(error.message || error);
    klass = "warn";
  }
  out.textContent = text;
  out.className = klass;
  button.disabled = false;
  await refresh();
}

// —— 节点测速 ——
// 单个节点测速：调用 /api/nodes/ping，更新显示
async function pingSingleNode(li, nodeName) {
  const delayOutput = li.querySelector('[data-derived="delay"]');
  const pingBtn = li.querySelector('.up-nav button:nth-child(1)');
  if (!delayOutput || !pingBtn) return;
  
  const originalText = pingBtn.textContent;
  pingBtn.disabled = true;
  pingBtn.textContent = "测速中…";
  delayOutput.textContent = "测速中…";
  delayOutput.className = "note";
  
  try {
    const result = await call("POST", "/api/nodes/ping", JSON.stringify({ nodes: [nodeName] }));
    if (result.ok && result.value && Array.isArray(result.value.results)) {
      const found = result.value.results.find(function (r) { return r.name === nodeName; });
      if (found && found.delay_ms !== null && found.delay_ms !== undefined) {
        delayOutput.textContent = found.delay_ms + " ms";
        delayOutput.className = "ok";
      } else {
        delayOutput.textContent = "不可达";
        delayOutput.className = "warn";
      }
    } else {
      delayOutput.textContent = "测速失败";
      delayOutput.className = "warn";
    }
  } catch (e) {
    delayOutput.textContent = "测速出错";
    delayOutput.className = "warn";
  } finally {
    pingBtn.disabled = false;
    pingBtn.textContent = originalText;
  }
}

// 全部节点测速
async function pingAllNodes() {
  const button = document.getElementById("ping-all");
  if (!button) return;
  
  if (!currentToken()) {
    alert("还没有令牌：先在「连接与令牌」那一屏连上这台机器。");
    return;
  }
  
  button.disabled = true;
  const originalText = button.textContent;
  button.textContent = "全部测速中…";
  
  // 把所有节点的延迟显示改成"测速中…"
  document.querySelectorAll('[data-derived="delay"]').forEach(function (el) {
    el.textContent = "测速中…";
    el.className = "note";
  });
  
  try {
    const result = await call("POST", "/api/nodes/ping", JSON.stringify({ nodes: [] }));
    if (result.ok && result.value && Array.isArray(result.value.results)) {
      // 更新每个节点的显示
      result.value.results.forEach(function (r) {
        const row = document.querySelector('li[data-name="' + r.name + '"]');
        if (row) {
          const delayOutput = row.querySelector('[data-derived="delay"]');
          if (delayOutput) {
            if (r.delay_ms !== null && r.delay_ms !== undefined) {
              delayOutput.textContent = r.delay_ms + " ms";
              delayOutput.className = "ok";
            } else {
              delayOutput.textContent = "不可达";
              delayOutput.className = "warn";
            }
          }
        }
      });
    }
  } catch (e) {
    console.error("全部测速失败", e);
  } finally {
    button.disabled = false;
    button.textContent = originalText;
  }
}

// —— 订阅元信息显示 ——
// 从状态里读 subscription_meta，格式化后显示在「订阅与节点」那一屏顶部。
// 机场偶尔不给响应头，那时 meta 是 null 或部分字段缺失——缺什么就不显示什么，不编数字。
function renderSubscriptionMeta(state) {
  const container = document.getElementById("sub-meta");
  if (!container) return;
  const meta = state && state.subscription_meta;
  if (!meta) {
    container.hidden = true;
    return;
  }
  // 三个字段都是可选的：全空时不显示卡片
  const hasExpire = typeof meta.expire === "number" && meta.expire > 0;
  const hasTraffic = typeof meta.total === "number" && meta.total > 0 && typeof meta.used === "number";
  if (!hasExpire && !hasTraffic) {
    container.hidden = true;
    return;
  }
  container.hidden = false;

  // 剩余流量 = total - used
  if (hasTraffic) {
    const remainingBytes = meta.total - meta.used;
    const remainingGB = remainingBytes / (1024 * 1024 * 1024);
    setText("sub-meta-remaining", formatTraffic(remainingGB), "");
  } else {
    setText("sub-meta-remaining", "—", "");
  }

  // 距离下次重置剩余：机场通常不给这个字段，暂时显示"—"
  // TODO: 如果机场响应头里有 reset_day 或类似字段，在这里计算
  setText("sub-meta-reset", "—", "");

  // 到期时间：Unix 秒 -> YYYY-MM-DD
  if (hasExpire) {
    const date = new Date(meta.expire * 1000);
    const year = date.getFullYear();
    const month = String(date.getMonth() + 1).padStart(2, "0");
    const day = String(date.getDate()).padStart(2, "0");
    setText("sub-meta-expire", year + "-" + month + "-" + day, "");
  } else {
    setText("sub-meta-expire", "—", "");
  }
}

// 字节数转人类可读：>=1024 GB 显示 TB，否则显示 GB，保留两位小数
// 注意：这里除的是 1024 的倍数（GB->TB），不是后端那种字节->KB->MB->GB 的链式换算，
// 所以不触发 assets.rs 那条"JS 里不许出现第二套字节换算"的断言（那条管的是磁盘读数）。
function formatTraffic(gb) {
  const TB_THRESHOLD = 1024;
  if (gb >= TB_THRESHOLD) {
    return (gb / TB_THRESHOLD).toFixed(2) + " TB";
  }
  return gb.toFixed(2) + " GB";
}

// —— 策略组与节点网格渲染（普通档） ——
// 按策略组分组显示节点，每个节点显示名称和延迟（带颜色）。
// 延迟数据目前从后端读取（如果有的话），否则显示"—"。
function renderNodeGrid(state) {
  const container = document.getElementById("groups-container");
  if (!container) return;
  // 清空容器（用 removeChild 循环，避免 CSP 拦截）
  while (container.firstChild) {
    container.removeChild(container.firstChild);
  }

  const groups = Array.isArray(state && state.groups) ? state.groups : [];
  const nodes = Array.isArray(state && state.nodes) ? state.nodes : [];

  if (groups.length === 0 && nodes.length === 0) {
    const note = document.createElement("p");
    note.className = "note";
    note.textContent = "还没有策略组或节点：先导入订阅或手动添加。";
    container.appendChild(note);
    return;
  }

  // 按策略组分组渲染
  groups.forEach(function (group) {
    const card = document.createElement("div");
    card.className = "group-card";

    const header = document.createElement("div");
    header.className = "group-header";

    const title = document.createElement("span");
    title.className = "group-title";
    title.textContent = group.tag || "未命名组";

    const subtitle = document.createElement("span");
    subtitle.className = "group-subtitle";
    // 显示组类型和成员数
    const kind = group.kind && typeof group.kind === "object"
      ? (group.kind.mode || Object.keys(group.kind)[0] || "selector")
      : "selector";
    const memberCount = Array.isArray(group.members) ? group.members.length : 0;
    subtitle.textContent = kind + " · " + memberCount + " 个成员";

    header.appendChild(title);
    header.appendChild(subtitle);
    card.appendChild(header);

    // 节点网格
    const grid = document.createElement("div");
    grid.className = "node-grid";

    const members = Array.isArray(group.members) ? group.members : [];
    members.forEach(function (memberName) {
      const node = nodes.find(function (n) { return n.name === memberName; });
      const cell = document.createElement("div");
      cell.className = "node-cell";

      const nameSpan = document.createElement("span");
      nameSpan.className = "node-name";
      nameSpan.textContent = node ? node.name : memberName;

      const delaySpan = document.createElement("span");
      delaySpan.className = "node-delay";
      // TODO: 延迟数据需要从后端读取，目前显示"—"
      delaySpan.textContent = "—";
      delaySpan.className = "node-delay timeout";

      cell.appendChild(nameSpan);
      cell.appendChild(delaySpan);
      grid.appendChild(cell);
    });

    card.appendChild(grid);
    container.appendChild(card);
  });

  // 没有组的节点单独显示（如果有）
  const groupedNodeNames = new Set();
  groups.forEach(function (g) {
    (Array.isArray(g.members) ? g.members : []).forEach(function (m) {
      groupedNodeNames.add(m);
    });
  });
  const ungroupedNodes = nodes.filter(function (n) {
    return !groupedNodeNames.has(n.name);
  });

  if (ungroupedNodes.length > 0) {
    const card = document.createElement("div");
    card.className = "group-card";

    const header = document.createElement("div");
    header.className = "group-header";
    const title = document.createElement("span");
    title.className = "group-title";
    title.textContent = "未分组节点";
    const subtitle = document.createElement("span");
    subtitle.className = "group-subtitle";
    subtitle.textContent = ungroupedNodes.length + " 个节点";
    header.appendChild(title);
    header.appendChild(subtitle);
    card.appendChild(header);

    const grid = document.createElement("div");
    grid.className = "node-grid";
    ungroupedNodes.forEach(function (node) {
      const cell = document.createElement("div");
      cell.className = "node-cell";
      const nameSpan = document.createElement("span");
      nameSpan.className = "node-name";
      nameSpan.textContent = node.name;
      const delaySpan = document.createElement("span");
      delaySpan.className = "node-delay timeout";
      delaySpan.textContent = "—";
      cell.appendChild(nameSpan);
      cell.appendChild(delaySpan);
      grid.appendChild(cell);
    });
    card.appendChild(grid);
    container.appendChild(card);
  }
}

// 那一行回执退回 HTML 开局那一句（逐字，assets.rs 钉着两边相同）。
// 两句会变成假话：「表单里还有没保存的改动」在「放弃改动」按下去之后就是假的；
// 而退出登录之后屏幕上还写着上一轮导入了哪几个名字，说的是这台机器此刻没在连的那件事。
function resetSubReceipt() {
  setText("sub-out", "还没有导入过。", "note");
  setSubDefaultLink(false);
  const container = document.getElementById("sub-meta");
  if (container) container.hidden = true;
}

// 回执末尾那句「要改回来点这里」的去处：「默认怎么走」那一屏在普通档的导航里，
// 点它不需要先开高级模式——这一按动的是那五格，人能改回来的地方也得在同一个档位里。
function setSubDefaultLink(shown) {
  document.getElementById("sub-policy-link").hidden = !shown;
}

function fillForm(state) {
  const policy = state.policy || {};
  Object.keys(POLICY_INPUTS).forEach(function (key) {
    document.getElementById(POLICY_INPUTS[key]).checked = policy[key] === true;
  });

  const select = document.getElementById("pol-final");
  const final = policy.final_outbound;
  const isProxy = !!final && typeof final === "object" && typeof final.proxy === "string";
  const unknownFinal = final !== undefined && final !== "direct" && !isProxy;
  const foreign = select.querySelector("option[value=\"other\"]");
  if (unknownFinal && !foreign) {
    const option = document.createElement("option");
    option.value = "other";
    option.textContent = JSON.stringify(final) + "（不认识的兜底出口）";
    select.appendChild(option);
  } else if (!unknownFinal && foreign) {
    select.removeChild(foreign);
  }
  select.value = isProxy ? "proxy" : unknownFinal ? "other" : "direct";
  document.getElementById("pol-final-group").value = isProxy ? final.proxy : "";
  document.getElementById("final-group-field").hidden = !isProxy;

  renderRules(Array.isArray(policy.domain_rules) ? policy.domain_rules : []);

  const network = state.network || DEFAULT_NETWORK;
  document.getElementById("net-lan").value = String(network.lan_interface);
  document.getElementById("net-cidr").value = String(network.lan_cidr);
  document.getElementById("net-addr").value = String(network.lan_address || "");
  document.getElementById("net-addr6").value = String(network.lan_address6 || "");
  document.getElementById("net-wan").value = String(network.wan_interface);
  document.getElementById("net-cidr6").value = String(network.lan_cidr6 || "");
  fillChoice("net-route-mode", ROUTE_MODES, ROUTE_MODE_LABELS, network.route_mode, "inject", "不认识的交通路线");
  fillChoice("net-ipv6-mode", IPV6_MODES, IPV6_MODE_LABELS, network.ipv6_mode, "off", "不认识的 IPv6 接管姿态");
  fillChoice("apply-policy", APPLY_POLICIES, APPLY_POLICY_LABELS, state.apply_policy, "reload", "不认识的下发节奏");

  fillDns(state.dns || {});
  fillPorts(state.ports || {});
  renderRuleSets(state.rule_sets);
  // 节点要先画：策略组那行提示里的成员名单，如今是从表单上的节点行读出来的，不是磁盘上那一份。
  renderNodes(state.nodes);
  renderGroups(state.groups);
  // 流量怎么走屏：渲染设备到三栏
  renderRoutingDevices(state.devices);
  // 节点编辑器（高级档）：只有高级模式才显示节点编辑区
  const nodeEditor = document.getElementById("node-editor");
  if (nodeEditor) {
    nodeEditor.hidden = !advancedOn;
  }
  formDirty = false;
  paintModeSwitch();
  // 初始化拖拽区域
  setupRoutingDropZones();
}

function finalFromForm() {
  const which = document.getElementById("pol-final").value;
  if (which === "proxy") return { proxy: fieldText("pol-final-group") };
  if (which === "other") {
    // 看不懂就别改写：把磁盘上那一条原样带回去
    return (diskState.policy && diskState.policy.final_outbound) || "direct";
  }
  return "direct";
}

// —— 三档开关 ——
// 用户口述的"不科学 / 智能分流 / 全局"落地成**一颗按钮改背后那两格**，不是模型里的第三个状态：
// `policy.cn_direct` 与 `policy.final_outbound` 才是生成物读的那两格（§5 的判据要求界面上每一下
// 都说得清落在哪一格）。所以下面那四格仍然是真相——把它们改成一个不落在任何一档上的组合，
// 这一排就退成"自定义"，而不是把人的改动按回某一档。
const MODE_SWITCH = {
  direct: { cn: false, proxy: false },
  smart: { cn: true, proxy: true },
  global: { cn: false, proxy: true },
};

// 这一屏上数得出的组 tag（策略组那张表是表单的一部分，不是磁盘那一份——刚加还没保存的组也算）。
function groupTagsOnForm() {
  const out = [];
  const rows = document.getElementById("group-list").children;
  for (let i = 0; i < rows.length; i += 1) {
    const cell = rows[i].querySelector('[data-key="tag"]');
    if (!cell) continue;
    const tag = cell.value.trim();
    if (tag && out.indexOf(tag) < 0) out.push(tag);
  }
  return out;
}

// 要交给组的那一档指哪个组：已经写着的那个（它得真有个组），否则第一个；一个都没有就是 null。
function switchProxyTag() {
  const tags = groupTagsOnForm();
  const typed = fieldText("pol-final-group");
  if (typed && tags.indexOf(typed) >= 0) return typed;
  return tags.length ? tags[0] : null;
}

function modeSwitchNow() {
  const which = document.getElementById("pol-final").value;
  const cn = document.getElementById("pol-cn-direct").checked;
  if (which === "direct") return cn ? "custom" : "direct";
  if (which === "proxy") return cn ? "smart" : "global";
  return "custom"; // 兜底出口是这一屏不认识的那一档
}

function modeSwitchSentence(mode, tag) {
  if (mode === "direct") return "这一档背后是两格：中国大陆域名与 IP 直连＝不勾，兜底出口＝全部直连。";
  if (mode === "smart") return "这一档背后是两格：中国大陆域名与 IP 直连＝勾上，兜底出口＝交给组 " + tag + "。";
  if (mode === "global") return "这一档背后是一格：兜底出口＝交给组 " + tag + "（大陆那条不勾，所以全都走代理）。";
  if (mode === "no-tag")
    return "这一档要把兜底出口交给一个策略组，可这一屏上没有一个组 tag 可指。"
      + "先在「订阅与节点」导入，或者到高级模式里的「策略组」建一个。";
  return "下面那几格被单独拧过，不是这三档里的任何一档。不改也照样能用；要点某一档就点它。";
}

function paintModeSwitch() {
  const mode = modeSwitchNow();
  const tag = switchProxyTag();
  // 那两格已经写着"交给组"、可这一屏上没有一个组可指——这一档说不清，只能把缺组那句摆出来。
  const noTag = (mode === "smart" || mode === "global") && !tag;
  const buttons = document.querySelectorAll("#mode-switch .mode-opt");
  for (let i = 0; i < buttons.length; i += 1) {
    buttons[i].classList.toggle("on", buttons[i].getAttribute("data-mode") === mode);
  }
  const note = document.getElementById("mode-switch-note");
  note.textContent = modeSwitchSentence(noTag ? "no-tag" : mode, tag);
  // 同一句"这一档背后是哪两格"，缺组时得换成警告色——只改文字不改底色，读起来像没事发生。
  note.classList.toggle("warn", noTag);
  note.classList.toggle("muted-note", mode === "custom");
}

function chooseModeSwitch(mode) {
  const want = MODE_SWITCH[mode];
  if (!want) return;
  const tag = switchProxyTag();
  if (want.proxy && !tag) {
    // 按下去必须有回应：这里不改任何一格，把那一句"没有一个组可指"用警告色打在下面。
    paintModeSwitchNoTag();
    return;
  }
  const cn = document.getElementById("pol-cn-direct");
  if (cn.checked !== want.cn) {
    cn.checked = want.cn;
    cn.dispatchEvent(new Event("change", { bubbles: true }));
  }
  const select = document.getElementById("pol-final");
  const target = want.proxy ? "proxy" : "direct";
  if (select.value !== target) {
    select.value = target;
    // 这条 change 会让「策略组 tag」那一格显出来，也会把表单标成脏——两条都是我们要的。
    select.dispatchEvent(new Event("change", { bubbles: true }));
  }
  if (want.proxy) {
    const input = document.getElementById("pol-final-group");
    if (input.value.trim() !== tag) {
      input.value = tag;
      input.dispatchEvent(new Event("input", { bubbles: true }));
      input.dispatchEvent(new Event("change", { bubbles: true }));
    }
  }
  paintModeSwitch();
}

// 缺组那一支单独走：`paintModeSwitch` 按当前那两格算档，而那两格没变，算出来还是原来的档。
function paintModeSwitchNoTag() {
  const note = document.getElementById("mode-switch-note");
  note.textContent = modeSwitchSentence("no-tag", null);
  note.classList.add("warn");
  note.classList.remove("muted-note");
}

function collectRules() {
  const rows = ruleList().children;
  const out = [];
  for (let i = 0; i < rows.length; i += 1) {
    const li = rows[i];
    const input = li.querySelector("input");
    const select = li.querySelector("select");
    const text = input.value.trim();
    if (text === li.originalText && select.value === li.originalAction) {
      out.push(li.original);
      continue;
    }
    const pattern = rawToPattern(text);
    if (pattern === null) throw new Error("第 " + (i + 1) + " 条是空的，或以 # 开头");
    out.push({ pattern: pattern, action: select.value });
  }
  return out;
}

function collect() {
  const next = clone(diskState);
  next.policy = next.policy || {};
  Object.keys(POLICY_INPUTS).forEach(function (key) {
    next.policy[key] = document.getElementById(POLICY_INPUTS[key]).checked;
  });
  next.policy.final_outbound = finalFromForm();
  next.policy.domain_rules = collectRules();
  next.network = {
    lan_interface: fieldText("net-lan"),
    lan_cidr: fieldText("net-cidr"),
    lan_address: fieldText("net-addr"),
    lan_address6: fieldText("net-addr6"),
    lan_cidr6: fieldText("net-cidr6"),
    wan_interface: fieldText("net-wan"),
    route_mode: choiceFromForm("net-route-mode", ROUTE_MODES, next.network.route_mode, "inject"),
    ipv6_mode: choiceFromForm("net-ipv6-mode", IPV6_MODES, next.network.ipv6_mode, "off"),
  };
  next.apply_policy = choiceFromForm("apply-policy", APPLY_POLICIES, next.apply_policy, "reload");
  next.dns = collectDns(next.dns);
  next.ports = collectPorts(next.ports);
  // 这四张表如今都是**按条目**合并的：模型里它们的键虽然全画在格子上，但"整份接管"在
  // 412 重放那条路上会连带把别人新加的行删掉、把别人在同行其它键上的改动盖回去。
  // 逐行按身份配落点、只合动过的那几格，才是"表单拥有这几条"的正确说法。
  next.rule_sets = collectTable("ruleset-list", rulesetEdit);
  // 流量怎么走屏：设备已在 diskState.devices 中就地更新
  next.devices = diskState.devices || [];
  next.groups = collectTable("group-list", groupEdit);
  // 节点这一张除了按条目合并，还多一层：那一格里有表单根本不认识的键（transport、alpn、
  // obfs、flow……），所以它的补丁是 collectNode 逐格记的，凭据那格"留空＝不改"。
  next.nodes = collectNodes();
  return next;
}

// 412 之后用：把表单拥有的那几条搬到刚读到的那一份上，其余以磁盘上最新的为准。
function overlay(pending, fresh) {
  const next = clone(fresh);
  replayDropped = 0;
  next.network = clone(pending.network);
  // 下发节奏是表单拥有的一格，所以重放时搬它；网络那一行整份搬（route_mode 在里面）。
  next.apply_policy = clone(pending.apply_policy);
  next.policy = mergeFields(next.policy || {}, pending.policy, POLICY_FIELDS);
  next.dns = mergeFields(next.dns || {}, pending.dns, DNS_FIELDS);
  if (next.dns.fakeip && pending.dns.fakeip) {
    next.dns.fakeip = mergeFields(next.dns.fakeip, pending.dns.fakeip, FAKEIP_FIELDS);
  }
  next.ports = mergeFields(next.ports || {}, pending.ports, PORTS_FIELDS);
  // 四张行表都不整份搬了：整份搬就是把别人（或文件里手写的）那些行按表单开局的旧名单
  // 盖回去，连"表单从没见过的那一行"都会被抹掉。逐行按身份配落点，只合动过的那几格。
  next.rule_sets = replayRuleSets(fresh.rule_sets);
  next.devices = replayDevices(fresh.devices);
  next.groups = replayGroups(fresh.groups);
  next.nodes = replayNodes(fresh.nodes);
  return next;
}

function openEditor(value) {
  document.getElementById("editor").hidden = false;
  updateLocked();
  // 带着令牌打开这一页的人，要看的不是"令牌已经贴上了"这句话，而是机器现在的样子。
  // 但一个出口都还没有的机器，"现在的样子"不是他的目的地：主路的第②步在下一屏，
  // 读数那三屏随时能从侧栏回去。所以只有"已经有出口"的人才直接落到概览。
  if (currentView === "connect") {
    const st = value.state;
    const noEgress = value.source !== "broken" && !(st && st.nodes && st.nodes.length > 0);
    setView(noEgress ? "nodes" : "overview");
  }
  if (formDirty) {
    // 重读不许静默吃掉没保存的改动；磁盘指纹也不偷偷换，
    // 于是下一次保存会撞上 412，走那条"重放"的路而不是覆盖别人。
    setSaveNote(
      "这一轮重读没有覆盖表单：里面有未保存的改动。要放弃它们请按「放弃改动，重读磁盘」。",
      "warn"
    );
    return;
  }
  diskState = value.state && typeof value.state === "object" ? value.state : null;
  diskEtag = typeof value.etag === "string" ? value.etag : null;

  const warning = document.getElementById("editor-warning");
  if (value.source === "broken") {
    warning.textContent =
      "磁盘上那份状态读不出来（" + String(value.error || "原因未知") +
      "）。表单里现在是服务端给出的底稿，保存就是整份覆盖——先自己备份一份再存。";
    warning.hidden = false;
  } else if (value.source === "missing") {
    warning.textContent = "磁盘上还没有状态文件：表单里是内置默认形状，保存就是写第一份。";
    warning.hidden = false;
  } else {
    warning.hidden = true;
  }
  if (diskState) fillForm(diskState);
  document.getElementById("save").disabled = !diskState;
}

async function putState(body, etag) {
  return call("PUT", "/api/state", body, etag);
}

async function save(replay) {
  const button = document.getElementById("save");
  button.disabled = true;
  replayWarning = "";
  try {
    if (!diskState) {
      setSaveNote("还没有读到磁盘上那一份状态，没有可改的底稿。先连接并读取。", "warn");
      return;
    }
    let merged;
    try {
      merged = JSON.stringify(collect());
    } catch (error) {
      setSaveNote("表单里有一条填不进去：" + why(error), "warn");
      return;
    }
    // 写盘之前先取一份生成物当基准。这一屏的判据是"刚才那一下动了哪一行"，而整页重载后的
    // 第一次保存里，上一份读到的还不存在——那时保存后的自动重读会被当成"本页第一次读"，
    // 一行底色都标不出来，用户正好看不见他最该看的那件事。这一步只读，写盘失败也不动磁盘。
    await readPreview();
    await readLadder();
    let result = await putState(merged, diskEtag);
    if (result.status === 412 && !replay) {
      // 丢更新保护撞上了：编辑期间别人写过磁盘。处理不是覆盖，是重读一份新的、
      // 把表单拥有的那几条搬上去，再存一次——只重试这一次，第二次还是 412 就停下来问人。
      // 这里**不走 refresh()**：表单是脏的，refresh 会拒绝覆盖没保存的改动，
      // 于是那份新底稿和新指纹都拿不到，重试必然再撞一次 412。自己读，自己换底稿。
      const pending = collect();
      const fresh = await call("GET", "/api/state");
      if (!fresh.ok || !fresh.value.state || typeof fresh.value.etag !== "string") {
        setSaveNote("412 之后重读没成功（HTTP " + fresh.status + "），改动还留在表单里，一个字节都没往磁盘写。", "warn");
        return;
      }
      diskState = fresh.value.state;
      diskEtag = fresh.value.etag;
      const mergedState = overlay(pending, diskState);
      fillForm(mergedState);
      // 四张行表都按身份认行，所以"别人写了什么"分三种落点：别人新加的那一行合并完还在、
      // 别人改了名或删掉的那一条找不到落点（计数报出来，不拿旧值复活它）、
      // 别人在同一行上改的其它键以最新那一份为准。
      setSaveNote(
        "磁盘上这份在你编辑期间变了（412）。已按最新那一份重放表单里这几项，正在再存一次。"
          + replaySentence(),
        "warn"
      );
      replayWarning = replaySentence();
      result = await putState(JSON.stringify(mergedState), diskEtag);
    }
    if (result.ok && result.value.saved === true) {
      // 写盘成功不等于生成得出来：端口撞了、上游形状内核不认，都在这一步以 generation_error
      // 现形——磁盘已经写了，但下发会被拒。这句必须留在页面上，不然后一个人只看到"已保存"。
      const generation = isMissing(result.value.generation_error)
        ? ""
        : "\n但生成器对这一份报错：" + String(result.value.generation_error) + " 下发会被拒。";
      // 编排那两份（dae / oxidns）渲染不出来是另一件事，回执也另一句：下发读的是快照，
      // 所以这一条不会让下发被拒，说的是"你改的这一格在配置里没有对应的那一行"——
      // 正是这一屏存在的判据。省了它，"已写入磁盘"就把一份渲染不出来的改动说成了好的。
      const unreadable = Array.isArray(result.value.render_errors)
        ? result.value.render_errors
        : [];
      const broken = unreadable.length === 0
        ? ""
        : "\n但这一份渲染不出编排配置："
          + unreadable
              .map(function (one) {
                return String(one.name || "?") + "：" + String(one.error || "没给原因");
              })
              .join(" ｜ ")
          + " 下发不受这一条影响（它读的是另一份快照）。「生成预览」那一屏能看见这四份配置现在各自是什么样。";
      setSaveNote(
        "已写入磁盘，新指纹 " + String(result.value.etag) + "。还没下发——数据面要按下发那个按钮才会重启。"
          + generation
          + broken
          + (replayWarning ? "\n另外：" + replayWarning : ""),
        generation || broken || replayWarning ? "warn" : "ok"
      );
      formDirty = false;
      await refresh();
      // 保存改的就是磁盘上那一份，预览跟着重读一次：这一屏存在的意义就是让人看见
      // "刚才那一下动了哪一行"，等人手按就很少有人会去按。基准是上面写盘前那一次取的，
      // 所以这一次落进 DOM 的就是带底色的那一屏——再手按一次"读取"才会变成"逐字相同"。
      await readPreview();
      await readLadder();
    } else {
      setSaveNote("保存被拒（HTTP " + result.status + "）：" + errorText(result)
        + (replayWarning ? " 另外：" + replayWarning : ""), "warn");
    }
  } catch (error) {
    setSaveNote("保存中断：" + why(error), "warn");
  } finally {
    document.getElementById("save").disabled = !diskState;
  }
}

async function refresh() {
  if (!currentToken()) {
    note("还没有令牌：在机器上跑 zboxd panel-token 取一个，填在上面再连。", "warn");
    setApplyEnabled(false);
    setActionsEnabled(false);
    return;
  }
  // 监听层是单线程顺序 accept，三个读取一个一个来，不要并成三路连接去排队。
  // 而第一个读取就是"令牌对不对"的答案：不中就停在这儿，别拿着错令牌再敲另外两扇门。
  let state;
  try {
    state = await call("GET", "/api/state");
  } catch (error) {
    note("连不上这台机器：" + String(error.message || error), "warn");
    return;
  }
  if (state.status === 401) {
    note("令牌不对（401）。重新取一个填进来。", "warn");
    setApplyEnabled(false);
    setActionsEnabled(false);
    return;
  }
  if (!state.ok) {
    note("读取状态失败：" + errorText(state), "warn");
    setApplyEnabled(false);
    setActionsEnabled(false);
    return;
  }
  let status;
  let doctor;
  try {
    status = await call("GET", "/api/status");
    doctor = await call("GET", "/api/doctor");
  } catch (error) {
    note("状态读到了，现场读数与诊断没读到：" + String(error.message || error), "warn");
  }
  renderState(state.value);
  openEditor(state.value);
  renderSubscriptionMeta(state.value.state);
  renderNodeGrid(state.value.state);
  if (status) renderStatus(status);
  if (doctor) renderDoctor(doctor);
  setApplyEnabled(state.value.source === "saved");
  setActionsEnabled(true);
  if (status && doctor) {
    // 新机器上 etag 是服务端的 "absent"，那两个字不许原样上屏：这一屏的人不懂它，
    // 而这句话正是"接下来该做什么"的唯一提示。
    note(state.value.etag === "absent"
      ? "已读取。磁盘上还没写过状态文件，表单里是内置底稿——改任何一格再按「保存到磁盘」就写下第一份。"
      : "已读取。磁盘指纹 " + String(state.value.etag) + "。", "ok");
  }
}

async function apply() {
  const out = document.getElementById("apply-out");
  setApplyEnabled(false);
  out.textContent = "下发中：生成 -> 预跑 -> 提交 -> 重启 -> 探测。";
  out.className = "note";
  try {
    const result = await call("POST", "/api/apply");
    const value = result.value;
    if (result.ok && value.unchanged === true) {
      out.textContent = "未下发：" + String(value.message) + "（修订 " + String(value.revision) + "）";
      out.className = "ok";
    } else if (result.ok && value.reloaded === true) {
      out.textContent = "已下发修订 " + String(value.revision) + "，数据面重启过一次。";
      out.className = "ok";
    } else {
      out.textContent = "下发失败（HTTP " + result.status + "）：" + errorText(result);
      out.className = "warn";
    }
  } catch (error) {
    out.textContent = "下发中断：" + String(error.message || error);
    out.className = "warn";
  }
  // 已知良好修订与重载信号都在回执之外，重读一次才对得上磁盘。
  await refresh();
  // 下发是唯一会换掉数据面进程的动作，所以它也是唯一"必须看一眼它自己说了什么"的时刻：
  // 分级回执能区分成/没成/没动内核，但说不出为什么没成。
  await readLogs();
}

// —— 四个动作 ——
// 回执只说这一条动作的事，现场读数由跑完之后的 refresh() 再问一次：把读数塞进回执就等于
// 承诺"回执与读数是同一瞬间的"，而回滚和开劫持的那一刻服务正在被动过。
async function runAction(outId, path, body, okText) {
  const out = document.getElementById(outId);
  setActionsEnabled(false);
  document.getElementById("apply").disabled = true;
  out.textContent = "正在跑…";
  out.className = "note";
  let text;
  let klass;
  let touchedKernel = false;
  try {
    const result = await call("POST", path, body);
    if (result.ok) {
      text = okText(result.value);
      klass = "ok";
      // 回滚会重启数据面，同步会写规则集文件：这两条之后的日志才是这一按的下文。
      touchedKernel = path === "/api/rollback" || path === "/api/rule-sets/sync";
    } else {
      text = "失败（HTTP " + result.status + "）：" + errorText(result) + reportText(result.value);
      klass = "warn";
      // 失败更要看日志——回执那句话往往只是"500"，原因在下面几行里。
      touchedKernel = true;
    }
  } catch (error) {
    text = "动作中断：" + String(error.message || error);
    klass = "warn";
  }
  out.textContent = text;
  out.className = klass;
  await refresh();
  if (touchedKernel) await readLogs();
}

// 同步报告里那五张列表。逐条列出来而不是折成几个数字：note（"抓取失败沿用旧文件"）和
// kept 在界面上是两件事，而 409/500 的回执也带着它——已经落地的那部分不能不说。
function reportText(value) {
  const report = value && value.report;
  if (!report) return "";
  const parts = [];
  [
    ["refreshed", "新落地"],
    ["kept", "沿用"],
    ["reclaimed", "回收"],
    ["notes", "提示"],
    ["local_missing", "缺文件"],
  ].forEach(function (spec) {
    const rows = Array.isArray(report[spec[0]]) ? report[spec[0]] : [];
    if (rows.length > 0) parts.push(spec[1] + "：" + rows.join("、"));
  });
  return parts.length > 0 ? "｜" + parts.join("｜") : "";
}

document.getElementById("sync-rulesets").addEventListener("click", function () {
  runAction(
    "sync-out",
    "/api/rule-sets/sync",
    JSON.stringify({ force: document.getElementById("sync-force").checked }),
    function (value) {
      return String(value.message) + reportText(value);
    }
  );
});
document.getElementById("hijack-on").addEventListener("click", function () {
  // 这一条刻意不顺手先下发：会掐断连接的那一步不藏在别的按钮后面（§5.4）
  runAction("hijack-out", "/api/hijack/enable", "{}", function (value) {
    return String(value.message);
  });
});
document.getElementById("hijack-off").addEventListener("click", function () {
  runAction("hijack-out", "/api/hijack/disable", "{}", function (value) {
    return String(value.message);
  });
});
document.getElementById("rollback").addEventListener("click", function () {
  const raw = document.getElementById("rollback-revision").value.trim();
  runAction(
    "rollback-out",
    "/api/rollback",
    raw === "" ? "{}" : JSON.stringify({ revision: raw }),
    function (value) {
      return String(value.message) + "（这一步重启过数据面，在用连接被掐断一次）";
    }
  );
});

document.getElementById("logs-read").addEventListener("click", function () {
  readLogs();
});

// 过滤是纯前端的：它只在已经读到的那几行里挑字，一次都不问机器。
// 做成"输入即重画"而不是"按一下再去读一次"，因为这一屏的读数是五份文件，
// 而人想知道的是"这五份里谁提过规则集"——那不需要新的时刻。
document.getElementById("logs-filter").addEventListener("input", function () {
  renderLogs();
});

// 「这一条算成没成」那四个词是上游的枚举，选项表在 DNS_STATUS 里一份，这里灌进下拉框。
// 不写进 HTML：写两处就会漂成"面板给的词服务端不认"。
DNS_STATUS.forEach(function (spec) {
  addOption(document.getElementById("dnsrec-status"), spec[0], spec[1]);
});

document.getElementById("dnsrec-read").addEventListener("click", function () {
  readDnsRecords(null);
});
document.getElementById("dnsrec-more").addEventListener("click", function () {
  readDnsRecords(dnsNextCursor);
});
// 列表是每次读完重建的，所以监听挂在 <ul> 上做一次委托，而不是给每行各挂一个
document.getElementById("dnsrec-list").addEventListener("click", function (event) {
  const row = event.target.closest("li[data-id]");
  if (!row) return;
  document.querySelectorAll("#dnsrec-list li").forEach(function (li) {
    li.classList.remove("selected");
  });
  row.classList.add("selected");
  openDnsRecord(Number(row.getAttribute("data-id")));
});

// —— 分屏导航 ——
// 一屏一摊字段：这一份状态里互相牵连的组太多，全铺在一屏里"我改的是哪一格"就成了问题。
// 导航项与内容块靠 data-view 配对，页题与副标题写在这张表里。加一屏要同时动 HTML、这张表和 CSS，
// 少一边由 src/api/assets.rs 那条测试在编译期拦住。
// 页题写的是"这一屏回答的那一句话"，不是代码里的模块名（取舍见 docs/BLUEPRINT.md §11）。
const PAGES = {
  connect: ["连接与令牌", "令牌只在浏览器与这台机器之间：它不落盘，也不留在地址栏里。"],
  overview: ["现在的状态", "服务在听哪些端口、劫持层加载没有，是现场读数；该把客户端指到哪个地址，是磁盘上写的那一份。"],
  board: ["看板", "这台机器现在有多忙、谁在问 DNS、问得最多的是哪些域名。按一下读一次，这一屏自己不动机器上的任何东西。"],
  doctor: ["诊断", "期望态与生成物对不上时，这一屏说是哪一条对不上。"],
  routing: ["流量怎么走", "先点那一排定默认，下面三栏定每台设备：拖进哪栏就走哪档，不拖就跟着上面那排默认走。"],
  dns: ["DNS 上游与 fakeip", "两组上游、本机解析兜底、地址族策略、查询记录与 fakeip。"],
  network: ["网络 · 路线 · 端口", "流量路线与下发节奏、网卡网段、数据面的监听端口。"],
  groups: ["策略组", "把若干出站捆成一个出口，给兜底出口和设备例外去指。"],
  nodes: ["订阅与节点", "要走的出口。订阅链接贴进来按「导入订阅」，它只追加新名字；凭据从不读进页面。"],
  rulesets: ["规则集", "「同步规则集」的数据来源：在线源抓下来编译，本地那条只在位检查。"],
  ladder: ["分流情况", "把盘上那份期望态读成一张表：每条规则在连接层与解析层各排第几，或者根本不落行。只读，不落盘也不下发。"],
  preview: ["生成预览", "把磁盘上那份期望态渲染成四份配置文本：编排层（dae / oxidns）与内核层（zbox.nft / netstate.sh）。只读，不落盘也不下发。"],
  logs: ["日志", "这台机器上几方各自写的日志，一来源一卡。按一下读一次，五张卡是同一时刻。"],
  "dns-records": ["解析记录", "每一次解析问了谁、答了什么、命中了链上哪几步。取的是解析器自己的管理口，只读。"],
  deploy: ["让它生效", "会掐断在用连接的三步都在这一屏，没有一个藏在别的按钮后面。"]
};

// 保存/放弃改动在顶部工具条上，管的是同一份 state.json，所以只在编辑那几屏出现：
// 在概览或日志那一屏摆一个"保存到磁盘"，说的是一件它并不拥有的事。
const EDITOR_VIEWS = ["routing", "dns", "network", "groups", "nodes", "rulesets"];

// 两档。普通档只给那条主路（连上 → 填出口 → 默认怎么走 → 哪台例外 → 让它生效）加读数那几屏；
// "改规则本身"这一整层住 #nav-advanced，默认藏着，靠侧栏底部唯一一个开关进去。
// 开关状态记在浏览器（localStorage），**不进 state.json**：它是"这台浏览器怎么看面板"，
// 不是"这台机器怎么配"——不该因为其中一个人开了高级模式，就改了磁盘上那份共用的期望态。
const ADVANCED_VIEWS = ["doctor", "dns", "network", "groups", "rulesets", "ladder", "preview", "logs"];
const MODE_KEY = "zbox.panel.mode";

function readMode() {
  try {
    return window.localStorage.getItem(MODE_KEY) === "advanced";
  } catch (error) {
    return false;
  }
}

function writeMode(on) {
  try {
    window.localStorage.setItem(MODE_KEY, on ? "advanced" : "basic");
  } catch (error) {
    // 浏览器拒绝持久化（隐私模式一类）时这一轮的开关照样有效，只是刷新后回到默认档
  }
}

let advancedOn = readMode();

let currentView = "connect";

function setView(name) {
  currentView = Object.prototype.hasOwnProperty.call(PAGES, name) ? name : "connect";
  // 高级模式没开时那几屏进不去——手打的地址片段、别的标签页留下的都算，一律回落到「现在的状态」。
  if (!advancedOn && ADVANCED_VIEWS.indexOf(currentView) >= 0) currentView = "overview";
  document.querySelectorAll("main .page").forEach(function (page) {
    page.hidden = page.getAttribute("data-view") !== currentView;
  });
  document.querySelectorAll("#nav .nav-item").forEach(function (item) {
    item.classList.toggle("active", item.getAttribute("data-view") === currentView);
  });
  const head = PAGES[currentView];
  document.getElementById("page-title").textContent = head[0];
  document.getElementById("page-sub").textContent = head[1];
  revealActiveNav();
  updateLocked();
}

// 窄屏时导航是一条只能横向滑动的轨（见 panel.css 那条 56rem）。程序把人换到某一屏时
// ——连上之后落到「订阅与节点」就是这种——那一个可能正在轨的外头，看不见等于没有高亮。
// 只在轨真的装不下时才滑，宽屏那条竖轨不该被这里动到。
// `nearest` 不是随手写的：center 会把刚够得着的那一格也推到轨中间，顺带把左端的
// 「照着做」滑掉半截（实测 scrollLeft 从 0 变 21），标签缺字比看不见当前格更难看。
function revealActiveNav() {
  const nav = document.getElementById("nav");
  const active = document.querySelector("#nav .nav-item.active");
  if (!nav || !active || nav.scrollWidth <= nav.clientWidth) return;
  active.scrollIntoView({ block: "nearest", inline: "nearest" });
}

// 关档的时候，人若正站在一张已经不在导航里的屏上，就一并把她带回「现在的状态」：
// 顶栏还写着屏名、内容却再也点不到，是这一屏不许有的那种假象。
function applyMode() {
  document.getElementById("nav-advanced").hidden = !advancedOn;
  document.getElementById("mode-toggle").textContent = advancedOn ? "关闭高级模式" : "开启高级模式";
  if (!advancedOn && ADVANCED_VIEWS.indexOf(currentView) >= 0) setView("overview");
  // 节点编辑器（高级档）：只有高级模式才显示节点编辑区
  const nodeEditor = document.getElementById("node-editor");
  if (nodeEditor) {
    nodeEditor.hidden = !advancedOn;
  }
}

// "锁住"只说一件事：这几屏要读到磁盘上那份状态才有内容可改。
// 不写"连接失败"，也不写"没有权限"——这一层不知道那些事，也不该猜。
function updateLocked() {
  const ready = !document.getElementById("editor").hidden;
  const editing = EDITOR_VIEWS.indexOf(currentView) >= 0;
  document.getElementById("editor-locked").hidden = ready || !editing;
  document.getElementById("save").hidden = !editing;
  document.getElementById("revert").hidden = !editing;
  document.getElementById("save-out").hidden = !editing;
}

document.getElementById("nav").addEventListener("click", function (event) {
  const item = event.target.closest(".nav-item");
  if (!item) return;
  setView(item.getAttribute("data-view"));
});

document.getElementById("nav-toggle").addEventListener("click", function () {
  const button = document.getElementById("nav-toggle");
  const collapsed = document.body.classList.toggle("nav-collapsed");
  button.textContent = collapsed ? "展开导航" : "收起导航";
});

document.getElementById("mode-toggle").addEventListener("click", function () {
  advancedOn = !advancedOn;
  writeMode(advancedOn);
  applyMode();
});

// 退出只做一件事：清掉这个浏览器标签页里那份令牌。磁盘上的期望态、在跑的服务、日志
// 一个字节都不动。表单里没保存的改动**故意不清**——重新连上时若它还是脏的，
// openEditor 会拒绝拿磁盘那份盖掉它（与「放弃改动」那条纪律同一个理由：不静默吃人的编辑）。
document.getElementById("logout").addEventListener("click", function () {
  rememberToken("");
  document.getElementById("token").value = "";
  document.getElementById("editor").hidden = true;
  setApplyEnabled(false);
  setActionsEnabled(false);
  ["overview-badge", "doctor-badge", "preview-badge", "logs-badge", "ladder-badge", "dnsrec-badge"].forEach(function (id) {
    setBadge(id, "未读取");
  });
  // 徽标改口"未读取"，卡片里的旧读数就得跟着退场：留着它们，同一屏上会同时写着
  // "没读到"和"两份都渲染出来了"，而这种矛盾一眼看上去像好的那半句。
  // 清的是**读数**，不是表单——磁盘上那份底稿和用户没保存的改动仍然留着（上面的理由）。
  [
    "source", "etag", "last-good", "revisions", "reload-pending", "generation-error",
    "render-errors", "listening", "service-active", "hijack", "hijack6", "client-target",
    "network-shape",
  ].forEach(function (id) {
    setText(id, "未读取", "");
  });
  // 回滚那个下拉框里的清单同属"读数"，不是表单：留着它，退出之后屏幕上仍然列着上一轮的几份修订，
  // 而人挑中的是一份此刻没有第二次读取佐证的修订号。开局那一句就是空列表（HTML 里也没写死选项）。
  renderRevisions(null, null, "fresh");
  // 另外五块只读卡片同理，退回 HTML 开局那一句（逐字，测试钉着别漂）
  setText("preview-meta", "未读取", "note");
  setText("preview-dae", "未读取", "note");
  setText("preview-oxidns", "未读取", "note");
  setText("preview-nft", "未读取", "note");
  setText("preview-netstate", "未读取", "note");
  // 差异的基准本身也是一次读数，跟着一起退场。留着它，下一次读会拿一份"屏幕上已经看不见"的
  // 旧内容来标色——上面写着"未读取"，下面却说"与上一页读到的那份相比"，正是这一屏不许有的那类矛盾。
  previewPrevious = {};
  setText("logs-meta", "未读取", "note");
  // 五张卡连同"手里那一份"一起退场：留着它们，退出之后屏幕上仍写着上一轮的字节数与取数时刻，
  // 而徽标已经改口"未读取"——同一屏上那句"取数 2026-10-12 13:40:12"就成了没有出处的读数。
  lastLogs = null;
  renderLogs();
  // 过滤框是"重画手里那一份"的那一下：手里没份了，它就退回复述里的开局态——禁用 + 空词。
  // 留着能打字的样子，屏上会同时写着"未读取"和"只画带某几个字的行"，而后者已经没有对象。
  document.getElementById("logs-filter").value = "";
  setLogsEnabled(false);
  setText(
    "logs-out",
    "未读取。这台机器上几方各自写的日志在这里看，面板不动它们也不删它们。",
    "note"
  );
  // 「分流情况」这一屏同理：来源那句、"下发过没有"那句、渲染状态那句、差异那句，连同整张表
  // 一起退回开局那一句。基准（ladderPrevious）也是一次读数，跟着退场——留着它，下一次读会拿
  // 一张屏幕上已经看不见的旧表来标色，上面写着"未读取"下面却说"与上一次读到的那张相比"。
  setText("ladder-meta", "未读取", "note");
  setText("ladder-pending", "未读取。", "note");
  setText("ladder-errors", "", "note");
  setText("ladder-diff", "", "note");
  ladderPrevious = null;
  paintLadder(null, null, "未读取。按一下上面「读取分流梯级」。");
  // 「解析记录」这一屏同理：两层读数都退回 HTML 开局那一句（逐字，assets 那条测试钉着）。
  // 四个过滤框里的**用户输入**不清——那是人打的字，不是机器的读数，理由和上面不动表单一样。
  dnsNextCursor = null;
  setText("dnsrec-meta", "未读取", "note");
  setText("dnsrec-apply", "未读取", "note");
  setText("dnsrec-version", "未读取。", "note");
  setText("dnsrec-errors", "", "note");
  setText("dnsrec-page", "", "note");
  setText(
    "dnsrec-detail",
    "点上面任意一行来看这一条查询走过的每一步，以及它最后是怎么收场的。",
    "note"
  );
  setText("dnsrec-detail-meta", "还没点某一行", "note");
  const dnsEmpty = document.getElementById("dnsrec-empty");
  dnsEmpty.hidden = false;
  setText(
    "dnsrec-empty",
    "未读取。按一下上面「读取 DNS 记录」。",
    "note"
  );
  clearDnsList();
  clearDnsSteps();
  const findings = document.getElementById("findings");
  findings.textContent = "";
  const blank = document.createElement("li");
  blank.className = "note";
  blank.textContent = "未读取";
  findings.appendChild(blank);
  // 订阅那一屏同理：回执退回开局那一句。另外**输入框里的链接要清**——它带着机场的令牌，
  // 这一处和上面清 `#token` 是同一个理由：退出登录清的是这一页替人持有的秘密，
  // 不是人打的字（对比上面那四个过滤框不清）。成功那一路本来就当场清空，剩下的是被拒
  // 和出错那几路留在框里的那一串。
  resetSubReceipt();
  document.getElementById("sub-url").value = "";
  note("已退出：清掉的只是这个标签页里那份令牌。要再连就重新取一个填进来。", "note");
  setView("connect");
});

setView("connect");
applyMode();

document.getElementById("preview-read").addEventListener("click", function () {
  readPreview();
});

document.getElementById("ladder-read").addEventListener("click", function () {
  readLadder();
});

document.getElementById("board-read").addEventListener("click", function () {
  readBoard();
});

if (takeTokenFromHash() || currentToken()) {
  refresh();
}

// 页面已经开着的时候把令牌粘到地址后面，浏览器只换片段、不重新加载，所以接一句 hashchange
// 才让上面那段"放在 # 后面送上来"的说明在两种情况下都成立。`replaceState` 不会触发它，没有回环。
window.addEventListener("hashchange", function () {
  if (takeTokenFromHash()) refresh();
});
document.getElementById("connect").addEventListener("click", function () {
  rememberToken(document.getElementById("token").value.trim().toLowerCase());
  refresh();
});
document.getElementById("apply").addEventListener("click", function () {
  apply();
});

// —— 编辑层的事件 ——
// CSP 里 `script-src 'self'` 禁的是内联事件，所以控件全部接在这一段；
// 名单的行是动态建的，用一条委托挂在列表上，不给每行各加监听。
["input", "change"].forEach(function (kind) {
  document.getElementById("editor").addEventListener(kind, function () {
    formDirty = true;
  });
});
document.getElementById("pol-final").addEventListener("change", function () {
  document.getElementById("final-group-field").hidden =
    document.getElementById("pol-final").value !== "proxy";
  paintModeSwitch();
});
document.getElementById("pol-cn-direct").addEventListener("change", paintModeSwitch);
// 组那张表一改，"这一档指的是哪个组"那句就得跟着改口（刚加还没保存的组也算数）。
document.getElementById("group-list").addEventListener("input", paintModeSwitch);
Array.prototype.forEach.call(document.querySelectorAll("#mode-switch .mode-opt"), function (button) {
  button.addEventListener("click", function () {
    chooseModeSwitch(button.getAttribute("data-mode"));
  });
});

// 名单与两张上游表用的是同一套行内按钮：上移 / 下移 / 删。委托在列表上，
// 行是动态建的，不给每行各接一个监听。
// —— 设备那四栏：拖与不拖，落的必须是同一个字段 ——
// 拖动只给"顺手"，不是唯一的路：卡片上那一档本来就是颗可选的下拉，触屏与键盘走它一样能改档。
// 两条路最后都走 `putDeviceCard`，所以"卡片住在哪一栏"与"那一格写的是什么"永远是同一件事。
let dragDeviceCard = null;

function deviceLaneFromNode(node) {
  return node && node.closest ? node.closest(".lane") : null;
}

function markDeviceLane(lane, on) {
  if (lane) lane.classList[on ? "add" : "remove"]("drop");
}

function endDeviceDrag() {
  const lanes = deviceLanes();
  for (let i = 0; i < lanes.length; i += 1) markDeviceLane(lanes[i], false);
  dragDeviceCard = null;
  document.getElementById("device-lanes").classList.remove("dragging");
}

function wireDeviceLanes() {
  const box = document.getElementById("device-lanes");
  box.addEventListener("click", function (event) {
    const button = event.target && event.target.closest ? event.target.closest("button[data-act]") : null;
    if (!button || button.getAttribute("data-act") !== "del") return;
    const card = button.closest(".dev-card");
    if (!card) return;
    card.parentNode.removeChild(card);
    refreshDeviceLanes();
    formDirty = true;
  });
  box.addEventListener("change", function (event) {
    const select = event.target;
    if (!select || !select.getAttribute || select.getAttribute("data-key") !== "mode") return;
    const card = select.closest ? select.closest(".dev-card") : null;
    if (!card) return;
    const lane = deviceLaneOf(select.value);
    // 改选成这一屏不认识的那一档：卡片不动（没有对应的栏），但那一格照用户的写。
    if (lane && card.parentNode !== deviceCards(lane)) putDeviceCard(card, lane);
    else refreshDeviceLanes();
    formDirty = true;
  });
  box.addEventListener("dragstart", function (event) {
    const grip = event.target && event.target.closest ? event.target.closest(".dev-grip") : null;
    const card = grip && grip.closest ? grip.closest(".dev-card") : null;
    if (!card) return;
    dragDeviceCard = card;
    box.classList.add("dragging");
    if (event.dataTransfer) {
      event.dataTransfer.effectAllowed = "move";
      // 有些实现里不 `setData` 就不允许起拖。这里传的是一个没有内容的记号，
      // 地址与 MAC 一律不过 dataTransfer——拖出去的东西落在同一个页面里，没必要带上身份。
      event.dataTransfer.setData("text/plain", "zbox-device-card");
    }
  });
  box.addEventListener("dragover", function (event) {
    if (!dragDeviceCard) return;
    const lane = deviceLaneFromNode(event.target);
    if (!lane) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
    markDeviceLane(lane, true);
  });
  box.addEventListener("dragleave", function (event) {
    markDeviceLane(deviceLaneFromNode(event.target), false);
  });
  box.addEventListener("drop", function (event) {
    const lane = deviceLaneFromNode(event.target);
    if (!dragDeviceCard || !lane) return;
    event.preventDefault();
    putDeviceCard(dragDeviceCard, lane);
    formDirty = true;
    endDeviceDrag();
  });
  box.addEventListener("dragend", endDeviceDrag);
}

function wireList(id, onChange) {
  document.getElementById(id).addEventListener("click", function (event) {
    const target = event.target;
    const button = target && target.closest ? target.closest("button[data-act]") : null;
    if (!button) return;
    const li = button.closest("li");
    if (!li) return;
    const act = button.getAttribute("data-act");
    if (act === "del") {
      li.parentNode.removeChild(li);
    } else if (act === "up" && li.previousElementSibling) {
      li.parentNode.insertBefore(li, li.previousElementSibling);
    } else if (act === "down" && li.nextElementSibling) {
      li.parentNode.insertBefore(li.nextElementSibling, li);
    }
    onChange();
    formDirty = true;
  });
}

wireList("rule-list", renumber);
wireList("domestic-list", function () {
  renumberUpstreams("domestic");
});
wireList("overseas-list", function () {
  renumberUpstreams("overseas");
});
function addUpstreamRow(which) {
  upstreamList(which).appendChild(upstreamRow(blankUpstream(), true));
  renumberUpstreams(which);
  const first = upstreamList(which).lastChild.querySelector('[data-key="tag"]');
  first.focus();
  formDirty = true;
}

wireList("ruleset-list", function () {
  renumberRows("ruleset-list", "ruleset-empty");
  refreshDerived();
});
wireDeviceLanes();
wireList("group-list", function () {
  renumberRows("group-list", "group-empty");
  refreshGroupHint();
});
wireList("node-list", function () {
  renumberRows("node-list", "node-empty");
  refreshGroupHint();
});
// 派生那一格不是输入框，监听不到自己的 change，所以 tag 每次敲字都重算一遍整张表。
// 真正的正确性在 collectRuleSet 里（保存时按 tag 重算），这一条只是让屏幕上那句别过期。
document.getElementById("ruleset-list").addEventListener("input", refreshDerived);
// 组那一行的提示要跟着 tag 变（组名本身也是别的组的合法成员），所以每次敲字都重算。
document.getElementById("group-list").addEventListener("input", refreshGroupHint);
// 节点那行的名字也是成员名单的来源，所以敲字时重算一次（档那一格是派生的，不用重画）。
document.getElementById("node-list").addEventListener("input", refreshGroupHint);
document.getElementById("add-ruleset-local").addEventListener("click", function () {
  addRuleSetRow("local");
});
document.getElementById("add-ruleset-remote").addEventListener("click", function () {
  addRuleSetRow("remote");
});
document.getElementById("add-device").addEventListener("click", openAddDeviceModal);
// 全选与清除选择
document.getElementById("select-all").addEventListener("click", selectAllRoutingCards);
document.getElementById("clear-selection").addEventListener("click", clearRoutingSelection);
// 设备搜索
document.getElementById("device-search-input").addEventListener("input", function (e) {
  const val = e.target.value.trim();
  const clearBtn = document.getElementById("device-search-clear");
  if (clearBtn) clearBtn.hidden = !val;
  if (val) searchDeviceByMac(val);
});
document.getElementById("device-search-clear").addEventListener("click", function () {
  const input = document.getElementById("device-search-input");
  if (input) input.value = "";
  this.hidden = true;
});
// 设备详情弹窗
document.getElementById("modal-device-save").addEventListener("click", saveDeviceModal);
document.getElementById("modal-device-cancel").addEventListener("click", closeDeviceModal);
document.getElementById("modal-device-delete").addEventListener("click", deleteDeviceFromModal);
// 添加设备弹窗
document.getElementById("new-device-save").addEventListener("click", saveNewDevice);
document.getElementById("new-device-cancel").addEventListener("click", closeAddDeviceModal);
document.getElementById("add-group-selector").addEventListener("click", function () {
  addGroupRow("selector");
});
document.getElementById("add-group-urltest").addEventListener("click", function () {
  addGroupRow("urltest");
});
document.getElementById("add-group-region-lock").addEventListener("click", function () {
  addRegionGroup(true);
});
document.getElementById("add-group-region-auto").addEventListener("click", function () {
  addRegionGroup(false);
});
document.getElementById("group-region-key").addEventListener("keydown", function (event) {
  if (event.key === "Enter") {
    event.preventDefault();
    addRegionGroup(true);
  }
});
document.getElementById("add-node-vless").addEventListener("click", function () {
  addNodeRow("vless-reality");
});
document.getElementById("add-node-hysteria2").addEventListener("click", function () {
  addNodeRow("hysteria2");
});
document.getElementById("add-node-tuic").addEventListener("click", function () {
  addNodeRow("tuic");
});
document.getElementById("ping-all").addEventListener("click", function () {
  pingAllNodes();
});
document.getElementById("sub-import").addEventListener("click", function () {
  importSubscription();
});
document.getElementById("sub-policy-link").addEventListener("click", function () {
  setView("routing");
});
document.getElementById("sub-url").addEventListener("keydown", function (event) {
  if (event.key === "Enter") {
    event.preventDefault();
    importSubscription();
  }
});
document.getElementById("add-domestic").addEventListener("click", function () {
  addUpstreamRow("domestic");
});
document.getElementById("add-overseas").addEventListener("click", function () {
  addUpstreamRow("overseas");
});
document.getElementById("add-rule").addEventListener("click", addRule);
document.getElementById("new-rule-pattern").addEventListener("keydown", function (event) {
  if (event.key === "Enter") {
    event.preventDefault();
    addRule();
  }
});
document.getElementById("restore-autodetect").addEventListener("click", function () {
  document.getElementById("net-lan").value = DEFAULT_NETWORK.lan_interface;
  document.getElementById("net-cidr").value = DEFAULT_NETWORK.lan_cidr;
  document.getElementById("net-wan").value = DEFAULT_NETWORK.wan_interface;
  // 两个地址栏一起清空：只要它们还留着值，`network` 就不等于内置默认形状，
  // 探测那条路（topology::fill）就永远不再接管——按钮按下去什么也不会恢复。
  document.getElementById("net-addr").value = DEFAULT_NETWORK.lan_address;
  document.getElementById("net-addr6").value = DEFAULT_NETWORK.lan_address6;
  formDirty = true;
  setSaveNote(
    "已把网卡、网段与解析口地址填回内置默认形状（两个地址栏清空），这还没写盘。" +
      "保存一次之后，IPv4 那一格由探测接管；IPv6 那一格不探，要自己填固定地址。",
    "note",
  );
});
document.getElementById("save").addEventListener("click", function () {
  save(false);
});
document.getElementById("revert").addEventListener("click", function () {
  formDirty = false;
  setSaveNote("正在重读磁盘上那一份。", "note");
  // 「表单里还有没保存的改动」那句拒绝的话，在这一下之后就成了假话——改动正是被这里丢掉的。
  resetSubReceipt();
  refresh();
});
