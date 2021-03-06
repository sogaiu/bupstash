use super::address::*;
use super::chunker;
use super::crypto;
use super::fsutil;
use super::htree;
use super::index;
use super::itemset;
use super::protocol::*;
use super::querycache;
use super::repository;
use super::rollsum;
use super::sendlog;
use super::xid::*;
use super::xtar;
use failure::Fail;
use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;

#[derive(Debug, Fail)]
pub enum ClientError {
    #[fail(display = "corrupt or tampered data")]
    CorruptOrTamperedDataError,
}

pub fn open_repository(
    w: &mut dyn std::io::Write,
    r: &mut dyn std::io::Read,
    lock_hint: LockHint,
) -> Result<(), failure::Error> {
    write_packet(
        w,
        &Packet::TOpenRepository(TOpenRepository {
            repository_protocol_version: "1".to_string(),
            lock_hint,
        }),
    )?;

    match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
        Packet::ROpenRepository(resp) => {
            let clock_skew = chrono::Utc::now().signed_duration_since(resp.now);
            const MAX_SKEW_MINS: i64 = 15;
            if clock_skew > chrono::Duration::minutes(MAX_SKEW_MINS)
                || clock_skew < chrono::Duration::minutes(-MAX_SKEW_MINS)
            {
                // This helps protect against inaccurate item timestamps, which protects users from unintentionally
                // deleting important backups when deleting based on timestamp queries. Instead they will be notified
                // of the clock mismatch as soon as we know about it.
                failure::bail!("server and client have clock skew larger than {} minutes, refusing connection.", MAX_SKEW_MINS);
            }
        }
        _ => failure::bail!("protocol error, expected begin ack packet"),
    }

    Ok(())
}

pub fn init_repository(
    r: &mut dyn std::io::Read,
    w: &mut dyn std::io::Write,
    storage_spec: Option<repository::StorageEngineSpec>,
) -> Result<(), failure::Error> {
    write_packet(w, &Packet::TInitRepository(storage_spec))?;
    match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
        Packet::RInitRepository => Ok(()),
        _ => failure::bail!("protocol error, expected begin ack packet"),
    }
}

struct ConnectionHtreeSink<'a, 'b> {
    checkpoint_bytes: u64,
    dirty_bytes: u64,
    send_log_session: &'a Option<std::cell::RefCell<sendlog::SendLogSession<'b>>>,
    r: &'a mut dyn std::io::Read,
    w: &'a mut dyn std::io::Write,
}

impl<'a, 'b> htree::Sink for ConnectionHtreeSink<'a, 'b> {
    fn add_chunk(
        &mut self,
        addr: &Address,
        data: std::vec::Vec<u8>,
    ) -> std::result::Result<(), failure::Error> {
        match self.send_log_session {
            Some(ref send_log_session) => {
                let mut send_log_session = send_log_session.borrow_mut();
                if send_log_session.cached_address(addr)? {
                    send_log_session.add_address(addr)?;
                } else {
                    self.dirty_bytes += data.len() as u64;
                    write_packet(
                        self.w,
                        &Packet::Chunk(Chunk {
                            address: *addr,
                            data,
                        }),
                    )?;
                    send_log_session.add_address(addr)?;
                }

                if self.dirty_bytes >= self.checkpoint_bytes {
                    self.dirty_bytes = 0;
                    write_packet(self.w, &Packet::TSendSync)?;
                    match read_packet(self.r, DEFAULT_MAX_PACKET_SIZE)? {
                        Packet::RSendSync => {
                            send_log_session.checkpoint()?;
                        }
                        _ => failure::bail!("protocol error, expected RSentSync packet"),
                    }
                }

                Ok(())
            }
            None => {
                write_packet(
                    self.w,
                    &Packet::Chunk(Chunk {
                        address: *addr,
                        data,
                    }),
                )?;
                Ok(())
            }
        }
    }
}

pub struct SendContext {
    pub progress: indicatif::ProgressBar,
    pub compression: crypto::DataCompression,
    pub use_stat_cache: bool,
    pub primary_key_id: Xid,
    pub send_key_id: Xid,
    pub hash_key: crypto::HashKey,
    pub data_ectx: crypto::EncryptionContext,
    pub metadata_ectx: crypto::EncryptionContext,
    pub checkpoint_bytes: u64,
}

pub enum DataSource {
    Subprocess(Vec<String>),
    Readable {
        description: String,
        data: Box<dyn std::io::Read>,
    },
    Directory {
        path: std::path::PathBuf,
        exclusions: Vec<glob::Pattern>,
    },
}

pub fn send(
    ctx: &mut SendContext,
    r: &mut dyn std::io::Read,
    w: &mut dyn std::io::Write,
    mut send_log: Option<sendlog::SendLog>,
    tags: BTreeMap<String, String>,
    data: &mut DataSource,
) -> Result<Xid, failure::Error> {
    let send_id = match send_log {
        Some(ref mut send_log) => send_log.last_send_id()?,
        None => None,
    };

    write_packet(w, &Packet::TBeginSend(TBeginSend { delta_id: send_id }))?;

    let ack = match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
        Packet::RBeginSend(ack) => ack,
        _ => failure::bail!("protocol error, expected begin ack packet"),
    };

    'retry: for _i in 0..256 {
        let mut index_tree = None;

        let send_log_session = match send_log {
            Some(ref mut send_log) => Some(std::cell::RefCell::new(
                send_log.session(ack.gc_generation)?,
            )),
            None => None,
        };

        if let Some(ref send_log_session) = send_log_session {
            send_log_session
                .borrow_mut()
                .perform_cache_invalidations(ack.has_delta_id)?;
        }

        let mut sink = ConnectionHtreeSink {
            checkpoint_bytes: ctx.checkpoint_bytes,
            dirty_bytes: 0,
            send_log_session: &send_log_session,
            w,
            r,
        };

        // XXX TODO these chunk parameters need to be investigated and tuned.
        let min_size = 256 * 1024;
        let max_size = 8 * 1024 * 1024;
        let chunk_mask = 0x000f_ffff;

        let mut chunker = chunker::RollsumChunker::new(
            rollsum::Rollsum::new_with_chunk_mask(chunk_mask),
            min_size,
            max_size,
        );
        let mut tw = htree::TreeWriter::new(max_size, chunk_mask);

        match data {
            DataSource::Subprocess(args) => {
                let quoted_args: Vec<String> =
                    args.iter().map(|x| shlex::quote(x).to_string()).collect();
                ctx.progress
                    .set_message(&("exec: ".to_string() + &quoted_args.join(" ")));

                let mut child = std::process::Command::new(args[0].clone())
                    .args(&args[1..])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .spawn()?;
                let mut data = child.stdout.as_mut().unwrap();
                send_chunks(ctx, &mut sink, &mut chunker, &mut tw, &mut data, None)?;
                let status = child.wait()?;
                if !status.success() {
                    failure::bail!("child failed with status {}", status.code().unwrap());
                }
            }
            DataSource::Readable {
                description,
                ref mut data,
            } => {
                ctx.progress.set_message(&description);
                send_chunks(ctx, &mut sink, &mut chunker, &mut tw, data, None)?;
            }
            DataSource::Directory { path, exclusions } => {
                let mut idx_chunker = chunker::RollsumChunker::new(
                    rollsum::Rollsum::new_with_chunk_mask(chunk_mask),
                    min_size,
                    max_size,
                );
                let mut idx_tw = htree::TreeWriter::new(max_size, chunk_mask);

                match send_dir(
                    ctx,
                    &mut sink,
                    &mut chunker,
                    &mut tw,
                    &mut idx_chunker,
                    &mut idx_tw,
                    &send_log_session,
                    &path,
                    &exclusions,
                ) {
                    Ok(()) => {
                        let chunk_data = idx_chunker.finish();
                        let idx_addr = crypto::keyed_content_address(&chunk_data, &ctx.hash_key);
                        idx_tw.add(
                            &mut sink,
                            &idx_addr,
                            ctx.data_ectx.encrypt_data(chunk_data, ctx.compression),
                        )?;

                        let (idx_tree_height, idx_address) = idx_tw.finish(&mut sink)?;

                        index_tree = Some(itemset::HTreeMetadata {
                            height: idx_tree_height,
                            address: idx_address,
                        });
                    }
                    Err(SendDirError::FilesystemModified) => {
                        ctx.progress.println(
                            "filesystem modified while sending, restarting send...".to_string(),
                        );
                        if let Some(ref send_log_session) = send_log_session {
                            write_packet(w, &Packet::TSendSync)?;
                            match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
                                Packet::RSendSync => {
                                    send_log_session.borrow_mut().checkpoint()?;
                                }
                                _ => failure::bail!("protocol error, expected RSentSync packet"),
                            }
                        }
                        continue 'retry;
                    }
                    Err(SendDirError::Other(err)) => return Err(err),
                }
            }
        }

        let chunk_data = chunker.finish();
        let addr = crypto::keyed_content_address(&chunk_data, &ctx.hash_key);
        tw.add(
            &mut sink,
            &addr,
            ctx.data_ectx.encrypt_data(chunk_data, ctx.compression),
        )?;
        let (data_tree_height, data_tree_address) = tw.finish(&mut sink)?;

        let plain_text_metadata = itemset::PlainTextItemMetadata {
            primary_key_id: ctx.primary_key_id,
            data_tree: itemset::HTreeMetadata {
                height: data_tree_height,
                address: data_tree_address,
            },
            index_tree,
        };

        let e_metadata = itemset::EncryptedItemMetadata {
            plain_text_hash: plain_text_metadata.hash(),
            send_key_id: ctx.send_key_id,
            hash_key_part_2: ctx.hash_key.part2.clone(),
            timestamp: chrono::Utc::now(),
            tags,
        };

        ctx.progress.set_message("syncing disks...");

        write_packet(
            w,
            &Packet::TAddItem(AddItem {
                gc_generation: ack.gc_generation,
                item: itemset::VersionedItemMetadata::V1(itemset::ItemMetadata {
                    plain_text_metadata,
                    encrypted_metadata: ctx.metadata_ectx.encrypt_data(
                        serde_bare::to_vec(&e_metadata)?,
                        crypto::DataCompression::Zstd,
                    ),
                }),
            }),
        )?;

        match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
            Packet::RAddItem(id) => {
                if send_log_session.is_some() {
                    send_log_session.unwrap().into_inner().commit(&id)?;
                }
                return Ok(id);
            }
            _ => failure::bail!("protocol error, expected an RAddItem packet"),
        }
    }

    failure::bail!("put retried too many times");
}

fn send_chunks(
    ctx: &mut SendContext,
    sink: &mut dyn htree::Sink,
    chunker: &mut chunker::RollsumChunker,
    tw: &mut htree::TreeWriter,
    data: &mut dyn std::io::Read,
    mut on_chunk: Option<&mut dyn FnMut(&Address)>,
) -> Result<usize, failure::Error> {
    let mut buf: Vec<u8> = vec![0; 1024 * 1024];
    let mut n_written: usize = 0;
    loop {
        match data.read(&mut buf) {
            Ok(0) => {
                return Ok(n_written);
            }
            Ok(n_read) => {
                let mut n_chunked = 0;
                while n_chunked != n_read {
                    let (n, c) = chunker.add_bytes(&buf[n_chunked..n_read]);
                    n_chunked += n;
                    if let Some(chunk_data) = c {
                        let addr = crypto::keyed_content_address(&chunk_data, &ctx.hash_key);
                        let encrypted_chunk =
                            ctx.data_ectx.encrypt_data(chunk_data, ctx.compression);
                        if let Some(ref mut on_chunk) = on_chunk {
                            on_chunk(&addr);
                        }
                        tw.add(sink, &addr, encrypted_chunk)?;
                    }
                }
                ctx.progress.inc(n_read as u64);
                n_written += n_read;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

#[derive(Debug)]
enum SendDirError {
    FilesystemModified,
    Other(failure::Error),
}

impl std::fmt::Display for SendDirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendDirError::FilesystemModified => {
                write!(f, "filesystem modified during directory send.")
            }
            SendDirError::Other(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for SendDirError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

impl From<failure::Error> for SendDirError {
    fn from(err: failure::Error) -> Self {
        SendDirError::Other(err)
    }
}

impl From<std::io::Error> for SendDirError {
    fn from(err: std::io::Error) -> Self {
        SendDirError::Other(err.into())
    }
}

impl From<nix::Error> for SendDirError {
    fn from(err: nix::Error) -> Self {
        SendDirError::Other(err.into())
    }
}

// A smear error is an error likely caused by the filesystem being altered
// by a concurrent process as we are making a snapshot.
fn likely_smear_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
    )
}

fn send_dir(
    ctx: &mut SendContext,
    sink: &mut dyn htree::Sink,
    chunker: &mut chunker::RollsumChunker,
    tw: &mut htree::TreeWriter,
    idx_chunker: &mut chunker::RollsumChunker,
    idx_tw: &mut htree::TreeWriter,
    send_log_session: &Option<std::cell::RefCell<sendlog::SendLogSession>>,
    path: &std::path::PathBuf,
    exclusions: &[glob::Pattern],
) -> Result<(), SendDirError> {
    let path = fsutil::absolute_path(&path)?;

    let mut addresses: Vec<u8> = Vec::new();
    let mut work_list = std::collections::VecDeque::new();
    work_list.push_back(path.clone());

    while let Some(cur_dir) = work_list.pop_front() {
        ctx.progress.set_message(&cur_dir.to_string_lossy());
        addresses.clear();
        let mut hash_state = crypto::HashState::new(Some(&ctx.hash_key));
        // Incorporate the absolute dir in our cache key.
        hash_state.update(cur_dir.as_os_str().as_bytes());
        // Null byte marks the end of path and tar headers in the hash space.
        hash_state.update(&[0]);

        let mut dir_ents = match fsutil::read_dirents(&cur_dir) {
            Ok(dir_ents) => dir_ents,
            Err(err) if likely_smear_error(&err) => return Err(SendDirError::FilesystemModified),
            Err(err) => return Err(SendDirError::Other(err.into())),
        };

        dir_ents.sort_by_key(|a| a.file_name());

        let mut tar_dir_ents = Vec::new();

        if cur_dir == path {
            let metadata = std::fs::metadata(&path)?;
            if !metadata.is_dir() {
                return Err(SendDirError::Other(failure::format_err!(
                    "{} is not a directory",
                    path.display()
                )));
            }
            let tar_path = ".".into();
            let tar_header_bytes = match xtar::dirent_to_tarheader(&metadata, &path, &tar_path) {
                Ok(hdr) => hdr,
                Err(err) if likely_smear_error(&err) => {
                    return Err(SendDirError::FilesystemModified)
                }
                Err(err) => return Err(SendDirError::Other(err.into())),
            };

            hash_state.update(&metadata.ctime().to_le_bytes()[..]);
            hash_state.update(&metadata.ctime_nsec().to_le_bytes()[..]);
            hash_state.update(&tar_header_bytes);
            tar_dir_ents.push((path.clone(), tar_path, metadata, tar_header_bytes));
        }

        'collect_dir_ents: for entry in dir_ents {
            let ent_path = entry.path();

            for excl in exclusions {
                if excl.matches_path(&ent_path) {
                    continue 'collect_dir_ents;
                }
            }

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(err) if likely_smear_error(&err) => {
                    return Err(SendDirError::FilesystemModified)
                }
                Err(err) => return Err(SendDirError::Other(err.into())),
            };
            let tar_path = ent_path.strip_prefix(&path).unwrap().to_path_buf();
            let tar_header_bytes = match xtar::dirent_to_tarheader(&metadata, &ent_path, &tar_path)
            {
                Ok(hdr) => hdr,
                Err(err) if likely_smear_error(&err) => {
                    return Err(SendDirError::FilesystemModified)
                }
                Err(err) => return Err(SendDirError::Other(err.into())),
            };

            if metadata.is_dir() {
                work_list.push_back(ent_path.clone());
            }

            hash_state.update(&metadata.ctime().to_le_bytes()[..]);
            hash_state.update(&metadata.ctime_nsec().to_le_bytes()[..]);
            hash_state.update(&tar_header_bytes);
            tar_dir_ents.push((ent_path, tar_path, metadata, tar_header_bytes));
        }

        let hash = hash_state.finish();

        let cache_lookup = if send_log_session.is_some() && ctx.use_stat_cache {
            send_log_session
                .as_ref()
                .unwrap()
                .borrow_mut()
                .stat_cache_lookup(&hash)?
        } else {
            None
        };

        match cache_lookup {
            Some((size, cached_addresses, cached_index)) => {
                debug_assert!(cached_addresses.len() % ADDRESS_SZ == 0);

                let dir_data_chunk_idx = tw.data_chunk_count();

                let mut address = Address::default();
                for cached_address in cached_addresses.chunks(ADDRESS_SZ) {
                    address.bytes[..].clone_from_slice(cached_address);
                    tw.add_addr(sink, 0, &address)?;
                }

                let mut dir_index: Vec<index::VersionedIndexEntry> =
                    serde_bare::from_slice(&cached_index).unwrap();

                for index_entry in dir_index.iter_mut() {
                    match index_entry {
                        index::VersionedIndexEntry::V1(ref mut index_entry) => {
                            index_entry.data_chunk_idx.0 += dir_data_chunk_idx;
                            index_entry.data_chunk_content_idx.0 += dir_data_chunk_idx;
                            index_entry.data_chunk_content_end_idx.0 += dir_data_chunk_idx;
                            index_entry.data_chunk_end_idx.0 += dir_data_chunk_idx;
                        }
                    }
                    send_chunks(
                        ctx,
                        sink,
                        idx_chunker,
                        idx_tw,
                        &mut std::io::Cursor::new(&serde_bare::to_vec(&index_entry).unwrap()),
                        None,
                    )?;
                }

                ctx.progress.inc(size);

                send_log_session
                    .as_ref()
                    .unwrap()
                    .borrow_mut()
                    .add_stat_cache_data(&hash[..], size, &addresses, &cached_index)?;
            }
            None => {
                let mut total_size: u64 = 0;
                let mut on_chunk = |addr: &Address| {
                    addresses.extend_from_slice(&addr.bytes[..]);
                };

                let dir_data_chunk_idx = tw.data_chunk_count();
                let mut dir_index: Vec<index::VersionedIndexEntry> =
                    Vec::with_capacity(tar_dir_ents.len());

                for (ent_path, tar_path, metadata, header_bytes) in tar_dir_ents.drain(..) {
                    ctx.progress.set_message(&ent_path.to_string_lossy());

                    let mut tar_ent_size = header_bytes.len() as u64;
                    let ent_data_chunk_idx = tw.data_chunk_count();
                    let ent_data_chunk_offset = chunker.buffered_count() as u64;

                    total_size += send_chunks(
                        ctx,
                        sink,
                        chunker,
                        tw,
                        &mut std::io::Cursor::new(header_bytes),
                        Some(&mut on_chunk),
                    )? as u64;

                    let ent_data_chunk_content_idx = tw.data_chunk_count();
                    let ent_data_chunk_content_offset = chunker.buffered_count() as u64;

                    let mut ent_data_chunk_content_end_idx = ent_data_chunk_content_idx;
                    let mut ent_data_chunk_content_end_offset = ent_data_chunk_content_offset;

                    if metadata.is_file() {
                        let mut f = match std::fs::OpenOptions::new()
                            .read(true)
                            .custom_flags(libc::O_NOATIME)
                            .open(&ent_path)
                        {
                            Ok(f) => f,
                            Err(err) if likely_smear_error(&err) => {
                                return Err(SendDirError::FilesystemModified)
                            }
                            Err(err) => return Err(SendDirError::Other(err.into())),
                        };

                        // For linux at least, shift file pages to the tail of the page cache, allowing
                        // the kernel to quickly evict these pages. This works well for the case of system
                        // backups, where we don't to trash the users current cache.
                        // One source on how linux treats this hint - https://lwn.net/Articles/449420
                        nix::fcntl::posix_fadvise(
                            f.as_raw_fd(),
                            0,
                            0,
                            nix::fcntl::PosixFadviseAdvice::POSIX_FADV_NOREUSE,
                        )?;

                        let file_len =
                            send_chunks(ctx, sink, chunker, tw, &mut f, Some(&mut on_chunk))?;

                        tar_ent_size += file_len as u64;
                        total_size += file_len as u64;

                        ent_data_chunk_content_end_idx = tw.data_chunk_count();
                        ent_data_chunk_content_end_offset = chunker.buffered_count() as u64;

                        /* Tar entries are rounded to 512 bytes */
                        let remaining = 512 - (file_len % 512);
                        if remaining < 512 {
                            tar_ent_size += remaining as u64;
                            let buf = [0; 512];
                            total_size += send_chunks(
                                ctx,
                                sink,
                                chunker,
                                tw,
                                &mut std::io::Cursor::new(&buf[..remaining as usize]),
                                Some(&mut on_chunk),
                            )? as u64;
                        }

                        if file_len != metadata.len() as usize {
                            return Err(SendDirError::FilesystemModified);
                        }
                    }

                    let ent_data_chunk_end_idx = tw.data_chunk_count();
                    let ent_data_chunk_end_offset = chunker.buffered_count() as u64;

                    let mut index_entry = index::IndexEntry {
                        path: tar_path.to_string_lossy().to_string(),
                        mode: serde_bare::Uint(metadata.permissions().mode() as u64),
                        size: serde_bare::Uint(if metadata.is_file() {
                            metadata.size()
                        } else {
                            0
                        }),
                        tar_size: serde_bare::Uint(tar_ent_size as u64),
                        ctime: serde_bare::Uint(metadata.ctime() as u64),
                        ctime_nsec: serde_bare::Uint(metadata.ctime_nsec() as u64),
                        data_chunk_idx: serde_bare::Uint(ent_data_chunk_idx - dir_data_chunk_idx),
                        data_chunk_content_idx: serde_bare::Uint(
                            ent_data_chunk_content_idx - dir_data_chunk_idx,
                        ),
                        data_chunk_content_end_idx: serde_bare::Uint(
                            ent_data_chunk_content_end_idx - dir_data_chunk_idx,
                        ),
                        data_chunk_end_idx: serde_bare::Uint(
                            ent_data_chunk_end_idx - dir_data_chunk_idx,
                        ),
                        data_chunk_offset: serde_bare::Uint(ent_data_chunk_offset),
                        data_chunk_content_offset: serde_bare::Uint(ent_data_chunk_content_offset),
                        data_chunk_content_end_offset: serde_bare::Uint(
                            ent_data_chunk_content_end_offset,
                        ),
                        data_chunk_end_offset: serde_bare::Uint(ent_data_chunk_end_offset),
                    };

                    dir_index.push(index::VersionedIndexEntry::V1(index_entry.clone()));

                    index_entry.data_chunk_idx.0 += dir_data_chunk_idx;
                    index_entry.data_chunk_content_idx.0 += dir_data_chunk_idx;
                    index_entry.data_chunk_content_end_idx.0 += dir_data_chunk_idx;
                    index_entry.data_chunk_end_idx.0 += dir_data_chunk_idx;

                    send_chunks(
                        ctx,
                        sink,
                        idx_chunker,
                        idx_tw,
                        &mut std::io::Cursor::new(
                            &serde_bare::to_vec(&index::VersionedIndexEntry::V1(index_entry))
                                .unwrap(),
                        ),
                        None,
                    )?;
                }

                if let Some(chunk_data) = chunker.force_split() {
                    let addr = crypto::keyed_content_address(&chunk_data, &ctx.hash_key);
                    on_chunk(&addr);
                    tw.add(
                        sink,
                        &addr,
                        ctx.data_ectx.encrypt_data(chunk_data, ctx.compression),
                    )?
                }

                if send_log_session.is_some() && ctx.use_stat_cache {
                    send_log_session
                        .as_ref()
                        .unwrap()
                        .borrow_mut()
                        .add_stat_cache_data(
                            &hash[..],
                            total_size,
                            &addresses,
                            &serde_bare::to_vec(&dir_index).unwrap(),
                        )?;
                }
            }
        }
    }

    // The final entry in a tarball is two null files.
    let buf = [0; 1024];
    send_chunks(
        ctx,
        sink,
        chunker,
        tw,
        &mut std::io::Cursor::new(&buf[..]),
        None,
    )?;

    Ok(())
}

pub struct DataRequestContext {
    pub progress: indicatif::ProgressBar,
    pub primary_key_id: Xid,
    pub hash_key_part_1: crypto::PartialHashKey,
    pub data_dctx: crypto::DecryptionContext,
    pub metadata_dctx: crypto::DecryptionContext,
}

pub fn request_data_stream(
    mut ctx: DataRequestContext,
    id: Xid,
    pick: Option<index::PickMap>,
    r: &mut dyn std::io::Read,
    w: &mut dyn std::io::Write,
    out: &mut dyn std::io::Write,
) -> Result<(), failure::Error> {
    write_packet(
        w,
        &Packet::TRequestData(TRequestData {
            id,
            ranges: if let Some(ref pick) = pick {
                Some(pick.data_chunk_ranges.clone())
            } else {
                None
            },
        }),
    )?;

    let metadata = match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
        Packet::RRequestData(resp) => match resp.metadata {
            Some(metadata) => metadata,
            None => failure::bail!("no stored items with the requested id"),
        },
        _ => failure::bail!("protocol error, expected ack request packet"),
    };

    // We only wanted to show the progress bar until we could start getting
    // messages, at this point we know the repository is unlocked.
    ctx.progress.finish_and_clear();

    match metadata {
        itemset::VersionedItemMetadata::V1(metadata) => {
            if ctx.primary_key_id != metadata.plain_text_metadata.primary_key_id {
                failure::bail!("decryption key does not match master key used for encryption");
            }

            let encrypted_metadata = metadata.decrypt_metadata(&mut ctx.metadata_dctx)?;
            let plain_text_metadata = metadata.plain_text_metadata;

            let hash_key =
                crypto::derive_hash_key(&ctx.hash_key_part_1, &encrypted_metadata.hash_key_part_2);

            let mut tr = htree::TreeReader::new(
                plain_text_metadata.data_tree.height,
                &plain_text_metadata.data_tree.address,
            );

            if let Some(pick) = pick {
                receive_partial_htree(ctx, &hash_key, r, &mut tr, pick, out)?;
            } else {
                receive_htree(ctx, &hash_key, r, &mut tr, out)?;
            }

            out.flush()?;
            Ok(())
        }
    }
}

pub fn request_index(
    mut ctx: DataRequestContext,
    id: Xid,
    r: &mut dyn std::io::Read,
    w: &mut dyn std::io::Write,
) -> Result<Vec<index::VersionedIndexEntry>, failure::Error> {
    write_packet(w, &Packet::TRequestIndex(TRequestIndex { id }))?;

    let metadata = match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
        Packet::RRequestIndex(resp) => match resp.metadata {
            Some(metadata) => metadata,
            None => failure::bail!("no stored items with the requested id"),
        },
        _ => failure::bail!("protocol error, expected ack request packet"),
    };

    ctx.progress.set_message("fetching content index...");

    match metadata {
        itemset::VersionedItemMetadata::V1(metadata) => {
            if ctx.primary_key_id != metadata.plain_text_metadata.primary_key_id {
                failure::bail!("decryption key does not match master key used for encryption");
            }

            let encrypted_metadata = metadata.decrypt_metadata(&mut ctx.metadata_dctx)?;
            let plain_text_metadata = metadata.plain_text_metadata;

            let hash_key =
                crypto::derive_hash_key(&ctx.hash_key_part_1, &encrypted_metadata.hash_key_part_2);

            let index_tree = match plain_text_metadata.index_tree {
               Some(index_tree) => index_tree,
                None => failure::bail!("requested item does not have a content index (tarball was not created by bupstash)"),
            };

            let mut tr = htree::TreeReader::new(index_tree.height, &index_tree.address);

            let mut index_data = std::io::Cursor::new(Vec::new());
            receive_htree(ctx, &hash_key, r, &mut tr, &mut index_data)?;

            let mut index: Vec<index::VersionedIndexEntry> = Vec::new();

            let index_data_size = index_data.position();
            index_data.set_position(0);
            while index_data.position() != index_data_size {
                match serde_bare::from_reader(&mut index_data) {
                    Ok(index_entry) => index.push(index_entry),
                    Err(err) => failure::bail!("error deserializing index: {}", err),
                }
            }

            Ok(index)
        }
    }
}

fn receive_htree(
    mut ctx: DataRequestContext,
    hash_key: &crypto::HashKey,
    r: &mut dyn std::io::Read,
    tr: &mut htree::TreeReader,
    out: &mut dyn std::io::Write,
) -> Result<(), failure::Error> {
    while let Some((height, addr)) = tr.next_addr()? {
        let data = match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
            Packet::Chunk(chunk) => {
                if addr != chunk.address {
                    return Err(ClientError::CorruptOrTamperedDataError.into());
                }
                chunk.data
            }
            _ => failure::bail!("protocol error, expected begin chunk packet"),
        };

        if height == 0 {
            let data = ctx.data_dctx.decrypt_data(data)?;
            if addr != crypto::keyed_content_address(&data, &hash_key) {
                return Err(ClientError::CorruptOrTamperedDataError.into());
            }
            out.write_all(&data)?;
        } else {
            if addr != htree::tree_block_address(&data) {
                return Err(ClientError::CorruptOrTamperedDataError.into());
            }
            tr.push_level(height - 1, data)?;
        }
    }

    out.flush()?;
    Ok(())
}

fn receive_partial_htree(
    mut ctx: DataRequestContext,
    hash_key: &crypto::HashKey,
    r: &mut dyn std::io::Read,
    tr: &mut htree::TreeReader,
    pick: index::PickMap,
    out: &mut dyn std::io::Write,
) -> Result<(), failure::Error> {
    let mut n_written: u64 = 0;
    let mut range_idx: usize = 0;
    let mut current_data_chunk_idx: u64 = 0;
    let mut pending_data_chunks = std::collections::VecDeque::new();

    while let Some((height, addr)) = tr.next_addr()? {
        let data = match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
            Packet::Chunk(chunk) => {
                if addr != chunk.address {
                    return Err(ClientError::CorruptOrTamperedDataError.into());
                }
                chunk.data
            }
            _ => failure::bail!("protocol error, expected begin chunk packet"),
        };

        if height == 0 {
            let data = ctx.data_dctx.decrypt_data(data)?;
            if addr != crypto::keyed_content_address(&data, &hash_key) {
                return Err(ClientError::CorruptOrTamperedDataError.into());
            }

            if let Some(chunk_idx) = pending_data_chunks.pop_back() {
                match pick.incomplete_data_chunks.get(&chunk_idx) {
                    Some(ranges) => {
                        for range in ranges.iter() {
                            let data = &data[range.start..std::cmp::min(data.len(), range.end)];
                            n_written += data.len() as u64;
                            out.write_all(data)?;
                        }
                    }
                    None => {
                        n_written += data.len() as u64;
                        out.write_all(&data)?;
                    }
                }
            }
        } else {
            if addr != htree::tree_block_address(&data) {
                return Err(ClientError::CorruptOrTamperedDataError.into());
            }

            if height == 1 {
                let mut filtered_data = Vec::with_capacity(data.len());

                for addr_bytes in data.chunks(ADDRESS_SZ) {
                    if let Some(current_range) = pick.data_chunk_ranges.get(range_idx) {
                        if current_data_chunk_idx >= current_range.start_idx
                            && current_data_chunk_idx <= current_range.end_idx
                        {
                            filtered_data.extend_from_slice(addr_bytes);
                            pending_data_chunks.push_front(current_data_chunk_idx);
                        }
                        current_data_chunk_idx += 1;

                        if current_data_chunk_idx > current_range.end_idx {
                            range_idx += 1;
                        }
                    }
                }

                tr.push_level(height - 1, filtered_data)?;
            } else if pick.data_chunk_ranges.get(range_idx).is_some() {
                tr.push_level(height - 1, data)?;
            }
        }
    }

    debug_assert!(n_written == pick.size);

    if pick.is_subtar {
        // The final entry in a tarball is two null files.
        let buf = [0; 1024];
        out.write_all(&buf[..])?;
    }

    out.flush()?;
    Ok(())
}

pub fn restore_removed(
    progress: indicatif::ProgressBar,
    r: &mut dyn std::io::Read,
    w: &mut dyn std::io::Write,
) -> Result<u64, failure::Error> {
    progress.set_message("restoring items...");

    write_packet(w, &Packet::TRestoreRemoved)?;
    match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
        Packet::RRestoreRemoved(RRestoreRemoved { n_restored }) => Ok(n_restored.0),
        _ => failure::bail!("protocol error, expected restore packet response or progress packet",),
    }
}

pub fn gc(
    progress: indicatif::ProgressBar,
    r: &mut dyn std::io::Read,
    w: &mut dyn std::io::Write,
) -> Result<repository::GCStats, failure::Error> {
    progress.set_message("collecting garbage...");
    write_packet(w, &Packet::TGc(TGc {}))?;

    loop {
        match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
            Packet::Progress(Progress::Notice(msg)) => {
                progress.println(&msg);
            }
            Packet::Progress(Progress::SetMessage(msg)) => {
                progress.set_message(&msg);
            }
            Packet::RGc(rgc) => return Ok(rgc.stats),
            _ => failure::bail!("protocol error, expected gc packet or progress packe."),
        };
    }
}

pub fn sync(
    progress: indicatif::ProgressBar,
    query_cache: &mut querycache::QueryCache,
    r: &mut dyn std::io::Read,
    w: &mut dyn std::io::Write,
) -> Result<(), failure::Error> {
    progress.set_message("syncing remote items...");

    let mut tx = query_cache.transaction()?;

    let after = tx.last_log_op()?;
    let gc_generation = tx.current_gc_generation()?;

    write_packet(
        w,
        &Packet::TRequestItemSync(TRequestItemSync {
            after,
            gc_generation,
        }),
    )?;

    let gc_generation = match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
        Packet::RRequestItemSync(ack) => ack.gc_generation,
        _ => failure::bail!("protocol error, expected items packet"),
    };

    tx.start_sync(gc_generation)?;

    loop {
        match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
            Packet::SyncLogOps(ops) => {
                if ops.is_empty() {
                    break;
                }
                for (opid, item_id, op) in ops {
                    tx.sync_op(opid, item_id, op)?;
                }
            }
            _ => failure::bail!("protocol error, expected items packet"),
        }
    }

    tx.commit()?;
    Ok(())
}

pub fn remove(
    progress: indicatif::ProgressBar,
    ids: Vec<Xid>,
    r: &mut dyn std::io::Read,
    w: &mut dyn std::io::Write,
) -> Result<(), failure::Error> {
    progress.set_message("removing items...");

    for chunked_ids in ids.chunks(4096) {
        let ids = chunked_ids.to_vec();
        write_packet(w, &Packet::TRmItems(ids))?;
        match read_packet(r, DEFAULT_MAX_PACKET_SIZE)? {
            Packet::RRmItems => {}
            _ => failure::bail!("protocol error, expected RRmItems"),
        }
    }
    Ok(())
}

pub fn hangup(w: &mut dyn std::io::Write) -> Result<(), failure::Error> {
    write_packet(w, &Packet::EndOfTransmission)?;
    Ok(())
}
