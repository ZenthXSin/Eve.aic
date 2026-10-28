// std-only Rust插件：由可信rustc隔离编译，启动后使用与Python/JS相同的JSON行协议。
use std::io;

fn main() {
    let mut request = String::new();
    io::stdin().read_line(&mut request).expect("protocol input");
    println!(
        "{}",
        r#"{"op":"return","value":{"language":"rust","protocol":1}}"#
    );
}
