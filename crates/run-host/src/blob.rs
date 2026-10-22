use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fluxaxiom_engine_api::BlobRef;
use sha2::{Digest, Sha256};

use crate::error::HostError;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 把原文按 sha256 写入数据目录。
///
/// **作用**：返回 `sha256:` 加小写 hex，供事件引用。
/// **边界**：`media_type` 为空则拒绝，且该类型不入库、不进路径。相同字节只保留一份。
/// 先写临时文件再 rename；调用方事务回滚后，文件可以留下。
pub(crate) fn put(root: &Path, bytes: &[u8], media_type: &str) -> Result<BlobRef, HostError> {
    if media_type.is_empty() {
        return Err(HostError::Invalid("media_type must not be empty"));
    }
    let hex = sha256_hex(bytes);
    let reference = BlobRef::new(format!("sha256:{hex}"));
    let destination = location(root, &hex);
    if destination.is_file() {
        return Ok(reference);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = temp_path(root);
    if let Err(error) = write_temp(&temp, bytes) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    match fs::rename(&temp, &destination) {
        Ok(()) => Ok(reference),
        Err(_) if destination.is_file() => {
            let _ = fs::remove_file(&temp);
            Ok(reference)
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error.into())
        }
    }
}

/// 按引用读回原文。
///
/// **作用**：给 `drive` 还原任务文本，也给调用方核对 blob。
/// **边界**：格式不是 `sha256:` 加 64 位小写 hex 时返回 `Invalid`。文件不存在返回 `NotFound`。
/// P0 不重新计算内容哈希；文件被换掉时，读到的字节可以和引用不一致。
pub(crate) fn read(root: &Path, blob_ref: &BlobRef) -> Result<Vec<u8>, HostError> {
    let path = stored_path(root, blob_ref)?;
    match fs::read(&path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => Err(HostError::NotFound),
        Err(error) => Err(error.into()),
    }
}

/// 已落盘内容的字节数。
///
/// **作用**：核对工具观测里的 `byte_count` 是否等于返回给模型的那份内容。
/// **边界**：引用格式不对，或文件不存在，都是 `Invalid`：这次观测还不能写成 event。磁盘错误仍是 `Io`。
/// P0 只看文件在不在，不核对内容是否仍等于引用里的哈希。
pub(crate) fn stored_len(root: &Path, blob_ref: &BlobRef) -> Result<u64, HostError> {
    let path = stored_path(root, blob_ref)?;
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
        Ok(_) => Err(HostError::Invalid("blob ref")),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            Err(HostError::Invalid("blob is not stored"))
        }
        Err(error) => Err(error.into()),
    }
}

fn stored_path(root: &Path, blob_ref: &BlobRef) -> Result<PathBuf, HostError> {
    Ok(location(root, digest_hex(blob_ref)?))
}

fn digest_hex(blob_ref: &BlobRef) -> Result<&str, HostError> {
    let value = blob_ref.as_str();
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(HostError::Invalid("blob ref"));
    };
    if hex.len() == 64
        && hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        Ok(hex)
    } else {
        Err(HostError::Invalid("blob ref"))
    }
}

fn location(root: &Path, hex: &str) -> PathBuf {
    root.join("blobs")
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(hex)
}

fn temp_path(root: &Path) -> PathBuf {
    let name = format!(
        "{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    root.join("blobs").join("tmp").join(name)
}

fn write_temp(path: &Path, bytes: &[u8]) -> Result<(), HostError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        hex.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
    }
    hex
}
