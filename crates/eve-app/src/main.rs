use eve_app::{ChatOptions, HELP, run_console};

#[tokio::main]
async fn main() {
    let result = async {
        let Some(options) = ChatOptions::parse(std::env::args_os().skip(1))? else {
            println!("{HELP}");
            return Ok::<_, eve_app::AppError>(());
        };
        eprintln!("Eve 已选择串行对话入口；/quit 退出，/help 查看说明。");
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let summary = run_console(options, stdin.lock(), stdout.lock()).await?;
        if summary.failed_turns > 0 {
            return Err("存在失败轮次，请查看本轮提示；没有自动重试。".into());
        }
        Ok(())
    }.await;
    if let Err(error) = result {
        eprintln!("Eve：{error}");
        std::process::exit(1);
    }
}
