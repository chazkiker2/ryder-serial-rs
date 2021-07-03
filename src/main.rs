#[macro_use]
extern crate log;

use env_logger::Env;
use eyre::Error;
use fehler::throws;

use ryder_serial;

#[throws]
#[tokio::main]
async fn main() {
    let env = Env::default()
        .filter_or("MY_LOG_LEVEL", "trace")
        .write_style_or("MY_LOG_STYLE", "always");

    env_logger::init_from_env(env);

    ryder_serial::run().await.expect("Failed to run");
    ryder_serial::run_no_async();
}
