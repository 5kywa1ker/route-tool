//! core 控制器：组合策略 / 健康检测 / 状态存储，实现业务动作。
//!
//! EnableBypass / DisableBypass / reconcile 等高层操作都在这里，
//! 供 IPC server 调用。

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// 健康检测循环世代：每次 stop 递增，旧世代循环停止产生副作用。
    health_generation: Arc<AtomicU64>,
}

impl Controller {
    pub fn new(store: core_lib::state_store::StateStore) -> Result<Self> {
        let (event_tx, _) = broadcast::channel(64);

        let config = Arc::new(RwLock::new(store.load_config()?));
        let runtime = Arc::new(RwLock::new(store.load_runtime()?));

        Ok(Self {
            config,
            runtime,
            store: store.clone(),
            overlay: Arc::new(RouteOverlayStrategy::new()),
            reconfig: Arc::new(AdapterReconfigStrategy::new(store)),
            handle: Arc::new(Mutex::new(None)),
            event_tx,
            health_cancel: Arc::new(Mutex::new(None)),
            health_generation: Arc::new(AtomicU64::new(0)),
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

    /// 校验配置合法性（UpdateConfig 与 enable 共用）。
    fn validate_config(cfg: &AppConfig) -> Result<()> {
        if cfg.adapter_id.trim().is_empty() {
            return Err(CoreError::ConfigIncomplete("请先在设置中选择网卡".into()));
        }
        if !is_valid_ipv4(&cfg.bypass_ip) {
            return Err(CoreError::ConfigIncomplete(
                "旁路由 IP 无效（需为非 0.0.0.0 的 IPv4 地址）".into(),
            ));
        }
        if cfg.health_check_interval_secs == 0 || cfg.health_check_interval_secs > 86400 {
            return Err(CoreError::ConfigIncomplete(
                "检测间隔需在 1~86400 秒之间".into(),
            ));
        }
        if cfg.failure_threshold == 0 || cfg.failure_threshold > 100 {
            return Err(CoreError::ConfigIncomplete(
                "失败阈值需在 1~100 之间".into(),
            ));
        }
        Ok(())
    }

    /// 更新配置：校验 + 落盘 + 启用中热生效（目标变更重放切换，健康参数刷新监控）。
    pub async fn update_config(&self, cfg: AppConfig) -> Result<()> {
        Self::validate_config(&cfg)?;

        self.store.save_config(&cfg)?;
        let old = {
            let mut guard = self.config.write().await;
            std::mem::replace(&mut *guard, cfg.clone())
        };

        if self.handle.lock().await.is_none() {
            return Ok(());
        }

        let target_changed = old.adapter_id != cfg.adapter_id
            || old.bypass_ip != cfg.bypass_ip
            || old.switch_mode != cfg.switch_mode
            || old.dns_override != cfg.dns_override;
        if target_changed {
            info!("config target changed while enabled; re-applying bypass");
            // enable() 内部先按旧句柄 disable（恢复/删路由），再按新配置启用。
            self.enable().await?;
        } else {
            // 仅健康参数变化：重启健康检测循环即可。
            let target = bypass_target(&cfg);
            self.start_health_monitor(&cfg, target).await;
        }
        Ok(())
    }

    /// 启动时一致性校验：按 runtime_state 中记录的预期状态修正实际状态。
    pub async fn reconcile_on_startup(&self) -> Result<()> {
        let rs = self.runtime.read().await.clone();
        if !rs.is_enabled {
            // 预期直连：清理残留叠加路由 / 孤儿快照（崩溃或禁用失败可能遗留）。
            let cfg = self.config.read().await.clone();
            self.cleanup_dirty_state(&cfg).await;
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
                // 校验两条 /1 叠加路由是否仍在，不在则重建。
                let IpAddr::V4(bypass_v4) = cfg.bypass_ip else {
                    return Err(CoreError::ConfigIncomplete("仅支持 IPv4 旁路由地址".into()));
                };
                match self.overlay.existing_handle_for(bypass_v4).await? {
                    Some(h) => {
                        info!("reconcile: overlay routes already in place");
                        *self.handle.lock().await = Some(h);
                    }
                    None => {
                        warn!("reconcile: expected bypass routes missing; re-adding");
                        let h = self.overlay.enable(&target).await?;
                        *self.handle.lock().await = Some(h);
                    }
                }
                self.mark_enabled(mode).await?;
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
                    warn!("reconcile: reconfig enabled but no snapshot; falling back to direct");
                    // 无快照无从恢复：清掉预期，保持当前状态，标记直连。
                    self.mark_disabled().await?;
                    return Ok(());
                }
            }
        }

        // 状态修正后必须恢复健康检测，否则崩溃自愈后将失去回退保护。
        self.start_health_monitor(&cfg, target).await;
        Ok(())
    }

    /// 预期直连时清理脏状态：残留叠加路由与孤儿快照。
    async fn cleanup_dirty_state(&self, cfg: &AppConfig) {
        if let IpAddr::V4(bypass_v4) = cfg.bypass_ip {
            match self.overlay.cleanup_routes_via(bypass_v4).await {
                Ok(0) => {}
                Ok(n) => warn!("reconcile: 清理了 {n} 条经 {bypass_v4} 的残留叠加路由"),
                Err(e) => warn!("reconcile: 清理残留叠加路由失败: {e}"),
            }
        }
        if self
            .store
            .load_snapshot()
            .map(|s| s.is_some())
            .unwrap_or(false)
        {
            warn!("reconcile: 发现孤儿网卡快照（预期直连），已删除");
            let _ = self.store.clear_snapshot();
        }
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
        Self::validate_config(&cfg)?;

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

        let handle = match strategy.enable(&target).await {
            Ok(h) => h,
            Err(e) => {
                // 启用失败不留孤儿快照，避免后续 disable 误恢复。
                if mode == SwitchMode::AdapterReconfig {
                    let _ = self.store.clear_snapshot();
                }
                return Err(e);
            }
        };
        *self.handle.lock().await = Some(handle);

        self.mark_enabled(mode).await?;
        self.start_health_monitor(&cfg, target).await;

        info!("bypass enabled via {mode:?}");
        Ok(())
    }

    /// 禁用旁路由（对外业务动作）。路由删除 / 快照恢复由各策略的 disable 完成
    /// （健康检测触发的自动回退也走同一条策略 disable 路径）。
    pub async fn disable(&self) -> Result<()> {
        self.stop_health_monitor().await;

        let handle = self.handle.lock().await.take();
        if let Some(handle) = handle {
            let strategy = self.strategy_for(handle.mode);
            if let Err(e) = strategy.disable(&handle).await {
                error!("禁用策略失败: {e}");
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

        // DHCP 状态：通过 netsh 查询。查询失败按静态处理（恢复时重放当前参数）。
        let is_dhcp = crate::netcheck::query_dhcp_enabled(&a.name)
            .await
            .unwrap_or(false);

        Ok(AdapterSnapshot {
            adapter_id: adapter_id.to_string(),
            is_dhcp_enabled: is_dhcp,
            static_ipv4: a.ipv4.clone(),
            static_ipv4_mask: vec![24; a.ipv4.len()],
            gateway: a.gateway.clone(),
            dns: a.dns.clone(),
        })
    }

    /// 启动健康检测循环。
    async fn start_health_monitor(&self, cfg: &AppConfig, target: BypassTarget) {
        // 先取消旧循环（世代递增，旧循环的在途探测不再产生副作用）。
        self.stop_health_monitor().await;

        let ctx = HealthCtx {
            inspector: self.inspector(),
            strategy: self.strategy_for(cfg.switch_mode),
            handle: self.handle.clone(),
            runtime: self.runtime.clone(),
            event_tx: self.event_tx.clone(),
            store: self.store.clone(),
            generation: self.health_generation.clone(),
        };
        let params = HealthParams {
            mode: cfg.switch_mode,
            target,
            interval: Duration::from_secs(u64::from(cfg.health_check_interval_secs.max(1))),
            threshold: cfg.failure_threshold.max(1),
            auto_reenable: cfg.auto_reenable_after_recovery,
            generation: self.health_generation.load(Ordering::SeqCst),
        };

        let (tx, rx) = watch::channel(false);
        *self.health_cancel.lock().await = Some(tx);
        tokio::spawn(run_loop(ctx, params, rx));
    }

    /// 停止健康检测循环。
    async fn stop_health_monitor(&self) {
        // 先递增世代：在途 tick 的回退/重建立即失效，防止 disable 后被复活。
        self.health_generation.fetch_add(1, Ordering::SeqCst);
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

fn is_valid_ipv4(ip: &IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if !v4.is_unspecified())
}
