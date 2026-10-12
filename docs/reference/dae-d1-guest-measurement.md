# dae D1 实测记录（guest 真机，非文档推断）

测量时间：guest 时钟 2026-10-01 20:18–20:21 +0800（宿主 2026-10-09）。
环境：Alpine 3.21.3 / kernel `6.12.111-0-lts` / dae `v2.1.1`（go1.26.8-X，linux/amd64，非 musl 静态包但可直接跑）/
VMware NAT(VMnet8)，guest 127 号地址，单网卡 eth0。被测配置 `/root/dae-probe/probe.dae`，只绑 dummy 网卡 `dae-probe`，
`auto_config_kernel_parameter: false`、`auto_config_firewall_rule: false`、`disable_waiting_network: true`、空 `routing{}`。
纪律：全程不绑 eth0、不写 /etc/fstab、不 rc-update、sysctl 只动 dae-probe 这一张卡。

## 一、生死门：eBPF 能加载

```
INFO Loading eBPF programs and maps into the kernel...
INFO The loading process takes about 120MB free memory, which will be released after loading.
INFO Loaded eBPF programs and maps
INFO Loading routing rules into kernel space (BPF)...
INFO Routing match set len: 1/1024
INFO Control plane built in 1.046557285s
```

**结论：本机内核可以加载 dae 的 BPF 程序与映射，分流规则也能进内核态。**此前从内核配置推出来的
担忧被推翻——`# CONFIG_BPF_STREAM_PARSER is not set` **不构成阻塞**（它挡住的是 Go 的 sockmap/sk_msg
路径，不是 tc 钩子）。cgroup2 与 bpffs 需要挂载，但挂载本身 trivial：

| 前置 | 实测 |
| --- | --- |
| `mount -t cgroup2 none /sys/fs/cgroup` | rc=0；controllers = `cpuset cpu io memory hugetlb pids` |
| `mount -t bpf none /sys/fs/bpf` | rc=0；`/proc/filesystems` 有 `nodev bpf`；pin 目录 `/sys/fs/bpf/dae` 由 dae 自建 |
| 内存 | 加载时 free=110 MiB / available=441 MiB，120 MB 的宣称需求实测通过（页缓存可回收） |

## 二、被推翻的假设：netkit 在本机不可用

```
INFO Kernel v6.12.111 supports Netkit, attempting to create Netkit device pair
WARN Failed to create Netkit device, falling back to veth error=... exit status 2: Error: Unknown device type.
INFO Created veth device pair (compatibility mode; Netkit was attempted but failed)
```

dae 按**内核版本号**判定 Netkit 可用（6.12 号段够新），但 Alpine `linux-lts` 里
`grep CONFIG_NETKIT /proc/config.gz` **无输出**（未编译），`ip link add type netkit` 报 Unknown device type。
所以生产路径是 **veth 兼容模式**。`docs` 里"Netkit 对 + 内核带 CVE-2025-37959 修复则用
`bpf_redirect_peer`、跳过 MAC 改写"那条分支在本机不生效——写生成器与排查逻辑时按 veth 算。

## 三、`auto_config_kernel_parameter: false` 时 ZBox 必须自己补的内核参数

dae 逐个接口检查，一次只报一个，全部为 FATAL 且退出：

| 顺序 | 检查项 | 需要的值 | 原错文 |
| --- | --- | --- | --- |
| 1 | `net.ipv6.conf.<if>.forwarding` | `1` | `ipforward on dae-probe is off` |
| 1' | `net.ipv4.conf.<if>.forwarding` | 也要求为 1（本机本来就 1） | 同上族 |
| 2 | `net.ipv4.conf.<if>.send_redirects` | `0` | `send_redirects on dae-probe is on` |

注意两点：`net.ipv6.conf.<if>.send_redirects` 这个键在本机**不存在**（`sysctl: error: ... is an unknown key`），
别写进生成器；检查是**按接口**的，`all` 的值不影响判定（实测 `net.ipv6.conf.all.forwarding=0` 时只要本卡为 1 即通过）。
对照参考：`auto_config_kernel_parameter: true` 时 dae 会自己写这些，但它同时会动全局转发，与现役 ZBox 用
`/etc/sysctl.d/` 文件管理的语义冲突，所以生成器该走"我们自己写 per-if 值 + 保持 false"。

⚠ 这三条**与 `network.ipv6_mode` 的三档无关**：2026-10-11 又一轮（`ipv6-kernel-measured.md` §10）
在只写 per-iface 键、不碰 `all.forwarding` 的组合下复现了同一串 FATAL，也就是说
`off` / `dns_only` 档下 dae 照样要这三条。`gen::netstate.rs` 现在按档位写键，
这一条是不是漏了 dae 那一半，是 #69 之前要先核的。

同轮还测到 dae 的**监听族与命名空间归属**（这一条决定劫持层的形状）：它自建 netns `daens`，
tproxy 口 `12345` 在里面是 `tcp LISTEN 0.0.0.0:12345` **加** `tcp LISTEN [::]:12345`
加 `udp UNCONN *:12345`，而宿主 ns `ss -lntu | grep -c 12345` = **0**。
两条后果：dae 自己就绑 v6，不需要我们替它补口；但宿主 ns 里那条 `tproxy to :<port>` 对它
**必不命中**，按 `ipv6-kernel-measured.md` §8 那是静默泄漏而不是黑洞。

## 四、配置文件权限是 run 期硬失败，而 `validate` 照样返回 0

```
$ dae validate -c probe.dae   -> rc=0   （0644 时只打印一行权限告警）
$ dae run     -c probe.dae    -> rc=1   FATAL Failed to read config
    err="permissions 0644 for '.../probe.dae' are too open; requires the file is NOT writable by
         the same group and NOT accessible by others; suggest 0640 or 0600"
```

**这直接给"能力探测 + validate 两道门"补了第三道**：`validate` 不是充分门禁，与当年 sing-box
`check` 返回 0 而 `run` FATAL 是同一类坑。ZBox 落盘 `.dae` 必须 `0600`（或 0640 + 属组），
并把"权限已检查"作为下发前置步骤，而不是等 run 起来才炸。

## 五、清理与崩溃语义（实测）

- 启动第一句是 `INFO purging stale TC filters before startup`——它会清自己遗留的 filter。
  **是否只清自己创建的，本轮无法判定**（eth0 上只有默认 `pfifo_fast`，没有别的 filter），标未核实。
- FATAL 退出后没有遗留 veth、netns、tc filter；`/sys/fs/bpf/dae` 空目录会留（需我们删）。
- TERM 正常退出走的是：`Received termination signal: terminated` → `BPF: Detaching owned hooks` →
  `Owned hooks detached` → `Shutdown: Fast exit enabled; skipping in-process control-plane teardown.
  Residual connection state will be purged on next startup.`
  即**快退出**：钩子摘除，但进程内连接状态靠下次启动清理，不是当场清零。
- `so_mark_from_dae` 未设时：`WARN ... using internal socket mark 0x100 to prevent dae UDP self-capture`。
  这条要进 ZBox 的 mark 规划表：内部 socket mark `0x100`、skb/cb mark `0x8000000`、策略表 2023（文档值，未实测）。

## 六、现场还原与副作用核查（逐项复核，不信脚本退出码）

拆场按逆序：删 pin 目录 → umount bpffs → umount cgroup2 → 删 dummy 网卡。复核结果全部回到基线：

`/proc/mounts` 无 cgroup/bpf 行、`/sys/fs/cgroup` 与 `/sys/fs/bpf` 均空、网卡只剩 lo+eth0、
`tc qdisc show dev eth0` 仍是 `pfifo_fast root`、无 netns、`ip -4 rule` 只有 `0:/32766:/32767:`、
`ip -6 rule` 只有 `0:/32766:`、`nft list tables` 空、默认路由仍是 `via 192.168.219.2 dev eth0 metric 202`。

现役服务未受影响：`rc-service zbox status` = started，sing-box pid 16980 仍在，监听口与基线逐条一致
（tcp/udp 53、9887、9888、7890、8888、22）。sing-box 的 DNS 实测正常：向 127.0.0.1 查 `baidu.com`
125ms 返回 4 条 A，日志有对应 `inbound/direct[dns-in]` 与 `dns: exchanged` 行。
`google.com` 查询超时**不是回归**，是本拓扑固有属性（劫持层关闭时境外域经 VMnet8 本就不通）。
另：sing-box 在 20:18:13 记了一次 `updated network environment: gateway 192.168.219.2`，
是 dummy 网卡增删触发的重探测，网关识别结果正确。

## 七、还没测（D1 没覆盖的）

D1 只证明"能加载 + 能绑定 + 能干净退出"，**没有验证任何数据面行为**：

1. cgroupv2 的进程名钩子是否真能取到 pname（`pname(...)` 规则 + 防 DNS 环依赖它）；
2. tproxy 命中与回滚、`dae0` veth pair 的 redirect 路径；
3. DNS 响应截获做域名归属的两条自认限制（共享 IP 误判、浏览器 DNS 缓存）→ 任务 #43；
4. `dae reload` 的保连接语义与失败后果、suspend 的实际状态；
5. 与现役 sing-box 服务共存的冲突面（本轮刻意错峰，未同跑）。
