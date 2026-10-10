//! Native HTTP and clock primitives, shared by every execution host.
use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use openwebide_plugin_runtime::sdk::{HttpRequest, HttpResponse};

pub async fn request(payload: &str) -> Result<String, String> {
    let input: HttpRequest = serde_json::from_str(payload).map_err(|error| error.to_string())?;
    if input.url.len() > 4096 || input.headers.len() > 32 || input.body_base64.len() > 512 * 1024 {
        return Err("Plugin HTTP request exceeds its limit.".into());
    }
    let uri = input
        .url
        .parse::<hyper::Uri>()
        .map_err(|_| "Invalid plugin HTTP URL")?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err("Plugin HTTP requests require HTTP or HTTPS.".into());
    }
    openwebide_core::network::check_http_host(uri.host().ok_or("HTTP URL has no host")?)?;
    let body = base64::engine::general_purpose::STANDARD
        .decode(input.body_base64)
        .map_err(|error| error.to_string())?;
    let mut request = hyper::Request::builder()
        .method(input.method.as_str())
        .uri(uri);
    for (name, value) in input.headers {
        if name.len() > 128 || value.len() > 8192 {
            return Err("Plugin HTTP header exceeds its limit.".into());
        }
        request = request.header(name, value);
    }
    let request = request
        .body(Full::new(Bytes::from(body)))
        .map_err(|error| error.to_string())?;
    let response = crate::runs::http_client::ReqwestHttpClient::default()
        .send(request)
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .take(64)
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .filter(|value| value.len() <= 8192)
                .map(|value| (name.to_string(), value.to_owned()))
        })
        .collect();
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| error.to_string())?;
        if let Ok(data) = frame.into_data() {
            let remaining = (2 * 1024 * 1024usize).saturating_sub(bytes.len());
            bytes.extend_from_slice(&data[..data.len().min(remaining)]);
            if bytes.len() == 2 * 1024 * 1024 {
                break;
            }
        }
    }
    serde_json::to_string(&HttpResponse {
        status,
        headers,
        body_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
    .map_err(|error| error.to_string())
}
