//! OAuth credential helpers for subscription-backed model providers.
//!
//! The first consumer is OpenAI Codex/ChatGPT OAuth. It intentionally reads
//! the credential files created by the official Codex CLI and pi-mono so this
//! project can reuse an existing browser login instead of asking for an API key.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::cancel::CancelToken;
use crate::core::error::ModelError;
use crate::core::net::{net_err_to_model, HttpMethod, HttpReq, NetEgress};

const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_CODEX_JWT_CLAIM: &str = "https://api.openai.com/auth";
const EXPIRY_SKEW_MS: i64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenAiCodexToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub account_id: String,
    pub expires_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct OpenAiCodexAuth {
    auth_path: Option<PathBuf>,
    access_token_override: Option<String>,
    static_token: Option<OpenAiCodexToken>,
}

impl OpenAiCodexAuth {
    pub fn new(auth_path: Option<PathBuf>) -> Self {
        Self {
            auth_path,
            access_token_override: None,
            static_token: None,
        }
    }

    pub fn with_access_token(mut self, token: Option<String>) -> Self {
        self.access_token_override = token.filter(|s| !s.trim().is_empty());
        self
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn from_static_token(
        access_token: impl Into<String>,
        account_id: impl Into<String>,
    ) -> Self {
        Self {
            auth_path: None,
            access_token_override: None,
            static_token: Some(OpenAiCodexToken {
                access_token: access_token.into(),
                refresh_token: None,
                account_id: account_id.into(),
                expires_at_ms: None,
            }),
        }
    }

    pub async fn resolve(
        &self,
        net: Arc<dyn NetEgress>,
        cancel: CancelToken,
    ) -> Result<OpenAiCodexToken, ModelError> {
        if let Some(token) = &self.static_token {
            return complete_account_id(token.clone());
        }

        if let Some(token) = self.token_from_override_or_env()? {
            return complete_account_id(token);
        }

        let Some(loaded) = load_openai_codex_auth(self.auth_path.as_deref())? else {
            return Err(ModelError::Auth(
                "OpenAI Codex OAuth credentials not found. Run `codex login`, pi-mono login, or set OPENAI_CODEX_ACCESS_TOKEN plus OPENAI_CODEX_ACCOUNT_ID.".into(),
            ));
        };

        if token_is_fresh(&loaded.token) {
            return complete_account_id(loaded.token);
        }

        let refreshed = refresh_openai_codex_token(&loaded.token, net, cancel).await?;
        if let Err(e) = persist_refreshed_token(&loaded, &refreshed) {
            tracing::warn!(
                path = %loaded.path.display(),
                error = %e,
                "failed to persist refreshed OpenAI Codex OAuth token"
            );
        }
        Ok(refreshed)
    }

    fn token_from_override_or_env(&self) -> Result<Option<OpenAiCodexToken>, ModelError> {
        let access_token = self
            .access_token_override
            .clone()
            .or_else(|| env_string("MUAGENT_CODEX_ACCESS_TOKEN"))
            .or_else(|| env_string("OPENAI_CODEX_ACCESS_TOKEN"));
        let Some(access_token) = access_token else {
            return Ok(None);
        };
        let account_id = env_string("MUAGENT_CODEX_ACCOUNT_ID")
            .or_else(|| env_string("OPENAI_CODEX_ACCOUNT_ID"))
            .or_else(|| extract_openai_codex_account_id(&access_token))
            .unwrap_or_default();
        let expires_at_ms = jwt_expiry_ms(&access_token);
        Ok(Some(OpenAiCodexToken {
            access_token,
            refresh_token: env_string("MUAGENT_CODEX_REFRESH_TOKEN")
                .or_else(|| env_string("OPENAI_CODEX_REFRESH_TOKEN")),
            account_id,
            expires_at_ms,
        }))
    }
}

#[derive(Clone, Debug)]
struct LoadedOpenAiCodexAuth {
    path: PathBuf,
    format: AuthFormat,
    document: Value,
    token: OpenAiCodexToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthFormat {
    PiMono,
    CodexCli,
    Generic,
}

fn load_openai_codex_auth(
    explicit_path: Option<&Path>,
) -> Result<Option<LoadedOpenAiCodexAuth>, ModelError> {
    let paths = auth_paths(explicit_path);
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if explicit_path.is_some() => {
                return Err(ModelError::Auth(format!(
                    "read OpenAI Codex auth {}: {e}",
                    path.display()
                )));
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read OpenAI Codex auth file");
                continue;
            }
        };
        let document: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) if explicit_path.is_some() => {
                return Err(ModelError::Auth(format!(
                    "parse OpenAI Codex auth {}: {e}",
                    path.display()
                )));
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to parse OpenAI Codex auth file");
                continue;
            }
        };
        if let Some(loaded) = auth_from_document(path.clone(), document) {
            return Ok(Some(loaded));
        }
    }
    Ok(None)
}

fn auth_paths(explicit_path: Option<&Path>) -> Vec<PathBuf> {
    if let Some(path) = explicit_path {
        return vec![path.to_path_buf()];
    }

    let mut out = Vec::new();
    for name in ["MUAGENT_CODEX_AUTH_FILE", "MUAGENT_OPENAI_CODEX_AUTH_FILE"] {
        if let Some(path) = env_string(name) {
            out.push(expand_home(path));
        }
    }
    if let Some(home) = home_dir() {
        out.push(home.join(".muagent").join("auth.json"));
        out.push(home.join(".pi").join("agent").join("auth.json"));
        out.push(home.join(".codex").join("auth.json"));
    }
    out
}

fn auth_from_document(path: PathBuf, document: Value) -> Option<LoadedOpenAiCodexAuth> {
    pi_mono_token(&document)
        .map(|token| LoadedOpenAiCodexAuth {
            path: path.clone(),
            format: AuthFormat::PiMono,
            document: document.clone(),
            token,
        })
        .or_else(|| {
            codex_cli_token(&document).map(|token| LoadedOpenAiCodexAuth {
                path: path.clone(),
                format: AuthFormat::CodexCli,
                document: document.clone(),
                token,
            })
        })
        .or_else(|| {
            generic_token(&document).map(|token| LoadedOpenAiCodexAuth {
                path,
                format: AuthFormat::Generic,
                document,
                token,
            })
        })
}

fn pi_mono_token(document: &Value) -> Option<OpenAiCodexToken> {
    let entry = document.get("openai-codex")?;
    let access_token = value_str(entry, "access")
        .or_else(|| value_str(entry, "access_token"))?
        .to_string();
    Some(OpenAiCodexToken {
        refresh_token: value_str(entry, "refresh")
            .or_else(|| value_str(entry, "refresh_token"))
            .map(ToOwned::to_owned),
        account_id: value_str(entry, "accountId")
            .or_else(|| value_str(entry, "account_id"))
            .map(ToOwned::to_owned)
            .or_else(|| extract_openai_codex_account_id(&access_token))
            .unwrap_or_default(),
        expires_at_ms: value_i64(entry, "expires").or_else(|| jwt_expiry_ms(&access_token)),
        access_token,
    })
}

fn codex_cli_token(document: &Value) -> Option<OpenAiCodexToken> {
    let tokens = document.get("tokens")?;
    let access_token = value_str(tokens, "access_token")?.to_string();
    Some(OpenAiCodexToken {
        refresh_token: value_str(tokens, "refresh_token").map(ToOwned::to_owned),
        account_id: value_str(tokens, "account_id")
            .map(ToOwned::to_owned)
            .or_else(|| extract_openai_codex_account_id(&access_token))
            .unwrap_or_default(),
        expires_at_ms: jwt_expiry_ms(&access_token),
        access_token,
    })
}

fn generic_token(document: &Value) -> Option<OpenAiCodexToken> {
    let access_token = value_str(document, "access_token")
        .or_else(|| value_str(document, "access"))?
        .to_string();
    Some(OpenAiCodexToken {
        refresh_token: value_str(document, "refresh_token")
            .or_else(|| value_str(document, "refresh"))
            .map(ToOwned::to_owned),
        account_id: value_str(document, "account_id")
            .or_else(|| value_str(document, "accountId"))
            .map(ToOwned::to_owned)
            .or_else(|| extract_openai_codex_account_id(&access_token))
            .unwrap_or_default(),
        expires_at_ms: value_i64(document, "expires_at_ms")
            .or_else(|| value_i64(document, "expires"))
            .or_else(|| jwt_expiry_ms(&access_token)),
        access_token,
    })
}

fn complete_account_id(mut token: OpenAiCodexToken) -> Result<OpenAiCodexToken, ModelError> {
    if token.account_id.trim().is_empty() {
        token.account_id = extract_openai_codex_account_id(&token.access_token).unwrap_or_default();
    }
    if token.account_id.trim().is_empty() {
        return Err(ModelError::Auth(
            "OpenAI Codex OAuth token has no ChatGPT account id; set OPENAI_CODEX_ACCOUNT_ID or refresh login credentials.".into(),
        ));
    }
    Ok(token)
}

fn token_is_fresh(token: &OpenAiCodexToken) -> bool {
    token
        .expires_at_ms
        .map(|expires| expires > now_ms() + EXPIRY_SKEW_MS)
        .unwrap_or(true)
}

#[derive(Debug, Deserialize)]
struct TokenRefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

async fn refresh_openai_codex_token(
    token: &OpenAiCodexToken,
    net: Arc<dyn NetEgress>,
    cancel: CancelToken,
) -> Result<OpenAiCodexToken, ModelError> {
    let refresh_token = token.refresh_token.as_deref().ok_or_else(|| {
        ModelError::Auth(
            "OpenAI Codex OAuth token is expired and no refresh token is available".into(),
        )
    })?;
    let body = form_urlencoded(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", OPENAI_CODEX_CLIENT_ID),
    ]);
    let mut headers = HashMap::new();
    headers.insert(
        "Content-Type".to_string(),
        "application/x-www-form-urlencoded".to_string(),
    );
    headers.insert("Accept".to_string(), "application/json".to_string());

    let resp = net
        .http(
            HttpReq {
                method: HttpMethod::Post,
                url: OPENAI_CODEX_TOKEN_URL.to_string(),
                headers,
                body: Some(body.into_bytes()),
            },
            cancel,
        )
        .await
        .map_err(net_err_to_model)?;

    if resp.status != 200 {
        return Err(ModelError::Auth(format!(
            "OpenAI Codex OAuth refresh failed: status {}: {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
        )));
    }

    let parsed: TokenRefreshResponse = serde_json::from_slice(&resp.body).map_err(|e| {
        ModelError::Parse(format!(
            "parse OpenAI Codex OAuth refresh response: {e}: {}",
            String::from_utf8_lossy(&resp.body)
        ))
    })?;
    let access_token = parsed
        .access_token
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            ModelError::Parse("OpenAI Codex OAuth refresh missing access_token".into())
        })?;
    let refresh_token = parsed
        .refresh_token
        .filter(|s| !s.trim().is_empty())
        .or_else(|| token.refresh_token.clone());
    let expires_at_ms = parsed
        .expires_in
        .map(|seconds| now_ms() + seconds.saturating_mul(1000))
        .or_else(|| jwt_expiry_ms(&access_token));
    complete_account_id(OpenAiCodexToken {
        account_id: extract_openai_codex_account_id(&access_token)
            .unwrap_or_else(|| token.account_id.clone()),
        access_token,
        refresh_token,
        expires_at_ms,
    })
}

fn persist_refreshed_token(
    loaded: &LoadedOpenAiCodexAuth,
    token: &OpenAiCodexToken,
) -> Result<(), String> {
    let mut document = loaded.document.clone();
    match loaded.format {
        AuthFormat::PiMono => update_pi_mono_document(&mut document, token)?,
        AuthFormat::CodexCli => update_codex_cli_document(&mut document, token)?,
        AuthFormat::Generic => update_generic_document(&mut document, token)?,
    }
    let bytes = serde_json::to_vec_pretty(&document).map_err(|e| e.to_string())?;
    if let Some(parent) = loaded.path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    fs::write(&loaded.path, bytes).map_err(|e| format!("write {}: {e}", loaded.path.display()))?;
    set_private_permissions(&loaded.path);
    Ok(())
}

fn update_pi_mono_document(document: &mut Value, token: &OpenAiCodexToken) -> Result<(), String> {
    let Some(root) = document.as_object_mut() else {
        return Err("auth document root is not an object".into());
    };
    let entry = root
        .entry("openai-codex".to_string())
        .or_insert_with(|| json!({}));
    let Some(entry) = entry.as_object_mut() else {
        return Err("openai-codex auth entry is not an object".into());
    };
    entry.insert("type".into(), Value::String("oauth".into()));
    entry.insert("access".into(), Value::String(token.access_token.clone()));
    if let Some(refresh) = &token.refresh_token {
        entry.insert("refresh".into(), Value::String(refresh.clone()));
    }
    if let Some(expires) = token.expires_at_ms {
        entry.insert("expires".into(), Value::Number(expires.into()));
    }
    entry.insert("accountId".into(), Value::String(token.account_id.clone()));
    Ok(())
}

fn update_codex_cli_document(document: &mut Value, token: &OpenAiCodexToken) -> Result<(), String> {
    let Some(root) = document.as_object_mut() else {
        return Err("auth document root is not an object".into());
    };
    let tokens = root
        .entry("tokens".to_string())
        .or_insert_with(|| json!({}));
    let Some(tokens) = tokens.as_object_mut() else {
        return Err("tokens auth entry is not an object".into());
    };
    tokens.insert(
        "access_token".into(),
        Value::String(token.access_token.clone()),
    );
    if let Some(refresh) = &token.refresh_token {
        tokens.insert("refresh_token".into(), Value::String(refresh.clone()));
    }
    tokens.insert("account_id".into(), Value::String(token.account_id.clone()));
    Ok(())
}

fn update_generic_document(document: &mut Value, token: &OpenAiCodexToken) -> Result<(), String> {
    let Some(root) = document.as_object_mut() else {
        return Err("auth document root is not an object".into());
    };
    root.insert(
        "access_token".into(),
        Value::String(token.access_token.clone()),
    );
    if let Some(refresh) = &token.refresh_token {
        root.insert("refresh_token".into(), Value::String(refresh.clone()));
    }
    root.insert("account_id".into(), Value::String(token.account_id.clone()));
    if let Some(expires) = token.expires_at_ms {
        root.insert("expires_at_ms".into(), Value::Number(expires.into()));
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) {}

pub fn extract_openai_codex_account_id(token: &str) -> Option<String> {
    jwt_payload(token)
        .and_then(|payload| payload.get(OPENAI_CODEX_JWT_CLAIM).cloned())
        .and_then(|auth| auth.get("chatgpt_account_id").cloned())
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
}

fn jwt_expiry_ms(token: &str) -> Option<i64> {
    jwt_payload(token)
        .and_then(|payload| payload.get("exp").cloned())
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_u64().and_then(|n| i64::try_from(n).ok()))
        })
        .map(|seconds| seconds.saturating_mul(1000))
}

fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64url_decode(payload)?;
    serde_json::from_slice(&decoded).ok()
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for b in input.bytes() {
        let value = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

fn form_urlencoded(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn form_encode(raw: &str) -> String {
    let mut out = String::new();
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn value_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn value_i64(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
    })
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

fn expand_home(raw: impl AsRef<str>) -> PathBuf {
    let raw = raw.as_ref();
    if let Some(rest) = raw.strip_prefix("~/") {
        home_dir().unwrap_or_else(|| PathBuf::from(".")).join(rest)
    } else {
        PathBuf::from(raw)
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pi_mono_auth_shape() {
        let token = fake_jwt("acct_jwt", 2_000_000_000);
        let doc = json!({
            "openai-codex": {
                "type": "oauth",
                "access": token,
                "refresh": "refresh_1",
                "expires": 2_000_000_000_000i64,
                "accountId": "acct_file"
            }
        });
        let loaded = auth_from_document(PathBuf::from("/tmp/auth.json"), doc).unwrap();
        assert_eq!(loaded.format, AuthFormat::PiMono);
        assert_eq!(loaded.token.account_id, "acct_file");
        assert_eq!(loaded.token.refresh_token.as_deref(), Some("refresh_1"));
    }

    #[test]
    fn parses_codex_cli_auth_shape() {
        let token = fake_jwt("acct_codex", 2_000_000_000);
        let doc = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": token,
                "refresh_token": "refresh_2"
            }
        });
        let loaded = auth_from_document(PathBuf::from("/tmp/auth.json"), doc).unwrap();
        assert_eq!(loaded.format, AuthFormat::CodexCli);
        assert_eq!(loaded.token.account_id, "acct_codex");
        assert_eq!(loaded.token.expires_at_ms, Some(2_000_000_000_000));
    }

    #[test]
    fn form_encoding_handles_token_characters() {
        let encoded = form_urlencoded(&[("refresh_token", "a/b+c=d")]);
        assert_eq!(encoded, "refresh_token=a%2Fb%2Bc%3Dd");
    }

    fn fake_jwt(account_id: &str, exp: i64) -> String {
        let payload = json!({
            OPENAI_CODEX_JWT_CLAIM: {
                "chatgpt_account_id": account_id
            },
            "exp": exp
        });
        format!(
            "{}.{}.{}",
            b64url(r#"{"alg":"none"}"#.as_bytes()),
            b64url(payload.to_string().as_bytes()),
            ""
        )
    }

    fn b64url(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        let mut i = 0;
        while i < bytes.len() {
            let b0 = bytes[i];
            let b1 = bytes.get(i + 1).copied().unwrap_or(0);
            let b2 = bytes.get(i + 2).copied().unwrap_or(0);
            out.push(TABLE[(b0 >> 2) as usize] as char);
            out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            if i + 1 < bytes.len() {
                out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
            }
            if i + 2 < bytes.len() {
                out.push(TABLE[(b2 & 0x3f) as usize] as char);
            }
            i += 3;
        }
        out
    }
}
