//! 桥接警告只允许固定分类和数值形状进入宿主日志。
use eve_plugin_api::{LogEntry, LogLevel};
use serde_json::Value;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const REASONS: &[&str] = &[
    "message_id",
    "sender_id",
    "reply_target",
    "target_scope",
    "target_message_id",
    "target_id",
    "content_type",
    "content_bytes",
    "c2c_target",
    "group_target",
    "normalized_text_empty",
    "normalized_text_bytes",
];
const VALUE_TYPES: &[&str] = &[
    "undefined",
    "null",
    "string",
    "number",
    "boolean",
    "object",
    "array",
    "other",
];

pub(super) fn entry(frame: &Value) -> Option<LogEntry> {
    let code = match frame.get("code").and_then(Value::as_str) {
        Some("invalid_route_or_text") => "bridge_invalid_route_or_text",
        Some("unsupported_message") => "bridge_unsupported_message",
        Some("pending_limit") => "bridge_pending_limit",
        Some("outbound_limit") => "bridge_outbound_limit",
        Some("frame_limit") => "bridge_frame_limit",
        Some("protocol_version") => "bridge_protocol_version",
        Some("sdk_error") => "bridge_sdk_error",
        _ => "bridge_warning",
    };
    let mut entry = LogEntry::new(LogLevel::Warn, "eve.qqbot", code).ok()?;
    if code != "bridge_invalid_route_or_text" {
        return Some(entry);
    }
    let Some(ingress) = frame.get("ingress") else {
        return Some(entry);
    };
    let Some(reason) = ingress
        .get("reason")
        .and_then(Value::as_str)
        .filter(|reason| REASONS.contains(reason))
    else {
        return Some(entry);
    };
    if ingress.get("version").and_then(Value::as_u64) != Some(1) {
        return Some(entry);
    }
    entry = entry
        .with_field("ingress_version", "1")
        .with_field("ingress_reason", reason)
        .with_field(
            "ingress_stage",
            if reason.starts_with("normalized_text_") {
                "normalized_text"
            } else {
                "envelope"
            },
        );
    for name in ["count", "total", "elapsed_ms"] {
        if let Some(value) = ingress
            .get(name)
            .and_then(Value::as_u64)
            .filter(|value| *value <= MAX_SAFE_INTEGER && (name == "elapsed_ms" || *value > 0))
        {
            entry = entry.with_field(format!("ingress_{name}"), value.to_string());
        }
    }
    if let Some(value) = ingress.get("counts_saturated").and_then(Value::as_bool) {
        entry = entry.with_field("ingress_counts_saturated", value.to_string());
    }
    let Some(last) = ingress.get("last") else {
        return Some(entry);
    };
    for (name, allowed) in [
        ("kind", &["c2c", "group", "other"][..]),
        (
            "event",
            &["c2c_message", "group_at_message", "group_message", "other"][..],
        ),
        ("target_scope", &["c2c", "group", "other"][..]),
        ("reply_target_type", VALUE_TYPES),
        ("attachments_type", VALUE_TYPES),
        ("elements_type", VALUE_TYPES),
        ("message_id_type", VALUE_TYPES),
        ("sender_id_type", VALUE_TYPES),
        ("target_message_id_type", VALUE_TYPES),
        ("target_id_type", VALUE_TYPES),
        ("group_id_type", VALUE_TYPES),
        ("content_type", VALUE_TYPES),
    ] {
        if let Some(value) = last
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| allowed.contains(value))
        {
            entry = entry.with_field(format!("ingress_last_{name}"), value);
        }
    }
    for name in [
        "message_id_bytes",
        "sender_id_bytes",
        "target_message_id_bytes",
        "target_id_bytes",
        "group_id_bytes",
        "content_bytes",
        "normalized_text_bytes",
        "attachments_count",
        "elements_count",
        "accepted_attachment_count",
    ] {
        match last.get(name) {
            Some(Value::Null) => {
                entry = entry.with_field(format!("ingress_last_{name}"), "null");
            }
            Some(value) => {
                if let Some(value) = value.as_u64().filter(|value| *value <= MAX_SAFE_INTEGER) {
                    entry = entry.with_field(format!("ingress_last_{name}"), value.to_string());
                }
            }
            None => {}
        }
    }
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::entry;
    use serde_json::json;

    #[test]
    fn ingress_warning_keeps_only_fixed_shapes_and_bounded_counts() {
        let frame = json!({
            "code":"invalid_route_or_text", "text":"PRIVATE_BODY", "user_id":"PRIVATE_ID",
            "ingress": {
                "version":1, "reason":"normalized_text_empty", "count":2, "total":3,
                "elapsed_ms":60_000, "counts_saturated":false, "secret":"PRIVATE_TOKEN",
                "last": {
                    "kind":"group", "event":"group_at_message", "target_scope":"group",
                    "reply_target_type":"object", "content_type":"string", "content_bytes":18,
                    "normalized_text_bytes":0, "message_id_type":"string", "message_id_bytes":128,
                    "sender_id":"PRIVATE_ID", "text":"PRIVATE_BODY", "content":"PRIVATE_BODY",
                    "attachments_type":"array", "attachments_count":0, "accepted_attachment_count":0,
                    "group_id_bytes":null, "elements_type":"undefined", "elements_count":null,
                    "url":"https://PRIVATE_HOST/PRIVATE_TOKEN"
                }
            }
        });
        let entry = entry(&frame).unwrap();
        assert_eq!(entry.message, "bridge_invalid_route_or_text");
        assert_eq!(entry.fields["ingress_reason"], "normalized_text_empty");
        assert_eq!(entry.fields["ingress_stage"], "normalized_text");
        assert_eq!(entry.fields["ingress_count"], "2");
        assert_eq!(entry.fields["ingress_total"], "3");
        assert_eq!(entry.fields["ingress_last_content_bytes"], "18");
        assert_eq!(entry.fields["ingress_last_group_id_bytes"], "null");
        assert!(!format!("{entry:?}").contains("PRIVATE"));
    }

    #[test]
    fn malformed_metadata_cannot_supply_arbitrary_log_values() {
        let frame = json!({"code":"invalid_route_or_text", "ingress": {
            "version":1, "reason":"content_type", "count":-1, "total":9_007_199_254_740_992_u64,
            "elapsed_ms":"PRIVATE", "counts_saturated":"PRIVATE",
            "last": {"kind":"PRIVATE", "event":"PRIVATE", "content_type":"PRIVATE",
                "content_bytes":-1, "message_id_bytes":1.5, "attachments_count":"PRIVATE"}
        }});
        let entry = entry(&frame).unwrap();
        assert_eq!(entry.fields.len(), 3);
        assert_eq!(entry.fields["ingress_stage"], "envelope");
        assert!(!format!("{entry:?}").contains("PRIVATE"));
    }

    #[test]
    fn old_or_unknown_warning_metadata_stays_compatible_without_echoing_it() {
        for frame in [
            json!({"code":"invalid_route_or_text"}),
            json!({"code":"invalid_route_or_text","ingress":{"version":2,"reason":"content_type"}}),
            json!({"code":"invalid_route_or_text","ingress":{"version":1,"reason":"PRIVATE"}}),
            json!({"code":"PRIVATE","ingress":{"version":1,"reason":"content_type"}}),
        ] {
            let entry = entry(&frame).unwrap();
            assert!(entry.fields.is_empty());
            assert!(!format!("{entry:?}").contains("PRIVATE"));
        }
    }
}
