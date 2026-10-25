//! 取得来源的 HTTP 下载：只接受 HTTPS（本机回环地址可用 HTTP，供本地验收），逐跳核对重定向，
//! 不携带凭据或 Cookie，按条件的大小上限截断。
use eve_provision_api::ProvisionError;
use reqwest::{Url, header, redirect::Policy};
use std::{net::IpAddr, time::Duration};

const MAX_REDIRECTS: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// 单次下载的总时长上限；大文件经慢速网络也应在此之内完成。
const TOTAL_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) struct Fetcher {
    client: reqwest::Client,
}

pub(crate) fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "下载超时".into()
    } else if error.is_connect() {
        "无法连接".into()
    } else {
        "传输中断".into()
    }
}

fn loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

fn allowed(url: &Url) -> bool {
    url.username().is_empty()
        && url.password().is_none()
        && match url.scheme() {
            "https" => url.host_str().is_some(),
            "http" => loopback(url),
            _ => false,
        }
}

impl Fetcher {
    pub(crate) fn new() -> Result<Self, ProvisionError> {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .user_agent(concat!("Eve-Provision/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| ProvisionError::Unavailable)?;
        Ok(Self { client })
    }

    /// 跟随重定向直到拿到成功响应；声明的长度超过上限时直接拒绝。
    pub(crate) async fn open(
        &self,
        url: &str,
        max_bytes: u64,
    ) -> Result<reqwest::Response, ProvisionError> {
        let mut current =
            Url::parse(url).map_err(|_| ProvisionError::InvalidInput("下载地址无法解析".into()))?;
        if !allowed(&current) {
            return Err(ProvisionError::InvalidInput(
                "下载地址须为 HTTPS，且不能包含账号密码".into(),
            ));
        }
        for _ in 0..=MAX_REDIRECTS {
            let response = self
                .client
                .get(current.clone())
                .send()
                .await
                .map_err(|e| ProvisionError::Fetch(describe(&e)))?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| ProvisionError::Fetch(format!("HTTP {}", status.as_u16())))?;
                let next = current
                    .join(location)
                    .map_err(|_| ProvisionError::Fetch("重定向地址无效".into()))?;
                if !allowed(&next) {
                    return Err(ProvisionError::Fetch("重定向到了不允许的地址".into()));
                }
                current = next;
                continue;
            }
            if !status.is_success() {
                return Err(ProvisionError::Fetch(format!("HTTP {}", status.as_u16())));
            }
            if response.content_length().is_some_and(|n| n > max_bytes) {
                return Err(ProvisionError::TooLarge);
            }
            return Ok(response);
        }
        Err(ProvisionError::Fetch("重定向次数过多".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_or_loopback_http_without_credentials() {
        let ok = |u: &str| allowed(&Url::parse(u).unwrap());
        assert!(ok("https://github.com/a/b.jar"));
        assert!(ok("http://127.0.0.1:8080/x") && ok("http://localhost/x") && ok("http://[::1]/x"));
        assert!(!ok("http://example.com/x") && !ok("ftp://example.com/x"));
        assert!(!ok("https://user:pw@example.com/x") && !ok("file:///etc/passwd"));
    }
}
