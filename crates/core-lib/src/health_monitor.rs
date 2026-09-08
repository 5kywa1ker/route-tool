//! 健康检测状态机与定时探测循环。
//!
//! 状态：Idle -> Healthy -> Degraded(n) -> Fallback（阈值触发自动回退）。
//! 仅在旁路由 enabled 时运行。

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, watch, Mutex, RwLock};
use tracing::{error, info, warn};

use crate::net_inspector::NetInspector;
use crate::state_store::StateStore;
use crate::switch_engine::SwitchStrategy;
use crate::{
    BypassTarget, HealthEvent, HealthEventType, HealthStatus, Result, RuntimeState, SwitchHandle,
    SwitchMode,
};

/// 探测超时（单次 ping 的最大等待，毫秒）。
const PING_TIMEOUT_MS: u32 = 3000;

/// 健康检测状态机内部状态。
#[derive(Debug, Clone, PartialEq, Eq)]
enum HealthState {
    /// 旁路由未启用。
    Idle,
    /// 启用且健康。
    Healthy,
    /// 连续失败计数未达阈值。
    Degraded(u32),
    /// 已自动回退到直连。
    Fallback,
}

/// 健康检测循环的上下文（共享给其他模块访问）。
#[derive(Clone)]
pub struct HealthCtx {
    pub inspector: Arc<dyn NetInspector>,
    pub strategy: Arc<dyn SwitchStrategy>,
    /// 当前生效的切换句柄（启用后由这里维护）。
    pub handle: Arc<Mutex<Option<SwitchHandle>>>,
    pub runtime: Arc<RwLock<RuntimeState>>,
    pub event_tx: broadcast::Sender<HealthEvent>,
    /// 运行状态落盘（回退/恢复也必须持久化，避免重启读到陈旧状态）。
    pub store: StateStore,
    /// 共享世代计数器：Controller 每次 stop 递增，旧世代循环停止产生副作用。
    pub generation: Arc<AtomicU64>,
}

/// 健康检测参数（从 AppConfig 派生的快照）。
#[derive(Debug, Clone)]
pub struct HealthParams {
    pub mode: SwitchMode,
    pub target: BypassTarget,
    pub interval: Duration,
    pub threshold: u32,
    pub auto_reenable: bool,
    /// 本循环的世代号（与 ctx.generation 不一致说明已被新循环取代）。
    pub generation: u64,
}

/// 运行健康检测循环，直到 cancel 收到 true。
pub async fn run_loop(ctx: HealthCtx, params: HealthParams, mut cancel: watch::Receiver<bool>) {
    let mut timer = tokio::time::interval(params.interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut state = HealthState::Idle;

    info!(
        "health monitor started: interval={:?} threshold={}",
        params.interval, params.threshold
    );

    loop {
        tokio::select! {
            _ = cancel.changed() => {
                if *cancel.borrow() {
                    info!("health monitor cancelled");
                    break;
                }
            }
            _ = timer.tick() => {
                tick(&ctx, &params, &mut state).await;
            }
        }
    }
}

/// 单次探测逻辑。
async fn tick(ctx: &HealthCtx, params: &HealthParams, state: &mut HealthState) {
    // 旧世代循环（已被新的 enable/disable 取代）不再产生任何副作用。
    if ctx.generation.load(Ordering::SeqCst) != params.generation {
        return;
    }
    // Idle 不探测（由 Controller 在启用时启动循环）。
    if *state == HealthState::Idle {
        return;
    }

    let probe = try_ping(ctx.inspector.as_ref(), params.target.bypass_ip).await;
    let consecutive = match probe {
        true => handle_success(ctx, params, state).await,
        false => handle_failure(ctx, params, state).await,
    };

    update_runtime_health(ctx, state, consecutive).await;
}

/// 返回 Ping 是否可达。
async fn try_ping(inspector: &dyn NetInspector, ip: IpAddr) -> bool {
    match inspector.ping(ip, PING_TIMEOUT_MS).await {
        Ok(true) => true,
        Ok(false) => false,
        Err(e) => {
            warn!("ping {ip} failed with error: {e}");
            false
        }
    }
}

/// 探测成功：恢复健康，必要时按 auto_reenable 重新启用，并校验路由是否仍生效。
async fn handle_success(
    ctx: &HealthCtx,
    params: &HealthParams,
    state: &mut HealthState,
) -> Option<u32> {
    match state {
        HealthState::Healthy => {
            // 持续健康：顺带校验切换句柄是否仍生效，网卡切换/睡眠唤醒后按需重建。
            ensure_active(ctx, params).await;
        }
        HealthState::Degraded(n) => {
            let n = *n;
            *state = HealthState::Healthy;
            info!("bypass probe recovered (was degraded x{n})");
            emit(ctx, HealthEventType::ProbeRecovered).await;
        }
        HealthState::Fallback => {
            if params.auto_reenable {
                info!("bypass recovered; auto re-enabling");
                match reenable(ctx, params).await {
                    Ok(()) => {
                        *state = HealthState::Healthy;
                        emit(ctx, HealthEventType::ProbeRecovered).await;
                    }
                    Err(e) => error!("auto re-enable failed: {e}"),
                }
            } else {
                emit(ctx, HealthEventType::ProbeRecovered).await;
            }
        }
        HealthState::Idle => {}
    }
    // 探测成功意味着连续失败清零。
    Some(0)
}

/// 探测失败：计数，达阈值触发回退。
async fn handle_failure(
    ctx: &HealthCtx,
    params: &HealthParams,
    state: &mut HealthState,
) -> Option<u32> {
    match state {
        HealthState::Degraded(n) => {
            let n = *n + 1;
            *state = HealthState::Degraded(n);
            emit(
                ctx,
                HealthEventType::ProbeFailed {
                    consecutive_count: n,
                },
            )
            .await;
            if n >= params.threshold {
                fallback(
                    ctx,
                    params,
                    state,
                    format!("连续探测失败 {n}/{} 次", params.threshold),
                )
                .await;
            }
            Some(n)
        }
        HealthState::Healthy => {
            *state = HealthState::Degraded(1);
            emit(
                ctx,
                HealthEventType::ProbeFailed {
                    consecutive_count: 1,
                },
            )
            .await;
            if 1 >= params.threshold {
                fallback(ctx, params, state, "阈值=1，首次探测即失败".to_string()).await;
            }
            Some(1)
        }
        HealthState::Fallback => None,
        HealthState::Idle => None,
    }
}

/// 触发自动回退：禁用当前策略（若句柄仍存在），更新运行状态，发事件。
async fn fallback(ctx: &HealthCtx, params: &HealthParams, state: &mut HealthState, reason: String) {
    if ctx.generation.load(Ordering::SeqCst) != params.generation {
        info!("health loop stale (generation changed); skip fallback");
        return;
    }
    info!("auto fallback triggered: {reason}");
    {
        let guard = ctx.handle.lock().await;
        if let Some(handle) = guard.as_ref() {
            if let Err(e) = ctx.strategy.disable(handle).await {
                error!("fallback disable failed: {e}");
            }
        }
    }
    *ctx.handle.lock().await = None;

    let snapshot = {
        let mut rs = ctx.runtime.write().await;
        rs.is_enabled = false;
        rs.current_mode = None;
        rs.health = HealthStatus::Fallback;
        rs.last_updated = chrono::Utc::now();
        rs.clone()
    };
    persist_runtime(ctx, &snapshot);

    *state = HealthState::Fallback;
    emit(ctx, HealthEventType::AutoFallbackTriggered { reason }).await;
}

/// 校验句柄对应状态仍生效；若不生效则重建（适配网卡切换/睡眠唤醒）。
async fn ensure_active(ctx: &HealthCtx, params: &HealthParams) {
    let active = {
        let guard = ctx.handle.lock().await;
        match guard.as_ref() {
            Some(h) => match ctx.strategy.is_active(h).await {
                Ok(a) => a,
                Err(e) => {
                    warn!("is_active check failed: {e}");
                    true
                }
            },
            None => false,
        }
    };

    if !active {
        info!("bypass no longer active (iface changed); re-enabling");
        if let Err(e) = reenable(ctx, params).await {
            warn!("rebuild failed: {e}");
        }
    }
}

/// 用配置重新启用旁路由，刷新句柄与运行状态。
async fn reenable(ctx: &HealthCtx, params: &HealthParams) -> Result<()> {
    if ctx.generation.load(Ordering::SeqCst) != params.generation {
        info!("health loop stale (generation changed); skip re-enable");
        return Ok(());
    }
    let new_handle = ctx.strategy.enable(&params.target).await?;
    *ctx.handle.lock().await = Some(new_handle);

    let snapshot = {
        let mut rs = ctx.runtime.write().await;
        rs.is_enabled = true;
        rs.current_mode = Some(params.mode);
        rs.health = HealthStatus::Healthy;
        rs.last_updated = chrono::Utc::now();
        rs.clone()
    };
    persist_runtime(ctx, &snapshot);
    Ok(())
}

/// 更新 runtime 的健康状态字段（与状态机保持一致）。
async fn update_runtime_health(ctx: &HealthCtx, state: &HealthState, consecutive: Option<u32>) {
    let mut rs = ctx.runtime.write().await;
    match state {
        HealthState::Idle => rs.health = HealthStatus::Idle,
        HealthState::Healthy => rs.health = HealthStatus::Healthy,
        HealthState::Degraded(_) => {
            let n = consecutive.unwrap_or(1);
            rs.health = HealthStatus::Degraded {
                consecutive_failures: n,
            };
        }
        HealthState::Fallback => rs.health = HealthStatus::Fallback,
    }
    rs.last_updated = chrono::Utc::now();
}

/// 将运行状态落盘（健康检测驱动的回退/恢复同样持久化，防止重启后读到陈旧预期）。
fn persist_runtime(ctx: &HealthCtx, rs: &RuntimeState) {
    if let Err(e) = ctx.store.save_runtime(rs) {
        warn!("持久化运行状态失败: {e}");
    }
}

async fn emit(ctx: &HealthCtx, event_type: HealthEventType) {
    let evt = HealthEvent {
        at: chrono::Utc::now(),
        reason: None,
        event_type,
    };
    // broadcast 无订阅者时会立即返回 Err，忽略即可。
    let _ = ctx.event_tx.send(evt);
    // 同时也写入 runtime（供不订阅的 UI GetStatus 轮询读到）。
    let mut rs = ctx.runtime.write().await;
    rs.last_updated = chrono::Utc::now();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net_inspector::NetInspector;
    use crate::state_store::StateStore;
    use crate::switch_engine::SwitchStrategy;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 可编程行为的 mock inspector：前 fail_first 次探测失败，之后成功。
    struct MockInspector {
        fail_first: AtomicU32,
    }

    #[async_trait]
    impl NetInspector for MockInspector {
        async fn list_adapters(&self) -> crate::Result<Vec<crate::AdapterInfo>> {
            Ok(vec![])
        }

        async fn current_route_state(&self, _adapter_id: &str) -> crate::Result<crate::RouteState> {
            Ok(crate::RouteState {
                default_via_bypass: false,
                default_next_hops: vec![],
            })
        }

        async fn ping(&self, _ip: IpAddr, _timeout_ms: u32) -> crate::Result<bool> {
            // fail_first=N：前 N 次失败，之后成功；u32::MAX 视为一直失败。
            let prev = self
                .fail_first
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                    if v == 0 {
                        Some(0)
                    } else {
                        Some(v - 1)
                    }
                });
            Ok(matches!(prev, Ok(0)))
        }
    }

    /// 记录 enable/disable 调用的 mock 策略。
    #[derive(Default)]
    struct MockStrategy {
        enable_count: AtomicU32,
        disable_count: AtomicU32,
    }

    #[async_trait]
    impl SwitchStrategy for MockStrategy {
        async fn enable(&self, _target: &BypassTarget) -> crate::Result<SwitchHandle> {
            self.enable_count.fetch_add(1, Ordering::SeqCst);
            Ok(SwitchHandle {
                mode: SwitchMode::RouteOverlay,
                if_index: Some(1),
                if_luid: Some(1),
                destination_prefix: Some("0.0.0.0/0".into()),
                next_hop: Some("10.0.0.1".parse().unwrap()),
                adapter_id: None,
                extra_routes: vec![],
            })
        }

        async fn disable(&self, _handle: &SwitchHandle) -> crate::Result<()> {
            self.disable_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn is_active(&self, _handle: &SwitchHandle) -> crate::Result<bool> {
            Ok(true)
        }

        async fn reconcile_on_startup(
            &self,
        ) -> crate::Result<crate::switch_engine::ReconcileAction> {
            Ok(crate::switch_engine::ReconcileAction::NoAction)
        }
    }

    fn test_ctx(
        inspector: Arc<dyn NetInspector>,
        strategy: Arc<dyn SwitchStrategy>,
    ) -> (HealthCtx, tokio::sync::broadcast::Receiver<HealthEvent>) {
        // 每个测试用例独立的 StateStore 目录，避免并行测试写同一文件。
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("RouteToolHealthTest_{}_{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = StateStore::new(dir);
        let _ = store.ensure_dirs();

        let (event_tx, event_rx) = tokio::sync::broadcast::channel(16);
        let ctx = HealthCtx {
            inspector,
            strategy,
            handle: Arc::new(Mutex::new(Some(SwitchHandle {
                mode: SwitchMode::RouteOverlay,
                if_index: Some(1),
                if_luid: Some(1),
                destination_prefix: Some("0.0.0.0/0".into()),
                next_hop: Some("10.0.0.1".parse().unwrap()),
                adapter_id: None,
                extra_routes: vec![],
            }))),
            runtime: Arc::new(RwLock::new(RuntimeState {
                is_enabled: true,
                current_mode: Some(SwitchMode::RouteOverlay),
                health: HealthStatus::Healthy,
                ..RuntimeState::default()
            })),
            event_tx,
            store,
            generation: Arc::new(AtomicU64::new(0)),
        };
        (ctx, event_rx)
    }

    fn params(threshold: u32, auto_reenable: bool) -> HealthParams {
        HealthParams {
            mode: SwitchMode::RouteOverlay,
            target: BypassTarget {
                adapter_id: "guid".into(),
                bypass_ip: "10.0.0.1".parse().unwrap(),
                dns: None,
                subnet_mask: None,
            },
            interval: Duration::from_secs(3600), // 手动 tick，不受定时影响
            threshold,
            auto_reenable,
            generation: 0,
        }
    }

    /// 把 broadcast Receiver 转成可 await 的包装（单消费者测试用）。
    async fn next_event(rx: &mut tokio::sync::broadcast::Receiver<HealthEvent>) -> HealthEvent {
        rx.recv().await.expect("event channel closed")
    }

    #[tokio::test]
    async fn healthy_stays_healthy_on_success() {
        let (ctx, rx) = test_ctx(
            Arc::new(MockInspector {
                fail_first: AtomicU32::new(0),
            }),
            Arc::new(MockStrategy::default()),
        );
        let p = params(3, false);
        let mut state = HealthState::Healthy;

        tick(&ctx, &p, &mut state).await;
        assert_eq!(state, HealthState::Healthy);
        let rs = ctx.runtime.read().await;
        assert_eq!(rs.health, HealthStatus::Healthy);
        assert!(rs.is_enabled);
        drop(rx);
    }

    #[tokio::test]
    async fn failures_degrade_then_fallback_at_threshold() {
        let (ctx, mut rx) = test_ctx(
            Arc::new(MockInspector {
                fail_first: AtomicU32::new(u32::MAX),
            }),
            Arc::new(MockStrategy::default()),
        );
        let p = params(3, false);
        let mut state = HealthState::Healthy;

        // 第 1 次失败：Degraded(1)。
        tick(&ctx, &p, &mut state).await;
        assert_eq!(state, HealthState::Degraded(1));
        let evt = next_event(&mut rx).await;
        assert!(matches!(
            evt.event_type,
            HealthEventType::ProbeFailed {
                consecutive_count: 1
            }
        ));

        // 第 2 次失败：Degraded(2)。
        tick(&ctx, &p, &mut state).await;
        assert_eq!(state, HealthState::Degraded(2));

        // 第 3 次失败：达到阈值 -> Fallback，策略被禁用，runtime 更新。
        tick(&ctx, &p, &mut state).await;
        assert_eq!(state, HealthState::Fallback);
        assert!(ctx.handle.lock().await.is_none());
        let rs = ctx.runtime.read().await;
        assert_eq!(rs.health, HealthStatus::Fallback);
        assert!(!rs.is_enabled);
        drop(rs);

        let evt = next_event(&mut rx).await; // ProbeFailed 2
        assert!(matches!(
            evt.event_type,
            HealthEventType::ProbeFailed {
                consecutive_count: 2
            }
        ));
        let evt = next_event(&mut rx).await; // ProbeFailed 3
        assert!(matches!(
            evt.event_type,
            HealthEventType::ProbeFailed {
                consecutive_count: 3
            }
        ));
        let evt = next_event(&mut rx).await; // AutoFallbackTriggered
        assert!(matches!(
            evt.event_type,
            HealthEventType::AutoFallbackTriggered { .. }
        ));
    }

    #[tokio::test]
    async fn recovery_from_degraded_clears_count() {
        let (ctx, mut rx) = test_ctx(
            Arc::new(MockInspector {
                fail_first: AtomicU32::new(1),
            }),
            Arc::new(MockStrategy::default()),
        );
        let p = params(3, false);
        let mut state = HealthState::Healthy;

        // 失败一次 -> Degraded(1)，随后成功 -> Healthy + ProbeRecovered。
        tick(&ctx, &p, &mut state).await;
        assert_eq!(state, HealthState::Degraded(1));
        tick(&ctx, &p, &mut state).await;
        assert_eq!(state, HealthState::Healthy);

        let evt = next_event(&mut rx).await; // ProbeFailed 1
        assert!(matches!(
            evt.event_type,
            HealthEventType::ProbeFailed { .. }
        ));
        let evt = next_event(&mut rx).await; // ProbeRecovered
        assert!(matches!(evt.event_type, HealthEventType::ProbeRecovered));
    }

    #[tokio::test]
    async fn fallback_with_auto_reenable_restarts_strategy() {
        let strategy = Arc::new(MockStrategy::default());
        // 首次探测失败，其后恢复（fail_first=1）。
        let (ctx, mut rx) = test_ctx(
            Arc::new(MockInspector {
                fail_first: AtomicU32::new(1),
            }),
            strategy.clone(),
        );
        let p = params(1, true); // 阈值 1：首次失败即回退
        let mut state = HealthState::Healthy;

        tick(&ctx, &p, &mut state).await;
        assert_eq!(state, HealthState::Fallback);
        assert_eq!(strategy.disable_count.load(Ordering::SeqCst), 1);

        // 恢复后 auto_reenable 触发重新启用。
        tick(&ctx, &p, &mut state).await;
        assert_eq!(state, HealthState::Healthy);
        assert_eq!(strategy.enable_count.load(Ordering::SeqCst), 1);
        assert!(ctx.handle.lock().await.is_some());
        let rs = ctx.runtime.read().await;
        assert!(rs.is_enabled);
        drop(rs);

        let _ = next_event(&mut rx).await; // ProbeFailed 1
        let _ = next_event(&mut rx).await; // AutoFallbackTriggered
        let evt = next_event(&mut rx).await; // ProbeRecovered
        assert!(matches!(evt.event_type, HealthEventType::ProbeRecovered));
    }

    #[tokio::test]
    async fn fallback_without_auto_reenable_stays_direct() {
        let strategy = Arc::new(MockStrategy::default());
        let (ctx, _rx) = test_ctx(
            Arc::new(MockInspector {
                fail_first: AtomicU32::new(u32::MAX),
            }),
            strategy.clone(),
        );
        let p = params(1, false);
        let mut state = HealthState::Healthy;

        tick(&ctx, &p, &mut state).await; // 失败 -> Fallback
        assert_eq!(state, HealthState::Fallback);
        tick(&ctx, &p, &mut state).await; // 仍失败，Fallback 不再翻转
        assert_eq!(state, HealthState::Fallback);
        assert_eq!(strategy.enable_count.load(Ordering::SeqCst), 0);
        assert!(ctx.handle.lock().await.is_none());
    }

    #[tokio::test]
    async fn idle_never_probes() {
        let inspector = Arc::new(MockInspector {
            fail_first: AtomicU32::new(u32::MAX),
        });
        let (ctx, mut rx) = test_ctx(inspector.clone(), Arc::new(MockStrategy::default()));
        let p = params(3, false);
        let mut state = HealthState::Idle;

        tick(&ctx, &p, &mut state).await;
        assert_eq!(state, HealthState::Idle);
        let rs = ctx.runtime.read().await;
        assert_eq!(rs.health, HealthStatus::Healthy); // runtime 未被 tick 改动
        drop(rs);
        // 无事件产生。
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn stale_generation_loop_produces_no_side_effects() {
        let strategy = Arc::new(MockStrategy::default());
        let (ctx, mut rx) = test_ctx(
            Arc::new(MockInspector {
                fail_first: AtomicU32::new(u32::MAX),
            }),
            strategy.clone(),
        );
        let p = params(1, false);
        let mut state = HealthState::Healthy;

        // 模拟循环已被新的 enable/disable 取代：世代前进。
        ctx.generation.store(1, Ordering::SeqCst);

        // 在途 tick 不再探测、不回退、不重建。
        tick(&ctx, &p, &mut state).await;
        assert_eq!(state, HealthState::Healthy);
        assert_eq!(strategy.disable_count.load(Ordering::SeqCst), 0);
        assert_eq!(strategy.enable_count.load(Ordering::SeqCst), 0);
        assert!(rx.try_recv().is_err());

        // 落盘状态不被旧世代污染：runtime_state.json 不存在（从未写入）。
        assert!(!ctx.store.base_dir().join("runtime_state.json").exists());
    }
}
