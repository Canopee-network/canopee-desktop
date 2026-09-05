use canopee_identity::IdentityId;
use canopee_node::Node;
use canopee_protocol::{NodeCommand, NodeResponse};
use canopee_storage::{AppManifest, Object, ObjectId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const CMD_TIMEOUT: Duration = Duration::from_secs(30);
const SERVER_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

async fn cmd(node: &Node, command: NodeCommand) -> Option<NodeResponse> {
    tokio::time::timeout(CMD_TIMEOUT, node.handle(command))
        .await
        .ok()
}

/// Fetches `id` from local storage, falling back to DHT providers (then any
/// explicit provider) and importing into the local store. Mirrors the CLI's
/// `get_or_fetch`.
async fn fetch_object(node: &Node, id: &ObjectId) -> anyhow::Result<Object> {
    if let Some(NodeResponse::Object { object }) = cmd(node, NodeCommand::Get { id: id.clone() }).await
    {
        return Ok(object);
    }
    let peers = match cmd(node, NodeCommand::FindProviders { id: id.clone() }).await {
        Some(NodeResponse::Providers { peer_ids }) => peer_ids,
        _ => Vec::new(),
    };
    for peer in peers {
        if let Some(NodeResponse::Exported { bundle }) =
            cmd(node, NodeCommand::FetchObject {
                peer_id: peer.clone(),
                id: id.clone(),
            })
            .await
            && cmd(node, NodeCommand::Import { bundle }).await.is_some()
            && let Some(NodeResponse::Object { object }) =
                cmd(node, NodeCommand::Get { id: id.clone() }).await
        {
            return Ok(object);
        }
    }
    anyhow::bail!("object {id} is neither local nor available from any provider")
}

/// Resolves `(owner, name)` to the latest app manifest object id. Local
/// home entries for the tray's own identity resolve instantly; anything else
/// goes through the network's signed `AppPointerRecord` DHT record.
async fn resolve_manifest(
    node: &Node,
    owner: &str,
    name: &str,
    self_identity: &str,
) -> anyhow::Result<ObjectId> {
    if owner == self_identity
        && let Some(NodeResponse::HomeIndex { index: Some(home) }) =
            cmd(node, NodeCommand::LoadHomeIndex).await
        && let Some(entry) = home.entries.iter().find(|e| e.name == name)
    {
        return Ok(entry.object.clone());
    }
    let owner_id = IdentityId::new(owner.to_string());
    match cmd(
        node,
        NodeCommand::ResolveAppPointer {
            owner: owner_id,
            name: name.to_string(),
        },
    )
    .await
    {
        Some(NodeResponse::AppPointer { record: Some(record) })
            if record.owner.to_string() == owner && record.name == name && record.verify() =>
        {
            Ok(record.manifest)
        }
        Some(NodeResponse::AppPointer { record: None }) => {
            anyhow::bail!("no app \"{name}\" published by {owner}")
        }
        _ => anyhow::bail!("could not resolve app \"{name}\" of {owner}"),
    }
}

/// Fetches `owner`'s app `name` (manifest + every referenced object),
/// serves it over HTTP on a local port, and opens it in the default browser.
/// Returns the served URL on success.
pub async fn open_app(
    node: &Node,
    owner: &str,
    name: &str,
    self_identity: &str,
) -> anyhow::Result<String> {
    let manifest_id = resolve_manifest(node, owner, name, self_identity).await?;
    let manifest_object = fetch_object(node, &manifest_id).await?;
    let manifest: AppManifest = manifest_object.decode()?;

    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    let entrypoint = fetch_object(node, &manifest.entrypoint).await?;
    files.insert("/".to_string(), entrypoint.payload.data);
    for (path, object_id) in &manifest.assets {
        let object = fetch_object(node, object_id).await?;
        files.insert(path.clone(), object.payload.data);
    }

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}/");
    tokio::spawn(serve_loop(listener, Arc::new(files)));
    Ok(url)
}

async fn serve_loop(
    listener: tokio::net::TcpListener,
    files: Arc<HashMap<String, Vec<u8>>>,
) {
    loop {
        let accepted =
            tokio::time::timeout(SERVER_IDLE_TIMEOUT, listener.accept()).await;
        let Ok(Ok((stream, _))) = accepted else {
            break;
        };
        let files = files.clone();
        tokio::spawn(async move {
            let _ = serve_connection(stream, &files).await;
        });
    }
}

/// Serves one connection: read one request line, answer with the file body
/// (SPA fallback to "/" for route-shaped paths) or 404, then close.
async fn serve_connection(
    stream: tokio::net::TcpStream,
    files: &HashMap<String, Vec<u8>>,
) -> anyhow::Result<()> {
    let mut reader = tokio::io::BufReader::new(stream);
    let mut request_line = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut request_line).await?;
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string();

    let (served, body) = if let Some(body) = files.get(&path) {
        (path.as_str(), Some(body.as_slice()))
    } else if !path.rsplit('/').next().unwrap_or("").contains('.') {
        match files.get("/") {
            Some(entrypoint) => ("/", Some(entrypoint.as_slice())),
            None => (path.as_str(), None),
        }
    } else {
        (path.as_str(), None)
    };

    let status = if body.is_some() { "200 OK" } else { "404 Not Found" };
    let content_type = if body.is_some() {
        guess_content_type(served)
    } else {
        "text/plain"
    };
    let body = body.unwrap_or_default();

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut writer = tokio::io::BufWriter::new(reader.into_inner());
    tokio::io::AsyncWriteExt::write_all(&mut writer, response.as_bytes()).await?;
    tokio::io::AsyncWriteExt::write_all(&mut writer, body).await?;
    tokio::io::AsyncWriteExt::flush(&mut writer).await?;
    Ok(())
}

fn guess_content_type(path: &str) -> &'static str {
    let name = if path == "/" { "index.html" } else { path };
    match name.rsplit('.').next() {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css",
        Some("js") | Some("mjs") => "application/javascript",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_cover_common_bundler_output() {
        for (path, expected) in [
            ("/", "text/html; charset=utf-8"),
            ("/index.html", "text/html; charset=utf-8"),
            ("/style.css", "text/css"),
            ("/app.js", "application/javascript"),
            ("/app.mjs", "application/javascript"),
            ("/app.json", "application/json"),
            ("/img.png", "image/png"),
            ("/img.webp", "image/webp"),
            ("/favicon.ico", "image/x-icon"),
            ("/app.wasm", "application/wasm"),
            ("/unknown.zzz", "application/octet-stream"),
        ] {
            assert_eq!(guess_content_type(path), expected, "mismatch for {path}");
        }
    }
}