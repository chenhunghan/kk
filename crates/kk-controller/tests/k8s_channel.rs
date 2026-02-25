mod mock_k8s;

use mock_k8s::*;
use std::sync::Arc;

use kk_controller::config::ControllerConfig;
use kk_controller::reconcilers::channel::{self, Ctx};
use serde_json::{Value, json};

fn test_config(data_dir: &str) -> ControllerConfig {
    ControllerConfig {
        image_connector: "kk-connector:test".into(),
        data_dir: data_dir.into(),
        namespace: "test-ns".into(),
        skill_clone_timeout: 60,
        pvc_claim_name: "kk-data".into(),
    }
}

/// Channel reconcile: Secret exists, Deployment becomes ready → status Connected.
#[tokio::test]
async fn channel_deployment_ready_sets_connected() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();

    let (client, records) = mock_client(|method, path, _body| {
        // GET /api/v1/namespaces/test-ns/secrets/tg-secret
        if method == "GET" && path.contains("/secrets/") {
            return (200, secret_response("tg-secret", "test-ns"));
        }
        // PATCH /apis/apps/v1/namespaces/test-ns/deployments/connector-my-channel
        if method == "PATCH" && path.contains("/deployments/") && !path.contains("/status") {
            return (
                200,
                deployment_response("connector-my-channel", "test-ns", 1),
            );
        }
        // PATCH /apis/kk.io/v1alpha1/namespaces/test-ns/channels/my-channel/status
        if method == "PATCH" && path.contains("/channels/") && path.contains("/status") {
            return (
                200,
                channel_response("my-channel", "test-ns", "tg-secret", "telegram"),
            );
        }
        (404, not_found_response("unknown"))
    });

    let ctx = Arc::new(Ctx {
        client,
        config: test_config(&data_dir),
    });
    let channel = Arc::new(channel_cr(
        "my-channel",
        "test-ns",
        "tg-secret",
        "telegram",
        None,
    ));

    let action = channel::reconcile(channel, ctx).await.unwrap();

    // Should requeue after 300s (success)
    assert_eq!(
        action,
        kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(300))
    );

    // Verify outbox dir was created
    assert!(tmp.path().join("outbox").join("my-channel").exists());

    // Verify status patch contained "Connected"
    let reqs = records.lock().unwrap();
    let status_patch = reqs
        .iter()
        .find(|r| r.method == "PATCH" && r.path.contains("/status"))
        .unwrap();
    let phase = status_patch.body["status"]["phase"].as_str().unwrap();
    assert_eq!(phase, "Connected");
}

/// Channel reconcile: Secret missing → status Error, requeue 30s.
#[tokio::test]
async fn channel_missing_secret_sets_error() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();

    let (client, records) = mock_client(|method, path, _body| {
        // GET Secret → 404
        if method == "GET" && path.contains("/secrets/") {
            return (404, not_found_response("secrets \"missing-secret\""));
        }
        // PATCH Channel status
        if method == "PATCH" && path.contains("/channels/") && path.contains("/status") {
            return (
                200,
                channel_response("my-channel", "test-ns", "tg-secret", "telegram"),
            );
        }
        (404, not_found_response("unknown"))
    });

    let ctx = Arc::new(Ctx {
        client,
        config: test_config(&data_dir),
    });
    let channel = Arc::new(channel_cr(
        "my-channel",
        "test-ns",
        "missing-secret",
        "telegram",
        None,
    ));

    let action = channel::reconcile(channel, ctx).await.unwrap();

    // Should requeue after 30s (secret missing)
    assert_eq!(
        action,
        kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(30))
    );

    // Verify status patch contained "Error"
    let reqs = records.lock().unwrap();
    let status_patch = reqs
        .iter()
        .find(|r| r.method == "PATCH" && r.path.contains("/status"))
        .unwrap();
    let phase = status_patch.body["status"]["phase"].as_str().unwrap();
    assert_eq!(phase, "Error");
    let message = status_patch.body["status"]["message"].as_str().unwrap();
    assert!(
        message.contains("not found"),
        "message should mention not found: {message}"
    );
}

/// Channel reconcile: Deployment not ready → status Pending.
#[tokio::test]
async fn channel_deployment_pending_sets_pending() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();

    let (client, records) = mock_client(|method, path, _body| {
        if method == "GET" && path.contains("/secrets/") {
            return (200, secret_response("tg-secret", "test-ns"));
        }
        if method == "PATCH" && path.contains("/deployments/") && !path.contains("/status") {
            // Return deployment with 0 ready replicas, no error conditions
            return (
                200,
                deployment_response("connector-my-channel", "test-ns", 0),
            );
        }
        if method == "PATCH" && path.contains("/channels/") && path.contains("/status") {
            return (
                200,
                channel_response("my-channel", "test-ns", "tg-secret", "telegram"),
            );
        }
        (404, not_found_response("unknown"))
    });

    let ctx = Arc::new(Ctx {
        client,
        config: test_config(&data_dir),
    });
    let channel = Arc::new(channel_cr(
        "my-channel",
        "test-ns",
        "tg-secret",
        "telegram",
        None,
    ));

    let action = channel::reconcile(channel, ctx).await.unwrap();
    assert_eq!(
        action,
        kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(300))
    );

    let reqs = records.lock().unwrap();
    let status_patch = reqs
        .iter()
        .find(|r| r.method == "PATCH" && r.path.contains("/status"))
        .unwrap();
    assert_eq!(
        status_patch.body["status"]["phase"].as_str().unwrap(),
        "Pending"
    );
}

/// Channel reconcile: Deployment CrashLoopBackOff → status Error.
#[tokio::test]
async fn channel_deployment_crashloop_sets_error() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();

    let (client, records) = mock_client(|method, path, _body| {
        if method == "GET" && path.contains("/secrets/") {
            return (200, secret_response("tg-secret", "test-ns"));
        }
        if method == "PATCH" && path.contains("/deployments/") && !path.contains("/status") {
            return (
                200,
                deployment_crashloop_response("connector-my-channel", "test-ns"),
            );
        }
        if method == "PATCH" && path.contains("/channels/") && path.contains("/status") {
            return (
                200,
                channel_response("my-channel", "test-ns", "tg-secret", "telegram"),
            );
        }
        (404, not_found_response("unknown"))
    });

    let ctx = Arc::new(Ctx {
        client,
        config: test_config(&data_dir),
    });
    let channel = Arc::new(channel_cr(
        "my-channel",
        "test-ns",
        "tg-secret",
        "telegram",
        None,
    ));

    let action = channel::reconcile(channel, ctx).await.unwrap();
    assert_eq!(
        action,
        kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(300))
    );

    let reqs = records.lock().unwrap();
    let status_patch = reqs
        .iter()
        .find(|r| r.method == "PATCH" && r.path.contains("/status"))
        .unwrap();
    assert_eq!(
        status_patch.body["status"]["phase"].as_str().unwrap(),
        "Error"
    );
    let message = status_patch.body["status"]["message"].as_str().unwrap();
    assert!(message.contains("CrashLoopBackOff"), "message: {message}");
}

/// Channel reconcile: spec.config keys appear as CONFIG_* env vars in Deployment.
#[tokio::test]
async fn channel_config_merged_as_env_vars() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();

    let deploy_body: Arc<std::sync::Mutex<Option<Value>>> = Arc::new(std::sync::Mutex::new(None));
    let deploy_body_capture = deploy_body.clone();

    let (client, _records) = mock_client(move |method, path, body| {
        if method == "GET" && path.contains("/secrets/") {
            return (200, secret_response("slack-secret", "test-ns"));
        }
        if method == "PATCH" && path.contains("/deployments/") && !path.contains("/status") {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            *deploy_body_capture.lock().unwrap() = Some(parsed);
            return (
                200,
                deployment_response("connector-slack-bot", "test-ns", 1),
            );
        }
        if method == "PATCH" && path.contains("/channels/") && path.contains("/status") {
            return (
                200,
                channel_response("slack-bot", "test-ns", "slack-secret", "slack"),
            );
        }
        (404, not_found_response("unknown"))
    });

    let config_val = json!({
        "botToken": "xoxb-test-123",
        "allowedChannels": "general,random"
    });
    let ctx = Arc::new(Ctx {
        client,
        config: test_config(&data_dir),
    });
    let channel = Arc::new(channel_cr(
        "slack-bot",
        "test-ns",
        "slack-secret",
        "slack",
        Some(config_val),
    ));

    let _action = channel::reconcile(channel, ctx).await.unwrap();

    let body = deploy_body.lock().unwrap();
    let body = body
        .as_ref()
        .expect("deployment patch body should be captured");

    // Find CONFIG_BOT_TOKEN and CONFIG_ALLOWED_CHANNELS in the container env
    let containers = &body["spec"]["template"]["spec"]["containers"];
    let env = containers[0]["env"]
        .as_array()
        .expect("env should be an array");

    let env_map: std::collections::HashMap<&str, &str> = env
        .iter()
        .filter_map(|e| Some((e["name"].as_str()?, e["value"].as_str()?)))
        .collect();

    assert_eq!(env_map.get("CONFIG_BOT_TOKEN"), Some(&"xoxb-test-123"));
    assert_eq!(
        env_map.get("CONFIG_ALLOWED_CHANNELS"),
        Some(&"general,random")
    );
}

/// Channel reconcile: Deployment has correct ownerReference.
#[tokio::test]
async fn channel_deployment_has_owner_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();

    let deploy_body: Arc<std::sync::Mutex<Option<Value>>> = Arc::new(std::sync::Mutex::new(None));
    let deploy_body_capture = deploy_body.clone();

    let (client, _records) = mock_client(move |method, path, body| {
        if method == "GET" && path.contains("/secrets/") {
            return (200, secret_response("tg-secret", "test-ns"));
        }
        if method == "PATCH" && path.contains("/deployments/") && !path.contains("/status") {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            *deploy_body_capture.lock().unwrap() = Some(parsed);
            return (
                200,
                deployment_response("connector-my-channel", "test-ns", 1),
            );
        }
        if method == "PATCH" && path.contains("/channels/") && path.contains("/status") {
            return (
                200,
                channel_response("my-channel", "test-ns", "tg-secret", "telegram"),
            );
        }
        (404, not_found_response("unknown"))
    });

    let ctx = Arc::new(Ctx {
        client,
        config: test_config(&data_dir),
    });
    let channel = Arc::new(channel_cr(
        "my-channel",
        "test-ns",
        "tg-secret",
        "telegram",
        None,
    ));

    let _action = channel::reconcile(channel, ctx).await.unwrap();

    let body = deploy_body.lock().unwrap();
    let body = body
        .as_ref()
        .expect("deployment patch body should be captured");

    let owner_refs = body["metadata"]["ownerReferences"]
        .as_array()
        .expect("ownerReferences");
    assert_eq!(owner_refs.len(), 1);
    assert_eq!(owner_refs[0]["kind"].as_str().unwrap(), "Channel");
    assert_eq!(owner_refs[0]["name"].as_str().unwrap(), "my-channel");
    assert!(owner_refs[0]["controller"].as_bool().unwrap());
    assert!(owner_refs[0]["blockOwnerDeletion"].as_bool().unwrap());
}

/// Channel reconcile: outbox directory is created on disk.
#[tokio::test]
async fn channel_creates_outbox_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();

    // Verify outbox dir doesn't exist yet
    assert!(!tmp.path().join("outbox").join("my-channel").exists());

    let (client, _records) = mock_client(|method, path, _body| {
        if method == "GET" && path.contains("/secrets/") {
            return (200, secret_response("tg-secret", "test-ns"));
        }
        if method == "PATCH" && path.contains("/deployments/") && !path.contains("/status") {
            return (
                200,
                deployment_response("connector-my-channel", "test-ns", 1),
            );
        }
        if method == "PATCH" && path.contains("/channels/") && path.contains("/status") {
            return (
                200,
                channel_response("my-channel", "test-ns", "tg-secret", "telegram"),
            );
        }
        (404, not_found_response("unknown"))
    });

    let ctx = Arc::new(Ctx {
        client,
        config: test_config(&data_dir),
    });
    let channel = Arc::new(channel_cr(
        "my-channel",
        "test-ns",
        "tg-secret",
        "telegram",
        None,
    ));

    let _action = channel::reconcile(channel, ctx).await.unwrap();

    // Verify outbox directory was created
    assert!(tmp.path().join("outbox").join("my-channel").exists());
}
