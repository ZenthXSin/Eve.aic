#[tokio::main]
async fn main() {
    let result = async {
        let Some(options) = eve_app::QqBotOptions::parse(std::env::args_os().skip(1))? else {
            println!("{}", eve_app::QQBOT_HELP);
            return Ok::<(), eve_app::AppError>(());
        };
        let summary = eve_app::run_qqbot(options).await?;
        println!("{}", serde_json::to_string(&summary)?);
        Ok(())
    }
    .await;
    if let Err(error) = result {
        eprintln!("Eve QQBot: {error}");
        std::process::exit(1);
    }
}
