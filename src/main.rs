use eyre::Error;
use fehler::throws;

use ryder_serial;

#[throws]
#[tokio::main]
async fn main() {
    ryder_serial::run().await?;
    // ryder_serial::run_no_async();
}
