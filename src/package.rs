use std::collections::HashSet;
use std::io::{Cursor, Read};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::config::{CURRENT_FORMAT_VERSION, MIN_FORMAT_VERSION, PACKAGE_KEY, PACKAGE_MAGIC};
use crate::models::PackageInfo;
use crate::util::truthy;

const MAX_ENTRIES: usize = 32;
const MAX_UNCOMPRESSED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_THEME_JSON_BYTES: u64 = 64 * 1024;

pub fn validate_package(data: &[u8]) -> Result<PackageInfo> {
    if data.len() < 4 + 12 + 16 || !data.starts_with(PACKAGE_MAGIC) {
        bail!("不是有效的 .sukisu 主题包");
    }
    let nonce_bytes = &data[4..16];
    let ciphertext = &data[16..];
    let cipher = Aes256Gcm::new_from_slice(&PACKAGE_KEY)
        .map_err(|_| anyhow::anyhow!("主题包密钥无效"))?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let plain = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("主题包解密或完整性校验失败"))?;

    let cursor = Cursor::new(plain);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|_| anyhow::anyhow!("主题包内容不是有效的 ZIP"))?;
    if archive.len() > MAX_ENTRIES {
        bail!("主题包条目过多");
    }
    let mut names = HashSet::new();
    let mut total_size: u64 = 0;
    for index in 0..archive.len() {
        let file = archive
            .by_index(index)
            .with_context(|| format!("无法读取主题包条目 {index}"))?;
        total_size = total_size.saturating_add(file.size());
        if total_size > MAX_UNCOMPRESSED_BYTES {
            bail!("主题包解压后过大");
        }
        if file.is_file() {
            names.insert(file.name().to_string());
        }
    }
    if !names.contains("theme.json") {
        bail!("主题包缺少 theme.json");
    }

    let mut theme_file = archive
        .by_name("theme.json")
        .map_err(|_| anyhow::anyhow!("主题包缺少 theme.json"))?;
    if theme_file.size() > MAX_THEME_JSON_BYTES {
        bail!("theme.json 过大");
    }
    let mut theme_text = String::new();
    theme_file
        .read_to_string(&mut theme_text)
        .map_err(|_| anyhow::anyhow!("theme.json 不是有效的 JSON"))?;
    let theme: Value = serde_json::from_str(&theme_text)
        .map_err(|_| anyhow::anyhow!("theme.json 不是有效的 JSON"))?;
    let Some(theme) = theme.as_object() else {
        bail!("theme.json 必须是对象");
    };
    let package_version = theme
        .get("version")
        .map(|value| match value {
            Value::Number(number) => number.as_u64().unwrap_or(0) as u32,
            Value::String(text) => text.trim().parse::<u32>().unwrap_or(0),
            _ => 0,
        })
        .unwrap_or(MIN_FORMAT_VERSION);
    if package_version != MIN_FORMAT_VERSION && package_version != CURRENT_FORMAT_VERSION {
        bail!("不支持的主题包版本");
    }
    let has_background = theme.get("hasBackground").map(truthy).unwrap_or(false);
    let has_card_background = theme.get("hasCardBackground").map(truthy).unwrap_or(false);
    if has_background && !names.contains("background") {
        bail!("主题包缺少背景图");
    }
    if has_card_background && !names.contains("card_background") {
        bail!("主题包缺少卡片背景图");
    }
    let stickers: Vec<&String> = names
        .iter()
        .filter(|name| name.starts_with("card_sticker_"))
        .collect();
    if stickers.len() > 5 {
        bail!("主题包贴纸数量过多");
    }
    for name in &stickers {
        let suffix = name.trim_start_matches("card_sticker_");
        if suffix.is_empty() || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
            bail!("主题包贴纸条目无效");
        }
    }
    Ok(PackageInfo {
        deep: true,
        package_version,
        has_background,
        has_card_background,
        sticker_count: stickers.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::Aead;
    use rand::RngCore;
    use zip::write::SimpleFileOptions;

    fn build_package(theme: &Value, extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer.start_file("theme.json", options).unwrap();
            use std::io::Write;
            writer
                .write_all(serde_json::to_string(theme).unwrap().as_bytes())
                .unwrap();
            for (name, data) in extra {
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            writer.finish().unwrap();
        }
        let plain = buffer.into_inner();
        let cipher = Aes256Gcm::new_from_slice(&PACKAGE_KEY).unwrap();
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plain.as_slice())
            .unwrap();
        let mut output = Vec::with_capacity(4 + 12 + ciphertext.len());
        output.extend_from_slice(PACKAGE_MAGIC);
        output.extend_from_slice(&nonce_bytes);
        output.extend_from_slice(&ciphertext);
        output
    }

    #[test]
    fn validates_package() {
        let theme = serde_json::json!({
            "version": 2,
            "hasBackground": true,
            "hasCardBackground": false,
        });
        let package = build_package(&theme, &[("background", b"image")]);
        let info = validate_package(&package).unwrap();
        assert!(info.deep);
        assert_eq!(info.package_version, 2);
        assert!(info.has_background);
        assert_eq!(info.sticker_count, 0);
    }

    #[test]
    fn rejects_tampered_package() {
        let theme = serde_json::json!({"version": 1});
        let mut package = build_package(&theme, &[]);
        let last = package.len() - 1;
        package[last] ^= 0xff;
        assert!(validate_package(&package).is_err());
    }
}
