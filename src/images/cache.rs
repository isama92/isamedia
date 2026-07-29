//! On-disk cache of fetched poster bytes.
//!
//! Stores the *original* bytes and keys them without a render size, so one entry
//! serves every terminal and every window size. That split is what makes two
//! things free: a resize re-encodes from these bytes instead of refetching, and
//! flipping to forced halfblocks throws away the encoded protocols in memory
//! while leaving the downloads intact.
//!
//! Nothing here is load-bearing. Every failure is a miss plus a `debug` line, and
//! the poster then arrives over the network exactly as it would have anyway.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::images::key::ImageKey;

/// Total bytes the cache may occupy before the startup prune trims it.
const BUDGET_BYTES: u64 = 128 << 20;

/// Entries older than this go regardless of budget. Because the artwork version
/// is part of the key, entries are immutable once written, so mtime is a good
/// stand-in for "first fetched". Deliberately mtime and not atime: `relatime` and
/// `noatime` make access times unreliable.
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Give up on a pathological directory rather than pinning a blocking thread.
const MAX_PRUNE_ENTRIES: usize = 50_000;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The poster cache directory, or `None` when the platform will not say where it
/// belongs — in which case the run is memory-only. Not being able to cache is
/// never a reason to fail to start.
pub fn dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "isamedia")?;
    Some(dirs.cache_dir().join("posters"))
}

/// Where a poster's bytes live.
///
/// The name is a digest rather than anything readable, on purpose. The bytes are
/// not secret, but the *set* of filenames would otherwise spell out the user's
/// library — the same reasoning that makes the config file owner-only. It also
/// sidesteps sanitising server-supplied ids into a filename.
///
/// The two-hex-digit parent is a 256-way fan-out: a large library across three
/// backends reaches thousands of files, and a flat directory that size makes the
/// prune walk slow everywhere and pathological on some filesystems.
pub fn entry_path(dir: &Path, key: &ImageKey, origin: &str) -> PathBuf {
    let (source, id, tag) = parts(key);
    // NUL separators so ("ab", "c") cannot compose to the same string as
    // ("a", "bc"). The origin is in here so two servers never share an entry.
    let composite = format!("{source}\0{origin}\0{id}\0{tag}");
    let digest = fnv1a(composite.as_bytes());
    let shard = format!("{:02x}", (digest >> 56) as u8);
    dir.join(shard).join(format!("{source}-{digest:016x}.img"))
}

fn parts(key: &ImageKey) -> (&'static str, &str, &str) {
    match key {
        ImageKey::Jellyfin { item, tag } => ("jf", item.as_str(), tag.as_deref().unwrap_or("")),
        ImageKey::Radarr { path } => ("radarr", path.as_str(), ""),
        ImageKey::Sonarr { path } => ("sonarr", path.as_str(), ""),
    }
}

/// FNV-1a, inline rather than pulling in a hashing crate for one use. 64 bits is
/// ample here: at a few thousand entries the collision probability is around
/// 1e-13, and the cost of losing that coin flip is one wrong thumbnail.
///
/// Deliberately not `DefaultHasher`: its output is explicitly not stable across
/// Rust releases, which would silently invalidate the whole cache on a toolchain
/// upgrade.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Read cached bytes, or `None` on any problem.
///
/// A zero-length or unreadable file is also unlinked, so a file damaged by
/// something other than our own writer heals itself on the next attempt instead
/// of poisoning that key forever.
pub fn read(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) if !bytes.is_empty() => Some(bytes),
        Ok(_) => {
            forget(path);
            None
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            tracing::debug!(%err, "unreadable poster cache entry");
            forget(path);
            None
        }
    }
}

/// Drop an entry, for bytes that turned out not to decode.
pub fn forget(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Write bytes atomically and owner-only. Best-effort: a failure is a `debug`
/// line, because the poster is already rendering from memory either way.
///
/// Deliberately not sharing `config::write_owner_only`: that one is `anyhow`-based
/// with config-specific context and propagates failure to the user, where a cache
/// write must never surface. The shapes rhyme, the contracts do not.
pub fn write(path: &Path, bytes: &[u8]) {
    let Some(parent) = path.parent() else { return };
    if let Err(err) = create_dir_owner_only(parent) {
        tracing::debug!(%err, "could not create the poster cache directory");
        return;
    }
    // Temp name tagged with pid and a counter, so a leftover from a crashed run
    // or a second isamedia process cannot collide.
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}.{seq}.tmp", std::process::id()));
    if let Err(err) = write_tmp(&tmp, bytes) {
        tracing::debug!(%err, "could not write a poster cache entry");
        forget(&tmp);
        return;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        tracing::debug!(%err, "could not replace a poster cache entry");
        forget(&tmp);
    }
}

fn write_tmp(tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn create_dir_owner_only(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Trim the cache: over-age entries first, then oldest-first until under budget.
///
/// Called once per process on a blocking thread, never per write — pruning on
/// every write would turn scrolling a library into a stream of `read_dir` calls.
/// Blocking; call through `spawn_blocking`.
pub fn prune(dir: &Path) {
    let mut entries = Vec::new();
    let mut total = 0u64;
    let Ok(shards) = std::fs::read_dir(dir) else {
        return; // Nothing cached yet, which is not worth a log line.
    };
    for shard in shards.flatten() {
        let Ok(files) = std::fs::read_dir(shard.path()) else {
            continue;
        };
        for file in files.flatten() {
            if entries.len() >= MAX_PRUNE_ENTRIES {
                tracing::debug!(
                    limit = MAX_PRUNE_ENTRIES,
                    "stopped walking the poster cache early"
                );
                break;
            }
            let Ok(meta) = file.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let modified = meta.modified().unwrap_or_else(|_| SystemTime::now());
            total += meta.len();
            entries.push((file.path(), modified, meta.len()));
        }
    }

    let now = SystemTime::now();
    // Oldest first, so the age sweep and the budget sweep walk the same order.
    entries.sort_by_key(|(_, modified, _)| *modified);

    let mut removed = 0usize;
    for (path, modified, len) in &entries {
        let too_old = now.duration_since(*modified).is_ok_and(|age| age > MAX_AGE);
        // Oldest-first, so once an entry is neither over-age nor needed for the
        // budget, nothing after it can be either.
        if !too_old && total <= BUDGET_BYTES {
            break;
        }
        forget(path);
        total = total.saturating_sub(*len);
        removed += 1;
    }
    if removed > 0 {
        tracing::debug!(removed, remaining_bytes = total, "pruned the poster cache");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "isamedia-poster-cache-{}-{label}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn jf(item: &str, tag: Option<&str>) -> ImageKey {
        ImageKey::Jellyfin {
            item: item.into(),
            tag: tag.map(str::to_string),
        }
    }

    #[test]
    fn the_entry_name_is_stable_and_reveals_nothing() {
        let dir = Path::new("/cache/posters");
        let path = entry_path(dir, &jf("abc123", Some("tag9")), "https://example.com");
        // Stable across calls: the cache would be useless otherwise.
        assert_eq!(
            path,
            entry_path(dir, &jf("abc123", Some("tag9")), "https://example.com")
        );
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // Nothing identifying in the filename, and nothing needing sanitising.
        assert!(!name.contains("abc123"), "{name}");
        assert!(!name.contains("example.com"), "{name}");
        assert!(
            name.strip_prefix("jf-")
                .and_then(|rest| rest.strip_suffix(".img"))
                .is_some_and(
                    |hex| hex.len() == 16 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
                ),
            "{name}"
        );
        // Two-hex fan-out parent.
        let shard = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert_eq!(shard.len(), 2);
    }

    #[test]
    fn a_new_artwork_version_is_a_new_entry() {
        // Why no invalidation logic exists anywhere: re-arted item, new key, new
        // file, and the old one is simply never read again.
        let dir = Path::new("/cache");
        assert_ne!(
            entry_path(dir, &jf("abc", Some("v1")), "https://example.com"),
            entry_path(dir, &jf("abc", Some("v2")), "https://example.com")
        );
    }

    #[test]
    fn entries_are_scoped_to_their_server_and_backend() {
        let dir = Path::new("/cache");
        assert_ne!(
            entry_path(dir, &jf("abc", None), "https://one.example.com"),
            entry_path(dir, &jf("abc", None), "https://two.example.com")
        );
        // Same path shape on Radarr and Sonarr must not share an entry.
        let path = "/MediaCover/1/poster.jpg";
        assert_ne!(
            entry_path(
                dir,
                &ImageKey::Radarr { path: path.into() },
                "https://example.com"
            ),
            entry_path(
                dir,
                &ImageKey::Sonarr { path: path.into() },
                "https://example.com"
            )
        );
    }

    #[test]
    fn a_written_entry_reads_back_owner_only_and_leaves_no_temp() {
        let dir = temp_dir("roundtrip");
        let path = entry_path(&dir, &jf("abc", Some("t")), "https://example.com");
        write(&path, b"poster-bytes");
        assert_eq!(read(&path).as_deref(), Some(&b"poster-bytes"[..]));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "cache entries must be owner-only");
            let parent = std::fs::metadata(path.parent().unwrap()).unwrap();
            assert_eq!(parent.permissions().mode() & 0o777, 0o700);
        }

        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "a temp file was left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_entry_reads_as_a_miss_and_heals_itself() {
        let dir = temp_dir("damaged");
        let path = entry_path(&dir, &jf("abc", None), "https://example.com");
        write(&path, b"x");
        // Truncate to zero, as an interrupted write from something else might.
        std::fs::write(&path, b"").unwrap();
        assert_eq!(read(&path), None);
        assert!(
            !path.exists(),
            "a zero-length entry should be unlinked so the next attempt refetches"
        );
        assert_eq!(read(&path), None, "a missing entry is just a quiet miss");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pruning_removes_over_age_entries_and_keeps_fresh_ones() {
        let dir = temp_dir("prune");
        let fresh = entry_path(&dir, &jf("fresh", None), "https://example.com");
        let stale = entry_path(&dir, &jf("stale", None), "https://example.com");
        write(&fresh, b"fresh-bytes");
        write(&stale, b"stale-bytes");
        // Backdate the stale one past MAX_AGE.
        let old = SystemTime::now() - MAX_AGE - Duration::from_secs(60);
        std::fs::File::open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();

        prune(&dir);

        assert!(fresh.exists(), "a recent entry should survive");
        assert!(!stale.exists(), "an over-age entry should go");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pruning_an_absent_directory_is_a_no_op() {
        prune(&temp_dir("missing"));
    }
}
