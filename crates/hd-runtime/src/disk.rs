use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use hd_platform::{DiskProvisionMethodV2, DiskProvisionResultV2, DiskProvisioner, PlatformError};

#[derive(Debug, Clone, Copy, Default)]
pub struct NativeDiskProvisioner;

const ANDROID_SPARSE_MAGIC: u32 = 0xed26_ff3a;
const ANDROID_SPARSE_HEADER_SIZE: usize = 28;
const ANDROID_SPARSE_CHUNK_HEADER_SIZE: usize = 12;
const CHUNK_TYPE_RAW: u16 = 0xcac1;
const CHUNK_TYPE_FILL: u16 = 0xcac2;
const CHUNK_TYPE_DONT_CARE: u16 = 0xcac3;
const CHUNK_TYPE_CRC32: u16 = 0xcac4;
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_HEADER_LBA: u64 = 1;
const GPT_PARTITION_ENTRY_MIN_SIZE: usize = 128;
const LP_METADATA_GEOMETRY_MAGIC: u32 = 0x616c_4467;
const LP_METADATA_GEOMETRY_OFFSETS: [u64; 2] = [4096, 8192];
// Direct Cuttlefish aggregates use a compact EROFS logical-partition super image and a small,
// formattable userdata seed.  These are structural truncation floors, not desktop storage quotas:
// the AOSP Android 15 all-target product is 1704 MiB / 3 MiB respectively.
const MIN_ANDROID_SUPER_BYTES: u64 = 1024 * 1024 * 1024;
const MIN_ANDROID_USERDATA_BYTES: u64 = 3 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct AndroidSparseInfo {
    file_header_size: u16,
    chunk_header_size: u16,
    block_size: u32,
    total_blocks: u32,
    total_chunks: u32,
    expanded_size: u64,
}

#[async_trait]
impl DiskProvisioner for NativeDiskProvisioner {
    async fn provision(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<DiskProvisionResultV2, PlatformError> {
        let source = source.to_owned();
        let destination = destination.to_owned();
        tokio::task::spawn_blocking(move || provision(&source, &destination))
            .await
            .map_err(|error| PlatformError::Process(format!("disk task join failed: {error}")))?
    }
}

fn provision(source: &Path, destination: &Path) -> Result<DiskProvisionResultV2, PlatformError> {
    let source_metadata = regular_file_metadata(source, "read base disk metadata")?;
    let sparse = inspect_android_sparse(source)?;
    let expected_size = sparse.map_or(source_metadata.len(), |info| info.expanded_size);
    if destination.exists() {
        let destination_metadata = regular_file_metadata(destination, "read overlay metadata")?;
        if destination_metadata.len() != expected_size {
            return Err(PlatformError::Process(format!(
                "existing overlay {} has {} bytes, expected {}",
                destination.display(),
                destination_metadata.len(),
                expected_size
            )));
        }
        validate_android_aggregate(destination)?;
        return Ok(DiskProvisionResultV2 {
            path: destination.to_owned(),
            method: DiskProvisionMethodV2::ExistingVerified,
            bytes: destination_metadata.len(),
        });
    }
    let parent = destination.parent().ok_or_else(|| {
        PlatformError::Process(format!("overlay has no parent: {}", destination.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(|source_error| PlatformError::Io {
        operation: "create overlay directory",
        path: parent.to_owned(),
        source: source_error,
    })?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("overlay"),
        uuid::Uuid::new_v4()
    ));
    let mut temporary_guard = TemporaryDisk::new(temporary.clone());
    let method = if let Some(info) = sparse {
        expand_android_sparse(source, &temporary, info)?;
        DiskProvisionMethodV2::AndroidSparseExpanded
    } else if reflink_copy::reflink(source, &temporary).is_ok() {
        DiskProvisionMethodV2::BlockClone
    } else {
        let available = fs2::available_space(parent).map_err(|source_error| PlatformError::Io {
            operation: "query overlay free space",
            path: parent.to_owned(),
            source: source_error,
        })?;
        if available < source_metadata.len().saturating_add(1024 * 1024 * 1024) {
            return Err(PlatformError::Process(format!(
                "insufficient disk space for {} byte overlay; {} bytes available",
                source_metadata.len(),
                available
            )));
        }
        std::fs::copy(source, &temporary).map_err(|source_error| PlatformError::Io {
            operation: "copy private overlay",
            path: temporary.clone(),
            source: source_error,
        })?;
        DiskProvisionMethodV2::FullCopy
    };
    let temporary_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temporary)
        .map_err(|source_error| PlatformError::Io {
            operation: "open temporary overlay",
            path: temporary.clone(),
            source: source_error,
        })?;
    temporary_file
        .sync_all()
        .map_err(|source_error| PlatformError::Io {
            operation: "sync temporary overlay",
            path: temporary.clone(),
            source: source_error,
        })?;
    drop(temporary_file);
    let copied_metadata = regular_file_metadata(&temporary, "verify temporary overlay")?;
    if copied_metadata.len() != expected_size {
        let _ = std::fs::remove_file(&temporary);
        return Err(PlatformError::Process(format!(
            "overlay copy length mismatch: expected {}, actual {}",
            expected_size,
            copied_metadata.len()
        )));
    }
    validate_android_aggregate(&temporary)?;
    hd_platform::replace_file(&temporary, destination)?;
    temporary_guard.disarm();
    Ok(DiskProvisionResultV2 {
        path: destination.to_owned(),
        method,
        bytes: copied_metadata.len(),
    })
}

pub fn validate_android_fstab(path: &Path) -> Result<(), PlatformError> {
    let text = std::fs::read_to_string(path).map_err(|source| PlatformError::Io {
        operation: "read Android fstab",
        path: path.to_owned(),
        source,
    })?;
    let mut mount_filesystems = BTreeMap::<String, BTreeSet<String>>::new();
    let mut data_profiles = Vec::<(BTreeSet<String>, BTreeSet<String>)>::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let columns = line.split_whitespace().collect::<Vec<_>>();
        if columns.len() != 5 {
            return Err(PlatformError::Process(format!(
                "Android fstab {} line {} has {} columns; expected 5",
                path.display(),
                index + 1,
                columns.len()
            )));
        }
        if !columns[1].starts_with('/') {
            continue;
        }
        mount_filesystems
            .entry(columns[1].to_owned())
            .or_default()
            .insert(columns[2].to_owned());
        if columns[1] == "/data" {
            data_profiles.push((
                columns[3].split(',').map(str::to_owned).collect(),
                columns[4].split(',').map(str::to_owned).collect(),
            ));
        }
    }
    let unsupported_alternatives = mount_filesystems
        .iter()
        .filter(|(_, filesystems)| filesystems.len() > 1)
        .filter(|(mount_point, filesystems)| {
            !is_supported_aosp_filesystem_alternative(mount_point, filesystems)
        })
        .map(|(mount_point, filesystems)| {
            format!(
                "{mount_point}={}",
                filesystems.iter().cloned().collect::<Vec<_>>().join("|")
            )
        })
        .collect::<Vec<_>>();
    if !unsupported_alternatives.is_empty() {
        return Err(PlatformError::Process(format!(
            "Android fstab {} has unsupported filesystem alternatives ({}); only AOSP ext4|erofs logical-partition and ext4|f2fs userdata alternatives are accepted",
            path.display(),
            unsupported_alternatives.join(", ")
        )));
    }
    let missing = ["/system", "/data", "/metadata"]
        .into_iter()
        .filter(|mount_point| !mount_filesystems.contains_key(*mount_point))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(PlatformError::Process(format!(
            "Android fstab {} is missing required mounts {}; regenerate the Guest artifact",
            path.display(),
            missing.join(", ")
        )));
    }
    if data_profiles.is_empty() || data_profiles.len() > 2 {
        return Err(PlatformError::Process(format!(
            "Android fstab {} has {} /data entries; expected one profile or the AOSP ext4|f2fs alternative pair",
            path.display(),
            data_profiles.len()
        )));
    }
    let data_encryption = data_profiles
        .iter()
        .map(|(mount_flags, fs_mgr_flags)| {
            mount_flags.contains("inlinecrypt")
                || fs_mgr_flags.iter().any(|flag| {
                    flag.starts_with("keydirectory=") || flag.starts_with("fileencryption=")
                })
        })
        .collect::<BTreeSet<_>>();
    if data_encryption.len() != 1 {
        return Err(PlatformError::Process(format!(
            "Android fstab {} mixes encrypted and unencrypted /data alternatives",
            path.display()
        )));
    }
    let data_encrypted = data_encryption.contains(&true);
    if !data_encrypted
        && data_profiles.iter().any(|(_, fs_mgr_flags)| {
            !fs_mgr_flags.contains("first_stage_mount") || fs_mgr_flags.contains("latemount")
        })
    {
        return Err(PlatformError::Process(format!(
            "Android fstab {} has an unencrypted /data profile that is not first-stage-only; HD passes this fstab through crosvm's Android DT, so a late /data entry would be mounted twice. Regenerate with --data-encryption none --fstab-suffix hd.direct",
            path.display()
        )));
    }
    Ok(())
}

fn is_supported_aosp_filesystem_alternative(
    mount_point: &str,
    filesystems: &BTreeSet<String>,
) -> bool {
    let expected = match mount_point {
        "/data" => ["ext4", "f2fs"],
        "/odm" | "/odm_dlkm" | "/product" | "/system" | "/system_dlkm" | "/system_ext"
        | "/vendor" | "/vendor_dlkm" => ["erofs", "ext4"],
        _ => return false,
    };
    filesystems.len() == expected.len()
        && expected
            .into_iter()
            .all(|filesystem| filesystems.contains(filesystem))
}

#[derive(Debug, Clone, Copy)]
struct GptEntryTable {
    offset: u64,
    count: u32,
    entry_size: usize,
}

fn android_gpt_entry_table(
    file: &mut File,
    path: &Path,
    file_len: u64,
) -> Result<Option<GptEntryTable>, PlatformError> {
    let header_offset = GPT_HEADER_LBA * 512;
    if file_len < header_offset + 92 {
        return Ok(None);
    }
    let mut header = [0_u8; 92];
    file.seek(SeekFrom::Start(header_offset))
        .and_then(|_| file.read_exact(&mut header))
        .map_err(|source| PlatformError::Io {
            operation: "read Android aggregate GPT header",
            path: path.to_owned(),
            source,
        })?;
    if &header[..GPT_SIGNATURE.len()] != GPT_SIGNATURE {
        // The provisioner is also exercised by format-specific fixtures. Actual HD Android
        // artifacts use GPT and are validated below.
        return Ok(None);
    }
    let entries_lba = u64::from_le_bytes(
        header[72..80]
            .try_into()
            .expect("GPT entry LBA is eight bytes"),
    );
    let count = u32::from_le_bytes(
        header[80..84]
            .try_into()
            .expect("GPT entry count is four bytes"),
    );
    let entry_size_raw = u32::from_le_bytes(
        header[84..88]
            .try_into()
            .expect("GPT entry size is four bytes"),
    );
    let entry_size = usize::try_from(entry_size_raw).map_err(|_| {
        PlatformError::Process(format!(
            "Android aggregate {} has an unsupported GPT entry size",
            path.display()
        ))
    })?;
    if count == 0 || count > 4096 || !(GPT_PARTITION_ENTRY_MIN_SIZE..=4096).contains(&entry_size) {
        return Err(PlatformError::Process(format!(
            "Android aggregate {} has invalid GPT entry geometry",
            path.display()
        )));
    }
    let offset = entries_lba.checked_mul(512).ok_or_else(|| {
        PlatformError::Process(format!(
            "Android aggregate {} GPT entry offset overflows",
            path.display()
        ))
    })?;
    let table_bytes = u64::from(count)
        .checked_mul(u64::from(entry_size_raw))
        .ok_or_else(|| {
            PlatformError::Process(format!(
                "Android aggregate {} GPT entry table size overflows",
                path.display()
            ))
        })?;
    if offset
        .checked_add(table_bytes)
        .is_none_or(|end| end > file_len)
    {
        return Err(PlatformError::Process(format!(
            "Android aggregate {} has a truncated GPT entry table",
            path.display()
        )));
    }
    Ok(Some(GptEntryTable {
        offset,
        count,
        entry_size,
    }))
}

fn parse_android_gpt_partition(
    entry: &[u8],
    path: &Path,
    file_len: u64,
) -> Result<Option<(String, u64, u64)>, PlatformError> {
    if entry[..16].iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    let first_lba = u64::from_le_bytes(entry[32..40].try_into().expect("first LBA is eight bytes"));
    let last_lba = u64::from_le_bytes(entry[40..48].try_into().expect("last LBA is eight bytes"));
    if last_lba < first_lba {
        return Err(PlatformError::Process(format!(
            "Android aggregate {} has an invalid GPT partition extent",
            path.display()
        )));
    }
    let name_units = entry[56..128]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect::<Vec<_>>();
    let name = String::from_utf16(&name_units).map_err(|_| {
        PlatformError::Process(format!(
            "Android aggregate {} has an invalid GPT partition name",
            path.display()
        ))
    })?;
    let sectors = last_lba
        .checked_sub(first_lba)
        .and_then(|sectors| sectors.checked_add(1))
        .ok_or_else(|| {
            PlatformError::Process(format!(
                "Android aggregate {} partition size overflows",
                path.display()
            ))
        })?;
    let size = sectors.checked_mul(512).ok_or_else(|| {
        PlatformError::Process(format!(
            "Android aggregate {} partition size overflows",
            path.display()
        ))
    })?;
    let end = last_lba
        .checked_add(1)
        .and_then(|sectors| sectors.checked_mul(512))
        .ok_or_else(|| {
            PlatformError::Process(format!(
                "Android aggregate {} partition extent overflows",
                path.display()
            ))
        })?;
    if end > file_len {
        return Err(PlatformError::Process(format!(
            "Android aggregate {} partition {name} extends past the disk",
            path.display()
        )));
    }
    let offset = first_lba.checked_mul(512).ok_or_else(|| {
        PlatformError::Process(format!(
            "Android aggregate {} partition offset overflows",
            path.display()
        ))
    })?;
    Ok(Some((name, offset, size)))
}

fn android_gpt_partitions(
    file: &mut File,
    path: &Path,
    file_len: u64,
    table: GptEntryTable,
) -> Result<BTreeMap<String, (u64, u64)>, PlatformError> {
    file.seek(SeekFrom::Start(table.offset))
        .map_err(|source| PlatformError::Io {
            operation: "seek Android aggregate GPT entries",
            path: path.to_owned(),
            source,
        })?;
    let mut entry = vec![0_u8; table.entry_size];
    let mut partitions = BTreeMap::new();
    for _ in 0..table.count {
        file.read_exact(&mut entry)
            .map_err(|source| PlatformError::Io {
                operation: "read Android aggregate GPT entry",
                path: path.to_owned(),
                source,
            })?;
        if let Some((name, offset, size)) = parse_android_gpt_partition(&entry, path, file_len)? {
            partitions.insert(name, (offset, size));
        }
    }
    Ok(partitions)
}

pub fn validate_android_aggregate(path: &Path) -> Result<(), PlatformError> {
    let mut file = File::open(path).map_err(|source| PlatformError::Io {
        operation: "open Android aggregate",
        path: path.to_owned(),
        source,
    })?;
    let file_len = file
        .metadata()
        .map_err(|source| PlatformError::Io {
            operation: "read Android aggregate metadata",
            path: path.to_owned(),
            source,
        })?
        .len();
    let Some(table) = android_gpt_entry_table(&mut file, path, file_len)? else {
        return Ok(());
    };
    let partitions = android_gpt_partitions(&mut file, path, file_len, table)?;
    for (name, minimum) in [
        ("super", MIN_ANDROID_SUPER_BYTES),
        ("userdata", MIN_ANDROID_USERDATA_BYTES),
    ] {
        let (_, size) = partitions.get(name).copied().ok_or_else(|| {
            PlatformError::Process(format!(
                "Android aggregate {} is missing the {name} partition",
                path.display()
            ))
        })?;
        if size < minimum {
            return Err(PlatformError::Process(format!(
                "Android aggregate {} has a {name} partition of {size} bytes; at least {minimum} bytes are required. Regenerate the Guest artifact from the Android sparse image's logical size",
                path.display()
            )));
        }
    }
    let (super_offset, super_size) = partitions
        .get("super")
        .copied()
        .expect("super presence was checked above");
    let mut magic = [0_u8; 4];
    file.seek(SeekFrom::Start(super_offset))
        .and_then(|_| file.read_exact(&mut magic))
        .map_err(|source| PlatformError::Io {
            operation: "read Android aggregate super header",
            path: path.to_owned(),
            source,
        })?;
    if u32::from_le_bytes(magic) == ANDROID_SPARSE_MAGIC {
        return Err(PlatformError::Process(format!(
            "Android aggregate {} embeds an unexpanded Android sparse image inside its super partition; regenerate the aggregate while expanding super.img",
            path.display()
        )));
    }
    for geometry_offset in LP_METADATA_GEOMETRY_OFFSETS {
        if geometry_offset + u64::try_from(magic.len()).expect("magic length fits u64") > super_size
        {
            return Err(PlatformError::Process(format!(
                "Android aggregate {} has a truncated super metadata geometry region",
                path.display()
            )));
        }
        file.seek(SeekFrom::Start(super_offset + geometry_offset))
            .and_then(|_| file.read_exact(&mut magic))
            .map_err(|source| PlatformError::Io {
                operation: "read Android aggregate super metadata geometry",
                path: path.to_owned(),
                source,
            })?;
        if u32::from_le_bytes(magic) != LP_METADATA_GEOMETRY_MAGIC {
            return Err(PlatformError::Process(format!(
                "Android aggregate {} has invalid logical-partition geometry at super offset {geometry_offset}; regenerate it from a valid expanded super.img",
                path.display()
            )));
        }
    }
    Ok(())
}

fn inspect_android_sparse(path: &Path) -> Result<Option<AndroidSparseInfo>, PlatformError> {
    let mut file = File::open(path).map_err(|source| PlatformError::Io {
        operation: "open base disk",
        path: path.to_owned(),
        source,
    })?;
    let mut header = [0_u8; ANDROID_SPARSE_HEADER_SIZE];
    match file.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(source) => {
            return Err(PlatformError::Io {
                operation: "read base disk header",
                path: path.to_owned(),
                source,
            });
        }
    }
    if u32::from_le_bytes(header[0..4].try_into().expect("four-byte sparse magic"))
        != ANDROID_SPARSE_MAGIC
    {
        return Ok(None);
    }
    let major = u16::from_le_bytes(header[4..6].try_into().expect("two-byte major version"));
    let file_header_size =
        u16::from_le_bytes(header[8..10].try_into().expect("two-byte file header size"));
    let chunk_header_size = u16::from_le_bytes(
        header[10..12]
            .try_into()
            .expect("two-byte chunk header size"),
    );
    let block_size = u32::from_le_bytes(header[12..16].try_into().expect("four-byte block size"));
    let total_blocks =
        u32::from_le_bytes(header[16..20].try_into().expect("four-byte block count"));
    let total_chunks =
        u32::from_le_bytes(header[20..24].try_into().expect("four-byte chunk count"));
    if major != 1
        || usize::from(file_header_size) < ANDROID_SPARSE_HEADER_SIZE
        || usize::from(chunk_header_size) < ANDROID_SPARSE_CHUNK_HEADER_SIZE
        || block_size == 0
        || total_blocks == 0
    {
        return Err(PlatformError::Process(format!(
            "unsupported Android sparse header in {}",
            path.display()
        )));
    }
    let expanded_size = u64::from(block_size)
        .checked_mul(u64::from(total_blocks))
        .ok_or_else(|| {
            PlatformError::Process(format!(
                "Android sparse expanded size overflows in {}",
                path.display()
            ))
        })?;
    Ok(Some(AndroidSparseInfo {
        file_header_size,
        chunk_header_size,
        block_size,
        total_blocks,
        total_chunks,
        expanded_size,
    }))
}

#[allow(clippy::too_many_lines)]
fn expand_android_sparse(
    source_path: &Path,
    destination_path: &Path,
    info: AndroidSparseInfo,
) -> Result<(), PlatformError> {
    let mut source = File::open(source_path).map_err(|source| PlatformError::Io {
        operation: "open Android sparse source",
        path: source_path.to_owned(),
        source,
    })?;
    source
        .seek(SeekFrom::Start(u64::from(info.file_header_size)))
        .map_err(|source| PlatformError::Io {
            operation: "seek Android sparse chunks",
            path: source_path.to_owned(),
            source,
        })?;
    let mut destination = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(destination_path)
        .map_err(|source| PlatformError::Io {
            operation: "create expanded sparse overlay",
            path: destination_path.to_owned(),
            source,
        })?;
    hd_platform::mark_file_sparse(&destination, destination_path)?;
    destination
        .set_len(info.expanded_size)
        .map_err(|source| PlatformError::Io {
            operation: "size expanded sparse overlay",
            path: destination_path.to_owned(),
            source,
        })?;

    let mut expanded_blocks = 0_u64;
    let mut chunk_header = vec![0_u8; usize::from(info.chunk_header_size)];
    for _ in 0..info.total_chunks {
        source
            .read_exact(&mut chunk_header)
            .map_err(|source| PlatformError::Io {
                operation: "read Android sparse chunk header",
                path: source_path.to_owned(),
                source,
            })?;
        let chunk_type = u16::from_le_bytes(
            chunk_header[0..2]
                .try_into()
                .expect("two-byte sparse chunk type"),
        );
        let chunk_blocks = u32::from_le_bytes(
            chunk_header[4..8]
                .try_into()
                .expect("four-byte sparse chunk block count"),
        );
        let total_size = u32::from_le_bytes(
            chunk_header[8..12]
                .try_into()
                .expect("four-byte sparse chunk size"),
        );
        let data_size = total_size
            .checked_sub(u32::from(info.chunk_header_size))
            .ok_or_else(|| invalid_sparse_chunk(source_path, chunk_type))?;
        let output_size = u64::from(chunk_blocks)
            .checked_mul(u64::from(info.block_size))
            .ok_or_else(|| invalid_sparse_chunk(source_path, chunk_type))?;
        let output_offset = expanded_blocks
            .checked_mul(u64::from(info.block_size))
            .ok_or_else(|| invalid_sparse_chunk(source_path, chunk_type))?;
        destination
            .seek(SeekFrom::Start(output_offset))
            .map_err(|source| PlatformError::Io {
                operation: "seek expanded sparse overlay",
                path: destination_path.to_owned(),
                source,
            })?;
        match chunk_type {
            CHUNK_TYPE_RAW if u64::from(data_size) == output_size => {
                let copied = io::copy(
                    &mut std::io::Read::by_ref(&mut source).take(output_size),
                    &mut destination,
                )
                .map_err(|source| PlatformError::Io {
                    operation: "expand Android sparse raw chunk",
                    path: destination_path.to_owned(),
                    source,
                })?;
                if copied != output_size {
                    return Err(invalid_sparse_chunk(source_path, chunk_type));
                }
            }
            CHUNK_TYPE_FILL if data_size == 4 => {
                let mut fill = [0_u8; 4];
                source
                    .read_exact(&mut fill)
                    .map_err(|source| PlatformError::Io {
                        operation: "read Android sparse fill chunk",
                        path: source_path.to_owned(),
                        source,
                    })?;
                if fill != [0; 4] {
                    write_fill(&mut destination, output_size, fill).map_err(|source| {
                        PlatformError::Io {
                            operation: "expand Android sparse fill chunk",
                            path: destination_path.to_owned(),
                            source,
                        }
                    })?;
                }
            }
            CHUNK_TYPE_DONT_CARE if data_size == 0 => {}
            CHUNK_TYPE_CRC32 if data_size == 4 && chunk_blocks == 0 => {
                let mut checksum = [0_u8; 4];
                source
                    .read_exact(&mut checksum)
                    .map_err(|source| PlatformError::Io {
                        operation: "read Android sparse checksum chunk",
                        path: source_path.to_owned(),
                        source,
                    })?;
            }
            _ => return Err(invalid_sparse_chunk(source_path, chunk_type)),
        }
        expanded_blocks = expanded_blocks
            .checked_add(u64::from(chunk_blocks))
            .ok_or_else(|| invalid_sparse_chunk(source_path, chunk_type))?;
        if expanded_blocks > u64::from(info.total_blocks) {
            return Err(invalid_sparse_chunk(source_path, chunk_type));
        }
    }
    if expanded_blocks != u64::from(info.total_blocks)
        || source
            .stream_position()
            .map_err(|source| PlatformError::Io {
                operation: "inspect Android sparse source length",
                path: source_path.to_owned(),
                source,
            })?
            != source
                .metadata()
                .map_err(|source| PlatformError::Io {
                    operation: "inspect Android sparse source metadata",
                    path: source_path.to_owned(),
                    source,
                })?
                .len()
    {
        return Err(PlatformError::Process(format!(
            "Android sparse chunk layout is inconsistent in {}",
            source_path.display()
        )));
    }
    Ok(())
}

fn invalid_sparse_chunk(path: &Path, chunk_type: u16) -> PlatformError {
    PlatformError::Process(format!(
        "invalid Android sparse chunk 0x{chunk_type:04x} in {}",
        path.display()
    ))
}

fn write_fill(destination: &mut File, size: u64, pattern: [u8; 4]) -> io::Result<()> {
    let mut buffer = vec![0_u8; 64 * 1024];
    for (index, byte) in buffer.iter_mut().enumerate() {
        *byte = pattern[index % pattern.len()];
    }
    let mut remaining = size;
    while remaining != 0 {
        let amount = usize::try_from(
            remaining.min(u64::try_from(buffer.len()).expect("fill buffer length fits in u64")),
        )
        .expect("bounded fill amount fits in usize");
        destination.write_all(&buffer[..amount])?;
        remaining -= u64::try_from(amount).expect("fill amount fits in u64");
    }
    Ok(())
}

#[derive(Debug)]
struct TemporaryDisk {
    path: PathBuf,
    armed: bool,
}

impl TemporaryDisk {
    const fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryDisk {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn regular_file_metadata(
    path: &Path,
    operation: &'static str,
) -> Result<std::fs::Metadata, PlatformError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| PlatformError::Io {
        operation,
        path: path.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(PlatformError::Process(format!(
            "{} is not a regular non-symlink file",
            path.display()
        )));
    }
    Ok(metadata)
}

pub fn unique_disk_path(root: &Path, instance_id: uuid::Uuid) -> PathBuf {
    root.join(format!("{instance_id}.img"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_sparse(path: &Path) {
        let block_size = 4096_u32;
        let file_header_size =
            u16::try_from(ANDROID_SPARSE_HEADER_SIZE).expect("sparse header size fits in u16");
        let chunk_header_size = u16::try_from(ANDROID_SPARSE_CHUNK_HEADER_SIZE)
            .expect("sparse chunk header size fits in u16");
        let chunk_header_size_u32 = u32::from(chunk_header_size);
        let mut file = File::create(path).expect("create sparse fixture");
        file.write_all(&ANDROID_SPARSE_MAGIC.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&0_u16.to_le_bytes()).unwrap();
        file.write_all(&file_header_size.to_le_bytes()).unwrap();
        file.write_all(&chunk_header_size.to_le_bytes()).unwrap();
        file.write_all(&block_size.to_le_bytes()).unwrap();
        file.write_all(&3_u32.to_le_bytes()).unwrap();
        file.write_all(&3_u32.to_le_bytes()).unwrap();
        file.write_all(&0_u32.to_le_bytes()).unwrap();

        file.write_all(&CHUNK_TYPE_RAW.to_le_bytes()).unwrap();
        file.write_all(&0_u16.to_le_bytes()).unwrap();
        file.write_all(&1_u32.to_le_bytes()).unwrap();
        file.write_all(&(block_size + chunk_header_size_u32).to_le_bytes())
            .unwrap();
        file.write_all(&vec![
            0x7a;
            usize::try_from(block_size)
                .expect("block size fits in usize")
        ])
        .unwrap();

        file.write_all(&CHUNK_TYPE_DONT_CARE.to_le_bytes()).unwrap();
        file.write_all(&0_u16.to_le_bytes()).unwrap();
        file.write_all(&1_u32.to_le_bytes()).unwrap();
        file.write_all(&chunk_header_size_u32.to_le_bytes())
            .unwrap();

        file.write_all(&CHUNK_TYPE_FILL.to_le_bytes()).unwrap();
        file.write_all(&0_u16.to_le_bytes()).unwrap();
        file.write_all(&1_u32.to_le_bytes()).unwrap();
        file.write_all(&(chunk_header_size_u32 + 4).to_le_bytes())
            .unwrap();
        file.write_all(&[1, 2, 3, 4]).unwrap();
    }

    #[test]
    fn expands_android_sparse_to_reusable_writable_overlay() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("aggregate_android.sparse.img");
        let destination = directory.path().join("overlay.img");
        write_test_sparse(&source);

        let first = provision(&source, &destination).unwrap();
        assert_eq!(first.method, DiskProvisionMethodV2::AndroidSparseExpanded);
        assert_eq!(first.bytes, 3 * 4096);
        let bytes = std::fs::read(&destination).unwrap();
        assert!(bytes[..4096].iter().all(|byte| *byte == 0x7a));
        assert!(bytes[4096..8192].iter().all(|byte| *byte == 0));
        for (index, byte) in bytes[8192..].iter().enumerate() {
            assert_eq!(*byte, [1, 2, 3, 4][index % 4]);
        }

        let second = provision(&source, &destination).unwrap();
        assert_eq!(second.method, DiskProvisionMethodV2::ExistingVerified);
        assert_eq!(second.bytes, 3 * 4096);
    }
}
