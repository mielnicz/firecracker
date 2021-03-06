// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[macro_use(crate_version, crate_authors)]
extern crate clap;
extern crate libc;
extern crate regex;
extern crate serde;
#[macro_use]
extern crate serde_derive;

extern crate fc_util;
extern crate sys_util;

mod cgroup;
mod chroot;
mod env;

use std::ffi::{CString, NulError, OsString};
use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::result;

use clap::{App, Arg, ArgMatches};

use env::Env;
use fc_util::validators;

pub const KVM_FD: i32 = 3;
pub const LISTENER_FD: i32 = 4;

const SOCKET_FILE_NAME: &str = "api.socket";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FirecrackerContext {
    pub id: String,
    pub jailed: bool,
    pub seccomp_level: u32,
    pub start_time_us: u64,
    pub start_time_cpu_us: u64,
}

#[derive(Debug)]
pub enum Error {
    Canonicalize(PathBuf, io::Error),
    CgroupInheritFromParent(PathBuf, String),
    CgroupLineNotFound(String, String),
    CgroupLineNotUnique(String, String),
    ChangeDevNetTunOwner(sys_util::Error),
    ChdirNewRoot(sys_util::Error),
    CloseNetNsFd(sys_util::Error),
    CloseDevNullFd(sys_util::Error),
    Copy(PathBuf, PathBuf, io::Error),
    CreateDir(PathBuf, io::Error),
    CStringParsing(NulError),
    Dup2(sys_util::Error),
    Exec(io::Error),
    FileCreate(PathBuf, io::Error),
    FileName(PathBuf),
    FileOpen(PathBuf, io::Error),
    FromBytesWithNul(&'static [u8]),
    GetOldFdFlags(sys_util::Error),
    Gid(String),
    InvalidInstanceId(validators::Error),
    Metadata(PathBuf, io::Error),
    MissingParent(PathBuf),
    MkdirOldRoot(sys_util::Error),
    MknodDevNetTun(sys_util::Error),
    MountBind(sys_util::Error),
    MountPropagationPrivate(sys_util::Error),
    NotAFile(PathBuf),
    NotAFolder(PathBuf),
    NotAlphanumeric(String),
    NumaNode(String),
    OpenDevKvm(sys_util::Error),
    OpenDevNull(sys_util::Error),
    OsStringParsing(PathBuf, OsString),
    PivotRoot(sys_util::Error),
    ReadLine(PathBuf, io::Error),
    ReadToString(PathBuf, io::Error),
    RegEx(regex::Error),
    RmOldRootDir(sys_util::Error),
    SetCurrentDir(io::Error),
    SetNetNs(sys_util::Error),
    SetSid(sys_util::Error),
    Uid(String),
    UmountOldRoot(sys_util::Error),
    UnexpectedKvmFd(i32),
    UnexpectedListenerFd(i32),
    UnshareNewNs(sys_util::Error),
    UnixListener(io::Error),
    UnsetCloexec(sys_util::Error),
    Write(PathBuf, io::Error),
}

pub type Result<T> = result::Result<T, Error>;

pub fn clap_app<'a, 'b>() -> App<'a, 'b> {
    // Initially, the uid and gid params had default values, but it turns out that it's quite
    // easy to shoot yourself in the foot by not setting proper permissions when preparing the
    // contents of the jail, so I think their values should be provided explicitly.
    App::new("jailer")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Jail a microVM.")
        .arg(
            Arg::with_name("id")
                .long("id")
                .help("Jail ID")
                .required(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("exec_file")
                .long("exec-file")
                .help("File path to exec into.")
                .required(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("numa_node")
                .long("node")
                .help("NUMA node to assign this microVM to.")
                .required(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("uid")
                .long("uid")
                .help("The user identifier the jailer switches to after exec.")
                .required(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("gid")
                .long("gid")
                .help("The group identifier the jailer switches to after exec.")
                .required(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("chroot_base")
                .long("chroot-base-dir")
                .help("The base folder where chroot jails are located.")
                .required(false)
                .default_value("/srv/jailer")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("netns")
                .long("netns")
                .help("Path to the network namespace this microVM should join.")
                .required(false)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("daemonize")
                .long("daemonize")
                .help("Daemonize the jailer before exec, by invoking setsid(), and redirecting the standard I/O file descriptors to /dev/null.")
                .required(false)
                .takes_value(false),
        )
        .arg(
            Arg::with_name("seccomp-level")
                .long("seccomp-level")
                .help("Level of seccomp filtering that will be passed to executed path as argument.\n
    - Level 0: No filtering.\n
    - Level 1: Seccomp filtering by syscall number.\n
    - Level 2: Seccomp filtering by syscall number and argument values.\n
")
                .required(false)
                .takes_value(true)
                .default_value("0")
                .possible_values(&["0", "1", "2"]),
        )
}

fn sanitize_process() {
    // First thing to do is make sure we don't keep any inherited FDs
    // other that IN, OUT and ERR.

    // Safe because sysconf is available on all flavors of Linux.
    let fd_size = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) } as i32;
    for i in 3..fd_size {
        // Safe because close() cannot fail when passed a valid parameter.
        unsafe { libc::close(i) };
    }
}

fn open_dev_kvm() -> Result<i32> {
    // Safe because we use a constant null-terminated string and verify the result.
    let ret = unsafe { libc::open("/dev/kvm\0".as_ptr() as *const libc::c_char, libc::O_RDWR) };

    if ret < 0 {
        return Err(Error::OpenDevKvm(sys_util::Error::last()));
    }

    if ret != KVM_FD {
        return Err(Error::UnexpectedKvmFd(ret));
    }

    Ok(ret)
}

pub fn run(args: ArgMatches, start_time_us: u64, start_time_cpu_us: u64) -> Result<()> {
    // We open /dev/kvm and create the listening socket. These file descriptors will be
    // passed on to Firecracker post exec, and used via knowing their values in advance.

    // TODO: can a malicious guest that takes over firecracker use its access to the KVM fd to
    // starve the host of resources? (cgroups should take care of that, but do they currently?)

    sanitize_process();
    open_dev_kvm()?;

    let env = Env::new(args, start_time_us, start_time_cpu_us)?;

    // Ensure the folder exists.
    fs::create_dir_all(env.chroot_dir())
        .map_err(|e| Error::CreateDir(env.chroot_dir().to_owned(), e))?;

    // The unwrap should not fail, since the end of chroot_dir looks like ..../<id>/root
    let listener = UnixListener::bind(env.chroot_dir().parent().unwrap().join(SOCKET_FILE_NAME))
        .map_err(|e| Error::UnixListener(e))?;

    let listener_fd = listener.as_raw_fd();
    if listener_fd != LISTENER_FD {
        return Err(Error::UnexpectedListenerFd(listener_fd));
    }

    // It turns out Rust is so safe, it opens everything with FD_CLOEXEC, which we have to unset.

    // This is safe because we know fd and the cmd are valid.
    let mut fd_flags = unsafe { libc::fcntl(listener_fd, libc::F_GETFD, 0) };
    if fd_flags < 0 {
        return Err(Error::GetOldFdFlags(sys_util::Error::last()));
    }

    fd_flags &= !libc::FD_CLOEXEC;

    // This is safe because we know the fd, the cmd, and the last arg are valid.
    if unsafe { libc::fcntl(listener_fd, libc::F_SETFD, fd_flags) } < 0 {
        return Err(Error::UnsetCloexec(sys_util::Error::last()));
    }

    env.run()
}

/// Turns an AsRef<Path> into a CString (c style string).
/// The expect should not fail, since Linux paths only contain valid Unicode chars (do they?),
/// and do not contain null bytes (do they?).
fn to_cstring<T: AsRef<Path>>(path: T) -> Result<CString> {
    let path_str = path
        .as_ref()
        .to_path_buf()
        .into_os_string()
        .into_string()
        .map_err(|e| Error::OsStringParsing(path.as_ref().to_path_buf(), e))?;
    CString::new(path_str).map_err(Error::CStringParsing)
}
