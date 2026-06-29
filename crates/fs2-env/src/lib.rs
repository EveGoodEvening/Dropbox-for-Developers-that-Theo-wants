//! Environment variable sync: dotenv parsing, materialization, env exec.
//!
//! Provides the env var data model, dotenv parser, and materializer.

pub mod dotenv;
pub mod model;

pub use dotenv::{parse_dotenv, DotenvEntry, DotenvError};
pub use model::{validate_env_name, validate_environment_name, EnvVar, EnvVarRecord};
