extern crate gpt;
extern crate simplelog;

use gpt::{disk, header, mbr, partition};
use simplelog::{Config, LevelFilter, SimpleLogger};
use std::{fs, io, path};

fn main() {
    // Setup logging
    let _ = SimpleLogger::init(LevelFilter::Trace, Config::default());

    // Inspect disk image, handling errors.
    if let Err(e) = run() {
        eprintln!("Failed to duplicate table: {}", e);
        std::process::exit(1)
    }
}

fn run() -> io::Result<()> {
    // First parameter is target disk image (optional, default: fixtures sample)
    let sample = "tests/fixtures/gpt-linux-disk-01.img".to_string();
    let input = std::env::args().nth(1).unwrap_or(sample);

    let outpath = path::Path::new("tests/fixtures/output.img");

    // Open disk image.
    let diskpath = path::Path::new(&input);
    let src = gpt::GptConfig::new().writable(false).open(diskpath)?;
    let pp = src.partitions().unwrap();

    let mut output = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .create(true)
        .open(outpath)?;

    let h = src.primary_header().unwrap();
    let old_len = (h.backup_lba + 1) * 512;
    output.set_len(old_len)?;

    let lba0 = mbr::ProtectiveMBR::new();
    lba0.overwrite_lba0(&mut output)?;
    partition::write_partitions(&mut output, 2, 128, pp, disk::LogicalBlockSize::Lb512)?;
    let h1 = header::Header::compute_new(true, pp, *src.guid(), h.backup_lba, 2)?;
    h1.write_primary(&mut output, disk::LogicalBlockSize::Lb512)?;
    h1.write_backup(&mut output, disk::LogicalBlockSize::Lb512)?;

    //h1.write_primary(&mut output, disk::LogicalBlockSize::Lb4096)?;

    output.sync_all()?;

    Ok(())
}
