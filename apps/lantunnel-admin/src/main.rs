use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use fs2::FileExt as _;
use tp_core::config::DEFAULT_GATEWAY_MAPPING_PROBE_PORT;
use tp_core::provisioning::{normalize_certificate_pem, GatewayBootstrapV2, TunnelOwnerFileV2};

#[derive(Parser)]
#[command(name = "lantunnel-admin", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the owner-only Tunnel file and the public Gateway Scope.
    InitTunnel(InitTunnelArgs),
    /// Allocate and sign one Peer from an owner-only Tunnel file.
    AddPeer(AddPeerArgs),
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("gateway_address")
        .required(true)
        .multiple(true)
        .args(["gateway_host", "gateway_ip"])
))]
struct InitTunnelArgs {
    #[arg(long, value_enum)]
    gateway_transport: GatewayTransport,
    #[arg(long)]
    gateway_host: Option<String>,
    #[arg(long)]
    gateway_ip: Option<IpAddr>,
    #[arg(long)]
    gateway_port: u16,
    /// Public UDP port the Gateway uses for NAT mapping probes.
    #[arg(long, default_value_t = DEFAULT_GATEWAY_MAPPING_PROBE_PORT)]
    gateway_mapping_port: u16,
    /// Certificate-only PEM trust anchor for a self-signed or private-CA Gateway.
    #[arg(long)]
    gateway_cert: Option<PathBuf>,
    #[arg(long, default_value = ".")]
    output_dir: PathBuf,
}

#[derive(Args)]
struct AddPeerArgs {
    #[arg(long)]
    tunnel: PathBuf,
    #[arg(long)]
    overlay_ip: Option<Ipv4Addr>,
    #[arg(long, default_value_t = 1)]
    replicas: u16,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum GatewayTransport {
    Quic,
    Websocket,
    Grpc,
}

impl GatewayTransport {
    fn as_str(self) -> &'static str {
        match self {
            Self::Quic => "quic",
            Self::Websocket => "websocket",
            Self::Grpc => "grpc",
        }
    }
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::InitTunnel(args) => init_tunnel(args),
        Command::AddPeer(args) => add_peer(args),
    }
}

fn add_peer(args: AddPeerArgs) -> Result<()> {
    let tunnel_path = args.tunnel;
    let lock_path = sidecar_lock_path(&tunnel_path);
    let lock_file = open_lock_file(&lock_path)?;
    lock_file
        .lock_exclusive()
        .with_context(|| format!("lock {}", lock_path.display()))?;

    if let Some(output) = &args.output {
        require_real_directory(parent_dir(output))?;
        require_absent(output)?;
    }
    let mut owner = read_owner(&tunnel_path)?;
    let peer = owner
        .add_peer(args.overlay_ip, args.replicas, args.name)
        .context("allocate and sign Peer")?;
    let peer_path = args
        .output
        .unwrap_or_else(|| parent_dir(&tunnel_path).join(format!("{}.peer", peer.peer.peer_id)));
    require_absent(&peer_path)?;

    replace_yaml_file(&tunnel_path, &owner, SecretFile::Yes)?;
    create_yaml_file(&peer_path, &peer, SecretFile::Yes)?;

    println!("Peer: {}", peer_path.display());
    println!("Overlay IP: {}", peer.peer.overlay_ip);
    Ok(())
}

fn init_tunnel(args: InitTunnelArgs) -> Result<()> {
    require_real_directory(&args.output_dir)?;

    let (dial_address, tls_server_name) = match (args.gateway_host, args.gateway_ip) {
        (Some(host), Some(ip)) => (ip.to_string(), Some(host)),
        (Some(host), None) => (host, None),
        (None, Some(ip)) => (ip.to_string(), None),
        (None, None) => unreachable!("clap requires a Gateway host or IP"),
    };
    let trusted_certificate_pem = args
        .gateway_cert
        .map(|path| read_certificate(&path))
        .transpose()?;
    let owner = TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
        transport: args.gateway_transport.as_str().to_owned(),
        dial_address,
        port: args.gateway_port,
        mapping_port: (args.gateway_mapping_port != DEFAULT_GATEWAY_MAPPING_PROBE_PORT)
            .then_some(args.gateway_mapping_port),
        tls_server_name,
        trusted_certificate_pem,
    })
    .context("create Tunnel owner state")?;
    let scope = owner.scope().context("create Gateway Scope")?;

    let tunnel_path = args.output_dir.join(format!("{}.tunnel", owner.tunnel_id));
    let scope_path = args.output_dir.join(format!("{}.scope", owner.tunnel_id));
    create_yaml_file(&tunnel_path, &owner, SecretFile::Yes)?;
    create_yaml_file(&scope_path, &scope, SecretFile::No)?;

    println!("Tunnel: {}", tunnel_path.display());
    println!("Scope: {}", scope_path.display());
    Ok(())
}

fn read_certificate(path: &Path) -> Result<String> {
    reject_symlink(path)?;
    let pem = fs::read_to_string(path)
        .with_context(|| format!("read Gateway certificate {}", path.display()))?;
    normalize_certificate_pem(&pem)
        .with_context(|| format!("validate Gateway certificate {}", path.display()))
}

fn require_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect output directory {}", path.display()))?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        bail!(
            "output directory must be a real directory: {}",
            path.display()
        );
    }
    set_owner_only_directory(path)
        .with_context(|| format!("secure output directory {}", path.display()))?;
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if metadata_is_link_or_reparse(&metadata) {
        bail!("refusing symbolic link: {}", path.display());
    }
    Ok(())
}

fn require_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("refusing to overwrite existing path: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn sidecar_lock_path(tunnel_path: &Path) -> PathBuf {
    let mut path = OsString::from(tunnel_path.as_os_str());
    path.push(".lock");
    PathBuf::from(path)
}

fn open_lock_file(path: &Path) -> Result<File> {
    let parent = parent_dir(path);
    require_real_directory(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
            bail!("lock path must be a regular file: {}", path.display());
        }
    }

    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    configure_secure_open(&mut options, true);
    let file = options
        .open(path)
        .with_context(|| format!("open lock file {}", path.display()))?;
    if !file.metadata().context("inspect lock file")?.is_file() {
        bail!("lock path must be a regular file: {}", path.display());
    }
    set_permissions(&file, path, SecretFile::Yes)?;
    Ok(file)
}

fn read_owner(path: &Path) -> Result<TunnelOwnerFileV2> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    configure_secure_open(&mut options, false);
    let mut file = options
        .open(path)
        .with_context(|| format!("open Tunnel file {}", path.display()))?;
    if !file.metadata().context("inspect Tunnel file")?.is_file() {
        bail!("Tunnel path must be a regular file: {}", path.display());
    }
    let mut yaml = String::new();
    file.read_to_string(&mut yaml)
        .with_context(|| format!("read Tunnel file {}", path.display()))?;
    let owner: TunnelOwnerFileV2 = serde_yaml::from_str(&yaml)
        .with_context(|| format!("parse Tunnel file {}", path.display()))?;
    owner
        .verify()
        .with_context(|| format!("verify Tunnel file {}", path.display()))?;
    Ok(owner)
}

#[cfg(unix)]
fn configure_secure_open(options: &mut OpenOptions, create: bool) {
    use std::os::unix::fs::OpenOptionsExt;

    options.custom_flags(libc::O_NOFOLLOW);
    if create {
        options.mode(0o600);
    }
}

#[cfg(windows)]
fn configure_secure_open(options: &mut OpenOptions, _create: bool) {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[derive(Clone, Copy)]
enum SecretFile {
    Yes,
    No,
}

fn create_yaml_file<T: serde::Serialize>(path: &Path, value: &T, secret: SecretFile) -> Result<()> {
    let parent = parent_dir(path);
    require_real_directory(parent)?;
    let yaml = serde_yaml::to_string(value).context("encode YAML")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary file in {}", parent.display()))?;
    set_permissions(temporary.as_file(), temporary.path(), secret)?;
    temporary
        .write_all(yaml.as_bytes())
        .context("write temporary YAML")?;
    temporary
        .as_file()
        .sync_all()
        .context("sync temporary YAML")?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("create {} without overwrite", path.display()))?;
    sync_parent(parent)?;
    Ok(())
}

fn replace_yaml_file<T: serde::Serialize>(
    path: &Path,
    value: &T,
    secret: SecretFile,
) -> Result<()> {
    reject_symlink(path)?;
    let parent = parent_dir(path);
    require_real_directory(parent)?;
    let yaml = serde_yaml::to_string(value).context("encode YAML")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary file in {}", parent.display()))?;
    set_permissions(temporary.as_file(), temporary.path(), secret)?;
    temporary
        .write_all(yaml.as_bytes())
        .context("write temporary YAML")?;
    temporary
        .as_file()
        .sync_all()
        .context("sync temporary YAML")?;
    reject_symlink(path)?;
    tp_core::atomic_file::persist_atomically(temporary, path)
        .with_context(|| format!("atomically replace {}", path.display()))?;
    sync_parent(parent)?;
    Ok(())
}

#[cfg(unix)]
fn set_permissions(file: &fs::File, _path: &Path, secret: SecretFile) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = match secret {
        SecretFile::Yes => 0o600,
        SecretFile::No => 0o644,
    };
    file.set_permissions(fs::Permissions::from_mode(mode))
        .context("set artifact permissions")
}

#[cfg(windows)]
fn set_permissions(_file: &fs::File, path: &Path, secret: SecretFile) -> Result<()> {
    if matches!(secret, SecretFile::Yes) {
        windows_security::set_owner_only(path, false).context("set owner-only artifact DACL")?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn set_owner_only_directory(path: &Path) -> std::io::Result<()> {
    windows_security::set_owner_only(path, true)
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    windows_security::metadata_is_reparse(metadata)
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<()> {
    fs::File::open(parent)
        .with_context(|| format!("open parent directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("sync parent directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
mod windows_security {
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::os::windows::io::AsRawHandle as _;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W, SET_ACCESS,
        SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ALL_ACCESS, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
    };

    pub(super) fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
        metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    pub(super) fn set_owner_only(path: &Path, directory: bool) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options
            .access_mode(READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(
                FILE_FLAG_OPEN_REPARSE_POINT
                    | if directory {
                        FILE_FLAG_BACKUP_SEMANTICS
                    } else {
                        0
                    },
            );
        let file = options.open(path)?;
        if metadata_is_reparse(&file.metadata()?) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing Windows reparse point",
            ));
        }

        // SAFETY: all pointers are either null or returned by the documented
        // Win32 security APIs, and allocated ACL/descriptor buffers are freed
        // after SetSecurityInfo has consumed them.
        unsafe {
            let handle = file.as_raw_handle();
            let mut owner: PSID = ptr::null_mut();
            let mut descriptor = ptr::null_mut();
            let status = GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            );
            if status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            if descriptor.is_null() || owner.is_null() {
                LocalFree(descriptor);
                return Err(io::Error::other("Windows object has no owner SID"));
            }

            let entry = EXPLICIT_ACCESS_W {
                grfAccessPermissions: FILE_ALL_ACCESS,
                grfAccessMode: SET_ACCESS,
                grfInheritance: if directory {
                    CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
                } else {
                    0
                },
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: ptr::null_mut(),
                    MultipleTrusteeOperation: 0,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_USER,
                    ptstrName: owner.cast(),
                },
            };
            let mut acl: *mut ACL = ptr::null_mut();
            let acl_status = SetEntriesInAclW(1, &entry, ptr::null(), &mut acl);
            if acl_status != ERROR_SUCCESS {
                LocalFree(descriptor);
                return Err(io::Error::from_raw_os_error(acl_status as i32));
            }
            let set_status = SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null(),
            );
            LocalFree(acl.cast());
            LocalFree(descriptor);
            if set_status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(set_status as i32));
            }
        }
        Ok(())
    }
}
