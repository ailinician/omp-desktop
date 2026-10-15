//! The Chromium processes behind the panel: which browser to run (the
//! user's setting or automatic discovery), starting and stopping it, the
//! private files of this app instance, and reaping what a crashed earlier
//! run left behind.

mod instance;
mod launch;
mod locate;
mod reap;
mod settings;
mod stderr;

pub use instance::{
    prepare_instance_dir, private_dirs, prune_dead, remove_dir_retrying, write_overlay,
};
pub use launch::Chromium;
pub use settings::{BrowserSettings, SettingsStore};
