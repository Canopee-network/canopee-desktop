use canopee_config::Config;
use std::collections::HashMap;

/// Parses `canopee://<owner>/<name>` into `(owner, name)`. `<owner>` is
/// either the canonical `canopee://identity/<peer-id>` form or a friendly
/// alias resolvable through the local aliases file. Identical rules to the
/// `canopee-cli` `uri` module.
pub fn parse(uri: &str) -> anyhow::Result<(String, String)> {
    let rest = uri
        .strip_prefix("canopee://")
        .ok_or_else(|| anyhow::anyhow!("not a canopee:// URI: {uri}"))?;
    let mut segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let name = segments
        .pop()
        .ok_or_else(|| anyhow::anyhow!("missing app name in {uri}"))?
        .to_string();

    let owner = if segments.first() == Some(&"identity") && segments.len() >= 2 {
        format!("canopee://{}", segments.join("/"))
    } else if segments.len() == 1 {
        segments[0].to_string()
    } else {
        anyhow::bail!("malformed canopee:// URI: {uri}");
    };

    Ok((owner, name))
}

fn load_aliases() -> anyhow::Result<HashMap<String, String>> {
    let path = Config::new().aliases_path();
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return Ok(HashMap::new()),
    };
    Ok(serde_json::from_str(&raw)?)
}

/// Resolves the owner component of a URI to the canonical
/// `canopee://identity/<peer-id>` form. Canonical owners pass through;
/// anything else is looked up in the aliases file.
pub fn resolve_owner(owner: &str) -> anyhow::Result<String> {
    if owner.starts_with("canopee://identity/") {
        return Ok(owner.to_string());
    }
    let aliases = load_aliases()?;
    aliases.get(owner).cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "no alias \"{owner}\" (set one with `canopee alias set {owner} <identity>`, \
             or use the canonical canopee://identity/<peer-id> form)"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_name() {
        let (owner, name) = parse("canopee://alice/portfolio").unwrap();
        assert_eq!(owner, "alice");
        assert_eq!(name, "portfolio");
    }

    #[test]
    fn parses_canonical_owner() {
        let (owner, name) = parse("canopee://identity/12D3KooWx/portfolio").unwrap();
        assert_eq!(owner, "canopee://identity/12D3KooWx");
        assert_eq!(name, "portfolio");
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("canopee://").is_err());
        assert!(parse("http://alice/portfolio").is_err());
        assert!(parse("canopee://alice").is_err());
        assert!(parse("canopee://a/b/c").is_err());
    }

    #[test]
    fn canonical_owner_passes_through() {
        let resolved = resolve_owner("canopee://identity/12D3KooWx").unwrap();
        assert_eq!(resolved, "canopee://identity/12D3KooWx");
    }
}