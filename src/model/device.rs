use serde::{Deserialize, Serialize};

/// LAN 设备及其分流策略。旁路由场景下设备由 ARP/DHCP 观测发现，不依赖设备端安装任何东西。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub ip: String,
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    pub mode: DeviceMode,
    /// IP 分配方式：DHCP 或静态
    #[serde(default)]
    pub ip_type: IpType,
    /// 显示名称：默认取 IP 最后一段（如 192.168.2.105 → "105"），用户可自定义
    #[serde(default)]
    pub display_name: Option<String>,
}

impl Device {
    /// 显示名称：用户自定义优先，否则取 IP 最后一段（192.168.2.105 → "105"）
    pub fn display_name(&self) -> String {
        if let Some(ref name) = self.display_name {
            return name.clone();
        }
        // 取 IP 最后一段：192.168.2.105 → "105"，fd00::1 → "1"
        self.ip
            .rsplit(|c| c == '.' || c == ':')
            .next()
            .unwrap_or(&self.ip)
            .to_string()
    }
}

/// IP 地址分配方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpType {
    /// DHCP 自动分配
    #[default]
    Dhcp,
    /// 静态 IP
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceMode {
    /// 跟随全局策略
    #[default]
    Global,
    /// 强制直连（电视、打印机、IoT）
    Direct,
    /// 强制走代理
    Proxy,
    /// 完全 bypass ZBox，不经本机
    Bypass,
}
