#![allow(dead_code)]
//! 官方文本/函数事件子集的确定性夹具，供 Provider、宿主和示例共用。
use super::http_support::Reply;
use serde_json::{Value, json};

pub fn text(text: &str) -> Value {
    json!({"type":"message","id":"msg-1","role":"assistant","status":"completed",
        "phase":"final_answer","content":[{"type":"output_text","text":text,"annotations":[]}]})
}
pub fn call(id: &str, name: &str, arguments: &str) -> Value {
    json!({"type":"function_call","id":format!("item-{id}"),"call_id":id,"name":name,"arguments":arguments,"status":"completed"})
}
pub fn events(output: Vec<Value>) -> Vec<Value> {
    let mut events = vec![
        json!({"type":"response.created","response":{"id":"resp-1","status":"in_progress","output":[]}}),
    ];
    for (i, item) in output.iter().enumerate() {
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        if item["type"] == "function_call" {
            added["arguments"] = json!("");
        } else {
            added["content"] = json!([]);
        }
        events.push(json!({"type":"response.output_item.added","output_index":i,"item":added}));
        if item["type"] == "function_call" {
            let args = item["arguments"].as_str().unwrap();
            for delta in args.chars().map(|c| c.to_string()) {
                events.push(json!({"type":"response.function_call_arguments.delta","output_index":i,"item_id":item["id"],"delta":delta}));
            }
            events.push(json!({"type":"response.function_call_arguments.done","output_index":i,"item_id":item["id"],"arguments":args}));
        } else {
            for (j, part) in item["content"].as_array().unwrap().iter().enumerate() {
                let mut added = part.clone();
                added["text"] = json!("");
                events.push(json!({"type":"response.content_part.added","output_index":i,"content_index":j,"item_id":item["id"],"part":added}));
                for delta in part["text"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .map(|c| c.to_string())
                {
                    events.push(json!({"type":"response.output_text.delta","output_index":i,"content_index":j,"item_id":item["id"],"delta":delta}));
                }
                events.push(json!({"type":"response.output_text.done","output_index":i,"content_index":j,"item_id":item["id"],"text":part["text"]}));
                events.push(json!({"type":"response.content_part.done","output_index":i,"content_index":j,"item_id":item["id"],"part":part}));
            }
        }
        events.push(json!({"type":"response.output_item.done","output_index":i,"item":item}));
    }
    events.push(json!({"type":"response.completed","response":{"id":"resp-1","status":"completed","error":null,"output":output}}));
    for (i, event) in events.iter_mut().enumerate() {
        event["sequence_number"] = json!(i);
    }
    events
}
pub fn reply(events: &[Value]) -> Reply {
    let mut reply = Reply::json(json!({}));
    reply.headers = vec![(
        "Content-Type".into(),
        "text/event-stream; charset=utf-8".into(),
    )];
    reply.chunked = true;
    reply.body = frames(events).into_bytes();
    reply
}
pub fn frames(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {}\n\n",
                event["type"].as_str().unwrap(),
                event
            )
        })
        .collect()
}
