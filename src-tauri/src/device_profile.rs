use image::imageops::FilterType;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

const PROFILE_FORMAT_VERSION: u8 = 1;
const MAX_NICKNAME_CHARACTERS: usize = 24;
const MAX_NICKNAME_BYTES: usize = 72;
const AVATAR_EDGE: u32 = 256;
const AVATAR_JPEG_QUALITY: u8 = 86;
const MAX_AVATAR_SOURCE_BYTES: u64 = 10 * 1024 * 1024;

pub const DEVICE_PROFILE_FILE_NAME: &str = "device-profile-v1.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceProfile {
    pub nickname: String,
    pub avatar_hash: Option<String>,
    pub avatar_path: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredDeviceProfile {
    format_version: u8,
    nickname: String,
    #[serde(default)]
    avatar_hash: Option<String>,
}

pub struct DeviceProfileState {
    path: PathBuf,
    profile: RwLock<DeviceProfile>,
}

impl DeviceProfileState {
    pub fn load_or_create(path: impl Into<PathBuf>, display_id: &str) -> Result<Self, String> {
        let path = path.into();
        let profile = match fs::read(&path) {
            Ok(contents) => {
                let stored: StoredDeviceProfile = serde_json::from_slice(&contents)
                    .map_err(|error| format!("设备资料文件无效：{error}"))?;
                if stored.format_version != PROFILE_FORMAT_VERSION {
                    return Err(format!("不支持的设备资料版本 {}", stored.format_version));
                }
                profile_from_stored(&path, stored)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let profile = DeviceProfile {
                    nickname: default_nickname(display_id),
                    avatar_hash: None,
                    avatar_path: None,
                };
                save_profile(&path, &profile)?;
                profile
            }
            Err(error) => return Err(format!("无法读取设备资料：{error}")),
        };

        Ok(Self {
            path,
            profile: RwLock::new(profile),
        })
    }

    pub fn snapshot(&self) -> DeviceProfile {
        self.profile.read().expect("device profile lock").clone()
    }

    pub fn save(&self, nickname: &str, avatar_hash: Option<&str>) -> Result<DeviceProfile, String> {
        let avatar_hash = avatar_hash.map(validate_avatar_hash).transpose()?;
        let profile = DeviceProfile {
            nickname: normalize_nickname(nickname)?,
            avatar_path: avatar_hash
                .as_deref()
                .map(|hash| path_string(&avatar_path_for_profile(&self.path, hash))),
            avatar_hash,
        };
        save_profile(&self.path, &profile)?;
        *self.profile.write().expect("device profile lock") = profile.clone();
        Ok(profile)
    }

    pub fn prepare_avatar(&self, source_path: &Path) -> Result<(String, PathBuf), String> {
        let metadata =
            fs::metadata(source_path).map_err(|error| format!("无法读取头像：{error}"))?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err("请选择有效的图片文件".to_owned());
        }
        if metadata.len() > MAX_AVATAR_SOURCE_BYTES {
            return Err("头像原图不能超过 10 MB".to_owned());
        }

        let source = fs::read(source_path).map_err(|error| format!("无法读取头像：{error}"))?;
        let image = image::load_from_memory(&source)
            .map_err(|_| "无法识别这张图片，请选择 JPG、PNG 或 WebP".to_owned())?;
        let edge = image.width().min(image.height());
        if edge == 0 {
            return Err("图片尺寸无效".to_owned());
        }
        let left = (image.width() - edge) / 2;
        let top = (image.height() - edge) / 2;
        let avatar = image
            .crop_imm(left, top, edge, edge)
            .resize_exact(AVATAR_EDGE, AVATAR_EDGE, FilterType::Lanczos3)
            .to_rgb8();
        let mut encoded = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, AVATAR_JPEG_QUALITY)
            .encode_image(&avatar)
            .map_err(|error| format!("无法处理头像：{error}"))?;

        let hash = hex::encode(Sha256::digest(&encoded));
        let avatar_path = avatar_path_for_profile(&self.path, &hash);
        let avatar_dir = avatar_path
            .parent()
            .ok_or_else(|| "头像路径无效".to_owned())?;
        fs::create_dir_all(avatar_dir).map_err(|error| format!("无法创建头像目录：{error}"))?;
        if !avatar_path.exists() {
            let temporary = avatar_dir.join(format!("self-{hash}.tmp"));
            fs::write(&temporary, encoded).map_err(|error| format!("无法保存头像：{error}"))?;
            fs::rename(&temporary, &avatar_path)
                .map_err(|error| format!("无法保存头像：{error}"))?;
        }
        Ok((hash, avatar_path))
    }

    pub fn remove_avatar_file(&self, hash: &str) {
        let path = avatar_path_for_profile(&self.path, hash);
        if let Err(error) = fs::remove_file(path) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("TossIt could not remove old avatar: {error}");
            }
        }
    }
}

pub fn normalize_nickname(value: &str) -> Result<String, String> {
    let nickname = value.trim();
    if nickname.is_empty() {
        return Err("昵称不能为空".to_owned());
    }
    if nickname.chars().count() > MAX_NICKNAME_CHARACTERS || nickname.len() > MAX_NICKNAME_BYTES {
        return Err(format!("昵称最多 {MAX_NICKNAME_CHARACTERS} 个字"));
    }
    if nickname.chars().any(char::is_control) {
        return Err("昵称不能包含控制字符".to_owned());
    }
    Ok(nickname.to_owned())
}

fn profile_from_stored(path: &Path, stored: StoredDeviceProfile) -> Result<DeviceProfile, String> {
    let avatar_hash = stored
        .avatar_hash
        .as_deref()
        .map(validate_avatar_hash)
        .transpose()?;
    let avatar_path = avatar_hash.as_deref().and_then(|hash| {
        let path = avatar_path_for_profile(path, hash);
        path.is_file().then(|| path_string(&path))
    });
    Ok(DeviceProfile {
        nickname: normalize_nickname(&stored.nickname)?,
        avatar_hash: avatar_path.as_ref().and(avatar_hash),
        avatar_path,
    })
}

fn validate_avatar_hash(value: &str) -> Result<String, String> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err("头像标识无效".to_owned())
    }
}

fn default_nickname(display_id: &str) -> String {
    let suffix = display_id.rsplit('-').next().unwrap_or(display_id);
    let device = if cfg!(target_os = "ios") {
        "iPhone"
    } else if cfg!(target_os = "macos") {
        "Mac"
    } else {
        "设备"
    };
    format!("{device} {suffix}")
}

fn avatar_path_for_profile(profile_path: &Path, hash: &str) -> PathBuf {
    profile_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("avatars")
        .join(format!("self-{hash}.jpg"))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn save_profile(path: &Path, profile: &DeviceProfile) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| "设备资料路径无效".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| format!("无法创建设备资料目录：{error}"))?;
    let stored = StoredDeviceProfile {
        format_version: PROFILE_FORMAT_VERSION,
        nickname: profile.nickname.clone(),
        avatar_hash: profile.avatar_hash.clone(),
    };
    let mut contents =
        serde_json::to_vec_pretty(&stored).map_err(|error| format!("无法保存设备资料：{error}"))?;
    contents.push(b'\n');
    fs::write(path, contents).map_err(|error| format!("无法保存设备资料：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nickname_is_trimmed_and_bounded() {
        assert_eq!(
            normalize_nickname("  Yang 的 iPhone  ").unwrap(),
            "Yang 的 iPhone"
        );
        assert!(normalize_nickname("   ").is_err());
        assert!(normalize_nickname(&"名".repeat(25)).is_err());
    }

    #[test]
    fn profile_survives_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(DEVICE_PROFILE_FILE_NAME);
        let profile = DeviceProfileState::load_or_create(&path, "AAAA-BBBB-CCCC").unwrap();
        profile.save("客厅的 iPhone", None).unwrap();

        let reloaded = DeviceProfileState::load_or_create(&path, "AAAA-BBBB-CCCC").unwrap();
        assert_eq!(reloaded.snapshot().nickname, "客厅的 iPhone");
        assert_eq!(reloaded.snapshot().avatar_hash, None);
    }

    #[test]
    fn avatar_is_center_cropped_and_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(DEVICE_PROFILE_FILE_NAME);
        let source = directory.path().join("source.png");
        image::RgbImage::from_pixel(640, 320, image::Rgb([40, 90, 180]))
            .save(&source)
            .unwrap();
        let profile = DeviceProfileState::load_or_create(&path, "AAAA-BBBB-CCCC").unwrap();
        let (hash, avatar_path) = profile.prepare_avatar(&source).unwrap();
        profile.save("客厅的 iPhone", Some(&hash)).unwrap();

        let avatar = image::open(&avatar_path).unwrap();
        assert_eq!(
            (avatar.width(), avatar.height()),
            (AVATAR_EDGE, AVATAR_EDGE)
        );
        let reloaded = DeviceProfileState::load_or_create(&path, "AAAA-BBBB-CCCC").unwrap();
        assert_eq!(
            reloaded.snapshot().avatar_hash.as_deref(),
            Some(hash.as_str())
        );
        assert_eq!(
            reloaded.snapshot().avatar_path.as_deref(),
            Some(path_string(&avatar_path).as_str())
        );
    }
}
