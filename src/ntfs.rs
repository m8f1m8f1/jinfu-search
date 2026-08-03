//! NTFS USN 记录解析与路径树重建。
//!
//! 这里不把 Win32 返回的可变长缓冲区直接转成 Rust 引用，而是逐字段做边界
//! 校验，避免损坏记录或未来布局变化导致越界读取。

use std::{
    collections::{HashMap, HashSet},
    io,
    mem::size_of,
    ptr,
};

use thiserror::Error;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_HANDLE_EOF, ERROR_JOURNAL_NOT_ACTIVE, GENERIC_READ, GENERIC_WRITE,
        HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    System::{
        IO::DeviceIoControl,
        Ioctl::{
            FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
            USN_REASON_FILE_DELETE,
        },
    },
};

pub const ROOT_FILE_REFERENCE: u64 = 5;
const FILE_REFERENCE_NUMBER_MASK: u64 = (1_u64 << 48) - 1;
const JOURNAL_BUFFER_BYTES: usize = 64 * 1024;
const MAX_JOURNAL_BATCHES_PER_SYNC: usize = 128;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum NtfsError {
    #[error("USN 缓冲区不完整：{0}")]
    Malformed(&'static str),
    #[error("不支持的 USN 主版本：{0}")]
    UnsupportedVersion(u16),
}

pub type Result<T> = std::result::Result<T, NtfsError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRecord {
    pub file_reference_number: u64,
    pub parent_file_reference_number: u64,
    pub usn: i64,
    pub timestamp_filetime: i64,
    pub reason: u32,
    pub file_attributes: u32,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct MftSnapshot {
    pub volume: char,
    pub journal_id: u64,
    pub next_usn: i64,
    pub records: Vec<UsnRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
}

#[derive(Debug, Clone)]
pub struct JournalDelta {
    pub journal: JournalInfo,
    pub next_usn: i64,
    pub records: Vec<UsnRecord>,
}

#[derive(Debug, Error)]
pub enum JournalReadError {
    #[error("USN 日志已重置或请求游标已被截断")]
    Reset,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Parse(#[from] NtfsError),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VolumeProbe {
    pub volume: char,
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub first_batch_records: usize,
}

/// 读取 NTFS 主文件表的轻量元数据快照。只读取卷元数据，不打开或读取单个文件。
///
/// 调用者必须已经取得卷句柄所需的系统权限；遇到权限错误时应回退到目录扫描，
/// 而不是尝试创建或修改 USN 日志。
pub fn snapshot_volume(volume: char) -> io::Result<MftSnapshot> {
    let volume = normalize_volume_letter(volume)?;
    let handle = VolumeHandle::open(volume)?;
    let journal = match query_journal(handle.0) {
        Ok(journal) => Some(journal),
        Err(error) if error.raw_os_error() == Some(ERROR_JOURNAL_NOT_ACTIVE as i32) => None,
        Err(error) => return Err(error),
    };
    let mut request = MftEnumDataV0 {
        start_file_reference_number: 0,
        low_usn: 0,
        // MFT 枚举本身不要求卷上存在 USN Journal。日志未启用时用最大
        // 边界完成一次性快照，但返回 0 游标，调用方不得据此做增量同步。
        high_usn: journal
            .as_ref()
            .map(|journal| journal.next_usn)
            .unwrap_or(i64::MAX),
    };
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut records = Vec::new();

    loop {
        let (next_reference, batch) = match enumerate_mft_batch(handle.0, &request, &mut buffer) {
            Ok(bytes_returned) => bytes_returned,
            Err(error) if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) => break,
            Err(error) => return Err(error),
        };
        if batch.is_empty() {
            break;
        }
        records.extend(batch);
        if next_reference == request.start_file_reference_number {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "FSCTL_ENUM_USN_DATA 未推进文件引用号",
            ));
        }
        request.start_file_reference_number = next_reference;
    }

    Ok(MftSnapshot {
        volume,
        journal_id: journal
            .as_ref()
            .map(|journal| journal.journal_id)
            .unwrap_or(0),
        next_usn: journal.map(|journal| journal.next_usn).unwrap_or(0),
        records,
    })
}

/// 重放从给定 USN 游标之后发生的关闭态变更记录。无变更时立即返回空记录，
/// 不轮询也不修改日志。
pub fn read_journal_since(
    volume: char,
    expected_journal_id: u64,
    start_usn: i64,
) -> std::result::Result<JournalDelta, JournalReadError> {
    let volume = normalize_volume_letter(volume)?;
    let handle = VolumeHandle::open(volume)?;
    let journal = query_journal(handle.0)?;
    if journal.journal_id != expected_journal_id || start_usn < journal.first_usn {
        return Err(JournalReadError::Reset);
    }

    let mut request = ReadUsnJournalDataV0 {
        start_usn,
        reason_mask: u32::MAX,
        return_only_on_close: 1,
        timeout: 0,
        bytes_to_wait_for: 0,
        journal_id: expected_journal_id,
    };
    let mut buffer = vec![0_u8; JOURNAL_BUFFER_BYTES];
    let mut records = Vec::new();

    for _ in 0..MAX_JOURNAL_BATCHES_PER_SYNC {
        let bytes_returned = unsafe {
            device_io_control(
                handle.0,
                FSCTL_READ_USN_JOURNAL,
                (&request as *const ReadUsnJournalDataV0).cast(),
                size_of::<ReadUsnJournalDataV0>(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        }?;
        if bytes_returned < size_of::<i64>() {
            return Err(JournalReadError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "FSCTL_READ_USN_JOURNAL 返回的缓冲区不足 8 字节",
            )));
        }
        let response = &buffer[..bytes_returned];
        let next_usn = i64::from_le_bytes(
            response[..8]
                .try_into()
                .expect("前置长度已经验证为至少 8 字节"),
        );
        let batch = parse_usn_buffer(response)?;
        records.extend(batch);
        if next_usn == request.start_usn || next_usn >= journal.next_usn {
            return Ok(JournalDelta {
                journal,
                next_usn,
                records,
            });
        }
        request.start_usn = next_usn;
    }

    // 高写入压力下把工作切成有限批次；下次同步从该游标继续，避免托盘线程
    // 被一次无限增长的 USN 队列长期占用。
    Ok(JournalDelta {
        journal,
        next_usn: request.start_usn,
        records,
    })
}

/// 只验证卷、USN 日志和一个 MFT 批次；不会写入索引数据库，也不会遍历整卷。
pub fn probe_volume(volume: char) -> io::Result<VolumeProbe> {
    let volume = normalize_volume_letter(volume)?;
    let handle = VolumeHandle::open(volume)?;
    let journal = query_journal(handle.0)?;
    let request = MftEnumDataV0 {
        start_file_reference_number: 0,
        low_usn: 0,
        high_usn: journal.next_usn,
    };
    let mut buffer = vec![0_u8; 1024 * 1024];
    let (_, records) = enumerate_mft_batch(handle.0, &request, &mut buffer)?;
    Ok(VolumeProbe {
        volume,
        journal_id: journal.journal_id,
        first_usn: journal.first_usn,
        next_usn: journal.next_usn,
        first_batch_records: records.len(),
    })
}

pub fn is_directory(record: &UsnRecord) -> bool {
    record.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0
}

pub fn is_deleted(record: &UsnRecord) -> bool {
    record.reason & USN_REASON_FILE_DELETE != 0
}

/// 解析 `FSCTL_ENUM_USN_DATA` / `FSCTL_READ_USN_JOURNAL` 输出。
/// 输出缓冲区前 8 字节是下一次读取用的 USN，不属于记录本身。
pub fn parse_usn_buffer(buffer: &[u8]) -> Result<Vec<UsnRecord>> {
    if buffer.len() < 8 {
        return Err(NtfsError::Malformed("缺少下一 USN"));
    }

    let mut records = Vec::new();
    let mut cursor = 8_usize;
    while cursor < buffer.len() {
        let record = &buffer[cursor..];
        let record_length = read_u32(record, 0)? as usize;
        if record_length < 60 {
            return Err(NtfsError::Malformed("USN_RECORD_V2 长度不足"));
        }
        let end = cursor
            .checked_add(record_length)
            .ok_or(NtfsError::Malformed("USN 记录长度溢出"))?;
        if end > buffer.len() {
            return Err(NtfsError::Malformed("USN 记录超出缓冲区"));
        }

        let record = &buffer[cursor..end];
        let major_version = read_u16(record, 4)?;
        if major_version != 2 {
            return Err(NtfsError::UnsupportedVersion(major_version));
        }

        let name_length = read_u16(record, 56)? as usize;
        let name_offset = read_u16(record, 58)? as usize;
        let name_end = name_offset
            .checked_add(name_length)
            .ok_or(NtfsError::Malformed("文件名长度溢出"))?;
        if !name_length.is_multiple_of(2) || name_offset < 60 || name_end > record.len() {
            return Err(NtfsError::Malformed("文件名范围无效"));
        }
        let utf16 = record[name_offset..name_end]
            .chunks_exact(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();

        records.push(UsnRecord {
            // NTFS 文件引用号的高 16 位是复用序列号，低 48 位才是 MFT
            // 记录号。路径树只按同一快照中的记录号连接；若保留高位，卷根会
            // 变成“sequence | 5”，整棵树便永远无法抵达 ROOT_FILE_REFERENCE。
            file_reference_number: read_u64(record, 8)? & FILE_REFERENCE_NUMBER_MASK,
            parent_file_reference_number: read_u64(record, 16)? & FILE_REFERENCE_NUMBER_MASK,
            usn: read_i64(record, 24)?,
            timestamp_filetime: read_i64(record, 32)?,
            reason: read_u32(record, 40)?,
            file_attributes: read_u32(record, 52)?,
            name: String::from_utf16_lossy(&utf16),
        });
        cursor = end;
    }

    Ok(records)
}

/// 以目录 FRN 为边构建相对卷根的路径。没有可达父链或出现环的条目会被跳过。
pub fn resolve_relative_paths(records: &[UsnRecord]) -> HashMap<u64, String> {
    let nodes = records
        .iter()
        .map(|record| (record.file_reference_number, record))
        .collect::<HashMap<_, _>>();
    let mut paths = HashMap::new();

    for record in records {
        if record.file_reference_number == ROOT_FILE_REFERENCE {
            continue;
        }
        if let Some(path) = resolve_path(record.file_reference_number, &nodes) {
            paths.insert(record.file_reference_number, path);
        }
    }
    paths
}

fn resolve_path(file_reference_number: u64, nodes: &HashMap<u64, &UsnRecord>) -> Option<String> {
    let mut chain = Vec::new();
    let mut visited = HashSet::new();
    let mut current = file_reference_number;

    while current != ROOT_FILE_REFERENCE {
        if !visited.insert(current) {
            return None;
        }
        let record = nodes.get(&current)?;
        chain.push(record.name.as_str());
        current = record.parent_file_reference_number;
    }

    chain.reverse();
    Some(chain.join("/"))
}

fn read_u16(buffer: &[u8], offset: usize) -> Result<u16> {
    let bytes = buffer
        .get(offset..offset + 2)
        .ok_or(NtfsError::Malformed("USN 字段越界"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(buffer: &[u8], offset: usize) -> Result<u32> {
    let bytes = buffer
        .get(offset..offset + 4)
        .ok_or(NtfsError::Malformed("USN 字段越界"))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(buffer: &[u8], offset: usize) -> Result<u64> {
    let bytes = buffer
        .get(offset..offset + 8)
        .ok_or(NtfsError::Malformed("USN 字段越界"))?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn read_i64(buffer: &[u8], offset: usize) -> Result<i64> {
    Ok(read_u64(buffer, offset)? as i64)
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MftEnumDataV0 {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawUsnJournalDataV0 {
    journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
}

#[repr(C)]
#[allow(dead_code)]
struct ReadUsnJournalDataV0 {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    journal_id: u64,
}

fn query_journal(handle: HANDLE) -> io::Result<JournalInfo> {
    let mut journal = RawUsnJournalDataV0::default();
    let bytes_returned = unsafe {
        device_io_control(
            handle,
            FSCTL_QUERY_USN_JOURNAL,
            ptr::null(),
            0,
            (&mut journal as *mut RawUsnJournalDataV0).cast(),
            size_of::<RawUsnJournalDataV0>(),
        )
    };
    let bytes_returned = bytes_returned?;
    if bytes_returned < size_of::<RawUsnJournalDataV0>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "FSCTL_QUERY_USN_JOURNAL 返回的数据不完整",
        ));
    }
    Ok(JournalInfo {
        journal_id: journal.journal_id,
        first_usn: journal.first_usn,
        next_usn: journal.next_usn,
    })
}

fn enumerate_mft_batch(
    handle: HANDLE,
    request: &MftEnumDataV0,
    buffer: &mut [u8],
) -> io::Result<(u64, Vec<UsnRecord>)> {
    let bytes_returned = unsafe {
        device_io_control(
            handle,
            FSCTL_ENUM_USN_DATA,
            (request as *const MftEnumDataV0).cast(),
            size_of::<MftEnumDataV0>(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    let bytes_returned = bytes_returned?;
    if bytes_returned < size_of::<i64>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "FSCTL_ENUM_USN_DATA 返回的缓冲区不足 8 字节",
        ));
    }
    let response = &buffer[..bytes_returned];
    let next_reference = u64::from_le_bytes(
        response[..8]
            .try_into()
            .expect("前置长度已经验证为至少 8 字节"),
    );
    let records = parse_usn_buffer(response).map_err(ntfs_error_to_io)?;
    Ok((next_reference, records))
}

unsafe fn device_io_control(
    handle: HANDLE,
    control_code: u32,
    input: *const core::ffi::c_void,
    input_length: usize,
    output: *mut core::ffi::c_void,
    output_length: usize,
) -> io::Result<usize> {
    let input_length = u32::try_from(input_length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "控制输入缓冲区过大"))?;
    let output_length = u32::try_from(output_length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "控制输出缓冲区过大"))?;
    let mut bytes_returned = 0_u32;
    let succeeded = unsafe {
        DeviceIoControl(
            handle,
            control_code,
            input,
            input_length,
            output,
            output_length,
            &mut bytes_returned,
            ptr::null_mut(),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes_returned as usize)
}

struct VolumeHandle(HANDLE);

impl VolumeHandle {
    fn open(volume: char) -> io::Result<Self> {
        let path = format!(r"\\.\{volume}:");
        let path = wide_null(&path);
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }
}

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn normalize_volume_letter(volume: char) -> io::Result<char> {
    if volume.is_ascii_alphabetic() {
        Ok(volume.to_ascii_uppercase())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "卷标必须是单个英文字母，例如 C",
        ))
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ntfs_error_to_io(error: NtfsError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
