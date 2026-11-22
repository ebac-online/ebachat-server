//! File storage: the local directory or S3 of `FileSettings`, as Mattermost's `filestore` drivers.

// Based on Mattermost server/platform/shared/filestore, Copyright (c) 2015-present Mattermost, Inc.
// Licensed under AGPL-3.0.

use std::sync::Arc;

use bytes::Bytes;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::config::Config;

pub struct FileStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    /// The directory of the local driver (none for S3).
    local: Option<std::path::PathBuf>,
}

impl FileStore {
    pub fn new(cfg: &Config) -> anyhow::Result<FileStore> {
        let f = |k: &str| cfg.str(&format!("FileSettings.{k}")).to_string();
        if f("DriverName") == "amazons3" {
            let ssl = cfg.bool("FileSettings.AmazonS3SSL");
            let endpoint = format!("{}://{}", if ssl { "https" } else { "http" }, f("AmazonS3Endpoint"));
            let mut b = object_store::aws::AmazonS3Builder::from_env()
                .with_bucket_name(f("AmazonS3Bucket"))
                .with_region(if f("AmazonS3Region").is_empty() { "us-east-1".into() } else { f("AmazonS3Region") })
                .with_endpoint(endpoint)
                .with_allow_http(!ssl);
            if !f("AmazonS3AccessKeyId").is_empty() {
                b = b.with_access_key_id(f("AmazonS3AccessKeyId")).with_secret_access_key(f("AmazonS3SecretAccessKey"));
            }
            if f("AmazonS3Endpoint") != "s3.amazonaws.com" {
                b = b.with_virtual_hosted_style_request(false);
            }
            return Ok(FileStore { store: Arc::new(b.build()?), prefix: f("AmazonS3PathPrefix"), local: None });
        }
        let dir = f("Directory");
        let dir = if dir.is_empty() { "./data/".to_string() } else { dir };
        std::fs::create_dir_all(&dir)?;
        let store = object_store::local::LocalFileSystem::new_with_prefix(&dir)?.with_automatic_cleanup(true);
        let local = Some(std::fs::canonicalize(&dir)?);
        Ok(FileStore { store: Arc::new(store), prefix: String::new(), local })
    }

    /// The file on disk with the local driver, to be read in place (a bulk import's zip).
    pub fn local_path(&self, p: &str) -> Option<std::path::PathBuf> {
        self.local.as_ref().map(|d| d.join(clean(p)))
    }

    /// The object's path with its name as given (`Path::from` would percent-encode non-ASCII, `%`, `#` and more).
    fn path(&self, p: &str) -> Path {
        let full =
            if self.prefix.is_empty() { p.to_string() } else { format!("{}/{p}", self.prefix.trim_end_matches('/')) };
        Path::parse(&full).unwrap_or_else(|_| Path::from(full))
    }

    pub async fn read(&self, p: &str) -> anyhow::Result<Bytes> {
        Ok(self.store.get(&self.path(p)).await?.bytes().await?)
    }

    /// The bytes `range` of a file, and its whole size.
    pub async fn read_range(&self, p: &str, range: std::ops::Range<u64>) -> anyhow::Result<Bytes> {
        Ok(self.store.get_range(&self.path(p), range).await?)
    }

    pub async fn size(&self, p: &str) -> anyhow::Result<u64> {
        Ok(self.store.head(&self.path(p)).await?.size)
    }

    pub async fn exists(&self, p: &str) -> bool {
        self.store.head(&self.path(p)).await.is_ok()
    }

    pub async fn write(&self, p: &str, data: Bytes) -> anyhow::Result<()> {
        self.store.put(&self.path(p), PutPayload::from_bytes(data)).await?;
        Ok(())
    }

    pub async fn remove(&self, p: &str) -> anyhow::Result<()> {
        self.store.delete(&self.path(p)).await?;
        Ok(())
    }

    /// `RemoveDirectory`: every file under the directory.
    pub async fn remove_dir(&self, p: &str) -> anyhow::Result<()> {
        use futures_util::StreamExt;
        let prefix = self.path(p);
        let mut list = self.store.list(Some(&prefix));
        let mut paths = vec![];
        while let Some(m) = list.next().await {
            paths.push(m?.location);
        }
        for l in paths {
            self.store.delete(&l).await?;
        }
        Ok(())
    }

    /// A file read as a stream, a range of its bytes or whole.
    pub async fn stream(&self, p: &str, range: Option<std::ops::Range<u64>>) -> anyhow::Result<Reading> {
        let opts = object_store::GetOptions { range: range.map(object_store::GetRange::Bounded), ..Default::default() };
        let r = self.store.get_opts(&self.path(p), opts).await?;
        let size = r.meta.size;
        Ok(Reading { stream: r.into_stream(), size })
    }

    /// `ListDirectory`: the entries right under `dir` (files and directories), as paths from the root, with
    /// their modification times in milliseconds (0 for directories).
    pub async fn list(&self, dir: &str) -> anyhow::Result<Vec<(String, i64)>> {
        let dir = clean(dir);
        let prefix = if dir.is_empty() { None } else { Some(self.path(&dir)) };
        let r = self.store.list_with_delimiter(prefix.as_ref()).await?;
        let root = format!("{}/", self.prefix.trim_end_matches('/'));
        let strip = |p: &Path| {
            let s = p.to_string();
            if self.prefix.is_empty() { s } else { s.trim_start_matches(&root).to_string() }
        };
        let mut out: Vec<(String, i64)> = r.common_prefixes.iter().map(|p| (strip(p), 0)).collect();
        out.extend(r.objects.iter().map(|o| (strip(&o.location), o.last_modified.timestamp_millis())));
        Ok(out)
    }

    /// `FileModTime`, in milliseconds.
    pub async fn modified(&self, p: &str) -> anyhow::Result<i64> {
        Ok(self.store.head(&self.path(&clean(p))).await?.last_modified.timestamp_millis())
    }

    /// A file written in parts as they come, so that an upload never sits whole in memory.
    pub async fn writer(&self, p: &str) -> anyhow::Result<Writer> {
        let upload = self.store.put_multipart(&self.path(p)).await?;
        Ok(Writer(object_store::WriteMultipart::new_with_chunk_size(upload, 5 * 1024 * 1024)))
    }
}

/// A file being read: its bytes as they arrive, and the whole object's size.
pub struct Reading {
    pub stream: futures_util::stream::BoxStream<'static, object_store::Result<Bytes>>,
    pub size: u64,
}

pub struct Writer(object_store::WriteMultipart);

impl Writer {
    pub async fn write(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.0.wait_for_capacity(4).await?;
        self.0.write(data);
        Ok(())
    }
    pub async fn finish(self) -> anyhow::Result<()> {
        self.0.finish().await?;
        Ok(())
    }
    pub async fn abort(self) {
        let _ = self.0.abort().await;
    }
}

/// A path as Go's `filepath.Join` cleans it: no `.` segments, no leading `./` or trailing `/`.
pub fn clean(p: &str) -> String {
    let mut parts: Vec<&str> = vec![];
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn names_stay_as_given_on_disk() {
        let dir = std::env::temp_dir().join(crate::app::new_id());
        std::fs::create_dir_all(&dir).unwrap();
        let store = object_store::local::LocalFileSystem::new_with_prefix(&dir).unwrap();
        let fs = FileStore { store: Arc::new(store), prefix: String::new(), local: Some(dir.clone()) };
        let name = "тест файл 100%41 #x+[a].txt";
        fs.write(&format!("a/{name}"), Bytes::from_static(b"hi")).await.unwrap();
        assert!(dir.join("a").join(name).exists());
        assert_eq!(fs.read(&format!("a/{name}")).await.unwrap(), Bytes::from_static(b"hi"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
