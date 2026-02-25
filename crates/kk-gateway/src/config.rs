use std::env;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GatewayConfig {
    pub data_dir: String,
    pub namespace: String,
    pub image_agent: String,
    pub api_keys_secret: String,
    pub job_active_deadline: u64,
    pub job_ttl_after_finished: u64,
    pub job_idle_timeout: u64,
    pub job_max_turns: u64,
    pub job_cpu_request: String,
    pub job_cpu_limit: String,
    pub job_memory_request: String,
    pub job_memory_limit: String,
    pub inbound_poll_interval_ms: u64,
    pub results_poll_interval_ms: u64,
    pub cleanup_interval_ms: u64,
    pub stale_message_timeout: u64,
    pub results_archive_ttl: u64,
    pub pvc_claim_name: String,
    pub state_reload_interval_ms: u64,
    pub agent_type: String,
}

impl GatewayConfig {
    pub fn from_env() -> Self {
        Self {
            data_dir: env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()),
            namespace: env::var("NAMESPACE").unwrap_or_else(|_| "default".into()),
            image_agent: env::var("IMAGE_AGENT").unwrap_or_else(|_| "kk-agent:latest".into()),
            api_keys_secret: env::var("API_KEYS_SECRET").unwrap_or_else(|_| "kk-api-keys".into()),
            job_active_deadline: env_u64("JOB_ACTIVE_DEADLINE", 300),
            job_ttl_after_finished: env_u64("JOB_TTL_AFTER_FINISHED", 300),
            job_idle_timeout: env_u64("JOB_IDLE_TIMEOUT", 120),
            job_max_turns: env_u64("JOB_MAX_TURNS", 25),
            job_cpu_request: env::var("JOB_CPU_REQUEST").unwrap_or_else(|_| "250m".into()),
            job_cpu_limit: env::var("JOB_CPU_LIMIT").unwrap_or_else(|_| "1".into()),
            job_memory_request: env::var("JOB_MEMORY_REQUEST").unwrap_or_else(|_| "256Mi".into()),
            job_memory_limit: env::var("JOB_MEMORY_LIMIT").unwrap_or_else(|_| "1Gi".into()),
            inbound_poll_interval_ms: env_u64("INBOUND_POLL_INTERVAL", 2000),
            results_poll_interval_ms: env_u64("RESULTS_POLL_INTERVAL", 2000),
            cleanup_interval_ms: env_u64("CLEANUP_INTERVAL", 60000),
            stale_message_timeout: env_u64("STALE_MESSAGE_TIMEOUT", 300),
            results_archive_ttl: env_u64("RESULTS_ARCHIVE_TTL", 86400),
            pvc_claim_name: env::var("PVC_CLAIM_NAME").unwrap_or_else(|_| "kk-data".into()),
            state_reload_interval_ms: env_u64("STATE_RELOAD_INTERVAL", 30000),
            agent_type: env::var("AGENT_TYPE").unwrap_or_else(|_| "claude".into()),
        }
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gateway_config_from_env() {
        unsafe { std::env::set_var("AGENT_TYPE", "gemini") };
        let config = GatewayConfig::from_env();
        assert_eq!(config.agent_type, "gemini");
        
        unsafe { std::env::set_var("AGENT_TYPE", "codex") };
        let config = GatewayConfig::from_env();
        assert_eq!(config.agent_type, "codex");
    }
}
