use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SerialNumber};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

const IDENTITY_FORMAT_VERSION: u8 = 1;
const SECRET_KEY_BYTES: usize = 32;
const PUBLIC_KEY_BYTES: usize = 32;
const SIGNATURE_BYTES: usize = 64;

pub const IDENTITY_FILE_NAME: &str = "identity-v1.json";

#[derive(Debug)]
pub struct DeviceIdentity {
    signing_key: SigningKey,
    summary: DeviceIdentitySummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentitySummary {
    pub peer_id: String,
    pub display_id: String,
    pub public_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsIdentityMaterial {
    pub certificate_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
    pub certificate_fingerprint: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredIdentity {
    format_version: u8,
    secret_key: String,
}

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("identity path has no parent directory")]
    MissingParent(PathBuf),
    #[error("identity I/O failed: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("identity file is not valid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("unsupported identity format version {0}")]
    UnsupportedFormatVersion(u8),
    #[error("identity secret key must be 32 bytes of hexadecimal data")]
    InvalidSecretKey,
    #[error("identity signature must be 64 bytes of hexadecimal data")]
    InvalidSignature,
    #[error("identity public key must be 32 bytes of hexadecimal data")]
    InvalidPublicKey,
    #[error("identity signature verification failed")]
    InvalidSignatureVerification,
    #[error("identity private key could not be encoded for TLS: {0}")]
    PrivateKeyEncoding(String),
    #[error("identity TLS certificate could not be created: {0}")]
    Certificate(String),
    #[error("the operating system could not generate secure random data: {0}")]
    Random(#[from] getrandom::Error),
    #[error("Apple Keychain failed: {0}")]
    Keychain(String),
}

impl DeviceIdentity {
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, IdentityError> {
        let path = path.as_ref();

        match fs::read(path) {
            Ok(contents) => {
                set_owner_only(path)?;
                Self::from_stored_bytes(&contents)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::create(path),
            Err(source) => Err(io_error(path, source)),
        }
    }

    #[cfg(any(target_os = "ios", target_os = "macos"))]
    pub fn load_or_create_in_keychain(
        legacy_path: impl AsRef<Path>,
        service: &str,
        account: &str,
    ) -> Result<Self, IdentityError> {
        use security_framework::passwords::{get_generic_password, set_generic_password};
        use security_framework_sys::base::errSecItemNotFound;

        let legacy_path = legacy_path.as_ref();
        match get_generic_password(service, account) {
            Ok(contents) => {
                let identity = Self::from_stored_bytes(&contents)?;
                remove_legacy_identity_file(legacy_path)?;
                return Ok(identity);
            }
            Err(error) if error.code() == errSecItemNotFound => {}
            Err(error) => return Err(IdentityError::Keychain(error.to_string())),
        }

        let (identity, contents) = match fs::read(legacy_path) {
            Ok(contents) => {
                set_owner_only(legacy_path)?;
                (Self::from_stored_bytes(&contents)?, contents)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::generate_stored()?,
            Err(source) => return Err(io_error(legacy_path, source)),
        };
        set_generic_password(service, account, &contents)
            .map_err(|error| IdentityError::Keychain(error.to_string()))?;
        let stored = get_generic_password(service, account)
            .map_err(|error| IdentityError::Keychain(error.to_string()))?;
        if stored != contents {
            return Err(IdentityError::Keychain(
                "stored identity could not be verified".to_owned(),
            ));
        }
        remove_legacy_identity_file(legacy_path)?;
        Ok(identity)
    }

    pub fn summary(&self) -> DeviceIdentitySummary {
        self.summary.clone()
    }

    pub fn sign(&self, message: &[u8]) -> String {
        hex::encode(self.signing_key.sign(message).to_bytes())
    }

    pub fn peer_id_for_public_key(public_key: &str) -> Result<String, IdentityError> {
        let public_key = decode_fixed::<PUBLIC_KEY_BYTES>(public_key)
            .map_err(|_| IdentityError::InvalidPublicKey)?;
        VerifyingKey::from_bytes(&public_key).map_err(|_| IdentityError::InvalidPublicKey)?;
        Ok(hex::encode_upper(Sha256::digest(public_key)))
    }

    pub fn tls_material(&self) -> Result<TlsIdentityMaterial, IdentityError> {
        let private_key = self
            .signing_key
            .to_pkcs8_der()
            .map_err(|error| IdentityError::PrivateKeyEncoding(error.to_string()))?;
        let private_key_der = private_key.as_bytes().to_vec();
        let key_pair = KeyPair::try_from(private_key_der.as_slice())
            .map_err(|error| IdentityError::Certificate(error.to_string()))?;
        let mut parameters = CertificateParams::new(vec!["tossit.local".to_owned()])
            .map_err(|error| IdentityError::Certificate(error.to_string()))?;
        parameters.serial_number = Some(SerialNumber::from_slice(
            &Sha256::digest(self.summary.public_key.as_bytes())[..16],
        ));
        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(
            DnType::CommonName,
            format!("TossIt {}", self.summary.display_id),
        );
        parameters.distinguished_name = distinguished_name;
        let certificate = parameters
            .self_signed(&key_pair)
            .map_err(|error| IdentityError::Certificate(error.to_string()))?;
        let certificate_der = certificate.der().to_vec();
        let certificate_fingerprint = hex::encode_upper(Sha256::digest(&certificate_der));

        Ok(TlsIdentityMaterial {
            certificate_der,
            private_key_der,
            certificate_fingerprint,
        })
    }

    pub fn verify(public_key: &str, message: &[u8], signature: &str) -> Result<(), IdentityError> {
        let public_key = decode_fixed::<PUBLIC_KEY_BYTES>(public_key)
            .map_err(|_| IdentityError::InvalidPublicKey)?;
        let signature = decode_fixed::<SIGNATURE_BYTES>(signature)
            .map_err(|_| IdentityError::InvalidSignature)?;
        let verifying_key =
            VerifyingKey::from_bytes(&public_key).map_err(|_| IdentityError::InvalidPublicKey)?;
        let signature = Signature::from_bytes(&signature);

        verifying_key
            .verify(message, &signature)
            .map_err(|_| IdentityError::InvalidSignatureVerification)
    }

    fn create(path: &Path) -> Result<Self, IdentityError> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| IdentityError::MissingParent(path.to_path_buf()))?;
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;

        let (identity, stored) = Self::generate_stored()?;

        let mut temporary =
            NamedTempFile::new_in(parent).map_err(|source| io_error(parent, source))?;
        set_owner_only(temporary.path())?;
        temporary
            .write_all(&stored)
            .map_err(|source| io_error(temporary.path(), source))?;
        temporary
            .as_file_mut()
            .sync_all()
            .map_err(|source| io_error(temporary.path(), source))?;

        match temporary.persist_noclobber(path) {
            Ok(_) => Ok(identity),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                let contents = fs::read(path).map_err(|source| io_error(path, source))?;
                Self::from_stored_bytes(&contents)
            }
            Err(error) => Err(io_error(path, error.error)),
        }
    }

    fn generate_stored() -> Result<(Self, Vec<u8>), IdentityError> {
        let mut secret_key = [0_u8; SECRET_KEY_BYTES];
        getrandom::fill(&mut secret_key)?;
        let identity = Self::from_signing_key(SigningKey::from_bytes(&secret_key));
        let stored = StoredIdentity {
            format_version: IDENTITY_FORMAT_VERSION,
            secret_key: hex::encode(secret_key),
        };
        let mut contents = serde_json::to_vec(&stored)?;
        contents.push(b'\n');
        Ok((identity, contents))
    }

    fn from_stored_bytes(contents: &[u8]) -> Result<Self, IdentityError> {
        let stored: StoredIdentity = serde_json::from_slice(contents)?;
        if stored.format_version != IDENTITY_FORMAT_VERSION {
            return Err(IdentityError::UnsupportedFormatVersion(
                stored.format_version,
            ));
        }

        let secret_key = decode_fixed::<SECRET_KEY_BYTES>(&stored.secret_key)
            .map_err(|_| IdentityError::InvalidSecretKey)?;
        Ok(Self::from_signing_key(SigningKey::from_bytes(&secret_key)))
    }

    fn from_signing_key(signing_key: SigningKey) -> Self {
        let public_key = signing_key.verifying_key().to_bytes();
        let peer_id = hex::encode_upper(Sha256::digest(public_key));
        let display_id = peer_id.as_bytes()[..12]
            .chunks(4)
            .map(|chunk| std::str::from_utf8(chunk).expect("hex fingerprint is valid UTF-8"))
            .collect::<Vec<_>>()
            .join("-");

        Self {
            signing_key,
            summary: DeviceIdentitySummary {
                peer_id,
                display_id,
                public_key: hex::encode(public_key),
            },
        }
    }
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
fn remove_legacy_identity_file(path: &Path) -> Result<(), IdentityError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

fn decode_fixed<const LENGTH: usize>(value: &str) -> Result<[u8; LENGTH], hex::FromHexError> {
    let mut bytes = [0_u8; LENGTH];
    hex::decode_to_slice(value, &mut bytes)?;
    Ok(bytes)
}

fn io_error(path: impl AsRef<Path>, source: io::Error) -> IdentityError {
    IdentityError::Io {
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<(), IdentityError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<(), IdentityError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_reload() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join(IDENTITY_FILE_NAME);

        let created = DeviceIdentity::load_or_create(&path).expect("create identity");
        let reloaded = DeviceIdentity::load_or_create(&path).expect("reload identity");

        assert_eq!(created.summary(), reloaded.summary());
        assert_eq!(
            created.sign(b"same message"),
            reloaded.sign(b"same message")
        );
    }

    #[test]
    fn identity_can_sign_and_verify_messages() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join(IDENTITY_FILE_NAME);
        let identity = DeviceIdentity::load_or_create(&path).expect("create identity");
        let summary = identity.summary();
        let message = b"tossit identity proof";
        let signature = identity.sign(message);

        assert!(DeviceIdentity::verify(&summary.public_key, message, &signature).is_ok());
        assert!(matches!(
            DeviceIdentity::verify(&summary.public_key, b"changed", &signature),
            Err(IdentityError::InvalidSignatureVerification)
        ));
    }

    #[test]
    fn tls_certificate_is_stable_and_bound_to_identity() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join(IDENTITY_FILE_NAME);

        let created = DeviceIdentity::load_or_create(&path).expect("create identity");
        let first = created.tls_material().expect("create TLS material");
        let reloaded = DeviceIdentity::load_or_create(&path).expect("reload identity");
        let second = reloaded.tls_material().expect("recreate TLS material");

        assert_eq!(first, second);
        assert!(!first.certificate_der.is_empty());
        assert!(!first.private_key_der.is_empty());
        assert_eq!(first.certificate_fingerprint.len(), 64);
    }

    #[test]
    fn corrupted_identity_is_rejected_instead_of_replaced() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join(IDENTITY_FILE_NAME);
        fs::write(&path, b"not-json").expect("write corrupt identity");

        assert!(matches!(
            DeviceIdentity::load_or_create(&path),
            Err(IdentityError::InvalidJson(_))
        ));
        assert_eq!(fs::read(&path).expect("read corrupt identity"), b"not-json");
    }

    #[cfg(unix)]
    #[test]
    fn persisted_secret_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join(IDENTITY_FILE_NAME);
        DeviceIdentity::load_or_create(&path).expect("create identity");

        let mode = fs::metadata(path)
            .expect("identity metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn reload_tightens_secret_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join(IDENTITY_FILE_NAME);
        DeviceIdentity::load_or_create(&path).expect("create identity");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .expect("weaken permissions for test");

        DeviceIdentity::load_or_create(&path).expect("reload identity");

        let mode = fs::metadata(path)
            .expect("identity metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
