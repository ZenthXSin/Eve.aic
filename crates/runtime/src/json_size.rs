//! 按相同 JSON 序列化规则计字节，不分配完整请求/历史副本。
use std::io::{self, Write};
struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "JSON byte count overflow")
        })?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
/// 返回 serde_json 紧凑输出的精确 UTF-8 字节数；与 to_vec 相同的序列化错误原样返回。
pub fn serialized_json_size<T: serde::Serialize + ?Sized>(
    value: &T,
) -> Result<usize, serde_json::Error> {
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_actual_json_for_escaped_unicode_nested_and_large_unsized_values() {
        let values = [
            serde_json::json!({"原话":["汉字\n引号\"\\",true,null,1.25,-42]}),
            serde_json::json!(["长上下文".repeat(100_000)]),
        ];
        for value in values {
            assert_eq!(
                serialized_json_size(&value).unwrap(),
                serde_json::to_vec(&value).unwrap().len()
            );
        }
        let slice = ["a", "汉字", "\r\n"];
        assert_eq!(
            serialized_json_size(&slice[..]).unwrap(),
            serde_json::to_vec(&slice[..]).unwrap().len()
        );
    }
    #[test]
    fn serialization_errors_and_counter_overflow_cannot_be_reported_as_valid_sizes() {
        struct Reject;
        impl serde::Serialize for Reject {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("reject sample"))
            }
        }
        assert!(serialized_json_size(&Reject).is_err());
        let mut counter = Counter(usize::MAX);
        assert!(counter.write(b"x").is_err());
        assert_eq!(counter.0, usize::MAX);
    }
}
