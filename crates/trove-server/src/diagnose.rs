//! Outbound-proxy diagnostics for "Test connection".
//!
//! When an HTTPS request through an HTTP proxy fails at the CONNECT step,
//! the HTTP stack only reports "tunnel error: unsuccessful" and drops the
//! proxy's answer. To give admins something actionable, Trove repeats the
//! CONNECT itself and reports what the proxy said (status line, identifying
//! headers, the start of its error page).

use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use trove_core::config::Config;
use trove_core::model::{OutboundMode, OutboundProxy};

/// The HTTP proxy (URL, username, password) a request to `target` would use.
pub fn effective_http_proxy(cfg: &Config, o: &OutboundProxy, target: &url::Url) -> Option<(String, String, String)> {
    let (url, user, pass) = match o.mode {
        OutboundMode::Direct => return None,
        OutboundMode::Custom => (o.url.trim().to_string(), o.username.clone(), o.password.clone()),
        OutboundMode::Inherit if !cfg.http_client.proxy.is_empty() => {
            (cfg.http_client.proxy.trim().to_string(), String::new(), String::new())
        }
        OutboundMode::Inherit => {
            let vars: &[&str] = if target.scheme() == "https" {
                &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
            } else {
                &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
            };
            let url = vars.iter().find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()))?;
            (url.trim().to_string(), String::new(), String::new())
        }
    };
    // Bare host:port means http://host:port, as reqwest assumes.
    let url = if url.contains("://") { url } else { format!("http://{url}") };
    Some((url, user, pass))
}

/// Send `CONNECT target` to an `http://` proxy and describe its answer.
pub async fn probe_connect(proxy: &str, username: &str, password: &str, target: &url::Url, user_agent: &str) -> String {
    let Ok(p) = url::Url::parse(proxy) else { return format!("invalid proxy URL {proxy}") };
    if p.scheme() != "http" {
        return format!("the proxy {} uses {}; only http:// proxies can be probed", redact(&p), p.scheme());
    }
    let (Some(host), Some(port)) = (p.host_str(), p.port_or_known_default()) else {
        return format!("the proxy URL {} has no host", redact(&p));
    };
    let (Some(t_host), Some(t_port)) = (target.host_str(), target.port_or_known_default()) else {
        return "the remote URL has no host".to_string();
    };
    let shown = redact(&p);
    let mut stream =
        match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((host.trim_matches(['[', ']']), port)))
            .await
        {
            Err(_) => return format!("connecting to the proxy {shown} timed out"),
            Ok(Err(e)) => return format!("cannot connect to the proxy {shown}: {e}"),
            Ok(Ok(s)) => s,
        };
    let mut req = format!(
        "CONNECT {t_host}:{t_port} HTTP/1.1\r\nHost: {t_host}:{t_port}\r\nUser-Agent: {user_agent}\r\nProxy-Connection: keep-alive\r\n"
    );
    let (user, pass) = if username.is_empty() {
        (pct_decode(p.username()), p.password().map(pct_decode).unwrap_or_default())
    } else {
        (username.to_string(), password.to_string())
    };
    if !user.is_empty() {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("\r\n");
    if let Err(e) = stream.write_all(req.as_bytes()).await {
        return format!("the proxy {shown} closed the connection: {e}");
    }
    let mut buf = Vec::with_capacity(4096);
    let head_end = loop {
        let mut chunk = [0u8; 4096];
        match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk)).await {
            Err(_) if buf.is_empty() => return format!("the proxy {shown} did not answer CONNECT within 10 s"),
            Err(_) | Ok(Ok(0)) | Ok(Err(_)) => break None,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
        }
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break Some(i);
        }
        if buf.len() > 32 * 1024 {
            break None;
        }
    };
    if buf.is_empty() {
        return format!("the proxy {shown} closed the connection without answering CONNECT {t_host}:{t_port}");
    }
    if !buf.starts_with(b"HTTP/") {
        return not_http(&buf, &shown);
    }
    let head_len = head_end.unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_len]).to_string();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default().trim().to_string();
    let code: u16 = status_line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let interesting: Vec<String> = lines
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .filter(|(k, _)| {
            ["server", "via", "proxy-agent", "proxy-authenticate", "x-squid-error", "x-error", "x-deny-reason"]
                .contains(&k.to_ascii_lowercase().as_str())
        })
        .map(|(k, v)| format!("{k}: {v}"))
        .collect();
    if code == 200 {
        return format!(
            "the proxy {shown} accepted CONNECT {t_host}:{t_port} ({status_line}) when asked directly; the failure is intermittent or happens after the tunnel (TLS)"
        );
    }
    // Read a little of the error page: proxies often explain the refusal there.
    if let Some(i) = head_end {
        let mut body = buf[i + 4..].to_vec();
        let mut chunk = [0u8; 4096];
        while body.len() < 4096 {
            match tokio::time::timeout(Duration::from_millis(800), stream.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => body.extend_from_slice(&chunk[..n]),
                _ => break,
            }
        }
        let text = page_text(&String::from_utf8_lossy(&body));
        let mut msg = format!("the proxy {shown} answered CONNECT {t_host}:{t_port} with \"{status_line}\"");
        if !interesting.is_empty() {
            msg.push_str(&format!(" [{}]", interesting.join("; ")));
        }
        msg.push_str(&format!(" — {}", explain(code, t_host, t_port)));
        if !text.is_empty() {
            msg.push_str(&format!(" Proxy page: \"{text}\""));
        }
        return msg;
    }
    format!(
        "the proxy {shown} answered CONNECT {t_host}:{t_port} with \"{status_line}\" — {}",
        explain(code, t_host, t_port)
    )
}

fn explain(code: u16, host: &str, port: u16) -> String {
    match code {
        407 => "the proxy wants credentials: set the proxy username/password (Basic auth; NTLM/Kerberos-only proxies are not supported)".into(),
        403 => format!("the proxy policy forbids tunnelling to {host}:{port}; ask for {host} (port {port}) to be allowed, or use an http:// remote if the mirror offers one"),
        400 | 405 | 501 => "this proxy port does not accept CONNECT (HTTPS tunnelling); check the port, or use an http:// remote".into(),
        502..=504 => format!("the proxy itself could not reach {host}:{port} (its DNS, firewall or upstream proxy)"),
        _ => "the proxy refused the HTTPS tunnel".into(),
    }
}

fn not_http(buf: &[u8], shown: &str) -> String {
    let hex: Vec<String> = buf.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let hint = match buf.first() {
        Some(0x05) | Some(0x04) => "this looks like a SOCKS proxy: use socks5h://",
        Some(0x15) | Some(0x16) => "this port speaks TLS: use an https:// proxy URL",
        _ => "check the proxy host and port",
    };
    format!("the proxy {shown} did not answer with HTTP (first bytes {}) — {hint}", hex.join(" "))
}

/// Visible text of an HTML error page, shortened.
fn page_text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut skip = false; // inside <style>/<script>
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if !in_tag && bytes[i] == b'<' {
            in_tag = true;
            let rest = &lower[i..];
            if rest.starts_with("<style") || rest.starts_with("<script") {
                skip = true;
            } else if rest.starts_with("</style") || rest.starts_with("</script") {
                skip = false;
            }
        } else if in_tag && bytes[i] == b'>' {
            in_tag = false;
            out.push(' ');
        } else if !in_tag && !skip {
            let ch = html[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        i += 1;
    }
    let text = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut short: String = text.chars().take(300).collect();
    if text.chars().count() > 300 {
        short.push('…');
    }
    short
}

fn redact(u: &url::Url) -> String {
    format!("{}://{}{}", u.scheme(), u.host_str().unwrap_or(""), u.port().map(|p| format!(":{p}")).unwrap_or_default())
}

fn pct_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fake_proxy(reply: &'static [u8]) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = s.read(&mut buf).await.unwrap();
            assert!(buf[..n].starts_with(b"CONNECT repo.example.org:443 HTTP/1.1\r\n"));
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(req.contains("Proxy-Authorization: Basic dTpwQHNz\r\n"), "{req}");
            s.write_all(reply).await.unwrap();
        });
        format!("http://u:p%40ss@{addr}")
    }

    #[tokio::test]
    async fn reports_the_proxy_answer() {
        let target = url::Url::parse("https://repo.example.org/repodata/repomd.xml").unwrap();
        let proxy = fake_proxy(
            b"HTTP/1.1 403 Forbidden\r\nServer: squid/5.7\r\nX-Squid-Error: ERR_ACCESS_DENIED 0\r\nContent-Length: 60\r\n\r\n<html><style>p{}</style><h1>Access Denied</h1><p>by policy</p></html>",
        )
        .await;
        let msg = probe_connect(&proxy, "", "", &target, "Trove").await;
        assert!(msg.contains("\"HTTP/1.1 403 Forbidden\""), "{msg}");
        assert!(msg.contains("X-Squid-Error: ERR_ACCESS_DENIED 0"), "{msg}");
        assert!(msg.contains("forbids tunnelling to repo.example.org:443"), "{msg}");
        assert!(msg.contains("Access Denied by policy"), "{msg}");
        assert!(!msg.contains("p%40ss") && !msg.contains("u:"), "credentials leaked: {msg}");

        let socks = fake_proxy(b"\x05\xff").await;
        let msg = probe_connect(&socks, "", "", &target, "Trove").await;
        assert!(msg.contains("socks5h://"), "{msg}");
    }
}
