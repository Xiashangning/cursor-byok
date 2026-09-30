//! Owns the on-disk caches under the data directory: fetched web content served from the
//! existing server plus the semble index and repository caches cleared by storage cleanup.
use std::{
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::Router;
use parking_lot::RwLock;
use semble_core::SembleConfig;
use tower_http::services::ServeDir;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::{config::managed_data_dir, Error, Result};

const CACHE_ROUTE: &str = "/web-cache";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebCacheEntry {
    pub url: String,
    pub file_path: String,
    pub size_bytes: i64,
    pub line_count: i64,
}

#[derive(Clone, Default)]
pub struct WebCache {
    inner: Option<Arc<WebCacheInner>>,
}

struct WebCacheInner {
    directory: PathBuf,
    service_addr: RwLock<Option<SocketAddr>>,
}

impl WebCache {
    /// `<数据目录>/cache/web`：网页抓取结果的存放目录。
    pub fn managed_directory() -> Result<PathBuf> {
        Ok(managed_data_dir()?.join("cache").join("web"))
    }

    pub fn managed() -> Result<Self> {
        Self::at(Self::managed_directory()?)
    }

    pub fn at(directory: PathBuf) -> Result<Self> {
        fs::create_dir_all(&directory)?;
        Ok(Self {
            inner: Some(Arc::new(WebCacheInner {
                directory,
                service_addr: RwLock::new(None),
            })),
        })
    }

    pub fn set_service_addr(&self, address: SocketAddr) {
        if let Some(inner) = &self.inner {
            *inner.service_addr.write() = Some(local_address(address));
        }
    }

    pub async fn store(&self, content: &str) -> Result<Option<WebCacheEntry>> {
        let Some(inner) = &self.inner else {
            return Ok(None);
        };
        let address = inner.service_addr.read().ok_or_else(|| {
            Error::Config("web cache is unavailable before the server starts listening".into())
        })?;
        let file_name = format!("{}.txt", Uuid::new_v4());
        let path = inner.directory.join(&file_name);
        let file_path = path.to_string_lossy().into_owned();
        let size_bytes = content.len() as i64;
        let line_count = content.lines().count() as i64;
        let bytes = content.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || fs::write(path, bytes))
            .await
            .map_err(|error| Error::Store(format!("web cache write task failed: {error}")))??;
        Ok(Some(WebCacheEntry {
            url: format!("http://{address}{CACHE_ROUTE}/{file_name}"),
            file_path,
            size_bytes,
            line_count,
        }))
    }

    pub fn router(&self) -> Router {
        let Some(inner) = &self.inner else {
            return Router::new();
        };
        Router::new().nest_service(CACHE_ROUTE, ServeDir::new(inner.directory.clone()))
    }

    #[cfg(test)]
    fn directory(&self) -> &std::path::Path {
        &self.inner.as_ref().expect("enabled web cache").directory
    }
}

/// 删除可重建的本地缓存：semble 索引与克隆仓库、网页抓取结果，返回释放的字节数。
pub fn clear_caches() -> Result<u64> {
    clear_directories(&cache_directories()?)
}

/// 可重建缓存的当前占用字节数。
pub fn cache_bytes() -> Result<u64> {
    let mut bytes = 0;
    for directory in cache_directories()? {
        bytes += directory_bytes(&directory)?;
    }
    Ok(bytes)
}

fn cache_directories() -> Result<Vec<PathBuf>> {
    let mut directories = SembleConfig::default()
        .rebuildable_cache_directories()
        .to_vec();
    directories.push(WebCache::managed_directory()?);
    Ok(directories)
}

/// 清空目录内容但保留目录本身：运行中的服务仍会向这些路径写入。
fn clear_directories(directories: &[PathBuf]) -> Result<u64> {
    let mut freed = 0;
    for directory in directories {
        freed += directory_bytes(directory)?;
        if directory.exists() {
            fs::remove_dir_all(directory)?;
        }
        fs::create_dir_all(directory)?;
    }
    Ok(freed)
}

fn directory_bytes(directory: &Path) -> Result<u64> {
    if !directory.is_dir() {
        return Ok(0);
    }
    let mut bytes = 0;
    for entry in WalkDir::new(directory) {
        let entry = entry.map_err(|error| cache_error(&error))?;
        if entry.file_type().is_file() {
            bytes += entry.metadata().map_err(|error| cache_error(&error))?.len();
        }
    }
    Ok(bytes)
}

fn cache_error(error: impl std::fmt::Display) -> Error {
    Error::Io(std::io::Error::other(format!("清理缓存失败: {error}")))
}

fn local_address(address: SocketAddr) -> SocketAddr {
    match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), address.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), address.port())
        }
        _ => address,
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use tempfile::tempdir;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::{clear_directories, SembleConfig, WebCache};

    #[test]
    fn clears_rebuildable_caches_and_keeps_embedding_models() {
        let root = tempdir().unwrap();
        let config = SembleConfig::new(root.path().join("semble"));
        let models = config.cache_dir.join("models/potion-code-16M-v2");
        let web = root.path().join("web");
        for (path, bytes) in [
            (config.cache_dir.join("indexes/v7/a/index.bin"), 10usize),
            (config.cache_dir.join("repos/clone/.git/config"), 5),
            (models.join("model.safetensors"), 2048),
            (web.join("page.txt"), 7),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, vec![0u8; bytes]).unwrap();
        }

        let mut directories = config.rebuildable_cache_directories().to_vec();
        directories.push(web.clone());
        let freed = clear_directories(&directories).unwrap();

        assert_eq!(freed, 22);
        for directory in directories {
            assert!(std::fs::read_dir(&directory).unwrap().next().is_none());
        }
        assert!(models.join("model.safetensors").is_file());
    }

    #[tokio::test]
    async fn stores_uuid_named_content_and_serves_it_from_existing_router() {
        let directory = tempdir().unwrap();
        let cache = WebCache::at(directory.path().join("cache").join("web")).unwrap();
        cache.set_service_addr("0.0.0.0:4312".parse().unwrap());

        let entry = cache
            .store("complete fetched content")
            .await
            .unwrap()
            .unwrap();
        let name = entry.url.rsplit('/').next().unwrap();
        let id = name.strip_suffix(".txt").unwrap();
        assert!(Uuid::parse_str(id).is_ok());
        assert_eq!(
            std::fs::read_to_string(cache.directory().join(name)).unwrap(),
            "complete fetched content"
        );
        assert_eq!(entry.url, format!("http://127.0.0.1:4312/web-cache/{name}"));
        assert_eq!(entry.size_bytes, 24);
        assert_eq!(entry.line_count, 1);

        let response = cache
            .router()
            .oneshot(
                Request::get(format!("/web-cache/{name}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "complete fetched content"
        );
    }
}
