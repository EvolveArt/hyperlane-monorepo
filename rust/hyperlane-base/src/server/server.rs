use axum::{
    http::{Response, StatusCode},
    routing::get,
    Router,
};
use bytes::Bytes;
use hyper::Body;
use prometheus::{Encoder, Registry};
use std::{net::SocketAddr, sync::Arc};
use tokio::task::JoinHandle;
use tracing::warn;

/// A server that serves metrics in OpenMetrics format.
pub struct Server {
    listen_port: u16,
    registry: Registry,
}

impl Server {
    /// Create a new server instance.
    pub fn new(listen_port: u16, registry: Registry) -> Self {
        Self {
            listen_port,
            registry,
        }
    }

    /// Run an HTTP server serving OpenMetrics format reports on `/metrics`
    pub fn run(self: Arc<Self>) -> JoinHandle<()> {
        let port = self.listen_port;
        tracing::info!(port, "starting prometheus server on 0.0.0.0");

        let server_clone = self.clone();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/metrics",
                get(move || {
                    let server = server_clone.clone();
                    async move {
                        match server.gather() {
                            Ok(metrics) => Response::builder()
                                .header("Content-Type", "text/plain; charset=utf-8")
                                .body(Body::from(metrics))
                                .unwrap(),
                            Err(_) => Response::builder()
                                .status(StatusCode::NOT_FOUND)
                                .body(Body::from("Failed to encode metrics"))
                                .unwrap(),
                        }
                    }
                }),
            );

            let addr = SocketAddr::from(([0, 0, 0, 0], port));
            axum::Server::bind(&addr)
                .serve(app.into_make_service())
                .await
                .expect("Failed to start server");
        })
    }

    /// Gather available metrics into an encoded (plaintext, OpenMetrics format)
    /// report.
    pub fn gather(&self) -> prometheus::Result<Vec<u8>> {
        let collected_metrics = self.registry.gather();
        let mut out_buf = Vec::with_capacity(1024 * 64);
        let encoder = prometheus::TextEncoder::new();
        encoder.encode(&collected_metrics, &mut out_buf)?;
        Ok(out_buf)
    }
}

#[cfg(test)]
mod tests {
    // use hyper::server;
    use prometheus::{Counter, Registry};
    use reqwest;

    use super::*;

    #[tokio::test]
    async fn test_metrics_endpoint() {
        let mock_registry = Registry::new();
        let counter = Counter::new("expected_metric_content", "test123").unwrap();
        mock_registry.register(Box::new(counter.clone())).unwrap();
        counter.inc();

        let server = Server::new(8080, mock_registry);
        let server = Arc::new(server);
        let _run_server = server.run();

        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

        let client = reqwest::Client::new();
        let response = client
            .get("http://127.0.0.1:8080/metrics")
            .send()
            .await
            .expect("Failed to send request");
        assert!(response.status().is_success());

        let body = response.text().await.expect("Failed to read response body");
        assert!(body.contains("expected_metric_content"));
    }
}