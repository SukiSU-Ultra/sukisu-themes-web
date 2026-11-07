use std::collections::HashSet;
use std::io::{Cursor, Read};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::config::{CURRENT_FORMAT_VERSION, MIN_FORMAT_VERSION, PACKAGE_KEY, PACKAGE_MAGIC};
use crate::models::PackageInfo;
use crate::util::truthy;

const MAX_ENTRIES: usize = 64;
const MAX_UNCOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_THEME_JSON_BYTES: u64 = 64 * 1024;
const MAX_APPEARANCE_BYTES: usize = 16 * 1024;
const ADAPTIVE_FORMAT_VERSION: u32 = 3;
const APPEARANCE_FORMAT_VERSION: u32 = 4;
const HOME_STATUS_FORMAT_VERSION: u32 = 5;
const MAX_STICKERS_PER_VARIANT: usize = 5;

pub fn validate_package(data: &[u8]) -> Result<PackageInfo> {
    if data.len() < 4 + 12 + 16 || !data.starts_with(PACKAGE_MAGIC) {
        bail!("不是有效的 .sukisu 主题包");
    }
    let nonce_bytes = &data[4..16];
    let ciphertext = &data[16..];
    let cipher =
        Aes256Gcm::new_from_slice(&PACKAGE_KEY).map_err(|_| anyhow::anyhow!("主题包密钥无效"))?;
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
    if !(MIN_FORMAT_VERSION..=CURRENT_FORMAT_VERSION).contains(&package_version) {
        bail!("不支持的主题包版本");
    }

    // v4 adds the static appearance overlay (text colours, palette tokens, image effects).
    // Older managers reject it explicitly instead of silently applying the base theme only.
    if let Some(appearance) = theme.get("appearance").filter(|value| !value.is_null()) {
        if package_version < APPEARANCE_FORMAT_VERSION {
            bail!("自定义外观需要格式 v{APPEARANCE_FORMAT_VERSION}");
        }
        if !appearance.is_object() {
            bail!("appearance 必须是对象");
        }
        validate_appearance(appearance)?;
    }

    let adaptive = theme.get("adaptive").map(truthy).unwrap_or(false);
    if package_version < HOME_STATUS_FORMAT_VERSION && uses_immersive_home(theme) {
        bail!("沉浸式首页需要格式 v{HOME_STATUS_FORMAT_VERSION}");
    }
    if package_version >= HOME_STATUS_FORMAT_VERSION {
        validate_home_status(theme)?;
    }
    if adaptive {
        if package_version < ADAPTIVE_FORMAT_VERSION {
            bail!("自适应主题包需要格式 v{ADAPTIVE_FORMAT_VERSION}");
        }
        let light = theme
            .get("light")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("自适应主题包缺少亮色变体"))?;
        let dark = theme
            .get("dark")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("自适应主题包缺少暗色变体"))?;
        let light_info = validate_variant("亮色", light, &names, "light", package_version)?;
        let dark_info = validate_variant("暗色", dark, &names, "dark", package_version)?;
        return Ok(PackageInfo {
            deep: true,
            package_version,
            has_background: light_info.has_background || dark_info.has_background,
            has_card_background: light_info.has_card_background || dark_info.has_card_background,
            sticker_count: light_info.sticker_count + dark_info.sticker_count,
            adaptive: true,
        });
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
    if stickers.len() > MAX_STICKERS_PER_VARIANT {
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
        adaptive: false,
    })
}

/// v5 adds the immersive home layout and its independent pill/banner sizing. Keep the fields
/// closed and range-checked so the store never serves values the manager would clamp differently.
fn uses_immersive_home(theme: &serde_json::Map<String, Value>) -> bool {
    fn variant_immersive(variant: &serde_json::Map<String, Value>) -> bool {
        variant.get("homeLayout").and_then(Value::as_str) == Some("immersive")
    }

    variant_immersive(theme)
        || theme
            .get("light")
            .and_then(Value::as_object)
            .map(variant_immersive)
            .unwrap_or(false)
        || theme
            .get("dark")
            .and_then(Value::as_object)
            .map(variant_immersive)
            .unwrap_or(false)
}

fn validate_home_status(theme: &serde_json::Map<String, Value>) -> Result<()> {
    if let Some(value) = theme.get("homeLayout").filter(|value| !value.is_null()) {
        let layout = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("homeLayout 必须是字符串"))?;
        if !matches!(layout, "classic" | "immersive") {
            bail!("未知的首页布局");
        }
    }
    if let Some(value) = theme
        .get("homeStatusAlignment")
        .filter(|value| !value.is_null())
    {
        let alignment = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("homeStatusAlignment 必须是字符串"))?;
        if !matches!(alignment, "start" | "end") {
            bail!("未知的状态区对齐");
        }
    }
    if let Some(value) = theme.get("homeStatusScale").filter(|value| !value.is_null()) {
        let number = value
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("homeStatusScale 必须是数字"))?;
        if !(0.5..=3.0).contains(&number) {
            bail!("状态区高度超出范围");
        }
    }
    if let Some(value) = theme.get("homePillScale").filter(|value| !value.is_null()) {
        let number = value
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("homePillScale 必须是数字"))?;
        if !(0.5..=1.5).contains(&number) {
            bail!("胶囊缩放超出范围");
        }
    }
    if let Some(value) = theme
        .get("homeStatusWidthFraction")
        .filter(|value| !value.is_null())
    {
        let number = value
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("homeStatusWidthFraction 必须是数字"))?;
        if !(0.45..=1.0).contains(&number) {
            bail!("状态区宽度超出范围");
        }
    }
    if let Some(value) = theme.get("homeCardImageStyle").filter(|value| !value.is_null()) {
        let style = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("homeCardImageStyle 必须是字符串"))?;
        if !matches!(style, "banner" | "off") {
            bail!("未知的图片位置");
        }
    }
    if let Some(value) = theme
        .get("showHomeCardImageWithoutBackground")
        .filter(|value| !value.is_null())
    {
        if !value.is_boolean() {
            bail!("showHomeCardImageWithoutBackground 必须是布尔值");
        }
    }
    Ok(())
}

/// Validates the static appearance overlay stored in v4+ packages. The client enforces the same
/// closed schema; checking here keeps invalid overlays out of the store.
fn validate_appearance(appearance: &Value) -> Result<()> {
    if serde_json::to_vec(appearance)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
        > MAX_APPEARANCE_BYTES
    {
        bail!("appearance 过大");
    }
    let object = appearance
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("appearance 必须是对象"))?;
    for key in object.keys() {
        if !matches!(key.as_str(), "schema" | "colors" | "image") {
            bail!("未知的外观字段 {key}");
        }
    }
    if let Some(schema) = object.get("schema").filter(|value| !value.is_null()) {
        let version = schema
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("appearance.schema 必须是整数"))?;
        if version != 1 {
            bail!("不支持的外观配置版本");
        }
    }
    if let Some(colors) = object.get("colors").filter(|value| !value.is_null()) {
        let colors = colors
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("appearance.colors 必须是对象"))?;
        for (mode, tokens) in colors {
            if tokens.is_null() {
                continue;
            }
            if !matches!(mode.as_str(), "shared" | "light" | "dark") {
                bail!("未知的外观颜色分组 {mode}");
            }
            let tokens = tokens
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("appearance.colors.{mode} 必须是对象"))?;
            for (token, value) in tokens {
                if value.is_null() {
                    continue;
                }
                if !matches!(
                    token.as_str(),
                    "accent"
                        | "onAccent"
                        | "textPrimary"
                        | "textSecondary"
                        | "textDisabled"
                        | "surface"
                        | "card"
                        | "outline"
                        | "error"
                ) {
                    bail!("未知的颜色字段 appearance.colors.{mode}.{token}");
                }
                let color = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("颜色字段必须是字符串"))?;
                if !is_hex_color(color) {
                    bail!("颜色字段必须是 #RRGGBB 或 #AARRGGBB");
                }
            }
        }
    }
    if let Some(image) = object.get("image").filter(|value| !value.is_null()) {
        let image = image
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("appearance.image 必须是对象"))?;
        for (target, effects) in image {
            if effects.is_null() {
                continue;
            }
            if target.as_str() != "background" {
                bail!("未知的图像效果分组 {target}");
            }
            validate_image_effects(effects)?;
        }
    }
    Ok(())
}

fn validate_image_effects(effects: &Value) -> Result<()> {
    let effects = effects
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("图像效果必须是对象"))?;
    for (key, value) in effects {
        if value.is_null() {
            continue;
        }
        let (min, max) = match key.as_str() {
            "sharpen" => (0.0, 100.0),
            "saturation" | "contrast" => (20.0, 200.0),
            "brightness" => (-100.0, 100.0),
            _ => bail!("未知的图像效果字段 {key}"),
        };
        let number = value
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("图像效果 {key} 必须是数字"))?;
        if !number.is_finite() || number < min || number > max {
            bail!("图像效果 {key} 超出范围");
        }
    }
    Ok(())
}

fn is_hex_color(value: &str) -> bool {
    let Some(hex) = value.strip_prefix('#') else {
        return false;
    };
    (hex.len() == 6 || hex.len() == 8) && hex.chars().all(|ch| ch.is_ascii_hexdigit())
}

struct VariantInfo {
    has_background: bool,
    has_card_background: bool,
    sticker_count: usize,
}

/// Validates one half of an adaptive package. Image entries use the mode suffix, so the same
/// checks as a v2 package apply per variant instead of once for the whole archive.
fn validate_variant(
    label: &str,
    variant: &serde_json::Map<String, Value>,
    names: &HashSet<String>,
    suffix: &str,
    package_version: u32,
) -> Result<VariantInfo> {
    if package_version >= HOME_STATUS_FORMAT_VERSION {
        validate_home_status(variant)?;
    }
    let has_background = variant.get("hasBackground").map(truthy).unwrap_or(false);
    let has_card_background = variant
        .get("hasCardBackground")
        .map(truthy)
        .unwrap_or(false);
    if has_background && !names.contains(&format!("background_{suffix}")) {
        bail!("{label}变体缺少背景图");
    }
    if has_card_background && !names.contains(&format!("card_background_{suffix}")) {
        bail!("{label}变体缺少卡片背景图");
    }
    let prefix = format!("card_sticker_{suffix}_");
    let stickers: Vec<&String> = names
        .iter()
        .filter(|name| name.starts_with(&prefix))
        .collect();
    if stickers.len() > MAX_STICKERS_PER_VARIANT {
        bail!("{label}变体贴纸数量过多");
    }
    for name in &stickers {
        let index = name.trim_start_matches(&prefix);
        if index.is_empty() || !index.chars().all(|ch| ch.is_ascii_digit()) {
            bail!("{label}变体贴纸条目无效");
        }
    }
    Ok(VariantInfo {
        has_background,
        has_card_background,
        sticker_count: stickers.len(),
    })
}
