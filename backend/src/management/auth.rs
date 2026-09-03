//! WebUI 用户快照与密码 hash 边界。

use std::sync::RwLock;

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use thiserror::Error;

use crate::config::resolve::ResolvedWebUiUser;
use crate::config::validate::normalize_webui_username;

pub(crate) const SETUP_PASSWORD_MIN_BYTES: usize = 12;
pub(crate) const PASSWORD_MAX_BYTES: usize = 1024;
pub(crate) const ARGON2_MEMORY_KIB: u32 = 19 * 1024;
pub(crate) const ARGON2_ITERATIONS: u32 = 2;
pub(crate) const ARGON2_PARALLELISM: u32 = 1;
pub(crate) const ARGON2_OUTPUT_BYTES: usize = 32;

#[derive(Clone)]
struct UserCredential {
    name: String,
    password_hash: String,
}

/// 可原子替换的认证用户快照；hash 永不进入 Debug 输出。
pub(crate) struct AuthState {
    users: RwLock<Vec<UserCredential>>,
    dummy_hash: String,
}

impl AuthState {
    pub(crate) fn new(users: &[ResolvedWebUiUser]) -> Result<Self, AuthError> {
        Ok(Self {
            users: RwLock::new(project_users(users)),
            dummy_hash: hash_password("fluxdns-unknown-user-dummy-password")?,
        })
    }

    pub(crate) fn setup_required(&self) -> bool {
        self.users
            .read()
            .map(|users| users.is_empty())
            .unwrap_or(false)
    }

    pub(crate) fn replace(&self, users: &[ResolvedWebUiUser]) {
        *self
            .users
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = project_users(users);
    }

    /// 未知用户仍执行 dummy verify，避免明显的用户名计时分支。
    pub(crate) fn authenticate(&self, username: &str, password: &str) -> Result<String, AuthError> {
        let username = normalize_webui_username(username).ok_or(AuthError::InvalidCredentials)?;
        if password.is_empty() || password.len() > PASSWORD_MAX_BYTES {
            return Err(AuthError::InvalidCredentials);
        }
        let users = self
            .users
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let credential = users.iter().find(|user| user.name == username);
        let hash = credential
            .map(|user| user.password_hash.as_str())
            .unwrap_or(&self.dummy_hash);
        let verified = verify_password(hash, password)?;
        match (credential, verified) {
            (Some(user), true) => Ok(user.name.clone()),
            _ => Err(AuthError::InvalidCredentials),
        }
    }
}

fn project_users(users: &[ResolvedWebUiUser]) -> Vec<UserCredential> {
    users
        .iter()
        .map(|user| UserCredential {
            name: user.name.clone(),
            password_hash: user.password_hash().to_owned(),
        })
        .collect()
}

pub(crate) fn validate_setup_credentials(
    username: &str,
    password: &str,
) -> Result<String, AuthError> {
    let username = normalize_webui_username(username).ok_or(AuthError::InvalidUsername)?;
    if !(SETUP_PASSWORD_MIN_BYTES..=PASSWORD_MAX_BYTES).contains(&password.len()) {
        return Err(AuthError::InvalidPasswordPolicy);
    }
    Ok(username)
}

pub(crate) fn hash_password(password: &str) -> Result<String, AuthError> {
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        Some(ARGON2_OUTPUT_BYTES),
    )
    .map_err(|_| AuthError::HashFailure)?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|_| AuthError::HashFailure)
}

fn verify_password(hash: &str, password: &str) -> Result<bool, AuthError> {
    if hash.starts_with("$argon2id$") {
        let parsed = PasswordHash::new(hash).map_err(|_| AuthError::HashFailure)?;
        return Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok());
    }
    if hash.starts_with("$2a$") || hash.starts_with("$2b$") || hash.starts_with("$2y$") {
        return bcrypt::verify(password, hash).map_err(|_| AuthError::HashFailure);
    }
    Err(AuthError::HashFailure)
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum AuthError {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("invalid username")]
    InvalidUsername,
    #[error("password does not satisfy setup policy")]
    InvalidPasswordPolicy,
    #[error("password hash operation failed")]
    HashFailure,
}

#[cfg(test)]
mod tests {
    use super::{hash_password, verify_password};

    #[test]
    fn new_hashes_are_argon2id_and_verify() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(verify_password(&hash, "correct horse battery staple").unwrap());
        assert!(!verify_password(&hash, "wrong password").unwrap());
    }

    #[test]
    fn configured_bcrypt_hashes_remain_compatible() {
        let hash = bcrypt::hash("legacy password", 4).unwrap();
        assert!(verify_password(&hash, "legacy password").unwrap());
        assert!(!verify_password(&hash, "wrong password").unwrap());
    }
}
