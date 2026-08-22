use crate::{
    error::ApiError,
    model::{MintKeyResponse, PairResponse},
    util::{iso_from_epoch, iso_now, now_epoch, write_private_json},
};
use axum::http::StatusCode;
use qrcode::{render::unicode, QrCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::sync::RwLock;
use uuid::Uuid;

const PAIR_CODE_LIFETIME_SECONDS: i64 = 15 * 60;
const PAIR_MARKER_START: &str = "<!-- board-api-pair:start -->";
const PAIR_MARKER_END: &str = "<!-- board-api-pair:end -->";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredKey {
    id: String,
    sha256: String,
    created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairChallenge {
    sha256: String,
    expires_at_epoch: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthStore {
    server_id: String,
    keys: Vec<StoredKey>,
    pair: Option<PairChallenge>,
}

pub struct AuthManager {
    path: PathBuf,
    host_document: PathBuf,
    store: RwLock<AuthStore>,
}

impl AuthManager {
    pub fn load_or_create(state_dir: &Path, host_document: PathBuf) -> Result<Self, String> {
        let path = state_dir.join("auth.json");
        let mut store = if path.exists() {
            let bytes = fs::read(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            serde_json::from_slice::<AuthStore>(&bytes)
                .map_err(|error| format!("invalid {}: {error}", path.display()))?
        } else {
            AuthStore {
                server_id: Uuid::new_v4().to_string(),
                keys: Vec::new(),
                pair: None,
            }
        };

        let mut raw_pair = None;
        if store.keys.is_empty()
            && store
                .pair
                .as_ref()
                .map(|pair| pair.expires_at_epoch <= now_epoch())
                .unwrap_or(true)
        {
            let code = generate_pair_code()?;
            store.pair = Some(PairChallenge {
                sha256: hash(&code),
                expires_at_epoch: now_epoch() + PAIR_CODE_LIFETIME_SECONDS,
            });
            raw_pair = Some(code);
        }
        write_private_json(&path, &store)?;

        if let Some(code) = raw_pair {
            let expires = store
                .pair
                .as_ref()
                .map(|pair| pair.expires_at_epoch)
                .unwrap_or_default();
            let qr = render_pair_qr(&code)?;
            write_pair_block(&host_document, &code, expires, &qr)?;
            tracing::warn!(
                pair_code = %code,
                expires_at = %iso_from_epoch(expires),
                "board API one-time pairing code\nScan QR code:\n{qr}"
            );
        }

        Ok(Self {
            path,
            host_document,
            store: RwLock::new(store),
        })
    }

    pub async fn server_id(&self) -> String {
        self.store.read().await.server_id.clone()
    }

    pub async fn authorize(&self, token: &str) -> bool {
        if !token.starts_with("board_") || token.len() < 20 {
            return false;
        }
        let candidate = hash(token);
        self.store
            .read()
            .await
            .keys
            .iter()
            .any(|key| constant_time_equal(key.sha256.as_bytes(), candidate.as_bytes()))
    }

    pub async fn pair(&self, code: &str, base_url: String) -> Result<PairResponse, ApiError> {
        let mut store = self.store.write().await;
        if !store.keys.is_empty() || store.pair.is_none() {
            return Err(ApiError::conflict(
                "pairing_unavailable",
                "pairing code has already been used",
            ));
        }
        let challenge = store.pair.as_ref().expect("checked above");
        if challenge.expires_at_epoch <= now_epoch() {
            drop(store);
            clear_pair_block(
                &self.host_document,
                "Pair code expired. Restart board-api to mint another.",
            )
            .map_err(ApiError::internal)?;
            return Err(ApiError::new(
                StatusCode::GONE,
                "pairing_expired",
                "pairing code expired",
            ));
        }
        let candidate = hash(&code.trim().to_ascii_uppercase());
        if !constant_time_equal(challenge.sha256.as_bytes(), candidate.as_bytes()) {
            return Err(ApiError::unauthorized());
        }

        let minted = mint_stored_key().map_err(ApiError::internal)?;
        let token = minted.1;
        store.keys.push(minted.0);
        store.pair = None;
        write_private_json(&self.path, &*store).map_err(ApiError::internal)?;
        let server_id = store.server_id.clone();
        drop(store);
        clear_pair_block(
            &self.host_document,
            "Pair code consumed; no active one-time code.",
        )
        .map_err(ApiError::internal)?;
        Ok(PairResponse {
            token,
            server_id,
            name: "board",
            base_url,
        })
    }

    pub async fn mint_key(&self) -> Result<MintKeyResponse, ApiError> {
        let (stored, token) = mint_stored_key().map_err(ApiError::internal)?;
        let id = stored.id.clone();
        let mut store = self.store.write().await;
        store.keys.push(stored);
        write_private_json(&self.path, &*store).map_err(ApiError::internal)?;
        Ok(MintKeyResponse { id, token })
    }

    pub async fn revoke(&self, id: &str) -> Result<(), ApiError> {
        let mut store = self.store.write().await;
        let old_len = store.keys.len();
        store.keys.retain(|key| key.id != id);
        if store.keys.len() == old_len {
            return Err(ApiError::not_found("key"));
        }
        write_private_json(&self.path, &*store).map_err(ApiError::internal)
    }
}

fn mint_stored_key() -> Result<(StoredKey, String), String> {
    let token = format!("board_{}", hex(&random_bytes(32)?));
    let stored = StoredKey {
        id: Uuid::new_v4().to_string(),
        sha256: hash(&token),
        created_at: iso_now(),
    };
    Ok((stored, token))
}

fn generate_pair_code() -> Result<String, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let bytes = random_bytes(8)?;
    Ok(bytes
        .into_iter()
        .map(|byte| ALPHABET[(byte as usize) % ALPHABET.len()] as char)
        .collect())
}

fn random_bytes(length: usize) -> Result<Vec<u8>, String> {
    let mut file = fs::File::open("/dev/urandom")
        .map_err(|error| format!("cannot open system random source: {error}"))?;
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("cannot read system random source: {error}"))?;
    Ok(bytes)
}

fn hash(value: &str) -> String {
    hex(Sha256::digest(value.as_bytes()))
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn render_pair_qr(code: &str) -> Result<String, String> {
    let code = QrCode::new(code.as_bytes())
        .map_err(|error| format!("cannot encode pairing QR code: {error}"))?;
    Ok(code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}

fn write_pair_block(path: &Path, code: &str, expires: i64, qr: &str) -> Result<(), String> {
    replace_pair_block(
        path,
        &format!(
            "{PAIR_MARKER_START}\n## Current pairing\n\nPair code: `{code}`\n\nScan this QR code with your phone camera. It contains the pair code only.\n\n```text\n{qr}\n```\n\nExpires: `{}`\n\nPair with `POST /v1/pair`; code works once.\n{PAIR_MARKER_END}",
            iso_from_epoch(expires)
        ),
    )
}

fn clear_pair_block(path: &Path, message: &str) -> Result<(), String> {
    replace_pair_block(
        path,
        &format!("{PAIR_MARKER_START}\n## Current pairing\n\n{message}\n{PAIR_MARKER_END}"),
    )
}

fn replace_pair_block(path: &Path, replacement: &str) -> Result<(), String> {
    let current = fs::read_to_string(path).unwrap_or_default();
    let updated = if let (Some(start), Some(end)) = (
        current.find(PAIR_MARKER_START),
        current.find(PAIR_MARKER_END),
    ) {
        let end = end + PAIR_MARKER_END.len();
        format!("{}{}{}", &current[..start], replacement, &current[end..])
    } else if current.is_empty() {
        format!("# board host\n\n{replacement}\n")
    } else {
        format!("{}\n\n{replacement}\n", current.trim_end())
    };
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    file.write_all(updated.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync {}: {error}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("cannot secure {}: {error}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_key_contains_hash_not_token() {
        let (stored, token) = mint_stored_key().unwrap();
        let serialized = serde_json::to_string(&stored).unwrap();
        assert!(token.starts_with("board_"));
        assert!(!serialized.contains(&token));
        assert_eq!(stored.sha256.len(), 64);
    }

    #[test]
    fn pair_codes_are_eight_safe_characters() {
        let code = generate_pair_code().unwrap();
        assert_eq!(code.len(), 8);
        assert!(code
            .chars()
            .all(|character| character.is_ascii_alphanumeric()));
    }

    #[test]
    fn pairing_qr_is_terminal_sized_and_deterministic() {
        let first = render_pair_qr("ABCDEFGH").unwrap();
        let second = render_pair_qr("ABCDEFGH").unwrap();
        let lines = first.lines().collect::<Vec<_>>();

        assert_eq!(first, second);
        assert!(lines.len() >= 10);
        assert!(lines
            .iter()
            .all(|line| line.chars().count() == lines[0].chars().count()));
        assert!(first.contains('█'));
        assert!(first.contains(' '));
    }

    #[test]
    fn host_pairing_block_contains_code_and_qr() {
        let path = std::env::temp_dir().join(format!("board-api-host-{}.md", Uuid::new_v4()));
        let qr = render_pair_qr("ABCDEFGH").unwrap();

        write_pair_block(&path, "ABCDEFGH", 1_800_000_000, &qr).unwrap();
        let host = fs::read_to_string(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        assert!(host.contains("Pair code: `ABCDEFGH`"));
        assert!(host.contains("Scan this QR code"));
        assert!(host.contains(&qr));
        assert_eq!(mode, 0o600);

        fs::remove_file(path).unwrap();
    }
}
