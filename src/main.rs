use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

mod sysv;

use sysv::Sysv;

#[derive(Parser)]
#[command(name = "mkfs-sysv", version, about = "Create SVR4 filesystem images")]
struct Cli {
    /// Populate from this directory tree (recursive)
    #[arg(short = 'd', long)]
    directory: Option<PathBuf>,

    /// Output filename
    filename: PathBuf,

    /// Image size in megabytes
    size_mb: u32,
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut sysv = Sysv::new(cli.size_mb)?;
    if let Some(src) = &cli.directory {
        sysv.populate_root(src)?;
    }
    let stats = sysv.write_to(&cli.filename)?;

    println!(
        "Created SVR4 image: {} ({}MB, {} blocks, {} inodes, magic=0x{:x})",
        cli.filename.display(),
        cli.size_mb,
        stats.total_blocks,
        stats.ninodes,
        sysv::MAGIC,
    );
    println!(
        "First data zone: {}, free blocks: {}, free inodes: {}",
        sysv::FIRST_DATA_ZONE,
        stats.free_blocks,
        stats.free_inodes,
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mkfs-sysv: {e:#}");
            ExitCode::FAILURE
        }
    }
}
