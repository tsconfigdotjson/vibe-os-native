//! The services VibeOS knows: a curated set, with no user-added servers in v1.
//! Each host offers its agents every one of them, connected or not, so a
//! service connected later reaches agents already running.

/// One supported service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Its name: its MCP server in an agent is `vibe-<name>`.
    pub name: String,
    /// Its name for people.
    pub title: String,
    /// Its MCP endpoint, streamable HTTP.
    pub endpoint: String,
    /// The same, read-only, if it has one.
    pub read_only_endpoint: Option<String>,
    /// What's asked for when connecting.
    pub scopes: Vec<String>,
    /// Tools no agent may use, whatever the owner chooses.
    pub denied_tools: Vec<String>,
}

const LINEAR_BASE: &str = "https://mcp.linear.app";

impl Service {
    /// Linear's remote MCP.
    pub fn linear() -> Service {
        Service {
            name: "linear".into(),
            title: "Linear".into(),
            endpoint: format!("{LINEAR_BASE}/mcp"),
            read_only_endpoint: Some(format!("{LINEAR_BASE}/mcp/readonly")),
            scopes: vec!["read".into(), "write".into()],
            denied_tools: Vec::new(),
        }
    }

    /// Linear, on a stand-in at `base`: how tests keep from the real one.
    pub fn linear_at(base: &str) -> Service {
        let real = Service::linear();
        Service {
            endpoint: real.endpoint.replace(LINEAR_BASE, base),
            read_only_endpoint: real
                .read_only_endpoint
                .map(|e| e.replace(LINEAR_BASE, base)),
            ..real
        }
    }
}

/// Every supported service.
pub fn supported() -> Vec<Service> {
    vec![Service::linear()]
}
