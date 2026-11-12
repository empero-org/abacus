mod support;

use abacus_agent::{agent::TurnOptions, git_checkpoint::CheckpointManager};
use serde_json::json;
use std::fs;
use std::process::Command;
use std::sync::Arc;
use support::{Mock, calls, project, says, turn};

#[tokio::test]
async fn test_agent_turn_creates_checkpoint_automatically() {
    let project = project(&[("existing.txt", "initial content\n")]);
    let session_id = "test_session_1";

    let mgr = Arc::new(tokio::sync::Mutex::new(CheckpointManager::new(project.workspace.clone())));

    let mock = Mock::script([
        calls(&[
            ("c1", "write_file", json!({"path": "existing.txt", "content": "updated content\n"})),
            ("c2", "write_file", json!({"path": "created.txt", "content": "new file\n"})),
        ]),
        says("Done modifying files."),
    ])
    .await;

    let options = TurnOptions {
        session_id: Some(session_id.to_string()),
        checkpoint_mgr: Some(mgr.clone()),
        ..project.options()
    };

    let (_completed, _) =
        turn(project.provider(&mock), project.asks("Modify existing and create new file"), options, |_| {}).await;

    let locked = mgr.lock().await;
    let checkpoints = locked.list_checkpoints(session_id);
    assert_eq!(checkpoints.len(), 2, "expected turn checkpoint plus 1 per-step checkpoint");
    let chk = &checkpoints[0];
    assert_eq!(chk.turn_index, 1);
    assert_eq!(chk.step_index, 0);
    assert!(chk.touched_files.contains(&"existing.txt".to_string()), "existing.txt should be in touched_files");
    assert!(chk.created_files.contains(&"created.txt".to_string()), "created.txt should be in created_files");

    // Verify metadata was saved
    let meta_file = project.workspace.join(".abacus").join("sessions").join(session_id).join("checkpoints.json");
    assert!(meta_file.exists(), "checkpoints.json metadata should be written");
}

#[tokio::test]
async fn test_read_only_tools_do_not_create_checkpoint() {
    let project = project(&[("data.txt", "hello world\n")]);
    let session_id = "test_session_ro";

    let mgr = Arc::new(tokio::sync::Mutex::new(CheckpointManager::new(project.workspace.clone())));

    let mock = Mock::script([calls(&[("c1", "read_file", json!({"path": "data.txt"}))]), says("Read the file.")]).await;

    let options = TurnOptions {
        session_id: Some(session_id.to_string()),
        checkpoint_mgr: Some(mgr.clone()),
        ..project.options()
    };

    let (_completed, _) = turn(project.provider(&mock), project.asks("Read file"), options, |_| {}).await;

    let locked = mgr.lock().await;
    let checkpoints = locked.list_checkpoints(session_id);
    assert_eq!(checkpoints.len(), 0, "read-only tools should not trigger a checkpoint");
}

#[tokio::test]
async fn test_all_mutating_tools_tracked_and_rollback_in_git_repo() {
    let project = project(&[
        ("keep_me.txt", "user untouched\n"),
        ("edit_target.txt", "before edit\n"),
        ("delete_target.txt", "before delete\n"),
        ("move_src.txt", "before move\n"),
    ]);

    // Initialize git repository
    let git_run = |args: &[&str]| {
        let status = Command::new("git").args(args).current_dir(&project.workspace).status().unwrap();
        assert!(status.success(), "git command {:?} failed", args);
    };

    git_run(&["init"]);
    git_run(&["config", "user.name", "Tester"]);
    git_run(&["config", "user.email", "tester@example.com"]);
    git_run(&["config", "core.autocrlf", "false"]);
    git_run(&["add", "."]);
    git_run(&["commit", "-m", "Initial commit"]);

    let session_id = "git_session";
    let mgr = Arc::new(tokio::sync::Mutex::new(CheckpointManager::new(project.workspace.clone())));

    let patch_content = "--- /dev/null\n+++ b/patched.txt\n@@ -0,0 +1 @@\n+patched content\n";

    let mock = Mock::script([
        calls(&[
            (
                "c1",
                "edit_file",
                json!({"path": "edit_target.txt", "old_text": "before edit\n", "new_text": "after edit\n"}),
            ),
            ("c2", "delete_file", json!({"path": "delete_target.txt"})),
            ("c3", "move_file", json!({"source": "move_src.txt", "destination": "move_dest.txt"})),
            ("c4", "write_file", json!({"path": "new_created.txt", "content": "brand new\n"})),
            ("c5", "apply_patch", json!({"patch": patch_content})),
        ]),
        says("Completed all file mutations."),
    ])
    .await;

    let options = TurnOptions {
        session_id: Some(session_id.to_string()),
        checkpoint_mgr: Some(mgr.clone()),
        ..project.options()
    };

    let (_completed, _) = turn(project.provider(&mock), project.asks("Perform all mutations"), options, |_| {}).await;

    // Verify mutations happened
    assert_eq!(fs::read_to_string(project.workspace.join("edit_target.txt")).unwrap(), "after edit\n");
    assert!(!project.workspace.join("delete_target.txt").exists());
    assert!(!project.workspace.join("move_src.txt").exists());
    assert!(project.workspace.join("move_dest.txt").exists());
    assert!(project.workspace.join("new_created.txt").exists());
    assert!(project.workspace.join("patched.txt").exists());

    // Verify checkpoint details
    let mut locked = mgr.lock().await;
    let checkpoints = locked.list_checkpoints(session_id);
    assert_eq!(checkpoints.len(), 5, "expected turn checkpoint plus 4 per-step checkpoints");
    let chk = &checkpoints[0];
    assert_eq!(chk.step_index, 0);
    assert!(!chk.is_fallback, "should use git shadow commit");
    assert!(chk.commit_sha.is_some());

    assert!(chk.touched_files.contains(&"edit_target.txt".to_string()));
    assert!(chk.touched_files.contains(&"delete_target.txt".to_string()));
    assert!(chk.touched_files.contains(&"move_src.txt".to_string()));

    assert!(chk.created_files.contains(&"move_dest.txt".to_string()));
    assert!(chk.created_files.contains(&"new_created.txt".to_string()));
    assert!(chk.created_files.contains(&"patched.txt".to_string()));

    // Now test rollback!
    let restored = locked.rollback(session_id, &chk.id).unwrap();
    assert!(!restored.is_empty());

    // Verify disk state after rollback:
    assert_eq!(
        fs::read_to_string(project.workspace.join("edit_target.txt")).unwrap().replace("\r\n", "\n"),
        "before edit\n"
    );
    assert!(project.workspace.join("delete_target.txt").exists());
    assert_eq!(
        fs::read_to_string(project.workspace.join("delete_target.txt")).unwrap().replace("\r\n", "\n"),
        "before delete\n"
    );
    assert!(project.workspace.join("move_src.txt").exists());
    assert_eq!(
        fs::read_to_string(project.workspace.join("move_src.txt")).unwrap().replace("\r\n", "\n"),
        "before move\n"
    );

    // Created files removed:
    assert!(!project.workspace.join("move_dest.txt").exists());
    assert!(!project.workspace.join("new_created.txt").exists());
    assert!(!project.workspace.join("patched.txt").exists());

    // Untouched file remains:
    assert_eq!(
        fs::read_to_string(project.workspace.join("keep_me.txt")).unwrap().replace("\r\n", "\n"),
        "user untouched\n"
    );
}
