use eve_app::{CHIME_HELP, ChimeOptions, run_chime};

#[tokio::main]
async fn main() {
    let result = async {
        let Some(options) = ChimeOptions::parse(std::env::args_os().skip(1))? else {
            println!("{CHIME_HELP}");
            return Ok::<_, eve_app::AppError>(());
        };
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let summary = run_chime(options, stdin.lock(), stdout.lock()).await?;
        eprintln!(
            "Eve 插话：处理 {} 条群消息，发出 {} 条回复。",
            summary.messages, summary.replies
        );
        Ok(())
    }
    .await;
    if let Err(error) = result {
        eprintln!("Eve 插话：{error}");
        std::process::exit(1);
    }
}
