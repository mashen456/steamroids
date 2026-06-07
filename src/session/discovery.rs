//! Connection Manager (CM) server discovery.
//!
//! Before opening a CM session you need a WebSocket endpoint to connect to.
//! Steam publishes the current list via the public `ISteamDirectory`
//! `GetCMListForConnect` `WebAPI` (no API key required). This module fetches and
//! parses it; each [`CmServer`] yields a [`wss://`](CmServer::ws_url) URL ready
//! for [`connect_ws`](crate::transport::connect_ws).

use serde::Deserialize;
use tracing::debug;

use crate::transport::proxy::ProxyConfig;
use crate::{Error, Result};

const DIRECTORY_URL: &str = "https://api.steampowered.com/ISteamDirectory/GetCMListForConnect/v1/";

/// A Connection Manager WebSocket endpoint returned by the directory.
#[derive(Debug, Clone)]
pub struct CmServer {
    /// `host:port` of the CM server.
    pub endpoint: String,
    /// Steam datacenter code (e.g. `"fra"`), when the directory reports one.
    pub datacenter: Option<String>,
    /// Relative load hint (lower is better), when reported.
    pub load: Option<f32>,
}

impl CmServer {
    /// The `wss://` URL to hand to [`connect_ws`](crate::transport::connect_ws).
    ///
    /// Steam's CM WebSocket listener lives at the `/cmsocket/` path.
    pub fn ws_url(&self) -> String {
        format!("wss://{}/cmsocket/", self.endpoint)
    }
}

/// Fetch the current list of CM WebSocket servers, optionally through `proxy`.
///
/// Servers are returned in the order Steam provides them (already roughly
/// load-sorted) — connect to the first that succeeds and fall back down the
/// list.
///
/// # Errors
///
/// [`Error::Network`] if the directory can't be reached or returns an
/// unparseable / empty list.
pub async fn discover_cm_servers(proxy: Option<&ProxyConfig>) -> Result<Vec<CmServer>> {
    let client = crate::http::client(proxy)?;
    let response = client
        .get(DIRECTORY_URL)
        .query(&[
            ("cmtype", "websockets"),
            ("format", "json"),
            ("cellid", "0"),
        ])
        .send()
        .await
        .map_err(|e| Error::Network(format!("GetCMListForConnect: {e}")))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| Error::Network(format!("GetCMListForConnect body: {e}")))?;
    if !status.is_success() {
        return Err(Error::Network(format!(
            "GetCMListForConnect: HTTP {status}"
        )));
    }

    let servers = parse_servers(&body)?;
    debug!(
        count = servers.len(),
        first = ?servers.first().map(|s| s.endpoint.as_str()),
        "discovered CM servers"
    );
    Ok(servers)
}

/// JSON shapes returned by `GetCMListForConnect` (only the fields we use).
#[derive(Deserialize)]
struct DirectoryResponse {
    response: ServerList,
}

#[derive(Deserialize)]
struct ServerList {
    #[serde(default)]
    serverlist: Vec<ServerEntry>,
}

#[derive(Deserialize)]
struct ServerEntry {
    endpoint: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    dc: Option<String>,
    load: Option<f32>,
}

/// Parse a `GetCMListForConnect` JSON body into [`CmServer`]s.
fn parse_servers(body: &str) -> Result<Vec<CmServer>> {
    let parsed: DirectoryResponse = serde_json::from_str(body)
        .map_err(|e| Error::Network(format!("GetCMListForConnect: unparseable response: {e}")))?;

    let servers: Vec<CmServer> = parsed
        .response
        .serverlist
        .into_iter()
        // We requested websockets; defend against any stray entry of another
        // type, but keep entries that omit the field.
        .filter(|e| e.kind.as_deref().is_none_or(|k| k == "websockets"))
        .map(|e| CmServer {
            endpoint: e.endpoint,
            datacenter: e.dc,
            load: e.load,
        })
        .collect();

    if servers.is_empty() {
        return Err(Error::Network(
            "GetCMListForConnect returned no websocket servers".into(),
        ));
    }
    Ok(servers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_uses_cmsocket_path() {
        let cm = CmServer {
            endpoint: "ext1-fra1.steamserver.net:27021".into(),
            datacenter: Some("fra".into()),
            load: Some(5.0),
        };
        assert_eq!(
            cm.ws_url(),
            "wss://ext1-fra1.steamserver.net:27021/cmsocket/"
        );
    }

    #[test]
    fn parses_directory_response() {
        let body = r#"{
            "response": {
                "serverlist": [
                    {"endpoint":"ext1-fra1.steamserver.net:27021","type":"websockets","dc":"fra","load":117,"wtd_load":5.7},
                    {"endpoint":"ext2-fra1.steamserver.net:27022","type":"websockets","dc":"fra"}
                ]
            }
        }"#;
        let servers = parse_servers(body).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].endpoint, "ext1-fra1.steamserver.net:27021");
        assert_eq!(servers[0].datacenter.as_deref(), Some("fra"));
        assert_eq!(servers[1].load, None);
        assert!(servers[0].ws_url().starts_with("wss://"));
    }

    #[test]
    fn filters_out_non_websocket_entries() {
        let body = r#"{"response":{"serverlist":[
            {"endpoint":"tcp1:27017","type":"netfilter"},
            {"endpoint":"ws1:27021","type":"websockets"}
        ]}}"#;
        let servers = parse_servers(body).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].endpoint, "ws1:27021");
    }

    #[test]
    fn empty_list_is_an_error() {
        let err = parse_servers(r#"{"response":{"serverlist":[]}}"#).unwrap_err();
        assert!(matches!(err, Error::Network(_)));
    }

    #[test]
    fn garbage_body_is_an_error() {
        let err = parse_servers("not json").unwrap_err();
        assert!(matches!(err, Error::Network(_)));
    }
}
