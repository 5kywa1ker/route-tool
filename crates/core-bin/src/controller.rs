//! core 控制器：组合策略 / 健康检测 / 状态存储，实现业务动作。
//!
//! EnableBypass / DisableBypass / reconcile 等高层操作都在这里，
//! 供 IPC server 调用。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, watch, Mutex, RwLock};
use tracing::{error, info, warn};

use core_lib::health_monitor::{run_loop, HealthCtx, HealthParams};
use core_lib::switch_engine::SwitchStrategy;
use core_lib::{
    AdapterSnapshot, AppConfig, BypassTarget, CoreError, HealthEvent, Result, RuntimeState,
    SwitchHandle, SwitchMode,
};
use core_win::adapter_reconfig::AdapterReconfigStrategy;
use core_win::route_overlay::RouteOverlayStrategy;

/// 顶层控制器。
pub struct Controller {
    pub config: Arc<RwLock<AppConfig>>,
    pub runtime: Arc<RwLock<RuntimeState>>,
    pub store: core_lib::state_store::StateStore,
    pub overlay: Arc<RouteOverlayStrategy>,
    pub reconfig: Arc<AdapterReconfigStrategy>,
    pub handle: Arc<Mutex<Option<SwitchHandle>>>,
    pub event_tx: broadcast::Sender<HealthEvent>,
    /// 健康检测循环取消信号。
    health_cancel: Arc<Mutex<Option<watch::Sender<bool>>>>,
}

impl Controller {
    pub fn new(store: core_lib::state_store::StateStore) -> Result<Self> {
        let (event_tx, _) = broadcast::channel(64);

        let config = Arc::new(RwLock::new(store.load_config()?));
        let runtime = Arc::new(RwLock::new(store.load_runtime()?));

        Ok(Self {
            config,
            runtime,
            store,
            overlay: Arc::new(RouteOverlayStrategy::new()),
            reconfig: Arc::new(AdapterReconfigStrategy::new()),
            handle: Arc::new(Mutex::new(None)),
            event_tx,
            health_cancel: Arc::new(Mutex::new(None)),
        })
    }

    /// 当前配置对应的策略。
    fn strategy_for(&self, mode: SwitchMode) -> Arc<dyn SwitchStrategy> {
        match mode {
            SwitchMode::RouteOverlay => self.overlay.clone() as Arc<dyn SwitchStrategy>,
            SwitchMode::AdapterReconfig => self.reconfig.clone() as Arc<dyn SwitchStrategy>,
        }
    }

    pub fn inspector(&self) -> Arc<dyn core_lib::net_inspector::NetInspector> {
        Arc::new(core_win::WinNetInspector)
    }

    /// 启动时一致性校验：按 runtime_state 中记录的预期状态修正实际状态。
    pub async fn reconcile_on_startup(&self) -> Result<()> {
        let rs = self.runtime.read().await.clone();
        if !rs.is_enabled {
            // 预期直连：检查是否有遗留的叠加路由（脏路由）。
            self.cleanup_dirty_routes().await?;
            info!("reconcile: expected disabled; no dirty state assumed");
            return Ok(());
        }

        let Some(mode) = rs.current_mode else {
            return Ok(());
        };

        info!("reconcile: expected enabled via {mode:?}; verifying");
        let cfg = self.config.read().await.clone();
        let target = bypass_target(&cfg);

        match mode {
            SwitchMode::RouteOverlay => {
                // 校验默认路由是否指向旁路由，不正确则重建。
                let state = core_win::adapters::current_route_state(&cfg.adapter_id)
                    .map_err(|e| CoreError::Network(format!("读取路由表失败: {e}")))?;
                if !state.default_via_bypass {
                    warn!("reconcile: expected bypass route missing; re-adding");
                    let h = self.overlay.enable(&target).await?;
                    *self.handle.lock().await = Some(h);
                    self.mark_enabled(mode).await?;
                } else {
                    info!("reconcile: route overlay already in place");
                }
            }
            SwitchMode::AdapterReconfig => {
                // 快照存在 -> 若网卡当前网关不是 bypass，则重新应用。
                if self.store.load_snapshot()?.is_some() {
                    let list = core_win::adapters::list_adapters()
                        .map_err(|e| CoreError::Network(format!("枚举网卡失败: {e}")))?;
                    let gw_ok = list
                        .iter()
                        .find(|a| a.id == cfg.adapter_id)
                        .map(|a| a.gateway.first() == Some(&cfg.bypass_ip))
                        .unwrap_or(false);
                    if !gw_ok {
                        warn!("reconcile: adapter state != expected; re-applying");
                        let h = self.reconfig.enable(&target).await?;
                        *self.handle.lock().await = Some(h);
                        self.mark_enabled(mode).await?;
                    }
                } else {
                    warn!("reconcile: reconfig enabled but no snapshot; cleaning to direct");
                    // 无快照无从恢复：清掉预期，保持当前状态，标记直连。
                    self.mark_disabled().await?;
                }
            }
        }
        Ok(())
    }

    /// 清理不处于预期中的叠加路由（预期直连时）。
    async fn cleanup_dirty_routes(&self) -> Result<()> {
        // 通过 core-bin 层面无法直接知道历史 next_hop；读取 runtime 之外
        // 还有一个更可靠的方法：route 表中 protocol=netmgmt 且我们记录过。
        // MVP：若 runtime 记录了 next_hop 则尝试删除。
        let rs = self.runtime.read().await.clone();
        let _ = rs;
        Ok(())
    }

    async fn mark_enabled(&self, mode: SwitchMode) -> Result<()> {
        let mut rs = self.runtime.write().await;
        rs.is_enabled = true;
        rs.current_mode = Some(mode);
        rs.last_updated = chrono::Utc::now();
        self.store.save_runtime(&rs.clone())?;
        Ok(())
    }

    async fn mark_disabled(&self) -> Result<()> {
        let mut rs = self.runtime.write().await;
        rs.is_enabled = false;
        rs.current_mode = None;
        rs.last_updated = chrono::Utc::now();
        self.store.save_runtime(&rs.clone())?;
        Ok(())
    }

    /// 启用旁路由（对外业务动作）。
    pub async fn enable(&self) -> Result<()> {
        let cfg = self.config.read().await.clone();
        if cfg.adapter_id.is_empty() || !is_valid_ipv4(&cfg.bypass_ip) {
            return Err(CoreError::ConfigIncomplete(
                "请先在设置中完成网卡与旁路由 IP 配置".into(),
            ));
        }

        // 已启用则先禁用（保证幂等）。
        if self.handle.lock().await.is_some() {
            self.disable().await?;
        }

        let mode = cfg.switch_mode;
        let target = bypass_target(&cfg);
        let strategy = self.strategy_for(mode);

        // AdapterReconfig：启用前先备份网卡状态。
        if mode == SwitchMode::AdapterReconfig {
            let snap = self.backup_adapter(&cfg.adapter_id).await?;
            self.store.save_snapshot(&snap)?;
        }

        let handle = strategy.enable(&target).await?;
        *self.handle.lock().await = Some(handle);

        self.mark_enabled(mode).await?;
        self.start_health_monitor(&cfg, target).await;

        info!("bypass enabled via {mode:?}");
        Ok(())
    }

    /// 禁用旁路由（对外业务动作）。
    pub async fn disable(&self) -> Result<()> {
        self.stop_health_monitor().await;

        let cfg = self.config.read().await.clone();
        let mode = cfg.switch_mode;
        let strategy = self.strategy_for(mode);

        let mut guard = self.handle.lock().await;
        if let Some(handle) = guard.take() {
            // AdapterReconfig：恢复快照。
            if handle.mode == SwitchMode::AdapterReconfig {
                match self.store.load_snapshot()? {
                    Some(snap) => {
                        if let Err(e) = self.restore_adapter(&snap).await {
                            error!("恢复网卡状态失败: {e}");
                        }
                        self.store.clear_snapshot()?;
                    }
                    None => warn!("无快照可恢复，保持当前网卡状态"),
                }
            } else if let Err(e) = strategy.disable(&handle).await {
                error!("禁用路由叠加失败: {e}");
                return Err(e);
            }
        }

        self.mark_disabled().await?;
        info!("bypass disabled");
        Ok(())
    }

    /// 备份网卡当前状态。
    async fn backup_adapter(&self, adapter_id: &str) -> Result<AdapterSnapshot> {
        let list = core_win::adapters::list_adapters()
            .map_err(|e| CoreError::Network(format!("枚举网卡失败: {e}")))?;
        let a = list
            .iter()
            .find(|x| x.id == adapter_id)
            .ok_or_else(|| CoreError::Network(format!("网卡 {adapter_id} 不存在")))?;

        // DHCP 状态：通过 netsh 查询。MVP 简化：记录 IP/网关/DNS 即可，
        // 恢复时按“静态恢复原参数”。若原为 DHCP，恢复时先开 DHCP 再覆盖静态。
        let is_dhcp = crate::netcheck::query_dhcp_enabled(&a.name).await.unwrap_or(false);

        Ok(AdapterSnapshot {
            adapter_id: adapter_id.to_string(),
            is_dhcp_enabled: is_dhcp,
            static_ipv4: a.ipv4.clone(),
            static_ipv4_mask: vec![24; a.ipv4.len()],
            gateway: a.gateway.clone(),
            dns: a.dns.clone(),
        })
    }

    /// 按快照恢复网卡状态。
    async fn restore_adapter(&self, snap: &AdapterSnapshot) -> Result<()> {
        let list = core_win::adapters::list_adapters()
            .map_err(|e| CoreError::Network(format!("枚举网卡失败: {e}")))?;
        let name = list
            .iter()
            .find(|x| x.id == snap.adapter_id)
            .map(|x| x.name.clone())
            .ok_or_else(|| CoreError::Network(format!("网卡 {} 已不存在", snap.adapter_id)))?;

        if snap.is_dhcp_enabled {
            core_win::netsh::enable_dhcp(&name).await?;
            core_win::netsh::reset_dns(&name).await?;
        } else {
            // 恢复原静态参数。
            let ip = snap
                .static_ipv4
                .first()
                .and_then(|x| match x {
                    std::net::IpAddr::V4(v4) => Some(v4),
                    _ => None,
                })
                .ok_or_else(|| CoreError::Network("快照缺少 IPv4".into()))?;
            let gw = snap
                .gateway
                .first()
                .and_then(|x| match x {
                    std::net::IpAddr::V4(v4) => Some(v4),
                    _ => None,
                })
                .ok_or_else(|| CoreError::Network("快照缺少网关".into()))?;
            let mask = std::net::Ipv4Addr::new(255, 255, 255, 0);

            core_win::netsh::set_static_ipv4(&name, *ip, mask, *gw).await?;
            if snap.dns.is_empty() {
                core_win::netsh::reset_dns(&name).await?;
            } else {
                let dns4: Vec<std::net::Ipv4Addr> = snap
                    .dns
                    .iter()
                    .filter_map(|x| match x {
                        std::net::IpAddr::V4(v4) => Some(*v4),
                        _ => None,
                    })
                    .collect();
                core_win::netsh::set_dns(&name, &dns4).await?;
            }
        }
        info!("adapter {} restored from snapshot", snap.adapter_id);
        Ok(())
    }

    /// 启动健康检测循环。
    async fn start_health_monitor(&self, cfg: &AppConfig, target: BypassTarget) {
        // 先取消旧循环。
        self.stop_health_monitor().await;

        let ctx = HealthCtx {
            inspector: self.inspector(),
            strategy: self.strategy_for(cfg.switch_mode),
            handle: self.handle.clone(),
            runtime: self.runtime.clone(),
            event_tx: self.event_tx.clone(),
        };
        let params = HealthParams {
            mode: cfg.switch_mode,
            target,
            interval: Duration::from_secs(u64::from(cfg.health_check_interval_secs.max(1))),
            threshold: cfg.failure_threshold.max(1),
            auto_reenable: cfg.auto_reenable_after_recovery,
        };

        let (tx, rx) = watch::channel(false);
        *self.health_cancel.lock().await = Some(tx);
        tokio::spawn(run_loop(ctx, params, rx));
    }

    /// 停止健康检测循环。
    async fn stop_health_monitor(&self) {
        if let Some(tx) = self.health_cancel.lock().await.take() {
            let _ = tx.send(true);
        }
    }
}

/// 由配置构造 BypassTarget。
fn bypass_target(cfg: &AppConfig) -> BypassTarget {
    BypassTarget {
        adapter_id: cfg.adapter_id.clone(),
        bypass_ip: cfg.bypass_ip,
        dns: cfg.dns_override.clone(),
    }
}

fn is_valid_ipv4(ip: &std::net::IpAddr) -> bool {
    matches!(ip, std::net::IpAddr::V4(v4) if !v4.is_unspecified())
}
