//! The live client handle shared with the tool adapters.

use std::sync::{Arc, Mutex};

use rmcp::RoleClient;
use rmcp::model::Tool as McpToolDef;
use rmcp::service::{Peer, ServiceError};

use crate::transport::Client;

/// A cloneable handle to a running server's request channel.
///
/// The hub keeps the [`Client`] itself, because graceful shutdown needs the
/// running service; the tools only need to send requests, so they hold a `Peer`
/// clone. Once the handle is closed, a later call reports the server as gone
/// instead of blocking on a channel nobody reads.
#[derive(Clone)]
pub struct ClientHandle {
    peer: Arc<Mutex<Option<Peer<RoleClient>>>>,
}

impl ClientHandle {
    pub fn new(client: &Client) -> Self {
        Self {
            peer: Arc::new(Mutex::new(Some(client.peer().clone()))),
        }
    }

    /// The peer, or `None` after shutdown.
    pub fn current(&self) -> Option<Peer<RoleClient>> {
        self.peer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Stop handing out the peer. Later calls fail instead of hanging.
    pub fn close(&self) {
        *self
            .peer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

/// Discover every tool a server offers.
pub async fn list_tools(client: &Client) -> Result<Vec<McpToolDef>, ServiceError> {
    client.list_all_tools().await
}

#[cfg(test)]
mod tests {
    use super::ClientHandle;
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_closed_handle_stops_handing_out_a_peer() {
        let handle = ClientHandle {
            peer: Arc::new(Mutex::new(None)),
        };

        assert!(handle.current().is_none());
        handle.close();
        assert!(handle.current().is_none());
    }
}
