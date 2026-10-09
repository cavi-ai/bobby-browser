//! Domain handlers behind the reserved dispatch authorization boundary.
use super::*;

mod automation;
mod browser;
mod emulation;
mod network;
mod page;
mod runtime;
mod support;
mod target;

pub(super) async fn dispatch(
    connection: &CdpConnection,
    request: CdpRequest,
    ctx: RequestContext,
    handler: Handler,
) -> CdpResponse {
    match handler {
        Handler::AuditsEnable
        | Handler::PerformanceEnable
        | Handler::LogEnable
        | Handler::NetworkEnable
        | Handler::RuntimeRunIfWaiting => connection.dispatch_support(request, ctx, handler).await,
        Handler::NetworkSetUserAgent => connection.dispatch_network(request, ctx, handler).await,
        Handler::TargetGetBrowserContexts
        | Handler::TargetCreateBrowserContext
        | Handler::TargetSetDiscoverTargets
        | Handler::TargetCreateTarget
        | Handler::TargetGetTargets
        | Handler::TargetGetTargetInfo
        | Handler::TargetAttachToBrowserTarget
        | Handler::TargetDetachFromTarget
        | Handler::TargetSetAutoAttach => connection.dispatch_target(request, ctx, handler).await,
        Handler::BrowserGetVersion | Handler::BrowserSetDownloadBehavior => {
            connection.dispatch_browser(request, ctx, handler).await
        }
        Handler::PageGetFrameTree
        | Handler::PageGetLayoutMetrics
        | Handler::PageCaptureScreenshot
        | Handler::PageAddScript
        | Handler::PageCreateIsolatedWorld
        | Handler::PageNavigate
        | Handler::PageSetLifecycle
        | Handler::PageEnable => connection.dispatch_page(request, ctx, handler).await,
        Handler::AutomationCheckpointSave
        | Handler::AutomationRecoveryInspect
        | Handler::AutomationEventsRead
        | Handler::AutomationProtocolInventory => {
            connection.dispatch_automation(request, ctx, handler).await
        }
        Handler::RuntimeEnable
        | Handler::RuntimeEvaluate
        | Handler::RuntimeReleaseObject
        | Handler::RuntimeCallFunctionOn => {
            connection.dispatch_runtime(request, ctx, handler).await
        }
        Handler::EmulationSetFocus
        | Handler::EmulationSetMedia
        | Handler::EmulationSetDeviceMetrics
        | Handler::EmulationSetTouch => connection.dispatch_emulation(request, ctx, handler).await,
    }
}
