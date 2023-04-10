/// constants for grant_type
pub const GRANT_TYPE_PASSWORD: &str = "password";
pub const GRANT_TYPE_AUTH_CODE: &str = "authorization_code";
pub const GRANT_TYPE_REFRESH_TOKEN: &str = "refresh_token";
pub const TOKEN_TYPE_HINT_ACCESS: &str = "access_token";

/// constants for API urls
/// AUTH
pub const DRACOON_TOKEN_URL: &str = "oauth/token";
pub const DRACOON_REDIRECT_URL: &str = "oauth/callback";
pub const DRACOON_TOKEN_REVOKE_URL: &str = "oauth/revoke";

/// API
pub const DRACOON_API_PREFIX: &str = "api/v4";

/// NODES
pub const NODES_BASE: &str = "nodes";
pub const FILES_BASE: &str = "files";
pub const FILES_FILE_KEY: &str = "user_file_key";
pub const FILES_UPLOAD: &str = "uploads";
pub const FILES_S3_URLS: &str = "s3_urls";
pub const FILES_S3_COMPLETE: &str = "s3";
pub const NODES_DOWNLOAD_URL: &str = "downloads";
pub const NODES_SEARCH: &str = "search";

pub const CHUNK_SIZE: usize = 1024 * 1024 * 32; // 32 MB

/// USER
pub const USER_BASE: &str = "user";
pub const USER_ACCOUNT: &str = "account";
pub const USER_ACCOUNT_KEYPAIR: &str = "keypair";


/// user agent header
pub const APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

pub fn get_client_credentials() -> (String, String) {
    let client_id = include_str!("../../.env")
        .split('\n')
        .next()
        .expect("env file has more than one line")
        .split("CLIENT_ID=")
        .nth(1)
        .expect("CLIENT_ID MUST be provided");
    let client_secret = include_str!("../../.env")
        .split('\n')
        .nth(1)
        .expect("env file has more than one line")
        .split("CLIENT_SECRET=")
        .nth(1)
        .expect("CLIENT_SECRET MUST be provided");

    (client_id.into(), client_secret.into())
}
