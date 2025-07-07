mod common;
mod config;
mod proxy;

use crate::config::Config;
use crate::proxy::*;

use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;
use worker::*;

#[event(fetch)]
async fn main(req: Request, env: Env, _: Context) -> Result<Response> {
    let uuid = match env.var("UUID") {
        Ok(val) => val
            .to_string()
            .parse::<Uuid>()
            .map_err(|e| Error::RustError(format!("Invalid UUID: {}", e)))?,
        Err(_) => return Err(Error::RustError("UUID is required".into())),
    };

    // get proxy ip list
    let proxy_ip: Vec<String> = env
        .var("PROXY_IP")
        .ok()
        .map(|x| {
            x.to_string()
                .split(|c: char| c.is_ascii_whitespace() || c == ',')
                .filter_map(|s| {
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let redirect_url = env
        .var("REDIRECT_URL")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "https://example.com".to_string());

    let truthy_values = ["true", "1", "yes", "on"];
    let enabled = env
        .var("ENABLED_LINK")
        .map(|v| {
            let val = v.to_string().to_lowercase();
            truthy_values.contains(&val.as_str())
        })
        .unwrap_or(false);

    let host = req.url()?.host_str().unwrap_or("").to_owned();
    let config = Config {
        uuid,
        host,
        proxy_ip,
        redirect_url,
        display_link: enabled,
    };

    Router::with_data(config)
        .on_async("/", tunnel)
        .on("/link", link)
        .run(req, env)
        .await
}

async fn tunnel(req: Request, cx: RouteContext<Config>) -> Result<Response> {
    if let Some(upgrade) = req.headers().get("Upgrade")? {
        if upgrade.to_ascii_lowercase() == "websocket" {
            let WebSocketPair { server, client } = WebSocketPair::new()?;
            server.accept()?;

            wasm_bindgen_futures::spawn_local(async move {
                let events = server.events().unwrap();
                if let Err(e) = VmessStream::new(cx.data, &server, events).process().await {
                    console_log!("[tunnel]: {}", e);
                }
            });

            return Response::from_websocket(client);
        }
    }

    let redirect_url = cx.data.redirect_url;
    let url = Url::parse(&redirect_url)?;
    Response::redirect(url)
}

fn link(_: Request, cx: RouteContext<Config>) -> Result<Response> {
    let redirect_url = cx.data.redirect_url;
    let display_link = cx.data.display_link;
    if !display_link {
        let url = Url::parse(&redirect_url)?;
        return Response::redirect(url);
    }

    #[derive(Serialize)]
    struct Link {
        description: String,
        link: String,
    }

    let link = {
        let host = cx.data.host.to_string();
        let uuid = cx.data.uuid.to_string();
        let config = json!({
            "ps": "tunl",
            "v": "2",
            "add": "162.159.16.149",
            "port": "80",
            "id": uuid,
            "aid": "0",
            "scy": "zero",
            "net": "ws",
            "type": "none",
            "host": host,
            "path": "",
            "tls": "",
            "sni": "",
            "alpn": ""}
        );
        format!("vmess://{}", URL_SAFE.encode(config.to_string()))
    };

    Response::from_json(&Link {
        link,
        description: "replace the IP address in the configuration with a clean one".to_string(),
    })
}
