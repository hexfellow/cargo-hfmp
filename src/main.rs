use clap::Parser;
use log::{error, info};
use std::fs::File;
use std::io::BufRead;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use toml::Table;
use zerocopy::{byte_slice, FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes};
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
// static_assertions::const_assert!(core::mem::size_of::<OtaHead>() == 512);

#[derive(Debug, Parser)]
#[command(
    about = "Create a OTA bin file for the given project. Run cargo build --release in target directory to build the project before running this tool."
)]
pub struct Cli {
    #[arg(help = "Path to target Cargo.toml")]
    pub path: String,
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

#[tokio::main]
async fn main() {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
    );
    let args = Cli::parse();
    info!("Args: {:?}", args);
    let path = PathBuf::from(args.path);
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
    // Remove all spaces, tabs, and newlines
    let git_hash = git_hash
        .replace(" ", "")
        .replace("\t", "")
        .replace("\n", "");
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
    // arm-none-eabi-objcopy -I elf32-littlearm  -O binary ./target/thumbv7em-none-eabihf/release/stm-bootloader a.bin
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
    // Fill the file to the nearest 8 bytes
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
