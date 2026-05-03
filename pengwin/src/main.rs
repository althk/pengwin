mod cli;
mod fs_context;
mod fsp;
mod fsp_context_impl;
mod fsp_impl;
mod cache;

use cache::CachedBlockDevice;

use clap::Parser;
use cli::{Cli, Command};
use fs_context::Ext4Fs;

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Mount {
            source,
            drive,
            verbose,
        } => {
            init_logging(verbose);
            if let Err(e) = cmd_mount(&source, &drive) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Command::Unmount { drive } => {
            if let Err(e) = cmd_unmount(&drive) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn init_logging(verbose: bool) {
    use tracing_subscriber::EnvFilter;
    let filter = if verbose {
        "pengwin=debug,ext4_core=debug"
    } else {
        "pengwin=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .init();
}

fn normalize_drive_letter(drive: &str) -> Result<String, String> {
    // let letter = drive.trim_end_matches(':').to_uppercase();
    // if letter.len() != 1 || !letter.chars().next().unwrap().is_ascii_alphabetic() {
    //     return Err(format!("invalid drive letter: {drive}"));
    // }
    // Ok(format!("{}:", letter))
    return Ok(drive.to_string());
}

fn cmd_mount(source: &str, drive: &str) -> Result<(), Box<dyn std::error::Error>> {
    let _fsp_init = fsp::check_winfsp_installed()?;
    let mountpoint = normalize_drive_letter(drive).map_err(|e| e)?;

    let is_raw_disk = source.starts_with(r"\\.\") || source.starts_with(r"\\?\");

    if is_raw_disk {
        #[cfg(not(windows))]
        return Err("raw disk access is Windows-only".into());

        #[cfg(windows)]
        {
            let dev = ext4_core::block_device::raw_disk::RawDiskDevice::open(
                std::path::Path::new(source),
            )?;
            let dev = CachedBlockDevice::new(dev, 4096);
            mount_fs(Ext4Fs::open(dev)?, source, drive, &mountpoint)
        }
    } else {
        let dev = ext4_core::block_device::image_file::ImageFileDevice::open(
            std::path::Path::new(source),
        )?;
        let dev = CachedBlockDevice::new(dev, 4096);
        mount_fs(Ext4Fs::open(dev)?, source, drive, &mountpoint)
    }
}

fn mount_fs<D>(
    fs: Ext4Fs<D>,
    source: &str,
    drive: &str,
    mountpoint: &str,
) -> Result<(), Box<dyn std::error::Error>>
where
    D: ext4_core::block_device::BlockDevice + Send + Sync + 'static,
{
    tracing::info!(source, mountpoint, "mounting ext4 filesystem");

    let mut host = fsp::Ext4Host::new(fs)?;
    host.mount(mountpoint)?;

    let (tx, rx) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })?;

    host.start()?;
    println!(
        "Mounted {} at {} (read-only). Press Ctrl+C to unmount.",
        source, drive
    );
    rx.recv().ok();
    host.stop();
    host.unmount();

    println!("Unmounted.");
    Ok(())
}

fn cmd_unmount(drive: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mountpoint = normalize_drive_letter(drive).map_err(|e| e)?;

    // WinFsp doesn't expose a remote stop signal API; use `net use /delete`.
    let status = std::process::Command::new("net")
        .args(["use", &mountpoint, "/delete"])
        .status()?;

    if !status.success() {
        return Err(format!(
            "net use {mountpoint} /delete failed (exit code {:?}); \
             try Ctrl+C in the mount process instead",
            status.code()
        )
        .into());
    }

    println!("Unmount signal sent to {}", drive);
    Ok(())
}
