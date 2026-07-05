pub mod calibrate;
pub mod complexity;
pub mod config;
pub mod detectors;
pub mod feedback;
pub mod judge;
pub mod judge_validation;
pub mod onboard;
pub mod pricing;
pub mod profiles;
pub mod sufficiency;
pub mod threads;
pub mod vkeys;

pub const SCORING_SCHEMA_VERSION: &str = "3";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8088;
