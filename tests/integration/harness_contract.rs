//! What a `Harness` promises: tmux-mcp serves only the harness's own tmux
//! server, and the `Shell` lives in the harness's own `HOME`, so nothing of
//! the developer's setup or history is in reach.

use crate::harness::Harness;

#[tokio::test]
async fn tmux_mcp_serves_only_the_harness_and_its_home() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let dir = h.dir().display().to_string();

    let reply = h.run(&shell, "echo $HOME").await;
    let row = h.list_panes().await.row(&shell);

    assert!(reply.contains(&format!("{dir}/home")), "{reply}");
    assert!(row.contains(&dir), "{row}");
}
