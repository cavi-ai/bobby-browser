#[path = "../../gateway-transport/test-support/shared_stdio.rs"]
mod support;

#[tokio::test]
async fn owner_shutdown_exits_even_when_the_host_keeps_stdin_open() {
    support::owner_close_exits_with_open_stdin(env!("CARGO_BIN_EXE_acp-gateway")).await;
}
