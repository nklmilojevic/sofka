use super::*;
use std::io::Read;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub trusted: bool,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            name: "official".into(),
            url: RAW_ROOT.into(),
            trusted: true,
        }
    }
}

impl Source {
    pub fn is_official(&self) -> bool {
        self.name == "official" && self.url == RAW_ROOT
    }

    pub fn identity(&self) -> &str {
        if self.is_official() {
            "official"
        } else {
            &self.url
        }
    }

    fn local_path(&self) -> Option<PathBuf> {
        self.url.strip_prefix("file://").map(PathBuf::from)
    }

    pub(super) fn validate_url(&self, url: &str) -> Result<(), String> {
        let base: http::Uri = self
            .url
            .parse()
            .map_err(|e| format!("invalid catalog URL: {e}"))?;
        let target: http::Uri = url
            .parse()
            .map_err(|e| format!("invalid download URL: {e}"))?;
        if !matches!(target.scheme_str(), Some("http" | "https"))
            || target.scheme() != base.scheme()
            || target.authority() != base.authority()
            || target
                .authority()
                .is_none_or(|authority| authority.as_str().contains('@'))
            || url.contains('#')
        {
            return Err("custom catalog downloads must use the catalog HTTP origin".into());
        }
        Ok(())
    }

    pub(super) fn artifact_location(&self, location: &str) -> Result<String, String> {
        if let Some(index) = self.local_path() {
            let root = index.parent().ok_or("catalog has no parent directory")?;
            let path = if let Some(path) = location.strip_prefix("file://") {
                PathBuf::from(path)
            } else {
                if location.contains("://") {
                    return Err("local catalogs require local package files".into());
                }
                root.join(location)
            };
            // Check the spelling before I/O. Canonical paths are checked when read.
            if !path.starts_with(root)
                || path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                return Err("package path must stay inside the catalog directory".into());
            }
            return Ok(format!("file://{}", path.display()));
        }
        let base: http::Uri = self
            .url
            .parse()
            .map_err(|e| format!("invalid catalog URL: {e}"))?;
        let url = resolve_redirect(&base, location)?;
        self.validate_url(&url)?;
        Ok(url)
    }

    pub(super) async fn read(
        &self,
        location: &str,
        limit: usize,
        budget: Budget,
        offline: bool,
    ) -> Result<Vec<u8>, String> {
        if let Some(path) = location.strip_prefix("file://") {
            let index = self
                .local_path()
                .ok_or("HTTP catalogs cannot read local files")?;
            let root = index
                .parent()
                .ok_or("catalog has no parent directory")?
                .canonicalize()
                .map_err(|e| format!("reading catalog directory: {e}"))?;
            let path = Path::new(path)
                .canonicalize()
                .map_err(|e| format!("local catalog file is unavailable: {e}"))?;
            if !path.starts_with(root) {
                return Err("package path must stay inside the catalog directory".into());
            }
            return read_limited(&path, limit);
        }
        if offline {
            return Err(
                "download is not cached; provide local files or run without --offline".into(),
            );
        }
        self.validate_url(location)?;
        tokio::time::timeout(
            budget.total,
            get_inner(location, limit, budget, false, Some(self)),
        )
        .await
        .map_err(|_| "custom catalog download timed out".to_string())?
    }

    fn cache_path(&self, cache: &Path) -> PathBuf {
        cache
            .join("catalogs")
            .join(format!("{}.json", digest(self.identity().as_bytes())))
    }

    async fn load(&self, cache: &Path, offline: bool) -> Result<CatalogSnapshot, String> {
        let path = self.cache_path(cache);
        let (bytes, fetched_at, cached) = if offline && self.local_path().is_none() {
            let bytes = read_limited(&path, CATALOG_MAX_BYTES)?;
            let fetched_at = cache_time(&path);
            (bytes, fetched_at, true)
        } else {
            (
                self.read(&self.url, CATALOG_MAX_BYTES, METADATA_BUDGET, offline)
                    .await?,
                now(),
                false,
            )
        };
        let mut catalog = Catalog::parse_from(&bytes, self)?;
        let revision = digest(&bytes);
        for plugin in &mut catalog.plugins {
            plugin.revision = revision.clone();
        }
        if !cached && let Err(error) = write_bytes(&path, &bytes) {
            eprintln!("warning: could not cache catalog {}: {error}", self.name);
        }
        Ok(CatalogSnapshot {
            catalog,
            commit: revision,
            fetched_at,
            offline: cached,
        })
    }
}

fn cache_time(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_secs())
}

fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("catalogs and packages must be regular files".into());
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err(format!("file exceeds the size limit of {limit} bytes"));
    }
    Ok(bytes)
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    official: bool,
    catalogs: Vec<Source>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            official: true,
            catalogs: Vec::new(),
        }
    }
}

impl Settings {
    fn sources(mut self, directory: &Path) -> Result<Vec<Source>, String> {
        let mut names = HashSet::from(["official".to_string()]);
        let mut locations = HashSet::new();
        for source in &mut self.catalogs {
            validate_id(&source.name)?;
            if !names.insert(source.name.clone()) {
                return Err(format!(
                    "duplicate or reserved catalog name {:?}",
                    source.name
                ));
            }
            if !source.trusted {
                return Err(format!(
                    "catalog {:?} is not trusted; review its plugins, then set trusted = true in catalogs.toml",
                    source.name
                ));
            }
            if source.url.starts_with("http://") || source.url.starts_with("https://") {
                source.validate_url(&source.url)?;
            } else {
                let path = source.url.strip_prefix("file://").unwrap_or(&source.url);
                if path.is_empty() || path.contains("://") {
                    return Err("catalog location must be a file path or an HTTP/HTTPS URL".into());
                }
                let path = Path::new(path);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    directory.join(path)
                };
                // Normalization keeps aliases from changing installation ownership.
                let mut normalized = PathBuf::new();
                for part in path.components() {
                    match part {
                        std::path::Component::CurDir => {}
                        std::path::Component::ParentDir => {
                            normalized.pop();
                        }
                        part => normalized.push(part.as_os_str()),
                    }
                }
                source.url = format!("file://{}", normalized.display());
            }
            if !locations.insert(source.url.clone()) {
                return Err("duplicate catalog location".into());
            }
        }
        if self.official {
            self.catalogs.insert(0, Source::default());
        }
        Ok(self.catalogs)
    }
}

fn configured(directory: &Path) -> Result<Vec<Source>, String> {
    let path = directory.join("catalogs.toml");
    let settings = match std::fs::read_to_string(&path) {
        Ok(text) => {
            toml::from_str::<Settings>(&text).map_err(|e| format!("invalid catalogs.toml: {e}"))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(error) => return Err(format!("reading catalogs.toml: {error}")),
    };
    settings.sources(directory)
}

pub async fn load_selected(
    offline: bool,
    selected: Option<&str>,
) -> Result<CatalogSnapshot, String> {
    let sources = configured(&config_dir()?)?;
    load_sources(sources, &cache_dir(), offline, selected).await
}

async fn load_sources(
    sources: Vec<Source>,
    cache: &Path,
    offline: bool,
    selected: Option<&str>,
) -> Result<CatalogSnapshot, String> {
    if let Some(name) = selected
        && !sources.iter().any(|source| source.name == name)
    {
        return Err(format!("unknown or disabled catalog {name:?}"));
    }
    let mut combined: Option<CatalogSnapshot> = None;
    for source in sources
        .into_iter()
        .filter(|source| selected.is_none_or(|name| name == source.name))
    {
        let mut snapshot = if source.is_official() {
            super::load(offline).await?
        } else {
            source.load(cache, offline).await?
        };
        for plugin in &mut snapshot.catalog.plugins {
            plugin.revision = snapshot.commit.clone();
        }
        if let Some(combined) = &mut combined {
            combined.catalog.plugins.extend(snapshot.catalog.plugins);
            combined.fetched_at = combined.fetched_at.min(snapshot.fetched_at);
            combined.offline |= snapshot.offline;
        } else {
            combined = Some(snapshot);
        }
    }
    combined.ok_or_else(|| "no plugin catalogs are enabled".into())
}

pub(super) fn cached_sources() -> Option<CatalogSnapshot> {
    let sources = configured(&config_dir().ok()?).ok()?;
    let cache = cache_dir();
    let mut combined: Option<CatalogSnapshot> = None;
    for source in sources {
        let snapshot = if source.is_official() {
            load_cached(&cache.join("catalog-cache.json")).ok()
        } else {
            read_limited(&source.cache_path(&cache), CATALOG_MAX_BYTES)
                .ok()
                .and_then(|bytes| {
                    Catalog::parse_from(&bytes, &source)
                        .ok()
                        .map(|catalog| CatalogSnapshot {
                            catalog,
                            commit: digest(&bytes),
                            fetched_at: cache_time(&source.cache_path(&cache)),
                            offline: true,
                        })
                })
        };
        if let Some(snapshot) = snapshot {
            if let Some(combined) = &mut combined {
                combined.catalog.plugins.extend(snapshot.catalog.plugins);
                combined.fetched_at = combined.fetched_at.min(snapshot.fetched_at);
            } else {
                combined = Some(snapshot);
            }
        }
    }
    combined
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "sofka-sources-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn source(&self) -> Source {
            Source {
                name: "team".into(),
                url: format!("file://{}/index.json", self.0.display()),
                trusted: true,
            }
        }
        fn index(&self) {
            let mut catalog = super::super::tests::catalog();
            catalog.plugins[0].versions[0].artifacts[0].url = "package.tar.zst".into();
            std::fs::write(
                self.0.join("index.json"),
                serde_json::to_vec(&catalog).unwrap(),
            )
            .unwrap();
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sources_require_trust_unique_names_and_valid_locations() {
        let dir = Directory::new();
        assert!(Settings::default().sources(&dir.0).unwrap()[0].is_official());
        for text in [
            "[[catalogs]]\nname='team'\nurl='index.json'",
            "[[catalogs]]\nname='official'\nurl='index.json'\ntrusted=true",
            "[[catalogs]]\nname='team'\nurl='ftp://host/index.json'\ntrusted=true",
            "[[catalogs]]\nname='team'\nurl='https://user:password@host/index.json'\ntrusted=true",
            "[[catalogs]]\nname='team'\nurl=''\ntrusted=true",
            "[[catalogs]]\nname='team'\nurl='index.json'\ntrusted=true\n[[catalogs]]\nname='team'\nurl='other.json'\ntrusted=true",
        ] {
            assert!(
                toml::from_str::<Settings>(text)
                    .unwrap()
                    .sources(&dir.0)
                    .is_err(),
                "{text}"
            );
        }
        let sources = toml::from_str::<Settings>(
            "official=false\n[[catalogs]]\nname='team'\nurl='./index.json'\ntrusted=true",
        )
        .unwrap()
        .sources(&dir.0)
        .unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].identity(), dir.source().identity());
    }

    #[tokio::test]
    async fn local_catalog_needs_no_cache_and_ambiguous_ids_require_selection() {
        let first = Directory::new();
        let second = Directory::new();
        first.index();
        second.index();
        let one = first.source();
        let mut two = second.source();
        two.name = "other".into();
        let sources = vec![one, two];
        let snapshot = load_sources(sources.clone(), &first.0.join("cache"), true, None)
            .await
            .unwrap();
        let id = &snapshot.catalog.plugins[0].id;
        assert!(
            snapshot
                .catalog
                .select(id)
                .unwrap_err()
                .contains("multiple catalogs")
        );
        let selected = load_sources(sources.clone(), &first.0.join("cache"), true, Some("team"))
            .await
            .unwrap();
        assert_eq!(
            selected.catalog.select(id).unwrap().plugin.source.name,
            "team"
        );
        assert!(
            load_sources(sources, &first.0, true, Some("missing"))
                .await
                .unwrap_err()
                .contains("unknown")
        );
    }

    #[test]
    fn custom_urls_and_redirects_cannot_leave_the_source_origin() {
        let source = Source {
            name: "team".into(),
            url: "https://plugins.internal/catalog/index.json".into(),
            trusted: true,
        };
        assert_eq!(
            source.artifact_location("packages/a.tar.zst").unwrap(),
            "https://plugins.internal/catalog/packages/a.tar.zst"
        );
        for url in [
            "http://plugins.internal/a",
            "https://other.internal/a",
            "file:///etc/passwd",
            "https://plugins.internal:8443/a",
            "https://user@plugins.internal/a",
        ] {
            assert!(source.artifact_location(url).is_err(), "{url}");
        }
        let http = Source {
            url: "http://plugins.internal/index.json".into(),
            ..source
        };
        assert!(http.artifact_location("a.tar.zst").is_ok());
        assert!(validate_http_uri(&"http://plugins.internal/a".parse().unwrap(), false).is_err());
    }

    #[tokio::test]
    async fn local_artifacts_are_verified_without_a_previous_download() {
        let dir = Directory::new();
        let source = dir.source();
        let bytes = b"package content";
        std::fs::write(dir.0.join("a.tar.zst"), bytes).unwrap();
        let mut artifact = Artifact {
            platform: "any".into(),
            url: "a.tar.zst".into(),
            blake3: digest(bytes),
            size: bytes.len() as u64,
        };
        let cache = dir.0.join("cache");
        let archive = artifact_from(&cache, &artifact, true, &source)
            .await
            .unwrap();
        assert_eq!(std::fs::read(archive.path()).unwrap(), bytes);
        artifact.blake3 = "0".repeat(64);
        assert!(
            artifact_from(&cache, &artifact, true, &source)
                .await
                .is_err()
        );
        artifact.url = "../outside.tar.zst".into();
        assert!(
            artifact_from(&cache, &artifact, true, &source)
                .await
                .is_err()
        );
        artifact.url = "missing.tar.zst".into();
        assert!(
            artifact_from(&cache, &artifact, true, &source)
                .await
                .unwrap_err()
                .contains("unavailable")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_artifact_symlinks_cannot_escape_the_catalog_directory() {
        let dir = Directory::new();
        let outside = Directory::new();
        std::fs::write(outside.0.join("package"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.0.join("package"), dir.0.join("package")).unwrap();
        assert!(
            dir.source()
                .read(
                    &format!("file://{}/package", dir.0.display()),
                    100,
                    METADATA_BUDGET,
                    true
                )
                .await
                .unwrap_err()
                .contains("inside")
        );
    }

    #[tokio::test]
    async fn caches_are_bound_to_source_locations_and_revalidated() {
        let dir = Directory::new();
        dir.index();
        let source = Source {
            name: "team".into(),
            url: "https://plugins.internal/index.json".into(),
            trusted: true,
        };
        let other = Source {
            url: "https://other.internal/index.json".into(),
            ..source.clone()
        };
        let bytes = std::fs::read(dir.0.join("index.json")).unwrap();
        write_bytes(&source.cache_path(&dir.0), &bytes).unwrap();
        assert!(source.load(&dir.0, true).await.unwrap().offline);
        assert!(other.load(&dir.0, true).await.is_err());
        let mut catalog: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        catalog["plugins"][0]["versions"][0]["artifacts"][0]["url"] =
            "https://outside.internal/package".into();
        write_bytes(
            &source.cache_path(&dir.0),
            &serde_json::to_vec(&catalog).unwrap(),
        )
        .unwrap();
        assert!(source.load(&dir.0, true).await.is_err());
    }

    #[tokio::test]
    async fn internal_http_fetch_and_cross_origin_redirect_rejection() {
        use std::io::Write;
        let dir = Directory::new();
        dir.index();
        let bytes = std::fs::read(dir.0.join("index.json")).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let source = Source {
            name: "team".into(),
            url: format!("http://{}/index.json", listener.local_addr().unwrap()),
            trusted: true,
        };
        let server = std::thread::spawn(move || {
            for response in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                if response == 2 {
                    write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://other.invalid/index.json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    let body = if response == 0 {
                        bytes.as_slice()
                    } else {
                        b"package"
                    };
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(body).unwrap();
                }
            }
        });
        assert!(source.load(&dir.0, false).await.is_ok());
        let artifact = Artifact {
            platform: "any".into(),
            url: "package.tar.zst".into(),
            blake3: digest(b"package"),
            size: 7,
        };
        let archive = artifact_from(&dir.0, &artifact, false, &source)
            .await
            .unwrap();
        assert_eq!(std::fs::read(archive.path()).unwrap(), b"package");
        assert!(
            source
                .load(&dir.0, false)
                .await
                .unwrap_err()
                .contains("origin")
        );
        server.join().unwrap();
    }
}
