//! 极简 HTML 转纯文本与链接提取：不执行脚本、不加载外部资源，只取可见文字、标题与 `<a href>`。
//! 不追求完整的 HTML 解析；无法识别的标记按普通标签忽略，不会把标记当作正文。

pub(crate) struct ParsedHtml {
    pub title: String,
    pub text: String,
    /// (href 原值, 链接文字)，按出现顺序。
    pub links: Vec<(String, String)>,
}

/// 内容不可见、整段跳过的元素。
const SKIPPED: [&str; 7] = [
    "script", "style", "noscript", "template", "svg", "iframe", "object",
];
/// 开闭都视为换行的块级元素。
const BLOCKS: [&str; 30] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "td",
    "th",
    "tr",
    "ul",
];

pub(crate) fn parse_html(input: &str) -> ParsedHtml {
    // ASCII 小写不改变字节偏移，只用于大小写不敏感地查找结束标签。
    let lower = input.to_ascii_lowercase();
    let mut text = TextBuilder::default();
    let mut title = TextBuilder::default();
    let mut links = Vec::new();
    let mut anchor: Option<(String, TextBuilder)> = None;
    let mut in_title = false;
    let mut index = 0;
    while index < input.len() {
        if input.as_bytes()[index] != b'<' {
            let end = input[index..]
                .find('<')
                .map_or(input.len(), |offset| index + offset);
            let decoded = decode_entities(&input[index..end]);
            if in_title {
                title.push(&decoded);
            } else {
                text.push(&decoded);
                if let Some((_, label)) = &mut anchor {
                    label.push(&decoded);
                }
            }
            index = end;
            continue;
        }
        if lower[index..].starts_with("<!--") {
            index = lower[index + 4..]
                .find("-->")
                .map_or(input.len(), |end| index + 4 + end + 3);
            continue;
        }
        let Some(close) = input[index..].find('>') else {
            break;
        };
        let tag = &input[index + 1..index + close];
        index += close + 1;
        let (closing, name, attributes) = split_tag(tag);
        if !closing && SKIPPED.contains(&name.as_str()) {
            let needle = format!("</{name}");
            index = lower[index..].find(&needle).map_or(input.len(), |start| {
                let start = index + start;
                input[start..]
                    .find('>')
                    .map_or(input.len(), |end| start + end + 1)
            });
            continue;
        }
        match name.as_str() {
            "title" => in_title = !closing,
            "a" if closing => {
                if let Some((href, label)) = anchor.take() {
                    links.push((href, label.finish()));
                }
            }
            "a" => {
                if let Some((href, label)) = anchor.take() {
                    links.push((href, label.finish()));
                }
                anchor = href_attribute(attributes).map(|href| (href, TextBuilder::default()));
            }
            name if BLOCKS.contains(&name) => {
                text.newline();
                if let Some((_, label)) = &mut anchor {
                    label.space();
                }
            }
            _ => {}
        }
    }
    if let Some((href, label)) = anchor.take() {
        links.push((href, label.finish()));
    }
    ParsedHtml {
        title: title.finish(),
        text: text.finish(),
        links,
    }
}

/// 纯文本与 Markdown：只规范行尾空白与空行，不提取链接。
pub(crate) fn normalize_plain(input: &str) -> String {
    let mut text = TextBuilder::default();
    for line in input.lines() {
        text.push(line);
        text.newline();
    }
    text.finish()
}

fn split_tag(tag: &str) -> (bool, String, &str) {
    let tag = tag.trim();
    let (closing, rest) = match tag.strip_prefix('/') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, tag),
    };
    let end = rest
        .find(|value: char| !value.is_ascii_alphanumeric())
        .unwrap_or(rest.len());
    (closing, rest[..end].to_ascii_lowercase(), &rest[end..])
}

fn href_attribute(attributes: &str) -> Option<String> {
    let lower = attributes.to_ascii_lowercase();
    let mut search = 0;
    while let Some(found) = lower[search..].find("href") {
        let start = search + found;
        search = start + 4;
        let before = lower[..start].chars().next_back();
        if before.is_some_and(|value| !value.is_whitespace() && value != '/') {
            continue;
        }
        let rest = attributes[search..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let body = &rest[1..];
                &body[..body.find(quote)?]
            }
            Some(_) => {
                let end = rest
                    .find(|value: char| value.is_whitespace() || value == '>')
                    .unwrap_or(rest.len());
                &rest[..end]
            }
            None => return None,
        };
        let value = decode_entities(value).trim().to_string();
        return (!value.is_empty()).then_some(value);
    }
    None
}

fn decode_entities(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let decoded = rest[1..]
            .find(';')
            .filter(|end| *end <= 10)
            .and_then(|end| entity(&rest[1..=end]).map(|value| (value, end + 2)));
        match decoded {
            Some((value, length)) => {
                output.push(value);
                rest = &rest[length..];
            }
            None => {
                output.push('&');
                rest = &rest[1..];
            }
        }
    }
    output.push_str(rest);
    output
}

fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ => {
            let number = name.strip_prefix('#')?;
            let value = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(value).filter(|value| !value.is_control() || value.is_whitespace())?
        }
    })
}

/// 合并连续空白为一个空格；块级边界保留单个换行。
#[derive(Default)]
struct TextBuilder {
    text: String,
    pending_space: bool,
}
impl TextBuilder {
    fn push(&mut self, input: &str) {
        for value in input.chars() {
            if value.is_whitespace() {
                self.pending_space = true;
            } else if !value.is_control() {
                if self.pending_space && !self.text.is_empty() && !self.text.ends_with('\n') {
                    self.text.push(' ');
                }
                self.pending_space = false;
                self.text.push(value);
            }
        }
    }
    fn space(&mut self) {
        self.pending_space = true;
    }
    fn newline(&mut self) {
        self.pending_space = false;
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
    }
    fn finish(self) -> String {
        self.text.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_visible_text_title_and_links_but_drops_scripts_styles_and_comments() {
        let parsed = parse_html(
            r#"<!DOCTYPE html><html><head><title>Mod &amp; Guide</title>
            <style>body { color: red }</style><script>alert("<p>x</p>")</script></head>
            <body><h1>Getting   started</h1><!-- hidden <a href="/secret">s</a> -->
            <p>Put the mod in the <b>mods</b> folder.&nbsp;Version&#32;146 &#x2014; ok</p>
            <a href="/wiki/modding/blocks.html">Blocks <i>guide</i></a>
            <A class=x HREF='next.html#top'>Next</A> <a name="anchor">no href</a>
            <a data-href="/x" href=plain.html>Plain</a> <ScRiPt>steal()</sCrIpT>tail</body></html>"#,
        );
        assert_eq!(parsed.title, "Mod & Guide");
        assert_eq!(
            parsed.text,
            "Getting started\nPut the mod in the mods folder. Version 146 \u{2014} ok\nBlocks guide Next no href Plain tail"
        );
        assert_eq!(
            parsed.links,
            vec![
                ("/wiki/modding/blocks.html".into(), "Blocks guide".into()),
                ("next.html#top".into(), "Next".into()),
                ("plain.html".into(), "Plain".into()),
            ]
        );
    }

    #[test]
    fn plain_text_is_normalized_without_links_and_entities_are_not_overdecoded() {
        assert_eq!(normalize_plain("  a  b \n\n\n c\t"), "a b\nc");
        assert_eq!(
            decode_entities("&amp;lt; &unknown; &#0; &"),
            "&lt; &unknown; &#0; &"
        );
        assert_eq!(parse_html("<p>unterminated <b").text, "unterminated");
    }
}
