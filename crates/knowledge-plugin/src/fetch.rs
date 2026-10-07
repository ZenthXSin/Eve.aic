//! 受控 HTTP 抓取：只 GET 来源策略允许的 URL，逐跳核对重定向目标，限制大小、类型与时长。
//! 不携带凭据或 Cookie，不执行脚本，不加载页面引用的其他资源。
use crate::html;
use eve_knowledge_api::{
    BoxFuture, FetchFailure, FetchedPage, KnowledgeError, KnowledgeResult, MAX_DOCUMENT_TEXT_BYTES,
    MAX_FETCH_BYTES, MAX_LABEL_BYTES, MAX_PAGE_LINKS, MAX_REDIRECTS, PageLink, SourceFetcher,
    SourcePolicy,
};
use reqwest::{StatusCode, Url, header, redirect::Policy};
use ring::digest::{Context, SHA256};
use std::{collections::BTreeSet, time::Duration};

const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const ACCEPT: &str = "text/html, application/xhtml+xml, text/plain, text/markdown;q=0.9";
const SUPPORTED: [&str; 4] = [
    "text/html",
    "application/xhtml+xml",
    "text/plain",
    "text/markdown",
];

pub struct HttpSourceFetcher {
    client: reqwest::Client,
}

impl HttpSourceFetcher {
    pub fn new() -> KnowledgeResult<Self> {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(FETCH_TIMEOUT)
            .user_agent(concat!("Eve-Research/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| KnowledgeError::Unavailable)?;
        Ok(Self { client })
    }

    async fn fetch_page(
        &self,
        policy: &SourcePolicy,
        url: &str,
    ) -> Result<FetchedPage, FetchFailure> {
        if !policy.allows(url) {
            return Err(FetchFailure::NotAllowed);
        }
        let mut current = Url::parse(url).map_err(|_| FetchFailure::NotAllowed)?;
        let mut redirects = 0;
        let response = loop {
            let response = self
                .client
                .get(current.clone())
                .header(header::ACCEPT, ACCEPT)
                .send()
                .await
                .map_err(transport)?;
            if !response.status().is_redirection() || response.status() == StatusCode::NOT_MODIFIED
            {
                break response;
            }
            if redirects == MAX_REDIRECTS {
                return Err(FetchFailure::TooManyRedirects);
            }
            redirects += 1;
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or(FetchFailure::HttpStatus(response.status().as_u16()))?;
            let mut next = current
                .join(location)
                .map_err(|_| FetchFailure::NotAllowed)?;
            next.set_fragment(None);
            // 重定向目标同样须在允许范围内；不能借跳转离开操作者配置的站点目录。
            if !policy.allows(next.as_str()) {
                return Err(FetchFailure::NotAllowed);
            }
            current = next;
        };
        if !response.status().is_success() {
            return Err(FetchFailure::HttpStatus(response.status().as_u16()));
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| SUPPORTED.contains(&value.as_str()))
            .ok_or(FetchFailure::UnsupportedType)?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_FETCH_BYTES)
        {
            return Err(FetchFailure::TooLarge);
        }
        let mut response = response;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            if body.len() as u64 + chunk.len() as u64 > MAX_FETCH_BYTES {
                return Err(FetchFailure::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        if body.is_empty() {
            return Err(FetchFailure::Empty);
        }
        let mut digest = Context::new(&SHA256);
        digest.update(&body);
        let sha256 = digest
            .finish()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let decoded = String::from_utf8_lossy(&body);
        let (title, text, raw_links) = if content_type.contains("html") {
            let parsed = html::parse_html(&decoded);
            (parsed.title, parsed.text, parsed.links)
        } else {
            (String::new(), html::normalize_plain(&decoded), Vec::new())
        };
        let final_url = String::from(current.clone());
        let mut seen = BTreeSet::from([final_url.clone()]);
        let mut links = Vec::new();
        for (href, label) in raw_links {
            if links.len() == MAX_PAGE_LINKS {
                break;
            }
            let Ok(mut target) = current.join(&href) else {
                continue;
            };
            target.set_fragment(None);
            let target = String::from(target);
            if policy.allows(&target) && seen.insert(target.clone()) {
                links.push(PageLink {
                    url: target,
                    text: prefix(&label, MAX_LABEL_BYTES).to_string(),
                });
            }
        }
        let text_truncated = text.len() > MAX_DOCUMENT_TEXT_BYTES;
        let page = FetchedPage {
            final_url,
            content_type,
            sha256,
            byte_count: body.len() as u64,
            title: prefix(&title, MAX_LABEL_BYTES).to_string(),
            text: prefix(&text, MAX_DOCUMENT_TEXT_BYTES)
                .trim_end()
                .to_string(),
            text_truncated,
            links,
        };
        if page.text.is_empty() {
            return Err(FetchFailure::Empty);
        }
        page.validate(policy).map_err(|_| FetchFailure::Empty)?;
        Ok(page)
    }
}

impl SourceFetcher for HttpSourceFetcher {
    fn fetch<'a>(
        &'a self,
        policy: &'a SourcePolicy,
        url: &'a str,
    ) -> BoxFuture<'a, Result<FetchedPage, FetchFailure>> {
        Box::pin(self.fetch_page(policy, url))
    }
}

fn transport(error: reqwest::Error) -> FetchFailure {
    if error.is_timeout() {
        FetchFailure::Timeout
    } else {
        FetchFailure::Network
    }
}

pub(crate) fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
