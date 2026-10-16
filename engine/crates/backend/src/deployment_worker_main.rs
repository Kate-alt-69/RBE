use std::process::ExitCode;

mod deployment_worker;

#[tokio::main]
async fn main() -> ExitCode {
    match deployment_worker::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("RBE deployment worker failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}
