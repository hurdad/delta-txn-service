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
