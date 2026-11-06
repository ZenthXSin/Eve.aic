//! 拒绝重复键，避免响应或工具参数的字段被 serde_json::Value 静默覆盖。
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

#[cfg(test)]
mod tests {
    use super::from_slice;
    use serde_json::{Number, Value};

    #[test]
    fn embedding_and_segment_numbers_keep_their_types() {
        #[derive(serde::Deserialize)]
        struct Fields {
            embedding: Vec<f32>,
            scale: f32,
        }

        let value = from_slice(br#"{"embedding":[1.0,0.5],"scale":1.0}"#).unwrap();
        let fields: Fields = serde_json::from_value(value).unwrap();
        assert_eq!(fields.embedding, vec![1.0, 0.5]);
        assert_eq!(fields.scale, 1.0);
    }

    #[test]
    fn numeric_precision_matches_the_active_serde_json_features() {
        for text in [
            "0",
            "-0",
            "-0.0",
            "1.0",
            "0.5",
            "-2.5e-3",
            "6.02214076e+23",
            "18446744073709551615",
            "-9223372036854775808",
            "18446744073709551616",
            "-9223372036854775809",
            "1234567890123456789012345678901234567890",
            "1.23456789012345678901234567890123456789",
            "1e400",
        ] {
            let expected = serde_json::from_str::<Number>(text);
            let document = format!(r#"{{"items":[{text}],"scale":{text}}}"#);
            match expected {
                Ok(number) => {
                    let expected = Value::Number(number);
                    let actual = from_slice(text.as_bytes()).unwrap();
                    assert_eq!(actual, expected, "{text}");
                    assert_eq!(actual.to_string(), expected.to_string(), "{text}");
                    let nested = from_slice(document.as_bytes()).unwrap();
                    assert_eq!(nested["items"][0], expected, "{text}");
                    assert_eq!(nested["scale"], expected, "{text}");
                }
                Err(_) => {
                    assert!(from_slice(text.as_bytes()).is_err(), "{text}");
                    assert!(from_slice(document.as_bytes()).is_err(), "{text}");
                }
            }
        }
    }

    #[test]
    fn private_marker_names_remain_literal_object_keys() {
        for key in [
            "$serde_json::private::Number",
            "$serde_json::private::RawValue",
        ] {
            for value in ["1.25", "not a number", r#"{"a":1,"a":2}"#] {
                let quoted = serde_json::to_string(value).unwrap();
                let document = format!(r#"{{"{key}":{quoted}}}"#);
                let parsed = from_slice(document.as_bytes()).unwrap();
                assert_eq!(parsed.as_object().unwrap().len(), 1);
                assert_eq!(parsed[key], value);

                let document = format!(r#"{{"before":1.0,"nested":[{document}],"after":false}}"#);
                let parsed = from_slice(document.as_bytes()).unwrap();
                assert_eq!(parsed.as_object().unwrap().len(), 3);
                assert!(parsed["before"].is_number());
                assert_eq!(parsed["nested"][0][key], value);
                assert_eq!(parsed["after"], false);
            }
        }
    }

    #[test]
    fn duplicate_keys_are_rejected_in_each_object_after_unescaping() {
        for text in [
            r#"{"a":1.0,"a":0.5}"#,
            r#"{"a":1,"\u0061":2}"#,
            r#"{"\u0061":1,"a":2}"#,
            r#"{"nested":[{"a":1.0,"a":0.5}]}"#,
            r#"{"$serde_json::private::Number":"1.0","$serde_json::private::Number":"0.5"}"#,
            r#"{"$serde_json::private::\u004eumber":"1.0","$serde_json::private::Number":"0.5"}"#,
            r#"{"$serde_json::private::RawValue":"1.0","$serde_json::private::RawValue":"0.5"}"#,
        ] {
            let error = from_slice(text.as_bytes()).unwrap_err();
            assert!(error.to_string().contains("duplicate JSON key"), "{error}");
        }
        let separate = from_slice(br#"[{"a":1.0},{"a":0.5}]"#).unwrap();
        assert_eq!(separate.as_array().unwrap().len(), 2);
    }

    #[test]
    fn invalid_json_and_trailing_input_are_rejected() {
        for text in [
            "",
            " ",
            "{}[]",
            "1.0 true",
            "[1.0,]",
            r#"{"a":1.0,}"#,
            r#"{"a":01}"#,
            r#"{"a":NaN}"#,
            r#"{"a":"\uZZZZ"}"#,
        ] {
            assert!(from_slice(text.as_bytes()).is_err(), "{text}");
        }
        let value = from_slice(b" \n {\"a\":[null,true,false,\"text\",{}]} \t").unwrap();
        assert_eq!(value["a"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn nested_containers_keep_the_default_recursion_limit() {
        for depth in [126, 127, 128, 129] {
            let arrays = format!("{}0.5{}", "[".repeat(depth), "]".repeat(depth));
            let objects = format!("{}0.5{}", r#"{"a":"#.repeat(depth), "}".repeat(depth));
            let mut mixed = "0.5".to_owned();
            for level in 0..depth {
                mixed = if level % 2 == 0 {
                    format!("[{mixed}]")
                } else {
                    format!(r#"{{"a":{mixed}}}"#)
                };
            }
            for text in [arrays, objects, mixed] {
                let actual = from_slice(text.as_bytes());
                let expected = serde_json::from_str::<Value>(&text);
                assert_eq!(actual.is_ok(), expected.is_ok(), "depth {depth}");
                assert_eq!(actual.is_ok(), depth < 128, "depth {depth}");
            }
        }
    }
}
