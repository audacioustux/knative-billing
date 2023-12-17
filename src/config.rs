use clap::Parser;
use once_cell::sync::Lazy;

#[derive(Parser, Debug)]
pub struct Config {
    #[clap(long, env, default_value = "8080")]
    pub port: u16,
    #[clap(long, env)]
    pub prometheus_svc: String,
    #[clap(long, env)]
    pub database_url: String,
    #[clap(long, default_value = "false")]
    pub dev: bool,
    #[clap(long, env)]
    pub knative_serving_svc: String,
    #[clap(long, env, default_value = "60")]
    pub billing_interval: u64,
}

pub static CONFIG: Lazy<Config> = Lazy::new(Config::parse);
