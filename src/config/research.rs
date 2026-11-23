//! Host-owned research integration settings; no credentials or I/O.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
}
impl Settings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
