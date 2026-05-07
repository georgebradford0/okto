pub mod k8s;
pub mod config;
pub use k8s::*;
pub use kube::Client;
pub use config::{read_config, write_config, Config, config_path, data_dir};
