//! ReqwestEgress —— 基于 `reqwest` 的 NetEgress 实现。
//!
//! - `rustls-tls` 默认(无 OpenSSL 依赖,移动端友好)
//! - CancelToken 会中断请求和响应体读取

use async_trait::async_trait;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::core::cancel::CancelToken;
use crate::core::net::{HttpMethod, HttpReq, HttpResp, NetEgress, NetErr};

pub struct ReqwestEgress {
    client: reqwest::Client,
}

impl ReqwestEgress {
    pub fn new() -> Result<Self, NetErr> {
        let timeout_secs = std::env::var("MUAGENT_HTTP_TIMEOUT_SEC")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .unwrap_or(60);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| NetErr::Io(e.to_string()))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl NetEgress for ReqwestEgress {
    async fn http(&self, req: HttpReq, cancel: CancelToken) -> Result<HttpResp, NetErr> {
        self.http_inner(req, cancel, None).await
    }

    async fn http_with_body_chunks(
        &self,
        req: HttpReq,
        cancel: CancelToken,
        chunks: Option<mpsc::UnboundedSender<Vec<u8>>>,
    ) -> Result<HttpResp, NetErr> {
        self.http_inner(req, cancel, chunks).await
    }
}

impl ReqwestEgress {
    async fn http_inner(
        &self,
        req: HttpReq,
        cancel: CancelToken,
        chunks: Option<mpsc::UnboundedSender<Vec<u8>>>,
    ) -> Result<HttpResp, NetErr> {
        if cancel.triggered() {
            return Err(NetErr::Cancelled);
        }

        let url = reqwest::Url::parse(&req.url).map_err(|e| NetErr::Connect(e.to_string()))?;

        let method = match req.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Delete => reqwest::Method::DELETE,
            HttpMethod::Patch => reqwest::Method::PATCH,
            HttpMethod::Head => reqwest::Method::HEAD,
            HttpMethod::Options => reqwest::Method::OPTIONS,
        };

        let mut rb = self.client.request(method, url);
        for (k, v) in &req.headers {
            rb = rb.header(k, v);
        }
        if let Some(body) = req.body {
            rb = rb.body(body);
        }

        let resp = tokio::select! {
            resp = rb.send() => resp.map_err(map_reqwest_err)?,
            _ = wait_cancel(cancel.child()) => return Err(NetErr::Cancelled),
        };

        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.to_string(), s.to_string())))
            .collect();
        let mut body = Vec::new();
        let mut resp = resp;
        loop {
            let chunk = tokio::select! {
                chunk = resp.chunk() => chunk.map_err(|e| NetErr::Io(e.to_string()))?,
                _ = wait_cancel(cancel.child()) => return Err(NetErr::Cancelled),
            };
            let Some(chunk) = chunk else {
                break;
            };
            if let Some(tx) = &chunks {
                let _ = tx.send(chunk.to_vec());
            }
            body.extend_from_slice(&chunk);
        }

        Ok(HttpResp {
            status,
            headers,
            body,
        })
    }
}

fn map_reqwest_err(e: reqwest::Error) -> NetErr {
    if e.is_timeout() {
        NetErr::Timeout
    } else if e.is_connect() {
        NetErr::Connect(e.to_string())
    } else {
        NetErr::Io(e.to_string())
    }
}

async fn wait_cancel(cancel: CancelToken) {
    while !cancel.triggered() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
