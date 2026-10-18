use dockdash::{BlobCache, Layer, LayerCompression};
use std::{
    fs::{self, File, FileTimes},
    io::Read,
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

fn contents(layer: &Layer, compression: LayerCompression) -> Vec<(String, Vec<u8>)> {
    let file = File::open(layer.path()).unwrap();
    let reader: Box<dyn Read> = match compression {
        LayerCompression::Zstd => Box::new(zstd::Decoder::new(file).unwrap()),
        LayerCompression::Gzip => Box::new(flate2::read::GzDecoder::new(file)),
    };
    tar::Archive::new(reader)
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            (path, bytes)
        })
        .collect()
}

fn write_fixed_time(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000)))
        .unwrap();
}

async fn mutation(directory: bool, mode: Option<u32>) {
    for compression in [LayerCompression::Zstd, LayerCompression::Gzip] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        let file = source.join("file");
        let cache = BlobCache::with_path(root.path().join("cache")).unwrap();
        for bytes in [b"before", b"after!"] {
            write_fixed_time(&file, bytes);
            let builder = Layer::builder()
                .unwrap()
                .blob_cache(cache.clone())
                .compression(compression);
            let builder = if directory {
                builder.directory(&source, "data")
            } else {
                builder.file(&file, "data/file", mode)
            }
            .unwrap();
            let layer = builder.build().await.unwrap();
            let entries = contents(&layer, compression);
            assert_eq!(
                entries.iter().find(|e| e.0 == "data/file").unwrap().1,
                bytes
            );
        }
    }
}

#[tokio::test]
async fn file_content_mutation_with_preserved_size_and_mtime() {
    mutation(false, None).await;
}
#[tokio::test]
async fn custom_mode_content_mutation_with_preserved_size_and_mtime() {
    mutation(false, Some(0o640)).await;
}
#[tokio::test]
async fn directory_content_mutation_with_preserved_size_and_mtime() {
    mutation(true, None).await;
}

#[tokio::test]
async fn cache_preserves_archive_entry_order() {
    for compression in [LayerCompression::Zstd, LayerCompression::Gzip] {
        let root = tempfile::tempdir().unwrap();
        let cache = BlobCache::with_path(root.path().to_path_buf()).unwrap();
        for paths in [["a", "b"], ["b", "a"]] {
            let mut builder = Layer::builder()
                .unwrap()
                .blob_cache(cache.clone())
                .compression(compression);
            for path in paths {
                builder = builder.data(path, path.as_bytes(), None).unwrap();
            }
            let layer = builder.build().await.unwrap();
            assert_eq!(
                contents(&layer, compression)
                    .iter()
                    .map(|e| e.0.as_str())
                    .collect::<Vec<_>>(),
                paths
            );
        }
    }
}

#[tokio::test]
async fn repeated_content_reuses_compressed_blob() {
    for compression in [LayerCompression::Zstd, LayerCompression::Gzip] {
        let root = tempfile::tempdir().unwrap();
        let cache = BlobCache::with_path(root.path().to_path_buf()).unwrap();
        let first = Layer::builder()
            .unwrap()
            .blob_cache(cache.clone())
            .compression(compression)
            .data("file", b"same", Some(0o640))
            .unwrap()
            .build()
            .await
            .unwrap();
        let second = Layer::builder()
            .unwrap()
            .blob_cache(cache)
            .compression(compression)
            .data("file", b"same", Some(0o640))
            .unwrap()
            .build()
            .await
            .unwrap();
        assert_eq!(first.blob_digest(), second.blob_digest());
        assert_eq!(
            fs::read(first.path()).unwrap(),
            fs::read(second.path()).unwrap()
        );
        assert_eq!(
            contents(&second, compression),
            vec![("file".into(), b"same".to_vec())]
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_target_change_with_preserved_metadata() {
    use std::os::unix::fs::symlink;
    for compression in [LayerCompression::Zstd, LayerCompression::Gzip] {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        let cache = BlobCache::with_path(root.path().join("cache")).unwrap();
        let mut original_time = None;
        for target in ["first", "other"] {
            let link = source.join("link");
            if link.exists() || fs::symlink_metadata(&link).is_ok() {
                fs::remove_file(&link).unwrap();
            }
            symlink(target, &link).unwrap();
            // Set symlink mtime without following it, including nanoseconds.
            let path = std::ffi::CString::new(link.as_os_str().as_encoded_bytes()).unwrap();
            let times = [libc::timespec {
                tv_sec: 1_700_000_000,
                tv_nsec: 0,
            }; 2];
            // SAFETY: path is NUL-terminated and times contains two initialized elements.
            assert_eq!(
                unsafe {
                    libc::utimensat(
                        libc::AT_FDCWD,
                        path.as_ptr(),
                        times.as_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                },
                0
            );
            let dir = File::open(&source).unwrap();
            let time =
                *original_time.get_or_insert_with(|| dir.metadata().unwrap().modified().unwrap());
            dir.set_times(FileTimes::new().set_modified(time)).unwrap();
            let layer = Layer::builder()
                .unwrap()
                .blob_cache(cache.clone())
                .compression(compression)
                .directory(&source, "data")
                .unwrap()
                .build()
                .await
                .unwrap();
            let file = File::open(layer.path()).unwrap();
            let reader: Box<dyn Read> = match compression {
                LayerCompression::Zstd => Box::new(zstd::Decoder::new(file).unwrap()),
                LayerCompression::Gzip => Box::new(flate2::read::GzDecoder::new(file)),
            };
            let mut archive = tar::Archive::new(reader);
            let entry = archive
                .entries()
                .unwrap()
                .map(Result::unwrap)
                .find(|e| e.path().unwrap() == Path::new("data/link"))
                .unwrap();
            assert!(entry.header().entry_type().is_symlink());
            assert_eq!(entry.link_name().unwrap().unwrap(), Path::new(target));
        }
    }
}

#[tokio::test]
async fn cached_headers_preserve_owner_and_mode() {
    for compression in [LayerCompression::Zstd, LayerCompression::Gzip] {
        let root = tempfile::tempdir().unwrap();
        let cache = BlobCache::with_path(root.path().to_path_buf()).unwrap();
        for (uid, gid, mode) in [(0, 0, 0o644), (1000, 1001, 0o644), (1000, 1001, 0o600)] {
            let layer = Layer::builder()
                .unwrap()
                .blob_cache(cache.clone())
                .compression(compression)
                .data_with_owner("file", b"same", mode, uid, gid)
                .unwrap()
                .build()
                .await
                .unwrap();
            let file = File::open(layer.path()).unwrap();
            let reader: Box<dyn Read> = match compression {
                LayerCompression::Zstd => Box::new(zstd::Decoder::new(file).unwrap()),
                LayerCompression::Gzip => Box::new(flate2::read::GzDecoder::new(file)),
            };
            let mut archive = tar::Archive::new(reader);
            let mut entry = archive.entries().unwrap().next().unwrap().unwrap();
            assert_eq!(
                (
                    entry.header().uid().unwrap(),
                    entry.header().gid().unwrap(),
                    entry.header().mode().unwrap()
                ),
                (uid, gid, mode)
            );
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"same");
        }
    }
}
