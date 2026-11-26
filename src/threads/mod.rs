pub mod actor;
mod commit;
pub mod configuration;
pub mod controls;
pub mod dispatcher;
pub mod manager;

use fridica_core::delegation;
mod effects;
pub mod external;
mod linked;
pub(crate) mod overleaf;
mod posts;
mod progress;
pub mod refusal;
use fridica_core::failure;
mod replies;
mod results;

pub mod runtime;
pub mod service;
pub mod verdict;
