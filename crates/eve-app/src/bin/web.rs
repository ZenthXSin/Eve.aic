#[tokio::main]
async fn main() {
    let result = async {
        let Some(options) = eve_app::WebOptions::parse(std::env::args_os().skip(1))? else {
            println!("{}", eve_app::WEB_HELP);
            return Ok::<(), eve_app::AppError>(());
        };
        eve_app::run_web(options).await
    }
    .await;
    if let Err(error) = result {
        eprintln!("Eve Web: {error}");
        std::process::exit(1);
    }
}
