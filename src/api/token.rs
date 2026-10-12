//! 面板的访问令牌：机器生成、定宽十六进制、落盘 0600。
//!
//! 这一档是 2026-10-06 在"argon2 口令 + cookie"与"机器生成高熵令牌"之间选定的后者，理由写在
//! `docs/ARCHITECTURE.md` §7.2 第 3 条：它**没有登录端点**，于是也没有在线猜口令的面，
//! 不需要限速、口令强度策略和"忘了口令怎么办"的第三条路；而且本地 cargo 缓存里一个摘要/随机
//! 类 crate 都没有，选它就不必为了登录去破坏 `--offline` 构建。
//!
//! 三条不是显然的实现约定：
//! - **每一个端点都要过这道门，包括只读的**。`GET /api/state` 返回的是磁盘形状，里面带着
//!   节点凭据（UUID / password / Reality 密钥）与规则集源 URL；未鉴权的读面就是凭据泄露面。
//! - **令牌只落文件，不进日志**。`zboxd serve` 不许把它打印到 stdout——OpenRC 的
//!   `supervise-daemon` 会把 stdout 收进服务日志，那就是把密钥写进一个默认可读的文件。
//!   要人看一眼时走显式命令（`zboxd panel-token`）。
//! - **随机源是可注入的**，不是"生产代码里藏一个测试能用的分支"：Unix 上真随机来自
//!   `/dev/urandom`，非 Unix（开发机）上生成路径直接报错，而鉴权逻辑本身两个平台都要跑单测。

use std::fmt;
use std::path::Path;

/// 令牌的随机字节数。16 字节 = 128 位：没有登录端点时，猜中一次等于拿到面板全部读写，
/// 所以熵要按"离线也猜不动"来给，而不是按"会话 id"来给。
pub const TOKEN_BYTES: usize = 16;

/// 磁盘形状：`TOKEN_BYTES * 2` 个小写十六进制字符。
pub const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;

/// 一份面板访问令牌。
///
/// `Debug` 是手写的：这一层的 `Request`/`Response` 常被 `assert!` 的 `{got:?}` 打进测试输出，
/// 顺带把令牌打出来是最容易发生的泄露，所以从类型上堵掉。
pub struct PanelToken {
    secret: String,
}

impl fmt::Debug for PanelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PanelToken(<已隐去 {TOKEN_HEX_LEN} 位令牌>)")
    }
}

impl PanelToken {
    /// 读现成的；没有就生成一份写下去。
    ///
    /// "没有就生成"是幂等的：面板重启、服务重启都拿同一份，用户不必重新粘贴。要换发是显式动作
    /// （删掉那个文件再启动），不是每次开机悄悄换——悄悄换发会让用户手里的书签式令牌变成
    /// 一个"时不时就打不开"的面板。
    pub fn load_or_create<F>(path: &Path, mut fill: F) -> Result<Self, String>
    where
        F: FnMut(&mut [u8]) -> Result<(), String>,
    {
        if path.is_file() {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("读取面板令牌 {} 失败: {e}", path.display()))?;
            return Self::new(text.trim()).map_err(|e| format!("{}: {e}", path.display()));
        }
        let mut buf = [0u8; TOKEN_BYTES];
        fill(&mut buf)?;
        let secret = hex(&buf);
        // 与状态文件同一套落盘纪律：0600 在 create 就给到、不留临时文件（`state::write_private`）
        crate::model::state::write_private(path, secret.as_bytes())?;
        Ok(Self { secret })
    }

    /// 由已知字符串构成（测试、以及以后 `--token` 那条显式覆盖的路径）。形状不对就拒。
    pub fn new(secret: &str) -> Result<Self, String> {
        if secret.len() != TOKEN_HEX_LEN {
            return Err(format!(
                "面板令牌应是 {TOKEN_HEX_LEN} 个十六进制字符（{TOKEN_BYTES} 字节），实际 {} 个字符",
                secret.len()
            ));
        }
        if !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("面板令牌只允许十六进制字符".to_string());
        }
        if !secret
            .bytes()
            .all(|b| !(b.is_ascii_hexdigit() && b.is_ascii_uppercase()))
        {
            // 大小写混着放会让"复制时手抖开了 CapsLock"变成一次难查的 401
            return Err("面板令牌应是小写十六进制".to_string());
        }
        Ok(Self {
            secret: secret.to_string(),
        })
    }

    /// 打印用的原值。调用方有责任只把它写到 stdout（交互式命令），不许写进任何日志文件。
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// 常数时间比较。请求里没带（`None`）直接不通过，不做"空令牌等于默认令牌"那种善意猜测。
    pub fn accepts(&self, offered: Option<&str>) -> bool {
        match offered {
            Some(candidate) => equal(self.secret.as_bytes(), candidate.as_bytes()),
            None => false,
        }
    }
}

/// 定宽输入才允许先比长度：这里两边都必须是 `TOKEN_HEX_LEN`，长度不是秘密。
fn equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Unix 上的随机源。`/dev/urandom` 在 Linux 上初始化完成后就不会阻塞，比 `/dev/random`
/// 更适合"开机第一次起面板"这个场景——后者在路由器上能等很久。
#[cfg(unix)]
pub fn urandom(buf: &mut [u8]) -> Result<(), String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open("/dev/urandom").map_err(|e| format!("打开 /dev/urandom 失败: {e}"))?;
    file.read_exact(buf)
        .map_err(|e| format!("从 /dev/urandom 读 {} 字节失败: {e}", buf.len()))
}

/// 非 Unix（开发机）没有这条路。**不做"那就用时钟凑一份"**：那等于把生产的鉴权强度
/// 换成一个开发机上才成立的弱实现，而这份代码以后是会被拿去 Linux 上跑的。
#[cfg(not(unix))]
pub fn urandom(_buf: &mut [u8]) -> Result<(), String> {
    Err("这台机器上没有 /dev/urandom，生成不了面板令牌；面板只在 Linux 目标上提供服务".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

    /// 每个用例一个私有目录，令牌文件叫 `panel.token`——断言"目录里只有它"时不能拿 `/tmp`
    /// 当父目录，那会把别人家的文件也算进来。
    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("zbox-token-{name}"))
            .join("panel.token")
    }

    /// 假的随机源：形状与生产一致（填满整段），内容按调用方给的字节来，好断言"存的就是它"。
    fn canned(bytes: &[u8]) -> impl FnMut(&mut [u8]) -> Result<(), String> {
        let mut seed = bytes.to_vec();
        move |buf: &mut [u8]| {
            for (i, slot) in buf.iter_mut().enumerate() {
                *slot = seed[i % seed.len()];
            }
            seed.push(0);
            Ok(())
        }
    }

    #[test]
    fn a_missing_token_file_is_generated_once_and_kept_afterwards() {
        let path = temp_path("create");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let first = PanelToken::load_or_create(&path, canned(b"\x11\x22\x33\x44")).unwrap();
        assert_eq!(first.secret(), "11223344112233441122334411223344");
        // 服务重启走同一条路：文件已存在就不该换发
        let again = PanelToken::load_or_create(&path, canned(b"\xee\xee\xee\xee")).unwrap();
        assert_eq!(
            again.secret(),
            first.secret(),
            "悄悄换发等于把用户的令牌作废"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "令牌文件同状态文件一样私密：{:o}",
                mode
            );
        }
        let leftovers: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["panel.token".to_string()], "不许留临时文件");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_handmade_token_must_be_lowercase_fixed_width_hex() {
        assert!(PanelToken::new(GOOD).is_ok());
        for bad in [
            GOOD.repeat(2),
            GOOD[..GOOD.len() - 1].to_string(),
            GOOD.to_uppercase(),
            format!("{}g", &GOOD[..GOOD.len() - 1]),
            String::new(),
        ] {
            assert!(PanelToken::new(&bad).is_err(), "不该收下: {bad}");
        }
    }

    #[test]
    fn only_the_exact_token_passes_and_absence_is_not_a_default() {
        let token = PanelToken::new(GOOD).unwrap();
        assert!(token.accepts(Some(GOOD)));
        assert!(!token.accepts(None), "没带头就是没鉴权，不拿默认值兜底");
        assert!(!token.accepts(Some("")));
        assert!(!token.accepts(Some(&GOOD.to_uppercase())));
        // 只差最后一位
        let mut off_by_one = GOOD.as_bytes().to_vec();
        let last = off_by_one.len() - 1;
        off_by_one[last] = if off_by_one[last] == b'0' { b'1' } else { b'0' };
        assert!(!token.accepts(Some(std::str::from_utf8(&off_by_one).unwrap())));
    }

    #[test]
    fn debug_output_does_not_contain_the_secret() {
        let token = PanelToken::new(GOOD).unwrap();
        let printed = format!("{token:?}");
        assert!(!printed.contains(GOOD), "{printed}");
        assert!(printed.contains("已隐去"), "{printed}");
    }

    #[cfg(unix)]
    #[test]
    fn the_real_random_source_gives_distinct_values_and_a_shaped_token() {
        let mut a = [0u8; TOKEN_BYTES];
        let mut b = [0u8; TOKEN_BYTES];
        urandom(&mut a).unwrap();
        urandom(&mut b).unwrap();
        assert_ne!(a, b, "两次读要给出不同字节，否则随机源是桩");
        let token = PanelToken::new(&hex(&a)).unwrap();
        assert_eq!(token.secret().len(), TOKEN_HEX_LEN);
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_refuses_to_invent_a_weaker_random_source() {
        let mut buf = [0u8; TOKEN_BYTES];
        let err = urandom(&mut buf).unwrap_err();
        assert!(err.contains("/dev/urandom"), "{err}");
        // 报错也要让 load_or_create 把话说清楚，别让调用方拿到半份令牌
        let path = temp_path("nonunix");
        assert!(PanelToken::load_or_create(&path, urandom).is_err());
    }
}
