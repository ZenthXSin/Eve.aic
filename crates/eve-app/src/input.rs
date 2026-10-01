//! 独立标准线程读取输入；不使用无法取消的 Tokio stdin 阻塞任务。
use crate::{AppError, MAX_INPUT_BYTES};
use std::io::{BufRead, Read};
use tokio::sync::mpsc;

pub(crate) enum InputEvent {
    Line(String),
    TooLarge,
    Failed(AppError),
}
pub(crate) fn start(
    input: impl BufRead + Send + 'static,
) -> std::io::Result<mpsc::Receiver<InputEvent>> {
    let (sender, receiver) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("eve-stdin".into())
        .spawn(move || {
            let mut input = input;
            loop {
                let event = (|| -> Result<Option<InputEvent>, AppError> {
                    let mut bytes = Vec::new();
                    let count = (&mut input)
                        .take((MAX_INPUT_BYTES + 3) as u64)
                        .read_until(b'\n', &mut bytes)?;
                    if count == 0 {
                        return Ok(None);
                    }
                    let newline = bytes.last() == Some(&b'\n');
                    if newline {
                        bytes.pop();
                    }
                    if bytes.last() == Some(&b'\r') {
                        bytes.pop();
                    }
                    if bytes.len() > MAX_INPUT_BYTES {
                        if !newline {
                            input.skip_until(b'\n')?;
                        }
                        return Ok(Some(InputEvent::TooLarge));
                    }
                    let text = String::from_utf8(bytes).map_err(|_| "输入必须为 UTF-8。")?;
                    Ok(Some(InputEvent::Line(text)))
                })();
                match event {
                    Ok(Some(event)) => {
                        if sender.blocking_send(event).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = sender.blocking_send(InputEvent::Failed(error));
                        break;
                    }
                }
            }
        })?;
    // 线程不持有 Kernel、控制服务或状态；退出不 join 阻塞的标准输入。
    Ok(receiver)
}
