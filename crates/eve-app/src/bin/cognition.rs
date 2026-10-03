#[tokio::main]
async fn main() {
    let result = async {
        let Some(options) = eve_app::CognitionOptions::parse(std::env::args_os().skip(1))? else {
            println!("{}", eve_app::COGNITION_HELP);
            return Ok::<(), eve_app::AppError>(());
        };
        let report = eve_app::run_cognition(options).await?;
        println!("{}", serde_json::to_string(&report)?);
        Ok(())
    }
    .await;
    if let Err(error) = result {
        eprintln!("Eve Cognition: {error}");
        std::process::exit(1);
    }
}
