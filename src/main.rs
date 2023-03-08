use simple_logger::SimpleLogger;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::{exit, Command, Stdio};
use structopt::StructOpt;

use log::{error, info};

mod config;

const PROGRESS_FLAG: &str = "--info=progress2";

#[derive(StructOpt, Debug)]
pub struct RemoteOpts {
    /// The name of the remote specified in the config
    #[structopt(short = "r", long = "remote")]
    name: Option<String>,

    /// Remote ssh build server with user or the name of the ssh entry
    #[structopt(short = "H", long = "remote-host")]
    host: Option<String>,

    /// The ssh port to communicate with the build server
    #[structopt(short = "p", long = "remote-ssh-port")]
    ssh_port: Option<u16>,

    /// The directory where cargo builds the project
    #[structopt(short, long = "remote-temp-dir")]
    temp_dir: Option<String>,

    #[structopt(
        short = "e",
        long = "env",
        help = "Environment profile. default_value = /etc/profile"
    )]
    env: Option<String>,
}

#[derive(StructOpt, Debug)]
#[structopt(name = "cargo-remote", bin_name = "cargo")]
enum Opts {
    #[structopt(name = "remote")]
    Remote {
        #[structopt(flatten)]
        remote_opts: RemoteOpts,

        #[structopt(
            short = "c",
            long = "copy-back",
            help = "Transfer the target folder or specific file from that folder back to the local machine"
        )]
        copy_back: Option<Option<String>>,

        #[structopt(
            long = "no-copy-lock",
            help = "don't transfer the Cargo.lock file back to the local machine"
        )]
        no_copy_lock: bool,

        #[structopt(
            long = "manifest-path",
            help = "Path to the manifest to execute",
            default_value = "Cargo.toml",
            parse(from_os_str)
        )]
        manifest_path: PathBuf,

        #[structopt(
            short = "h",
            long = "transfer-hidden",
            help = "Transfer hidden files and directories to the build server"
        )]
        hidden: bool,
    },
}

fn main() {
    SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .with_utc_timestamps()
        .env()
        .init()
        .unwrap();

    let Opts::Remote {
        remote_opts,
        copy_back,
        no_copy_lock,
        manifest_path,
        hidden,
    } = Opts::from_args();

    let mut metadata_cmd = cargo_metadata::MetadataCommand::new();
    metadata_cmd.manifest_path(manifest_path).no_deps();

    let project_metadata = metadata_cmd.exec().unwrap();
    let project_dir = project_metadata.workspace_root;
    info!("Project dir: {:?}", project_dir);

    let conf = match config::Config::new(&project_dir) {
        Ok(conf) => conf,
        Err(error) => {
            error!("{}", error);
            exit(-3);
        }
    };

    let remote = match conf.get_remote(&remote_opts) {
        Some(remote) => remote,
        None => {
            error!("No remote build server was defined (use config file or the --remote flags)");
            exit(4);
        }
    };

    let build_server = remote.host;

    // generate a unique build path by using the hashed project dir as folder on the remote machine
    let mut hasher = DefaultHasher::new();
    project_dir.hash(&mut hasher);
    let build_path = format!("{}/{}/", remote.temp_dir, hasher.finish());

    info!("Transferring sources to build server.");
    // transfer project to build server
    let mut rsync_to = Command::new("rsync");
    rsync_to
        .arg("-a".to_owned())
        .arg("--delete")
        .arg("--compress")
        .arg("-e")
        .arg(format!("ssh -p {}", remote.ssh_port))
        .arg(PROGRESS_FLAG)
        .arg("--exclude")
        .arg("target");

    if !hidden {
        rsync_to.arg("--exclude").arg(".*");
    }

    rsync_to
        .arg("--rsync-path")
        .arg("mkdir -p rust && rsync")
        .arg(format!("{}/", project_dir.to_string_lossy()))
        .arg(format!("{}:{}", build_server, build_path))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            error!("Failed to transfer project to build server (error: {})", e);
            exit(-4);
        });
    info!("Environment profile: {:?}", remote.env);
    info!("Build path: {:?}", build_path);
    let build_command = format!("source {}; cd {}; nix-shell;", remote.env, build_path,);

    info!("Starting build process.");
    let output = Command::new("ssh")
        .args(&["-p", &remote.ssh_port.to_string()])
        .arg("-t")
        .arg(&build_server)
        .arg(build_command)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            error!("Failed to run cargo command remotely (error: {})", e);
            exit(-5);
        });

    if let Some(file_name) = copy_back {
        info!("Transferring artifacts back to client.");
        let file_name = file_name.unwrap_or_else(String::new);
        Command::new("rsync")
            .arg("-a")
            .arg("--delete")
            .arg("--compress")
            .arg("-e")
            .arg(format!("ssh -p {}", remote.ssh_port))
            .arg(PROGRESS_FLAG)
            .arg(format!(
                "{}:{}target/{}",
                build_server, build_path, file_name
            ))
            .arg(format!(
                "{}/target/{}",
                project_dir.to_string_lossy(),
                file_name
            ))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::inherit())
            .output()
            .unwrap_or_else(|e| {
                error!(
                    "Failed to transfer target back to local machine (error: {})",
                    e
                );
                exit(-6);
            });
    }

    if !no_copy_lock {
        info!("Transferring Cargo.lock file back to client.");
        Command::new("rsync")
            .arg("-a")
            .arg("--delete")
            .arg("--compress")
            .arg("-e")
            .arg(format!("ssh -p {}", remote.ssh_port))
            .arg(PROGRESS_FLAG)
            .arg(format!("{}:{}Cargo.lock", build_server, build_path))
            .arg(format!("{}/Cargo.lock", project_dir.to_string_lossy()))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::inherit())
            .output()
            .unwrap_or_else(|e| {
                error!(
                    "Failed to transfer Cargo.lock back to local machine (error: {})",
                    e
                );
                exit(-7);
            });
    }

    if !output.status.success() {
        exit(output.status.code().unwrap_or(1))
    }
}
