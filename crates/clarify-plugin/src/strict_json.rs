//! 拒绝重复键，避免模型输出或持久状态的字段被 serde_json::Value 静默覆盖。
use serde::{
    Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value, value::RawValue};
use std::fmt;

// serde_json 默认最多接受 127 层容器；RawValue 分层解析也必须共享同一深度预算。
const MAX_CONTAINER_DEPTH: usize = 127;

pub(crate) fn from_slice(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    let raw = serde_json::from_slice::<&RawValue>(bytes)?;
    from_raw(raw, MAX_CONTAINER_DEPTH)
}

fn from_raw(raw: &RawValue, remaining_depth: usize) -> Result<Value, serde_json::Error> {
    // 根据真实 JSON token 区分容器与标量，不把高精度数字的内部表示当成对象，
    // 也不把用户提供的同名私有字段解释成数字或 RawValue。
    if !matches!(raw.get().as_bytes().first(), Some(b'[' | b'{')) {
        return serde_json::from_str(raw.get());
    }
    if remaining_depth == 0 {
        return Err(de::Error::custom("recursion limit exceeded"));
    }
    let mut parser = serde_json::Deserializer::from_str(raw.get());
    let value = parser.deserialize_any(StrictVisitor {
        remaining_depth: remaining_depth - 1,
    })?;
    parser.end()?;
    Ok(value)
}

struct StrictVisitor {
    remaining_depth: usize,
}

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate keys")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut array = Vec::new();
        while let Some(raw) = seq.next_element::<&RawValue>()? {
            array.push(from_raw(raw, self.remaining_depth).map_err(de::Error::custom)?);
        }
        Ok(Value::Array(array))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom("duplicate JSON key"));
            }
            let raw = map.next_value::<&RawValue>()?;
            let value = from_raw(raw, self.remaining_depth).map_err(de::Error::custom)?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}
