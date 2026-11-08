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
const ATTACHMENT_REJECTION_REASONS: &[&str] = &[
    "invalid_shape",
    "unsupported_type",
    "invalid_url",
    "quoted",
    "element_not_selected",
    "duplicate",
    "limit",
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
        Some("send_failure") => "bridge_send_failure",
        _ => "bridge_warning",
    };
    let mut entry = LogEntry::new(LogLevel::Warn, "eve.qqbot", code).ok()?;
    if code == "bridge_send_failure" {
        return Some(outbound_entry(entry, frame));
    }
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
    if let Some(rejections) = last.get("attachment_rejections").and_then(Value::as_object) {
        for reason in ATTACHMENT_REJECTION_REASONS {
            if let Some(value) = rejections
                .get(*reason)
                .and_then(Value::as_u64)
                .filter(|value| *value > 0 && *value <= MAX_SAFE_INTEGER)
            {
                entry = entry.with_field(
                    format!("ingress_last_attachment_rejected_{reason}"),
                    value.to_string(),
                );
            }
        }
    }
    Some(entry)
}

fn outbound_entry(mut entry: LogEntry, frame: &Value) -> LogEntry {
    let Some(outbound) = frame.get("outbound") else {
        return entry;
    };
    if outbound.get("version").and_then(Value::as_u64) != Some(1) {
        return entry;
    }
    let Some(stage) = outbound
        .get("stage")
        .and_then(Value::as_str)
        .filter(|stage| {
            matches!(
                *stage,
                "sdk_message_request" | "sdk_token" | "receipt_validation" | "sdk_operation"
            )
        })
    else {
        return entry;
    };
    let Some(kind) = outbound
        .get("failure_kind")
        .and_then(Value::as_str)
        .filter(|kind| {
            matches!(
                *kind,
                "transport_unknown"
                    | "timeout"
                    | "connection"
                    | "dns"
                    | "tls"
                    | "http"
                    | "platform_message_id_unknown"
            )
        })
    else {
        return entry;
    };
    entry = entry
        .with_field("outbound_version", "1")
        .with_field("outbound_stage", stage)
        .with_field("outbound_failure_kind", kind);
    if let Some(value) = outbound
        .get("elapsed_ms")
        .and_then(Value::as_u64)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
    {
        entry = entry.with_field("outbound_elapsed_ms", value.to_string());
    }
    if let Some(value) = outbound
        .get("http_status")
        .and_then(Value::as_u64)
        .filter(|value| (100..=599).contains(value))
    {
        entry = entry.with_field("outbound_http_status", value.to_string());
    }
    entry
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
    fn attachment_rejections_keep_only_fixed_reasons_and_positive_bounded_counts() {
        let frame = json!({"code":"invalid_route_or_text", "ingress": {
            "version":1, "reason":"normalized_text_empty", "last": {
                "accepted_attachment_count":0,
                "attachment_rejections": {
                    "invalid_shape":1, "unsupported_type":2, "invalid_url":3, "quoted":4,
                    "element_not_selected":5, "duplicate":6, "limit":9_007_199_254_740_991_u64,
                    "audio/PRIVATE_MIME":1, "url":"https://PRIVATE_HOST/PRIVATE_TOKEN"
                }
            }
        }});
        let entry = entry(&frame).unwrap();
        assert_eq!(entry.fields.len(), 11);
        for (reason, count) in [
            ("invalid_shape", "1"),
            ("unsupported_type", "2"),
            ("invalid_url", "3"),
            ("quoted", "4"),
            ("element_not_selected", "5"),
            ("duplicate", "6"),
            ("limit", "9007199254740991"),
        ] {
            assert_eq!(
                entry.fields[&format!("ingress_last_attachment_rejected_{reason}")],
                count
            );
        }
        assert!(!format!("{entry:?}").contains("PRIVATE"));
    }

    #[test]
    fn malformed_attachment_counts_do_not_replace_the_original_warning() {
        for rejections in [
            json!(null),
            json!([{"unsupported_type":1}]),
            json!("PRIVATE"),
            json!({
                "invalid_shape":-1, "unsupported_type":"PRIVATE", "invalid_url":1.5,
                "quoted":0, "element_not_selected":null, "duplicate":true,
                "limit":9_007_199_254_740_992_u64, "PRIVATE":1
            }),
        ] {
            let frame = json!({"code":"invalid_route_or_text", "ingress": {
                "version":1, "reason":"normalized_text_empty", "count":1,
                "last":{"accepted_attachment_count":0,"attachment_rejections":rejections}
            }});
            let entry = entry(&frame).unwrap();
            assert_eq!(entry.message, "bridge_invalid_route_or_text");
            assert_eq!(entry.fields.len(), 5);
            assert_eq!(entry.fields["ingress_reason"], "normalized_text_empty");
            assert_eq!(entry.fields["ingress_count"], "1");
            assert!(!format!("{entry:?}").contains("PRIVATE"));
        }
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

    #[test]
    fn send_failure_keeps_only_fixed_stages_kinds_and_bounded_values() {
        for (stage, kind) in [
            ("sdk_message_request", "transport_unknown"),
            ("sdk_token", "timeout"),
            ("receipt_validation", "platform_message_id_unknown"),
            ("receipt_validation", "transport_unknown"),
            ("sdk_operation", "connection"),
            ("sdk_operation", "dns"),
            ("sdk_operation", "tls"),
            ("sdk_message_request", "http"),
        ] {
            let frame = json!({"code":"send_failure", "text":"PRIVATE_TEXT", "outbound": {
                "version":1, "stage":stage, "failure_kind":kind,
                "elapsed_ms":10_674, "http_status":200, "biz_code":"PRIVATE_CODE",
                "message":"PRIVATE_ERROR", "stack":"PRIVATE_STACK", "cause":{"code":"PRIVATE"},
                "target_id":"PRIVATE_ID", "path":"https://PRIVATE_HOST/PRIVATE_TOKEN"
            }});
            let entry = entry(&frame).unwrap();
            assert_eq!(entry.message, "bridge_send_failure");
            assert_eq!(entry.fields.len(), 5);
            assert_eq!(entry.fields["outbound_version"], "1");
            assert_eq!(entry.fields["outbound_stage"], stage);
            assert_eq!(entry.fields["outbound_failure_kind"], kind);
            assert_eq!(entry.fields["outbound_elapsed_ms"], "10674");
            assert_eq!(entry.fields["outbound_http_status"], "200");
            assert!(!format!("{entry:?}").contains("PRIVATE"));
        }
    }

    #[test]
    fn send_failure_rejects_malformed_or_unknown_metadata_without_echoing_it() {
        for outbound in [
            json!(null),
            json!("PRIVATE"),
            json!({"version":2, "stage":"sdk_operation", "failure_kind":"timeout"}),
            json!({"version":1, "stage":"PRIVATE", "failure_kind":"timeout"}),
            json!({"version":1, "stage":"sdk_operation", "failure_kind":"PRIVATE"}),
        ] {
            let frame = json!({"code":"send_failure", "outbound":outbound});
            let entry = entry(&frame).unwrap();
            assert_eq!(entry.message, "bridge_send_failure");
            assert!(entry.fields.is_empty());
            assert!(!format!("{entry:?}").contains("PRIVATE"));
        }
        assert!(
            entry(&json!({"code":"send_failure"}))
                .unwrap()
                .fields
                .is_empty()
        );
        for (elapsed, status) in [
            (json!(-1), json!(99)),
            (json!(9_007_199_254_740_992_u64), json!(600)),
            (json!(1.5), json!(200.5)),
            (json!("PRIVATE"), json!("PRIVATE")),
            (json!(null), json!(null)),
            (json!(true), json!(false)),
        ] {
            let frame = json!({"code":"send_failure", "outbound": {
                "version":1, "stage":"sdk_operation", "failure_kind":"transport_unknown",
                "elapsed_ms":elapsed, "http_status":status
            }});
            let entry = entry(&frame).unwrap();
            assert_eq!(entry.fields.len(), 3);
            assert!(!format!("{entry:?}").contains("PRIVATE"));
        }
    }

    #[test]
    fn successful_http_without_a_receipt_preserves_the_unknown_delivery_mapping() {
        for status in [200, 201, 204, 299] {
            let frame = json!({
                "version":1, "type":"delivery", "ok":false,
                "failure_kind":"transport_unknown", "http_status":status
            });
            assert!(super::super::delivery_unknown(&frame));
            assert_eq!(
                super::super::file_result(&frame, false),
                crate::QqFileResult::Unknown
            );
            assert_eq!(
                super::super::outreach_result(&frame, false),
                crate::QqOutreachResult::Unknown
            );
        }
        let rejected = json!({"ok":false, "failure_kind":"http", "http_status":403});
        assert!(!super::super::delivery_unknown(&rejected));
        assert!(matches!(
            super::super::file_result(&rejected, false),
            crate::QqFileResult::Failed {
                http_status: Some(403),
                ..
            }
        ));
    }
}
