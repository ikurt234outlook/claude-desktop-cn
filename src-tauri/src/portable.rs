use crate::{
    claude::{official_executable_check_script, ClaudePackage, ClaudeTargetKind},
    operation::{OperationOutcome, OperationState},
    util::{self, ps_path, ps_quote},
};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    cmp::Ordering,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Download sources, tried in this order until one validates. The GitHub mirror
/// is first: the agentsmirror endpoint still answers but serves an older build
/// (4.29.0 while the current release is 4.4212.0), and the official redirect
/// host is unreachable from the mainland. Every source is verified against the
/// Anthropic MSIX signature before it is used.
const GITHUB_URL: &str =
    "https://github.com/Wangnov/claude-app-mirror/releases/latest/download/Claude-win-x64.msix";
const MIRROR_URL: &str = "https://claudeapp.agentsmirror.com/latest/win-x64";
/// Anthropic's alternate feed host; api.anthropic.com answers 403 in many networks.
const OFFICIAL_URL: &str = "https://releases.claude.com/api/desktop/win32/x64/msix/latest/redirect";
const MSIX_DIR: &str = "portable-msix";
const EXTRACT_DIR: &str = "portable-extract";
const RECORD: &str = "claude-portable.json";
const MIN_MSIX_BYTES: u64 = 1024 * 1024;
/// The two staging directories live inside the target root so every swap is a
/// same-volume rename. `.update-new` holds the unpacked release, `.update-old`
/// the files it replaced until the caller commits.
const STAGE_NEW: &str = ".update-new";
const STAGE_OLD: &str = ".update-old";
/// Never swapped: the user's data and launcher, the two staging directories, and
/// the metadata folder Windows maintains for a registered loose package.
const PROTECTED: [&str; 5] = [
    "data",
    "Claude.cmd",
    STAGE_NEW,
    STAGE_OLD,
    "microsoft.system.package.metadata",
];
/// Windows reports a file still in use as access denied or sharing violation.
const BUSY_CODES: [i32; 2] = [5, 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    Mirror,
    GitHub,
    Official,
}

impl Source {
    fn from_value(value: &str) -> Self {
        match value {
            "mirror" => Self::Mirror,
            "official" => Self::Official,
            _ => Self::GitHub,
        }
    }

    fn url(self) -> &'static str {
        match self {
            Self::Mirror => MIRROR_URL,
            Self::GitHub => GITHUB_URL,
            Self::Official => OFFICIAL_URL,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Mirror => "国内镜像",
            Self::GitHub => "GitHub",
            Self::Official => "Anthropic 官方",
        }
    }

    /// Falls back to the next source instead of failing the install outright.
    fn next(self) -> Option<Self> {
        match self {
            Self::GitHub => Some(Self::Mirror),
            Self::Mirror => Some(Self::Official),
            Self::Official => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableRecord {
    pub install_location: String,
    pub version: String,
    /// Whether the copy is Appx-registered for Cowork.
    pub registered: bool,
    pub package_family_name: String,
}

fn record_path() -> Result<PathBuf, String> {
    Ok(util::data_dir()?.join(RECORD))
}

/// The portable install this assistant manages, if its files are still present.
pub fn query() -> Result<Option<ClaudePackage>, String> {
    let path = record_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(&path).map_err(|error| format!("读取便携安装记录失败：{error}"))?;
    let record: PortableRecord =
        serde_json::from_str(&raw).map_err(|error| format!("便携安装记录无效：{error}"))?;
    let package = ClaudePackage {
        package_full_name: record.install_location.clone(),
        package_family_name: record.package_family_name,
        version: record.version,
        architecture: "X64".into(),
        install_location: record.install_location,
        kind: ClaudeTargetKind::Portable,
    };
    // A folder the user deleted or moved is not an install any more; report none
    // so the UI offers a fresh install instead of failing every operation.
    if !package.exe().is_file() {
        return Ok(None);
    }
    Ok(Some(package))
}

pub fn record() -> Result<Option<PortableRecord>, String> {
    let path = record_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|error| format!("便携安装记录无效：{error}"))
}

pub fn save_record(record: &PortableRecord) -> Result<(), String> {
    util::write_json(&record_path()?, record)
        .map_err(|error| format!("保存便携安装记录失败：{error}"))
}

/// Applies the new release to the portable record once the files and the Appx
/// registration both succeeded. A caller that fails earlier keeps the old
/// record, which still describes the files that are on disk.
pub fn apply_update(record: &mut PortableRecord, version: &str, family: &str) {
    record.version = version.to_string();
    record.registered = !family.trim().is_empty();
    record.package_family_name = family.to_string();
}

/// Whether the loose copy in this folder is currently Appx-registered. A failed
/// query is an error: reporting "not registered" would silently skip the
/// re-registration an upgrade needs.
pub fn is_registered(root: &Path) -> Result<bool, String> {
    Ok(registration(root)?.is_some())
}

/// Entry point for the operation worker. The UI packs the target directory,
/// source and registration choice as `target|source|register`.
pub fn install_target(
    options: &str,
    operation: &OperationState,
) -> Result<OperationOutcome, String> {
    let mut parts = options.split('|');
    let target = parts.next().unwrap_or_default();
    let source = parts.next().unwrap_or("mirror");
    let register = parts.next().unwrap_or("0") == "1";
    install(Path::new(target), source, register, operation)
}

pub fn install(
    target: &Path,
    source: &str,
    register: bool,
    operation: &OperationState,
) -> Result<OperationOutcome, String> {
    let (target, existing) = resolve_target(target)?;
    if existing {
        // A moved or forgotten copy: keep its files and re-establish management
        // instead of downloading the package again.
        operation.step("接管已有的便携安装");
        let version = loose_version(&target)?;
        write_launcher(&target)?;
        // Re-registration is only meaningful for an unregistered copy; a
        // registered one already carries the package identity it needs.
        let family = match registration(&target)? {
            Some((family, _)) => family,
            None if register => register_appx(&target, &loose_identity(&version), operation)?,
            None => String::new(),
        };
        let record = PortableRecord {
            install_location: target.display().to_string(),
            version,
            registered: !family.is_empty(),
            package_family_name: family,
        };
        save_record(&record)?;
        return Ok(OperationOutcome::done(format!(
            "已接管 {} 处的便携版 Claude。",
            record.install_location
        )));
    }
    let data = util::data_dir()?;
    let cache = data.join(MSIX_DIR);
    fs::create_dir_all(&cache).map_err(|error| format!("创建下载目录失败：{error}"))?;

    operation.step("选择下载源");
    let mut source = Source::from_value(source);
    let mut last_error = String::new();
    let mut package = None;
    while package.is_none() {
        operation.step(&format!("下载 Claude MSIX（{}）", source.label()));
        match download_and_verify(source, &cache, operation) {
            Ok(path) => package = Some(path),
            Err(error) => {
                operation.log(format!("{}下载失败：{error}", source.label()));
                last_error = error;
                match source.next() {
                    Some(next) => source = next,
                    None => break,
                }
            }
        }
    }
    let (msix, identity) = package.ok_or_else(|| format!("所有下载源均失败：{last_error}"))?;
    operation.log(format!(
        "已校验 {} {}（{}）",
        identity.name,
        identity.version,
        source.label()
    ));

    operation.step("解压 MSIX 到临时目录");
    let extract = data.join(EXTRACT_DIR).join(format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis()
    ));
    expand(&msix, &extract, operation)?;
    let result: Result<PortableRecord, String> = (|| {
        operation.step("复制应用文件到目标目录");
        copy_tree(&extract, &target)?;
        write_launcher(&target)?;
        let record = PortableRecord {
            install_location: target.display().to_string(),
            version: identity.version.clone(),
            registered: false,
            package_family_name: String::new(),
        };
        save_record(&record)?;
        Ok(record)
    })();
    let _ = fs::remove_dir_all(&extract);
    let mut record = result?;

    if register {
        operation.step("注册 AppX 与 Cowork 服务");
        match register_appx(&target, &identity, operation) {
            Ok(family) => {
                record.registered = true;
                record.package_family_name = family;
                save_record(&record)?;
            }
            Err(error) => {
                // The files are installed and usable without Cowork; keep them.
                operation.log(format!("AppX 注册失败：{error}"));
                return Err(format!(
                    "应用文件已安装到 {}，但 AppX/Cowork 注册失败：{error}。可直接运行 Claude.cmd。",
                    target.display()
                ));
            }
        }
    }

    Ok(OperationOutcome::done(format!(
        "Claude Desktop {} 已安装到 {}。",
        record.version, record.install_location
    )))
}

/// Windows tools such as Add-AppxPackage reject a manifest path that mixes
/// separators, so the target is normalised before any use.
fn normalize_path(text: &str) -> PathBuf {
    let trimmed = text.trim().trim_end_matches(['\\', '/']);
    PathBuf::from(if cfg!(windows) {
        trimmed.replace('/', "\\")
    } else {
        trimmed.to_string()
    })
}

/// Returns the target directory and whether it already holds a portable copy
/// that should be taken over instead of downloaded again.
fn resolve_target(path: &Path) -> Result<(PathBuf, bool), String> {
    let trimmed = path.to_string_lossy().trim().to_string();
    if trimmed.is_empty() {
        return Err("请选择便携安装目录。".into());
    }
    let target = normalize_path(&trimmed);
    if !target.is_absolute() {
        return Err("便携安装目录必须是绝对路径。".into());
    }
    if target.parent().is_none() {
        return Err("不能安装到盘符根目录，请选择子目录。".into());
    }
    if target
        .components()
        .any(|component| component.as_os_str().eq_ignore_ascii_case("WindowsApps"))
    {
        return Err("不能使用 WindowsApps 目录，请选择其他位置。".into());
    }
    if target.exists() {
        // Only an empty folder or a previous portable install may be reused.
        let has_files = fs::read_dir(&target)
            .map_err(|error| format!("读取目标目录失败：{error}"))?
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() != "data");
        let managed = target.join("app").join("Claude.exe").is_file();
        if has_files && !managed {
            return Err("目标目录已有其他文件，请选择空目录。".into());
        }
        return Ok((target, managed));
    }
    Ok((target, false))
}

/// Version from the loose `AppxManifest.xml` shipped inside a portable copy.
fn loose_version(root: &Path) -> Result<String, String> {
    let manifest = root.join("AppxManifest.xml");
    if !manifest.is_file() {
        return Err("目标目录缺少 AppxManifest.xml，无法确认版本。".into());
    }
    let identity = util::powershell(&format!(
        "[xml]$m=Get-Content -LiteralPath {} -Raw -Encoding UTF8; $m.Package.Identity.Name + '|' + $m.Package.Identity.Version",
        ps_path(&manifest)
    ))?;
    let (name, version) = identity.trim().split_once('|').unwrap_or(("", ""));
    if name != "Claude" {
        return Err("该目录不是 Claude 便携版。".into());
    }
    if version.is_empty() {
        return Err("AppxManifest.xml 没有可识别的版本。".into());
    }
    Ok(version.to_string())
}

/// Identity fields `register_appx` needs when no package was downloaded.
fn loose_identity(version: &str) -> MsixIdentity {
    MsixIdentity {
        name: "Claude".into(),
        version: version.into(),
        architecture: "x64".into(),
        publisher: String::new(),
        executable: String::new(),
    }
}

/// Family name and version of the Appx package Windows has registered from this
/// folder, if any. The version is read from the registration rather than the
/// recorded install, because the two disagree while an update is in progress.
fn registration(root: &Path) -> Result<Option<(String, String)>, String> {
    let raw = util::powershell(&format!(
        "$package=@(Get-AppxPackage -Name Claude -ErrorAction SilentlyContinue | Where-Object {{ $_.InstallLocation -and $_.InstallLocation.TrimEnd('\\') -ieq {} }}); if ($package.Count -ge 1) {{ $package[0].PackageFamilyName + '|' + $package[0].Version }}",
        ps_quote(root.to_string_lossy().trim_end_matches('\\'))
    ))?;
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let (family, version) = raw
        .split_once('|')
        .ok_or_else(|| "Appx 注册状态无法解析。".to_string())?;
    if family.trim().is_empty() {
        return Err("Appx 注册没有返回包族名。".into());
    }
    Ok(Some((
        family.trim().to_string(),
        version.trim().to_string(),
    )))
}

/// The version Windows currently has registered for this folder.
pub fn registered_version(root: &Path) -> Result<Option<String>, String> {
    Ok(registration(root)?.map(|(_, version)| version))
}

/// Puts the Appx registration back on the version the files now are, after a
/// rolled-back upgrade. Windows keeps the release it was last given, and with
/// the new files gone that registration would describe a version that is not
/// there. Nothing to do when the folder is unregistered or already agrees.
pub fn restore_registration(root: &Path, operation: &OperationState) -> Result<(), String> {
    let files = loose_version(root)?;
    let Some(registered) = registered_version(root)? else {
        return Ok(());
    };
    if util::compare_versions(&registered, &files) == Ordering::Equal {
        return Ok(());
    }
    operation.log(format!("把 AppX 注册从 {registered} 还原到 {files}",));
    register_appx(root, &loose_identity(&files), operation).map(|_| ())
}

/// An in-place upgrade: the release is unpacked beside the live copy, then each
/// top-level entry is swapped by rename. The replaced files are kept until the
/// caller commits, so a failure at any point can put the copy back.
pub struct StagedUpdate {
    root: PathBuf,
    identity: MsixIdentity,
    patience: Duration,
    restored: Cell<bool>,
    /// The names the current swap considered, so a rollback touches nothing else.
    swapped: RefCell<Vec<String>>,
}

impl StagedUpdate {
    /// Unpacks and validates the package next to the running copy. Nothing in the
    /// live installation changes here, so this runs before Claude is closed.
    pub fn stage(
        root: &Path,
        msix: &Path,
        expected_version: &str,
        operation: &OperationState,
    ) -> Result<Self, String> {
        let identity = read_identity(msix)?;
        if util::compare_versions(&identity.version, expected_version) != Ordering::Equal {
            return Err(format!(
                "下载的 MSIX 版本为 {}，与官方版本 {expected_version} 不一致。",
                identity.version
            ));
        }
        let update = Self {
            root: root.to_path_buf(),
            identity,
            patience: Duration::from_secs(15),
            restored: Cell::new(false),
            swapped: RefCell::new(Vec::new()),
        };
        // Only package files ever live here, and a stale one would be swapped in.
        for error in update
            .remove(&update.staged_new())
            .into_iter()
            .chain(update.remove(&update.staged_old()))
        {
            operation.log(error);
        }
        expand(msix, &update.staged_new(), operation)?;
        let staged = update.staged_new().join("app").join("Claude.exe");
        if !util::powershell(&official_executable_check_script(&staged))?
            .eq_ignore_ascii_case("Valid Anthropic")
        {
            update.discard();
            return Err("新版本 Claude.exe 的 Anthropic 签名校验失败。".into());
        }
        Ok(update)
    }

    pub fn identity(&self) -> &MsixIdentity {
        &self.identity
    }

    fn staged_new(&self) -> PathBuf {
        self.root.join(STAGE_NEW)
    }

    fn staged_old(&self) -> PathBuf {
        self.root.join(STAGE_OLD)
    }

    pub fn remove(&self, dir: &Path) -> Vec<String> {
        if !dir.exists() {
            return Vec::new();
        }
        // The staged release and its backup set both come from packages, so a
        // reparse point in either is never expected and never followed.
        util::remove_entry(dir).err().into_iter().collect()
    }

    /// Renames `from` to `to`, waiting out a file that is still being released.
    fn rename(&self, from: &Path, to: &Path) -> Result<(), String> {
        let deadline = Instant::now() + self.patience;
        loop {
            match fs::rename(from, to) {
                Ok(()) => return Ok(()),
                Err(error)
                    if !error
                        .raw_os_error()
                        .is_some_and(|code| BUSY_CODES.contains(&code))
                        || Instant::now() >= deadline =>
                {
                    return Err(format!(
                        "替换 {} 失败：{error}",
                        from.file_name().unwrap_or_default().to_string_lossy()
                    ))
                }
                Err(_) => std::thread::sleep(Duration::from_millis(250)),
            }
        }
    }

    /// Every top-level name to consider: the staged release's entries first, then
    /// the previous version's leftovers, minus what must survive the upgrade.
    fn swap_names(&self) -> Result<Vec<String>, String> {
        let mut names: Vec<String> = Vec::new();
        let push = |name: String, names: &mut Vec<String>| {
            if !PROTECTED
                .iter()
                .any(|item| item.eq_ignore_ascii_case(&name))
            {
                names.push(name);
            }
        };
        for dir in [self.staged_new(), self.root.clone()] {
            for entry in fs::read_dir(&dir)
                .map_err(|error| format!("读取 {} 失败：{error}", dir.display()))?
                .flatten()
            {
                let name = entry.file_name().to_string_lossy().to_string();
                if !names.contains(&name) {
                    push(name, &mut names);
                }
            }
        }
        Ok(names)
    }

    pub fn swap(&self) -> Result<(), String> {
        let names = self.swap_names()?;
        *self.swapped.borrow_mut() = names.clone();
        fs::create_dir_all(self.staged_old())
            .map_err(|error| format!("创建升级备份目录失败：{error}"))?;
        let result: Result<(), String> = (|| {
            for name in &names {
                let live = self.root.join(name);
                if live.exists() {
                    self.rename(&live, &self.staged_old().join(name))?;
                }
                let staged = self.staged_new().join(name);
                if staged.exists() {
                    self.rename(&staged, &live)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            return Err(match self.restore() {
                Ok(()) => format!("{error}；请关闭 Claude Desktop 后重试。"),
                Err(restore) => format!("{error}；回滚也失败：{restore}"),
            });
        }
        Ok(())
    }

    /// Puts every entry back the way `swap` found it, touching only the names
    /// the swap itself considered: a directory the user created inside the
    /// installation is never moved. A name the old version never had returns to
    /// the staging folder instead of the installation.
    pub fn restore(&self) -> Result<(), String> {
        let (old, new) = (self.staged_old(), self.staged_new());
        if !old.exists() {
            return Ok(());
        }
        let mut errors = Vec::new();
        for name in self.swapped.borrow().iter() {
            let (live, previous, staged) = (self.root.join(name), old.join(name), new.join(name));
            let result: Result<(), String> = (|| {
                if !previous.exists() {
                    // A name the new release introduced: it goes back to staging.
                    if live.exists() && !staged.exists() {
                        fs::rename(&live, &staged)
                            .map_err(|error| format!("回收 {name} 失败：{error}"))?;
                    }
                    return Ok(());
                }
                if live.exists() {
                    fs::rename(&live, &staged)
                        .map_err(|error| format!("回收 {name} 失败：{error}"))?;
                }
                fs::rename(&previous, &live).map_err(|error| format!("还原 {name} 失败：{error}"))
            })();
            errors.extend(result.err());
        }
        if errors.is_empty() {
            self.restored.set(true);
            Ok(())
        } else {
            Err(errors.join("；"))
        }
    }

    /// Best-effort cleanup once the swap is committed, or of everything the
    /// release staged when the installation was rolled back. The installation is
    /// already usable either way, so problems are only reported.
    pub fn finish(&self) -> Vec<String> {
        self.remove(&self.staged_new())
            .into_iter()
            .chain(self.remove(&self.staged_old()))
            .collect()
    }

    /// Drops the staged release without touching the installation.
    pub fn discard(&self) -> Vec<String> {
        self.remove(&self.staged_new())
    }
}

fn download_and_verify(
    source: Source,
    cache: &Path,
    operation: &OperationState,
) -> Result<(PathBuf, MsixIdentity), String> {
    let partial = cache.join(format!("download-{}.msix.partial", source.label()));
    let url = resolve_url(source.url())?;
    operation.set_cancelable(true);
    let result = (|| {
        download(&url, &partial, operation)?;
        let size = fs::metadata(&partial)
            .map_err(|error| format!("读取下载文件失败：{error}"))?
            .len();
        if size < MIN_MSIX_BYTES {
            return Err("下载的文件过小，可能不是完整的 MSIX。".into());
        }
        verify_signature(&partial)?;
        let identity = read_identity(&partial)?;
        let final_path = cache.join(format!("Claude-{}.msix", identity.version));
        util::replace_file(&partial, &final_path)
            .map_err(|error| format!("保存 MSIX 失败：{error}"))?;
        Ok((final_path, identity))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

/// The official endpoint answers with a redirect to a versioned CDN URL.
fn resolve_url(url: &str) -> Result<String, String> {
    if url != OFFICIAL_URL {
        return Ok(url.to_string());
    }
    let script = format!(
        "$request=[System.Net.HttpWebRequest]::Create({}); $request.AllowAutoRedirect=$false; $request.Timeout=60000; try {{ $response=$request.GetResponse() }} catch [System.Net.WebException] {{ $response=$_.Exception.Response; if (-not $response) {{ throw }} }}; try {{ $location=$response.Headers['Location']; if (-not $location) {{ throw '官方接口没有返回下载地址。' }}; $location }} finally {{ $response.Close() }}",
        ps_quote(url)
    );
    let location = util::powershell(&script)?;
    let location = if location.starts_with('/') {
        format!("https://downloads.claude.ai{location}")
    } else {
        location
    };
    if !location.starts_with("https://") {
        return Err("官方下载地址不符合预期。".into());
    }
    Ok(location)
}

fn download(url: &str, destination: &Path, operation: &OperationState) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("下载地址必须使用 HTTPS。".into());
    }
    // A 200 MB MSIX needs a generous total budget; connect_timeout only guards
    // the handshake, so a slow mirror is not mistaken for a dead one.
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(30 * 60))
        .build()
        .map_err(|error| format!("创建下载请求失败：{error}"))?;
    operation.log(format!("请求 {url}"));
    let mut response = client
        .get(url)
        .send()
        .map_err(|error| format!("下载失败：{}", error.without_url()))?
        .error_for_status()
        .map_err(|error| format!("下载请求失败：{}", error.without_url()))?;
    let total = response.content_length();
    let mut file =
        fs::File::create(destination).map_err(|error| format!("创建下载文件失败：{error}"))?;
    let mut downloaded = 0u64;
    let mut last_report = Instant::now();
    let mut buffer = [0u8; 128 * 1024];
    operation.download_progress(0, total);
    loop {
        let count = response
            .read(&mut buffer)
            .map_err(|error| format!("读取下载数据失败：{error}"))?;
        if count == 0 {
            break;
        }
        file.write_all(&buffer[..count])
            .map_err(|error| format!("写入下载文件失败：{error}"))?;
        downloaded += count as u64;
        if last_report.elapsed() >= Duration::from_millis(250) {
            operation.download_progress(downloaded, total);
            operation.cancelled()?;
            last_report = Instant::now();
        }
    }
    operation.download_progress(downloaded, total);
    if downloaded == 0 || total.is_some_and(|total| downloaded != total) {
        return Err(format!(
            "下载不完整：收到 {downloaded} 字节，预期 {} 字节。",
            total.map_or("未知".into(), |value| value.to_string())
        ));
    }
    file.sync_all()
        .map_err(|error| format!("写入下载文件失败：{error}"))?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MsixIdentity {
    pub name: String,
    pub version: String,
    pub architecture: String,
    pub publisher: String,
    pub executable: String,
}

fn read_identity(msix: &Path) -> Result<MsixIdentity, String> {
    let script = format!(
        r#"Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$zip=[System.IO.Compression.ZipFile]::OpenRead({path})
try {{
    $entry=$zip.GetEntry('AppxManifest.xml')
    if ($null -eq $entry) {{ throw 'MSIX 缺少 AppxManifest.xml。' }}
    $reader=New-Object System.IO.StreamReader($entry.Open())
    try {{ [xml]$manifest=$reader.ReadToEnd() }} finally {{ $reader.Dispose() }}
}} finally {{ $zip.Dispose() }}
$identity=$manifest.Package.Identity
# 4.29 ships four Application entries (Desktop plus SSH helpers); take the one
# whose executable is the desktop entry point.
$application=@($manifest.Package.Applications.Application | Where-Object {{ $_.Executable -ieq 'app\Claude.exe' }})
if ($application.Count -ne 1) {{ throw 'MSIX 没有唯一的 Claude 启动入口。' }}
$application=$application[0]
[pscustomobject]@{{
    Name=$identity.Name
    Version=$identity.Version
    Architecture=$identity.ProcessorArchitecture
    Publisher=$identity.Publisher
    Executable=$application.Executable
}} | ConvertTo-Json -Compress"#,
        path = ps_path(msix)
    );
    let raw = util::powershell(&script)?;
    let identity: MsixIdentity =
        serde_json::from_str(&raw).map_err(|error| format!("MSIX 清单无法解析：{error}"))?;
    if identity.name != "Claude" {
        return Err(format!("这不是 Claude 安装包：{}", identity.name));
    }
    if !identity.architecture.eq_ignore_ascii_case("x64") {
        return Err(format!("仅支持 x64 包，实际为 {}。", identity.architecture));
    }
    if !identity.publisher.to_lowercase().contains("anthropic") {
        return Err(format!("发布者不是 Anthropic：{}", identity.publisher));
    }
    if !identity.executable.eq_ignore_ascii_case(r"app\Claude.exe") {
        return Err(format!("启动入口不符合预期：{}", identity.executable));
    }
    Ok(identity)
}

/// PowerShell cannot always parse the MSIX PKCX signature; treat a present,
/// non-trivial signature entry as evidence that the container is intact.
fn verify_signature(msix: &Path) -> Result<(), String> {
    let script = format!(
        r#"$sig=Get-AuthenticodeSignature -LiteralPath {path}
if ($sig.Status -eq 'Valid') {{
    if ($sig.SignerCertificate -and $sig.SignerCertificate.Subject -notmatch 'Anthropic') {{ throw 'MSIX 签名发布者不是 Anthropic。' }}
    'valid'
    return
}}
if ($sig.Status -notin @('UnknownError','NotSupported','NotSigned')) {{ throw ('MSIX 签名无效：' + $sig.Status) }}
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip=[System.IO.Compression.ZipFile]::OpenRead({path})
try {{
    $entry=$zip.GetEntry('AppxSignature.p7x')
    if ($null -eq $entry -or $entry.Length -lt 100) {{ throw 'MSIX 缺少有效的签名条目。' }}
}} finally {{ $zip.Dispose() }}
'unverified'"#,
        path = ps_path(msix)
    );
    util::powershell(&script).map(|_| ())
}

fn expand(msix: &Path, destination: &Path, operation: &OperationState) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| format!("创建解压目录失败：{error}"))?;
    // MSIX/OPC stores URI-escaped entry names such as %40ant; the archive
    // extractor is told to decode them so the native module path is real.
    let script = format!(
        r#"Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$zip=[System.IO.Compression.ZipFile]::OpenRead({msix})
$root=[IO.Path]::GetFullPath({root})
$prefix=$root.TrimEnd('\') + '\'
$count=0
try {{
    foreach ($entry in $zip.Entries) {{
        $relative=[Uri]::UnescapeDataString($entry.FullName).Replace('/', '\')
        if ([string]::IsNullOrWhiteSpace($relative)) {{ continue }}
        $target=[IO.Path]::GetFullPath((Join-Path $root $relative))
        if (-not $target.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {{ throw ('MSIX 条目越界：' + $entry.FullName) }}
        if ($relative.EndsWith('\')) {{ [IO.Directory]::CreateDirectory($target) | Out-Null; continue }}
        [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($target)) | Out-Null
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $target, $true)
        $count += 1
    }}
}} finally {{ $zip.Dispose() }}
$count"#,
        msix = ps_path(msix),
        root = ps_path(destination)
    );
    util::powershell(&script)?;
    let native = destination
        .join("app")
        .join("resources")
        .join("app.asar.unpacked")
        .join("node_modules")
        .join("@ant")
        .join("claude-native")
        .join("claude-native-binding.node");
    if !native.is_file() {
        return Err(format!(
            "解压结果缺少 Claude 原生模块：{}。请重试或更换下载源。",
            native.display()
        ));
    }
    operation.log("MSIX 已解压");
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    fs::create_dir_all(target).map_err(|error| format!("创建目标目录失败：{error}"))?;
    let script = format!(
        "$ErrorActionPreference='Stop'; robocopy {} {} /E /COPY:DAT /DCOPY:DAT /R:2 /W:1 /NFL /NDL /NJH /NJS /NP | Out-Null; if ($LASTEXITCODE -ge 8) {{ throw ('robocopy 退出码 ' + $LASTEXITCODE) }}; exit 0",
        ps_path(source),
        ps_path(target)
    );
    util::powershell(&script).map(|_| ())
}

/// Mirrors claude-down's launcher: ASCII with CRLF so cmd.exe parses it
/// regardless of the system code page.
pub fn write_launcher(root: &Path) -> Result<(), String> {
    let text = "@echo off\r\n\
setlocal\r\n\
set \"APP=%~dp0app\\Claude.exe\"\r\n\
if not exist \"%APP%\" (\r\n\
    echo Missing app\\Claude.exe. Reinstall with the assistant.\r\n\
    exit /b 1\r\n\
)\r\n\
set \"USER_DATA=%~dp0data\"\r\n\
pushd \"%~dp0app\" || exit /b 1\r\n\
start \"\" \"%APP%\" --user-data-dir \"%USER_DATA%\" %*\r\n\
set \"EXIT_CODE=%ERRORLEVEL%\"\r\n\
popd\r\n\
exit /b %EXIT_CODE%\r\n";
    fs::write(root.join("Claude.cmd"), text).map_err(|error| format!("写入启动器失败：{error}"))
}

#[derive(Deserialize)]
struct Registration {
    #[serde(rename = "Family")]
    family: String,
    #[serde(rename = "Service")]
    service: bool,
}

/// Registers the loose files as an Appx package and starts its Cowork service.
/// Both steps need administrator rights, so they run in one elevated script.
pub(crate) fn register_appx(
    root: &Path,
    identity: &MsixIdentity,
    operation: &OperationState,
) -> Result<String, String> {
    let manifest = root.join("AppxManifest.xml");
    if !manifest.is_file() {
        return Err("目标目录缺少 AppxManifest.xml。".into());
    }
    if !developer_mode() {
        return Err("需要开启 Windows 开发者模式才能注册便携版 AppX。".into());
    }
    let work = util::data_dir()?.join("portable-register");
    fs::create_dir_all(&work).map_err(|error| format!("创建注册工作目录失败：{error}"))?;
    let result = work.join("register-result.json");
    let _ = fs::remove_file(&result);
    let script = work.join("register-appx.ps1");
    util::write_script(
        &script,
        &format!(
            r#"$ErrorActionPreference='Stop'
Add-AppxPackage -Register {manifest} -ErrorAction Stop
$package=@(Get-AppxPackage -Name Claude | Where-Object {{ $_.InstallLocation -and $_.InstallLocation.TrimEnd('\') -ieq {root} }})
if ($package.Count -ne 1) {{ throw 'AppX 注册后无法确认安装位置。' }}
$expected=[version]{version}
if ([version]$package[0].Version -ne $expected) {{ throw ('AppX 注册后版本为 ' + $package[0].Version + '，预期 ' + $expected + '。') }}
# The service must be restarted so it runs the executable these files now hold:
# a swap or a rollback can leave a running process mapped to a folder that has
# since moved.
$service=Get-CimInstance Win32_Service -Filter "Name='CoworkVMService'" -ErrorAction SilentlyContinue
if ($service -and $service.State -ne 'Stopped') {{
    Stop-Service -Name CoworkVMService -ErrorAction Stop
    $deadline=[DateTime]::UtcNow.AddSeconds(15)
    do {{
        $service=Get-CimInstance Win32_Service -Filter "Name='CoworkVMService'" -ErrorAction Stop
        if (-not $service -or $service.State -eq 'Stopped') {{ break }}
        if ([DateTime]::UtcNow -ge $deadline) {{ throw '等待 CoworkVMService 停止超时。' }}
        Start-Sleep -Milliseconds 250
    }} while ($true)
}}
$service=Get-CimInstance Win32_Service -Filter "Name='CoworkVMService'" -ErrorAction SilentlyContinue
if ($service) {{
    Start-Service -Name CoworkVMService -ErrorAction Stop
    $service=Get-CimInstance Win32_Service -Filter "Name='CoworkVMService'" -ErrorAction SilentlyContinue
}}
[pscustomobject]@{{ Family=$package[0].PackageFamilyName; Service=[bool]($service -and $service.State -eq 'Running') }} | ConvertTo-Json -Compress | Set-Content -LiteralPath {result} -Encoding UTF8"#,
            manifest = ps_path(&manifest),
            root = ps_quote(root.to_string_lossy().trim_end_matches('\\')),
            version = ps_quote(&identity.version),
            result = ps_path(&result),
        ),
    )?;
    operation.log(format!(
        "注册 {} {}，需要管理员授权",
        identity.name, identity.version
    ));
    util::run_elevated_script_with_logs(&script, &work, |line| {
        if !line.trim().is_empty() {
            operation.log(line.to_string());
        }
    })?;
    let raw =
        fs::read_to_string(&result).map_err(|error| format!("AppX 注册没有返回结果：{error}"))?;
    let registration: Registration =
        serde_json::from_str(raw.trim_start_matches('\u{feff}').trim())
            .map_err(|error| format!("AppX 注册结果无效：{error}"))?;
    operation.log(if registration.service {
        "CoworkVMService 已运行"
    } else {
        "未找到 CoworkVMService，Cowork 不可用"
    });
    if registration.family.trim().is_empty() {
        return Err("AppX 注册后没有返回包族名。".into());
    }
    Ok(registration.family.trim().to_string())
}

pub fn clean_cache(data: &Path) -> Vec<String> {
    let mut errors =
        util::prune_versioned_downloads(data, &data.join(MSIX_DIR), "Claude-", ".msix", None);
    for entry in fs::read_dir(data.join(MSIX_DIR))
        .into_iter()
        .flatten()
        .flatten()
    {
        if entry.file_name().to_string_lossy().ends_with(".partial") {
            errors.extend(
                fs::remove_file(entry.path())
                    .err()
                    .map(|error| format!("清理未完成下载失败：{error}")),
            );
        }
    }
    errors.extend(util::keep_newest_entries(
        data,
        &data.join(EXTRACT_DIR),
        |_| true,
        0,
    ));
    errors
}

/// Whether a portable copy can be registered here; drives the UI option.
/// Read natively: this runs during startup, and a PowerShell round trip costs
/// seconds on a cold machine.
pub fn developer_mode() -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD,
    };
    let wide = |text: &str| -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let key = wide(r"SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock");
    let name = wide("AllowDevelopmentWithoutDevLicense");
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut value as *mut u32 as *mut std::ffi::c_void,
            &mut size,
        )
    };
    // A missing value keeps the previous behaviour: only an explicit 0 blocks it.
    status != 0 || value != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_fall_back_in_a_fixed_order() {
        assert_eq!(Source::from_value("mirror"), Source::Mirror);
        assert_eq!(Source::from_value("github"), Source::GitHub);
        assert_eq!(Source::from_value("official"), Source::Official);
        // An unknown or absent choice must land on the current release mirror,
        // not on the stale mainland endpoint.
        assert_eq!(Source::from_value("anything"), Source::GitHub);
        assert_eq!(Source::from_value(""), Source::GitHub);
        let mut source = Source::GitHub;
        let mut seen = Vec::new();
        loop {
            seen.push(source.label());
            match source.next() {
                Some(next) => source = next,
                None => break,
            }
        }
        assert_eq!(seen, ["GitHub", "国内镜像", "Anthropic 官方"]);
    }

    /// Builds the layout `stage` would produce, without downloading or
    /// expanding a package: the named entries become files in both directories.
    fn fixture(name: &str, live: &[&str], staged: &[&str]) -> StagedUpdate {
        let root = std::env::temp_dir().join(format!(
            "claude-portable-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let write = |dir: &Path, names: &[&str]| {
            fs::create_dir_all(dir).unwrap();
            for name in names {
                fs::write(dir.join(name), name.as_bytes()).unwrap();
            }
        };
        write(&root, live);
        write(&root.join(STAGE_NEW), staged);
        StagedUpdate {
            root,
            identity: MsixIdentity {
                name: "Claude".into(),
                version: "4.4212.0".into(),
                architecture: "x64".into(),
                publisher: "CN=Anthropic".into(),
                executable: r"app\Claude.exe".into(),
            },
            patience: Duration::ZERO,
            restored: Cell::new(false),
            swapped: RefCell::new(Vec::new()),
        }
    }

    /// The installation's own entries; the two staging directories are expected
    /// to sit alongside them while an upgrade is in progress.
    fn contents(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != STAGE_NEW && name != STAGE_OLD)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn swap_replaces_package_files_and_keeps_user_state() {
        let update = fixture(
            "swap",
            &["app", "data", "Claude.cmd", "gone.txt"],
            &["app", "assets"],
        );
        update.swap().unwrap();
        assert_eq!(
            contents(&update.root),
            ["Claude.cmd", "app", "assets", "data"]
        );
        // Only the release's own entries move; the previous version's leftovers
        // wait in the backup set for the commit instead of being deleted.
        assert_eq!(contents(&update.staged_old()), ["app", "gone.txt"]);
        assert_eq!(
            fs::read_to_string(update.root.join("assets"))
                .unwrap()
                .trim(),
            "assets"
        );
        update.finish();
        assert_eq!(
            contents(&update.root),
            ["Claude.cmd", "app", "assets", "data"]
        );
        let root = update.root.clone();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restore_puts_back_every_replaced_entry() {
        let update = fixture("restore", &["app", "data", "gone.txt"], &["app", "assets"]);
        let before = contents(&update.root);
        update.swap().unwrap();
        assert_ne!(contents(&update.root), before);
        update.restore().unwrap();
        assert_eq!(contents(&update.root), before);
        // The staged release is intact again and the backup set empty.
        assert_eq!(contents(&update.root.join(STAGE_NEW)), ["app", "assets"]);
        assert!(contents(&update.root.join(STAGE_OLD)).is_empty());
        update.finish();
        assert!(!update.staged_new().exists());
        let root = update.root.clone();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unreplaceable_entry_fails_the_swap_without_touching_the_installation() {
        let update = fixture("blocked", &["app", "data"], &["app"]);
        // A directory where the backup set expects the file: the rename cannot
        // succeed, which is how a file still held open by another process looks.
        fs::create_dir_all(update.staged_old().join("app")).unwrap();
        let error = update.swap().unwrap_err();
        assert!(error.contains("替换 app 失败"), "{error}");
        assert_eq!(contents(&update.staged_new()), ["app"]);
        // `.update-old\app` is the directory the rollback could not replace, and
        // that alone declares the rollback incomplete, so the backup set stays.
        assert_eq!(contents(&update.root), ["app", "data"]);
        let root = update.root.clone();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn target_must_be_an_absolute_subdirectory() {
        assert!(resolve_target(Path::new("Claude")).is_err());
        assert!(resolve_target(Path::new("")).is_err());
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        assert!(resolve_target(Path::new(root)).is_err());
    }

    #[test]
    fn only_an_empty_or_managed_directory_is_reused() {
        let root = std::env::temp_dir().join(format!(
            "claude-portable-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(resolve_target(&root).unwrap().1, false);
        std::fs::write(root.join("notes.txt"), b"x").unwrap();
        assert!(resolve_target(&root).is_err());
        std::fs::remove_file(root.join("notes.txt")).unwrap();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("app").join("Claude.exe"), b"x").unwrap();
        assert_eq!(resolve_target(&root).unwrap().1, true);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn identity_skips_helper_applications_in_the_manifest() {
        // Claude 4.29 declares the desktop app plus three SSH helpers; reading
        // `Application.Executable` without a filter yields an array.
        let root = std::env::temp_dir().join(format!(
            "claude-portable-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let manifest = root.join("AppxManifest.xml");
        std::fs::write(
            &manifest,
            r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10">
  <Identity Name="Claude" Publisher="CN=Anthropic, O=Anthropic" Version="4.29.0.0" ProcessorArchitecture="x64" />
  <Applications>
    <Application Id="Claude" Executable="app\Claude.exe" />
    <Application Id="SshAskpass" Executable="app\resources\claude-ssh-askpass.exe" />
    <Application Id="SshProxy" Executable="app\resources\claude-ssh-proxy.exe" />
  </Applications>
</Package>"#,
        )
        .unwrap();
        let msix = root.join("fixture.msix");
        util::powershell(&format!(
            "Add-Type -AssemblyName System.IO.Compression.FileSystem; $z=[System.IO.Compression.ZipFile]::Open({msix},'Create'); try {{ [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($z,{source},'AppxManifest.xml') }} finally {{ $z.Dispose() }}",
            msix = ps_path(&msix),
            source = ps_path(&manifest)
        ))
        .unwrap();

        let identity = read_identity(&msix).unwrap();
        assert_eq!(identity.name, "Claude");
        assert_eq!(identity.version, "4.29.0.0");
        assert_eq!(identity.executable, r"app\Claude.exe");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn launcher_is_ascii_crlf() {
        let root = std::env::temp_dir().join(format!(
            "claude-portable-launcher-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        write_launcher(&root).unwrap();
        let bytes = std::fs::read(root.join("Claude.cmd")).unwrap();
        assert!(bytes.is_ascii(), "launcher must be ASCII");
        assert!(bytes.windows(2).all(|pair| pair != b"\n\r"));
        assert!(bytes.windows(2).any(|pair| pair == b"\r\n"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn identity_rejects_a_foreign_package() {
        let script_package = |name: &str, architecture: &str, publisher: &str, exe: &str| {
            serde_json::json!({
                "Name": name,
                "Version": "1.0.0.0",
                "Architecture": architecture,
                "Publisher": publisher,
                "Executable": exe,
            })
            .to_string()
        };
        let parse = |raw: &str| -> Result<(), String> {
            let identity: MsixIdentity = serde_json::from_str(raw).unwrap();
            if identity.name != "Claude" {
                return Err("name".into());
            }
            if !identity.architecture.eq_ignore_ascii_case("x64") {
                return Err("architecture".into());
            }
            if !identity.publisher.to_lowercase().contains("anthropic") {
                return Err("publisher".into());
            }
            Ok(())
        };
        assert!(parse(&script_package(
            "Claude",
            "x64",
            "CN=Anthropic PBC",
            "app\\Claude.exe"
        ))
        .is_ok());
        assert!(parse(&script_package(
            "Other",
            "x64",
            "CN=Anthropic PBC",
            "app\\Claude.exe"
        ))
        .is_err());
        assert!(parse(&script_package(
            "Claude",
            "arm64",
            "CN=Anthropic PBC",
            "app\\Claude.exe"
        ))
        .is_err());
        assert!(parse(&script_package(
            "Claude",
            "x64",
            "CN=Evil",
            "app\\Claude.exe"
        ))
        .is_err());
    }
}
