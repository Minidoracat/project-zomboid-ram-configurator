// Project Zomboid RAM Configurator - Windows core
// Copyright (C) 2023-2026 Minidoracat
// SPDX-License-Identifier: GPL-3.0-only
//
//! Windows-only core for reading and rewriting the `-Xmx` heap limit inside
//! `ProjectZomboid64.json`.
//!
//! Behaviour is a port of `PZ_Memory_Configurator.ps1` (and the contract pinned
//! by `tests/MemoryConfigurator.Tests.ps1`): strict JSON, byte-minimal edits,
//! one-generation `.bak` rotation through `ReplaceFileW`, and refusal whenever
//! the state that the user confirmed is no longer the state on disk.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use regex::bytes::Regex as ByteRegex;
use serde::de::{self, MapAccess, SeqAccess, Visitor};

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, MoveFileExW, ReplaceFileW};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
};
use windows_sys::Win32::System::SystemInformation::{
    GetPhysicallyInstalledSystemMemory, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};

/// Bytes in one gibibyte.
const GIB: u64 = 1 << 30;
/// Upper bound of an accepted user value, mirroring the `[1-9][0-9]{0,5}` rule.
const MAX_GB: u64 = 999_999;
/// Steam application id of Project Zomboid.
const APP_MANIFEST: &str = "appmanifest_108600.acf";
/// Launcher configuration file inside the game folder.
const CONFIG_NAME: &str = "ProjectZomboid64.json";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A diagnosable failure. `code` is a stable identifier the UI translates,
/// `detail` carries the raw (English) diagnostics, including OS messages and
/// recovery paths.
#[derive(Clone, Debug)]
pub struct AppError {
    pub code: &'static str,
    pub detail: String,
}

impl AppError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for AppError {}

pub type Result<T, E = AppError> = std::result::Result<T, E>;

/// A configuration file exactly as it was read, plus the values parsed out of
/// it. `bytes` is the confirmation token: every write refuses unless the file
/// on disk still matches it byte for byte.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub heap_bytes: u64,
    pub xms_bytes: Option<u64>,
}

/// Physically installed memory and the amount currently available.
#[derive(Clone, Copy, Debug)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

/// Recommendation strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Heavy mod lists: 80% of physical memory.
    Heavy,
    /// Leave plenty for the rest of the system: 50% of physical memory.
    Balanced,
}

/// A restore that was prepared against a known state. `target_bytes` is `None`
/// when the launcher configuration was missing at preparation time; the restore
/// then creates it and refuses to overwrite a file that appeared meanwhile.
#[derive(Clone, Debug)]
pub struct RestorePlan {
    pub path: PathBuf,
    pub target_bytes: Option<Vec<u8>>,
    pub backup: Snapshot,
}

// ---------------------------------------------------------------------------
// Recommendation and input validation
// ---------------------------------------------------------------------------

/// `floor(min(total * factor, total - 4 GiB))` in whole gigabytes, or `None`
/// below 8 GiB of physical memory (and when detection failed, i.e. `0`).
pub fn recommendation(total_bytes: u64, mode: Mode) -> Option<u64> {
    if total_bytes < 8 * GIB {
        return None;
    }
    let total_gb = total_bytes as f64 / GIB as f64;
    let factor = match mode {
        Mode::Heavy => 0.8,
        Mode::Balanced => 0.5,
    };
    let value = (total_gb * factor).min(total_gb - 4.0).floor();
    if value < 1.0 {
        return None;
    }
    Some(value as u64)
}

/// Parse a user supplied gigabyte value. Only a plain ASCII positive integer of
/// at most six digits is accepted, and it must stay below physical memory.
pub fn validate_gb(input: &str, total_bytes: u64) -> Result<u64> {
    let raw = input.as_bytes();
    let plain_integer = matches!(raw.len(), 1..=6)
        && raw[0] != b'0'
        && raw.iter().all(|byte| byte.is_ascii_digit());
    if !plain_integer {
        return Err(AppError::new(
            "invalid_value",
            format!(
                "Invalid memory value: '{input}'. Use a plain positive integer number of gigabytes (1-{MAX_GB})."
            ),
        ));
    }
    let gb: u64 = input
        .parse()
        .map_err(|_| AppError::new("invalid_value", format!("Invalid memory value: '{input}'.")))?;
    check_gb(gb, total_bytes)?;
    Ok(gb)
}

/// Shared guard for every accepted gigabyte value; returns the byte count.
fn check_gb(gb: u64, total_bytes: u64) -> Result<u64> {
    if gb == 0 || gb > MAX_GB {
        return Err(AppError::new(
            "invalid_value",
            format!("Memory value out of range: {gb} GB (allowed 1-{MAX_GB})."),
        ));
    }
    if total_bytes == 0 {
        return Err(AppError::new(
            "memory_unavailable",
            "Physical memory size is unknown, so no limit can be written.",
        ));
    }
    let requested = gb * GIB;
    if requested >= total_bytes {
        return Err(AppError::new(
            "invalid_value",
            format!(
                "Requested {gb} GB is not below the physical memory of {:.2} GB; refusing to write.",
                total_bytes as f64 / GIB as f64
            ),
        ));
    }
    Ok(requested)
}

// ---------------------------------------------------------------------------
// System memory
// ---------------------------------------------------------------------------

/// Installed memory from `GetPhysicallyInstalledSystemMemory` (SMBIOS, so the
/// hardware reserved region is included) plus currently available memory from
/// `GlobalMemoryStatusEx`. A failing call is reported, never reported as zero.
pub fn system_memory() -> Result<MemoryInfo> {
    let mut kibibytes: u64 = 0;
    if unsafe { GetPhysicallyInstalledSystemMemory(&mut kibibytes) } == 0 {
        return Err(AppError::new(
            "memory_unavailable",
            format!(
                "GetPhysicallyInstalledSystemMemory failed: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    let total_bytes = kibibytes
        .checked_mul(1024)
        .filter(|total| *total > 0)
        .ok_or_else(|| {
            AppError::new(
                "memory_unavailable",
                format!("Reported physical memory is not usable: {kibibytes} KiB"),
            )
        })?;

    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(AppError::new(
            "memory_unavailable",
            format!(
                "GlobalMemoryStatusEx failed: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }

    Ok(MemoryInfo {
        total_bytes,
        available_bytes: status.ullAvailPhys,
    })
}

// ---------------------------------------------------------------------------
// Strict JSON
// ---------------------------------------------------------------------------

/// A JSON document validated while it is parsed: every object, at every depth,
/// must have unique member names. Names are compared after unescaping, so
/// `"vmArgs"` and `"vm\u0041rgs"` collide.
enum StrictJson {
    Null,
    Bool,
    Number,
    Str(String),
    Array(Vec<StrictJson>),
    Object(Vec<(String, StrictJson)>),
}

impl StrictJson {
    fn type_name(&self) -> &'static str {
        match self {
            StrictJson::Null => "null",
            StrictJson::Bool => "a boolean",
            StrictJson::Number => "a number",
            StrictJson::Str(_) => "a string",
            StrictJson::Array(_) => "an array",
            StrictJson::Object(_) => "an object",
        }
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = StrictJson;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
        Ok(StrictJson::Null)
    }

    fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
        Ok(StrictJson::Null)
    }

    fn visit_some<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        deserializer.deserialize_any(StrictJsonVisitor)
    }

    fn visit_bool<E: de::Error>(self, _value: bool) -> std::result::Result<Self::Value, E> {
        Ok(StrictJson::Bool)
    }

    fn visit_i64<E: de::Error>(self, _value: i64) -> std::result::Result<Self::Value, E> {
        Ok(StrictJson::Number)
    }

    fn visit_u64<E: de::Error>(self, _value: u64) -> std::result::Result<Self::Value, E> {
        Ok(StrictJson::Number)
    }

    fn visit_f64<E: de::Error>(self, _value: f64) -> std::result::Result<Self::Value, E> {
        Ok(StrictJson::Number)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(StrictJson::Str(value.to_owned()))
    }

    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<StrictJson>()? {
            items.push(item);
        }
        Ok(StrictJson::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut members = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value::<StrictJson>()?;
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate JSON property: {key}")));
            }
            members.push((key, value));
        }
        Ok(StrictJson::Object(members))
    }
}

impl<'de> serde::Deserialize<'de> for StrictJson {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

/// What the launcher configuration says about the heap.
struct ConfigInfo {
    /// The whole `vmArgs` entry, e.g. `-Xmx3072m`.
    argument: String,
    heap_bytes: u64,
    xms_bytes: Option<u64>,
}

fn parse_config(bytes: &[u8]) -> Result<ConfigInfo> {
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let text = std::str::from_utf8(body).map_err(|err| {
        AppError::new(
            "invalid_json",
            format!("File is not valid UTF-8 text: {err}"),
        )
    })?;

    let root: StrictJson = serde_json::from_str(text)
        .map_err(|err| AppError::new("invalid_json", format!("Not valid JSON: {err}")))?;

    let members = match &root {
        StrictJson::Object(members) => members,
        other => {
            return Err(AppError::new(
                "invalid_json",
                format!(
                    "JSON root must be an object containing vmArgs, but it is {}.",
                    other.type_name()
                ),
            ));
        }
    };

    let vm_args = members
        .iter()
        .find(|(name, _)| name.as_str() == "vmArgs")
        .map(|(_, value)| value)
        .ok_or_else(|| {
            AppError::new(
                "invalid_json",
                "JSON root has no exact vmArgs entry (names are case sensitive).",
            )
        })?;

    let items = match vm_args {
        StrictJson::Array(items) => items,
        other => {
            return Err(AppError::new(
                "invalid_json",
                format!(
                    "vmArgs must be an array of JVM argument strings, but it is {}.",
                    other.type_name()
                ),
            ));
        }
    };

    let mut arguments: Vec<&str> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            StrictJson::Str(value) => arguments.push(value.as_str()),
            other => {
                return Err(AppError::new(
                    "invalid_json",
                    format!(
                        "vmArgs must contain only strings, but it contains {}.",
                        other.type_name()
                    ),
                ));
            }
        }
    }

    let heap: Vec<&str> = arguments
        .iter()
        .copied()
        .filter(|argument| argument.starts_with("-Xmx"))
        .collect();
    if heap.is_empty() {
        return Err(AppError::new(
            "invalid_heap",
            "No -Xmx argument found in root vmArgs.",
        ));
    }
    if heap.len() > 1 {
        return Err(AppError::new(
            "invalid_heap",
            format!(
                "Found {} -Xmx arguments in root vmArgs; expected exactly one.",
                heap.len()
            ),
        ));
    }
    let argument = heap[0];
    let heap_bytes = parse_jvm_size(argument, "Xmx")?;
    if heap_bytes == 0 {
        return Err(AppError::new(
            "invalid_heap",
            "The -Xmx heap limit must be positive.",
        ));
    }

    let initial: Vec<&str> = arguments
        .iter()
        .copied()
        .filter(|argument| argument.starts_with("-Xms"))
        .collect();
    if initial.len() > 1 {
        return Err(AppError::new(
            "invalid_heap",
            "Multiple -Xms arguments are not supported; keep exactly one.",
        ));
    }
    let xms_bytes = match initial.first() {
        Some(value) => Some(parse_jvm_size(value, "Xms")?),
        None => None,
    };

    Ok(ConfigInfo {
        argument: argument.to_owned(),
        heap_bytes,
        xms_bytes,
    })
}

/// `-Xmx3072m` style sizes: digits plus an optional `k`/`m`/`g` suffix, any case.
fn parse_jvm_size(argument: &str, option: &str) -> Result<u64> {
    let malformed = || {
        AppError::new(
            "invalid_heap",
            format!("Malformed -{option} argument: '{argument}'."),
        )
    };
    let out_of_range = || {
        AppError::new(
            "invalid_heap",
            format!("Value of -{option} is out of range: '{argument}'."),
        )
    };

    let rest = argument
        .strip_prefix('-')
        .and_then(|rest| rest.strip_prefix(option))
        .ok_or_else(malformed)?;

    let (digits, multiplier) = match rest.as_bytes().last() {
        Some(byte) if byte.is_ascii_digit() => (rest, 1u64),
        Some(b'k' | b'K') => (&rest[..rest.len() - 1], 1024u64),
        Some(b'm' | b'M') => (&rest[..rest.len() - 1], 1024u64 * 1024),
        Some(b'g' | b'G') => (&rest[..rest.len() - 1], 1024u64 * 1024 * 1024),
        _ => return Err(malformed()),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed());
    }

    let significant = digits.trim_start_matches('0');
    if significant.len() > 18 {
        return Err(out_of_range());
    }
    let number: u64 = if significant.is_empty() {
        0
    } else {
        significant.parse().map_err(|_| out_of_range())?
    };
    number.checked_mul(multiplier).ok_or_else(out_of_range)
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The one-generation backup that lives next to a configuration file.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".bak");
    PathBuf::from(name)
}

/// Read and validate a launcher configuration. The returned snapshot is the
/// token that [`write_limit`] requires, so read immediately before showing the
/// state to the user.
pub fn read_config(path: &Path) -> Result<Snapshot> {
    let full = absolute(path);
    let bytes = read_bytes(&full)?;
    let info = parse_config(&bytes)?;
    Ok(Snapshot {
        path: full,
        bytes,
        heap_bytes: info.heap_bytes,
        xms_bytes: info.xms_bytes,
    })
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Set the heap limit to `gb` gigabytes.
///
/// Only the single literal `-Xmx` token is rewritten; the BOM, line endings,
/// key order, comments-as-values and every other byte survive untouched. The
/// previous file becomes `<path>.bak`. Writing the value that is already in
/// effect is a no-op that neither touches the file nor rotates the backup.
pub fn write_limit(snapshot: &Snapshot, gb: u64, total_bytes: u64) -> Result<Snapshot> {
    let requested = check_gb(gb, total_bytes)?;
    let path = snapshot.path.as_path();

    if !path.is_file() {
        return Err(AppError::new(
            "io",
            format!("File not found: {}", path.display()),
        ));
    }
    if is_read_only(path)? {
        return Err(AppError::new(
            "read_only",
            format!("Target file is read-only: {}", path.display()),
        ));
    }

    let current = read_bytes(path)?;
    if current != snapshot.bytes {
        return Err(changed(path));
    }
    let info = parse_config(&snapshot.bytes)?;

    if info.heap_bytes == requested {
        return Ok(Snapshot {
            path: path.to_path_buf(),
            bytes: snapshot.bytes.clone(),
            heap_bytes: info.heap_bytes,
            xms_bytes: info.xms_bytes,
        });
    }

    if let Some(xms) = info.xms_bytes
        && xms > requested
    {
        return Err(AppError::new(
            "xms_conflict",
            format!(
                "The existing -Xms ({:.2} GB) would exceed the requested -Xmx ({gb} GB). Lower -Xms yourself first; this tool never rewrites -Xms.",
                xms as f64 / GIB as f64
            ),
        ));
    }

    let candidate = replace_heap_literal(&snapshot.bytes, &info.argument, &format!("-Xmx{gb}g"))?;
    let backup = backup_path(path);
    write_atomic(
        path,
        &candidate,
        Some(&snapshot.bytes),
        Some(backup.as_path()),
        Some(requested),
    )?;

    let result = read_config(path)?;
    if result.heap_bytes != requested {
        return Err(AppError::new(
            "replace_failed",
            format!(
                "Write verification failed: {} now reports {} bytes of heap. The previous version is at '{}'.",
                path.display(),
                result.heap_bytes,
                backup.display()
            ),
        ));
    }
    Ok(result)
}

/// Swap the single literal `-Xmx…` token for `replacement`, leaving every other
/// byte in place. Refuses when the token is missing, appears more than once, or
/// does not match the parsed argument (which happens when the value is written
/// with JSON escapes and therefore has no unambiguous literal to edit).
fn replace_heap_literal(bytes: &[u8], argument: &str, replacement: &str) -> Result<Vec<u8>> {
    static PATTERN: LazyLock<ByteRegex> = LazyLock::new(|| {
        ByteRegex::new(r"-Xmx[0-9]+[kKmMgG]?").expect("the -Xmx literal pattern is valid")
    });
    let pattern = &*PATTERN;

    let mut matches = pattern.find_iter(bytes);
    let found = matches.next().ok_or_else(|| {
        AppError::new(
            "invalid_heap",
            "Expected exactly one literal -Xmx token in the file but found none; refusing to edit.",
        )
    })?;
    if matches.next().is_some() {
        return Err(AppError::new(
            "invalid_heap",
            format!(
                "Expected exactly one literal -Xmx token in the file but found {}; refusing to edit.",
                pattern.find_iter(bytes).count()
            ),
        ));
    }
    if found.as_bytes() != argument.as_bytes() {
        return Err(AppError::new(
            "invalid_heap",
            "The -Xmx token in the file text does not match the parsed value; refusing to edit.",
        ));
    }

    let mut out = Vec::with_capacity(bytes.len() + replacement.len());
    out.extend_from_slice(&bytes[..found.start()]);
    out.extend_from_slice(replacement.as_bytes());
    out.extend_from_slice(&bytes[found.end()..]);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Restoring
// ---------------------------------------------------------------------------

/// Prepare a restore from `<path>.bak`. The backup must parse; the target may
/// be damaged or missing. The plan pins both states, so [`restore_config`]
/// refuses once anything moved underneath it.
pub fn prepare_restore(path: &Path) -> Result<RestorePlan> {
    let full = absolute(path);
    let backup = backup_path(&full);
    if !backup.is_file() {
        return Err(AppError::new(
            "missing_backup",
            format!("No backup file found: {}", backup.display()),
        ));
    }

    let bytes = read_bytes(&backup)?;
    let info = parse_config(&bytes).map_err(|err| {
        AppError::new(
            err.code,
            format!("Backup '{}' is unusable: {}", backup.display(), err.detail),
        )
    })?;

    let target_bytes = if full.is_file() {
        if is_read_only(&full)? {
            return Err(AppError::new(
                "read_only",
                format!("Target file is read-only: {}", full.display()),
            ));
        }
        Some(read_bytes(&full)?)
    } else {
        None
    };

    Ok(RestorePlan {
        path: full,
        target_bytes,
        backup: Snapshot {
            path: backup,
            bytes,
            heap_bytes: info.heap_bytes,
            xms_bytes: info.xms_bytes,
        },
    })
}

/// Put the backup back byte for byte. The backup itself is kept and never
/// rotated; a target that was missing when the plan was made is created, not
/// overwritten. The backup is pinned for the whole operation, so the bytes
/// verified here are the bytes that reach the target.
pub fn restore_config(plan: &RestorePlan) -> Result<Snapshot> {
    // While this handle lives nobody else can write to, delete or replace the
    // backup, so it cannot change between this check and the swap.
    let mut pinned = open_shared_read(&plan.backup.path)?;
    let mut backup_now = Vec::with_capacity(plan.backup.bytes.len());
    pinned
        .read_to_end(&mut backup_now)
        .map_err(|err| io_error(&plan.backup.path, "read", err))?;
    if backup_now != plan.backup.bytes {
        return Err(changed(&plan.backup.path));
    }

    if plan.target_bytes.is_some() && plan.path.is_file() && is_read_only(&plan.path)? {
        return Err(AppError::new(
            "read_only",
            format!("Target file is read-only: {}", plan.path.display()),
        ));
    }

    let replaced = write_atomic(
        &plan.path,
        &backup_now,
        plan.target_bytes.as_deref(),
        None,
        Some(plan.backup.heap_bytes),
    );
    // Only now, once the swap either happened or failed, may the backup move.
    drop(pinned);
    replaced?;

    let result = read_config(&plan.path)?;
    if result.heap_bytes != plan.backup.heap_bytes {
        return Err(AppError::new(
            "replace_failed",
            format!(
                "Restore verification failed: {} now reports {} bytes of heap. The backup is still at '{}'.",
                plan.path.display(),
                result.heap_bytes,
                plan.backup.path.display()
            ),
        ));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Atomic replacement
// ---------------------------------------------------------------------------

/// Files to delete unless the replacement reached the point where Windows may
/// already have moved data around.
struct Cleanup {
    paths: Vec<PathBuf>,
    armed: bool,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.armed {
            for path in &self.paths {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// Stage `bytes` next to `path`, validate the staged file, re-check that the
/// target is still in the expected state, then swap it in.
///
/// * `expected` `Some(bytes)`: the target must still equal them; the swap uses
///   `ReplaceFileW`, which writes the previous version to `backup`.
/// * `expected` `None`: the target must still be absent and is created with a
///   non-replacing `MoveFileExW`.
///
/// When the swap itself fails the staged candidate and a copy of the previous
/// backup are deliberately kept, and their paths are listed in the error.
fn write_atomic(
    path: &Path,
    bytes: &[u8],
    expected: Option<&[u8]>,
    backup: Option<&Path>,
    expect_heap: Option<u64>,
) -> Result<()> {
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => {
            return Err(AppError::new(
                "io",
                format!("Cannot determine the folder of: {}", path.display()),
            ));
        }
    };
    let name = path.file_name().ok_or_else(|| {
        AppError::new(
            "io",
            format!("Not a configuration file path: {}", path.display()),
        )
    })?;

    let (temp, mut handle) = create_unique(directory, name)?;
    let mut cleanup = Cleanup {
        paths: vec![temp.clone()],
        armed: true,
    };

    handle
        .write_all(bytes)
        .and_then(|()| handle.flush())
        .and_then(|()| handle.sync_all())
        .map_err(|err| io_error(&temp, "write", err))?;
    drop(handle);

    let staged = read_bytes(&temp)?;
    if staged != bytes {
        return Err(AppError::new(
            "io",
            format!(
                "The staged file '{}' does not match the prepared content; nothing was replaced.",
                temp.display()
            ),
        ));
    }
    let info = parse_config(&staged)?;
    if let Some(heap) = expect_heap
        && info.heap_bytes != heap
    {
        return Err(AppError::new(
            "invalid_heap",
            format!(
                "Rewritten content reports {} bytes of heap instead of {heap}; nothing was replaced.",
                info.heap_bytes
            ),
        ));
    }

    let mut previous: Option<PathBuf> = None;
    if let Some(existing) = backup.filter(|backup| backup.is_file()) {
        let previous_bytes = read_bytes(existing)?;
        let mut copy_name = temp.clone().into_os_string();
        copy_name.push(".previous-backup");
        let copy = PathBuf::from(copy_name);
        let mut handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&copy)
            .map_err(|err| io_error(&copy, "create", err))?;
        handle
            .write_all(&previous_bytes)
            .and_then(|()| handle.sync_all())
            .map_err(|err| io_error(&copy, "write", err))?;
        cleanup.paths.push(copy.clone());
        previous = Some(copy);
    }

    // Everything else is staged, so this is the last thing before the swap: no
    // further blocking I/O may follow, or the comparison goes stale again.
    match expected {
        Some(bytes) => {
            let now = read_bytes(path)?;
            if now != bytes {
                return Err(changed(path));
            }
        }
        None => {
            if path.exists() {
                return Err(AppError::new(
                    "changed",
                    format!(
                        "'{}' appeared while the restore was being prepared; nothing was overwritten.",
                        path.display()
                    ),
                ));
            }
        }
    }

    // Past this point a failing swap may already have moved files, so the
    // recovery copies must survive for the user to inspect.
    cleanup.armed = false;

    let target = wide(path.as_os_str());
    let candidate = wide(temp.as_os_str());
    let replaced = match expected {
        Some(_) => {
            let backup_wide = backup.map(|backup| wide(backup.as_os_str()));
            let backup_ptr = backup_wide
                .as_ref()
                .map_or(std::ptr::null(), |wide| wide.as_ptr());
            unsafe {
                ReplaceFileW(
                    target.as_ptr(),
                    candidate.as_ptr(),
                    backup_ptr,
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            }
        }
        None => unsafe { MoveFileExW(candidate.as_ptr(), target.as_ptr(), 0) },
    };

    if replaced == 0 {
        let err = std::io::Error::last_os_error();
        return Err(AppError::new(
            "replace_failed",
            format!(
                "Replacement failed: {err}. Do not retry until checking these recovery paths: target '{}'; backup '{}'; candidate '{}'; previous backup '{}'.",
                path.display(),
                backup.map_or_else(|| "-".to_owned(), |backup| backup.display().to_string()),
                temp.display(),
                previous
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |copy| copy.display().to_string()),
            ),
        ));
    }

    if let Some(copy) = previous {
        // The replacement is already committed, so a failed cleanup must not be
        // reported as a failed write; it must not be hidden either, because the
        // leftover holds the backup generation this write rotated out.
        if let Err(err) = fs::remove_file(&copy) {
            eprintln!(
                "{} was replaced successfully, but the temporary copy of the previous backup could not be removed: '{}': {err}",
                path.display(),
                copy.display()
            );
        }
    }
    Ok(())
}

/// Create a file nobody else owns, in `directory`, named after `name`.
fn create_unique(directory: &Path, name: &OsStr) -> Result<(PathBuf, fs::File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // ponytail: a name collision just retries with a fresh stamp, so there is
    // no UUID dependency.
    for _ in 0..64 {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos() as u64);
        let ticket = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut candidate = name.to_os_string();
        candidate.push(format!(".pztmp{stamp:x}{ticket:x}"));
        let path = directory.join(candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(handle) => return Ok((path, handle)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(io_error(&path, "create", err)),
        }
    }
    Err(AppError::new(
        "io",
        format!(
            "Could not create a staging file in {}: too many name collisions.",
            directory.display()
        ),
    ))
}

// ---------------------------------------------------------------------------
// Steam discovery
// ---------------------------------------------------------------------------

/// Game folders that hold a `ProjectZomboid64.json` (or only its `.bak`),
/// discovered through the Steam registry roots and every Steam library.
pub fn find_installations() -> Result<Vec<PathBuf>> {
    let mut roots: Vec<String> = Vec::new();
    for (hive, subkey, name) in [
        (HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath"),
        (
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\WOW6432Node\Valve\Steam",
            "InstallPath",
        ),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\Valve\Steam", "InstallPath"),
    ] {
        if let Some(value) = registry_string(hive, subkey, name)
            && !value.trim().is_empty()
        {
            roots.push(value);
        }
    }
    Ok(installations_from_roots(&roots))
}

fn installations_from_roots(roots: &[String]) -> Vec<PathBuf> {
    let mut libraries: Vec<PathBuf> = Vec::new();
    for root in roots {
        let Some(directory) = clean_directory(root) else {
            continue;
        };
        push_unique(&mut libraries, &directory);

        let vdf = directory.join("steamapps").join("libraryfolders.vdf");
        let Ok(raw) = fs::read(&vdf) else { continue };
        let text = String::from_utf8_lossy(&raw);
        for captures in VDF_PATH.captures_iter(&text) {
            let value = vdf_unescape(&captures[1]);
            if let Some(library) = clean_directory(&value) {
                push_unique(&mut libraries, &library);
            }
        }
    }

    let mut installations: Vec<PathBuf> = Vec::new();
    for library in &libraries {
        let manifest = library.join("steamapps").join(APP_MANIFEST);
        let Ok(raw) = fs::read(&manifest) else {
            continue;
        };
        let text = String::from_utf8_lossy(&raw);
        let Some(captures) = ACF_INSTALL_DIR.captures(&text) else {
            continue;
        };

        // Folder names keep their inner spaces; a value that is not a plain
        // folder name would escape the library, so it is refused.
        let install_dir = vdf_unescape(&captures[1]);
        if install_dir.trim().is_empty()
            || install_dir.contains('\\')
            || install_dir.contains('/')
            || install_dir.trim_matches('.').is_empty()
        {
            continue;
        }

        let game = library.join("steamapps").join("common").join(&install_dir);
        let config = game.join(CONFIG_NAME);
        if config.is_file() || backup_path(&config).is_file() {
            push_unique(&mut installations, &game);
        }
    }
    installations
}

/// Steam writes paths with either separator, sometimes quoted; only existing
/// directories survive.
fn clean_directory(raw: &str) -> Option<PathBuf> {
    let cleaned = raw.trim().trim_matches('"').trim().replace('/', "\\");
    if cleaned.is_empty() {
        return None;
    }
    let path = std::path::absolute(&cleaned).ok()?;
    path.is_dir().then_some(path)
}

/// Only `\\` and `\"` are real VDF escapes; a lone backslash (hand-edited
/// files) stays as it is.
fn vdf_unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(current) = chars.next() {
        if current != '\\' {
            out.push(current);
            continue;
        }
        match chars.as_str().chars().next() {
            Some(next @ ('\\' | '"')) => {
                out.push(next);
                chars.next();
            }
            _ => out.push('\\'),
        }
    }
    out
}

/// Steam library roots inside `libraryfolders.vdf`.
static VDF_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)"path"\s*"((?:[^"\\]|\\.)*)""#).expect("the VDF path pattern is valid")
});

/// Game folder name inside `appmanifest_108600.acf`.
static ACF_INSTALL_DIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)"installdir"\s*"((?:[^"\\]|\\.)*)""#)
        .expect("the ACF installdir pattern is valid")
});

fn push_unique(list: &mut Vec<PathBuf>, path: &Path) {
    let text = path.as_os_str().to_string_lossy();
    let trimmed = text.trim_end_matches(['\\', '/']);
    // Keep `D:\` intact: trimming it to `D:` would make later joins relative.
    let normalized = if trimmed.is_empty() || trimmed.ends_with(':') {
        text.as_ref()
    } else {
        trimmed
    };
    if normalized.is_empty() {
        return;
    }
    let key = normalized.to_lowercase();
    if list
        .iter()
        .any(|existing| existing.as_os_str().to_string_lossy().to_lowercase() == key)
    {
        return;
    }
    list.push(PathBuf::from(normalized));
}

fn registry_string(hive: HKEY, subkey: &str, name: &str) -> Option<String> {
    let subkey = wide(OsStr::new(subkey));
    let name = wide(OsStr::new(name));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;

    let mut size: u32 = 0;
    let status = unsafe {
        RegGetValueW(
            hive,
            subkey.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS || size < 2 {
        return None;
    }

    let mut buffer = vec![0u16; size as usize / 2 + 1];
    let mut written = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            hive,
            subkey.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut written,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }

    let characters = (written as usize / 2).min(buffer.len());
    let value = &buffer[..characters];
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(characters);
    Some(String::from_utf16_lossy(&value[..end]))
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|err| io_error(path, "read", err))
}

/// Open `path` for reading while denying writers and deleters: other openers
/// may only read it. The restriction lives as long as the returned handle.
fn open_shared_read(path: &Path) -> Result<fs::File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .map_err(|err| io_error(path, "open", err))
}

fn is_read_only(path: &Path) -> Result<bool> {
    Ok(fs::metadata(path)
        .map_err(|err| io_error(path, "inspect", err))?
        .permissions()
        .readonly())
}

fn io_error(path: &Path, action: &str, err: std::io::Error) -> AppError {
    AppError::new(
        "io",
        format!("Failed to {action} {}: {err}", path.display()),
    )
}

fn changed(path: &Path) -> AppError {
    AppError::new(
        "changed",
        format!(
            "'{}' changed since it was read. Read it again and confirm the new state; nothing was written.",
            path.display()
        ),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

    fn case_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "pzmc-rs-{tag}-{stamp:x}-{:x}",
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create the case directory");
        dir
    }

    /// Mirrors a real `ProjectZomboid64.json`: CRLF, nested objects, CJK text.
    fn game_json(vm_args: &[&str]) -> String {
        let args = vm_args
            .iter()
            .map(|argument| format!("        \"{argument}\""))
            .collect::<Vec<_>>()
            .join(",\r\n");
        format!(
            "{{\r\n    \"mainClass\": \"zombie/gameStates/MainScreenState\",\r\n\
             \x20   \"classpath\": [ \".\" ],\r\n\
             \x20   \"note\": \"遊戲設定 – 請勿破壞\",\r\n\
             \x20   \"nested\": {{ \"launcher\": {{ \"profiles\": [ 1, 2, 3 ], \"label\": \"中文標籤\" }} }},\r\n\
             \x20   \"vmArgs\": [\r\n{args}\r\n    ]\r\n}}\r\n"
        )
    }

    fn write_file(path: &Path, text: &str) {
        fs::write(path, text.as_bytes()).expect("write the fixture");
    }

    fn new_config(dir: &Path, name: &str, vm_args: &[&str]) -> PathBuf {
        let path = dir.join(name);
        write_file(&path, &game_json(vm_args));
        path
    }

    fn set_read_only(path: &Path, read_only: bool) {
        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_readonly(read_only);
        fs::set_permissions(path, permissions).expect("set permissions");
    }

    #[test]
    fn recommendation_takes_the_smaller_of_the_share_and_the_reserve() {
        assert_eq!(recommendation(8 * GIB, Mode::Heavy), Some(4));
        assert_eq!(recommendation(24 * GIB, Mode::Heavy), Some(19));
        assert_eq!(
            recommendation(16 * GIB - 128 * 1024 * 1024, Mode::Heavy),
            Some(11)
        );
        assert_eq!(recommendation(16 * GIB, Mode::Balanced), Some(8));
        assert_eq!(recommendation(64 * GIB, Mode::Balanced), Some(32));
        for total in [0, 1, 8 * GIB - 1] {
            assert_eq!(recommendation(total, Mode::Heavy), None, "{total} bytes");
            assert_eq!(recommendation(total, Mode::Balanced), None, "{total} bytes");
        }
    }

    #[test]
    fn reads_every_accepted_unit() {
        let dir = case_dir("parse");
        let rows: [(&str, u64); 8] = [
            ("-Xmx12g", 12 * GIB),
            ("-Xmx16G", 16 * GIB),
            ("-Xmx3072m", 3072 * 1024 * 1024),
            ("-Xmx4096M", 4096 * 1024 * 1024),
            ("-Xmx8388608k", 8_388_608 * 1024),
            ("-Xmx6291456K", 6_291_456 * 1024),
            ("-Xmx2147483648", 2_147_483_648),
            ("-Xmx0012g", 12 * GIB),
        ];
        for (argument, bytes) in rows {
            let path = new_config(
                &dir,
                "parse.json",
                &["-Djava.awt.headless=true", "-Xms1024m", argument],
            );
            let snapshot = read_config(&path).unwrap_or_else(|err| panic!("{argument}: {err}"));
            assert_eq!(snapshot.heap_bytes, bytes, "{argument}");
            assert_eq!(snapshot.xms_bytes, Some(1024 * 1024 * 1024), "{argument}");
        }
    }

    #[test]
    fn refuses_ambiguous_or_non_standard_json() {
        let dir = case_dir("parse-reject");
        let path = dir.join("bad.json");
        let rows: [(&str, &str); 26] = [
            ("no heap", r#"{ "vmArgs": [ "-Xms1024m" ] }"#),
            ("two heaps", r#"{ "vmArgs": [ "-Xmx4g", "-Xmx8g" ] }"#),
            (
                "two initial heaps",
                r#"{ "vmArgs": [ "-Xms1g", "-Xms2g", "-Xmx8g" ] }"#,
            ),
            ("fraction", r#"{ "vmArgs": [ "-Xmx1.5g" ] }"#),
            ("bad unit", r#"{ "vmArgs": [ "-Xmx12gb" ] }"#),
            ("not a number", r#"{ "vmArgs": [ "-Xmxabc" ] }"#),
            ("negative", r#"{ "vmArgs": [ "-Xmx-4g" ] }"#),
            ("empty value", r#"{ "vmArgs": [ "-Xmx" ] }"#),
            ("thousands separator", r#"{ "vmArgs": [ "-Xmx12,288m" ] }"#),
            ("zero", r#"{ "vmArgs": [ "-Xmx0g" ] }"#),
            (
                "overflow",
                r#"{ "vmArgs": [ "-Xmx9999999999999999999g" ] }"#,
            ),
            ("truncated json", r#"{ "vmArgs": [ "-Xmx4g" "#),
            ("empty file", ""),
            (
                "no vmArgs",
                r#"{ "mainClass": "zombie/gameStates/MainScreenState" }"#,
            ),
            (
                "only nested vmArgs",
                r#"{ "launcher": { "vmArgs": [ "-Xmx4g" ] } }"#,
            ),
            ("wrong case", r#"{ "VMARGS": [ "-Xmx4g" ] }"#),
            ("duplicate root key", r#"{"vmArgs":[],"vmArgs":["-Xmx4g"]}"#),
            (
                "escaped duplicate key",
                r#"{"vmArgs":[],"vm\u0041rgs":["-Xmx4g"]}"#,
            ),
            (
                "duplicate nested key",
                r#"{"vmArgs":["-Xmx4g"],"n":{"a":1,"\u0061":2}}"#,
            ),
            ("leading zero number", r#"{"vmArgs":["-Xmx4g"],"n":01}"#),
            ("trailing comma", r#"{"vmArgs":["-Xmx4g"],}"#),
            ("vmArgs is a string", r#"{"vmArgs":"-Xmx4g"}"#),
            ("root is an array", r#"[{"vmArgs":["-Xmx4g"]}]"#),
            ("vmArgs holds a number", r#"{"vmArgs":["-Xmx4g",4096]}"#),
            (
                "malformed initial heap",
                r#"{"vmArgs":["-Xms1.5g","-Xmx4g"]}"#,
            ),
            ("multiplier overflow", r#"{"vmArgs":["-Xmx17179869184g"]}"#),
        ];
        for (name, text) in rows {
            write_file(&path, text);
            let err = read_config(&path).expect_err(name);
            assert!(
                matches!(err.code, "invalid_json" | "invalid_heap"),
                "{name}: unexpected code {}",
                err.code
            );
        }

        // JavaScript-flavoured JSON and NaN are rejected as syntax errors.
        for text in [
            "{vmArgs:['-Xmx4g']}",
            "{'vmArgs':['-Xmx4g']}",
            r#"{"vmArgs":["-Xmx4g"],"n":NaN}"#,
            r#"{"vmArgs":["-Xmx4g"]} trailing"#,
        ] {
            write_file(&path, text);
            assert_eq!(read_config(&path).expect_err(text).code, "invalid_json");
        }

        // Not text at all: refused before any JSON parsing, BOM or not.
        for bytes in [
            b"{\"vmArgs\":[\"-Xmx4\xFFg\"]}".to_vec(),
            [BOM, b"{\"vmArgs\":[\"-Xmx\xC3\x28g\"]}".as_slice()].concat(),
        ] {
            fs::write(&path, &bytes).expect("write the fixture");
            assert_eq!(
                read_config(&path).expect_err("not utf-8").code,
                "invalid_json"
            );
        }
    }

    #[test]
    fn write_touches_only_the_heap_token_and_keeps_the_original_as_backup() {
        let dir = case_dir("it's [odd] 中文 !");
        let path = new_config(
            &dir,
            "Project Zomboid64 [測試]'s !.json",
            &[
                "-Djava.awt.headless=true",
                "-Xms1024m",
                "-XX:+UseZGC",
                "-Dzomboid.note=中文參數",
                "-Xmx3072m",
            ],
        );
        // A real launcher file may carry a BOM; it must survive the edit.
        let original = {
            let body = fs::read(&path).expect("read fixture");
            let mut bytes = BOM.to_vec();
            bytes.extend_from_slice(&body);
            fs::write(&path, &bytes).expect("write BOM fixture");
            bytes
        };

        let snapshot = read_config(&path).expect("read");
        let result = write_limit(&snapshot, 12, 16 * GIB).expect("write");

        assert_eq!(result.heap_bytes, 12 * GIB);
        assert_eq!(result.xms_bytes, Some(1024 * 1024 * 1024));
        assert!(result.bytes.starts_with(BOM), "the BOM must be preserved");

        let expected = {
            let text = String::from_utf8(original[3..].to_vec()).expect("utf-8");
            let mut bytes = BOM.to_vec();
            bytes.extend_from_slice(text.replace("-Xmx3072m", "-Xmx12g").as_bytes());
            bytes
        };
        assert_eq!(result.bytes, expected, "only the -Xmx token may change");
        assert_eq!(
            fs::read(backup_path(&path)).expect("backup"),
            original,
            "the backup must equal the original byte for byte"
        );
        assert_eq!(read_config(&path).expect("re-read").heap_bytes, 12 * GIB);
    }

    #[test]
    fn refuses_values_that_are_not_plain_gigabytes_or_exceed_physical_memory() {
        let dir = case_dir("validate");
        let path = new_config(&dir, CONFIG_NAME, &["-Xms1024m", "-Xmx3072m"]);
        let snapshot = read_config(&path).expect("read");
        let original = snapshot.bytes.clone();

        for value in [
            "0",
            "",
            " ",
            "abc",
            "1.5",
            "-4",
            "12g",
            "0x8",
            "1e3",
            "12,288",
            "１２",
            "١٢",
            "+8",
            "08",
            "1234567",
            "12; Remove-Item C:\\ -Recurse",
            "12\" & calc",
            "$(1+1)",
            "999999999999999999999",
            "16",
            "20",
        ] {
            let err = validate_gb(value, 16 * GIB).expect_err(value);
            assert_eq!(err.code, "invalid_value", "{value}");
        }
        assert_eq!(validate_gb("12", 16 * GIB).expect("12 GB"), 12);
        assert_eq!(
            validate_gb("12", 0).expect_err("unknown memory").code,
            "memory_unavailable"
        );

        // The same guard applies to the write path itself.
        assert_eq!(
            write_limit(&snapshot, 16, 16 * GIB)
                .expect_err("at physical size")
                .code,
            "invalid_value"
        );
        assert_eq!(
            write_limit(&snapshot, 0, 16 * GIB).expect_err("zero").code,
            "invalid_value"
        );
        assert_eq!(fs::read(&path).expect("target"), original);
        assert!(
            !backup_path(&path).exists(),
            "a refusal must not create a backup"
        );
    }

    #[test]
    fn refuses_to_drop_the_limit_below_the_existing_initial_heap() {
        let dir = case_dir("xms");
        let path = new_config(&dir, CONFIG_NAME, &["-Xms8g", "-Xmx12g"]);
        let snapshot = read_config(&path).expect("read");

        let err = write_limit(&snapshot, 4, 16 * GIB).expect_err("xms conflict");
        assert_eq!(err.code, "xms_conflict");
        assert_eq!(fs::read(&path).expect("target"), snapshot.bytes);
        assert!(!backup_path(&path).exists());

        // Equal values are fine: -Xms8g with -Xmx8g is a valid configuration.
        assert_eq!(
            write_limit(&snapshot, 8, 16 * GIB)
                .expect("equal sizes")
                .heap_bytes,
            8 * GIB
        );
    }

    #[test]
    fn read_only_targets_are_refused_before_anything_is_staged() {
        let dir = case_dir("readonly");
        let path = new_config(&dir, CONFIG_NAME, &["-Xms1024m", "-Xmx3072m"]);
        let snapshot = read_config(&path).expect("read");
        set_read_only(&path, true);

        let write = write_limit(&snapshot, 8, 16 * GIB);
        let restore_backup = {
            write_file(&backup_path(&path), &game_json(&["-Xmx4g"]));
            prepare_restore(&path)
        };
        set_read_only(&path, false);

        assert_eq!(write.expect_err("read-only write").code, "read_only");
        assert_eq!(
            restore_backup.expect_err("read-only restore").code,
            "read_only"
        );
        assert_eq!(fs::read(&path).expect("target"), snapshot.bytes);
    }

    #[test]
    fn backups_hold_one_generation_and_equal_values_change_nothing() {
        let dir = case_dir("rotate");
        let path = new_config(&dir, CONFIG_NAME, &["-Xms1024m", "-Xmx3072m"]);
        let backup = backup_path(&path);
        let original = fs::read(&path).expect("original");

        let first = write_limit(&read_config(&path).expect("read"), 8, 16 * GIB).expect("first");
        assert_eq!(fs::read(&backup).expect("backup"), original);

        let second = write_limit(&read_config(&path).expect("read"), 10, 16 * GIB).expect("second");
        assert_eq!(second.heap_bytes, 10 * GIB);
        assert_eq!(
            fs::read(&backup).expect("backup"),
            first.bytes,
            "the backup must hold the previous generation"
        );

        let before = fs::metadata(&backup)
            .and_then(|meta| meta.modified())
            .expect("mtime");
        let again = write_limit(&read_config(&path).expect("read"), 10, 16 * GIB).expect("no-op");
        assert_eq!(
            again.bytes, second.bytes,
            "a no-op returns the current state"
        );
        assert_eq!(fs::read(&path).expect("target"), second.bytes);
        assert_eq!(fs::read(&backup).expect("backup"), first.bytes);
        assert_eq!(
            fs::metadata(&backup)
                .and_then(|meta| meta.modified())
                .expect("mtime"),
            before,
            "a no-op must not rotate the backup"
        );
    }

    #[test]
    fn restores_even_when_the_target_is_damaged_or_missing() {
        let dir = case_dir("restore");
        let path = new_config(
            &dir,
            CONFIG_NAME,
            &["-Xms1024m", "-Xmx3072m", "-Dnote=中文參數"],
        );
        let backup = backup_path(&path);
        let original = fs::read(&path).expect("original");
        write_limit(&read_config(&path).expect("read"), 8, 16 * GIB).expect("write");

        // Damaged target: the backup still parses, so the restore proceeds.
        write_file(&path, "{ 這不是有效的 JSON");
        let restored = restore_config(&prepare_restore(&path).expect("plan")).expect("restore");
        assert_eq!(restored.heap_bytes, 3072 * 1024 * 1024);
        assert_eq!(fs::read(&path).expect("target"), original);
        assert_eq!(
            fs::read(&backup).expect("backup"),
            original,
            "the backup is kept"
        );

        // Missing target: created, and the backup is still kept.
        fs::remove_file(&path).expect("remove target");
        let plan = prepare_restore(&path).expect("plan");
        assert!(plan.target_bytes.is_none());
        assert_eq!(
            restore_config(&plan).expect("restore").heap_bytes,
            3072 * 1024 * 1024
        );
        assert_eq!(fs::read(&path).expect("target"), original);
        assert_eq!(fs::read(&backup).expect("backup"), original);

        // A target that appears after the plan was made is never overwritten.
        fs::remove_file(&path).expect("remove target");
        let plan = prepare_restore(&path).expect("plan");
        write_file(&path, &game_json(&["-Xmx12g"]));
        let appeared = fs::read(&path).expect("target");
        assert_eq!(restore_config(&plan).expect_err("race").code, "changed");
        assert_eq!(fs::read(&path).expect("target"), appeared);
    }

    #[test]
    fn restore_refuses_missing_or_damaged_backups() {
        let dir = case_dir("restore-reject");

        let lonely = new_config(&dir, "no-backup.json", &["-Xmx3072m"]);
        let bytes = fs::read(&lonely).expect("target");
        assert_eq!(
            prepare_restore(&lonely).expect_err("no backup").code,
            "missing_backup"
        );
        assert_eq!(fs::read(&lonely).expect("target"), bytes);

        for text in [
            r#"{ "vmArgs": [ "-Xmx4g" "#,
            r#"{ "vmArgs": [ "-Xms1024m" ] }"#,
            "",
        ] {
            let path = new_config(&dir, "bad-backup.json", &["-Xmx8g"]);
            write_file(&backup_path(&path), text);
            let target = fs::read(&path).expect("target");
            let backup = fs::read(backup_path(&path)).expect("backup");
            let err = prepare_restore(&path).expect_err("damaged backup");
            assert!(
                matches!(err.code, "invalid_json" | "invalid_heap"),
                "{}",
                err.code
            );
            assert!(err.detail.contains("Backup"), "{}", err.detail);
            assert_eq!(fs::read(&path).expect("target"), target);
            assert_eq!(fs::read(backup_path(&path)).expect("backup"), backup);
        }
    }

    #[test]
    fn a_state_that_changed_after_confirmation_is_never_overwritten() {
        let dir = case_dir("confirmation-race");
        let path = new_config(&dir, CONFIG_NAME, &["-Xmx4g"]);
        let confirmed = read_config(&path).expect("read");
        write_file(&backup_path(&path), &game_json(&["-Xmx3g"]));
        let backup = fs::read(backup_path(&path)).expect("backup");

        // Something else rewrote the file between display and confirmation.
        write_file(&path, &game_json(&["-Xmx6g"]));
        let current = fs::read(&path).expect("target");

        assert_eq!(
            write_limit(&confirmed, 8, 16 * GIB)
                .expect_err("stale snapshot")
                .code,
            "changed"
        );
        assert_eq!(fs::read(&path).expect("target"), current);
        assert_eq!(fs::read(backup_path(&path)).expect("backup"), backup);

        // The same protection covers a backup that moved after planning.
        let plan = prepare_restore(&path).expect("plan");
        write_file(&backup_path(&path), &game_json(&["-Xmx5g"]));
        assert_eq!(
            restore_config(&plan).expect_err("stale backup").code,
            "changed"
        );
        assert_eq!(fs::read(&path).expect("target"), current);
    }

    #[test]
    fn finds_installations_across_libraries_and_deduplicates() {
        let dir = case_dir("steam");
        let steam = dir.join("Steam");
        let second = dir.join("D Games").join("SteamLibrary");
        let empty = dir.join("Empty Library");
        let without_json = dir.join("Broken Library");

        let make_library = |root: &Path, install_dir: Option<&str>, with_json: bool| -> PathBuf {
            fs::create_dir_all(root.join("steamapps").join("common")).expect("library");
            let Some(install_dir) = install_dir else {
                return PathBuf::new();
            };
            write_file(
                &root.join("steamapps").join(APP_MANIFEST),
                &format!(
                    "\"AppState\"\r\n{{\r\n\t\"appid\"\t\t\"108600\"\r\n\t\"installdir\"\t\t\"{install_dir}\"\r\n}}\r\n"
                ),
            );
            let game = root.join("steamapps").join("common").join(install_dir);
            fs::create_dir_all(&game).expect("game dir");
            if with_json {
                write_file(
                    &game.join(CONFIG_NAME),
                    &game_json(&["-Xms1024m", "-Xmx3072m"]),
                );
            }
            game
        };

        let first_game = make_library(&steam, Some("Project Zomboid"), true);
        let second_game = make_library(&second, Some("Project Zomboid 中文 !"), false);
        // Only a backup survived in the second library: still a valid install.
        write_file(
            &backup_path(&second_game.join(CONFIG_NAME)),
            &game_json(&["-Xmx4g"]),
        );
        make_library(&empty, None, false);
        make_library(&without_json, Some("Project Zomboid"), false);

        let escaped = second.to_string_lossy().replace('\\', r"\\");
        write_file(
            &steam.join("steamapps").join("libraryfolders.vdf"),
            &format!(
                "\"libraryfolders\"\r\n{{\r\n\t\"0\"\r\n\t{{\r\n\t\t\"path\"\t\t\"{escaped}\"\r\n\t}}\r\n     \"1\"\r\n     {{\r\n               \"path\"     \"{}\"\r\n     }}\r\n\t\"2\"\r\n\t{{\r\n\t\t\"path\"\t\t\"{}\"\r\n\t}}\r\n\t\"3\"\r\n\t{{\r\n\t\t\"path\"\t\t\"{}\"\r\n\t}}\r\n}}\r\n",
                empty.display(),
                without_json.display(),
                dir.join("不存在的收藏庫").display(),
            ),
        );

        let roots = vec![
            steam.to_string_lossy().into_owned(),
            format!("\"{}\\\"", steam.to_string_lossy()),
            steam.to_string_lossy().replace('\\', "/"),
        ];
        let mut found = installations_from_roots(&roots);
        found.sort();
        let mut expected = vec![first_game, second_game];
        expected.sort();
        assert_eq!(found, expected);

        assert!(
            installations_from_roots(&[
                empty.to_string_lossy().into_owned(),
                dir.join("完全不存在").to_string_lossy().into_owned(),
            ])
            .is_empty(),
            "libraries without the game yield nothing"
        );
    }

    #[test]
    fn escaped_or_repeated_heap_literals_are_refused() {
        let dir = case_dir("literal");
        let path = dir.join(CONFIG_NAME);

        // The parsed argument has no literal counterpart in the file text.
        write_file(&path, r#"{"vmArgs":["-Xmx\u0034g"]}"#);
        let snapshot = read_config(&path).expect("read");
        assert_eq!(snapshot.heap_bytes, 4 * GIB);
        let err = write_limit(&snapshot, 8, 16 * GIB).expect_err("escaped literal");
        assert_eq!(err.code, "invalid_heap");
        assert_eq!(fs::read(&path).expect("target"), snapshot.bytes);

        // A second literal elsewhere in the document is ambiguous.
        write_file(&path, r#"{"note":"-Xmx4g","vmArgs":["-Xmx4g"]}"#);
        let snapshot = read_config(&path).expect("read");
        assert_eq!(
            write_limit(&snapshot, 8, 16 * GIB)
                .expect_err("two literals")
                .code,
            "invalid_heap"
        );
        assert_eq!(fs::read(&path).expect("target"), snapshot.bytes);
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn a_failed_replacement_keeps_the_candidate_and_the_previous_backup() {
        let dir = case_dir("replace-fail");
        let path = new_config(&dir, CONFIG_NAME, &["-Xms1024m", "-Xmx3072m"]);
        let backup = backup_path(&path);
        // A first write puts a generation into the backup, so the failing one
        // has something to copy aside.
        let first = write_limit(&read_config(&path).expect("read"), 8, 16 * GIB).expect("first");
        let previous_generation = fs::read(&backup).expect("backup");
        let snapshot = read_config(&path).expect("read");

        // Denying DELETE sharing on the target makes ReplaceFileW fail for real.
        let lock = open_shared_read(&path).expect("hold the target");
        let err = write_limit(&snapshot, 10, 16 * GIB).expect_err("sharing violation");
        drop(lock);

        assert_eq!(err.code, "replace_failed");
        assert_eq!(
            fs::read(&path).expect("target"),
            first.bytes,
            "the target survives"
        );
        assert_eq!(
            fs::read(&backup).expect("backup"),
            previous_generation,
            "the backup survives"
        );

        let expected_candidate = String::from_utf8(first.bytes.clone())
            .expect("utf-8")
            .replace("-Xmx8g", "-Xmx10g")
            .into_bytes();
        let kept: Vec<PathBuf> = fs::read_dir(&dir)
            .expect("list the folder")
            .map(|entry| entry.expect("entry").path())
            .filter(|entry| entry.to_string_lossy().contains(".pztmp"))
            .collect();
        assert_eq!(
            kept.len(),
            2,
            "candidate and previous-backup copy are kept: {kept:?}"
        );
        for entry in &kept {
            let name = entry
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned();
            let bytes = fs::read(entry).expect("kept file");
            if name.ends_with(".previous-backup") {
                assert_eq!(bytes, previous_generation, "{name}");
            } else {
                assert_eq!(bytes, expected_candidate, "{name}");
            }
            assert!(
                err.detail.contains(&name),
                "the error must name {name}: {}",
                err.detail
            );
        }
    }

    #[test]
    fn a_restore_pins_the_backup_it_verified() {
        let dir = case_dir("restore-pin");
        let path = new_config(&dir, CONFIG_NAME, &["-Xmx3072m"]);
        let backup = backup_path(&path);
        write_file(&backup, &game_json(&["-Xmx4g"]));
        let target = fs::read(&path).expect("target");
        let pinned_bytes = fs::read(&backup).expect("backup");
        let plan = prepare_restore(&path).expect("plan");

        // The pin a restore holds refuses writers and deleters for its whole
        // life, so the verified backup cannot change while the restore stages.
        let guard = open_shared_read(&backup).expect("pin the backup");
        assert!(
            fs::write(&backup, b"{}").is_err(),
            "a writer must be refused"
        );
        assert!(
            fs::remove_file(&backup).is_err(),
            "a delete must be refused"
        );
        assert_eq!(
            fs::read(&backup).expect("readers stay allowed"),
            pinned_bytes
        );
        drop(guard);

        // And the restore takes that pin: it cannot even start while someone
        // else holds the backup open for writing.
        let writer = OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ)
            .open(&backup)
            .expect("open the backup for writing");
        let err = restore_config(&plan).expect_err("backup held by a writer");
        drop(writer);
        assert_eq!(err.code, "io");
        assert_eq!(
            fs::read(&path).expect("target"),
            target,
            "nothing was overwritten"
        );

        // Unheld, the same plan restores exactly the bytes it verified.
        let restored = restore_config(&plan).expect("restore");
        assert_eq!(restored.bytes, pinned_bytes);
        assert_eq!(fs::read(&path).expect("target"), pinned_bytes);
        assert_eq!(
            fs::read(&backup).expect("backup"),
            pinned_bytes,
            "the backup is kept"
        );
    }
}
