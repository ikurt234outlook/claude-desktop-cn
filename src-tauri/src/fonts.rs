use crate::{
    operation::{OperationOutcome, OperationState},
    util,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

const IMPORTED_PREFIX: &str = "CN-Imported-";
const CSS_NAME: &str = "claude-cn-font.css";
const FONT_DIR: &str = "fonts";
const STAGE_DIR: &str = "font-stage";

/// A font file the user added, kept in the assistant's data directory and
/// republished into the frontend archive on every apply.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedFont {
    pub id: String,
    /// Family name read from the file, shown in the picker.
    pub label: String,
    pub file: String,
    /// CSS `format()` keyword: woff2, truetype or opentype.
    pub format: String,
}

impl ImportedFont {
    /// Internal CSS family, so a user font never collides with a system family.
    pub fn css_family(&self) -> String {
        format!("{IMPORTED_PREFIX}{}", self.id)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FontSettings {
    /// Interface family: a system family, `import:<id>`, or empty for Claude's default.
    pub ui_family: String,
    /// Chat family.
    pub chat_family: String,
    /// Family used for CJK glyphs.
    pub cjk_family: String,
    pub imported: Vec<ImportedFont>,
}

impl FontSettings {
    pub fn is_default(&self) -> bool {
        self.ui_family.trim().is_empty()
            && self.chat_family.trim().is_empty()
            && self.cjk_family.trim().is_empty()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FontState {
    pub settings: FontSettings,
    pub available: Vec<String>,
    pub applied: bool,
}

fn settings_path() -> Result<PathBuf, String> {
    Ok(util::data_dir()?.join("font-settings.json"))
}

fn fonts_dir() -> Result<PathBuf, String> {
    Ok(util::data_dir()?.join(FONT_DIR))
}

pub fn load_settings() -> Result<FontSettings, String> {
    let path = settings_path()?;
    if !path.exists() {
        return Ok(FontSettings::default());
    }
    let raw = fs::read_to_string(&path).map_err(|error| format!("读取字体设置失败：{error}"))?;
    let settings: FontSettings =
        serde_json::from_str(&raw).map_err(|error| format!("字体设置格式无效：{error}"))?;
    validate(&settings)?;
    Ok(settings)
}

pub fn save_settings(settings: FontSettings) -> Result<FontSettings, String> {
    validate(&settings)?;
    // A family that points at a missing import would silently fall back to the
    // system font, so reject it instead of publishing a broken stylesheet.
    for family in [
        &settings.ui_family,
        &settings.chat_family,
        &settings.cjk_family,
    ] {
        if let Some(id) = family.strip_prefix("import:") {
            if !settings.imported.iter().any(|font| font.id == id) {
                return Err("所选字体已不存在，请重新选择。".into());
            }
        }
    }
    // Keep the style sheet in step with the saved settings so the frontend only
    // has to copy it into the archive.
    if let Ok(dir) = fonts_dir() {
        for font in &settings.imported {
            let path = dir.join(&font.file);
            if !path.is_file() {
                return Err(format!("字体文件缺失：{}", font.file));
            }
        }
    }
    util::write_json(&settings_path()?, &settings)
        .map_err(|error| format!("保存字体设置失败：{error}"))?;
    Ok(settings)
}

fn validate(settings: &FontSettings) -> Result<(), String> {
    for (label, value) in [
        ("界面字体", &settings.ui_family),
        ("聊天字体", &settings.chat_family),
        ("中文字体", &settings.cjk_family),
    ] {
        let value = value.trim();
        if value.len() > 160 {
            return Err(format!("{label}名称过长。"));
        }
        if value.contains(['"', '\'', '\\', ';', '{', '}', '<', '>', '\n', '\r']) {
            return Err(format!("{label}名称包含不允许的字符。"));
        }
    }
    for font in &settings.imported {
        if !font.id.chars().all(|ch| ch.is_ascii_hexdigit())
            || font.file.contains(['/', '\\'])
            || !matches!(font.format.as_str(), "woff2" | "truetype" | "opentype")
        {
            return Err("导入字体记录无效。".into());
        }
    }
    Ok(())
}

/// Families installed for the current user, which is what the frontend can use.
pub fn installed_families() -> Result<Vec<String>, String> {
    let raw = util::powershell(
        "Add-Type -AssemblyName System.Drawing; (New-Object System.Drawing.Text.InstalledFontCollection).Families | ForEach-Object { $_.Name }",
    )?;
    let mut families: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    families.sort_by_key(|name| name.to_lowercase());
    families.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    Ok(families)
}

pub fn state() -> Result<FontState, String> {
    Ok(FontState {
        applied: published(),
        settings: load_settings()?,
        available: installed_families()?,
    })
}

/// True when the frontend archive currently carries the font style sheet. The
/// engine repacks `ion-dist.asar` and removes its unpacked workspace, so the
/// archive index is the only durable evidence.
fn published() -> bool {
    let Some(package) = crate::claude::query_package().ok().flatten() else {
        return false;
    };
    let resources = package.resources();
    let entry = format!("assets/v1/{CSS_NAME}");
    if fs::read_to_string(
        resources
            .join("ion-dist")
            .join("assets")
            .join("v1")
            .join(CSS_NAME),
    )
    .is_ok()
    {
        return true;
    }
    asar_contains(&resources.join("ion-dist.asar"), &entry)
}

/// Walks the archive's JSON index. The first 16 bytes are the pickle header:
/// `[4][headerSize][jsonLength][json…]`.
pub(crate) fn asar_contains(archive: &Path, entry: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = fs::File::open(archive) else {
        return false;
    };
    let mut header = [0u8; 16];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    let length = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    let mut json = vec![0u8; length];
    if file.seek(SeekFrom::Start(16)).is_err() || file.read_exact(&mut json).is_err() {
        return false;
    }
    let Ok(index) = serde_json::from_slice::<serde_json::Value>(&json) else {
        return false;
    };
    let mut node = &index;
    for part in entry.split('/') {
        match node.get("files").and_then(|files| files.get(part)) {
            Some(next) => node = next,
            None => return false,
        }
    }
    true
}

/// Copies a user font into the assistant's data directory and records it.
pub fn import_font(source: &Path) -> Result<ImportedFont, String> {
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_lowercase)
        .filter(|value| matches!(value.as_str(), "ttf" | "otf" | "woff2"))
        .ok_or("仅支持 .ttf、.otf 或 .woff2 字体文件。")?;
    let digest = util::sha256(source)?;
    let id = digest.chars().take(12).collect::<String>();
    let file = format!("{id}.{extension}");
    let dir = fonts_dir()?;
    fs::create_dir_all(&dir).map_err(|error| format!("创建字体目录失败：{error}"))?;
    let destination = dir.join(&file);
    if !destination.is_file() {
        fs::copy(source, &destination).map_err(|error| format!("复制字体文件失败：{error}"))?;
    }
    let label = font_family_name(&destination).unwrap_or_else(|| {
        source
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("导入字体")
            .to_string()
    });
    let font = ImportedFont {
        id,
        label,
        file,
        format: match extension.as_str() {
            "woff2" => "woff2",
            "otf" => "opentype",
            _ => "truetype",
        }
        .into(),
    };
    let mut settings = load_settings()?;
    settings.imported.retain(|item| item.id != font.id);
    settings.imported.push(font.clone());
    save_settings(settings)?;
    Ok(font)
}

/// Reads the family name from a font file; woff2 has no reader here, so callers
/// fall back to the file name.
fn font_family_name(path: &Path) -> Option<String> {
    let script = format!(
        "Add-Type -AssemblyName System.Drawing; $c=New-Object System.Drawing.Text.PrivateFontCollection; $c.AddFontFile({}); $c.Families[0].Name",
        util::ps_path(path)
    );
    util::powershell(&script)
        .ok()
        .filter(|name| !name.is_empty())
}

pub fn remove_font(id: &str) -> Result<FontSettings, String> {
    let mut settings = load_settings()?;
    let removed = settings
        .imported
        .iter()
        .find(|font| font.id == id)
        .cloned()
        .ok_or("字体记录不存在。")?;
    settings.imported.retain(|font| font.id != id);
    let reference = format!("import:{id}");
    for family in [
        &mut settings.ui_family,
        &mut settings.chat_family,
        &mut settings.cjk_family,
    ] {
        if *family == reference {
            family.clear();
        }
    }
    save_settings(settings)?;
    let _ = fs::remove_file(fonts_dir()?.join(&removed.file));
    load_settings()
}

/// Stage path handed to a localization run. Always present so a re-apply keeps
/// the archive in step with the saved settings, including removing the style
/// sheet after the user returned every family to Claude's default.
pub fn stage_for_apply() -> Option<PathBuf> {
    stage(&load_settings().ok()?).ok()
}

/// Stages the style sheet and font files that the engine copies into the
/// frontend archive. An empty stage means "restore the default fonts".
pub fn stage(settings: &FontSettings) -> Result<PathBuf, String> {
    let data = util::data_dir()?;
    let root = data.join(STAGE_DIR);
    let _ = fs::remove_dir_all(&root);
    let stage = root.join("current");
    fs::create_dir_all(stage.join(FONT_DIR))
        .map_err(|error| format!("创建字体工作目录失败：{error}"))?;
    let css = stylesheet(settings);
    if css.is_empty() {
        return Ok(stage);
    }
    fs::write(stage.join(CSS_NAME), css).map_err(|error| format!("写入字体样式失败：{error}"))?;
    let source = fonts_dir()?;
    for font in &settings.imported {
        if !used(settings, font) {
            continue;
        }
        fs::copy(
            source.join(&font.file),
            stage.join(FONT_DIR).join(&font.file),
        )
        .map_err(|error| format!("准备字体文件失败：{error}"))?;
    }
    Ok(stage)
}

fn used(settings: &FontSettings, font: &ImportedFont) -> bool {
    let family = format!("import:{}", font.id);
    settings.ui_family == family || settings.chat_family == family || settings.cjk_family == family
}

/// The published style sheet. Empty when every family is Claude's default.
pub fn stylesheet(settings: &FontSettings) -> String {
    let ui = resolve(&settings.ui_family, settings);
    let chat = resolve(&settings.chat_family, settings);
    let cjk = resolve(&settings.cjk_family, settings);
    if ui.is_none() && chat.is_none() && cjk.is_none() {
        return String::new();
    }

    let mut css = String::from("/* Generated by Claude 中文助手. Do not edit. */\n");
    for font in &settings.imported {
        if !used(settings, font) {
            continue;
        }
        css.push_str(&format!(
            "@font-face{{font-family:\"{family}\";src:url(\"/assets/v1/fonts/{file}\") format(\"{format}\");font-display:swap}}\n",
            family = font.css_family(),
            file = font.file,
            format = font.format,
        ));
    }
    css.push_str(":root{\n");
    if let Some(family) = &ui {
        // Claude reads these through --font-ui, so redefine the whole chain and
        // keep its fallbacks for glyphs the chosen family lacks.
        for variable in [
            "--font-anthropic-sans",
            "--font-ui",
            "--font-sans-serif",
            "--cds-font-sans",
            "--cds-font-system",
            "--font-user-message",
        ] {
            css.push_str(&format!(
                "  {variable}:{family},var(--font-sans-fallbacks) !important;\n"
            ));
        }
    }
    if let Some(family) = &chat {
        for variable in [
            "--font-anthropic-serif",
            "--font-claude-response",
            "--font-serif",
            "--font-ui-serif",
        ] {
            css.push_str(&format!(
                "  {variable}:{family},var(--font-cjk-serif-fallback),Georgia,serif !important;\n"
            ));
        }
    }
    if let Some(family) = &cjk {
        css.push_str(&format!(
            "  --font-cjk-sans-sc:{family},\"Microsoft YaHei UI\",\"Microsoft YaHei\" !important;\n"
        ));
        css.push_str(&format!(
            "  --font-cjk-serif-sc:{family},\"Microsoft YaHei UI\",\"Microsoft YaHei\" !important;\n"
        ));
    }
    css.push_str("}\n");
    css
}

fn resolve(value: &str, settings: &FontSettings) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(id) = value.strip_prefix("import:") {
        return settings
            .imported
            .iter()
            .find(|font| font.id == id)
            .map(|font| format!("\"{}\"", font.css_family()));
    }
    Some(format!("\"{value}\""))
}

/// Applies the saved fonts through the engine's own archive handling, so the
/// repack and the Claude.exe hash sync stay in one place.
pub fn apply(operation: &OperationState) -> Result<OperationOutcome, String> {
    let settings = load_settings()?;
    let restore = settings.is_default();
    run(operation, &settings, restore)?;
    Ok(OperationOutcome::done(if restore {
        "字体已恢复为 Claude 默认。"
    } else {
        "界面与聊天字体已应用。"
    }))
}

pub fn restore(operation: &OperationState) -> Result<OperationOutcome, String> {
    let empty = FontSettings::default();
    run(operation, &empty, true)?;
    save_settings(FontSettings::default())?;
    Ok(OperationOutcome::done("字体已恢复为 Claude 默认。"))
}

fn run(operation: &OperationState, settings: &FontSettings, restore: bool) -> Result<(), String> {
    let stage = stage(settings)?;
    let package = crate::claude::query_package()?.ok_or("未安装 Claude Desktop。")?;
    operation.step(if restore {
        "恢复默认字体"
    } else {
        "应用界面与聊天字体"
    });
    crate::patch::run_font_engine(&stage, &package, operation)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(ui: &str, chat: &str) -> FontSettings {
        FontSettings {
            ui_family: ui.into(),
            chat_family: chat.into(),
            ..FontSettings::default()
        }
    }

    #[test]
    fn default_settings_publish_nothing() {
        assert!(stylesheet(&FontSettings::default()).is_empty());
        assert!(stylesheet(&settings("  ", " ")).is_empty());
    }

    #[test]
    fn asar_index_reports_published_entry() {
        let root = std::env::temp_dir().join(format!("font-asar-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let archive = root.join("ion-dist.asar");
        let index = serde_json::json!({
            "files": {
                "index.html": {"size": 1, "offset": "0"},
                "assets": {"files": {"v1": {"files": {
                    "claude-cn-font.css": {"size": 1, "offset": "1"}
                }}}}
            }
        });
        let mut json = serde_json::to_vec(&index).unwrap();
        let padding = (4 - json.len() % 4) % 4;
        json.extend(vec![b' '; padding]);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&((json.len() + 4) as u32).to_le_bytes());
        bytes.extend_from_slice(&(json.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(index.to_string().len() as u32).to_le_bytes());
        bytes.extend_from_slice(&json);
        fs::write(&archive, bytes).unwrap();

        assert!(asar_contains(&archive, "assets/v1/claude-cn-font.css"));
        assert!(!asar_contains(&archive, "assets/v1/missing.css"));
        assert!(!asar_contains(&archive, "assets/v1/missing.css"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ui_family_redefines_the_whole_sans_chain() {
        let css = stylesheet(&settings("LXGW WenKai", ""));
        for variable in ["--font-ui", "--font-anthropic-sans", "--cds-font-sans"] {
            assert!(
                css.contains(&format!(
                    "{variable}:\"LXGW WenKai\",var(--font-sans-fallbacks)"
                )),
                "{variable} missing from {css}"
            );
        }
        assert!(css.contains("!important"));
        assert!(!css.contains("--font-claude-response"));
    }

    #[test]
    fn chat_family_redefines_the_response_chain() {
        let css = stylesheet(&settings("", "Songti SC"));
        assert!(css.contains("--font-claude-response:\"Songti SC\""));
        assert!(css.contains("--font-serif:\"Songti SC\""));
        assert!(!css.contains("--font-ui:"));
    }

    #[test]
    fn imported_fonts_use_a_namespaced_family() {
        let font = ImportedFont {
            id: "a1b2c3d4e5f6".into(),
            label: "My Sans".into(),
            file: "a1b2c3d4e5f6.woff2".into(),
            format: "woff2".into(),
        };
        let mut value = settings("import:a1b2c3d4e5f6", "");
        value.imported.push(font);
        let css = stylesheet(&value);
        assert!(css.contains("@font-face{font-family:\"CN-Imported-a1b2c3d4e5f6\";src:url(\"/assets/v1/fonts/a1b2c3d4e5f6.woff2\") format(\"woff2\");font-display:swap}"));
        assert!(css.contains("--font-ui:\"CN-Imported-a1b2c3d4e5f6\""));
    }

    #[test]
    fn unused_imports_are_not_published() {
        let mut value = settings("", "");
        value.imported.push(ImportedFont {
            id: "0011223344ff".into(),
            label: "Unused".into(),
            file: "0011223344ff.ttf".into(),
            format: "truetype".into(),
        });
        assert!(stylesheet(&value).is_empty());
    }

    #[test]
    fn family_names_cannot_escape_the_declaration() {
        let mut value = settings("Bad\"Font;}", "");
        value.ui_family = value.ui_family.trim().to_string();
        assert!(validate(&value).is_err());
        let mut ok = settings("Microsoft YaHei", "");
        ok.ui_family = ok.ui_family.trim().to_string();
        assert!(validate(&ok).is_ok());
    }
}
