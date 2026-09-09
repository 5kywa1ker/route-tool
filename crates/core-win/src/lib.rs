//! core-win：Windows 专属实现。
//!
//! 提供 core-lib 中 trait 的 Windows 落地：网卡探测（GetAdaptersAddresses）、
//! 路由叠加（IP Helper CreateIpForwardEntry2）、网卡直改（netsh）与 ICMP ping。

pub mod adapter_reconfig;
pub mod adapters;
pub mod dhcp;
pub mod icmp;
pub mod inspector;
pub mod netsh;
pub mod route_overlay;

pub use inspector::WinNetInspector;
