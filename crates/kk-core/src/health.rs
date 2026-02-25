//! Minimal HTTP health server using raw TCP.
//! Replaces axum for trivial /healthz, /readyz, and optional extra endpoints.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::error;

type BoxHandler = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = String> + Send>> + Send + Sync>;

/// A route: (path, handler returning response body).
pub struct Route {
    pub path: String,
    handler: BoxHandler,
}

/// Builder for configuring and running a health server.
pub struct HealthServer {
    port: u16,
    routes: Vec<Route>,
}

impl HealthServer {
    /// Create a health server on the given port with default /healthz and /readyz routes.
    pub fn new(port: u16) -> Self {
        let mut server = Self {
            port,
            routes: Vec::new(),
        };
        server.route("/healthz", || async { "ok".into() });
        server.route("/readyz", || async { "ok".into() });
        server
    }

    /// Add a route with an async handler.
    pub fn route<F, Fut>(&mut self, path: &str, handler: F) -> &mut Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = String> + Send + 'static,
    {
        self.routes.push(Route {
            path: path.into(),
            handler: Arc::new(move || Box::pin(handler())),
        });
        self
    }

    /// Run the server (blocks forever).
    pub async fn run(self) -> std::io::Result<()> {
        let listener = TcpListener::bind(format!("0.0.0.0:{}", self.port)).await?;
        let routes: Arc<Vec<Route>> = Arc::new(self.routes);

        loop {
            let (mut stream, _) = listener.accept().await?;
            let routes = routes.clone();

            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = match stream.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };

                let request = String::from_utf8_lossy(&buf[..n]);
                let path = match request.split_whitespace().nth(1) {
                    Some(p) => p,
                    None => return,
                };

                // Find matching route
                let response = if let Some(route) = routes.iter().find(|r| r.path == path) {
                    let body = (route.handler)().await;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found".into()
                };

                if let Err(e) = stream.write_all(response.as_bytes()).await {
                    error!(error = %e, "health server write error");
                }
            });
        }
    }
}
