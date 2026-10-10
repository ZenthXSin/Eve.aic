//! 语义向量（embedding）契约：可替换的向量模型、返回值校验、余弦相似度与紧凑量化。
//!
//! 向量只用于在宿主已经授权的范围内给已保存的记忆排序，不是事实、语义理解或置信度的证明。
//! 不同模型或维度的向量不可比较；实现须声明自己的模型标识与维度，宿主据此隔离索引。
use std::{fmt, future::Future, pin::Pin};

/// 一次请求至多嵌入的条数。
pub const MAX_EMBED_BATCH: usize = 16;
/// 单条输入的 UTF-8 字节上限；更长的正文由调用方截断后再嵌入。
pub const MAX_EMBED_INPUT_BYTES: usize = 2048;
pub const MAX_DIMENSIONS: u32 = 4096;
pub const MAX_MODEL_BYTES: usize = 256;

pub type EmbedResult<T> = Result<T, EmbeddingError>;
pub type EmbedFuture<'a> = Pin<Box<dyn Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmbeddingError {
    InvalidInput,
    Provider,
    Timeout,
    /// 返回的条数、维度或数值不符合声明。
    InvalidOutput,
    Cancelled,
}
impl fmt::Display for EmbeddingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "向量请求输入无效",
            Self::Provider => "向量模型请求失败",
            Self::Timeout => "向量模型请求超时",
            Self::InvalidOutput => "向量模型返回无效",
            Self::Cancelled => "向量模型请求已取消",
        })
    }
}
impl std::error::Error for EmbeddingError {}

/// 向量模型的稳定标识与维度；索引按它隔离，不含端点或凭据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddingProfile {
    pub model: String,
    pub dimensions: u32,
}
impl EmbeddingProfile {
    pub fn validate(&self) -> EmbedResult<()> {
        if self.model.is_empty()
            || self.model.len() > MAX_MODEL_BYTES
            || self
                .model
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
            || self.dimensions == 0
            || self.dimensions > MAX_DIMENSIONS
        {
            return Err(EmbeddingError::InvalidInput);
        }
        Ok(())
    }
}

/// 可替换的向量模型。一次调用至多一次远端请求，输出须通过 `validate_vectors`。
pub trait EmbeddingProvider: Send + Sync {
    fn profile(&self) -> &EmbeddingProfile;
    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a>;
}

/// 输入：1 到 `MAX_EMBED_BATCH` 条，每条非空、不超过字节上限、不含 NUL。
pub fn validate_inputs(inputs: &[String]) -> EmbedResult<()> {
    if inputs.is_empty()
        || inputs.len() > MAX_EMBED_BATCH
        || inputs.iter().any(|input| {
            input.trim().is_empty() || input.len() > MAX_EMBED_INPUT_BYTES || input.contains('\0')
        })
    {
        return Err(EmbeddingError::InvalidInput);
    }
    Ok(())
}

/// 返回值与输入一一对应，维度等于声明，每个分量有限且向量非零。
pub fn validate_vectors(
    profile: &EmbeddingProfile,
    inputs: usize,
    vectors: &[Vec<f32>],
) -> EmbedResult<()> {
    if vectors.len() != inputs
        || vectors.iter().any(|vector| {
            vector.len() != profile.dimensions as usize
                || vector.iter().any(|value| !value.is_finite())
                || vector.iter().all(|value| *value == 0.0)
        })
    {
        return Err(EmbeddingError::InvalidOutput);
    }
    Ok(())
}

/// 余弦相似度，范围 [-1, 1]；长度不同或含零向量时为 0。
pub fn cosine(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let (mut dot, mut left_norm, mut right_norm) = (0f64, 0f64, 0f64);
    for (a, b) in left.iter().zip(right) {
        let (a, b) = (f64::from(*a), f64::from(*b));
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        return 0.0;
    }
    (dot / (left_norm.sqrt() * right_norm.sqrt())).clamp(-1.0, 1.0) as f32
}

/// 紧凑保存：按最大绝对分量缩放到 i8，再编码为十六进制。余弦只依赖方向，量化误差很小但不为零。
#[derive(Clone, Debug, PartialEq)]
pub struct QuantizedVector {
    pub scale: f32,
    pub values: Vec<i8>,
}
impl QuantizedVector {
    pub fn quantize(vector: &[f32]) -> Option<Self> {
        let peak = vector.iter().map(|value| value.abs()).fold(0f32, f32::max);
        if !peak.is_finite() || peak == 0.0 {
            return None;
        }
        let scale = peak / 127.0;
        Some(Self {
            scale,
            values: vector
                .iter()
                .map(|value| (value / scale).round().clamp(-127.0, 127.0) as i8)
                .collect(),
        })
    }
    pub fn restore(&self) -> Vec<f32> {
        self.values
            .iter()
            .map(|value| f32::from(*value) * self.scale)
            .collect()
    }
    pub fn to_hex(&self) -> String {
        self.values
            .iter()
            .map(|value| format!("{:02x}", *value as u8))
            .collect()
    }
    pub fn from_hex(scale: f32, text: &str) -> Option<Self> {
        if !scale.is_finite() || scale <= 0.0 || !text.len().is_multiple_of(2) {
            return None;
        }
        let values = (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
            .map(|byte| byte.map(|byte| byte as i8))
            .collect::<Option<Vec<i8>>>()?;
        (!values.is_empty() && values.iter().any(|value| *value != 0))
            .then_some(Self { scale, values })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_ranks_direction_and_rejects_mismatched_shapes() {
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn quantized_vectors_round_trip_through_hex_with_small_error() {
        let vector = vec![0.12, -0.5, 0.33, 0.0, 0.91];
        let quantized = QuantizedVector::quantize(&vector).unwrap();
        let restored = QuantizedVector::from_hex(quantized.scale, &quantized.to_hex()).unwrap();
        assert_eq!(restored, quantized);
        assert!(cosine(&vector, &restored.restore()) > 0.999);
        assert!(QuantizedVector::quantize(&[0.0, 0.0]).is_none());
        for (scale, text) in [
            (0.0, "01"),
            (1.0, "0"),
            (1.0, "zz"),
            (1.0, "0000"),
            (f32::NAN, "01"),
        ] {
            assert!(QuantizedVector::from_hex(scale, text).is_none(), "{text}");
        }
    }

    #[test]
    fn inputs_outputs_and_profiles_are_bounded() {
        let profile = EmbeddingProfile {
            model: "embed-small".into(),
            dimensions: 2,
        };
        assert!(profile.validate().is_ok());
        assert!(validate_inputs(&["a".into()]).is_ok());
        assert!(validate_inputs(&[]).is_err());
        assert!(validate_inputs(&[" ".into()]).is_err());
        assert!(validate_inputs(&vec!["a".to_string(); MAX_EMBED_BATCH + 1]).is_err());
        assert!(validate_vectors(&profile, 1, &[vec![1.0, 0.0]]).is_ok());
        for bad in [
            vec![],
            vec![vec![1.0]],
            vec![vec![0.0, 0.0]],
            vec![vec![f32::NAN, 1.0]],
        ] {
            assert!(validate_vectors(&profile, 1, &bad).is_err());
        }
        for bad in [("", 2), ("a b", 2), ("m", 0), ("m", MAX_DIMENSIONS + 1)] {
            let profile = EmbeddingProfile {
                model: bad.0.into(),
                dimensions: bad.1,
            };
            assert!(profile.validate().is_err());
        }
    }
}
