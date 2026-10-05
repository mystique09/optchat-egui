use optchat::{integrations::Integrations, local_tools};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn files_and_approval() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note.txt");
    let cancel = CancellationToken::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let integrations = Integrations::empty();
    let input = json!({"path":path,"content":"hello 🌍"});
    let call = integrations.call("write_file", input.clone(), &tx, &cancel);
    let deny = async {
        rx.recv().await.unwrap().answer.send(false).unwrap();
    };
    let (result, ()) = tokio::join!(call, deny);
    assert!(result.error);
    assert!(!path.exists());
    let call = integrations.call("write_file", input.clone(), &tx, &cancel);
    let allow = async {
        rx.recv().await.unwrap().answer.send(true).unwrap();
    };
    let (result, ()) = tokio::join!(call, allow);
    assert!(!result.error, "{}", result.text);
    assert!(local_tools::call("write_file", input, &cancel).await.error);
    let read = local_tools::call("read_file", json!({"path":path}), &cancel).await;
    assert!(!read.error);
    assert!(read.text.contains("hello 🌍"));
    assert!(
        local_tools::call("read_file", json!({"path":"relative"}), &cancel)
            .await
            .error
    );
    assert!(
        !local_tools::call("list_directory", json!({"path":dir.path()}), &cancel)
            .await
            .error
    );
}

#[tokio::test]
async fn shell_exit_output_timeout_and_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let run = |command: &str, timeout| json!({"command":command,"cwd":dir.path(),"timeout_seconds":timeout});
    let result =
        local_tools::call("shell", run("printf hello; printf error >&2", 5), &cancel).await;
    assert!(!result.error);
    assert!(result.text.contains("hello") && result.text.contains("error"));
    assert!(
        local_tools::call("shell", run("exit 7", 5), &cancel)
            .await
            .error
    );
    let result = local_tools::call("shell", run("yes x | head -c 100000", 5), &cancel).await;
    assert!(!result.error);
    assert!(result.text.contains("truncated"));
    assert!(result.text.len() < 70000);
    let result = local_tools::call("shell", run("sleep 2; touch survived", 1), &cancel).await;
    assert!(result.error && result.text.contains("timed out"));
    let operation = local_tools::call("shell", run("sleep 2; touch survived", 5), &cancel);
    let stop = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(operation, stop);
    assert!(result.error && result.text.contains("canceled"));
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(!dir.path().join("survived").exists());
}
