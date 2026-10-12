use reqwest::Url;

pub(super) struct ParsedProxy {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

pub(super) fn parse(line: &str) -> Result<ParsedProxy, &'static str> {
    if line.contains("://") {
        parse_url(line)
    } else {
        parse_fields(line)
    }
}

fn parse_fields(line: &str) -> Result<ParsedProxy, &'static str> {
    let (host, remainder) = if line.starts_with('[') {
        let end = line.find(']').ok_or("invalid_host")?;
        (
            &line[..=end],
            line[end + 1..].strip_prefix(':').ok_or("invalid_port")?,
        )
    } else {
        line.split_once(':').ok_or("invalid_port")?
    };
    let fields: Vec<_> = remainder.splitn(3, ':').collect();
    let port: u16 = fields[0].parse().map_err(|_| "invalid_port")?;
    if port == 0 || host.is_empty() {
        return Err("invalid_port");
    }
    let mut parsed = parse_url(&format!("http://{host}:{port}"))?;
    match fields.as_slice() {
        [_] => {}
        [_, username, password] if !username.is_empty() => {
            parsed.username = Some((*username).to_string());
            parsed.password = Some((*password).to_string());
        }
        _ => return Err("invalid_auth"),
    }
    Ok(parsed)
}

fn parse_url(line: &str) -> Result<ParsedProxy, &'static str> {
    let url = Url::parse(line).map_err(|_| "invalid_url")?;
    if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h") {
        return Err("unsupported_scheme");
    }
    if url.query().is_some() || url.fragment().is_some() || !matches!(url.path(), "" | "/") {
        return Err("invalid_url");
    }
    let host = url.host_str().ok_or("invalid_host")?;
    let port = url.port_or_known_default().ok_or("invalid_port")?;
    if port == 0 {
        return Err("invalid_port");
    }
    let username = if url.username().is_empty() {
        if url.password().is_some() {
            return Err("invalid_auth");
        }
        None
    } else {
        Some(
            urlencoding::decode(url.username())
                .map_err(|_| "invalid_auth")?
                .into_owned(),
        )
    };
    let password = username
        .as_ref()
        .map(|_| {
            urlencoding::decode(url.password().unwrap_or(""))
                .map(|p| p.into_owned())
                .map_err(|_| "invalid_auth")
        })
        .transpose()?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(ParsedProxy {
        url: format!("{}://{host}:{port}", url.scheme()),
        username,
        password,
    })
}

pub(super) fn validate_health_url(value: &str) -> anyhow::Result<String> {
    let url = Url::parse(value).map_err(|_| anyhow::anyhow!("Invalid proxy health-check URL."))?;
    anyhow::ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Proxy health-check URL must use HTTPS without authentication, query or fragment."
    );
    Ok(url.to_string())
}
