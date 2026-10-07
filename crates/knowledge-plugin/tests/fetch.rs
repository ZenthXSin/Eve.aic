use eve_knowledge_api::*;
use eve_knowledge_plugin::HttpSourceFetcher;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

struct Route {
    status: &'static str,
    headers: Vec<String>,
    body: Vec<u8>,
}

/// 只在本机回环地址监听的最小 HTTP/1.1 服务；记录请求行与请求头。
struct Site {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

async fn site(routes: BTreeMap<&'static str, Route>) -> Site {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    let routes = Arc::new(routes);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let routes = routes.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0; 1024];
                while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                }
                let request = String::from_utf8_lossy(&buffer).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                log.lock().unwrap().push(request);
                let response = match routes.get(path.as_str()) {
                    Some(route) => {
                        let mut head =
                            format!("HTTP/1.1 {}\r\nConnection: close\r\n", route.status);
                        for header in &route.headers {
                            head.push_str(header);
                            head.push_str("\r\n");
                        }
                        if !route.headers.iter().any(|header| {
                            header.starts_with("Content-Length") || header.starts_with("X-Stream")
                        }) {
                            head.push_str(&format!("Content-Length: {}\r\n", route.body.len()));
                        }
                        head.push_str("\r\n");
                        let mut bytes = head.into_bytes();
                        bytes.extend_from_slice(&route.body);
                        bytes
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Site { base, requests }
}

fn html(body: &str) -> Route {
    Route {
        status: "200 OK",
        headers: vec!["Content-Type: text/html; charset=utf-8".into()],
        body: body.as_bytes().to_vec(),
    }
}

fn redirect(location: String) -> Route {
    Route {
        status: "302 Found",
        headers: vec![format!("Location: {location}")],
        body: vec![],
    }
}

#[tokio::test]
async fn fetches_allowed_pages_follows_in_scope_redirects_and_keeps_only_in_scope_links() {
    let index = r#"<html><head><title>Mod docs</title><script>fetch("/steal")</script></head>
        <body><h1>Modding</h1><p>Start here.</p>
        <a href="start.html#top">Getting started</a>
        <a href="/docs/blocks.html">Blocks</a>
        <a href="/docs/start.html">Duplicate</a>
        <a href="/private/admin.html">Admin</a>
        <a href="https://evil.example/docs/x.html">Elsewhere</a>
        <a href="mailto:a@b.c">Mail</a></body></html>"#;
    let mut routes = BTreeMap::new();
    routes.insert("/docs/index.html", html(index));
    routes.insert(
        "/docs/plain.txt",
        Route {
            status: "200 OK",
            headers: vec!["Content-Type: text/plain".into()],
            body: b"  version 146 \n\n notes ".to_vec(),
        },
    );
    let site = site(routes).await;
    let seed = format!("{}/docs/index.html", site.base);
    let policy = SourcePolicy::new(std::slice::from_ref(&seed)).unwrap();
    let fetcher = HttpSourceFetcher::new().unwrap();
    let page = fetcher.fetch(&policy, &seed).await.unwrap();
    assert_eq!(page.final_url, seed);
    assert_eq!(page.title, "Mod docs");
    assert_eq!(page.content_type, "text/html");
    assert_eq!(page.byte_count, index.len() as u64);
    assert!(page.text.starts_with("Modding\nStart here."));
    assert!(!page.text.contains("steal"));
    let links: Vec<_> = page
        .links
        .iter()
        .map(|link| {
            (
                link.url.strip_prefix(&site.base).unwrap(),
                link.text.as_str(),
            )
        })
        .collect();
    assert_eq!(
        links,
        [
            ("/docs/start.html", "Getting started"),
            ("/docs/blocks.html", "Blocks")
        ]
    );
    page.validate(&policy).unwrap();
    let plain = fetcher
        .fetch(&policy, &format!("{}/docs/plain.txt", site.base))
        .await
        .unwrap();
    assert_eq!(plain.text, "version 146\nnotes");
    assert!(plain.links.is_empty());
    let request = site.requests.lock().unwrap()[0].to_ascii_lowercase();
    assert!(request.starts_with("get /docs/index.html http/1.1"));
    assert!(!request.contains("cookie") && !request.contains("authorization"));
    assert!(request.contains("user-agent: eve-research/"));
}

#[tokio::test]
async fn redirects_out_of_scope_size_type_status_and_scope_violations_are_refused() {
    let mut routes = BTreeMap::new();
    let large = vec![b'a'; MAX_FETCH_BYTES as usize + 1];
    routes.insert(
        "/docs/large.html",
        Route {
            status: "200 OK",
            headers: vec!["Content-Type: text/html".into()],
            body: large.clone(),
        },
    );
    routes.insert(
        "/docs/streamed.html",
        Route {
            status: "200 OK",
            // 不声明长度、以关闭连接结束：读取时累计字节，超过上限即放弃。
            headers: vec!["Content-Type: text/html".into(), "X-Stream: close".into()],
            body: large.clone(),
        },
    );
    routes.insert(
        "/docs/zero.html",
        Route {
            status: "200 OK",
            headers: vec!["Content-Type: text/html".into(), "Content-Length: 0".into()],
            body: vec![],
        },
    );
    routes.insert(
        "/docs/image.png",
        Route {
            status: "200 OK",
            headers: vec!["Content-Type: image/png".into()],
            body: vec![1, 2, 3],
        },
    );
    routes.insert("/docs/empty.html", html("<script>only()</script>"));
    routes.insert("/docs/target.html", html("<p>redirected content</p>"));
    routes.insert(
        "/docs/gone.html",
        Route {
            status: "410 Gone",
            headers: vec!["Content-Type: text/html".into()],
            body: b"gone".to_vec(),
        },
    );
    let site = site(routes).await;
    let base = site.base.clone();
    let mut routes = BTreeMap::new();
    routes.insert("/docs/out.html", redirect("/private/secret.html".into()));
    routes.insert(
        "/docs/away.html",
        redirect("https://evil.example/docs/x.html".into()),
    );
    routes.insert(
        "/docs/in.html",
        redirect(format!("{base}/docs/target.html#frag")),
    );
    routes.insert("/docs/loop.html", redirect("/docs/loop.html".into()));
    let second = self::site(routes).await;
    let seeds = [
        format!("{}/docs/", site.base),
        format!("{}/docs/", second.base),
    ];
    let policy = SourcePolicy::new(&seeds).unwrap();
    let fetcher = HttpSourceFetcher::new().unwrap();
    let fetch = |url: String| {
        let fetcher = &fetcher;
        let policy = &policy;
        async move { fetcher.fetch(policy, &url).await }
    };
    assert_eq!(
        fetch(format!("{}/docs/large.html", site.base)).await.err(),
        Some(FetchFailure::TooLarge)
    );
    assert_eq!(
        fetch(format!("{}/docs/image.png", site.base)).await.err(),
        Some(FetchFailure::UnsupportedType)
    );
    assert_eq!(
        fetch(format!("{}/docs/empty.html", site.base)).await.err(),
        Some(FetchFailure::Empty)
    );
    assert_eq!(
        fetch(format!("{}/docs/streamed.html", site.base))
            .await
            .err(),
        Some(FetchFailure::TooLarge)
    );
    assert_eq!(
        fetch(format!("{}/docs/zero.html", site.base)).await.err(),
        Some(FetchFailure::Empty)
    );
    assert_eq!(
        fetch(format!("{}/docs/gone.html", site.base)).await.err(),
        Some(FetchFailure::HttpStatus(410))
    );
    assert_eq!(
        fetch(format!("{}/private/x.html", site.base)).await.err(),
        Some(FetchFailure::NotAllowed)
    );
    assert_eq!(
        fetch(format!("{}/docs/out.html", second.base)).await.err(),
        Some(FetchFailure::NotAllowed)
    );
    assert_eq!(
        fetch(format!("{}/docs/away.html", second.base)).await.err(),
        Some(FetchFailure::NotAllowed)
    );
    assert_eq!(
        fetch(format!("{}/docs/loop.html", second.base)).await.err(),
        Some(FetchFailure::TooManyRedirects)
    );
    // 跨入口页面范围的重定向允许；最终 URL 去掉片段。
    let page = fetch(format!("{}/docs/in.html", second.base))
        .await
        .unwrap();
    assert_eq!(page.final_url, format!("{base}/docs/target.html"));
    assert_eq!(page.text, "redirected content");
    let first: Vec<String> = site
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.split_whitespace().nth(1).unwrap().to_string())
        .collect();
    assert!(
        !first.iter().any(|path| path.starts_with("/private")),
        "范围外 URL 从未被请求"
    );
    let second_paths: Vec<String> = second
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.split_whitespace().nth(1).unwrap().to_string())
        .collect();
    assert_eq!(
        second_paths
            .iter()
            .filter(|path| *path == "/docs/loop.html")
            .count(),
        1 + MAX_REDIRECTS
    );
}
