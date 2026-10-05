use eve_segment_api::{SegmentLimits, SegmentPlan, SegmentPlanner, SegmentRequest, plan_or_single};
use eve_segment_plugin::{PARAGRAPH_PLANNER, ParagraphPlanner};

const QQ: SegmentLimits = SegmentLimits {
    max_segments: 3,
    max_segment_bytes: 32_768,
    max_pause_ms: 2_500,
};

fn plan(text: &str) -> SegmentPlan {
    let plan = ParagraphPlanner::default()
        .plan(&SegmentRequest { text, limits: QQ })
        .unwrap();
    plan.validate(text, &QQ).unwrap();
    assert_eq!(plan.planner, PARAGRAPH_PLANNER);
    plan
}
fn parts(text: &str) -> Vec<String> {
    plan(text).texts(text).map(str::to_owned).collect()
}

#[test]
fn single_paragraph_replies_are_sent_whole() {
    assert_eq!(parts("好的，主人。"), ["好的，主人。"]);
    let one = "这是一个比较长的单段回复，没有空行也没有换行，所以无论多长都应该作为一条消息完整发送给用户。";
    assert_eq!(parts(one), [one]);
}

#[test]
fn short_natural_paragraphs_are_separate_messages() {
    assert_eq!(
        parts("好。\n\n你先说，我听着。"),
        ["好。", "你先说，我听着。"]
    );
    assert_eq!(parts("好的。\n马上处理。"), ["好的。", "马上处理。"]);
}

#[test]
fn explicit_minimum_length_can_keep_short_paragraphs_together() {
    let text = "好的。\n\n马上处理。";
    let planner = ParagraphPlanner {
        min_chars: 40,
        ..ParagraphPlanner::default()
    };
    let plan = planner.plan(&SegmentRequest { text, limits: QQ }).unwrap();
    plan.validate(text, &QQ).unwrap();
    assert_eq!(plan.texts(text).collect::<Vec<_>>(), [text]);
}

#[test]
fn natural_paragraphs_become_ordered_segments_with_bounded_pauses() {
    let text = "好的主人，结论是可以做到。\n\n原因是现有接口已经保存了完整回复，分段只决定在哪里断开，不会改写内容。\n\n要不要我现在帮你试一下？\n";
    let plan = plan(text);
    assert_eq!(
        plan.texts(text).collect::<Vec<_>>(),
        [
            "好的主人，结论是可以做到。",
            "原因是现有接口已经保存了完整回复，分段只决定在哪里断开，不会改写内容。",
            "要不要我现在帮你试一下？",
        ]
    );
    assert_eq!(plan.segments[0].pause_before_ms, 0);
    let middle = "原因是现有接口已经保存了完整回复，分段只决定在哪里断开，不会改写内容。";
    assert_eq!(
        plan.segments[1].pause_before_ms,
        400 + 25 * middle.chars().count() as u64
    );
    let tail_chars = "要不要我现在帮你试一下？".chars().count() as u64;
    assert_eq!(plan.segments[2].pause_before_ms, 400 + 25 * tail_chars);
    let long = format!("开头。\n\n{}", "很长的解释内容。".repeat(20));
    let long_plan = ParagraphPlanner::default()
        .plan(&SegmentRequest {
            text: &long,
            limits: QQ,
        })
        .unwrap();
    assert_eq!(long_plan.segments[1].pause_before_ms, 2_500);
}

#[test]
fn code_blocks_lists_headings_and_intro_lines_stay_together() {
    let text = "这里是修改后的函数，可以直接替换原来的实现：\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\n## 注意事项\n\n- 第一项需要先保存。\n\n- 第二项不要重试。\n\n有问题随时告诉我。";
    let got = parts(text);
    assert_eq!(got.len(), 3);
    assert!(got[0].starts_with("这里是修改后的函数") && got[0].ends_with("```"));
    assert!(got[0].contains("fn a() {}\n\nfn b() {}"));
    assert!(got[1].starts_with("## 注意事项") && got[1].ends_with("第二项不要重试。"));
    assert_eq!(got[2], "有问题随时告诉我。");
}

#[test]
fn lines_without_blank_separators_split_only_between_prose_sentences() {
    let text = "好的，我看完了这份计划，整体方向没有问题。\n需要注意的步骤：\n1. 先备份状态目录。\n2. 再换新的数据库配置。\n做完这些之后告诉我结果，我再帮你核对恢复语义是否正确。";
    let got = parts(text);
    assert_eq!(
        got,
        [
            "好的，我看完了这份计划，整体方向没有问题。",
            "需要注意的步骤：\n1. 先备份状态目录。\n2. 再换新的数据库配置。",
            "做完这些之后告诉我结果，我再帮你核对恢复语义是否正确。",
        ]
    );
}

#[test]
fn many_paragraphs_merge_middle_explanations_first_and_keep_crlf_content() {
    let text = "可以。\r\n\r\n第一点解释比较长，需要说明当前接口的边界和保存顺序。\r\n\r\n第二点解释同样需要展开，说明失败时为什么不重试。\r\n\r\n第三点解释补充恢复后的检查。\r\n\r\n还需要我继续吗？";
    let got = parts(text);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], "可以。");
    assert!(got[1].starts_with("第一点") && got[1].ends_with("恢复后的检查。"));
    assert!(got[1].contains("\r\n\r\n第二点"));
    assert_eq!(got[2], "还需要我继续吗？");
}

#[test]
fn punctuation_only_fragments_and_rules_do_not_become_separate_messages() {
    let text = "第一部分说明现在的状态，已经保存完成并且可以查看。\n\n---\n\n第二部分说明下一步要做什么，需要你确认后再继续。\n\n😊";
    let got = parts(text);
    assert_eq!(got.len(), 2);
    assert!(got[0].ends_with("---"));
    assert!(got[1].ends_with("再继续。\n\n😊"));
}

#[test]
fn unsatisfiable_budget_falls_back_to_the_whole_reply() {
    let text = "第一段内容足够长，可以单独作为一条消息发送出去。\n\n第二段内容也足够长，同样可以单独发送。\n\n第三段。";
    let tight = SegmentLimits {
        max_segments: 2,
        max_segment_bytes: 60,
        max_pause_ms: 1_000,
    };
    assert!(
        ParagraphPlanner::default()
            .plan(&SegmentRequest {
                text,
                limits: tight
            })
            .is_err()
    );
    let roomy = SegmentLimits {
        max_segment_bytes: text.len(),
        ..tight
    };
    let (plan, warning) = plan_or_single(&ParagraphPlanner::default(), text, roomy).unwrap();
    assert!(warning.is_none());
    assert_eq!(plan.segments.len(), 2);
    assert!(plan.segments[1].pause_before_ms <= 1_000);
    let one = SegmentLimits {
        max_segments: 1,
        ..roomy
    };
    let (plan, _) = plan_or_single(&ParagraphPlanner::default(), text, one).unwrap();
    assert_eq!(plan.segments.len(), 1);
}

const FIRST: &str = "第一部分说明现在的状态，已经保存完成并且可以查看。";
const SECOND: &str = "第二部分说明下一步要做什么，需要你确认后再继续。";

#[test]
fn indented_and_list_nested_code_stays_whole() {
    let nested = "步骤如下：\n\n1. 安装依赖：\n    ```bash\n    npm install\n\n    npm run build\n    ```\n2. 启动服务。\n\n完成后告诉我结果，我再帮你检查日志和配置是否正确。";
    let got = parts(nested);
    assert_eq!(got.len(), 2);
    assert!(got[0].contains("npm install\n\n    npm run build\n    ```\n2. 启动服务。"));
    let indented =
        format!("{FIRST}示例代码如下：\n\n    fn a() {{}}\n\n    fn b() {{}}\n\n{SECOND}");
    let got = parts(&indented);
    assert_eq!(got.len(), 2);
    assert!(got[0].ends_with("fn a() {}\n\n    fn b() {}"));
    let inline = "```ls``` 可以列出文件，下面是完整脚本：\n\n```\necho a\n\necho b\n```\n\n运行后把输出发给我，我再帮你看看是哪里出了问题。";
    let got = parts(inline);
    assert_eq!(got.len(), 2);
    assert!(got[0].ends_with("echo a\n\necho b\n```"));
}

#[test]
fn list_continuations_and_chinese_or_emphasized_lists_stay_together() {
    let continuation = "需要注意：\n1. 先备份状态目录。\n   备份前先停止服务。\n2. 再换新的数据库配置。\n做完之后告诉我，我再帮你核对一遍。";
    assert_eq!(
        parts(continuation),
        [
            "需要注意：\n1. 先备份状态目录。\n   备份前先停止服务。\n2. 再换新的数据库配置。",
            "做完之后告诉我，我再帮你核对一遍。",
        ]
    );
    for list in [
        "注意以下几点：\n（1）先备份状态目录里的全部文件。\n（2）再换新的数据库配置。\n（3）最后重启服务并检查日志。",
        "注意以下几点：\n① 先备份状态目录里的全部文件。\n② 再换新的数据库配置。\n③ 最后重启服务并检查日志。",
        "注意以下几点：\n一、先备份状态目录里的全部文件。\n二、再换新的数据库配置。\n三、最后重启服务并检查日志。",
        "**1. 备份**：先备份状态目录里的全部文件。\n**2. 配置**：再换新的数据库配置。\n**3. 重启**：最后重启服务并检查日志。",
    ] {
        assert_eq!(parts(list), [list]);
    }
    let bold_intro = "**具体步骤如下：**\n\n1. 先备份状态目录。\n2. 再换新的数据库配置。\n\n完成后告诉我结果，我再帮你检查。";
    let got = parts(bold_intro);
    assert_eq!(got.len(), 2);
    assert!(got[0].starts_with("**具体步骤如下：**\n\n1."));
    // 普通句子开头的数字或数词不是列表。
    let prose = "2024年的计划已经整理好了，主要是完善状态恢复。\n十分感谢你的耐心，我们下一步先补测试再扩展功能。";
    assert_eq!(parts(prose).len(), 2);
}

#[test]
fn content_free_blocks_and_rules_after_headings_never_stand_alone() {
    for tail in ["❤️", "👍🏻", "……", "！！", "\u{feff}\u{feff}"] {
        let got = parts(&format!("{FIRST}\n\n{SECOND}\n\n{tail}"));
        assert_eq!(got.len(), 2, "{tail:?}");
        assert!(got[1].ends_with(tail));
        let got = parts(&format!("{FIRST}\n\n{tail}\n\n{SECOND}"));
        assert_eq!(got, [format!("{FIRST}\n\n{tail}"), SECOND.to_string()]);
    }
    let heading = format!("## 第一步\n\n---\n\n{FIRST}\n\n{SECOND}");
    let got = parts(&heading);
    assert_eq!(got[0], format!("## 第一步\n\n---\n\n{FIRST}"));
}

#[test]
fn many_short_paragraphs_are_sent_whole_quickly() {
    let text = "ab\n\n".repeat(8192);
    let text = &text[..32_768];
    let started = std::time::Instant::now();
    let whole = plan(text);
    assert_eq!(whole.segments.len(), 1);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    let sixty_four: String = (0..64).map(|i| format!("第{i}段内容。\n\n")).collect();
    assert_eq!(plan(&sixty_four).segments.len(), 3);
    let sixty_five: String = (0..65).map(|i| format!("第{i}段内容。\n\n")).collect();
    assert_eq!(plan(&sixty_five).segments.len(), 1);
}
