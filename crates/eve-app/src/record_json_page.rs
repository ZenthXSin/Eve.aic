//! 原始 JSON 分页：流式计数/哈希，只保留一页，不另建整份转义正文。
use eve_llm_api::ToolExecutionError;
use serde::Serialize;
use serde_json::{Value, json};
use std::io::Write;

const PAGE_BYTES: usize = 8192;
struct Window {
    offset: usize,
    total: usize,
    boundary: bool,
    bytes: Vec<u8>,
    digest: ring::digest::Context,
}
impl Window {
    fn new(offset: usize) -> Self {
        Self {
            offset,
            total: 0,
            boundary: true,
            bytes: Vec::with_capacity(PAGE_BYTES),
            digest: ring::digest::Context::new(&ring::digest::SHA256),
        }
    }
    fn finish(mut self, reference: &str) -> Result<Value, ToolExecutionError> {
        let invalid = || ToolExecutionError::Rejected("分页偏移无效".into());
        if self.offset > self.total || !self.boundary {
            return Err(invalid());
        }
        if let Err(error) = std::str::from_utf8(&self.bytes) {
            if error.error_len().is_some() {
                return Err(invalid());
            }
            self.bytes.truncate(error.valid_up_to());
        }
        let end = self
            .offset
            .checked_add(self.bytes.len())
            .ok_or_else(invalid)?;
        let fragment = String::from_utf8(self.bytes).map_err(|_| invalid())?;
        let hash = self
            .digest
            .finish()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(
            json!({"reference":reference,"offset":self.offset,"next_offset":(end<self.total).then_some(end),"total_bytes":self.total,"record_json_fragment":fragment,"sha256":hash,"original_unmodified":true}),
        )
    }
}
impl Write for Window {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let end = self
            .total
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("原文长度无法计数"))?;
        if self.offset >= self.total && self.offset < end {
            self.boundary = bytes[self.offset - self.total] & 0xc0 != 0x80;
        }
        let start = self.offset.max(self.total);
        let stop = self.offset.saturating_add(PAGE_BYTES).min(end);
        if start < stop {
            self.bytes
                .extend_from_slice(&bytes[start - self.total..stop - self.total]);
        }
        self.digest.update(bytes);
        self.total = end;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(crate) fn page<T: Serialize + ?Sized>(
    reference: &str,
    value: &T,
    offset: usize,
) -> Result<Value, ToolExecutionError> {
    let mut window = Window::new(offset);
    serde_json::to_writer(&mut window, value)
        .map_err(|_| ToolExecutionError::Failed("原始记录无法编码；未返回部分正文".into()))?;
    window.finish(reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_match_complete_json_hash_and_utf8_boundaries_including_large_escaped_text() {
        let value = json!({"text":"中文𝄞\n\"\\".repeat(6000),"tail":"结尾"});
        let bytes = serde_json::to_vec(&value).unwrap();
        let original = std::str::from_utf8(&bytes).unwrap();
        let expected_hash = ring::digest::digest(&ring::digest::SHA256, &bytes)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        for offset in (0..60).chain([8190, 8191, 8192, bytes.len(), bytes.len() + 1, usize::MAX]) {
            let result = page("fixture", &value, offset);
            if offset > bytes.len() || !original.is_char_boundary(offset) {
                assert!(result.is_err());
                continue;
            }
            let mut end = bytes.len().min(offset.saturating_add(PAGE_BYTES));
            while !original.is_char_boundary(end) {
                end -= 1;
            }
            let result = result.unwrap();
            assert_eq!(result["record_json_fragment"], &original[offset..end]);
            assert_eq!(result["total_bytes"], bytes.len());
            assert_eq!(result["sha256"], expected_hash);
            assert_eq!(
                result["next_offset"],
                json!((end < bytes.len()).then_some(end))
            );
        }
    }
    #[test]
    fn one_byte_writes_keep_one_page_and_handle_split_multibyte_characters() {
        let bytes = serde_json::to_vec(&"中文𝄞".repeat(2000)).unwrap();
        let mut window = Window::new(1);
        for byte in &bytes {
            window.write_all(std::slice::from_ref(byte)).unwrap();
            assert!(window.bytes.len() <= PAGE_BYTES);
        }
        assert_eq!(
            window.finish("fixture").unwrap(),
            page("fixture", &"中文𝄞".repeat(2000), 1).unwrap()
        );
    }
}
