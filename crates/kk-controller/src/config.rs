use std::env;

#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// Container image for kk-connector deployments
    pub image_connector: String,
    /// Root data directory (PVC mount point)
    pub data_dir: String,
    /// Namespace the controller operates in
    pub namespace: String,
    /// Timeout in seconds for git clone operations
    pub skill_clone_timeout: u64,
    /// Name of the PVC to mount in connector deployments
    pub pvc_claim_name: String,
}

impl ControllerConfig {
    pub fn from_env() -> Self {
        let namespace = env::var("NAMESPACE").unwrap_or_else(|_| {
            // Auto-detect from in-cluster service account
            std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "default".into())
        });

        Self {
            image_connector: env::var("IMAGE_CONNECTOR")
                .unwrap_or_else(|_| "kk-connector:latest".into()),
            data_dir: env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()),
            namespace,
            skill_clone_timeout: env::var("SKILL_CLONE_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
            pvc_claim_name: env::var("PVC_CLAIM_NAME").unwrap_or_else(|_| "kk-data".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VARS: &[&str] = &[
        "IMAGE_CONNECTOR",
        "DATA_DIR",
        "NAMESPACE",
        "SKILL_CLONE_TIMEOUT",
        "PVC_CLAIM_NAME",
    ];

    /// SAFETY: env mutation is unsafe in Rust 2024; single serial test avoids races.
    unsafe fn clear_vars() {
        for var in VARS {
            unsafe { env::remove_var(var) };
        }
    }

    /// Consolidated env test — runs defaults, overrides, and invalid-timeout
    /// sequentially in one test to avoid parallel env races.
    #[test]
    fn test_config_from_env() {
        // SAFETY: test-only env manipulation, single thread
        unsafe {
            clear_vars();
        }

        // --- defaults ---
        let config = ControllerConfig::from_env();
        assert_eq!(config.image_connector, "kk-connector:latest");
        assert_eq!(config.data_dir, "/data");
        assert_eq!(config.namespace, "default");
        assert_eq!(config.skill_clone_timeout, 60);
        assert_eq!(config.pvc_claim_name, "kk-data");

        // --- overrides ---
        unsafe {
            env::set_var("IMAGE_CONNECTOR", "my-registry/kk-connector:v1.2.3");
            env::set_var("DATA_DIR", "/mnt/pvc");
            env::set_var("NAMESPACE", "production");
            env::set_var("SKILL_CLONE_TIMEOUT", "120");
            env::set_var("PVC_CLAIM_NAME", "my-pvc");
        }

        let config = ControllerConfig::from_env();
        assert_eq!(config.image_connector, "my-registry/kk-connector:v1.2.3");
        assert_eq!(config.data_dir, "/mnt/pvc");
        assert_eq!(config.namespace, "production");
        assert_eq!(config.skill_clone_timeout, 120);
        assert_eq!(config.pvc_claim_name, "my-pvc");

        // --- invalid timeout falls back to default ---
        unsafe {
            env::set_var("SKILL_CLONE_TIMEOUT", "not-a-number");
        }
        let config = ControllerConfig::from_env();
        assert_eq!(config.skill_clone_timeout, 60);

        // cleanup
        unsafe {
            clear_vars();
        }
    }
}
