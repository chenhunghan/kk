#![allow(dead_code)]

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::service_fn;

/// Record of a single HTTP request the mock received.
#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub method: String,
    pub path: String,
    pub body: Value,
}

/// Build a `kube::Client` backed by a closure that handles API requests.
///
/// The handler receives `(method, path, body_bytes)` and returns `(status_code, response_json)`.
/// All requests are recorded in the returned `Vec<RequestRecord>` for assertions.
pub fn mock_client<F>(handler: F) -> (kube::Client, Arc<Mutex<Vec<RequestRecord>>>)
where
    F: Fn(&str, &str, &[u8]) -> (u16, Value) + Send + Sync + 'static,
{
    let records: Arc<Mutex<Vec<RequestRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let records_clone = records.clone();
    let handler = Arc::new(handler);

    let service = service_fn(move |req: Request<kube::client::Body>| {
        let handler = handler.clone();
        let records = records_clone.clone();
        async move {
            let method = req.method().to_string();
            let path = req.uri().path().to_string();
            let query = req.uri().query().unwrap_or("");
            let full_path = if query.is_empty() {
                path.clone()
            } else {
                format!("{path}?{query}")
            };

            let body_bytes = http_body_util::BodyExt::collect(req.into_body())
                .await
                .unwrap()
                .to_bytes();

            let body_json: Value = if body_bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body_bytes).unwrap_or(Value::Null)
            };

            records.lock().unwrap().push(RequestRecord {
                method: method.clone(),
                path: full_path.clone(),
                body: body_json.clone(),
            });

            let (status, response_body) = handler(&method, &full_path, &body_bytes);

            let response = Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(
                    serde_json::to_vec(&response_body).unwrap(),
                )))
                .unwrap();

            Ok::<_, std::convert::Infallible>(response)
        }
    });

    let client = kube::Client::new(service, "test-ns");
    (client, records)
}

/// Build a K8s Secret JSON response.
pub fn secret_response(name: &str, namespace: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "uid": format!("secret-uid-{name}"),
            "resourceVersion": "1"
        },
        "data": {}
    })
}

/// Build a K8s Deployment JSON response.
pub fn deployment_response(name: &str, namespace: &str, ready_replicas: i32) -> Value {
    let mut val = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "uid": format!("deploy-uid-{name}"),
            "resourceVersion": "1"
        },
        "spec": {
            "replicas": 1,
            "selector": { "matchLabels": { "app": "kk-connector" } },
            "template": {
                "metadata": { "labels": { "app": "kk-connector" } },
                "spec": { "containers": [{ "name": "connector", "image": "test" }] }
            }
        },
        "status": {
            "replicas": 1,
            "readyReplicas": ready_replicas,
            "availableReplicas": ready_replicas
        }
    });
    if ready_replicas == 0 {
        val["status"]
            .as_object_mut()
            .unwrap()
            .remove("readyReplicas");
    }
    val
}

/// Build a Deployment response with an error condition (CrashLoopBackOff).
pub fn deployment_crashloop_response(name: &str, namespace: &str) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "uid": format!("deploy-uid-{name}"),
            "resourceVersion": "1"
        },
        "spec": {
            "replicas": 1,
            "selector": { "matchLabels": { "app": "kk-connector" } },
            "template": {
                "metadata": { "labels": { "app": "kk-connector" } },
                "spec": { "containers": [{ "name": "connector", "image": "test" }] }
            }
        },
        "status": {
            "replicas": 1,
            "conditions": [{
                "type": "Available",
                "status": "False",
                "message": "Deployment does not have minimum availability: CrashLoopBackOff"
            }]
        }
    })
}

/// Build a Channel CR JSON response (for status patch responses).
pub fn channel_response(
    name: &str,
    namespace: &str,
    secret_ref: &str,
    channel_type: &str,
) -> Value {
    json!({
        "apiVersion": "kk.io/v1alpha1",
        "kind": "Channel",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "uid": format!("channel-uid-{name}"),
            "resourceVersion": "2"
        },
        "spec": {
            "type": channel_type,
            "secretRef": secret_ref
        }
    })
}

/// Build a K8s API 404 Not Found error response.
pub fn not_found_response(resource: &str) -> Value {
    json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("{resource} not found"),
        "reason": "NotFound",
        "code": 404
    })
}

/// Build a Skill CR JSON response (for PATCH responses).
pub fn skill_response(name: &str, namespace: &str, source: &str) -> Value {
    json!({
        "apiVersion": "kk.io/v1alpha1",
        "kind": "Skill",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "uid": format!("skill-uid-{name}"),
            "resourceVersion": "2",
            "finalizers": ["skills.kk.io/cleanup"]
        },
        "spec": {
            "source": source
        }
    })
}

/// Build a Skill CR object for test input.
pub fn skill_cr(
    name: &str,
    namespace: &str,
    source: &str,
    with_finalizer: bool,
) -> kk_controller::crd::Skill {
    use kk_controller::crd::{Skill, SkillSpec};
    use kube::api::ObjectMeta;

    let finalizers = if with_finalizer {
        Some(vec!["skills.kk.io/cleanup".into()])
    } else {
        None
    };

    Skill {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some(namespace.into()),
            uid: Some(format!("skill-uid-{name}")),
            finalizers,
            ..Default::default()
        },
        spec: SkillSpec {
            source: source.into(),
        },
        status: None,
    }
}

/// Build a Channel CR object for test input.
pub fn channel_cr(
    name: &str,
    namespace: &str,
    secret_ref: &str,
    channel_type: &str,
    config: Option<Value>,
) -> kk_controller::crd::Channel {
    use kk_controller::crd::{Channel, ChannelSpec};
    use kube::api::ObjectMeta;

    Channel {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some(namespace.into()),
            uid: Some(format!("channel-uid-{name}")),
            ..Default::default()
        },
        spec: ChannelSpec {
            channel_type: channel_type.into(),
            secret_ref: secret_ref.into(),
            config,
        },
        status: None,
    }
}
