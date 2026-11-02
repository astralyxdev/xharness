//! What's typed in a Browser window's address bar, as a URL to load.

/// Where searches go.
const SEARCH: &str = "https://duckduckgo.com/?q=";

/// The URL for `typed`: a URL as it is; `3000` or `:3000` for a dev server
/// on this machine; `localhost:5173`, an IP, or a host with a port over
/// http; `example.com` over https; a path (`/…`, `~/…`) as a file; anything
/// else, like words with spaces, as a search. `None` for nothing typed.
pub fn url_for(typed: &str) -> Option<String> {
    let text = typed.trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if ["http://", "https://", "file://"].iter().any(|scheme| lower.starts_with(scheme))
        || ["about:", "data:"].iter().any(|scheme| lower.starts_with(scheme))
    {
        return Some(text.to_string());
    }
    if let Some(port) = text.strip_prefix(':').or(Some(text)).filter(|port| is_port(port)) {
        return Some(format!("http://localhost:{port}"));
    }
    if let Some(path) = file_path(text) {
        return Some(format!("file://{}", encode_path(&path)));
    }
    if text.contains(char::is_whitespace) {
        return Some(search(text));
    }
    let host = text.split(['/', '?', '#']).next().unwrap_or(text);
    let (name, port) = split_port(host);
    let local = name.eq_ignore_ascii_case("localhost")
        || name.ends_with(".localhost")
        || name.parse::<std::net::Ipv4Addr>().is_ok()
        || (name.starts_with('[') && name.ends_with(']'));
    if local || (port.is_some() && !name.is_empty()) {
        return Some(format!("http://{text}"));
    }
    if name.contains('.') && !name.starts_with('.') && !name.ends_with('.') {
        return Some(format!("https://{text}"));
    }
    Some(search(text))
}

fn is_port(text: &str) -> bool {
    !text.is_empty() && text.len() <= 5 && text.bytes().all(|b| b.is_ascii_digit()) && text.parse::<u16>().is_ok_and(|port| port > 0)
}

/// `host:port` as the host and the port, if it has one (`[::1]:3000` too).
fn split_port(host: &str) -> (&str, Option<&str>) {
    let colon = if host.starts_with('[') {
        host.find("]:").map(|ix| ix + 1)
    } else {
        host.rfind(':')
    };
    match colon {
        Some(ix) if is_port(&host[ix + 1..]) => (&host[..ix], Some(&host[ix + 1..])),
        _ => (host, None),
    }
}

fn file_path(text: &str) -> Option<String> {
    if text.starts_with('/') {
        return Some(text.to_string());
    }
    let rest = text.strip_prefix("~/")?;
    let home = std::env::home_dir()?;
    Some(home.join(rest).display().to_string())
}

fn search(query: &str) -> String {
    let mut url = String::from(SEARCH);
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => url.push(byte as char),
            b' ' => url.push('+'),
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

fn encode_path(path: &str) -> String {
    let mut encoded = String::new();
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// A URL as the address bar shows it while you're not editing it: the
/// whole thing, but without a lone trailing slash after the host
/// (`https://example.com/` reads as `https://example.com`).
pub fn display(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    match rest.strip_suffix('/') {
        Some(host) if !host.contains('/') && !host.is_empty() => format!("{scheme}://{host}"),
        _ => url.to_string(),
    }
}

/// How safe a page's connection is, as the address bar shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// https: encrypted.
    Secure,
    /// Plain http to another machine: anyone on the way can read it.
    NotSecure,
    /// Plain http to this machine or the local network (a dev server).
    Local,
    /// A file on this machine.
    File,
    /// Nothing loaded, or an address that isn't a connection (`about:`).
    Other,
}

/// How `url`'s connection is secured.
pub fn security(url: &str) -> Security {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Security::Other;
    };
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Security::Secure,
        "file" => Security::File,
        "http" => {
            let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            // Credentials before the host (`user:pass@host`).
            let host = host.rsplit('@').next().unwrap_or(host);
            let (name, _) = split_port(host);
            if is_local_host(name) { Security::Local } else { Security::NotSecure }
        }
        _ => Security::Other,
    }
}

/// This machine, a private network address, or a name that only resolves
/// locally (`*.localhost`, `*.local`, `*.test`, a bare host name).
fn is_local_host(name: &str) -> bool {
    let name = name.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if name == "localhost" || name.ends_with(".localhost") || name.ends_with(".local") || name.ends_with(".test") {
        return true;
    }
    if let Ok(ip) = name.parse::<std::net::Ipv4Addr>() {
        return ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified();
    }
    if let Ok(ip) = name.parse::<std::net::Ipv6Addr>() {
        return ip.is_loopback() || ip.is_unspecified() || (ip.segments()[0] & 0xfe00) == 0xfc00;
    }
    // A name without dots only resolves on the local network.
    !name.is_empty() && !name.contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_is_only_flagged_away_from_this_machine() {
        assert_eq!(security("https://example.com/a"), Security::Secure);
        assert_eq!(security("http://example.com"), Security::NotSecure);
        assert_eq!(security("http://user:pw@example.com:8080/x"), Security::NotSecure);
        assert_eq!(security("http://localhost:3000"), Security::Local);
        assert_eq!(security("http://api.localhost/x"), Security::Local);
        assert_eq!(security("http://127.0.0.1:4000"), Security::Local);
        assert_eq!(security("http://192.168.1.20:8080"), Security::Local);
        assert_eq!(security("http://10.0.0.5"), Security::Local);
        assert_eq!(security("http://[::1]:3000"), Security::Local);
        assert_eq!(security("http://myapp.test:8000"), Security::Local);
        assert_eq!(security("http://devbox:8000"), Security::Local);
        assert_eq!(security("http://8.8.8.8"), Security::NotSecure);
        assert_eq!(security("file:///tmp/a.html"), Security::File);
        assert_eq!(security("about:blank"), Security::Other);
        assert_eq!(security(""), Security::Other);
    }

    #[test]
    fn dev_servers_are_quick_to_type() {
        assert_eq!(url_for("3000").as_deref(), Some("http://localhost:3000"));
        assert_eq!(url_for(":5173").as_deref(), Some("http://localhost:5173"));
        assert_eq!(url_for("localhost:8080/admin").as_deref(), Some("http://localhost:8080/admin"));
        assert_eq!(url_for("127.0.0.1:4000").as_deref(), Some("http://127.0.0.1:4000"));
        assert_eq!(url_for("[::1]:3000").as_deref(), Some("http://[::1]:3000"));
        assert_eq!(url_for("myapp.test:8000").as_deref(), Some("http://myapp.test:8000"));
        assert_eq!(url_for("api.localhost").as_deref(), Some("http://api.localhost"));
    }

    #[test]
    fn hosts_get_https_and_urls_stay_as_they_are() {
        assert_eq!(url_for("example.com").as_deref(), Some("https://example.com"));
        assert_eq!(url_for("  docs.rs/gpui?x=1 ").as_deref(), Some("https://docs.rs/gpui?x=1"));
        assert_eq!(url_for("http://example.com").as_deref(), Some("http://example.com"));
        assert_eq!(url_for("HTTPS://Example.com").as_deref(), Some("HTTPS://Example.com"));
        assert_eq!(url_for("about:blank").as_deref(), Some("about:blank"));
    }

    #[test]
    fn paths_open_files_and_the_rest_is_a_search() {
        assert_eq!(url_for("/tmp/my page.html").as_deref(), Some("file:///tmp/my%20page.html"));
        assert_eq!(url_for("rust gpui").as_deref(), Some("https://duckduckgo.com/?q=rust+gpui"));
        assert_eq!(url_for("gpui").as_deref(), Some("https://duckduckgo.com/?q=gpui"));
        assert_eq!(url_for("c++ & go").as_deref(), Some("https://duckduckgo.com/?q=c%2B%2B+%26+go"));
        assert_eq!(url_for("99999").as_deref(), Some("https://duckduckgo.com/?q=99999"));
        assert_eq!(url_for("   "), None);
    }

    #[test]
    fn display_drops_only_a_lone_trailing_slash() {
        assert_eq!(display("https://example.com/"), "https://example.com");
        assert_eq!(display("https://example.com/docs/"), "https://example.com/docs/");
        assert_eq!(display("about:blank"), "about:blank");
    }
}
