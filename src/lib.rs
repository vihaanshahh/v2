pub mod accepted;
pub mod bandwidth;
pub mod display;
pub mod engine;
pub mod hardware;
pub mod models;
pub mod ollama;
pub mod sources;
pub mod ui;

#[cfg(feature = "daemon")]
pub mod activity;
#[cfg(feature = "daemon")]
pub mod console;
#[cfg(feature = "daemon")]
pub mod doctor;
#[cfg(feature = "daemon")]
pub mod endpoints;
#[cfg(feature = "daemon")]
pub mod manage;
#[cfg(feature = "daemon")]
pub mod mesh;
#[cfg(feature = "daemon")]
pub mod ollama_api;
#[cfg(feature = "daemon")]
pub mod paths;
#[cfg(feature = "daemon")]
pub mod policy;
#[cfg(feature = "daemon")]
pub mod proxy;
#[cfg(feature = "daemon")]
pub mod usage;

#[cfg(all(test, feature = "daemon"))]
pub(crate) mod test_support;
