use clap::Parser;
use clap::Subcommand;
use log::{error, info};
use std::fs::File;
use std::io::BufRead;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use toml::Table;
use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes};
const MAGIC_WORD: u32 = 0xABCD5432;

#[derive(TryFromBytes, IntoBytes, Immutable, PartialEq, KnownLayout, Copy, Clone, Debug)]
#[repr(packed)]
struct OtaHead {
    // Always 0xABCD5432, 0b10101011110011010101010000110010
    magic_word: u32,
    // CRC 16 (IBM SDLC) checksum of Everything that is valid after magic_word
    crc: u16,
    version: [u8; 32],
    project_name: [u8; 16],
    timestamp: u64,
    // The size of the firmware in bytes, HEAD size not included
    size: u32,
    reserved: [u8; 446],
}
static_assertions::const_assert!(core::mem::size_of::<OtaHead>() == 512);

#[derive(Debug, Subcommand, Clone)]
#[clap(disable_help_subcommand = true)]
enum Commands {
    #[command(about = "Create the ota bin file")]
    Encode {
        #[arg(help = "Path to project Cargo.toml's directory")]
        path: String,
    },
    #[command(about = "Decode the ota bin file")]
    Decode {
        #[arg(help = "Path to ota bin file")]
        path: String,
    },
}

#[derive(Debug, Parser)]
#[command(
    about = "Create a OTA bin file for the given project. Run cargo build --release in target directory to build the project before running this tool."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

fn run_command_live(mut cmd: Child, what: &String) -> ExitStatus {
    let stdout = cmd.stdout.take().expect("Failed to capture stdout");
    let stderr = cmd.stderr.take().expect("Failed to capture stderr");

    let handle_stdout = std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            println!("{}", line.expect("Failed to read line from stdout"));
        }
    });

    let handle_stderr = std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines() {
            eprintln!("{}", line.expect("Failed to read line from stderr"));
        }
    });

    handle_stdout.join().expect("Failed to join stdout thread");
    handle_stderr.join().expect("Failed to join stderr thread");

    let status = cmd
        .wait()
        .expect(format!("Failed to wait on {what}").as_str());
    status
}

async fn encode(path: String) {
    let path = PathBuf::from(path);
    // Check if the file exists
    if !path.exists() {
        error!("File does not exist: {}", path.display());
        return;
    }
    // Read the file
    let file_bytes = tokio::fs::read(path.join("Cargo.toml")).await.unwrap();
    let file: String = String::from_utf8(file_bytes).unwrap();
    // Try to find "embassy" in the file
    let embassy_index = file.find("embassy");
    if embassy_index.is_none() {
        error!(
            "Cargo.toml does not seem contain embassy, are you sure this is a embedded project?"
        );
        std::process::exit(1);
    }

    let git_hash = std::process::Command::new("git")
        .args(&["rev-parse", "--short", "HEAD"])
        .current_dir(path.clone())
        .output()
        .expect("Failed to execute git command")
        .stdout;
    let git_hash = std::str::from_utf8(&git_hash).expect("Failed to parse git hash");
    let git_is_dirty = std::process::Command::new("git")
        .args(&["status", "--porcelain"])
        .current_dir(path.clone())
        .output()
        .expect("Failed to execute git command")
        .status
        .success();
    // Remove all spaces, tabs, and newlines
    let git_hash = git_hash
        .replace(" ", "")
        .replace("\t", "")
        .replace("\n", "");
    let git_hash = if git_is_dirty {
        format!("{}-dirty", git_hash)
    } else {
        git_hash.to_string()
    };
    info!("Git hash: {}", git_hash);
    let mut gh = git_hash.bytes().collect::<Vec<u8>>();
    if gh.len() > 31 {
        panic!("git hash is too long");
    }
    gh.push(0);
    let mut version = [0u8; 32];
    version[..gh.len()].copy_from_slice(&gh);

    let value = toml::from_str::<Table>(&file).unwrap();
    let project_name = value["package"]["name"].as_str().unwrap().to_string();
    info!("Project name: {}", project_name);

    // Check if target/thumbv7em-none-eabihf/release/{project_name} exists
    let bin_path = path
        .join("target/thumbv7em-none-eabihf/release")
        .join(project_name.clone());
    if !bin_path.exists() {
        error!(
            "Bin file does not exist: {}. Did you run cargo build --release first?",
            bin_path.display()
        );
        return;
    }

    // todo Check if arm-none-eabi-objcopy is installed, if not hint the user to install it

    let cmd = std::process::Command::new("arm-none-eabi-objcopy")
        .arg("-I")
        .arg("elf32-littlearm")
        .arg("-O")
        .arg("binary")
        .arg(
            path.join("target/thumbv7em-none-eabihf/release")
                .join(project_name.clone())
                .as_path(),
        )
        .arg(path.join("xstd-app-tool-temp.bin").as_path())
        .current_dir(path.clone())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to run apt-get update");
    let status = run_command_live(cmd, &("arm-none-eabi-objcopy".to_string()));
    if status.success() {
        info!("objcopy success");
    } else {
        error!("objcopy failed, {}", status);
    }
    let project_name = project_name.as_bytes();
    if project_name.len() > 15 {
        panic!("project name is too long");
    }
    let mut project_name_string = project_name.to_vec();
    project_name_string.push(0);
    let mut project_name = [0u8; 16];
    project_name[..project_name_string.len()].copy_from_slice(&project_name_string);

    let mut file_bytes = tokio::fs::read(path.join("xstd-app-tool-temp.bin"))
        .await
        .unwrap();
    // Fill the file to the nearest 8 bytes.
    // This is important!
    while file_bytes.len() % 8 != 0 {
        file_bytes.push(0xFF);
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let mut ota_head = OtaHead {
        magic_word: MAGIC_WORD,
        crc: 0,
        version,
        project_name,
        timestamp,
        size: file_bytes.len() as u32,
        reserved: [0; 446],
    };
    let mut ota_bytes = ota_head.as_bytes().to_vec();
    while ota_bytes.len() < 512 {
        ota_bytes.push(0xFF);
    }
    assert!(ota_bytes.len() == 512);

    let file_name = format!(
        "{}-{}-ota.bin",
        value["package"]["name"].as_str().unwrap().to_string(),
        git_hash
    );
    let file_path = path.join(file_name);
    let mut file = File::create(file_path.clone()).unwrap();
    info!("Created ota bin file at {}", file_path.display());
    let final_length = 512 + file_bytes.len();
    let mut total_bytes = [ota_bytes, file_bytes].concat();
    assert!(total_bytes.len() == final_length);
    const X25: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_IBM_SDLC);
    let crc = X25.checksum(&total_bytes.as_slice()[6..]);
    let crc_bytes = crc.to_le_bytes();
    ota_head.crc = crc;
    // little endian
    total_bytes[4] = crc_bytes[0];
    total_bytes[5] = crc_bytes[1];

    info!("OTA head: {:?}", ota_head);
    // Write the file
    file.write_all(total_bytes.as_bytes()).unwrap();
    file.sync_all().unwrap();
    // Remove the temp file
    std::fs::remove_file(path.join("xstd-app-tool-temp.bin")).unwrap();
}

async fn decode(path: String) {
    let path = PathBuf::from(path);
    let file_bytes = tokio::fs::read(path).await.expect("Failed to read file");
    if file_bytes.len() < 512 {
        error!("File is too short to be an ota bin file");
        std::process::exit(1);
    }
    let ota_head_bytes = &file_bytes[0..512];
    let ota_head = match OtaHead::try_read_from_bytes(ota_head_bytes) {
        Ok(ota_head) => ota_head,
        Err(e) => {
            error!("Invalid ota head {}", e);
            std::process::exit(1);
        }
    };
    if ota_head.magic_word != 0xABCD5432 {
        error!("Invalid magic word");
        std::process::exit(1);
    }
    const X25: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_IBM_SDLC);
    let crc = X25.checksum(&file_bytes[6..]);
    let expected_crc = ota_head.crc;
    if crc != expected_crc {
        error!(
            "CRC mismatch, expected 0x{:X}, got 0x{:X}",
            expected_crc, crc
        );
        std::process::exit(1);
    }
    let build_time =
        match chrono::DateTime::<chrono::Utc>::from_timestamp(ota_head.timestamp as i64, 0) {
            Some(t) => t.to_rfc3339(),
            None => "Unknown".to_string(),
        };
    let firmware_size = ota_head.size;
    info!(
        "Valid Bin File!:\n  Project Name: {}\n  Version: {}\n  Created at: {}\n  Firmware Size: {}B ({:.2}KB)",
        String::from_utf8_lossy(&ota_head.project_name),
        String::from_utf8_lossy(&ota_head.version),
        build_time,
        firmware_size,
        firmware_size as f32 / 1024.0
    );
}

#[tokio::main]
async fn main() {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
    );

    let mut args: Vec<_> = std::env::args_os().collect();
    // info!("Args: {:?}", args);
    let args = if args.len() >= 2 {
        if args[1].to_str() == Some("hfmp") {
            // info!("Seem to be calling from cargo hfmp!");
            args.remove(1);
            // info!("Args: {:?}", args);
            Cli::parse_from(args)
        } else {
            Cli::parse()
        }
    } else {
        Cli::parse()
    };
    // info!("Args: {:?}", args);
    let cmd = args.command;
    match cmd {
        Commands::Encode { path } => encode(path).await,
        Commands::Decode { path } => decode(path).await,
    }
}
