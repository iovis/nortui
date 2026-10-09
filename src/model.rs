use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceState {
    Active,
    Inactive,
    Activating,
    Deactivating,
    Failed,
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VpnState {
    Connected,
    Connecting,
    Disconnected,
    Paused,
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VpnStatus {
    pub state: VpnState,
    pub country: Option<String>,
    pub city: Option<String>,
    pub ip: Option<String>,
    pub server: Option<String>,
    pub technology: Option<String>,
    pub protocol: Option<String>,
    pub uptime: Option<std::time::Duration>,
    pub received: Option<String>,
    pub sent: Option<String>,
}

impl VpnStatus {
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            state: VpnState::Unavailable(message.into()),
            country: None,
            city: None,
            ip: None,
            server: None,
            technology: None,
            protocol: None,
            uptime: None,
            received: None,
            sent: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub service: ServiceState,
    pub vpn: Option<VpnStatus>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            service: ServiceState::Unavailable("Checking service…".into()),
            vpn: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Location {
    pub country: String,
    pub city: Option<String>,
}

impl Location {
    pub fn label(&self) -> String {
        let country = self.country.replace('_', " ");
        self.city.as_ref().map_or(country.clone(), |city| {
            format!("{} / {country}", city.replace('_', " "))
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Connect(Option<Location>),
    Reconnect,
    Disconnect,
    StartService,
    StopService,
    Refresh,
}

impl Action {
    pub fn label(&self) -> String {
        match self {
            Self::Connect(Some(location)) => format!("Connecting to {}…", location.label()),
            Self::Connect(None) | Self::Reconnect => "Connecting to recommended server…".into(),
            Self::Disconnect => "Disconnecting…".into(),
            Self::StartService => "Starting service…".into(),
            Self::StopService => "Stopping service…".into(),
            Self::Refresh => "Refreshing…".into(),
        }
    }
}
