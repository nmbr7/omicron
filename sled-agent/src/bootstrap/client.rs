//! Interface for making API requests to a Sled Agent's Bootstrap API.
//!
//! This should be replaced with a client generated from the OpenAPI spec
//! generated by the server.

use omicron_common::api::external::Error;
use omicron_common::api::internal::bootstrap_agent::ShareRequest;
use omicron_common::api::internal::bootstrap_agent::ShareResponse;
use omicron_common::http_client::HttpClient;

use http::Method;
use hyper::Body;
use slog::Logger;
use std::net::SocketAddr;

/// Client for sled agent's bootstrapping API.
///
/// This is a subset of the full sled agent API.
pub struct Client {
    /// Sled agent server address
    pub service_address: SocketAddr,
    /// underlying HTTP client
    client: HttpClient,
}

impl Client {
    /// Create a new client to make requests to the sled agent running at
    /// `server_addr`.
    pub fn new(server_addr: SocketAddr, log: Logger) -> Client {
        Client {
            service_address: server_addr,
            client: HttpClient::new("sled agent (bootstrap)", server_addr, log),
        }
    }

    /// Provides an identity to another sled agent, and receives
    /// a portion of a key necessary to unlock the trust quorum.
    pub async fn request_share(
        &self,
        identity: Vec<u8>,
    ) -> Result<ShareResponse, Error> {
        let path = "/request_share";
        let body = Body::from(
            serde_json::to_string(&ShareRequest { identity }).unwrap(),
        );
        let mut response = self.client.request(Method::GET, path, body).await?;
        /* TODO-robustness handle 300-level? */
        assert!(response.status().is_success());
        let value = self
            .client
            .read_json::<ShareResponse>(
                &self.client.error_message_base(&Method::GET, path),
                &mut response,
            )
            .await?;
        Ok(value)
    }
}