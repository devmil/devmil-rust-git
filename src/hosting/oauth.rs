//! OAuth URLs, forms and response decoding. Callback addresses and login scheduling belong to callers.
use super::model::{HostingConnection, HostingError};
use super::parsers::percent_encode;
use super::providers::{provider_plugin, AuthorizationCodePkce, DeviceAuthorization};
use serde::Deserialize;
use std::fmt::Write as _;
use zeroize::Zeroize;
pub fn oauth_authorize_url(
    connection: &HostingConnection,
    specification: AuthorizationCodePkce,
    callback_url: &str,
    state: &str,
    challenge: &str,
) -> String {
    let mut url = format!(
        "{}{}?client_id={}&redirect_uri={}&response_type=code&state={}",
        connection.web_base_url.trim_end_matches('/'),
        specification.authorization_path,
        percent_encode(&connection.oauth_client_id),
        percent_encode(callback_url),
        percent_encode(state),
    );
    if !specification.scopes.is_empty() {
        url.push_str("&scope=");
        url.push_str(&percent_encode(&specification.scopes.join(" ")));
    }
    let _ = write!(
        url,
        "&code_challenge={}&code_challenge_method=S256",
        percent_encode(challenge),
    );
    url
}

pub fn oauth_token_url(connection: &HostingConnection) -> String {
    format!(
        "{}{}",
        connection.web_base_url.trim_end_matches('/'),
        provider_plugin(connection.provider()).token_path()
    )
}

/// Form for exchanging a public-client PKCE code. Callers must clear the returned secret.
pub fn oauth_authorization_code_form(
    connection: &HostingConnection,
    code: &str,
    callback_url: &str,
    verifier: &str,
) -> String {
    format!(
        "client_id={}&code={}&grant_type=authorization_code&redirect_uri={}&code_verifier={}",
        percent_encode(&connection.oauth_client_id),
        percent_encode(code),
        percent_encode(callback_url),
        percent_encode(verifier)
    )
}

/// Form for refreshing a public-client token. Callers must clear the returned secret.
pub fn oauth_refresh_token_form(connection: &HostingConnection, refresh_token: &str) -> String {
    format!(
        "client_id={}&grant_type=refresh_token&refresh_token={}",
        percent_encode(&connection.oauth_client_id),
        percent_encode(refresh_token),
    )
}

pub fn device_authorization_url(
    connection: &HostingConnection,
    specification: DeviceAuthorization,
) -> String {
    format!(
        "{}{}",
        connection.web_base_url.trim_end_matches('/'),
        specification.device_code_path,
    )
}

pub fn device_authorization_request_form(
    connection: &HostingConnection,
    specification: DeviceAuthorization,
) -> String {
    format!(
        "client_id={}&scope={}",
        percent_encode(&connection.oauth_client_id),
        percent_encode(&specification.scopes.join(" "))
    )
}

pub fn device_authorization_poll_form(connection: &HostingConnection, device_code: &str) -> String {
    format!(
        "client_id={}&device_code={}&grant_type={}",
        percent_encode(&connection.oauth_client_id),
        percent_encode(device_code),
        percent_encode("urn:ietf:params:oauth:grant-type:device_code")
    )
}

/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn validate_device_verification_uri(
    connection: &HostingConnection,
    specification: DeviceAuthorization,
    verification_uri: &str,
) -> Result<(), HostingError> {
    let expected = format!(
        "{}{}",
        connection.web_base_url.trim_end_matches('/'),
        specification.verification_path,
    );
    (verification_uri == expected)
        .then_some(())
        .ok_or(HostingError::MalformedResponse)
}

pub fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| percent_decode(value)).flatten()
    })
}

fn percent_decode(value: &str) -> Option<String> {
    let mut output = Vec::with_capacity(value.len());
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            output.push(u8::try_from((high << 4) | low).ok()?);
        } else {
            output.push(if byte == b'+' { b' ' } else { byte });
        }
    }
    String::from_utf8(output).ok()
}

pub fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(char::from(ALPHABET[((value >> 18) & 63) as usize]));
        output.push(char::from(ALPHABET[((value >> 12) & 63) as usize]));
        if chunk.len() > 1 {
            output.push(char::from(ALPHABET[((value >> 6) & 63) as usize]));
        }
        if chunk.len() > 2 {
            output.push(char::from(ALPHABET[(value & 63) as usize]));
        }
    }
    output
}

#[derive(Deserialize)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

#[derive(Deserialize)]
pub struct DeviceAuthorizationResponse {
    #[serde(default)]
    pub device_code: String,
    #[serde(default)]
    pub user_code: String,
    #[serde(default)]
    pub verification_uri: String,
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default)]
    pub interval: u64,
    #[serde(default)]
    pub error: String,
}

impl Drop for DeviceAuthorizationResponse {
    fn drop(&mut self) {
        self.device_code.zeroize();
    }
}

pub enum DeviceAuthorizationPoll {
    Authorized(OAuthTokenResponse),
    Pending,
    SlowDown(Option<u64>),
    Denied,
    Expired,
    Disabled,
}

#[derive(Deserialize)]
struct DeviceAuthorizationPollPayload {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    error: String,
    #[serde(default)]
    interval: Option<u64>,
}

impl Drop for DeviceAuthorizationPollPayload {
    fn drop(&mut self) {
        self.access_token.zeroize();
        if let Some(refresh_token) = &mut self.refresh_token {
            refresh_token.zeroize();
        }
    }
}

#[derive(Deserialize)]
pub struct OAuthIdentity {
    #[serde(default)]
    pub login: String,
    #[serde(default)]
    pub username: String,
}

impl OAuthTokenResponse {
    pub fn zeroize(&mut self) {
        self.access_token.zeroize();
        if let Some(refresh_token) = &mut self.refresh_token {
            refresh_token.zeroize();
        }
    }
}

impl Drop for OAuthTokenResponse {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_device_authorization(
    body: &[u8],
) -> Result<DeviceAuthorizationResponse, HostingError> {
    let response = if let Ok(response) = serde_json::from_slice(body) {
        response
    } else {
        let text = std::str::from_utf8(body).map_err(|_| HostingError::MalformedResponse)?;
        DeviceAuthorizationResponse {
            device_code: query_value(text, "device_code").unwrap_or_default(),
            user_code: query_value(text, "user_code").unwrap_or_default(),
            verification_uri: query_value(text, "verification_uri").unwrap_or_default(),
            expires_in: query_value(text, "expires_in")
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
            interval: query_value(text, "interval")
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
            error: query_value(text, "error").unwrap_or_default(),
        }
    };
    match response.error.as_str() {
        "" => {}
        "device_flow_disabled" => {
            return Err(HostingError::Unavailable(
                "Device Flow is disabled for this GitHub OAuth application. Enable Device Flow in its OAuth App settings, then try again."
                    .into(),
            ));
        }
        "incorrect_client_credentials" => return Err(HostingError::Unauthorized),
        _ => return Err(HostingError::MalformedResponse),
    }
    if response.device_code.is_empty()
        || response.device_code.len() > 512
        || response.user_code.is_empty()
        || response.user_code.len() > 32
        || response.user_code.chars().any(char::is_control)
        || response.verification_uri.is_empty()
        || response.verification_uri.len() > 8_192
        || response.expires_in == 0
        || response.interval == 0
    {
        Err(HostingError::MalformedResponse)
    } else {
        Ok(response)
    }
}

/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_device_authorization_poll(
    body: &[u8],
) -> Result<DeviceAuthorizationPoll, HostingError> {
    let payload =
        if let Ok(payload) = serde_json::from_slice::<DeviceAuthorizationPollPayload>(body) {
            payload
        } else {
            let text = std::str::from_utf8(body).map_err(|_| HostingError::MalformedResponse)?;
            DeviceAuthorizationPollPayload {
                access_token: query_value(text, "access_token").unwrap_or_default(),
                refresh_token: query_value(text, "refresh_token"),
                expires_in: query_value(text, "expires_in").and_then(|value| value.parse().ok()),
                error: query_value(text, "error").unwrap_or_default(),
                interval: query_value(text, "interval").and_then(|value| value.parse().ok()),
            }
        };
    if !payload.access_token.is_empty() {
        return Ok(DeviceAuthorizationPoll::Authorized(OAuthTokenResponse {
            access_token: payload.access_token.clone(),
            refresh_token: payload.refresh_token.clone(),
            expires_in: payload.expires_in,
        }));
    }
    match payload.error.as_str() {
        "authorization_pending" => Ok(DeviceAuthorizationPoll::Pending),
        "slow_down" => Ok(DeviceAuthorizationPoll::SlowDown(payload.interval)),
        "access_denied" => Ok(DeviceAuthorizationPoll::Denied),
        "expired_token" | "token_expired" => Ok(DeviceAuthorizationPoll::Expired),
        "device_flow_disabled" => Ok(DeviceAuthorizationPoll::Disabled),
        "incorrect_client_credentials" | "incorrect_device_code" => Err(HostingError::Unauthorized),
        _ => Err(HostingError::MalformedResponse),
    }
}

/// Decodes an OAuth token endpoint response without accepting tokens on an error status.
/// Error descriptions remain private; only standardized error codes are classified.
/// # Errors
/// Rejects unsuccessful HTTP statuses, rejected grants/clients and malformed tokens.
pub fn parse_oauth_token_response(
    status: u16,
    body: &[u8],
) -> Result<OAuthTokenResponse, HostingError> {
    match status {
        200..=299 => parse_oauth_token(body),
        400 => Err(parse_oauth_token(body)
            .err()
            .unwrap_or(HostingError::MalformedResponse)),
        status => {
            super::http::require_success_status(status).and(Err(HostingError::MalformedResponse))
        }
    }
}

/// # Errors
/// Rejects malformed, unsafe, oversized, or unauthorized provider output.
pub fn parse_oauth_token(body: &[u8]) -> Result<OAuthTokenResponse, HostingError> {
    let text = std::str::from_utf8(body).map_err(|_| HostingError::MalformedResponse)?;
    let mut token = if text.trim_start().starts_with('{') {
        #[derive(Deserialize)]
        struct TokenError {
            error: Option<String>,
        }
        let error: TokenError =
            serde_json::from_slice(body).map_err(|_| HostingError::MalformedResponse)?;
        if let Some(error) = error.error {
            return Err(oauth_token_error(&error));
        }
        serde_json::from_slice(body).map_err(|_| HostingError::MalformedResponse)?
    } else {
        if let Some(error) = query_value(text, "error") {
            return Err(oauth_token_error(&error));
        }
        OAuthTokenResponse {
            access_token: query_value(text, "access_token")
                .ok_or(HostingError::MalformedResponse)?,
            refresh_token: query_value(text, "refresh_token"),
            expires_in: query_value(text, "expires_in")
                .map(|value| value.parse().map_err(|_| HostingError::MalformedResponse))
                .transpose()?,
        }
    };
    if token.access_token.is_empty()
        || token.access_token.len() > 16 * 1024
        || token
            .refresh_token
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 16 * 1024)
        || token.expires_in == Some(0)
    {
        token.zeroize();
        return Err(HostingError::MalformedResponse);
    }
    Ok(token)
}

fn oauth_token_error(error: &str) -> HostingError {
    match error {
        "invalid_client" | "unauthorized_client" => HostingError::InvalidAccount,
        "invalid_grant" | "access_denied" | "expired_token" | "token_expired" => {
            HostingError::Unauthorized
        }
        _ => HostingError::MalformedResponse,
    }
}

#[cfg(test)]
mod token_error_tests {
    use super::*;

    #[test]
    fn http_errors_never_authorize_a_token_and_preserve_client_rejection() {
        assert!(matches!(
            parse_oauth_token_response(400, br#"{"error":"unauthorized_client"}"#),
            Err(HostingError::InvalidAccount)
        ));
        assert!(matches!(
            parse_oauth_token_response(400, br#"{"error":"invalid_grant"}"#),
            Err(HostingError::Unauthorized)
        ));
        assert!(matches!(
            parse_oauth_token_response(400, br#"{"access_token":"sentinel"}"#),
            Err(HostingError::MalformedResponse)
        ));
        assert!(parse_oauth_token_response(200, br#"{"access_token":"sentinel"}"#).is_ok());
    }

    #[test]
    fn client_rejections_are_configuration_failures_in_json_and_form_responses() {
        for body in [
            br#"{"error":"unauthorized_client","error_description":"private-server-detail"}"#
                .as_slice(),
            br#"{"error":"invalid_client"}"#.as_slice(),
            b"error=unauthorized_client&error_description=private-server-detail".as_slice(),
        ] {
            assert!(matches!(
                parse_oauth_token(body),
                Err(HostingError::InvalidAccount)
            ));
        }
        assert!(matches!(
            parse_oauth_token(br#"{"error":"invalid_grant"}"#),
            Err(HostingError::Unauthorized)
        ));
        assert!(matches!(
            parse_oauth_token(br#"{"error":"unknown_private_error"}"#),
            Err(HostingError::MalformedResponse)
        ));
    }
}
