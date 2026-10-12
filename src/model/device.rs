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
