use std::collections::HashMap;

/// Load object-store configuration from environment variables.
/// Works for S3, MinIO, local FS.
pub fn load_storage_options() -> HashMap<String, String> {
    let mut opts = HashMap::new();

    for (k, v) in std::env::vars() {
        if k.starts_with("AWS_") {
            opts.insert(k, v);
        }
    }

    opts
}

/// Load the optional table_uri allowlist from `DELTA_TXN_ALLOWED_TABLE_PREFIXES`
/// (comma-separated URI prefixes). Returns `None` when unset, meaning any
/// table_uri a client supplies is permitted (the historical, unrestricted behavior).
pub fn load_allowed_table_prefixes() -> Option<Vec<String>> {
    let raw = std::env::var("DELTA_TXN_ALLOWED_TABLE_PREFIXES").ok()?;
    let prefixes: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if prefixes.is_empty() {
        None
    } else {
        Some(prefixes)
    }
}

/// Checks a table_uri against the allowlist. A `None` allowlist permits everything.
pub fn is_table_uri_allowed(table_uri: &str, allowlist: &Option<Vec<String>>) -> bool {
    match allowlist {
        None => true,
        Some(prefixes) => prefixes
            .iter()
            .any(|prefix| table_uri.starts_with(prefix.as_str())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_table_uri_allowed_with_no_allowlist_allows_everything() {
        assert!(is_table_uri_allowed("s3://anything/here", &None));
    }

    #[test]
    fn is_table_uri_allowed_checks_prefixes() {
        let allowlist = Some(vec!["s3://bucket/tables/".to_string()]);
        assert!(is_table_uri_allowed("s3://bucket/tables/foo", &allowlist));
        assert!(!is_table_uri_allowed(
            "s3://other-bucket/tables/foo",
            &allowlist
        ));
    }
}
