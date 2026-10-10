//! One exhaustive routing table behind reserved-dispatch authorization.
//! Each method owns validation, identifier resolution, execution, and shaping.
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
        Handler::AutomationCheckpointSave => {
            connection
                .handle_automation_checkpoint_save(request, ctx)
                .await
        }
        Handler::AutomationRecoveryInspect => {
            connection
                .handle_automation_recovery_inspect(request, ctx)
                .await
        }
        Handler::AutomationEventsRead => {
            connection.handle_automation_events_read(request, ctx).await
        }
        Handler::AutomationProtocolInventory => {
            connection
                .handle_automation_protocol_inventory(request, ctx)
                .await
        }
        Handler::BrowserGetVersion => connection.handle_browser_get_version(request, ctx).await,
        Handler::BrowserSetDownloadBehavior => {
            connection
                .handle_browser_set_download_behavior(request, ctx)
                .await
        }
        Handler::EmulationSetFocus => connection.handle_emulation_set_focus(request, ctx).await,
        Handler::EmulationSetMedia => connection.handle_emulation_set_media(request, ctx).await,
        Handler::EmulationSetDeviceMetrics => {
            connection
                .handle_emulation_set_device_metrics(request, ctx)
                .await
        }
        Handler::EmulationSetTouch => connection.handle_emulation_set_touch(request, ctx).await,
        Handler::NetworkSetUserAgent => {
            connection.handle_network_set_user_agent(request, ctx).await
        }
        Handler::PageGetFrameTree => connection.handle_page_get_frame_tree(request, ctx).await,
        Handler::PageGetLayoutMetrics => {
            connection
                .handle_page_get_layout_metrics(request, ctx)
                .await
        }
        Handler::PageCaptureScreenshot => {
            connection
                .handle_page_capture_screenshot(request, ctx)
                .await
        }
        Handler::PageAddScript => connection.handle_page_add_script(request, ctx).await,
        Handler::PageCreateIsolatedWorld => {
            connection
                .handle_page_create_isolated_world(request, ctx)
                .await
        }
        Handler::PageNavigate => connection.handle_page_navigate(request, ctx).await,
        Handler::PageSetLifecycle => connection.handle_page_set_lifecycle(request, ctx).await,
        Handler::PageEnable => connection.handle_page_enable(request, ctx).await,
        Handler::RuntimeEnable => connection.handle_runtime_enable(request, ctx).await,
        Handler::RuntimeEvaluate => connection.handle_runtime_evaluate(request, ctx).await,
        Handler::RuntimeReleaseObject => {
            connection.handle_runtime_release_object(request, ctx).await
        }
        Handler::RuntimeCallFunctionOn => {
            connection
                .handle_runtime_call_function_on(request, ctx)
                .await
        }
        Handler::AuditsEnable => connection.handle_audits_enable(request, ctx).await,
        Handler::PerformanceEnable => connection.handle_audits_enable(request, ctx).await,
        Handler::LogEnable => connection.handle_log_enable(request, ctx).await,
        Handler::NetworkEnable => connection.handle_log_enable(request, ctx).await,
        Handler::RuntimeRunIfWaiting => connection.handle_log_enable(request, ctx).await,
        Handler::TargetGetBrowserContexts => {
            connection
                .handle_target_get_browser_contexts(request, ctx)
                .await
        }
        Handler::TargetCreateBrowserContext => {
            connection
                .handle_target_create_browser_context(request, ctx)
                .await
        }
        Handler::TargetSetDiscoverTargets => {
            connection
                .handle_target_set_discover_targets(request, ctx)
                .await
        }
        Handler::TargetCreateTarget => connection.handle_target_create_target(request, ctx).await,
        Handler::TargetGetTargets => connection.handle_target_get_targets(request, ctx).await,
        Handler::TargetGetTargetInfo => {
            connection.handle_target_get_target_info(request, ctx).await
        }
        Handler::TargetAttachToBrowserTarget => {
            connection
                .handle_target_attach_to_browser_target(request, ctx)
                .await
        }
        Handler::TargetDetachFromTarget => {
            connection
                .handle_target_detach_from_target(request, ctx)
                .await
        }
        Handler::TargetSetAutoAttach => {
            connection.handle_target_set_auto_attach(request, ctx).await
        }
    }
}
