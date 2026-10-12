//! Rail widget plugins: user-configured executables whose JSON stdout the
//! frontend renders as a rail card. `config.rs` reads `widgets.json`;
//! `run.rs` executes one plugin with a timeout and an output cap. Output
//! validation lives in the frontend (`src/app/widgets.js`) so the schema has
//! one implementation and one regression test.

pub mod config;
pub mod run;

pub use config::{load, WidgetConfig};
pub use run::{context, run_plugin, PluginContext, RunResult};
