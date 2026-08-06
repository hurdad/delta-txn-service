use std::collections::HashMap;

/// Load object-store configuration from environment variables.
/// Works for S3, MinIO, local FS.
///
/// Every `AWS_*` env var is forwarded verbatim (AWS_ACCESS_KEY_ID,
/// AWS_SECRET_ACCESS_KEY, AWS_ENDPOINT_URL for MinIO/S3-compatible
/// storage, AWS_REGION, etc.) -- object_store/deltalake's own
/// storage-options parsing picks the ones it recognizes and ignores the
/// rest, so this deliberately doesn't maintain its own allowlist of
/// specific key names that would need updating every time delta-rs adds
/// support for a new one.
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

/// Checks a table_uri against the allowlist. A `None` allowlist permits
/// everything.
///
/// FIXED (previously plain `str::starts_with`, not path-segment-aware -- a
/// prefix configured without a trailing `/` also matched an unrelated
/// sibling path merely sharing that character prefix, e.g.
/// `s3://bucket/tables` matching `s3://bucket/tables-other-tenant/secret`;
/// see `is_table_uri_allowed_prefix_without_trailing_slash_does_not_match_sibling_paths`
/// below, which pins down the *fixed* behavior -- it used to assert the
/// opposite). A match now requires either an exact match, or the prefix
/// followed immediately by `/` in `table_uri` -- so a configured prefix
/// works the same whether or not an operator remembered a trailing slash,
/// and can never accidentally admit a same-character-prefixed sibling.
pub fn is_table_uri_allowed(table_uri: &str, allowlist: &Option<Vec<String>>) -> bool {
    match allowlist {
        None => true,
        Some(prefixes) => prefixes.iter().any(|prefix| {
            let prefix = prefix.strip_suffix('/').unwrap_or(prefix.as_str());
            table_uri == prefix
                || table_uri
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        }),
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

    // Regression test for the fixed character-prefix-matching footgun
    // (see this module's own git history / the project's audit notes for
    // the original finding): a prefix configured *without* a trailing `/`
    // must NOT match an unrelated sibling path that merely shares those
    // characters.
    #[test]
    fn is_table_uri_allowed_prefix_without_trailing_slash_does_not_match_sibling_paths() {
        let allowlist = Some(vec!["s3://bucket/tables".to_string()]);
        assert!(!is_table_uri_allowed(
            "s3://bucket/tables-other-tenant/secret",
            &allowlist
        ));
    }

    #[test]
    fn is_table_uri_allowed_prefix_without_trailing_slash_still_matches_real_children() {
        let allowlist = Some(vec!["s3://bucket/tables".to_string()]);
        assert!(is_table_uri_allowed("s3://bucket/tables/foo", &allowlist));
    }

    #[test]
    fn is_table_uri_allowed_matches_exact_prefix_with_or_without_trailing_slash() {
        let with_slash = Some(vec!["s3://bucket/tables/".to_string()]);
        let without_slash = Some(vec!["s3://bucket/tables".to_string()]);
        // The bare prefix itself (no further path segment) should match
        // regardless of which form an operator configured.
        assert!(is_table_uri_allowed("s3://bucket/tables", &with_slash));
        assert!(is_table_uri_allowed("s3://bucket/tables/", &without_slash));
    }
}
