//! A pure-Rust library to work with GPT partition tables.
//!
//! It provides support for manipulating (R/W) GPT headers and partition
//! tables. Raw disk devices as well as disk images are supported.
//!
//! ```rust,no_run
//! extern crate gpt;
//!
//! fn inspect_disk() {
//!     let diskpath = std::path::Path::new("/dev/sdz");
//!     let cfg = gpt::GptConfig::new().writable(false);
//!
//!     let disk = cfg.open(diskpath).expect("failed to open disk");
//!
//!     println!("Disk header: {:#?}", disk.primary_header());
//!     println!("Partition layout: {:#?}", disk.partitions());
//! }
//! ```

#[macro_use]
extern crate bitflags;
extern crate byteorder;
extern crate crc;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;
extern crate uuid;

use std::io::Write;
use std::{fs, io, path};

pub mod disk;
pub mod header;
pub mod mbr;
pub mod partition;
mod partition_types;

/// Configuration options to open a GPT disk.
#[derive(Debug, Eq, PartialEq)]
pub struct GptConfig {
    /// Logical block size.
    lb_size: disk::LogicalBlockSize,
    /// Whether to open a GPT partition table in writable mode.
    writable: bool,
    /// Whether to expect and parse an initialized disk image.
    initialized: bool,
    /// Whether to require and validate backup header and table.
    require_backup: bool,
}

impl GptConfig {
    // TODO(lucab): complete support for skipping backup
    // header, etc, then expose all config knobs here.

    /// Create a new default configuration.
    pub fn new() -> Self {
        GptConfig::default()
    }

    /// Whether to open a GPT partition table in writable mode.
    pub fn writable(mut self, writable: bool) -> Self {
        self.writable = writable;
        self
    }

    /// Whether to assume an initialized GPT disk and read its
    /// partition table on open.
    pub fn initialized(mut self, initialized: bool) -> Self {
        self.initialized = initialized;
        self
    }

    /// Whether to require and validate a backup header.
    pub fn require_backup(mut self, required: bool) -> Self {
        self.require_backup = required;
        self
    }

    /// Size of logical blocks (sectors) for this disk.
    pub fn logical_block_size(mut self, lb_size: disk::LogicalBlockSize) -> Self {
        self.lb_size = lb_size;
        self
    }

    /// Open the GPT disk at the given path and inspect it according
    /// to configuration options.
    pub fn open(self, diskpath: &path::Path) -> io::Result<GptDisk> {
        // Uninitialized disk, no headers/table to parse.
        if !self.initialized {
            let file = fs::OpenOptions::new()
                .write(self.writable)
                .read(true)
                .open(diskpath)?;
            let empty = GptDisk {
                config: self,
                file,
                guid: uuid::Uuid::new_v4(),
                path: diskpath.to_path_buf(),
                mbr: mbr::ProtectiveMBR::new(),
                primary_header: None,
                primary_partition_table: None,
                backup_header: None,
                backup_partition_table: None,
            };
            return Ok(empty);
        }

        // Proper GPT disk, fully inspect its layout.
        let mut file = fs::OpenOptions::new()
            .write(self.writable)
            .read(true)
            .open(diskpath)?;
        let h1 = header::read_primary_header(&mut file, self.lb_size)?;
        let table = partition::file_read_partitions(&mut file, &h1, self.lb_size)?;

        let (h2, p2) = if self.require_backup {
            let h = Some(header::read_backup_header(&mut file, self.lb_size)?);
            let t = Some(table.clone());
            (h, t)
        } else {
            (None, None)
        };
        let disk = GptDisk {
            config: self,
            file,
            guid: h1.disk_guid,
            path: diskpath.to_path_buf(),
            mbr: mbr::ProtectiveMBR::new(),
            primary_header: Some(h1),
            primary_partition_table: Some(table),
            backup_header: h2,
            backup_partition_table: p2,
        };
        Ok(disk)
    }
}

impl Default for GptConfig {
    fn default() -> Self {
        Self {
            lb_size: disk::DEFAULT_SECTOR_SIZE,
            initialized: true,
            require_backup: true,
            writable: false,
        }
    }
}

/// A file-backed GPT disk.
#[derive(Debug)]
pub struct GptDisk {
    config: GptConfig,
    file: fs::File,
    guid: uuid::Uuid,
    path: path::PathBuf,
    mbr: mbr::ProtectiveMBR,
    primary_header: Option<header::Header>,
    primary_partition_table: Option<Vec<partition::Partition>>,
    backup_header: Option<header::Header>,
    backup_partition_table: Option<Vec<partition::Partition>>,
}

impl GptDisk {
    /// Retrieve protective-MBR, if any.
    pub fn protective_mbr(&self) -> &mbr::ProtectiveMBR {
        &self.mbr
    }

    /// Retrieve primary header, if any.
    pub fn primary_header(&self) -> Option<&header::Header> {
        self.primary_header.as_ref()
    }

    /// Retrieve backup header, if any.
    pub fn backup_header(&self) -> Option<&header::Header> {
        self.backup_header.as_ref()
    }

    /// Retrieve partition entries.
    pub fn partitions(&self) -> Option<&[partition::Partition]> {
        let pt1 = match self.primary_partition_table {
            Some(ref pp) => Some(pp.as_slice()),
            None => None,
        };
        let pt2 = match self.backup_partition_table {
            Some(ref pp) => Some(pp.as_slice()),
            None => None,
        };

        match (pt1, pt2) {
            (p1, None) => p1,
            (p1, p2) if p1 != p2 => p1,
            (p1, _) => p1,
        }
    }

    /// Retrieve disk UUID.
    pub fn guid(&self) -> &uuid::Uuid {
        &self.guid
    }

    /// Retrieve disk logical block size.
    pub fn logical_block_size(&self) -> &disk::LogicalBlockSize {
        &self.config.lb_size
    }

    /// Update disk UUID.
    ///
    /// If no UUID is specified, a new random one is generated.
    /// No changes are recorded to disk until `write()` is called.
    pub fn update_guid(&mut self, uuid: Option<uuid::Uuid>) -> io::Result<&Self> {
        let guid = match uuid {
            Some(u) => u,
            None => {
                let u = uuid::Uuid::new_v4();
                debug!("Generated random uuid: {}", u);
                u
            }
        };
        self.guid = guid;
        Ok(self)
    }

    /// Update current partition table.
    ///
    /// No changes are recorded to disk until `write()` is called.
    pub fn update_partitions(&mut self, pp: Vec<partition::Partition>) -> io::Result<&Self> {
        // TODO(lucab): validate partitions.
        let bak = header::find_backup_lba(&mut self.file, self.config.lb_size)?;
        let h1 = header::Header::compute_new(true, &pp, self.guid, bak, 2)?;
        let h2 = header::Header::compute_new(false, &pp, self.guid, bak, 2)?;
        self.primary_header = Some(h1);
        self.backup_header = Some(h2);
        self.primary_partition_table = Some(pp.clone());
        self.backup_partition_table = Some(pp);
        self.config.initialized = true;
        Ok(self)
    }

    /// Persist state to disk, consuming this disk object.
    ///
    /// This is a destructive action, as it overwrite headers and
    /// partitions entries on disk. All writes are flushed to disk
    /// before returning the underlying `File` object.
    pub fn write(mut self) -> io::Result<fs::File> {
        if !self.config.writable {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "disk not opened in writable mode",
            ));
        }

        if !self.config.initialized {
            self.mbr.overwrite_lba0(&mut self.file)?;
        } else {
            self.mbr.update_conservative(&mut self.file)?;
        }

        let bak = header::find_backup_lba(&mut self.file, self.config.lb_size)?;
        let p2 = self.backup_partition_table
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "missing backup partition table"))?;
        let h2 = header::Header::compute_new(true, &p2, self.guid, bak, 2)?;
        let p1 = self.primary_partition_table
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "missing backup partition table"))?;
        let h1 = header::Header::compute_new(true, &p1, self.guid, bak, 2)?;
        // TODO(lucab): write partition entries to disk.
        h2.write_backup(&mut self.file, self.config.lb_size)?;
        h1.write_primary(&mut self.file, self.config.lb_size)?;
        self.file.flush()?;
        self.primary_header = Some(h1);
        self.backup_header = Some(h2);

        Ok(self.file)
    }
}
