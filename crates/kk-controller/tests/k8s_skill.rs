mod mock_k8s;

use mock_k8s::*;
use std::sync::Arc;

use kk_controller::config::ControllerConfig;
use kk_controller::reconcilers::skill::{self, Ctx};

fn test_config(data_dir: &str) -> ControllerConfig {
    ControllerConfig {
        image_connector: "kk-connector:test".into(),
        data_dir: data_dir.into(),
        namespace: "test-ns".into(),
        skill_clone_timeout: 5,
        pvc_claim_name: "kk-data".into(),
    }
}

/// Skill without finalizer → finalizer adds itself via PATCH, returns await_change.
#[tokio::test]
async fn skill_without_finalizer_adds_it() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();

    let (client, records) = mock_client(|method, path, _body| {
        // PATCH to add finalizer
        if method == "PATCH" && path.contains("/skills/") && !path.contains("/status") {
            return (
                200,
                skill_response("my-skill", "test-ns", "owner/repo/path"),
            );
        }
        (404, not_found_response("unknown"))
    });

    let ctx = Arc::new(Ctx {
        client,
        config: test_config(&data_dir),
    });
    // No finalizer set
    let skill = Arc::new(skill_cr("my-skill", "test-ns", "owner/repo/path", false));

    let action = skill::reconcile(skill, ctx).await.unwrap();

    // Should return await_change (finalizer was just added, reconcile will re-trigger)
    assert_eq!(action, kube::runtime::controller::Action::await_change());

    // Verify a PATCH was sent to add the finalizer
    let reqs = records.lock().unwrap();
    let finalizer_patch = reqs
        .iter()
        .find(|r| r.method == "PATCH" && r.path.contains("/skills/") && !r.path.contains("/status"))
        .unwrap();
    // The body should be a JSON Patch adding the finalizer
    assert!(
        finalizer_patch
            .body
            .to_string()
            .contains("skills.kk.io/cleanup"),
        "patch should add finalizer: {:?}",
        finalizer_patch.body
    );
}

/// Skill with finalizer + invalid source → status Error, requeue 300s.
#[tokio::test]
async fn skill_invalid_source_sets_error_status() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();

    let (client, records) = mock_client(|method, path, _body| {
        // PATCH status for error
        if method == "PATCH" && path.contains("/skills/") && path.contains("/status") {
            return (200, skill_response("bad-skill", "test-ns", "invalid"));
        }
        (404, not_found_response("unknown"))
    });

    let ctx = Arc::new(Ctx {
        client,
        config: test_config(&data_dir),
    });
    // With finalizer, so apply_skill runs; invalid source (only 1 segment)
    let skill = Arc::new(skill_cr("bad-skill", "test-ns", "invalid", true));

    let action = skill::reconcile(skill, ctx).await.unwrap();

    // Invalid source → requeue after 300s
    assert_eq!(
        action,
        kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(300))
    );

    // Verify status was patched with Error
    let reqs = records.lock().unwrap();
    let status_patch = reqs
        .iter()
        .find(|r| r.method == "PATCH" && r.path.contains("/status"))
        .unwrap();
    let phase = status_patch.body["status"]["phase"].as_str().unwrap();
    assert_eq!(phase, "Error");
    let message = status_patch.body["status"]["message"].as_str().unwrap();
    assert!(
        message.contains("expected owner/repo/path"),
        "message: {message}"
    );
}

/// Skill with finalizer + valid source but git clone fails → status Error.
#[tokio::test]
async fn skill_git_clone_failure_sets_error() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();

    let (client, records) = mock_client(|method, path, _body| {
        // PATCH status (first for "Fetching", then for "Error")
        if method == "PATCH" && path.contains("/skills/") && path.contains("/status") {
            return (
                200,
                skill_response(
                    "clone-fail",
                    "test-ns",
                    "nonexistent-owner/nonexistent-repo/path",
                ),
            );
        }
        (404, not_found_response("unknown"))
    });

    let ctx = Arc::new(Ctx {
        client,
        config: test_config(&data_dir),
    });
    // Valid source format but repo doesn't exist
    let skill = Arc::new(skill_cr(
        "clone-fail",
        "test-ns",
        "nonexistent-owner-kk-test/nonexistent-repo-kk-test/path",
        true,
    ));

    let result = skill::reconcile(skill, ctx).await;

    // Git clone should fail — either as an error or as a status update
    // The reconciler returns Err(Error::GitClone(...)) when git clone fails
    assert!(result.is_err(), "expected git clone to fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("git clone failed") || err.contains("finalizer"),
        "error should mention git clone: {err}"
    );

    // Verify status was patched (at least "Fetching")
    let reqs = records.lock().unwrap();
    let status_patches: Vec<_> = reqs
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.contains("/status"))
        .collect();
    assert!(
        !status_patches.is_empty(),
        "should have at least one status patch"
    );
}

/// Skill being deleted (has deletion timestamp + finalizer) → cleanup runs, finalizer removed.
#[tokio::test]
async fn skill_cleanup_on_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();

    // Create the skill directory that cleanup should remove
    let skill_dir = tmp.path().join("skills").join("deleteme");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "test").unwrap();
    assert!(skill_dir.exists());

    let (client, records) = mock_client(|method, path, _body| {
        // PATCH to remove finalizer after cleanup
        if method == "PATCH" && path.contains("/skills/") && !path.contains("/status") {
            return (
                200,
                skill_response("deleteme", "test-ns", "owner/repo/path"),
            );
        }
        (404, not_found_response("unknown"))
    });

    let ctx = Arc::new(Ctx {
        client,
        config: test_config(&data_dir),
    });

    // Skill with finalizer AND deletion timestamp → triggers Cleanup
    use kk_controller::crd::{Skill, SkillSpec};
    use kube::api::ObjectMeta;

    let skill = Arc::new(Skill {
        metadata: ObjectMeta {
            name: Some("deleteme".into()),
            namespace: Some("test-ns".into()),
            uid: Some("skill-uid-deleteme".into()),
            finalizers: Some(vec!["skills.kk.io/cleanup".into()]),
            deletion_timestamp: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                chrono::Utc::now(),
            )),
            ..Default::default()
        },
        spec: SkillSpec {
            source: "owner/repo/path".into(),
        },
        status: None,
    });

    let action = skill::reconcile(skill, ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());

    // Verify skill directory was removed
    assert!(!skill_dir.exists(), "skill directory should be cleaned up");

    // Verify finalizer removal PATCH was sent
    let reqs = records.lock().unwrap();
    let finalizer_patch = reqs
        .iter()
        .find(|r| r.method == "PATCH" && r.path.contains("/skills/") && !r.path.contains("/status"))
        .unwrap();
    assert!(
        finalizer_patch
            .body
            .to_string()
            .contains("skills.kk.io/cleanup"),
        "patch should reference finalizer: {:?}",
        finalizer_patch.body
    );
}
