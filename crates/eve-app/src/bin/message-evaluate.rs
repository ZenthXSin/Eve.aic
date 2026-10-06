use std::io::Write;

#[tokio::main]
async fn main() {
    let result = async {
        let Some(options) = eve_app::MessageEvaluationOptions::parse(std::env::args_os().skip(1))?
        else {
            println!("{}", eve_app::MESSAGE_EVALUATION_HELP);
            return Ok::<(), eve_app::AppError>(());
        };
        let stdout = options.output.is_none();
        let report = eve_app::run_message_evaluation(options).await?;
        if stdout {
            std::io::stdout()
                .lock()
                .write_all(&eve_app::message_evaluation_json(&report)?)?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        eprintln!("Eve 消息评估：{error}");
        std::process::exit(1);
    }
}
