// Copyright 2018-2020 the Deno authors. All rights reserved. MIT license.
// Copyright 2020 the Dvm authors. All rights reserved. MIT license.

use anyhow::{anyhow, Result};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use semver_parser::version::{parse as semver_parse, Version};
use tempfile::TempDir;
use url::Url;
use which::which;

use std::env;
use std::fs;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::string::String;

// TODO(ry) Auto detect target triples for the uploaded files.
#[cfg(windows)]
const ARCHIVE_NAME: &str = "deno-x86_64-pc-windows-msvc.zip";
#[cfg(target_os = "macos")]
const ARCHIVE_NAME: &str = "deno-x86_64-apple-darwin.zip";
#[cfg(target_os = "linux")]
const ARCHIVE_NAME: &str = "deno-x86_64-unknown-linux-gnu.zip";

pub fn upgrade_command(
  dry_run: bool,
  force: bool,
  version: Option<String>,
) -> Result<()> {
  let client_builder = Client::builder();
  let client = client_builder.build()?;

  let current_version = semver_parse(crate::version::DENO).unwrap();

  let install_version = match version {
    Some(passed_version) => match semver_parse(&passed_version) {
      Ok(ver) => {
        if !force && current_version == ver {
          println!("Version {} is already installed", &ver);
          return Ok(());
        } else {
          ver
        }
      }
      Err(_) => {
        eprintln!("Invalid semver passed");
        std::process::exit(1)
      }
    },
    None => {
      let latest_version = get_latest_version(&client)?;

      if !force && current_version >= latest_version {
        println!(
          "Local deno version {} is the most recent release",
          &crate::version::DENO
        );
        return Ok(());
      } else {
        latest_version
      }
    }
  };

  let archive_data = download_package(
    &compose_url_to_exec(&install_version)?,
    client,
    &install_version,
  )?;
  let old_exe_path = which("deno").unwrap();
  let new_exe_path = unpack(archive_data, &install_version)?;
  let permissions = fs::metadata(&old_exe_path)?.permissions();
  fs::set_permissions(&new_exe_path, permissions)?;
  check_exe(&new_exe_path, &install_version)?;

  if !dry_run {
    replace_exe(&new_exe_path, &old_exe_path)?;
  }

  println!("Upgrade done successfully");

  Ok(())
}

fn get_latest_version(client: &Client) -> Result<Version> {
  println!("Checking for latest version");
  let body = client
    .get(Url::parse(
      "https://github.com/denoland/deno/releases/latest",
    )?)
    .send()?
    .text()?;
  let v = find_version(&body)?;
  println!("The latest version is {}", &v);
  Ok(semver_parse(&v).unwrap())
}

fn download_package(
  url: &Url,
  client: Client,
  version: &Version,
) -> Result<Vec<u8>> {
  println!("downloading {}", url);
  let url = url.clone();
  let version = version.clone();

  let mut response = match client.get(url.clone()).send() {
    Ok(response) => response,
    Err(error) => {
      println!("Network error {}", &error);
      std::process::exit(1)
    }
  };

  if response.status().is_success() {
    println!("Version has been found");
    println!("Deno is upgrading to version {}", &version);
  }

  if response.status() == StatusCode::NOT_FOUND {
    println!("Version has not been found, aborting");
    std::process::exit(1)
  }

  if response.status().is_client_error() || response.status().is_server_error()
  {
    println!("Download '{}' failed: {}", &url, response.status());
    std::process::exit(1)
  }

  let mut buf: Vec<u8> = vec![];
  response.copy_to(&mut buf)?;
  Ok(buf)
}

fn compose_url_to_exec(version: &Version) -> Result<Url> {
  let s = format!(
    "https://github.com/denoland/deno/releases/download/v{}/{}",
    version, ARCHIVE_NAME
  );
  Ok(Url::parse(&s)?)
}

fn find_version(text: &str) -> Result<String> {
  let re = Regex::new(r#"v(\d+\.\d+\.\d+) "#)?;
  if let Some(_mat) = re.find(text) {
    let mat = _mat.as_str();
    return Ok(mat[1..mat.len() - 1].to_string());
  }
  Err(anyhow!("Cannot read latest tag version"))
}

fn unpack(archive_data: Vec<u8>, version: &Version) -> Result<PathBuf> {
  let dvm_dir = get_dvm_root()?.join(format!("{}", version));
  fs::create_dir_all(&dvm_dir)?;
  let exe_ext = if cfg!(windows) { "exe" } else { "" };
  let exe_path = dvm_dir.join("deno").with_extension(exe_ext);

  let archive_ext = Path::new(ARCHIVE_NAME)
    .extension()
    .and_then(|ext| ext.to_str())
    .unwrap();
  let unpack_status = match archive_ext {
    "gz" => {
      let exe_file = fs::File::create(&exe_path)?;
      let mut cmd = Command::new("gunzip")
        .arg("-c")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(exe_file))
        .spawn()?;
      cmd.stdin.as_mut().unwrap().write_all(&archive_data)?;
      cmd.wait()?
    }
    "zip" if cfg!(windows) => {
      let archive_path = dvm_dir.join("deno.zip");
      fs::write(&archive_path, &archive_data)?;
      Command::new("powershell.exe")
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(
          "& {
            param($Path, $DestinationPath)
            trap { $host.ui.WriteErrorLine($_.Exception); exit 1 }
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            [System.IO.Compression.ZipFile]::ExtractToDirectory(
              $Path,
              $DestinationPath
            );
          }",
        )
        .arg("-Path")
        .arg(format!("'{}'", &archive_path.to_str().unwrap()))
        .arg("-DestinationPath")
        .arg(format!("'{}'", &dvm_dir.to_str().unwrap()))
        .spawn()?
        .wait()?
    }
    "zip" => {
      let archive_path = dvm_dir.join("deno.zip");
      fs::write(&archive_path, &archive_data)?;
      Command::new("unzip")
        .current_dir(&dvm_dir)
        .arg(archive_path)
        .spawn()?
        .wait()?
    }
    ext => panic!("Unsupported archive type: '{}'", ext),
  };
  assert!(unpack_status.success());
  assert!(exe_path.exists());
  Ok(exe_path)
}

fn replace_exe(new: &Path, old: &Path) -> Result<()> {
  if cfg!(windows) {
    // On windows you cannot replace the currently running executable.
    // so first we rename it to deno.old.exe
    fs::rename(old, old.with_extension("old.exe"))?;
  } else {
    fs::remove_file(old)?;
  }
  fs::copy(new, old)?;
  Ok(())
}

fn check_exe(exe_path: &Path, expected_version: &Version) -> Result<()> {
  let output = Command::new(exe_path)
    .arg("-V")
    .stderr(std::process::Stdio::inherit())
    .output()?;
  let stdout = String::from_utf8(output.stdout)?;
  assert!(output.status.success());
  assert_eq!(stdout.trim(), format!("deno {}", expected_version));
  Ok(())
}

fn get_dvm_root() -> Result<PathBuf> {
  // Note: on Windows, the $HOME environment variable may be set by users or by
  // third party software, but it is non-standard and should not be relied upon.
  let home_env_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
  let mut home_path = match env::var_os(home_env_var).map(PathBuf::from) {
    Some(home_path) => home_path,
    None => {
      // Use temp dir
      TempDir::new()?.into_path()
    }
  };

  home_path.push(".dvm");
  Ok(home_path)
}

#[test]
fn test_compose_url_to_exec() {
  let v = semver_parse("0.0.1").unwrap();
  let url = compose_url_to_exec(&v).unwrap();
  #[cfg(windows)]
  assert_eq!(url.as_str(), "https://github.com/denoland/deno/releases/download/v0.0.1/deno-x86_64-pc-windows-msvc.zip");
  #[cfg(target_os = "macos")]
  assert_eq!(
    url.as_str(),
    "https://github.com/denoland/deno/releases/download/v0.0.1/deno-x86_64-apple-darwin.zip"
  );
  #[cfg(target_os = "linux")]
  assert_eq!(url.as_str(), "https://github.com/denoland/deno/releases/download/v0.0.1/deno-x86_64-unknown-linux-gnu.zip");
}
